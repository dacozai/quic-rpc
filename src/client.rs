//! Client side api
//!
//! The main entry point is [RpcClient].
use crate::{
    message::{BidiStreamingMsg, ClientStreamingMsg, RpcMsg, ServerStreamingMsg},
    transport::ConnectionErrors,
    Service, ServiceConnection,
};
use futures::{
    future::BoxFuture, stream::BoxStream, FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt,
};
use pin_project::pin_project;
use std::{
    error,
    fmt::{self, Debug},
    marker::PhantomData,
    pin::Pin,
    result,
    task::{Context, Poll},
};

/// A client for a specific service
///
/// This is a wrapper around a [ServiceConnection] that serves as the entry point
/// for the client DSL. `S` is the service type, `C` is the substream source.
#[derive(Debug)]
pub struct RpcClient<S, C> {
    source: C,
    p: PhantomData<S>,
}

impl<S, C: Clone> Clone for RpcClient<S, C> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            p: PhantomData,
        }
    }
}

/// Sink that can be used to send updates to the server for the two interaction patterns
/// that support it, [crate::message::ClientStreaming] and [crate::message::BidiStreaming].
#[pin_project]
#[derive(Debug)]
pub struct UpdateSink<S: Service, C: ServiceConnection<S>, T: Into<S::Req>>(
    #[pin] C::SendSink,
    PhantomData<T>,
);

impl<S: Service, C: ServiceConnection<S>, T: Into<S::Req>> Sink<T> for UpdateSink<S, C, T> {
    type Error = C::SendError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let req: S::Req = item.into();
        self.project().0.start_send_unpin(req)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_close_unpin(cx)
    }
}

