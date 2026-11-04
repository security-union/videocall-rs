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

//! Integration tests for the relay presence reports on
//! `internal.participant_presence` (issues #1551, #1628, #2702) and the
//! `idle ⟺ zero present` display invariant. Requires `DATABASE_URL`; the
//! NATS end-to-end tests also need `NATS_URL` and skip without it.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use futures::StreamExt;
use meeting_api::db::meetings as db_meetings;
use meeting_api::db::participants as db_participants;
use meeting_api::feed_events::{new_feed_channel, FeedChange, FeedChangeReason};
use meeting_api::nats_consumers::{
    apply_participant_presence, end_due_host_left_meetings,
    spawn_participant_presence_consumer_inner,
};
use meeting_api::nats_events::{ParticipantPresencePayload, PARTICIPANT_PRESENCE_SUBJECT};
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ListFeedResponse};

const S1: u64 = 0x8000_0000_0000_0011;
const S2: u64 = 0x8000_0000_0000_0022;

async fn create_meeting(pool: &PgPool, host: &str, room_id: &str, extra: serde_json::Value) {
    cleanup_test_data(pool, room_id).await;
    let mut body = serde_json::json!({ "meeting_id": room_id, "attendees": [] });
    for (k, v) in extra.as_object().expect("extra must be an object") {
        body[k] = v.clone();
    }
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create must succeed");
}

/// Waiting room off, so every joiner is auto-admitted.
async fn create_meeting_wr_off(pool: &PgPool, host: &str, room_id: &str) {
    create_meeting(
        pool,
        host,
        room_id,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
}

async fn send(pool: &PgPool, method: &str, uri: &str, caller: &str) -> StatusCode {
    let req = request_with_cookie(method, uri, caller)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Tester"}"#))
        .unwrap();
    build_app(pool.clone()).oneshot(req).await.unwrap().status()
}

async fn join(pool: &PgPool, room_id: &str, email: &str) {
    let status = send(
        pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        email,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "join must succeed for {email}");
}

async fn meeting_pk(pool: &PgPool, room_id: &str) -> i32 {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .unwrap()
        .expect("meeting row must exist")
        .id
}

async fn meeting(pool: &PgPool, room_id: &str) -> db_meetings::MeetingRow {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .unwrap()
        .expect("meeting row must exist")
}

async fn status_of(pool: &PgPool, room_id: &str, user: &str) -> db_participants::ParticipantRow {
    db_participants::get_status(pool, meeting_pk(pool, room_id).await, user)
        .await
        .unwrap()
        .expect("participant row must exist")
}

async fn force_state(pool: &PgPool, meeting_pk: i32, state: &str) {
    sqlx::query("UPDATE meetings SET state = $1 WHERE id = $2")
        .bind(state)
        .bind(meeting_pk)
        .execute(pool)
        .await
        .expect("force_state UPDATE must succeed");
}

fn report(room_id: &str, user: &str, session_id: u64, present: bool) -> ParticipantPresencePayload {
    ParticipantPresencePayload {
        room_id: room_id.to_string(),
        user_id: user.to_string(),
        session_id,
        present,
    }
}

/// Apply one presence report through the production consumer function.
async fn apply(
    pool: &PgPool,
    room_id: &str,
    user: &str,
    session_id: u64,
    present: bool,
) -> Option<FeedChange> {
    let (feed_tx, mut feed_rx) = new_feed_channel();
    apply_participant_presence(
        pool,
        None,
        &feed_tx,
        &meeting(pool, room_id).await,
        &report(room_id, user, session_id, present),
    )
    .await;
    feed_rx.try_recv().ok()
}

/// Let the deferred host-left end of `room_id` fall due, then run the
/// production settle pass. Returns `room_id`'s feed nudge from that pass.
async fn settle_host_left(pool: &PgPool, room_id: &str) -> Option<FeedChangeReason> {
    sqlx::query(
        "UPDATE meetings SET host_left_end_due_at = NOW() - INTERVAL '1 second' \
         WHERE id = $1 AND host_left_end_due_at IS NOT NULL",
    )
    .bind(meeting_pk(pool, room_id).await)
    .execute(pool)
    .await
    .expect("elapse the debounce window");
    let healthy = db_participants::presence_healthy(pool, None)
        .await
        .unwrap_or(false);
    let (feed_tx, mut feed_rx) = new_feed_channel();
    end_due_host_left_meetings(pool, None, &feed_tx, healthy)
        .await
        .expect("settle pass");
    std::iter::from_fn(|| feed_rx.try_recv().ok())
        .find(|c| c.meeting_id == room_id)
        .map(|c| c.reason)
}

async fn list_feed(pool: &PgPool, caller: &str) -> APIResponse<ListFeedResponse> {
    let req = request_with_cookie("GET", "/api/v1/meetings/feed", caller)
        .body(Body::empty())
        .unwrap();
    let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "list feed must return 200");
    response_json(resp).await
}

