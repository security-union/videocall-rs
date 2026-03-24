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
 */

//! Integration tests for WebSocket session lifecycle (moved from inline `#[cfg(test)]` module).

use actix::Actor;
use actix_web::{web, App, HttpRequest, HttpServer};
use actix_web_actors::ws;
use futures_util::StreamExt;
use protobuf::Message as ProtoMessage;
use sec_api::actors::chat_server::ChatServer;
use sec_api::actors::transports::ws_chat_session::WsChatSession;
use sec_api::server_diagnostics::ServerDiagnostics;
use sec_api::session_manager::SessionManager;
use serial_test::serial;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// Test helper: create a database pool for future JWT flow integration tests.
#[allow(dead_code)]
async fn get_test_pool() -> sqlx::PgPool {
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
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
                                nats_client,
                                tracker_sender,
                                session_manager,
                                false, // tests use non-observer sessions
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
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
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
