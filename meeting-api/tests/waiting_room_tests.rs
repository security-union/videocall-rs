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

//! Integration tests for waiting room management endpoints.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, AdmitAllResponse, ParticipantStatusResponse, WaitingRoomResponse},
    APIError,
};

/// Create a meeting, have host join, and add an attendee to the waiting room.
async fn setup_with_waiting_attendee(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    // Create meeting.
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

    // Host joins (activates).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Attendee joins (enters waiting room).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "880e8400-e29b-41d4-a716-446655440003",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

// ── Get waiting room ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_waiting_room_success() {
    let pool = get_test_pool().await;
    let room_id = "test-waiting-room";
    setup_with_waiting_attendee(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/waiting"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<WaitingRoomResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.waiting.len(), 1);
    assert_eq!(
        body.result.waiting[0].user_id,
        "880e8400-e29b-41d4-a716-446655440003"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Admit participant ────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_admit_participant_success() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-participant";
    setup_with_waiting_attendee(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        r#"{"user_id":"880e8400-e29b-41d4-a716-446655440003"}"#,
    ))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.admitted_at.is_some());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_admit_participant_not_found() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-not-found";
    cleanup_test_data(&pool, room_id).await;

    // Create meeting, host joins (no attendee in waiting room).
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

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Try to admit non-existent participant.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        r#"{"user_id":"dd0e8400-e29b-41d4-a716-446655440008"}"#,
    ))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "PARTICIPANT_NOT_FOUND");

    cleanup_test_data(&pool, room_id).await;
}

// ── Admit all ────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_admit_all_participants() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-all";
    cleanup_test_data(&pool, room_id).await;

    // Create meeting, host joins.
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

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Add 3 attendees to waiting room.
    let attendee_uuids = [
        "ee0e8400-e29b-41d4-a716-446655440009",
        "ff0e8400-e29b-41d4-a716-44665544000a",
        "00111111-e29b-41d4-a716-44665544000b",
    ];
    for uuid in &attendee_uuids {
        let app = build_app(pool.clone());
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), uuid)
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
    }

    // Host admits all.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit-all"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<AdmitAllResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.admitted_count, 3);
    assert_eq!(body.result.admitted.len(), 3);

    cleanup_test_data(&pool, room_id).await;
}

// ── Reject participant ───────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_reject_participant_success() {
    let pool = get_test_pool().await;
    let room_id = "test-reject-participant";
    setup_with_waiting_attendee(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/reject"),
        "550e8400-e29b-41d4-a716-446655440000",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(
        r#"{"user_id":"880e8400-e29b-41d4-a716-446655440003"}"#,
    ))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "rejected");

    cleanup_test_data(&pool, room_id).await;
}
