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

//! Regression tests for the two `db_participants::join_attendee` call sites in
//! `join_as_attendee`:
//!
//! 1. **None host-check path** (`waiting_room_enabled=false`, meeting not yet
//!    active): `join_attendee` is called with `check_host_gone_for = None`.
//!    The invariant states it always returns `Some`; we replaced the former
//!    `.expect()` with `.ok_or_else(AppError::internal(...))` to eliminate the
//!    panic path.  This test verifies the happy path continues to work.
//!
//! 2. **Some host-check path** (active meeting, `end_on_host_leave=false`,
//!    `admitted_can_admit=false`): `join_attendee` is called with
//!    `check_host_gone_for = Some(creator_id)`.  When the host has left the
//!    meeting, the function returns `None`, and the handler must return
//!    `JOINING_NOT_ALLOWED` (HTTP 403) rather than panic.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, ParticipantStatusResponse},
    APIError,
};

// ── Test 1: None host-check path ────────────────────────────────────────

/// When a non-host attendee joins a meeting that has not yet been activated
/// and `waiting_room_enabled=false`, `join_as_attendee` must:
///  - auto-activate the meeting, and
///  - call `join_attendee` with `check_host_gone_for = None`,
///  - auto-admit the attendee and return a room token.
///
/// This is the branch where `.expect()` was replaced by `.ok_or_else(…)`.
#[tokio::test]
#[serial]
async fn test_attendee_joins_inactive_meeting_no_waiting_room_gets_admitted() {
    let pool = get_test_pool().await;
    let room_id = "test-invariant-none-host-check";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting with waiting room disabled.  The host does NOT join,
    // so the meeting remains in the "idle" state.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": false
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Attendee (non-host) joins the idle meeting.  This must hit the
    // `current_state != "active" && !waiting_room_enabled` branch.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Attendee"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Attendee joining idle no-WR meeting should succeed"
    );

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(
        body.result.status, "admitted",
        "Attendee should be auto-admitted when waiting room is disabled"
    );
    assert!(
        body.result.room_token.is_some(),
        "Auto-admitted attendee should receive a room_token"
    );
    assert!(!body.result.is_host);
    assert!(
        !body.result.waiting_room_enabled,
        "Response should reflect waiting_room_enabled=false"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Test 2: Some host-check path ────────────────────────────────────────

/// When `end_on_host_leave=false` and `admitted_can_admit=false` (the
/// host-check is active), and the host has left a still-active meeting, a
/// new attendee joining must receive HTTP 403 `JOINING_NOT_ALLOWED` rather
/// than a panic.
///
/// Setup:
///  1. Create meeting with `end_on_host_leave=false`, `waiting_room_enabled=false`
///     (so the first attendee gets auto-admitted, keeping the meeting alive
///     when the host leaves).
///  2. Host joins → meeting activates.
///  3. Attendee A joins → auto-admitted (keeps meeting active after host leaves).
///  4. Host leaves → meeting stays active (1 admitted participant remains).
///  5. Attendee B tries to join → host is gone → `JOINING_NOT_ALLOWED`.
#[tokio::test]
#[serial]
async fn test_attendee_blocked_when_host_gone_and_end_on_host_leave_false() {
    let pool = get_test_pool().await;
    let room_id = "test-invariant-some-host-check";
    cleanup_test_data(&pool, room_id).await;

    // 1. Create meeting: no waiting room, host does not end meeting on leave.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": false,
                "end_on_host_leave": false
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // 2. Host joins → activates the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. Attendee A joins and is auto-admitted.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee-a@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");

    // 4. Host leaves.  Because `end_on_host_leave=false` and there is still
    //    one admitted participant, the meeting remains active.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. Attendee B tries to join.  The host-gone check
    //    (`check_host_gone_for = Some(creator_id)`) fires inside the
    //    transaction, `join_attendee` returns None, and the handler must
    //    respond with 403 JOINING_NOT_ALLOWED — not panic.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee-b@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "New attendee should be blocked when host has left and end_on_host_leave=false"
    );

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(
        body.result.code, "JOINING_NOT_ALLOWED",
        "Error code must be JOINING_NOT_ALLOWED"
    );

    cleanup_test_data(&pool, room_id).await;
}
