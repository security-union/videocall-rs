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
use crate::messages::server::{ForceDisconnect, Leave, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::TrackerSender;
use crate::session_manager::SessionManager;
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, Actor, ActorContext, Addr, AsyncContext, ContextFutureSpawner, Handler,
    Running, StreamHandler, WrapFuture,
};
use actix_web_actors::ws::{self, WebsocketContext};
use tracing::{error, info};

use super::common;

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
                    // Send SESSION_ASSIGNED first: explicit session_id for this connection
                    let session_assigned = act.logic.build_session_assigned();
                    ctx.binary(session_assigned);
                    let meeting_started = act
                        .logic
                        .build_meeting_started(result.start_time_ms, &result.creator_id);
                    ctx.binary(meeting_started);
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
        common::log_force_disconnect(self.logic.id, &self.logic.room);
        ctx.stop();
    }
}

/// Handle outbound packets (forwarding to ChatServer)
impl Handler<Packet> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        common::forward_packet_to_chat_server(
            &self.logic.addr,
            self.logic.id,
            self.logic.email.clone(),
            self.logic.room.clone(),
            msg,
        );
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

                // Activate on first successfully parsed packet (guard: skip if already activated)
                common::try_activate_from_first_packet(
                    &self.logic.addr,
                    self.logic.id,
                    &mut self.activated,
                    &data,
                );

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
    fn join_room(&self, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.logic.addr.send(self.logic.create_join_room_message());
        let join_room = join_room.into_actor(self);
        join_room
            .then(|response, act, ctx| {
                if common::handle_join_room_response(response, &act.logic.room, act.logic.id)
                    .is_err()
                {
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
    use crate::test_utils;
    use actix::Actor;
    use actix_web::{web, App, HttpRequest, HttpServer};
    use actix_web_actors::ws;
    use serial_test::serial;
    use std::time::Duration;

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
        test_utils::wait_for_meeting_started(&mut ws_alice, Duration::from_secs(5)).await?;
        println!("✓ Alice connected and received MEETING_STARTED");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let mut ws_bob = connect_ws_client(port, room_id, "bob")
            .await
            .expect("connect bob");
        test_utils::wait_for_meeting_started(&mut ws_bob, Duration::from_secs(5)).await?;
        println!("✓ Bob connected and received MEETING_STARTED");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let mut ws_charlie = connect_ws_client(port, room_id, "charlie")
            .await
            .expect("connect charlie");
        test_utils::wait_for_meeting_started(&mut ws_charlie, Duration::from_secs(5)).await?;
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

    /// Verifies that relayed packets carry consistent session_ids: when Alice sends,
    /// Bob receives with session_id=Alice's; when Bob sends, Alice receives with Bob's.
    #[actix_rt::test]
    #[serial]
    async fn test_session_id_consistency_websocket() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::sink)
            .try_init();

        videocall_types::FeatureFlags::set_meeting_management_override(true);

        let result = test_session_id_consistency_ws_impl().await;

        videocall_types::FeatureFlags::clear_meeting_management_override();

        if let Err(e) = result {
            panic!("Session ID consistency test failed: {e}");
        }
    }

    async fn test_session_id_consistency_ws_impl() -> anyhow::Result<()> {
        use futures_util::SinkExt;
        use protobuf::Message as ProtoMessage;
        use tokio_tungstenite::tungstenite::Message;
        use videocall_types::protos::media_packet::media_packet::MediaType;
        use videocall_types::protos::media_packet::MediaPacket;
        use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let room_id = "ws-session-id-test";
        let port = 18081;

        start_websocket_server(port).await;
        wait_for_server_ready(port).await;

        let mut ws_alice = connect_ws_client(port, room_id, "alice")
            .await
            .map_err(|e| anyhow::anyhow!("connect alice: {e}"))?;
        let alice_session_id = test_utils::wait_for_meeting_started_with_session_id(
            &mut ws_alice,
            Duration::from_secs(5),
        )
        .await?;

        let mut ws_bob = connect_ws_client(port, room_id, "bob")
            .await
            .map_err(|e| anyhow::anyhow!("connect bob: {e}"))?;
        let bob_session_id = test_utils::wait_for_meeting_started_with_session_id(
            &mut ws_bob,
            Duration::from_secs(5),
        )
        .await?;

        assert_ne!(
            alice_session_id, bob_session_id,
            "Alice and Bob must have different session_ids"
        );
        assert_ne!(alice_session_id, 0, "Alice session_id must not be zero");
        assert_ne!(bob_session_id, 0, "Bob session_id must not be zero");

        // Alice sends MEDIA packet with session_id=0; server should fill it and relay to Bob
        let media = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            email: "alice".to_string(),
            ..Default::default()
        };
        let packet = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            email: "alice".to_string(),
            data: media.write_to_bytes()?,
            session_id: 0,
            ..Default::default()
        };
        let bytes = packet.write_to_bytes()?;

        ws_alice
            .send(Message::Binary(bytes))
            .await
            .map_err(|e| anyhow::anyhow!("alice send: {e}"))?;

        // Bob receives: must have session_id == alice_session_id
        let received = futures_util::StreamExt::next(&mut ws_bob)
            .await
            .ok_or_else(|| anyhow::anyhow!("No message from stream"))?
            .map_err(|e| anyhow::anyhow!("bob recv: {e}"))?;
        let recv_bytes = match &received {
            Message::Binary(b) => b.clone(),
            _ => anyhow::bail!("Expected binary message"),
        };
        let recv_packet = PacketWrapper::parse_from_bytes(&recv_bytes)?;
        assert_eq!(
            recv_packet.session_id, alice_session_id,
            "Bob must receive packet with Alice's session_id, got {}",
            recv_packet.session_id
        );

        // Bob sends MEDIA packet with session_id=0; server should fill it and relay to Alice
        let media_b = MediaPacket {
            media_type: MediaType::AUDIO.into(),
            email: "bob".to_string(),
            ..Default::default()
        };
        let packet_b = PacketWrapper {
            packet_type: PacketType::MEDIA.into(),
            email: "bob".to_string(),
            data: media_b.write_to_bytes()?,
            session_id: 0,
            ..Default::default()
        };
        ws_bob
            .send(Message::Binary(packet_b.write_to_bytes()?))
            .await
            .map_err(|e| anyhow::anyhow!("bob send: {e}"))?;

        // Alice receives: must have session_id == bob_session_id
        let received_a = futures_util::StreamExt::next(&mut ws_alice)
            .await
            .ok_or_else(|| anyhow::anyhow!("No message from stream"))?
            .map_err(|e| anyhow::anyhow!("alice recv: {e}"))?;
        let recv_bytes_a = match &received_a {
            Message::Binary(b) => b.clone(),
            _ => anyhow::bail!("Expected binary message"),
        };
        let recv_packet_a = PacketWrapper::parse_from_bytes(&recv_bytes_a)?;
        assert_eq!(
            recv_packet_a.session_id, bob_session_id,
            "Alice must receive packet with Bob's session_id, got {}",
            recv_packet_a.session_id
        );

        Ok(())
    }
}
