use crate::adapter;
use futures::FutureExt;
use libp2p::{
    core::upgrade::ReadyUpgrade,
    swarm::{
        handler::{
            ConnectionEvent, DialUpgradeError, FullyNegotiatedInbound, FullyNegotiatedOutbound,
        },
        ConnectionHandler, ConnectionHandlerEvent, ConnectionId, KeepAlive, NegotiatedSubstream,
        SubstreamProtocol,
    },
    PeerId,
};

use std::collections::VecDeque;
use std::task::{Context, Poll};

#[derive(Debug)]
pub enum Event {
    InboundStream(NegotiatedSubstream),
    OutboundGrpcChannel(adapter::TonicChannelResult),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("time out")]
    Timeout,
}

pub struct Handler {
    remote_peer_id: PeerId,
    connection_id: ConnectionId,
    protocol_name: String,
    inbound_streams: VecDeque<NegotiatedSubstream>,
    outbound: Option<OutboundState>,
}

impl Handler {
    pub fn new(remote_peer_id: PeerId, connection_id: ConnectionId, protocol_name: String) -> Self {
        Handler {
            remote_peer_id,
            connection_id,
            protocol_name,
            inbound_streams: VecDeque::new(),
            outbound: None,
        }
    }

    fn on_dial_upgrade_error(
        &mut self,
        DialUpgradeError { .. }: DialUpgradeError<
            <Self as ConnectionHandler>::OutboundOpenInfo,
            <Self as ConnectionHandler>::OutboundProtocol,
        >,
    ) {
    }
}

pub type InEvent = void::Void;

impl ConnectionHandler for Handler {
    type InEvent = InEvent;
    type OutEvent = Event;
    type Error = Error;
    type InboundProtocol = ReadyUpgrade<String>;
    type OutboundProtocol = ReadyUpgrade<String>;
    type OutboundOpenInfo = ();
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<ReadyUpgrade<String>, ()> {
        SubstreamProtocol::new(ReadyUpgrade::new(self.protocol_name.clone()), ())
    }

    fn on_behaviour_event(&mut self, _: Self::InEvent) {}

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        if let Some(stream) = self.inbound_streams.pop_front() {
            return Poll::Ready(ConnectionHandlerEvent::Custom(Event::InboundStream(stream)));
        }

        match self.outbound.take() {
            Some(OutboundState::MakeGrpcChannel(mut fut)) => match fut.poll_unpin(cx) {
                Poll::Ready(result) => {
                    self.outbound = Some(OutboundState::GrpcServing);
                    return Poll::Ready(ConnectionHandlerEvent::Custom(
                        Event::OutboundGrpcChannel(result),
                    ));
                }
                Poll::Pending => {}
            },
            Some(OutboundState::OpenStream) => {
                self.outbound = Some(OutboundState::OpenStream);
            }
            Some(OutboundState::GrpcServing) => {
                self.outbound = Some(OutboundState::GrpcServing);
            }
            None => {
                self.outbound = Some(OutboundState::OpenStream);
                let protocol =
                    SubstreamProtocol::new(ReadyUpgrade::new(self.protocol_name.clone()), ());
                return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol });
            }
        }

        Poll::Pending
    }

    fn on_connection_event(
        &mut self,
        event: ConnectionEvent<
            Self::InboundProtocol,
            Self::OutboundProtocol,
            Self::InboundOpenInfo,
            Self::OutboundOpenInfo,
        >,
    ) {
        match event {
            ConnectionEvent::FullyNegotiatedInbound(FullyNegotiatedInbound {
                protocol: stream,
                ..
            }) => {
                if !self.inbound_streams.is_empty() {
                    tracing::warn!(
                        "New inbound gRPC request from {} while a previous one \
                         is still pending. Queueing the new one.",
                        self.remote_peer_id,
                    );
                }
                self.inbound_streams.push_back(stream);
            }
            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol: stream,
                ..
            }) => {
                let stream_wrapper = adapter::NegotiatedStreamWrapper::new(
                    stream,
                    self.remote_peer_id.clone(),
                    self.connection_id,
                );
                self.outbound = Some(OutboundState::MakeGrpcChannel(
                    adapter::make_stream_to_tonic_channel(stream_wrapper).boxed(),
                ));
            }
            ConnectionEvent::DialUpgradeError(dial_upgrade_error) => {
                self.on_dial_upgrade_error(dial_upgrade_error)
            }
            ConnectionEvent::AddressChange(_) | ConnectionEvent::ListenUpgradeError(_) => {}
        }
    }
}

enum OutboundState {
    OpenStream,
    MakeGrpcChannel(adapter::GrpcChannelMakeFuture),
    GrpcServing,
}
