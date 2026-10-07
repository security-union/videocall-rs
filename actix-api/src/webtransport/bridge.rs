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

use crate::actors::priority_drop::{
    dimension_fill, queue_byte_kind_label, OutboundPriority, SharedQueueByteMeter,
};
use crate::actors::session_logic::{
    DownlinkDropSink, DownlinkReliefSignal, RELIEF_SOURCE_UNISTREAM_SHED,
};
use crate::actors::shed_escalation::{DownlinkShedEscalation, EscalationAction};
use crate::actors::transports::wt_chat_session::{
    audio_media_kind_code, enqueue_unistream, wt_unistream_byte_budget_for, DownlinkStreamKey,
    PublisherStreamKind, WtInbound, WtInboundSource, WtOutboundFrame, ECHO_DROP_KIND,
};
use crate::constants::{
    MAX_FRAME_SIZE, WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY, WT_DOWNLINK_CONTROL_CHANNEL_CAPACITY,
    WT_DOWNLINK_CONTROL_RESERVE, WT_DOWNLINK_KEY_CHANNEL_CAPACITY,
    WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN, WT_DOWNLINK_LANE_RESPAWN_COOLDOWN,
    WT_DOWNLINK_LANE_RETIRE_GRACE, WT_DOWNLINK_OVERFLOW_CHANNEL_CAPACITY,
    WT_DOWNLINK_STREAM_IDLE_SWEEP, WT_DOWNLINK_STREAM_IDLE_TIMEOUT, WT_DOWNLINK_TEARDOWN_DRAIN,
    WT_MAX_DOWNLINK_STREAMS, WT_UNISTREAM_BACKPRESSURE_POLL, WT_UNISTREAM_BACKPRESSURE_SHED_RATIO,
    WT_UNISTREAM_WRITE_DEADLINE,
};
use crate::metrics::{
    OUTBOUND_CHANNEL_DROPS_TOTAL, RELAY_DATAGRAM_UNISTREAM_FALLBACKS_TOTAL,
    RELAY_DOWNLINK_LANE_TASK_ENTRIES, RELAY_DOWNLINK_SHED_ESCALATIONS_TOTAL,
    RELAY_DOWNLINK_STREAM_FINISHES_TOTAL, RELAY_DOWNLINK_STREAM_IDLE_REAPS_TOTAL,
    RELAY_DOWNLINK_STREAM_OVERFLOW_FRAMES_TOTAL, RELAY_DOWNLINK_STREAM_QUEUE_DROPS_TOTAL,
    RELAY_DOWNLINK_STREAM_SLOTS, RELAY_INBOUND_BRIDGE_DROPS_TOTAL,
    RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL, RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL,
    RELAY_WT_SESSION_CLOSES_TOTAL,
};
use actix::Addr;
use bytes::Bytes;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};
use videocall_types::wt_close::{
    WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE, WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE,
};
use web_transport_quinn::{quinn, Session, SessionError, WebTransportError};

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
    join_set: JoinSet<&'static str>,
    session: Session,
    escalation: DownlinkShedEscalation,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<WtOutboundFrame>,
        datagram_rx: mpsc::Receiver<WtOutboundFrame>,
        unistream_fallback_tx: mpsc::Sender<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        datagram_send_calls: Arc<AtomicU64>,
        drops: DownlinkDropSink,
        downlink_mode: DownlinkStreamMode,
        escalation: DownlinkShedEscalation,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        Self::new_with_callback(
            session,
            actor_addr,
            unistream_rx,
            datagram_rx,
            unistream_fallback_tx,
            unistream_bytes,
            None,
            datagram_send_calls,
            drops,
            downlink_mode,
            escalation,
        )
    }

    /// Create a new bridge with optional callback for packet tracking.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_callback<A>(
        session: Session,
        actor_addr: Addr<A>,
        unistream_rx: mpsc::Receiver<WtOutboundFrame>,
        datagram_rx: mpsc::Receiver<WtOutboundFrame>,
        unistream_fallback_tx: mpsc::Sender<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        on_packet_sent: Option<PacketSentCallback>,
        datagram_send_calls: Arc<AtomicU64>,
        drops: DownlinkDropSink,
        downlink_mode: DownlinkStreamMode,
        escalation: DownlinkShedEscalation,
    ) -> Self
    where
        A: actix::Actor<Context = actix::Context<A>> + actix::Handler<WtInbound>,
    {
        let mut join_set = JoinSet::new();

        // Wrap the test callback in an Arc so it can be shared between the
        // two writer tasks without `Clone` being required on the boxed
        // closure type. `Option<Arc<...>>` lets us cheaply share a single
        // counter across both writers; in production both are `None`.
        let on_packet_sent = on_packet_sent.map(Arc::new);
        let bridge_escalation = escalation.clone();

        Self::spawn_unistream_reader(&mut join_set, session.clone(), actor_addr.clone());
        Self::spawn_datagram_reader(&mut join_set, session.clone(), actor_addr);
        match downlink_mode {
            DownlinkStreamMode::Single => Self::spawn_unistream_writer(
                &mut join_set,
                session.clone(),
                unistream_rx,
                unistream_bytes.clone(),
                on_packet_sent.clone(),
                drops.relief().clone(),
                escalation,
            ),
            DownlinkStreamMode::PerPublisherV1 => Self::spawn_downlink_dispatcher(
                &mut join_set,
                session.clone(),
                unistream_rx,
                unistream_bytes.clone(),
                on_packet_sent.clone(),
                drops,
                escalation,
            ),
        }
        Self::spawn_datagram_writer(
            &mut join_set,
            session.clone(),
            datagram_rx,
            unistream_fallback_tx,
            unistream_bytes,
            on_packet_sent,
            datagram_send_calls,
        );

        Self {
            join_set,
            session,
            escalation: bridge_escalation,
        }
    }

    /// Wait for the first bridge task to end; return who closed the session, the
    /// peer's WT close code and reason if it sent one, and that task's label (or
    /// `task_failed` if it panicked).
    #[must_use]
    pub async fn wait_for_disconnect(&mut self) -> String {
        let ended = match self.join_set.join_next().await {
            Some(Ok(label)) => label,
            Some(Err(_)) => "task_failed",
            None => "none",
        };
        match self.closed_by() {
            (closed_by, Some(wt_close)) => {
                format!("closed_by={closed_by} wt_close={wt_close} ended={ended}")
            }
            (closed_by, None) => format!("closed_by={closed_by} ended={ended}"),
        }
    }

    /// `no_close_frame`: no WT close capsule and no CONNECTION_CLOSE arrived — an idle
    /// timeout, stateless reset, CONNECT stream FIN/reset without a capsule, or a
    /// transport error the relay itself detected.
    fn closed_by(&self) -> (&'static str, Option<String>) {
        let shed_close = self.escalation.session_closed();
        let close_reason = self.session.close_reason();
        let conn: &quinn::Connection = &self.session;
        let peer_close_frames = conn.stats().frame_rx.connection_close;
        match close_reason {
            _ if shed_close => ("relay_shed", None),
            Some(SessionError::WebTransportError(WebTransportError::Closed(code, reason))) => (
                "peer_wt",
                Some(quote_capped(&format!("code={code} reason={reason}"))),
            ),
            None => ("relay_task", None),
            Some(_) if peer_close_frames > 0 => ("peer_quic", None),
            Some(_) => ("no_close_frame", None),
        }
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
    fn spawn_unistream_reader<A>(
        join_set: &mut JoinSet<&'static str>,
        session: Session,
        actor_addr: Addr<A>,
    ) where
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
            "uni_reader"
        });
    }

    /// Spawn Datagram reader task.
    fn spawn_datagram_reader<A>(
        join_set: &mut JoinSet<&'static str>,
        session: Session,
        actor_addr: Addr<A>,
    ) where
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
            "datagram_reader"
        });
    }

    /// Spawn the single-stream UniStream writer task.
    ///
    /// Owns the one persistent server→client uni stream, opened lazily and kept
    /// for the session. Drains `unistream_rx` and writes length-prefix-framed
    /// packets onto it, so QUIC's per-stream ordering is the delivery order.
    ///
    /// Separate from `spawn_datagram_writer` so a parked `write_all` here cannot
    /// stop datagrams (discussion #756).
    ///
    /// Backpressure-gated shed (#1638): a parked write is shed — the wedged
    /// stream RESET, a fresh one opened — only while the lane is genuinely
    /// backing up, never on wall-clock alone. The two reset paths recover
    /// OPPOSITELY: `write_timeout` DROPS the frame, stamps `downlink_relief` and
    /// feeds [`escalate_unistream_shed`]; `write_error` RE-SENDS it on the fresh
    /// stream and fires the packet-sent callback, and a second error ends the
    /// writer.
    #[allow(clippy::too_many_arguments)]
    fn spawn_unistream_writer(
        join_set: &mut JoinSet<&'static str>,
        session: Session,
        mut unistream_rx: mpsc::Receiver<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
        downlink_relief: DownlinkReliefSignal,
        escalation: DownlinkShedEscalation,
    ) {
        join_set.spawn(async move {
            let mut persistent_stream: Option<web_transport_quinn::SendStream> = None;

            // Built ONCE per task and `reset()` per frame, so a prompt write
            // allocates no timer.
            let mut backpressure_ticker = tokio::time::interval(WT_UNISTREAM_BACKPRESSURE_POLL);

            while let Some(frame) = unistream_rx.recv().await {
                // On DEQUEUE, not on a successful write: the slot is free
                // from here and a shed frame must not stay charged (#2717).
                unistream_bytes.on_dequeue(frame.priority, frame.bytes.len());

                match write_one_frame(
                    &session,
                    &mut persistent_stream,
                    None,
                    frame.priority,
                    &frame.bytes,
                    &unistream_rx,
                    &unistream_bytes,
                    &mut backpressure_ticker,
                    &downlink_relief,
                    &escalation,
                )
                .await
                {
                    FrameOutcome::Delivered => {
                        if let Some(ref callback) = on_packet_sent {
                            callback();
                        }
                    }
                    FrameOutcome::Dropped => {}
                    FrameOutcome::WriterDead => break,
                }
            }
            info!("WebTransport UniStream writer ended");
            "uni_writer"
        });
    }

    /// Replaces [`Self::spawn_unistream_writer`] for a
    /// [`DownlinkStreamMode::PerPublisherV1`] client. Owns no QUIC stream: each
    /// lane writes its own from its own TASK, which is what removes
    /// cross-publisher head-of-line blocking. Each lane debits the #2717 byte
    /// meter on dequeue, so this hand-off does not.
    #[allow(clippy::too_many_arguments)]
    fn spawn_downlink_dispatcher(
        join_set: &mut JoinSet<&'static str>,
        session: Session,
        mut unistream_rx: mpsc::Receiver<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        on_packet_sent: Option<std::sync::Arc<PacketSentCallback>>,
        drops: DownlinkDropSink,
        escalation: DownlinkShedEscalation,
    ) {
        join_set.spawn(async move {
            let mut map = DownlinkStreamMap::new(LaneFactory {
                session,
                unistream_bytes,
                on_packet_sent,
                drops,
                escalation,
            });
            let mut idle_sweep = tokio::time::interval(downlink_idle_sweep());

            loop {
                // NOT `biased`: that would starve the sweep in a busy room, where
                // `recv` is ready on almost every iteration.
                tokio::select! {
                    received = unistream_rx.recv() => {
                        let Some(frame) = received else { break };
                        map.dispatch(frame);
                    }

                    _ = idle_sweep.tick() => {
                        map.reap_idle();
                    }
                }
            }

            map.close_and_join().await;
            info!("WebTransport downlink dispatcher ended");
            "downlink_dispatcher"
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
    /// The two writers DO share the reliable lane's channel (#2716), but this task
    /// only `try_send`s into it, so a wedged uni stream costs a dropped divert,
    /// not a stalled audio lane. `send_datagram` is attempted FIRST; `TooLarge` /
    /// `UnsupportedByPeer` is counted AND diverted rather than dropped.
    #[allow(clippy::too_many_arguments)]
    fn spawn_datagram_writer(
        join_set: &mut JoinSet<&'static str>,
        session: Session,
        mut datagram_rx: mpsc::Receiver<WtOutboundFrame>,
        unistream_fallback_tx: mpsc::Sender<WtOutboundFrame>,
        unistream_bytes: Arc<SharedQueueByteMeter>,
        on_packet_sent: Option<Arc<PacketSentCallback>>,
        datagram_send_calls: Arc<AtomicU64>,
    ) {
        join_set.spawn(async move {
            while let Some(frame) = datagram_rx.recv().await {
                match session.send_datagram(frame.bytes.clone()) {
                    Ok(()) => {
                        datagram_send_calls.fetch_add(1, Ordering::Relaxed);
                        if let Some(ref callback) = on_packet_sent {
                            callback();
                        }
                    }
                    // Datagrams are unreliable: record the failure and continue
                    // draining. `record_datagram_send_failure` increments the
                    // outbound-bridge datagram counter (issue 2030) and logs.
                    Err(e) => {
                        let reason = record_datagram_send_failure(&e);
                        if is_recoverable_on_unistream(reason) {
                            divert_to_unistream(
                                &unistream_fallback_tx,
                                &unistream_bytes,
                                frame,
                                reason,
                            );
                        }
                    }
                }
            }
            info!("WebTransport Datagram writer ended");
            "datagram_writer"
        });
    }
}

/// Caps the whole `wt_close` value (code and peer-supplied reason), then quotes it onto one line.
const CLOSE_CAUSE_FIELD_MAX_CHARS: usize = 200;

fn quote_capped(peer_text: &str) -> String {
    let capped: String = peer_text
        .chars()
        .take(CLOSE_CAUSE_FIELD_MAX_CHARS)
        .collect();
    format!("{capped:?}")
}

/// Which downlink topology the relay serves one receiver (#2723).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownlinkStreamMode {
    /// Pre-#2723 client: ONE persistent stream, no stream header.
    Single,
    /// The client advertised `ds=1`: one stream per (publisher, media kind), one
    /// receiver control stream and one shared overflow stream, each opening with
    /// a v1 stream header.
    PerPublisherV1,
}

/// Payload length of the #2723 v1 stream header. The frame carrying it is
/// length-prefixed like every other frame on the stream.
pub(crate) const DOWNLINK_STREAM_HEADER_LEN: usize = 15;

/// Magic bytes that open a #2723 downlink stream. Its first byte decodes as a
/// protobuf wire type that does not exist, so no valid `PacketWrapper` can begin
/// with it and a client seeing no magic is talking to a pre-#2723 relay.
const DOWNLINK_STREAM_HEADER_MAGIC: [u8; 4] = *b"VCDS";

/// Version byte of the #2723 stream header. A future revision bumps this AND the
/// `ds=<n>` value the client advertises.
pub(crate) const DOWNLINK_STREAM_PROTOCOL_VERSION: u8 = 1;

/// Publisher keys one receiver may hold: [`WT_MAX_DOWNLINK_STREAMS`] minus the
/// three always-available receiver-scoped lanes — control, audio (#2724) and
/// overflow.
pub(crate) const MAX_PUBLISHER_DOWNLINK_STREAMS: usize = WT_MAX_DOWNLINK_STREAMS - 3;

/// What one server-initiated downlink stream carries, as announced in its
/// header (#2723).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DownlinkStreamClass {
    Control,
    Publisher {
        session_id: u64,
        kind: PublisherStreamKind,
    },
    Overflow,
    /// Every speaker's audio on one receiver-scoped stream (#2724).
    Audio,
}

impl DownlinkStreamClass {
    fn code(self) -> u8 {
        match self {
            DownlinkStreamClass::Control => 0,
            DownlinkStreamClass::Publisher { .. } => 1,
            DownlinkStreamClass::Overflow => 2,
            DownlinkStreamClass::Audio => 3,
        }
    }

    pub(crate) fn header(self) -> [u8; DOWNLINK_STREAM_HEADER_LEN] {
        let mut out = [0u8; DOWNLINK_STREAM_HEADER_LEN];
        out[0..4].copy_from_slice(&DOWNLINK_STREAM_HEADER_MAGIC);
        out[4] = DOWNLINK_STREAM_PROTOCOL_VERSION;
        out[5] = self.code();
        match self {
            DownlinkStreamClass::Publisher { session_id, kind } => {
                out[6..14].copy_from_slice(&session_id.to_be_bytes());
                out[14] = kind.media_kind_code();
            }
            // Receiver-scoped, so no publisher id; the kind byte is filled in
            // anyway because the client stores it verbatim.
            DownlinkStreamClass::Audio => out[14] = audio_media_kind_code(),
            DownlinkStreamClass::Control | DownlinkStreamClass::Overflow => {}
        }
        out
    }
}

impl From<DownlinkStreamKey> for DownlinkStreamClass {
    fn from(key: DownlinkStreamKey) -> Self {
        match key {
            DownlinkStreamKey::Control => DownlinkStreamClass::Control,
            DownlinkStreamKey::Publisher { session_id, kind } => {
                DownlinkStreamClass::Publisher { session_id, kind }
            }
            DownlinkStreamKey::Audio => DownlinkStreamClass::Audio,
            DownlinkStreamKey::Shared => DownlinkStreamClass::Overflow,
        }
    }
}

impl DownlinkStreamClass {
    /// Control, then audio, then ONE media tier round-robin; media must not be
    /// split further (#1977's preference lives in the budgets, contract A5).
    /// Orders streams that ALREADY hold credit, nothing more.
    fn send_priority(self) -> i32 {
        match self {
            DownlinkStreamClass::Control => 10,
            DownlinkStreamClass::Audio => 7,
            DownlinkStreamClass::Publisher { .. } | DownlinkStreamClass::Overflow => 5,
        }
    }

    fn hand_off_capacity(self) -> usize {
        match self {
            DownlinkStreamClass::Control => WT_DOWNLINK_CONTROL_CHANNEL_CAPACITY,
            DownlinkStreamClass::Audio => WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY,
            DownlinkStreamClass::Overflow => WT_DOWNLINK_OVERFLOW_CHANNEL_CAPACITY,
            DownlinkStreamClass::Publisher { .. } => WT_DOWNLINK_KEY_CHANNEL_CAPACITY,
        }
    }

    fn reserved_slots(self) -> usize {
        match self {
            DownlinkStreamClass::Control => WT_DOWNLINK_CONTROL_RESERVE,
            _ => 0,
        }
    }

    /// Whether a sustainedly-parked write on this class may be shed by #1638's
    /// backpressure deadline. False for AUDIO alone: `reset` would discard the
    /// buffered audio the peer has yet to deliver (#2724, contract A6).
    fn sheds_on_backpressure(self) -> bool {
        !matches!(self, DownlinkStreamClass::Audio)
    }
}

/// The rule the CLIENT applies to a stream's first frame: `None` means "not a v1
/// header", i.e. a pre-#2723 relay, so the frame is a packet. `#[cfg(test)]`
/// because nothing on the relay decodes its own downlink headers.
#[cfg(test)]
pub(crate) fn parse_downlink_header(payload: &[u8]) -> Option<DownlinkStreamClass> {
    if payload.len() != DOWNLINK_STREAM_HEADER_LEN
        || payload[0..4] != DOWNLINK_STREAM_HEADER_MAGIC
        || payload[4] != DOWNLINK_STREAM_PROTOCOL_VERSION
    {
        return None;
    }
    match payload[5] {
        0 => Some(DownlinkStreamClass::Control),
        1 => {
            let mut id = [0u8; 8];
            id.copy_from_slice(&payload[6..14]);
            let kind = match payload[14] {
                k if k == PublisherStreamKind::Video.media_kind_code() => {
                    PublisherStreamKind::Video
                }
                k if k == PublisherStreamKind::Screen.media_kind_code() => {
                    PublisherStreamKind::Screen
                }
                _ => return None,
            };
            Some(DownlinkStreamClass::Publisher {
                session_id: u64::from_be_bytes(id),
                kind,
            })
        }
        2 => Some(DownlinkStreamClass::Overflow),
        3 => Some(DownlinkStreamClass::Audio),
        _ => None,
    }
}

/// Production always uses [`WT_DOWNLINK_STREAM_IDLE_TIMEOUT`]; #1637's seam.
#[cfg(not(test))]
fn downlink_idle_timeout() -> std::time::Duration {
    WT_DOWNLINK_STREAM_IDLE_TIMEOUT
}

#[cfg(not(test))]
fn downlink_idle_sweep() -> std::time::Duration {
    WT_DOWNLINK_STREAM_IDLE_SWEEP
}

#[cfg(test)]
static TEST_IDLE_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
static TEST_IDLE_SWEEP_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
fn downlink_idle_timeout() -> std::time::Duration {
    match TEST_IDLE_TIMEOUT_MS.load(Ordering::Relaxed) {
        0 => WT_DOWNLINK_STREAM_IDLE_TIMEOUT,
        ms => std::time::Duration::from_millis(ms),
    }
}

#[cfg(test)]
fn downlink_idle_sweep() -> std::time::Duration {
    match TEST_IDLE_SWEEP_MS.load(Ordering::Relaxed) {
        0 => WT_DOWNLINK_STREAM_IDLE_SWEEP,
        ms => std::time::Duration::from_millis(ms),
    }
}

