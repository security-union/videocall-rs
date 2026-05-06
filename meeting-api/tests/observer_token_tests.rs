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

//! Integration tests for the observer token and waiting_for_meeting flow.
//!
//! These tests verify the new behavior where non-host participants joining
//! an inactive meeting receive a `waiting_for_meeting` status with an
//! `observer_token` (a JWT with `observer: true`, `room_join: false`) that
//! grants read-only access for push notifications via the media server.
//!
//! NATS is `None` in all tests (no NATS server running), which is fine
//! because all NATS publish functions gracefully no-op when the client is
//! `None`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use jsonwebtoken::{decode, DecodingKey, Validation};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, AdmitAllResponse, ParticipantStatusResponse, WaitingRoomResponse},
    RoomAccessTokenClaims,
};

/// Helper: create a meeting (idle state, host has NOT joined).
async fn setup_idle_meeting(pool: &sqlx::PgPool, room_id: &str) {
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
}

/// Helper: create a meeting and have the host join (activates it).
async fn setup_active_meeting(pool: &sqlx::PgPool, room_id: &str) {
    setup_idle_meeting(pool, room_id).await;

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

/// Decode an observer or room token JWT using the test secret.
fn decode_token(token: &str) -> RoomAccessTokenClaims {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    let data = decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("token should be a valid JWT signed with test secret");
    data.claims
}

// -- Observer token JWT claims for waiting_for_meeting ------------------------

#[tokio::test]
#[serial]
async fn test_waiting_for_meeting_observer_token_is_valid_jwt() {
    let pool = get_test_pool().await;
    let room_id = "test-observer-jwt-valid";
    setup_idle_meeting(&pool, room_id).await;

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
    assert_eq!(body.result.status, "waiting_for_meeting");
    let token = body
        .result
        .observer_token
        .expect("waiting_for_meeting must include observer_token");

    let claims = decode_token(&token);
    assert_eq!(claims.sub, "attendee@example.com");
    assert_eq!(claims.room, room_id);
    assert!(claims.observer, "Observer token must have observer=true");
    assert!(
        !claims.room_join,
        "Observer token must have room_join=false"
    );
    assert!(!claims.is_host);

    cleanup_test_data(&pool, room_id).await;
}

// -- Waiting attendee in active meeting receives observer_token ----------------

#[tokio::test]
#[serial]
async fn test_waiting_attendee_receives_observer_token() {
    let pool = get_test_pool().await;
    let room_id = "test-waiting-observer";
    setup_active_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Waiting Person"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "waiting");
    assert!(
        body.result.room_token.is_none(),
        "Waiting attendee should NOT have a room_token"
    );
    let token = body
        .result
        .observer_token
        .expect("Waiting attendee must receive an observer_token");

    let claims = decode_token(&token);
    assert_eq!(claims.sub, "attendee@example.com");
    assert_eq!(claims.room, room_id);
    assert!(claims.observer);
    assert!(!claims.room_join);
    assert_eq!(claims.display_name, "Waiting Person");

    cleanup_test_data(&pool, room_id).await;
}

// -- Auto-admitted attendee does NOT receive observer_token --------------------

#[tokio::test]
#[serial]
async fn test_observer_token_not_present_when_auto_admitted() {
    let pool = get_test_pool().await;
    let room_id = "test-no-observer-auto-admit";
    setup_active_meeting_no_waiting_room(&pool, room_id).await;

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
    assert_eq!(body.result.status, "admitted");
    assert!(
        body.result.room_token.is_some(),
        "Auto-admitted attendee should receive a room_token"
    );
    assert!(
        body.result.observer_token.is_none(),
        "Auto-admitted attendee should NOT receive an observer_token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// -- Auto-admitted guest still receives observer_token ------------------------

#[tokio::test]
#[serial]
async fn test_auto_admitted_guest_receives_observer_token_in_active_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-guest-observer-auto-admit-active";
    cleanup_test_data(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "allow_guests": true,
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

    let app = build_app(pool.clone());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Guest Auto Active"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.is_guest);
    assert!(
        body.result.room_token.is_some(),
        "Auto-admitted guest should receive a room_token"
    );
    let observer = body
        .result
        .observer_token
        .expect("Auto-admitted guest must receive an observer_token");
    let claims = decode_token(&observer);
    assert!(claims.observer);
    assert!(!claims.room_join);
    assert!(
        claims
            .sub
            .starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX),
        "observer token subject should be a guest user id"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_auto_admitted_guest_receives_observer_token_when_meeting_not_active() {
    let pool = get_test_pool().await;
    let room_id = "test-guest-observer-auto-admit-not-active";
    cleanup_test_data(&pool, room_id).await;

    // Create an idle meeting with waiting room off so guest join auto-activates
    // and auto-admits through the non-active join path.
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "allow_guests": true,
                "waiting_room_enabled": false
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    let app = build_app(pool.clone());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/api/v1/meetings/{room_id}/join-guest"))
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Guest Auto Idle"}"#))
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.is_guest);
    assert!(
        body.result.room_token.is_some(),
        "Auto-admitted guest should receive a room_token"
    );
    let observer = body
        .result
        .observer_token
        .expect("Auto-admitted guest must receive an observer_token");
    let claims = decode_token(&observer);
    assert!(claims.observer);
    assert!(!claims.room_join);
    assert!(
        claims
            .sub
            .starts_with(videocall_meeting_types::GUEST_USER_ID_PREFIX),
        "observer token subject should be a guest user id"
    );

    cleanup_test_data(&pool, room_id).await;
}

// -- waiting_for_meeting includes waiting_room_enabled field -------------------

#[tokio::test]
#[serial]
async fn test_waiting_for_meeting_includes_waiting_room_enabled() {
    let pool = get_test_pool().await;
    let room_id = "test-wfm-wr-field";
    setup_idle_meeting(&pool, room_id).await;

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
    assert_eq!(body.result.status, "waiting_for_meeting");
    // After the Option<bool> -> bool migration, the field is always present;
    // assert the meeting actually has waiting-room enabled (the server-side
    // flag this fixture set up at meeting creation).
    assert!(
        body.result.waiting_room_enabled,
        "waiting_for_meeting response should reflect waiting_room_enabled=true"
    );

    cleanup_test_data(&pool, room_id).await;
}

// -- waiting_for_meeting with display_name ------------------------------------

#[tokio::test]
#[serial]
async fn test_waiting_for_meeting_with_display_name() {
    let pool = get_test_pool().await;
    let room_id = "test-wfm-display-name";
    setup_idle_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Alice Wonderland"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "waiting_for_meeting");
    assert_eq!(
        body.result.display_name.as_deref(),
        Some("Alice Wonderland"),
        "Display name should be echoed in the response"
    );

    let token = body
        .result
        .observer_token
        .expect("should have observer_token");
    let claims = decode_token(&token);
    assert_eq!(claims.display_name, "Alice Wonderland");

    cleanup_test_data(&pool, room_id).await;
}

// -- Host join does NOT include observer_token --------------------------------

#[tokio::test]
#[serial]
async fn test_host_join_does_not_include_observer_token() {
    let pool = get_test_pool().await;
    let room_id = "test-host-no-observer";
    setup_idle_meeting(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"The Host"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.is_host);
    assert!(
        body.result.room_token.is_some(),
        "Host should receive a room_token"
    );
    assert!(
        body.result.observer_token.is_none(),
        "Host should NOT receive an observer_token"
    );

    cleanup_test_data(&pool, room_id).await;
}

// -- Admit and reject still work with NATS=None (graceful degradation) --------

#[tokio::test]
#[serial]
async fn test_admit_and_reject_work_with_nats_none() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-reject-nats-none";
    setup_active_meeting(&pool, room_id).await;

    for email in &["alice@example.com", "bob@example.com"] {
        let app = build_app(pool.clone());
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), email)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // Verify waiting room has 2 people.
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
    assert_eq!(body.result.waiting.len(), 2);

    // Admit alice.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/admit"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"alice@example.com"}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "admitted");
    assert_eq!(body.result.user_id, "alice@example.com");

    // Reject bob.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/reject"),
        "host@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"user_id":"bob@example.com"}"#))
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert_eq!(body.result.status, "rejected");
    assert_eq!(body.result.user_id, "bob@example.com");

    // Waiting room should now be empty.
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
        "Waiting room should be empty after admit+reject"
    );

    cleanup_test_data(&pool, room_id).await;
}

// -- Admit-all works with NATS=None -------------------------------------------

#[tokio::test]
#[serial]
async fn test_admit_all_works_with_nats_none() {
    let pool = get_test_pool().await;
    let room_id = "test-admit-all-nats-none";
    setup_active_meeting(&pool, room_id).await;

    for i in 1..=3 {
        let app = build_app(pool.clone());
        let email = format!("attendee{i}@example.com");
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), &email)
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
    }

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
    for p in &body.result.admitted {
        assert_eq!(p.status, "admitted");
    }

    cleanup_test_data(&pool, room_id).await;
}
