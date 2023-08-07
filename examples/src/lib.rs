pub mod nodeinfo {
    #[allow(non_snake_case)]
    pub mod pb {
        tonic::include_proto!("proto.v1");
    }

    pub use pb::{
        node_service_server::{NodeService, NodeServiceServer},
        NodeInfoRequest, NodeInfoResponse,
    };
    use tonic::{Request, Response, Status};

    #[derive(Default)]
    pub struct DefaultNodeInfoService {
        peer_id: String,
    }

    #[tonic::async_trait]
    impl NodeService for DefaultNodeInfoService {
        async fn info(
            &self,
            _request: Request<NodeInfoRequest>,
        ) -> std::result::Result<Response<NodeInfoResponse>, Status> {
            //TODO: to be implement
            let res = NodeInfoResponse {
                id: self.peer_id.clone(),
                addresses: vec!["sfdsaf".to_string(), "dsfdsaf".to_string()],
                peers: vec!["aaaaaa".to_string()],
                protocols: vec!["333333".to_string()],
            };
            Ok(Response::new(res))
        }
    }

    pub fn new(peer_id: String) -> NodeServiceServer<DefaultNodeInfoService> {
        NodeServiceServer::new(DefaultNodeInfoService { peer_id })
    }
}
