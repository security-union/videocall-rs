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

//! WebTransport Chat Session Actor
//!
//! This is a thin transport adapter that delegates all business logic
//! to `SessionLogic`. It handles WebTransport-specific I/O via channels.

use crate::actors::chat_server::ChatServer;
use crate::actors::packet_handler::DATAGRAM_MAX_SIZE;
use crate::actors::priority_drop::{
    evaluate as evaluate_priority_drop, OutboundPriority, PriorityDropDecision,
};
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::{
    wt_outbound_channel_capacity, CLIENT_TIMEOUT, WT_DATAGRAM_CHANNEL_CAPACITY,
};
use crate::messages::server::{ActivateConnection, Packet};
use crate::messages::session::Message;
use crate::metrics::{
    OUTBOUND_CHANNEL_DROPS_TOTAL, RELAY_OUTBOUND_QUEUE_DEPTH, RELAY_PACKET_DROPS_TOTAL,
};
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::{
    fut, Actor, ActorContext, ActorFutureExt, Addr, AsyncContext, Context, ContextFutureSpawner,
    Handler, Message as ActixMessage, Running, WrapFuture,
};
use bytes::Bytes;
use protobuf::Message as ProtobufMessage;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, trace, warn};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{RoomId, SessionId, UserId};

/// Heartbeat interval for WebTransport sessions
const WT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Keep-alive ping data (WebTransport-specific)
const KEEP_ALIVE_PING: &[u8] = b"ping";

/// Routing decision for an outbound WebTransport packet.
///
/// Produced by [`build_outbound`] and consumed by [`WtChatSession::send_auto`]
/// to pick the correct per-primitive channel. The bridge no longer sees this
/// enum — each variant maps 1:1 to a dedicated bridge writer task drained by
/// its own bounded channel of [`Bytes`].
///
/// The split-channel topology is the central architectural fix for the
/// WT-freeze symptom: when QUIC flow-control credits on the persistent uni
/// stream drain to zero, the unistream writer task blocks on `write_all`.
/// Because datagrams are drained by an independent task on an independent
/// channel, they continue to flow through `send_datagram` even while the
/// unistream writer is parked. See discussion #756 for the full analysis.
#[derive(Debug, Clone)]
pub enum WtOutbound {
    /// Send via the persistent unidirectional QUIC stream (reliable, ordered,
    /// length-prefix framed). Used for video, screen, and oversized
    /// audio/control packets.
    UniStream(Bytes),
    /// Send via QUIC datagram (unreliable, unordered, low latency).
    /// Used for small audio media (Opus frames) and non-media control that
    /// fits within `DATAGRAM_MAX_SIZE`.
    Datagram(Bytes),
}

/// Result of attempting to send an outbound message to the WebTransport channel.
enum WtSendResult {
    /// Message sent successfully.
    Sent,
    /// Channel is full at the time of `try_send`; message was dropped.
    /// The transport-agnostic drop counters and the legacy media-kind
    /// labels (`audio`/`video`/`screen`/`media`/`control`/`unknown`,
    /// or `overflow_critical` for Critical packets) are bumped at the
    /// call site.
    Dropped,
    /// Channel is closed; connection is dead.
    Dead,
    /// Packet was *preemptively* dropped before `try_send` by the
    /// priority-drop policy because the channel was approaching
    /// saturation. Distinct from `Dropped` so callers can keep both
    /// semantic paths in pattern matches even if today they take the
    /// same action (fire CONGESTION feedback). The drop metric is
    /// already incremented inside `send_auto` with the policy-
    /// specific label (`priority_drop_video` / `priority_drop_audio`).
    PriorityDropped,
}

/// Source of inbound data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WtInboundSource {
    UniStream,
    Datagram,
}

/// Inbound message from WebTransport session
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct WtInbound {
    pub data: Bytes,
    pub source: WtInboundSource,
}

/// Signal to stop the session (sent when I/O tasks end)
#[derive(ActixMessage)]
#[rtype(result = "()")]
pub struct StopSession;

/// WebTransport Chat Session Actor
///
/// A thin transport adapter that delegates business logic to `SessionLogic`.
/// Handles WebTransport-specific I/O via channels.
///
/// ### Why two outbound senders?
///
/// As of the Phase 2 WT-freeze fix (discussion #756), the actor holds
/// **two** independent `mpsc::Sender<Bytes>` handles — one feeding the
/// persistent uni-stream writer task and one feeding the datagram writer
/// task. The split mirrors the QUIC primitives:
///
/// * `unistream_tx` (capacity = [`wt_outbound_channel_capacity`], today 4096)
///   absorbs video, screen, and any oversized audio/control. This is where
///   QUIC flow control surfaces; the priority-drop policy applies here.
/// * `datagram_tx` (capacity = [`WT_DATAGRAM_CHANNEL_CAPACITY`], 512) carries
///   audio media (Opus, ~80B) and non-media control under MTU. Datagrams
///   are independent of stream flow control, so the channel exists only to
///   absorb scheduling jitter.
///
/// Previously a single channel multiplexed both. When QUIC stalled the
/// uni-stream, the writer task parked on `write_all`, and audio datagrams
/// queued behind the stalled video write in the same channel. The split
/// removes that coupling: a stalled stream cannot starve datagrams.
pub struct WtChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: actix::clock::Instant,

    /// Channel to the persistent unidirectional QUIC stream writer task.
    /// Used for video, screen, and oversized audio/control packets. The
    /// priority-drop policy is evaluated against this channel's fill ratio.
    unistream_tx: mpsc::Sender<Bytes>,

    /// Channel to the datagram writer task. Used for audio media (when it
    /// fits MTU) and small non-media control. Independent of `unistream_tx`
    /// so a stalled uni-stream cannot block datagram delivery.
    datagram_tx: mpsc::Sender<Bytes>,

    /// Track if ActivateConnection has been sent
    activated: bool,
}