/// Sink that can be used to send updates to the server for the two interaction patterns
/// that support it, [crate::message::ClientStreaming] and [crate::message::BidiStreaming].
#[pin_project]
#[derive(Debug)]
pub struct MappedUpdateSink<S2, S, C, T>(#[pin] C::SendSink, PhantomData<T>, PhantomData<S2>)
where
    S2: Service,
    S: Service,
    C: ServiceConnection<S>,
    T: Into<S2::Req>,
    S2::Req: Into<S::Req>;

impl<S2, S, C, T> Sink<T> for MappedUpdateSink<S2, S, C, T>
where
    S2: Service,
    S: Service,
    C: ServiceConnection<S>,
    T: Into<S2::Req>,
    S2::Req: Into<S::Req>,
{
    type Error = C::SendError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_ready_unpin(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let req: S2::Req = item.into();
        let req: S::Req = req.into();
        self.project().0.start_send_unpin(req)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_flush_unpin(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().0.poll_close_unpin(cx)
    }
}

impl<S: Service, C: ServiceConnection<S>> RpcClient<S, C> {
    /// Create a new rpc client for a specific [Service] given a compatible
    /// [ServiceConnection].
    ///
    /// This is where a generic typed connection is converted into a client for a specific service.
    pub fn new(source: C) -> Self {
        Self {
            source,
            p: PhantomData,
        }
    }

    /// Get the underlying connection
    pub fn into_inner(self) -> C {
        self.source
    }

    /// RPC call to the server, single request, single response
    pub async fn rpc_mapped<S2, M>(&self, msg: M) -> result::Result<M::Response, RpcClientError<C>>
    where
        M: RpcMsg<S2>,
        S2: Service,
        S2::Req: Into<S::Req> + TryFrom<S::Req> + Send + 'static,
        S2::Res: Into<S::Res> + TryFrom<S::Res> + Send + 'static,
    {
        let msg: <S2 as Service>::Req = msg.into();
        let msg: <S as Service>::Req = msg.into();
        let (mut send, mut recv) = self.source.open_bi().await.map_err(RpcClientError::Open)?;
        send.send(msg).await.map_err(RpcClientError::<C>::Send)?;
        let res = recv
            .next()
            .await
            .ok_or(RpcClientError::<C>::EarlyClose)?
            .map_err(RpcClientError::<C>::RecvError)?;
        // keep send alive until we have the answer
        drop(send);
        let res = S2::Res::try_from(res).map_err(|_| RpcClientError::DowncastError)?;
        M::Response::try_from(res).map_err(|_| RpcClientError::DowncastError)
    }

    /// RPC call to the server, single request, single response
    pub async fn rpc<M>(&self, msg: M) -> result::Result<M::Response, RpcClientError<C>>
    where
        M: RpcMsg<S>,
    {
        self.rpc_mapped::<S, _>(msg).await
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn server_streaming_mapped<S2, M>(
        &self,
        msg: M,
    ) -> result::Result<
        BoxStream<'static, result::Result<M::Response, StreamingResponseItemError<C>>>,
        StreamingResponseError<C>,
    >
    where
        M: ServerStreamingMsg<S2>,
        S2: Service,
        S2::Req: Into<S::Req> + TryFrom<S::Req> + Send + 'static,
        S2::Res: Into<S::Res> + TryFrom<S::Res> + Send + 'static,
    {
        let msg: S2::Req = msg.into();
        let msg: S::Req = msg.into();
        let (mut send, recv) = self
            .source
            .open_bi()
            .await
            .map_err(StreamingResponseError::Open)?;
        send.send(msg)
            .map_err(StreamingResponseError::<C>::Send)
            .await?;
        let recv = recv.map(move |x| match x {
            Ok(x) => {
                let x =
                    S2::Res::try_from(x).map_err(|_| StreamingResponseItemError::DowncastError)?;
                M::Response::try_from(x).map_err(|_| StreamingResponseItemError::DowncastError)
            }
            Err(e) => Err(StreamingResponseItemError::RecvError(e)),
        });
        // keep send alive so the request on the server side does not get cancelled
        let recv = DeferDrop(recv, send).boxed();
        Ok(recv)
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn server_streaming<M>(
        &self,
        msg: M,
    ) -> result::Result<
        BoxStream<'static, result::Result<M::Response, StreamingResponseItemError<C>>>,
        StreamingResponseError<C>,
    >
    where
        M: ServerStreamingMsg<S>,
    {
        self.server_streaming_mapped::<S, _>(msg).await
    }

    /// Call to the server that allows the client to stream, single response
    pub async fn client_streaming_mapped<S2, M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            MappedUpdateSink<S2, S, C, M::Update>,
            BoxFuture<'static, result::Result<M::Response, ClientStreamingItemError<C>>>,
        ),
        ClientStreamingError<C>,
    >
    where
        M: ClientStreamingMsg<S2>,
        S2: Service,
        S2::Req: Into<S::Req> + TryFrom<S::Req> + Send + 'static,
        S2::Res: Into<S::Res> + TryFrom<S::Res> + Send + 'static,
    {
        let msg: S2::Req = msg.into();
        let msg: S::Req = msg.into();
        let (mut send, mut recv) = self
            .source
            .open_bi()
            .await
            .map_err(ClientStreamingError::Open)?;
        send.send(msg).map_err(ClientStreamingError::Send).await?;
        let send = MappedUpdateSink::<S2, S, C, M::Update>(send, PhantomData, PhantomData);
        let recv = async move {
            let item = recv
                .next()
                .await
                .ok_or(ClientStreamingItemError::EarlyClose)?;

            match item {
                Ok(x) => {
                    let x = S2::Res::try_from(x)
                        .map_err(|_| ClientStreamingItemError::DowncastError)?;
                    M::Response::try_from(x).map_err(|_| ClientStreamingItemError::DowncastError)
                }
                Err(e) => Err(ClientStreamingItemError::RecvError(e)),
            }
        }
        .boxed();
        Ok((send, recv))
    }

    /// Call to the server that allows the client to stream, single response
    pub async fn client_streaming<M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            MappedUpdateSink<S, S, C, M::Update>,
            BoxFuture<'static, result::Result<M::Response, ClientStreamingItemError<C>>>,
        ),
        ClientStreamingError<C>,
    >
    where
        M: ClientStreamingMsg<S>,
    {
        self.client_streaming_mapped::<S, _>(msg).await
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn bidi_mapped<S2, M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            MappedUpdateSink<S2, S, C, M::Update>,
            BoxStream<'static, result::Result<M::Response, BidiItemError<C>>>,
        ),
        BidiError<C>,
    >
    where
        M: BidiStreamingMsg<S2>,
        S2: Service,
        S2::Req: Into<S::Req> + TryFrom<S::Req> + Send + 'static,
        S2::Res: Into<S::Res> + TryFrom<S::Res> + Send + 'static,
    {
        let msg: S2::Req = msg.into();
        let msg: S::Req = msg.into();
        let (mut send, recv) = self.source.open_bi().await.map_err(BidiError::Open)?;
        send.send(msg).await.map_err(BidiError::<C>::Send)?;
        let send = MappedUpdateSink(send, PhantomData, PhantomData);
        let recv = recv
            .map(|x| match x {
                Ok(x) => {
                    let x = S2::Res::try_from(x).map_err(|_| BidiItemError::DowncastError)?;
                    M::Response::try_from(x).map_err(|_| BidiItemError::DowncastError)
                }
                Err(e) => Err(BidiItemError::RecvError(e)),
            })
            .boxed();
        Ok((send, recv))
    }

    /// Bidi call to the server, request opens a stream, response is a stream
    pub async fn bidi<M>(
        &self,
        msg: M,
    ) -> result::Result<
        (
            MappedUpdateSink<S, S, C, M::Update>,
            BoxStream<'static, result::Result<M::Response, BidiItemError<C>>>,
        ),
        BidiError<C>,
    >
    where
        M: BidiStreamingMsg<S>,
    {
        self.bidi_mapped::<S, _>(msg).await
    }
}

impl<S: Service, C: ServiceConnection<S>> AsRef<C> for RpcClient<S, C> {
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
