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

//! Integration tests for the timestamp semantics of
//! [`db_meetings::activate`].
//!
//! These tests pin down the contract added in `meeting-api/src/db/meetings.rs`:
//!
//! 1. Activating an `idle` meeting refreshes `started_at` to NOW() and leaves
//!    `ended_at` as NULL.
//! 2. Activating an `ended` meeting refreshes `started_at` AND clears `ended_at`.
//! 3. Activating an already-`active` meeting is idempotent — neither
//!    `started_at` nor `ended_at` are touched.
//!
//! Tests run against a live Postgres pool exposed via `DATABASE_URL`. They
//! exercise the `db_meetings` API directly so the assertions are free of
//! any HTTP / auth / route layering.

mod test_helpers;

use chrono::{DateTime, Utc};
use meeting_api::db::meetings as db_meetings;
use serde_json::json;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;

/// Insert an idle meeting owned by `creator_id` into the test DB.
///
/// Mirrors the production `db_meetings::create_with_options` defaults used
/// by the `POST /api/v1/meetings` route. Returns the `MeetingRow` so the
/// caller can use the row's primary key to drive subsequent state changes.
async fn create_idle_meeting(
    pool: &PgPool,
    room_id: &str,
    creator_id: &str,
) -> db_meetings::MeetingRow {
    cleanup_test_data(pool, room_id).await;
    db_meetings::create_with_options(
        pool,
        room_id,
        creator_id,
        None,
        &json!([]),
        // waiting_room_enabled / admitted_can_admit / end_on_host_leave / allow_guests
        true,
        false,
        true,
        false,
    )
    .await
    .expect("create_with_options must succeed")
}

/// Re-fetch a meeting by `room_id`. Panics if the row has been deleted; we
/// always create + clean inside each test so this is a hard programming error
/// in the test, not a recoverable case.
async fn refetch(pool: &PgPool, room_id: &str) -> db_meetings::MeetingRow {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id must not error")
        .expect("meeting row must still exist")
}

/// Compare two timestamps for equality at microsecond resolution.
///
/// Postgres `timestamptz` keeps microsecond precision. Round-tripping through
/// `chrono::DateTime<Utc>` preserves that, so an exact `==` comparison is
/// safe and semantically meaningful: equal here means "no UPDATE touched
/// the row's timestamp at all".
fn ts_eq(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a == b
}

// ── Scenario 1: idle → active refreshes started_at ──────────────────────────

