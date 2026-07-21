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

//! Concurrency tests for the waiting room, run against both backends.
//!
//! `join_attendee` reads `waiting_room_enabled` then inserts a participant whose
//! status depends on the read; `update_waiting_room_enabled` writes that flag and
//! admits the waiting when disabling. They must serialize via
//! `db::lock::begin_write` (`FOR UPDATE` on PostgreSQL, `BEGIN IMMEDIATE` on
//! SQLite). A plain deferred `pool.begin()` takes no lock and permits:
//!
//! ```text
//! T1 join_attendee : BEGIN; SELECT waiting_room_enabled -> true
//! T2 host toggle   : BEGIN; UPDATE waiting_room_enabled = false;
//!                           UPDATE participants SET status='admitted' WHERE status='waiting'; COMMIT
//! T1               : INSERT participant status='waiting'; COMMIT   -- stranded, never admitted
//! ```
//!
//! Reverting `begin_write` to `pool.begin()` fails
//! [`test_toggle_off_between_a_joins_read_and_write_never_strands`] on PostgreSQL
//! (the strand above) and [`test_concurrent_writers_do_not_surface_lock_errors`]
//! on SQLite (its single write lock turns the same revert into `database is
//! locked`). Those two hold the line; the swept-delay tests cover contention
//! broadly but the window is too short for timing alone to catch the revert.
//!
//! The *enabling* side has no final-state assertion: "waiting room on, attendee
//! admitted" is also the legal serial order (join, then enable — enabling does
//! not evict), so it is indistinguishable from the rows afterwards. It is instead
//! covered by [`test_toggle_on_race_stays_internally_consistent`] (each join's
//! result matches the flag it acted on) and
//! [`test_join_after_a_committed_enable_always_waits`] (once the host's call
//! returns, no later join is auto-admitted).
//!
//! Where a test does depend on timing it sweeps rather than sleeps: each
//! iteration offsets the competing task by a slightly larger delay, so the
//! window is crossed at many points instead of one lucky one.

mod test_helpers;

use chrono::Utc;
use meeting_api::db::{meetings as db_meetings, participants as db_participants, q, DbPool};
use serde_json::json;
use serial_test::serial;
use std::time::Duration;
use test_helpers::*;

/// Number of interleavings to try per race test.
const ITERATIONS: u32 = 40;

/// How much later each iteration starts the competing task.
const SWEEP_STEP: Duration = Duration::from_micros(75);

/// Create a fresh meeting for one iteration of a race and return its id.
async fn fresh_meeting(pool: &DbPool, room_id: &str, waiting_room_enabled: bool) -> i32 {
    cleanup_test_data(pool, room_id).await;
    db_meetings::create_with_options(
        pool,
        room_id,
        "host@example.com",
        None,
        &json!([]),
        waiting_room_enabled,
    )
    .await
    .expect("create meeting for race iteration")
    .id
}

