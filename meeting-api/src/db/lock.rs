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

//! Write-transaction primitives, the one place the two dialects genuinely differ.
//!
//! Two code paths mutate the waiting room and must serialize against each other:
//! [`crate::db::participants::join_attendee`] (reads `waiting_room_enabled`,
//! then inserts the participant as `waiting` or `admitted`) and
//! [`crate::db::meetings::update_waiting_room_enabled`] (flips the flag and, when
//! disabling, admits everyone who is currently waiting). If they interleave, a
//! participant can be parked in the waiting room of a meeting whose waiting room
//! was just turned off, and nothing will ever admit them.
//!
//! PostgreSQL serializes them with `SELECT ... FOR UPDATE` on the `meetings` row.
//! SQLite has no row locks, so the transaction has to take the database write
//! lock up front with `BEGIN IMMEDIATE`; a deferred transaction that upgrades to
//! a write mid-flight can fail with `SQLITE_BUSY_SNAPSHOT` and, worse, would have
//! read the flag before taking the lock.
//!
//! **Both** paths must use [`begin_write`]. Making only one of them immediate
//! moves the race rather than closing it.

use crate::db::DbPool;
use futures::future::BoxFuture;

/// The compiled-in sqlx driver.
#[cfg(feature = "postgres")]
pub type Db = sqlx::Postgres;

/// The compiled-in sqlx driver.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type Db = sqlx::Sqlite;

/// A transaction that has already taken whatever write lock the backend needs.
pub type WriteTransaction = sqlx::Transaction<'static, Db>;

/// Read `waiting_room_enabled` for one meeting under the transaction's write lock.
///
/// On PostgreSQL the row lock is acquired by this statement. On SQLite the
/// database write lock was already acquired by `BEGIN IMMEDIATE`, so the plain
/// `SELECT` is equally serialized.
#[cfg(feature = "postgres")]
pub const SELECT_WAITING_ROOM_LOCKED: &str =
    "SELECT waiting_room_enabled FROM meetings WHERE id = $1 FOR UPDATE";

/// Read `waiting_room_enabled` for one meeting under the transaction's write lock.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub const SELECT_WAITING_ROOM_LOCKED: &str =
    "SELECT waiting_room_enabled FROM meetings WHERE id = $1";

/// Begin a transaction intended to write, acquiring the write lock immediately.
#[cfg(feature = "postgres")]
pub async fn begin_write(pool: &DbPool) -> Result<WriteTransaction, sqlx::Error> {
    pool.begin().await
}

/// Begin a transaction intended to write, acquiring the write lock immediately.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn begin_write(pool: &DbPool) -> Result<WriteTransaction, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Run a write transaction, retrying it if the backend reports lock contention.
///
/// PostgreSQL takes row locks and waits, so this simply awaits `op` once and the
/// behaviour is unchanged from before the SQLite work.
#[cfg(feature = "postgres")]
pub async fn with_write_retry<'a, T, F>(mut op: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> BoxFuture<'a, Result<T, sqlx::Error>>,
{
    op().await
}

/// Run a write transaction, retrying it if SQLite reports `SQLITE_BUSY` /
/// `SQLITE_LOCKED`.
///
/// `busy_timeout` (set on every pooled connection by [`crate::db::connect`])
/// already makes SQLite spin internally, so reaching this retry means contention
/// outlasted that timeout. `op` must be replayable — either it opens its own
/// transaction, which a failed attempt has already rolled back, or it is a
/// single autocommit statement, which applied nothing if it failed.
///
/// # Latency budget
///
/// Retries are bounded by [`RETRY_DEADLINE`] wall-clock, not by an attempt
/// count. This is a real-time conferencing API sitting behind a client timeout:
/// a bound of "5 attempts" is really "5 × `busy_timeout` + backoff", which at
/// the production `busy_timeout` of 5s is ~25s of an HTTP handler holding a
/// connection open long after the caller has given up.
///
/// The deadline gates *starting* a new attempt, so the true worst case is
/// `RETRY_DEADLINE + busy_timeout` (~8s) — an attempt already in flight is not
/// interrupted, because cancelling a transaction mid-commit is worse than
/// waiting for it.
///
/// Only lock contention is retried. Notably [`sqlx::Error::PoolTimedOut`] is
/// passed straight through: it means every connection is checked out, so
/// retrying would queue behind the same exhausted pool and multiply a 30s
/// acquire timeout rather than shorten it.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn with_write_retry<'a, T, F>(mut op: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> BoxFuture<'a, Result<T, sqlx::Error>>,
{
    use std::time::Instant;

    let started = Instant::now();
    let mut backoff = BASE_BACKOFF;
    let mut attempt: u32 = 1;

    loop {
        match op().await {
            Err(err)
                if is_lock_contention(&err)
                    && started.elapsed().saturating_add(backoff) < RETRY_DEADLINE =>
            {
                tracing::warn!(
                    "SQLite write contention on attempt {attempt} ({:?} elapsed of {RETRY_DEADLINE:?}), \
                     retrying in {backoff:?}: {err}",
                    started.elapsed()
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
                attempt += 1;
            }
            result => return result,
        }
    }
}

/// How long [`with_write_retry`] may keep starting fresh attempts.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub const RETRY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// Delay before the first retry; doubles up to [`MAX_BACKOFF`].
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
const BASE_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

/// Ceiling on the exponential backoff, so a long deadline still retries often.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
const MAX_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// Whether an error is SQLite refusing to take the write lock.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
fn is_lock_contention(err: &sqlx::Error) -> bool {
    /// `SQLITE_BUSY`
    const BUSY: i32 = 5;
    /// `SQLITE_LOCKED`
    const LOCKED: i32 = 6;

    let sqlx::Error::Database(db_err) = err else {
        return false;
    };
    let Some(code) = db_err.code() else {
        return false;
    };
    // sqlx surfaces SQLite's *extended* result code; the primary code is the
    // low byte (e.g. 517 = SQLITE_BUSY_SNAPSHOT -> 5).
    matches!(code.parse::<i32>().map(|c| c & 0xff), Ok(BUSY | LOCKED))
}