/// Test-only override; `0` restores the production constants.
#[cfg(test)]
fn set_downlink_idle_for_test(timeout_ms: u64, sweep_ms: u64) {
    TEST_IDLE_TIMEOUT_MS.store(timeout_ms, Ordering::Relaxed);
    TEST_IDLE_SWEEP_MS.store(sweep_ms, Ordering::Relaxed);
}

/// What happened to one frame handed to [`write_one_frame`]. `Dropped` is the
/// `write_timeout` shed, already booked by [`account_unistream_shed`];
/// `WriterDead` means the caller must stop draining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameOutcome {
    Delivered,
    Dropped,
    WriterDead,
}

/// Open one server→client uni stream and, in per-publisher mode, write its v1
/// header frame before any packet.
async fn open_downlink_stream(
    session: &Session,
    class: Option<DownlinkStreamClass>,
    context: &'static str,
) -> Option<web_transport_quinn::SendStream> {
    let mut stream = match session.open_uni().await {
        Ok(s) => s,
        Err(e) => {
            error!("Error opening downlink UniStream ({context}): {e}");
            return None;
        }
    };
    // `None` is single-stream mode: no header, and quinn's default priority, so
    // a pre-#2723 client sees the byte stream it sees today.
    let Some(class) = class else {
        return Some(stream);
    };
    let _ = stream.set_priority(class.send_priority());
    let header = class.header();
    let len = (DOWNLINK_STREAM_HEADER_LEN as u32).to_be_bytes();
    if let Err(e) = stream.write_all(&len).await {
        error!("Error writing downlink stream-header length ({context}): {e}");
        return None;
    }
    if let Err(e) = stream.write_all(&header[..]).await {
        error!("Error writing downlink stream header ({context}): {e}");
        return None;
    }
    Some(stream)
}

/// Write ONE length-prefixed frame onto `slot`'s stream, opening it lazily and
/// applying the #1638 shed recovery, then #2726's escalation. Shared by the
/// single-stream writer and every #2723 per-key lane, so both run ONE
/// implementation. `rx` is the queue whose occupancy arms the shed.
#[allow(clippy::too_many_arguments)]
async fn write_one_frame(
    session: &Session,
    slot: &mut Option<web_transport_quinn::SendStream>,
    class: Option<DownlinkStreamClass>,
    priority: OutboundPriority,
    data: &Bytes,
    rx: &mpsc::Receiver<WtOutboundFrame>,
    queued: &SharedQueueByteMeter,
    ticker: &mut tokio::time::Interval,
    downlink_relief: &DownlinkReliefSignal,
    escalation: &DownlinkShedEscalation,
) -> FrameOutcome {
    if slot.is_none() {
        match open_downlink_stream(session, class, "initial").await {
            Some(stream) => *slot = Some(stream),
            None => return FrameOutcome::WriterDead,
        }
    }

    let len: u32 = data
        .len()
        .try_into()
        .expect("packet exceeds u32::MAX bytes; video frames should be well under 4GB");
    let len_header = len.to_be_bytes();

    let stream = slot.as_mut().expect("stream was just opened");
    // `None` is the #2724 audio lane: park on flow control with no deadline.
    let shed_reason = write_framed_with_backpressure_shed(
        stream,
        &len_header,
        data,
        rx,
        queued,
        ticker,
        class.is_none_or(DownlinkStreamClass::sheds_on_backpressure),
    )
    .await;

    let Some(reason) = shed_reason else {
        escalation.note_write_completed();
        return FrameOutcome::Delivered;
    };

    // Before the re-open, so the books survive a failed `open_uni`.
    account_unistream_shed(reason, priority, data.len(), downlink_relief);
    // #2726: a stage-2 close REPLACES the reset.
    if reason == "write_timeout" && escalate_unistream_shed(session, escalation) {
        return FrameOutcome::WriterDead;
    }
    if let Some(mut wedged) = slot.take() {
        let _ = wedged.reset(UNISTREAM_SHED_RESET_CODE);
    }
    let Some(mut fresh) = open_downlink_stream(session, class, reason).await else {
        return FrameOutcome::WriterDead;
    };

    if reason != "write_error" {
        *slot = Some(fresh);
        return FrameOutcome::Dropped;
    }

    if let Err(e) = fresh.write_all(&len_header).await {
        error!("Error writing length header to fresh UniStream after write-error retry: {e}");
        return FrameOutcome::WriterDead;
    }
    if let Err(e) = fresh.write_all(data).await {
        error!("Error writing payload to fresh UniStream after write-error retry: {e}");
        return FrameOutcome::WriterDead;
    }
    *slot = Some(fresh);
    escalation.note_write_completed();
    FrameOutcome::Delivered
}

/// `true` means the session was CLOSED, so the caller must stop rather than
/// reset and re-open (#2726). Reads no QUIC stats (contract E16).
fn escalate_unistream_shed(session: &Session, escalation: &DownlinkShedEscalation) -> bool {
    match escalation.record_shed() {
        EscalationAction::Proceed => false,
        EscalationAction::ArmedStage1 => {
            RELAY_DOWNLINK_SHED_ESCALATIONS_TOTAL
                .with_label_values(&["webtransport", "one"])
                .inc();
            warn!(
                "Receiver downlink escalated to stage 1: repeated #1638 sheds, so \
                 this receiver's camera VIDEO is shed at admission until the hold \
                 decays (#2726)"
            );
            false
        }
        EscalationAction::Close(reason) => {
            RELAY_DOWNLINK_SHED_ESCALATIONS_TOTAL
                .with_label_values(&["webtransport", "two"])
                .inc();
            RELAY_WT_SESSION_CLOSES_TOTAL
                .with_label_values(&["webtransport", reason.label()])
                .inc();
            warn!(
                "Closing a WebTransport session with code {} ({}): its downlink \
                 stayed wedged past the escalation threshold, so the client is \
                 asked to re-elect (#2726)",
                WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE,
                reason.label(),
            );
            session.close(
                WT_CLOSE_CODE_DOWNLINK_UNRECOVERABLE,
                WT_CLOSE_REASON_DOWNLINK_UNRECOVERABLE,
            );
            true
        }
    }
}

/// Everything a lane task needs, cloned once per lane (#2723).
struct LaneFactory {
    session: Session,
    unistream_bytes: Arc<SharedQueueByteMeter>,
    on_packet_sent: Option<Arc<PacketSentCallback>>,
    drops: DownlinkDropSink,
    escalation: DownlinkShedEscalation,
}

impl LaneFactory {
    fn spawn(&self, class: DownlinkStreamClass, tasks: &mut JoinSet<()>) -> DownlinkLane {
        let (tx, rx) = mpsc::channel::<WtOutboundFrame>(class.hand_off_capacity());
        let abort = tasks.spawn(run_downlink_lane(
            self.session.clone(),
            class,
            rx,
            self.unistream_bytes.clone(),
            self.on_packet_sent.clone(),
            self.drops.relief().clone(),
            self.escalation.clone(),
        ));
        DownlinkLane {
            tx,
            abort,
            last_dispatch: tokio::time::Instant::now(),
            owns_stream: true,
            reserved: class.reserved_slots(),
            // A first death is replaced at once; the gate only spaces the
            // REPLACEMENTS, which stamp it forward.
            respawn_after: tokio::time::Instant::now(),
        }
    }
}

/// Holds one slot on [`RELAY_DOWNLINK_STREAM_SLOTS`] while its lane task lives.
/// The decrement is in `Drop` so CANCELLATION also releases it.
struct SlotGuard;

impl SlotGuard {
    fn acquire() -> Self {
        RELAY_DOWNLINK_STREAM_SLOTS
            .with_label_values(&["webtransport"])
            .inc();
        SlotGuard
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        RELAY_DOWNLINK_STREAM_SLOTS
            .with_label_values(&["webtransport"])
            .dec();
    }
}

/// One publisher key's route: its hand-off queue plus the idle reaper's clock.
struct DownlinkLane {
    tx: mpsc::Sender<WtOutboundFrame>,
    abort: tokio::task::AbortHandle,
    last_dispatch: tokio::time::Instant,
    /// False when this key is pinned to the SHARED overflow stream, whose `tx` and
    /// `abort` these then are, so retiring this entry must touch neither.
    owns_stream: bool,
    reserved: usize,
    /// Earliest instant THIS key may be replaced again once its task exits.
    /// Per key, so one key's churn cannot hold another key's recovery.
    respawn_after: tokio::time::Instant,
}

impl DownlinkLane {
    fn pinned_to(overflow: &DownlinkLane) -> Self {
        Self {
            tx: overflow.tx.clone(),
            abort: overflow.abort.clone(),
            last_dispatch: tokio::time::Instant::now(),
            owns_stream: false,
            reserved: overflow.reserved,
            respawn_after: overflow.respawn_after,
        }
    }
}

/// `relay_packet_drops_total{drop_reason}` for a frame let go because its lane's
/// task had exited. A member of [`crate::metrics::RELAY_DROP_KINDS`].
const LANE_DEAD_DROP_REASON: &str = "lane_dead";

/// Why [`DownlinkStreamMap::dispatch`] let a frame go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaneReject {
    /// The lane's queue was full, or at its reserve floor for this priority.
    Full,
    /// The lane's task has exited, so its channel is closed until it is replaced.
    Dead,
}

/// One receiver's downlink streams, keyed by publisher and media kind (#2723).
struct DownlinkStreamMap {
    factory: LaneFactory,
    control: DownlinkLane,
    /// Receiver-scoped audio (#2724). Eager like `control` and never idle-reaped.
    audio: DownlinkLane,
    publishers: std::collections::HashMap<DownlinkStreamKey, DownlinkLane>,
    /// Publisher keys that hold a stream of their own, maintained incrementally
    /// so the per-frame cap check is O(1). `publishers` also holds the keys
    /// pinned to the overflow stream, which cost no slot.
    owned_publisher_streams: usize,
    overflow: Option<DownlinkLane>,
    /// Lanes whose entry has been reaped but whose task has not exited yet.
    retiring: Vec<(tokio::task::AbortHandle, tokio::time::Instant)>,
    tasks: JoinSet<()>,
    /// Gates the relief stamp on a SUSTAINED run of tail drops (#2745).
    drop_run: LaneDropRun,
    /// The overflow lane's own respawn gate. It lives here rather than on the
    /// lane because replacing it means dropping the entry entirely, to unpin
    /// every key that holds a clone of its sender.
    overflow_respawn_after: tokio::time::Instant,
}

/// How long this receiver has been tail-dropping without a break. A run ends
/// when no drop lands for a whole [`WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN`].
#[derive(Default)]
struct LaneDropRun {
    started: Option<tokio::time::Instant>,
    last: Option<tokio::time::Instant>,
}

impl LaneDropRun {
    /// Record a drop at `now` and report whether the run has reached `sustain`.
    /// Keeps returning `true` while the run continues, because the epoch decays
    /// on its own.
    fn note_drop(&mut self, now: tokio::time::Instant, sustain: std::time::Duration) -> bool {
        let broke = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) > sustain);
        if broke {
            self.started = Some(now);
        }
        self.last = Some(now);
        self.started
            .is_some_and(|started| now.saturating_duration_since(started) >= sustain)
    }
}

impl DownlinkStreamMap {
    fn new(factory: LaneFactory) -> Self {
        let mut tasks = JoinSet::new();
        // Eager lane, lazy stream: neither receiver-scoped lane may lose its
        // slot to a publisher, and a task opens no QUIC stream until a frame
        // arrives.
        let control = factory.spawn(DownlinkStreamClass::Control, &mut tasks);
        let audio = factory.spawn(DownlinkStreamClass::Audio, &mut tasks);
        Self {
            factory,
            control,
            audio,
            publishers: std::collections::HashMap::new(),
            owned_publisher_streams: 0,
            overflow: None,
            retiring: Vec::new(),
            tasks,
            drop_run: LaneDropRun::default(),
            overflow_respawn_after: tokio::time::Instant::now(),
        }
    }

    /// Replace a lane whose task has exited, so the next frame for that key has a
    /// live one. Nothing is pushed onto `retiring`: the task is already gone.
    ///
    /// Gated PER KEY by [`WT_DOWNLINK_LANE_RESPAWN_COOLDOWN`], so a key rejecting
    /// at full frame rate cannot hold another key's recovery, and a whole map
    /// dying together recovers in one cooldown rather than one lane per cooldown.
    fn replace_dead_lane(&mut self, key: DownlinkStreamKey) {
        let now = tokio::time::Instant::now();
        match key {
            DownlinkStreamKey::Control => {
                if now < self.control.respawn_after {
                    return;
                }
                self.control = self
                    .factory
                    .spawn(DownlinkStreamClass::Control, &mut self.tasks);
                self.control.respawn_after = now + WT_DOWNLINK_LANE_RESPAWN_COOLDOWN;
            }
            DownlinkStreamKey::Audio => {
                if now < self.audio.respawn_after {
                    return;
                }
                self.audio = self
                    .factory
                    .spawn(DownlinkStreamClass::Audio, &mut self.tasks);
                self.audio.respawn_after = now + WT_DOWNLINK_LANE_RESPAWN_COOLDOWN;
            }
            DownlinkStreamKey::Shared => self.replace_dead_overflow(now),
            DownlinkStreamKey::Publisher { .. } => {
                let Some((owns_stream, respawn_after)) = self
                    .publishers
                    .get(&key)
                    .map(|lane| (lane.owns_stream, lane.respawn_after))
                else {
                    return;
                };
                // A pinned entry's `tx` is the OVERFLOW lane's, so a `Closed` on
                // it names the overflow lane, not this key.
                if !owns_stream {
                    self.replace_dead_overflow(now);
                    return;
                }
                if now < respawn_after {
                    return;
                }
                let mut fresh = self.factory.spawn(key.into(), &mut self.tasks);
                fresh.respawn_after = now + WT_DOWNLINK_LANE_RESPAWN_COOLDOWN;
                // In place, so the key keeps the slot it already owned and
                // `owned_publisher_streams` does not move.
                self.publishers.insert(key, fresh);
            }
        }
    }

    /// Forget the overflow lane and every key pinned to it; those entries hold a
    /// clone of its sender, so leaving them behind keeps routing to a dead lane.
    /// The next frame re-opens it through [`Self::ensure_overflow`].
    fn replace_dead_overflow(&mut self, now: tokio::time::Instant) {
        if now < self.overflow_respawn_after {
            return;
        }
        self.overflow_respawn_after = now + WT_DOWNLINK_LANE_RESPAWN_COOLDOWN;
        self.overflow = None;
        self.publishers.retain(|_, lane| lane.owns_stream);
    }

    fn ensure_overflow(&mut self) {
        if self.overflow.is_none() {
            self.overflow = Some(
                self.factory
                    .spawn(DownlinkStreamClass::Overflow, &mut self.tasks),
            );
        }
    }

    fn overflow_mut(&mut self) -> &mut DownlinkLane {
        self.ensure_overflow();
        self.overflow.as_mut().expect("just ensured")
    }

    /// Sticky for the life of the MAP ENTRY: migrating a live key would split its
    /// frames across two streams with no ordering between them.
    fn publisher_lane(&mut self, key: DownlinkStreamKey) -> &mut DownlinkLane {
        if self.publishers.contains_key(&key) {
            return self.publishers.get_mut(&key).expect("just checked");
        }
        if self.owned_publisher_streams < MAX_PUBLISHER_DOWNLINK_STREAMS {
            let lane = self.factory.spawn(key.into(), &mut self.tasks);
            self.owned_publisher_streams += 1;
            return self.publishers.entry(key).or_insert(lane);
        }
        self.ensure_overflow();
        let pinned = DownlinkLane::pinned_to(self.overflow.as_ref().expect("just ensured"));
        self.publishers.entry(key).or_insert(pinned)
    }

    fn dispatch(&mut self, frame: WtOutboundFrame) {
        let key = frame.key;
        // EXACTLY `evaluate_dual`'s never-preempt set.
        let protected = matches!(
            frame.priority,
            OutboundPriority::Critical | OutboundPriority::Control
        );
        let sent_to_overflow;
        let rejected = {
            let lane = match key {
                DownlinkStreamKey::Control => &mut self.control,
                DownlinkStreamKey::Audio => &mut self.audio,
                DownlinkStreamKey::Shared => self.overflow_mut(),
                DownlinkStreamKey::Publisher { .. } => self.publisher_lane(key),
            };
            sent_to_overflow = !lane.owns_stream || key == DownlinkStreamKey::Shared;
            lane.last_dispatch = tokio::time::Instant::now();
            // `is_closed` only where the reserve floor already rejected, so the
            // classification is exact without an extra load on the hot path.
            if !protected && lane.tx.capacity() <= lane.reserved && !lane.tx.is_closed() {
                Some((LaneReject::Full, frame))
            } else {
                match lane.tx.try_send(frame) {
                    Ok(()) => None,
                    Err(mpsc::error::TrySendError::Full(f)) => Some((LaneReject::Full, f)),
                    Err(mpsc::error::TrySendError::Closed(f)) => Some((LaneReject::Dead, f)),
                }
            }
        };
        if sent_to_overflow && rejected.is_none() {
            // Only one of the two is a cap event.
            let cause = if key == DownlinkStreamKey::Shared {
                "unattributed"
            } else {
                "cap"
            };
            RELAY_DOWNLINK_STREAM_OVERFLOW_FRAMES_TOTAL
                .with_label_values(&["webtransport", cause])
                .inc();
        }
        // TAIL DROP, never a block: whoever lets the frame go debits the byte
        // meter (#2717).
        if let Some((reject, f)) = rejected {
            self.factory
                .unistream_bytes
                .on_dequeue(f.priority, f.bytes.len());
            RELAY_DOWNLINK_STREAM_QUEUE_DROPS_TOTAL
                .with_label_values(&["webtransport", queue_byte_kind_label(f.priority)])
                .inc();
            // Also on the series operators already alert on.
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", outbound_drop_kind(f.priority)])
                .inc();
            match reject {
                LaneReject::Full => {
                    self.factory.drops.record(
                        publisher_session_id(f.key),
                        "channel_full",
                        outbound_drop_kind(f.priority),
                    );
                    if self.drop_run.note_drop(
                        tokio::time::Instant::now(),
                        WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN,
                    ) {
                        self.factory.drops.stamp_relief();
                    }
                }
                // A dead lane is a broken stream, not a slow receiver.
                LaneReject::Dead => {
                    self.factory.drops.record(
                        0,
                        LANE_DEAD_DROP_REASON,
                        outbound_drop_kind(f.priority),
                    );
                    self.replace_dead_lane(key);
                }
            }
            if f.priority == OutboundPriority::Critical {
                let cause = match reject {
                    LaneReject::Full => "its lane's reserve was also exhausted",
                    LaneReject::Dead => "its lane's task had exited",
                };
                error!(
                    "Dropped a Critical control frame ({} bytes) at the downlink \
                     dispatcher: {}",
                    f.bytes.len(),
                    cause
                );
            }
        }
    }

    /// Close publisher streams that have gone quiet: the relay has no per-peer
    /// "publisher left" event, so absence of frames IS the leave signal.
    fn reap_idle(&mut self) {
        // A `JoinSet` frees a finished task's cell only when the owner joins it.
        while self.tasks.try_join_next().is_some() {}

        let now = tokio::time::Instant::now();
        // Dropping the sender cancels nothing while the task is parked inside
        // `write_all`, so: one grace, then abort.
        self.retiring.retain(|(abort, due)| {
            if abort.is_finished() {
                return false;
            }
            if now >= *due {
                abort.abort();
                return false;
            }
            true
        });

        let stale: Vec<DownlinkStreamKey> = self
            .publishers
            .iter()
            .filter(|(_, lane)| now.duration_since(lane.last_dispatch) >= downlink_idle_timeout())
            .map(|(key, _)| *key)
            .collect();
        RELAY_DOWNLINK_LANE_TASK_ENTRIES
            .with_label_values(&["webtransport"])
            .set(self.tasks.len() as f64);
        if stale.is_empty() {
            return;
        }
        for key in &stale {
            if let Some(lane) = self.publishers.remove(key) {
                if lane.owns_stream {
                    self.owned_publisher_streams -= 1;
                    self.retiring
                        .push((lane.abort, now + WT_DOWNLINK_LANE_RETIRE_GRACE));
                }
            }
        }
        RELAY_DOWNLINK_STREAM_IDLE_REAPS_TOTAL
            .with_label_values(&["webtransport"])
            .inc_by(stale.len() as f64);
    }

    /// Drop every sender, then await each lane so its clean `finish` reaches the
    /// wire, bounded by [`WT_DOWNLINK_TEARDOWN_DRAIN`].
    async fn close_and_join(self) {
        let Self {
            factory,
            control,
            audio,
            publishers,
            owned_publisher_streams: _,
            overflow,
            retiring,
            mut tasks,
            drop_run: _,
            overflow_respawn_after: _,
        } = self;
        drop(control);
        drop(audio);
        drop(publishers);
        drop(overflow);
        drop(retiring);
        drop(factory);
        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(WT_DOWNLINK_TEARDOWN_DRAIN, drain)
            .await
            .is_err()
        {
            warn!("Downlink lanes did not drain within the teardown budget; aborting them");
        }
    }
}

