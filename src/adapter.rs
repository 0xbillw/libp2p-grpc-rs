use futures::{
    future::BoxFuture,
    io::{AsyncRead as FutsAsyncRead, AsyncWrite as FutsAsyncWrite},
    Stream,
};
use libp2p::{
    swarm::{ConnectionId, NegotiatedSubstream},
    PeerId,
};
use pin_project::pin_project;
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::mpsc,
};
use tonic::transport::{server::Connected, Endpoint as TonicEndpoint, Uri};
pub use tonic::transport::{Channel, Error as TonicTransportError};
use tower::{BoxError, Service};

#[pin_project]
#[derive(Debug)]
pub struct NegotiatedStreamWrapper {
    #[pin]
    inner: NegotiatedSubstream,
    peer_id: PeerId,
    connection_id: ConnectionId,
}

impl NegotiatedStreamWrapper {
    pub fn new(stream: NegotiatedSubstream, peer_id: PeerId, connection_id: ConnectionId) -> Self {
        Self {
            inner: stream,
            peer_id,
            connection_id,
        }
    }

    pub fn peer_id(&self) -> &PeerId {
        &self.peer_id
    }

    pub fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    pub fn into_inner(self) -> NegotiatedSubstream {
        self.inner
    }
}

impl Connected for NegotiatedStreamWrapper {
    type ConnectInfo = ();

    fn connect_info(&self) -> Self::ConnectInfo {}
}

impl AsRef<NegotiatedSubstream> for NegotiatedStreamWrapper {
    fn as_ref(&self) -> &NegotiatedSubstream {
        &self.inner
    }
}

impl AsMut<NegotiatedSubstream> for NegotiatedStreamWrapper {
    fn as_mut(&mut self) -> &mut NegotiatedSubstream {
        &mut self.inner
    }
}

impl AsyncRead for NegotiatedStreamWrapper {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let unfilled = buf.initialize_unfilled();
        let poll = self.project().inner.poll_read(cx, unfilled);
        if let Poll::Ready(Ok(num)) = &poll {
            buf.advance(*num);
        }
        poll.map_ok(|_| ())
    }
}

impl AsyncWrite for NegotiatedStreamWrapper {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        self.project().inner.poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        self.project().inner.poll_close(cx)
    }
}

#[pin_project]
pub struct StreamPumper {
    #[pin]
    stream_rx: mpsc::UnboundedReceiver<io::Result<NegotiatedStreamWrapper>>,
}

impl StreamPumper {
    pub fn new(stream_rx: mpsc::UnboundedReceiver<io::Result<NegotiatedStreamWrapper>>) -> Self {
        Self { stream_rx }
    }
}

impl Stream for StreamPumper {
    type Item = io::Result<NegotiatedStreamWrapper>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().stream_rx.as_mut().poll_recv(cx)
    }
}

#[pin_project]
struct OutboundStreamMaker {
    #[pin]
    stream_rx: Arc<Mutex<mpsc::Receiver<Result<NegotiatedStreamWrapper, BoxError>>>>,
}

impl OutboundStreamMaker {
    pub fn new(stream_rx: mpsc::Receiver<Result<NegotiatedStreamWrapper, BoxError>>) -> Self {
        Self {
            stream_rx: Arc::new(Mutex::new(stream_rx)),
        }
    }
}

#[pin_project]
struct NegotiatedStreamFuture {
    #[pin]
    stream_rx: Arc<Mutex<mpsc::Receiver<Result<NegotiatedStreamWrapper, BoxError>>>>,
}

impl core::future::Future for NegotiatedStreamFuture {
    type Output = Result<NegotiatedStreamWrapper, BoxError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = self.project();
        let r = me
            .stream_rx
            .lock()
            .unwrap()
            .poll_recv(cx)
            .map(|opt| opt.unwrap());

        return r;
    }
}

impl Service<Uri> for OutboundStreamMaker {
    type Error = BoxError;
    type Response = NegotiatedStreamWrapper;
    type Future = NegotiatedStreamFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, _: Uri) -> Self::Future {
        NegotiatedStreamFuture {
            stream_rx: self.stream_rx.clone(),
        }
    }
}

pub type OutboundGrpcChannelResult = Result<Channel, TonicTransportError>;

pub type GrpcChannelMakeFuture = BoxFuture<'static, OutboundGrpcChannelResult>;

pub async fn make_outbound_stream_to_grpc_channel(
    stream: NegotiatedStreamWrapper,
) -> OutboundGrpcChannelResult {
    let (tx, rx) = mpsc::channel(1);
    let mns = OutboundStreamMaker::new(rx);
    let fut = async move {
        // The URI here is not used and has no meaning for connect_with_connector()
        TonicEndpoint::try_from("http://[::]:50051")
            .unwrap()
            .connect_with_connector(mns)
            .await
    };
    let (ch_result, send_result) = tokio::join!(fut, tx.send(Ok(stream)));
    if let Err(send_err) = send_result {
        tracing::error!("send stream to channel maker error: {:?}", send_err);
    }
    ch_result
}
