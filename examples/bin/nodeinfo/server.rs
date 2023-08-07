use clap::Parser;
use examples::nodeinfo::{self};
use futures::prelude::*;
use libp2p::core::upgrade::Version;
use libp2p::{
    identify, identity, noise, ping,
    swarm::{NetworkBehaviour, SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId, Transport,
};
use libp2p_grpc_rs::{Behaviour as GrpcBehaviour, Event as GrpcEvent};
use tonic::transport::Server;

#[derive(Parser, Debug, Clone)]
#[clap(name = "nodeinfo-server")]
pub struct Args {
    #[clap(long, short)]
    pub listener_port: Option<u16>,
    #[clap(long, short)]
    pub key_seed: Option<u8>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();
    let local_key = if let Some(seed) = args.key_seed {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        identity::Keypair::ed25519_from_bytes(bytes).unwrap()
    } else {
        identity::Keypair::generate_ed25519()
    };
    let local_peer_id = PeerId::from(local_key.public());
    tracing::info!("Local peer id: {}", local_peer_id);

    let transport = tcp::tokio::Transport::default()
        .upgrade(Version::V1Lazy)
        .authenticate(noise::Config::new(&local_key)?)
        .multiplex(yamux::Config::default())
        .boxed();

    let router = Server::builder().add_service(nodeinfo::new(local_peer_id.to_base58()));

    let mut swarm = SwarmBuilder::with_tokio_executor(
        transport,
        ComposedBehaviour {
            grpc: GrpcBehaviour::new(local_peer_id.clone(), Some(router)),
            identify: identify::Behaviour::new(identify::Config::new(
                "/ipfs/id/1.0.0".to_string(),
                local_key.public(),
            )),
            ping: ping::Behaviour::new(Default::default()),
        },
        local_peer_id,
    )
    .build();

    let listener_port = args.listener_port.unwrap_or(0);
    swarm.listen_on(format!("/ip4/127.0.0.1/tcp/{}", listener_port).parse()?)?;

    loop {
        match swarm.select_next_some().await {
            SwarmEvent::NewListenAddr { address, .. } => tracing::info!("Listening on {address:?}"),
            evt => {
                tracing::debug!(swarm_event = ?evt, "other swarm event");
            }
        }
    }
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "ComposedEvent")]
struct ComposedBehaviour {
    grpc: GrpcBehaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
}

#[derive(Debug)]
enum ComposedEvent {
    Grpc(GrpcEvent),
    Identify(identify::Event),
    Ping(ping::Event),
}

impl From<GrpcEvent> for ComposedEvent {
    fn from(event: GrpcEvent) -> Self {
        ComposedEvent::Grpc(event)
    }
}

impl From<identify::Event> for ComposedEvent {
    fn from(event: identify::Event) -> Self {
        ComposedEvent::Identify(event)
    }
}

impl From<ping::Event> for ComposedEvent {
    fn from(event: ping::Event) -> Self {
        ComposedEvent::Ping(event)
    }
}
