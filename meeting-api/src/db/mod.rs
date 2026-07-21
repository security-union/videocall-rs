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
//! The backend is selected at compile time by a Cargo feature:
//!
//! - `postgres` (default) — [`DbPool`] is [`sqlx::PgPool`]
//! - `sqlite` — [`DbPool`] is [`sqlx::SqlitePool`]
//!
//! The query modules themselves are shared. Everything the two dialects
//! disagree about is funnelled through two small shims so that there is exactly
//! one copy of every statement:
//!
//! - [`q`] rewrites `$N` placeholders to `?N` for SQLite. Both dialects accept
//!   *numbered* parameters, so bind order is identical and callers never change.
//! - [`lock`] provides the write-transaction primitives (`FOR UPDATE` on
//!   PostgreSQL, `BEGIN IMMEDIATE` + busy retry on SQLite).
//!
//! Timestamps are always bound as `chrono::Utc::now()` parameters rather than
//! written as `NOW()` / `CURRENT_TIMESTAMP`. Besides removing dialect
//! divergence this keeps every timestamp SQLite stores in one lexicographically
//! sortable format: `datetime('now')` renders `2026-07-21 04:39:04` (space
//! separator, no sub-second, no offset) while a bound `DateTime<Utc>` renders
//! RFC 3339. Mixing both in a TEXT column makes `ORDER BY created_at DESC`
//! return rows out of order.

use std::borrow::Cow;

pub mod lock;
pub mod meetings;
pub mod oauth;
pub mod participants;

#[cfg(all(feature = "postgres", feature = "sqlite"))]
compile_error!(
    "meeting-api: the `postgres` and `sqlite` features are mutually exclusive. \
     Build with `--no-default-features --features sqlite` to select SQLite."
);

#[cfg(not(any(feature = "postgres", feature = "sqlite")))]
compile_error!(
    "meeting-api: exactly one database backend feature must be enabled \
     (`postgres` or `sqlite`)."
);

/// Connection pool for the compiled-in backend.
#[cfg(feature = "postgres")]
pub type DbPool = sqlx::PgPool;

/// Connection pool for the compiled-in backend.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub type DbPool = sqlx::SqlitePool;

/// Connect to PostgreSQL.
#[cfg(feature = "postgres")]
pub async fn connect(database_url: &str) -> DbPool {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(database_url)
        .await
        .expect("failed to connect to PostgreSQL");

    tracing::info!("Connected to PostgreSQL");
    pool
}

/// Connect to SQLite.
///
/// Everything here is set through `SqliteConnectOptions` rather than by issuing
/// `PRAGMA` statements against the pool: `busy_timeout`, `foreign_keys` and
/// `journal_mode` are *per connection*, so a `PRAGMA` executed on one pooled
/// connection leaves every other connection on the defaults.
///
/// - `Wal` lets readers proceed while a writer holds the write lock.
/// - `busy_timeout` makes SQLite wait rather than immediately returning
///   `SQLITE_BUSY`; [`lock::with_write_retry`] is the backstop beyond it.
/// - `foreign_keys(true)` is mandatory. SQLite ignores foreign keys unless the
///   pragma is on, which would silently disable the `ON DELETE CASCADE` from
///   `meeting_participants` to `meetings`.
///
/// `max_connections(5)` is safe despite SQLite's single-writer model: WAL keeps
/// readers off the write lock, and every write transaction starts with
/// `BEGIN IMMEDIATE` (see [`lock`]) so writers queue on the write lock instead
/// of deadlocking on a mid-transaction upgrade.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn connect(database_url: &str) -> DbPool {
    use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
    use std::str::FromStr;
    use std::time::Duration;

    let options = SqliteConnectOptions::from_str(database_url)
        .expect("invalid SQLite DATABASE_URL")
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true)
        .create_if_missing(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("failed to connect to SQLite");

    tracing::info!("Connected to SQLite");
    pool
}

/// Adapt a PostgreSQL-flavoured statement to the compiled-in dialect.
///
/// On PostgreSQL this is the identity function and borrows the input.
#[cfg(feature = "postgres")]
#[inline]
pub fn q(sql: &str) -> Cow<'_, str> {
    Cow::Borrowed(sql)
}

/// Adapt a PostgreSQL-flavoured statement to the compiled-in dialect.
///
/// On SQLite this rewrites `$N` placeholders to `?N`. SQLite supports numbered
/// parameters natively, so `$1 -> ?1` preserves both the numbering and the bind
/// order, including statements that reference the same parameter twice.
///
/// Single-quoted string literals are skipped so a literal `$` inside a value is
/// never touched. (No current query contains one; the scanner keeps that from
/// becoming a silent trap if one is added.)
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn q(sql: &str) -> Cow<'_, str> {
    if !sql.contains('$') {
        return Cow::Borrowed(sql);
    }

    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string_literal = false;

    while let Some(c) = chars.next() {
        match c {
            // A doubled quote inside a literal closes and immediately reopens
            // it, which leaves `in_string_literal` correct either way.
            '\'' => {
                in_string_literal = !in_string_literal;
                out.push(c);
            }
            '$' if !in_string_literal && chars.peek().is_some_and(char::is_ascii_digit) => {
                out.push('?');
            }
            _ => out.push(c),
        }
    }

    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::q;

    #[test]
    fn leaves_statements_without_placeholders_borrowed() {
        let sql = "SELECT COUNT(*) FROM meetings";
        assert!(matches!(q(sql), std::borrow::Cow::Borrowed(_)));
        assert_eq!(q(sql), sql);
    }

    #[test]
    fn rewrites_placeholders_for_the_active_dialect() {
        let sql = "SELECT 1 FROM meetings WHERE room_id = $1 AND creator_id = $2";
        if cfg!(feature = "postgres") {
            assert_eq!(q(sql), sql);
        } else {
            assert_eq!(
                q(sql),
                "SELECT 1 FROM meetings WHERE room_id = ?1 AND creator_id = ?2"
            );
        }
    }

    #[test]
    fn never_rewrites_inside_string_literals() {
        let sql = "UPDATE meetings SET state = 'a$1b' WHERE id = $1";
        if cfg!(feature = "postgres") {
            assert_eq!(q(sql), sql);
        } else {
            assert_eq!(q(sql), "UPDATE meetings SET state = 'a$1b' WHERE id = ?1");
        }
    }

    /// Every query in this crate is written with `$N` placeholders. A bare `$`
    /// (PostgreSQL dollar-quoting, `$$`) would be silently mangled, so assert
    /// the scanner only reacts to `$` followed by a digit.
    #[test]
    fn ignores_dollars_not_followed_by_a_digit() {
        let sql = "SELECT '$' , $$tag$$ FROM meetings";
        assert_eq!(q(sql), sql);
    }
}
