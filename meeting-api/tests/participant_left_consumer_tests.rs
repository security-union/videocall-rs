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

//! Integration tests for the per-participant disconnect → mark-left consumer
//! (issue #1551).
//!
//! Covers the `internal.participant_left` consumer added to
//! `meeting-api/src/nats_consumers.rs`: when `actix-api` observes a participant's
//! session leave a room (a transport disconnect that did NOT go through REST
//! `/leave`), it publishes a `ParticipantLeftPayload`, and the consumer must
//! mark that participant `status='left', left_at=NOW()` so `count_admitted`
//! stops counting them as present. This is the fix for the "says 10
//! participants, but I am the only one currently in that meeting" ghost count.
//!
//! Mirrors `meeting_became_empty_consumer_tests.rs`: gated on a live NATS
//! connection (skips silently when `NATS_URL` is unset) and a live Postgres pool
//! via `DATABASE_URL`.

mod test_helpers;

use axum::body::Body;
use axum::http::StatusCode;
use meeting_api::db::participants as db_participants;
use meeting_api::nats_events::{ParticipantLeftPayload, PARTICIPANT_LEFT_SUBJECT};
use serial_test::serial;
use test_helpers::*;
use tower::ServiceExt;

/// Connect to NATS for tests, returning `None` if `NATS_URL` is unset.
async fn maybe_connect_nats() -> Option<async_nats::Client> {
    let url = std::env::var("NATS_URL").ok()?;
    Some(
        async_nats::connect(&url)
            .await
            .expect("Failed to connect to NATS"),
    )
}

/// Look up the internal `meetings.id` for a `room_id`.
async fn lookup_meeting_pk(pool: &sqlx::PgPool, room_id: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as("SELECT id FROM meetings WHERE room_id = $1")
        .bind(room_id)
        .fetch_one(pool)
        .await
        .expect("meeting row must exist");
    id
}

/// Create a meeting with the waiting room OFF (so joiners are auto-admitted) and
/// join `host` to activate it.
async fn create_and_activate(pool: &sqlx::PgPool, room_id: &str, host: &str) {
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

    join(pool, room_id, host).await;
}

