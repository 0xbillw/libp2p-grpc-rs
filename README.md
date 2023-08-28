# libp2p-grpc-rs

A library written in Rust language for running gRPC on LibP2P base on [rust-libp2p](https://github.com/libp2p/rust-libp2p) and [tonic](https://github.com/hyperium/tonic) crates. Inspired by [go-libp2p-grpc](https://github.com/drgomesp/go-libp2p-grpc)

## Usage

> For a more example, check the **[examples/](https://github.com/0xbillw/libp2p-grpc-rs/tree/main/examples)** folder.

Given an RPC service:

```proto
service NodeService {
  // Echo asks a node infomation to respond with a message.
  rpc Info(NodeInfoRequest) returns (NodeInfoResponse) {}
}
```

Required dependencies in your Cargo.toml:

```toml
[dependencies]
tonic = <tonic-version>
prost = <prost-version>
libp2p-grpc-rs = <libp2p-grpc-rs-version>

[build-dependencies]
tonic-build = <tonic-version>
```

For the server side, implement the service (see tonic's [tutorial](https://github.com/hyperium/tonic/tree/master/examples)), then build libp2p Swarm and start listening.

```rust
let transport = tcp::tokio::Transport::default()
    .upgrade(Version::V1Lazy)
    .authenticate(noise::Config::new(&local_key)?)
    .multiplex(yamux::Config::default())
    .boxed();

// create tonic gRPC router for services
let router = Server::builder().add_service(nodeinfo::new(local_peer_id.to_base58()));

let mut swarm = SwarmBuilder::with_tokio_executor(
    transport,
    GrpcBehaviour::new(local_peer_id.clone(), Some(router)),
    local_peer_id,
)
.build();

let listener_port = args.listener_port.unwrap_or(0);
swarm.listen_on(format!("/ip4/127.0.0.1/tcp/{}", listener_port).parse()?)?;
...

```

For the client side, the Behaviour of this crate is introduced to build Swarm of libp2p. 

```rust
use libp2p_grpc_rs::Behaviour as GrpcBehaviour;
...
let swarm = SwarmBuilder::with_tokio_executor(
    transport,
    GrpcBehaviour::new(local_peer_id.clone(), None),
    local_peer_id,
)
.build();
```

and the initiate_grpc_outbound(target_peer_id) method of behaviour is called to get the Channel of tonic.

```rust
let grpc_outbound_result = self.swarm
    .behaviour_mut()
    .initiate_grpc_outbound(&target_peer_id);
let channel = match grpc_outbound_result.map().await {
    Ok((channel, _)) => channel,
    Err(e) => {
        tracing::error!("gRPC outbound error: {:?}", e);
        return
    }                        
};
// build the client of the service
let mut client = NodeServiceClient::new(channel);
// call the service api
let resp = client.info(Request::new(NodeInfoRequest {})).await.unwrap();
tracing::debug!(response=?resp, "got response");
```

## TODO

- [ ] Documentation comments
- [ ] Unit or integration tests
- [ ] More examples of gRPC features
- [ ] Performance testing

## Contributing

PRs accepted.
