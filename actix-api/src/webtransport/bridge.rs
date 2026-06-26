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
use crate::constants::{
    MAX_FRAME_SIZE, WT_UNISTREAM_BACKPRESSURE_POLL, WT_UNISTREAM_BACKPRESSURE_SHED_RATIO,
    WT_UNISTREAM_WRITE_DEADLINE,
};
use crate::metrics::{RELAY_INBOUND_BRIDGE_DROPS_TOTAL, RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL};
use actix::Addr;
use bytes::Bytes;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use web_transport_quinn::Session;

/// WebTransport/HTTP/3 application error code used when the relay RESETS a
/// wedged persistent server→client uni stream (#1638).
///
/// The code is informational only — the client treats any reset of an inbound
/// uni stream as an EOF/error on that stream and discards any partial frame
/// (see `videocall-client`'s `handle_unidirectional_stream`), then accepts the
/// freshly re-opened stream and resyncs at a clean frame boundary. We use a
/// non-zero sentinel so the reset is distinguishable on the wire from a clean
/// `finish()` (code 0) for anyone inspecting QUIC traces.
const UNISTREAM_SHED_RESET_CODE: u32 = 1;

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
                let len = buf.len();
                // #1146: this is the WT audio/control path. Previously the
                // try_send result was discarded (`let _ =`), so an inbound
                // mailbox overflow here was completely invisible. Count every
                // drop; keep the per-drop log at debug since datagrams are
                // high-rate (the counter is the durable, alertable signal).
                if let Err(e) = actor_addr.try_send(WtInbound {
                    data: buf,
                    source: WtInboundSource::Datagram,
                }) {
                    RELAY_INBOUND_BRIDGE_DROPS_TOTAL
                        .with_label_values(&["webtransport", "datagram"])
                        .inc();
                    debug!("Dropped inbound WT datagram ({} bytes): {}", len, e);
                }
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
    /// the duration of the session.
    ///
    /// **Topology invariant** (the reason this task exists separately
    /// from `spawn_datagram_writer`): when QUIC flow-control credits on
    /// this stream drain to zero (any congested receiver), `write_all`
    /// here blocks. Because the datagram writer runs in its own task
    /// drained by its own channel, datagrams continue to flow through
    /// the unrelated `send_datagram` path even while this writer is
    /// parked. This is the central architectural fix for the 5-minute
    /// WT freeze described in discussion #756.
    ///
    /// **Backpressure-gated shed** (#1638 — "#979 part 2"): a parked write is
    /// shed (RESET the wedged stream, RE-OPEN a fresh persistent stream) ONLY when
    /// the outbound channel is genuinely backing up — never merely because a
    /// single write took a long wall-clock time. Two paths end in a reset, and
    /// they recover OPPOSITELY (the current frame is DROPPED on the timeout path
    /// but RE-SENT on the error path):
    ///
    /// * **Stalled-while-backed-up (stream alive but flow-control-wedged).** A
    ///   slow receiver stops granting QUIC credits, so `write_all` parks. While
    ///   it is parked the writer polls the channel depth every
    ///   [`WT_UNISTREAM_BACKPRESSURE_POLL`]; a tick spent at or above
    ///   [`WT_UNISTREAM_BACKPRESSURE_SHED_RATIO`] full advances a
    ///   stalled-while-backed-up accumulator, and the stream is reset once that
    ///   accumulator reaches [`WT_UNISTREAM_WRITE_DEADLINE`]. A tick spent BELOW
    ///   the ratio (healthy / draining) RESETS the accumulator. So the shed fires
    ///   iff the 512-deep `unistream_tx` is actually filling because this writer
    ///   (its only consumer) is parked and `try_send` is starting to return
    ///   `Full` for publishers targeting that receiver — the #1631 M1 cascade.
    ///   Counted as `reason="write_timeout"`. The current frame is **DROPPED** (it
    ///   would re-wedge the fresh stream against the same starved receiver — see
    ///   below); the packet-sent callback does NOT fire for it.
    /// * **Write error (stream already torn down).** The stream returned an I/O
    ///   error — it is genuinely broken (not merely slow). The pre-existing
    ///   single-retry recovery, independent of backpressure: the fresh stream
    ///   starts clean, so the complete frame (`len_header` + `data`) is **RE-SENT**
    ///   on it and, on success, DELIVERED — the packet-sent callback DOES fire (the
    ///   frame is counted, not dropped). A second error terminates the writer.
    ///   Counted as `reason="write_error"`.
    ///
    /// **Why the gate, and why it cannot spuriously reset a healthy stream:** the
    /// relay's WT actor runs on a SINGLE-THREADED runtime. Under CPU/scheduling
    /// starvation the one thread may not POLL a write future for a long
    /// wall-clock interval even when QUIC credits are available — the executor
    /// was busy, NOT the receiver's downlink. The v1 cut used a bare per-frame
    /// `tokio::time::timeout` on wall-clock and therefore reset HEALTHY streams
    /// under that starvation (it failed `test_lobby_isolation`). A healthy
    /// low-traffic stream's channel never reaches
    /// [`WT_UNISTREAM_BACKPRESSURE_SHED_RATIO`] (the writer drains every frame
    /// promptly, so depth hovers near 0), so its accumulator never advances and
    /// it is NEVER reset — a slow poll just delays it. The accumulator advances
    /// only when the channel is sustainedly backed up, which only happens when
    /// the receiver isn't draining. That is exactly the invariant: reset iff the
    /// channel is actually wedging, never merely because the executor was slow.
    ///
    /// Why the CURRENT frame is dropped on the **timeout** path but RE-SENT on the
    /// **error** path: on the timeout path the receiver is flow-control-wedged, so
    /// re-sending the parked frame first would immediately re-stall the new stream
    /// against that same starved receiver and re-wedge the channel — so we drop it.
    /// Dropping keeps the new stream's first bytes a COMPLETE `[len][payload]`
    /// frame (the next frame from the channel), so the client's per-stream framing
    /// buffer — which is freshly allocated per accepted stream and never carries a
    /// mid-frame continuation across a reset (see `videocall-client`'s
    /// `handle_unidirectional_stream`) — resyncs immediately at a frame boundary. A
    /// reset mid-frame therefore discards the client's partial frame cleanly; it
    /// never desyncs the decoder.
    ///
    /// On the error path the receiver is NOT flow-control-wedged (the stream is
    /// just broken), so re-sending will not re-wedge: the fresh stream has its own
    /// flow-control window and the client allocates a fresh per-stream buffer that
    /// reads the re-sent frame whole from offset 0. Re-sending is therefore safe
    /// AND necessary — dropping here would silently lose a frame (e.g. a peer's
    /// reply during session churn, the exact `test_lobby_isolation` failure that
    /// the #1638 over-broad drop introduced).
    fn spawn_unistream_writer(
        join_set: &mut JoinSet<()>,
        session: Session,
        mut unistream_rx: mpsc::Receiver<Bytes>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
    ) {
        join_set.spawn(async move {
            let mut persistent_stream: Option<web_transport_quinn::SendStream> = None;

            // Backpressure poll ticker, constructed ONCE for the lifetime of this
            // writer task and reused across every frame. The per-frame shed helper
            // borrows it mutably and `reset()`s it at the start of each call, so a
            // frame that writes promptly allocates no timer (recovering the prior
            // `tokio::time::timeout` design's zero-alloc-on-success property). The
            // shed accumulator is per-call and local to the helper, so sharing this
            // ticker carries no stall state between frames (see
            // `write_framed_with_backpressure_shed`).
            let mut backpressure_ticker = tokio::time::interval(WT_UNISTREAM_BACKPRESSURE_POLL);

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

                // Write the WHOLE framed message (header + payload) as one future
                // so a stall on either half is treated identically. The future
                // parks NORMALLY on QUIC flow control — it is NOT wrapped in a
                // wall-clock timeout. Instead we run it under a backpressure-gated
                // shed: a periodic tick samples the outbound channel depth, and the
                // write is only shed once the channel has been sustainedly backed
                // up (see `write_framed_with_backpressure_shed`). A slow poll under
                // executor starvation on a NON-backed-up channel therefore only
                // delays the write; it never sheds it. `unistream_rx` is read
                // immutably for the depth probe — disjoint from the `&mut stream`
                // borrowed by the write future.
                let stream = persistent_stream.as_mut().expect("stream was just opened");
                let shed_reason = write_framed_with_backpressure_shed(
                    stream,
                    &len_header,
                    &data,
                    &unistream_rx,
                    &mut backpressure_ticker,
                )
                .await;

                if let Some(reason) = shed_reason {
                    RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
                        .with_label_values(&["webtransport", reason])
                        .inc();
                    // RESET the wedged/broken stream so the receiver's side
                    // surfaces an error/EOF on it and the QUIC send buffer for it
                    // is released. `reset` may itself report `ClosedStream` (the
                    // stream was already gone) — that is fine, we are tearing it
                    // down regardless, so the result is intentionally ignored.
                    if let Some(mut wedged) = persistent_stream.take() {
                        let _ = wedged.reset(UNISTREAM_SHED_RESET_CODE);
                    }
                    // Re-open a fresh persistent stream.
                    let mut fresh = match session.open_uni().await {
                        Ok(s) => s,
                        Err(e2) => {
                            error!(
                                "Error opening fresh UniStream after shed ({}): {}",
                                reason, e2
                            );
                            break;
                        }
                    };

                    // The two shed reasons demand OPPOSITE recovery (see the doc
                    // comment on this fn and #1638):
                    //
                    // * `write_timeout` (backpressure-gated stall): the receiver
                    //   is flow-control-wedged. DROP the current frame — re-sending
                    //   it onto the fresh stream would immediately re-stall against
                    //   the same starved receiver and re-wedge the channel. The
                    //   next loop iteration drains the NEXT frame onto `fresh`, so
                    //   the client resyncs at a clean frame boundary. Do NOT fire
                    //   the packet-sent callback for the dropped frame.
                    //
                    // * `write_error` (stream returned an I/O error — broken, NOT
                    //   flow-control-wedged): restore the pre-existing single-retry.
                    //   The fresh stream starts clean, so RE-SEND the complete frame
                    //   (`len_header` + `data`) on it; the client's per-stream
                    //   framing buffer reads the whole frame from offset 0. On
                    //   success the frame IS delivered — fall through to fire the
                    //   callback (deliver + count). If the retry write ALSO errors,
                    //   terminate the writer exactly as the original did.
                    if reason == "write_error" {
                        if let Err(e2) = fresh.write_all(&len_header).await {
                            error!(
                                "Error writing length header to fresh UniStream after \
                                 write-error retry: {}",
                                e2
                            );
                            break;
                        }
                        if let Err(e2) = fresh.write_all(&data).await {
                            error!(
                                "Error writing payload to fresh UniStream after \
                                 write-error retry: {}",
                                e2
                            );
                            break;
                        }
                        // Retry delivered the frame on the fresh stream. Keep it as
                        // the persistent stream and fall through to fire the
                        // packet-sent callback below (the frame was delivered +
                        // must be counted — this is the regression fix: do NOT drop
                        // it).
                        persistent_stream = Some(fresh);
                    } else {
                        // `write_timeout`: keep the fresh stream for the NEXT frame
                        // and drop the current one (skip the callback, continue).
                        persistent_stream = Some(fresh);
                        continue;
                    }
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

/// Pure backpressure predicate for the #1638 writer shed.
///
/// Returns `true` iff the outbound `unistream_tx` channel is at or above
/// [`WT_UNISTREAM_BACKPRESSURE_SHED_RATIO`] full — i.e. REAL backpressure is
/// present: the writer (the channel's only consumer) is parked and publishers
/// have filled at least that fraction of the buffer. `depth` is the number of
/// frames currently queued (`max_capacity - capacity`), `max_capacity` the
/// channel's fixed bound.
///
/// This is the gate that prevents the v1 spurious-reset regression: the shed
/// accumulator advances ONLY on ticks where this returns `true`. A healthy
/// low-traffic stream drains every frame promptly, so its depth hovers near 0
/// and this returns `false` — its shed never arms no matter how starved the
/// executor is. Extracted as a free function so the threshold arithmetic
/// (`ceil`-equivalent: `depth as f64 >= max_capacity as f64 * ratio`) is
/// unit-testable without standing up a real QUIC session.
fn channel_is_backed_up(depth: usize, max_capacity: usize) -> bool {
    if max_capacity == 0 {
        // Degenerate (cannot happen — the channel is always built with a
        // non-zero cap, see `resolve_wt_outbound_channel_capacity`'s zero
        // rejection), but treat "no capacity" as never-backed-up so we never
        // shed on a nonsense bound.
        return false;
    }
    (depth as f64) >= (max_capacity as f64) * WT_UNISTREAM_BACKPRESSURE_SHED_RATIO
}

/// Write one length-prefixed frame onto the persistent uni stream, shedding ONLY
/// under sustained real backpressure (#1638).
///
/// Returns the shed reason for the operator-facing metric/log. The CALLER
/// ([`Self::spawn_unistream_writer`]) maps each reason to its recovery:
/// * `None` — the write completed; no shed; the frame is delivered + counted.
/// * `Some("write_error")` — the stream returned an I/O error (genuinely broken,
///   not merely slow); reset+reopen regardless of backpressure, then the caller
///   RE-SENDS the frame on the fresh stream and counts it (single-retry recovery).
/// * `Some("write_timeout")` — the write stayed parked while the channel was
///   sustainedly backed up for [`WT_UNISTREAM_WRITE_DEADLINE`]; the caller resets
///   the wedged stream and DROPS the frame so the writer resumes draining.
///
/// ## Why this cannot spuriously reset a healthy stream under executor starvation
///
/// The write future parks normally on QUIC flow control. We do NOT bound it by
/// wall-clock. We instead `select!` it against a periodic
/// [`WT_UNISTREAM_BACKPRESSURE_POLL`] tick and accumulate elapsed time toward the
/// shed grace ONLY across ticks where [`channel_is_backed_up`] holds. The
/// accumulator is reset on any tick where the channel is below the ratio. So:
///
/// * On a HEALTHY (non-backed-up) stream the channel depth stays near 0, every
///   tick resets the accumulator, and the write is left to park — executor
///   starvation that delays the poll only delays the write, it can never make
///   the accumulator reach the grace. No reset.
/// * On a genuinely WEDGED receiver the writer is parked AND publishers keep
///   filling the channel past the ratio, so successive ticks advance the
///   accumulator until it reaches the grace and the stream is shed — bounding
///   how long one wedged receiver can hold the channel full.
///
/// `unistream_rx` is borrowed immutably here purely to read `capacity()` /
/// `max_capacity()`; the write future borrows the `stream` mutably; `ticker` is
/// borrowed mutably to arm the backpressure poll. All three borrows are disjoint.
///
/// `ticker` is owned by the caller ([`Self::spawn_unistream_writer`]) and reused
/// across every frame on this writer task, so the success path performs **no**
/// timer allocation (it was previously rebuilt per frame). The per-call
/// `stalled_while_backed_up` accumulator below is a fresh local each call, so
/// reusing the ticker cannot leak stall state from a prior frame: a stale tick
/// that became ready between frames only samples the (now-empty) channel and
/// resets the fresh accumulator. We still [`reset`](tokio::time::Interval::reset)
/// the ticker at the top of each call so its next tick fires one full poll
/// interval from now rather than immediately — preserving the prior design's
/// "a promptly-completing write never samples backpressure" fast path.
async fn write_framed_with_backpressure_shed(
    stream: &mut web_transport_quinn::SendStream,
    len_header: &[u8; 4],
    data: &Bytes,
    unistream_rx: &mpsc::Receiver<Bytes>,
    ticker: &mut tokio::time::Interval,
) -> Option<&'static str> {
    let max_capacity = unistream_rx.max_capacity();

    // The framed write parks normally on flow control. `tokio::pin!` lets us poll
    // it repeatedly across `select!` iterations without it being moved/dropped
    // between ticks (so partial progress is preserved while we wait).
    let framed_write = async {
        stream.write_all(len_header).await?;
        stream.write_all(data).await
    };
    tokio::pin!(framed_write);

    // The hoisted `ticker` is shared across frames, so its next tick may already
    // be ready (or even overdue) from a prior frame. Reset it so the next tick
    // fires one full `WT_UNISTREAM_BACKPRESSURE_POLL` from NOW: a write that
    // completes promptly never samples backpressure at all (pure fast path), and
    // the accumulator only advances after a real poll interval has elapsed. This
    // is the zero-alloc equivalent of constructing a fresh `interval` per frame.
    ticker.reset();

    // Total time the write has stayed parked WHILE the channel was backed up.
    // Fresh per call (NOT carried in the shared ticker), so reusing the ticker
    // across frames cannot leak a prior frame's accumulated stall. Advances only
    // on backed-up ticks; reset to zero on any healthy tick.
    let mut stalled_while_backed_up = std::time::Duration::ZERO;

    loop {
        tokio::select! {
            // Bias the write arm so a ready write always wins over a coincident
            // tick — we never shed a write that has actually completed.
            biased;

            write_result = &mut framed_write => {
                return match write_result {
                    Ok(()) => None,
                    Err(e) => {
                        warn!(
                            "Error writing to persistent UniStream ({}); resetting, \
                             reopening and re-sending the frame on a fresh stream",
                            e
                        );
                        Some("write_error")
                    }
                };
            }

            _ = ticker.tick() => {
                let depth = max_capacity.saturating_sub(unistream_rx.capacity());
                if channel_is_backed_up(depth, max_capacity) {
                    stalled_while_backed_up += WT_UNISTREAM_BACKPRESSURE_POLL;
                    if stalled_while_backed_up >= WT_UNISTREAM_WRITE_DEADLINE {
                        warn!(
                            "Persistent UniStream write parked while the outbound \
                             channel stayed backed up ({}/{} queued) past {}ms; \
                             resetting and reopening (frame dropped)",
                            depth,
                            max_capacity,
                            WT_UNISTREAM_WRITE_DEADLINE.as_millis()
                        );
                        return Some("write_timeout");
                    }
                } else {
                    // Channel is draining / healthy: the write being slow here is
                    // NOT congestion (e.g. the executor was just slow to poll us).
                    // Reset the accumulator so only SUSTAINED backpressure sheds.
                    stalled_while_backed_up = std::time::Duration::ZERO;
                }
            }
        }
    }
}

/// Minimal abstraction over a byte source that fills a buffer exactly,
/// used by [`read_length_prefixed_frame`].
///
/// We deliberately collapse all I/O errors to `Err(())` because the
/// framing logic only needs to distinguish "the read succeeded" from
/// "the read did not produce all the requested bytes" — it does not
/// care about the underlying error type. This lets the same framing
/// function drive a real `web_transport_quinn::RecvStream` in
/// production and an in-memory byte slice in unit tests, eliminating
/// the parallel test-only re-implementation that previously existed.
trait FrameReader {
    /// Fill `buf` entirely or return `Err(())`. Returning `Err(())` is
    /// the only signal for EOF — at a frame boundary the framing logic
    /// interprets it as a clean stream close.
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()>;
}

impl FrameReader for web_transport_quinn::RecvStream {
    async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        web_transport_quinn::RecvStream::read_exact(self, buf)
            .await
            .map_err(|_| ())
    }
}

/// Read one length-prefixed frame (`[4-byte BE length][payload]`) from any
/// byte source that implements [`FrameReader`]. In production the source is
/// a WebTransport uni stream; in tests it is an in-memory byte slice.
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
async fn read_length_prefixed_frame<R: FrameReader>(
    stream: &mut R,
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
                    // #1146: count the drop so a sustained inbound-media drop is
                    // visible on dashboards/alerts, not just in the warn log
                    // (which at volume is itself noise/cost).
                    RELAY_INBOUND_BRIDGE_DROPS_TOTAL
                        .with_label_values(&["webtransport", "unistream"])
                        .inc();
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
    //! These tests drive the real production [`read_length_prefixed_frame`]
    //! against an in-memory byte source. The function is generic over the
    //! [`FrameReader`] trait, and we implement that trait for a tiny
    //! [`BytesCursor`] adapter below. This means the framing logic the
    //! tests assert is byte-for-byte the same logic that runs in
    //! production — there is no parallel re-implementation to drift
    //! out of sync.
    //!
    //! Integration of the real [`web_transport_quinn::RecvStream`] path
    //! (including QUIC's read-exact error variants) is covered by the
    //! end-to-end tests in `actix-api/src/webtransport/mod.rs`
    //! (`test_relay_packet_webtransport_between_two_clients` etc.).

    use super::*;

    /// Minimal in-memory implementation of [`FrameReader`] for unit
    /// tests. Consumes from a `Vec<u8>` exactly the way the real
    /// `RecvStream::read_exact` consumes from a quinn stream: returns
    /// `Ok(())` only when the full buffer can be filled, otherwise
    /// returns `Err(())` to signal EOF / truncation.
    struct BytesCursor {
        buf: Vec<u8>,
        pos: usize,
    }

    impl BytesCursor {
        fn new(buf: Vec<u8>) -> Self {
            Self { buf, pos: 0 }
        }
    }

    impl FrameReader for BytesCursor {
        async fn read_exact(&mut self, out: &mut [u8]) -> Result<(), ()> {
            if self.buf.len() - self.pos < out.len() {
                // Mirror RecvStream::read_exact's behaviour: on
                // insufficient bytes the test cursor returns Err
                // *without* consuming any of the partial read. The
                // production framing logic only inspects success/failure,
                // not the remaining cursor state, so this matches.
                return Err(());
            }
            out.copy_from_slice(&self.buf[self.pos..self.pos + out.len()]);
            self.pos += out.len();
            Ok(())
        }
    }

    /// Terminal state of the per-stream reader loop. Mirrors the way
    /// [`read_framed_packets_loop`] reacts to the four possible outcomes
    /// of [`read_length_prefixed_frame`], so each test can assert both
    /// the decoded payload list and the reason the loop stopped.
    #[derive(Debug, PartialEq, Eq)]
    enum TerminalStatus {
        CleanEof,
        TruncatedHeader,
        TruncatedPayload { expected: usize },
        Malformed { len: usize },
    }

    /// Drive the real [`read_length_prefixed_frame`] over a byte slice
    /// until it terminates, collecting all decoded payloads and the
    /// terminal reason. This is the *only* decode entry point used by
    /// the test suite; there is no parallel re-implementation to keep
    /// in sync with production.
    async fn decode_all(buf: &[u8]) -> (Vec<Vec<u8>>, TerminalStatus) {
        let mut cursor = BytesCursor::new(buf.to_vec());
        let mut payloads = Vec::new();
        loop {
            match read_length_prefixed_frame(&mut cursor).await {
                Ok(Some(p)) => payloads.push(p),
                Ok(None) => return (payloads, TerminalStatus::CleanEof),
                Err(FramedReadError::Malformed { len }) => {
                    return (payloads, TerminalStatus::Malformed { len });
                }
                Err(FramedReadError::TruncatedHeader) => {
                    return (payloads, TerminalStatus::TruncatedHeader);
                }
                Err(FramedReadError::TruncatedPayload { expected }) => {
                    return (payloads, TerminalStatus::TruncatedPayload { expected });
                }
            }
        }
    }

    /// Convenience wrapper so the tests stay synchronous-looking. Spins
    /// up a single-threaded runtime per call — fine for these
    /// microsecond-scale framing tests.
    fn decode_frames_from_bytes(buf: &[u8]) -> (Vec<Vec<u8>>, TerminalStatus) {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build current-thread runtime")
            .block_on(decode_all(buf))
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

// =============================================================================
// #1638 writer-deadline regression tests
// =============================================================================
//
// These exercise the REAL production `spawn_unistream_writer` via
// `WebTransportBridge::new_with_callback` against a REAL `web_transport_quinn`
// session pair stood up in-process over loopback (NO NATS, NO full relay
// server). The bridge writer is hard-typed to `web_transport_quinn::Session`,
// so the only way to drive the genuine production code path is with a real
// session — there is no trait seam to mock. We therefore build a minimal
// HTTP/3 WebTransport handshake in-process and stall the CLIENT's read side so
// QUIC flow-control credits on the server→client uni stream drain to zero,
// reproducing the exact downlink-stall failure mode the fix targets.
#[cfg(test)]
mod writer_shed_tests {
    use super::*;
    use crate::constants::WT_UNISTREAM_WRITE_DEADLINE;
    use actix::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use web_transport_quinn::quinn;

    /// Minimal actor implementing `Handler<WtInbound>` so we can build a real
    /// `WebTransportBridge` without standing up a full `WtChatSession` (which
    /// needs NATS, SessionManager, addresses, …). The bridge's writer task —
    /// the code under test — never touches this actor; it only drains the
    /// outbound channel onto the session's uni stream. The reader tasks forward
    /// inbound frames here, which the test ignores.
    struct StubActor;
    impl Actor for StubActor {
        type Context = Context<Self>;
    }
    impl Handler<WtInbound> for StubActor {
        type Result = ();
        fn handle(&mut self, _msg: WtInbound, _ctx: &mut Self::Context) {}
    }

    /// Build a hermetic in-process `web_transport_quinn` server endpoint on an
    /// ephemeral loopback port using the committed DER test cert + key. Returns
    /// the bound address and the `Server` so the caller can `accept()`.
    fn build_test_server() -> (std::net::SocketAddr, web_transport_quinn::Server) {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};

        // CARGO_MANIFEST_DIR points at the actix-api crate root; the certs live
        // under <crate>/certs. These are committed DER fixtures (an X.509 cert
        // and a PKCS#8 key) — the client uses no-cert-verification, so trust is
        // irrelevant; we only need a parseable cert+key for the server config.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let cert_der =
            std::fs::read(format!("{manifest_dir}/certs/localhost.der")).expect("read cert der");
        let key_der =
            std::fs::read(format!("{manifest_dir}/certs/localhost_key.der")).expect("read key der");

        let chain = vec![CertificateDer::from(cert_der)];
        let key = PrivateKeyDer::try_from(key_der).expect("parse pkcs8 key der");

        let provider = rustls::crypto::ring::default_provider();
        let mut crypto = rustls::ServerConfig::builder_with_provider(provider.into())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("tls13")
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .expect("single cert");
        crypto.alpn_protocols = vec![web_transport_quinn::ALPN.as_bytes().to_vec()];

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(crypto).expect("quic server config"),
        ));
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = quinn::Endpoint::server(server_config, addr).expect("server endpoint");
        let bound = endpoint.local_addr().expect("local addr");
        (bound, web_transport_quinn::Server::new(endpoint))
    }

    /// Connect a `web_transport_quinn` client (no cert verification) to the
    /// given loopback address.
    async fn connect_test_client(addr: std::net::SocketAddr) -> web_transport_quinn::Session {
        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()
            .expect("client builder");
        let url =
            url::Url::parse(&format!("https://127.0.0.1:{}/test", addr.port())).expect("parse url");
        client.connect(url).await.expect("client connect")
    }

    /// Drive a frame onto the bridge's outbound unistream channel.
    fn push(tx: &mpsc::Sender<Bytes>, n: usize) -> Result<(), mpsc::error::TrySendError<Bytes>> {
        tx.try_send(Bytes::from(vec![0xCD; n]))
    }

    /// REGRESSION TEST (#1638): a stalled-downlink receiver whose outbound
    /// channel is ACTUALLY BACKED UP must NOT wedge that channel full
    /// indefinitely — the backpressure-gated writer sheds within the grace and
    /// resumes draining.
    ///
    /// Setup: a real server→client uni stream where the client accepts the
    /// stream but never reads it, so QUIC flow-control credits drain to zero and
    /// the server's `write_all` parks. We push enough frames to FILL the writer's
    /// channel (depth 16/16, far above the [`WT_UNISTREAM_BACKPRESSURE_SHED_RATIO`]
    /// = 0.5 gate of depth ≥ 8), so the genuine-backpressure condition the shed
    /// keys on is unambiguously present. We then assert the channel does NOT stay
    /// full past the grace (the writer reset+reopened the wedged stream and
    /// drained more frames).
    ///
    /// This still exercises the REAL production shed: because the channel is
    /// genuinely backed up here, `write_framed_with_backpressure_shed`'s
    /// stalled-while-backed-up accumulator advances on every poll tick and reaches
    /// [`WT_UNISTREAM_WRITE_DEADLINE`] → `write_timeout` shed. (The sibling
    /// `healthy_low_traffic_write_is_not_shed_under_executor_starvation` proves the
    /// COMPLEMENT — a NON-backed-up channel never sheds even when the poll is
    /// starved — which is the spurious-reset regression this redesign fixes.)
    ///
    /// PROOF THE TEST BITES: on a writer that NEVER sheds (replace the
    /// `write_framed_with_backpressure_shed` call with a bare
    /// `framed_write.await`), `write_all` parks forever, the writer never drains
    /// again, and the channel stays full until the outer `tokio::time::timeout`
    /// fires → the test FAILS (panics on timeout). With the fix, the writer sheds
    /// once the channel has been backed up for `WT_UNISTREAM_WRITE_DEADLINE` and
    /// the channel regains capacity → the test PASSES.
    #[actix_rt::test]
    async fn stalled_receiver_does_not_wedge_channel_forever() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();

        // Accept the client session on the server side in the background.
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });

        // Connect the client. CRITICAL: we hold the session but DO NOT accept or
        // read its incoming uni stream, so once the server opens the persistent
        // uni stream and writes a flow-control window's worth of bytes, further
        // writes park on credit exhaustion — the exact downlink stall the fix
        // targets.
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        // Build the bridge with the REAL production writer over the REAL server
        // session. The channel cap is small so it fills quickly under stall.
        const CAP: usize = 16;
        let (uni_tx, uni_rx) = mpsc::channel::<Bytes>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<Bytes>(CAP);
        let sent = Arc::new(AtomicUsize::new(0));
        let sent_cb = sent.clone();
        let on_sent: PacketSentCallback = Box::new(move || {
            sent_cb.fetch_add(1, Ordering::SeqCst);
        });

        let stub = StubActor.start();
        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            stub,
            uni_rx,
            dgram_rx,
            Some(on_sent),
        );

        // Outer guard: if a regression causes the writer to park forever, this
        // makes the whole test FAIL (timeout) rather than hang CI indefinitely.
        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            // Push frames large enough to exhaust the receive window quickly.
            // Some will be accepted; once the writer parks on the stalled stream
            // the channel fills and `try_send` starts returning Full.
            let frame_bytes = 64 * 1024;
            let mut full_observed = false;
            for _ in 0..(CAP * 4) {
                if push(&uni_tx, frame_bytes).is_err() {
                    full_observed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert!(
                full_observed,
                "test setup failed: channel never filled — the receiver stall did \
                 not park the writer (window too large or frames too small)"
            );

            // The channel is now full (writer parked on the wedged stream). The
            // FIX must shed within ~WT_UNISTREAM_WRITE_DEADLINE and resume
            // draining, so capacity must return. Poll for capacity to reappear
            // for up to a few deadlines' worth of time.
            let recover_deadline = std::time::Instant::now()
                + WT_UNISTREAM_WRITE_DEADLINE * 4
                + Duration::from_secs(2);
            let mut recovered = false;
            while std::time::Instant::now() < recover_deadline {
                if uni_tx.capacity() > 0 {
                    recovered = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            assert!(
                recovered,
                "REGRESSION (#1638): outbound unistream channel stayed FULL past \
                 the writer deadline — the writer parked on the stalled receiver \
                 and never shed. capacity={}",
                uni_tx.capacity()
            );

            // After recovery, a fresh push must be admitted (the writer is
            // draining again onto the fresh stream), proving the reset+reopen
            // recovered the writer rather than killing it.
            // Drain any slack then confirm the channel keeps accepting.
            let mut post_recovery_admitted = 0usize;
            for _ in 0..CAP {
                if push(&uni_tx, frame_bytes).is_ok() {
                    post_recovery_admitted += 1;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                post_recovery_admitted > 0,
                "after shed the writer must keep draining (admitted 0 post-recovery)"
            );

            // Keep the client session alive until the end so the connection is
            // not torn down early (which would mask the stall with a clean EOF).
            drop(client_session);
        })
        .await;

        outcome.expect(
            "REGRESSION (#1638): writer never shed the stalled stream within the \
             test window — the channel stayed wedged (un-bounded writer parks \
             forever on QUIC flow control)",
        );
    }

    /// REGRESSION TEST (the #1638 over-broad-drop bug — the bug THIS change fixes):
    /// a single transient **write error** must be recovered by RE-SENDING the frame
    /// on a fresh stream and FIRING the packet-sent callback (deliver + count) — it
    /// must NOT be dropped.
    ///
    /// This is the `test_lobby_isolation` failure mode in miniature: #1638
    /// collapsed BOTH shed reasons into "reset + reopen + DROP the frame + skip the
    /// callback". That is correct for `write_timeout` (the receiver is
    /// flow-control-wedged) but WRONG for `write_error` (the stream is just broken):
    /// a transient error — e.g. `invalid STOP_SENDING` during session churn — then
    /// silently DROPS the frame instead of retrying it. In `test_lobby_isolation`
    /// the dropped frame is a peer's reply, so the receiving peer's counter never
    /// reaches its expected value. The pre-#1638 writer single-retried on a write
    /// error and delivered the frame; this test pins that restored behaviour.
    ///
    /// Setup (drives the REAL production `spawn_unistream_writer` via
    /// `WebTransportBridge::new_with_callback` over a REAL loopback session — NO
    /// NATS, NO relay server): the client accepts the server's FIRST persistent uni
    /// stream and immediately `stop()`s it (sends STOP_SENDING), which surfaces on
    /// the server writer's `write_all` as a genuine I/O **write error** — the exact
    /// `write_error` shed reason, induced the same real way the sibling tests induce
    /// a real stall. The client then accepts EVERY subsequent server stream and
    /// drains it (granting flow control), so the writer's re-send on the fresh
    /// stream completes. We push exactly ONE (large) frame and assert the
    /// `on_packet_sent` callback fires exactly once — i.e. the frame was RE-SENT
    /// and counted. (The frame is large so the first `write_all` is guaranteed
    /// still in-flight when STOP_SENDING lands and therefore reliably errors — see
    /// the comment at the push site.)
    ///
    /// PROOF THE TEST BITES: on the current (drops-on-`write_error`) code the writer
    /// resets+reopens but `continue`s WITHOUT re-sending and WITHOUT firing the
    /// callback, so the count stays 0 and the final assert FAILS. With the fix the
    /// writer re-sends the frame on the fresh stream and fires the callback → count
    /// == 1 → the assert PASSES. (Reverting the production split — making
    /// `write_error` `continue` like `write_timeout` — re-breaks this test.)
    #[actix_rt::test]
    async fn write_error_resends_frame_and_counts_it_not_dropped() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();

        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });

        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        // Client side: accept the server's persistent uni streams. STOP_SENDING the
        // FIRST one (this is what makes the server writer's `write_all` return an
        // I/O error → the `write_error` shed). Then accept and DRAIN every later
        // stream so the writer's re-send on the fresh stream actually completes
        // (the receiver grants flow control by reading). Runs until the session
        // closes.
        let client_drainer = tokio::spawn(async move {
            let mut stream_index = 0usize;
            // `accept_uni` returns `Err` once the session closes (test teardown),
            // which ends this `while let` cleanly.
            while let Ok(mut recv) = client_session.accept_uni().await {
                if stream_index == 0 {
                    // Induce ONE real write error on the server's first persistent
                    // stream by STOP_SENDING it.
                    let _ = recv.stop(0u32);
                } else {
                    // Drain the re-sent frame on the fresh stream so the server's
                    // retry `write_all` makes progress and returns Ok (which is what
                    // fires the callback in the writer). Read until EOF / error; we
                    // don't assert on the contents, only that draining lets the
                    // server's write complete.
                    let mut buf = vec![0u8; 64 * 1024];
                    while let Ok(Some(_n)) = recv.read(&mut buf).await {}
                }
                stream_index += 1;
            }
        });

        const CAP: usize = 16;
        let (uni_tx, uni_rx) = mpsc::channel::<Bytes>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<Bytes>(CAP);
        let sent = Arc::new(AtomicUsize::new(0));
        let sent_cb = sent.clone();
        let on_sent: PacketSentCallback = Box::new(move || {
            sent_cb.fetch_add(1, Ordering::SeqCst);
        });

        let stub = StubActor.start();
        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            stub,
            uni_rx,
            dgram_rx,
            Some(on_sent),
        );

        // Push EXACTLY ONE frame. The writer opens stream #1 and writes — the
        // client STOP_SENDINGs that stream → the server's `write_all` returns an
        // I/O error → the writer resets, opens stream #2, and (with the fix)
        // RE-SENDS this frame onto it, then fires the callback.
        //
        // The frame is LARGE (4 MiB, above quinn's default ~1.25 MiB stream window
        // — the same size the sibling `healthy_low_traffic_*` test uses to force a
        // genuine park). This removes the only timing race in the setup: a small
        // write would complete into the local send buffer and return `Ok` BEFORE
        // the STOP_SENDING frame is processed (no error → the normal path fires the
        // callback and the test would pass for the wrong reason). A 4 MiB write
        // CANNOT complete in one shot — it must await window grant, so it is
        // guaranteed still in-flight when STOP_SENDING lands and reliably errors.
        // The client drainer reads stream #2 to completion, granting the window the
        // re-send needs, so the retry `write_all` returns `Ok` and the callback
        // fires.
        let frame_len = 4 * 1024 * 1024;
        uni_tx
            .send(Bytes::from(vec![0xAB; frame_len]))
            .await
            .expect("push one frame onto the outbound unistream channel");

        // Poll for the callback to fire. On the FIXED code it reaches 1 once the
        // re-send completes; on the BUGGY (drops-on-error) code it stays 0 forever,
        // so this loop exhausts its deadline and the assert below FAILS.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while std::time::Instant::now() < deadline {
            if sent.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            sent.load(Ordering::SeqCst),
            1,
            "REGRESSION (#1638 over-broad drop): a single write error must RE-SEND \
             the frame on a fresh stream and FIRE the packet-sent callback (deliver \
             + count) — it was DROPPED instead (callback never fired). This is the \
             dropped-peer-reply that breaks test_lobby_isolation."
        );

        // Tear down: closing the session ends the client drainer loop cleanly.
        drop(uni_tx);
        client_drainer.abort();
    }

    /// REGRESSION TEST (#1638 follow-up): a HEALTHY, non-backed-up stream must
    /// NOT be shed even when its write parks for far longer than
    /// [`WT_UNISTREAM_WRITE_DEADLINE`] — the exact spurious-reset the v1
    /// wall-clock `tokio::time::timeout(write_all)` caused (it failed
    /// `test_lobby_isolation`). The genuine congestion signal is the outbound
    /// channel BACKING UP, not wall-clock on one write.
    ///
    /// Setup: a real server→client uni stream where the client accepts but never
    /// reads, so `write_all` parks on QUIC flow control — BUT the outbound channel
    /// passed to the production `write_framed_with_backpressure_shed` is EMPTY
    /// (depth 0, far below the 0.5 shed ratio). A parked write here stands in for
    /// "the executor was slow to drive a healthy write": the receiver isn't the
    /// bottleneck (no other frames are queued behind it). The shed must therefore
    /// NEVER fire.
    ///
    /// We drive the REAL production helper directly and assert it does NOT return
    /// a shed within a window that is MANY times the deadline. Because a
    /// non-backed-up parked write parks FOREVER (correct behaviour), the only way
    /// to make this terminate is an outer timeout — and the outer timeout firing
    /// (the helper still parked) is exactly the PASS condition.
    ///
    /// PROOF THE TEST BITES: on the v1 wall-clock writer, the equivalent
    /// `tokio::time::timeout(WT_UNISTREAM_WRITE_DEADLINE, framed_write)` elapses
    /// after 1s regardless of channel depth and returns a shed, so the helper
    /// WOULD return `Some("write_timeout")` well within this window → the
    /// `select!` below would take the `shed` branch and the test would FAIL
    /// ("spuriously shed a healthy non-backed-up stream"). With the fix the helper
    /// stays parked on the empty channel, the outer sleep wins, and the test
    /// PASSES.
    #[actix_rt::test]
    async fn healthy_low_traffic_write_is_not_shed_under_executor_starvation() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();

        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });

        // Client connects but never accepts/reads the server's uni stream, so
        // once the server writes a flow-control window's worth the write parks.
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        // Open the persistent uni stream the same way the production writer does.
        let mut stream = server_session
            .open_uni()
            .await
            .expect("open server->client uni stream");

        // CRITICAL: the channel is EMPTY — depth 0, well below the 0.5 shed ratio.
        // This is the "healthy / non-backed-up" condition. We never push to it, so
        // `channel_is_backed_up` is false on every poll tick and the shed
        // accumulator can never advance.
        const CAP: usize = 16;
        let (_uni_tx, uni_rx) = mpsc::channel::<Bytes>(CAP);
        assert_eq!(
            uni_rx.max_capacity().saturating_sub(uni_rx.capacity()),
            0,
            "precondition: the channel under test must be empty (non-backed-up)"
        );

        // A payload large enough to exhaust the fresh stream's AND the
        // connection's flow-control window so a SINGLE `write_all` genuinely
        // parks mid-frame (the receiver never reads). quinn's default stream
        // receive window is ~1.25 MiB and the connection window ~1.5 MiB; a 4 MiB
        // frame — above `MAX_FRAME_SIZE` (4_000_000) and well past a real 1080p
        // keyframe ceiling — blows past both windows, so the write parks rather
        // than completing into the buffer. The test only needs the frame to exceed
        // the quinn window (it does), not to equal `MAX_FRAME_SIZE`. We assert
        // below that it actually parked (the helper must NOT return promptly).
        let len: u32 = (4 * 1024 * 1024) as u32;
        let header = len.to_be_bytes();
        let data = Bytes::from(vec![0xEE; len as usize]);

        // Wait MANY deadlines. If the helper sheds within this window on a
        // non-backed-up channel, that is the spurious-reset bug.
        let watchdog = WT_UNISTREAM_WRITE_DEADLINE * 4 + Duration::from_secs(2);

        // The production writer owns one ticker for the whole task and reuses it
        // per frame; mirror that here by constructing one and passing it in.
        let mut ticker = tokio::time::interval(WT_UNISTREAM_BACKPRESSURE_POLL);

        tokio::select! {
            shed = write_framed_with_backpressure_shed(&mut stream, &header, &data, &uni_rx, &mut ticker) => {
                panic!(
                    "REGRESSION (#1638): the writer SHED a healthy, non-backed-up \
                     stream (channel depth 0) just because the write parked past \
                     the wall-clock deadline (shed={shed:?}). The shed must key on \
                     the outbound channel backing up, NOT on a per-write deadline. \
                     This is the spurious reset that broke test_lobby_isolation."
                );
            }
            _ = tokio::time::sleep(watchdog) => {
                // Helper is still parked after 4× the deadline + 2s on an empty
                // channel — correct: a non-backed-up write is never shed.
            }
        }

        drop(client_session);
    }
}