/// Pure outbound-routing decision used by [`WtChatSession::send_auto`].
///
/// Extracted as a free function so it can be unit-tested in isolation
/// (constructing a real `WtChatSession` requires a populated
/// `SessionLogic`, which requires NATS, addresses, etc.).
///
/// Routing rules (priority order):
/// 1. Non-media, fits MTU → datagram (control / heartbeats / RTT).
/// 2. Media + audio + fits MTU → datagram (Opus frames are 50-200B,
///    well below the ~1200B MTU; avoids per-receiver UniStream HOL
///    blocking when a single UDP segment is lost).
/// 3. Everything else (video, screen, oversized audio, oversized
///    control) → reliable unidirectional stream.
fn build_outbound(data: Vec<u8>, is_media: bool, is_audio: bool) -> WtOutbound {
    let fits_datagram = data.len() <= DATAGRAM_MAX_SIZE;
    if is_media {
        if is_audio && fits_datagram {
            WtOutbound::Datagram(data.into())
        } else {
            WtOutbound::UniStream(data.into())
        }
    } else if fits_datagram {
        WtOutbound::Datagram(data.into())
    } else {
        WtOutbound::UniStream(data.into())
    }
}

/// Classify a dropped outbound packet for the
/// `videocall_outbound_channel_drops_total{kind=...}` label.
///
/// Mirrors the WS site (`ws_chat_session::Handler<Message>`):
/// * `parsed=false` → `"unknown"` — the upstream `PacketWrapper` parse
///   failed, so we cannot trust `is_media`. Emit the same fallback the
///   WS path uses so alerts tuned on `kind` behave consistently across
///   transports (issue #610).
/// * `parsed=true && !is_media` → `"control"`.
/// * `parsed=true && is_media && media_type == Some(AUDIO)`  → `"audio"`.
/// * `parsed=true && is_media && media_type == Some(VIDEO)`  → `"video"`.
/// * `parsed=true && is_media && media_type == Some(SCREEN)` → `"screen"`.
/// * `parsed=true && is_media && media_type` is anything else (HEARTBEAT,
///   KEYFRAME_REQUEST, encrypted/unparseable inner) → `"media"`. This is
///   the legacy catch-all so existing alerts that pivot on `kind="media"`
///   still see a series.
///
/// Extracted as a free function so the mapping can be unit-tested
/// without spinning up a real `WtChatSession`.
fn drop_kind_label(parsed: bool, is_media: bool, media_type: Option<MediaType>) -> &'static str {
    if !parsed {
        return "unknown";
    }
    if !is_media {
        return "control";
    }
    match media_type {
        Some(MediaType::AUDIO) => "audio",
        Some(MediaType::VIDEO) => "video",
        Some(MediaType::SCREEN) => "screen",
        _ => "media",
    }
}