/// Activating an `idle` meeting must:
/// * Move state to `active`.
/// * Refresh `started_at` to NOW() — strictly later than the pre-activation
///   value (which was set during INSERT).
/// * Leave `ended_at` as NULL (it was NULL on INSERT).
#[tokio::test]
#[serial]
async fn test_activate_idle_refreshes_started_at_and_leaves_ended_at_null() {
    let pool = get_test_pool().await;
    let room_id = "activate-idle-refresh-started-at";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    // Capture the timestamps as they look on the freshly-inserted row.
    let started_at_before = row.started_at;
    assert!(
        row.ended_at.is_none(),
        "newly-created idle meeting must have ended_at = NULL"
    );
    assert_eq!(row.state.as_deref(), Some("idle"), "fresh row must be idle");

    // Sleep a few millis so NOW() has time to advance past the INSERT timestamp
    // even on hardware where successive NOW() calls would otherwise tie at the
    // microsecond level.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("activate must not error");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("active"),
        "state must transition to 'active'"
    );
    assert!(
        after.started_at > started_at_before,
        "started_at must refresh forward on idle->active; before={started_at_before}, after={}",
        after.started_at
    );
    assert!(
        after.ended_at.is_none(),
        "ended_at must remain NULL when activating from idle"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 2: ended → active refreshes started_at AND clears ended_at ─────

/// A meeting that was previously ended (state = 'ended', ended_at set) must,
/// on re-activation:
/// * Have `state` flipped back to `active`.
/// * Have `started_at` refreshed forward (to the new activation time).
/// * Have `ended_at` cleared back to NULL.
///
/// This guards against a regression where a re-activated meeting still
/// reports an `ended_at` (which would surface as a stale "ended" indicator
/// in any downstream UI that uses `ended_at` as a sentinel).
#[tokio::test]
#[serial]
async fn test_activate_ended_refreshes_started_at_and_clears_ended_at() {
    let pool = get_test_pool().await;
    let room_id = "activate-ended-refresh-and-clear";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    // Drive the row through idle -> active -> ended.
    db_meetings::activate(&pool, row.id)
        .await
        .expect("first activate must succeed");
    db_meetings::end_meeting(&pool, row.id)
        .await
        .expect("end_meeting must succeed");

    let ended = refetch(&pool, room_id).await;
    assert_eq!(
        ended.state.as_deref(),
        Some("ended"),
        "row must be in 'ended' state before the test re-activates it"
    );
    assert!(
        ended.ended_at.is_some(),
        "end_meeting must populate ended_at"
    );
    let started_before_reactivation = ended.started_at;

    // Sleep so NOW() advances past `started_before_reactivation` (which was
    // set when the row first activated milliseconds ago).
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("re-activate must not error");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("active"),
        "state must transition back to 'active' on re-activation"
    );
    assert!(
        after.started_at > started_before_reactivation,
        "started_at must refresh forward on ended->active; \
         before={started_before_reactivation}, after={}",
        after.started_at
    );
    assert!(
        after.ended_at.is_none(),
        "ended_at must be cleared back to NULL on ended->active; got {:?}",
        after.ended_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 3: active → active is idempotent ──────────────────────────────

/// Calling `activate()` on a meeting whose state is already `active` must NOT
/// touch `started_at` or `ended_at`. The CASE expression in the UPDATE is the
/// single guard against accidentally bumping the start time on every re-join
/// of an active meeting (which would silently rewrite session metrics).
///
/// We capture both timestamps before the redundant call and assert exact
/// equality after — any mutation would mean the CASE branch is wrong.
#[tokio::test]
#[serial]
async fn test_activate_active_is_idempotent_does_not_touch_timestamps() {
    let pool = get_test_pool().await;
    let room_id = "activate-active-idempotent";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    // First, transition idle -> active so we have a stable baseline.
    db_meetings::activate(&pool, row.id)
        .await
        .expect("first activate must succeed");

    let baseline = refetch(&pool, room_id).await;
    assert_eq!(
        baseline.state.as_deref(),
        Some("active"),
        "row must be active before the redundant activate"
    );
    let started_at_baseline = baseline.started_at;
    let ended_at_baseline = baseline.ended_at;
    assert!(
        ended_at_baseline.is_none(),
        "active row must have ended_at = NULL"
    );

    // Sleep so that *if* the UPDATE were to refresh `started_at = NOW()` we'd
    // see a clear forward jump — without this the same-microsecond NOW()
    // could mask a regression on fast hardware.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Idempotent activate.
    db_meetings::activate(&pool, row.id)
        .await
        .expect("idempotent activate must not error");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("active"),
        "state must remain 'active'"
    );
    assert!(
        ts_eq(after.started_at, started_at_baseline),
        "started_at must NOT change on active->active; \
         baseline={started_at_baseline}, after={}",
        after.started_at
    );
    assert_eq!(
        after.ended_at, ended_at_baseline,
        "ended_at must NOT change on active->active; \
         baseline={ended_at_baseline:?}, after={:?}",
        after.ended_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 4: ended → active monotonicity guard ──────────────────────────

/// `created_at` is immutable; `started_at` is monotonically refreshed on every
/// idle/ended -> active transition. Therefore for any meeting that has gone
/// through at least one re-activation, `started_at >= created_at` must hold
/// AND a re-activation's `started_at` must be strictly later than the
/// original creation time.
///
/// This is a cross-field invariant the home page / list endpoints depend on
/// ("Started X minutes ago" must always be a non-negative duration relative
/// to "Created").
#[tokio::test]
#[serial]
async fn test_started_at_monotonically_advances_past_created_at() {
    let pool = get_test_pool().await;
    let room_id = "activate-started-monotonic-vs-created";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;
    let created_at = row.created_at;

    // Spread the activations across wall-clock time so the deltas are visible.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db_meetings::activate(&pool, row.id).await.unwrap();
    let after_first = refetch(&pool, room_id).await;
    assert!(
        after_first.started_at >= created_at,
        "started_at must be >= created_at on first activation"
    );

    db_meetings::end_meeting(&pool, row.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    db_meetings::activate(&pool, row.id).await.unwrap();
    let after_second = refetch(&pool, room_id).await;
    assert!(
        after_second.started_at > after_first.started_at,
        "started_at must advance forward on the second activation; \
         after_first={}, after_second={}",
        after_first.started_at,
        after_second.started_at
    );
    assert_eq!(
        after_second.created_at, created_at,
        "created_at must be immutable across re-activations"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 5: end_meeting is idempotent — ended_at is stamped once ───────

/// Calling [`db_meetings::end_meeting`] more than once for the same meeting
/// must NOT re-stamp `ended_at`. The NATS host-leave consumer can fire the
/// `MEETING_ENDED_BY_HOST` event multiple times in pathological conditions
/// (stream re-subscribe after disconnect, multi-replica fan-out without a
/// queue group), and downstream consumers (search indexer, meeting-history)
/// rely on `ended_at` being the first-end timestamp.
///
/// The SQL guard is `state <> 'ended'` (the WHERE clause short-circuits the
/// second UPDATE entirely) reinforced by `COALESCE(ended_at, NOW())` (so
/// even if the WHERE were ever loosened, the original timestamp would still
/// be preserved).
#[tokio::test]
#[serial]
async fn test_end_meeting_is_idempotent_does_not_restamp_ended_at() {
    let pool = get_test_pool().await;
    let room_id = "end-meeting-idempotent-ended-at";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");

    // First end_meeting: stamps ended_at.
    db_meetings::end_meeting(&pool, row.id)
        .await
        .expect("first end_meeting must succeed");

    let after_first = refetch(&pool, room_id).await;
    assert_eq!(
        after_first.state.as_deref(),
        Some("ended"),
        "row must transition to 'ended' on the first end_meeting"
    );
    let ended_at_first = after_first
        .ended_at
        .expect("first end_meeting must populate ended_at");

    // Sleep so that *if* a second end_meeting were to refresh `ended_at = NOW()`
    // we'd see a clear forward jump — without this the same-microsecond NOW()
    // could mask a regression on fast hardware.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Second end_meeting (simulates a duplicate NATS host-leave fan-out).
    db_meetings::end_meeting(&pool, row.id)
        .await
        .expect("idempotent end_meeting must not error");

    let after_second = refetch(&pool, room_id).await;
    assert_eq!(
        after_second.state.as_deref(),
        Some("ended"),
        "row must remain 'ended' after a duplicate end_meeting"
    );
    assert!(
        ts_eq(
            after_second
                .ended_at
                .expect("ended_at must remain populated after duplicate end"),
            ended_at_first
        ),
        "ended_at must NOT change on a duplicate end_meeting; \
         first={ended_at_first}, second={:?}",
        after_second.ended_at
    );

    cleanup_test_data(&pool, room_id).await;
}
