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

//! SQLite-only coverage with no PostgreSQL analogue: the per-connection pragmas
//! `db::connect` sets, and the `SQLITE_BUSY` retry in `db::lock::with_write_retry`.
//! Everything else runs against both backends from the shared suite.
//!
//! Tests use the same `DATABASE_URL` database dbmate migrated for the rest of the
//! suite; each pool differs from production in exactly the one option under test,
//! so a passing test shows that option is load-bearing. Being SQLite-only, this
//! file binds timestamps directly rather than via the `db::now_sql` shim.
//!
//! Every test must `pool.close().await` before returning: they share one file,
//! and sqlx tears a dropped pool's connections down in the background — the last
//! runs a WAL checkpoint that takes the write lock. A `busy_timeout(0)` pool
//! opened by the next test would hit that as an instant `SQLITE_BUSY`. Closing
//! makes teardown synchronous so opens never overlap it.
//!
//! The `db::mod` feature guards (both backends / neither refuse to compile) are
//! verified by hand rather than with a `trybuild` dev-dependency.

#![cfg(all(feature = "sqlite", not(feature = "postgres")))]

mod test_helpers;

use chrono::Utc;
use meeting_api::db::{lock, q, DbPool};
use serial_test::serial;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;
use std::time::Duration;
use test_helpers::{cleanup_test_data, get_test_pool};

// ── Per-connection pragmas ──────────────────────────────────────────────

/// The pragmas are per *connection*, so setting them on only the first would
/// leave the rest on SQLite's defaults. Hold all five at once and check each —
/// `busy_timeout` especially, which fails silently until a connection contends.
#[tokio::test]
#[serial]
async fn test_every_pooled_connection_has_the_required_pragmas() {
    let pool = get_test_pool().await;

    // `db::connect` builds the pool with max_connections(5); holding five at
    // once guarantees each was configured on open rather than reused.
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(pool.acquire().await.expect("acquire pooled connection"));
    }

    for (n, conn) in held.iter_mut().enumerate() {
        let foreign_keys: i64 = sqlx::query("PRAGMA foreign_keys")
            .fetch_one(&mut **conn)
            .await
            .expect("read foreign_keys")
            .get(0);
        assert_eq!(
            foreign_keys, 1,
            "connection {n} has foreign_keys off; ON DELETE CASCADE is inert on it"
        );

        let journal_mode: String = sqlx::query("PRAGMA journal_mode")
            .fetch_one(&mut **conn)
            .await
            .expect("read journal_mode")
            .get(0);
        assert_eq!(
            journal_mode.to_lowercase(),
            "wal",
            "connection {n} is not in WAL mode; readers will block on the writer"
        );

        let busy_timeout: i64 = sqlx::query("PRAGMA busy_timeout")
            .fetch_one(&mut **conn)
            .await
            .expect("read busy_timeout")
            .get(0);
        assert_eq!(
            busy_timeout, 5_000,
            "connection {n} has busy_timeout {busy_timeout}ms, not the 5000ms \
             db::connect sets; it will surface SQLITE_BUSY without waiting"
        );
    }

    // Return the checked-out connections, then close synchronously — see the
    // module docs.
    drop(held);
    pool.close().await;
}

/// The pragma check above proves the setting is on; this proves it is load
/// bearing. The same cascading delete against a pool built *without*
/// `foreign_keys` silently leaves orphans, which is what the production pool
/// would do if that option were ever dropped.
#[tokio::test]
#[serial]
async fn test_cascade_depends_on_the_foreign_keys_pragma() {
    let room_id = "sqlite-fk-off";
    let pool = variant_pool(/* foreign_keys */ false, Duration::from_secs(5)).await;
    cleanup_test_data(&pool, room_id).await;

    let (meeting_id,): (i32,) = sqlx::query_as(&q(
        "INSERT INTO meetings (room_id, started_at) VALUES ($1, $2) RETURNING id",
    ))
    .bind(room_id)
    .bind(Utc::now())
    .fetch_one(&pool)
    .await
    .expect("seed meeting");

    sqlx::query(&q(
        "INSERT INTO meeting_participants (meeting_id, user_id) VALUES ($1, $2)",
    ))
    .bind(meeting_id)
    .bind("guest@example.com")
    .execute(&pool)
    .await
    .expect("seed participant");

    sqlx::query(&q("DELETE FROM meetings WHERE id = $1"))
        .bind(meeting_id)
        .execute(&pool)
        .await
        .expect("delete without foreign key enforcement");

    let orphans: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1",
    ))
    .bind(meeting_id)
    .fetch_one(&pool)
    .await
    .expect("count orphans");
    assert_eq!(
        orphans.0, 1,
        "expected the pragma-off pool to leave an orphan — if this fails the schema \
         gained enforcement from somewhere else and the cascade test is no longer \
         proving that db::connect turns foreign keys on"
    );

    // The orphan outlived its meeting, so cleanup_test_data's room_id join can
    // no longer reach it.
    sqlx::query(&q("DELETE FROM meeting_participants WHERE meeting_id = $1"))
        .bind(meeting_id)
        .execute(&pool)
        .await
        .expect("clean up the orphan");
    pool.close().await;
}

