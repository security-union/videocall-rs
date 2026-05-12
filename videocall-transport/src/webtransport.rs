//! A service to connect to a server through the
//! [`WebTransport` Protocol](https://datatracker.ietf.org/doc/draft-ietf-webtrans-overview/).
//!
//! Forked from yew-webtransport (MIT licensed, Copyright (c) 2022 Security Union),
//! adapted to use `videocall_types::Callback` instead of `yew::Callback`.

use anyhow::{anyhow, Error};
use futures::channel::oneshot::channel;
use futures::lock::Mutex as AsyncMutex;
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt, rc::Rc};
use thiserror::Error as ThisError;
use videocall_types::Callback;
use wasm_bindgen_futures::JsFuture;

use gloo_console::log;
use js_sys::{Boolean, JsString, Promise, Reflect, Uint8Array};
use wasm_bindgen::{prelude::Closure, JsCast, JsValue};
use web_sys::{
    ReadableStream, ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportDatagramDuplexStream, WebTransportReceiveStream, WritableStream,
    WritableStreamDefaultWriter,
};

/// Cumulative count of datagrams dropped because the writable stream was locked.
static DATAGRAM_DROP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Returns the total number of datagrams dropped since process start.
pub fn datagram_drop_count() -> u64 {
    DATAGRAM_DROP_COUNT.load(Ordering::Relaxed)
}

/// Maximum length-prefixed frame payload size on a persistent stream (4 MB).
/// Matches the server's `MAX_FRAME_SIZE` so honest senders never trip the
/// server-side guard.  Frames larger than this are dropped client-side rather
/// than being written and immediately torn down by the receiver.
pub const PERSISTENT_STREAM_MAX_FRAME_SIZE: usize = 4_000_000;

/// Holds a persistent unidirectional stream and its writer so the QUIC stream
/// stays open across multiple sends, preserving packet ordering.
///
/// Each `PersistentSendStream` corresponds to **one** QUIC unidirectional
/// stream.  We open one of these per media type (audio, video, screen,
/// control) so that head-of-line blocking on one media type cannot stall the
/// others.  See `webtransport-client` / Phase 2 architectural fix for
/// background.
pub struct PersistentSendStream {
    /// The underlying `WritableStream` (QUIC send stream).  Kept alive so the
    /// stream is not garbage-collected while the writer is in use.
    _stream: WritableStream,
    /// The writer acquired from `_stream`.  Reused for every reliable send
    /// on this media type.  The writer enforces FIFO ordering of writes; we
    /// do not need to serialise writes ourselves once the writer is created.
    writer: WritableStreamDefaultWriter,
}

/// Map of per-media-type persistent send streams, keyed by an opaque `u8`
/// stream identifier.  The transport layer does not interpret the key — that
/// is the caller's responsibility (see `MediaStreamKey` in
/// `videocall-client/src/connection/webmedia.rs`).
///
/// The map is wrapped in an `AsyncMutex` so that the lazy-creation path is
/// race-free across concurrent `send_on_persistent_stream` invocations.  In
/// single-threaded WASM the mutex is purely a re-entrancy guard across
/// `.await` points; it does **not** block writes once the stream exists.
pub type PersistentStreamMap = Rc<AsyncMutex<HashMap<u8, PersistentSendStream>>>;

/// Construct an empty persistent-stream map.  Stored inside `WebTransportTask`
/// and threaded through `send_on_persistent_stream`.
pub fn new_persistent_stream_map() -> PersistentStreamMap {
    Rc::new(AsyncMutex::new(HashMap::new()))
}

/// Errors raised when attempting to parse a length-prefix-framed payload
/// out of a stream buffer.  Used by `parse_persistent_stream_frame` and
/// (indirectly) by the server-side reader at
/// `actix-api/src/webtransport/bridge.rs`.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameParseError {
    /// Less than 4 header bytes are available — caller should accumulate
    /// more data and retry.
    NeedMoreHeader,
    /// Header is present but the indicated payload is not fully buffered
    /// yet — caller should accumulate more data and retry.
    NeedMorePayload {
        /// Number of payload bytes still missing.
        missing: usize,
    },
    /// Decoded length is zero or exceeds `PERSISTENT_STREAM_MAX_FRAME_SIZE`.
    /// The stream is unrecoverable; caller should close it and drop any
    /// buffered data.
    InvalidLength(usize),
}

