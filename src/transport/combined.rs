//! Transport that combines two other transports
use super::{Connection, ConnectionCommon, ConnectionErrors, LocalAddr, ServerEndpoint};
use futures_lite::Stream;
use futures_sink::Sink;
use pin_project::pin_project;
use std::{
    error, fmt,
    fmt::Debug,
    pin::Pin,
    task::{Context, Poll},
};

/// A connection that combines two other connections
pub struct CombinedConnection<A, B> {
    /// First connection
    pub a: Option<A>,
    /// Second connection
    pub b: Option<B>,
}

impl<A: Connection, B: Connection<In = A::In, Out = A::Out>> CombinedConnection<A, B> {
    /// Create a combined connection from two other connections
    ///
    /// It will always use the first connection that is not `None`.
    pub fn new(a: Option<A>, b: Option<B>) -> Self {
        Self { a, b }
    }
}
impl<A: Clone, B: Clone> Clone for CombinedConnection<A, B> {
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}

impl<A: Debug, B: Debug> Debug for CombinedConnection<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CombinedConnection")
            .field("a", &self.a)
            .field("b", &self.b)
            .finish()
    }
}

/// An endpoint that combines two other endpoints
pub struct CombinedServerEndpoint<A, B> {
    /// First endpoint
    pub a: Option<A>,
    /// Second endpoint
    pub b: Option<B>,
    /// Local addresses from all endpoints
    local_addr: Vec<LocalAddr>,
}

impl<A: ServerEndpoint, B: ServerEndpoint<In = A::In, Out = A::Out>> CombinedServerEndpoint<A, B> {
    /// Create a combined server endpoint from two other server endpoints
    ///
    /// When listening for incoming connections with
    /// [crate::ServerEndpoint::accept], all configured channels will be listened on,
    /// and the first to receive a connection will be used. If no channels are configured,
    /// accept_bi will not throw an error but wait forever.
    pub fn new(a: Option<A>, b: Option<B>) -> Self {
        let mut local_addr = Vec::with_capacity(2);
        if let Some(a) = &a {
            local_addr.extend(a.local_addr().iter().cloned())
        };
        if let Some(b) = &b {
            local_addr.extend(b.local_addr().iter().cloned())
        };
        Self { a, b, local_addr }
    }

    /// Get back the inner endpoints
    pub fn into_inner(self) -> (Option<A>, Option<B>) {
        (self.a, self.b)
    }
}

impl<A: Clone, B: Clone> Clone for CombinedServerEndpoint<A, B> {
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
            local_addr: self.local_addr.clone(),
        }
    }
}

impl<A: Debug, B: Debug> Debug for CombinedServerEndpoint<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CombinedServerEndpoint")
            .field("a", &self.a)
            .field("b", &self.b)
            .finish()
    }
}

