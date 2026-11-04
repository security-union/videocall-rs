use std::{
    fmt,
    future::{poll_fn, Future},
    io::Cursor,
    ops::Deref,
    pin::Pin,
    sync::{Arc, Mutex, OnceLock},
    task::{Context, Poll, Waker},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use futures::stream::{FuturesUnordered, Stream, StreamExt};

use crate::{
    proto::{ConnectRequest, ConnectResponse, Frame, StreamUni, VarInt},
    ClientError, Connected, RecvStream, SendStream, SessionError, Settings, WebTransportError,
};

/// An established WebTransport session, acting like a full QUIC connection. See [`quinn::Connection`].
///
/// It is important to remember that WebTransport is layered on top of QUIC:
///   1. Each stream starts with a few bytes identifying the stream type and session ID.
///   2. Errors codes are encoded with the session ID, so they aren't full QUIC error codes.
///   3. Stream IDs may have gaps in them, used by HTTP/3 transparant to the application.
///
/// Deref is used to expose non-overloaded methods on [`quinn::Connection`].
/// These should be safe to use with WebTransport, but file a PR if you find one that isn't.
#[derive(Clone)]
pub struct Session {
    conn: quinn::Connection,

    // The session ID, as determined by the stream ID of the connect request.
    session_id: Option<VarInt>,

    // The accept logic is stateful, so use an Arc<Mutex> to share it.
    accept: Option<Arc<Mutex<SessionAccept>>>,

    // Cache the headers in front of each stream we open.
    header_uni: Vec<u8>,
    header_bi: Vec<u8>,
    header_datagram: Vec<u8>,

    // Keep a reference to the settings and connect stream to avoid closing them until dropped.
    #[allow(dead_code)]
    settings: Option<Arc<Settings>>,

    // The send side of the CONNECT stream, used to write the CloseWebTransportSession capsule.
    // Wrapped in Arc<Mutex<Option<...>>> so close() can take it exactly once.
    connect_send: Arc<Mutex<Option<quinn::SendStream>>>,

    // Session error, set once by either local close() or the background task
    // when a remote CloseWebTransportSession capsule is received.
    // Uses OnceLock for set-once, first-writer-wins semantics with lock-free reads.
    error: Arc<OnceLock<SessionError>>,

    // The request sent by the client.
    request: ConnectRequest,

    // The response sent by the server.
    response: ConnectResponse,
}

impl Session {
    pub(crate) fn new(conn: quinn::Connection, settings: Settings, connect: Connected) -> Self {
        // The session ID is the stream ID of the CONNECT request.
        let session_id = connect.session_id();

        // Cache the tiny header we write in front of each stream we open.
        let mut header_uni = Vec::new();
        StreamUni::WEBTRANSPORT.encode(&mut header_uni);
        session_id.encode(&mut header_uni);

        let mut header_bi = Vec::new();
        Frame::WEBTRANSPORT.encode(&mut header_bi);
        session_id.encode(&mut header_bi);

        let mut header_datagram = Vec::new();
        session_id.encode(&mut header_datagram);

        let error: Arc<OnceLock<SessionError>> = Arc::new(OnceLock::new());

        // Accept logic is stateful, so use an Arc<Mutex> to share it.
        let accept = SessionAccept::new(conn.clone(), session_id, error.clone());

        let this = Self {
            conn,
            accept: Some(Arc::new(Mutex::new(accept))),
            session_id: Some(session_id),
            header_uni,
            header_bi,
            header_datagram,
            settings: Some(Arc::new(settings)),
            connect_send: Arc::new(Mutex::new(Some(connect.send))),
            error: error.clone(),
            request: connect.request.clone(),
            response: connect.response.clone(),
        };

        // Run a background task to read capsules from the CONNECT recv stream.
        let conn2 = this.conn.clone();
        tokio::spawn(Self::run_recv(conn2, connect.recv, error));

        this
    }

    // Read capsules from the CONNECT recv stream until it's closed,
    // then record the close error and tear down the connection.
    async fn run_recv(
        conn: quinn::Connection,
        recv: quinn::RecvStream,
        error: Arc<OnceLock<SessionError>>,
    ) {
        let close_info = Self::read_capsules(recv).await;
        let code = close_info.as_ref().map_or(0, |(c, _)| *c);

        let http3_code: quinn::VarInt = web_transport_proto::error_to_http3(code)
            .try_into()
            .unwrap();

        // Try to record the remote close error. If close() already set
        // the error, it owns the connection teardown, so we bail out.
        match close_info {
            Some((code, reason)) => {
                let err = WebTransportError::Closed(code, reason.clone());
                if error.set(err.into()).is_err() {
                    return;
                }
                conn.close(http3_code, reason.as_bytes());
            }
            None => {
                let err = quinn::ConnectionError::LocallyClosed.into();
                if error.set(err).is_err() {
                    return;
                }
                conn.close(http3_code, b"");
            }
        };
    }

    // Keep reading capsules from the CONNECT recv stream until it's closed.
    // Returns Some((code, reason)) if a CloseWebTransportSession capsule was received,
    // or None if the stream closed without a capsule.
    async fn read_capsules(recv: quinn::RecvStream) -> Option<(u32, String)> {
        let mut reader = web_transport_proto::Http3CapsuleReader::new(recv);
        loop {
            match reader.read().await {
                Ok(Some(web_transport_proto::Capsule::CloseWebTransportSession {
                    code,
                    reason,
                })) => return Some((code, reason)),
                Ok(Some(web_transport_proto::Capsule::Grease { .. })) => {}
                Ok(Some(web_transport_proto::Capsule::Unknown { typ, payload })) => {
                    tracing::warn!(%typ, size = payload.len(), "unknown capsule");
                }
                Ok(None) => return None,
                Err(e) => {
                    tracing::warn!(?e, "failed to read capsule");
                    return None;
                }
            }
        }
    }

    /// Connect using an established QUIC connection if you want to create the connection yourself.
    /// This will only work with a brand new QUIC connection using the HTTP/3 ALPN.
    pub async fn connect(
        conn: quinn::Connection,
        request: impl Into<ConnectRequest>,
    ) -> Result<Session, ClientError> {
        let request = request.into();

        // Perform the H3 handshake by sending/reciving SETTINGS frames.
        let settings = Settings::connect(&conn).await?;

        // Send the HTTP/3 CONNECT request.
        let connect = Connected::open(&conn, request).await?;

        // Return the resulting session with a reference to the control/connect streams.
        // If either stream is closed, then the session will be closed, so we need to keep them around.
        let session = Session::new(conn, settings, connect);

        Ok(session)
    }

    /// Accept a new unidirectional stream. See [`quinn::Connection::accept_uni`].
    pub async fn accept_uni(&self) -> Result<RecvStream, SessionError> {
        if let Some(accept) = &self.accept {
            poll_fn(|cx| accept.lock().unwrap().poll_accept_uni(cx))
                .await
                .map_err(|e| self.map_error(e))
        } else {
            let recv = self
                .conn
                .accept_uni()
                .await
                .map_err(|e| self.map_error(e))?;
            Ok(RecvStream::new(recv, self.error.clone()))
        }
    }

    /// Accept a new bidirectional stream. See [`quinn::Connection::accept_bi`].
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream), SessionError> {
        if let Some(accept) = &self.accept {
            poll_fn(|cx| accept.lock().unwrap().poll_accept_bi(cx))
                .await
                .map_err(|e| self.map_error(e))
        } else {
            let (send, recv) = self.conn.accept_bi().await.map_err(|e| self.map_error(e))?;
            Ok((
                SendStream::new(send, self.error.clone()),
                RecvStream::new(recv, self.error.clone()),
            ))
        }
    }

    /// Open a new unidirectional stream. See [`quinn::Connection::open_uni`].
    pub async fn open_uni(&self) -> Result<SendStream, SessionError> {
        let mut send = self.conn.open_uni().await.map_err(|e| self.map_error(e))?;

        // Set the stream priority to max and then write the stream header.
        // Otherwise the application could write data with lower priority than the header, resulting in queuing.
        // Also the header is very important for determining the session ID without reliable reset.
        send.set_priority(i32::MAX).ok();
        Self::write_full(&mut send, &self.header_uni)
            .await
            .map_err(|e| self.map_error(e))?;

        // Reset the stream priority back to the default of 0.
        send.set_priority(0).ok();
        Ok(SendStream::new(send, self.error.clone()))
    }

    /// [`Self::open_uni`] with the send reserve set before the stream header, so the header itself passes a full window.
    pub async fn open_uni_with_send_reserve(&self, bytes: u64) -> Result<SendStream, SessionError> {
        let mut send = self.conn.open_uni().await.map_err(|e| self.map_error(e))?;
        self.conn
            .set_stream_send_reserve(send.id(), bytes)
            .map_err(|_| WebTransportError::WriteError(quinn::WriteError::ClosedStream))?;

        send.set_priority(i32::MAX).ok();
        Self::write_full(&mut send, &self.header_uni)
            .await
            .map_err(|e| self.map_error(e))?;

        send.set_priority(0).ok();
        Ok(SendStream::new(send, self.error.clone()))
    }

    /// Open a new bidirectional stream. See [`quinn::Connection::open_bi`].
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream), SessionError> {
        let (mut send, recv) = self.conn.open_bi().await.map_err(|e| self.map_error(e))?;

        // Set the stream priority to max and then write the stream header.
        // Otherwise the application could write data with lower priority than the header, resulting in queuing.
        // Also the header is very important for determining the session ID without reliable reset.
        send.set_priority(i32::MAX).ok();
        Self::write_full(&mut send, &self.header_bi)
            .await
            .map_err(|e| self.map_error(e))?;

        // Reset the stream priority back to the default of 0.
        send.set_priority(0).ok();
        Ok((
            SendStream::new(send, self.error.clone()),
            RecvStream::new(recv, self.error.clone()),
        ))
    }

    /// Asynchronously receives an application datagram from the remote peer.
    ///
    /// This method is used to receive an application datagram sent by the remote
    /// peer over the connection.
    /// It waits for a datagram to become available and returns the received bytes.
    pub async fn read_datagram(&self) -> Result<Bytes, SessionError> {
        let mut datagram = self
            .conn
            .read_datagram()
            .await
            .map_err(|e| self.map_error(e))?;

        let mut cursor = Cursor::new(&datagram);

        if let Some(session_id) = self.session_id {
            // We have to check and strip the session ID from the datagram.
            let actual_id =
                VarInt::decode(&mut cursor).map_err(|_| WebTransportError::UnknownSession)?;
            if actual_id != session_id {
                return Err(WebTransportError::UnknownSession.into());
            }
        }

        // Return the datagram without the session ID.
        let datagram = datagram.split_off(cursor.position() as usize);

        Ok(datagram)
    }

    /// Sends an application datagram to the remote peer.
    ///
    /// Datagrams are unreliable and may be dropped or delivered out of order.
    /// The data must be smaller than [`max_datagram_size`](Self::max_datagram_size).
    pub fn send_datagram(&self, data: Bytes) -> Result<(), SessionError> {
        let result = if !self.header_datagram.is_empty() {
            // Unfortunately, we need to allocate/copy each datagram because of the Quinn API.
            // Pls go +1 if you care: https://github.com/quinn-rs/quinn/issues/1724
            let mut buf = BytesMut::with_capacity(self.header_datagram.len() + data.len());

            // Prepend the datagram with the header indicating the session ID.
            buf.extend_from_slice(&self.header_datagram);
            buf.extend_from_slice(&data);

            self.conn.send_datagram(buf.into())
        } else {
            self.conn.send_datagram(data)
        };

        result.map_err(|e| self.map_error(e))?;
        Ok(())
    }

    /// Computes the maximum size of datagrams that may be passed to
    /// [`send_datagram`](Self::send_datagram).
    pub fn max_datagram_size(&self) -> usize {
        let mtu = self
            .conn
            .max_datagram_size()
            .expect("datagram support is required");
        mtu.saturating_sub(self.header_datagram.len())
    }

    /// Close the session with an error code and reason.
    ///
    /// When there is a session ID (WebTransport over HTTP/3), a `CloseWebTransportSession`
    /// capsule is written on the CONNECT stream before the QUIC connection is closed.
    /// This allows browser clients to receive the close code and reason via `WebTransport.closed`.
    ///
    /// The capsule write and connection close happen asynchronously in a spawned task.
    /// Callers should `await` [`Session::closed()`] to ensure the capsule has been
    /// delivered. Session operations will fail once the QUIC connection is closed.
    pub fn close(&self, code: u32, reason: &[u8]) {
        // Record the local close error. First writer wins — if the background
        // task already set a remote close error, or close() was already called,
        // this is a no-op.
        let err = SessionError::ConnectionError(quinn::ConnectionError::LocallyClosed);
        if self.error.set(err).is_err() {
            return;
        }

        if self.session_id.is_some() {
            // Take the send stream for the capsule write.
            let send = self.connect_send.lock().unwrap().take();

            if let Some(send) = send {
                let reason = String::from_utf8_lossy(reason).into_owned();
                let conn = self.conn.clone();
                let capsule =
                    web_transport_proto::Capsule::CloseWebTransportSession { code, reason };
                let timeout = (self.rtt() * 3).max(Duration::from_millis(100));

                tokio::spawn(async move {
                    Self::close_with_capsule(conn, send, capsule, code, timeout).await;
                });
            }
        } else {
            // Raw QUIC mode: no capsule needed.
            self.conn.close(code.into(), reason);
        }
    }

    /// Write the CloseWebTransportSession capsule, finish the stream, wait for
    /// the peer to close the connection (or timeout), then force-close.
    async fn close_with_capsule(
        conn: quinn::Connection,
        mut send: quinn::SendStream,
        capsule: web_transport_proto::Capsule,
        code: u32,
        timeout: std::time::Duration,
    ) {
        let http3_code: quinn::VarInt = web_transport_proto::error_to_http3(code)
            .try_into()
            .unwrap();

        // Encode the capsule, then wrap it in an HTTP/3 DATA frame.
        // In HTTP/3, capsule data is carried inside DATA frames on the CONNECT
        // stream (RFC 9297 Section 3.2).
        let mut capsule_bytes = Vec::new();
        capsule.encode(&mut capsule_bytes);

        let mut frame = Vec::new();
        Frame::DATA.encode(&mut frame);
        let Ok(len) = VarInt::try_from(capsule_bytes.len()) else {
            tracing::warn!("capsule too large to encode as DATA frame");
            conn.close(http3_code, b"");
            return;
        };
        len.encode(&mut frame);
        frame.extend_from_slice(&capsule_bytes);

        // Bound the entire graceful-close sequence (capsule write, FIN,
        // waiting for the peer) with a single timeout.  Without this, an
        // unresponsive peer can cause write_all to block indefinitely when
        // the send buffer fills up and no idle timeout is configured.
        let graceful = async {
            // Write the DATA frame to the CONNECT send stream.
            if let Err(e) = send.write_all(&frame).await {
                tracing::warn!(?e, "failed to write CloseWebTransportSession capsule");
                conn.close(http3_code, b"");
                return;
            }

            // FIN the send stream so the peer knows no more capsules are coming.
            if let Err(e) = send.finish() {
                tracing::warn!(?e, "failed to finish CONNECT send stream");
                conn.close(http3_code, b"");
                return;
            }

            // Wait for the peer to close the CONNECT stream after receiving the capsule.
            conn.closed().await;
        };

        if tokio::time::timeout(timeout, graceful).await.is_err() {
            tracing::debug!("timeout waiting for peer to close; force-closing connection");
            conn.close(http3_code, b"");
        }
    }

    /// Wait until the session is closed, returning the error. See [`quinn::Connection::closed`].
    ///
    /// If the peer sent a `CloseWebTransportSession` capsule, the returned error will be
    /// [`WebTransportError::Closed`] with the code and reason from the capsule.
    ///
    /// Unlike [`quinn::Connection::closed`], this does **not** return early when
    /// [`close()`](Self::close) has been called. It waits for the underlying QUIC
    /// connection to shut down, ensuring the `CloseWebTransportSession` capsule has
    /// been delivered. Use [`close_reason()`](Self::close_reason) for a non-blocking check.
    pub async fn closed(&self) -> SessionError {
        self.map_error(self.conn.closed().await)
    }

    /// Return why the session was closed, or None if it's not closed. See [`quinn::Connection::close_reason`].
    pub fn close_reason(&self) -> Option<SessionError> {
        self.conn.close_reason().map(|e| self.map_error(e))
    }

    /// Replace connection-level errors with the stored session error if available.
    fn map_error(&self, e: impl Into<SessionError>) -> SessionError {
        let e = e.into();
        if let Some(err) = self.error.get() {
            if matches!(
                &e,
                SessionError::ConnectionError(_)
                    | SessionError::WebTransportError(WebTransportError::Closed(..))
                    | SessionError::SendDatagramError(quinn::SendDatagramError::ConnectionLost(_))
            ) {
                return err.clone();
            }
        }
        e
    }

    async fn write_full(send: &mut quinn::SendStream, buf: &[u8]) -> Result<(), SessionError> {
        match send.write_all(buf).await {
            Ok(_) => Ok(()),
            Err(quinn::WriteError::ConnectionLost(err)) => Err(err.into()),
            Err(err) => Err(WebTransportError::WriteError(err).into()),
        }
    }

    /// Create a new session from a raw QUIC connection and a URL.
    ///
    /// This is used to pretend like a QUIC connection is a WebTransport session.
    /// It's a hack, but it makes it much easier to support WebTransport and raw QUIC simultaneously.
    pub fn raw(
        conn: quinn::Connection,
        request: impl Into<ConnectRequest>,
        response: impl Into<ConnectResponse>,
    ) -> Self {
        Self {
            conn,
            session_id: None,
            header_uni: Default::default(),
            header_bi: Default::default(),
            header_datagram: Default::default(),
            accept: None,
            settings: None,
            connect_send: Arc::new(Mutex::new(None)),
            error: Arc::new(OnceLock::new()),
            request: request.into(),
            response: response.into(),
        }
    }

    pub fn request(&self) -> &ConnectRequest {
        &self.request
    }

    pub fn response(&self) -> &ConnectResponse {
        &self.response
    }

    /// Return connection-level statistics.
    pub fn stats(&self) -> SessionStats {
        SessionStats {
            stats: self.conn.stats(),
            rtt: self.conn.rtt(),
        }
    }
}

