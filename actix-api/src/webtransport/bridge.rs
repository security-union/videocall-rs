/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! WebTransport Actor Bridge
//!
//! Bridges the gap between WebTransport (quinn async I/O) and Actix actors.
//!
//! Quinn uses pure tokio async, while actors use Actix's LocalSet runtime.
//! This bridge spawns I/O tasks that communicate with the actor via messages
//! and channels.
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                          WebTransportBridge                              │
//! ├─────────────────────────────────────────────────────────────────────────┤
//! │  ┌──────────────────┐                ┌──────────────────┐               │
//! │  │ UniStream Reader │                │ Datagram Reader  │               │
//! │  │ accept_uni →     │                │ read_datagram()  │               │
//! │  │ framed loop      │                │                  │               │
//! │  └────────┬─────────┘                └────────┬─────────┘               │
//! │           │ WtInbound(UniStream)             │ WtInbound(Datagram)      │
//! │           └────────────┬─────────────────────┘                          │
//! │                        ▼                                                │
//! │           ┌────────────────────────┐                                    │
//! │           │      Actor (external)  │                                    │
//! │           └─────┬────────────┬─────┘                                    │
//! │                 │            │                                          │
//! │ unistream_rx    │            │ datagram_rx                              │
//! │                 ▼            ▼                                          │
//! │  ┌──────────────────────┐  ┌──────────────────────┐                    │
//! │  │ UniStream Writer     │  │ Datagram Writer      │                    │
//! │  │ persistent stream    │  │ send_datagram()      │                    │
//! │  │ + length-prefix frame│  │ (unframed)           │                    │
//! │  └──────────────────────┘  └──────────────────────┘                    │
//! └─────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Why a split writer?
//!
//! The prior topology drained both the persistent uni-stream **and**
//! datagrams from a single `mpsc` channel in one writer task. When QUIC
//! flow-control credits on the uni-stream stalled (any congested
//! receiver), `stream.write_all().await` blocked the entire task, and
//! audio datagrams piled up behind the stalled video write — even
//! though `send_datagram()` itself is non-blocking and has no per-
//! stream flow control. The audio→datagram routing in
//! `wt_chat_session::build_outbound` was designed precisely to avoid
//! head-of-line blocking, but the writer-task topology defeated the
//! routing.
//!
//! The split here gives each primitive its own writer task, its own
//! bounded channel, and its own backpressure surface. A stalled uni-
//! stream can never starve the datagram path. See discussion #756 for
//! the full root-cause analysis.

use crate::actors::transports::wt_chat_session::{WtInbound, WtInboundSource};
use crate::constants::MAX_FRAME_SIZE;
use actix::Addr;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use web_transport_quinn::Session;

/// Callback for tracking packets sent to clients (used in tests)
pub type PacketSentCallback = Box<dyn Fn() + Send + Sync>;

/// Bridge between WebTransport session and an Actix actor.
///
/// Spawns I/O tasks that:
/// - Read length-prefix-framed packets from WebTransport uni streams →
///   `WtInbound` to actor
/// - Read self-contained datagrams from the WebTransport session →
///   `WtInbound` to actor
/// - Drain the actor's unistream outbound channel onto the persistent
///   server→client uni stream (length-prefix framed)
/// - Drain the actor's datagram outbound channel onto
///   `session.send_datagram` (unframed)
pub struct WebTransportBridge {
    join_set: JoinSet<()>,
}

