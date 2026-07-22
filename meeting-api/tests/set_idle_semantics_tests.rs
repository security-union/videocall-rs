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

//! Integration tests for [`db_meetings::set_idle`] — the presence-driven
//! everyone-left → idle transition.
//!
//! These pin the contract of the `UPDATE … SET state='idle' WHERE id=$1 AND
//! state='active'` guard added in `meeting-api/src/db/meetings.rs`:
//!
//! 1. `active` → `idle` flips the state and leaves `started_at` / `ended_at`
//!    untouched.
//! 2. `ended` is terminal: `set_idle` is a no-op on an ended meeting (the
//!    end-vs-idle race must let END win).
//! 3. `idle` → `idle` is idempotent (a duplicate empty event no-ops).
//! 4. Lifecycle: idle → active → idle → active round-trips correctly, and the
//!    re-activation refreshes `started_at` (proving `set_idle` did not corrupt
//!    the timestamps `activate` depends on).
//!
//! Tests run against a live Postgres pool via `DATABASE_URL`. They exercise the
//! `db_meetings` API directly, free of HTTP / auth / route layering. In CI these
//! run against the provisioned Postgres; locally they compile but require a DB
//! to execute.

mod test_helpers;

use chrono::{DateTime, Utc};
use meeting_api::db::meetings as db_meetings;
use serde_json::json;
use serial_test::serial;
use sqlx::PgPool;
use test_helpers::*;

/// Insert an idle meeting owned by `creator_id` into the test DB.
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
        // waiting_room_enabled / admitted_can_admit / end_on_host_leave / allow_guests / recording_allowed_for_all
        true,
        false,
        true,
        false,
        false,
    )
    .await
    .expect("create_with_options must succeed")
}

/// Re-fetch a meeting by `room_id`.
async fn refetch(pool: &PgPool, room_id: &str) -> db_meetings::MeetingRow {
    db_meetings::get_by_room_id(pool, room_id)
        .await
        .expect("get_by_room_id must not error")
        .expect("meeting row must still exist")
}

/// Exact-equality timestamp comparison (microsecond resolution survives the
/// Postgres `timestamptz` ↔ `chrono::DateTime<Utc>` round-trip), so equal here
/// means "no UPDATE touched the timestamp".
fn ts_eq(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a == b
}

// ── Scenario 1: active → idle flips state, preserves timestamps ─────────────