impl Deref for Session {
    type Target = quinn::Connection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.conn.fmt(f)
    }
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.conn.stable_id() == other.conn.stable_id()
    }
}

impl Eq for Session {}

// Type aliases just so clippy doesn't complain about the complexity.
type AcceptUni = dyn Stream<Item = Result<quinn::RecvStream, quinn::ConnectionError>> + Send;
type AcceptBi = dyn Stream<Item = Result<(quinn::SendStream, quinn::RecvStream), quinn::ConnectionError>>
    + Send;
type PendingUni = dyn Future<Output = Result<(StreamUni, quinn::RecvStream), SessionError>> + Send;
type PendingBi = dyn Future<Output = Result<Option<(quinn::SendStream, quinn::RecvStream)>, SessionError>>
    + Send;

// Logic just for accepting streams, which is annoying because of the stream header.
pub struct SessionAccept {
    session_id: VarInt,

    // Shared session error for propagation to accepted streams.
    error: Arc<OnceLock<SessionError>>,

    // We also need to keep a reference to the qpack streams if the endpoint (incorrectly) creates them.
    // Again, this is just so they don't get closed until we drop the session.
    qpack_encoder: Option<quinn::RecvStream>,
    qpack_decoder: Option<quinn::RecvStream>,

    accept_uni: Pin<Box<AcceptUni>>,
    accept_bi: Pin<Box<AcceptBi>>,

