use crate::{
    adapter,
    handler::{Event as HandlerEvent, Handler},
    protocol::DEFAULT_PROTOCOL_NAME,
    InboundId, InboundIdGen, OutboundId, OutboundRequest,
};
use futures::{
    channel::oneshot, future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt,
};
use libp2p::{
    core::{ConnectedPoint, Endpoint, Multiaddr},
    identity::PeerId,
    swarm::{
        self,
        behaviour::{ConnectionClosed, ConnectionEstablished, FromSwarm, NewListener},
        derive_prelude::ListenerId,
        dial_opts::DialOpts,
        ConnectionDenied, ConnectionId, DialFailure, NetworkBehaviour, NotifyHandler,
        PollParameters, THandler, THandlerInEvent, THandlerOutEvent, ToSwarm,
    },
};
use smallvec::SmallVec;
use std::{
    collections::{HashMap, VecDeque},
    io,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tonic::transport::server::Router;

#[derive(Debug)]
pub enum Event {
    InboundSuccess {
        peer: PeerId,
        inbound_id: InboundId,
    },

    InboundFailure {
        peer: PeerId,
        inbound_id: InboundId,
        error: InboundError,
    },

    OutboundSuccess {
        peer: PeerId,
        outbound_id: OutboundId,
        channel: adapter::Channel,
    },

    OutboundFailure {
        peer: PeerId,
        outbound_id: OutboundId,
        error: OutboundError,
    },

    LiteralOutboundFailure {
        peer: PeerId,
        outbound_id: OutboundId,
        error: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum InboundError {
    #[error("reading the inbound request timed out")]
    Timeout,

    #[error("the connection closed before a response could be send")]
    ConnectionClosed,

    #[error("the local peer supports none of the protocol requested by the remote")]
    UnsupportedProtocol,
}

#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("Failed to dial the requested peer")]
    DialFailure,

    #[error("the request timed out before a response was received")]
    Timeout,

    #[error(transparent)]
    GrpcChannel(#[from] adapter::TonicTransportError),

    #[error("the remote supports none of the requested protocol")]
    UnsupportedProtocol,
}

#[derive(Debug, Clone)]
pub struct Config {
    protocol_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            protocol_name: String::from_utf8_lossy(DEFAULT_PROTOCOL_NAME).to_string(),
        }
    }
}

impl Config {
    pub fn with_protocol_name(mut self, name: &str) -> Self {
        self.protocol_name = name.to_string();
        self
    }
}

pub struct Behaviour {
    local_peer_id: PeerId,
    config: Config,
    connected: HashMap<PeerId, SmallVec<[Connection; 2]>>,
    pending_events: VecDeque<ToSwarm<Event, OutboundRequest>>,

    grpc_service_router: Option<Router>,
    inbound_stream_tx: Option<mpsc::UnboundedSender<io::Result<adapter::NegotiatedStreamWrapper>>>,

    inbound_id_gen: InboundIdGen,
    next_outbound_id: OutboundId,
    addresses: HashMap<PeerId, SmallVec<[Multiaddr; 6]>>,
    pending_outbounds: HashMap<PeerId, OutboundRequest>,
    pending_grpc_channel_futs: FuturesUnordered<
        BoxFuture<'static, (adapter::OutboundGrpcChannelResult, OutboundRequest, PeerId)>,
    >,
}

impl Behaviour {
    pub fn new(local_peer_id: PeerId, grpc_service_router: Option<Router>) -> Self {
        Self {
            local_peer_id,
            config: Default::default(),
            connected: HashMap::new(),
            grpc_service_router,
            inbound_stream_tx: None,
            pending_events: VecDeque::new(),
            addresses: HashMap::new(),
            pending_outbounds: HashMap::new(),
            inbound_id_gen: InboundIdGen::new(1),
            next_outbound_id: OutboundId::new(1),
            pending_grpc_channel_futs: FuturesUnordered::new(),
        }
    }

    pub fn with_protocol_name(mut self, name: &str) -> Self {
        self.config.protocol_name = name.to_string();
        self
    }

    pub fn with_config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    fn on_connection_established(
        &mut self,
        ConnectionEstablished {
            peer_id,
            connection_id,
            endpoint,
            other_established,
            ..
        }: ConnectionEstablished,
    ) {
        let address = match endpoint {
            ConnectedPoint::Dialer { address, .. } => Some(address.clone()),
            ConnectedPoint::Listener { .. } => None,
        };

        self.connected
            .entry(peer_id)
            .or_default()
            .push(Connection::new(connection_id, address));

        if other_established == 0 {
            if let Some(req) = self.pending_outbounds.remove(&peer_id) {
                if let Some(connections) = self.connected.get_mut(&peer_id) {
                    let conn = connections
                        .iter_mut()
                        .filter(|conn| conn.id == connection_id)
                        .nth(0)
                        .expect("the connection set must contain the connection_id that was just inserted");
                    conn.serving_channel = Some((req.outbound_id, None));
                    self.pending_events.push_back(ToSwarm::NotifyHandler {
                        peer_id,
                        handler: NotifyHandler::One(conn.id),
                        event: req,
                    });
                } else {
                    unreachable!(
                        "The connected HashMap must contain the Peer that was just inserted"
                    );
                }
            }
        }
    }

    fn on_connection_closed(
        &mut self,
        ConnectionClosed {
            peer_id,
            connection_id,
            remaining_established,
            ..
        }: ConnectionClosed<<Self as NetworkBehaviour>::ConnectionHandler>,
    ) {
        let connections = self
            .connected
            .get_mut(&peer_id)
            .expect("Expected some established connection to peer before closing.");

        let _ = connections
            .iter()
            .position(|c| c.id == connection_id)
            .map(|p: usize| connections.remove(p))
            .expect("Expected connection to be established before closing.");

        debug_assert_eq!(connections.is_empty(), remaining_established == 0);
        if connections.is_empty() {
            self.connected.remove(&peer_id);
        }
    }

    fn on_new_listener(&mut self, listener_id: ListenerId) {
        if let Some(router) = self.grpc_service_router.take() {
            let (in_tx, in_rx) = mpsc::unbounded_channel();
            self.inbound_stream_tx = Some(in_tx);
            tokio::spawn(router.serve_with_incoming(adapter::StreamPumper::new(in_rx)));
            tracing::info!("gRPC serve on listener: {:?}", listener_id);
        } else {
            tracing::warn!("the tonic service router not set, gRPC not on serve");
        }
    }

    pub fn set_grpc_service_router(&mut self, grpc_service_router: Router) {
        self.grpc_service_router = Some(grpc_service_router);
    }

    pub fn initiate_grpc_outbound(&mut self, target_peer_id: &PeerId) -> GrpcOutbound {
        if let Some(conns) = self.connected.get(target_peer_id) {
            for conn in conns {
                if let Some((outbound_id, Some(channel))) = &conn.serving_channel {
                    return GrpcOutbound::ServingChannel {
                        channel: channel.clone(),
                        outbound_id: *outbound_id,
                    };
                }
            }
        }

        if let Some(req) = self.pending_outbounds.get(target_peer_id) {
            return GrpcOutbound::PendingOutbound {
                outbound_id: req.outbound_id,
            };
        }

        let (chr_tx, chr_rx) = oneshot::channel();
        self.pending_events.push_back(ToSwarm::Dial {
            opts: DialOpts::peer_id(*target_peer_id).build(),
        });
        let outbound_id = self.next_outbound_id();
        self.pending_outbounds.insert(
            *target_peer_id,
            OutboundRequest {
                peer: *target_peer_id,
                outbound_id,
                sender: chr_tx,
            },
        );

        GrpcOutbound::WaitingChannel {
            outbound_id,
            channel_rx: chr_rx,
        }
    }

    fn next_outbound_id(&mut self) -> OutboundId {
        let outbound_id = self.next_outbound_id;
        self.next_outbound_id += 1;
        outbound_id
    }

    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Adds a known address for a peer that can be used for
    /// dialing attempts by the `Swarm`, i.e. is returned
    /// by [`NetworkBehaviour::addresses_of_peer`].
    ///
    /// Addresses added in this way are only removed by `remove_address`.
    pub fn add_address(&mut self, peer: &PeerId, address: Multiaddr) {
        self.addresses.entry(*peer).or_default().push(address);
    }

    /// Removes an address of a peer previously added via `add_address`.
    pub fn remove_address(&mut self, peer: &PeerId, address: &Multiaddr) {
        let mut last = false;
        if let Some(addresses) = self.addresses.get_mut(peer) {
            addresses.retain(|a| a != address);
            last = addresses.is_empty();
        }
        if last {
            self.addresses.remove(peer);
        }
    }

    /// Checks whether a peer is currently connected.
    pub fn is_connected(&self, peer: &PeerId) -> bool {
        if let Some(connections) = self.connected.get(peer) {
            !connections.is_empty()
        } else {
            false
        }
    }

    fn on_dial_failure(&mut self, DialFailure { peer_id, .. }: DialFailure) {
        if let Some(peer) = peer_id {
            if let Some(req) = self.pending_outbounds.remove(&peer) {
                self.pending_events
                    .push_back(ToSwarm::GenerateEvent(Event::OutboundFailure {
                        peer,
                        outbound_id: req.outbound_id,
                        error: OutboundError::DialFailure,
                    }));
                let _ = req.sender.send(Err(OutboundError::DialFailure));
            }
        }
    }

    fn remove_pending_outbound(
        &mut self,
        peer_id: &PeerId,
        connection_id: ConnectionId,
        outbount_id: &OutboundId,
    ) -> bool {
        self.get_connection_mut(peer_id, connection_id)
            .map(|c| {
                if let Some((id, ch_opt)) = &c.serving_channel {
                    debug_assert!(ch_opt.is_none(), "Expect the none serving channel");
                    if *outbount_id == *id {
                        c.serving_channel = None;
                        return true;
                    }
                }
                return false;
            })
            .unwrap_or(false)
    }

    fn get_connection_mut(
        &mut self,
        peer_id: &PeerId,
        connection_id: ConnectionId,
    ) -> Option<&mut Connection> {
        self.connected
            .get_mut(peer_id)
            .and_then(|connections| connections.iter_mut().find(|c| c.id == connection_id))
    }
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = Handler;
    type OutEvent = Event;

    fn handle_established_inbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> std::result::Result<THandler<Self>, ConnectionDenied> {
        Ok(Handler::new(
            self.config.protocol_name.clone(),
            self.inbound_id_gen.clone(),
        ))
    }

    fn handle_established_outbound_connection(
        &mut self,
        _: ConnectionId,
        _: PeerId,
        _: &Multiaddr,
        _: Endpoint,
    ) -> std::result::Result<THandler<Self>, ConnectionDenied> {
        Ok(Handler::new(
            self.config.protocol_name.clone(),
            self.inbound_id_gen.clone(),
        ))
    }

    fn handle_pending_outbound_connection(
        &mut self,
        _connection_id: ConnectionId,
        maybe_peer: Option<PeerId>,
        _addresses: &[Multiaddr],
        _effective_role: Endpoint,
    ) -> Result<Vec<Multiaddr>, ConnectionDenied> {
        let peer = match maybe_peer {
            None => return Ok(vec![]),
            Some(peer) => peer,
        };

        let mut addresses = Vec::new();
        if let Some(connections) = self.connected.get(&peer) {
            addresses.extend(connections.iter().filter_map(|c| c.target_address.clone()))
        }
        if let Some(more) = self.addresses.get(&peer) {
            addresses.extend(more.into_iter().cloned());
        }
        Ok(addresses)
    }

    fn on_connection_handler_event(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
        event: THandlerOutEvent<Self>,
    ) {
        match event {
            HandlerEvent::InboundStream { inbound_id, stream } => {
                if let Some(sender) = &mut self.inbound_stream_tx {
                    if let Err(e) = sender.send(Ok(adapter::NegotiatedStreamWrapper::new(
                        stream,
                        peer,
                        connection_id,
                    ))) {
                        tracing::error!(error = ?e, "inbound stream send error");
                    }
                    tracing::debug!(remote_peer=?peer, ?connection_id, ?inbound_id, "got gRPC inbound stream");
                    self.pending_events
                        .push_back(ToSwarm::GenerateEvent(Event::InboundSuccess {
                            peer,
                            inbound_id,
                        }));
                }
            }

            HandlerEvent::OutboundStream {
                outbound_request,
                stream,
            } => {
                let stream_wrapper =
                    adapter::NegotiatedStreamWrapper::new(stream, peer, connection_id);
                let fut = async move {
                    let chr = adapter::make_outbound_stream_to_grpc_channel(stream_wrapper).await;
                    (chr, outbound_request, peer)
                }
                .boxed();
                self.pending_grpc_channel_futs.push(fut);
            }

            HandlerEvent::InboundTimeout(inbound_id) => {
                self.pending_events
                    .push_back(ToSwarm::GenerateEvent(Event::InboundFailure {
                        peer,
                        inbound_id,
                        error: InboundError::Timeout,
                    }));
            }

            HandlerEvent::InboundUnsupportedProtocols(inbound_id) => {
                self.pending_events
                    .push_back(ToSwarm::GenerateEvent(Event::InboundFailure {
                        peer,
                        inbound_id,
                        error: InboundError::UnsupportedProtocol,
                    }));
            }

            HandlerEvent::OutboundTimeout(req) => {
                let removed = self.remove_pending_outbound(&peer, connection_id, &req.outbound_id);
                debug_assert!(
                    removed,
                    "Expect outbound_id to be pending before request times out."
                );

                self.pending_events
                    .push_back(ToSwarm::GenerateEvent(Event::OutboundFailure {
                        peer,
                        outbound_id: req.outbound_id,
                        error: OutboundError::Timeout,
                    }));
            }

            HandlerEvent::OutboundUnsupportedProtocols(req) => {
                let removed = self.remove_pending_outbound(&peer, connection_id, &req.outbound_id);
                debug_assert!(
                    removed,
                    "Expect outbound_id to be pending before failing to connect.",
                );

                self.pending_events
                    .push_back(ToSwarm::GenerateEvent(Event::OutboundFailure {
                        peer,
                        outbound_id: req.outbound_id,
                        error: OutboundError::UnsupportedProtocol,
                    }));
            }
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _: &mut impl PollParameters,
    ) -> Poll<ToSwarm<Self::OutEvent, THandlerInEvent<Self>>> {
        if let Some(evt) = self.pending_events.pop_front() {
            return Poll::Ready(evt);
        } else if self.pending_events.capacity() > crate::EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_events.shrink_to_fit();
        }

        match self.pending_grpc_channel_futs.poll_next_unpin(cx) {
            Poll::Ready(Some((chr, req, peer))) => match chr {
                Ok(channel) => {
                    tracing::debug!("gRPC channel ready for {} {}", req.peer, req.outbound_id);
                    if let Some(conns) = self.connected.get_mut(&peer) {
                        for conn in conns {
                            if let Some((outbound_id, _)) = &conn.serving_channel {
                                if req.outbound_id == *outbound_id {
                                    conn.serving_channel =
                                        Some((*outbound_id, Some(channel.clone())));
                                }
                            }
                        }
                    }
                    let _ = req.sender.send(Ok(channel.clone()));
                    return Poll::Ready(ToSwarm::GenerateEvent(Event::OutboundSuccess {
                        peer: req.peer,
                        outbound_id: req.outbound_id,
                        channel,
                    }));
                }
                Err(error) => {
                    let err_str = error.to_string();
                    let _ = req.sender.send(Err(OutboundError::GrpcChannel(error)));
                    return Poll::Ready(ToSwarm::GenerateEvent(Event::LiteralOutboundFailure {
                        peer: req.peer,
                        outbound_id: req.outbound_id,
                        error: err_str,
                    }));
                }
            },
            Poll::Ready(None) | Poll::Pending => {}
        }

        Poll::Pending
    }

    fn on_swarm_event(&mut self, event: swarm::behaviour::FromSwarm<Self::ConnectionHandler>) {
        match event {
            FromSwarm::ConnectionEstablished(connection_established) => {
                self.on_connection_established(connection_established)
            }
            FromSwarm::ConnectionClosed(connection_closed) => {
                self.on_connection_closed(connection_closed)
            }
            FromSwarm::NewListener(NewListener { listener_id }) => {
                self.on_new_listener(listener_id)
            }
            FromSwarm::DialFailure(dial_failure) => self.on_dial_failure(dial_failure),
            FromSwarm::NewListenAddr(_)
            | FromSwarm::ListenFailure(_)
            | FromSwarm::AddressChange(_)
            | FromSwarm::ExpiredListenAddr(_)
            | FromSwarm::ListenerError(_)
            | FromSwarm::ListenerClosed(_)
            | FromSwarm::NewExternalAddr(_)
            | FromSwarm::ExpiredExternalAddr(_) => {}
        }
    }
}

struct Connection {
    id: ConnectionId,
    target_address: Option<Multiaddr>,
    serving_channel: Option<(OutboundId, Option<adapter::Channel>)>,
}

impl Connection {
    fn new(id: ConnectionId, address: Option<Multiaddr>) -> Self {
        Self {
            id,
            target_address: address,
            serving_channel: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GrpcOutboundMapError {
    #[error("the prev gRPC outbound request is pending")]
    PrevOutbondPending(OutboundId),

    #[error(transparent)]
    OutboundError(#[from] OutboundError),

    #[error(transparent)]
    SenderCanceled(#[from] oneshot::Canceled),
}

pub enum GrpcOutbound {
    ServingChannel {
        outbound_id: OutboundId,
        channel: adapter::Channel,
    },

    WaitingChannel {
        outbound_id: OutboundId,
        channel_rx: oneshot::Receiver<Result<adapter::Channel, OutboundError>>,
    },

    PendingOutbound {
        outbound_id: OutboundId,
    },
}

impl GrpcOutbound {
    pub async fn map(self) -> Result<(adapter::Channel, OutboundId), GrpcOutboundMapError> {
        match self {
            Self::ServingChannel {
                channel,
                outbound_id,
            } => Ok((channel, outbound_id)),

            Self::WaitingChannel {
                channel_rx,
                outbound_id,
            } => match channel_rx.await {
                Ok(chr) => match chr {
                    Ok(ch) => Ok((ch, outbound_id)),
                    Err(e) => Err(GrpcOutboundMapError::OutboundError(e)),
                },
                Err(e) => Err(GrpcOutboundMapError::SenderCanceled(e)),
            },

            Self::PendingOutbound { outbound_id } => {
                Err(GrpcOutboundMapError::PrevOutbondPending(outbound_id))
            }
        }
    }
}
