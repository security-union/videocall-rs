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

//! Integration tests for meeting CRUD endpoints.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{
        APIResponse, CreateMeetingResponse, DeleteMeetingResponse, ListMeetingsResponse,
        MeetingInfoResponse,
    },
    APIError,
};

/// Lower bound for any Unix epoch timestamp emitted as **milliseconds**.
///
/// 1_000_000_000_000 ms = 2001-09-09 — well before any meeting in this
/// codebase could exist. A timestamp at or above this floor is conclusively
/// in milliseconds; one below it (but above 0) is almost certainly seconds,
/// which is the regression these checks guard against.
const MS_LOWER_BOUND: i64 = 1_000_000_000_000;

// ── Create ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_create_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-create-meeting-success";
    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());

    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": ["user1@example.com", "user2@example.com"],
                "password": "secret123"
            }))
            .unwrap(),
        ))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.host, "host@example.com");
    assert!(body.result.has_password);
    assert_eq!(body.result.attendees.len(), 2);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_create_meeting_generates_id() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"attendees":[]}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
    assert!(body.success);
    assert!(!body.result.meeting_id.is_empty());
    assert_eq!(body.result.meeting_id.len(), 12);

    cleanup_test_data(&pool, &body.result.meeting_id).await;
}

#[tokio::test]
#[serial]
async fn test_create_meeting_duplicate_id() {
    let pool = get_test_pool().await;
    let room_id = "test-duplicate-meeting";
    cleanup_test_data(&pool, room_id).await;

    let payload = serde_json::to_string(&serde_json::json!({
        "meeting_id": room_id,
        "attendees": []
    }))
    .unwrap();

    // Create first meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(payload.clone()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Try to create a duplicate.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(payload))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "MEETING_EXISTS");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_create_meeting_too_many_attendees() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let attendees: Vec<String> = (0..101).map(|i| format!("user{i}@example.com")).collect();
    let payload = serde_json::to_string(&serde_json::json!({
        "meeting_id": "too-many-attendees",
        "attendees": attendees
    }))
    .unwrap();

    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(payload))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "TOO_MANY_ATTENDEES");
}

// ── Get ──────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-get-meeting";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting first.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "password": "secret"
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Get it back.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.host, "host@example.com");
    assert!(body.result.has_password);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_get_meeting_not_found() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = request_with_cookie(
        "GET",
        "/api/v1/meetings/nonexistent-meeting",
        "user@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "MEETING_NOT_FOUND");
}

// ── List ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_list_meetings_success() {
    let pool = get_test_pool().await;
    let room_id = "test-list-meetings";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
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

    // List meetings.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        "/api/v1/meetings?limit=10&offset=0",
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ListMeetingsResponse> = response_json(resp).await;
    assert!(body.success);
    let summary = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("created meeting must appear in the list response");

    // `MeetingSummary` timestamps were converted from `.timestamp()` (seconds)
    // to `.timestamp_millis()` (milliseconds). Both `created_at` and
    // `started_at` must clear the millisecond floor; if either silently drops
    // back to seconds, this assertion flips red. `ended_at` is None for an
    // idle meeting and is exercised separately in the joined-meetings suite.
    assert!(
        summary.created_at >= MS_LOWER_BOUND,
        "MeetingSummary.created_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        summary.created_at
    );
    assert!(
        summary.started_at >= MS_LOWER_BOUND,
        "MeetingSummary.started_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {}",
        summary.started_at
    );
    assert!(
        summary.ended_at.is_none(),
        "ended_at must remain None for an idle meeting; got {:?}",
        summary.ended_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── List timestamp magnitude (post idle/ended cycle) ─────────────────────

/// Locks in millisecond emission for `MeetingSummary.ended_at` (the optional
/// case). This complements `test_list_meetings_success`, which only covers
/// the idle case where `ended_at` is None.
#[tokio::test]
#[serial]
async fn test_list_meetings_returns_ended_at_in_milliseconds() {
    let pool = get_test_pool().await;
    let room_id = "test-list-meetings-ended-at-ms";
    cleanup_test_data(&pool, room_id).await;

    // Create the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": false,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Host joins to activate, then ends the meeting so `ended_at` is set.
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
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "end must succeed");

    // List and inspect.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        "/api/v1/meetings?limit=10&offset=0",
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ListMeetingsResponse> = response_json(resp).await;
    let summary = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("ended meeting must still appear in owner's list");

    assert_eq!(summary.state, "ended", "meeting state must be 'ended'");
    let ended_at = summary
        .ended_at
        .expect("ended meeting must have ended_at populated");
    assert!(
        ended_at >= MS_LOWER_BOUND,
        "MeetingSummary.ended_at must be in milliseconds (>= {MS_LOWER_BOUND}), got {ended_at}"
    );
    assert!(
        ended_at >= summary.started_at,
        "ended_at must be >= started_at; ended_at={ended_at}, started_at={}",
        summary.started_at
    );
    assert!(
        summary.created_at >= MS_LOWER_BOUND,
        "MeetingSummary.created_at must be in milliseconds, got {}",
        summary.created_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Delete ───────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_delete_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-delete-meeting";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
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

    // Owner deletes meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "DELETE",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<DeleteMeetingResponse> = response_json(resp).await;
    assert!(body.success);

    // Verify meeting is soft-deleted (returns 404).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_delete_meeting_not_owner() {
    let pool = get_test_pool().await;
    let room_id = "test-delete-not-owner";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting as host.
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

    // Non-owner tries to delete.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "DELETE",
        &format!("/api/v1/meetings/{room_id}"),
        "other@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    cleanup_test_data(&pool, room_id).await;
}
