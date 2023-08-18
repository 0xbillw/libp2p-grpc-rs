use clap::Parser;
use examples::nodeinfo::{self};
use futures::prelude::*;
use libp2p::core::upgrade::Version;
use libp2p::{
    identity, noise,
    swarm::{SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId, Transport,
};
use libp2p_grpc_rs::Behaviour as GrpcBehaviour;
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
        GrpcBehaviour::new(local_peer_id.clone(), Some(router)),
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