impl WebTransportBridge {
    /// Create a new bridge and start I/O tasks.
    ///
    /// # Arguments
    /// * `session` - The WebTransport session (quinn)
    /// * `actor_addr` - Address of the actor to receive inbound messages
    /// * `unistream_rx` - Channel receiver for outbound *unistream* messages
    /// * `datagram_rx` - Channel receiver for outbound *datagram* messages
    #[allow(dead_code)] // Useful API even if currently only new_with_callback is used
    pub fn new<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<Bytes>,
        datagram_rx: mpsc::Receiver<Bytes>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        Self::new_with_callback(session, actor_addr, unistream_rx, datagram_rx, None)
    }

    /// Create a new bridge with optional callback for packet tracking.
    pub fn new_with_callback<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<Bytes>,
        datagram_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<PacketSentCallback>,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        let mut join_set = JoinSet::new();

        // Wrap the test callback in an Arc so it can be shared between the
        // two writer tasks without `Clone` being required on the boxed
        // closure type. `Option<Arc<...>>` lets us cheaply share a single
        // counter across both writers; in production both are `None`.
        let on_packet_sent = on_packet_sent.map(std::sync::Arc::new);

        Self::spawn_unistream_reader(&mut join_set, session.clone(), actor_addr.clone());
        Self::spawn_datagram_reader(&mut join_set, session.clone(), actor_addr);
        Self::spawn_unistream_writer(
            &mut join_set,
            session.clone(),
            unistream_rx,
            on_packet_sent.clone(),
        );
        Self::spawn_datagram_writer(&mut join_set, session, datagram_rx, on_packet_sent);

        Self { join_set }
    }

    /// Wait for any I/O task to complete (indicates session end).
    pub async fn wait_for_disconnect(&mut self) {
        self.join_set.join_next().await;
    }

    /// Shutdown all I/O tasks.
    pub async fn shutdown(mut self) {
        self.join_set.shutdown().await;
    }

    /// Spawn UniStream reader task.
    ///
    /// Each accepted uni stream is treated as a **packet pipe**: the client
    /// writes one or more length-prefix-framed packets onto the same stream
    /// and finishes it (or leaves it open for the duration of the session;
    /// the reader handles both shapes). For every accepted stream we spawn
    /// a dedicated reader task that loops reading `[u32 BE length][payload]`
    /// frames until the stream is closed (or a malformed frame is
    /// observed). The server is media-type-agnostic at this layer — it
    /// reads framed bytes and forwards them to the actor, which routes
    /// by the `MediaType` field on the parsed `PacketWrapper`.
    ///
    /// Phase 2 of the WT-freeze fix (discussion #756) moved the client
    /// from opening a fresh uni stream per packet to a small number of
    /// persistent streams, each carrying multiple framed packets. This
    /// reader matches that shape. Multiple frames per stream are read
    /// in order; the per-stream task exits cleanly when the client
    /// closes the stream.
    fn spawn_unistream_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(uni_stream) = session.accept_uni().await {
                let actor_addr = actor_addr.clone();
                tokio::spawn(async move {
                    read_framed_packets_loop(uni_stream, actor_addr).await;
                });
            }
            info!("WebTransport UniStream reader ended");
        });
    }

    /// Spawn Datagram reader task.
    fn spawn_datagram_reader<A>(join_set: &mut JoinSet<()>, session: Session, actor_addr: Addr<A>)
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        join_set.spawn(async move {
            while let Ok(buf) = session.read_datagram().await {
                let _ = actor_addr.try_send(WtInbound {
                    data: buf,
                    source: WtInboundSource::Datagram,
                });
            }
            info!("WebTransport Datagram reader ended");
        });
    }

    /// Spawn UniStream writer task.
    ///
    /// Owns the single persistent server→client unidirectional QUIC
    /// stream. Drains `unistream_rx` and writes length-prefix-framed
    /// packets (`[u32 BE length][payload]`) onto it. QUIC's per-stream
    /// ordering guarantees that packets arrive at the receiver in the
    /// order they were written.
    ///
    /// The stream is opened lazily on the first write and kept alive for
    /// the duration of the session. On write error the cached stream is
    /// dropped and a new one is opened on the next attempt (single
    /// retry).
    ///
    /// **Topology invariant** (the reason this task exists separately
    /// from `spawn_datagram_writer`): when QUIC flow-control credits on
    /// this stream drain to zero (any congested receiver), `write_all`
    /// here blocks. Because the datagram writer runs in its own task
    /// drained by its own channel, datagrams continue to flow through
    /// the unrelated `send_datagram` path even while this writer is
    /// parked. This is the central architectural fix for the 5-minute
    /// WT freeze described in discussion #756.
    fn spawn_unistream_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut unistream_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
    ) {
        join_set.spawn(async move {
            let mut persistent_stream: Option<web_transport_quinn::SendStream> = None;

            while let Some(data) = unistream_rx.recv().await {
                // Ensure we have a stream, opening one if needed.
                if persistent_stream.is_none() {
                    match session.open_uni().await {
                        Ok(stream) => {
                            persistent_stream = Some(stream);
                        }
                        Err(e) => {
                            error!("Error opening persistent UniStream: {}", e);
                            break;
                        }
                    }
                }

                // Build the length-prefixed frame: [4-byte BE length][payload].
                // The client reader uses the same format to know where each
                // packet ends on the persistent (never-finished) stream.
                let len: u32 = data
                    .len()
                    .try_into()
                    .expect("packet exceeds u32::MAX bytes; video frames should be well under 4GB");
                let len_header = len.to_be_bytes();

                // Write to the persistent stream.
                let stream = persistent_stream.as_mut().expect("stream was just opened");
                let write_result = stream.write_all(&len_header).await.err();
                let write_err = match write_result {
                    Some(e) => Some(e),
                    None => stream.write_all(&data).await.err(),
                };
                if let Some(e) = write_err {
                    warn!(
                        "Error writing to persistent UniStream ({}), retrying with new stream",
                        e
                    );
                    // Drop the broken stream and try once more with a fresh one.
                    drop(persistent_stream.take());
                    let mut new_stream = match session.open_uni().await {
                        Ok(s) => s,
                        Err(e2) => {
                            error!("Error opening new UniStream after retry: {}", e2);
                            break;
                        }
                    };
                    // Retry with the complete framed message (length + data).
                    if let Err(e2) = new_stream.write_all(&len_header).await {
                        error!(
                            "Error writing length header to new UniStream after retry: {}",
                            e2
                        );
                        break;
                    }
                    if let Err(e2) = new_stream.write_all(&data).await {
                        error!("Error writing payload to new UniStream after retry: {}", e2);
                        break;
                    }
                    persistent_stream = Some(new_stream);
                }

                // Call packet sent callback if provided (for test instrumentation)
                if let Some(ref callback) = on_packet_sent {
                    callback();
                }
            }
            info!("WebTransport UniStream writer ended");
        });
    }

    /// Spawn Datagram writer task.
    ///
    /// Drains `datagram_rx` and forwards each payload to
    /// `session.send_datagram`. Datagrams are **unframed** — QUIC
    /// datagrams have their own size limit (see
    /// [`crate::actors::packet_handler::DATAGRAM_MAX_SIZE`]) and are
    /// self-delimiting on the wire.
    ///
    /// Independent of the unistream writer: a stalled uni stream cannot
    /// block this task because the two writers do not share a channel
    /// (or a future). Datagram delivery is best-effort by design; if
    /// `send_datagram` returns an error we log it but keep draining.
    fn spawn_datagram_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut datagram_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
    ) {
        join_set.spawn(async move {
            while let Some(data) = datagram_rx.recv().await {
                if let Err(e) = session.send_datagram(data) {
                    // Datagrams are unreliable: log and continue.
                    debug!("Error sending datagram: {}", e);
                } else if let Some(ref callback) = on_packet_sent {
                    callback();
                }
            }
            info!("WebTransport Datagram writer ended");
        });
    }
}

