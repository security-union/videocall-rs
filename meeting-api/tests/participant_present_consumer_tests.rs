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

//! Integration tests for the presence-driven mark-present + re-activate path
//! and the `idle ⟺ zero present` display-state invariant (issue #1628).
//!
//! Two distinct bugs are pinned here:
//!
//! 1. **The list/feed showed a meeting `idle` while participants were present.**
//!    The displayed `state` used to be the raw `meetings.state` column, which
//!    could lag at `'idle'` after a transport-only reconnect (the meeting went
//!    empty → `set_idle`, then the participant reconnected over the transport
//!    without re-hitting REST `/join`, so nothing re-activated the column). The
//!    fix derives the displayed state from the live present count via
//!    [`db_meetings::display_state`], applied at every list/feed read site, so a
//!    meeting with ≥1 present participant is NEVER reported `idle`.
//!
//! 2. **Re-activation was asymmetric.** `set_idle` fired on the transport
//!    "room empty" event, but re-activation only happened on a REST `/join`. The
//!    new `internal.participant_present` consumer
//!    ([`nats_consumers::spawn_participant_present_consumer`]) closes the
//!    asymmetry: on a transport (re)connect it restores the participant's
//!    presence row ([`db_participants::mark_present_by_connect`]) and
//!    re-activates the meeting (`idle -> active`) — without ever resurrecting an
//!    `ended` meeting.
//!
//! Tests run against a live Postgres pool via `DATABASE_URL`. The NATS
//! end-to-end test additionally requires `NATS_URL` and skips silently when it
//! is unset, mirroring `participant_left_consumer_tests.rs`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::meetings as db_meetings;
use meeting_api::db::participants as db_participants;
use meeting_api::feed_events::{new_feed_channel, FeedChangeReason};
use meeting_api::nats_consumers::apply_participant_present;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;
use tower::ServiceExt;
use videocall_meeting_types::responses::{APIResponse, ListFeedResponse};

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Create a meeting with the waiting room OFF (so joiners are auto-admitted).
async fn create_meeting_wr_off(pool: &PgPool, host: &str, room_id: &str) {
    cleanup_test_data(pool, room_id).await;
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": false,
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED, "create must succeed");
}

/// `POST /api/v1/meetings/{room_id}/join` as `email` (auto-admitted when WR off).
async fn join(pool: &PgPool, room_id: &str, email: &str) {
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", &format!("/api/v1/meetings/{room_id}/join"), email)
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"display_name":"Tester"}"#))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "join must succeed for {email}"
    );
}

/// Look up the internal `meetings.id` for a `room_id`.
async fn lookup_meeting_pk(pool: &PgPool, room_id: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as("SELECT id FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("meeting row must exist");
    id
}

/// `GET /api/v1/meetings/feed` as `caller`, returning the parsed body.
async fn list_feed(pool: &PgPool, caller: &str) -> APIResponse<ListFeedResponse> {
    let app = build_app(pool.clone());
    let req = request_with_cookie("GET", "/api/v1/meetings/feed", caller)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "list feed must return 200");
    response_json(resp).await
}

/// Raw-write the `meetings.state` column, simulating a column that lagged behind
/// live presence (the stuck-`idle`-with-people-in-it condition).
async fn force_state(pool: &PgPool, meeting_pk: i32, state: &str) {
    sqlx::query("UPDATE meetings SET state = $1 WHERE id = $2")
        .bind(state)
        .bind(meeting_pk)
        .execute(pool)
        .await
        .expect("force_state UPDATE must succeed");
}

// ════════════════════════════════════════════════════════════════════════════
// PART 2 — the `idle ⟺ zero present` invariant, asserted through the REAL
// `GET /api/v1/meetings/feed` route.
// ════════════════════════════════════════════════════════════════════════════

