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

//! Integration tests for the host-initiated kick endpoint.
//!
//! Covers:
//! - `POST /api/v1/meetings/{meeting_id}/kick` — host removes one participant
//!
//! NATS is `None` in [`build_app`]; the publish calls are no-ops so no NATS
//! assertions are made here. Mirrors `host_disable_video_tests.rs`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
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

/// Create a meeting, have the host join (activates it), then have an attendee
/// join. The attendee is left in `waiting` status — the host does NOT admit them.
async fn setup_with_waiting_participant(pool: &sqlx::PgPool, room_id: &str) {
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

/// Look up the participant's DB status. Panics if the meeting or participant row is missing.
async fn fetch_status(pool: &sqlx::PgPool, room_id: &str, user_id: &str) -> String {
    let meeting = db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id query failed")
        .expect("meeting row should exist");
    let row = db_participants::get_status(pool, meeting.id, user_id)
        .await
        .expect("get_status query failed")
        .expect("participant row should exist");
    row.status
}

// ── POST /kick ───────────────────────────────────────────────────────────

/// Host successfully kicks an admitted participant → 200, success=true, and the
/// DB row transitions to `status='kicked'`.
#[tokio::test]
#[serial]
async fn host_kicks_participant_returns_200() {
    let pool = get_test_pool().await;
    let room_id = "test-host-kicks-participant";
    setup_with_admitted_participant(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<serde_json::Value> = response_json(resp).await;
    assert!(body.success);

    // The kick() DB function should have transitioned the admitted row to 'kicked'.
    let status = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(status, "kicked");

    cleanup_test_data(&pool, room_id).await;
}

/// After a kick, the participant's DB row has `status == "kicked"` — focuses the
/// assertion squarely on the DB side-effect.
#[tokio::test]
#[serial]
async fn kick_sets_participant_status_to_kicked() {
    let pool = get_test_pool().await;
    let room_id = "test-kick-sets-status-kicked";
    setup_with_admitted_participant(&pool, room_id).await;

    // Sanity check: participant is admitted before the kick.
    let pre = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(pre, "admitted");

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let post = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(post, "kicked");

    cleanup_test_data(&pool, room_id).await;
}

/// A non-host user (never joined) calling `/kick` is rejected with HTTP 403.
#[tokio::test]
#[serial]
async fn non_host_kick_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-non-host-kick-forbidden";
    setup_active_meeting(&pool, room_id).await;

    // "other@example.com" never joined; they are not in the participants table.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
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

/// An admitted (non-host) participant calling `/kick` is rejected with HTTP 403.
/// Exercises the `!row.is_host` branch of `require_host`.
#[tokio::test]
#[serial]
async fn admitted_non_host_kick_returns_403() {
    let pool = get_test_pool().await;
    let room_id = "test-admitted-non-host-kick-forbidden";
    setup_with_admitted_participant(&pool, room_id).await;

    // participant@example.com is admitted but is NOT the host.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
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
#[tokio::test]
#[serial]
async fn kick_empty_user_id_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-kick-empty-user-id";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
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
async fn host_kicks_self_returns_400() {
    let pool = get_test_pool().await;
    let room_id = "test-host-kicks-self";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
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

/// Calling `/kick` on a meeting that does not exist returns HTTP 404.
#[tokio::test]
#[serial]
async fn kick_nonexistent_meeting_returns_404() {
    let pool = get_test_pool().await;
    let room_id = "nonexistent-meeting-for-kick";

    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
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

/// Kicking a `waiting` participant is a no-op at the DB level: `kick()` only
/// transitions from `'admitted'`, so the row stays in `'waiting'`. The endpoint
/// still returns 200 (the NATS publish succeeds regardless).
#[tokio::test]
#[serial]
async fn kick_waiting_participant_does_not_change_status() {
    let pool = get_test_pool().await;
    let room_id = "test-kick-waiting-no-op";
    setup_with_waiting_participant(&pool, room_id).await;

    // Sanity: participant is in waiting state (waiting_room_enabled is true by default).
    let pre = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(pre, "waiting");

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    // Endpoint returns 200 — NATS publish is None-op in tests, and the DB
    // update simply affects 0 rows for a non-admitted target.
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<serde_json::Value> = response_json(resp).await;
    assert!(body.success);

    // The DB row must still be 'waiting'; kick() only transitions admitted rows.
    let post = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(post, "waiting");

    cleanup_test_data(&pool, room_id).await;
}

/// A kicked participant can rejoin the meeting via `/join`. After rejoin their
/// status is no longer `'kicked'` (it becomes either `'waiting'` or `'admitted'`
/// depending on the meeting's waiting-room setting).
#[tokio::test]
#[serial]
async fn kicked_participant_can_rejoin() {
    let pool = get_test_pool().await;
    let room_id = "test-kicked-participant-rejoin";
    setup_with_admitted_participant(&pool, room_id).await;

    // Kick the participant.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/kick"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"participant@example.com"}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let kicked_status = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_eq!(kicked_status, "kicked");

    // Participant rejoins via /join.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "participant@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Their row must no longer be 'kicked' — they're either back in the waiting
    // room or admitted directly (depending on the meeting's waiting_room_enabled
    // setting). Both are valid post-rejoin states.
    let post = fetch_status(&pool, room_id, "participant@example.com").await;
    assert_ne!(
        post, "kicked",
        "after /join, participant status must no longer be 'kicked' (got {post})"
    );
    assert!(
        post == "waiting" || post == "admitted",
        "unexpected post-rejoin status: {post}"
    );

    cleanup_test_data(&pool, room_id).await;
}
