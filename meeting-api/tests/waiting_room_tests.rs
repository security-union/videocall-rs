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
        "attendee@example.com",
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
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<WaitingRoomResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.meeting_id, room_id);
    assert_eq!(body.result.waiting.len(), 1);
    assert_eq!(body.result.waiting[0].user_id, "attendee@example.com");

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
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
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

    // Try to admit non-existent participant.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"nonexistent@example.com"}"#))
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

    // Add 3 attendees to waiting room.
    for i in 1..=3 {
        let app = build_app(pool.clone());
        let email = format!("attendee{i}@example.com");
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), &email)
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
    }

    // Host admits all.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit-all"),
        "host@example.com",
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

// ── Admit re-activates an idle meeting (idle -> active) ────────────────────

/// Read the current `state` column for a meeting directly from the DB.
async fn meeting_state(pool: &sqlx::PgPool, room_id: &str) -> String {
    sqlx::query_as::<_, (String,)>("SELECT state FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("meeting row must exist")
        .0
}

/// Admitting a waiting participant must re-activate a meeting that has gone
/// `idle` (everyone-left → idle). This guards the `db_meetings::activate` call
/// added to the `admit` handler so a participant admitted into a briefly-idle
/// meeting reliably lands in an `active` meeting.
#[tokio::test]
#[serial]
async fn test_admit_reactivates_idle_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-reactivates-idle";
    setup_with_waiting_attendee(&pool, room_id).await;

    // Precondition: meeting is active with one waiting attendee.
    assert_eq!(meeting_state(&pool, room_id).await, "active");

    // Simulate the presence-driven empty->idle transition (as if everyone left
    // and the became-empty consumer fired).
    let meeting = meeting_api::db::meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    meeting_api::db::meetings::set_idle(&pool, meeting.id)
        .await
        .expect("set_idle must succeed");
    assert_eq!(meeting_state(&pool, room_id).await, "idle");

    // Host admits the waiting attendee — this must flip the meeting back to
    // active.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        meeting_state(&pool, room_id).await,
        "active",
        "admitting a participant into an idle meeting must re-activate it"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// `admit-all` must likewise re-activate an idle meeting when it admits at
/// least one participant.
#[tokio::test]
#[serial]
async fn test_admit_all_reactivates_idle_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-all-reactivates-idle";
    setup_with_waiting_attendee(&pool, room_id).await;

    let meeting = meeting_api::db::meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    meeting_api::db::meetings::set_idle(&pool, meeting.id)
        .await
        .expect("set_idle must succeed");
    assert_eq!(meeting_state(&pool, room_id).await, "idle");

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit-all"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<AdmitAllResponse> = response_json(resp).await;
    assert!(body.result.admitted_count >= 1);

    assert_eq!(
        meeting_state(&pool, room_id).await,
        "active",
        "admit-all must re-activate an idle meeting when it admits anyone"
    );

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
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "rejected");

    cleanup_test_data(&pool, room_id).await;
}

// ── admitted_can_admit authorization ─────────────────────────────────────

/// Helper: create a meeting with `admitted_can_admit` set, activate it,
/// admit a "co-host" participant, and leave a second attendee waiting.
async fn setup_with_admitted_and_waiting(
    pool: &sqlx::PgPool,
    room_id: &str,
    admitted_can_admit: bool,
) {
    cleanup_test_data(pool, room_id).await;

    // Create meeting with the flag.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "admitted_can_admit": admitted_can_admit
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

    // "cohost" joins → enters waiting room.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "cohost@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host admits "cohost".
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"cohost@example.com"}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // "waiter" joins → enters waiting room.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "waiter@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

#[tokio::test]
#[serial]
async fn test_admitted_non_host_can_admit_when_flag_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-aca-admit-ok";
    setup_with_admitted_and_waiting(&pool, room_id, true).await;

    // Non-host admitted participant ("cohost") admits "waiter".
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "cohost@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"waiter@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "admitted");
    assert_eq!(body.result.user_id, "waiter@example.com");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_admitted_non_host_cannot_admit_when_flag_disabled() {
    let pool = get_test_pool().await;
    let room_id = "test-aca-admit-denied";
    setup_with_admitted_and_waiting(&pool, room_id, false).await;

    // Non-host admitted participant ("cohost") tries to admit "waiter" → 403.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "cohost@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"waiter@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_waiting_user_cannot_admit_even_when_flag_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-aca-waiting-denied";
    cleanup_test_data(&pool, room_id).await;

    // Create meeting with admitted_can_admit = true.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "admitted_can_admit": true
            }))
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
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // "sneaky" joins → still in waiting room (not admitted).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "sneaky@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // "victim" joins → also in waiting room.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "victim@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // "sneaky" (waiting, not admitted) tries to admit "victim" → 403.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "sneaky@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"victim@example.com"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);

    cleanup_test_data(&pool, room_id).await;
}