impl WtChatSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        is_guest: bool,
        unistream_tx: mpsc::Sender<Bytes>,
        datagram_tx: mpsc::Sender<Bytes>,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
        observer: bool,
        instance_id: Option<String>,
        is_host: bool,
        end_on_host_leave: bool,
    ) -> Self {
        let logic = SessionLogic::new(
            addr,
            room,
            user_id,
            display_name,
            is_guest,
            nats_client,
            tracker_sender,
            session_manager,
            observer,
            instance_id,
            "webtransport",
            is_host,
            end_on_host_leave,
        );

        WtChatSession {
            logic,
            heartbeat: actix::clock::Instant::now(),
            unistream_tx,
            datagram_tx,
            activated: false,
        }
    }

    /// Send outbound message via the channel (reliable unidirectional stream).
    /// Returns false if the channel is closed (connection dead).
    ///
    /// `send()` is used for server-originated control packets that are
    /// part of the session lifecycle: `SESSION_ASSIGNED`,
    /// `MEETING_STARTED`, `MEETING_ENDED`. These are *Critical* under
    /// the priority-drop policy — they are never preemptively dropped
    /// and only fail when the channel is genuinely full. When that
    /// happens we record `kind="overflow_critical"` on the protocol-
    /// wide counter so saturation severe enough to drop lifecycle
    /// packets is alertable on its own (separate from the much higher-
    /// volume media drops).
    fn send(&self, data: Vec<u8>) -> bool {
        // Lifecycle control (SESSION_ASSIGNED, MEETING_STARTED, MEETING_ENDED)
        // routes via the reliable uni-stream channel by design — these packets
        // are not idempotent and must arrive in order. They never use the
        // datagram path even though they typically fit MTU.
        match self.unistream_tx.try_send(data.into()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "UniStream outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                false
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "webtransport", "channel_full"])
                    .inc();
                // Lifecycle control packet dropped on real overflow.
                // The priority-drop policy guarantees these are never
                // preempted by media saturation; a drop here means the
                // channel is so full even the highest-priority packets
                // cannot be admitted. Pages on this should be loud.
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["webtransport", "overflow_critical"])
                    .inc();
                error!(
                    "UniStream outbound channel full for session {} on Critical control packet, dropping (overflow_critical)",
                    self.logic.id
                );
                true // Channel still open, just full
            }
        }
    }

    /// Send outbound message, automatically choosing datagram or stream.
    ///
    /// Routing rules (in priority order):
    ///
    /// 1. **Non-media control packets** (heartbeats, RTT probes, diagnostics,
    ///    AES key exchange, …) that fit within `DATAGRAM_MAX_SIZE` use
    ///    unreliable datagrams. They are periodic and expendable, so lower
    ///    overhead matters more than guaranteed delivery.
    /// 2. **Audio media** packets (Opus frames are typically 50-200B,
    ///    well below the ~1200B datagram MTU) also use datagrams so a
    ///    single dropped UDP segment does not head-of-line block every
    ///    subsequent audio frame on a shared per-receiver UniStream.
    ///    Lossy audio is far less perceptible than a multi-hundred-ms
    ///    audio gap waiting on QUIC retransmit.
    /// 3. **Video / screen media**, oversized audio, and any other media
    ///    use the reliable unidirectional stream — keeping ordered delivery
    ///    avoids visual artifacts and matches encoder expectations.
    ///
    /// The `is_media` and `is_audio` hints are pre-computed by the caller
    /// from an already-parsed `PacketWrapper` / `MediaPacket`, avoiding a
    /// redundant protobuf parse on every outbound packet.
    ///
    /// `parsed` is a tri-state signal: `true` if the upstream
    /// `PacketWrapper` parsed successfully (so `is_media` is trustworthy),
    /// `false` if the parse failed and `is_media` is the safe-default
    /// fallback (`false`). This is used only for the drop-counter `kind`
    /// label so it matches the WS site's `unknown` fallback — routing
    /// continues to honour the same safe default it always has.
    ///
    /// `media_type` is the inner `MediaPacket.media_type` when it could be
    /// extracted (the inner parse succeeded). It is used solely to refine
    /// the drop-counter `kind` label into `audio`/`video`/`screen` — the
    /// 2026-05-08 production storm dropped 25,081 packets to one slow
    /// receiver and the metric had no way to tell audio from video. This
    /// hint does NOT influence routing; routing is decided by `is_audio`,
    /// which preserves the original behaviour for encrypted inner payloads.
    fn send_auto(
        &self,
        data: Vec<u8>,
        is_media: bool,
        is_audio: bool,
        parsed: bool,
        packet_type: PacketType,
        media_type: Option<MediaType>,
    ) -> WtSendResult {
        // Classify the packet first — both the routing decision (datagram
        // vs unistream) and the priority-drop pre-check depend on it.
        let outbound = build_outbound(data, is_media, is_audio);
        let priority = OutboundPriority::classify(parsed, packet_type, media_type);

        // Priority-drop pre-check is evaluated against the destination
        // channel's fill, not a single unified queue. After the channel
        // split, audio media routes via `datagram_tx` and never collides
        // with video on `unistream_tx`, so audio's 95% drop threshold
        // applies to the datagram channel and video's 80% threshold
        // applies to the unistream channel. Critical / Control never
        // preempt on either channel — `evaluate_priority_drop` enforces
        // that internally.
        //
        // Lifecycle note: a reconnection wave that needs to deliver
        // SESSION_ASSIGNED + MEETING_STARTED still goes through —
        // Critical packets are never preempted by this layer regardless
        // of channel fill.
        let (target_total_capacity, free_capacity) = match &outbound {
            WtOutbound::UniStream(_) => {
                (wt_outbound_channel_capacity(), self.unistream_tx.capacity())
            }
            WtOutbound::Datagram(_) => (WT_DATAGRAM_CHANNEL_CAPACITY, self.datagram_tx.capacity()),
        };

        if let PriorityDropDecision::Drop { reason } =
            evaluate_priority_drop(priority, free_capacity, target_total_capacity)
        {
            // Mirror the legacy drop-counter pair so per-room and
            // protocol-wide series both observe the preempt. The
            // protocol-wide counter uses the priority-specific reason
            // label so dashboards can distinguish a policy-driven drop
            // from a genuine channel-overflow drop.
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[&self.logic.room, "webtransport", reason])
                .inc();
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["webtransport", reason])
                .inc();
            trace!(
                "Priority-drop {reason} on WT session {}: free={free_capacity}/{target_total_capacity}",
                self.logic.id,
            );
            return WtSendResult::PriorityDropped;
        }

        // Phase 2 split: route the bytes to the dedicated per-primitive
        // channel. The bridge writer tasks drain these independently — a
        // stalled unistream writer cannot back up the datagram channel.
        let try_send_result = match outbound {
            WtOutbound::UniStream(bytes) => self.unistream_tx.try_send(bytes),
            WtOutbound::Datagram(bytes) => self.datagram_tx.try_send(bytes),
        };

        match try_send_result {
            Ok(()) => WtSendResult::Sent,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Outbound channel closed for session {}, connection dead",
                    self.logic.id
                );
                WtSendResult::Dead
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Phase 8b TELEM-8: this drop site previously lacked any
                // counter increment, so a flood of media drops only surfaced
                // in the log line below. Increment both the room-tagged
                // RELAY_PACKET_DROPS_TOTAL (for per-room investigation) and
                // the protocol-wide OUTBOUND_CHANNEL_DROPS_TOTAL (for
                // alerting). `kind` is derived from the already-parsed
                // `is_media` hint plus the inner `MediaType` — we explicitly
                // avoid an extra protobuf parse on the drop hot path.
                //
                // Issue #610: when the upstream parse failed (`parsed=false`)
                // we cannot trust `is_media`, so emit `kind="unknown"` to
                // match the WS site's fallback. Without this, malformed
                // wire-bytes would silently inflate WT's `control` series
                // while WS would distinguish them as `unknown`, breaking
                // alerts tuned on the same label across transports.
                //
                // 2026-05-08 audio-quality follow-up: when the inner
                // `MediaType` is known, the label is refined into
                // `audio`/`video`/`screen` so operators can attribute a
                // congestion storm to the specific media stream. Anything
                // else (HEARTBEAT, KEYFRAME_REQUEST, encrypted inner)
                // continues to use the legacy `media` catch-all.
                //
                // 2026-05-11 priority-drop policy (discussion #699): if
                // the priority is Critical (SESSION_ASSIGNED,
                // CONGESTION, RSA_PUB_KEY, MEETING) and try_send still
                // fails, emit `kind="overflow_critical"` so the
                // exceptional case of a lifecycle packet dropped is
                // alertable independently of normal media drops.
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "webtransport", "channel_full"])
                    .inc();
                let kind = if priority == OutboundPriority::Critical {
                    "overflow_critical"
                } else {
                    drop_kind_label(parsed, is_media, media_type)
                };
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["webtransport", kind])
                    .inc();
                error!(
                    "Outbound channel full for session {}, dropping message (kind={kind})",
                    self.logic.id
                );
                WtSendResult::Dropped
            }
        }
    }

    /// Check if either outbound channel is closed.
    ///
    /// The session is considered dead if **either** primitive's writer task
    /// has gone away — the actor cannot meaningfully continue if it can only
    /// deliver half of its outbound traffic. In practice both channels are
    /// dropped together when the bridge tears down on session end, so this
    /// is symmetric.
    fn is_connection_dead(&self) -> bool {
        self.unistream_tx.is_closed() || self.datagram_tx.is_closed()
    }

    /// Start heartbeat check (WebTransport-specific timing).
    ///
    /// Emits the `relay_outbound_queue_depth` gauge as the sum of the two
    /// per-primitive channels. The label scheme is preserved
    /// (`transport=webtransport`) so existing dashboards continue to work:
    /// the gauge now reflects the *total* outbound backlog across both
    /// primitives — the same operational signal it had before the split.
    /// Per-primitive depth can still be derived from the per-channel
    /// capacity constants and the `kind` label on `videocall_outbound_channel_drops_total`.
    fn start_heartbeat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(WT_HEARTBEAT_INTERVAL, |act, ctx| {
            // Per-primitive depths. Resolved capacity is memoised, so the
            // unistream call is a single pointer read after init; the
            // datagram capacity is a `const`.
            let uni_depth =
                wt_outbound_channel_capacity().saturating_sub(act.unistream_tx.capacity());
            let dgram_depth =
                WT_DATAGRAM_CHANNEL_CAPACITY.saturating_sub(act.datagram_tx.capacity());
            // Sum so the existing gauge label scheme is preserved end-to-end.
            let depth = uni_depth + dgram_depth;
            RELAY_OUTBOUND_QUEUE_DEPTH
                .with_label_values(&[&act.logic.room, "webtransport"])
                .set(depth as f64);

            // Check if connection is dead (channel closed)
            if act.is_connection_dead() {
                warn!(
                    "WebTransport connection dead (channel closed), stopping session {}",
                    act.logic.id
                );
                ctx.stop();
                return;
            }

            // Check heartbeat timeout
            if actix::clock::Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                warn!(
                    "WebTransport client heartbeat failed, disconnecting session {}",
                    act.logic.id
                );
                ctx.stop();
            }
        });
    }
}

