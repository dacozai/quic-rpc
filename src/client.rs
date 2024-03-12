//! Client side api
//!
//! The main entry point is [RpcClient].
use crate::{
    map::{ChainedMapper, MapService, Mapper},
    message::{BidiStreamingMsg, ClientStreamingMsg, RpcMsg, ServerStreamingMsg},
    transport::ConnectionErrors,
    Service, ServiceConnection,
};
use futures::{future::BoxFuture, FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use pin_project::pin_project;
use std::{
    error,
    fmt::{self, Debug},
    marker::PhantomData,
    pin::Pin,
    result,
    sync::Arc,
    task::{Context, Poll},
};

/// Sync version of `future::stream::BoxStream`.
pub type BoxStreamSync<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + Sync + 'a>>;

/// A client for a specific service
///
/// This is a wrapper around a [ServiceConnection] that serves as the entry point
/// for the client DSL. `S` is the service type, `C` is the substream source.
#[derive(Debug)]
pub struct RpcClient<S, C, SInner = S> {
    source: C,
    map: Arc<dyn MapService<S, SInner>>,
}

impl<S, C: Clone, SInner> Clone for RpcClient<S, C, SInner> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            map: Arc::clone(&self.map),
        }
    }
}

/// Sink that can be used to send updates to the server for the two interaction patterns
/// that support it, [crate::message::ClientStreaming] and [crate::message::BidiStreaming].
#[pin_project]
#[derive(Debug)]
pub struct UpdateSink<S, C, T, SInner = S>(
    #[pin] C::SendSink,
    PhantomData<T>,
    Arc<dyn MapService<S, SInner>>,
)
where
    S: Service,
    SInner: Service,
    C: ServiceConnection<S>,
    T: Into<SInner::Req>;

impl<S, C, T, SInner> Sink<T> for UpdateSink<S, C, T, SInner>
where
    S: Service,
    SInner: Service,
    C: ServiceConnection<S>,
    T: Into<SInner::Req>,
{
    type Error = C::SendError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let req = self.2.req_into_outer(item.into());
        self.project().0.start_send_unpin(req)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_close_unpin(cx)
    }
}

impl<S, C> RpcClient<S, C, S>
where
    S: Service,
    C: ServiceConnection<S>,
{
    /// Create a new rpc client for a specific [Service] given a compatible
    /// [ServiceConnection].
    ///
    /// This is where a generic typed connection is converted into a client for a specific service.
    pub fn new(source: C) -> Self {
        Self {
            source,
            map: Arc::new(Mapper::new()),
        }
    }
}

