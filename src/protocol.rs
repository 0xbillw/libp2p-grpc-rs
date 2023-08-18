use crate::{adapter, OutboundError};
use futures::{channel::oneshot, future};
use libp2p::{
    core::upgrade::{InboundUpgrade, OutboundUpgrade, UpgradeInfo},
    identity::PeerId,
    swarm::NegotiatedSubstream,
};
use std::{
    fmt, io, iter, ops,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

pub const DEFAULT_PROTOCOL_NAME: &[u8] = b"/libp2p/grpc/1.0.0";

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct OutboundId(u64);

impl OutboundId {
    pub fn new(n: u64) -> Self {
        Self(n)
    }
}

impl Default for OutboundId {
    fn default() -> Self {
        Self(0)
    }
}

impl fmt::Display for OutboundId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ops::AddAssign<u64> for OutboundId {
    fn add_assign(&mut self, rhs: u64) {
        self.0 += rhs
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct InboundId(u64);

impl InboundId {
    pub fn new(n: u64) -> Self {
        Self(n)
    }
}

impl Default for InboundId {
    fn default() -> Self {
        Self(0)
    }
}

impl fmt::Display for InboundId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl ops::AddAssign<u64> for InboundId {
    fn add_assign(&mut self, rhs: u64) {
        self.0 += rhs
    }
}

#[derive(Debug, Clone)]
pub struct InboundIdGen {
    n: Arc<AtomicU64>,
}

impl InboundIdGen {
    pub fn new(start: u64) -> Self {
        Self {
            n: Arc::new(AtomicU64::new(start)),
        }
    }
    pub fn next(&self) -> InboundId {
        InboundId::new(self.n.fetch_add(1, Ordering::SeqCst))
    }
}

impl Default for InboundIdGen {
    fn default() -> Self {
        Self::new(0)
    }
}

unsafe impl Send for InboundIdGen {}

#[derive(Debug)]
pub struct OutboundRequest {
    pub(crate) peer: PeerId,
    pub(crate) outbound_id: OutboundId,
    pub(crate) sender: oneshot::Sender<Result<adapter::Channel, OutboundError>>,
}

#[derive(Debug)]
pub struct DirectGrpcUpgradeProtocol {
    pub(crate) protocol_name: String,
}

impl UpgradeInfo for DirectGrpcUpgradeProtocol {
    type Info = String;
    type InfoIter = iter::Once<String>;

    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(self.protocol_name.clone())
    }
}

impl OutboundUpgrade<NegotiatedSubstream> for DirectGrpcUpgradeProtocol {
    type Output = NegotiatedSubstream;
    type Error = io::Error;
    type Future = future::Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_outbound(self, stream: NegotiatedSubstream, _: Self::Info) -> Self::Future {
        future::ready(Ok(stream))
    }
}

impl InboundUpgrade<NegotiatedSubstream> for DirectGrpcUpgradeProtocol {
    type Output = NegotiatedSubstream;
    type Error = io::Error;
    type Future = future::Ready<Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, stream: NegotiatedSubstream, _: Self::Info) -> Self::Future {
        future::ready(Ok(stream))
    }
}
