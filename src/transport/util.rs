use std::{
    pin::Pin,
    task::{self, Poll},
};

use futures_lite::Stream;
use futures_sink::Sink;
use pin_project::pin_project;
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::LengthDelimitedCodec;

#[pin_project]
pub struct FramedPostcardRead<T, In>(
    #[pin]
    tokio_serde::SymmetricallyFramed<
        tokio_util::codec::FramedRead<T, tokio_util::codec::LengthDelimitedCodec>,
        In,
        tokio_serde_postcard::SymmetricalPostcard<In>,
    >,
);

impl<T: AsyncRead, In: DeserializeOwned> FramedPostcardRead<T, In> {
    /// Wrap a socket in a length delimited codec and postcard encoding
    pub fn new(inner: T, max_frame_length: usize) -> Self {
        // configure length delimited codec with max frame length
        let framing = LengthDelimitedCodec::builder()
            .max_frame_length(max_frame_length)
            .new_codec();
        // create the actual framing. This turns the AsyncRead/AsyncWrite into a Stream/Sink of Bytes/BytesMut
        let framed = tokio_util::codec::FramedRead::new(inner, framing);
        let postcard = tokio_serde_postcard::Postcard::new();
        // create the actual framing. This turns the Stream/Sink of Bytes/BytesMut into a Stream/Sink of In/Out
        let framed = tokio_serde::Framed::new(framed, postcard);
        Self(framed)
    }
}

impl<T, In> FramedPostcardRead<T, In> {
    /// Get the underlying binary stream
    ///
    /// This can be useful if you want to drop the framing and use the underlying stream directly
    /// after exchanging some messages.
    pub fn into_inner(self) -> T {
        self.0.into_inner().into_inner()
    }
}

impl<T: AsyncRead, In: DeserializeOwned> Stream for FramedPostcardRead<T, In> {
    type Item = Result<In, std::io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.project().0).poll_next(cx)
    }
}

/// Wrapper that wraps a bidirectional binary stream in a length delimited codec and postcard encoding
/// to get a bidirectional stream of rpc Messages
#[pin_project]
pub struct FramedPostcardWrite<T, Out>(
    #[pin]
    tokio_serde::SymmetricallyFramed<
        tokio_util::codec::FramedWrite<T, tokio_util::codec::LengthDelimitedCodec>,
        Out,
        tokio_serde_postcard::SymmetricalPostcard<Out>,
    >,
);

impl<T: AsyncWrite, Out: Serialize> FramedPostcardWrite<T, Out> {
    /// Wrap a socket in a length delimited codec and postcard encoding
    pub fn new(inner: T, max_frame_length: usize) -> Self {
        // configure length delimited codec with max frame length
        let framing = LengthDelimitedCodec::builder()
            .max_frame_length(max_frame_length)
            .new_codec();
        // create the actual framing. This turns the AsyncRead/AsyncWrite into a Stream/Sink of Bytes/BytesMut
        let framed = tokio_util::codec::FramedWrite::new(inner, framing);
        let postcard = tokio_serde_postcard::SymmetricalPostcard::new();
        // create the actual framing. This turns the Stream/Sink of Bytes/BytesMut into a Stream/Sink of In/Out
        let framed = tokio_serde::SymmetricallyFramed::new(framed, postcard);
        Self(framed)
    }
}

impl<T, Out> FramedPostcardWrite<T, Out> {
    /// Get the underlying binary stream
    ///
    /// This can be useful if you want to drop the framing and use the underlying stream directly
    /// after exchanging some messages.
    pub fn into_inner(self) -> T {
        self.0.into_inner().into_inner()
    }
}

impl<T: AsyncWrite, Out: Serialize> Sink<Out> for FramedPostcardWrite<T, Out> {
    type Error = std::io::Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.project().0).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Out) -> Result<(), Self::Error> {
        Pin::new(&mut self.project().0).start_send(item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.project().0).poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.project().0).poll_close(cx)
    }
}
