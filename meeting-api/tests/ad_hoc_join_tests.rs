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

//! Integration tests for ad-hoc meeting creation via the join endpoint.
//!
//! When a user joins a meeting that does not exist, the system auto-creates
//! the meeting with that user as host.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{
    APIResponse, MeetingInfoResponse, ParticipantStatusResponse,
};

#[tokio::test]
#[serial]
async fn test_ad_hoc_join_creates_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-adhoc-create";
    cleanup_test_data(&pool, room_id).await;

    // Join a meeting that doesn't exist yet.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "pioneer@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Pioneer"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success);
    assert_eq!(body.result.status, "admitted");
    assert!(body.result.is_host, "Auto-creator should be the host");
    assert!(
        body.result.room_token.is_some(),
        "Host should get a room_token"
    );

    // Verify the meeting was actually created and is active.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "pioneer@example.com",
    )
    .body(Body::empty())
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let meeting: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    assert_eq!(meeting.result.meeting_id, room_id);
    assert_eq!(meeting.result.state, "active");
    assert_eq!(meeting.result.host, "pioneer@example.com");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn test_ad_hoc_join_token_is_valid_jwt() {
    let pool = get_test_pool().await;
    let room_id = "test-adhoc-token";
    cleanup_test_data(&pool, room_id).await;

    // Join (auto-creates meeting + issues token).
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

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    let token = body.result.room_token.expect("host should receive a token");

    // Decode the JWT without verifying signature (we don't have the secret in
    // this test file, but we can verify it's a structurally valid JWT).
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use videocall_meeting_types::RoomAccessTokenClaims;

    let secret = "test-secret-for-integration-tests";
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    let data = decode::<RoomAccessTokenClaims>(
        &token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .expect("token should be a valid JWT signed with test secret");

    assert_eq!(data.claims.sub, "host@example.com");
    assert_eq!(data.claims.room, room_id);
    assert!(data.claims.is_host);
    assert!(data.claims.room_join);
    assert_eq!(data.claims.iss, "videocall-meeting-backend");

    cleanup_test_data(&pool, room_id).await;
}