// ── The `idle ⟺ zero present` display invariant (issue #1628) ───────────────

#[tokio::test]
#[serial]
async fn feed_never_reports_idle_with_present_participant() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host@example.com";
    let room_id = "pp-invariant-idle-with-people";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    force_state(&pool, meeting_pk(&pool, room_id).await, "idle").await;

    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(row.participant_count, 1);
    assert_eq!(row.state, "active");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn feed_reports_idle_when_no_one_present_even_if_column_says_active() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host2@example.com";
    let room_id = "pp-invariant-active-empty";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let pk = meeting_pk(&pool, room_id).await;
    sqlx::query(
        "UPDATE meeting_participants SET status = 'left', left_at = NOW() WHERE meeting_id = $1",
    )
    .bind(pk)
    .execute(&pool)
    .await
    .expect("mark all left must succeed");
    force_state(&pool, pk, "active").await;

    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(row.state, "idle");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn feed_reports_ended_even_with_present_row() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host3@example.com";
    let room_id = "pp-invariant-ended-terminal";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    db_meetings::end_meeting(&pool, meeting_pk(&pool, room_id).await)
        .await
        .expect("end_meeting must succeed");

    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(row.state, "ended");

    cleanup_test_data(&pool, room_id).await;
}

/// The defense-in-depth `left_at IS NULL` guard on both counts: a forged
/// `admitted`/`waiting` row with `left_at` set is not counted.
#[tokio::test]
#[serial]
async fn admitted_with_left_at_is_excluded_from_count() {
    let pool = get_test_pool().await;
    let room_id = "test-participant-left-guard";
    let host = "plg-host@example.com";
    let stale = "plg-stale@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, stale).await;
    let pk = meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk, true)
            .await
            .unwrap(),
        2
    );

    sqlx::query(
        "UPDATE meeting_participants SET left_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted'",
    )
    .bind(pk)
    .bind(stale)
    .execute(&pool)
    .await
    .expect("raw UPDATE must succeed");
    assert_eq!(
        db_participants::count_admitted(&pool, pk, true)
            .await
            .unwrap(),
        1
    );

    sqlx::query(
        "UPDATE meeting_participants SET status = 'waiting' WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(pk)
    .bind(stale)
    .execute(&pool)
    .await
    .expect("flip to waiting must succeed");
    assert_eq!(db_participants::count_waiting(&pool, pk).await.unwrap(), 0);

    cleanup_test_data(&pool, room_id).await;
}

// ── LEFT reports: only the live session departs ─────────────────────────────

