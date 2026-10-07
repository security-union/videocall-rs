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

// ── Attendee joins inactive meeting -> waiting_for_meeting ───────────────

#[tokio::test]
#[serial]
async fn test_join_meeting_not_active_returns_waiting_for_meeting() {
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

    // Non-host tries to join an idle meeting -- now returns waiting_for_meeting
    // with an observer_token instead of an error.
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
        body.result.status, "waiting_for_meeting",
        "Non-host joining idle meeting should get waiting_for_meeting status"
    );
    assert!(!body.result.is_host);
    assert!(
        body.result.room_token.is_none(),
        "waiting_for_meeting should NOT include a room_token"
    );
    assert!(
        body.result.observer_token.is_some(),
        "waiting_for_meeting should include an observer_token for push notifications"
    );

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
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
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
    assert_eq!(body.result.user_id, "host@example.com");
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
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
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
async fn test_attendee_join_ended_meeting_rejected_not_active() {
    let pool = get_test_pool().await;
    let room_id = "test-rejoin-ended";
    // setup_active_meeting uses the create defaults: waiting_room_enabled=true
    // and end_on_host_leave=true, so the host leaving ends the meeting.
    setup_active_meeting(&pool, room_id).await;

    // Host leaves → meeting ends (end_on_host_leave=true).
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Attendee tries to join the ended meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    // Corrected behavior (issue #742): a non-host joining a terminated meeting
    // must be rejected with MEETING_NOT_ACTIVE — the same code get_my_status /
    // get_guest_status already return for an ended meeting — instead of being
    // stranded in a phantom `waiting_for_meeting` waiting room.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: APIResponse<APIError> = response_json(resp).await;
    assert!(!body.success);
    assert_eq!(
        body.result.code, "MEETING_NOT_ACTIVE",
        "Non-host joining an ended meeting must be rejected with MEETING_NOT_ACTIVE"
    );

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
    .body(Body::from(r#"{"user_id":"attendee@example.com"}"#))
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

async fn post_as(
    pool: &sqlx::PgPool,
    uri: &str,
    caller: &str,
    body: serde_json::Value,
) -> StatusCode {
    let req = request_with_cookie("POST", uri, caller)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap().status()
}

async fn list_participants_as(
    pool: &sqlx::PgPool,
    room_id: &str,
    caller: &str,
) -> Result<(), (u16, String)> {
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/participants"),
        caller,
    )
    .body(Body::empty())
    .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    if status == StatusCode::OK {
        return Ok(());
    }
    let body: APIResponse<APIError> = response_json(resp).await;
    Err((status.as_u16(), body.result.code))
}

#[tokio::test]
#[serial]
async fn get_participants_rejects_waiting_rejected_and_kicked_callers() {
    let pool = get_test_pool().await;
    let room_id = "test-get-participants-non-members";
    setup_active_meeting(&pool, room_id).await;
    let host = "host@example.com";
    let not_in_meeting = Err((404, "NOT_IN_MEETING".to_string()));

    for user in [
        "waiter@example.com",
        "rejected@example.com",
        "kicked@example.com",
    ] {
        let join = format!("/api/v1/meetings/{room_id}/join");
        assert_eq!(
            post_as(&pool, &join, user, serde_json::json!({})).await,
            StatusCode::OK
        );
    }
    let reject = format!("/api/v1/meetings/{room_id}/reject");
    let rejected = serde_json::json!({ "user_id": "rejected@example.com" });
    assert_eq!(
        post_as(&pool, &reject, host, rejected).await,
        StatusCode::OK
    );
    let admit = format!("/api/v1/meetings/{room_id}/admit");
    let kicked = serde_json::json!({ "user_id": "kicked@example.com" });
    assert_eq!(
        post_as(&pool, &admit, host, kicked.clone()).await,
        StatusCode::OK
    );
    assert_eq!(
        list_participants_as(&pool, room_id, "kicked@example.com").await,
        Ok(())
    );
    let kick = format!("/api/v1/meetings/{room_id}/kick");
    assert_eq!(post_as(&pool, &kick, host, kicked).await, StatusCode::OK);

    assert_eq!(
        list_participants_as(&pool, room_id, "waiter@example.com").await,
        not_in_meeting
    );
    assert_eq!(
        list_participants_as(&pool, room_id, "rejected@example.com").await,
        not_in_meeting
    );
    assert_eq!(
        list_participants_as(&pool, room_id, "kicked@example.com").await,
        not_in_meeting
    );
    assert_eq!(list_participants_as(&pool, room_id, host).await, Ok(()));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn get_participants_admits_current_instance_leavers_and_the_owner_only() {
    let pool = get_test_pool().await;
    let room_id = "test-get-participants-instance";
    cleanup_test_data(&pool, room_id).await;
    let host = "host@example.com";
    let create = serde_json::json!({
        "meeting_id": room_id,
        "attendees": [],
        "waiting_room_enabled": false,
        "end_on_host_leave": false,
    });
    assert_eq!(
        post_as(&pool, "/api/v1/meetings", host, create).await,
        StatusCode::CREATED
    );
    assert_eq!(
        list_participants_as(&pool, room_id, host).await,
        Ok(()),
        "the owner may list before having a participant row"
    );
    let join = format!("/api/v1/meetings/{room_id}/join");
    for user in [host, "previous@example.com", "leaver@example.com"] {
        assert_eq!(
            post_as(&pool, &join, user, serde_json::json!({})).await,
            StatusCode::OK
        );
    }
    let leave = format!("/api/v1/meetings/{room_id}/leave");
    assert_eq!(
        post_as(&pool, &leave, "leaver@example.com", serde_json::json!({})).await,
        StatusCode::OK
    );
    assert_eq!(
        list_participants_as(&pool, room_id, "leaver@example.com").await,
        Ok(()),
        "a leaver of the current instance keeps access"
    );

    let end = format!("/api/v1/meetings/{room_id}/end");
    assert_eq!(
        post_as(&pool, &end, host, serde_json::json!({})).await,
        StatusCode::OK
    );
    assert_eq!(
        post_as(&pool, &join, "newcomer@example.com", serde_json::json!({})).await,
        StatusCode::OK
    );

    assert_eq!(
        list_participants_as(&pool, room_id, "previous@example.com").await,
        Err((404, "NOT_IN_MEETING".to_string())),
        "a participant of a previous instance is not a member"
    );
    assert_eq!(
        list_participants_as(&pool, room_id, host).await,
        Ok(()),
        "the owner keeps access after the new instance retired their row"
    );
    assert_eq!(
        list_participants_as(&pool, room_id, "newcomer@example.com").await,
        Ok(())
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn get_participants_rejects_a_kicked_or_rejected_caller_who_requeues_and_leaves() {
    let pool = get_test_pool().await;
    let room_id = "test-get-participants-requeue";
    setup_active_meeting(&pool, room_id).await;
    let host = "host@example.com";
    let kicked = "kicked@example.com";
    let rejected = "rejected@example.com";
    let join = format!("/api/v1/meetings/{room_id}/join");
    let admit = format!("/api/v1/meetings/{room_id}/admit");
    let leave = format!("/api/v1/meetings/{room_id}/leave");
    let no_body = serde_json::json!({});

    for user in [kicked, rejected] {
        assert_eq!(
            post_as(&pool, &join, user, no_body.clone()).await,
            StatusCode::OK
        );
        let target = serde_json::json!({ "user_id": user });
        assert_eq!(post_as(&pool, &admit, host, target).await, StatusCode::OK);
        assert_eq!(list_participants_as(&pool, room_id, user).await, Ok(()));
    }
    let kick = format!("/api/v1/meetings/{room_id}/kick");
    let target = serde_json::json!({ "user_id": kicked });
    assert_eq!(post_as(&pool, &kick, host, target).await, StatusCode::OK);
    assert_eq!(
        post_as(&pool, &join, rejected, no_body.clone()).await,
        StatusCode::OK
    );
    let reject = format!("/api/v1/meetings/{room_id}/reject");
    let target = serde_json::json!({ "user_id": rejected });
    assert_eq!(post_as(&pool, &reject, host, target).await, StatusCode::OK);

    for user in [kicked, rejected] {
        assert_eq!(
            post_as(&pool, &join, user, no_body.clone()).await,
            StatusCode::OK
        );
        assert_eq!(
            post_as(&pool, &leave, user, no_body.clone()).await,
            StatusCode::OK
        );
        assert_eq!(
            list_participants_as(&pool, room_id, user).await,
            Err((404, "NOT_IN_MEETING".to_string())),
            "{user}"
        );
    }

    cleanup_test_data(&pool, room_id).await;
}