    // Keep track of work being done to read/write the WebTransport stream header.
    pending_uni: FuturesUnordered<Pin<Box<PendingUni>>>,
    pending_bi: FuturesUnordered<Pin<Box<PendingBi>>>,

    // Wakers from concurrent callers of accept_bi / accept_uni.
    // When one caller gets a stream, all others are woken so they can retry.
    // This fixes the lost-waker bug where the unfold stream only stores one waker.
    bi_wakers: Vec<Waker>,
    uni_wakers: Vec<Waker>,
}

impl SessionAccept {
    pub(crate) fn new(
        conn: quinn::Connection,
        session_id: VarInt,
        error: Arc<OnceLock<SessionError>>,
    ) -> Self {
        // Create a stream that just outputs new streams, so it's easy to call from poll.
        let accept_uni = Box::pin(futures::stream::unfold(conn.clone(), |conn| async {
            Some((conn.accept_uni().await, conn))
        }));

        let accept_bi = Box::pin(futures::stream::unfold(conn, |conn| async {
            Some((conn.accept_bi().await, conn))
        }));

        Self {
            session_id,
            error,

            qpack_decoder: None,
            qpack_encoder: None,

            accept_uni,
            accept_bi,

            pending_uni: FuturesUnordered::new(),
            pending_bi: FuturesUnordered::new(),

            bi_wakers: Vec::new(),
            uni_wakers: Vec::new(),
        }
    }

