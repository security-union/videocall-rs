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

//! Integration tests for the `POST /api/v1/chat/token` endpoint.
//!
//! This endpoint exchanges a valid videocall session for a chat-service bearer
//! token by calling an external chat service with server-side credentials.
//!
//! Tests cover:
//! - 404 when chat is not configured
//! - 400 for empty meeting_id
//! - 401 without auth
//! - Correct room_id derivation from prefix + meeting_id
//! - 502 when the external chat service returns an error

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::{routes, state::AppState, token::generate_session_token};
use sqlx::postgres::PgPoolOptions;
use test_helpers::TEST_JWT_SECRET;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ChatTokenResponse};
use videocall_meeting_types::APIError;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_SESSION_TTL: i64 = 3600;

/// Create a lazy PgPool that never actually connects (the chat endpoint does
/// not query the database, so no real connection is needed).
fn lazy_pool() -> sqlx::PgPool {
    PgPoolOptions::new()
        .max_connections(1)
        .connect_lazy("postgres://localhost/unused")
        .expect("lazy pool creation should not fail")
}

/// Build a router with a custom AppState for chat testing.
fn build_chat_app(state: AppState) -> axum::Router {
    routes::router().with_state(state)
}

/// Build an AppState with chat NOT configured (default state).
fn state_without_chat() -> AppState {
    AppState {
        db: lazy_pool(),
        jwt_secret: TEST_JWT_SECRET.to_string(),
        token_ttl_secs: 600,
        session_ttl_secs: TEST_SESSION_TTL,
        oauth: None,
        jwks_cache: None,
        cookie_domain: None,
        cookie_name: "session".to_string(),
        cookie_secure: false,
        nats: None,
        service_version_urls: Vec::new(),
        http_client: reqwest::Client::new(),
        chat_service_url: None,
        chat_service_api_key: None,
        chat_room_prefix: String::new(),
    }
}

/// Build an AppState with chat configured, pointing to the given mock server URL.
fn state_with_chat(mock_url: &str, api_key: &str, room_prefix: &str) -> AppState {
    AppState {
        chat_service_url: Some(mock_url.to_string()),
        chat_service_api_key: Some(api_key.to_string()),
        chat_room_prefix: room_prefix.to_string(),
        ..state_without_chat()
    }
}

/// Build an authenticated POST request with a signed session JWT cookie.
fn authed_chat_request(email: &str, body: &str) -> axum::http::Request<Body> {
    let jwt = generate_session_token(TEST_JWT_SECRET, email, email, TEST_SESSION_TTL)
        .expect("signing session JWT for test should not fail");
    axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/chat/token")
        .header("Cookie", format!("session={jwt}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Test 1: Returns 404 when chat service is not configured
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_token_returns_404_when_chat_not_configured() {
    let state = state_without_chat();
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":"standup"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "CHAT_NOT_CONFIGURED");
}

// ---------------------------------------------------------------------------
// Test 2: Returns 400 for empty meeting_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_token_returns_400_for_empty_meeting_id() {
    // Chat must be configured so we get past the 404 check.
    // The mock server won't be called because validation fails first.
    let mock_server = MockServer::start().await;
    let state = state_with_chat(&mock_server.uri(), "test-key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":""}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "INVALID_MEETING_ID");
}

#[tokio::test]
async fn test_chat_token_returns_400_for_whitespace_only_meeting_id() {
    let mock_server = MockServer::start().await;
    let state = state_with_chat(&mock_server.uri(), "test-key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":"   "}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "INVALID_MEETING_ID");
}

// ---------------------------------------------------------------------------
// Test 3: Returns 401 without auth
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_token_returns_401_without_auth() {
    let state = state_without_chat();
    let app = build_chat_app(state);

    // No session cookie, no Bearer header.
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v1/chat/token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"meeting_id":"standup"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "UNAUTHORIZED");
}

// ---------------------------------------------------------------------------
// Test 4: Derives correct room_id from prefix + meeting_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_token_derives_correct_room_id() {
    let mock_server = MockServer::start().await;

    let test_email = "alice@test.com";
    let expected_room_id = "test-standup";

    // Mock the external chat service's /auth/token endpoint.
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(header("Authorization", "Bearer my-api-key"))
        .and(body_json(serde_json::json!({
            "user_id": test_email,
            "display_name": test_email,
            "room_id": expected_room_id,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "chat-token-abc123",
            "expires_at": 1700000000_i64
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let state = state_with_chat(&mock_server.uri(), "my-api-key", "test-");
    let app = build_chat_app(state);

    let req = authed_chat_request(test_email, r#"{"meeting_id":"standup"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ChatTokenResponse> = test_helpers::response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.token, "chat-token-abc123");
    assert_eq!(body.result.room_id, expected_room_id);
    assert_eq!(body.result.expires_at, Some(1700000000));
}

#[tokio::test]
async fn test_chat_token_with_empty_prefix() {
    let mock_server = MockServer::start().await;

    let test_email = "bob@test.com";

    // With empty prefix, room_id should equal meeting_id.
    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .and(body_json(serde_json::json!({
            "user_id": test_email,
            "display_name": test_email,
            "room_id": "daily",
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "tok-daily",
        })))
        .expect(1)
        .mount(&mock_server)
        .await;

    let state = state_with_chat(&mock_server.uri(), "key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request(test_email, r#"{"meeting_id":"daily"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ChatTokenResponse> = test_helpers::response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.room_id, "daily");
    assert_eq!(body.result.token, "tok-daily");
    assert_eq!(body.result.expires_at, None);
}

// ---------------------------------------------------------------------------
// Test 5: Handles chat service errors gracefully (502)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_chat_token_returns_502_when_chat_service_returns_500() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let state = state_with_chat(&mock_server.uri(), "key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":"standup"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "CHAT_SERVICE_ERROR");
}

#[tokio::test]
async fn test_chat_token_returns_502_when_chat_service_unreachable() {
    // Point to a URL that won't have anything listening.
    let state = state_with_chat("http://127.0.0.1:1", "key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":"standup"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "CHAT_SERVICE_ERROR");
}

#[tokio::test]
async fn test_chat_token_returns_502_when_chat_service_returns_invalid_json() {
    let mock_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/auth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        .expect(1)
        .mount(&mock_server)
        .await;

    let state = state_with_chat(&mock_server.uri(), "key", "");
    let app = build_chat_app(state);

    let req = authed_chat_request("alice@test.com", r#"{"meeting_id":"standup"}"#);
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let body: APIResponse<APIError> = test_helpers::response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "CHAT_SERVICE_ERROR");
}
