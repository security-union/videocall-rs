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

//! Integration tests for JWT room access token validation on the Media Server.
//!
//! These tests start the **real** WebSocket server with the **real** production
//! lobby handlers (`sec_api::lobby`) and use the **real** token generation from
//! the Meeting Backend (`meeting_api::token::generate_room_token`).
//!
//! Transport: uses [`NativeWebSocketClient`] from `videocall-transport` — the
//! same shared transport layer used by all native clients (bot, CLI, etc.).
//!
//! Verified scenarios:
//! - Valid tokens issued by meeting-api allow connection via `GET /lobby?token=`
//! - Expired / invalid / unauthorized tokens are rejected
//! - The deprecated `GET /lobby/{email}/{room}` works only when FF=off
//! - The deprecated endpoint returns 410 Gone when FF=on

use actix::Actor;
use actix_web::{web, App, HttpServer};
use protobuf::Message as ProtoMessage;
use sec_api::{
    actors::chat_server::ChatServer,
    lobby::{ws_connect, ws_connect_authenticated},
    models::AppState,
    server_diagnostics::ServerDiagnostics,
    session_manager::SessionManager,
};
use serial_test::serial;
use std::time::Duration;
use videocall_transport::native_websocket::{NativeWebSocketClient, WebSocketConnectError};
use videocall_types::FeatureFlags;

const JWT_SECRET: &str = "test-secret-for-integration-tests";
const TOKEN_TTL_SECS: i64 = 60;
const JWT_PORT: u16 = 18090;

// =========================================================================
// Server helpers -- starts the REAL server with REAL handlers
// =========================================================================

/// Start the real WebSocket server with the production lobby handlers.
async fn start_real_ws_server(port: u16) {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat = ChatServer::new(nats_client.clone()).await.start();
    let session_manager = SessionManager::new();
    let (_, tracker_sender, _) = ServerDiagnostics::new_with_channel(nats_client.clone());

    let state = AppState {
        chat,
        nats_client,
        tracker_sender,
        session_manager,
    };

    actix_rt::spawn(async move {
        let _ = HttpServer::new(move || {
            App::new()
                .app_data(web::Data::new(state.clone()))
                .service(ws_connect_authenticated)
                .service(ws_connect)
        })
        .bind(format!("127.0.0.1:{port}"))
        .expect("Failed to bind JWT test server")
        .run()
        .await;
    });
}