/// The interleaving from the module docs, forced rather than waited for.
///
/// A join cannot be paused between its read and its write from outside — but it
/// can be *blocked* there. This test holds an uncommitted `meeting_participants`
/// row for the joining user, which the join's `INSERT ... ON CONFLICT` must wait
/// on, and that wait is the window:
///
/// ```text
/// W  (this test)   : BEGIN; INSERT participant (meeting, attendee)   -- uncommitted
/// T1 join_attendee : BEGIN; SELECT waiting_room_enabled -> true; INSERT ... blocked on W
/// T2 host toggle   : BEGIN; UPDATE waiting_room_enabled = false;
///                           admit-all sees nothing waiting; COMMIT
/// W                : ROLLBACK
/// T1               : INSERT participant status='waiting'; COMMIT    -- stranded
/// ```
///
/// With `begin_write` intact, T1 takes the meeting's write lock *before* its
/// read, so T2's UPDATE queues behind T1 instead of slipping past it: T1 commits
/// its `waiting` row first and T2's admit-all then finds and admits it.
///
/// The interleaving above is exact on PostgreSQL (W blocks T1 but not T2, since
/// they touch different rows). SQLite's single write lock makes W block T2 too,
/// so both writers just queue behind W; the assertion still holds, less
/// pointedly. That asymmetry is why `begin_write` has two implementations.
#[tokio::test]
#[serial]
async fn test_toggle_off_between_a_joins_read_and_write_never_strands() {
    let pool = get_test_pool().await;
    let room_id = "race-forced-interleave";
    let meeting_id = fresh_meeting(&pool, room_id, true).await;
    let attendee = "attendee@example.com";

    // W: claim the attendee's (meeting_id, user_id) key without committing, so
    // any join for that attendee blocks on the unique constraint.
    let mut blocker = pool.begin().await.expect("begin blocking transaction");
    sqlx::query(&q(
        "INSERT INTO meeting_participants (meeting_id, user_id, status, joined_at, created_at, updated_at) \
         VALUES ($1, $2, 'waiting', $3, $3, $3)",
    ))
    .bind(meeting_id)
    .bind(attendee)
    .bind(Utc::now())
    .execute(&mut *blocker)
    .await
    .expect("claim the participant key");

    let joiner = {
        let pool = pool.clone();
        tokio::spawn(async move {
            db_participants::join_attendee(&pool, meeting_id, attendee, None).await
        })
    };
    // Let the join reach its INSERT and park there.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let toggler = {
        let pool = pool.clone();
        let room_id = room_id.to_string();
        tokio::spawn(async move {
            db_meetings::update_waiting_room_enabled(&pool, &room_id, "host@example.com", false)
                .await
        })
    };
    // Give the toggle every chance to run to completion ahead of the join.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Guard against a vacuous pass: if the join already finished, it never sat
    // in the window and the assertions below would hold for the boring reason.
    assert!(
        !joiner.is_finished(),
        "the join completed before the blocking key was released, so it never \
         parked between its read of waiting_room_enabled and its INSERT — this \
         run proves nothing about the race"
    );

    blocker
        .rollback()
        .await
        .expect("release the participant key");

    let (_, row, _) = joiner
        .await
        .expect("join task should not panic")
        .expect("join_attendee should succeed once the key is free");
    toggler
        .await
        .expect("toggle task should not panic")
        .expect("update_waiting_room_enabled should succeed")
        .expect("toggle should match the meeting");

    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .expect("re-read meeting")
        .expect("meeting should exist");
    let status = db_participants::get_status(&pool, meeting_id, attendee)
        .await
        .expect("read participant status")
        .expect("participant should exist");

    assert!(
        !meeting.waiting_room_enabled,
        "the host's disable must have stuck"
    );
    assert_eq!(
        status.status, "admitted",
        "the attendee is stranded: the waiting room is off and they are still '{}' \
         (the join returned '{}'). The join's read of waiting_room_enabled was not \
         serialized against the toggle — db::lock::begin_write must take the write \
         lock before that read.",
        status.status, row.status
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Disabling the waiting room concurrently with a join must never leave the
/// joiner stranded in `waiting` with the waiting room already off.
///
/// Both serial orders admit the attendee: joining first puts them in `waiting`
/// and the toggle's sweep admits them; toggling first makes the join observe a
/// disabled waiting room and auto-admit. So `waiting_room_enabled = false` with
/// a `waiting` participant is not equivalent to any serial execution — it is a
/// participant nothing will ever let in.
#[tokio::test]
#[serial]
async fn test_toggle_off_never_strands_a_waiting_participant() {
    let pool = get_test_pool().await;
    let mut violations: Vec<String> = Vec::new();

    for i in 0..ITERATIONS {
        let room_id = format!("race-off-{i}");
        let meeting_id = fresh_meeting(&pool, &room_id, true).await;

        let joiner = {
            let pool = pool.clone();
            tokio::spawn(async move {
                db_participants::join_attendee(&pool, meeting_id, "attendee@example.com", None)
                    .await
            })
        };
        let toggler = {
            let pool = pool.clone();
            let room_id = room_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SWEEP_STEP * i).await;
                db_meetings::update_waiting_room_enabled(&pool, &room_id, "host@example.com", false)
                    .await
            })
        };

        let join_result = joiner.await.expect("join task should not panic");
        let toggle_result = toggler.await.expect("toggle task should not panic");

        join_result.unwrap_or_else(|e| panic!("iteration {i}: join_attendee failed: {e}"));
        let toggled = toggle_result
            .unwrap_or_else(|e| panic!("iteration {i}: update_waiting_room_enabled failed: {e}"))
            .unwrap_or_else(|| panic!("iteration {i}: toggle matched no meeting"));
        assert!(
            !toggled.waiting_room_enabled,
            "iteration {i}: the toggle should have disabled the waiting room"
        );

        let meeting = db_meetings::get_by_room_id(&pool, &room_id)
            .await
            .expect("re-read meeting")
            .expect("meeting should exist");
        let stranded = db_participants::count_waiting(&pool, meeting_id)
            .await
            .expect("count waiting");

        if !meeting.waiting_room_enabled && stranded > 0 {
            violations.push(format!(
                "iteration {i} (toggle delayed {:?}): waiting_room_enabled = false but \
                 {stranded} participant(s) left in 'waiting'",
                SWEEP_STEP * i
            ));
        }

        cleanup_test_data(&pool, &room_id).await;
    }

    assert!(
        violations.is_empty(),
        "non-serializable outcomes observed — a join interleaved with the waiting room \
         being disabled stranded the participant. This is exactly what db::lock::begin_write \
         prevents; a plain pool.begin() (BEGIN DEFERRED / no FOR UPDATE) reproduces it.\n{}",
        violations.join("\n")
    );
}