/// Send sink for combined channels
#[pin_project(project = SendSinkProj)]
pub enum SendSink<A: ConnectionCommon, B: ConnectionCommon> {
    /// A variant
    A(#[pin] A::SendSink),
    /// B variant
    B(#[pin] B::SendSink),
}

impl<A: ConnectionCommon, B: ConnectionCommon<In = A::In, Out = A::Out>> Sink<A::Out>
    for SendSink<A, B>
{
    type Error = self::SendError<A, B>;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.project() {
            SendSinkProj::A(sink) => sink.poll_ready(cx).map_err(Self::Error::A),
            SendSinkProj::B(sink) => sink.poll_ready(cx).map_err(Self::Error::B),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: A::Out) -> Result<(), Self::Error> {
        match self.project() {
            SendSinkProj::A(sink) => sink.start_send(item).map_err(Self::Error::A),
            SendSinkProj::B(sink) => sink.start_send(item).map_err(Self::Error::B),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.project() {
            SendSinkProj::A(sink) => sink.poll_flush(cx).map_err(Self::Error::A),
            SendSinkProj::B(sink) => sink.poll_flush(cx).map_err(Self::Error::B),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.project() {
            SendSinkProj::A(sink) => sink.poll_close(cx).map_err(Self::Error::A),
            SendSinkProj::B(sink) => sink.poll_close(cx).map_err(Self::Error::B),
        }
    }
}

/// RecvStream for combined channels
#[pin_project(project = ResStreamProj)]
pub enum RecvStream<A: ConnectionCommon, B: ConnectionCommon> {
    /// A variant
    A(#[pin] A::RecvStream),
    /// B variant
    B(#[pin] B::RecvStream),
}

impl<A: ConnectionCommon, B: ConnectionCommon<In = A::In, Out = A::Out>> Stream
    for RecvStream<A, B>
{
    type Item = Result<A::In, RecvError<A, B>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.project() {
            ResStreamProj::A(stream) => stream.poll_next(cx).map_err(RecvError::<A, B>::A),
            ResStreamProj::B(stream) => stream.poll_next(cx).map_err(RecvError::<A, B>::B),
        }
    }
}

/// SendError for combined channels
#[derive(Debug)]
pub enum SendError<A: ConnectionErrors, B: ConnectionErrors> {
    /// A variant
    A(A::SendError),
    /// B variant
    B(B::SendError),
}

impl<A: ConnectionErrors, B: ConnectionErrors> fmt::Display for SendError<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<A: ConnectionErrors, B: ConnectionErrors> error::Error for SendError<A, B> {}

/// RecvError for combined channels
#[derive(Debug)]
pub enum RecvError<A: ConnectionErrors, B: ConnectionErrors> {
    /// A variant
    A(A::RecvError),
    /// B variant
    B(B::RecvError),
}

impl<A: ConnectionErrors, B: ConnectionErrors> fmt::Display for RecvError<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<A: ConnectionErrors, B: ConnectionErrors> error::Error for RecvError<A, B> {}

/// OpenBiError for combined channels
#[derive(Debug)]
pub enum OpenBiError<A: ConnectionErrors, B: ConnectionErrors> {
    /// A variant
    A(A::OpenError),
    /// B variant
    B(B::OpenError),
    /// None of the two channels is configured
    NoChannel,
}

impl<A: ConnectionErrors, B: ConnectionErrors> fmt::Display for OpenBiError<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<A: ConnectionErrors, B: ConnectionErrors> error::Error for OpenBiError<A, B> {}

/// AcceptBiError for combined channels
#[derive(Debug)]
pub enum AcceptBiError<A: ConnectionErrors, B: ConnectionErrors> {
    /// A variant
    A(A::OpenError),
    /// B variant
    B(B::OpenError),
}

impl<A: ConnectionErrors, B: ConnectionErrors> fmt::Display for AcceptBiError<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl<A: ConnectionErrors, B: ConnectionErrors> error::Error for AcceptBiError<A, B> {}

impl<A: ConnectionErrors, B: ConnectionErrors> ConnectionErrors for CombinedConnection<A, B> {
    type SendError = self::SendError<A, B>;
    type RecvError = self::RecvError<A, B>;
    type OpenError = self::OpenBiError<A, B>;
}

impl<A: Connection, B: Connection<In = A::In, Out = A::Out>> ConnectionCommon
    for CombinedConnection<A, B>
{
    type In = A::In;
    type Out = A::Out;
    type RecvStream = self::RecvStream<A, B>;
    type SendSink = self::SendSink<A, B>;
}

impl<A: Connection, B: Connection<In = A::In, Out = A::Out>> Connection
    for CombinedConnection<A, B>
{
    async fn open(&self) -> Result<(Self::SendSink, Self::RecvStream), Self::OpenError> {
        let this = self.clone();
        // try a first, then b
        if let Some(a) = this.a {
            let (send, recv) = a.open().await.map_err(OpenBiError::A)?;
            Ok((SendSink::A(send), RecvStream::A(recv)))
        } else if let Some(b) = this.b {
            let (send, recv) = b.open().await.map_err(OpenBiError::B)?;
            Ok((SendSink::B(send), RecvStream::B(recv)))
        } else {
            Err(OpenBiError::NoChannel)
        }
    }
}

impl<A: ConnectionErrors, B: ConnectionErrors> ConnectionErrors for CombinedServerEndpoint<A, B> {
    type SendError = self::SendError<A, B>;
    type RecvError = self::RecvError<A, B>;
    type OpenError = self::AcceptBiError<A, B>;
}

impl<A: ServerEndpoint, B: ServerEndpoint<In = A::In, Out = A::Out>> ConnectionCommon
    for CombinedServerEndpoint<A, B>
{
    type In = A::In;
    type Out = A::Out;
    type RecvStream = self::RecvStream<A, B>;
    type SendSink = self::SendSink<A, B>;
}

impl<A: ServerEndpoint, B: ServerEndpoint<In = A::In, Out = A::Out>> ServerEndpoint
    for CombinedServerEndpoint<A, B>
{
    async fn accept(&self) -> Result<(Self::SendSink, Self::RecvStream), Self::OpenError> {
        let a_fut = async {
            if let Some(a) = &self.a {
                let (send, recv) = a.accept().await.map_err(AcceptBiError::A)?;
                Ok((SendSink::A(send), RecvStream::A(recv)))
            } else {
                std::future::pending().await
            }
        };
        let b_fut = async {
            if let Some(b) = &self.b {
                let (send, recv) = b.accept().await.map_err(AcceptBiError::B)?;
                Ok((SendSink::B(send), RecvStream::B(recv)))
            } else {
                std::future::pending().await
            }
        };
        async move {
            tokio::select! {
                res = a_fut => res,
                res = b_fut => res,
            }
        }
        .await
    }

    fn local_addr(&self) -> &[LocalAddr] {
        &self.local_addr
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        transport::{
            combined::{self, OpenBiError},
            flume,
        },
        Connection,
    };

    #[tokio::test]
    async fn open_empty_channel() {
        let channel = combined::CombinedConnection::<
            flume::FlumeConnection<(), ()>,
            flume::FlumeConnection<(), ()>,
        >::new(None, None);
        let res = channel.open().await;
        assert!(matches!(res, Err(OpenBiError::NoChannel)));
    }
}