/// A meeting whose raw `meetings.state` column lags at `'idle'` while a
/// participant is present MUST be reported `active` by the feed — never `idle`.
///
/// This is the core Part-2 regression. It forces the exact stuck condition
/// (raw column `'idle'`, one admitted-and-present participant) and asserts the
/// route returns `active`. On the un-fixed code the route returned the raw
/// column value (`"idle"`) verbatim, so this assertion FAILS without the
/// `display_state` derivation.
#[tokio::test]
#[serial]
async fn feed_never_reports_idle_with_present_participant() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host@example.com";
    let room_id = "pp-invariant-idle-with-people";

    // Host creates + joins (auto-admitted, present). Meeting becomes active.
    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        1,
        "precondition: exactly one present participant"
    );

    // Simulate the stuck condition: the column lagged at 'idle' (e.g. a brief
    // empty event landed) while the participant is still present.
    force_state(&pool, pk, "idle").await;
    let raw = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        raw.state.as_deref(),
        Some("idle"),
        "precondition: raw column is forced to 'idle'"
    );

    // The route MUST derive 'active' from the live present count.
    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(
        row.participant_count, 1,
        "the feed's own present count is 1"
    );
    assert_eq!(
        row.state, "active",
        "a meeting with >=1 present participant must be reported 'active', never \
         'idle' — fails on the un-fixed code that surfaced the raw 'idle' column"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// The symmetric direction: a meeting whose raw column lags at `'active'` while
/// NOBODY is present MUST be reported `idle`. Guarantees `idle ⟺ zero present`
/// in both directions, so the count and the state can never contradict.
#[tokio::test]
#[serial]
async fn feed_reports_idle_when_no_one_present_even_if_column_says_active() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host2@example.com";
    let room_id = "pp-invariant-active-empty";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // Everyone leaves the roster (present count -> 0) but the column lags 'active'.
    sqlx::query(
        "UPDATE meeting_participants SET status = 'left', left_at = NOW() \
         WHERE meeting_id = $1",
    )
    .bind(pk)
    .execute(&pool)
    .await
    .expect("mark all left must succeed");
    force_state(&pool, pk, "active").await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        0,
        "precondition: nobody present"
    );

    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(
        row.state, "idle",
        "a meeting with zero present participants must be reported 'idle', even \
         when the raw column lagged at 'active'"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// `ended` is terminal: even if a present-participant row survives the instant
/// the meeting ends (an in-flight roster write racing `end_meeting`), the feed
/// MUST still report `ended`, never flip back to `active`.
#[tokio::test]
#[serial]
async fn feed_reports_ended_even_with_present_row() {
    let pool = get_test_pool().await;
    let host = "pp-inv-host3@example.com";
    let room_id = "pp-invariant-ended-terminal";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        1,
        "precondition: a present participant remains"
    );

    // End the meeting while a present row still exists (the race).
    db_meetings::end_meeting(&pool, pk)
        .await
        .expect("end_meeting must succeed");

    let body = list_feed(&pool, host).await;
    let row = body
        .result
        .meetings
        .iter()
        .find(|m| m.meeting_id == room_id)
        .expect("meeting must appear in the owner's feed");
    assert_eq!(
        row.state, "ended",
        "ended is terminal and must win over a present-participant row"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ════════════════════════════════════════════════════════════════════════════
// mark_present_by_connect — DB-level restore + privilege-escalation guards.
// ════════════════════════════════════════════════════════════════════════════

/// A previously-admitted participant marked `left` by a disconnect is restored
/// to `status='admitted', left_at=NULL` on (transport) reconnect — and is then
/// counted as present again. FAILS on the un-fixed code (no such function).
#[tokio::test]
#[serial]
async fn mark_present_restores_disconnected_admitted_participant() {
    let pool = get_test_pool().await;
    let host = "pp-mp-host@example.com";
    let ghost = "pp-mp-ghost@example.com";
    let room_id = "pp-mark-present-restore";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // Disconnect the ghost (no REST /leave) → marked left.
    db_participants::mark_left_by_disconnect(&pool, pk, ghost)
        .await
        .unwrap();
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        1,
        "ghost is no longer present after disconnect"
    );

    // Transport reconnect → mark present must restore them.
    let rows = db_participants::mark_present_by_connect(&pool, pk, ghost)
        .await
        .expect("mark_present_by_connect must not error");
    assert_eq!(rows, 1, "exactly the ghost's left row is restored");

    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        2,
        "the reconnected participant is counted as present again"
    );
    let status = db_participants::get_status(&pool, pk, ghost)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(status.status, "admitted", "status restored to admitted");
    assert!(status.left_at.is_none(), "left_at cleared to NULL");

    cleanup_test_data(&pool, room_id).await;
}

