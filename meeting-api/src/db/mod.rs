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

//! Database query modules.
//!
//! The active backend is selected at compile time via Cargo feature flags:
//! - `postgres` (default) — uses `sqlx::PgPool`
//! - `sqlite` — uses `sqlx::SqlitePool`
//!
//! The two backends share a single set of query modules. sqlx's SQLite driver
//! parses `$N` placeholders natively, so the same query strings run verbatim on
//! both backends; the only backend-specific pieces are the small helpers below
//! (the SQL "now" expression, the row-lock clause, and the write-transaction /
//! busy-retry strategy).

#[cfg(all(feature = "postgres", feature = "sqlite"))]
compile_error!(
    "features `postgres` and `sqlite` are mutually exclusive; enable exactly one \
     (use --no-default-features --features sqlite for SQLite)"
);

#[cfg(not(any(feature = "postgres", feature = "sqlite")))]
compile_error!("enable exactly one database backend feature: `postgres` or `sqlite`");

pub mod meetings;
pub mod oauth;
pub mod participants;

// ---- Pool / backend type aliases -------------------------------------------
// Routes and state reference `db::DbPool` so they stay backend-agnostic.
// `Db` is the sqlx database type, used for transaction generics in helpers.
//
// The sqlite arms use `all(feature = "sqlite", not(feature = "postgres"))` so
// that a (misconfigured) both-features build reports ONLY the mutual-exclusion
// compile_error above, not a cascade of duplicate-definition errors.

#[cfg(feature = "postgres")]
pub type DbPool = sqlx::PgPool;
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type DbPool = sqlx::SqlitePool;

#[cfg(feature = "postgres")]
pub(crate) type Db = sqlx::Postgres;
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub(crate) type Db = sqlx::Sqlite;

/// SQL expression yielding the current timestamp.
///
/// On SQLite this is an RFC3339-with-milliseconds string so values sort
/// consistently with sqlx-bound `DateTime<Utc>` (which encodes via `to_rfc3339`
/// on SQLite, e.g. `2026-07-21T12:34:56.789+00:00`).
#[cfg(feature = "postgres")]
pub(crate) const SQL_NOW: &str = "NOW()";
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub(crate) const SQL_NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00','now')";

/// Row-lock clause appended to the meeting-row SELECT that serializes joins
/// against waiting-room toggles.
///
/// Empty on SQLite because the `BEGIN IMMEDIATE` transaction already holds the
/// database write lock, so no explicit row lock is needed (nor supported).
#[cfg(feature = "postgres")]
pub(crate) const FOR_UPDATE: &str = " FOR UPDATE";
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub(crate) const FOR_UPDATE: &str = "";

/// Begin a write transaction.
///
/// On SQLite this issues `BEGIN IMMEDIATE` so the write lock is acquired up
/// front (a plain deferred `BEGIN` would only take the lock on first write,
/// racing a concurrent toggle). On PostgreSQL a normal `BEGIN` is used and
/// serialization is provided by explicit `FOR UPDATE` row locks.
pub(crate) async fn begin_write_tx(
    pool: &DbPool,
) -> Result<sqlx::Transaction<'static, Db>, sqlx::Error> {
    // `Pool::begin_with` exists for every Pool in sqlx 0.8, so a single fn with
    // a runtime `cfg!` branch keeps this backend-agnostic.
    if cfg!(feature = "sqlite") {
        pool.begin_with("BEGIN IMMEDIATE").await
    } else {
        pool.begin().await
    }
}

/// Run a transactional closure, retrying up to 3 times on `SQLITE_BUSY`.
///
/// `busy_timeout` does not cover `SQLITE_BUSY_SNAPSHOT` (which occurs when a
/// deferred/read snapshot upgrades to a write inside a multi-statement
/// transaction and conflicts under WAL), so we retry the whole closure. A
/// dropped `Transaction` rolls back, so each retry re-reads fresh state. On
/// PostgreSQL the busy check is a compile-time constant `false`, so errors
/// pass straight through without retrying.
///
/// Only the two multi-statement transactional functions
/// (`participants::join_attendee` and `meetings::update_waiting_room_enabled`)
/// are wrapped intentionally — those are the only ones that read then write
/// within a single transaction and can hit `SQLITE_BUSY_SNAPSHOT`.
/// Single-statement writes (`admit`, `admit_all`, `leave`, `create`, etc.)
/// cannot produce a snapshot upgrade and are already covered by the 5s
/// `busy_timeout`, so wrapping them would add no value.
pub(crate) async fn with_busy_retry<T, F, Fut>(op: F) -> Result<T, sqlx::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if attempt < 3 && is_sqlite_busy(&err) => {
                tokio::time::sleep(std::time::Duration::from_millis(50 * attempt as u64)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Returns `true` when a sqlx error is a SQLite "busy" error.
///
/// `DatabaseError::code()` on SQLite returns the extended result code as a
/// decimal string; all busy variants share the primary code 5
/// (SQLITE_BUSY=5, SQLITE_BUSY_RECOVERY=261, SQLITE_BUSY_SNAPSHOT=517), so we
/// mask with `& 0xFF`. Always `false` on PostgreSQL builds.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
fn is_sqlite_busy(err: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db) = err {
        if let Some(code) = db.code() {
            if let Ok(code) = code.parse::<i32>() {
                return code & 0xFF == 5;
            }
        }
    }
    false
}

#[cfg(feature = "postgres")]
fn is_sqlite_busy(_err: &sqlx::Error) -> bool {
    false
}

// ---- Shared row types ------------------------------------------------------
// These derive `sqlx::FromRow` and work identically for both backends.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

/// Row returned from the `meetings` table.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct MeetingRow {
    pub id: i32,
    pub room_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub creator_id: Option<String>,
    pub password_hash: Option<String>,
    pub state: Option<String>,
    pub attendees: Option<JsonValue>,
    pub host_display_name: Option<String>,
    pub waiting_room_enabled: bool,
}

/// Row returned from the `meeting_participants` table.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct ParticipantRow {
    pub id: i32,
    pub meeting_id: i32,
    pub user_id: String,
    pub status: String,
    pub is_host: bool,
    pub is_required: bool,
    pub joined_at: DateTime<Utc>,
    pub admitted_at: Option<DateTime<Utc>>,
    pub left_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub display_name: Option<String>,
}

/// Stored PKCE challenge/verifier and CSRF state for an in-flight OAuth flow.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct OAuthRequestRow {
    pub pkce_challenge: Option<String>,
    pub pkce_verifier: Option<String>,
    pub csrf_state: Option<String>,
    pub return_to: Option<String>,
    pub nonce: Option<String>,
}

// ---- Conversions to API response types -------------------------------------

impl ParticipantRow {
    /// Convert a database row into the API response type.
    /// Optionally attach a `room_token` (only for the participant themselves).
    pub fn into_participant_status(
        self,
        room_token: Option<String>,
    ) -> videocall_meeting_types::responses::ParticipantStatusResponse {
        videocall_meeting_types::responses::ParticipantStatusResponse {
            user_id: self.user_id,
            display_name: self.display_name,
            status: self.status,
            is_host: self.is_host,
            joined_at: self.joined_at.timestamp(),
            admitted_at: self.admitted_at.map(|t| t.timestamp()),
            room_token,
            observer_token: None,
            waiting_room_enabled: None,
            host_display_name: None,
            host_user_id: None,
        }
    }
}