    // This is poll-based because we accept and decode streams in parallel.
    // In async land I would use tokio::JoinSet, but that requires a runtime.
    // It's better to use FuturesUnordered instead because it's agnostic.
    pub fn poll_accept_uni(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<RecvStream, SessionError>> {
        loop {
            // Accept any new streams.
            if let Poll::Ready(Some(res)) = self.accept_uni.poll_next_unpin(cx) {
                // Start decoding the header and add the future to the list of pending streams.
                let recv = match res {
                    Ok(recv) => recv,
                    Err(e) => {
                        for waker in self.uni_wakers.drain(..) {
                            waker.wake();
                        }
                        return Poll::Ready(Err(e.into()));
                    }
                };
                let pending = Self::decode_uni(recv, self.session_id);
                self.pending_uni.push(Box::pin(pending));

                continue;
            }

            // Poll the list of pending streams.
            let (typ, recv) = match self.pending_uni.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(res))) => res,
                Poll::Ready(Some(Err(err))) => {
                    // Ignore the error, the stream was probably reset early.
                    tracing::warn!(?err, "failed to decode unidirectional stream");
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => {
                    if !self.uni_wakers.iter().any(|w| w.will_wake(cx.waker())) {
                        self.uni_wakers.push(cx.waker().clone());
                    }
                    return Poll::Pending;
                }
            };

            // Decide if we keep looping based on the type.
            match typ {
                StreamUni::WEBTRANSPORT => {
                    let recv = RecvStream::new(recv, self.error.clone());
                    for waker in self.uni_wakers.drain(..) {
                        waker.wake();
                    }
                    return Poll::Ready(Ok(recv));
                }
                StreamUni::QPACK_DECODER => {
                    self.qpack_decoder = Some(recv);
                }
                StreamUni::QPACK_ENCODER => {
                    self.qpack_encoder = Some(recv);
                }
                _ => {
                    // ignore unknown streams
                    tracing::debug!(?typ, "ignoring unknown unidirectional stream");
                }
            }
        }
    }

    // Reads the stream header, returning the stream type.
    async fn decode_uni(
        mut recv: quinn::RecvStream,
        expected_session: VarInt,
    ) -> Result<(StreamUni, quinn::RecvStream), SessionError> {
        // Read the VarInt at the start of the stream.
        let typ = VarInt::read(&mut recv)
            .await
            .map_err(|_| WebTransportError::UnknownSession)?;
        let typ = StreamUni(typ);

        if typ == StreamUni::WEBTRANSPORT {
            // Read the session_id and validate it
            let session_id = VarInt::read(&mut recv)
                .await
                .map_err(|_| WebTransportError::UnknownSession)?;
            if session_id != expected_session {
                return Err(WebTransportError::UnknownSession.into());
            }
        }

        // We need to keep a reference to the qpack streams if the endpoint (incorrectly) creates them, so return everything.
        Ok((typ, recv))
    }

    pub fn poll_accept_bi(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(SendStream, RecvStream), SessionError>> {
        loop {
            // Accept any new streams.
            if let Poll::Ready(Some(res)) = self.accept_bi.poll_next_unpin(cx) {
                // Start decoding the header and add the future to the list of pending streams.
                let (send, recv) = match res {
                    Ok(pair) => pair,
                    Err(e) => {
                        for waker in self.bi_wakers.drain(..) {
                            waker.wake();
                        }
                        return Poll::Ready(Err(e.into()));
                    }
                };
                let pending = Self::decode_bi(send, recv, self.session_id);
                self.pending_bi.push(Box::pin(pending));

                continue;
            }

            // Poll the list of pending streams.
            let res = match self.pending_bi.poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(res))) => res,
                Poll::Ready(Some(Err(err))) => {
                    // Ignore the error, the stream was probably reset early.
                    tracing::warn!(?err, "failed to decode bidirectional stream");
                    continue;
                }
                Poll::Ready(None) | Poll::Pending => {
                    if !self.bi_wakers.iter().any(|w| w.will_wake(cx.waker())) {
                        self.bi_wakers.push(cx.waker().clone());
                    }
                    return Poll::Pending;
                }
            };

            if let Some((send, recv)) = res {
                // Wrap the streams in our own types for correct error codes.
                let send = SendStream::new(send, self.error.clone());
                let recv = RecvStream::new(recv, self.error.clone());
                for waker in self.bi_wakers.drain(..) {
                    waker.wake();
                }
                return Poll::Ready(Ok((send, recv)));
            }

            // Keep looping if it's a stream we want to ignore.
        }
    }

    // Reads the stream header, returning Some if it's a WebTransport stream.
    async fn decode_bi(
        send: quinn::SendStream,
        mut recv: quinn::RecvStream,
        expected_session: VarInt,
    ) -> Result<Option<(quinn::SendStream, quinn::RecvStream)>, SessionError> {
        let typ = VarInt::read(&mut recv)
            .await
            .map_err(|_| WebTransportError::UnknownSession)?;
        if Frame(typ) != Frame::WEBTRANSPORT {
            tracing::debug!(?typ, "ignoring unknown bidirectional stream");
            return Ok(None);
        }

        // Read the session ID and validate it.
        let session_id = VarInt::read(&mut recv)
            .await
            .map_err(|_| WebTransportError::UnknownSession)?;
        if session_id != expected_session {
            return Err(WebTransportError::UnknownSession.into());
        }

        Ok(Some((send, recv)))
    }
}