// =============================================================================
// Actor Implementation
// =============================================================================

impl Actor for WtChatSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start
        self.logic.track_connection_start();

        // Start session via SessionManager
        let session_manager = self.logic.session_manager.clone();
        let room = self.logic.room.clone();
        let user_id = self.logic.user_id.clone();
        let session_id = self.logic.id;

        ctx.wait(
            async move {
                session_manager
                    .start_session(&room, &user_id, session_id)
                    .await
            }
            .into_actor(self)
            .map(|result, act, ctx| match result {
                Ok(result) => {
                    act.send(act.logic.build_session_assigned());
                    let bytes = act
                        .logic
                        .build_meeting_started(result.start_time_ms, &result.creator_id);
                    act.send(bytes);
                }
                Err(e) => {
                    error!("Failed to start session: {}", e);
                    let bytes = act
                        .logic
                        .build_meeting_ended(&format!("Session rejected: {e}"));
                    act.send(bytes);
                    ctx.stop();
                }
            }),
        );

        // Register with ChatServer
        let addr = ctx.address();
        self.logic
            .addr
            .send(self.logic.create_connect_message(addr.recipient()))
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("Failed to connect to ChatServer: {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);

        // Join room
        self.join_room(ctx);

        // Start heartbeat AFTER all initialization is complete to avoid
        // premature timeout if Connect/JoinRoom are slow under load.
        self.start_heartbeat(ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer.
///
/// Uses `send_auto` to route packets across two QUIC primitives:
///
/// * Datagrams — non-media control packets (heartbeats, RTT, diagnostics,
///   AES key exchange) **and** small audio media packets that fit within
///   the datagram MTU. Routing audio over datagrams avoids head-of-line
///   blocking on the shared per-receiver UniStream when a single UDP
///   segment is lost: the next audio frame still arrives on time even
///   though QUIC has not yet retransmitted the lost one.
/// * Reliable unidirectional streams — video/screen media (which would
///   show visual artifacts under loss) and any oversized audio/control
///   that exceeds the datagram MTU.
///
/// The outbound `msg.msg` is a serialized `PacketWrapper`. We parse it
/// once to extract the sender's `session_id` (for congestion tracking),
/// the `packet_type`, and — when MEDIA — the inner `MediaType`, so
/// `send_auto` does not need to re-parse anything.
///
/// Encrypted media payloads cannot be inspected for `MediaType`; in
/// that case `is_audio` falls back to `false` and the packet uses the
/// reliable stream — preserving today's behaviour for end-to-end
/// encrypted streams.
///
/// Note: `msg.session` is the **receiver's** session ID (set by
/// `chat_server::handle_msg`), NOT the sender's. The sender's session
/// ID lives inside the serialized `PacketWrapper.session_id` field.
impl Handler<Message> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);

        // Parse the PacketWrapper once to extract the sender's session_id,
        // user_id, and packet_type. This avoids a redundant parse in send_auto
        // and ensures congestion tracking targets the correct (sender) session.
        let parsed = PacketWrapper::parse_from_bytes(&msg.msg).ok();
        // Whether the outer `PacketWrapper` parsed at all. Threaded into
        // `send_auto` so the drop-counter `kind` label can fall back to
        // "unknown" on parse failure (issue #610) — matching the WS site.
        let parse_succeeded = parsed.is_some();
        let sender_session_id = parsed.as_ref().map(|pw| pw.session_id).unwrap_or(0);
        let sender_user_id = parsed
            .as_ref()
            .map(|pw| pw.user_id.clone())
            .unwrap_or_default();
        // Resolve the outer PacketType for the priority-drop classifier.
        // `enum_value().ok()` falls back to `PACKET_TYPE_UNKNOWN` when
        // the wire bytes carry a value not in our enum — Control class
        // under the priority policy, i.e. never preemptively dropped.
        let packet_type = parsed
            .as_ref()
            .and_then(|pw| pw.packet_type.enum_value().ok())
            .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN);
        let is_media = packet_type == PacketType::MEDIA;

        // For MEDIA packets, peek at the inner MediaType. We use the
        // resolved `MediaType` enum twice:
        //   * `is_audio` controls per-frame routing (audio uses datagrams).
        //   * `media_type` (Some/None) refines the drop-counter `kind`
        //     label into `audio`/`video`/`screen` so a storm can be
        //     attributed to a specific media stream. The 2026-05-08
        //     production storm dropped 25,081 packets in 3 minutes and
        //     we had no metric-level way to tell audio from video.
        //   * priority-drop classifier consumes both `packet_type` and
        //     `media_type` to decide whether to preempt the enqueue.
        //
        // Encrypted payloads fail to parse and therefore (a) route via
        // the reliable stream — the safer default — and (b) fall through
        // to the `media` catch-all label, preserving the legacy series.
        let inner_media_type = if is_media {
            parsed
                .as_ref()
                .and_then(|pw| MediaPacket::parse_from_bytes(&pw.data).ok())
                .and_then(|mp| mp.media_type.enum_value().ok())
        } else {
            None
        };
        let is_audio = matches!(inner_media_type, Some(MediaType::AUDIO));

        match self.send_auto(
            bytes,
            is_media,
            is_audio,
            parse_succeeded,
            packet_type,
            inner_media_type,
        ) {
            WtSendResult::Sent => {}
            WtSendResult::Dead => {
                ctx.stop();
            }
            WtSendResult::Dropped | WtSendResult::PriorityDropped => {
                // Either real channel-full or priority preempt — both
                // are drops from the sender's perspective. Record the
                // drop for the actual sender so we can fire CONGESTION
                // feedback when the threshold is exceeded. This is what
                // tells the offending sender to step its quality tier
                // down; without it, the sender keeps sending the same
                // volume of video and the receiver keeps shedding it.
                if sender_session_id != 0 {
                    self.logic
                        .on_outbound_drop(sender_session_id, &sender_user_id);
                }
            }
        }
    }
}