/// Enabling the waiting room concurrently with a join must not corrupt the
/// pair (flag observed, status written) or surface lock errors to the caller.
///
/// `join_attendee` returns the `waiting_room_enabled` it read under the lock,
/// and the route issues a room-access token from exactly that value, so the
/// value and the row it produced have to agree in every interleaving. On SQLite
/// this is also the path that exercises `BEGIN IMMEDIATE` contention: an error
/// escaping here means `with_write_retry` failed to absorb a busy database.
#[tokio::test]
#[serial]
async fn test_toggle_on_race_stays_internally_consistent() {
    let pool = get_test_pool().await;

    for i in 0..ITERATIONS {
        let room_id = format!("race-on-{i}");
        let meeting_id = fresh_meeting(&pool, &room_id, false).await;

        let joiner = {
            let pool = pool.clone();
            tokio::spawn(async move {
                db_participants::join_attendee(&pool, meeting_id, "attendee@example.com", None)
                    .await
            })
        };
        let toggler = {
            let pool = pool.clone();
            let room_id = room_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(SWEEP_STEP * i).await;
                db_meetings::update_waiting_room_enabled(&pool, &room_id, "host@example.com", true)
                    .await
            })
        };

        let (auto_admitted, row, observed_waiting_room) = joiner
            .await
            .expect("join task should not panic")
            .unwrap_or_else(|e| panic!("iteration {i}: join_attendee failed: {e}"));
        toggler
            .await
            .expect("toggle task should not panic")
            .unwrap_or_else(|e| panic!("iteration {i}: update_waiting_room_enabled failed: {e}"))
            .unwrap_or_else(|| panic!("iteration {i}: toggle matched no meeting"));

        assert_eq!(
            auto_admitted, !observed_waiting_room,
            "iteration {i}: auto_admitted must be the negation of the flag read under the lock"
        );
        assert_eq!(
            row.status,
            if observed_waiting_room {
                "waiting"
            } else {
                "admitted"
            },
            "iteration {i}: the participant's status must match the flag the join acted on; \
             a mismatch means the row was written against a different value than the one \
             the caller was handed (and mints a room token from)"
        );

        let meeting = db_meetings::get_by_room_id(&pool, &room_id)
            .await
            .expect("re-read meeting")
            .expect("meeting should exist");
        assert!(
            meeting.waiting_room_enabled,
            "iteration {i}: the host's enable must have stuck"
        );

        cleanup_test_data(&pool, &room_id).await;
    }
}