pub struct SessionStats {
    stats: quinn::ConnectionStats,
    rtt: std::time::Duration,
}

impl web_transport_trait::Stats for SessionStats {
    fn bytes_sent(&self) -> Option<u64> {
        Some(self.stats.udp_tx.bytes)
    }

    fn bytes_received(&self) -> Option<u64> {
        Some(self.stats.udp_rx.bytes)
    }

    fn bytes_lost(&self) -> Option<u64> {
        Some(self.stats.path.lost_bytes)
    }

    fn packets_sent(&self) -> Option<u64> {
        Some(self.stats.udp_tx.datagrams)
    }

    fn packets_received(&self) -> Option<u64> {
        Some(self.stats.udp_rx.datagrams)
    }

    fn packets_lost(&self) -> Option<u64> {
        Some(self.stats.path.lost_packets)
    }

    fn rtt(&self) -> Option<std::time::Duration> {
        Some(self.rtt)
    }

    fn estimated_send_rate(&self) -> Option<u64> {
        let rtt_secs = self.rtt.as_secs_f64();
        if self.stats.path.cwnd > 0 && rtt_secs > 0.0 {
            Some((self.stats.path.cwnd as f64 * 8.0 / rtt_secs) as u64)
        } else {
            None
        }
    }
}

impl web_transport_trait::Session for Session {
    type SendStream = SendStream;
    type RecvStream = RecvStream;
    type Error = SessionError;

    async fn accept_uni(&self) -> Result<Self::RecvStream, Self::Error> {
        Self::accept_uni(self).await
    }

    async fn accept_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        Self::accept_bi(self).await
    }

    async fn open_bi(&self) -> Result<(Self::SendStream, Self::RecvStream), Self::Error> {
        Self::open_bi(self).await
    }

    async fn open_uni(&self) -> Result<Self::SendStream, Self::Error> {
        Self::open_uni(self).await
    }

    fn close(&self, code: u32, reason: &str) {
        Self::close(self, code, reason.as_bytes());
    }

    async fn closed(&self) -> Self::Error {
        Self::closed(self).await
    }

    fn send_datagram(&self, data: Bytes) -> Result<(), Self::Error> {
        Self::send_datagram(self, data)
    }

    async fn recv_datagram(&self) -> Result<Bytes, Self::Error> {
        Self::read_datagram(self).await
    }

    fn max_datagram_size(&self) -> usize {
        Self::max_datagram_size(self)
    }

    fn protocol(&self) -> Option<&str> {
        self.response.protocol.as_deref()
    }

    #[allow(refining_impl_trait)]
    fn stats(&self) -> SessionStats {
        Self::stats(self)
    }
}
