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

//! Integration tests for host-initiated mute endpoints.
//!
//! Covers:
//! - `POST /api/v1/meetings/{meeting_id}/mute`     — host mutes a participant
//! - `POST /api/v1/meetings/{meeting_id}/mute-all` — host mutes every participant
//!
//! NATS is `None` in [`build_app`]; the publish calls are no-ops so no NATS
//! assertions are made here.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{responses::APIResponse, APIError};

// ── Setup helpers ────────────────────────────────────────────────────────

/// Create a meeting, have the host join (activates it), then have an attendee
/// join (enters the waiting room) and be admitted. Returns with the meeting
/// active and one non-host participant in `admitted` state.
async fn setup_with_admitted_participant(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    // Create meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host joins (activates).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Attendee joins (enters waiting room).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "participant@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host admits attendee.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// Create a meeting and have only the host join (activates it).
async fn setup_active_meeting(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

// ── POST /mute ───────────────────────────────────────────────────────────

/// Host successfully mutes an admitted participant → HTTP 200.
#[tokio::test]
#[serial]
async fn host_mutes_participant_returns_200() {
    let pool = get_test_pool().await;
    let room_id = "test-host-mutes-participant";
    setup_with_admitted_participant(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<serde_json::Value> = response_json(resp).await;
    assert!(body.success);

    cleanup_test_data(&pool, room_id).await;
}

/// A non-host user calling `/mute` is rejected with HTTP 403.
#[tokio::test]
#[serial]
async fn non_host_mute_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-non-host-mute-forbidden";
    setup_active_meeting(&pool, room_id).await;

    // "other@example.com" never joined; they are not in the participants table.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "other@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"host@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_HOST");

    cleanup_test_data(&pool, room_id).await;
}

/// An admitted (non-host) participant calling `/mute` is rejected with HTTP 403.
/// This specifically exercises the `!row.is_host` branch of `require_host`,
/// unlike `non_host_mute_returns_403` which uses an unknown user (None row).
#[tokio::test]
#[serial]
async fn admitted_non_host_mute_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-admitted-non-host-mute-forbidden";
    setup_with_admitted_participant(&pool, room_id).await;

    // participant@example.com is admitted but is NOT the host.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "participant@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"host@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_HOST");

    cleanup_test_data(&pool, room_id).await;
}

/// Host sending an empty `user_id` is rejected with HTTP 400.
/// The server rejects empty user_id to prevent misuse (use /mute-all for that).
#[tokio::test]
#[serial]
async fn mute_empty_user_id_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-mute-empty-user-id";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":""}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);

    cleanup_test_data(&pool, room_id).await;
}

/// Non-host calling `/mute-all` is rejected with HTTP 403.
#[tokio::test]
#[serial]
async fn non_host_mute_all_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-non-host-mute-all-forbidden";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute-all"),
        "other@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_HOST");

    cleanup_test_data(&pool, room_id).await;
}

/// Host sending their own `user_id` in the body is rejected with HTTP 400.
#[tokio::test]
#[serial]
async fn host_mutes_self_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-host-mutes-self";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"host@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);

    cleanup_test_data(&pool, room_id).await;
}

/// Calling `/mute` on a meeting that does not exist returns HTTP 404.
#[tokio::test]
#[serial]
async fn mute_nonexistent_meeting_returns_404() {
    let pool = get_test_pool().await;
    let room_id = "nonexistent-meeting-for-mute";

    // Make sure there is no stale data.
    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "MEETING_NOT_FOUND");
}

// ── POST /mute-all ───────────────────────────────────────────────────────

/// Host calling `/mute-all` on an active meeting returns HTTP 200.
#[tokio::test]
#[serial]
async fn mute_all_returns_200() {
    let pool = get_test_pool().await;
    let room_id = "test-mute-all-host";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/mute-all"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<serde_json::Value> = response_json(resp).await;
    assert!(body.success);

    cleanup_test_data(&pool, room_id).await;
}
