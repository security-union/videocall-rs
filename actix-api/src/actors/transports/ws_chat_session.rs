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

//! WebSocket Chat Session Actor
//!
//! This is a thin transport adapter that delegates all business logic
//! to `SessionLogic`. It handles WebSocket-specific I/O via `WebsocketContext`.

use crate::actors::chat_server::ChatServer;
use crate::actors::priority_drop::{
    evaluate_dual as evaluate_priority_drop_dual, OutboundPriority, PriorityDropDecision,
    QueueByteMeter,
};
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::{
    ws_mailbox_capacity, CLIENT_TIMEOUT, FRAGMENT_ASSEMBLY_IDLE_TIMEOUT,
    FRAGMENT_ASSEMBLY_MAX_LIFETIME, HEARTBEAT_INTERVAL, MAX_FRAME_SIZE,
    WS_OUTBOUND_CHANNEL_CAPACITY, WS_OUTBOUND_SCREEN_BYTE_BUDGET, WS_OUTBOUND_VIDEO_BYTE_BUDGET,
};
use crate::messages::server::{ActivateConnection, Packet};
use crate::messages::session::Message;
use crate::metrics::{
    OUTBOUND_CHANNEL_DROPS_TOTAL, RELAY_PACKET_DROPS_TOTAL, WS_FRAGMENTED_INBOUND_TOTAL,
    WS_FRAGMENT_DISCARDED_TOTAL,
};
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, Actor, ActorContext, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{self, WebsocketContext};
use bytes::BytesMut;
use protobuf::Message as ProtobufMessage;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, trace};
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{RoomId, SessionId, UserId};

/// Classify a dropped outbound packet for the
/// `videocall_outbound_channel_drops_total{kind=...}` label.
///
/// Mirrors the WT helper at `wt_chat_session::drop_kind_label`. Refining
/// the legacy `media` bucket into `audio`/`video`/`screen` lets operators
/// attribute a congestion storm to a specific media stream — the
/// 2026-05-08 production storm dropped 25,081 packets in 3 minutes and
/// the metric had no way to tell audio from video.
///
/// * `parsed=false` → `"unknown"` — outer parse failed.
/// * `parsed=true && !is_media` → `"control"`.
/// * `parsed=true && is_media && Some(AUDIO)`  → `"audio"`.
/// * `parsed=true && is_media && Some(VIDEO)`  → `"video"`.
/// * `parsed=true && is_media && Some(SCREEN)` → `"screen"`.
/// * `parsed=true && is_media && anything else (HEARTBEAT, KEYFRAME_REQUEST,
///   encrypted/unparseable inner)` → `"media"` — the legacy catch-all so
///   existing alerts pivoting on `kind="media"` still see a series.
// `pub(crate)` so the metric-taxonomy coverage guard
// (`metrics::tests::relay_drop_kinds_covers_all_emitted_drop_labels`) can
// enumerate this emit site's output directly instead of against a hand-copied
// literal list (issue #1186). Kept in lock-step with the `wt_chat_session`
// copy — that test asserts both copies agree.
pub(crate) fn drop_kind_label(
    parsed: bool,
    is_media: bool,
    media_type: Option<MediaType>,
) -> &'static str {
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

/// One queued outbound packet. The priority rides WITH the payload, so the
/// drain knows which [`QueueByteMeter`] bucket to credit — no parallel
/// structure exists to fall out of step (#2261).
pub(crate) struct OutboundFrame {
    priority: OutboundPriority,
    bytes: Vec<u8>,
}

/// `0` disables the byte dimension: audio costs slots, not bytes (#2261).
pub(crate) fn ws_byte_budget_for(priority: OutboundPriority) -> usize {
    match priority {
        OutboundPriority::Video => WS_OUTBOUND_VIDEO_BYTE_BUDGET,
        OutboundPriority::Screen => WS_OUTBOUND_SCREEN_BYTE_BUDGET,
        OutboundPriority::Audio | OutboundPriority::Critical | OutboundPriority::Control => 0,
    }
}

/// The WS queue's slot and byte bounds, bound here so they are testable.
pub(crate) fn ws_outbound_decision(
    priority: OutboundPriority,
    free_capacity: usize,
    queued: &QueueByteMeter,
) -> PriorityDropDecision {
    evaluate_priority_drop_dual(
        priority,
        free_capacity,
        WS_OUTBOUND_CHANNEL_CAPACITY,
        queued.queued_for(priority),
        ws_byte_budget_for(priority),
    )
}

/// WebSocket Chat Session Actor
///
/// A thin transport adapter that delegates business logic to `SessionLogic`.
/// Handles WebSocket-specific I/O via `WebsocketContext`.
pub struct WsChatSession {
    /// Shared session logic (business logic)
    logic: SessionLogic,

    /// Heartbeat tracking (transport-specific timing)
    heartbeat: Instant,

    /// Track if ActivateConnection has been sent
    activated: bool,

    /// Bounded outbound channel sender — packets are enqueued here and
    /// drained by a `StreamHandler<Vec<u8>>` registered in `started()`.
    /// When the channel is full, `on_outbound_drop()` records the drop.
    outbound_tx: mpsc::Sender<OutboundFrame>,

    /// Receiver half, consumed once by `started()` via `ctx.add_stream()`.
    outbound_rx: Option<ReceiverStream<OutboundFrame>>,

    /// Live byte occupancy of `outbound_tx`; both mutators take `&mut self`.
    outbound_bytes: QueueByteMeter,

    /// In-flight fragmented inbound message (#2600).
    fragment: FragmentBuffer,
}

/// Length- and time-bounded accumulator for a fragmented inbound WS message (#2600).
#[derive(Default)]
pub(crate) struct FragmentBuffer {
    buf: Option<BytesMut>,
    /// Set once by `begin`; never refreshed, so it bounds a deliberate holder.
    opened: Option<Instant>,
    /// Refreshed by every accepted `extend`, so a slow uplink survives.
    last_active: Option<Instant>,
}

impl FragmentBuffer {
    /// `true` when a partial was actually discarded, so the caller can count it.
    fn reset(&mut self) -> bool {
        self.opened = None;
        self.last_active = None;
        self.buf.take().is_some()
    }

    fn begin(&mut self, bytes: &[u8], now: Instant) {
        self.buf = Some(BytesMut::from(bytes));
        self.opened = Some(now);
        self.last_active = Some(now);
    }

    /// `false` only when the LIMIT refused the append. Refreshes the idle clock but NOT
    /// [`FragmentBuffer::opened`], so an empty-`Continue` trickle cannot extend the lifetime.
    fn extend(&mut self, bytes: &[u8], limit: usize, now: Instant) -> bool {
        let Some(buf) = self.buf.as_mut() else {
            return true;
        };
        if buf.len().saturating_add(bytes.len()) > limit {
            self.reset();
            return false;
        }
        buf.extend_from_slice(bytes);
        self.last_active = Some(now);
        true
    }

    fn take(&mut self) -> Option<BytesMut> {
        self.opened = None;
        self.last_active = None;
        self.buf.take()
    }