/// Encode `payload` as a `[u32 BE length][payload]` frame ready to be
/// written to a persistent WebTransport unidirectional stream.
///
/// The returned `Vec<u8>` is a single chunk — when handed to JS as one
/// `Uint8Array` and written via `writer.write_with_chunk`, the JS
/// WritableStream spec guarantees that the header and body cannot be
/// interleaved with another frame's bytes on the wire.
///
/// `payload.len()` is required to be at most `PERSISTENT_STREAM_MAX_FRAME_SIZE`;
/// callers must enforce this themselves (the send path drops over-sized
/// frames before reaching this helper).
pub fn frame_persistent_stream_payload(payload: &[u8]) -> Vec<u8> {
    let len = payload.len() as u32;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Attempt to extract a complete `[u32 BE length][payload]` frame from
/// `buf`.  On success returns `Ok((payload, rest))` where `rest` is the
/// remaining unconsumed bytes.  This is the symmetric reverse of
/// `frame_persistent_stream_payload`.
///
/// `Err(FrameParseError::NeedMoreHeader)` and
/// `Err(FrameParseError::NeedMorePayload)` indicate that the caller must
/// accumulate more data and retry.  `Err(FrameParseError::InvalidLength)`
/// indicates an unrecoverable framing violation; the stream must be
/// closed.
///
/// The client itself does not currently call this — the client receives
/// framed payloads via the existing `handle_unidirectional_stream` in
/// `videocall-client/src/connection/webtransport.rs` which has its own
/// inline framing parser.  This helper is exported so the server-side
/// implementation and the unit tests can share the same protocol
/// definition.
pub fn parse_persistent_stream_frame(buf: &[u8]) -> Result<(&[u8], &[u8]), FrameParseError> {
    if buf.len() < 4 {
        return Err(FrameParseError::NeedMoreHeader);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len == 0 || len > PERSISTENT_STREAM_MAX_FRAME_SIZE {
        return Err(FrameParseError::InvalidLength(len));
    }
    let frame_end = 4 + len;
    if buf.len() < frame_end {
        return Err(FrameParseError::NeedMorePayload {
            missing: frame_end - buf.len(),
        });
    }
    Ok((&buf[4..frame_end], &buf[frame_end..]))
}

/// Represents formatting errors.
#[derive(Debug, ThisError)]
pub enum FormatError {
    #[error("received text for a binary format")]
    ReceivedTextForBinary,
    #[error("received binary for a text format")]
    ReceivedBinaryForText,
    #[error("trying to encode a binary format as Text")]
    CantEncodeBinaryAsText,
}

/// A representation of a value which can be stored and restored as a text.
pub type Text = Result<String, Error>;

/// A representation of a value which can be stored and restored as a binary.
pub type Binary = Result<Vec<u8>, Error>;

/// The status of a WebTransport connection. Used for status notifications.
#[derive(Clone, Debug, PartialEq)]
pub enum WebTransportStatus {
    /// Fired when a WebTransport connection has opened.
    Opened,
    /// Fired when a WebTransport connection has closed.
    Closed(JsValue),
    /// Fired when a WebTransport connection has failed.
    Error(JsValue),
    /// Closed/errored before `ready()` resolved — handshake never completed.
    ClosedBeforeReady(String),
    /// Closed/errored after `ready()` resolved — session was established first.
    ClosedAfterReady(String),
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum WebTransportError {
    #[error("{0}")]
    CreationError(String),
}

/// A handle to control the WebTransport connection.
///
/// When dropped, the underlying `WebTransport` is closed, which causes all
/// reader loops (datagrams, unidirectional, bidirectional) to terminate because
/// their `reader.read()` futures resolve with errors on a closed transport.
#[must_use = "the connection will be closed when the task is dropped"]
pub struct WebTransportTask {
    pub transport: Rc<WebTransport>,
    #[allow(dead_code)]
    notification: Callback<WebTransportStatus>,
    #[allow(dead_code)]
    listeners: [Promise; 2],
    /// Stored so the closures live as long as the task and are properly dropped
    /// instead of being leaked via `forget()`. The closed closure is wrapped in
    /// `Rc` because it is shared across multiple promise chains (`ready.catch`,
    /// `closed.then`, `closed.catch`).
    #[allow(dead_code)]
    opened_closure: Closure<dyn FnMut(JsValue)>,
    #[allow(dead_code)]
    closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
    /// Per-media-type persistent unidirectional send streams.  Lazily
    /// populated by `send_on_persistent_stream` on first send for each key.
    /// On stream-write error the entry is removed; the next send for that
    /// key opens a fresh stream.
    pub persistent_streams: PersistentStreamMap,
}

impl WebTransportTask {
    fn new(
        transport: Rc<WebTransport>,
        notification: Callback<WebTransportStatus>,
        listeners: [Promise; 2],
        opened_closure: Closure<dyn FnMut(JsValue)>,
        closed_closure: Rc<Closure<dyn FnMut(JsValue)>>,
    ) -> WebTransportTask {
        WebTransportTask {
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
            persistent_streams: new_persistent_stream_map(),
        }
    }
}

impl Drop for WebTransportTask {
    fn drop(&mut self) {
        // Close the underlying WebTransport session. This causes the reader
        // loops (datagrams, unidirectional streams, bidirectional streams) to
        // break out of their `reader.read()` await — the futures resolve with
        // errors on a closed transport, allowing the spawn_local tasks and
        // their captured Rc<WebTransport> clones to be cleaned up.
        self.transport.close();
    }
}

impl fmt::Debug for WebTransportTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebTransportTask")
    }
}

/// A WebTransport service attached to a user context.
#[derive(Default, Debug)]
pub struct WebTransportService {}

impl WebTransportService {
    /// Connects to a server through a WebTransport connection. Needs callbacks for
    /// datagrams, unidirectional streams, bidirectional streams, and status notifications.
    pub fn connect(
        url: &str,
        on_datagram: Callback<Vec<u8>>,
        on_unidirectional_stream: Callback<WebTransportReceiveStream>,
        on_bidirectional_stream: Callback<WebTransportBidirectionalStream>,
        notification: Callback<WebTransportStatus>,
    ) -> Result<WebTransportTask, WebTransportError> {
        let ConnectCommon(transport, listeners, opened_closure, closed_closure) =
            Self::connect_common(url, &notification)?;
        let transport = Rc::new(transport);

        Self::start_listening_incoming_datagrams(transport.datagrams(), on_datagram);
        Self::start_listening_incoming_unidirectional_streams(
            transport.incoming_unidirectional_streams(),
            on_unidirectional_stream,
        );
        Self::start_listening_incoming_bidirectional_streams(
            transport.incoming_bidirectional_streams(),
            on_bidirectional_stream,
        );

        Ok(WebTransportTask::new(
            transport,
            notification,
            listeners,
            opened_closure,
            closed_closure,
        ))
    }

    fn start_listening_incoming_unidirectional_streams(
        incoming_streams: ReadableStream,
        callback: Callback<WebTransportReceiveStream>,
    ) {
        let read_result: ReadableStreamDefaultReader =
            incoming_streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — the transport is already
                        // shut down and a redundant close() throws when the
                        // session is still in "connecting" state.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!(
                                    "Failed to read 'done' from unidirectional stream result",
                                    &e
                                );
                                break;
                            }
                        };
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportReceiveStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn start_listening_incoming_datagrams(
        datagrams: WebTransportDatagramDuplexStream,
        callback: Callback<Vec<u8>>,
    ) {
        let incoming_datagrams: ReadableStreamDefaultReader =
            datagrams.readable().get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(incoming_datagrams.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — see unidirectional handler.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from datagram result", &e);
                                break;
                            }
                        };
                        if done.is_truthy() {
                            break;
                        }
                        let value: Uint8Array =
                            match Reflect::get(&result, &JsString::from("value")) {
                                Ok(val) => val.unchecked_into(),
                                Err(e) => {
                                    log!("Failed to read 'value' from datagram result", &e);
                                    break;
                                }
                            };
                        process_binary(&value, &callback);
                    }
                }
            }
        });
    }

    fn start_listening_incoming_bidirectional_streams(
        streams: ReadableStream,
        callback: Callback<WebTransportBidirectionalStream>,
    ) {
        let read_result: ReadableStreamDefaultReader = streams.get_reader().unchecked_into();
        wasm_bindgen_futures::spawn_local(async move {
            loop {
                let read_result = JsFuture::from(read_result.read()).await;
                match read_result {
                    Err(_) => {
                        // Expected when the transport is closed (Drop or network
                        // failure).  Don't re-close — see unidirectional handler.
                        break;
                    }
                    Ok(result) => {
                        let done = match Reflect::get(&result, &JsString::from("done")) {
                            Ok(val) => val.unchecked_into::<Boolean>(),
                            Err(e) => {
                                log!("Failed to read 'done' from bidirectional stream result", &e);
                                break;
                            }
                        };
                        if let Ok(value) = Reflect::get(&result, &JsString::from("value")) {
                            if value.is_undefined() {
                                break;
                            }
                            let value: WebTransportBidirectionalStream = value.unchecked_into();
                            callback.emit(value);
                        }
                        if done.is_truthy() {
                            break;
                        }
                    }
                }
            }
        });
    }

    fn connect_common(
        url: &str,
        notification: &Callback<WebTransportStatus>,
    ) -> Result<ConnectCommon, WebTransportError> {
        let transport = WebTransport::new(url);
        let transport = transport.map_err(|e| {
            WebTransportError::CreationError(format!("Failed to create WebTransport: {e:?}"))
        })?;

        // Track whether the handshake (`ready()`) has completed, so that
        // subsequent close/error events can be classified correctly.
        let handshake_complete = Rc::new(Cell::new(false));
        // Guard against emitting connection-lost more than once per connection
        // (browser may fire both `closed` and `ready.catch` for the same failure).
        let fired = Rc::new(Cell::new(false));

        let notify = notification.clone();
        let hs_flag = handshake_complete.clone();

        // Both closures are stored in the WebTransportTask struct so they are
        // dropped when the task is dropped, instead of being leaked via
        // `forget()`. Previously, every reconnection/re-election cycle would
        // permanently leak two closures into WASM linear memory.
        let opened_closure = Closure::wrap(Box::new(move |_: JsValue| {
            hs_flag.set(true);
            notify.emit(WebTransportStatus::Opened);
        }) as Box<dyn FnMut(JsValue)>);

        let notify = notification.clone();
        let hs_flag_closed = handshake_complete.clone();
        let fired_closed = fired.clone();
        // `closed_closure` is shared via `Rc` because it is referenced by
        // multiple promise chains (`ready.catch`, `closed.then`, `closed.catch`).
        let closed_closure = Rc::new(Closure::wrap(Box::new(move |e: JsValue| {
            if fired_closed.replace(true) {
                return; // already emitted
            }
            let msg = e.as_string().unwrap_or_else(|| format!("{e:?}"));
            if hs_flag_closed.get() {
                notify.emit(WebTransportStatus::ClosedAfterReady(msg));
            } else {
                notify.emit(WebTransportStatus::ClosedBeforeReady(msg));
            }
        }) as Box<dyn FnMut(JsValue)>));
        let ready = transport
            .ready()
            .then(&opened_closure)
            .catch(&closed_closure);
        let closed = transport
            .closed()
            .then(&closed_closure)
            .catch(&closed_closure);

        {
            let listeners = [ready, closed];
            Ok(ConnectCommon(
                transport,
                listeners,
                opened_closure,
                closed_closure,
            ))
        }
    }
}
struct ConnectCommon(
    WebTransport,
    [Promise; 2],
    Closure<dyn FnMut(JsValue)>,
    Rc<Closure<dyn FnMut(JsValue)>>,
);