/// Handle inbound data from WebTransport session
impl Handler<WtInbound> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: WtInbound, ctx: &mut Self::Context) -> Self::Result {
        // Update heartbeat
        self.heartbeat = actix::clock::Instant::now();

        // Handle keep-alive ping (WebTransport-specific)
        if msg.source == WtInboundSource::Datagram && msg.data.as_ref() == KEEP_ALIVE_PING {
            trace!("Received keep-alive ping for session {}", self.logic.id);
            return;
        }

        let action = self.logic.handle_inbound(&msg.data);

        if !self.activated && SessionLogic::should_activate_on_action(&action) {
            self.logic.addr.do_send(ActivateConnection {
                session: self.logic.id,
            });
            self.activated = true;
            info!(
                "Session {} activated on first non-RTT packet",
                self.logic.id
            );
        }

        match action {
            InboundAction::Echo(data) => {
                // RTT echo is routed onto the same primitive it arrived on
                // so the round-trip measurement reflects that primitive's
                // path — a UniStream probe measures stream RTT (which can
                // be inflated by flow-control stalls), a Datagram probe
                // measures the unframed datagram RTT.
                let echo_bytes = Bytes::from(data.as_ref().clone());
                let try_send_result = match msg.source {
                    WtInboundSource::UniStream => self.unistream_tx.try_send(echo_bytes),
                    WtInboundSource::Datagram => self.datagram_tx.try_send(echo_bytes),
                };
                match try_send_result {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        warn!(
                            "Outbound channel closed while echoing RTT for session {}",
                            self.logic.id
                        );
                        ctx.stop();
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        RELAY_PACKET_DROPS_TOTAL
                            .with_label_values(&[&self.logic.room, "webtransport", "channel_full"])
                            .inc();
                        // Phase 8b TELEM-8: protocol-wide aggregate. RTT echo
                        // gets its own `kind` so we can distinguish probe
                        // congestion (early signal) from media congestion
                        // (already-degraded call) in the alerting layer.
                        OUTBOUND_CHANNEL_DROPS_TOTAL
                            .with_label_values(&["webtransport", "rtt"])
                            .inc();
                        error!(
                            "Outbound channel full, dropping RTT echo for session {}",
                            self.logic.id
                        );
                    }
                }
            }
            InboundAction::Forward(data) => {
                ctx.notify(Packet { data });
            }
            InboundAction::Processed | InboundAction::KeepAlive => {}
        }
    }
}

/// Handle stop signal
impl Handler<StopSession> for WtChatSession {
    type Result = ();