/// Drain one key's hand-off queue onto its own QUIC stream (#2723). A task parked
/// on a wedged `write_all` blocks only its own key.
#[allow(clippy::too_many_arguments)]
async fn run_downlink_lane(
    session: Session,
    class: DownlinkStreamClass,
    rx: mpsc::Receiver<WtOutboundFrame>,
    unistream_bytes: Arc<SharedQueueByteMeter>,
    on_packet_sent: Option<Arc<PacketSentCallback>>,
    downlink_relief: DownlinkReliefSignal,
    escalation: DownlinkShedEscalation,
) {
    let _slot = SlotGuard::acquire();
    // Both must survive CANCELLATION: `reap_idle` aborts a retiring lane.
    let mut stream = LaneStream(None);
    let mut queue = LaneQueue {
        rx,
        bytes: unistream_bytes.clone(),
    };
    let mut backpressure_ticker = tokio::time::interval(WT_UNISTREAM_BACKPRESSURE_POLL);

    while let Some(frame) = queue.rx.recv().await {
        // On DEQUEUE, not on a successful write: the slot is free from here and
        // a shed frame must not stay charged (#2717).
        unistream_bytes.on_dequeue(frame.priority, frame.bytes.len());
        match write_one_frame(
            &session,
            &mut stream.0,
            Some(class),
            frame.priority,
            &frame.bytes,
            &queue.rx,
            &unistream_bytes,
            &mut backpressure_ticker,
            &downlink_relief,
            &escalation,
        )
        .await
        {
            FrameOutcome::Delivered => {
                if let Some(ref callback) = on_packet_sent {
                    callback();
                }
            }
            FrameOutcome::Dropped => {}
            FrameOutcome::WriterDead => break,
        }
    }

    if let Some(mut open) = stream.0.take() {
        let _ = open.finish();
        RELAY_DOWNLINK_STREAM_FINISHES_TOTAL
            .with_label_values(&["webtransport"])
            .inc();
    }
}

/// Resets a lane's stream on ABORT: quinn's `SendStream::drop` only `finish()`es
/// it, leaving it open after `SlotGuard` released the slot (#2757).
struct LaneStream(Option<web_transport_quinn::SendStream>);

impl Drop for LaneStream {
    fn drop(&mut self) {
        if let Some(mut open) = self.0.take() {
            let _ = open.reset(UNISTREAM_SHED_RESET_CODE);
            RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
                .with_label_values(&["webtransport", LANE_ABORT_RESET_REASON])
                .inc();
        }
    }
}

/// Releases a lane's #2717 byte charge on EVERY exit path, cancellation
/// included; an abort otherwise inflates `byte_fill` until reconnect (#2745).
struct LaneQueue {
    rx: mpsc::Receiver<WtOutboundFrame>,
    bytes: Arc<SharedQueueByteMeter>,
}

impl Drop for LaneQueue {
    fn drop(&mut self) {
        self.rx.close();
        while let Ok(frame) = self.rx.try_recv() {
            self.bytes.on_dequeue(frame.priority, frame.bytes.len());
        }
    }
}

/// Whether a `send_datagram` failure means "never carryable as a datagram here"
/// (#2716). Teardown reasons would only queue onto a dying stream.
fn is_recoverable_on_unistream(reason: &str) -> bool {
    matches!(reason, "too_large" | "unsupported")
}

/// Divert a payload the datagram lane cannot carry, and count the outcome
/// (#2716). `try_send`: blocking would put audio behind a wedged uni stream, and
/// an accepted divert is charged on the reliable lane (#2717).
fn divert_to_unistream(
    tx: &mpsc::Sender<WtOutboundFrame>,
    unistream_bytes: &SharedQueueByteMeter,
    frame: WtOutboundFrame,
    reason: &'static str,
) {
    let outcome = match enqueue_unistream(tx, unistream_bytes, frame) {
        Ok(()) => "unistream",
        Err(e) => {
            debug!("Datagram fallback to unistream failed (reason={reason}): {e}");
            "dropped"
        }
    };
    RELAY_DATAGRAM_UNISTREAM_FALLBACKS_TOTAL
        .with_label_values(&[reason, outcome])
        .inc();
}

/// Classify a datagram `send_datagram` failure into a BOUNDED `reason` label.
///
/// The match is exhaustive over both error enums, so a new upstream variant is a
/// compile error rather than an unbounded label. The Display string is NEVER a
/// label value.
fn datagram_send_failure_reason(err: &SessionError) -> &'static str {
    use web_transport_quinn::quinn::SendDatagramError;
    match err {
        SessionError::SendDatagramError(SendDatagramError::TooLarge) => "too_large",
        SessionError::SendDatagramError(SendDatagramError::ConnectionLost(_)) => "connection_lost",
        SessionError::SendDatagramError(SendDatagramError::UnsupportedByPeer) => "unsupported",
        SessionError::SendDatagramError(SendDatagramError::Disabled) => "disabled",
        SessionError::ConnectionError(_) => "connection_lost",
        SessionError::WebTransportError(_) => "webtransport",
    }
}

/// Record an outbound WebTransport datagram send ERROR (issue 2030), returning
/// the classified reason for [`is_recoverable_on_unistream`] (#2716).
///
/// ERRORS ONLY: an overflow returns `Ok` and never reaches here (#2712).
fn record_datagram_send_failure(err: &SessionError) -> &'static str {
    let reason = datagram_send_failure_reason(err);
    RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
        .with_label_values(&["webtransport", reason])
        .inc();
    debug!("Error sending datagram (reason={}): {}", reason, err);
    reason
}

/// Pure backpressure predicate for the #1638 writer shed.
///
/// `true` iff the lane is at or above [`WT_UNISTREAM_BACKPRESSURE_SHED_RATIO`]
/// in ANY bounded dimension: the receiver's admission channel in single-stream
/// mode or one key's hand-off queue per #2723, plus the receiver-wide byte
/// budgets, which catch a stalled lane that plateaus below the slot gate.
fn channel_is_backed_up(depth: usize, max_capacity: usize, queued: &SharedQueueByteMeter) -> bool {
    let slot_fill = dimension_fill(depth, max_capacity);
    let byte_fill = queued.max_budget_fill(wt_unistream_byte_budget_for);
    slot_fill.max(byte_fill) >= WT_UNISTREAM_BACKPRESSURE_SHED_RATIO as f32
}

/// Write one length-prefixed frame onto the persistent uni stream, shedding ONLY
/// under sustained real backpressure (#1638).
///
/// `None` means the write completed. `Some("write_error")` means the stream is
/// broken and the caller RE-SENDS on a fresh one; `Some("write_timeout")` means
/// it stayed parked while [`channel_is_backed_up`] held for
/// [`WT_UNISTREAM_WRITE_DEADLINE`] and the caller resets and DROPS the frame.
/// Never `write_timeout` when `shed_on_backpressure` is false.
///
/// The write is never bounded by wall-clock: it is `select!`ed against a
/// [`WT_UNISTREAM_BACKPRESSURE_POLL`] tick and any tick below the ratio resets
/// the accumulator, so a healthy lane can never be shed however slowly its poll
/// is scheduled.
async fn write_framed_with_backpressure_shed(
    stream: &mut web_transport_quinn::SendStream,
    len_header: &[u8; 4],
    data: &Bytes,
    unistream_rx: &mpsc::Receiver<WtOutboundFrame>,
    queued: &SharedQueueByteMeter,
    ticker: &mut tokio::time::Interval,
    shed_on_backpressure: bool,
) -> Option<&'static str> {
    let max_capacity = unistream_rx.max_capacity();

    // `pin!` so the future survives repeated `select!` polls with its partial
    // progress intact.
    let framed_write = async {
        stream.write_all(len_header).await?;
        stream.write_all(data).await
    };
    tokio::pin!(framed_write);

    // #2724: no ticker and no accumulator, so no path returns "write_timeout"
    // for a class that must never be reset out from under buffered data.
    if !shed_on_backpressure {
        return match framed_write.await {
            Ok(()) => None,
            Err(e) => {
                warn!(
                    "Error writing to a non-sheddable downlink stream ({}); resetting, \
                     reopening and re-sending the frame on a fresh stream",
                    e
                );
                Some("write_error")
            }
        };
    }

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
                if channel_is_backed_up(depth, max_capacity, queued) {
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

/// Every value is one the taxonomy already emits, so operators' existing alerts
/// on `videocall_outbound_channel_drops_total` keep seeing this loss (#2723).
/// `reason` for a reset applied by [`LaneStream::drop`], not by the #1638 shed.
pub(crate) const LANE_ABORT_RESET_REASON: &str = "lane_abort";

/// The publisher this frame came from; `0` when the key is receiver-scoped.
fn publisher_session_id(key: DownlinkStreamKey) -> u64 {
    match key {
        DownlinkStreamKey::Publisher { session_id, .. } => session_id,
        DownlinkStreamKey::Control | DownlinkStreamKey::Audio | DownlinkStreamKey::Shared => 0,
    }
}

fn outbound_drop_kind(priority: OutboundPriority) -> &'static str {
    match priority {
        OutboundPriority::Critical => "overflow_critical",
        OutboundPriority::Control => "control",
        OutboundPriority::Video => "video",
        OutboundPriority::Screen => "screen",
        OutboundPriority::Audio => "audio",
        OutboundPriority::ProbeEcho => ECHO_DROP_KIND,
    }
}

