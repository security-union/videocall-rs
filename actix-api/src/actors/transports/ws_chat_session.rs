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
use crate::actors::session_logic::{InboundAction, SessionLogic};
use crate::constants::{CLIENT_TIMEOUT, HEARTBEAT_INTERVAL};
use crate::messages::server::{ActivateConnection, ClientMessage, ForceDisconnect, Leave, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, Actor, ActorContext, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{self, WebsocketContext};
use protobuf::Message as ProtobufMessage;
use tracing::{error, info, trace};
use videocall_types::protos::packet_wrapper::packet_wrapper::ConnectionPhase;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub use crate::actors::session_logic::{Email, RoomId, SessionId};

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
}

impl WsChatSession {
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
    ) -> Self {
        let logic = SessionLogic::new(
            addr,
            room,
            email,
            nats_client,
            tracker_sender,
            session_manager,
        );

        WsChatSession {
            logic,
            heartbeat: Instant::now(),
            activated: false,
        }
    }

    /// Start heartbeat check (WebSocket-specific: uses ping frames)
    fn start_heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
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
        // Track connection start
        self.logic.track_connection_start("websocket");

        // Start session via SessionManager
        let session_manager = self.logic.session_manager.clone();
        let room = self.logic.room.clone();
        let email = self.logic.email.clone();
        let session_id = self.logic.id;

        ctx.wait(
            async move {
                session_manager
                    .start_session(&room, &email, session_id)
                    .await
            }
            .into_actor(self)
            .map(|result, act, ctx| match result {
                Ok(result) => {
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

        // Start heartbeat
        self.start_heartbeat(ctx);

        // Register with ChatServer
        let addr = ctx.address();
        self.logic
            .addr
            .send(self.logic.create_connect_message(
                addr.clone().recipient(),
                addr.recipient::<ForceDisconnect>(),
            ))
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
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        self.logic.on_stopping();
        Running::Stop
    }
}

// =============================================================================
// Message Handlers
// =============================================================================

/// Handle outbound messages from ChatServer
impl Handler<Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        let bytes = self.logic.handle_outbound(&msg);
        ctx.binary(bytes);
    }
}

/// Handle force disconnect (e.g. on session ID collision)
impl Handler<ForceDisconnect> for WsChatSession {
    type Result = ();

    fn handle(&mut self, _msg: ForceDisconnect, ctx: &mut Self::Context) -> Self::Result {
        info!(
            "Force disconnect for session {} in room {}",
            self.logic.id, self.logic.room
        );
        ctx.stop();
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
        self.logic.addr.do_send(ClientMessage {
            session: self.logic.id,
            user: self.logic.email.clone(),
            room: self.logic.room.clone(),
            msg,
        });
    }
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
                // Update heartbeat
                self.heartbeat = Instant::now();

                // Check connection_phase from inbound packet (guard: skip if already activated)
                self.try_activate_from_first_packet(&data);

                // Delegate to shared logic
                match self.logic.handle_inbound(&data) {
                    InboundAction::Echo(bytes) => {
                        ctx.binary(bytes.as_ref().clone());
                    }
                    InboundAction::Forward(bytes) => {
                        ctx.notify(Packet { data: bytes });
                    }
                    InboundAction::Processed | InboundAction::KeepAlive => {
                        // Already handled
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
            ws::Message::Close(reason) => {
                info!(
                    "Close received for session {} in room {}",
                    self.logic.id, self.logic.room
                );
                self.logic.addr.do_send(Leave {
                    session: self.logic.id,
                    room: self.logic.room.clone(),
                    user_id: self.logic.email.clone(),
                });
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
    /// Parse inbound packet and activate on first ACTIVE or UNSPECIFIED phase.
    /// Skips if already activated or during PROBING.
    fn try_activate_from_first_packet(&mut self, data: &[u8]) {
        if self.activated {
            return;
        }
        let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) else {
            return;
        };
        let Ok(phase) = packet_wrapper.connection_phase.enum_value() else {
            return;
        };
        let should_activate = matches!(
            phase,
            ConnectionPhase::ACTIVE | ConnectionPhase::CONNECTION_PHASE_UNSPECIFIED
        );
        if !should_activate {
            return;
        }
        self.logic.addr.do_send(ActivateConnection {
            session: self.logic.id,
        });
        self.activated = true;
        info!(
            "Session {} activated on first {:?} packet",
            self.logic.id, phase
        );
    }

    fn join_room(&self, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.logic.addr.send(self.logic.create_join_room_message());
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                match response {
                    Ok(Ok(())) => {
                        info!(
                            "Successfully joined room {} for session {}",
                            act.logic.room, act.logic.id
                        );
                    }
                    Ok(Err(e)) => {
                        error!("Failed to join room: {}", e);
                        ctx.stop();
                    }
                    Err(err) => {
                        error!("Error sending JoinRoom: {:?}", err);
                        ctx.stop();
                    }
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
        let nats_client = async_nats::ConnectOptions::new()
            .no_echo()
            .connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");
        let chat = ChatServer::new(nats_client.clone()).await.start();
        let session_manager = SessionManager::new();

        let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());

        actix_rt::spawn(async move {
            let _ = HttpServer::new(move || {
                let chat = chat.clone();
                let nats_client = nats_client.clone();
                let tracker_sender = tracker_sender.clone();
                let session_manager = session_manager.clone();

                App::new().route(
                    "/ws/{room}/{email}",
                    web::get().to(
                        move |req: HttpRequest,
                              stream: web::Payload,
                              path: web::Path<(String, String)>| {
                            let chat = chat.clone();
                            let nats_client = nats_client.clone();
                            let tracker_sender = tracker_sender.clone();
                            let session_manager = session_manager.clone();

                            async move {
                                let (room, email) = path.into_inner();
                                let actor = WsChatSession::new(
                                    chat,
                                    room,
                                    email,
                                    nats_client,
                                    tracker_sender,
                                    session_manager,
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
