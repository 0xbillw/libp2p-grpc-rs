use anyhow::{anyhow, Result};
use clap::Parser;
use examples::nodeinfo::pb::{node_service_client::NodeServiceClient, NodeInfoRequest};
use futures::{channel::mpsc, future::BoxFuture, prelude::*, stream::FuturesUnordered};
use libp2p::{
    core::upgrade::Version,
    identity,
    multiaddr::Protocol,
    noise,
    swarm::SwarmBuilder,
    tcp, yamux, Multiaddr, PeerId, Swarm, Transport,
};
use libp2p_grpc_rs::Behaviour as GrpcBehaviour;
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
        GrpcBehaviour::new(local_peer_id.clone(), None),
        local_peer_id,
    )
    .build();

    let (el, mut command_tx) = EventLoop::new(swarm);
    let jh = tokio::spawn(el.run());
    for i in 0..2 {
        tracing::debug!("--> round {}", i);
        let _ = command_tx
            .send(Command::GetRemoteInfo {
                peer_id: remote_peer_id,
                address: args.remote.clone(),
            })
            .await;
    }
    let _ = jh.await;
    Ok(())
}

#[derive(Debug)]
enum Command {
    GetRemoteInfo { peer_id: PeerId, address: Multiaddr },
}

struct EventLoop {
    swarm: Swarm<GrpcBehaviour>,
    command_rx: mpsc::Receiver<Command>,
    command_exec_futs: FuturesUnordered<BoxFuture<'static, ()>>,
}

impl EventLoop {
    pub fn new(swarm: Swarm<GrpcBehaviour>) -> (Self, mpsc::Sender<Command>) {
        let (command_tx, command_rx) = mpsc::channel(3);
        let el = EventLoop {
            swarm,
            command_rx,
            command_exec_futs: FuturesUnordered::new(),
        };
        (el, command_tx)
    }

    pub async fn run(mut self) {
        loop {
            futures::select! {
                evt = self.swarm.select_next_some() => {                    
                    tracing::debug!(swarm_event=?evt, "catch SwarmEvent")
                },
                opt = self.command_rx.next() => match opt {
                    Some(cmd) => self.handle_command(cmd),
                    None => break,
                },
                _ = self.command_exec_futs.next() => {}
            }
        }
        tracing::debug!("event loop exit");
    }

    fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::GetRemoteInfo { peer_id, address } => {
                tracing::debug!("begin create channel, address: {:?}", address);
                self.swarm
                    .behaviour_mut()
                    .add_address(&peer_id, address.clone());

                let result = self.swarm.behaviour_mut().initiate_grpc_outbound(&peer_id);
                let fut = async move {
                    let (ch, outbound_id) = match result.map().await {
                        Ok((ch, outbound_id)) => (ch, outbound_id),
                        Err(e) => {
                            tracing::error!("gRPC outbound error: {:?}", e);
                            return
                        }                        
                    };
                    let mut client = NodeServiceClient::new(ch);
                        for i in 0..3 {
                            let resp = client.info(Request::new(NodeInfoRequest {})).await.unwrap();
                            tracing::debug!(index=%i, response=?resp, outbound_id=%outbound_id, "got response");
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        }
                    
                }.boxed();
                self.command_exec_futs.push(fut);

            }
        }
    }
}
