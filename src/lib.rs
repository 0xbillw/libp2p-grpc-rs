pub mod adapter;
mod behaviour;
mod handler;

pub use behaviour::{Behaviour, Event};

pub const DEFAULT_PROTOCOL_NAME: &[u8] = b"/libp2p/grpc/1.0.0";

const EMPTY_QUEUE_SHRINK_THRESHOLD: usize = 100;
