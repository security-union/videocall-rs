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

use crate::client_diagnostics::health_processor;
use crate::messages::server::{ClientMessage, Leave, Packet};
use crate::messages::session::Message;
use crate::server_diagnostics::{
    send_connection_ended, send_connection_started, DataTracker, TrackerSender,
};
use crate::session_manager::SessionManager;
use crate::{actors::chat_server::ChatServer, constants::CLIENT_TIMEOUT};
use std::sync::Arc;

use crate::{
    constants::HEARTBEAT_INTERVAL,
    messages::server::{Connect, Disconnect, JoinRoom},
};
use actix::ActorFutureExt;
use actix::{
    clock::Instant, fut, ActorContext, ContextFutureSpawner, Handler, Running, StreamHandler,
    WrapFuture,
};
use actix::{Actor, Addr, AsyncContext};
use actix_web_actors::ws::{self, WebsocketContext};
use protobuf::Message as ProtobufMessage;
use tracing::{error, info, trace};
use uuid::Uuid;
use videocall_types::protos::media_packet::media_packet::MediaType;
use videocall_types::protos::media_packet::MediaPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

pub type RoomId = String;
pub type Email = String;
pub type SessionId = String;

pub struct WsChatSession {
    pub id: SessionId,
    pub room: RoomId,
    pub addr: Addr<ChatServer>,
    pub heartbeat: Instant,
    pub email: Email,
    pub creator_id: String,
    pub nats_client: async_nats::client::Client,
    pub tracker_sender: TrackerSender,
    pub session_manager: SessionManager,
}

impl WsChatSession {
    pub fn new(
        addr: Addr<ChatServer>,
        room: String,
        email: String,
        // creator_id: String,
        nats_client: async_nats::client::Client,
        tracker_sender: TrackerSender,
        session_manager: SessionManager,
    ) -> Self {
        let session_id = Uuid::new_v4().to_string();
        info!(
            "new session with room {} and email {} and session_id {:?}",
            room, email, session_id
        );

        WsChatSession {
            id: session_id.clone(),
            heartbeat: Instant::now(),
            room: room.clone(),
            email: email.clone(),
            creator_id: email.clone(),
            addr,
            nats_client,
            tracker_sender,
            session_manager,
        }
    }

    /// Check if the binary data is an RTT packet that should be echoed back
    fn is_rtt_packet(&self, data: &[u8]) -> bool {
        if let Ok(packet_wrapper) = PacketWrapper::parse_from_bytes(data) {
            if packet_wrapper.packet_type == PacketType::MEDIA.into() {
                if let Ok(media_packet) = MediaPacket::parse_from_bytes(&packet_wrapper.data) {
                    return media_packet.media_type == MediaType::RTT.into();
                }
            }
        }
        false
    }

    fn heartbeat(&self, ctx: &mut WebsocketContext<Self>) {
        ctx.run_interval(HEARTBEAT_INTERVAL, |act, ctx| {
            if Instant::now().duration_since(act.heartbeat) > CLIENT_TIMEOUT {
                // heartbeat timed out
                println!("Websocket Client heartbeat failed, disconnecting!");
                // notify chat server
                act.addr.do_send(Disconnect {
                    session: act.id.clone(),
                    room: act.room.clone(),
                    user_id: act.creator_id.clone(),
                });
                // stop actor
                error!("hearbeat timeout");
                ctx.stop();
                // don't try to send a ping
                return;
            }
            ctx.ping(b"");
        });
    }
}

impl Actor for WsChatSession {
    type Context = WebsocketContext<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Track connection start for metrics
        send_connection_started(
            &self.tracker_sender,
            self.id.clone(),
            self.email.clone(),
            self.room.clone(),
            "websocket".to_string(),
        );

        // Start session using SessionManager
        let session_manager = self.session_manager.clone();
        let room_id = self.room.clone();
        let creator_id = self.creator_id.clone();

        ctx.wait(
            async move {
                match session_manager.start_session(&room_id, &creator_id).await {
                    // Return result.creator_id to ensure correct host is identified (not the joining user)
                    Ok(result) => Ok((result.start_time_ms, result.creator_id)),
                    Err(e) => {
                        error!("failed to start session: {}", e);
                        Err(e.to_string())
                    }
                }
            }
            .into_actor(self)
            .map(|result, act, ctx| {
                match result {
                    Ok((start_time_ms, actual_creator_id)) => {
                        // Send MEETING_STARTED packet (protobuf)
                        let bytes = SessionManager::build_meeting_started_packet(
                            &act.room,
                            start_time_ms,
                            &actual_creator_id,
                        );
                        ctx.binary(bytes);
                    }
                    Err(error_msg) => {
                        // Send error to client and close connection
                        let bytes = SessionManager::build_meeting_ended_packet(
                            &act.room,
                            &format!("Session rejected: {error_msg}"),
                        );
                        ctx.binary(bytes);
                        ctx.close(Some(actix_web_actors::ws::CloseReason {
                            code: actix_web_actors::ws::CloseCode::Policy,
                            description: Some("Session rejected".to_string()),
                        }));
                        ctx.stop();
                    }
                }
            }),
        );

