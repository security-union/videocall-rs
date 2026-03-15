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

//! Integration tests for the end-meeting endpoint and meeting stats fields.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, MeetingInfoResponse},
    APIError,
};

// ── End Meeting ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_end_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-success";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({
            "meeting_id": room_id,
            "attendees": []
        }))
        .unwrap(),
    ))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // End the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.state, "ended");
    assert!(body.result.ended_at.is_some());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_not_owner() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-not-owner";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting as host.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
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

    // Non-owner tries to end it.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "660e8400-e29b-41d4-a716-446655440001",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "NOT_OWNER");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_not_found() {
    let pool = get_test_pool().await;
    let app = build_app(pool.clone());

    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings/nonexistent-end-test/end",
        "770e8400-e29b-41d4-a716-446655440002",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(body.result.code, "MEETING_NOT_FOUND");
}

#[tokio::test]
#[serial]
async fn test_end_meeting_idempotent() {
    let pool = get_test_pool().await;
    let room_id = "test-end-meeting-idempotent";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
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

    // End it the first time.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body1: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(body1.result.state, "ended");
    let ended_at_1 = body1.result.ended_at;

    // End it again — should be idempotent, same state.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body2: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(body2.result.state, "ended");
    assert_eq!(body2.result.ended_at, ended_at_1);

    cleanup_test_data(&pool, room_id).await;
}

// ── Meeting Stats Fields ────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_meeting_includes_stats_fields() {
    let pool = get_test_pool().await;
    let room_id = "test-meeting-stats-fields";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
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

    // Get the meeting and verify stats fields are present.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.participant_count, 0);
    assert_eq!(body.result.waiting_count, 0);
    assert!(body.result.started_at > 0);
    assert!(body.result.ended_at.is_none());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_end_meeting_populates_ended_at() {
    let pool = get_test_pool().await;
    let room_id = "test-end-populates-ended-at";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
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

    // Verify ended_at is null before ending.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let before: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(before.result.ended_at.is_none());

    // End the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify ended_at is populated after ending.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let after: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(after.result.state, "ended");
    assert!(after.result.ended_at.is_some());
    assert!(after.result.ended_at.unwrap() > 0);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_update_meeting_returns_stats_fields() {
    let pool = get_test_pool().await;
    let room_id = "test-update-meeting-stats";
    cleanup_test_data(&pool, room_id).await;

    // Create a meeting with waiting room enabled.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        "/api/v1/meetings",
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({
            "meeting_id": room_id,
            "attendees": [],
            "waiting_room_enabled": true
        }))
        .unwrap(),
    ))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Update waiting room setting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        serde_json::to_string(&serde_json::json!({
            "waiting_room_enabled": false
        }))
        .unwrap(),
    ))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    // Verify stats fields are in the update response.
    assert_eq!(body.result.participant_count, 0);
    assert_eq!(body.result.waiting_count, 0);
    assert!(body.result.started_at > 0);
    assert!(!body.result.waiting_room_enabled);

    cleanup_test_data(&pool, room_id).await;
}
