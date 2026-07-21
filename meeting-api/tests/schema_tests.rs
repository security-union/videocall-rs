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

//! Schema-level integration tests, run against both backends.
//!
//! `dbmate/sqlite/db/migrations` is a hand-port of `dbmate/db/migrations`; no
//! compiler compares the two. These tests are that comparison. Each one states
//! a property of the PostgreSQL schema and asserts the SQLite port has it too,
//! which is why they are *not* `cfg`-gated: running the identical assertions on
//! PostgreSQL is what makes them a port check rather than a description of
//! whatever SQLite happens to do.
//!
//! They go through the shared `db` layer wherever a function exists for the
//! operation, so the `q()` placeholder shim and the bound-timestamp convention
//! are exercised alongside the DDL.

mod test_helpers;

use chrono::Utc;
use meeting_api::db::{meetings as db_meetings, participants as db_participants, q, DbPool};
use serde_json::json;
use serial_test::serial;
use test_helpers::*;

/// Ends a meeting the way the delete path does, so `room_id` becomes reusable.
async fn soft_delete(pool: &DbPool, room_id: &str, creator: &str) {
    db_meetings::soft_delete(pool, room_id, creator)
        .await
        .expect("soft delete should succeed")
        .expect("soft delete should match a row");
}

// ── Foreign keys ────────────────────────────────────────────────────────

/// `ON DELETE CASCADE` is inert on SQLite unless `PRAGMA foreign_keys` is on,
/// and that pragma is per connection. This runs through `get_test_pool`, i.e.
/// `meeting_api::db::connect`, precisely because a hand-rolled connection with
/// its own options would pass while production silently leaked orphan rows.
#[tokio::test]
#[serial]
async fn test_deleting_a_meeting_cascades_to_its_participants() {
    let pool = get_test_pool().await;
    let room_id = "schema-cascade";
    cleanup_test_data(&pool, room_id).await;

    let meeting = db_meetings::create(&pool, room_id, "host@example.com", None, &json!([]))
        .await
        .expect("create meeting");
    db_participants::upsert_host(&pool, meeting.id, "host@example.com", None)
        .await
        .expect("insert host");
    db_participants::join_attendee(&pool, meeting.id, "guest@example.com", None)
        .await
        .expect("insert attendee");

    let before: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1",
    ))
    .bind(meeting.id)
    .fetch_one(&pool)
    .await
    .expect("count before");
    assert_eq!(before.0, 2, "both participants should have been inserted");

    sqlx::query(&q("DELETE FROM meetings WHERE id = $1"))
        .bind(meeting.id)
        .execute(&pool)
        .await
        .expect("hard delete meeting");

    let after: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1",
    ))
    .bind(meeting.id)
    .fetch_one(&pool)
    .await
    .expect("count after");
    assert_eq!(
        after.0, 0,
        "deleting a meeting must cascade to meeting_participants; \
         orphans here mean PRAGMA foreign_keys is off on this connection"
    );
}

/// The other half of the same pragma: a participant may not reference a
/// meeting that does not exist. Without `foreign_keys` SQLite accepts this.
#[tokio::test]
#[serial]
async fn test_participant_referencing_a_missing_meeting_is_rejected() {
    let pool = get_test_pool().await;

    let result = sqlx::query(&q(
        "INSERT INTO meeting_participants (meeting_id, user_id, status, joined_at, created_at, updated_at) \
         VALUES ($1, $2, 'waiting', $3, $3, $3)",
    ))
    .bind(-424242_i32)
    .bind("ghost@example.com")
    .bind(Utc::now())
    .execute(&pool)
    .await;

    assert!(
        result.is_err(),
        "insert against a non-existent meeting_id must violate the foreign key"
    );
}

// ── CHECK constraints ───────────────────────────────────────────────────