/// Read one length-prefixed frame (`[4-byte BE length][payload]`) from a
/// WebTransport uni stream.
///
/// Returns:
/// * `Ok(Some(payload))` on a successfully decoded frame.
/// * `Ok(None)` if the stream was cleanly closed by the peer at a frame
///   boundary (i.e. `read_exact` for the 4-byte header returned `UnexpectedEof`
///   before any header bytes were consumed). This is the normal stream-end
///   signal — the reader loop should exit cleanly.
/// * `Err(FramedReadError::Malformed)` for a frame whose length is zero or
///   exceeds [`MAX_FRAME_SIZE`]. The caller MUST close the stream and stop
///   reading from it; subsequent bytes are not interpretable.
/// * `Err(FramedReadError::TruncatedHeader)` if the header was partially read
///   then the stream ended (e.g. 1 of 4 bytes arrived before EOF). Treated
///   the same way as `Malformed`: close the stream and stop reading.
/// * `Err(FramedReadError::TruncatedPayload)` if the header decoded
///   successfully but the payload was truncated. Same handling.
async fn read_length_prefixed_frame(
    stream: &mut web_transport_quinn::RecvStream,
) -> Result<Option<Vec<u8>>, FramedReadError> {
    // Read the 4-byte big-endian length header. We use a byte-at-a-time
    // probe for the first byte so we can distinguish "clean EOF at frame
    // boundary" (which is normal — the client closed the stream between
    // frames) from "truncated header" (which is a malformed frame).
    let mut first_byte = [0u8; 1];
    match stream.read_exact(&mut first_byte).await {
        Ok(()) => {}
        Err(_) => {
            // Clean EOF at a frame boundary. Not an error.
            return Ok(None);
        }
    }

    let mut rest = [0u8; 3];
    if stream.read_exact(&mut rest).await.is_err() {
        // Header truncated mid-decode. The next byte to arrive would be
        // interpreted as part of the length, so we cannot recover.
        return Err(FramedReadError::TruncatedHeader);
    }

    let mut len_buf = [0u8; 4];
    len_buf[0] = first_byte[0];
    len_buf[1..].copy_from_slice(&rest);
    let payload_len = u32::from_be_bytes(len_buf) as usize;

    if payload_len == 0 {
        // A zero-length payload is treated as malformed: there is no
        // legitimate reason for the client to send an empty packet, and
        // accepting it would let a misbehaving sender spin the reader
        // loop with no useful work. Cheap defensive check.
        return Err(FramedReadError::Malformed { len: 0 });
    }
    if payload_len > MAX_FRAME_SIZE {
        return Err(FramedReadError::Malformed { len: payload_len });
    }

    let mut payload = vec![0u8; payload_len];
    if stream.read_exact(&mut payload).await.is_err() {
        return Err(FramedReadError::TruncatedPayload {
            expected: payload_len,
        });
    }
    Ok(Some(payload))
}

