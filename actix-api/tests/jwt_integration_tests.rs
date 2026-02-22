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
//! Verified scenarios:
//! - Valid tokens issued by meeting-api allow connection via `GET /lobby?token=`
//! - Expired / invalid / unauthorized tokens are rejected
//! - The deprecated `GET /lobby/{email}/{room}` works only when FF=off
//! - The deprecated endpoint returns 410 Gone when FF=on

use actix::Actor;
use actix_web::{web, App, HttpServer};
use sec_api::{
    actors::chat_server::ChatServer,
    lobby::{ws_connect, ws_connect_authenticated},
    models::AppState,
    server_diagnostics::ServerDiagnostics,
    session_manager::SessionManager,
    test_utils,
};
use serial_test::serial;
use std::time::Duration;
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
    let nats_client = async_nats::ConnectOptions::new()
        .no_echo()
        .connect(&nats_url)
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
            // Register the REAL production handlers from sec_api::lobby
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
    // Probe with the deprecated endpoint (FF is off during setup)
    let url = format!("ws://127.0.0.1:{port}/lobby/probe/probe");
    for _ in 0..50 {
        if tokio_tungstenite::connect_async(&url).await.is_ok() {
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
// Connection helpers
// =========================================================================

/// Connect via the primary token-based endpoint: GET /lobby?token=<JWT>
async fn try_connect_with_token(
    port: u16,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u16,
> {
    let url = format!(
        "ws://127.0.0.1:{port}/lobby?token={token}",
        token = urlencoding::encode(token)
    );
    match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("unexpected connection error: {e}"),
    }
}

/// Connect via the deprecated path-based endpoint: GET /lobby/{email}/{room}
async fn try_connect_deprecated(
    port: u16,
    email: &str,
    room: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    u16,
> {
    let url = format!("ws://127.0.0.1:{port}/lobby/{email}/{room}");
    match tokio_tungstenite::connect_async(&url).await {
        Ok((ws, _)) => Ok(ws),
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => Err(resp.status().as_u16()),
        Err(e) => panic!("unexpected connection error: {e}"),
    }
}

// =========================================================================
// Tests: token-based endpoint with REAL meeting-api token generation
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_valid_meeting_api_token_connects() {
    let port = JWT_PORT;
    setup(port).await;

    // Use the REAL meeting-api token generation
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

    let mut ws = result.unwrap();
    test_utils::wait_for_meeting_started(&mut ws, Duration::from_secs(5))
        .await
        .expect("should receive MEETING_STARTED from real server");
    drop(ws);
}

#[actix_rt::test]
#[serial]
async fn test_expired_meeting_api_token_rejected() {
    let port = JWT_PORT + 1;
    setup(port).await;

    // Generate a token that expired 120 seconds ago (past the 60s leeway)
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

    // Token signed with a different secret than what the server uses
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

    // The token contains identity and room -- no need for URL params
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

    let mut ws = result.unwrap();
    test_utils::wait_for_meeting_started(&mut ws, Duration::from_secs(5))
        .await
        .expect("should receive MEETING_STARTED");
    drop(ws);
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

    let mut ws = result.unwrap();
    test_utils::wait_for_meeting_started(&mut ws, Duration::from_secs(5))
        .await
        .expect("should receive MEETING_STARTED");
    drop(ws);

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

    // Host token
    let host_token = meeting_api::token::generate_room_token(
        JWT_SECRET,
        TOKEN_TTL_SECS,
        "host@company.com",
        room,
        true,
        "Host Alice",
    )
    .expect("should generate host token");

    // Attendee token
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
    let mut ws_host = host_result.unwrap();
    test_utils::wait_for_meeting_started(&mut ws_host, Duration::from_secs(5))
        .await
        .expect("host should receive MEETING_STARTED");

    let attendee_result = try_connect_with_token(port, &attendee_token).await;
    assert!(attendee_result.is_ok(), "attendee token should connect");
    let mut ws_attendee = attendee_result.unwrap();
    test_utils::wait_for_meeting_started(&mut ws_attendee, Duration::from_secs(5))
        .await
        .expect("attendee should receive MEETING_STARTED");

    drop(ws_host);
    drop(ws_attendee);
}
