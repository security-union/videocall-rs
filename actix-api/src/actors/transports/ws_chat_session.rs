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
    evaluate as evaluate_priority_drop, OutboundPriority, PriorityDropDecision,
};
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::{CLIENT_TIMEOUT, HEARTBEAT_INTERVAL, WS_OUTBOUND_CHANNEL_CAPACITY};
use crate::messages::server::{ActivateConnection, Packet};
use crate::messages::session::Message;
use crate::metrics::{
    OUTBOUND_CHANNEL_DROPS_TOTAL, RELAY_OUTBOUND_QUEUE_DEPTH, RELAY_PACKET_DROPS_TOTAL,
};
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, Actor, ActorContext, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{self, WebsocketContext};
use protobuf::Message as ProtobufMessage;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, trace};
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
    /// When the channel is full, `on_outbound_drop()` fires CONGESTION.
    outbound_tx: mpsc::Sender<Vec<u8>>,

    /// Receiver half, consumed once by `started()` via `ctx.add_stream()`.
    outbound_rx: Option<ReceiverStream<Vec<u8>>>,
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

        let (outbound_tx, outbound_rx) = mpsc::channel::<Vec<u8>>(WS_OUTBOUND_CHANNEL_CAPACITY);

        WsChatSession {
            logic,
            heartbeat: Instant::now(),
            activated: false,
            outbound_tx,
            outbound_rx: Some(ReceiverStream::new(outbound_rx)),
        }
    }

    /// Start heartbeat check (WebSocket-specific: uses ping frames)
    fn start_heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            // Sample outbound queue depth for Prometheus
            let depth = WS_OUTBOUND_CHANNEL_CAPACITY - act.outbound_tx.capacity();
            RELAY_OUTBOUND_QUEUE_DEPTH
                .with_label_values(&[&act.logic.room, "websocket"])
                .set(depth as f64);

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
/// is dropped and `on_outbound_drop()` fires CONGESTION feedback to the
/// sender via NATS — mirroring the WebTransport relay pattern.
///
/// **Priority-drop policy (discussion #699)**: before `try_send`, the
/// per-session `actors::priority_drop` evaluator decides whether to
/// preempt the enqueue based on packet priority and channel fill:
///
/// * Video / screen frames are shed at ~80% channel fill so audio
///   gets the headroom (one 1-2 Mbps video frame buffer is worth
///   ~200 audio frames at ~50 kbps).
/// * Audio frames preserved until ~95% fill.
/// * Control packets are never preempted by the policy. Critical
///   lifecycle packets (`SESSION_ASSIGNED`, `CONGESTION`,
///   `RSA_PUB_KEY`, `MEETING`) also use the `overflow_critical` kind
///   label when they fail on real channel overflow, so a saturation
///   severe enough to drop lifecycle traffic is alertable on its own.
///
/// On any drop (preempted or real overflow), the sender's
/// `on_outbound_drop` still fires so CONGESTION feedback reaches the
/// upstream sender that caused the saturation. Without that callback
/// the offending sender keeps sending at the same rate and the
/// receiver keeps shedding their traffic.
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
        // still feed `on_outbound_drop` (the CONGESTION trigger).
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
            evaluate_priority_drop(priority, free_capacity, WS_OUTBOUND_CHANNEL_CAPACITY)
        {
            // Priority-driven preempt: record both the per-room and
            // protocol-wide counters with the policy-specific label,
            // and fire `on_outbound_drop` so the offending sender
            // gets CONGESTION feedback.
            RELAY_PACKET_DROPS_TOTAL
                .with_label_values(&[&self.logic.room, "websocket", reason])
                .inc();
            OUTBOUND_CHANNEL_DROPS_TOTAL
                .with_label_values(&["websocket", reason])
                .inc();
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

        match self.outbound_tx.try_send(bytes) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                RELAY_PACKET_DROPS_TOTAL
                    .with_label_values(&[&self.logic.room, "websocket", "channel_full"])
                    .inc();
                // Real channel-full drop (priority policy already
                // admitted this packet — it's Control or Critical,
                // or the priority bands did not preempt). Fire
                // CONGESTION feedback for the upstream sender. The
                // metric `kind` label distinguishes Critical (loud)
                // from other channel-full drops.
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
        self.logic
            .addr
            .do_send(self.logic.create_client_message(msg));
    }
}

// =============================================================================
// Outbound Drain Stream Handler
// =============================================================================

/// Drain the bounded outbound channel into actual WebSocket binary frames.
/// This runs on the actor's event loop, so writes are serialized with all
/// other actor processing — no additional synchronization needed.
impl StreamHandler<Vec<u8>> for WsChatSession {
    fn handle(&mut self, bytes: Vec<u8>, ctx: &mut Self::Context) {
        ctx.binary(bytes);
    }

    /// Override default `finished()` which calls `ctx.stop()`. The outbound
    /// channel closing is already handled in `Handler<Message>` via
    /// `TrySendError::Closed`, so we do NOT want the actor to stop here.
    fn finished(&mut self, _ctx: &mut Self::Context) {}
}

// =============================================================================
// WebSocket Stream Handler
// =============================================================================

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

                let action = self.logic.handle_inbound(&data);

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
                        ctx.notify(Packet { data: bytes });
                    }
                    InboundAction::Processed | InboundAction::KeepAlive => {}
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
            _ => (),
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
