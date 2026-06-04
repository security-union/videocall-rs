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

//! Integration tests for host-initiated disable-video endpoints.
//!
//! Covers:
//! - `POST /api/v1/meetings/{meeting_id}/disable-video`     — host asks one participant
//! - `POST /api/v1/meetings/{meeting_id}/disable-video-all` — host asks every participant
//!
//! NATS is `None` in [`build_app`]; the publish calls are no-ops so no NATS
//! assertions are made here. Mirrors `host_mute_tests.rs`.

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

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "participant@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

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

// ── POST /disable-video ───────────────────────────────────────────────────

/// Host successfully asks an admitted participant to disable their camera → 200.
#[tokio::test]
#[serial]
async fn host_disables_video_for_participant_returns_200() {
    let pool = get_test_pool().await;
    let room_id = "test-host-disables-video-participant";
    setup_with_admitted_participant(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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

/// A non-host user calling `/disable-video` is rejected with HTTP 403.
#[tokio::test]
#[serial]
async fn non_host_disable_video_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-non-host-disable-video-forbidden";
    setup_active_meeting(&pool, room_id).await;

    // "other@example.com" never joined; they are not in the participants table.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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

/// An admitted (non-host) participant calling `/disable-video` is rejected
/// with HTTP 403. Exercises the `!row.is_host` branch of `require_host`.
#[tokio::test]
#[serial]
async fn admitted_non_host_disable_video_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-admitted-non-host-disable-video-forbidden";
    setup_with_admitted_participant(&pool, room_id).await;

    // participant@example.com is admitted but is NOT the host.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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
/// The server rejects empty user_id to prevent misuse (use /disable-video-all).
#[tokio::test]
#[serial]
async fn disable_video_empty_user_id_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-disable-video-empty-user-id";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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

/// Host sending their own `user_id` in the body is rejected with HTTP 400.
#[tokio::test]
#[serial]
async fn host_disables_video_self_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-host-disables-video-self";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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

/// Calling `/disable-video` on a meeting that does not exist returns HTTP 404.
#[tokio::test]
#[serial]
async fn disable_video_nonexistent_meeting_returns_404() {
    let pool = get_test_pool().await;
    let room_id = "nonexistent-meeting-for-disable-video";

    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video"),
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

// ── POST /disable-video-all ──────────────────────────────────────────────

/// Non-host calling `/disable-video-all` is rejected with HTTP 403.
#[tokio::test]
#[serial]
async fn non_host_disable_video_all_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-non-host-disable-video-all-forbidden";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video-all"),
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

/// Host calling `/disable-video-all` on an active meeting returns HTTP 200.
#[tokio::test]
#[serial]
async fn disable_video_all_returns_200() {
    let pool = get_test_pool().await;
    let room_id = "test-disable-video-all-host";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/disable-video-all"),
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