impl<S, C, SInner> RpcClient<S, C, SInner>
where
    S: Service,
    C: ServiceConnection<S>,
    SInner: Service,
{
    /// Get the underlying connection
    pub fn into_inner(self) -> C {
        self.source
    }

    /// Map this channel's service into an inner service.
    ///
    /// This method is available if the required bounds are upheld:
    /// SNext::Req: Into<SInner::Req> + TryFrom<SInner::Req>,
    /// SNext::Res: Into<SInner::Res> + TryFrom<SInner::Res>,
    ///
    /// Where SNext is the new service to map to and SInner is the current inner service.
    ///
    /// This method can be chained infintely.
    pub fn map<SNext>(self) -> RpcClient<S, C, SNext>
    where
        SNext: Service,
        SNext::Req: Into<SInner::Req> + TryFrom<SInner::Req>,
        SNext::Res: Into<SInner::Res> + TryFrom<SInner::Res>,
    {
        let map = ChainedMapper::new(self.map);
        RpcClient {
            source: self.source,
            map: Arc::new(map),
        }
    }

    /// RPC call to the server, single request, single response
    pub async fn rpc<M>(&self, msg: M) -> result::Result<M::Response, RpcClientError<C>>
    where
        M: RpcMsg<SInner>,
    {
        let msg = self.map.req_into_outer(msg.into());
        let (mut send, mut recv) = self.source.open_bi().await.map_err(RpcClientError::Open)?;
        send.send(msg).await.map_err(RpcClientError::<C>::Send)?;
        let res = recv
            .next()
            .await
            .ok_or(RpcClientError::<C>::EarlyClose)?
            .map_err(RpcClientError::<C>::RecvError)?;
        // keep send alive until we have the answer
        drop(send);
        let res = self
            .map
            .res_try_into_inner(res)
            .map_err(|_| RpcClientError::DowncastError)?;
        M::Response::try_from(res).map_err(|_| RpcClientError::DowncastError)
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn server_streaming<M>(
        &self,
        msg: M,
    ) -> result::Result<
        BoxStreamSync<'static, result::Result<M::Response, StreamingResponseItemError<C>>>,
        StreamingResponseError<C>,
    >
    where
        M: ServerStreamingMsg<SInner>,
    {
        let msg = self.map.req_into_outer(msg.into());
        let (mut send, recv) = self
            .source
            .open_bi()
            .await
            .map_err(StreamingResponseError::Open)?;
        send.send(msg)
            .map_err(StreamingResponseError::<C>::Send)
            .await?;
        let map = Arc::clone(&self.map);
        let recv = recv.map(move |x| match x {
            Ok(x) => {
                let x = map
                    .res_try_into_inner(x)
                    .map_err(|_| StreamingResponseItemError::DowncastError)?;
                M::Response::try_from(x).map_err(|_| StreamingResponseItemError::DowncastError)
            }
            Err(e) => Err(StreamingResponseItemError::RecvError(e)),
        });
        // keep send alive so the request on the server side does not get cancelled
        let recv = Box::pin(DeferDrop(recv, send));
        Ok(recv)
    }

    /// Call to the server that allows the client to stream, single response
    pub async fn client_streaming<M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            UpdateSink<S, C, M::Update, SInner>,
            BoxFuture<'static, result::Result<M::Response, ClientStreamingItemError<C>>>,
        ),
        ClientStreamingError<C>,
    >
    where
        M: ClientStreamingMsg<SInner>,
    {
        let msg = self.map.req_into_outer(msg.into());
        let (mut send, mut recv) = self
            .source
            .open_bi()
            .await
            .map_err(ClientStreamingError::Open)?;
        send.send(msg).map_err(ClientStreamingError::Send).await?;
        let send = UpdateSink::<S, C, M::Update, SInner>(send, PhantomData, Arc::clone(&self.map));
        let map = Arc::clone(&self.map);
        let recv = async move {
            let item = recv
                .next()
                .await
                .ok_or(ClientStreamingItemError::EarlyClose)?;

            match item {
                Ok(x) => {
                    let x = map
                        .res_try_into_inner(x)
                        .map_err(|_| ClientStreamingItemError::DowncastError)?;
                    M::Response::try_from(x).map_err(|_| ClientStreamingItemError::DowncastError)
                }
                Err(e) => Err(ClientStreamingItemError::RecvError(e)),
            }
        }
        .boxed();
        Ok((send, recv))
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn bidi<M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            UpdateSink<S, C, M::Update, SInner>,
            BoxStreamSync<'static, result::Result<M::Response, BidiItemError<C>>>,
        ),
        BidiError<C>,
    >
    where
        M: BidiStreamingMsg<SInner>,
    {
        let msg = self.map.req_into_outer(msg.into());
        let (mut send, recv) = self.source.open_bi().await.map_err(BidiError::Open)?;
        send.send(msg).await.map_err(BidiError::<C>::Send)?;
        let send = UpdateSink(send, PhantomData, Arc::clone(&self.map));
        let map = Arc::clone(&self.map);
        let recv = Box::pin(recv.map(move |x| match x {
            Ok(x) => {
                let x = map
                    .res_try_into_inner(x)
                    .map_err(|_| BidiItemError::DowncastError)?;
                M::Response::try_from(x).map_err(|_| BidiItemError::DowncastError)
            }
            Err(e) => Err(BidiItemError::RecvError(e)),
        }));
        Ok((send, recv))
    }
}

