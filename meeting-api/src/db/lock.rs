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

//! Write-transaction primitives.
//!
//! `join_attendee` (reads `waiting_room_enabled`, then inserts) and
//! `update_waiting_room_enabled` (flips the flag, admits the waiting) must
//! serialize, or an attendee lands in the waiting room of a meeting whose
//! waiting room was just turned off and is never admitted. PostgreSQL serializes
//! them with `SELECT ... FOR UPDATE`; SQLite has no row locks, so the
//! transaction takes the database write lock up front with `BEGIN IMMEDIATE`.
//!
//! Both paths must use [`begin_write`] — making only one immediate moves the
//! race rather than closing it.

use crate::db::DbPool;
use futures::future::BoxFuture;

/// The compiled-in sqlx driver.
#[cfg(feature = "postgres")]
pub type Db = sqlx::Postgres;

/// The compiled-in sqlx driver.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type Db = sqlx::Sqlite;

/// A transaction that already holds the backend's write lock.
pub type WriteTransaction = sqlx::Transaction<'static, Db>;

/// Read `waiting_room_enabled` under the write lock: `FOR UPDATE` locks the row
/// on PostgreSQL; on SQLite `BEGIN IMMEDIATE` already holds the write lock.
#[cfg(feature = "postgres")]
pub const SELECT_WAITING_ROOM_LOCKED: &str =
    "SELECT waiting_room_enabled FROM meetings WHERE id = $1 FOR UPDATE";

/// See the PostgreSQL variant.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub const SELECT_WAITING_ROOM_LOCKED: &str =
    "SELECT waiting_room_enabled FROM meetings WHERE id = $1";

/// Begin a write transaction, acquiring the write lock immediately.
#[cfg(feature = "postgres")]
pub async fn begin_write(pool: &DbPool) -> Result<WriteTransaction, sqlx::Error> {
    pool.begin().await
}

/// Begin a transaction intended to write, acquiring the write lock immediately.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn begin_write(pool: &DbPool) -> Result<WriteTransaction, sqlx::Error> {
    pool.begin_with("BEGIN IMMEDIATE").await
}

/// Await `op` once. PostgreSQL waits on its row locks, so there is nothing to
/// retry.
#[cfg(feature = "postgres")]
pub async fn with_write_retry<'a, T, F>(mut op: F) -> Result<T, sqlx::Error>
where
    F: FnMut() -> BoxFuture<'a, Result<T, sqlx::Error>>,
{
    op().await
}

/// Retry `op` while SQLite reports `SQLITE_BUSY` / `SQLITE_LOCKED`.
///
/// `op` must be replayable — it owns its transaction (rolled back on failure) or
/// is one autocommit statement. Bounded by [`RETRY_DEADLINE`] wall-clock, not an
/// attempt count: an attempt can itself burn `busy_timeout` (5s in production),
/// so counting attempts would hold an HTTP handler open long past the caller's
/// timeout. The deadline gates *starting* an attempt, not interrupting one, so
/// the worst case is `RETRY_DEADLINE + busy_timeout`.
///
/// Only contention is retried; [`sqlx::Error::PoolTimedOut`] passes through,
/// since retrying an exhausted pool only multiplies its acquire timeout.
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

/// Whether an error is SQLite refusing the write lock (`SQLITE_BUSY` / `LOCKED`).
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
fn is_lock_contention(err: &sqlx::Error) -> bool {
    const BUSY: i32 = 5;
    const LOCKED: i32 = 6;

    let sqlx::Error::Database(db_err) = err else {
        return false;
    };
    let Some(code) = db_err.code() else {
        return false;
    };
    // sqlx reports the extended result code; the primary code is the low byte
    // (e.g. 517 = SQLITE_BUSY_SNAPSHOT -> 5).
    matches!(code.parse::<i32>().map(|c| c & 0xff), Ok(BUSY | LOCKED))
}