pub fn process_binary(bytes: &Uint8Array, callback: &Callback<Vec<u8>>) {
    let data = bytes.to_vec();
    callback.emit(data);
}

impl WebTransportTask {
    /// Sends data to a WebTransport connection via datagram.
    ///
    /// Datagrams are unreliable and expendable by design (heartbeats, RTT probes,
    /// diagnostics). If the writable side is already locked by a concurrent write,
    /// the packet is silently dropped instead of killing the entire transport
    /// connection. Only fatal errors (transport closed, write failure after
    /// acquiring the lock) close the transport.
    pub fn send_datagram(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let stream = transport.datagrams();
            let writable: WritableStream = stream.writable();
            if writable.locked() {
                DATAGRAM_DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                log!("datagram dropped (stream busy)");
                return;
            }
            let writer = match writable.get_writer() {
                Ok(w) => w,
                Err(e) => {
                    log!("error: ", format!("{e:?}"));
                    transport.close();
                    return;
                }
            };
            let data = Uint8Array::from(data.as_slice());
            let result = match JsFuture::from(writer.ready()).await {
                Ok(_) => JsFuture::from(writer.write_with_chunk(&data)).await,
                err => err,
            };
            writer.release_lock();
            if let Err(e) = result {
                log!(
                    "datagram write failed, closing transport:",
                    format!("{e:?}")
                );
                transport.close();
            }
        });
    }

    /// Sends a length-prefix-framed packet on a **persistent** unidirectional
    /// QUIC stream identified by `stream_key`.
    ///
    /// Phase 2 of the WebTransport freeze fix (HCL discussion #756): instead of
    /// opening a fresh QUIC stream per packet (the legacy
    /// `send_unidirectional_stream` behaviour), each media type reuses a
    /// long-lived stream.  This collapses ~80 streams/sec/sender to ~3
    /// streams/connection and eliminates the relay-side `accept_uni` storm and
    /// tokio-scheduler reorder that produced the user's five-minute WT
    /// audio+video freeze.
    ///
    /// ## Framing
    ///
    /// Every frame is written as `[u32 BE length][payload]`.  The length
    /// excludes the 4-byte header.  Both client and server are framed-only:
    /// there is no per-packet-stream fallback.
    ///
    /// The header is emitted as a single `write_with_chunk` together with the
    /// payload so the JS WritableStream cannot interleave the length prefix of
    /// one frame with the payload of another (chunks are atomic — the WebIDL
    /// spec guarantees no sub-chunk interleaving).
    ///
    /// ## Concurrency
    ///
    /// The lazy-creation path is guarded by a per-task `AsyncMutex` so that
    /// two concurrent `send_on_persistent_stream` invocations for the same
    /// key cannot both observe `None` and race to open a stream.  Once the
    /// stream exists, the WritableStream writer enforces FIFO ordering of
    /// writes internally; we do not need to hold the mutex across the
    /// `write_with_chunk` await.
    ///
    /// ## Error handling
    ///
    /// On any write error the entry for `stream_key` is removed from the
    /// map.  The next send for that key will open a fresh stream.  The
    /// transport is NOT closed — a single failed frame must not kill the
    /// session for all participants.  The receiver detects the closed stream
    /// (via EOF) and discards any partial buffer; framing guarantees that a
    /// truncated frame becomes a clean stream-closed event rather than a
    /// silently-corrupted payload.
    pub fn send_on_persistent_stream(
        transport: Rc<WebTransport>,
        streams: PersistentStreamMap,
        stream_key: u8,
        data: Vec<u8>,
    ) {
        // Frame-size and emptiness guards.  Mirrors the server-side
        // `read_length_prefixed_frame` contract: zero-length payloads are
        // treated as malformed (no legitimate caller has a reason to send
        // one), and over-large payloads are rejected up front to avoid
        // writing a bad header that would force an immediate stream restart
        // on the receiver.
        if data.is_empty() {
            log!("persistent stream send dropped: empty payload");
            return;
        }
        if data.len() > PERSISTENT_STREAM_MAX_FRAME_SIZE {
            log!(
                "persistent stream send dropped: payload exceeds max frame size,",
                data.len() as u32,
                ">",
                PERSISTENT_STREAM_MAX_FRAME_SIZE as u32
            );
            return;
        }

        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = async {
                // --- Wait for the transport handshake ------------------------
                // ready() resolves once the underlying QUIC session is
                // established.  Calling create_unidirectional_stream() before
                // ready() resolves throws.
                JsFuture::from(transport.ready())
                    .await
                    .map_err(|e| anyhow!("transport.ready() failed: {:?}", e))?;

                // --- Ensure a writer exists for this stream_key --------------
                // Lock the map across the create-or-reuse decision so two
                // concurrent senders for the same key cannot both observe
                // `None` and open duplicate streams.
                let writer = {
                    use std::collections::hash_map::Entry;
                    let mut map = streams.lock().await;
                    if let Entry::Vacant(entry) = map.entry(stream_key) {
                        let stream: WritableStream =
                            JsFuture::from(transport.create_unidirectional_stream())
                                .await
                                .map_err(|e| {
                                    anyhow!(
                                        "failed to create unidirectional stream for key {}: {:?}",
                                        stream_key,
                                        e
                                    )
                                })?
                                .unchecked_into();
                        let writer = stream
                            .get_writer()
                            .map_err(|e| anyhow!("error getting writer: {:?}", e))?;
                        entry.insert(PersistentSendStream {
                            _stream: stream,
                            writer,
                        });
                    }
                    // Clone the writer JsValue so we can release the map
                    // lock before the (potentially long) write await.
                    map.get(&stream_key)
                        .expect("entry was just inserted or already existed")
                        .writer
                        .clone()
                };

                // --- Build the framed payload --------------------------------
                // [u32 BE length][payload] in a single Uint8Array so the
                // browser cannot split the header off from its body.
                let framed = frame_persistent_stream_payload(&data);
                let chunk = Uint8Array::from(framed.as_slice());

                // --- Write the frame ----------------------------------------
                // writer.ready() resolves when there is backpressure room.
                // writer.write() returns immediately after enqueueing; the
                // browser serialises chunks from this writer in call order.
                JsFuture::from(writer.ready())
                    .await
                    .map_err(|e| anyhow!("writer.ready() failed: {:?}", e))?;
                JsFuture::from(writer.write_with_chunk(&chunk))
                    .await
                    .map_err(|e| anyhow!("write_with_chunk failed: {:?}", e))?;
                Ok(())
            }
            .await;

            if let Err(e) = result {
                // Stream is broken — remove it from the map so the next send
                // for this key opens a fresh stream.  We do this *after* the
                // map lock has been dropped (the lock is scoped above) so we
                // don't deadlock with ourselves.
                let mut map = streams.lock().await;
                map.remove(&stream_key);
                log!(
                    "persistent stream send failed (stream reset, frame dropped):",
                    e.to_string()
                );
            }
        });
    }

    /// Sends data to a WebTransport connection via a unidirectional stream.
    ///
    /// **Legacy per-packet path.** Used only as a fallback for transports that
    /// have not migrated to `send_on_persistent_stream` yet.  Phase 2 of the
    /// WebTransport freeze fix replaces this with persistent per-media-type
    /// streams; new code paths should call `send_on_persistent_stream` instead.
    ///
    /// Stream errors (creation failure, write backpressure, QUIC congestion) are
    /// transient -- they affect only this single frame send. The transport is NOT
    /// closed on failure; if the transport is genuinely dead, the reader loops and
    /// the `closed` promise will detect it independently.
    pub fn send_unidirectional_stream(transport: Rc<WebTransport>, data: Vec<u8>) {
        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = async {
                JsFuture::from(transport.ready())
                    .await
                    .map_err(|e| anyhow!("{:?}", e))?;
                let stream: WritableStream =
                    JsFuture::from(transport.create_unidirectional_stream())
                        .await
                        .map_err(|e| anyhow!("failed to create Writeable stream {:?}", e))?
                        .unchecked_into();
                let writer = stream
                    .get_writer()
                    .map_err(|e| anyhow!("Error getting writer {:?}", e))?;
                let data = Uint8Array::from(data.as_slice());
                JsFuture::from(writer.ready())
                    .await
                    .map_err(|e| anyhow!("Error getting writer ready {:?}", e))?;
                JsFuture::from(writer.write_with_chunk(&data))
                    .await
                    .map_err(|e| anyhow!("Error writing to stream: {:?}", e))?;
                writer.release_lock();
                JsFuture::from(stream.close())
                    .await
                    .map_err(|e| anyhow!("Error closing stream {:?}", e))?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants.
                log!(
                    "unidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }

    /// Sends data to a WebTransport connection via a bidirectional stream and
    /// reads the response.
    ///
    /// Stream errors are transient -- they affect only this single stream
    /// exchange. The transport is NOT closed on failure; if the transport is
    /// genuinely dead, the reader loops and the `closed` promise will detect it
    /// independently. The inner reader task will terminate naturally when the
    /// stream's readable side ends or errors out.
    pub fn send_bidirectional_stream(
        transport: Rc<WebTransport>,
        data: Vec<u8>,
        callback: Callback<Vec<u8>>,
    ) {
        wasm_bindgen_futures::spawn_local(async move {
            let result: Result<(), anyhow::Error> = {
                let transport = transport.clone();
                async move {
                    let stream = JsFuture::from(transport.create_bidirectional_stream()).await;
                    let stream: WebTransportBidirectionalStream =
                        stream.map_err(|e| anyhow!("{:?}", e))?.unchecked_into();
                    let readable: ReadableStreamDefaultReader =
                        stream.readable().get_reader().unchecked_into();
                    let (sender, receiver) = channel();
                    wasm_bindgen_futures::spawn_local(async move {
                        loop {
                            let read_result = JsFuture::from(readable.read()).await;
                            match read_result {
                                Err(e) => {
                                    // Stream read error -- log and stop reading.
                                    // Do NOT close the transport; this is a
                                    // single-stream failure.
                                    log!(
                                        "bidirectional stream read error (stopping reader):",
                                        format!("{e:?}")
                                    );
                                    break;
                                }
                                Ok(result) => {
                                    let done =
                                        match Reflect::get(&result, &JsString::from("done")) {
                                            Ok(val) => val.unchecked_into::<Boolean>(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'done' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
                                    if done.is_truthy() {
                                        break;
                                    }
                                    let value: Uint8Array =
                                        match Reflect::get(&result, &JsString::from("value")) {
                                            Ok(val) => val.unchecked_into(),
                                            Err(e) => {
                                                log!(
                                                    "Failed to read 'value' from bidi send reader result",
                                                    &e
                                                );
                                                break;
                                            }
                                        };
                                    process_binary(&value, &callback);
                                }
                            }
                        }
                        let _ = sender.send(true);
                    });
                    let writer = stream
                        .writable()
                        .get_writer()
                        .map_err(|e| anyhow!("{:?}", e))?;

                    JsFuture::from(writer.ready())
                        .await
                        .map_err(|e| anyhow!("{:?}", e))?;
                    let data = Uint8Array::from(data.as_slice());
                    let _ = JsFuture::from(writer.write_with_chunk(&data))
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    JsFuture::from(writer.close())
                        .await
                        .map_err(|e| anyhow::anyhow!("{:?}", e))?;
                    let _ = receiver.await?;
                    Ok(())
                }
            }
            .await;
            if let Err(e) = result {
                // Transient stream error -- log and drop the packet. Do NOT
                // close the transport; a single failed frame should not kill
                // the entire connection for all participants. The inner reader
                // task (if spawned) will terminate when the stream ends.
                log!(
                    "bidirectional stream send failed (frame dropped):",
                    e.to_string()
                );
            }
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests for the length-prefix framing protocol.
//
// The framing helpers (`frame_persistent_stream_payload` and
// `parse_persistent_stream_frame`) are pure-Rust and run on the host target.
// The WebTransport send path itself is WASM-only (it depends on the JS
// WritableStream API) and is exercised by integration tests rather than
// unit tests.
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod framing_tests {
    use super::*;

    #[test]
    fn frame_round_trips_byte_for_byte() {
        let payloads: Vec<Vec<u8>> = vec![
            vec![0x00],
            vec![0xFF; 1],
            (0u8..=255).collect(),
            b"the quick brown fox jumps over the lazy dog".to_vec(),
            vec![0xAA; 1500],
            vec![0x55; 64 * 1024],
        ];
        for payload in &payloads {
            let framed = frame_persistent_stream_payload(payload);
            assert_eq!(framed.len(), 4 + payload.len(), "framed length wrong");
            let (parsed, rest) =
                parse_persistent_stream_frame(&framed).expect("frame should parse back cleanly");
            assert_eq!(parsed, payload.as_slice(), "round-trip payload differs");
            assert!(rest.is_empty(), "no trailing bytes expected");
        }
    }

    #[test]
    fn parses_thousand_concatenated_frames_in_order() {
        // Simulate 1000 framed packets accumulated on the wire (the scenario
        // where the JS chunk boundary does not align with frame boundaries).
        // The parser must extract every payload in order and with no
        // length-prefix corruption.
        const N: usize = 1000;
        let mut originals: Vec<Vec<u8>> = Vec::with_capacity(N);
        let mut buffer: Vec<u8> = Vec::new();
        for i in 0..N {
            // Mix of small, medium, and occasionally larger frames to exercise
            // the parser at different boundary alignments.
            let len = match i % 5 {
                0 => 1,
                1 => 80,        // typical Opus audio frame size
                2 => 1200,      // datagram-MTU-sized
                3 => 8 * 1024,  // video delta range
                _ => 64 * 1024, // small keyframe range
            };
            let payload: Vec<u8> = (0..len).map(|j| ((i + j) & 0xFF) as u8).collect();
            buffer.extend_from_slice(&frame_persistent_stream_payload(&payload));
            originals.push(payload);
        }

        // Walk the buffer one frame at a time.  We deliberately use the
        // returned `rest` slice as the next iteration's input so that any
        // off-by-one bug in the consumed-byte count surfaces here.
        let mut cursor: &[u8] = &buffer;
        for (idx, expected) in originals.iter().enumerate() {
            let (parsed, rest) = parse_persistent_stream_frame(cursor)
                .unwrap_or_else(|e| panic!("frame {idx} failed to parse: {e:?}"));
            assert_eq!(parsed, expected.as_slice(), "frame {idx} payload mismatch");
            cursor = rest;
        }
        assert!(cursor.is_empty(), "all bytes should be consumed");
    }

    #[test]
    fn need_more_header_when_buffer_short() {
        for short in 0..4 {
            let buf = vec![0u8; short];
            assert_eq!(
                parse_persistent_stream_frame(&buf),
                Err(FrameParseError::NeedMoreHeader),
            );
        }
    }

    #[test]
    fn need_more_payload_when_body_short() {
        // Header claims 100 bytes but only 50 are present.
        let mut buf = (100u32).to_be_bytes().to_vec();
        buf.extend(std::iter::repeat_n(0u8, 50));
        match parse_persistent_stream_frame(&buf) {
            Err(FrameParseError::NeedMorePayload { missing }) => {
                assert_eq!(missing, 50);
            }
            other => panic!("expected NeedMorePayload, got {other:?}"),
        }
    }

    #[test]
    fn zero_length_is_invalid() {
        let buf = (0u32).to_be_bytes();
        assert_eq!(
            parse_persistent_stream_frame(&buf),
            Err(FrameParseError::InvalidLength(0)),
        );
    }

    #[test]
    fn oversized_length_is_invalid() {
        // One byte over the limit must be rejected.
        let too_big = (PERSISTENT_STREAM_MAX_FRAME_SIZE + 1) as u32;
        let mut buf = too_big.to_be_bytes().to_vec();
        // Pad with bogus bytes; the length check fires before we look at the body.
        buf.extend(std::iter::repeat_n(0u8, 8));
        assert_eq!(
            parse_persistent_stream_frame(&buf),
            Err(FrameParseError::InvalidLength(
                PERSISTENT_STREAM_MAX_FRAME_SIZE + 1
            )),
        );
    }

    #[test]
    fn max_size_payload_is_accepted() {
        // A payload at exactly the max size must round-trip.  We use a small
        // pattern so the test stays fast; the size is what we are validating.
        let payload = vec![0xC3u8; PERSISTENT_STREAM_MAX_FRAME_SIZE];
        let framed = frame_persistent_stream_payload(&payload);
        let (parsed, rest) =
            parse_persistent_stream_frame(&framed).expect("max-size frame must parse");
        assert_eq!(parsed.len(), PERSISTENT_STREAM_MAX_FRAME_SIZE);
        assert!(rest.is_empty());
    }

    /// Property: interleaving the wire bytes of two senders that each wrote
    /// `[len][payload]` as a single chunk must never decode into a corrupt
    /// frame.  The JS WritableStream guarantees no sub-chunk interleaving,
    /// so on the wire we only ever see fully-concatenated frames.  This
    /// test asserts that the *parser* respects that invariant: any input
    /// that is two adjacent valid frames decodes back to those two
    /// payloads exactly.
    #[test]
    fn two_adjacent_frames_decode_to_two_payloads() {
        let a: Vec<u8> = (0u8..=99).collect();
        let b: Vec<u8> = (100u8..=199).collect();
        let mut wire = frame_persistent_stream_payload(&a);
        wire.extend_from_slice(&frame_persistent_stream_payload(&b));

        let (got_a, rest) = parse_persistent_stream_frame(&wire).unwrap();
        assert_eq!(got_a, a.as_slice());
        let (got_b, rest2) = parse_persistent_stream_frame(rest).unwrap();
        assert_eq!(got_b, b.as_slice());
        assert!(rest2.is_empty());
    }

    /// Stream-restart simulation at the protocol level.
    ///
    /// The real WT stream-restart path lives behind the JS WebTransport API
    /// and is not unit-testable.  What *is* testable is the invariant the
    /// restart relies on: a truncated frame (write failed mid-write, stream
    /// reset on the wire) must not be silently consumed by the parser as if
    /// it were a complete frame.  We assert that the parser reports
    /// "NeedMorePayload" — the receiver then sees EOF on the stream and
    /// discards the partial buffer.  When the sender opens a fresh stream
    /// for the next send, the receiver starts a new buffer.  The framing
    /// protocol is what makes this clean.
    #[test]
    fn truncated_frame_is_detected_not_silently_consumed() {
        let payload = vec![0xABu8; 500];
        let framed = frame_persistent_stream_payload(&payload);

        // Drop the last byte to simulate a mid-frame stream reset.
        let truncated = &framed[..framed.len() - 1];

        match parse_persistent_stream_frame(truncated) {
            Err(FrameParseError::NeedMorePayload { missing }) => {
                assert_eq!(missing, 1, "must report the exact shortfall");
            }
            other => panic!("truncated frame must surface as NeedMorePayload, got {other:?}"),
        }
    }
}
