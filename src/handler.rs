use crate::{protocol::DirectGrpcUpgradeProtocol, InboundId, InboundIdGen, OutboundId};
use libp2p::{
    core::upgrade::{NegotiationError, UpgradeError},
    swarm::{
        handler::{
            ConnectionEvent, ConnectionHandlerUpgrErr, DialUpgradeError, FullyNegotiatedInbound,
            FullyNegotiatedOutbound, ListenUpgradeError,
        },
        ConnectionHandler, ConnectionHandlerEvent, KeepAlive, NegotiatedSubstream,
        SubstreamProtocol,
    },
};
use std::{
    collections::VecDeque,
    fmt::Debug,
    io,
    task::{Context, Poll},
};

#[derive(Debug)]
pub enum Event {
    InboundStream {
        inbound_id: InboundId,
        stream: NegotiatedSubstream,
    },

    OutboundStream {
        outbound_id: OutboundId,
        stream: NegotiatedSubstream,
    },

    OutboundTimeout(OutboundId),

    OutboundUnsupportedProtocols(OutboundId),

    InboundTimeout(InboundId),

    InboundUnsupportedProtocols(InboundId),
}

pub struct Handler {
    protocol_name: String,
    outbound: Option<OutboundId>,
    pending_error: Option<ConnectionHandlerUpgrErr<io::Error>>,
    pending_events: VecDeque<Event>,
    inbound_id_gen: InboundIdGen,
}

impl Handler {
    pub fn new(protocol_name: String, inbound_id_gen: InboundIdGen) -> Self {
        Handler {
            protocol_name,
            inbound_id_gen,
            outbound: None,
            pending_error: None,
            pending_events: VecDeque::new(),
        }
    }

    fn on_dial_upgrade_error(
        &mut self,
        DialUpgradeError { info, error }: DialUpgradeError<
            <Self as ConnectionHandler>::OutboundOpenInfo,
            <Self as ConnectionHandler>::OutboundProtocol,
        >,
    ) {
        match error {
            ConnectionHandlerUpgrErr::Timeout => {
                self.pending_events.push_back(Event::OutboundTimeout(info));
            }
            ConnectionHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {
                // The remote merely doesn't support the protocol(s) we requested.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                // An event is reported to permit user code to react to the fact that
                // the remote peer does not support the requested protocol(s).
                self.pending_events
                    .push_back(Event::OutboundUnsupportedProtocols(info));
            }
            _ => {
                // Anything else is considered a fatal error or misbehaviour of
                // the remote peer and results in closing the connection.
                self.pending_error = Some(error);
            }
        }
    }

    fn on_listen_upgrade_error(
        &mut self,
        ListenUpgradeError { info, error }: ListenUpgradeError<
            <Self as ConnectionHandler>::InboundOpenInfo,
            <Self as ConnectionHandler>::InboundProtocol,
        >,
    ) {
        match error {
            ConnectionHandlerUpgrErr::Timeout => {
                self.pending_events.push_back(Event::InboundTimeout(info))
            }
            ConnectionHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {
                // The local peer merely doesn't support the protocol(s) requested.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                // An event is reported to permit user code to react to the fact that
                // the local peer does not support the requested protocol(s).
                self.pending_events
                    .push_back(Event::InboundUnsupportedProtocols(info));
            }
            _ => {
                // Anything else is considered a fatal error or misbehaviour of
                // the remote peer and results in closing the connection.
                self.pending_error = Some(error);
            }
        }
    }
}

impl ConnectionHandler for Handler {
    type InEvent = OutboundId;
    type OutEvent = Event;
    type Error = ConnectionHandlerUpgrErr<io::Error>;
    type InboundProtocol = DirectGrpcUpgradeProtocol;
    type InboundOpenInfo = InboundId;
    type OutboundProtocol = DirectGrpcUpgradeProtocol;
    type OutboundOpenInfo = OutboundId;

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        SubstreamProtocol::new(
            DirectGrpcUpgradeProtocol {
                protocol_name: self.protocol_name.clone(),
            },
            self.inbound_id_gen.next(),
        )
    }

    fn on_behaviour_event(&mut self, outbound_id: Self::InEvent) {
        if let Some(prev_outbound_id) = self.outbound.replace(outbound_id) {
            tracing::warn!(
                "The previous {:?} was not processed and will be replaced with a new {:?}",
                prev_outbound_id,
                outbound_id
            );
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        KeepAlive::Yes
    }

    fn poll(
        &mut self,
        _: &mut Context<'_>,
    ) -> Poll<
        ConnectionHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        // Check for a pending (fatal) error.
        if let Some(err) = self.pending_error.take() {
            // The handler will not be polled again by the `Swarm`.
            return Poll::Ready(ConnectionHandlerEvent::Close(err));
        }
        // Drain pending events.
        if let Some(event) = self.pending_events.pop_front() {
            return Poll::Ready(ConnectionHandlerEvent::Custom(event));
        } else if self.pending_events.capacity() > crate::EMPTY_QUEUE_SHRINK_THRESHOLD {
            self.pending_events.shrink_to_fit();
        }

        if let Some(outbound_id) = self.outbound.take() {
            let protocol = SubstreamProtocol::new(
                DirectGrpcUpgradeProtocol {
                    protocol_name: self.protocol_name.clone(),
                },
                outbound_id,
            );
            return Poll::Ready(ConnectionHandlerEvent::OutboundSubstreamRequest { protocol });
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
                info,
            }) => self.pending_events.push_back(Event::InboundStream {
                inbound_id: info,
                stream,
            }),

            ConnectionEvent::FullyNegotiatedOutbound(FullyNegotiatedOutbound {
                protocol: stream,
                info: outbound_id,
            }) => self.pending_events.push_back(Event::OutboundStream {
                outbound_id,
                stream,
            }),

            ConnectionEvent::DialUpgradeError(dial_upgrade_error) => {
                self.on_dial_upgrade_error(dial_upgrade_error)
            }

            ConnectionEvent::ListenUpgradeError(listen_upgrade_error) => {
                self.on_listen_upgrade_error(listen_upgrade_error)
            }

            ConnectionEvent::AddressChange(_) => {}
        }
    }
}