    /// Reclaims on EITHER bound; only the lifetime resists a client refreshing the idle clock.
    fn reap_if_stale(&mut self, now: Instant) -> Option<usize> {
        let opened = self.opened?;
        let idle_from = self.last_active.unwrap_or(opened);
        if now.duration_since(idle_from) <= FRAGMENT_ASSEMBLY_IDLE_TIMEOUT
            && now.duration_since(opened) <= FRAGMENT_ASSEMBLY_MAX_LIFETIME
        {
            return None;
        }
        let n = self.buf.as_ref().map(|b| b.len()).unwrap_or(0);
        self.reset();
        Some(n)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buf.as_ref().map(|b| b.len()).unwrap_or(0)
    }
    #[cfg(test)]
    fn is_open(&self) -> bool {
        self.buf.is_some()
    }
}

impl WsChatSession {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        user_id: String,
        display_name: String,
        is_guest: bool,
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
            "websocket",
            is_host,
            end_on_host_leave,
        );

        let (outbound_tx, outbound_rx) =
            mpsc::channel::<OutboundFrame>(WS_OUTBOUND_CHANNEL_CAPACITY);

        WsChatSession {
            logic,
            heartbeat: Instant::now(),
            activated: false,
            outbound_tx,
            outbound_rx: Some(ReceiverStream::new(outbound_rx)),
            outbound_bytes: QueueByteMeter::default(),
            fragment: FragmentBuffer::default(),
        }
    }

    /// Start heartbeat check (WebSocket-specific: uses ping frames)
    fn start_heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            // Sample outbound queue depth/bytes for Prometheus
            let depth = WS_OUTBOUND_CHANNEL_CAPACITY - act.outbound_tx.capacity();
            crate::metrics::record_ws_outbound_queue_sample(
                &act.logic.room,
                &act.logic.id.to_string(),
                depth,
                &act.outbound_bytes,
            );

            if let Some(n) = act.fragment.reap_if_stale(Instant::now()) {
                WS_FRAGMENT_DISCARDED_TOTAL
                    .with_label_values(&["abandoned"])
                    .inc();
                debug!(
                    "Abandoned fragment sequence ({n} B) reclaimed on session {}",
                    act.logic.id
                );
            }

            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                error!("WebSocket client heartbeat failed, disconnecting!");
                ctx.stop();
                return;
            }
            ctx.ping(b"");
        });
    }
}

// =============================================================================
// Actor Implementation
// =============================================================================

impl Actor for WsChatSession {
    type Context = WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        crate::metrics::init_ws_fragment_discard_series();

        // Relocate the overflow point off the tiny default actor mailbox
        // onto the policy-aware bounded outbound channel (issue #1057).
        //
        // `WebsocketContext`'s mailbox defaults to actix `DEFAULT_CAPACITY`
        // (16). That mailbox sits *in front* of `outbound_tx` in the relay
        // fan-out path: ChatServer does `recipient.try_send(Message)` (a
        // mailbox enqueue), then `Handler<Message>` enqueues into the
        // bounded `outbound_tx`. Under a bursty fan-out storm (keyframe /
        // join / screen-share spikes) the 16-slot mailbox overflows long
        // before `outbound_tx` does — and that mailbox is a *dumb* queue:
        // it drops indiscriminately (audio, control, video alike) and cannot
        // feed the drop tracker.
        // The observed symptom is room-wide video freezes (276 `mailbox_full`
        // drops in one meeting while `relay_outbound_queue_depth` stayed 0,
        // proving the smart channel never filled).
        //
        // Sizing the mailbox AT the outbound channel capacity (issue #1057)
        // relocates a *steady-state* overflow onto `outbound_tx`, which is
        // policy-aware: it sheds camera VIDEO first (~80%), then SCREEN (~90%,
        // issue 1977), protects AUDIO to ~95%, never preempts
        // CONTROL/CONGESTION/MEETING, and records drops via `on_outbound_drop`.
        // So a genuine overflow becomes camera-first + screen-and-audio-protected
        // instead of a total stall.
        //
        // BUT a publisher-join fan-out BURST (issue #1144) overflows even a
        // mailbox sized AT the channel: #1144 saw 303 `mailbox_full` drops in
        // one second on a build that already had #1057's mailbox=128, because
        // the keyframe/join spike arrives in a tight sub-second window before
        // the actor is next scheduled to drain. So we add a modest
        // `INBOUND_MAILBOX_HEADROOM_FACTOR` (2×) of slack: enough to absorb a
        // single join wave across one scheduling gap and let it SPILL onto the
        // policy-aware `outbound_tx` (the shedding surface) rather than being
        // dropped indiscriminately at the dumb mailbox. The mailbox→channel
        // hand-off in `Handler<Message>` is CPU-bound (it does NOT block on the
        // socket write), so the actor drains this slack quickly; the headroom
        // is burst-absorption, NOT a deep buffer for a slow receiver (the
        // outbound channel's byte budgets still enforce fail-fast video
        // staleness bounds). The argument is the shared `ws_mailbox_capacity()`
        // binding (issue #1062) — the SINGLE source of truth that the guard
        // test also asserts, so editing the value here is tracked by the test
        // (the prior duplicated `WS_MAILBOX_CAPACITY` test constant could drift
        // from this call site silently).
        ctx.set_mailbox_capacity(ws_mailbox_capacity());