#[tokio::test]
#[serial]
async fn left_of_the_live_session_marks_left_and_nudges() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-live";
    let host = "pl-host@example.com";
    let ghost = "pl-ghost@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;

    apply(&pool, room_id, ghost, S1, true).await;
    let nudge = apply(&pool, room_id, ghost, S1, false).await;

    let row = status_of(&pool, room_id, ghost).await;
    assert_eq!(row.status, "left");
    assert!(row.left_at.is_some());
    assert_eq!(
        nudge.map(|c| c.reason),
        Some(FeedChangeReason::ParticipantLeft)
    );
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn stale_left_after_a_rest_rejoin_has_no_effect() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-stale-after-rejoin";
    let host = "pls-host@example.com";
    create_meeting(&pool, host, room_id, serde_json::json!({})).await;
    join(&pool, room_id, host).await;
    apply(&pool, room_id, host, S1, true).await;

    // The owner reloads: a REST join, then the old session's LEFT arrives late.
    join(&pool, room_id, host).await;
    let nudge = apply(&pool, room_id, host, S1, false).await;

    let row = status_of(&pool, room_id, host).await;
    assert_eq!(row.status, "admitted");
    assert!(row.left_at.is_none());
    assert!(nudge.is_none());
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active"),
        "the only host's stale departure must not end an end-on-host-leave meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn left_of_an_old_session_after_a_newer_present_has_no_effect() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-old-session";
    let host = "plo-host@example.com";
    create_meeting(&pool, host, room_id, serde_json::json!({})).await;
    join(&pool, room_id, host).await;

    // WT session S1, then a re-election onto WS session S2; S1's LEFT lands last.
    apply(&pool, room_id, host, S1, true).await;
    apply(&pool, room_id, host, S2, true).await;
    apply(&pool, room_id, host, S1, false).await;

    assert_eq!(status_of(&pool, room_id, host).await.status, "admitted");
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    apply(&pool, room_id, host, S2, false).await;
    assert_eq!(status_of(&pool, room_id, host).await.status, "left");
    settle_host_left(&pool, room_id).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended"),
        "the live session's departure still ends the meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn last_present_host_left_ends_the_meeting_only_when_the_last_host_goes() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-last-host";
    let host = "plh-host@example.com";
    let co = "plh-cohost@example.com";
    let attendee = "plh-attendee@example.com";
    create_meeting(
        &pool,
        host,
        room_id,
        serde_json::json!({ "co_hosts": [co], "waiting_room_enabled": false }),
    )
    .await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, co).await;
    join(&pool, room_id, attendee).await;
    apply(&pool, room_id, host, S1, true).await;
    apply(&pool, room_id, co, S2, true).await;

    apply(&pool, room_id, co, S2, false).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    apply(&pool, room_id, host, S1, false).await;
    let nudge = settle_host_left(&pool, room_id).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );
    assert_eq!(nudge, Some(FeedChangeReason::Ended));

    cleanup_test_data(&pool, room_id).await;
}

// ── Last-host LEFT is debounced ─────────────────────────────────────────────

/// Sole host reloads and re-elects onto the other relay: the old relay's LEFT
/// lands before the new session's PRESENT.
#[tokio::test]
#[serial]
async fn host_left_then_present_on_another_relay_keeps_the_meeting() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-before-present";
    let host = "phlp-host@example.com";
    let attendee = "phlp-attendee@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, attendee).await;
    apply(&pool, room_id, host, S1, true).await;
    apply(&pool, room_id, attendee, S2, true).await;

    let nudge = apply(&pool, room_id, host, S1, false).await;
    assert_eq!(
        nudge.map(|c| c.reason),
        Some(FeedChangeReason::ParticipantLeft)
    );
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active"),
        "a relay-reported last-host departure must not end the meeting at once"
    );

    const S3: u64 = 0x8000_0000_0000_0033;
    apply(&pool, room_id, host, S3, true).await;
    let nudge = settle_host_left(&pool, room_id).await;

    assert_eq!(nudge, None);
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );
    let row = status_of(&pool, room_id, host).await;
    assert_eq!(row.status, "admitted");
    assert!(row.is_host);
    assert!(row.left_at.is_none());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn host_left_without_return_ends_the_meeting_once_due() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-no-return";
    let host = "phln-host@example.com";
    let attendee = "phln-attendee@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, attendee).await;
    apply(&pool, room_id, host, S1, true).await;
    apply(&pool, room_id, attendee, S2, true).await;

    apply(&pool, room_id, host, S1, false).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );
    let (feed_tx, _feed_rx) = new_feed_channel();
    end_due_host_left_meetings(&pool, None, &feed_tx, false)
        .await
        .unwrap();
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active"),
        "nothing ends before the debounce window passes"
    );

    let nudge = settle_host_left(&pool, room_id).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );
    assert_eq!(nudge, Some(FeedChangeReason::Ended));
    assert_eq!(
        settle_host_left(&pool, room_id).await,
        None,
        "a settled end is not re-announced"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn host_present_before_stale_left_defers_nothing() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-present-before-left";
    let host = "phpl-host@example.com";
    create_meeting(&pool, host, room_id, serde_json::json!({})).await;
    join(&pool, room_id, host).await;
    apply(&pool, room_id, host, S1, true).await;

    apply(&pool, room_id, host, S2, true).await;
    assert!(apply(&pool, room_id, host, S1, false).await.is_none());

    assert_eq!(settle_host_left(&pool, room_id).await, None);
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );
    assert_eq!(status_of(&pool, room_id, host).await.status, "admitted");

    cleanup_test_data(&pool, room_id).await;
}