// ── SQLITE_BUSY retry ───────────────────────────────────────────────────

/// `with_write_retry` must turn lock contention into a delayed success.
///
/// Production sets `busy_timeout(5s)`, so SQLite spins internally and the retry
/// is only reached when contention outlasts that — impossible to provoke in a
/// test without a five-second stall. This uses a pool with `busy_timeout(0)`
/// instead, which surfaces `SQLITE_BUSY` immediately and puts the retry on the
/// same code path it would take in production.
#[tokio::test]
#[serial]
async fn test_with_write_retry_recovers_from_a_busy_database() {
    let room_id = "sqlite-busy-retry";
    let pool = variant_pool(true, Duration::ZERO).await;
    cleanup_test_data(&pool, room_id).await;

    // First: prove the setup really does produce SQLITE_BUSY. Without this the
    // success below could just mean nothing was ever contended.
    let holder = hold_write_lock(&pool, Duration::from_millis(120));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let unretried = lock::begin_write(&pool).await;
    assert!(
        unretried.is_err(),
        "expected BEGIN IMMEDIATE to fail while another writer holds the lock \
         and busy_timeout is 0"
    );
    holder.await.expect("holder task should not panic");

    // Then: the same contention, absorbed by the retry.
    let holder = hold_write_lock(&pool, Duration::from_millis(120));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let pool_for_op = pool.clone();
    let result = lock::with_write_retry(move || {
        let pool = pool_for_op.clone();
        Box::pin(async move {
            let mut tx = lock::begin_write(&pool).await?;
            let sql = q("INSERT INTO meetings (room_id, started_at) VALUES ($1, $2)");
            sqlx::query(&sql)
                .bind(room_id)
                .bind(Utc::now())
                .execute(&mut *tx)
                .await?;
            tx.commit().await
        })
    })
    .await;
    holder.await.expect("holder task should not panic");

    result.expect("with_write_retry should have retried past the busy window");

    let written: (i64,) = sqlx::query_as(&q("SELECT COUNT(*) FROM meetings WHERE room_id = $1"))
        .bind(room_id)
        .fetch_one(&pool)
        .await
        .expect("count written rows");
    assert_eq!(
        written.0, 1,
        "the retried transaction must be applied exactly once — a rolled-back attempt \
         that left rows behind would double-insert here"
    );

    cleanup_test_data(&pool, room_id).await;
    pool.close().await;
}

/// Contention that never clears must surface as an error rather than hanging,
/// and must do so within the documented latency budget.
#[tokio::test]
#[serial]
async fn test_with_write_retry_gives_up_on_permanent_contention() {
    let pool = variant_pool(true, Duration::ZERO).await;

    let mut blocker = pool.acquire().await.expect("acquire blocker connection");
    sqlx::raw_sql("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .expect("take the write lock and keep it");

    let pool_for_op = pool.clone();
    let started = std::time::Instant::now();
    let result: Result<(), sqlx::Error> = lock::with_write_retry(move || {
        let pool = pool_for_op.clone();
        Box::pin(async move {
            lock::begin_write(&pool).await?;
            Ok(())
        })
    })
    .await;
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "with_write_retry must bound its attempts and return the error, not spin forever"
    );
    // busy_timeout is 0 here, so every attempt fails instantly and the only
    // thing bounding the loop is the deadline itself.
    assert!(
        elapsed < lock::RETRY_DEADLINE * 2,
        "gave up after {elapsed:?}, which is not bounded by RETRY_DEADLINE ({:?}) — \
         the retry loop is counting attempts rather than watching the clock",
        lock::RETRY_DEADLINE
    );

    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("release the write lock");
    drop(blocker);
    pool.close().await;
}

// ── helpers ─────────────────────────────────────────────────────────────

/// Hold the database write lock for `duration`, then release it.
fn hold_write_lock(pool: &DbPool, duration: Duration) -> tokio::task::JoinHandle<()> {
    let pool = pool.clone();
    tokio::spawn(async move {
        let tx = lock::begin_write(&pool).await.expect("take the write lock");
        tokio::time::sleep(duration).await;
        tx.rollback().await.expect("release the write lock");
    })
}

/// A pool on the suite's `DATABASE_URL` database, differing from the production
/// pool only in the options passed here — used by tests that must provoke what
/// production suppresses. Everything else uses [`test_helpers::get_test_pool`].
async fn variant_pool(foreign_keys: bool, busy_timeout: Duration) -> DbPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let options = SqliteConnectOptions::from_str(&url)
        .expect("valid SQLite DATABASE_URL")
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(busy_timeout)
        .foreign_keys(foreign_keys);

    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("open variant SQLite pool")
}
