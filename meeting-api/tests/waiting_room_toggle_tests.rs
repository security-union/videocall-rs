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

//! Integration tests for the waiting room toggle feature:
//! - PATCH /api/v1/meetings/{meeting_id} (update waiting_room_enabled)
//! - Attendee auto-admit when waiting room is disabled
//! - Auto-admit-all when toggling waiting room OFF with participants waiting

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{
        APIResponse, CreateMeetingResponse, ListMeetingsResponse, MeetingInfoResponse,
        ParticipantStatusResponse, WaitingRoomResponse,
    },
    APIError,
};

/// Helper: create a meeting and have the host join (activates it).
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

/// Helper: create a meeting with waiting room disabled and have the host join.
async fn setup_active_meeting_no_waiting_room(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

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

// ── PATCH update meeting ────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_update_meeting_toggle_waiting_room_off() {
    let pool = get_test_pool().await;
    let room_id = "test-toggle-wr-off";
    setup_active_meeting(&pool, room_id).await;

    // Toggle waiting room OFF.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert!(!body.result.waiting_room_enabled);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_update_meeting_toggle_waiting_room_on() {
    let pool = get_test_pool().await;
    let room_id = "test-toggle-wr-on";
    cleanup_test_data(&pool, room_id).await;

    // Create with waiting room OFF.
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
    let _ = app.oneshot(req).await.unwrap();

    // Toggle waiting room ON.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":true}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.success);
    assert!(body.result.waiting_room_enabled);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_update_meeting_non_owner_forbidden() {
    let pool = get_test_pool().await;
    let room_id = "test-toggle-wr-forbidden";
    setup_active_meeting(&pool, room_id).await;

    // Non-owner tries to update.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "other@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_update_meeting_not_found() {
    let pool = get_test_pool().await;
    cleanup_test_data(&pool, "nonexistent-toggle").await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        "/api/v1/meetings/nonexistent-toggle",
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "MEETING_NOT_FOUND");
}

// ── Create meeting with waiting_room_enabled ────────────────────────────

#[tokio::test]
#[serial]
async fn test_create_meeting_defaults_waiting_room_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-create-wr-default";
    cleanup_test_data(&pool, room_id).await;

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

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
    assert!(body.success);
    assert!(
        body.result.waiting_room_enabled,
        "Waiting room should default to true"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_create_meeting_with_waiting_room_disabled() {
    let pool = get_test_pool().await;
    let room_id = "test-create-wr-disabled";
    cleanup_test_data(&pool, room_id).await;

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

    let body: APIResponse<CreateMeetingResponse> = response_json(resp).await;
    assert!(body.success);
    assert!(
        !body.result.waiting_room_enabled,
        "Waiting room should be disabled when explicitly set to false"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── GET meeting returns waiting_room_enabled ────────────────────────────

#[tokio::test]
#[serial]
async fn test_get_meeting_returns_waiting_room_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-get-wr-field";
    cleanup_test_data(&pool, room_id).await;

    // Create with waiting room OFF.
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
    let _ = app.oneshot(req).await.unwrap();

    // GET the meeting and check the field.
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
    assert!(
        !body.result.waiting_room_enabled,
        "GET should reflect waiting_room_enabled=false"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── List meetings returns waiting_room_enabled ──────────────────────────

#[tokio::test]
#[serial]
async fn test_list_meetings_returns_waiting_room_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-list-wr-field";
    cleanup_test_data(&pool, room_id).await;

    // Create with waiting room OFF.
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
    let _ = app.oneshot(req).await.unwrap();

    // List meetings.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        "/api/v1/meetings?limit=100&offset=0",
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ListMeetingsResponse> = response_json(resp).await;
    assert!(body.success);
    let meeting = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting should appear in list");
    assert!(
        !meeting.waiting_room_enabled,
        "List should reflect waiting_room_enabled=false"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Attendee auto-admitted when waiting room disabled ────────────────────

#[tokio::test]
#[serial]
async fn test_attendee_auto_admitted_when_waiting_room_off() {
    let pool = get_test_pool().await;
    let room_id = "test-auto-admit-wr-off";
    setup_active_meeting_no_waiting_room(&pool, room_id).await;

    // Attendee joins.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Auto Attendee"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(
        body.result.status, "admitted",
        "Attendee should be auto-admitted when waiting room is off"
    );
    assert!(
        body.result.room_token.is_some(),
        "Auto-admitted attendee should receive a room_token"
    );
    assert!(!body.result.is_host);

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_attendee_waits_when_waiting_room_on() {
    let pool = get_test_pool().await;
    let room_id = "test-wait-wr-on";
    setup_active_meeting(&pool, room_id).await;

    // Attendee joins (waiting room is on by default).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(
        body.result.status, "waiting",
        "Attendee should be in waiting room when enabled"
    );
    assert!(
        body.result.room_token.is_none(),
        "Waiting attendee should NOT get a room_token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Toggle waiting room OFF auto-admits waiting participants ─────────────

#[tokio::test]
#[serial]
async fn test_toggle_waiting_room_off_admits_waiting_participants() {
    let pool = get_test_pool().await;
    let room_id = "test-toggle-admits-waiting";
    setup_active_meeting(&pool, room_id).await;

    // 3 attendees join (enter waiting room).
    for i in 1..=3 {
        let app = build_app(pool.clone());
        let email = format!("attendee{i}@example.com");
        let req = request_with_cookie(
            "POST",
            &format!("/api/v1/meetings/{room_id}/join"),
            &email,
        )
        .body(Body::empty())
        .unwrap();
        let _ = app.oneshot(req).await.unwrap();
    }

    // Verify waiting room has 3 people.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/waiting"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body: APIResponse<WaitingRoomResponse> = response_json(resp).await;
    assert_eq!(body.result.waiting.len(), 3, "Should have 3 waiting");

    // Host toggles waiting room OFF.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify waiting room is now empty (all admitted).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/waiting"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body: APIResponse<WaitingRoomResponse> = response_json(resp).await;
    assert_eq!(
        body.result.waiting.len(),
        0,
        "All participants should be admitted after toggling waiting room off"
    );

    // Verify attendees are now admitted and can get tokens.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        "attendee1@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");
    assert!(
        body.result.room_token.is_some(),
        "Previously-waiting attendee should now get a room_token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── New attendee auto-admitted after toggle ──────────────────────────────

#[tokio::test]
#[serial]
async fn test_new_attendee_auto_admitted_after_toggle_off() {
    let pool = get_test_pool().await;
    let room_id = "test-new-attendee-after-toggle";
    setup_active_meeting(&pool, room_id).await;

    // Host toggles waiting room OFF.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // NEW attendee joins after toggle.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "late-joiner@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(
        body.result.status, "admitted",
        "New attendee should be auto-admitted after host toggled waiting room off"
    );
    assert!(body.result.room_token.is_some());

    cleanup_test_data(&pool, room_id).await;
}

// ── Toggle back ON restores waiting room behavior ───────────────────────

#[tokio::test]
#[serial]
async fn test_toggle_waiting_room_back_on_restores_waiting() {
    let pool = get_test_pool().await;
    let room_id = "test-toggle-back-on";
    setup_active_meeting(&pool, room_id).await;

    // Toggle OFF.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":false}"#))
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Toggle back ON.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "PATCH",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"waiting_room_enabled":true}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert!(body.result.waiting_room_enabled);

    // New attendee should now go to waiting room.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee-after-reon@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(
        body.result.status, "waiting",
        "After re-enabling waiting room, new attendees should wait"
    );
    assert!(body.result.room_token.is_none());

    cleanup_test_data(&pool, room_id).await;
}