async fn due_in_secs(pool: &PgPool, room_id: &str) -> Option<f64> {
    sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM host_left_end_due_at - NOW())::FLOAT8 \
         FROM meetings WHERE room_id = $1",
    )
    .bind(room_id)
    .fetch_one(pool)
    .await
    .expect("read host_left_end_due_at")
}

/// The sole host's relay LEFT leaves the meeting idle with its end pending.
async fn sole_host_left(pool: &PgPool, room_id: &str, host: &str, extra: serde_json::Value) {
    create_meeting(pool, host, room_id, extra).await;
    join(pool, room_id, host).await;
    apply(pool, room_id, host, S1, true).await;
    apply(pool, room_id, host, S1, false).await;
    assert_eq!(meeting(pool, room_id).await.state.as_deref(), Some("idle"));
}

#[tokio::test]
#[serial]
async fn host_left_end_is_due_a_debounce_window_after_the_left() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-due-at";
    let host = "phld-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;

    let due = due_in_secs(&pool, room_id)
        .await
        .expect("an end is pending");
    assert!(
        (64.0..=65.5).contains(&due),
        "the end must be due ~65 s after the LEFT, got {due}"
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_host_left_end_is_not_settled_before_it_is_due() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-not-due";
    let host = "phnd-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;
    let pk = meeting_pk(&pool, room_id).await;

    assert!(!db_participants::end_host_left_if_due(&pool, pk, true)
        .await
        .expect("settle one"));

    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));
    assert!(due_in_secs(&pool, room_id).await.is_some());

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn an_attendee_cannot_revive_a_meeting_whose_host_left_end_is_pending() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-attendee-refused";
    let host = "phar-host@example.com";
    let attendee = "phar-attendee@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;

    let status = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        attendee,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "MEETING_NOT_ACTIVE");
    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));

    assert_eq!(
        settle_host_left(&pool, room_id).await,
        Some(FeedChangeReason::Ended)
    );
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn the_owner_restarting_drops_the_pending_host_left_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-owner-restart";
    let host = "phor-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;

    join(&pool, room_id, host).await;

    assert_eq!(due_in_secs(&pool, room_id).await, None);
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_co_host_restarting_drops_the_pending_host_left_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-cohost-restart";
    let host = "phcr-host@example.com";
    let co = "phcr-cohost@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "co_hosts": [co], "waiting_room_enabled": false }),
    )
    .await;

    join(&pool, room_id, co).await;

    assert_eq!(due_in_secs(&pool, room_id).await, None);
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn turning_end_on_host_leave_off_cancels_the_pending_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-option-off";
    let host = "phoo-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;

    db_meetings::update_meeting_settings(
        &pool,
        room_id,
        None,
        None,
        Some(false),
        None,
        None,
        None,
        Default::default(),
        &meeting_api::password::PasswordUpdate::Unchanged,
    )
    .await
    .expect("update settings")
    .expect("meeting exists");

    assert_eq!(settle_host_left(&pool, room_id).await, None);
    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn only_the_settle_lock_holder_ends_due_meetings() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-settle-lock";
    let host = "phsl-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;
    let mut other_replica = pool.acquire().await.expect("connection");
    sqlx::query("SELECT pg_advisory_lock(3156001)")
        .execute(&mut *other_replica)
        .await
        .expect("take the settle lock");

    let settled = settle_host_left(&pool, room_id).await;
    sqlx::query("SELECT pg_advisory_unlock(3156001)")
        .execute(&mut *other_replica)
        .await
        .expect("release the settle lock");

    assert_eq!(settled, None, "a replica without the lock must not settle");
    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_deleted_meeting_is_not_ended_by_its_pending_host_left_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-deleted";
    let host = "phdl-host@example.com";
    sole_host_left(&pool, room_id, host, serde_json::json!({})).await;
    let pk = meeting_pk(&pool, room_id).await;
    db_meetings::soft_delete(&pool, room_id, host)
        .await
        .expect("soft delete")
        .expect("meeting deleted");
    assert_eq!(due_in_secs(&pool, room_id).await, None);

    // A LEFT whose meeting lookup predates the delete still commits a deadline.
    sqlx::query("UPDATE meetings SET host_left_end_due_at = NOW() WHERE id = $1")
        .bind(pk)
        .execute(&pool)
        .await
        .expect("elapse the debounce window");

    let (feed_tx, mut feed_rx) = new_feed_channel();
    end_due_host_left_meetings(&pool, None, &feed_tx, false)
        .await
        .expect("settle pass");
    assert!(
        std::iter::from_fn(|| feed_rx.try_recv().ok()).all(|c| c.meeting_id != room_id),
        "the sweep must not announce a deleted meeting"
    );
    assert!(!db_participants::host_left_ends_due(&pool, i64::MAX)
        .await
        .expect("list due ends")
        .iter()
        .any(|(id, _)| *id == pk));
    assert!(!db_participants::end_host_left_if_due(&pool, pk, false)
        .await
        .expect("settle one"));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_activation_refuses_a_pending_or_settled_host_left_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-locked-activation";
    let host = "phla-host@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    let pk = meeting_pk(&pool, room_id).await;

    assert!(db_meetings::start_instance_for_attendee(&pool, pk, false)
        .await
        .expect("pending")
        .is_none());
    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));

    // The end settles after a joiner read its `idle` snapshot.
    settle_host_left(&pool, room_id).await;
    assert!(db_meetings::start_instance_for_attendee(&pool, pk, false)
        .await
        .expect("ended")
        .is_none());
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );

    cleanup_test_data(&pool, room_id).await;
}

