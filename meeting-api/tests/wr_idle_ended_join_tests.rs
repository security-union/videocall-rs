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

//! Integration tests covering idle/ended meeting join behaviour under various
//! waiting-room configurations, the WR-toggle race scenario, and an
//! authorization-bypass regression guard.
//!
//! Scenarios tested:
//!
//! 1. Non-host joins **idle** meeting with WR **off**  
//!    → meeting is activated and participant is auto-admitted.
//!
//! 2. Non-host joins **ended** meeting with WR **off**  
//!    → meeting is re-activated AND participant is auto-admitted.
//!
//! 3. Non-host joins **idle** meeting with WR **on**  
//!    → participant lands in `waiting_for_meeting`; meeting stays idle.
//!
//! 4. **WR-toggle race** — attendee join races against host toggling WR off→on.  
//!    → participant ends up as `admitted` (saw WR=off) **or** `waiting` (saw
//!    WR=on). No stuck state (`waiting_for_meeting` on an already-active meeting).
//!
//! 5. **No authorization bypass** — with WR on and meeting idle, a non-host
//!    cannot activate the meeting; it remains `idle`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{
    APIResponse, MeetingInfoResponse, ParticipantStatusResponse,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Create an idle meeting with `waiting_room_enabled = false`.
/// Does NOT have the host join, so the meeting stays in `idle` state.
async fn create_idle_meeting_wr_off(pool: &sqlx::PgPool, room_id: &str) {
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
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting creation must succeed"
    );
}

/// Create an idle meeting with `waiting_room_enabled = true` (the default).
/// Does NOT have the host join.
async fn create_idle_meeting_wr_on(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", "host@example.com")
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": []
                // waiting_room_enabled defaults to true
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "meeting creation must succeed"
    );
}

/// Build a GET-meeting request and return its `MeetingInfoResponse`.
async fn get_meeting_info(pool: &sqlx::PgPool, room_id: &str) -> MeetingInfoResponse {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET meeting must succeed");
    let body: APIResponse<MeetingInfoResponse> = response_json(resp).await;
    body.result
}

// ── Scenario 1: idle meeting + WR off → auto-admitted ────────────────────────