/// Privilege-escalation guard: `mark_present_by_connect` MUST NOT promote a
/// `waiting`-room participant to `admitted` (that would bypass the waiting room).
/// A spurious/forged present event for a waiter is a zero-row no-op.
#[tokio::test]
#[serial]
async fn mark_present_does_not_promote_waiting_participant() {
    let pool = get_test_pool().await;
    let host = "pp-mp-wr-host@example.com";
    let waiter = "pp-mp-waiter@example.com";
    let room_id = "pp-mark-present-waiting-guard";

    // Waiting room ON so a non-host joiner stays 'waiting'.
    cleanup_test_data(&pool, room_id).await;
    let app = build_app(pool.clone());
    let req = request_with_cookie("POST", "/api/v1/meetings", host)
        .header("Content-Type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({
                "meeting_id": room_id,
                "attendees": [],
                "waiting_room_enabled": true,
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::CREATED
    );
    join(&pool, room_id, host).await; // host activates
    join(&pool, room_id, waiter).await; // attendee queues (waiting)
    let pk = lookup_meeting_pk(&pool, room_id).await;

    let before = db_participants::get_status(&pool, pk, waiter)
        .await
        .unwrap()
        .expect("waiter row exists");
    assert_eq!(before.status, "waiting", "precondition: waiter is waiting");

    // A present event for the waiter MUST NOT admit them.
    let rows = db_participants::mark_present_by_connect(&pool, pk, waiter)
        .await
        .expect("must not error");
    assert_eq!(rows, 0, "a waiting participant must NOT be promoted");
    let after = db_participants::get_status(&pool, pk, waiter)
        .await
        .unwrap()
        .expect("waiter row exists");
    assert_eq!(
        after.status, "waiting",
        "the waiter must remain in the waiting room — the present event must \
         never bypass admission control"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Privilege-escalation guard: a `kicked` participant MUST NOT be un-kicked by a
/// present event. (`kicked` rows have `admitted_at` set but must stay removed.)
#[tokio::test]
#[serial]
async fn mark_present_does_not_unkick_participant() {
    let pool = get_test_pool().await;
    let host = "pp-mp-kick-host@example.com";
    let kicked = "pp-mp-kicked@example.com";
    let room_id = "pp-mark-present-kick-guard";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, kicked).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // Force the participant into 'kicked' (admitted_at remains set).
    sqlx::query(
        "UPDATE meeting_participants SET status = 'kicked', left_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(pk)
    .bind(kicked)
    .execute(&pool)
    .await
    .expect("force kicked must succeed");

    let rows = db_participants::mark_present_by_connect(&pool, pk, kicked)
        .await
        .expect("must not error");
    assert_eq!(rows, 0, "a kicked participant must NOT be restored");
    let after = db_participants::get_status(&pool, pk, kicked)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(
        after.status, "kicked",
        "the kicked participant must stay removed — present event must not un-kick"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ════════════════════════════════════════════════════════════════════════════
// apply_participant_present — the consumer's transition + nudge logic, exercised
// directly against the real production function.
// ════════════════════════════════════════════════════════════════════════════

/// A present event for a disconnected participant in an `idle` meeting must
/// (a) restore the participant, (b) re-activate the meeting `idle -> active`,
/// and (c) emit a `Joined` nudge. This is the asymmetry fix end-to-end at the
/// transition layer. FAILS on the un-fixed code (function does not exist).
#[tokio::test]
#[serial]
async fn apply_present_reactivates_idle_and_nudges() {
    let pool = get_test_pool().await;
    let host = "pp-ap-host@example.com";
    let ghost = "pp-ap-ghost@example.com";
    let room_id = "pp-apply-present-reactivate";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // Drive the meeting into the exact stuck state: ghost disconnected and the
    // meeting column lagged to 'idle' (room-empty event) even though the ghost
    // is about to reconnect over the transport.
    db_participants::mark_left_by_disconnect(&pool, pk, ghost)
        .await
        .unwrap();
    force_state(&pool, pk, "idle").await;

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(meeting.state.as_deref(), Some("idle"), "precondition: idle");

    let (feed_tx, mut feed_rx) = new_feed_channel();
    apply_participant_present(&pool, &feed_tx, "test-subject", &meeting, ghost).await;

    // (a)+(b): the meeting re-activated and the ghost is present again.
    let after = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.state.as_deref(),
        Some("active"),
        "the meeting must re-activate idle -> active on the present event"
    );
    let status = db_participants::get_status(&pool, pk, ghost)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(status.status, "admitted", "ghost restored to admitted");

    // (c): a Joined nudge was emitted.
    let change = feed_rx
        .try_recv()
        .expect("a feed nudge must be emitted on re-activation");
    assert_eq!(change.reason, FeedChangeReason::Joined, "nudge is Joined");
    assert_eq!(change.meeting_id, room_id, "nudge carries the room_id");

    cleanup_test_data(&pool, room_id).await;
}

/// `ended` is terminal: a present event MUST NOT resurrect an ended meeting. The
/// re-activation is an atomic `UPDATE … WHERE state = 'idle'`
/// (`reactivate_from_idle`), so an `ended` row matches zero rows. FAILS if the
/// consumer ever re-activated an `ended` meeting (e.g. by calling the
/// unconditional `activate()`).
#[tokio::test]
#[serial]
async fn apply_present_never_resurrects_ended_meeting() {
    let pool = get_test_pool().await;
    let host = "pp-ap-ended-host@example.com";
    let ghost = "pp-ap-ended-ghost@example.com";
    let room_id = "pp-apply-present-ended";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // The meeting ended (host left with end_on_host_leave), ghost row marked left.
    db_participants::mark_left_by_disconnect(&pool, pk, ghost)
        .await
        .unwrap();
    db_meetings::end_meeting(&pool, pk)
        .await
        .expect("end_meeting must succeed");
    let ended = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.state.as_deref(), Some("ended"), "precondition: ended");

    let (feed_tx, _feed_rx) = new_feed_channel();
    // A stray present event races in (e.g. a late reconnect after the end).
    apply_participant_present(&pool, &feed_tx, "test-subject", &ended, ghost).await;

    let after = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.state.as_deref(),
        Some("ended"),
        "ended is terminal — a present event must NEVER resurrect it to active"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// An already-present participant in an already-active meeting produces NO
/// state change and NO nudge (idempotency / nudge-cardinality). A duplicate or
/// redelivered present event must be a clean no-op.
#[tokio::test]
#[serial]
async fn apply_present_is_noop_when_already_present_and_active() {
    let pool = get_test_pool().await;
    let host = "pp-ap-noop-host@example.com";
    let room_id = "pp-apply-present-noop";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        meeting.state.as_deref(),
        Some("active"),
        "precondition: active with the host present"
    );

    let (feed_tx, mut feed_rx) = new_feed_channel();
    apply_participant_present(&pool, &feed_tx, "test-subject", &meeting, host).await;

    assert!(
        feed_rx.try_recv().is_err(),
        "no nudge must be emitted when nothing changed (already present + active)"
    );
    let after = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.state.as_deref(), Some("active"), "state unchanged");

    cleanup_test_data(&pool, room_id).await;
}

// ════════════════════════════════════════════════════════════════════════════
// reactivate_from_idle — atomic, terminal-`ended`-safe re-activation.
// ════════════════════════════════════════════════════════════════════════════

/// `reactivate_from_idle` flips an `idle` meeting to `active` (one row), and is
/// a zero-row no-op on `active` (idempotent) and — critically — on `ended`
/// (terminal). The `WHERE state = 'idle'` guard is what makes the end-vs-present
/// race impossible without trusting a possibly-stale snapshot. FAILS on the
/// un-fixed code (no such function) and would fail if the guard were dropped.
#[tokio::test]
#[serial]
async fn reactivate_from_idle_is_atomic_and_ended_safe() {
    let pool = get_test_pool().await;
    let host = "pp-react-host@example.com";
    let room_id = "pp-reactivate-from-idle";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // idle -> active flips exactly one row.
    force_state(&pool, pk, "idle").await;
    let rows = db_meetings::reactivate_from_idle(&pool, pk).await.unwrap();
    assert_eq!(rows, 1, "idle -> active must flip exactly one row");
    let m = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(m.state.as_deref(), Some("active"), "now active");

    // active -> (no-op): already active, zero rows.
    let rows = db_meetings::reactivate_from_idle(&pool, pk).await.unwrap();
    assert_eq!(rows, 0, "already-active is a zero-row no-op (idempotent)");

    // ended -> (no-op): terminal, must NEVER resurrect.
    db_meetings::end_meeting(&pool, pk).await.unwrap();
    let rows = db_meetings::reactivate_from_idle(&pool, pk).await.unwrap();
    assert_eq!(
        rows, 0,
        "ended is terminal — reactivate_from_idle must no-op"
    );
    let m = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        m.state.as_deref(),
        Some("ended"),
        "ended must be preserved — never resurrected to active"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ════════════════════════════════════════════════════════════════════════════
// End-to-end NATS consumer test (gated on NATS_URL).
// ════════════════════════════════════════════════════════════════════════════

/// Connect to NATS for tests, returning `None` if `NATS_URL` is unset.
async fn maybe_connect_nats() -> Option<async_nats::Client> {
    let url = std::env::var("NATS_URL").ok()?;
    Some(
        async_nats::connect(&url)
            .await
            .expect("Failed to connect to NATS"),
    )
}

/// Full path: actix-api publishes `internal.participant_present`, the consumer
/// restores the participant and re-activates the meeting (`idle -> active`).
#[tokio::test]
#[serial]
async fn participant_present_event_reactivates_meeting() {
    use meeting_api::nats_consumers::spawn_participant_present_consumer_inner;
    use meeting_api::nats_events::{ParticipantPresentPayload, PARTICIPANT_PRESENT_SUBJECT};

    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping participant_present integration test");
        return;
    };
    let pool = get_test_pool().await;
    let host = "pp-e2e-host@example.com";
    let ghost = "pp-e2e-ghost@example.com";
    let room_id = "pp-present-e2e-reactivate";

    create_meeting_wr_off(&pool, host, room_id).await;
    join(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    // Stuck state: ghost disconnected + column lagged to idle.
    db_participants::mark_left_by_disconnect(&pool, pk, ghost)
        .await
        .unwrap();
    force_state(&pool, pk, "idle").await;

    // Spawn the consumer and wait for it to subscribe.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (feed_tx, _feed_rx) = new_feed_channel();
    let _handle = spawn_participant_present_consumer_inner(
        Some(nats.clone()),
        pool.clone(),
        feed_tx,
        Some(ready_tx),
    )
    .expect("consumer must spawn when NATS is available");
    ready_rx.await.expect("consumer must signal readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The relay reports the ghost reconnected.
    let payload = ParticipantPresentPayload {
        room_id: room_id.to_string(),
        user_id: ghost.to_string(),
    };
    nats.publish(
        PARTICIPANT_PRESENT_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("publish must succeed");

    // Poll until the meeting re-activates (up to 5s).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let m = db_meetings::get_by_room_id(&pool, room_id)
            .await
            .unwrap()
            .unwrap();
        if m.state.as_deref() == Some("active") || std::time::Instant::now() >= deadline {
            assert_eq!(
                m.state.as_deref(),
                Some("active"),
                "the present event must re-activate the meeting idle -> active"
            );
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        2,
        "the reconnected ghost is counted present again"
    );

    cleanup_test_data(&pool, room_id).await;
}
