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
use crate::constants::{wt_outbound_channel_capacity, CLIENT_TIMEOUT};
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

/// Outbound message with transport type specification
#[derive(Debug, Clone)]
pub enum WtOutbound {
    /// Send via UniStream (reliable, ordered)
    UniStream(Bytes),
    /// Send via Datagram (unreliable, unordered, low latency)
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
pub struct WtChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: actix::clock::Instant,

    /// Channel to send data back to WebTransport session
    outbound_tx: mpsc::Sender<WtOutbound>,

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
        outbound_tx: mpsc::Sender<WtOutbound>,
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
            outbound_tx,
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
        match self
            .outbound_tx
            .try_send(WtOutbound::UniStream(data.into()))
        {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(
                    "Outbound channel closed for session {}, connection dead",
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
                    "Outbound channel full for session {} on Critical control packet, dropping (overflow_critical)",
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
        // Priority-drop pre-check: before paying the `try_send` cost we
        // ask the per-session policy whether this packet should be
        // preempted given the current channel fill. Video / screen are
        // shed at ~80% fill, audio at ~95% fill, control / critical
        // never preempt. See `actors::priority_drop` for the policy.
        //
        // Lifecycle note: this branch fires PURELY based on channel
        // depth — it does NOT distinguish reconnection storms from
        // steady-state saturation. The Critical packet set
        // (SESSION_ASSIGNED, CONGESTION, RSA_PUB_KEY, MEETING) is
        // *never* preempted here, so a reconnection wave that needs
        // to deliver SESSION_ASSIGNED + MEETING_STARTED still goes
        // through. Media drops during the storm are exactly the
        // intended behaviour — they free queue slots for the lifecycle
        // traffic that the new participant actually needs.
        let priority = OutboundPriority::classify(parsed, packet_type, media_type);
        let total_capacity = wt_outbound_channel_capacity();
        let free_capacity = self.outbound_tx.capacity();
        if let PriorityDropDecision::Drop { reason } =
            evaluate_priority_drop(priority, free_capacity, total_capacity)
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
                "Priority-drop {reason} on WT session {}: free={free_capacity}/{total_capacity}",
                self.logic.id,
            );
            return WtSendResult::PriorityDropped;
        }

        let outbound = build_outbound(data, is_media, is_audio);

        match self.outbound_tx.try_send(outbound) {
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

    /// Check if the outbound channel is closed
    fn is_connection_dead(&self) -> bool {
        self.outbound_tx.is_closed()
    }

    /// Start heartbeat check (WebTransport-specific timing)
    fn start_heartbeat(&self, ctx: &mut Context<Self>) {
        ctx.run_interval(WT_HEARTBEAT_INTERVAL, |act, ctx| {
            // `depth = total_capacity - free_capacity`. Resolved capacity is
            // memoised, so the call is a single pointer read after init.
            let depth = wt_outbound_channel_capacity().saturating_sub(act.outbound_tx.capacity());
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
                let outbound = match msg.source {
                    WtInboundSource::UniStream => {
                        WtOutbound::UniStream(Bytes::from(data.as_ref().clone()))
                    }
                    WtInboundSource::Datagram => {
                        WtOutbound::Datagram(Bytes::from(data.as_ref().clone()))
                    }
                };
                match self.outbound_tx.try_send(outbound) {
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
}