        self.heartbeat(ctx);
        let addr = ctx.address();
        self.addr
            .send(Connect {
                id: self.id.clone(),
                addr: addr.recipient(),
            })
            .into_actor(self)
            .then(|res, _act, ctx| {
                if let Err(err) = res {
                    error!("error {:?}", err);
                    ctx.stop();
                }
                fut::ready(())
            })
            .wait(ctx);

        // Join the room
        self.join(self.room.clone(), ctx);
    }

    fn stopping(&mut self, _: &mut Self::Context) -> Running {
        info!("Session stopping: {} in room {}", self.id, self.room);
        // Track connection end for metrics
        send_connection_ended(&self.tracker_sender, self.id.clone());

        // notify chat server
        self.addr.do_send(Disconnect {
            session: self.id.clone(),
            room: self.room.clone(),
            user_id: self.creator_id.clone(),
        });

        Running::Stop
    }
}

impl Handler<Message> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Message, ctx: &mut Self::Context) -> Self::Result {
        // Track sent data when forwarding messages to clients
        let data_tracker = DataTracker::new(self.tracker_sender.clone());
        data_tracker.track_sent(&self.id, msg.msg.len() as u64);
        ctx.binary(msg.msg);
    }
}

impl Handler<Packet> for WsChatSession {
    type Result = ();

