use anyhow::{anyhow, Result};
use clap::Parser;
use examples::nodeinfo::pb::{node_service_client::NodeServiceClient, NodeInfoRequest};
use futures::{channel::mpsc, prelude::*};
use libp2p::{
    core::upgrade::Version,
    identity,
    multiaddr::Protocol,
    noise, ping,
    swarm::{NetworkBehaviour, SwarmBuilder},
    tcp, yamux, Multiaddr, PeerId, Swarm, Transport,
};
use libp2p_grpc_rs::{Behaviour as GrpcBehaviour, Event as GrpcEvent};
use tonic::Request;

#[derive(Parser, Debug, Clone)]
#[clap(name = "nodeinfo-client")]
pub struct Args {
    #[clap(long, short)]
    pub listener_port: Option<u16>,
    #[clap(long, short)]
    pub key_seed: Option<u8>,
    #[clap(long, short)]
    pub remote: Multiaddr,
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "ComposedEvent")]
struct ComposedBehaviour {
    ping: ping::Behaviour,
    grpc: GrpcBehaviour,
}

#[derive(Debug)]
enum ComposedEvent {
    Ping(ping::Event),
    Grpc(GrpcEvent),
}

impl From<GrpcEvent> for ComposedEvent {
    fn from(event: GrpcEvent) -> Self {
        ComposedEvent::Grpc(event)
    }
}

impl From<ping::Event> for ComposedEvent {
    fn from(event: ping::Event) -> Self {
        ComposedEvent::Ping(event)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
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
    tracing::debug!("Local peer id: {}", local_peer_id);
    let remote_peer_id = if let Some(Protocol::P2p(mh)) = args.remote.iter().last() {
        PeerId::from_multihash(mh).map_err(|_| anyhow!("invalid multihash"))?
    } else {
        return Err(anyhow!("wrong P2P remote address"));
    };
    tracing::debug!("remote address: {:?}", args.remote);

    let transport = tcp::tokio::Transport::default()
        .upgrade(Version::V1Lazy)
        .authenticate(noise::Config::new(&local_key)?)
        .multiplex(yamux::Config::default())
        .boxed();

    let swarm = SwarmBuilder::with_tokio_executor(
        transport,
        ComposedBehaviour {
            ping: ping::Behaviour::new(Default::default()),
            grpc: GrpcBehaviour::new(local_peer_id.clone(), None),
        },
        local_peer_id,
    )
    .build();

    let (mut command_tx, command_rx) = mpsc::channel(3);
    let el = EventLoop { swarm, command_rx };
    let jh = tokio::spawn(el.run());
    let _ = command_tx
        .send(Command::GetRemoteInfo {
            peer_id: remote_peer_id,
            address: args.remote,
        })
        .await;
    let _ = jh.await;
    Ok(())
}

#[derive(Debug)]
enum Command {
    GetRemoteInfo { peer_id: PeerId, address: Multiaddr },
}

struct EventLoop {
    swarm: Swarm<ComposedBehaviour>,
    command_rx: mpsc::Receiver<Command>,
}

impl EventLoop {
    pub async fn run(mut self) {
        loop {
            futures::select! {
                evt = self.swarm.select_next_some() => {
                    tracing::debug!(event=?evt, "swarm event")
                },
                command = self.command_rx.next() => match command {
                    Some(c) => self.handle_command(c).await,
                    None =>  break,
                },
            }
        }
        tracing::debug!("event loop exit");
    }

    async fn handle_command(&mut self, cmd: Command) {
        tracing::debug!(cmd = ?cmd);
        match cmd {
            Command::GetRemoteInfo { peer_id, address } => {
                tracing::debug!("begin create channel, address: {:?}", address);
                self.swarm
                    .behaviour_mut()
                    .grpc
                    .add_address(&peer_id, address.clone());
                let _ = self.swarm.dial(address);
                // FIXME: the client can't outbound stream,
                // the log block on:
                // multistream_select::dialer_select: Dialer: Expecting proposed protocol: /yamux/1.0.0
                // yamux::connection: new connection: aabe2b2b (Client)
                let ch = self
                    .swarm
                    .behaviour_mut()
                    .grpc
                    .grpc_client_channel(peer_id)
                    .await
                    .unwrap();
                let mut client = NodeServiceClient::new(ch);
                let resp = client.info(Request::new(NodeInfoRequest {})).await.unwrap();
                tracing::debug!(response=?resp, "got response");
            }
        }
    }
}
