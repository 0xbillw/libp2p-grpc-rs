pub mod adapter;
mod behaviour;
mod handler;
mod protocol;

pub use behaviour::{Behaviour, Event, GrpcOutbound, OutboundError};
pub(crate) use protocol::{InboundId, InboundIdGen, OutboundId, OutboundRequest};

const EMPTY_QUEUE_SHRINK_THRESHOLD: usize = 100;