        // Register the outbound drain stream. Packets enqueued via
        // outbound_tx are pulled here and written as WS binary frames.
        if let Some(rx_stream) = self.outbound_rx.take() {
            ctx.add_stream(rx_stream);
        }

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
                    ctx.binary(act.logic.build_session_assigned());
                    let bytes = act
                        .logic
                        .build_meeting_started(result.start_time_ms, &result.creator_id);
                    ctx.binary(bytes);
                }
                Err(e) => {
                    error!("Failed to start session: {}", e);
                    let bytes = act
                        .logic
                        .build_meeting_ended(&format!("Session rejected: {e}"));
                    ctx.binary(bytes);
                    ctx.close(Some(ws::CloseReason {
                        code: ws::CloseCode::Policy,
                        description: Some("Session rejected".to_string()),
                    }));
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
        if self.fragment.reset() {
            WS_FRAGMENT_DISCARDED_TOTAL
                .with_label_values(&["abandoned"])
                .inc();
        }
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer.
///
/// Enqueues serialized bytes into the bounded `outbound_tx` channel instead
/// of calling `ctx.binary()` directly. The `StreamHandler<Vec<u8>>` drains
/// the channel on the actor event loop. When the channel is full, the packet
/// is dropped and `on_outbound_drop()` records the drop — mirroring the
/// WebTransport relay pattern.
///
/// **Priority-drop policy (discussion #699)**: before `try_send`, the
/// per-session `actors::priority_drop` evaluator decides whether to
/// preempt the enqueue based on packet priority and channel fill:
///
/// * Camera VIDEO frames are shed first at ~80% channel fill, then SCREEN
///   frames at ~90% (issue 1977: screen outranks cameras), so audio gets the
///   headroom (one 1-2 Mbps video frame buffer is worth ~200 audio frames at
///   ~50 kbps).
/// * Audio frames preserved until ~95% fill.
/// * Control packets are never preempted by the policy. Critical
///   lifecycle packets (`SESSION_ASSIGNED`, `CONGESTION`,
///   `RSA_PUB_KEY`, `MEETING`) also use the `overflow_critical` kind
///   label when they fail on real channel overflow, so a saturation
///   severe enough to drop lifecycle traffic is alertable on its own.
///
/// On any drop (preempted or real overflow), `on_outbound_drop` still records
/// the drop for metrics and the #979 keyframe-relax path.
impl Handler<Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        // Lazily compute the parsed metadata for the priority-drop
        // classifier. We parse the outer wrapper unconditionally
        // because the policy needs the `packet_type`; the inner
        // `MediaPacket` parse only happens for MEDIA packets. The hot
        // path is media (~99%), so the inner parse cost would be
        // paid almost every call regardless of the saturation state.
        //
        // We pull out sender_session_id / user_id here so a drop can
        // still feed `on_outbound_drop`.
        let parsed = PacketWrapper::parse_from_bytes(&msg.msg).ok();
        let parse_succeeded = parsed.is_some();
        let sender_session_id = parsed.as_ref().map(|pw| pw.session_id).unwrap_or(0);
        let sender_user_id = parsed
            .as_ref()
            .map(|pw| pw.user_id.clone())
            .unwrap_or_default();
        let packet_type = parsed
            .as_ref()
            .and_then(|pw| pw.packet_type.enum_value().ok())
            .unwrap_or(PacketType::PACKET_TYPE_UNKNOWN);
        let is_media = packet_type == PacketType::MEDIA;
        let media_type = if is_media {
            parsed
                .as_ref()
                .and_then(|pw| MediaPacket::parse_from_bytes(&pw.data).ok())
                .and_then(|mp| mp.media_type.enum_value().ok())
        } else {
            None
        };

        // Call `handle_outbound` BEFORE the priority-drop check so the
        // per-room outbound bytes counter and DataTracker still see
        // every packet — this matches WT's accounting and avoids a
        // counter discontinuity if a deploy moves the call site.
        // The drop path discards `bytes` without sending it.
        let bytes = self.logic.handle_outbound(&msg);

        let priority = OutboundPriority::classify(parse_succeeded, packet_type, media_type);
        let free_capacity = self.outbound_tx.capacity();
        if let PriorityDropDecision::Drop { reason } =
            ws_outbound_decision(priority, free_capacity, &self.outbound_bytes)
        {
            // Priority-driven preempt: record both the per-room and
            // protocol-wide counters with the policy-specific label,
            // and feed `on_outbound_drop` so the drop is recorded.
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[&self.logic.room, "websocket", reason])
                .inc();
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["websocket", reason])
                .inc();
            // Per-session attribution (Tier B #1): name the slow receiver.
            self.logic.record_session_drop(reason);
            trace!(
                "Priority-drop {reason} on WS session {}: free={free_capacity}/{}",
                self.logic.id,
                WS_OUTBOUND_CHANNEL_CAPACITY,
            );
            if sender_session_id != 0 {
                self.logic
                    .on_outbound_drop(sender_session_id, &sender_user_id);
            }
            drop(bytes);
            return;
        }

        let enqueued_len = bytes.len();
        match self.outbound_tx.try_send(OutboundFrame { priority, bytes }) {
            Ok(()) => {
                self.outbound_bytes.on_enqueue(priority, enqueued_len);
                self.logic.observe_outbound_delivery(&msg)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "websocket", "channel_full"])
                    .inc();
                // Real channel-full drop (priority policy already
                // admitted this packet — it's Control or Critical,
                // or the priority bands did not preempt). Record the
                // drop for the upstream sender. The metric `kind`
                // label distinguishes Critical (loud) from other
                // channel-full drops.
                //
                // 2026-05-08 audio-quality follow-up: when the wrapper says
                // MEDIA, peek at the inner `MediaPacket.media_type` so we
                // can emit `kind="audio" | "video" | "screen"` instead of
                // the catch-all `kind="media"`. Encrypted / unparseable
                // inner payloads fall through to the legacy `media` label,
                // preserving backwards compatibility.
                //
                // 2026-05-11 priority-drop policy (discussion #699):
                // a Critical packet that still fails try_send goes to
                // `overflow_critical` so an alerting rule can pivot on
                // it directly. Anything else uses the existing label
                // helper.
                if sender_session_id != 0 {
                    self.logic
                        .on_outbound_drop(sender_session_id, &sender_user_id);
                }
                let kind = if priority == OutboundPriority::Critical {
                    "overflow_critical"
                } else {
                    drop_kind_label(parse_succeeded, is_media, media_type)
                };
                OUTBOUND_CHANNEL_DROPS_TOTAL
                    .with_label_values(&["websocket", kind])
                    .inc();
                // Per-session attribution (Tier B #1): name the slow receiver.
                self.logic.record_session_drop(kind);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                ctx.stop();
            }
        }
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        trace!(
            "Forwarding packet to ChatServer: session {} room {}",
            self.logic.id,
            self.logic.room
        );
        // #2136: `requires_host` rides the Packet so the funnel can refuse to fan
        // out a MEETING_TIMER from a non-host. Both transports delegate to the
        // SAME `client_message_for` rather than mirroring the branch, so the
        // gate cannot be disabled on one transport only.
        self.logic.addr.do_send(self.logic.client_message_for(msg));
    }
}

// =============================================================================
// Outbound Drain Stream Handler
// =============================================================================

/// Drain the bounded outbound channel into actual WebSocket binary frames.
/// This runs on the actor's event loop, so writes are serialized with all
/// other actor processing — no additional synchronization needed.
impl StreamHandler<OutboundFrame> for WsChatSession {
    fn handle(&mut self, frame: OutboundFrame, ctx: &mut Self::Context) {
        self.outbound_bytes
            .on_dequeue(frame.priority, frame.bytes.len());
        ctx.binary(frame.bytes);
    }

    /// Override default `finished()` which calls `ctx.stop()`. The outbound
    /// channel closing is already handled in `Handler<Message>` via
    /// `TrySendError::Closed`, so we do NOT want the actor to stop here.
    fn finished(&mut self, _ctx: &mut Self::Context) {}
}

// =============================================================================
// WebSocket Stream Handler
// =============================================================================

impl WsChatSession {
    /// A complete data frame mid-sequence is an RFC 6455 violation actix passes through;
    /// the partial must not survive to be spliced onto later bytes.
    fn discard_open_fragment(&mut self) {
        if self.fragment.reset() {
            WS_FRAGMENT_DISCARDED_TOTAL
                .with_label_values(&["interleaved"])
                .inc();
            debug!(
                "Discarded an open fragment sequence on session {}: interleaved data frame",
                self.logic.id
            );
        }
    }

    fn extend_fragment(&mut self, bytes: &[u8]) {
        if !self.fragment.extend(bytes, MAX_FRAME_SIZE, Instant::now()) {
            WS_FRAGMENT_DISCARDED_TOTAL
                .with_label_values(&["over_size"])
                .inc();
            debug!(
                "Fragmented inbound message exceeded {} B on session {}; discarded",
                MAX_FRAME_SIZE, self.logic.id
            );
        }
    }