/// The meeting ends while a joiner's activation is in flight: it must wait
/// for that end and refuse, not decide from the row as it was.
#[tokio::test]
#[serial]
async fn attendee_activation_waits_for_a_concurrent_end() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-concurrent-end";
    let host = "phce-host@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "waiting_room_enabled": false }),
    )
    .await;
    let pk = meeting_pk(&pool, room_id).await;
    sqlx::query("UPDATE meetings SET host_left_end_due_at = NULL WHERE id = $1")
        .bind(pk)
        .execute(&pool)
        .await
        .expect("start from an idle meeting with nothing pending");

    let mut ending = pool.begin().await.expect("begin");
    sqlx::query("SELECT id FROM meetings WHERE id = $1 FOR UPDATE")
        .bind(pk)
        .execute(&mut *ending)
        .await
        .expect("lock the meeting");
    db_meetings::end_meeting_in(&mut ending, pk)
        .await
        .expect("end, uncommitted");

    let joiner = tokio::spawn({
        let pool = pool.clone();
        async move { db_meetings::start_instance_for_attendee(&pool, pk, false).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!joiner.is_finished(), "activation must wait for the lock");
    ending.commit().await.expect("commit the end");

    let activation = tokio::time::timeout(std::time::Duration::from_secs(10), joiner)
        .await
        .expect("activation finishes once the end commits")
        .expect("join task")
        .expect("activation query");
    assert!(activation.is_none(), "got {activation:?}");
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn attendee_activation_reopens_a_keep_going_meeting() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-keep-going";
    let host = "phkg-host@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "waiting_room_enabled": false, "end_on_host_leave": false }),
    )
    .await;
    let pk = meeting_pk(&pool, room_id).await;
    db_meetings::end_meeting(&pool, pk).await.expect("end");

    assert!(db_meetings::start_instance_for_attendee(&pool, pk, false)
        .await
        .expect("reopen")
        .is_some());
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn a_waiting_room_joiner_is_refused_while_the_host_left_end_is_pending() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-left-wr-refused";
    let host = "phwr-host@example.com";
    let attendee = "phwr-attendee@example.com";
    sole_host_left(
        &pool,
        room_id,
        host,
        serde_json::json!({ "waiting_room_enabled": true }),
    )
    .await;

    let status = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/join"),
        attendee,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST, "MEETING_NOT_ACTIVE");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn rest_leave_by_the_last_host_still_ends_immediately() {
    let pool = get_test_pool().await;
    let room_id = "pp-host-rest-leave";
    let host = "phrl-host@example.com";
    let attendee = "phrl-attendee@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, attendee).await;
    apply(&pool, room_id, host, S1, true).await;

    let status = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        host,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn duplicate_left_is_idempotent() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-duplicate";
    let host = "pld-host@example.com";
    let leaver = "pld-leaver@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, leaver).await;
    apply(&pool, room_id, leaver, S1, true).await;

    apply(&pool, room_id, leaver, S1, false).await;
    let left_at = status_of(&pool, room_id, leaver).await.left_at;
    let nudge = apply(&pool, room_id, leaver, S1, false).await;

    assert!(nudge.is_none(), "a repeated LEFT changes nothing");
    assert_eq!(status_of(&pool, room_id, leaver).await.left_at, left_at);

    cleanup_test_data(&pool, room_id).await;
}

