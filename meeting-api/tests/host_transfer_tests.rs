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

//! Integration tests for transfer-host (single-host model) and the host-leave
//! continuity it implies.
//!
//! Covers:
//! - `POST /api/v1/meetings/{meeting_id}/transfer-host` — atomic host handoff
//! - host-leave end rules: the (single) host leaving ends the meeting when
//!   `end_on_host_leave=true`; a transferred-away ex-creator leaving does not
//! - single-host reset: the transfer target is demoted on meeting end, and the
//!   creator reclaims sole host on rejoin
//!
//! NATS is `None` in [`build_app`]; publish calls are no-ops. Requires a live
//! Postgres via `DATABASE_URL`; tests are `#[serial]`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use jsonwebtoken::{decode, DecodingKey, Validation};
use meeting_api::db::{meetings as db_meetings, participants as db_participants};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::{
    responses::{APIResponse, ParticipantStatusResponse},
    APIError, RoomAccessTokenClaims,
};

const HOST: &str = "host@example.com";
const PARTICIPANT: &str = "participant@example.com";

// ── Setup helpers ────────────────────────────────────────────────────────

async fn create_meeting(pool: &sqlx::PgPool, room_id: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
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

/// Create a meeting with an explicit `end_on_host_leave` setting.
async fn create_meeting_with_eohl(pool: &sqlx::PgPool, room_id: &str, eohl: bool) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "end_on_host_leave": eohl
            }))
            .unwrap(),
        ))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

async fn join(pool: &sqlx::PgPool, room_id: &str, user: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), user)
        .body(Body::empty())
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

