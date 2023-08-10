use crate::{
    adapter,
    handler::{self, Event as HandlerEvent, Handler},
};
use libp2p::{
    core::{ConnectedPoint, Endpoint, Multiaddr},
    identity::PeerId,
    swarm::{
        self,
        behaviour::{ConnectionClosed, ConnectionEstablished, FromSwarm, NewListener},
        derive_prelude::ListenerId,
        dial_opts::DialOpts,
        ConnectionDenied, ConnectionId, NetworkBehaviour, PollParameters, THandler,
        THandlerInEvent, THandlerOutEvent, ToSwarm,
    },
};
use smallvec::SmallVec;
use std::{
    collections::{HashMap, VecDeque},
    io,
    task::{Context, Poll},
};
use tokio::sync::{mpsc, oneshot};
use tonic::transport::server::Router;

#[derive(Debug)]
pub enum Event {
    OutboundSuccess {
        peer: PeerId,
        connection_id: ConnectionId,
    },
}

enum Command {
    Outbound {
        sender: oneshot::Sender<adapter::TonicChannelResult>,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    protocol_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            protocol_name: String::from_utf8_lossy(crate::DEFAULT_PROTOCOL_NAME).to_string(),
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
    grpc_service_router: Option<Router>,
    inbound_stream_tx: Option<mpsc::UnboundedSender<io::Result<adapter::NegotiatedStreamWrapper>>>,
    connected: HashMap<PeerId, SmallVec<[Connection; 2]>>,
    pending_events: VecDeque<ToSwarm<Event, handler::InEvent>>,
    addresses: HashMap<PeerId, SmallVec<[Multiaddr; 6]>>,
    pending_commands: HashMap<PeerId, Command>,
}

impl Behaviour {
    pub fn new(local_peer_id: PeerId, grpc_service_router: Option<Router>) -> Self {
        Self {
            local_peer_id,
            config: Default::default(),
            grpc_service_router,
            inbound_stream_tx: None,
            connected: HashMap::new(),
            pending_events: VecDeque::new(),
            addresses: HashMap::new(),
            pending_commands: HashMap::new(),
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

    pub async fn grpc_client_channel(
        &mut self,
        target_peer_id: PeerId,
    ) -> adapter::TonicChannelResult {
        let (tx, rx) = oneshot::channel();
        self.pending_events.push_back(ToSwarm::Dial {
            opts: DialOpts::peer_id(target_peer_id.clone()).build(),
        });
        self.pending_commands
            .insert(target_peer_id, Command::Outbound { sender: tx });
        let r = rx.await.unwrap();
        return r;
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
}

impl NetworkBehaviour for Behaviour {
    type ConnectionHandler = Handler;
    type OutEvent = Event;

    fn handle_established_inbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _: &Multiaddr,
        _: &Multiaddr,
    ) -> std::result::Result<THandler<Self>, ConnectionDenied> {
        Ok(Handler::new(
            peer_id,
            connection_id,
            self.config.protocol_name.clone(),
        ))
    }

    fn handle_established_outbound_connection(
        &mut self,
        connection_id: ConnectionId,
        peer_id: PeerId,
        _: &Multiaddr,
        _: Endpoint,
    ) -> std::result::Result<THandler<Self>, ConnectionDenied> {
        Ok(Handler::new(
            peer_id,
            connection_id,
            self.config.protocol_name.clone(),
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
            addresses.extend(connections.iter().filter_map(|c| c.address.clone()))
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
            HandlerEvent::InboundStream(stream) => {
                tracing::debug!(inbound_stream = ?stream);
                if let Some(sender) = &mut self.inbound_stream_tx {
                    if let Err(e) = sender.send(Ok(adapter::NegotiatedStreamWrapper::new(
                        stream,
                        peer,
                        connection_id,
                    ))) {
                        tracing::error!(error = ?e, "inbound stream send error");
                    }
                }
            }
            HandlerEvent::OutboundGrpcChannel(result) => {
                if let Some(cmd) = self.pending_commands.remove(&peer) {
                    match cmd {
                        Command::Outbound {
                            sender: channel_sender,
                            ..
                        } => {
                            let _ = channel_sender.send(result);
                        }
                    }
                }
            }
        }
    }

    fn poll(
        &mut self,
        _: &mut Context<'_>,
        _: &mut impl PollParameters,
    ) -> Poll<ToSwarm<Self::OutEvent, THandlerInEvent<Self>>> {
        if let Some(evt) = self.pending_events.pop_front() {
            return Poll::Ready(evt);
        } else if self.pending_events.capacity() > crate::EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_events.shrink_to_fit();
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
            FromSwarm::NewListenAddr(_)
            | FromSwarm::ListenFailure(_)
            | FromSwarm::DialFailure(_)
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
    address: Option<Multiaddr>,
}

impl Connection {
    fn new(id: ConnectionId, address: Option<Multiaddr>) -> Self {
        Self { id, address }
    }
}