/// `set_idle` on an `active` meeting must:
/// * Move state to `idle`.
/// * Leave `started_at` UNCHANGED (an idle meeting that was active retains its
///   original start time — only `activate` refreshes it on re-activation).
/// * Leave `ended_at` as NULL (it was never ended).
#[tokio::test]
#[serial]
async fn test_set_idle_active_flips_state_and_preserves_timestamps() {
    let pool = get_test_pool().await;
    let room_id = "set-idle-active-to-idle";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");

    let active = refetch(&pool, room_id).await;
    assert_eq!(
        active.state.as_deref(),
        Some("active"),
        "row must be active before set_idle"
    );
    let started_at_active = active.started_at;
    assert!(
        active.ended_at.is_none(),
        "active row must have ended_at NULL"
    );

    // Sleep so that if set_idle erroneously refreshed started_at = NOW() we'd
    // see a forward jump.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    db_meetings::set_idle(&pool, row.id)
        .await
        .expect("set_idle must not error");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("idle"),
        "state must transition active -> idle"
    );
    assert!(
        ts_eq(after.started_at, started_at_active),
        "set_idle must NOT touch started_at; before={started_at_active}, after={}",
        after.started_at
    );
    assert!(
        after.ended_at.is_none(),
        "set_idle must NOT set ended_at; got {:?}",
        after.ended_at
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 2: ended is terminal — set_idle is a no-op ─────────────────────

/// `set_idle` MUST NOT resurrect an ended meeting. If the host left with
/// `end_on_host_leave=true` and `end_meeting` landed first, the row is
/// `ended` and a racing empty event must no-op. This is the end-vs-idle race
/// guarantee in the "END landed first" ordering.
#[tokio::test]
#[serial]
async fn test_set_idle_is_noop_on_ended_meeting() {
    let pool = get_test_pool().await;
    let room_id = "set-idle-noop-on-ended";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");
    db_meetings::end_meeting(&pool, row.id)
        .await
        .expect("end_meeting must succeed");

    let ended = refetch(&pool, room_id).await;
    assert_eq!(
        ended.state.as_deref(),
        Some("ended"),
        "row must be ended before set_idle"
    );
    let ended_at_before = ended.ended_at.expect("end_meeting must stamp ended_at");

    db_meetings::set_idle(&pool, row.id)
        .await
        .expect("set_idle on ended must not error (it is a no-op)");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("ended"),
        "ended is terminal — set_idle must leave it ended"
    );
    assert!(
        ts_eq(
            after.ended_at.expect("ended_at must remain populated"),
            ended_at_before
        ),
        "set_idle must not disturb ended_at on an ended meeting"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 3: idle → idle is idempotent ───────────────────────────────────

/// A duplicate empty event (NATS re-subscribe, or two replicas observing the
/// same drain) calls `set_idle` on an already-idle meeting. The `state='active'`
/// guard makes this a zero-row no-op.
#[tokio::test]
#[serial]
async fn test_set_idle_is_idempotent_on_already_idle() {
    let pool = get_test_pool().await;
    let room_id = "set-idle-idempotent-already-idle";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    db_meetings::activate(&pool, row.id)
        .await
        .expect("activate must succeed");
    db_meetings::set_idle(&pool, row.id)
        .await
        .expect("first set_idle must succeed");

    let first = refetch(&pool, room_id).await;
    assert_eq!(first.state.as_deref(), Some("idle"), "row must be idle");
    let started_at_first = first.started_at;

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    // Duplicate empty event.
    db_meetings::set_idle(&pool, row.id)
        .await
        .expect("idempotent set_idle must not error");

    let after = refetch(&pool, room_id).await;
    assert_eq!(
        after.state.as_deref(),
        Some("idle"),
        "state must remain idle after a duplicate set_idle"
    );
    assert!(
        ts_eq(after.started_at, started_at_first),
        "duplicate set_idle must not touch started_at"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Scenario 4: full idle → active → idle → active round-trip ────────────────

/// The complete presence lifecycle: a meeting goes idle when empty, active when
/// someone (re)joins, idle again when they leave, and active again on the next
/// join. Critically, the second `activate` must refresh `started_at` forward —
/// proving `set_idle` left the row in a clean `idle` state that `activate`'s
/// `state IN ('idle','ended')` branch correctly re-activates.
#[tokio::test]
#[serial]
async fn test_idle_active_idle_active_round_trip() {
    let pool = get_test_pool().await;
    let room_id = "set-idle-round-trip";
    let row = create_idle_meeting(&pool, room_id, "host@example.com").await;

    // First join: idle -> active.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db_meetings::activate(&pool, row.id).await.unwrap();
    let after_first_join = refetch(&pool, room_id).await;
    assert_eq!(after_first_join.state.as_deref(), Some("active"));
    let started_first = after_first_join.started_at;

    // Everyone leaves: active -> idle.
    db_meetings::set_idle(&pool, row.id).await.unwrap();
    let after_leave = refetch(&pool, room_id).await;
    assert_eq!(after_leave.state.as_deref(), Some("idle"));
    assert!(
        ts_eq(after_leave.started_at, started_first),
        "started_at must be preserved across active->idle"
    );

    // Rejoin: idle -> active, started_at refreshes forward.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    db_meetings::activate(&pool, row.id).await.unwrap();
    let after_rejoin = refetch(&pool, room_id).await;
    assert_eq!(after_rejoin.state.as_deref(), Some("active"));
    assert!(
        after_rejoin.started_at > started_first,
        "started_at must refresh forward on idle->active rejoin; \
         first={started_first}, rejoin={}",
        after_rejoin.started_at
    );
    assert!(
        after_rejoin.ended_at.is_none(),
        "ended_at must remain NULL across the idle/active round-trip"
    );

    cleanup_test_data(&pool, room_id).await;
}