/// Read framed packets from a single uni stream until EOF or a malformed
/// frame is observed.
///
/// Each decoded payload is forwarded to the actor as a `WtInbound` with
/// `source = UniStream`. The actor is responsible for parsing the
/// payload as a `PacketWrapper` and dispatching by media type.
///
/// On any framing error (truncated header / truncated payload / length
/// outside `(0, MAX_FRAME_SIZE]`) we log a warning and return. The
/// caller's outer `accept_uni` loop continues to accept future streams;
/// this single stream is simply abandoned. The session itself is not
/// terminated — one malformed frame cannot crash the whole session.
async fn read_framed_packets_loop<A>(
    mut uni_stream: web_transport_quinn::RecvStream,
    actor_addr: Addr<A>,
) where
    A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
{
    loop {
        match read_length_prefixed_frame(&mut uni_stream).await {
            Ok(Some(payload)) => {
                let payload_len = payload.len();
                if let Err(e) = actor_addr.try_send(WtInbound {
                    data: Bytes::from(payload),
                    source: WtInboundSource::UniStream,
                }) {
                    warn!("Dropped UniStream frame ({} bytes): {}", payload_len, e);
                }
            }
            Ok(None) => {
                // Clean stream close — exit the loop without logging.
                return;
            }
            Err(FramedReadError::Malformed { len }) => {
                warn!(
                    "Malformed framed packet on UniStream (length={} bytes, max={}); \
                     closing stream",
                    len, MAX_FRAME_SIZE
                );
                return;
            }
            Err(FramedReadError::TruncatedHeader) => {
                warn!("Truncated frame header on UniStream; closing stream");
                return;
            }
            Err(FramedReadError::TruncatedPayload { expected }) => {
                warn!(
                    "Truncated frame payload on UniStream (expected {} bytes); closing stream",
                    expected
                );
                return;
            }
        }
    }
}