impl<S, C, SInner> AsRef<C> for RpcClient<S, C, SInner>
where
    S: Service,
    C: ServiceConnection<S>,
    SInner: Service,
{
    fn as_ref(&self) -> &C {
        &self.source
    }
}

/// Client error. All client DSL methods return a `Result` with this error type.
#[derive(Debug)]
pub enum RpcClientError<C: ConnectionErrors> {
    /// Unable to open a substream at all
    Open(C::OpenError),
    /// Unable to send the request to the server
    Send(C::SendError),
    /// Server closed the stream before sending a response
    EarlyClose,
    /// Unable to receive the response from the server
    RecvError(C::RecvError),
    /// Unexpected response from the server
    DowncastError,
}

impl<C: ConnectionErrors> fmt::Display for RpcClientError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<C: ConnectionErrors> error::Error for RpcClientError<C> {}

/// Server error when accepting a bidi request
#[derive(Debug)]
pub enum BidiError<C: ConnectionErrors> {
    /// Unable to open a substream at all
    Open(C::OpenError),
    /// Unable to send the request to the server
    Send(C::SendError),
}

impl<C: ConnectionErrors> fmt::Display for BidiError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<C: ConnectionErrors> error::Error for BidiError<C> {}

/// Server error when receiving an item for a bidi request
#[derive(Debug)]
pub enum BidiItemError<C: ConnectionErrors> {
    /// Unable to receive the response from the server
    RecvError(C::RecvError),
    /// Unexpected response from the server
    DowncastError,
}

impl<C: ConnectionErrors> fmt::Display for BidiItemError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<C: ConnectionErrors> error::Error for BidiItemError<C> {}

/// Server error when accepting a client streaming request
#[derive(Debug)]
pub enum ClientStreamingError<C: ConnectionErrors> {
    /// Unable to open a substream at all
    Open(C::OpenError),
    /// Unable to send the request to the server
    Send(C::SendError),
}

impl<C: ConnectionErrors> fmt::Display for ClientStreamingError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<C: ConnectionErrors> error::Error for ClientStreamingError<C> {}

/// Server error when receiving an item for a client streaming request
#[derive(Debug)]
pub enum ClientStreamingItemError<C: ConnectionErrors> {
    /// Connection was closed before receiving the first message
    EarlyClose,
    /// Unable to receive the response from the server
    RecvError(C::RecvError),
    /// Unexpected response from the server
    DowncastError,
}

impl<C: ConnectionErrors> fmt::Display for ClientStreamingItemError<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<C: ConnectionErrors> error::Error for ClientStreamingItemError<C> {}

/// Server error when accepting a server streaming request
#[derive(Debug)]
pub enum StreamingResponseError<C: ConnectionErrors> {
    /// Unable to open a substream at all
    Open(C::OpenError),
    /// Unable to send the request to the server
    Send(C::SendError),
}

impl<S: ConnectionErrors> fmt::Display for StreamingResponseError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<S: ConnectionErrors> error::Error for StreamingResponseError<S> {}

/// Client error when handling responses from a server streaming request
#[derive(Debug)]
pub enum StreamingResponseItemError<S: ConnectionErrors> {
    /// Unable to receive the response from the server
    RecvError(S::RecvError),
    /// Unexpected response from the server
    DowncastError,
}

impl<S: ConnectionErrors> fmt::Display for StreamingResponseItemError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<S: ConnectionErrors> error::Error for StreamingResponseItemError<S> {}

/// Wrap a stream with an additional item that is kept alive until the stream is dropped
#[pin_project]
struct DeferDrop<S: Stream, X>(#[pin] S, X);

impl<S: Stream, X> Stream for DeferDrop<S, X> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().0.poll_next(cx)
    }
}