async fn admit(pool: &sqlx::PgPool, room_id: &str, user: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/admit"), HOST)
        .header("Content-Type", "application/json")
        .body(Body::from(format!(r#"{{"user_id":"{user}"}}"#)))
        .unwrap();
    let _ = app.oneshot(req).await.unwrap();
}

/// Create a meeting, host joins (activates), one attendee joins + admitted.
async fn setup_with_admitted_participant(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;
    create_meeting(pool, room_id).await;
    join(pool, room_id, HOST).await;
    join(pool, room_id, PARTICIPANT).await;
    admit(pool, room_id, PARTICIPANT).await;
}

/// Create a meeting and have only the host join (activates it).
async fn setup_active_meeting(pool: &sqlx::PgPool, room_id: &str) {
    cleanup_test_data(pool, room_id).await;
    create_meeting(pool, room_id).await;
    join(pool, room_id, HOST).await;
}

/// POST a JSON `{"user_id": ...}` body to a host endpoint as `caller`.
async fn post_host_action(
    pool: &sqlx::PgPool,
    room_id: &str,
    action: &str,
    caller: &str,
    target: &str,
) -> axum::response::Response {
    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "POST",
        &format!("/api/v1/meetings/{room_id}/{action}"),
        caller,
    )
    .header("Content-Type", "application/json")
    .body(Body::from(format!(r#"{{"user_id":"{target}"}}"#)))
    .unwrap();
    app.oneshot(req).await.unwrap()
}

async fn fetch_is_host(pool: &sqlx::PgPool, room_id: &str, user_id: &str) -> bool {
    let meeting = db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists");
    let row = db_participants::get_status(pool, meeting.id, user_id)
        .await
        .expect("get_status")
        .expect("participant row exists");
    row.is_host
}

async fn fetch_meeting_state(pool: &sqlx::PgPool, room_id: &str) -> Option<String> {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists")
        .state
}

async fn fetch_creator_id(pool: &sqlx::PgPool, room_id: &str) -> Option<String> {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists")
        .creator_id
}

fn decode_token(token: &str) -> RoomAccessTokenClaims {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(TEST_JWT_SECRET.as_bytes()),
        &validation,
    )
    .expect("token should be a valid JWT signed with test secret")
    .claims
}

/// Leave the meeting as `user`. Returns the response.
async fn leave(pool: &sqlx::PgPool, room_id: &str, user: &str) -> axum::response::Response {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/leave"), user)
        .body(Body::empty())
        .unwrap();
    app.oneshot(req).await.unwrap()
}

// ── transfer-host ──────────────────────────────────────────────────────────

/// Transfer is atomic and single-host: the target becomes host, the caller is
/// demoted, and `meetings.creator_id` is never rewritten.
#[tokio::test]
#[serial]
async fn transfer_promotes_target_and_demotes_caller() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-atomic";
    setup_with_admitted_participant(&pool, room_id).await;

    let resp = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert!(
        fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "transfer target must become host"
    );
    assert!(
        !fetch_is_host(&pool, room_id, HOST).await,
        "transfer source (caller) must be demoted"
    );
    assert_eq!(
        fetch_creator_id(&pool, room_id).await.as_deref(),
        Some(HOST),
        "creator_id must remain the original creator after transfer"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// A non-host caller is rejected with 403 NOT_HOST.
#[tokio::test]
#[serial]
async fn transfer_requires_caller_host() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-requires-host";
    setup_with_admitted_participant(&pool, room_id).await;

    let resp = post_host_action(&pool, room_id, "transfer-host", PARTICIPANT, HOST).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body: APIResponse<APIError> = response_json(resp).await;
    assert_eq!(body.result.code, "NOT_HOST");

    cleanup_test_data(&pool, room_id).await;
}

/// Transferring to yourself is rejected with 400.
#[tokio::test]
#[serial]
async fn transfer_rejects_self() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-rejects-self";
    setup_active_meeting(&pool, room_id).await;

    let resp = post_host_action(&pool, room_id, "transfer-host", HOST, HOST).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    cleanup_test_data(&pool, room_id).await;
}

/// Transfer to a guest is rejected with 400 (and the caller keeps host).
#[tokio::test]
#[serial]
async fn transfer_rejects_guest_target() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-rejects-guest";
    setup_active_meeting(&pool, room_id).await;

    let resp = post_host_action(&pool, room_id, "transfer-host", HOST, "guest:xyz").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        fetch_is_host(&pool, room_id, HOST).await,
        "caller must keep host when transfer is rejected"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Transfer to a non-admitted user is rejected with 400, and the transaction
/// rolls back so the caller is NOT demoted (no successor → no step-down).
#[tokio::test]
#[serial]
async fn transfer_rejects_non_admitted_target_keeps_caller_host() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-rejects-non-admitted";
    setup_active_meeting(&pool, room_id).await;

    let resp = post_host_action(
        &pool,
        room_id,
        "transfer-host",
        HOST,
        "stranger@example.com",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(
        fetch_is_host(&pool, room_id, HOST).await,
        "caller must keep host when the transfer target is not admitted (rollback)"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// On the un-fixed code (promote-first + unconditional demote, no row lock /
/// no `is_host` guard) this same setup would promote the admitted target `A`
/// and return `Some`, producing a second host — both assertions below fail.
#[tokio::test]
#[serial]
async fn transfer_from_already_demoted_caller_is_noop() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-already-demoted-caller-noop";
    setup_with_admitted_participant(&pool, room_id).await;

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .expect("get_by_room_id")
        .expect("meeting exists");

    // Simulate the race-loser state: a concurrent transfer already demoted H.
    sqlx::query(
        "UPDATE meeting_participants SET is_host = FALSE WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting.id)
    .bind(HOST)
    .execute(&pool)
    .await
    .expect("out-of-band demote of host");

    // The race loser: H is no longer host, so the guarded demote affects 0 rows
    // and the whole transfer rolls back to a no-op.
    let result = db_participants::transfer_host(&pool, meeting.id, HOST, PARTICIPANT)
        .await
        .expect("transfer_host must not error");
    assert!(
        result.is_none(),
        "a transfer from an already-demoted caller must be a no-op (Ok(None))"
    );

    // No second host was created: the target was never promoted.
    assert!(
        !fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "the target must NOT be promoted when the caller was no longer host"
    );
    assert!(
        !fetch_is_host(&pool, room_id, HOST).await,
        "the (already-demoted) caller must remain non-host"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// After a transfer the target's `/status` returns is_host=true with a
/// room_token that decodes to is_host=true.
#[tokio::test]
#[serial]
async fn transferred_target_status_returns_host_token() {
    let pool = get_test_pool().await;
    let room_id = "test-transferred-status-host-token";
    setup_with_admitted_participant(&pool, room_id).await;

    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;

    let app = build_app(pool.clone());
    let req = request_with_cookie(
        "GET",
        &format!("/api/v1/meetings/{room_id}/status"),
        PARTICIPANT,
    )
    .body(Body::empty())
    .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: APIResponse<ParticipantStatusResponse> = response_json(resp).await;
    assert!(
        body.result.is_host,
        "transferred user's status must be host"
    );
    let token = body
        .result
        .room_token
        .expect("admitted user must receive a room_token");
    let claims = decode_token(&token);
    assert!(
        claims.is_host,
        "room_token must encode is_host=true for the transfer target"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// After a transfer the new host CAN remove the ex-creator — the creator has no
/// special kick immunity in the single-host model (the host is the authority).
#[tokio::test]
#[serial]
async fn new_host_can_kick_ex_creator() {
    let pool = get_test_pool().await;
    let room_id = "test-new-host-kicks-ex-creator";
    setup_with_admitted_participant(&pool, room_id).await;
    // Transfer host to PARTICIPANT — now PARTICIPANT is the host and HOST is a
    // plain participant.
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;

    // The new host removes the ex-creator — allowed.
    let resp = post_host_action(&pool, room_id, "kick", PARTICIPANT, HOST).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    let row = db_participants::get_status(&pool, meeting.id, HOST)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        row.status, "kicked",
        "the ex-creator must be removable by the new host"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── single-host reset ──────────────────────────────────────────────────────

/// On meeting end the transfer target is demoted (single-host reset for the
/// next activation).
#[tokio::test]
#[serial]
async fn transfer_target_demoted_on_end() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-target-demoted-on-end";
    setup_with_admitted_participant(&pool, room_id).await;
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;
    assert!(fetch_is_host(&pool, room_id, PARTICIPANT).await);

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    db_meetings::end_meeting(&pool, meeting.id).await.unwrap();

    assert!(
        !fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "the transfer target must be demoted on meeting end"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// While the meeting is ACTIVE the host stays the transfer target — the creator
/// rejoining does NOT reclaim host (and never becomes a second host).
#[tokio::test]
#[serial]
async fn creator_rejoin_active_does_not_reclaim_host() {
    let pool = get_test_pool().await;
    let room_id = "test-creator-rejoin-no-reclaim";
    setup_with_admitted_participant(&pool, room_id).await;
    // Transfer host to PARTICIPANT (eohl default true), then the ex-creator
    // leaves — the meeting stays active because PARTICIPANT (the host) remains.
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;
    assert!(fetch_is_host(&pool, room_id, PARTICIPANT).await);
    let _ = leave(&pool, room_id, HOST).await;
    assert_ne!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "the meeting must stay active (transfer target is still the host)"
    );

    // Creator rejoins the ACTIVE meeting → must NOT reclaim host; PARTICIPANT
    // stays the sole host.
    join(&pool, room_id, HOST).await;
    assert!(
        !fetch_is_host(&pool, room_id, HOST).await,
        "the creator must not reclaim host while the meeting is active"
    );
    assert!(
        fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "the transfer target stays host while the meeting is active"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// After the meeting ends, the creator becomes the sole host again on the next
/// (re)activation — the transfer is per-meeting-instance.
#[tokio::test]
#[serial]
async fn creator_rehosts_on_reactivation_after_end() {
    let pool = get_test_pool().await;
    let room_id = "test-creator-rehost-on-reactivation";
    setup_with_admitted_participant(&pool, room_id).await;
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;
    assert!(fetch_is_host(&pool, room_id, PARTICIPANT).await);

    // End the meeting (resets host to the creator for the next activation).
    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    db_meetings::end_meeting(&pool, meeting.id).await.unwrap();

    // Creator rejoins → reactivates → becomes sole host; the previous transfer
    // target is no longer host.
    join(&pool, room_id, HOST).await;
    assert!(
        fetch_is_host(&pool, room_id, HOST).await,
        "the creator must become host again on reactivation after end"
    );
    assert!(
        !fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "the previous transfer target must not be host after reactivation"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── host-leave continuity (single host) ────────────────────────────────────

/// end_on_host_leave=true: the host leaving ends the meeting for everyone.
#[tokio::test]
#[serial]
async fn creator_leave_ends_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-creator-leave-ends";
    setup_with_admitted_participant(&pool, room_id).await;

    let resp = leave(&pool, room_id, HOST).await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "the host leaving must end the meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// After a transfer, the (now non-host) ex-creator leaving does NOT end the
/// meeting — host moved to the transfer target, who keeps it alive.
#[tokio::test]
#[serial]
async fn transfer_then_ex_creator_leave_keeps_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-transfer-ex-creator-leave-keeps";
    setup_with_admitted_participant(&pool, room_id).await;
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;
    assert!(!fetch_is_host(&pool, room_id, HOST).await);

    let resp = leave(&pool, room_id, HOST).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_ne!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "a transferred-away ex-creator leaving must not end the meeting"
    );
    assert!(
        fetch_is_host(&pool, room_id, PARTICIPANT).await,
        "the transfer target remains host"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// After a transfer, the NEW host (transfer target) leaving DOES end the meeting
/// under eohl — the single current host left.
#[tokio::test]
#[serial]
async fn transferred_host_leave_ends_meeting() {
    let pool = get_test_pool().await;
    let room_id = "test-transferred-host-leave-ends";
    setup_with_admitted_participant(&pool, room_id).await;
    let _ = post_host_action(&pool, room_id, "transfer-host", HOST, PARTICIPANT).await;

    // PARTICIPANT is now the host; their leave ends the meeting even though the
    // ex-creator (HOST) is still present.
    let resp = leave(&pool, room_id, PARTICIPANT).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "the current host (transfer target) leaving must end the meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// With end_on_host_leave=OFF the host leaving keeps the meeting alive, and the
/// creator retains is_host so a rejoin returns them as host.
#[tokio::test]
#[serial]
async fn creator_keeps_host_on_leave_when_eohl_off() {
    let pool = get_test_pool().await;
    let room_id = "test-creator-keeps-host-on-leave";
    cleanup_test_data(&pool, room_id).await;
    create_meeting_with_eohl(&pool, room_id, false).await;
    join(&pool, room_id, HOST).await;
    join(&pool, room_id, PARTICIPANT).await;
    admit(&pool, room_id, PARTICIPANT).await;

    let _ = leave(&pool, room_id, HOST).await;
    assert_ne!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "eohl=off: meeting must stay alive after the host leaves"
    );
    assert!(
        fetch_is_host(&pool, room_id, HOST).await,
        "the creator must keep host even after leaving (eohl off)"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Host-last-out pin: with end_on_host_leave=OFF, the host leaving
/// as the SOLE admitted participant must keep the meeting alive. This is the
/// path the collapsed leave logic broke — it dropped Rule (b) and fell through
/// to the empty-room end (`count_admitted == 0 → end`). Here the host is
/// alone, so `count_admitted` becomes 0 on leave: only Rule (b) keeps it alive.
#[tokio::test]
#[serial]
async fn eohl_off_sole_host_leave_keeps_meeting_alive() {
    let pool = get_test_pool().await;
    let room_id = "test-eohl-off-sole-host-leave-keeps";
    cleanup_test_data(&pool, room_id).await;
    create_meeting_with_eohl(&pool, room_id, false).await;
    // Only the host joins — they are the single admitted participant.
    join(&pool, room_id, HOST).await;

    let resp = leave(&pool, room_id, HOST).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_ne!(
        fetch_meeting_state(&pool, room_id).await.as_deref(),
        Some("ended"),
        "eohl=off: the sole host leaving must NOT end the meeting (Rule b), \
         even though the room is now empty"
    );
    assert!(
        fetch_is_host(&pool, room_id, HOST).await,
        "eohl=off: the host keeps is_host after leaving so a rejoin returns them as host"
    );

    cleanup_test_data(&pool, room_id).await;
}