async fn wait_for_server(port: u16) {
    let url = format!("ws://127.0.0.1:{port}/lobby/probe/probe");
    for _ in 0..50 {
        if NativeWebSocketClient::connect(&url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("WS server not ready after 5 seconds on port {port}");
}

/// Ensure the server is started and the FF is off (clean slate).
async fn setup(port: u16) {
    FeatureFlags::clear_meeting_management_override();
    std::env::set_var("JWT_SECRET", JWT_SECRET);
    start_real_ws_server(port).await;
    wait_for_server(port).await;
}

// =========================================================================
// Connection helpers using videocall-transport
// =========================================================================

/// Connect via the primary token-based endpoint: GET /lobby?token=<JWT>
///
/// Returns `Ok(client, inbound_rx)` on success, or `Err(http_status)` when
/// the server rejects the WebSocket upgrade.
async fn try_connect_with_token(
    port: u16,
    token: &str,
) -> Result<(NativeWebSocketClient, tokio::sync::mpsc::Receiver<Vec<u8>>), u16> {
    let url = format!(
        "ws://127.0.0.1:{port}/lobby?token={token}",
        token = urlencoding::encode(token)
    );
    NativeWebSocketClient::try_connect(&url)
        .await
        .map_err(|e| match e {
            WebSocketConnectError::HttpError { status } => status,
            other => panic!("unexpected connection error: {other}"),
        })
}

/// Connect via the deprecated path-based endpoint: GET /lobby/{email}/{room}
async fn try_connect_deprecated(
    port: u16,
    email: &str,
    room: &str,
) -> Result<(NativeWebSocketClient, tokio::sync::mpsc::Receiver<Vec<u8>>), u16> {
    let url = format!("ws://127.0.0.1:{port}/lobby/{email}/{room}");
    NativeWebSocketClient::try_connect(&url)
        .await
        .map_err(|e| match e {
            WebSocketConnectError::HttpError { status } => status,
            other => panic!("unexpected connection error: {other}"),
        })
}

/// Wait for the MEETING_STARTED protobuf packet from the real server.
async fn wait_for_meeting_started(
    inbound_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
    use videocall_types::protos::meeting_packet::MeetingPacket;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
    use videocall_types::protos::packet_wrapper::PacketWrapper;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            msg = inbound_rx.recv() => {
                if let Some(data) = msg {
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

// =========================================================================
// Tests: token-based endpoint with REAL meeting-api token generation
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_valid_meeting_api_token_connects() {
    let port = JWT_PORT;
    setup(port).await;

    let token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        "alice@test.com",
        "jwt-room-1",
        true,
        "Alice",
    )
    .expect("meeting-api should generate a valid token");

    let result = try_connect_with_token(port, &token).await;
    assert!(result.is_ok(), "valid meeting-api token should connect");

    let (_client, mut rx) = result.unwrap();
    wait_for_meeting_started(&mut rx)
        .await
        .expect("should receive MEETING_STARTED from real server");
}

#[actix_rt::test]
#[serial]
async fn test_expired_meeting_api_token_rejected() {
    let port = JWT_PORT + 1;
    setup(port).await;

    let token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        -120,
        "alice@test.com",
        "jwt-room-2",
        false,
        "Alice",
    )
    .expect("should generate token");

    let result = try_connect_with_token(port, &token).await;
    assert!(result.is_err(), "expired token should be rejected");
    assert_eq!(result.unwrap_err(), 401);
}

#[actix_rt::test]
#[serial]
async fn test_wrong_secret_token_rejected() {
    let port = JWT_PORT + 2;
    setup(port).await;

    let token = meeting_api::token::generate_room_token(
        "completely-different-secret",
        TOKEN_TTL_SECS,
        "alice@test.com",
        "jwt-room-3",
        false,
        "Alice",
    )
    .expect("should generate token");

    let result = try_connect_with_token(port, &token).await;
    assert!(result.is_err(), "wrong-secret token should be rejected");
    assert_eq!(result.unwrap_err(), 403);
}

#[actix_rt::test]
#[serial]
async fn test_garbage_token_rejected() {
    let port = JWT_PORT + 3;
    setup(port).await;

    let result = try_connect_with_token(port, "not.a.real.jwt").await;
    assert!(result.is_err(), "garbage token should be rejected");
    assert_eq!(result.unwrap_err(), 403);
}

#[actix_rt::test]
#[serial]
async fn test_token_identity_extracted_from_jwt() {
    let port = JWT_PORT + 4;
    setup(port).await;

    let token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        "bob@example.com",
        "my-special-room",
        false,
        "Bob",
    )
    .expect("should generate token");

    let result = try_connect_with_token(port, &token).await;
    assert!(result.is_ok(), "token with identity in claims should work");

    let (_client, mut rx) = result.unwrap();
    wait_for_meeting_started(&mut rx)
        .await
        .expect("should receive MEETING_STARTED");
}

// =========================================================================
// Tests: deprecated endpoint with REAL handlers
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_deprecated_endpoint_works_when_ff_off() {
    let port = JWT_PORT + 5;
    setup(port).await;
    FeatureFlags::set_meeting_management_override(false);

    let result = try_connect_deprecated(port, "alice", "jwt-room-6").await;
    assert!(
        result.is_ok(),
        "deprecated endpoint with FF=off should allow connection"
    );

    let (_client, mut rx) = result.unwrap();
    wait_for_meeting_started(&mut rx)
        .await
        .expect("should receive MEETING_STARTED");

    FeatureFlags::clear_meeting_management_override();
}

#[actix_rt::test]
#[serial]
async fn test_deprecated_endpoint_returns_gone_when_ff_on() {
    let port = JWT_PORT + 6;
    setup(port).await;
    FeatureFlags::set_meeting_management_override(true);

    let result = try_connect_deprecated(port, "alice", "jwt-room-7").await;
    assert!(
        result.is_err(),
        "deprecated endpoint with FF=on should return 410 Gone"
    );
    assert_eq!(result.unwrap_err(), 410);

    FeatureFlags::clear_meeting_management_override();
}

// =========================================================================
// Tests: host vs attendee tokens with REAL meeting-api generation
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_host_and_attendee_tokens_both_connect() {
    let port = JWT_PORT + 7;
    setup(port).await;

    let room = "team-standup";

    let host_token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        "host@company.com",
        room,
        true,
        "Host Alice",
    )
    .expect("should generate host token");

    let attendee_token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        "attendee@company.com",
        room,
        false,
        "Attendee Bob",
    )
    .expect("should generate attendee token");

    // Both should connect to the REAL server
    let host_result = try_connect_with_token(port, &host_token).await;
    assert!(host_result.is_ok(), "host token should connect");
    let (_host_client, mut host_rx) = host_result.unwrap();
    wait_for_meeting_started(&mut host_rx)
        .await
        .expect("host should receive MEETING_STARTED");

    let attendee_result = try_connect_with_token(port, &attendee_token).await;
    assert!(attendee_result.is_ok(), "attendee token should connect");
    let (_attendee_client, mut attendee_rx) = attendee_result.unwrap();
    wait_for_meeting_started(&mut attendee_rx)
        .await
        .expect("attendee should receive MEETING_STARTED");
}
