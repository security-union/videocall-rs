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

//! Tests for the parts of the SQLite backend that have no PostgreSQL analogue:
//! the per-connection pragmas `db::connect` sets, and the `SQLITE_BUSY` retry
//! in `db::lock::with_write_retry`.
//!
//! Everything else about the SQLite build is covered by the shared suite, which
//! runs unchanged under `--no-default-features --features sqlite`.
//!
//! ## Feature guards (verified by hand, not by a test)
//!
//! `db::mod` refuses to compile with both backends or with neither. Adding
//! `trybuild` to prove that would cost a dev-dependency and a compile of the
//! whole crate per case, so it is checked manually instead. Observed on this
//! branch:
//!
//! ```text
//! $ cargo check -p meeting-api --all-features
//! error: meeting-api: the `postgres` and `sqlite` features are mutually exclusive.
//!        Build with `--no-default-features --features sqlite` to select SQLite.
//!   --> meeting-api/src/db/mod.rs:46:1
//! error: could not compile `meeting-api` (lib) due to 1 previous error
//!
//! $ cargo check -p meeting-api --no-default-features
//! error: meeting-api: exactly one database backend feature must be enabled
//!        (`postgres` or `sqlite`).
//!   --> meeting-api/src/db/mod.rs:52:1
//! error[E0432]: unresolved import `crate::db::DbPool`
//!   --> meeting-api/src/db/lock.rs:33:5
//! ```
//!
//! The `--all-features` case stops at the guard with nothing else reported. The
//! no-backend case reports the guard first and then the expected cascade of
//! `DbPool` being undefined, which is why the guard lives in `db/mod.rs`: it is
//! the first thing the user reads.

#![cfg(all(feature = "sqlite", not(feature = "postgres")))]

mod test_helpers;

use meeting_api::db::{lock, DbPool};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::Row;
use std::str::FromStr;
use std::time::Duration;
use test_helpers::sqlite_support;

// ── Per-connection pragmas ──────────────────────────────────────────────

/// `foreign_keys`, `journal_mode` and `busy_timeout` are per *connection*, so a
/// pool that set them only while opening the first one would leave the other
/// four on SQLite's defaults — foreign keys off, rollback journal, no wait.
/// Hold every connection in the pool at once and check each of them.
#[tokio::test]
async fn test_every_pooled_connection_has_the_required_pragmas() {
    let pool = sqlite_support::migrated_pool().await;

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
    }
}

/// The pragma check above proves the setting is on; this proves it is load
/// bearing. The same cascading delete against a pool built *without*
/// `foreign_keys` silently leaves orphans, which is what the production pool
/// would do if that option were ever dropped.
#[tokio::test]
async fn test_cascade_depends_on_the_foreign_keys_pragma() {
    let path = scratch_path("fk-off");
    let pool = raw_pool(&path, /* foreign_keys */ false, Duration::from_secs(5)).await;
    sqlite_support::migrate(&pool).await;

    sqlx::raw_sql(
        "INSERT INTO meetings (id, room_id, started_at, created_at, updated_at) \
         VALUES (1, 'fk-off', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'); \
         INSERT INTO meeting_participants (meeting_id, user_id) VALUES (1, 'guest@example.com'); \
         DELETE FROM meetings WHERE id = 1;",
    )
    .execute(&pool)
    .await
    .expect("seed and delete without foreign key enforcement");

    let orphans: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM meeting_participants")
        .fetch_one(&pool)
        .await
        .expect("count orphans");
    assert_eq!(
        orphans.0, 1,
        "expected the pragma-off pool to leave an orphan — if this fails the schema \
         gained enforcement from somewhere else and the cascade test is no longer \
         proving that db::connect turns foreign keys on"
    );

    pool.close().await;
    cleanup_db_files(&path);
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
async fn test_with_write_retry_recovers_from_a_busy_database() {
    let path = scratch_path("busy-retry");
    let pool = raw_pool(&path, true, Duration::ZERO).await;
    sqlite_support::migrate(&pool).await;

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
            sqlx::query(
                "INSERT INTO meetings (room_id, started_at, created_at, updated_at) \
                 VALUES ('busy-retry', '2026-01-01T00:00:00+00:00', \
                         '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00')",
            )
            .execute(&mut *tx)
            .await?;
            tx.commit().await
        })
    })
    .await;
    holder.await.expect("holder task should not panic");

    result.expect("with_write_retry should have retried past the busy window");

    let written: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM meetings WHERE room_id = 'busy-retry'")
            .fetch_one(&pool)
            .await
            .expect("count written rows");
    assert_eq!(
        written.0, 1,
        "the retried transaction must be applied exactly once — a rolled-back attempt \
         that left rows behind would double-insert here"
    );

    pool.close().await;
    cleanup_db_files(&path);
}

/// Contention that never clears must surface as an error rather than hanging.
#[tokio::test]
async fn test_with_write_retry_gives_up_on_permanent_contention() {
    let path = scratch_path("busy-forever");
    let pool = raw_pool(&path, true, Duration::ZERO).await;
    sqlite_support::migrate(&pool).await;

    let mut blocker = pool.acquire().await.expect("acquire blocker connection");
    sqlx::raw_sql("BEGIN IMMEDIATE")
        .execute(&mut *blocker)
        .await
        .expect("take the write lock and keep it");

    let pool_for_op = pool.clone();
    let result: Result<(), sqlx::Error> = lock::with_write_retry(move || {
        let pool = pool_for_op.clone();
        Box::pin(async move {
            lock::begin_write(&pool).await?;
            Ok(())
        })
    })
    .await;

    assert!(
        result.is_err(),
        "with_write_retry must bound its attempts and return the error, not spin forever"
    );

    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *blocker)
        .await
        .expect("release the write lock");
    drop(blocker);
    pool.close().await;
    cleanup_db_files(&path);
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

/// A pool with deliberately non-production options.
///
/// Only the tests that need to *provoke* what production suppresses use this;
/// everything else goes through `meeting_api::db::connect`.
async fn raw_pool(path: &std::path::Path, foreign_keys: bool, busy_timeout: Duration) -> DbPool {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("valid SQLite URL")
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(busy_timeout)
        .foreign_keys(foreign_keys)
        .create_if_missing(true);

    SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("open scratch SQLite pool")
}

/// A fresh database file for one test, removed first so reruns start clean.
fn scratch_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("meeting-api-tests");
    std::fs::create_dir_all(&dir).expect("create test scratch dir");
    let path = dir.join(format!("{name}-{}.db", std::process::id()));
    cleanup_db_files(&path);
    path
}

fn cleanup_db_files(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}