// =============================================================================
// #1638 backpressure-predicate unit tests
// =============================================================================
//
// Pure, fast tests for `channel_is_backed_up` — the gate that decides whether a
// parked write's stall counts toward the shed. These drive the REAL production
// predicate (no re-implementation) over its full decision boundary so the 0.5
// ratio and the never-shed-when-empty invariant are pinned.
#[cfg(test)]
mod backpressure_predicate_tests {
    use super::*;

    #[test]
    fn empty_channel_is_never_backed_up() {
        // The healthy steady state: a draining writer keeps depth at 0. This is
        // the case the v1 wall-clock shed got wrong — it MUST be "not backed up".
        assert!(!channel_is_backed_up(0, 512));
        assert!(!channel_is_backed_up(0, 16));
    }

    #[test]
    fn below_half_is_not_backed_up() {
        // Just under the 0.5 ratio at the production 512 cap: 255 < 256.
        assert!(!channel_is_backed_up(255, 512));
        // And at the small test cap: 7 < 8.
        assert!(!channel_is_backed_up(7, 16));
    }

    #[test]
    fn at_or_above_half_is_backed_up() {
        // Exactly at the ratio boundary (depth == 50% of cap) counts as backed
        // up — the gate is `>=`, so the boundary arms the shed.
        assert!(channel_is_backed_up(256, 512));
        assert!(channel_is_backed_up(8, 16));
        // Above the ratio, and at full.
        assert!(channel_is_backed_up(400, 512));
        assert!(channel_is_backed_up(512, 512));
        assert!(channel_is_backed_up(16, 16));
    }

    #[test]
    fn zero_capacity_is_never_backed_up() {
        // Degenerate guard: a 0-cap channel (which the production resolver never
        // builds) must not divide-by-zero into a false shed.
        assert!(!channel_is_backed_up(0, 0));
        assert!(!channel_is_backed_up(5, 0));
    }

    #[test]
    fn ratio_matches_the_documented_half_threshold() {
        // Pin the production ratio used by the predicate. If someone retunes
        // WT_UNISTREAM_BACKPRESSURE_SHED_RATIO they must revisit this boundary
        // (and the shed-grace math in the doc comments).
        assert_eq!(WT_UNISTREAM_BACKPRESSURE_SHED_RATIO, 0.5);
    }
}