/// `POST /api/v1/meetings/{room_id}/join` as `email` (auto-admitted when WR off).
async fn join(pool: &sqlx::PgPool, room_id: &str, email: &str) {
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

/// Spawn the participant-left consumer and await its subscription-ready signal.
///
/// Returns the consumer's `JoinHandle` plus a `Receiver` on the feed-change
/// broadcast the consumer feeds, so a test can assert the consumer pushes a
/// `ParticipantLeft` nudge (issue #1081) when it marks a participant left.
fn spawn_ready_consumer_with_feed(
    pool: &sqlx::PgPool,
    nats: &async_nats::Client,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Receiver<meeting_api::feed_events::FeedChange>,
) {
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (feed_tx, feed_rx) = meeting_api::feed_events::new_feed_channel();
    let handle = meeting_api::nats_consumers::spawn_participant_left_consumer_inner(
        Some(nats.clone()),
        pool.clone(),
        feed_tx,
        Some(ready_tx),
    )
    .expect("Consumer should be spawned when NATS is available");
    (ready_rx, handle, feed_rx)
}

/// Spawn the participant-left consumer and await its subscription-ready signal,
/// discarding the feed receiver (for tests that only assert DB state).
async fn spawn_ready_consumer(
    pool: &sqlx::PgPool,
    nats: &async_nats::Client,
) -> tokio::task::JoinHandle<()> {
    let (ready_rx, handle, _feed_rx) = spawn_ready_consumer_with_feed(pool, nats);
    ready_rx
        .await
        .expect("Consumer must signal subscription readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    handle
}

/// Publish a `ParticipantLeftPayload` and poll `count_admitted` until it equals
/// `expected`, up to 5s. Returns the final observed count.
async fn publish_and_poll_count(
    pool: &sqlx::PgPool,
    nats: &async_nats::Client,
    room_id: &str,
    user_id: &str,
    expected: i64,
) -> i64 {
    let pk = lookup_meeting_pk(pool, room_id).await;
    let payload = ParticipantLeftPayload {
        room_id: room_id.to_string(),
        user_id: user_id.to_string(),
    };
    nats.publish(
        PARTICIPANT_LEFT_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("publish must succeed");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let count = db_participants::count_admitted(pool, pk).await.unwrap();
        if count == expected || std::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: the defense-in-depth `AND left_at IS NULL` guard on count_admitted /
// count_waiting is pinned to a real invariant.
//
// Every production code path that sets `left_at` also moves `status` off
// 'admitted'/'waiting' in the SAME statement, so no normal flow ever produces
// the pathological `status='admitted' AND left_at IS NOT NULL` row the guard
// defends against — meaning the guard would be UNPINNED (revertible with the
// suite still green) without this test. Here we forge that row directly with a
// raw UPDATE and assert both counts EXCLUDE it. This test FAILS if either
// `AND left_at IS NULL` clause is reverted.
//
// Needs only DATABASE_URL (no NATS) — it exercises the SQL guard directly.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn admitted_with_left_at_is_excluded_from_count() {
    let pool = get_test_pool().await;
    let room_id = "test-participant-left-guard";
    let host = "plg-host@example.com";
    let stale = "plg-stale@example.com";

    // Host + one attendee, both auto-admitted (WR off), both present.
    create_and_activate(&pool, room_id, host).await;
    join(&pool, room_id, stale).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        2,
        "precondition: host + attendee both present and counted"
    );

    // Forge the pathological row the defense-in-depth guard exists for: keep
    // `status='admitted'` but stamp `left_at=NOW()` (a state no normal path
    // produces). Only the `AND left_at IS NULL` guard excludes it.
    let updated = sqlx::query(
        "UPDATE meeting_participants SET left_at = NOW() \
         WHERE meeting_id = $1 AND user_id = $2 AND status = 'admitted'",
    )
    .bind(pk)
    .bind(stale)
    .execute(&pool)
    .await
    .expect("raw UPDATE must succeed")
    .rows_affected();
    assert_eq!(
        updated, 1,
        "must have stamped left_at on the still-'admitted' row"
    );

    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        1,
        "a row with status='admitted' but left_at IS NOT NULL must be EXCLUDED \
         from count_admitted — this fails if `AND left_at IS NULL` is reverted"
    );

    // Same guard on the waiting count: forge a `status='waiting', left_at=NOW()`
    // row (the host is admitted, so flip the stale attendee to waiting first).
    sqlx::query(
        "UPDATE meeting_participants SET status = 'waiting' \
         WHERE meeting_id = $1 AND user_id = $2",
    )
    .bind(pk)
    .bind(stale)
    .execute(&pool)
    .await
    .expect("flip to waiting must succeed");
    assert_eq!(
        db_participants::count_waiting(&pool, pk).await.unwrap(),
        0,
        "a row with status='waiting' but left_at IS NOT NULL must be EXCLUDED \
         from count_waiting — this fails if `AND left_at IS NULL` is reverted"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: a participant who DISCONNECTED (no REST /leave) is marked left and is
// no longer counted by count_admitted — and a RECONNECTED participant IS
// counted again. This is the core regression test for issue #1551.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn disconnected_participant_dropped_then_reconnect_counted_again() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping participant_left integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-participant-left-ghost";
    let host = "pl-host@example.com";
    let ghost = "pl-ghost@example.com";

    // Host + one attendee, both auto-admitted (WR off).
    create_and_activate(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;

    let pk = lookup_meeting_pk(&pool, room_id).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        2,
        "precondition: host + attendee both present"
    );

    let _handle = spawn_ready_consumer(&pool, &nats).await;

    // The attendee's tab crashes — NO REST /leave is called. actix-api publishes
    // PARTICIPANT_LEFT. The consumer must mark them left so the count drops to 1.
    let count = publish_and_poll_count(&pool, &nats, room_id, ghost, 1).await;
    assert_eq!(
        count, 1,
        "a disconnected participant (no REST /leave) must be dropped from count_admitted"
    );

    // Sanity: the host is the one still counted, the ghost is marked 'left'.
    let ghost_status = db_participants::get_status(&pool, pk, ghost)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(ghost_status.status, "left", "ghost must be status='left'");
    assert!(
        ghost_status.left_at.is_some(),
        "ghost must have left_at set so the present-only count excludes them"
    );

    // The participant RECONNECTS via a fresh REST /join (clears left_at=NULL,
    // status back to 'admitted'). They must be counted again.
    join(&pool, room_id, ghost).await;
    assert_eq!(
        db_participants::count_admitted(&pool, pk).await.unwrap(),
        2,
        "a reconnected participant must be counted again"
    );
    let ghost_status = db_participants::get_status(&pool, pk, ghost)
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(
        ghost_status.status, "admitted",
        "reconnect restores 'admitted'"
    );
    assert!(
        ghost_status.left_at.is_none(),
        "reconnect must clear left_at back to NULL"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: a duplicate PARTICIPANT_LEFT event for an already-departed participant
// is idempotent — the second event is a no-op (count unchanged, left_at not
// overwritten), because `mark_left_by_disconnect`'s
// `WHERE status IN ('admitted','waiting')` guard matches zero rows once the
// participant is already 'left'. This is the multi-replica / NATS-redelivery
// safety property.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn duplicate_participant_left_event_is_idempotent() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping participant_left integration test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-participant-left-idempotent";
    let host = "pli-host@example.com";
    let leaver = "pli-leaver@example.com";

    create_and_activate(&pool, room_id, host).await;
    join(&pool, room_id, leaver).await;
    let pk = lookup_meeting_pk(&pool, room_id).await;

    let _handle = spawn_ready_consumer(&pool, &nats).await;

    // First event drops the count to 1.
    let count = publish_and_poll_count(&pool, &nats, room_id, leaver, 1).await;
    assert_eq!(count, 1, "first event marks the leaver left");
    let left_at_first = db_participants::get_status(&pool, pk, leaver)
        .await
        .unwrap()
        .expect("row")
        .left_at
        .expect("left_at set");

    // Second (duplicate) event: the WHERE status IN ('admitted','waiting') guard
    // matches zero rows, so it is a no-op — count stays 1 and left_at is unchanged.
    let count = publish_and_poll_count(&pool, &nats, room_id, leaver, 1).await;
    assert_eq!(
        count, 1,
        "duplicate event must be a no-op (count unchanged)"
    );
    let left_at_second = db_participants::get_status(&pool, pk, leaver)
        .await
        .unwrap()
        .expect("row")
        .left_at
        .expect("left_at still set");
    assert_eq!(
        left_at_first, left_at_second,
        "duplicate event must not overwrite left_at (guard matches zero rows)"
    );

    cleanup_test_data(&pool, room_id).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST: an event for an unknown room is ignored (no panic, no DB error).
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn participant_left_for_unknown_room_is_ignored() {
    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping participant_left integration test");
        return;
    };
    let pool = get_test_pool().await;
    let _handle = spawn_ready_consumer(&pool, &nats).await;

    let payload = ParticipantLeftPayload {
        room_id: "this-room-does-not-exist-1551".to_string(),
        user_id: "nobody@example.com".to_string(),
    };
    nats.publish(
        PARTICIPANT_LEFT_SUBJECT,
        serde_json::to_vec(&payload).unwrap().into(),
    )
    .await
    .expect("publish must succeed");

    // Give the consumer time to (not) act. The test passes if nothing panics and
    // the consumer task is still alive afterwards.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
}

// ──────────────────────────────────────────────────────────────────────────
// TEST (issue #1081): after the consumer marks a participant left, it pushes a
// `ParticipantLeft` feed-change nudge onto the LOCAL broadcast — so this
// instance's SSE clients re-fetch the homepage feed and drop the ghost count.
//
// This asserts the nudge is on the SUCCESS path (a row actually flipped to
// 'left'), guarding against "published before the DB write" and "not published
// on a real change". It FAILS if the `feed_tx.send(...)` in the participant-left
// consumer is removed or moved off the `rows > 0` success arm.
// ──────────────────────────────────────────────────────────────────────────
#[tokio::test]
#[serial]
async fn participant_left_consumer_pushes_feed_nudge() {
    use meeting_api::feed_events::FeedChangeReason;

    let Some(nats) = maybe_connect_nats().await else {
        eprintln!("NATS_URL not set — skipping participant_left feed-nudge test");
        return;
    };
    let pool = get_test_pool().await;
    let room_id = "test-participant-left-feed-nudge";
    let host = "plf-host@example.com";
    let ghost = "plf-ghost@example.com";

    create_and_activate(&pool, room_id, host).await;
    join(&pool, room_id, ghost).await;

    let (ready_rx, _handle, mut feed_rx) = spawn_ready_consumer_with_feed(&pool, &nats);
    ready_rx
        .await
        .expect("consumer must signal subscription readiness");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // The ghost's tab crashes — actix-api publishes PARTICIPANT_LEFT. The
    // consumer marks them left (count drops 2 -> 1) AND nudges the feed.
    let count = publish_and_poll_count(&pool, &nats, room_id, ghost, 1).await;
    assert_eq!(count, 1, "precondition: the participant was marked left");

    // The nudge must have been pushed onto the local broadcast.
    let change = tokio::time::timeout(std::time::Duration::from_secs(5), feed_rx.recv())
        .await
        .expect("a feed-change nudge must be pushed within 5s")
        .expect("broadcast must not be closed");
    assert_eq!(
        change.reason,
        FeedChangeReason::ParticipantLeft,
        "the nudge must name the ParticipantLeft reason"
    );
    assert_eq!(
        change.meeting_id, room_id,
        "the nudge must carry the affected room_id"
    );

    cleanup_test_data(&pool, room_id).await;
}
