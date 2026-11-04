use std::{
    io,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};

use crate::{ClosedStream, SessionError, WriteError};

/// A stream that can be used to send bytes. See [`quinn::SendStream`].
///
/// This wrapper is mainly needed for error codes, which is unfortunate.
/// WebTransport uses u32 error codes and they're mapped in a reserved HTTP/3 error space.
#[derive(Debug)]
pub struct SendStream {
    stream: quinn::SendStream,
    error: Arc<OnceLock<SessionError>>,
}

impl SendStream {
    pub(crate) fn new(stream: quinn::SendStream, error: Arc<OnceLock<SessionError>>) -> Self {
        Self { stream, error }
    }

    /// Replace connection-level errors with the stored session error if available.
    fn map_error(&self, e: impl Into<WriteError>) -> WriteError {
        let e = e.into();
        if let Some(err) = self.error.get() {
            if matches!(
                &e,
                WriteError::SessionError(_) | WriteError::InvalidStopped(_)
            ) {
                return WriteError::SessionError(err.clone());
            }
        }
        e
    }

    /// Abruptly reset the stream with the provided error code. See [`quinn::SendStream::reset`].
    /// This is a u32 with WebTransport because we share the error space with HTTP/3.
    pub fn reset(&mut self, code: u32) -> Result<(), ClosedStream> {
        let code = web_transport_proto::error_to_http3(code);
        let code = quinn::VarInt::try_from(code).unwrap();
        self.stream.reset(code).map_err(Into::into)
    }

    /// Wait until the stream has been stopped and return the error code. See [`quinn::SendStream::stopped`].
    ///
    /// Unlike Quinn, this returns None if the code is not a valid WebTransport error code.
    /// Also unlike Quinn, this returns a SessionError, not a StoppedError, because 0-RTT is not supported.
    pub async fn stopped(&self) -> Result<Option<u32>, SessionError> {
        match self.stream.stopped().await {
            Ok(Some(code)) => Ok(web_transport_proto::error_from_http3(code.into_inner())),
            Ok(None) => Ok(None),
            Err(quinn::StoppedError::ConnectionLost(conn_err)) => {
                Err(self.error.get().cloned().unwrap_or_else(|| conn_err.into()))
            }
            Err(quinn::StoppedError::ZeroRttRejected) => unreachable!("0-RTT not supported"),
        }
    }

    // Unfortunately, we have to wrap WriteError for a bunch of functions.

    /// Write some data to the stream, returning the size written. See [`quinn::SendStream::write`].
    pub async fn write(&mut self, buf: &[u8]) -> Result<usize, WriteError> {
        self.stream.write(buf).await.map_err(|e| self.map_error(e))
    }

    /// Write all of the data to the stream. See [`quinn::SendStream::write_all`].
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), WriteError> {
        self.stream
            .write_all(buf)
            .await
            .map_err(|e| self.map_error(e))
    }

    /// Write chunks of data to the stream. See [`quinn::SendStream::write_chunks`].
    pub async fn write_chunks(&mut self, bufs: &mut [Bytes]) -> Result<quinn::Written, WriteError> {
        self.stream
            .write_chunks(bufs)
            .await
            .map_err(|e| self.map_error(e))
    }

    /// Write a chunk of data to the stream. See [`quinn::SendStream::write_chunk`].
    pub async fn write_chunk(&mut self, buf: Bytes) -> Result<(), WriteError> {
        self.stream
            .write_chunk(buf)
            .await
            .map_err(|e| self.map_error(e))
    }

    /// Write all of the chunks of data to the stream. See [`quinn::SendStream::write_all_chunks`].
    pub async fn write_all_chunks(&mut self, bufs: &mut [Bytes]) -> Result<(), WriteError> {
        self.stream
            .write_all_chunks(bufs)
            .await
            .map_err(|e| self.map_error(e))
    }

    /// Mark the stream as finished, such that no more data can be written. See [`quinn::SendStream::finish`].
    ///
    /// WARNING: This is implicitly called on Drop, but it's a common footgun in Quinn.
    /// If you cancel futures by dropping them you'll get incomplete writes.
    pub fn finish(&mut self) -> Result<(), ClosedStream> {
        self.stream.finish().map_err(Into::into)
    }

    pub fn set_priority(&self, order: i32) -> Result<(), ClosedStream> {
        self.stream.set_priority(order).map_err(Into::into)
    }

    pub fn priority(&self) -> Result<i32, ClosedStream> {
        self.stream.priority().map_err(Into::into)
    }

    /// Return the underlying QUIC stream ID.
    ///
    /// > **Warning**
    /// >
    /// > WebTransport sessions share the QUIC connection with HTTP/3 and potentially other sessions.
    /// > The [quinn::StreamId::index] might not increment by 1 like expected when using [quinn].
    /// > This is why the Javascript WebTransport API does not expose the Stream ID.
    pub fn quic_id(&self) -> quinn::StreamId {
        self.stream.id()
    }
}

impl tokio::io::AsyncWrite for SendStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // We have to use this syntax because quinn added its own poll_write method.
        tokio::io::AsyncWrite::poll_write(Pin::new(&mut self.stream), cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

impl web_transport_trait::SendStream for SendStream {
    type Error = WriteError;

    fn set_priority(&mut self, order: u8) {
        Self::set_priority(self, order.into()).ok();
    }

    fn reset(&mut self, code: u32) {
        Self::reset(self, code).ok();
    }

    fn finish(&mut self) -> Result<(), Self::Error> {
        Self::finish(self).map_err(|_| WriteError::ClosedStream)
    }

    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        Self::write(self, buf).await
    }

    async fn write_buf<B: Buf + Send>(&mut self, buf: &mut B) -> Result<usize, Self::Error> {
        // This can avoid making a copy when Buf is Bytes, as Quinn will allocate anyway.
        let size = buf.chunk().len();
        let chunk = buf.copy_to_bytes(size);
        self.write_chunk(chunk).await?;
        Ok(size)
    }

    async fn write_chunk(&mut self, chunk: Bytes) -> Result<(), Self::Error> {
        self.write_chunk(chunk).await
    }

    async fn closed(&mut self) -> Result<(), Self::Error> {
        // NOTE: This used to require &mut in an older version of Quinn.
        match self.stopped().await? {
            Some(code) => Err(WriteError::Stopped(code)),
            None => Ok(()),
        }
    }
}
