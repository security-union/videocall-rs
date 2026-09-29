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
    apply_participant_presence, spawn_participant_presence_consumer_inner,
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

    let nudge = apply(&pool, room_id, host, S1, false).await;
    assert_eq!(
        meeting(&pool, room_id).await.state.as_deref(),
        Some("ended")
    );
    assert_eq!(nudge.map(|c| c.reason), Some(FeedChangeReason::Ended));

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