/// A second replica that lags one session behind: it applies PRESENT(s) after
/// the first replica already applied LEFT(s). The session stays gone.
#[tokio::test]
#[serial]
async fn lagging_replica_cannot_bring_back_a_session_that_left() {
    let pool = get_test_pool().await;
    let room_id = "pp-lagging-replica";
    let host = "plr-host@example.com";
    let ghost = "plr-ghost@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;

    apply(&pool, room_id, ghost, S1, true).await;
    apply(&pool, room_id, ghost, S1, false).await;
    let nudge = apply(&pool, room_id, ghost, S1, true).await;
    assert!(nudge.is_none());
    assert_eq!(status_of(&pool, room_id, ghost).await.status, "left");

    apply(&pool, room_id, ghost, S1, false).await;
    assert_eq!(status_of(&pool, room_id, ghost).await.status, "left");

    cleanup_test_data(&pool, room_id).await;
}

/// A row written before presence tracking existed has no live session; its
/// departure still applies, so meetings in progress across the deploy keep working.
#[tokio::test]
#[serial]
async fn left_applies_to_a_row_that_predates_presence_tracking() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-legacy-row";
    let host = "pll-host@example.com";
    let ghost = "pll-ghost@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    sqlx::query(
        "UPDATE meeting_participants SET live_session_id = NULL \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_pk(&pool, room_id).await)
    .bind(ghost)
    .execute(&pool)
    .await
    .expect("forge a pre-migration row");

    apply(&pool, room_id, ghost, S1, false).await;
    assert_eq!(status_of(&pool, room_id, ghost).await.status, "left");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn last_departure_sets_the_meeting_idle() {
    let pool = get_test_pool().await;
    let room_id = "pp-left-idle";
    let host = "pli-host@example.com";
    create_meeting(
        &pool,
        host,
        room_id,
        serde_json::json!({ "end_on_host_leave": false }),
    )
    .await;
    join(&pool, room_id, host).await;
    apply(&pool, room_id, host, S1, true).await;

    apply(&pool, room_id, host, S1, false).await;
    assert_eq!(meeting(&pool, room_id).await.state.as_deref(), Some("idle"));

    cleanup_test_data(&pool, room_id).await;
}