    fn handle(&mut self, _msg: StopSession, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Received stop signal for WebTransport session {} in room {}",
            self.logic.id, self.logic.room
        );
        ctx.stop();
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WtChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        trace!(
            "Forwarding packet to ChatServer: session {} room {}",
            self.logic.id,
            self.logic.room
        );
        self.logic
            .addr
            .do_send(self.logic.create_client_message(msg));
    }
}

// =============================================================================
// Helper Methods
// =============================================================================

impl WtChatSession {
    fn join_room(&self, ctx: &mut Context<Self>) {
        let join_room = self.logic.addr.send(self.logic.create_join_room_message());
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                if act.logic.handle_join_room_result(response) {
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: construct an `is_audio` test packet of the requested size.
    /// Size is the *outbound bytes* length passed into `build_outbound`.
    fn audio_bytes(size: usize) -> Vec<u8> {
        vec![0xAA; size]
    }

    /// Helper: construct a `is_video / control` test packet of the requested size.
    fn other_bytes(size: usize) -> Vec<u8> {
        vec![0xBB; size]
    }

    fn is_datagram(o: &WtOutbound) -> bool {
        matches!(o, WtOutbound::Datagram(_))
    }

    fn is_unistream(o: &WtOutbound) -> bool {
        matches!(o, WtOutbound::UniStream(_))
    }

    // -----------------------------------------------------------------------
    // Sub-change B: audio-via-datagram routing
    // -----------------------------------------------------------------------

    #[test]
    fn audio_media_under_mtu_routes_via_datagram() {
        // Opus frames are typically 50-200B; 100B sits comfortably below the
        // ~1200B datagram MTU, so audio should now travel by datagram.
        let out = build_outbound(
            audio_bytes(100),
            /*is_media=*/ true,
            /*is_audio=*/ true,
        );
        assert!(
            is_datagram(&out),
            "small audio should route to datagram, got {:?}",
            out
        );
    }

    #[test]
    fn audio_media_at_mtu_routes_via_datagram() {
        // Boundary case: payload exactly at the MTU still uses datagram.
        let out = build_outbound(
            audio_bytes(DATAGRAM_MAX_SIZE),
            /*is_media=*/ true,
            /*is_audio=*/ true,
        );
        assert!(
            is_datagram(&out),
            "audio at MTU boundary should still use datagram, got {:?}",
            out
        );
    }

    #[test]
    fn audio_media_over_mtu_falls_back_to_unistream() {
        // Oversized audio (rare — e.g. concatenated frames) must use the
        // reliable stream because datagrams above MTU would be rejected
        // by the QUIC layer.
        let out = build_outbound(
            audio_bytes(1500),
            /*is_media=*/ true,
            /*is_audio=*/ true,
        );
        assert!(
            is_unistream(&out),
            "oversized audio must fall back to UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn video_media_under_mtu_still_routes_via_unistream() {
        // Even tiny video media (e.g. KEYFRAME_REQUEST replays) keep the
        // reliable stream — we do not want any per-frame loss for video.
        let out = build_outbound(
            other_bytes(100),
            /*is_media=*/ true,
            /*is_audio=*/ false,
        );
        assert!(
            is_unistream(&out),
            "video media under MTU must still use UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn video_media_large_routes_via_unistream() {
        // A representative 50KB video packet (e.g. an I-frame fragment).
        let out = build_outbound(
            other_bytes(50_000),
            /*is_media=*/ true,
            /*is_audio=*/ false,
        );
        assert!(
            is_unistream(&out),
            "video media must route via UniStream, got {:?}",
            out
        );
    }

    // -----------------------------------------------------------------------
    // Existing-behaviour preservation
    // -----------------------------------------------------------------------

    #[test]
    fn small_control_packet_still_routes_via_datagram() {
        // Control / non-media packets ≤ MTU keep their existing datagram path.
        let out = build_outbound(
            other_bytes(100),
            /*is_media=*/ false,
            /*is_audio=*/ false,
        );
        assert!(
            is_datagram(&out),
            "small control packet must still use datagram, got {:?}",
            out
        );
    }

    #[test]
    fn oversized_control_packet_routes_via_unistream() {
        let out = build_outbound(
            other_bytes(DATAGRAM_MAX_SIZE + 1),
            /*is_media=*/ false,
            /*is_audio=*/ false,
        );
        assert!(
            is_unistream(&out),
            "oversized control packet must use UniStream, got {:?}",
            out
        );
    }

    #[test]
    fn defensive_is_audio_without_is_media_still_routes_as_control() {
        // If a caller incorrectly sets is_audio=true while is_media=false,
        // we treat it as a regular non-media control packet (datagram if
        // small, stream otherwise). is_audio is meaningful only for media.
        let out = build_outbound(
            audio_bytes(100),
            /*is_media=*/ false,
            /*is_audio=*/ true,
        );
        assert!(
            is_datagram(&out),
            "is_audio without is_media should fall through to control routing, got {:?}",
            out
        );
    }

    // -----------------------------------------------------------------------
    // Issue #610: WT outbound-drop label parity with WS
    //
    // The WS site emits `kind="unknown"` when the upstream PacketWrapper
    // parse fails. WT used to hard-code `kind="control"` for the same
    // path because it lost the parse-success signal before reaching the
    // drop counter. These tests lock in the new tri-state mapping so a
    // future revert to the old `if is_media { "media" } else { "control" }`
    // branch fails CI.
    //
    // 2026-05-08 audio-quality follow-up: extended the helper to refine
    // the `media` bucket into `audio`/`video`/`screen` based on the
    // inner `MediaPacket.media_type`. Tests in the next block lock in
    // that mapping.
    // -----------------------------------------------------------------------

    #[test]
    fn drop_kind_unknown_when_parse_failed() {
        // Issue #610: when the outer PacketWrapper parse failed upstream,
        // we cannot trust `is_media`, so the counter must record this
        // drop as `kind="unknown"` to match the WS site.
        assert_eq!(
            drop_kind_label(/*parsed=*/ false, /*is_media=*/ false, None),
            "unknown",
            "parse-fail must map to `unknown` regardless of is_media"
        );
        // Even if a stale `is_media=true` and a stale `media_type` somehow
        // propagate, parse-fail wins — we never want to attribute a
        // malformed packet to a media kind we did not actually classify.
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ false,
                /*is_media=*/ true,
                Some(MediaType::AUDIO),
            ),
            "unknown",
            "parse-fail must override stale is_media + media_type"
        );
    }

    #[test]
    fn drop_kind_media_when_parsed_and_is_media_no_inner_type() {
        // Backwards-compat: when the inner MediaPacket couldn't be
        // classified (encrypted payload, parse failure, future MediaType
        // not in our enum), fall back to the legacy `media` bucket so
        // existing alerts pivoting on `kind="media"` still see a series.
        assert_eq!(
            drop_kind_label(/*parsed=*/ true, /*is_media=*/ true, None,),
            "media",
        );
    }

    #[test]
    fn drop_kind_control_when_parsed_and_not_media() {
        // media_type is meaningful only for media packets. Even if a
        // caller incorrectly threads a `Some(...)` while `is_media=false`,
        // the label MUST stay `control` — `is_media` is the gate.
        assert_eq!(
            drop_kind_label(/*parsed=*/ true, /*is_media=*/ false, None,),
            "control",
        );
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ false,
                Some(MediaType::AUDIO),
            ),
            "control",
            "is_media=false must map to control even with a Some(MediaType)"
        );
    }