/// Book one shed: the reset counter for either reason, and for `write_timeout`
/// — the only reason whose frame is DROPPED — the relief stamp plus a drop
/// counter for `Critical`, `ProbeEcho` and `Control`. The relief stamp is NOT
/// gated on the frame's class: `write_timeout` is a LANE state (#2718, #2721).
fn account_unistream_shed(
    reason: &'static str,
    priority: OutboundPriority,
    frame_len: usize,
    downlink_relief: &DownlinkReliefSignal,
) {
    RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
        .with_label_values(&["webtransport", reason])
        .inc();
    if reason != "write_timeout" {
        return;
    }
    downlink_relief.stamp(RELIEF_SOURCE_UNISTREAM_SHED);
    match priority {
        OutboundPriority::Critical => {
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", "overflow_critical"])
                .inc();
            error!(
                "Shed a Critical control frame ({frame_len} bytes) off a wedged \
                 UniStream (overflow_critical)"
            );
        }
        // #2721: same series as the admission shed.
        OutboundPriority::ProbeEcho => {
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", ECHO_DROP_KIND])
                .inc();
        }
        OutboundPriority::Control => {
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", outbound_drop_kind(priority)])
                .inc();
            warn!(
                "Shed a Control frame ({frame_len} bytes) off a wedged UniStream: \
                 a session-lifecycle packet was lost to downlink backpressure"
            );
        }
        OutboundPriority::Audio | OutboundPriority::Video | OutboundPriority::Screen => {}
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
                    if matches!(e, actix::prelude::SendError::Closed(_)) {
                        return;
                    }
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
// These exercise the REAL production `spawn_unistream_writer` against a REAL
// `web_transport_quinn` session pair stood up in-process over loopback. The
// writer is hard-typed to that session, so there is no trait seam to mock.

#[cfg(test)]
mod writer_shed_tests {
    use super::*;
    use crate::actors::chat_server::{observe_downlink_relief, DownlinkRelayState};
    use crate::actors::session_logic::DOWNLINK_EPOCH_NEVER;
    use crate::actors::transports::wt_chat_session::{wt_unistream_admit, WtAdmission};
    use actix::prelude::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use web_transport_quinn::quinn;

    /// Minimal actor implementing `Handler<WtInbound>` so we can build a real
    /// `WebTransportBridge` without standing up a full `WtChatSession` (which
    /// needs NATS, SessionManager, addresses, …). The bridge's writer task —
    /// the code under test — never touches this actor; it only drains the
    /// outbound channel onto the session's uni stream. The reader tasks forward
    /// inbound frames here, which the test ignores.
    pub(super) struct StubActor;
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
    pub(super) fn build_test_server() -> (std::net::SocketAddr, web_transport_quinn::Server) {
        build_test_server_with_transport(None)
    }

    pub(super) fn build_test_server_with_transport(
        transport: Option<Arc<quinn::TransportConfig>>,
    ) -> (std::net::SocketAddr, web_transport_quinn::Server) {
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

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(crypto).expect("quic server config"),
        ));
        if let Some(transport) = transport {
            server_config.transport_config(transport);
        }
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let endpoint = quinn::Endpoint::server(server_config, addr).expect("server endpoint");
        let bound = endpoint.local_addr().expect("local addr");
        (bound, web_transport_quinn::Server::new(endpoint))
    }

    /// Connect a `web_transport_quinn` client (no cert verification) to the
    /// given loopback address.
    pub(super) async fn connect_test_client(
        addr: std::net::SocketAddr,
    ) -> web_transport_quinn::Session {
        let client = web_transport_quinn::ClientBuilder::new()
            .dangerous()
            .with_no_certificate_verification()
            .expect("client builder");
        let url =
            url::Url::parse(&format!("https://127.0.0.1:{}/test", addr.port())).expect("parse url");
        client.connect(url).await.expect("client connect")
    }

    /// The signal plus the epoch behind it, so a test sees what was stamped.
    pub(super) fn test_relief_signal() -> (DownlinkReliefSignal, Arc<AtomicU64>) {
        let epoch = Arc::new(AtomicU64::new(DOWNLINK_EPOCH_NEVER));
        (DownlinkReliefSignal::new(Arc::clone(&epoch)), epoch)
    }

    pub(super) fn test_drop_sink() -> (DownlinkDropSink, Arc<AtomicU64>) {
        let (relief, epoch) = test_relief_signal();
        (test_drop_sink_with(relief, Arc::default()), epoch)
    }

    pub(super) fn test_drop_sink_with(
        relief: DownlinkReliefSignal,
        congestion: Arc<std::sync::Mutex<crate::actors::session_logic::CongestionTracker>>,
    ) -> DownlinkDropSink {
        DownlinkDropSink::new(
            "test-room",
            1,
            "webtransport",
            relief,
            congestion,
            crate::actors::session_logic::SessionDropBooking::open(),
        )
    }

    /// Drive a frame onto the bridge's outbound unistream channel.
    pub(super) fn push(
        tx: &mpsc::Sender<WtOutboundFrame>,
        n: usize,
    ) -> Result<(), mpsc::error::TrySendError<WtOutboundFrame>> {
        tx.try_send(WtOutboundFrame::new(
            OutboundPriority::Video,
            Bytes::from(vec![0xCD; n]),
        ))
    }

    /// REGRESSION TEST (#1638): a stalled-downlink receiver whose outbound
    /// channel is ACTUALLY BACKED UP must NOT wedge that channel full
    /// indefinitely — the backpressure-gated writer sheds within the grace and
    /// resumes draining.
    ///
    /// BITES: replace the shed call with a bare `framed_write.await` and the
    /// channel stays full until the outer timeout fires.
    // Serial: its real sheds move counters sibling tests assert deltas on.
    #[actix_rt::test]
    #[serial_test::serial]
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
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
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
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            Some(on_sent),
            Arc::new(AtomicU64::new(0)),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
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
    /// BITES: make `write_error` `continue` like `write_timeout` and the callback
    /// count stays 0.
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
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let sent = Arc::new(AtomicUsize::new(0));
        let sent_cb = sent.clone();
        let on_sent: PacketSentCallback = Box::new(move || {
            sent_cb.fetch_add(1, Ordering::SeqCst);
        });

        let stub = StubActor.start();
        let (drops, epoch) = test_drop_sink();
        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            stub,
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            Some(on_sent),
            Arc::new(AtomicU64::new(0)),
            drops,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        // ONE frame, LARGE on purpose: a small write would complete into the local
        // send buffer and return `Ok` before STOP_SENDING is processed, so the
        // test would pass for the wrong reason.
        let frame_len = 4 * 1024 * 1024;
        uni_tx
            .send(WtOutboundFrame::new(
                OutboundPriority::Video,
                Bytes::from(vec![0xAB; frame_len]),
            ))
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

        // #2718, the guard's NEGATIVE direction: this frame was re-sent and
        // DELIVERED, so arming relief would shed a healthy receiver's non-base
        // video. BITES: invert the guard to stamp on any shed reason.
        assert_eq!(
            epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "a write_error frame is re-sent and delivered, so the shed must not \
             stamp the relief epoch",
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
    /// A non-backed-up parked write parks FOREVER, so the outer timeout firing IS
    /// the pass condition. BITES: a wall-clock
    /// `timeout(WT_UNISTREAM_WRITE_DEADLINE, framed_write)` sheds regardless of
    /// channel depth.
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

        // EMPTY in BOTH dimensions — the healthy condition the gate reads.
        const CAP: usize = 16;
        let (_uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let queued = SharedQueueByteMeter::default();
        assert_eq!(
            uni_rx.max_capacity().saturating_sub(uni_rx.capacity()),
            0,
            "precondition: the channel under test must be empty (non-backed-up)"
        );
        assert_eq!(
            queued.snapshot().queued_total(),
            0,
            "precondition: the byte dimension must also be empty"
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
            shed = write_framed_with_backpressure_shed(&mut stream, &header, &data, &uni_rx, &queued, &mut ticker, true) => {
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

    struct PathStatIntervalGuard;
    impl Drop for PathStatIntervalGuard {
        fn drop(&mut self) {
            crate::webtransport::set_path_stat_sample_interval_for_test(
                crate::actors::transports::wt_chat_session::WT_HEARTBEAT_INTERVAL,
            );
        }
    }

    fn stream_reset_total(reason: &str) -> f64 {
        RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
            .with_label_values(&["webtransport", reason])
            .get()
    }

    fn datagram_send_failure_total() -> f64 {
        use prometheus::core::Collector;
        RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .collect()
            .iter()
            .flat_map(|family| family.get_metric())
            .map(|metric| metric.get_counter().get_value())
            .sum()
    }

    /// #2712: silently-evicted datagrams reach the derived silent-drop gauge and
    /// are INVISIBLE to the #2030 counter.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn datagram_queue_overflow_publishes_silent_drops_and_no_send_errors() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        crate::webtransport::set_path_stat_sample_interval_for_test(Duration::from_millis(50));
        let _interval_guard = PathStatIntervalGuard;

        let mut transport = quinn::TransportConfig::default();
        transport.datagram_send_buffer_size(1);
        let (addr, mut server) = build_test_server_with_transport(Some(Arc::new(transport)));

        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let server_conn = (*server_session).clone();

        let room = "dgram-overflow-2712";
        let session_id = "overflow-session-1";
        const PUSHES: u64 = 500;
        const PAYLOAD: usize = 256;

        let (dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(PUSHES as usize);
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let calls = Arc::new(AtomicU64::new(0));

        let failures_before = datagram_send_failure_total();

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            None,
            calls.clone(),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );
        let sampler = crate::webtransport::spawn_connection_path_sampler(
            server_conn.clone(),
            room,
            session_id,
            calls.clone(),
        );

        let outcome = tokio::time::timeout(Duration::from_secs(20), async {
            // Small enough that every send takes the `Ok` arm: loss is eviction,
            // not refusal.
            for _ in 0..PUSHES {
                dgram_tx
                    .send(WtOutboundFrame::new(
                        OutboundPriority::Audio,
                        Bytes::from(vec![0xA5; PAYLOAD]),
                    ))
                    .await
                    .expect("datagram channel accepts the push");
            }

            let drained = std::time::Instant::now() + Duration::from_secs(10);
            while calls.load(Ordering::Relaxed) < PUSHES && std::time::Instant::now() < drained {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                calls.load(Ordering::Relaxed),
                PUSHES,
                "the bridge datagram writer must record every successful \
                 send_datagram; a short count means sends failed, which would \
                 invalidate the overflow assertions below"
            );

            // Poll on BOTH: a mid-drain tick publishes a partial call count.
            let published = std::time::Instant::now() + Duration::from_secs(5);
            let mut drops = 0.0_f64;
            let mut published_calls = 0.0_f64;
            let mut frames = 0.0_f64;
            while (drops <= 0.0 || published_calls < PUSHES as f64)
                && std::time::Instant::now() < published
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
                drops = crate::metrics::RELAY_CONNECTION_DATAGRAM_SILENT_DROPS
                    .with_label_values(&[room, session_id])
                    .get();
                published_calls = crate::metrics::RELAY_CONNECTION_DATAGRAM_SEND_CALLS
                    .with_label_values(&[room, session_id])
                    .get();
                frames = crate::metrics::RELAY_CONNECTION_FRAME_TX_DATAGRAM
                    .with_label_values(&[room, session_id])
                    .get();
            }

            assert_eq!(
                published_calls, PUSHES as f64,
                "the sampler must publish the writer's shared call counter"
            );
            assert!(
                drops > 0.0,
                "quinn evicted queued datagrams (calls={published_calls}, \
                 transmitted frames={frames}) but the silent-drop gauge read \
                 {drops} — the calls-minus-frames series is not wired up"
            );
            assert!(
                frames < published_calls,
                "test setup failed: quinn transmitted every datagram \
                 (frames={frames}, calls={published_calls}), so no eviction \
                 happened — datagram_send_buffer_size is not taking effect"
            );
            // #2731: without this the tx->rx field swap ships green.
            assert!(
                frames > 0.0,
                "the frames series must come from frame_tx.datagram: a reading \
                 of {frames} means the sampler is on the receive-side field and \
                 every transmitted datagram reads as a silent drop"
            );

            assert_eq!(
                datagram_send_failure_total(),
                failures_before,
                "the #2030 send-ERROR counter must not move for overflow \
                 evictions — send_datagram returned Ok for all {PUSHES} sends"
            );

            Ok::<(), anyhow::Error>(())
        })
        .await;

        crate::webtransport::stop_connection_path_sampler(sampler, room, session_id);

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("datagram overflow test failed: {e}"),
            Err(_) => panic!("datagram overflow test timed out after 20s"),
        }
    }

    fn fallback_total(reason: &str, outcome: &str) -> f64 {
        RELAY_DATAGRAM_UNISTREAM_FALLBACKS_TOTAL
            .with_label_values(&[reason, outcome])
            .get()
    }

    fn too_large_failure_total() -> f64 {
        RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .with_label_values(&["webtransport", "too_large"])
            .get()
    }

    /// #2717 B1. BITES on a slot-only `channel_is_backed_up`: nothing is reset
    /// and the watchdog fires.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_byte_shed_stalled_receiver_is_still_reset_within_the_grace() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        // Holds the session but NEVER accepts or reads the uni stream, so QUIC
        // credits drain to zero and the writer parks.
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let total = crate::constants::wt_outbound_channel_capacity();
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(total);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let meter = Arc::new(SharedQueueByteMeter::default());

        let resets_before = stream_reset_total("write_timeout");

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            meter.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        let frame_bytes = crate::constants::tier_frame_bytes(
            &videocall_aq::constants::VIDEO_QUALITY_TIERS
                [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
        );

        let outcome = tokio::time::timeout(Duration::from_secs(25), async {
            let mut shed_seen = false;
            for _ in 0..(total * 2) {
                let admitted = wt_unistream_admit(
                    &uni_tx,
                    &meter,
                    OutboundPriority::Video,
                    Bytes::from(vec![0x33; frame_bytes]),
                    DownlinkStreamKey::Control,
                );
                if matches!(admitted, WtAdmission::PriorityDropped { .. }) {
                    shed_seen = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            assert!(
                shed_seen,
                "test setup failed: the enqueue policy never shed, so the lane \
                 never reached the plateau this test is about",
            );

            let depth = total.saturating_sub(uni_tx.capacity());
            assert!(
                depth * 2 < total,
                "test setup failed: the lane filled {depth}/{total} slots, past \
                 the slot gate, so a slot-only predicate would arm and the test \
                 would pass on the un-fixed code",
            );

            let deadline = std::time::Instant::now()
                + WT_UNISTREAM_WRITE_DEADLINE * 4
                + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if stream_reset_total("write_timeout") > resets_before {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!(
                "no write_timeout stream reset after 4x the grace: the byte-shed \
                 plateau ({depth}/{total} slots) never armed the #1638 gate, so \
                 this receiver's video is frozen with no recovery path",
            );
        })
        .await;

        outcome.expect("the writer must shed a wedged receiver, not park forever");
    }

    fn outbound_drops(kind: &str) -> f64 {
        OUTBOUND_CHANNEL_DROPS_TOTAL
            .with_label_values(&["webtransport", kind])
            .get()
    }

    /// #2746 item 4. BITES: restore the empty Control arm.
    #[test]
    #[serial_test::serial]
    fn a_control_shed_victim_is_counted_and_logged() {
        let (relief, _epoch) = test_relief_signal();
        let control_before = outbound_drops("control");
        let critical_before = outbound_drops("overflow_critical");

        account_unistream_shed("write_error", OutboundPriority::Control, 64, &relief);
        assert_eq!(
            outbound_drops("control") - control_before,
            0.0,
            "a write_error frame is re-sent on the fresh stream, so nothing was \
             dropped",
        );

        account_unistream_shed("write_timeout", OutboundPriority::Control, 64, &relief);
        assert_eq!(
            outbound_drops("control") - control_before,
            1.0,
            "a shed lifecycle packet must be visible on the same kind the \
             admission hop books",
        );
        assert_eq!(
            outbound_drops("overflow_critical") - critical_before,
            0.0,
            "Control is not Critical: the loud series must stay for the \
             lifecycle packets that really are Critical",
        );
    }

    /// BITES: book `overflow_critical` for `ProbeEcho`, or delete the arm.
    #[test]
    #[serial_test::serial]
    fn a_shed_probe_echo_books_the_rtt_kind_never_overflow_critical() {
        let (relief, _epoch) = test_relief_signal();
        let rtt_before = outbound_drops(ECHO_DROP_KIND);
        let critical_before = outbound_drops("overflow_critical");

        account_unistream_shed("write_timeout", OutboundPriority::ProbeEcho, 64, &relief);

        assert_eq!(outbound_drops(ECHO_DROP_KIND) - rtt_before, 1.0);
        assert_eq!(
            outbound_drops("overflow_critical") - critical_before,
            0.0,
            "a probe echo is not a lifecycle packet",
        );

        account_unistream_shed("write_timeout", OutboundPriority::Critical, 64, &relief);
        assert_eq!(
            outbound_drops("overflow_critical") - critical_before,
            1.0,
            "the Critical booking must survive the probe-echo arm",
        );
        assert_eq!(
            outbound_drops(ECHO_DROP_KIND) - rtt_before,
            1.0,
            "a Critical shed must not inflate the probe series",
        );
    }

    /// #2721 decision pin. BITES: gate `downlink_relief.stamp` on the frame
    /// class; stamp on `write_error`.
    #[test]
    #[serial_test::serial]
    fn a_probe_echo_shed_still_stamps_relief_but_a_write_error_never_does() {
        let (relief, epoch) = test_relief_signal();
        let stamps_before = relief_stamp_total(RELIEF_SOURCE_UNISTREAM_SHED);
        let rtt_before = outbound_drops(ECHO_DROP_KIND);

        account_unistream_shed("write_error", OutboundPriority::ProbeEcho, 64, &relief);
        assert_eq!(
            epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "a write_error frame is re-sent on the fresh stream, so nothing was \
             dropped and no relief is owed",
        );
        assert_eq!(
            outbound_drops(ECHO_DROP_KIND) - rtt_before,
            0.0,
            "a re-sent echo is not an unechoed probe",
        );

        account_unistream_shed("write_timeout", OutboundPriority::ProbeEcho, 64, &relief);
        assert_ne!(
            epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "the lane was backed up for the whole grace — that is congestion \
             whatever frame happened to be at the head",
        );
        assert!(relief_stamp_total(RELIEF_SOURCE_UNISTREAM_SHED) > stamps_before);
    }

    pub(super) fn relief_stamp_total(source: &str) -> f64 {
        crate::metrics::RELAY_DOWNLINK_RELIEF_STAMPS_TOTAL
            .with_label_values(&[source])
            .get()
    }

    /// BITES: delete `downlink_relief.stamp(...)`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_write_timeout_shed_stamps_the_relief_epoch_and_emits_one_downlink_congestion() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        // Never reads the uni stream, so credits drain and the writer parks.
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let total = crate::constants::wt_outbound_channel_capacity();
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(total);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let meter = Arc::new(SharedQueueByteMeter::default());
        let (relief, epoch) = test_relief_signal();
        let drops = test_drop_sink_with(relief, Arc::default());

        assert_eq!(
            epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "a fresh receiver starts with no relief crossing",
        );
        let stamps_before = relief_stamp_total(RELIEF_SOURCE_UNISTREAM_SHED);
        let resets_before = stream_reset_total("write_timeout");

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            meter.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            drops,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        let frame_bytes = crate::constants::tier_frame_bytes(
            &videocall_aq::constants::VIDEO_QUALITY_TIERS
                [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
        );

        let outcome = tokio::time::timeout(Duration::from_secs(25), async {
            for _ in 0..(total * 2) {
                if matches!(
                    wt_unistream_admit(
                        &uni_tx,
                        &meter,
                        OutboundPriority::Video,
                        Bytes::from(vec![0x33; frame_bytes]),
                        DownlinkStreamKey::Control,
                    ),
                    WtAdmission::PriorityDropped { .. }
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }

            let deadline = std::time::Instant::now()
                + WT_UNISTREAM_WRITE_DEADLINE * 4
                + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if epoch.load(Ordering::Relaxed) != DOWNLINK_EPOCH_NEVER {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!(
                "the relief epoch is still DOWNLINK_EPOCH_NEVER after 4x the \
                 #1638 grace (write_timeout resets in this window: {}) — the \
                 shed reset the stream and dropped a frame without telling the \
                 receiver to step down",
                stream_reset_total("write_timeout") - resets_before,
            );
        })
        .await;
        outcome.expect("the writer must stamp the relief epoch when it sheds");

        assert!(
            stream_reset_total("write_timeout") > resets_before,
            "test setup failed: the epoch moved without a write_timeout reset, \
             so this is not the path under test",
        );
        assert!(
            relief_stamp_total(RELIEF_SOURCE_UNISTREAM_SHED) > stamps_before,
            "the shed stamped the epoch but is invisible to operators — \
             relay_downlink_relief_stamps_total{{source=\"unistream_shed\"}} did \
             not move",
        );

        let relay_state = std::cell::Cell::new(DownlinkRelayState::new());
        let (congested_now, first) = observe_downlink_relief(&relay_state, &epoch);
        assert!(
            congested_now,
            "the stamped epoch must read as congested inside the relief window — \
             this is the gate that sheds non-base camera video",
        );
        assert!(
            first.entered_congestion,
            "the fan-out closure must see the healthy->congested edge and queue \
             one DOWNLINK_CONGESTION",
        );

        let (still_congested, second) = observe_downlink_relief(&relay_state, &epoch);
        assert!(still_congested, "the window has not elapsed");
        assert!(
            !second.entered_congestion,
            "a second fan-out tick inside the same episode must NOT queue another \
             DOWNLINK_CONGESTION",
        );
    }

    fn overflow_critical_total() -> f64 {
        OUTBOUND_CHANNEL_DROPS_TOTAL
            .with_label_values(&["webtransport", "overflow_critical"])
            .get()
    }

    /// A sacrificed AES_KEY must reach the lifecycle-drop counter.
    /// BITES: drop the `Critical` arm.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_shed_critical_frame_is_counted_as_overflow_critical() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        const CAP: usize = 16;
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(CAP);
        let (relief, epoch) = test_relief_signal();
        let drops = test_drop_sink_with(relief, Arc::default());

        let critical_before = overflow_critical_total();
        let resets_before = stream_reset_total("write_timeout");

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            None,
            Arc::new(AtomicU64::new(0)),
            drops,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        // FIRST frame is Critical and larger than the stream window, so the writer
        // parks on it and it becomes the shed victim.
        uni_tx
            .send(WtOutboundFrame::new(
                OutboundPriority::Critical,
                Bytes::from(vec![0xC1; 4 * 1024 * 1024]),
            ))
            .await
            .expect("push the Critical frame");

        let outcome = tokio::time::timeout(Duration::from_secs(25), async {
            // Past the shed ratio, so the shed arms while it is parked.
            while uni_tx.capacity() > CAP / 2 {
                if push(&uni_tx, 1024).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }

            let deadline = std::time::Instant::now()
                + WT_UNISTREAM_WRITE_DEADLINE * 4
                + Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                if overflow_critical_total() > critical_before {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!(
                "a Critical frame was shed off the wedged stream (write_timeout \
                 resets in this window: {}) but overflow_critical never moved — a \
                 lifecycle packet died silently",
                stream_reset_total("write_timeout") - resets_before,
            );
        })
        .await;
        outcome.expect("the shed must count a Critical victim");

        assert!(
            stream_reset_total("write_timeout") > resets_before,
            "test setup failed: overflow_critical moved without a write_timeout \
             reset, so this is not the path under test",
        );
        assert_ne!(
            epoch.load(Ordering::Relaxed),
            DOWNLINK_EPOCH_NEVER,
            "the same shed must also arm relief",
        );
    }

    /// BITES: delete the writer's `on_dequeue` and the meter never falls.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn the_unistream_writer_credits_the_byte_meter_as_it_drains() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        // A HEALTHY receiver: reads, so the writer drains instead of parking.
        let client_session = connect_test_client(addr).await;
        let client_drainer = tokio::spawn(async move {
            if let Ok(mut recv) = client_session.accept_uni().await {
                let mut buf = vec![0u8; 64 * 1024];
                while let Ok(Some(_)) = recv.read(&mut buf).await {}
            }
            std::future::pending::<()>().await;
        });
        let server_session = server_session_fut.await.expect("join server session");

        const FRAMES: usize = 8;
        const FRAME_BYTES: usize = 3_000;
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(64);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let meter = Arc::new(SharedQueueByteMeter::default());

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            meter.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        // Through the production credit site.
        for _ in 0..FRAMES {
            enqueue_unistream(
                &uni_tx,
                &meter,
                WtOutboundFrame::new(
                    OutboundPriority::Video,
                    Bytes::from(vec![0x42; FRAME_BYTES]),
                ),
            )
            .expect("the 64-slot lane accepts the push");
        }
        assert_eq!(
            meter.queued_for(OutboundPriority::Video),
            FRAMES * FRAME_BYTES,
            "precondition: the enqueue must have charged the lane, or the drain \
             assertion below passes vacuously",
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while meter.queued_for(OutboundPriority::Video) > 0 && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert_eq!(
            meter.queued_for(OutboundPriority::Video),
            0,
            "the writer drained {FRAMES} frames but the camera bucket still \
             reads {} bytes — nothing debits the meter, so the byte shed would \
             pin this receiver permanently",
            meter.queued_for(OutboundPriority::Video),
        );

        client_drainer.abort();
    }

    /// #2716: a payload above the live `max_datagram_size()` must be DELIVERED on
    /// the reliable uni stream and STILL book `too_large`.
    /// BITES: 0 of 1204 bytes arrive.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn oversized_datagram_falls_back_to_the_unistream_instead_of_too_large() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        // Pin the path MTU so the ceiling cannot climb past the payload.
        let mut transport = quinn::TransportConfig::default();
        transport.mtu_discovery_config(None);
        let (addr, mut server) = build_test_server_with_transport(Some(Arc::new(transport)));

        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        const PAYLOAD: usize = crate::actors::packet_handler::DATAGRAM_MAX_SIZE;
        assert!(
            server_session.max_datagram_size() < PAYLOAD,
            "test setup failed: max_datagram_size() is {} — not below the \
             {PAYLOAD}-byte payload, so no fallback would be exercised",
            server_session.max_datagram_size(),
        );

        // LOCKSTEP (#2716): the gap between quinn's raw ceiling and the WT one IS
        // the session header.
        let raw_connection: &web_transport_quinn::quinn::Connection = &server_session;
        assert_eq!(
            raw_connection
                .max_datagram_size()
                .expect("loopback peer advertises max_datagram_frame_size")
                - server_session.max_datagram_size(),
            crate::constants::WT_DATAGRAM_SESSION_HEADER_BYTES,
        );

        let received = Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
        let received_writer = received.clone();
        let client_drainer = tokio::spawn(async move {
            if let Ok(mut recv) = client_session.accept_uni().await {
                let mut buf = vec![0u8; 8 * 1024];
                while let Ok(Some(n)) = recv.read(&mut buf).await {
                    received_writer.lock().await.extend_from_slice(&buf[..n]);
                }
            }
            std::future::pending::<()>().await;
        });

        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let (dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let calls = Arc::new(AtomicU64::new(0));

        let fallbacks_before = fallback_total("too_large", "unistream");
        let too_large_before = too_large_failure_total();

        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            None,
            calls.clone(),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            DownlinkShedEscalation::new(),
        );

        dgram_tx
            .send(WtOutboundFrame::new(
                OutboundPriority::Audio,
                Bytes::from(vec![0x7E; PAYLOAD]),
            ))
            .await
            .expect("datagram channel accepts the push");

        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut got = Vec::new();
        while std::time::Instant::now() < deadline {
            got = received.lock().await.clone();
            if got.len() >= 4 + PAYLOAD {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        assert_eq!(
            got.len(),
            4 + PAYLOAD,
            "the oversized datagram must arrive as ONE length-prefixed frame on \
             the reliable uni stream; got {} bytes. On the un-fixed writer \
             send_datagram refuses it TooLarge and nothing is ever written.",
            got.len(),
        );
        assert_eq!(
            u32::from_be_bytes(got[..4].try_into().expect("4-byte header")) as usize,
            PAYLOAD,
            "the fallback must reuse the unistream writer's length framing",
        );
        assert!(
            got[4..].iter().all(|b| *b == 0x7E),
            "the delivered payload must be the diverted datagram, byte for byte",
        );

        assert_eq!(
            fallback_total("too_large", "unistream") - fallbacks_before,
            1.0,
            "exactly one too_large-to-unistream divert must be counted",
        );
        assert_eq!(
            too_large_failure_total() - too_large_before,
            1.0,
            "the #2030 too_large error counter must STILL record the refusal — \
             diverting the packet must not blind the panel that reads it",
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "a diverted packet never reached send_datagram, so the #2712 \
             datagram_send_calls counter must stay at zero",
        );

        client_drainer.abort();
    }

    /// #2716: a full reliable lane must DROP the fallback, never await.
    #[test]
    #[serial_test::serial]
    fn a_full_unistream_channel_drops_the_fallback_instead_of_blocking() {
        let (tx, _rx) = mpsc::channel::<WtOutboundFrame>(1);
        tx.try_send(WtOutboundFrame::control(Bytes::from_static(b"occupied")))
            .expect("first slot is free");

        let dropped_before = fallback_total("too_large", "dropped");
        let queued_before = fallback_total("too_large", "unistream");

        let meter = SharedQueueByteMeter::default();
        divert_to_unistream(
            &tx,
            &meter,
            WtOutboundFrame::control(Bytes::from_static(b"overflow")),
            "too_large",
        );

        assert_eq!(
            fallback_total("too_large", "dropped") - dropped_before,
            1.0,
            "a full reliable lane must count the fallback as dropped",
        );
        assert_eq!(
            fallback_total("too_large", "unistream") - queued_before,
            0.0,
            "a dropped fallback must not also be counted as queued",
        );
        assert_eq!(
            meter.queued_for(OutboundPriority::Control),
            0,
            "a REFUSED divert never entered the reliable lane, so charging its \
             bytes there would pin the byte shed on a packet that is not queued \
             (#2717)",
        );
    }

    /// #2717: an ACCEPTED divert moves the packet onto the reliable lane, so
    /// that lane's byte meter must carry it.
    #[test]
    #[serial_test::serial]
    fn an_accepted_divert_charges_the_reliable_lane_byte_meter() {
        let (tx, _rx) = mpsc::channel::<WtOutboundFrame>(1);
        let meter = SharedQueueByteMeter::default();
        let payload = Bytes::from(vec![0x5A; 1_400]);

        divert_to_unistream(
            &tx,
            &meter,
            WtOutboundFrame::new(OutboundPriority::Audio, payload),
            "too_large",
        );

        assert_eq!(
            meter.queued_for(OutboundPriority::Audio),
            1_400,
            "a diverted packet occupies the reliable lane; leaving it uncharged \
             lets the lane hold bytes the shed cannot see",
        );
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
    use crate::actors::priority_drop::OutboundPriority;
    use crate::constants::{OUTBOUND_SCREEN_BYTE_BUDGET, OUTBOUND_VIDEO_BYTE_BUDGET};

    fn no_bytes() -> SharedQueueByteMeter {
        SharedQueueByteMeter::default()
    }

    #[test]
    fn empty_channel_is_never_backed_up() {
        // The healthy steady state: a draining writer keeps depth at 0. This is
        // the case the v1 wall-clock shed got wrong — it MUST be "not backed up".
        assert!(!channel_is_backed_up(0, 512, &no_bytes()));
        assert!(!channel_is_backed_up(0, 16, &no_bytes()));
    }

    #[test]
    fn below_half_is_not_backed_up() {
        // Just under the ratio. The cap is a fixture; the predicate is a pure
        // ratio.
        assert!(!channel_is_backed_up(255, 512, &no_bytes()));
        // And at the small test cap: 7 < 8.
        assert!(!channel_is_backed_up(7, 16, &no_bytes()));
    }

    #[test]
    fn at_or_above_half_is_backed_up() {
        // Exactly at the ratio boundary (depth == 50% of cap) counts as backed
        // up — the gate is `>=`, so the boundary arms the shed.
        assert!(channel_is_backed_up(256, 512, &no_bytes()));
        assert!(channel_is_backed_up(8, 16, &no_bytes()));
        // Above the ratio, and at full.
        assert!(channel_is_backed_up(400, 512, &no_bytes()));
        assert!(channel_is_backed_up(512, 512, &no_bytes()));
        assert!(channel_is_backed_up(16, 16, &no_bytes()));
    }

    #[test]
    fn zero_capacity_is_never_backed_up() {
        // Degenerate guard: a 0-cap channel (which the production resolver never
        // builds) must not divide-by-zero into a false shed.
        assert!(!channel_is_backed_up(0, 0, &no_bytes()));
        assert!(!channel_is_backed_up(5, 0, &no_bytes()));
    }

    #[test]
    fn ratio_matches_the_documented_half_threshold() {
        // Pin the production ratio used by the predicate. If someone retunes
        // WT_UNISTREAM_BACKPRESSURE_SHED_RATIO they must revisit this boundary
        // (and the shed-grace math in the doc comments).
        assert_eq!(WT_UNISTREAM_BACKPRESSURE_SHED_RATIO, 0.5);
    }

    /// The byte-shed plateau must arm the #1638 gate. BITES on slots alone.
    #[test]
    fn the_byte_shed_plateau_arms_the_backpressure_gate() {
        let total = crate::constants::wt_outbound_channel_capacity();

        // Camera parked where `wt_unistream_decision` stops admitting.
        let frame = crate::constants::tier_frame_bytes(
            &videocall_aq::constants::VIDEO_QUALITY_TIERS
                [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
        );
        let frames = (OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100) / frame + 1;
        let meter = no_bytes();
        meter.on_enqueue(OutboundPriority::Video, frames * frame);

        assert!(
            frames * 2 < total,
            "test setup: {frames} frames must be far below the {total}-slot \
             gate, or this does not exercise the byte dimension",
        );
        assert!(
            !channel_is_backed_up(frames, total, &no_bytes()),
            "precondition: on SLOTS alone this plateau is not backed up — that \
             is precisely the B1 blind spot",
        );
        assert!(
            channel_is_backed_up(frames, total, &meter),
            "a lane pinned at its camera byte shed point must arm the #1638 \
             gate; otherwise a wedged receiver is never recovered",
        );

        let screen = no_bytes();
        screen.on_enqueue(
            OutboundPriority::Screen,
            OUTBOUND_SCREEN_BYTE_BUDGET * 90 / 100,
        );
        assert!(channel_is_backed_up(1, total, &screen));
    }

    /// Below every budget, still unarmed.
    #[test]
    fn a_lane_below_its_byte_budgets_is_not_backed_up() {
        let total = crate::constants::wt_outbound_channel_capacity();
        let meter = no_bytes();
        meter.on_enqueue(
            OutboundPriority::Video,
            OUTBOUND_VIDEO_BYTE_BUDGET * 40 / 100,
        );
        meter.on_enqueue(
            OutboundPriority::Screen,
            OUTBOUND_SCREEN_BYTE_BUDGET * 40 / 100,
        );
        assert!(!channel_is_backed_up(1, total, &meter));

        // No byte budget, so the lane still has to fill on SLOTS.
        let unbudgeted = no_bytes();
        unbudgeted.on_enqueue(OutboundPriority::Audio, 100 * OUTBOUND_SCREEN_BYTE_BUDGET);
        assert!(!channel_is_backed_up(1, total, &unbudgeted));
        assert!(channel_is_backed_up(total / 2, total, &unbudgeted));
    }
}

#[cfg(test)]
mod datagram_send_failure_tests {
    //! Unit tests for the outbound WebTransport datagram send-failure counter
    //! (issue 2030). These drive the real production
    //! [`datagram_send_failure_reason`] and [`record_datagram_send_failure`] —
    //! there is no re-implementation of the classification to drift out of sync.
    //!
    //! `RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL` is a process-global
    //! prometheus counter that the #2712 overflow test also sums, so both take the
    //! DEFAULT `#[serial_test::serial]` key — that is what makes the `.get()`
    //! deltas below exact under cargo's parallel runner.

    use super::*;
    use web_transport_quinn::quinn::SendDatagramError;

    /// Every `reason` the classifier can emit maps to the exact string the metric
    /// doc advertises. `ConnectionLost`/`ConnectionError` have no public
    /// constructor, but the exhaustive match handles them at compile time.
    /// MUTATION: change any arm's returned string and the matching assert fails.
    #[test]
    fn classifies_each_constructible_variant() {
        use web_transport_quinn::WebTransportError;

        assert_eq!(
            datagram_send_failure_reason(&SessionError::SendDatagramError(
                SendDatagramError::TooLarge
            )),
            "too_large"
        );
        assert_eq!(
            datagram_send_failure_reason(&SessionError::SendDatagramError(
                SendDatagramError::UnsupportedByPeer
            )),
            "unsupported"
        );
        assert_eq!(
            datagram_send_failure_reason(&SessionError::SendDatagramError(
                SendDatagramError::Disabled
            )),
            "disabled"
        );
        assert_eq!(
            datagram_send_failure_reason(&SessionError::WebTransportError(
                WebTransportError::UnknownSession
            )),
            "webtransport"
        );
    }

    /// The classifier NEVER returns the error's Display string — only a value
    /// from the fixed closed set — so the label stays cardinality-bounded even
    /// when the underlying error carries an unbounded peer-supplied message.
    ///
    /// MUTATION: replace the classifier body with `err.to_string().leak()` (or
    /// any Display-derived value) and this fails.
    #[test]
    fn reason_is_from_the_closed_set() {
        const CLOSED_SET: &[&str] = &[
            "too_large",
            "connection_lost",
            "unsupported",
            "disabled",
            "webtransport",
        ];
        for err in [
            SessionError::SendDatagramError(SendDatagramError::TooLarge),
            SessionError::SendDatagramError(SendDatagramError::UnsupportedByPeer),
            SessionError::SendDatagramError(SendDatagramError::Disabled),
        ] {
            let reason = datagram_send_failure_reason(&err);
            assert!(
                CLOSED_SET.contains(&reason),
                "reason {reason:?} escaped the bounded closed set"
            );
        }
    }

    /// `record_datagram_send_failure` increments exactly the
    /// `{transport="webtransport", reason=<classified>}` series and nothing
    /// else. Reads the specific labeled series before/after and asserts the
    /// delta, so this exercises the real production increment path.
    ///
    /// MUTATION: remove the `.inc()` in `record_datagram_send_failure` and the
    /// `too_large` delta drops to 0 → fail. Change the `reason` label passed to
    /// `with_label_values` (or the classifier's `TooLarge` arm) and the
    /// asserted `too_large` series stops moving → fail.
    #[test]
    #[serial_test::serial]
    fn record_increments_the_classified_series() {
        let too_large_before = RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .with_label_values(&["webtransport", "too_large"])
            .get();
        let disabled_before = RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .with_label_values(&["webtransport", "disabled"])
            .get();

        record_datagram_send_failure(&SessionError::SendDatagramError(
            SendDatagramError::TooLarge,
        ));

        let too_large_after = RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .with_label_values(&["webtransport", "too_large"])
            .get();
        let disabled_after = RELAY_OUTBOUND_BRIDGE_DATAGRAM_SEND_FAILURES_TOTAL
            .with_label_values(&["webtransport", "disabled"])
            .get();

        assert_eq!(
            too_large_after - too_large_before,
            1.0,
            "the classified series must advance by exactly one"
        );
        assert_eq!(
            disabled_after - disabled_before,
            0.0,
            "an unrelated reason series must not move"
        );
    }
}

/// #2723: the per-publisher downlink stream map — wire header, key routing,
/// wedge isolation, per-stream shed, idle/teardown close, cap + overflow.
#[cfg(test)]
mod downlink_stream_tests {
    use super::writer_shed_tests::{
        build_test_server, build_test_server_with_transport, connect_test_client, push,
        relief_stamp_total, test_drop_sink, test_drop_sink_with, test_relief_signal, StubActor,
    };
    use super::*;
    use crate::actors::session_logic::RELIEF_SOURCE_OUTBOUND_DROP;
    use crate::actors::transports::wt_chat_session::{wt_unistream_admit, WtAdmission};
    use crate::constants::{
        WT_DOWNLINK_KEY_CHANNEL_CAPACITY, WT_SHED_ESCALATION_ROUND,
        WT_SHED_ESCALATION_STAGE1_ROUNDS, WT_SHED_ESCALATION_STAGE2_ROUNDS,
    };
    use actix::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    /// A short channel reads ~99% full to the #2717 policy, which then sheds
    /// every media frame before the bridge sees it.
    fn shared_channel_capacity() -> usize {
        crate::constants::wt_outbound_channel_capacity()
    }

    fn video_key(session_id: u64) -> DownlinkStreamKey {
        DownlinkStreamKey::Publisher {
            session_id,
            kind: PublisherStreamKind::Video,
        }
    }

    fn screen_key(session_id: u64) -> DownlinkStreamKey {
        DownlinkStreamKey::Publisher {
            session_id,
            kind: PublisherStreamKind::Screen,
        }
    }

    fn publisher_class(session_id: u64, kind: PublisherStreamKind) -> DownlinkStreamClass {
        DownlinkStreamClass::Publisher { session_id, kind }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum StreamEvent {
        Opened(DownlinkStreamClass),
        Frame(DownlinkStreamClass, usize),
        Finished(DownlinkStreamClass),
        Reset(DownlinkStreamClass),
        /// The first frame carried no v1 header: a pre-#2723 single stream.
        LegacyFrame(Vec<u8>),
    }

    /// Distinguishes a clean FIN from a RESET; `read_length_prefixed_frame`
    /// collapses both into `Ok(None)`.
    async fn read_frame(recv: &mut web_transport_quinn::RecvStream) -> Result<Option<Vec<u8>>, ()> {
        let mut len = [0u8; 4];
        match recv.read_exact(&mut len).await {
            Ok(()) => {}
            Err(web_transport_quinn::ReadExactError::FinishedEarly(_)) => return Ok(None),
            Err(_) => return Err(()),
        }
        let mut payload = vec![0u8; u32::from_be_bytes(len) as usize];
        match recv.read_exact(&mut payload).await {
            Ok(()) => Ok(Some(payload)),
            Err(web_transport_quinn::ReadExactError::FinishedEarly(_)) => Ok(None),
            Err(_) => Err(()),
        }
    }

    /// A stream whose class matches `wedge` is accepted but NEVER read, which
    /// parks the relay's write for exactly one key on QUIC flow control.
    async fn collect_stream(
        mut recv: web_transport_quinn::RecvStream,
        out: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
        wedge: Option<DownlinkStreamClass>,
    ) {
        let head = match read_frame(&mut recv).await {
            Ok(Some(p)) => p,
            _ => return,
        };
        let Some(class) = parse_downlink_header(&head) else {
            let _ = out.send(StreamEvent::LegacyFrame(head));
            while let Ok(Some(p)) = read_frame(&mut recv).await {
                let _ = out.send(StreamEvent::LegacyFrame(p));
            }
            return;
        };
        let _ = out.send(StreamEvent::Opened(class));
        if wedge == Some(class) {
            // `received_reset` waits WITHOUT consuming bytes, so credits stay
            // exhausted and this key's write stays parked.
            if let Ok(Some(_code)) = recv.received_reset().await {
                let _ = out.send(StreamEvent::Reset(class));
            }
            return;
        }
        loop {
            match read_frame(&mut recv).await {
                Ok(Some(p)) => {
                    let _ = out.send(StreamEvent::Frame(class, p.len()));
                }
                Ok(None) => {
                    let _ = out.send(StreamEvent::Finished(class));
                    return;
                }
                Err(()) => {
                    let _ = out.send(StreamEvent::Reset(class));
                    return;
                }
            }
        }
    }

    struct Harness {
        server_session: Session,
        uni_tx: mpsc::Sender<WtOutboundFrame>,
        dgram_tx: mpsc::Sender<WtOutboundFrame>,
        meter: Arc<SharedQueueByteMeter>,
        congestion: Arc<std::sync::Mutex<crate::actors::session_logic::CongestionTracker>>,
        events: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
        relief_epoch: Arc<AtomicU64>,
        escalation: DownlinkShedEscalation,
        bridge: Option<WebTransportBridge>,
    }

    impl Harness {
        /// Through the PRODUCTION credit site, so the byte meter is charged as in
        /// production.
        fn offer(&self, key: DownlinkStreamKey, priority: OutboundPriority, len: usize) {
            let frame = WtOutboundFrame::keyed(priority, Bytes::from(vec![0xCD; len]), key);
            enqueue_unistream(&self.uni_tx, &self.meter, frame)
                .expect("the production-capacity test channel must accept the frame");
        }

        fn try_offer(
            &self,
            key: DownlinkStreamKey,
            priority: OutboundPriority,
            len: usize,
        ) -> bool {
            let frame = WtOutboundFrame::keyed(priority, Bytes::from(vec![0xCD; len]), key);
            enqueue_unistream(&self.uni_tx, &self.meter, frame).is_ok()
        }

        async fn collect_until(
            &mut self,
            budget: Duration,
            mut pred: impl FnMut(&[StreamEvent]) -> bool,
        ) -> Vec<StreamEvent> {
            let mut seen = Vec::new();
            let deadline = tokio::time::Instant::now() + budget;
            loop {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    return seen;
                }
                match tokio::time::timeout(deadline - now, self.events.recv()).await {
                    Ok(Some(ev)) => {
                        seen.push(ev);
                        if pred(&seen) {
                            return seen;
                        }
                    }
                    Ok(None) | Err(_) => return seen,
                }
            }
        }
    }

    async fn spin_up(mode: DownlinkStreamMode, wedge: Option<DownlinkStreamClass>) -> Harness {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("server session");

        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(recv) = client_session.accept_uni().await {
                tokio::spawn(collect_stream(recv, events_tx.clone(), wedge));
            }
        });

        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(shared_channel_capacity());
        let (dgram_tx, dgram_rx) =
            mpsc::channel::<WtOutboundFrame>(crate::constants::WT_DATAGRAM_CHANNEL_CAPACITY);
        let meter = Arc::new(SharedQueueByteMeter::default());
        let (relief, relief_epoch) = test_relief_signal();
        let congestion: Arc<std::sync::Mutex<crate::actors::session_logic::CongestionTracker>> =
            Arc::default();
        let drops = test_drop_sink_with(relief, Arc::clone(&congestion));
        let escalation = DownlinkShedEscalation::new();
        let bridge = WebTransportBridge::new_with_callback(
            server_session.clone(),
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            meter.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            drops,
            mode,
            escalation.clone(),
        );
        Harness {
            server_session,
            uni_tx,
            dgram_tx,
            meter,
            congestion,
            events,
            relief_epoch,
            escalation,
            bridge: Some(bridge),
        }
    }

    struct IdleOverride;

    impl IdleOverride {
        fn set(timeout_ms: u64, sweep_ms: u64) -> Self {
            set_downlink_idle_for_test(timeout_ms, sweep_ms);
            IdleOverride
        }
    }

    impl Drop for IdleOverride {
        fn drop(&mut self) {
            set_downlink_idle_for_test(0, 0);
        }
    }

    #[test]
    fn the_stream_header_pins_the_v1_wire_contract() {
        assert_eq!(DOWNLINK_STREAM_HEADER_LEN, 15);
        assert_eq!(
            DownlinkStreamClass::Control.header(),
            [b'V', b'C', b'D', b'S', 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        );
        assert_eq!(
            publisher_class(0x0102_0304_0506_0708, PublisherStreamKind::Video).header(),
            [b'V', b'C', b'D', b'S', 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, 1],
            "publisher id is big-endian and VIDEO is media-kind 1",
        );
        let screen = publisher_class(9, PublisherStreamKind::Screen).header();
        assert_eq!(screen[5], 1, "class byte 1 = publisher");
        assert_eq!(screen[14], 3, "SCREEN is media-kind 3");
        assert_eq!(
            DownlinkStreamClass::Overflow.header()[5],
            2,
            "class byte 2 = overflow",
        );
    }

    #[test]
    fn every_stream_class_round_trips_through_the_header() {
        for class in [
            DownlinkStreamClass::Control,
            DownlinkStreamClass::Overflow,
            publisher_class(1, PublisherStreamKind::Video),
            publisher_class(u64::MAX, PublisherStreamKind::Screen),
        ] {
            assert_eq!(parse_downlink_header(&class.header()), Some(class));
        }
    }

    #[test]
    fn a_non_header_payload_is_rejected() {
        let good = DownlinkStreamClass::Control.header();
        assert!(parse_downlink_header(&good[..14]).is_none(), "short");
        let mut long = good.to_vec();
        long.push(0);
        assert!(parse_downlink_header(&long).is_none(), "long");
        let mut bad_magic = good;
        bad_magic[0] = b'X';
        assert!(parse_downlink_header(&bad_magic).is_none(), "magic");
        let mut bad_version = good;
        bad_version[4] = DOWNLINK_STREAM_PROTOCOL_VERSION + 1;
        assert!(parse_downlink_header(&bad_version).is_none(), "version");
        let mut bad_class = good;
        bad_class[5] = 9;
        assert!(parse_downlink_header(&bad_class).is_none(), "class");
        let mut bad_kind = publisher_class(1, PublisherStreamKind::Video).header();
        bad_kind[14] = 2; // AUDIO never keys a publisher stream
        assert!(parse_downlink_header(&bad_kind).is_none(), "media kind");
    }

    #[test]
    fn a_packet_wrapper_can_never_start_with_the_header_magic() {
        use protobuf::Enum as _;
        use protobuf::Message as _;
        for packet_type in PacketType::VALUES {
            let mut pw = PacketWrapper::new();
            pw.packet_type = (*packet_type).into();
            pw.user_id = b"someone@example.com".to_vec();
            pw.data = vec![0xFF; 64];
            pw.session_id = u64::MAX;
            let bytes = pw.write_to_bytes().expect("serialize PacketWrapper");
            assert_ne!(
                bytes[0], DOWNLINK_STREAM_HEADER_MAGIC[0],
                "{packet_type:?} serialized to a first byte that collides with the \
                 stream-header magic; the client could no longer tell a v1 header \
                 from a packet",
            );
        }
        // Structural: the magic's first byte decodes as a wire type protobuf does
        // not define.
        assert_eq!(
            DOWNLINK_STREAM_HEADER_MAGIC[0] & 0x07,
            6,
            "the magic's leading byte must decode as a reserved protobuf wire type",
        );
    }

    #[test]
    fn the_downlink_budget_matches_the_verified_browser_limit() {
        assert_eq!(
            crate::constants::WT_BROWSER_MAX_SERVER_UNI_STREAMS,
            100,
            "quiche kDefaultMaxStreamsPerConnection (100); Chrome advertises 103 \
             and reserves 3 for the HTTP/3 control and QPACK streams",
        );
        assert_eq!(
            MAX_PUBLISHER_DOWNLINK_STREAMS + 3,
            WT_MAX_DOWNLINK_STREAMS,
            "the publisher budget must reserve exactly the control, audio and \
             overflow lanes",
        );
        // videocall-client's MAX_CONCURRENT_INBOUND_READERS must stay at or above
        // this cap and the crates cannot share a constant.
        assert_eq!(
            (WT_MAX_DOWNLINK_STREAMS, MAX_PUBLISHER_DOWNLINK_STREAMS),
            (48, 45),
        );
        // A whole-map shed needs one fresh stream ID per lane while every reset
        // one still counts against the peer's limit; the binding form is the const
        // assertion in constants.rs, which fails the BUILD.
        assert_eq!(2 * WT_MAX_DOWNLINK_STREAMS, 96);
        // #2724 / contract C11: the `cause="cap"` canary needs this many
        // participants publishing BOTH camera and screen to one receiver.
        assert_eq!(MAX_PUBLISHER_DOWNLINK_STREAMS / 2 + 1, 23);
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_big_publisher_does_not_head_of_line_block_a_small_one() {
        const BIG_SENDER: u64 = 11;
        const SMALL_SENDER: u64 = 22;
        const BIG: usize = 512 * 1024;
        const SMALL: usize = 512;
        const BIG_FRAMES: usize = 8;
        const SMALL_FRAMES: usize = 3;

        let mut split = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        for _ in 0..BIG_FRAMES {
            split.offer(screen_key(BIG_SENDER), OutboundPriority::Screen, BIG);
        }
        for _ in 0..SMALL_FRAMES {
            split.offer(video_key(SMALL_SENDER), OutboundPriority::Video, SMALL);
        }

        let small_class = publisher_class(SMALL_SENDER, PublisherStreamKind::Video);
        let big_class = publisher_class(BIG_SENDER, PublisherStreamKind::Screen);
        let seen = split
            .collect_until(Duration::from_secs(20), |seen| {
                let big = seen
                    .iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(c, _) if *c == big_class))
                    .count();
                let small = seen
                    .iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(c, _) if *c == small_class))
                    .count();
                big >= BIG_FRAMES && small >= SMALL_FRAMES
            })
            .await;

        let first_small = seen
            .iter()
            .position(|e| matches!(e, StreamEvent::Frame(c, _) if *c == small_class))
            .unwrap_or_else(|| panic!("no small frame arrived at all: {seen:?}"));
        let last_big = seen
            .iter()
            .rposition(|e| matches!(e, StreamEvent::Frame(c, _) if *c == big_class))
            .unwrap_or_else(|| panic!("no big frame arrived at all: {seen:?}"));
        assert!(
            first_small < last_big,
            "with per-publisher streams the small publisher's frames must overtake \
             the big one's; they did not: {seen:?}",
        );

        // The pre-#2723 topology, same scenario: everything shares ONE ordered
        // stream, so the small frames cannot overtake anything.
        let mut shared = spin_up(DownlinkStreamMode::Single, None).await;
        for _ in 0..BIG_FRAMES {
            shared.offer(screen_key(BIG_SENDER), OutboundPriority::Screen, BIG);
        }
        for _ in 0..SMALL_FRAMES {
            shared.offer(video_key(SMALL_SENDER), OutboundPriority::Video, SMALL);
        }
        let legacy = shared
            .collect_until(Duration::from_secs(20), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::LegacyFrame(p) if p.len() == SMALL))
                    .count()
                    >= SMALL_FRAMES
            })
            .await;
        let legacy_first_small = legacy
            .iter()
            .position(|e| matches!(e, StreamEvent::LegacyFrame(p) if p.len() == SMALL))
            .unwrap_or_else(|| {
                panic!(
                    "no small frame on the legacy stream: {} events",
                    legacy.len()
                )
            });
        let legacy_big_before = legacy[..legacy_first_small]
            .iter()
            .filter(|e| matches!(e, StreamEvent::LegacyFrame(p) if p.len() == BIG))
            .count();
        assert_eq!(
            legacy_big_before, BIG_FRAMES,
            "on the pre-#2723 single stream every big frame is strictly in front \
             of the first small one — that is the head-of-line blocking this \
             issue removes",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_shed_resets_only_the_wedged_publishers_stream() {
        const WEDGED: u64 = 31;
        const HEALTHY: u64 = 32;

        let wedge = publisher_class(WEDGED, PublisherStreamKind::Video);
        let healthy = publisher_class(HEALTHY, PublisherStreamKind::Screen);
        let before = stream_reset_total("write_timeout");

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;

        // Wait for the wedged stream to exist so the flood really lands in ITS
        // hand-off queue rather than racing the lane's creation.
        let _ = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;

        // Fill the wedged key's queue past the shed ratio and keep it there, so
        // `channel_is_backed_up` holds for the whole #1638 grace.
        for _ in 0..(WT_DOWNLINK_KEY_CHANNEL_CAPACITY * 2) {
            h.try_offer(video_key(WEDGED), OutboundPriority::Video, 64 * 1024);
        }
        for _ in 0..3 {
            h.offer(screen_key(HEALTHY), OutboundPriority::Screen, 256);
        }

        let seen = h
            .collect_until(Duration::from_secs(15), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(c, _) if *c == healthy))
                    .count()
                    >= 3
                    && seen.contains(&StreamEvent::Reset(wedge))
                    && seen
                        .iter()
                        .filter(|e| **e == StreamEvent::Opened(wedge))
                        .count()
                        >= 2
            })
            .await;

        assert!(
            stream_reset_total("write_timeout") > before,
            "the wedged key must have been shed on the write_timeout path",
        );
        // Pins that the dispatcher wiring EXECUTES. BITES: drop `escalation`
        // from `LaneFactory::spawn` (#2726).
        assert_eq!(
            h.escalation.rounds_recorded(),
            1,
            "a real per-key shed must open exactly one escalation round (#2726)",
        );
        assert!(
            seen.contains(&StreamEvent::Reset(wedge)),
            "the wedged key's stream must be RESET: {seen:?}",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::Frame(c, _) if *c == healthy))
                .count(),
            3,
            "the healthy publisher must keep every frame across the other key's \
             shed: {seen:?}",
        );
        assert!(
            !seen.contains(&StreamEvent::Reset(healthy)),
            "the healthy publisher's stream must not be reset: {seen:?}",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| **e == StreamEvent::Opened(wedge))
                .count(),
            2,
            "the re-opened stream must carry a fresh v1 header for the SAME key — \
             a client keyed off the header would otherwise never bind it: {seen:?}",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| **e == StreamEvent::Opened(healthy))
                .count(),
            1,
            "and the healthy key must not be re-opened at all: {seen:?}",
        );
        assert_ne!(
            h.relief_epoch.load(Ordering::Relaxed),
            crate::actors::session_logic::DOWNLINK_EPOCH_NEVER,
            "any key's write_timeout shed stamps the RECEIVER's relief epoch (#2718)",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn receiver_scoped_frames_ride_the_control_stream() {
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 16);
        h.offer(DownlinkStreamKey::Control, OutboundPriority::ProbeEcho, 8);
        h.offer(video_key(7), OutboundPriority::Video, 6);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(..)))
                    .count()
                    >= 3
            })
            .await;

        let control_frames: Vec<usize> = seen
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Frame(DownlinkStreamClass::Control, n) => Some(*n),
                _ => None,
            })
            .collect();
        assert_eq!(
            control_frames,
            vec![16, 8],
            "Critical control and the RTT echo share the receiver control stream, \
             in order: {seen:?}",
        );
        assert!(
            seen.contains(&StreamEvent::Frame(
                publisher_class(7, PublisherStreamKind::Video),
                6
            )),
            "camera media must ride its publisher's own stream: {seen:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn an_old_client_gets_one_headerless_stream() {
        let mut h = spin_up(DownlinkStreamMode::Single, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 9);
        h.offer(video_key(1), OutboundPriority::Video, 5);
        h.offer(video_key(2), OutboundPriority::Video, 7);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::LegacyFrame(_)))
                    .count()
                    >= 3
            })
            .await;

        assert!(
            !seen.iter().any(|e| matches!(e, StreamEvent::Opened(_))),
            "a pre-#2723 client must never see a stream header: {seen:?}",
        );
        let lengths: Vec<usize> = seen
            .iter()
            .filter_map(|e| match e {
                StreamEvent::LegacyFrame(p) => Some(p.len()),
                _ => None,
            })
            .collect();
        assert_eq!(
            lengths,
            vec![9, 5, 7],
            "every frame rides ONE ordered stream, exactly as before #2723",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_quiet_publisher_stream_is_finished_and_its_slot_returned() {
        let _idle = IdleOverride::set(150, 50);
        let before_reaps = idle_reaps_total();
        let before_finishes = finishes_total();

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let quiet = publisher_class(55, PublisherStreamKind::Video);
        h.offer(video_key(55), OutboundPriority::Video, 10);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Finished(quiet))
            })
            .await;

        assert!(
            seen.contains(&StreamEvent::Finished(quiet)),
            "a publisher gone quiet past the idle grace must see a clean FIN: {seen:?}",
        );
        assert!(
            !seen.contains(&StreamEvent::Reset(quiet)),
            "the idle close must be a finish, never a reset: {seen:?}",
        );
        assert!(
            idle_reaps_total() > before_reaps,
            "the idle sweep must book the eviction",
        );
        assert!(
            finishes_total() > before_finishes,
            "the clean finish must be booked separately from the shed resets",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn receiver_teardown_finishes_every_open_stream() {
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 3);
        h.offer(video_key(71), OutboundPriority::Video, 4);
        h.offer(screen_key(71), OutboundPriority::Screen, 5);

        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(..)))
                    .count()
                    >= 3
            })
            .await;
        assert_eq!(
            opened
                .iter()
                .filter(|e| matches!(e, StreamEvent::Opened(_)))
                .count(),
            3,
            "three distinct keys must have opened three streams: {opened:?}",
        );

        // The actor dropping BOTH outbound senders is what teardown looks like to
        // the bridge: the datagram writer holds a clone of the unistream sender
        // (#2716), so the unistream channel only closes once it too is gone.
        let Harness {
            uni_tx, dgram_tx, ..
        } = &mut h;
        let (uni_tx, dgram_tx) = (uni_tx.clone(), dgram_tx.clone());
        drop(uni_tx);
        drop(dgram_tx);
        h.uni_tx = mpsc::channel(1).0;
        h.dgram_tx = mpsc::channel(1).0;

        let closed = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Finished(_)))
                    .count()
                    >= 3
            })
            .await;
        assert_eq!(
            closed
                .iter()
                .filter(|e| matches!(e, StreamEvent::Finished(_)))
                .count(),
            3,
            "teardown must cleanly finish every open stream: {closed:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn past_the_cap_new_keys_share_the_counted_overflow_stream() {
        const EXTRA: usize = 3;
        let before = overflow_frames_total("cap");
        let unattributed_before = overflow_frames_total("unattributed");
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 3);
        h.offer(DownlinkStreamKey::Audio, OutboundPriority::Audio, 7);
        for i in 0..(MAX_PUBLISHER_DOWNLINK_STREAMS + EXTRA) as u64 {
            h.offer(video_key(1000 + i), OutboundPriority::Video, 32);
        }

        let want = MAX_PUBLISHER_DOWNLINK_STREAMS + EXTRA + 2;
        let seen = h
            .collect_until(Duration::from_secs(30), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(..)))
                    .count()
                    >= want
            })
            .await;

        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::Opened(_)))
                .count(),
            WT_MAX_DOWNLINK_STREAMS,
            "the relay must open its cap and not one stream more: {} opened",
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::Opened(_)))
                .count(),
        );
        let overflow_frames = seen
            .iter()
            .filter(|e| matches!(e, StreamEvent::Frame(DownlinkStreamClass::Overflow, _)))
            .count();
        assert_eq!(
            overflow_frames, EXTRA,
            "every key past the cap must ride the ONE overflow stream",
        );
        assert_eq!(
            overflow_frames_total("cap") - before,
            EXTRA as f64,
            "each of those frames must be counted as a CAP event",
        );
        assert_eq!(
            overflow_frames_total("unattributed"),
            unattributed_before,
            "a cap event must not be booked as the routine media_kind-rollout \
             cause: only the cap series is a canary",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn an_overflowed_key_stays_on_the_overflow_stream_after_slots_free() {
        const PINNED: u64 = 7777;
        // Long enough that the pinned key, kept busy below, never goes idle;
        // short enough that the one-shot filler keys do.
        let _idle = IdleOverride::set(400, 50);
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        for i in 0..MAX_PUBLISHER_DOWNLINK_STREAMS as u64 {
            h.offer(video_key(2000 + i), OutboundPriority::Video, 16);
        }
        h.offer(video_key(PINNED), OutboundPriority::Video, 16);

        let seen = h
            .collect_until(Duration::from_secs(30), |seen| {
                seen.iter()
                    .any(|e| matches!(e, StreamEvent::Frame(DownlinkStreamClass::Overflow, _)))
            })
            .await;
        assert!(
            seen.iter()
                .any(|e| matches!(e, StreamEvent::Frame(DownlinkStreamClass::Overflow, _))),
            "setup failed: the pinned key never reached the overflow stream: {seen:?}",
        );
        let _ = overflow_frames_total("cap");

        // Keep the pinned key hot while the one-shot filler keys go idle and
        // their slots are reaped.
        let reaps_before = idle_reaps_total();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline
            && idle_reaps_total() - reaps_before < MAX_PUBLISHER_DOWNLINK_STREAMS as f64 / 2.0
        {
            h.offer(video_key(PINNED), OutboundPriority::Video, 16);
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        assert!(
            idle_reaps_total() > reaps_before,
            "setup failed: no slot was freed, so stickiness was never exercised",
        );

        h.offer(video_key(PINNED), OutboundPriority::Video, 99);
        let after = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Frame(DownlinkStreamClass::Overflow, 99))
            })
            .await;
        assert!(
            after.contains(&StreamEvent::Frame(DownlinkStreamClass::Overflow, 99)),
            "the pinned key must still ride the overflow stream: {after:?}",
        );
        assert!(
            !after.iter().any(|e| matches!(
                e,
                StreamEvent::Opened(DownlinkStreamClass::Publisher {
                    session_id: PINNED,
                    ..
                })
            )),
            "the pinned key must never be migrated onto a stream of its own: {after:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn aborting_the_bridge_releases_every_stream_slot() {
        let before = stream_slots();
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 3);
        h.offer(video_key(81), OutboundPriority::Video, 4);
        h.offer(screen_key(81), OutboundPriority::Screen, 5);
        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Opened(_)))
                    .count()
                    >= 3
            })
            .await;
        assert_eq!(
            opened
                .iter()
                .filter(|e| matches!(e, StreamEvent::Opened(_)))
                .count(),
            3,
            "setup failed: three lanes must have opened a stream: {opened:?}",
        );
        assert_eq!(
            stream_slots() - before,
            4.0,
            "setup failed: the gauge counts LANES, and the eager audio lane holds \
             a slot without opening a stream (#2724)",
        );

        h.bridge
            .take()
            .expect("the harness owns the bridge")
            .shutdown()
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && stream_slots() > before {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            stream_slots(),
            before,
            "every slot must be released when the lane tasks are cancelled",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_dispatcher_tail_drop_is_counted_and_uncharged() {
        const WEDGED: u64 = 91;
        let wedge = publisher_class(WEDGED, PublisherStreamKind::Video);
        let before = queue_drops_total("video");
        let alerted_before = outbound_drops("video");

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;
        let _ = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;

        for _ in 0..(WT_DOWNLINK_KEY_CHANNEL_CAPACITY * 4) {
            h.try_offer(video_key(WEDGED), OutboundPriority::Video, 64 * 1024);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        assert!(
            queue_drops_total("video") > before,
            "a wedged key's full hand-off queue must book tail drops",
        );
        assert!(
            outbound_drops("video") > alerted_before,
            "the dispatcher drop must ALSO book the series the three prometheus \
             alert rules watch, or this new drop surface silently replaces it",
        );

        let (uni_tx, dgram_tx) = (h.uni_tx.clone(), h.dgram_tx.clone());
        drop(uni_tx);
        drop(dgram_tx);
        h.uni_tx = mpsc::channel(1).0;
        h.dgram_tx = mpsc::channel(1).0;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline && h.meter.snapshot().queued_total() != 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            h.meter.snapshot().queued_total(),
            0,
            "the #2717 byte meter must return to zero — a leak here shrinks the \
             receiver's admission budget for the rest of the session",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn media_cannot_tail_drop_critical_control_off_a_wedged_lane() {
        let mut h = spin_up(
            DownlinkStreamMode::PerPublisherV1,
            Some(DownlinkStreamClass::Control),
        )
        .await;

        h.offer(
            DownlinkStreamKey::Control,
            OutboundPriority::Video,
            64 * 1024,
        );
        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(DownlinkStreamClass::Control))
            })
            .await;
        assert!(
            opened.contains(&StreamEvent::Opened(DownlinkStreamClass::Control)),
            "setup failed: the control stream was never opened or wedged",
        );

        let media_drops_before = queue_drops_total("video");
        // Large frames, so the QUEUE itself fills inside the shed grace and the
        // reserve, not the shed, is what is under test.
        for _ in 0..(WT_DOWNLINK_CONTROL_CHANNEL_CAPACITY + 96) {
            h.try_offer(
                DownlinkStreamKey::Control,
                OutboundPriority::Video,
                64 * 1024,
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            queue_drops_total("video") > media_drops_before,
            "setup failed: the control lane never filled, so the reserve was \
             never exercised",
        );

        let critical_before = overflow_critical_drops();
        // Control, not just Critical: `evaluate_dual` promises BOTH are never
        // preempted, and KEYFRAME_REQUEST and HEARTBEAT are Control.
        let control_before = outbound_drops("control");
        assert!(
            h.try_offer(DownlinkStreamKey::Control, OutboundPriority::Control, 777),
            "the admission gate must still accept a Control frame",
        );
        assert!(
            h.try_offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 4242),
            "the admission gate must still accept a Critical frame",
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            overflow_critical_drops(),
            critical_before,
            "media saturation dropped a Critical control frame at the dispatcher",
        );
        assert_eq!(
            outbound_drops("control"),
            control_before,
            "media saturation dropped a Control frame — a relayed KEYFRAME_REQUEST \
             or HEARTBEAT — at the dispatcher; the reserve covers exactly \
             `evaluate_dual`'s never-preempt set, not Critical alone",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn the_idle_sweep_drains_finished_lane_tasks() {
        let _idle = IdleOverride::set(150, 50);
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        const KEYS: u64 = 12;
        for i in 0..KEYS {
            h.offer(video_key(3000 + i), OutboundPriority::Video, 16);
        }
        let seen = h
            .collect_until(Duration::from_secs(15), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Finished(_)))
                    .count()
                    >= KEYS as usize
            })
            .await;
        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::Finished(_)))
                .count(),
            KEYS as usize,
            "setup failed: the keys were never reaped: {seen:?}",
        );

        // One more sweep after the last lane exited, so the drain runs with
        // every finished entry already notified.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            lane_task_entries() <= 2.0,
            "the dispatcher must not keep a task entry per reaped lane; \
             {KEYS} keys were reaped and {} entries remain",
            lane_task_entries(),
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_parked_lane_is_aborted_when_its_key_is_reaped() {
        const PARKED: u64 = 4242;
        let wedge = publisher_class(PARKED, PublisherStreamKind::Video);
        let _idle = IdleOverride::set(150, 50);

        let before = stream_slots();
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;

        // One big frame parks the lane with an EMPTY queue, the state the #1638
        // shed by design never rescues.
        h.offer(video_key(PARKED), OutboundPriority::Video, 2 * 1024 * 1024);
        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;
        assert!(
            opened.contains(&StreamEvent::Opened(wedge)),
            "setup failed: the lane never opened its stream: {opened:?}",
        );
        // The eager control and audio lanes are never reaped, so the parked
        // publisher lane is the third slot.
        let with_control = before + 2.0;
        assert_eq!(
            stream_slots(),
            with_control + 1.0,
            "setup failed: the parked lane is not holding a slot",
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline && stream_slots() > with_control {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            stream_slots(),
            with_control,
            "a parked lane must not survive its own reap: the map freed its slot \
             to a new key while the task still held the stream",
        );
    }

    #[test]
    fn control_outranks_media_and_media_is_one_tier() {
        let control = DownlinkStreamClass::Control.send_priority();
        let video = publisher_class(1, PublisherStreamKind::Video).send_priority();
        let screen = publisher_class(1, PublisherStreamKind::Screen).send_priority();
        let overflow = DownlinkStreamClass::Overflow.send_priority();
        assert!(
            control > video,
            "control {control} must outrank media {video}"
        );
        assert_eq!(
            (video, screen, overflow),
            (video, video, video),
            "quinn's priority is STRICT, so any split here would re-order media \
             against #1977's admission budgets, which already prefer screen over \
             camera by shedding cameras first",
        );
    }

    /// D1: the constants alone prove nothing — pin that the value reaches the
    /// stream, over the real loopback session.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn an_opened_stream_carries_the_priority_it_was_given() {
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let session = h.server_session.clone();

        for class in [
            DownlinkStreamClass::Control,
            DownlinkStreamClass::Audio,
            publisher_class(1, PublisherStreamKind::Video),
            publisher_class(1, PublisherStreamKind::Screen),
            DownlinkStreamClass::Overflow,
        ] {
            let s = open_downlink_stream(&session, Some(class), "test")
                .await
                .expect("the loopback session must open a stream");
            assert_eq!(
                s.priority().expect("the stream is open"),
                class.send_priority(),
                "{class:?}: the class's send priority must reach the QUIC stream",
            );
        }

        // Single-stream mode must leave quinn's default alone.
        let legacy = open_downlink_stream(&session, None, "test")
            .await
            .expect("open");
        assert_eq!(
            legacy.priority().expect("the stream is open"),
            0,
            "a pre-#2723 client's stream must carry no priority hint",
        );
    }

    #[test]
    fn only_the_control_lane_is_room_sized_and_reserved() {
        assert_eq!(
            DownlinkStreamClass::Audio.hand_off_capacity(),
            WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY,
        );
        assert_eq!(
            DownlinkStreamClass::Audio.reserved_slots(),
            0,
            "only audio keys the audio lane, so there is nothing to protect it from",
        );
        assert_eq!(
            DownlinkStreamClass::Control.hand_off_capacity(),
            WT_DOWNLINK_CONTROL_CHANNEL_CAPACITY,
        );
        assert_eq!(
            DownlinkStreamClass::Control.reserved_slots(),
            WT_DOWNLINK_CONTROL_RESERVE,
        );
        assert_eq!(
            DownlinkStreamClass::Overflow.hand_off_capacity(),
            WT_DOWNLINK_OVERFLOW_CHANNEL_CAPACITY,
            "the overflow lane aggregates every pre-media_kind publisher, so the \
             per-publisher figure does not size it either",
        );
        for class in [
            publisher_class(1, PublisherStreamKind::Video),
            publisher_class(1, PublisherStreamKind::Screen),
        ] {
            assert_eq!(class.hand_off_capacity(), WT_DOWNLINK_KEY_CHANNEL_CAPACITY);
        }
        for class in [
            publisher_class(1, PublisherStreamKind::Video),
            publisher_class(1, PublisherStreamKind::Screen),
            DownlinkStreamClass::Overflow,
        ] {
            assert_eq!(
                class.reserved_slots(),
                0,
                "{class:?} never carries a Critical or non-media Control frame, \
                 so a reserve there would cost media slots for nothing",
            );
        }
    }

    // -----------------------------------------------------------------------
    // #2724: the dedicated reliable audio stream
    // -----------------------------------------------------------------------

    #[test]
    fn the_audio_stream_header_is_class_three_and_names_audio() {
        let header = DownlinkStreamClass::Audio.header();
        assert_eq!(&header[0..4], b"VCDS");
        assert_eq!(
            header[4], 1,
            "class 3 rides protocol v1: a version bump would drop every shipped \
             #2723 client back to the legacy single stream",
        );
        assert_eq!(header[5], 3, "the audio class byte");
        assert_eq!(
            &header[6..14],
            &[0u8; 8],
            "receiver-scoped, so there is no publisher session id",
        );
        assert_eq!(header[14], 2, "MediaKind::AUDIO");

        let mut classes = vec![
            DownlinkStreamClass::Control.header()[5],
            DownlinkStreamClass::Audio.header()[5],
            DownlinkStreamClass::Overflow.header()[5],
            publisher_class(1, PublisherStreamKind::Video).header()[5],
        ];
        classes.sort_unstable();
        classes.dedup();
        assert_eq!(classes.len(), 4, "every class byte must be distinct");
    }

    #[test]
    fn audio_outranks_media_and_control_outranks_audio() {
        let control = DownlinkStreamClass::Control.send_priority();
        let audio = DownlinkStreamClass::Audio.send_priority();
        let camera = publisher_class(1, PublisherStreamKind::Video).send_priority();
        let screen = publisher_class(1, PublisherStreamKind::Screen).send_priority();
        let overflow = DownlinkStreamClass::Overflow.send_priority();

        assert!(control > audio, "control must still be served first");
        assert!(
            audio > camera,
            "audio must outrank video: the admission policy already sheds camera \
             first, so a transmit priority that served video first would pull the \
             other way on the same packet",
        );
        assert_eq!(
            (camera, screen, overflow),
            (camera, camera, camera),
            "C9 stands: every media class shares ONE tier, round-robin",
        );
    }

    /// BITES `sheds_on_backpressure` returning true for audio.
    #[test]
    fn audio_is_the_only_class_exempt_from_the_backpressure_shed() {
        assert!(
            !DownlinkStreamClass::Audio.sheds_on_backpressure(),
            "resetting the audio stream discards the buffered undelivered \
             audio this issue exists to deliver",
        );
        for class in [
            DownlinkStreamClass::Control,
            DownlinkStreamClass::Overflow,
            publisher_class(1, PublisherStreamKind::Video),
            publisher_class(1, PublisherStreamKind::Screen),
        ] {
            assert!(
                class.sheds_on_backpressure(),
                "{class:?} must keep the #1638 shed: stale frames there are worthless",
            );
        }
    }

    /// FAILS on the un-fixed code, where `for_media` routed every audio frame to
    /// the CONTROL lane and no class-3 stream existed.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn audio_rides_its_own_stream_not_control_and_not_the_video_stream() {
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 11);
        h.offer(DownlinkStreamKey::Audio, OutboundPriority::Audio, 97);
        h.offer(video_key(5), OutboundPriority::Video, 4_096);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(..)))
                    .count()
                    >= 3
            })
            .await;

        assert!(
            seen.contains(&StreamEvent::Frame(DownlinkStreamClass::Audio, 97)),
            "the audio frame must arrive on the class-3 stream: {seen:?}",
        );
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, StreamEvent::Frame(DownlinkStreamClass::Control, 97))),
            "and must NOT arrive on the control stream: {seen:?}",
        );
        assert!(
            !seen.iter().any(|e| matches!(
                e,
                StreamEvent::Frame(DownlinkStreamClass::Publisher { .. }, 97)
            )),
            "nor on any publisher's video stream, which is the cross-media \
             head-of-line blocking #622 decided against: {seen:?}",
        );
        assert!(
            seen.contains(&StreamEvent::Frame(DownlinkStreamClass::Control, 11))
                && seen.contains(&StreamEvent::Frame(
                    publisher_class(5, PublisherStreamKind::Video),
                    4_096
                )),
            "control and video must be unaffected: {seen:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_wedged_audio_stream_is_never_reset_and_never_blocks_another_lane() {
        const HEALTHY: u64 = 77;

        let wedge = DownlinkStreamClass::Audio;
        let healthy = publisher_class(HEALTHY, PublisherStreamKind::Video);
        let before = stream_reset_total("write_timeout");

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;

        h.offer(DownlinkStreamKey::Audio, OutboundPriority::Audio, 97);
        let setup = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;
        assert!(
            setup.contains(&StreamEvent::Opened(wedge)),
            "setup failed: the audio stream never opened: {setup:?}",
        );

        // Past the stream window, so the write genuinely parks and the queue sits
        // well above the shed ratio: on any other class this would arm
        // `write_timeout`. Kept under `wt_outbound_channel_capacity` so the flood
        // cannot fill the shared channel out from under the healthy publisher.
        const FLOOD: usize = WT_DOWNLINK_AUDIO_CHANNEL_CAPACITY + 128;
        for _ in 0..FLOOD {
            h.try_offer(DownlinkStreamKey::Audio, OutboundPriority::Audio, 24 * 1024);
        }
        for _ in 0..3 {
            h.offer(video_key(HEALTHY), OutboundPriority::Video, 256);
        }

        // Well past the shed deadline: a sheddable lane would have reset by now.
        let seen = h
            .collect_until(
                WT_UNISTREAM_WRITE_DEADLINE * 4 + Duration::from_secs(2),
                |seen| seen.contains(&StreamEvent::Reset(wedge)),
            )
            .await;

        // Without this the no-reset assertions below are vacuous: a lane that
        // drained was never wedged.
        assert!(
            h.meter.snapshot().queued_total() > 24 * 1024,
            "setup failed: the audio lane drained instead of wedging",
        );
        assert!(
            !seen.contains(&StreamEvent::Reset(wedge)),
            "the audio stream was RESET; its buffered audio is now discarded at \
             the receiver, which is #1878's burst gap on the reliable path: {seen:?}",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| **e == StreamEvent::Opened(wedge))
                .count(),
            0,
            "and it must never be re-opened, because it was never torn down — the \
             one open is in `setup`: {seen:?}",
        );
        assert_eq!(
            stream_reset_total("write_timeout"),
            before,
            "no write_timeout shed may be booked while only the audio lane is wedged",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::Frame(c, _) if *c == healthy))
                .count(),
            3,
            "and the wedged audio lane must not hold up another publisher's \
             frames — the per-lane task split is what guarantees that: {seen:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_legacy_receiver_gets_no_separate_audio_stream() {
        let mut h = spin_up(DownlinkStreamMode::Single, None).await;

        h.offer(DownlinkStreamKey::Audio, OutboundPriority::Audio, 97);
        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 11);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::LegacyFrame(_)))
                    .count()
                    >= 2
            })
            .await;

        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, StreamEvent::Opened(_) | StreamEvent::Frame(..))),
            "a pre-#2723 client must see no stream header at all: {seen:?}",
        );
        assert_eq!(
            seen.iter()
                .filter(|e| matches!(e, StreamEvent::LegacyFrame(p) if p.len() == 97))
                .count(),
            1,
            "its audio arrives on the one headerless stream, as today: {seen:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn unattributable_bulk_media_rides_the_overflow_stream() {
        let unattributed_before = overflow_frames_total("unattributed");
        let cap_before = overflow_frames_total("cap");
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;

        h.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 11);
        h.offer(DownlinkStreamKey::Shared, OutboundPriority::Control, 22);

        let seen = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(..)))
                    .count()
                    >= 2
            })
            .await;
        assert!(
            seen.contains(&StreamEvent::Frame(DownlinkStreamClass::Control, 11)),
            "Critical control keeps the lifecycle lane: {seen:?}",
        );
        assert!(
            seen.contains(&StreamEvent::Frame(DownlinkStreamClass::Overflow, 22)),
            "bulk unattributable media must ride the overflow stream: {seen:?}",
        );
        assert!(
            overflow_frames_total("unattributed") > unattributed_before,
            "and must book the ROLLOUT cause, never the cap canary",
        );
        assert_eq!(
            overflow_frames_total("cap"),
            cap_before,
            "no publisher-key cap was reached, so the canary must stay flat",
        );
    }

    /// #2745. BITES: delete the `drops.record(...)` call in `dispatch`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_lane_tail_drop_books_every_drop_but_arms_relief_only_when_sustained() {
        const WEDGED: u64 = 7745;
        let wedge = publisher_class(WEDGED, PublisherStreamKind::Video);
        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;

        let room_before = packet_drops_total("channel_full");
        let session_before = session_drops_total("video");
        let relief_before = relief_stamp_total(RELIEF_SOURCE_OUTBOUND_DROP);

        h.offer(video_key(WEDGED), OutboundPriority::Video, 2 * 1024 * 1024);
        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;
        assert!(
            opened.contains(&StreamEvent::Opened(wedge)),
            "setup failed: the lane never opened its stream: {opened:?}",
        );

        // ONE burst, inside the sustain window.
        for _ in 0..(WT_DOWNLINK_KEY_CHANNEL_CAPACITY * 3) {
            h.offer(video_key(WEDGED), OutboundPriority::Video, 4 * 1024);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while packet_drops_total("channel_full") <= room_before
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(
            packet_drops_total("channel_full") > room_before,
            "every lane tail drop must book relay_packet_drops_total for the \
             ROOM immediately; without it a v1 client's video loss is \
             unattributable",
        );
        assert!(
            session_drops_total("video") > session_before,
            "every lane tail drop must name the slow receiver on \
             relay_session_drops_total immediately",
        );
        assert!(
            crate::actors::session_logic::lock_congestion(&h.congestion).is_actively_congested(),
            "every lane tail drop must feed the sender-keyed congestion tracker \
             immediately, which is what relaxes the #979 keyframe limiter",
        );
        assert_eq!(
            relief_stamp_total(RELIEF_SOURCE_OUTBOUND_DROP),
            relief_before,
            "a burst that fills one 32-slot lane must NOT arm relief: that is \
             0.62 s of an unfiltered three-layer publisher, and arming costs \
             this receiver 8 s of shed layers plus a 6-18 s client dwell",
        );

        let sustained = std::time::Instant::now()
            + WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN
            + Duration::from_secs(3);
        while std::time::Instant::now() < sustained
            && relief_stamp_total(RELIEF_SOURCE_OUTBOUND_DROP) <= relief_before
        {
            for _ in 0..WT_DOWNLINK_KEY_CHANNEL_CAPACITY {
                h.offer(video_key(WEDGED), OutboundPriority::Video, 4 * 1024);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            relief_stamp_total(RELIEF_SOURCE_OUTBOUND_DROP) > relief_before,
            "a run of tail drops lasting a whole \
             WT_DOWNLINK_LANE_DROP_RELIEF_SUSTAIN is a wedged lane, not a \
             burst, and must arm the #1219 relief epoch",
        );
    }

    /// The gate itself, on injected instants. BITES: drop the run-break check
    /// (the second case then reads as sustained) or the elapsed check (the
    /// first case does).
    #[test]
    fn a_relief_arming_run_needs_drops_across_the_whole_window() {
        const SUSTAIN: Duration = Duration::from_secs(1);
        let t0 = tokio::time::Instant::now();
        let mut run = LaneDropRun::default();

        assert!(!run.note_drop(t0, SUSTAIN), "one drop is never a run");
        assert!(
            !run.note_drop(t0 + Duration::from_millis(500), SUSTAIN),
            "half a window in, the run has not reached the bar",
        );
        assert!(
            run.note_drop(t0 + Duration::from_millis(1_000), SUSTAIN),
            "drops spanning the whole window arm relief",
        );
        assert!(
            run.note_drop(t0 + Duration::from_millis(1_500), SUSTAIN),
            "a continuing run keeps refreshing the epoch, which decays on its \
             own after RECEIVER_DOWNLINK_RELIEF_WINDOW",
        );

        let after_gap = t0 + Duration::from_millis(3_000);
        assert!(
            !run.note_drop(after_gap, SUSTAIN),
            "drops that stopped for longer than the window start a fresh run",
        );
        assert!(
            !run.note_drop(after_gap + Duration::from_millis(999), SUSTAIN),
            "a gap of exactly the window does NOT break the run, so this is \
             still the run that started at the gap",
        );
        assert!(run.note_drop(after_gap + Duration::from_millis(1_000), SUSTAIN));
    }

    /// #2757. BITES: remove `impl Drop for LaneStream` — the client then sees a
    /// clean FIN, no `Reset` event is published and this times out.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_reaped_lane_resets_its_stream_instead_of_finishing_it() {
        const PARKED: u64 = 7757;
        let wedge = publisher_class(PARKED, PublisherStreamKind::Video);
        let _idle = IdleOverride::set(150, 50);
        let resets_before = stream_reset_total(LANE_ABORT_RESET_REASON);

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;
        h.offer(video_key(PARKED), OutboundPriority::Video, 2 * 1024 * 1024);

        let seen = h
            .collect_until(Duration::from_secs(20), |seen| {
                seen.contains(&StreamEvent::Reset(wedge))
            })
            .await;
        assert!(
            seen.contains(&StreamEvent::Reset(wedge)),
            "the reaped lane's stream must be RESET, not dropped: a dropped \
             quinn SendStream is finished, which leaves it open and undrained \
             while its slot has already been handed to another key: {seen:?}",
        );
        assert!(
            stream_reset_total(LANE_ABORT_RESET_REASON) > resets_before,
            "the abort reset must be visible to operators",
        );
    }

    /// #2745 / #2746. BITES: remove `impl Drop for LaneQueue`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_reaped_lane_leaves_no_bytes_charged_to_the_receiver() {
        const PARKED: u64 = 7746;
        const QUEUED: usize = 8;
        const FRAME: usize = 4 * 1024;
        let wedge = publisher_class(PARKED, PublisherStreamKind::Video);
        let _idle = IdleOverride::set(150, 50);

        let mut h = spin_up(DownlinkStreamMode::PerPublisherV1, Some(wedge)).await;
        h.offer(video_key(PARKED), OutboundPriority::Video, 2 * 1024 * 1024);
        let opened = h
            .collect_until(Duration::from_secs(10), |seen| {
                seen.contains(&StreamEvent::Opened(wedge))
            })
            .await;
        assert!(
            opened.contains(&StreamEvent::Opened(wedge)),
            "setup failed: the lane never opened its stream: {opened:?}",
        );

        for _ in 0..QUEUED {
            h.offer(video_key(PARKED), OutboundPriority::Video, FRAME);
        }
        let charged = std::time::Instant::now() + Duration::from_secs(5);
        while h.meter.queued_for(OutboundPriority::Video) < QUEUED * FRAME
            && std::time::Instant::now() < charged
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            h.meter.queued_for(OutboundPriority::Video) >= QUEUED * FRAME,
            "setup failed: the queued frames were never charged (got {})",
            h.meter.queued_for(OutboundPriority::Video),
        );

        let drained = std::time::Instant::now() + Duration::from_secs(20);
        while h.meter.queued_for(OutboundPriority::Video) > 0 && std::time::Instant::now() < drained
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            h.meter.queued_for(OutboundPriority::Video),
            0,
            "an aborted lane must release its queued frames' byte charge; a \
             permanently inflated byte_fill reads as congestion that is not \
             there and sheds a healthy receiver's video",
        );
    }

    /// Leave a lane in the state a `WriterDead` exit leaves it: the task is gone
    /// and its receiver with it, so every further `try_send` returns `Closed`.
    fn kill_lane(lane: &mut DownlinkLane) {
        let (tx, rx) = mpsc::channel::<WtOutboundFrame>(WT_DOWNLINK_KEY_CHANNEL_CAPACITY);
        drop(rx);
        lane.tx = tx;
    }

    /// A map over a live loopback session, with its drop sink on `room` so the
    /// counters this test reads cannot be moved by another test.
    fn lane_map(
        harness: &Harness,
        room: &str,
        congestion: Arc<std::sync::Mutex<crate::actors::session_logic::CongestionTracker>>,
    ) -> DownlinkStreamMap {
        let (relief, _epoch) = test_relief_signal();
        DownlinkStreamMap::new(LaneFactory {
            session: harness.server_session.clone(),
            unistream_bytes: Arc::new(SharedQueueByteMeter::default()),
            on_packet_sent: None,
            drops: crate::actors::session_logic::DownlinkDropSink::new(
                room,
                DEAD_LANE_SESSION,
                "webtransport",
                relief,
                congestion,
                crate::actors::session_logic::SessionDropBooking::open(),
            ),
            escalation: DownlinkShedEscalation::new(),
        })
    }

    const DEAD_LANE_SESSION: u64 = 91_001;

    fn keyed_frame(priority: OutboundPriority, key: DownlinkStreamKey) -> WtOutboundFrame {
        WtOutboundFrame::keyed(priority, Bytes::from_static(b"payload"), key)
    }

    fn room_drops(room: &str, reason: &str) -> f64 {
        crate::metrics::RELAY_PACKET_DROPS_TOTAL
            .with_label_values(&[room, "webtransport", reason])
            .get()
    }

    fn room_session_drops(room: &str, kind: &str) -> f64 {
        crate::metrics::RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&[room, "webtransport", &DEAD_LANE_SESSION.to_string(), kind])
            .get()
    }

    /// A lane whose task exited on `WriterDead` returns `Closed` for every
    /// later frame. Booking those as `channel_full` tells the #979 tracker and
    /// the #1219 relief run that this RECEIVER is slow, which sheds its layers
    /// for a fault that is one broken stream. BITES: fold the `Closed` arm back
    /// into `Full`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_dead_lane_drop_is_not_booked_as_receiver_congestion() {
        const ROOM: &str = "dead-lane-congestion-room";
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let congestion: Arc<std::sync::Mutex<crate::actors::session_logic::CongestionTracker>> =
            Arc::default();
        let mut map = lane_map(&h, ROOM, Arc::clone(&congestion));

        let key = video_key(7_001);
        map.publisher_lane(key);
        let dead = map.publishers.get_mut(&key).expect("just spawned");
        kill_lane(dead);
        // Hold THIS key's replacement off so every frame below meets the dead
        // lane.
        dead.respawn_after = tokio::time::Instant::now() + Duration::from_secs(600);

        let drops = u64::from(crate::constants::CONGESTION_DROP_THRESHOLD) + 1;
        for _ in 0..drops {
            map.dispatch(keyed_frame(OutboundPriority::Video, key));
        }

        assert_eq!(
            room_drops(ROOM, LANE_DEAD_DROP_REASON),
            drops as f64,
            "every frame rejected by a closed lane must book its own drop reason",
        );
        assert_eq!(
            room_drops(ROOM, "channel_full"),
            0.0,
            "a closed lane is not a full one; booking it as channel_full is what \
             makes a broken stream read as a slow receiver",
        );
        assert_eq!(
            room_session_drops(ROOM, "video"),
            drops as f64,
            "the receiver is still losing these frames, so its per-session drop \
             series must still name them",
        );
        assert!(
            map.drop_run.last.is_none(),
            "a dead lane must not extend the #2745 relief run",
        );
        assert!(
            !crate::actors::session_logic::lock_congestion(&congestion).is_actively_congested(),
            "more than CONGESTION_DROP_THRESHOLD dead-lane rejects must still \
             leave the #979 tracker clear",
        );
        map.close_and_join().await;
    }

    /// BITES: delete the owning-publisher arm of `replace_dead_lane`. Without
    /// it the entry keeps its closed channel and its slot for the session's
    /// life, because `dispatch` refreshes `last_dispatch` before the send so
    /// `reap_idle` never sees the key go idle.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_dead_publisher_lane_is_replaced_in_place_keeping_its_slot() {
        const ROOM: &str = "dead-lane-publisher-room";
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let mut map = lane_map(&h, ROOM, Arc::default());

        let key = video_key(7_002);
        map.publisher_lane(key);
        assert_eq!(
            map.owned_publisher_streams, 1,
            "setup failed: no owned lane"
        );
        kill_lane(map.publishers.get_mut(&key).expect("just spawned"));

        map.dispatch(keyed_frame(OutboundPriority::Video, key));

        assert!(
            !map.publishers
                .get(&key)
                .expect("the key must keep its entry")
                .tx
                .is_closed(),
            "a dead publisher lane must be replaced with a live one",
        );
        assert_eq!(
            map.owned_publisher_streams, 1,
            "the key still owns one stream, so the slot count must not move: \
             decrementing here and re-incrementing on the next frame would let \
             the cap drift under churn",
        );
        map.close_and_join().await;
    }

    /// The cooldown is PER KEY. BITES: put it back on the map — the second key
    /// below is then refused because the first consumed the only token, which is
    /// how a video lane rejecting at frame rate starves a dead control lane.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn two_dead_lanes_are_both_replaced_inside_one_cooldown() {
        const ROOM: &str = "dead-lane-two-keys-room";
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let mut map = lane_map(&h, ROOM, Arc::default());

        let publisher = video_key(7_004);
        map.publisher_lane(publisher);
        kill_lane(map.publishers.get_mut(&publisher).expect("just spawned"));
        kill_lane(&mut map.control);

        // Back to back, well inside one WT_DOWNLINK_LANE_RESPAWN_COOLDOWN.
        map.dispatch(keyed_frame(OutboundPriority::Video, publisher));
        map.dispatch(keyed_frame(
            OutboundPriority::Control,
            DownlinkStreamKey::Control,
        ));

        assert!(
            !map.publishers
                .get(&publisher)
                .expect("the publisher key must keep its entry")
                .tx
                .is_closed(),
            "the publisher lane must be replaced",
        );
        assert!(
            !map.control.tx.is_closed(),
            "the control lane must be replaced in the SAME cooldown: its recovery \
             cannot wait on an unrelated key's",
        );
        map.close_and_join().await;
    }

    /// Every key pinned to the overflow lane holds a CLONE of its sender, so a
    /// dead overflow lane is dead for all of them. BITES: delete the `retain`
    /// in `drop_overflow` (the pinned key keeps routing into the closed
    /// channel), or its `self.overflow = None`.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_dead_overflow_lane_unpins_every_key_routed_to_it() {
        const ROOM: &str = "dead-lane-overflow-room";
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let mut map = lane_map(&h, ROOM, Arc::default());

        map.ensure_overflow();
        let pinned = video_key(7_003);
        let pinned_lane =
            DownlinkLane::pinned_to(map.overflow.as_ref().expect("just ensured overflow"));
        map.publishers.insert(pinned, pinned_lane);
        kill_lane(map.overflow.as_mut().expect("just ensured overflow"));
        let dead = map.overflow.as_ref().expect("still present").tx.clone();
        map.publishers.get_mut(&pinned).expect("just inserted").tx = dead;

        map.dispatch(keyed_frame(OutboundPriority::Video, pinned));

        assert!(
            map.overflow.is_none(),
            "a dead overflow lane must be forgotten so the next frame opens a \
             fresh one",
        );
        assert!(
            !map.publishers.contains_key(&pinned),
            "a key pinned to the dead overflow lane must be unpinned with it",
        );

        // The overflow key carries its own cooldown, like every other key.
        map.ensure_overflow();
        kill_lane(map.overflow.as_mut().expect("just re-ensured overflow"));
        map.dispatch(keyed_frame(
            OutboundPriority::Video,
            DownlinkStreamKey::Shared,
        ));
        assert!(
            map.overflow
                .as_ref()
                .expect("the entry must survive a refused replacement")
                .tx
                .is_closed(),
            "a second overflow death inside WT_DOWNLINK_LANE_RESPAWN_COOLDOWN must \
             not open another stream",
        );
        map.close_and_join().await;
    }

    /// The replacement is what a failing `open_uni` would otherwise run once per
    /// frame. BITES: delete the `respawn_after` gate, or stop stamping it
    /// forward on the replacement.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn one_key_cannot_respawn_faster_than_the_cooldown() {
        const ROOM: &str = "dead-lane-cooldown-room";
        let h = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        let mut map = lane_map(&h, ROOM, Arc::default());

        kill_lane(&mut map.control);
        map.dispatch(keyed_frame(
            OutboundPriority::Control,
            DownlinkStreamKey::Control,
        ));
        assert!(
            !map.control.tx.is_closed(),
            "setup failed: the first death must be replaced immediately",
        );

        kill_lane(&mut map.control);
        map.dispatch(keyed_frame(
            OutboundPriority::Control,
            DownlinkStreamKey::Control,
        ));
        assert!(
            map.control.tx.is_closed(),
            "a second death inside WT_DOWNLINK_LANE_RESPAWN_COOLDOWN must not \
             spawn another lane",
        );
        map.close_and_join().await;
    }

    fn packet_drops_total(reason: &str) -> f64 {
        crate::metrics::RELAY_PACKET_DROPS_TOTAL
            .with_label_values(&["test-room", "webtransport", reason])
            .get()
    }

    fn session_drops_total(kind: &str) -> f64 {
        crate::metrics::RELAY_SESSION_DROPS_TOTAL
            .with_label_values(&["test-room", "webtransport", "1", kind])
            .get()
    }

    fn lane_task_entries() -> f64 {
        RELAY_DOWNLINK_LANE_TASK_ENTRIES
            .with_label_values(&["webtransport"])
            .get()
    }

    fn overflow_frames_total(cause: &str) -> f64 {
        RELAY_DOWNLINK_STREAM_OVERFLOW_FRAMES_TOTAL
            .with_label_values(&["webtransport", cause])
            .get()
    }

    fn idle_reaps_total() -> f64 {
        RELAY_DOWNLINK_STREAM_IDLE_REAPS_TOTAL
            .with_label_values(&["webtransport"])
            .get()
    }

    fn finishes_total() -> f64 {
        RELAY_DOWNLINK_STREAM_FINISHES_TOTAL
            .with_label_values(&["webtransport"])
            .get()
    }

    fn queue_drops_total(kind: &str) -> f64 {
        RELAY_DOWNLINK_STREAM_QUEUE_DROPS_TOTAL
            .with_label_values(&["webtransport", kind])
            .get()
    }

    fn overflow_critical_drops() -> f64 {
        outbound_drops("overflow_critical")
    }

    fn outbound_drops(kind: &str) -> f64 {
        OUTBOUND_CHANNEL_DROPS_TOTAL
            .with_label_values(&["webtransport", kind])
            .get()
    }

    fn stream_slots() -> f64 {
        RELAY_DOWNLINK_STREAM_SLOTS
            .with_label_values(&["webtransport"])
            .get()
    }

    fn stream_reset_total(reason: &str) -> f64 {
        RELAY_OUTBOUND_BRIDGE_STREAM_RESETS_TOTAL
            .with_label_values(&["webtransport", reason])
            .get()
    }

    fn escalations_total(stage: &str) -> f64 {
        RELAY_DOWNLINK_SHED_ESCALATIONS_TOTAL
            .with_label_values(&["webtransport", stage])
            .get()
    }

    fn session_closes_total(reason: &str) -> f64 {
        RELAY_WT_SESSION_CLOSES_TOTAL
            .with_label_values(&["webtransport", reason])
            .get()
    }

    async fn wait_until_process_epoch_reaches(floor_ms: u64) {
        let now = crate::actors::session_logic::downlink_congested_epoch_now();
        if now < floor_ms {
            tokio::time::sleep(Duration::from_millis(floor_ms - now)).await;
        }
    }

    /// Write count ADVANCING, so the bar under test is the run, not the stall.
    fn seed_past_rounds(escalation: &DownlinkShedEscalation, rounds: u64) {
        let base = crate::actors::session_logic::downlink_congested_epoch_now();
        let step = WT_SHED_ESCALATION_ROUND.as_millis() as u64;
        for back in (1..=rounds).rev() {
            escalation.record_shed_at(base - back * step, rounds - back);
        }
        assert_eq!(
            escalation.rounds_recorded() as u64,
            rounds,
            "test setup failed: seeded rounds were coalesced or trimmed",
        );
        assert!(
            !escalation.session_closed(),
            "test setup failed: the seeds already closed the session",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_stage_two_escalation_closes_the_real_session_with_the_re_election_code() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        wait_until_process_epoch_reaches(
            (WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 + 1)
                * WT_SHED_ESCALATION_ROUND.as_millis() as u64,
        )
        .await;

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let escalation = DownlinkShedEscalation::new();
        seed_past_rounds(&escalation, WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 - 1);
        escalation.note_write_completed();
        let stage_two_before = escalations_total("two");
        let closes_before = session_closes_total("shed_rounds");

        assert!(
            escalate_unistream_shed(&server_session, &escalation),
            "the round that reaches WT_SHED_ESCALATION_STAGE2_ROUNDS must close \
             the session instead of resetting another stream",
        );
        assert_eq!(
            escalations_total("two") - stage_two_before,
            1.0,
            "the stage-2 episode must be countable",
        );
        assert_eq!(
            session_closes_total("shed_rounds") - closes_before,
            1.0,
            "and attributed to the round threshold, not to stalled credit",
        );

        let seen = tokio::time::timeout(Duration::from_secs(10), client_session.closed())
            .await
            .expect("the relay's close must reach the client");
        match seen {
            SessionError::WebTransportError(web_transport_quinn::WebTransportError::Closed(
                code,
                ref reason,
            )) => {
                // LITERALS: asserting a constant against itself lets a
                // renumbering through (contract E5).
                assert_eq!(
                    code, 1001,
                    "the client half matches on 1001; renumbering breaks re-election",
                );
                assert_eq!(reason.as_str(), "downlink-shed-escalation");
            }
            other => panic!("expected a WebTransport application close, got {other:?}"),
        }

        assert!(
            !escalate_unistream_shed(&server_session, &escalation),
            "another lane of the SAME whole-map shed must not close again",
        );
        assert_eq!(
            session_closes_total("shed_rounds") - closes_before,
            1.0,
            "one close, whatever the lane count",
        );
    }

    struct CloseCauseRig {
        client: web_transport_quinn::Session,
        bridge: WebTransportBridge,
        uni_tx: mpsc::Sender<WtOutboundFrame>,
        dgram_tx: mpsc::Sender<WtOutboundFrame>,
        escalation: DownlinkShedEscalation,
    }

    async fn close_cause_rig(transport: Option<Arc<quinn::TransportConfig>>) -> CloseCauseRig {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (addr, mut server) = build_test_server_with_transport(transport);
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let client = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let (dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let escalation = DownlinkShedEscalation::new();
        let bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            Arc::new(SharedQueueByteMeter::default()),
            None,
            Arc::new(AtomicU64::new(0)),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            escalation.clone(),
        );
        CloseCauseRig {
            client,
            bridge,
            uni_tx,
            dgram_tx,
            escalation,
        }
    }

    async fn close_cause_after(bridge: &mut WebTransportBridge) -> String {
        tokio::time::timeout(Duration::from_secs(10), bridge.wait_for_disconnect())
            .await
            .expect("the bridge must end")
    }

    /// Splits a close cause into `(key, value)` pairs, decoding quoted values.
    fn close_cause_tokens(cause: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = cause;
        while !rest.is_empty() {
            let (key, after) = rest.split_once('=').expect("key=value");
            let (value, next) = match after.strip_prefix('"') {
                Some(quoted) => {
                    let (mut value, mut escaped, mut end) = (String::new(), false, None);
                    for (i, c) in quoted.char_indices() {
                        if escaped {
                            value.push(if c == 'n' { '\n' } else { c });
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '"' {
                            end = Some(i);
                            break;
                        } else {
                            value.push(c);
                        }
                    }
                    (value, &quoted[end.expect("closing quote") + 1..])
                }
                None => {
                    let (v, n) = after.split_once(' ').unwrap_or((after, ""));
                    (v.to_string(), n)
                }
            };
            out.push((key.to_string(), value));
            rest = next.strip_prefix(' ').unwrap_or(next);
        }
        out
    }

    fn close_cause_field_of(cause: &str, key: &str) -> String {
        close_cause_tokens(cause)
            .into_iter()
            .find_map(|(k, v)| (k == key).then_some(v))
            .unwrap_or_else(|| panic!("no {key}= in {cause:?}"))
    }

    fn assert_close_cause_keys(cause: &str, expected: &[&str]) {
        let keys: Vec<String> = close_cause_tokens(cause)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, expected, "{cause:?}");
    }

    fn assert_a_reader_ended(cause: &str) {
        let ended = close_cause_field_of(cause, "ended");
        assert!(
            ended == "uni_reader" || ended == "datagram_reader",
            "a closed connection ends a reader first, got {cause:?}",
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn wait_for_disconnect_returns_the_peers_close_code_and_reason() {
        let mut rig = close_cause_rig(None).await;

        rig.client.close(4242, b"client-left");
        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert!(
            cause
                .starts_with(r#"closed_by=peer_wt wt_close="code=4242 reason=client-left" ended="#),
            "the close cause must carry the peer's code and reason, got {cause:?}",
        );
        assert_a_reader_ended(&cause);
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_peer_quic_close_without_a_capsule_is_named_as_a_peer_close() {
        let mut rig = close_cause_rig(None).await;

        let conn: &quinn::Connection = &rig.client;
        conn.close(0x100u32.into(), b"tab-gone");
        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert_close_cause_keys(&cause, &["closed_by", "ended"]);
        assert_eq!(
            close_cause_field_of(&cause, "closed_by"),
            "peer_quic",
            "{cause:?}"
        );
        assert_a_reader_ended(&cause);
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn an_idle_timeout_is_not_named_as_a_peer_or_relay_close() {
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(
            Duration::from_millis(300)
                .try_into()
                .expect("idle timeout fits"),
        ));
        let mut rig = close_cause_rig(Some(Arc::new(transport))).await;

        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert_close_cause_keys(&cause, &["closed_by", "ended"]);
        assert_eq!(
            close_cause_field_of(&cause, "closed_by"),
            "no_close_frame",
            "{cause:?}"
        );
        assert_a_reader_ended(&cause);
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_relay_side_end_names_the_bridge_task_that_ended() {
        let mut rig = close_cause_rig(None).await;

        drop(rig.dgram_tx);
        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert_eq!(cause, "closed_by=relay_task ended=datagram_writer");
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_peer_close_reason_is_logged_on_one_bounded_line() {
        let mut rig = close_cause_rig(None).await;

        let reason = format!("line1\nline2{}", "x".repeat(500));
        rig.client.close(7, reason.as_bytes());
        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert!(
            !cause.contains('\n'),
            "a raw newline must not reach the log: {cause:?}"
        );
        assert_close_cause_keys(&cause, &["closed_by", "wt_close", "ended"]);
        let wt_close = close_cause_field_of(&cause, "wt_close");
        assert_eq!(
            wt_close.chars().count(),
            200,
            "the peer's reason must be capped: {cause:?}"
        );
        assert!(wt_close.contains("line1\nline2"), "{cause:?}");
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_peer_close_reason_cannot_forge_log_fields() {
        let mut rig = close_cause_rig(None).await;

        rig.client.close(
            7,
            br#"x" closed_by=relay_task ended=datagram_writer is_guest=false"#,
        );
        let cause = close_cause_after(&mut rig.bridge).await;
        rig.bridge.shutdown().await;

        assert_close_cause_keys(&cause, &["closed_by", "wt_close", "ended"]);
        assert_eq!(close_cause_field_of(&cause, "closed_by"), "peer_wt");
        assert_eq!(
            close_cause_field_of(&cause, "wt_close"),
            r#"code=7 reason=x" closed_by=relay_task ended=datagram_writer is_guest=false"#,
        );
        assert_a_reader_ended(&cause);
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_stage_two_shed_close_ends_the_writer_and_is_named_in_the_close_cause() {
        wait_until_process_epoch_reaches(
            (WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 + 1)
                * WT_SHED_ESCALATION_ROUND.as_millis() as u64,
        )
        .await;
        // The client never reads its uni stream, so the relay's writer wedges.
        let mut rig = close_cause_rig(None).await;
        let base = crate::actors::session_logic::downlink_congested_epoch_now();
        let step = WT_SHED_ESCALATION_ROUND.as_millis() as u64;
        let seeds = WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 - 1;
        for i in 0..seeds {
            rig.escalation
                .record_shed_at(base - (seeds - 1 - i) * step, i);
        }

        let uni_tx = rig.uni_tx.clone();
        let feeder = tokio::spawn(async move {
            while !matches!(
                push(&uni_tx, 64 * 1024),
                Err(mpsc::error::TrySendError::Closed(_))
            ) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let cause = close_cause_after(&mut rig.bridge).await;
        feeder.abort();
        rig.bridge.shutdown().await;

        assert_close_cause_keys(&cause, &["closed_by", "ended"]);
        assert_eq!(
            close_cause_field_of(&cause, "closed_by"),
            "relay_shed",
            "{cause:?}"
        );
        assert_eq!(
            close_cause_field_of(&cause, "ended"),
            "uni_writer",
            "{cause:?}"
        );
    }

    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_stage_one_escalation_arms_the_admission_gate_and_is_counted() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        wait_until_process_epoch_reaches(
            (WT_SHED_ESCALATION_STAGE1_ROUNDS as u64 + 1)
                * WT_SHED_ESCALATION_ROUND.as_millis() as u64,
        )
        .await;

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let escalation = DownlinkShedEscalation::new();
        seed_past_rounds(&escalation, WT_SHED_ESCALATION_STAGE1_ROUNDS as u64 - 1);
        assert!(
            !escalation.camera_video_is_shed(),
            "below the threshold this receiver keeps its camera video",
        );
        let stage_one_before = escalations_total("one");

        assert!(
            !escalate_unistream_shed(&server_session, &escalation),
            "stage 1 still resets and re-opens; only stage 2 closes",
        );
        assert_eq!(
            escalations_total("one") - stage_one_before,
            1.0,
            "the stage-1 episode must be countable",
        );
        assert!(
            escalation.camera_video_is_shed(),
            "stage 1 must arm the admission gate that drops this receiver's \
             camera VIDEO",
        );
        assert!(
            !escalation.session_closed(),
            "stage 1 never closes a session"
        );
    }

    /// The DIRECTION pin contract E16 rests on, on two REAL loopback sessions.
    /// BITES: move `note_write_completed` off the delivered path.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_reading_peer_advances_the_delivery_count_and_a_wedged_one_freezes_it() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut reading = spin_up(DownlinkStreamMode::PerPublisherV1, None).await;
        reading.offer(DownlinkStreamKey::Control, OutboundPriority::Critical, 64);
        reading.offer(video_key(9), OutboundPriority::Video, 4_096);
        let _ = reading
            .collect_until(Duration::from_secs(10), |seen| {
                seen.iter()
                    .filter(|e| matches!(e, StreamEvent::Frame(_, _)))
                    .count()
                    >= 2
            })
            .await;
        assert!(
            reading.escalation.writes_completed() >= 2,
            "a peer that reads must advance the delivery counter; it read {} \
             frames and the counter is {}",
            2,
            reading.escalation.writes_completed(),
        );

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let total = crate::constants::wt_outbound_channel_capacity();
        let (uni_tx, uni_rx) = mpsc::channel::<WtOutboundFrame>(total);
        let (_dgram_tx, dgram_rx) = mpsc::channel::<WtOutboundFrame>(16);
        let meter = Arc::new(SharedQueueByteMeter::default());
        let wedged = DownlinkShedEscalation::new();
        let _bridge = WebTransportBridge::new_with_callback(
            server_session,
            StubActor.start(),
            uni_rx,
            dgram_rx,
            uni_tx.clone(),
            meter.clone(),
            None,
            Arc::new(AtomicU64::new(0)),
            test_drop_sink().0,
            DownlinkStreamMode::Single,
            wedged.clone(),
        );

        let frame_bytes = crate::constants::tier_frame_bytes(
            &videocall_aq::constants::VIDEO_QUALITY_TIERS
                [videocall_aq::constants::DEFAULT_VIDEO_TIER_INDEX],
        );
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            for _ in 0..(total * 2) {
                if matches!(
                    wt_unistream_admit(
                        &uni_tx,
                        &meter,
                        OutboundPriority::Video,
                        Bytes::from(vec![0x44; frame_bytes]),
                        DownlinkStreamKey::Control,
                    ),
                    WtAdmission::PriorityDropped { .. }
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            loop {
                let before = wedged.writes_completed();
                tokio::time::sleep(WT_UNISTREAM_WRITE_DEADLINE + Duration::from_millis(500)).await;
                if wedged.writes_completed() == before {
                    return before;
                }
            }
        })
        .await;
        let frozen_at = outcome.expect(
            "a receiver that never reads must stop completing writes; the count \
             kept climbing for 30 s, so the signal does not identify a wedge",
        );
        assert!(
            wedged.rounds_recorded() > 0,
            "test setup failed: the wedged session never shed, so nothing was \
             measured while parked",
        );
        assert_eq!(
            wedged.writes_completed(),
            frozen_at,
            "the delivery count must stay frozen while the peer is not reading",
        );
    }

    /// Structural: the escalation is gated on the literal `write_timeout`, which
    /// the non-shedding arm never returns. BITES: return it anyway.
    #[actix_rt::test]
    #[serial_test::serial]
    async fn a_wedged_audio_lane_never_returns_the_write_timeout_that_opens_a_round() {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (addr, mut server) = build_test_server();
        let server_session_fut = tokio::spawn(async move {
            let request = server.accept().await.expect("accept request");
            request.ok().await.expect("respond ok")
        });
        let _client_session = connect_test_client(addr).await;
        let server_session = server_session_fut.await.expect("join server session");

        let mut stream = server_session.open_uni().await.expect("open uni");
        let (tx, rx) = mpsc::channel::<WtOutboundFrame>(4);
        for _ in 0..4 {
            let _ = tx.try_send(WtOutboundFrame::keyed(
                OutboundPriority::Audio,
                Bytes::from_static(b"x"),
                DownlinkStreamKey::Audio,
            ));
        }
        let meter = SharedQueueByteMeter::default();
        let mut ticker = tokio::time::interval(WT_UNISTREAM_BACKPRESSURE_POLL);
        let big = Bytes::from(vec![0x5A; MAX_FRAME_SIZE / 2]);

        let parked = tokio::time::timeout(
            WT_UNISTREAM_WRITE_DEADLINE * 4,
            write_framed_with_backpressure_shed(
                &mut stream,
                &(big.len() as u32).to_be_bytes(),
                &big,
                &rx,
                &meter,
                &mut ticker,
                DownlinkStreamClass::Audio.sheds_on_backpressure(),
            ),
        )
        .await;
        assert!(
            parked.is_err(),
            "the audio lane must still be parked after 4x the #1638 grace; \
             anything it returned here would have been a shed round (#2726)",
        );
    }
}