    fn handle(&mut self, msg: Packet, _ctx: &mut Self::Context) -> Self::Result {
        let room_id = self.room.clone();
        trace!(
            "got message and sending to chat session {} email {} room {}",
            self.id.clone(),
            self.email.clone(),
            room_id
        );
        self.addr.do_send(ClientMessage {
            session: self.id.clone(),
            user: self.email.clone(),
            room: room_id,
            msg,
        });
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for WsChatSession {
    fn handle(&mut self, item: Result<ws::Message, ws::ProtocolError>, ctx: &mut Self::Context) {
        let msg = match item {
            Ok(msg) => msg,
            Err(err) => {
                error!("protocol error 2 {:?}", err);
                ctx.stop();
                return;
            }
        };

        match msg {
            ws::Message::Binary(msg) => {
                let msg_bytes = msg.to_vec();

                // Track received data
                let data_tracker = DataTracker::new(self.tracker_sender.clone());
                data_tracker.track_received(&self.id, msg_bytes.len() as u64);

                // Check if this is an RTT packet that should be echoed back
                if self.is_rtt_packet(&msg_bytes) {
                    trace!("Echoing RTT packet back to sender: {}", self.email);
                    // Track sent data for echo
                    let data_tracker = DataTracker::new(self.tracker_sender.clone());
                    data_tracker.track_sent(&self.id, msg_bytes.len() as u64);
                    ctx.binary(msg_bytes);
                } else if health_processor::is_health_packet_bytes(&msg_bytes) {
                    // Process health packet for diagnostics (don't relay to other peers)
                    health_processor::process_health_packet_bytes(
                        &msg_bytes,
                        self.nats_client.clone(),
                    );
                } else {
                    // Normal packet processing - forward to chat server
                    ctx.notify(Packet {
                        data: Arc::new(msg_bytes),
                    });
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
                    self.id, self.room
                );

                // Send Leave message to ChatServer (which will handle session end via SessionManager)
                self.addr.do_send(Leave {
                    session: self.id.clone(),
                    room: self.room.clone(),
                    user_id: self.creator_id.clone(),
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

impl WsChatSession {
    fn join(&self, room_id: String, ctx: &mut WebsocketContext<Self>) {
        let join_room = self.addr.send(JoinRoom {
            room: room_id.clone(),
            session: self.id.clone(),
            user_id: self.creator_id.clone(),
        });
        let join_room = join_room.into_actor(self);
        join_room
            .then(move |response, act, ctx| {
                match response {
                    Ok(res) if res.is_ok() => {
                        act.room = room_id;
                    }
                    Ok(res) => {
                        error!("error {:?}", res);
                    }
                    Err(err) => {
                        error!("error {:?}", err);
                        ctx.stop();
                    }
                }
                fut::ready(())
            })
            .wait(ctx);
    }
}

// ==========================================================================
// Meeting Lifecycle Integration Test (WebSocket)
// ==========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::actors::chat_server::ChatServer;
    use crate::models::meeting::Meeting;
    use crate::models::session_participant::SessionParticipant;
    use crate::server_diagnostics::ServerDiagnostics;
    use crate::session_manager::SessionManager;
    use actix::Actor;
    use actix_web::{web, App, HttpRequest, HttpServer};
    use actix_web_actors::ws;
    use futures_util::StreamExt;
    use protobuf::Message as ProtoMessage;
    use serial_test::serial;
    use sqlx::PgPool;
    use std::time::Duration;
    use tokio_tungstenite::tungstenite::Message;

    async fn get_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    async fn cleanup_room(pool: &PgPool, room_id: &str) {
        let _ = sqlx::query("DELETE FROM session_participants WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
    }

    /// Start WebSocket server for testing
    async fn start_websocket_server(pool: PgPool, port: u16) {
        let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
        let nats_client = async_nats::connect(&nats_url)
            .await
            .expect("Failed to connect to NATS");

        let chat = ChatServer::new(nats_client.clone(), Some(pool.clone()))
            .await
            .start();
        let session_manager = SessionManager::new(Some(pool));

        let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());

        // Use actix_rt::spawn which doesn't require Send
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

    async fn wait_for_participant_count(
        pool: &PgPool,
        room_id: &str,
        expected: i64,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let count = SessionParticipant::count_active(pool, room_id)
                .await
                .unwrap_or(-1);
            if count == expected {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        anyhow::bail!("Timeout waiting for participant count to be {expected}")
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
        println!("=== STARTING MEETING LIFECYCLE TEST (WebSocket) ===");

        let pool = get_test_pool().await;
        let room_id = "ws-meeting-lifecycle-test";
        let port = 18080; // Use a unique port for testing

        cleanup_room(&pool, room_id).await;

        println!("Starting WebSocket server on port {port}...");
        start_websocket_server(pool.clone(), port).await;

        // Wait for server to be ready
        wait_for_server_ready(port).await;
        println!("✓ Server ready");

        // ========== STEP 1: First user connects - meeting should be created ==========
        println!("\n--- Step 1: Alice connects (first participant) ---");

        let mut ws_alice = connect_ws_client(port, room_id, "alice")
            .await
            .expect("connect alice");
        wait_for_meeting_started(&mut ws_alice, Duration::from_secs(5)).await?;
        println!("✓ Alice connected and received MEETING_STARTED");

        // Verify: 1 participant, meeting exists
        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(
            count, 1,
            "Should have 1 active participant after Alice joins"
        );
        println!("✓ Participant count: {count}");

        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert!(meeting.is_some(), "Meeting should exist");
        let meeting = meeting.unwrap();
        assert_eq!(meeting.creator_id, Some("alice".to_string()));
        assert!(meeting.ended_at.is_none());
        println!("✓ Meeting created with creator=alice");

        // ========== STEP 2: Second user connects ==========
        println!("\n--- Step 2: Bob connects (second participant) ---");

        let mut ws_bob = connect_ws_client(port, room_id, "bob")
            .await
            .expect("connect bob");
        wait_for_meeting_started(&mut ws_bob, Duration::from_secs(5)).await?;
        println!("✓ Bob connected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 2, "Should have 2 active participants");
        println!("✓ Participant count: {count}");

        // ========== STEP 3: Third user connects ==========
        println!("\n--- Step 3: Charlie connects (third participant) ---");

        let mut ws_charlie = connect_ws_client(port, room_id, "charlie")
            .await
            .expect("connect charlie");
        wait_for_meeting_started(&mut ws_charlie, Duration::from_secs(5)).await?;
        println!("✓ Charlie connected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 3, "Should have 3 active participants");
        println!("✓ Participant count: {count}");

        // ========== STEP 4: Charlie disconnects - count drops ==========
        println!("\n--- Step 4: Charlie disconnects ---");

        drop(ws_charlie);
        wait_for_participant_count(&pool, room_id, 2, Duration::from_secs(5)).await?;
        println!("✓ Charlie disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 2, "Should have 2 active participants");
        println!("✓ Participant count: {count}");

        // Meeting should still be active
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert!(meeting.ended_at.is_none(), "Meeting should still be active");
        println!("✓ Meeting still active");

        // ========== STEP 5: Bob disconnects ==========
        println!("\n--- Step 5: Bob disconnects ---");

        drop(ws_bob);
        wait_for_participant_count(&pool, room_id, 1, Duration::from_secs(5)).await?;
        println!("✓ Bob disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 1, "Should have 1 active participant");
        println!("✓ Participant count: {count}");

        // ========== STEP 6: Alice (host/last) disconnects - meeting ends ==========
        println!("\n--- Step 6: Alice (host) disconnects - meeting should end ---");

        drop(ws_alice);
        wait_for_participant_count(&pool, room_id, 0, Duration::from_secs(5)).await?;
        println!("✓ Alice disconnected");

        let count = SessionParticipant::count_active(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        assert_eq!(count, 0, "Should have 0 active participants");
        println!("✓ Participant count: {count}");

        // Meeting should be ended
        let meeting = Meeting::get_by_room_id_async(&pool, room_id)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .unwrap();
        assert!(meeting.ended_at.is_some(), "Meeting should be ended");
        println!("✓ Meeting ended at {:?}", meeting.ended_at);

        // ========== CLEANUP ==========
        cleanup_room(&pool, room_id).await;

        println!("\n=== MEETING LIFECYCLE TEST PASSED (WebSocket) ===");
        Ok(())
    }
}