// ── PRESENT reports ─────────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn present_restores_a_participant_marked_left() {
    let pool = get_test_pool().await;
    let host = "pp-mp-host@example.com";
    let ghost = "pp-mp-ghost@example.com";
    let room_id = "pp-mark-present-restore";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    apply(&pool, room_id, ghost, S1, true).await;
    apply(&pool, room_id, ghost, S1, false).await;

    let nudge = apply(&pool, room_id, ghost, S2, true).await;

    let row = status_of(&pool, room_id, ghost).await;
    assert_eq!(row.status, "admitted");
    assert!(row.left_at.is_none());
    assert_eq!(nudge.map(|c| c.reason), Some(FeedChangeReason::Joined));
    let pk = meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk, true)
            .await
            .unwrap(),
        2
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn present_does_not_admit_a_waiting_participant() {
    let pool = get_test_pool().await;
    let host = "pp-mp-wr-host@example.com";
    let waiter = "pp-mp-waiter@example.com";
    let room_id = "pp-mark-present-waiting-guard";
    create_meeting(&pool, host, room_id, serde_json::json!({})).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, waiter).await;

    apply(&pool, room_id, waiter, S1, true).await;
    assert_eq!(status_of(&pool, room_id, waiter).await.status, "waiting");

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn present_does_not_unkick_a_participant() {
    let pool = get_test_pool().await;
    let host = "pp-mp-kick-host@example.com";
    let kicked = "pp-mp-kicked@example.com";
    let room_id = "pp-mark-present-kick-guard";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, kicked).await;
    let status = send(
        &pool,
        "POST",
        &format!("/api/v1/meetings/{room_id}/leave"),
        kicked,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    sqlx::query(
        "UPDATE meeting_participants SET status = 'kicked' WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(meeting_pk(&pool, room_id).await)
    .bind(kicked)
    .execute(&pool)
    .await
    .expect("force kicked");

    apply(&pool, room_id, kicked, S1, true).await;
    assert_eq!(status_of(&pool, room_id, kicked).await.status, "kicked");

    cleanup_test_data(&pool, room_id).await;
}

/// Resuming an idle meeting someone is present in is not a new instance:
/// `started_at` is kept.
#[tokio::test]
#[serial]
async fn present_resumes_an_idle_meeting_without_a_new_instance() {
    let pool = get_test_pool().await;
    let host = "pp-ap-host@example.com";
    let ghost = "pp-ap-ghost@example.com";
    let room_id = "pp-apply-present-reactivate";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    apply(&pool, room_id, ghost, S1, true).await;
    apply(&pool, room_id, ghost, S1, false).await;
    let pk = meeting_pk(&pool, room_id).await;
    force_state(&pool, pk, "idle").await;
    let started_at = meeting(&pool, room_id).await.started_at;

    let nudge = apply(&pool, room_id, ghost, S2, true).await;

    let after = meeting(&pool, room_id).await;
    assert_eq!(after.state.as_deref(), Some("active"));
    assert_eq!(after.started_at, started_at);
    assert_eq!(status_of(&pool, room_id, ghost).await.status, "admitted");
    assert_eq!(nudge.map(|c| c.reason), Some(FeedChangeReason::Joined));

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn present_never_resurrects_an_ended_meeting() {
    let pool = get_test_pool().await;
    let host = "pp-ap-ended-host@example.com";
    let room_id = "pp-apply-present-ended";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    db_meetings::end_meeting(&pool, meeting_pk(&pool, room_id).await)
        .await
        .expect("end_meeting must succeed");

    apply(&pool, room_id, host, S1, true).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );

    cleanup_test_data(&pool, room_id).await;
}

#[tokio::test]
#[serial]
async fn present_is_a_silent_no_op_when_already_present_and_active() {
    let pool = get_test_pool().await;
    let host = "pp-ap-noop-host@example.com";
    let room_id = "pp-apply-present-noop";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;

    assert!(apply(&pool, room_id, host, S1, true).await.is_none());
    assert!(apply(&pool, room_id, host, S1, true).await.is_none());
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("active")
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── End to end over NATS ────────────────────────────────────────────────────

async fn spawn_consumer(
    pool: &PgPool,
    nats: &async_nats::Client,
) -> tokio::sync::broadcast::Receiver<FeedChange> {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (feed_tx, feed_rx) = new_feed_channel();
    spawn_participant_presence_consumer_inner(
        Some(nats.clone()),
        pool.clone(),
        feed_tx,
        Some(ready_tx),
    )
    .expect("consumer must spawn when NATS is available");
    ready_rx.await.expect("consumer must signal readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    feed_rx
}

async fn publish(nats: &async_nats::Client, payload: &ParticipantPresencePayload) {
    nats.publish(
        PARTICIPANT_PRESENCE_SUBJECT,
        serde_json::to_vec(payload).unwrap().into(),
    )
    .await
    .expect("publish must succeed");
}

#[tokio::test]
#[serial]
async fn presence_reports_over_nats_drive_the_roster() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping participant presence NATS test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "pp-nats-roster";
    let host = "pn-host@example.com";
    let ghost = "pn-ghost@example.com";
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    let pk = meeting_pk(&pool, room_id).await;
    let mut feed_rx = spawn_consumer(&pool, &nats).await;

    publish(
        &nats,
        &report("this-room-does-not-exist-2702", ghost, S1, false),
    )
    .await;
    publish(&nats, &report(room_id, ghost, 0, false)).await;
    publish(&nats, &report(room_id, ghost, S1, true)).await;
    publish(&nats, &report(room_id, ghost, S1, false)).await;

    let change = tokio::time::timeout(std::time::Duration::from_secs(5), feed_rx.recv())
        .await
        .expect("a feed nudge must arrive within 5s")
        .expect("broadcast must not be closed");
    assert_eq!(change.reason, FeedChangeReason::ParticipantLeft);
    assert_eq!(change.meeting_id, room_id);
    assert_eq!(
        db_participants::count_admitted(&pool, pk, true)
            .await
            .unwrap(),
        1
    );
    assert_eq!(status_of(&pool, room_id, ghost).await.status, "left");

    join(&pool, room_id, ghost).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk, true)
            .await
            .unwrap(),
        2
    );

    cleanup_test_data(&pool, room_id).await;
}