    #[test]
    fn drop_kind_audio_when_inner_is_audio() {
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ true,
                Some(MediaType::AUDIO),
            ),
            "audio",
        );
    }

    #[test]
    fn drop_kind_video_when_inner_is_video() {
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ true,
                Some(MediaType::VIDEO),
            ),
            "video",
        );
    }

    #[test]
    fn drop_kind_screen_when_inner_is_screen() {
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ true,
                Some(MediaType::SCREEN),
            ),
            "screen",
        );
    }

    #[test]
    fn drop_kind_falls_back_to_media_for_uncommon_media_types() {
        // HEARTBEAT and KEYFRAME_REQUEST are MEDIA packet types that
        // are NOT audio/video/screen. They should stay in the legacy
        // `media` bucket so we don't pollute the new fine-grained
        // labels with bookkeeping traffic.
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ true,
                Some(MediaType::HEARTBEAT),
            ),
            "media",
            "HEARTBEAT must fall through to the legacy `media` catch-all"
        );
        assert_eq!(
            drop_kind_label(
                /*parsed=*/ true,
                /*is_media=*/ true,
                Some(MediaType::KEYFRAME_REQUEST),
            ),
            "media",
            "KEYFRAME_REQUEST must fall through to the legacy `media` catch-all"
        );
    }

    #[test]
    fn drop_kind_label_emits_only_documented_values() {
        // Guard against typos / future drift: the only kinds this mapping
        // ever returns are the six documented in metrics.rs
        // (`audio`, `video`, `screen`, `media`, `control`, `unknown`). The
        // seventh documented value, `rtt`, is emitted by the inbound-echo
        // path, not by this helper.
        let media_types = [
            None,
            Some(MediaType::AUDIO),
            Some(MediaType::VIDEO),
            Some(MediaType::SCREEN),
            Some(MediaType::HEARTBEAT),
            Some(MediaType::KEYFRAME_REQUEST),
        ];
        for parsed in [false, true] {
            for is_media in [false, true] {
                for mt in media_types {
                    let kind = drop_kind_label(parsed, is_media, mt);
                    assert!(
                        matches!(
                            kind,
                            "audio" | "video" | "screen" | "media" | "control" | "unknown"
                        ),
                        "drop_kind_label returned unexpected kind={kind} for \
                         (parsed={parsed}, is_media={is_media}, media_type={mt:?})"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2 split-channel invariant (discussion #756)
    //
    // The architectural fix is that the unistream and datagram channels are
    // independent: a saturated unistream channel — typical when QUIC flow
    // control stalls on a slow receiver — must not block the datagram
    // channel from accepting new audio frames. These tests lock in that
    // invariant at the `mpsc::channel` level. The bridge-level invariant
    // (a stalled `write_all` on the persistent stream does not park the
    // datagram writer task) is enforced by the structural split in
    // `bridge.rs::spawn_unistream_writer` / `spawn_datagram_writer` —
    // those run in independent `tokio::spawn` tasks and never share state.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn split_channels_datagrams_survive_unistream_saturation() {
        // The whole point of the channel split: if the unistream channel
        // were a shared queue with datagrams (the pre-Phase-2 topology),
        // saturating it would also drop audio. Under the split,
        // saturating the unistream channel has zero effect on the
        // datagram channel.
        const UNI_CAP: usize = 64;
        const DGRAM_CAP: usize = 16;

        let (uni_tx, _uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);
        let (dgram_tx, mut dgram_rx) = mpsc::channel::<Bytes>(DGRAM_CAP);

        // Saturate the unistream channel completely — we never drain it,
        // simulating a writer task parked on `stream.write_all().await`.
        for i in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0xBB; 100]))
                .unwrap_or_else(|_| panic!("uni slot {i} must accept"));
        }
        assert_eq!(
            uni_tx.capacity(),
            0,
            "unistream channel must be exactly full before the test runs"
        );

        // The next unistream send must fail (channel full) — proves we
        // really did saturate it.
        match uni_tx.try_send(Bytes::from(vec![0xBB; 100])) {
            Err(mpsc::error::TrySendError::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }

        // Now push DGRAM_CAP audio packets onto the datagram channel —
        // every one must succeed because the channels are independent.
        for i in 0..DGRAM_CAP {
            dgram_tx
                .try_send(Bytes::from(vec![0xAA; 80]))
                .unwrap_or_else(|_| {
                    panic!("datagram slot {i} must accept while unistream is full")
                });
        }

        // Drain the datagram receiver and confirm we received exactly
        // DGRAM_CAP packets — none were silently dropped or blocked.
        let mut received = 0usize;
        while dgram_rx.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(
            received, DGRAM_CAP,
            "datagram channel must deliver every packet pushed while unistream is saturated",
        );
    }

    #[tokio::test]
    async fn split_channels_unistream_back_to_admit_after_drain() {
        // Sanity check that the unistream channel itself recovers normally
        // after being drained — proves the split doesn't introduce any
        // accidental persistence of the saturated state.
        const UNI_CAP: usize = 8;
        let (uni_tx, mut uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);

        for _ in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0; 1]))
                .expect("saturating sends must succeed");
        }
        assert!(uni_tx.try_send(Bytes::from(vec![0; 1])).is_err());

        // Drain everything.
        while uni_rx.try_recv().is_ok() {}

        // After drain we can send up to UNI_CAP again.
        for _ in 0..UNI_CAP {
            uni_tx
                .try_send(Bytes::from(vec![0; 1]))
                .expect("post-drain sends must succeed");
        }
    }

    /// Integration-style verification of the split-writer flow.
    ///
    /// We do not build a real `WebTransportBridge` (that requires a real
    /// `quinn::Connection`), but we *do* mirror the bridge's per-primitive
    /// writer-task topology: spawn one task that drains the unistream
    /// receiver into a stub stream (which artificially blocks, mimicking
    /// QUIC flow-control stall), and another that drains the datagram
    /// receiver into a counter. The test then pushes N video packets onto
    /// the unistream channel and M audio packets onto the datagram
    /// channel concurrently, and asserts that all M datagrams are
    /// delivered even while the unistream writer is parked on the stub
    /// stream's blocking write.
    #[tokio::test]
    async fn split_writer_topology_datagrams_unblocked_by_stream_stall() {
        use std::sync::Arc;
        use tokio::sync::Notify;

        const UNI_CAP: usize = 64;
        const DGRAM_CAP: usize = 32;
        const M_AUDIO: usize = 30;

        let (uni_tx, mut uni_rx) = mpsc::channel::<Bytes>(UNI_CAP);
        let (dgram_tx, mut dgram_rx) = mpsc::channel::<Bytes>(DGRAM_CAP);

        // Stub "stream" — a writer task that consumes from `uni_rx` but
        // blocks forever on a single `Notify::notified()` after the first
        // message. This mimics a real `stream.write_all().await` that has
        // stalled on QUIC flow-control credit exhaustion. Critically, this
        // task never yields — once parked, it does NOT come back to drain
        // more packets. That is the production failure mode the split is
        // designed to survive.
        let stall = Arc::new(Notify::new());
        let stall_writer = stall.clone();
        let unistream_writer = tokio::spawn(async move {
            // Consume the first message to prove the writer was alive,
            // then park indefinitely.
            let _ = uni_rx.recv().await;
            stall_writer.notified().await;
            // After notify, drain remaining (used only by the test
            // teardown so the channel close is observed cleanly).
            while uni_rx.recv().await.is_some() {}
        });

        // Datagram writer — fully independent task that pulls from
        // `dgram_rx` and forwards each payload to a shared counter.
        // Mirrors `spawn_datagram_writer` (no blocking, no shared state
        // with the unistream writer).
        let delivered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let delivered_writer = delivered.clone();
        let datagram_writer = tokio::spawn(async move {
            while let Some(_packet) = dgram_rx.recv().await {
                delivered_writer.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });

        // Push N video packets onto the unistream channel (some will
        // saturate after the writer parks). The exact count doesn't
        // matter; only that the writer becomes blocked.
        for _ in 0..UNI_CAP {
            // Use try_send so the test never deadlocks on a real `send`
            // awaiting capacity. We expect some sends to fail once the
            // writer is parked and the channel fills up — that's the
            // failure mode we tolerate.
            let _ = uni_tx.try_send(Bytes::from(vec![0xBB; 1024]));
        }

        // Push M audio packets onto the *datagram* channel. The whole
        // point: every one of these must be delivered to the datagram
        // writer's counter, even while the unistream writer is parked.
        for i in 0..M_AUDIO {
            // `send` (not `try_send`) — datagram channel has capacity
            // DGRAM_CAP and the writer drains promptly, so this will not
            // block on a healthy split. If split independence is broken
            // we'll deadlock and fail the test's outer timeout.
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                dgram_tx.send(Bytes::from(vec![0xAA; 80])).await
            })
            .await
            .unwrap_or_else(|_| panic!("audio packet {i} blocked — split-channel invariant broken"))
            .expect("datagram channel must remain open");
        }

        // Give the datagram writer a moment to drain.
        for _ in 0..50 {
            if delivered.load(std::sync::atomic::Ordering::SeqCst) >= M_AUDIO {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            delivered.load(std::sync::atomic::Ordering::SeqCst),
            M_AUDIO,
            "every audio datagram must be delivered while the unistream writer is parked",
        );

        // Clean teardown: release the parked unistream writer and drop
        // the senders so both writer tasks exit.
        stall.notify_one();
        drop(uni_tx);
        drop(dgram_tx);
        let _ = unistream_writer.await;
        let _ = datagram_writer.await;
    }
}