    /// BOTH inbound arms must route through here (#2600).
    fn dispatch_inbound(&mut self, data: &[u8], ctx: &mut ws::WebsocketContext<Self>) {
        let action = self.logic.handle_inbound(data);

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
            InboundAction::Echo(bytes) => {
                ctx.binary(bytes.as_ref().clone());
            }
            InboundAction::Forward(bytes) => {
                ctx.notify(Packet {
                    data: bytes,
                    requires_host: false,
                });
            }
            // #2136: same mailbox hop as `Forward`; the flag tells
            // `Handler<Packet>` to build a host-gated `ClientMessage` that
            // `ChatServer` refuses to fan out unless this session is the room's
            // current host.
            InboundAction::ForwardHostOnly(bytes) => {
                ctx.notify(Packet {
                    data: bytes,
                    requires_host: true,
                });
            }
            InboundAction::Processed | InboundAction::KeepAlive => {}
        }
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                error!("WebSocket protocol error: {:?}", err);
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Binary(data) => {
                self.heartbeat = Instant::now();
                self.discard_open_fragment();
                self.dispatch_inbound(&data, ctx);
            }
            // #2600: a large binary message can arrive as
            // `FirstBinary` -> `Continue`* -> `Last` instead of one `Binary`.
            ws::Message::Continuation(item) => {
                self.heartbeat = Instant::now();
                match item {
                    actix_http::ws::Item::FirstBinary(bytes) => {
                        self.fragment.begin(&bytes, Instant::now());
                    }
                    actix_http::ws::Item::FirstText(_) => {}
                    actix_http::ws::Item::Continue(bytes) => {
                        self.extend_fragment(&bytes);
                    }
                    actix_http::ws::Item::Last(bytes) => {
                        self.extend_fragment(&bytes);
                        if let Some(buf) = self.fragment.take() {
                            WS_FRAGMENTED_INBOUND_TOTAL.inc();
                            let data = buf.freeze();
                            debug!(
                                "ws reassembled session={} bytes={}",
                                self.logic.id,
                                data.len()
                            );
                            self.dispatch_inbound(&data, ctx);
                        }
                    }
                }
            }
            ws::Message::Ping(msg) => {
                self.heartbeat = Instant::now();
                ctx.pong(&msg);
            }
            ws::Message::Pong(_) => {
                self.heartbeat = Instant::now();
            }
            ws::Message::Text(_) => {
                self.heartbeat = Instant::now();
                self.discard_open_fragment();
            }
            ws::Message::Close(reason) => {
                info!(
                    "Close received for session {} in room {}",
                    self.logic.id, self.logic.room
                );
                // Do NOT send Leave here. ctx.stop() triggers stopping() which
                // sends Disconnect with the correct observer flag. A separate
                // Leave would bypass the observer check and emit a spurious
                // PARTICIPANT_LEFT for observer (waiting-room) sessions.
                ctx.close(reason);
                ctx.stop();
            }
            // Exhaustive on purpose (#2600): a new actix variant must be a compile
            // error, not a silent drop. `Nop` is encoder-only.
            ws::Message::Nop => {}
        }
    }

    fn started(&mut self, _ctx: &mut Self::Context) {}

    fn finished(&mut self, ctx: &mut Self::Context) {
        ctx.stop()
    }
}

// =============================================================================
// Helper Methods
// =============================================================================

impl WsChatSession {
    fn join_room(&self, ctx: &mut WebsocketContext<Self>) {
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

// ==========================================================================
// Session Lifecycle Integration Test (WebSocket)
// ==========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::chat_server::ChatServer;
    use crate::constants::{INBOUND_MAILBOX_HEADROOM_FACTOR, WS_OUTBOUND_LEGACY_SLOT_CAPACITY};
    use crate::server_diagnostics::ServerDiagnostics;
    use crate::session_manager::SessionManager;
    use actix::Actor;
    use actix_web::{web, App, HttpRequest, HttpServer};
    use actix_web_actors::ws;
    use futures_util::StreamExt;
    use protobuf::Message as ProtoMessage;
    use serial_test::serial;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    // ----------------------------------------------------------------------
    // Issue #1057 (+ #1144 headroom + #1062 shared binding): mailbox capacity
    // must be the outbound channel capacity TIMES the burst-headroom factor.
    //
    // `WsChatSession::started` calls `ctx.set_mailbox_capacity(
    // ws_mailbox_capacity())` so the actor mailbox (a) stops being the (default
    // 16-slot) overflow point in front of the policy-aware `outbound_tx`
    // (#1057), and (b) has modest burst slack so a publisher-join fan-out spike
    // spills onto that policy-aware channel instead of being dropped
    // indiscriminately at the mailbox (#1144).
    //
    // #1062: the value `started()` installs is the SHARED `ws_mailbox_capacity()`
    // binding, and this test asserts properties of THAT SAME function — not a
    // parallel hand-copied constant. We still cannot read the capacity back off
    // a live `WebsocketContext` without standing up NATS, so the test pins the
    // value the call site feeds; because both the call site and the test now go
    // through `ws_mailbox_capacity()`, altering that one binding's value is
    // necessarily reflected in both. (Previously a duplicated
    // `WS_MAILBOX_CAPACITY` test constant could silently diverge from the
    // `started()` expression — the drift hazard #1062 closes.)
    //
    // The invariants pinned:
    //   * `ws_mailbox_capacity()` == channel × headroom (the exact `started()`
    //     argument),
    //   * it is >= the outbound channel capacity (#1057: never smaller than the
    //     smart channel, or the dumb mailbox becomes the bottleneck), and
    //   * it is strictly larger than actix's `DEFAULT_CAPACITY` (16) so a future
    //     accidental revert to the tiny default mailbox fails CI.
    // ----------------------------------------------------------------------

    // Issue #2261. Frame counts below are LITERAL, not recomputed from the
    // fill ratios, so mutating a ratio fails these rather than moving with it.
    /// One camera frame at the default tier (600 kbps / 25 fps).
    const CAMERA_FRAME_BYTES: usize = 3_000;
    /// One SCREEN frame at the single rung (4423 kbps / 10 fps).
    const SCREEN_FRAME_BYTES: usize = 55_287;

    #[test]
    fn ws_byte_budgets_reproduce_the_legacy_slot_depths() {
        for (priority, admit_frames, shed_frames) in [
            (OutboundPriority::Video, 102usize, 103usize),
            (OutboundPriority::Screen, 115, 116),
        ] {
            assert_eq!(
                evaluate_legacy_slot_bound(priority, admit_frames),
                PriorityDropDecision::Admit,
                "{priority:?}: legacy 128-slot bound admitted {admit_frames} frames",
            );
            assert!(
                matches!(
                    evaluate_legacy_slot_bound(priority, shed_frames),
                    PriorityDropDecision::Drop { .. }
                ),
                "{priority:?}: legacy 128-slot bound shed at {shed_frames} frames",
            );
        }

        #[allow(clippy::type_complexity)]
        let cases = [
            (
                OutboundPriority::Video,
                CAMERA_FRAME_BYTES,
                102usize,
                103usize,
            ),
            (OutboundPriority::Screen, SCREEN_FRAME_BYTES, 115, 116),
        ];
        for (priority, frame_bytes, admit_frames, shed_frames) in cases {
            assert_eq!(
                ws_outbound_decision(
                    priority,
                    WS_OUTBOUND_CHANNEL_CAPACITY,
                    &meter_of(priority, admit_frames, frame_bytes),
                ),
                PriorityDropDecision::Admit,
                "{priority:?} must still be admitted at {admit_frames} queued frames",
            );
            assert!(
                matches!(
                    ws_outbound_decision(
                        priority,
                        WS_OUTBOUND_CHANNEL_CAPACITY,
                        &meter_of(priority, shed_frames, frame_bytes),
                    ),
                    PriorityDropDecision::Drop { .. }
                ),
                "{priority:?} must shed at {shed_frames} queued frames — the depth \
                 the legacy 128-slot bound produced",
            );
        }
    }

    fn meter_of(priority: OutboundPriority, frames: usize, frame_bytes: usize) -> QueueByteMeter {
        let mut meter = QueueByteMeter::default();
        for _ in 0..frames {
            meter.on_enqueue(priority, frame_bytes);
        }
        meter
    }

    #[test]
    fn camera_shed_point_ignores_screen_bytes_in_the_same_queue() {
        // A receiver's queue is mixed by construction. 6 screen frames is
        // 331,722 B, past 0.80 x WS_OUTBOUND_VIDEO_BYTE_BUDGET (307,200), so a
        // SHARED byte counter sheds every camera packet in the room here.
        let mut queue = meter_of(OutboundPriority::Screen, 6, SCREEN_FRAME_BYTES);
        assert!(
            queue.queued_total() > WS_OUTBOUND_VIDEO_BYTE_BUDGET * 80 / 100,
            "precondition: the screen backlog alone must exceed the camera \
             shed point, or this test proves nothing",
        );
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Video,
                WS_OUTBOUND_CHANNEL_CAPACITY,
                &queue
            ),
            PriorityDropDecision::Admit,
            "camera must be judged on camera bytes, not on the presenter's",
        );