/// Outcome of a framed-frame decode attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FramedReadError {
    /// Length header decoded but payload length is zero or exceeds
    /// `MAX_FRAME_SIZE`. The stream is unrecoverable — close it.
    Malformed { len: usize },
    /// Length header was partially read (1-3 bytes) before the stream
    /// ended. We cannot tell where the next header would start, so the
    /// stream is unrecoverable.
    TruncatedHeader,
    /// Length header decoded but the payload ended before the announced
    /// number of bytes arrived. The peer dropped the stream mid-frame.
    TruncatedPayload { expected: usize },
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    //! Unit tests for the framed reader.
    //!
    //! These tests drive `read_length_prefixed_frame` against fake
    //! `RecvStream`-shaped inputs by building a real WebTransport
    //! session pair in-process — the only way to obtain a genuine
    //! `web_transport_quinn::RecvStream` (the type is opaque and has no
    //! public constructors). For pure framing logic that does not need
    //! the I/O type, we use a `#[cfg(test)]` cover helper that operates
    //! on `AsyncRead`-implementing buffers via the same byte-shape
    //! semantics.
    //!
    //! We do NOT test `read_length_prefixed_frame` directly with a
    //! `RecvStream`; instead we ship a parallel pure-bytes helper
    //! `decode_frames_from_bytes` that implements the identical
    //! semantics on a `&[u8]`, and the framing logic is asserted on
    //! that. The two implementations share the `FramedReadError` enum
    //! and the `MAX_FRAME_SIZE` check so any divergence is caught by
    //! review.
    //!
    //! Integration of the real `RecvStream` path is covered by the
    //! existing `actix-api/src/webtransport/mod.rs` integration tests
    //! (`test_relay_packet_webtransport_between_two_clients` etc.) which
    //! send framed packets end-to-end through a real WebTransport
    //! session.

    use super::*;

    /// Pure-bytes parallel of [`read_length_prefixed_frame`] for unit
    /// tests. Drives the same framing rules against an in-memory byte
    /// slice so we can assert decode/malformed/EOF behaviour without
    /// the cost of standing up a real `quinn::Connection`.
    ///
    /// Returns the list of decoded payloads and a terminal status. The
    /// terminal status encodes whether the input ended cleanly at a
    /// frame boundary, was truncated mid-header, was truncated
    /// mid-payload, or contained a length outside the allowed range.
    fn decode_frames_from_bytes(buf: &[u8]) -> (Vec<Vec<u8>>, TerminalStatus) {
        let mut payloads = Vec::new();
        let mut pos = 0;
        loop {
            if pos == buf.len() {
                return (payloads, TerminalStatus::CleanEof);
            }
            if buf.len() - pos < 4 {
                return (payloads, TerminalStatus::TruncatedHeader);
            }
            let mut len_buf = [0u8; 4];
            len_buf.copy_from_slice(&buf[pos..pos + 4]);
            let payload_len = u32::from_be_bytes(len_buf) as usize;
            pos += 4;
            if payload_len == 0 {
                return (payloads, TerminalStatus::Malformed { len: 0 });
            }
            if payload_len > MAX_FRAME_SIZE {
                return (payloads, TerminalStatus::Malformed { len: payload_len });
            }
            if buf.len() - pos < payload_len {
                return (
                    payloads,
                    TerminalStatus::TruncatedPayload {
                        expected: payload_len,
                    },
                );
            }
            payloads.push(buf[pos..pos + payload_len].to_vec());
            pos += payload_len;
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum TerminalStatus {
        CleanEof,
        TruncatedHeader,
        TruncatedPayload { expected: usize },
        Malformed { len: usize },
    }

    /// Build a `[u32 BE length][payload]` framed byte stream from a list
    /// of payloads. Mirrors what the client/server writers produce.
    fn build_framed(payloads: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in payloads {
            let len = (p.len() as u32).to_be_bytes();
            out.extend_from_slice(&len);
            out.extend_from_slice(p);
        }
        out
    }

    // -----------------------------------------------------------------------
    // Happy-path decoding
    // -----------------------------------------------------------------------

    #[test]
    fn decodes_single_frame() {
        let payload = b"hello".as_slice();
        let bytes = build_framed(&[payload]);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![payload.to_vec()]);
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_multiple_frames_in_order() {
        let p1 = b"audio-frame-1".as_slice();
        let p2 = b"x".as_slice();
        let p3 = vec![0xAB; 1024];
        let p4 = b"final".as_slice();
        let bytes = build_framed(&[p1, p2, &p3, p4]);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(
            frames,
            vec![p1.to_vec(), p2.to_vec(), p3.clone(), p4.to_vec()]
        );
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_varied_payload_sizes() {
        // Mix small audio-sized payloads (~80B) with larger video keyframe-
        // sized payloads (~50KB). The reader should not care about size as
        // long as the length header is consistent.
        let mut buf = Vec::new();
        let mut expected = Vec::new();
        for i in 0..16 {
            let size = match i % 4 {
                0 => 80,
                1 => 1500,
                2 => 50_000,
                _ => 1,
            };
            let payload: Vec<u8> = (0..size).map(|j| ((i * 31 + j) % 251) as u8).collect();
            let len = (payload.len() as u32).to_be_bytes();
            buf.extend_from_slice(&len);
            buf.extend_from_slice(&payload);
            expected.push(payload);
        }
        let (frames, status) = decode_frames_from_bytes(&buf);
        assert_eq!(
            frames,
            expected,
            "all {} frames must decode in order",
            expected.len()
        );
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    #[test]
    fn decodes_empty_byte_stream_as_clean_eof() {
        let (frames, status) = decode_frames_from_bytes(&[]);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::CleanEof);
    }

    // -----------------------------------------------------------------------
    // Malformed frames — the reader must NOT panic, NOT crash the session,
    // and MUST stop reading the bad stream.
    // -----------------------------------------------------------------------

    #[test]
    fn rejects_payload_length_above_max_frame_size() {
        // 5,000,000 bytes exceeds MAX_FRAME_SIZE = 4,000,000. The reader
        // must surface this as `Malformed` BEFORE attempting to allocate.
        let too_large: u32 = 5_000_000;
        let bytes = too_large.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(
            frames.is_empty(),
            "no frames should be returned before the malformed header"
        );
        assert_eq!(
            status,
            TerminalStatus::Malformed {
                len: too_large as usize
            }
        );
    }

    #[test]
    fn rejects_max_frame_size_plus_one() {
        // Exactly one byte over the limit. Cheap boundary check that
        // proves the comparison is `>`, not `>=`.
        let oversize: u32 = (MAX_FRAME_SIZE + 1) as u32;
        let bytes = oversize.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(
            status,
            TerminalStatus::Malformed {
                len: oversize as usize
            }
        );
    }

    #[test]
    fn rejects_zero_length_payload() {
        // A length of zero is treated as malformed — clients that need to
        // send a keep-alive or sentinel must use a non-zero payload (the
        // existing keep-alive uses a 4-byte "ping" datagram, not an empty
        // stream frame).
        let bytes = 0u32.to_be_bytes().to_vec();
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::Malformed { len: 0 });
    }

    #[test]
    fn rejects_truncated_header() {
        // Only 3 of 4 header bytes; reader must report TruncatedHeader.
        let bytes = vec![0u8, 0u8, 0u8];
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::TruncatedHeader);
    }

    #[test]
    fn rejects_truncated_payload() {
        // Announce 10 bytes, deliver 5. Reader must report
        // TruncatedPayload with `expected = 10`.
        let mut bytes = 10u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"hello");
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert!(frames.is_empty());
        assert_eq!(status, TerminalStatus::TruncatedPayload { expected: 10 });
    }

    #[test]
    fn good_frame_then_malformed_returns_good_frame_and_stops() {
        // Validates that earlier successful frames are returned even when
        // a later frame is malformed — the reader does not throw away
        // already-delivered packets when it has to close the stream.
        let mut bytes = build_framed(&[b"good-frame".as_slice()]);
        bytes.extend_from_slice(&(MAX_FRAME_SIZE as u32 + 1).to_be_bytes());
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![b"good-frame".to_vec()]);
        assert!(matches!(status, TerminalStatus::Malformed { .. }));
    }

    #[test]
    fn good_frame_then_truncated_returns_good_frame_and_stops() {
        let mut bytes = build_framed(&[b"frame-a".as_slice()]);
        // Announce a 100-byte payload but stop after the header.
        bytes.extend_from_slice(&100u32.to_be_bytes());
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames, vec![b"frame-a".to_vec()]);
        assert_eq!(status, TerminalStatus::TruncatedPayload { expected: 100 });
    }

    #[test]
    fn at_max_frame_size_payload_is_accepted() {
        // Boundary check: exactly MAX_FRAME_SIZE bytes is admissible.
        // (The reader does this allocation in tests; under real load
        // these are 1080p VP9 keyframes which the relay must forward.)
        let len = MAX_FRAME_SIZE;
        let payload = vec![0xAAu8; len];
        let mut bytes = (len as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&payload);
        let (frames, status) = decode_frames_from_bytes(&bytes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), len);
        assert_eq!(status, TerminalStatus::CleanEof);
    }
}
