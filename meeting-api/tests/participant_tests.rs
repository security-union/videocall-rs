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

//! Integration tests for participant join, leave, status, and list endpoints.

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

/// Helper: create a meeting and have the host join (activates it).
async fn setup_active_meeting(pool: &sqlx::PgPool, room_id: &str) {
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
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Host User"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

// ── Host join ────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_join_meeting_host_activates() {
    let pool = get_test_pool().await;
    let room_id = "test-host-join";
    cleanup_test_data(&pool, room_id).await;

    // Create meeting (idle state).
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host joins.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Host User"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.is_host);
    assert!(
        body.result.room_token.is_some(),
        "Host should receive a room_token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Attendee joins waiting room ──────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_join_meeting_attendee_waits() {
    let pool = get_test_pool().await;
    let room_id = "test-attendee-wait";
    setup_active_meeting(&pool, room_id).await;

    // Attendee joins.
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
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "waiting");
    assert!(!body.result.is_host);
    assert!(
        body.result.room_token.is_none(),
        "Waiting attendee should NOT get a token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Attendee cannot join inactive meeting ────────────────────────────────

#[tokio::test]
#[serial]
async fn test_join_meeting_not_active() {
    let pool = get_test_pool().await;
    let room_id = "test-join-not-active";
    cleanup_test_data(&pool, room_id).await;

    // Create meeting but do NOT have the host join (still idle).
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({ "meeting_id": room_id, "attendees": [] }))
                .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Non-host tries to join.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "MEETING_NOT_ACTIVE");

    cleanup_test_data(&pool, room_id).await;
}

// ── Leave ────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_leave_meeting_success() {
    let pool = get_test_pool().await;
    let room_id = "test-leave-meeting";
    setup_active_meeting(&pool, room_id).await;

    // Attendee joins.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
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
    .body(Body::from(r#"{"email":"attendee@example.com"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Attendee leaves.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "left");

    cleanup_test_data(&pool, room_id).await;
}

// ── Get my status ────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_my_status_success() {
    let pool = get_test_pool().await;
    let room_id = "test-get-my-status";
    setup_active_meeting(&pool, room_id).await;

    // Host checks their own status.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.email, "host@example.com");
    assert!(body.result.is_host);
    assert_eq!(body.result.status, "admitted");
    assert!(
        body.result.room_token.is_some(),
        "Admitted host should get room_token on status poll"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Status refused after meeting ends ────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_status_refused_after_meeting_ends() {
    let pool = get_test_pool().await;
    let room_id = "test-status-after-ended";
    setup_active_meeting(&pool, room_id).await;

    // 1. Attendee joins.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // 2. Host admits attendee.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"email":"attendee@example.com"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // 3. Verify attendee can get a token while meeting is active.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(
        body.result.room_token.is_some(),
        "Should get token while active"
    );

    // 4. Host leaves → meeting ends.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // 5. Attendee tries to get status/token → should be refused.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "MEETING_NOT_ACTIVE");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_attendee_cannot_rejoin_ended_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-rejoin-ended";
    setup_active_meeting(&pool, room_id).await;

    // Host leaves → meeting ends.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Attendee tries to join → should be refused.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "MEETING_NOT_ACTIVE");

    cleanup_test_data(&pool, room_id).await;
}

// ── Get participants ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_participants_success() {
    let pool = get_test_pool().await;
    let room_id = "test-get-participants";
    setup_active_meeting(&pool, room_id).await;

    // Attendee joins + is admitted.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
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
    .body(Body::from(r#"{"email":"attendee@example.com"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // List admitted participants.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/participants"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<Vec<ParticipantStatusResponse>> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.len(), 2); // host + admitted attendee

    cleanup_test_data(&pool, room_id).await;
}