/// The observable ordering guarantee on the enabling side: once the host's
/// `update_waiting_room_enabled(true)` has *returned*, every join that starts
/// afterwards must land in the waiting room. A join that still auto-admits here
/// would be reading a snapshot older than a committed transaction.
#[tokio::test]
#[serial]
async fn test_join_after_a_committed_enable_always_waits() {
    let pool = get_test_pool().await;
    let room_id = "race-enable-then-join";
    let meeting_id = fresh_meeting(&pool, room_id, false).await;

    db_meetings::update_waiting_room_enabled(&pool, room_id, "host@example.com", true)
        .await
        .expect("enable waiting room")
        .expect("toggle should match the meeting");

    // Several joiners, concurrently, all starting after the enable committed.
    let mut joins = Vec::new();
    for n in 0..8 {
        let pool = pool.clone();
        joins.push(tokio::spawn(async move {
            db_participants::join_attendee(
                &pool,
                meeting_id,
                &format!("guest{n}@example.com"),
                None,
            )
            .await
        }));
    }

    for (n, join) in joins.into_iter().enumerate() {
        let (auto_admitted, row, observed) = join
            .await
            .expect("join task should not panic")
            .unwrap_or_else(|e| panic!("guest {n}: join_attendee failed: {e}"));
        assert!(
            !auto_admitted && observed && row.status == "waiting",
            "guest {n} was admitted after the host's enable had already committed \
             (auto_admitted={auto_admitted}, observed_waiting_room={observed}, status={})",
            row.status
        );
    }

    assert_eq!(
        db_participants::count_admitted(&pool, meeting_id)
            .await
            .expect("count admitted"),
        0,
        "no participant may be admitted once the waiting room is on"
    );

    cleanup_test_data(&pool, room_id).await;
}

/// Many writers at once: joins racing repeated toggles. Every call must either
/// succeed or be retried into success by `db::lock::with_write_retry` — a
/// `SQLITE_BUSY` reaching the caller is a bug, and so is a lost participant.
#[tokio::test]
#[serial]
async fn test_concurrent_writers_do_not_surface_lock_errors() {
    let pool = get_test_pool().await;
    let room_id = "race-writer-storm";
    let meeting_id = fresh_meeting(&pool, room_id, true).await;

    let mut tasks = Vec::new();
    for n in 0..12 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            db_participants::join_attendee(
                &pool,
                meeting_id,
                &format!("storm{n}@example.com"),
                None,
            )
            .await
            .map(|_| ())
        }));
    }
    for n in 0..6 {
        let pool = pool.clone();
        let room_id = room_id.to_string();
        tasks.push(tokio::spawn(async move {
            db_meetings::update_waiting_room_enabled(
                &pool,
                &room_id,
                "host@example.com",
                n % 2 == 0,
            )
            .await
            .map(|_| ())
        }));
    }

    for (n, task) in tasks.into_iter().enumerate() {
        task.await
            .expect("task should not panic")
            .unwrap_or_else(|e| panic!("writer {n} failed under contention: {e}"));
    }

    // Every joiner must exist exactly once, in one of the two live statuses.
    let waiting = db_participants::count_waiting(&pool, meeting_id)
        .await
        .expect("count waiting");
    let admitted = db_participants::count_admitted(&pool, meeting_id)
        .await
        .expect("count admitted");
    assert_eq!(
        waiting + admitted,
        12,
        "all 12 joiners must be present exactly once (waiting={waiting}, admitted={admitted})"
    );

    // And the stranding invariant still holds after the storm.
    let meeting = db_meetings::get_by_room_id(&pool, room_id)
        .await
        .expect("re-read meeting")
        .expect("meeting should exist");
    if !meeting.waiting_room_enabled {
        assert_eq!(
            waiting, 0,
            "the waiting room ended up disabled with {waiting} participant(s) still waiting"
        );
    }

    cleanup_test_data(&pool, room_id).await;
}