/// A designated co-host admitted from the waiting room gets the host role when
/// their transport connects, announced to clients and relays.
#[tokio::test]
#[serial]
async fn co_host_is_promoted_and_announced_when_their_transport_connects() {
    let Some(nats) = maybe_nats().await else {
        eprintln!("NATS_URL not set — skipping promote-on-connect NATS test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "pp-promote-on-connect";
    let owner = "ppc-owner@example.com";
    let co = "ppc-cohost@example.com";
    create_meeting(&pool, owner, room_id, serde_json::json!({})).await;
    join(&pool, room_id, owner).await;
    join(&pool, room_id, co).await;
    for (path, body) in [
        (
            "co-hosts",
            serde_json::json!({ "user_id": co, "persist": false }),
        ),
        ("admit", serde_json::json!({ "user_id": co })),
    ] {
        let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/{path}"), owner)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = build_app(pool.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path} must succeed");
    }
    assert!(
        !status_of(&pool, room_id, co).await.is_host,
        "admitting does not make a co-host a host before they connect"
    );

    let mut host_changes = nats
        .subscribe(meeting_api::nats_events::MEETING_HOST_CHANGE_SUBJECT)
        .await
        .expect("subscribe");
    let (feed_tx, _feed_rx) = new_feed_channel();
    apply_participant_presence(
        &pool,
        Some(&nats),
        &feed_tx,
        &meeting(&pool, room_id).await,
        &report(room_id, co, S1, true),
    )
    .await;

    assert!(status_of(&pool, room_id, co).await.is_host);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), host_changes.next())
        .await
        .expect("a host change must be published within 5s")
        .expect("subscription open");
    let change: meeting_api::nats_events::MeetingHostChangePayload =
        serde_json::from_slice(&msg.payload).expect("payload");
    assert_eq!(
        (
            change.room_id.as_str(),
            change.user_id.as_str(),
            change.is_host
        ),
        (room_id, co, true)
    );

    cleanup_test_data(&pool, room_id).await;
}