        for _ in 0..102 {
            queue.on_enqueue(OutboundPriority::Video, CAMERA_FRAME_BYTES);
        }
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Video,
                WS_OUTBOUND_CHANNEL_CAPACITY,
                &queue
            ),
            PriorityDropDecision::Admit,
            "102 camera frames is the pre-#2261 depth and must still be admitted",
        );

        queue.on_enqueue(OutboundPriority::Video, CAMERA_FRAME_BYTES);
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Video,
                WS_OUTBOUND_CHANNEL_CAPACITY,
                &queue
            ),
            PriorityDropDecision::Drop {
                reason: "priority_drop_video"
            },
        );

        // Screen is likewise unaffected by the camera bytes.
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Screen,
                WS_OUTBOUND_CHANNEL_CAPACITY,
                &queue
            ),
            PriorityDropDecision::Admit,
        );
    }

    /// The pre-#2261 decision for `queued` packets in a 128-slot queue.
    fn evaluate_legacy_slot_bound(
        priority: OutboundPriority,
        queued: usize,
    ) -> PriorityDropDecision {
        crate::actors::priority_drop::evaluate(
            priority,
            WS_OUTBOUND_LEGACY_SLOT_CAPACITY - queued,
            WS_OUTBOUND_LEGACY_SLOT_CAPACITY,
        )
    }

    #[test]
    fn ws_audio_is_slot_bound_never_byte_bound() {
        assert_eq!(ws_byte_budget_for(OutboundPriority::Audio), 0);

        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Audio,
                WS_OUTBOUND_CHANNEL_CAPACITY.saturating_sub(116),
                &meter_of(OutboundPriority::Screen, 116, SCREEN_FRAME_BYTES),
            ),
            PriorityDropDecision::Admit,
        );

        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Audio,
                WS_OUTBOUND_CHANNEL_CAPACITY.saturating_sub(390),
                &meter_of(OutboundPriority::Audio, 390, 100),
            ),
            PriorityDropDecision::Admit,
        );

        assert!(matches!(
            ws_outbound_decision(
                OutboundPriority::Audio,
                WS_OUTBOUND_CHANNEL_CAPACITY.saturating_sub(973),
                &meter_of(OutboundPriority::Audio, 973, 100),
            ),
            PriorityDropDecision::Drop { .. }
        ));
    }

    #[test]
    fn ws_outbound_decision_uses_both_the_slot_and_byte_bounds() {
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Audio,
                WS_OUTBOUND_CHANNEL_CAPACITY.saturating_sub(900),
                &meter_of(OutboundPriority::Audio, 900, 100),
            ),
            PriorityDropDecision::Admit,
        );
        assert_eq!(
            ws_outbound_decision(
                OutboundPriority::Video,
                WS_OUTBOUND_CHANNEL_CAPACITY.saturating_sub(103),
                &meter_of(OutboundPriority::Video, 103, CAMERA_FRAME_BYTES),
            ),
            PriorityDropDecision::Drop {
                reason: "priority_drop_video"
            },
        );
    }

    /// actix mailbox default — see `actix::mailbox::DEFAULT_CAPACITY`.
    /// Re-declared here so the test fails loudly if the dumb default mailbox
    /// ever becomes the overflow point again (issue #1057).
    const ACTIX_DEFAULT_MAILBOX_CAPACITY: usize = 16;

    #[test]
    fn ws_mailbox_capacity_is_channel_times_headroom() {
        // Sentinel: the outbound channel capacity itself. Relocating overflow
        // to this policy-aware channel is the #1057 fix.
        assert_eq!(
            WS_OUTBOUND_CHANNEL_CAPACITY, 1024,
            "WS outbound channel capacity changed; update issue #1057 \
             rationale and any operator docs before changing this sentinel",
        );
        // Sentinel: the documented burst-headroom factor (#1144). If this
        // changes, update the `INBOUND_MAILBOX_HEADROOM_FACTOR` burst math.
        assert_eq!(
            INBOUND_MAILBOX_HEADROOM_FACTOR, 2,
            "inbound mailbox headroom factor changed; re-validate the #1144 \
             join-burst absorption math before changing this sentinel",
        );
        // #1062: assert against the SHARED binding `started()` actually calls,
        // so changing the call-site value is tracked here. The mailbox is
        // channel × headroom (256 by default).
        assert_eq!(
            ws_mailbox_capacity(),
            WS_OUTBOUND_CHANNEL_CAPACITY * INBOUND_MAILBOX_HEADROOM_FACTOR,
            "ws_mailbox_capacity() must equal channel × headroom — this is the \
             exact argument WsChatSession::started feeds set_mailbox_capacity",
        );
        assert_eq!(ws_mailbox_capacity(), 2048);
        // #1057 invariant: the mailbox must be >= the smart channel, never
        // smaller (a smaller mailbox would re-create the dumb-bottleneck).
        assert!(ws_mailbox_capacity() >= WS_OUTBOUND_CHANNEL_CAPACITY);
        // Guard: well clear of actix's dumb 16-slot default.
        assert!(ws_mailbox_capacity() > ACTIX_DEFAULT_MAILBOX_CAPACITY);
    }

    // ----------------------------------------------------------------------
    // Drop-kind label tests — mirror the WT helper tests so the WS site
    // emits the same `audio` / `video` / `screen` / `media` / `control` /
    // `unknown` set, and so the legacy `media` catch-all is preserved for
    // packets we cannot classify (HEARTBEAT, KEYFRAME_REQUEST, encrypted
    // inner). 2026-05-08 audio-quality follow-up.
    // ----------------------------------------------------------------------

    #[test]
    fn ws_drop_kind_unknown_when_parse_failed() {
        assert_eq!(
            super::drop_kind_label(/*parsed=*/ false, /*is_media=*/ false, None),
            "unknown",
        );
        assert_eq!(
            super::drop_kind_label(
                /*parsed=*/ false,
                /*is_media=*/ true,
                Some(MediaType::AUDIO),
            ),
            "unknown",
            "parse-fail must override stale is_media + media_type"
        );
    }

    #[test]
    fn ws_drop_kind_control_when_parsed_and_not_media() {
        assert_eq!(
            super::drop_kind_label(/*parsed=*/ true, /*is_media=*/ false, None,),
            "control",
        );
    }

    #[test]
    fn ws_drop_kind_audio_video_screen() {
        assert_eq!(
            super::drop_kind_label(true, true, Some(MediaType::AUDIO)),
            "audio",
        );
        assert_eq!(
            super::drop_kind_label(true, true, Some(MediaType::VIDEO)),
            "video",
        );
        assert_eq!(
            super::drop_kind_label(true, true, Some(MediaType::SCREEN)),
            "screen",
        );
    }

    #[test]
    fn ws_drop_kind_media_catchall_for_other_media_types() {
        // Backwards compat: legacy `media` bucket for HEARTBEAT,
        // KEYFRAME_REQUEST, and encrypted/unparseable inner payloads.
        assert_eq!(
            super::drop_kind_label(true, true, None),
            "media",
            "encrypted/unparseable inner must fall back to legacy `media`"
        );
        assert_eq!(
            super::drop_kind_label(true, true, Some(MediaType::HEARTBEAT)),
            "media",
        );
        assert_eq!(
            super::drop_kind_label(true, true, Some(MediaType::KEYFRAME_REQUEST)),
            "media",
        );
    }

    /// Test helper: create a database pool for future JWT flow integration tests.
    #[allow(dead_code)]
    async fn get_test_pool() -> sqlx::PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        sqlx::PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    /// Start WebSocket server for testing
    async fn start_websocket_server(port: u16) {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat = ChatServer::new(nats_client.clone()).await.start();
        let session_manager = SessionManager::new();

        let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());

        // Use actix_rt::spawn which doesn't require Send
        actix_rt::spawn(async move {
            let _ = HttpServer::new(move || {
                let chat = chat.clone();
                let nats_client = nats_client.clone();
                let tracker_sender = tracker_sender.clone();
                let session_manager = session_manager.clone();

                App::new().route(
                    "/ws/{room}/{user_id}",
                    web::get().to(
                        move |req: HttpRequest,
                              stream: web::Payload,
                              path: web::Path<(String, String)>| {
                            let chat = chat.clone();
                            let nats_client = nats_client.clone();
                            let tracker_sender = tracker_sender.clone();
                            let session_manager = session_manager.clone();

                            async move {
                                let (room, user_id) = path.into_inner();
                                let display_name = user_id.clone(); // test fallback
                                let actor = WsChatSession::new(
                                    chat,
                                    room,
                                    user_id,
                                    display_name,
                                    false, // test sessions are never guests
                                    nats_client,
                                    tracker_sender,
                                    session_manager,
                                    false, // tests use non-observer sessions
                                    None,  // no instance_id
                                    false, // is_host
                                    false, // end_on_host_leave
                                );
                                ws::start(actor, &req, stream)
                                    .map_err(actix_web::error::ErrorInternalServerError)
                            }
                        },
                    ),
                )
            })
            .bind(format!("127.0.0.1:{port}"))
            .expect("Failed to bind server")
            .run()
            .await;
        });
    }

    async fn wait_for_server_ready(port: u16) {
        let url = format!("ws://127.0.0.1:{port}/ws/test/test");
        for _ in 0..50 {
            if tokio_tungstenite::connect_async(&url).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("WebSocket server not ready after 5 seconds");
    }

    async fn connect_ws_client(
        port: u16,
        room: &str,
        user: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Box<dyn std::error::Error>,
    > {
        let url = format!("ws://127.0.0.1:{port}/ws/{room}/{user}");
        let (ws_stream, _) = tokio_tungstenite::connect_async(&url).await?;
        Ok(ws_stream)
    }

    async fn wait_for_meeting_started(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                msg = ws.next() => {
                    if let Some(Ok(Message::Binary(data))) = msg {
                        if let Ok(wrapper) = PacketWrapper::parse_from_bytes(&data) {
                            if wrapper.packet_type == PacketType::MEETING.into() {
                                if let Ok(meeting) = MeetingPacket::parse_from_bytes(&wrapper.data) {
                                    if meeting.event_type == MeetingEventType::MEETING_STARTED.into() {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        anyhow::bail!("Timeout waiting for MEETING_STARTED")
    }

    #[test]
    fn fragment_buffer_refuses_over_limit_and_discards_the_partial() {
        let t0 = Instant::now();
        let mut f = FragmentBuffer::default();
        f.begin(&[1u8; 10], t0);
        assert!(
            f.extend(&[2u8; 5], 20, t0),
            "under the limit must be accepted"
        );
        assert_eq!(f.len(), 15);
        assert!(
            !f.extend(&[3u8; 100], 20, t0),
            "over the limit must be REFUSED"
        );
        assert!(
            !f.is_open(),
            "the partial must be DISCARDED, never truncated"
        );
        assert!(
            f.take().is_none(),
            "a Last after a refusal must dispatch nothing"
        );
    }

    #[test]
    fn fragment_buffer_ignores_continuation_without_an_opener() {
        let mut f = FragmentBuffer::default();
        assert!(
            f.extend(b"orphan", 1000, Instant::now()),
            "no opener: a no-op, not an error"
        );
        assert!(!f.is_open());
        assert!(f.take().is_none());
    }

    #[test]
    fn fragment_buffer_reset_drops_a_spliceable_partial() {
        let mut f = FragmentBuffer::default();
        f.begin(b"first-half", Instant::now());
        f.reset();
        assert!(f.extend(b"attacker", 1000, Instant::now()));
        assert!(f.take().is_none(), "must not splice across the reset");
    }

    #[test]
    fn fragment_buffer_reaps_an_abandoned_sequence() {
        let t0 = Instant::now();
        let mut f = FragmentBuffer::default();
        f.begin(&[7u8; 4096], t0);
        assert_eq!(
            f.reap_if_stale(t0 + Duration::from_secs(5)),
            None,
            "5s idle must NOT be reaped"
        );
        assert!(f.is_open());
        assert_eq!(
            f.reap_if_stale(t0 + Duration::from_secs(20)),
            Some(4096),
            "20s idle MUST be reaped, reporting the bytes freed"
        );
        assert!(!f.is_open());
        assert_eq!(
            f.reap_if_stale(t0 + Duration::from_secs(99)),
            None,
            "nothing open: nothing to reap"
        );
    }

    /// Covers BOTH live arms: actix's codec emits a finished frame without consulting its
    /// continuation flag, so `Binary` and `Text` can each arrive mid-sequence (#2600).
    #[actix_rt::test]
    #[serial]
    async fn interleaved_binary_discards_the_open_fragment_issue_2600() {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::{MediaKind, PacketType};
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();

        let room = "ws-frag-splice-2600";
        let port = 18097;
        start_websocket_server(port).await;
        wait_for_server_ready(port).await;

        let mut pubr = connect_ws_client(port, room, "splice-publisher")
            .await
            .expect("connect publisher");
        let mut recv = connect_ws_client(port, room, "splice-receiver")
            .await
            .expect("connect receiver");

        let build = |tag: u8, len: u32| -> Vec<u8> {
            let media = MediaPacket {
                media_type: MediaType::VIDEO.into(),
                user_id: b"splice-publisher".to_vec(),
                data: (0..len).map(|_| tag).collect(),
                frame_type: "key".to_string(),
                ..Default::default()
            };
            let w = PacketWrapper {
                packet_type: PacketType::MEDIA.into(),
                user_id: b"splice-publisher".to_vec(),
                media_kind: MediaKind::VIDEO.into(),
                data: media.write_to_bytes().expect("serialize MediaPacket"),
                ..Default::default()
            };
            w.write_to_bytes().expect("serialize PacketWrapper")
        };
        let abandoned = build(0xAA, 2048);
        let interleaved = build(0xBB, 512);
        // A distinct length, so a splice through the Text arm is distinguishable from the Binary one.
        let abandoned_text_phase = build(0xCC, 3072);

        tokio::time::sleep(Duration::from_millis(1500)).await;

        // Phase 1: interleave a complete Binary frame (actix passes it through despite RFC 6455).
        let half = abandoned.len() / 2;
        pubr.send(Message::Frame(Frame::message(
            abandoned[..half].to_vec(),
            OpCode::Data(Data::Binary),
            false,
        )))
        .await
        .expect("send opener");
        pubr.send(Message::Binary(interleaved.clone()))
            .await
            .expect("send interleaved complete frame");
        pubr.send(Message::Frame(Frame::message(
            abandoned[half..].to_vec(),
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .expect("send closer");

        // Phase 2: same splice via a complete TEXT frame — a live guard, not defence-in-depth.
        let half_t = abandoned_text_phase.len() / 2;
        pubr.send(Message::Frame(Frame::message(
            abandoned_text_phase[..half_t].to_vec(),
            OpCode::Data(Data::Binary),
            false,
        )))
        .await
        .expect("send text-phase opener");
        pubr.send(Message::Text("x".repeat(64)))
            .await
            .expect("send interleaved complete Text frame");
        pubr.send(Message::Frame(Frame::message(
            abandoned_text_phase[half_t..].to_vec(),
            OpCode::Data(Data::Continue),
            true,
        )))
        .await
        .expect("send text-phase closer");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut saw_interleaved = false;
        while tokio::time::Instant::now() < deadline {
            tokio::select! {
                msg = recv.next() => match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if let Ok(w) = PacketWrapper::parse_from_bytes(&data) {
                            if w.packet_type == PacketType::MEDIA.into() {
                                let inner = MediaPacket::parse_from_bytes(&w.data)
                                    .expect("forwarded MEDIA must parse");
                                assert_ne!(
                                    inner.data.len(),
                                    2048,
                                    "the abandoned partial was SPLICED and forwarded — \
                                     the Binary arm did not discard it (issue 2600)"
                                );
                                assert_ne!(
                                    inner.data.len(),
                                    3072,
                                    "the abandoned partial was SPLICED and forwarded — \
                                     the Text arm did not discard it (issue 2600)"
                                );
                                if inner.data.len() == 512 {
                                    saw_interleaved = true;
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => panic!("receiver socket error: {e}"),
                    None => panic!("receiver socket closed"),
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
        assert!(
            saw_interleaved,
            "premise: the interleaved complete frame must itself be forwarded"
        );
    }

    /// An empty `Continue` is an accepted append, so it refreshes the idle clock (#2600).
    #[test]
    fn fragment_buffer_lifetime_cap_reclaims_an_empty_continue_trickle() {
        let t0 = Instant::now();
        let mut f = FragmentBuffer::default();
        f.begin(&[0u8; 1024], t0);

        let step = FRAGMENT_ASSEMBLY_IDLE_TIMEOUT.mul_f32(0.9);
        let mut now = t0;
        while now.duration_since(t0) < FRAGMENT_ASSEMBLY_MAX_LIFETIME {
            now += step;
            assert!(
                f.extend(&[], MAX_FRAME_SIZE, now),
                "empty append is accepted"
            );
            if now.duration_since(t0) <= FRAGMENT_ASSEMBLY_MAX_LIFETIME {
                assert_eq!(
                    f.reap_if_stale(now),
                    None,
                    "inside the lifetime the idle refresh still spares it"
                );
            }
        }
        assert_eq!(
            f.reap_if_stale(now),
            Some(1024),
            "past the lifetime it is reclaimed DESPITE a refreshed idle clock"
        );
        assert!(!f.is_open(), "and the held bytes are released");
    }

    #[test]
    fn fragment_buffer_completed_sequence_is_not_later_reaped() {
        let t0 = Instant::now();
        let mut f = FragmentBuffer::default();
        f.begin(&[7u8; 512], t0);
        assert_eq!(f.take().map(|b| b.len()), Some(512));
        assert_eq!(
            f.reap_if_stale(t0 + Duration::from_secs(99)),
            None,
            "a completed sequence must not later read as abandoned"
        );
    }

    /// The ordering asserts below admit 5s..20s, so they cannot pin the value (#2600).
    #[test]
    fn fragment_deadlines_are_the_shipped_values() {
        assert_eq!(FRAGMENT_ASSEMBLY_IDLE_TIMEOUT, Duration::from_secs(10));
        assert_eq!(FRAGMENT_ASSEMBLY_MAX_LIFETIME, Duration::from_secs(60));
    }

    #[test]
    fn fragment_idle_timeout_is_bounded_and_below_client_timeout() {
        assert!(
            FRAGMENT_ASSEMBLY_MAX_LIFETIME > FRAGMENT_ASSEMBLY_IDLE_TIMEOUT,
            "lifetime {FRAGMENT_ASSEMBLY_MAX_LIFETIME:?} must exceed idle {FRAGMENT_ASSEMBLY_IDLE_TIMEOUT:?}"
        );
        assert!(
            FRAGMENT_ASSEMBLY_IDLE_TIMEOUT < CLIENT_TIMEOUT,
            "idle deadline {FRAGMENT_ASSEMBLY_IDLE_TIMEOUT:?} must be under CLIENT_TIMEOUT {CLIENT_TIMEOUT:?}"
        );
        assert!(
            FRAGMENT_ASSEMBLY_IDLE_TIMEOUT >= HEARTBEAT_INTERVAL,
            "must be at least the {HEARTBEAT_INTERVAL:?} tick or reaping is coin-flip granular"
        );
    }

    /// A slow uplink still sending must survive, or a multi-MB keyframe dies mid-arrival.
    #[test]
    fn fragment_buffer_idle_clock_spares_a_slow_but_active_uplink() {
        let t0 = Instant::now();
        let mut f = FragmentBuffer::default();
        f.begin(&[0u8; 1024], t0);
        let step = FRAGMENT_ASSEMBLY_IDLE_TIMEOUT.mul_f32(0.9);
        let steps = (FRAGMENT_ASSEMBLY_MAX_LIFETIME.as_secs_f64() / step.as_secs_f64()) as u32 - 1;
        let mut now = t0;
        for _ in 0..steps {
            now += step;
            assert!(f.extend(&[1u8; 1024], MAX_FRAME_SIZE, now));
            assert_eq!(
                f.reap_if_stale(now),
                None,
                "still arriving: must not be reaped"
            );
        }
        assert!(f.is_open(), "many idle deadlines elapsed and it survived");
        assert!(
            f.reap_if_stale(now + Duration::from_secs(20)).is_some(),
            "then it goes idle and IS reaped"
        );
    }

    #[actix_rt::test]
    #[serial]
    async fn fragmented_inbound_media_is_reassembled_and_forwarded_issue_2600() {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::{Data, OpCode};
        use tokio_tungstenite::tungstenite::protocol::frame::Frame;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();

        let room = "ws-frag-2600";
        let port = 18099;
        start_websocket_server(port).await;
        wait_for_server_ready(port).await;

        let mut pubr = connect_ws_client(port, room, "frag-publisher")
            .await
            .expect("connect publisher");
        let mut recv = connect_ws_client(port, room, "frag-receiver")
            .await
            .expect("connect receiver");

        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let media = MediaPacket {
            media_type: MediaType::VIDEO.into(),
            user_id: b"frag-publisher".to_vec(),
            data: payload.clone(),
            frame_type: "key".to_string(),
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            user_id: b"frag-publisher".to_vec(),
            // Without this `classify_packet` takes the opaque-Data branch, not Media.
            media_kind: videocall_types::protos::packet_wrapper::packet_wrapper::MediaKind::VIDEO
                .into(),
            data: media.write_to_bytes().expect("serialize MediaPacket"),
            ..Default::default()
        };
        let bytes = wrapper.write_to_bytes().expect("serialize PacketWrapper");
        assert!(bytes.len() > 2, "need a payload we can split");

        tokio::time::sleep(Duration::from_millis(1500)).await;

        // THREE frames. A *finished* `OpCode::Continue` decodes to `Item::Last`, so a
        // two-frame sequence never reaches the `Item::Continue` arm at all.
        let a = bytes.len() / 3;
        let b = 2 * bytes.len() / 3;
        for (chunk, opcode, fin) in [
            (&bytes[..a], OpCode::Data(Data::Binary), false),
            (&bytes[a..b], OpCode::Data(Data::Continue), false),
            (&bytes[b..], OpCode::Data(Data::Continue), true),
        ] {
            pubr.send(Message::Frame(Frame::message(chunk.to_vec(), opcode, fin)))
                .await
                .expect("send fragment");
        }

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut got = false;
        while tokio::time::Instant::now() < deadline && !got {
            tokio::select! {
                msg = recv.next() => {
                    match msg {
                        Some(Ok(Message::Binary(data))) => {
                            if let Ok(w) = PacketWrapper::parse_from_bytes(&data) {
                                if w.packet_type == PacketType::MEDIA.into() {
                                    let inner = MediaPacket::parse_from_bytes(&w.data)
                                        .expect("forwarded MEDIA must parse");
                                    assert_eq!(
                                        inner.data, payload,
                                        "reassembled payload must be byte-identical"
                                    );
                                    got = true;
                                }
                            }
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => panic!("receiver socket error: {e}"),
                        None => panic!("receiver socket closed"),
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }

        assert!(
            got,
            "fragmented MEDIA packet was never forwarded — the relay dropped the \
             continuation sequence (issue #2600)"
        );
    }

    #[actix_rt::test]
    #[serial]
    async fn test_meeting_lifecycle_websocket() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .try_init();

        // Enable meeting management for this test
        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let result = test_meeting_lifecycle_ws_impl().await;

        // Clean up feature flag
        videocall_types::FeatureFlags::clear_meeting_management_override();

        if let Err(e) = result {
            panic!("Test failed: {e}");
        }
    }

    async fn test_meeting_lifecycle_ws_impl() -> anyhow::Result<()> {
        println!("=== STARTING SESSION LIFECYCLE TEST (WebSocket) ===");

        let room_id = "ws-meeting-lifecycle-test";
        let port = 18080; // Use a unique port for testing

        println!("Starting WebSocket server on port {port}...");
        start_websocket_server(port).await;

        // Wait for server to be ready
        wait_for_server_ready(port).await;
        println!("✓ Server ready");

        // ========== STEP 1: First user connects ==========
        println!("\n--- Step 1: Alice connects (first participant) ---");

        let mut ws_alice = connect_ws_client(port, room_id, "alice")
            .await
            .expect("connect alice");
        wait_for_meeting_started(&mut ws_alice, Duration::from_secs(5)).await?;
        println!("✓ Alice connected and received MEETING_STARTED");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let mut ws_bob = connect_ws_client(port, room_id, "bob")
            .await
            .expect("connect bob");
        wait_for_meeting_started(&mut ws_bob, Duration::from_secs(5)).await?;
        println!("✓ Bob connected and received MEETING_STARTED");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let mut ws_charlie = connect_ws_client(port, room_id, "charlie")
            .await
            .expect("connect charlie");
        wait_for_meeting_started(&mut ws_charlie, Duration::from_secs(5)).await?;
        println!("✓ Charlie connected and received MEETING_STARTED");

        // ========== STEP 4: Charlie disconnects ==========
        println!("\n--- Step 4: Charlie disconnects ---");
        drop(ws_charlie);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Charlie disconnected");

        // ========== STEP 5: Bob disconnects ==========
        println!("\n--- Step 5: Bob disconnects ---");
        drop(ws_bob);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Bob disconnected");

        // ========== STEP 6: Alice (last) disconnects ==========
        println!("\n--- Step 6: Alice disconnects - session ends ---");
        drop(ws_alice);
        tokio::time::sleep(Duration::from_millis(500)).await;
        println!("✓ Alice disconnected");

        println!("\n=== SESSION LIFECYCLE TEST PASSED (WebSocket) ===");
        Ok(())
    }
}