/// Non-host joins an idle meeting that has the waiting room disabled.
/// Expected: meeting is activated, participant status is `admitted`, room_token
/// is issued, and the meeting's DB state flips to `active`.
#[tokio::test]
#[serial]
async fn test_non_host_joins_idle_wr_off_auto_admitted() {
    let pool = get_test_pool().await;
    let room_id = "wr-idle-off-auto-admit";
    create_idle_meeting_wr_off(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Eager Attendee"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success, "join must succeed");
    assert_eq!(
        body.result.status, "admitted",
        "non-host should be auto-admitted when WR is off"
    );
    assert!(!body.result.is_host, "attendee must not be flagged as host");
    assert!(
        body.result.room_token.is_some(),
        "auto-admitted attendee must receive a room_token"
    );
    // Meeting must have been activated.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(
        info.state, "active",
        "meeting must be activated by the first non-host joiner when WR is off"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 2: ended meeting + WR off → re-activated and admitted ────────────

/// Non-host joins a meeting whose state is `ended` with WR off.
/// Expected: meeting is re-activated, participant is auto-admitted with a
/// room_token, and the meeting state is back to `active`.
#[tokio::test]
#[serial]
async fn test_non_host_joins_ended_wr_off_reactivates_and_admits() {
    let pool = get_test_pool().await;
    let room_id = "wr-ended-off-readmit";
    create_idle_meeting_wr_off(&pool, room_id).await;

    // Host joins to activate.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let _ = app.oneshot(req).await.unwrap();

    // Host ends the meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/end"),
        "host@example.com",
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "end meeting must succeed");

    // Verify the meeting is truly ended before the attendee joins.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(info.state, "ended", "meeting must be in ended state");

    // Non-host joins the ended meeting.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Late Joiner"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success, "join must succeed");
    assert_eq!(
        body.result.status, "admitted",
        "non-host must be auto-admitted when WR is off even on an ended meeting"
    );
    assert!(
        body.result.room_token.is_some(),
        "auto-admitted attendee must receive a room_token"
    );

    // Meeting must have been re-activated.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(
        info.state, "active",
        "ended meeting must be re-activated when non-host joins with WR off"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 3: idle meeting + WR on → waiting_for_meeting ───────────────────

/// Non-host joins an idle meeting that has the waiting room enabled.
/// Expected: status is `waiting_for_meeting`, no room_token is issued, an
/// observer_token is provided, and the meeting remains `idle` (the non-host
/// must not be able to activate it).
#[tokio::test]
#[serial]
async fn test_non_host_joins_idle_wr_on_waiting_for_meeting() {
    let pool = get_test_pool().await;
    let room_id = "wr-idle-on-wait-for-mtg";
    create_idle_meeting_wr_on(&pool, room_id).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "attendee@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Patient Attendee"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success, "join must succeed");
    assert_eq!(
        body.result.status, "waiting_for_meeting",
        "non-host joining an idle meeting with WR on must get waiting_for_meeting"
    );
    assert!(!body.result.is_host, "attendee must not be flagged as host");
    assert!(
        body.result.room_token.is_none(),
        "waiting_for_meeting must NOT include a room_token"
    );
    assert!(
        body.result.observer_token.is_some(),
        "waiting_for_meeting must include an observer_token for push notifications"
    );

    // The meeting must still be idle — the non-host must not have activated it.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(
        info.state, "idle",
        "meeting must remain idle after non-host joins with WR on"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 4: WR-toggle race — no stuck state ───────────────────────────────

/// Attendee join races against host toggling the waiting room from off to on.
///
/// Both operations execute concurrently. Because `join_attendee` holds a
/// `FOR UPDATE` row lock on the meeting while reading `waiting_room_enabled`,
/// and `update_meeting_settings` uses the same lock, exactly one of the
/// following deterministic outcomes must occur:
///
/// * **Attendee observed WR=off** → status `admitted`, room_token present.
/// * **Attendee observed WR=on** → status `waiting`, room_token absent.
///
/// A `waiting_for_meeting` result would indicate the attendee somehow hit the
/// inactive-meeting code path on an already-active meeting — that is the
/// stuck-state bug this test guards against.
#[tokio::test]
#[serial]
async fn test_wr_toggle_race_join_and_toggle_no_stuck_state() {
    let pool = get_test_pool().await;
    let room_id = "wr-race-toggle-no-stuck";
    cleanup_test_data(&pool, room_id).await;

    // Setup: active meeting with WR=off.
    {
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
    }
    {
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

    // Verify setup: meeting is active with WR=off before the race.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(info.state, "active");
    assert!(!info.waiting_room_enabled);

    // Race: attendee join + host WR toggle, truly concurrent.
    let pool_join = pool.clone();
    let pool_toggle = pool.clone();
    let room_id_join = room_id.to_string();
    let room_id_toggle = room_id.to_string();

    let join_handle = tokio::spawn(async move {
        let app = build_app(pool_join);
        let req = request_with_cookie(
            "POST",
            &format!("/api/v1/meetings/{room_id_join}/join"),
            "attendee@example.com",
        )
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Racer"}"#))
        .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status_code = resp.status();
        let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
        (status_code, body)
    });

    let toggle_handle = tokio::spawn(async move {
        let app = build_app(pool_toggle);
        let req = request_with_cookie(
            "PATCH",
            &format!("/api/v1/meetings/{room_id_toggle}"),
            "host@example.com",
        )
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"waiting_room_enabled":true}"#))
        .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        resp.status()
    });

    let (join_result, toggle_result) = tokio::join!(join_handle, toggle_handle);
    let (join_status_code, join_body) = join_result.expect("join task must not panic");
    let toggle_status_code = toggle_result.expect("toggle task must not panic");

    // Toggle must have succeeded.
    assert_eq!(
        toggle_status_code,
        StatusCode::OK,
        "WR toggle must succeed"
    );

    // Join must have succeeded with HTTP 200.
    assert_eq!(join_status_code, StatusCode::OK, "join must return HTTP 200");
    assert!(join_body.success, "join must succeed");

    // The only valid outcomes are `admitted` (saw WR=off) or `waiting` (saw WR=on).
    // `waiting_for_meeting` would mean the attendee hit the inactive-meeting path
    // on an already-active meeting — the stuck-state bug.
    let participant_status = join_body.result.status.as_str();
    assert!(
        participant_status == "admitted" || participant_status == "waiting",
        "race outcome must be 'admitted' or 'waiting', got '{participant_status}' \
         — 'waiting_for_meeting' on an active meeting is the stuck-state bug"
    );

    if participant_status == "admitted" {
        assert!(
            join_body.result.room_token.is_some(),
            "admitted attendee must receive a room_token"
        );
    } else {
        assert!(
            join_body.result.room_token.is_none(),
            "waiting attendee must NOT receive a room_token"
        );
        assert!(
            join_body.result.observer_token.is_some(),
            "waiting attendee must receive an observer_token"
        );
    }

    // Meeting must still be active — the race must not have left it in a broken state.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(
        info.state, "active",
        "meeting must still be active after the race"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 5: no authorization bypass ──────────────────────────────────────

/// A non-host joining an idle meeting with WR **on** must NOT be able to
/// activate it.  The meeting must remain `idle` after the join attempt,
/// preventing a stranger from activating a meeting they do not own.
///
/// This test also verifies the `waiting_room_enabled` field is correctly
/// reflected in the response so the client cannot infer a bypass opportunity.
#[tokio::test]
#[serial]
async fn test_non_host_cannot_activate_meeting_with_wr_on() {
    let pool = get_test_pool().await;
    let room_id = "wr-no-authz-bypass";
    create_idle_meeting_wr_on(&pool, room_id).await;

    // Stranger (not the host, not an invited attendee) attempts to join.
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        "stranger@example.com",
    )
    .header("Content-Type", "application/json")
    .body(Body::from(r#"{"display_name":"Stranger"}"#))
    .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(body.success, "join call must succeed");
    assert_eq!(
        body.result.status, "waiting_for_meeting",
        "stranger joining idle meeting with WR on must get waiting_for_meeting, not bypass"
    );
    assert!(
        body.result.room_token.is_none(),
        "stranger must NOT receive a room_token"
    );
    // The response should reflect WR=on so the client knows WR is guarding the meeting.
    assert_eq!(
        body.result.waiting_room_enabled,
        Some(true),
        "response must expose waiting_room_enabled=true"
    );

    // The meeting must still be idle — the stranger's join attempt must not have
    // activated it.  A meeting host who hasn't joined yet must remain in control
    // of when the session starts.
    let info = get_meeting_info(&pool, room_id).await;
    assert_eq!(
        info.state, "idle",
        "meeting must remain idle after stranger joins with WR on — \
         only the host can activate the meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}