/// PostgreSQL has `CHECK (jsonb_array_length(attendees) <= 100)`; the port uses
/// `json_array_length`. Both must reject the 101st attendee.
#[tokio::test]
#[serial]
async fn test_attendees_over_one_hundred_are_rejected() {
    let pool = get_test_pool().await;
    let room_id = "schema-attendees-limit";
    cleanup_test_data(&pool, room_id).await;

    let hundred: Vec<String> = (0..100).map(|i| format!("a{i}@example.com")).collect();
    db_meetings::create(&pool, room_id, "host@example.com", None, &json!(hundred))
        .await
        .expect("100 attendees is at the limit and must be accepted");
    cleanup_test_data(&pool, room_id).await;

    let hundred_one: Vec<String> = (0..101).map(|i| format!("a{i}@example.com")).collect();
    let result = db_meetings::create(
        &pool,
        room_id,
        "host@example.com",
        None,
        &json!(hundred_one),
    )
    .await;

    assert!(
        result.is_err(),
        "101 attendees must be rejected by the attendees length CHECK"
    );
    assert!(
        db_meetings::get_by_room_id(&pool, room_id)
            .await
            .expect("lookup")
            .is_none(),
        "the rejected insert must not have left a row behind"
    );
}

/// `VARCHAR(255)` in the PostgreSQL schema became `TEXT` + `CHECK (length(..) <= 255)`
/// in the port, because SQLite ignores type-name length limits outright. Assert
/// the limit is enforced on the columns that take user-controlled strings.
#[tokio::test]
#[serial]
async fn test_columns_ported_from_varchar255_reject_longer_values() {
    let pool = get_test_pool().await;
    let room_id = "schema-length-limit";
    cleanup_test_data(&pool, room_id).await;

    let too_long = "x".repeat(256);
    let at_limit = "y".repeat(255);

    // room_id
    assert!(
        db_meetings::create(&pool, &too_long, "host@example.com", None, &json!([]))
            .await
            .is_err(),
        "a 256-character room_id must be rejected"
    );

    // creator_id
    assert!(
        db_meetings::create(&pool, room_id, &too_long, None, &json!([]))
            .await
            .is_err(),
        "a 256-character creator_id must be rejected"
    );

    let meeting = db_meetings::create(&pool, room_id, "host@example.com", None, &json!([]))
        .await
        .expect("create meeting with in-range values");

    // display_name on meeting_participants
    assert!(
        db_participants::upsert_host(&pool, meeting.id, "host@example.com", Some(&too_long))
            .await
            .is_err(),
        "a 256-character display_name must be rejected"
    );
    db_participants::upsert_host(&pool, meeting.id, "host@example.com", Some(&at_limit))
        .await
        .expect("255 characters is at the limit and must be accepted");

    // user_id on meeting_participants
    assert!(
        db_participants::join_attendee(&pool, meeting.id, &too_long, None)
            .await
            .is_err(),
        "a 256-character user_id must be rejected"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Partial unique index ────────────────────────────────────────────────

/// `idx_meetings_room_id_unique_active` is `UNIQUE (room_id) WHERE deleted_at IS NULL`.
/// Both halves matter: reuse after deletion is a product requirement, and
/// blocking reuse while active is what stops two live meetings sharing a room.
#[tokio::test]
#[serial]
async fn test_room_id_is_unique_only_among_live_meetings() {
    let pool = get_test_pool().await;
    let room_id = "schema-partial-unique";
    cleanup_test_data(&pool, room_id).await;

    let first = db_meetings::create(&pool, room_id, "host@example.com", None, &json!([]))
        .await
        .expect("first meeting");

    let duplicate = db_meetings::create(&pool, room_id, "host@example.com", None, &json!([])).await;
    assert!(
        duplicate.is_err(),
        "a second live meeting must not be able to take a room_id that is in use"
    );

    soft_delete(&pool, room_id, "host@example.com").await;

    let reused = db_meetings::create(&pool, room_id, "someone-else@example.com", None, &json!([]))
        .await
        .expect("room_id must be reusable once the previous meeting is soft-deleted");
    assert_ne!(
        reused.id, first.id,
        "reuse must create a new meeting, not resurrect the old one"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── updated_at ──────────────────────────────────────────────────────────

/// The SQLite port has no `AFTER UPDATE` trigger for `updated_at`, on purpose:
/// SQLite evaluates `RETURNING` before AFTER-triggers fire, so a trigger-driven
/// `updated_at` comes back stale — the caller sees the value from *before* its
/// own write. Every UPDATE therefore sets `updated_at` explicitly. This test is
/// what stops the triggers being reintroduced.
#[tokio::test]
#[serial]
async fn test_update_returning_yields_the_new_updated_at() {
    let pool = get_test_pool().await;
    let room_id = "schema-updated-at";
    cleanup_test_data(&pool, room_id).await;

    let created = db_meetings::create(&pool, room_id, "host@example.com", None, &json!([]))
        .await
        .expect("create meeting");

    // Guarantee a strictly greater timestamp regardless of clock resolution.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let toggled =
        db_meetings::update_waiting_room_enabled(&pool, room_id, "host@example.com", false)
            .await
            .expect("toggle waiting room")
            .expect("toggle should match the meeting");

    assert!(
        toggled.updated_at > created.updated_at,
        "UPDATE ... RETURNING updated_at returned {:?}, which is not newer than the \
         pre-update value {:?} — a stale value here means updated_at is being written \
         by an AFTER trigger instead of by the statement itself",
        toggled.updated_at,
        created.updated_at
    );

    // And the returned value must match what was actually persisted.
    let persisted = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .expect("re-read meeting")
        .expect("meeting should exist");
    assert_eq!(
        persisted.updated_at, toggled.updated_at,
        "the value RETURNING produced must be the value stored in the row"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ── Timestamp ordering ──────────────────────────────────────────────────

/// SQLite stores these timestamps as TEXT, so `ORDER BY created_at DESC` is a
/// lexicographic sort of whatever format was written. `datetime('now')` renders
/// `2026-07-21 04:39:04` while a bound `DateTime<Utc>` renders RFC 3339 with a
/// `T`; mixing the two in one column silently misorders rows (`' ' < 'T'`).
/// Insert through the application, which binds RFC 3339 everywhere, and assert
/// the ordering the list endpoint depends on.
#[tokio::test]
#[serial]
async fn test_list_by_owner_orders_by_created_at_descending() {
    let pool = get_test_pool().await;
    let creator = "ordering-host@example.com";
    let room_ids = ["schema-order-1", "schema-order-2", "schema-order-3"];
    for room_id in room_ids {
        cleanup_test_data(&pool, room_id).await;
    }

    for room_id in room_ids {
        db_meetings::create(&pool, room_id, creator, None, &json!([]))
            .await
            .expect("create meeting");
        // A gap far wider than either backend's timestamp resolution, so the
        // expected order below is never a coin flip.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let listed = db_meetings::list_by_owner(&pool, creator, 10, 0)
        .await
        .expect("list meetings");

    let listed_rooms: Vec<&str> = listed.iter().map(|m| m.room_id.as_str()).collect();
    assert_eq!(
        listed_rooms,
        vec!["schema-order-3", "schema-order-2", "schema-order-1"],
        "newest meeting must sort first"
    );

    // Assert the underlying column is monotonically decreasing too, so a future
    // change that returns the right rows in the wrong order still fails here.
    for pair in listed.windows(2) {
        assert!(
            pair[0].created_at >= pair[1].created_at,
            "created_at must be non-increasing across the result set, got {:?} then {:?}",
            pair[0].created_at,
            pair[1].created_at
        );
    }

    for room_id in room_ids {
        cleanup_test_data(&pool, room_id).await;
    }
}

/// The format guard behind the test above: a timestamp written by the
/// application must round-trip through the column and still compare correctly
/// against a later one *as text*, which is how SQLite will compare them.
#[tokio::test]
#[serial]
async fn test_stored_timestamps_sort_the_same_as_they_compare() {
    let pool = get_test_pool().await;
    let room_id = "schema-timestamp-format";
    cleanup_test_data(&pool, room_id).await;

    db_meetings::create(&pool, room_id, "host@example.com", None, &json!([]))
        .await
        .expect("create meeting");

    // A cutoff safely in the past must select the row; one in the future must not.
    let past = Utc::now() - chrono::Duration::hours(1);
    let future = Utc::now() + chrono::Duration::hours(1);

    let after_past: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meetings WHERE room_id = $1 AND created_at > $2",
    ))
    .bind(room_id)
    .bind(past)
    .fetch_one(&pool)
    .await
    .expect("compare against past cutoff");
    assert_eq!(
        after_past.0, 1,
        "a row created now must compare as later than an hour ago"
    );

    let after_future: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meetings WHERE room_id = $1 AND created_at > $2",
    ))
    .bind(room_id)
    .bind(future)
    .fetch_one(&pool)
    .await
    .expect("compare against future cutoff");
    assert_eq!(
        after_future.0, 0,
        "a row created now must not compare as later than an hour from now"
    );

    cleanup_test_data(&pool, room_id).await;
}
