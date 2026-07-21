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

//! Database queries, shared across both backends. The active backend is a
//! compile-time Cargo feature: `postgres` (default) or `sqlite`. Three shims
//! absorb the dialect differences so each statement is written once:
//!
//! - [`q`] rewrites `$N` placeholders to `?N` on SQLite.
//! - [`now_sql`] + [`bind_now`] supply the current timestamp.
//! - [`lock`] provides the write-transaction primitives.
//!
//! # Clock source
//!
//! Timestamps use the *database* clock on PostgreSQL (`NOW()`) and a bound
//! `Utc::now()` on SQLite. The split is deliberate: a client clock would key
//! `list_by_owner`'s `ORDER BY created_at DESC` on per-pod wall time — NTP skew
//! could invert rows — and could yield `updated_at < created_at` against the
//! `BEFORE UPDATE` trigger. SQLite is a single-writer file with no second clock,
//! so binding is safe there; its schema DEFAULTs also emit RFC 3339 so a TEXT
//! column never mixes sort-incompatible formats.
//!
//! `users.created_at` / `last_login` are the schema's only `TIMESTAMP` (no time
//! zone) columns and use [`now_naive_sql`] / [`bind_now_naive`] so PostgreSQL
//! applies no session-timezone cast. Neither is ordered on.

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
/// Options go on `SqliteConnectOptions`, not `PRAGMA` on the pool: pragmas are
/// per-connection, so setting them once would leave the other pooled connections
/// on SQLite's defaults.
///
/// - `foreign_keys(true)` is required, or `ON DELETE CASCADE` is silently inert.
/// - `Wal` + `synchronous(Normal)` let readers proceed past the writer and fsync
///   at checkpoint rather than every commit; still crash-safe under WAL.
/// - `busy_timeout` waits instead of returning `SQLITE_BUSY`;
///   [`lock::with_write_retry`] is the backstop past it.
/// - `create_if_missing` is left to the URL (`?mode=rwc`) so a bad `DATABASE_URL`
///   fails at startup instead of serving an empty, unmigrated database.
///
/// `max_connections(5)` is safe: WAL keeps readers off the write lock and every
/// writer takes it up front via `BEGIN IMMEDIATE` (see [`lock`]).
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub async fn connect(database_url: &str) -> DbPool {
    use sqlx::sqlite::{
        SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
    };
    use std::str::FromStr;
    use std::time::Duration;

    let options = SqliteConnectOptions::from_str(database_url)
        .expect("invalid SQLite DATABASE_URL")
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await
        .expect("failed to connect to SQLite");

    tracing::info!("Connected to SQLite");
    pool
}

/// Render `template` for the active dialect: replace each `{now}` with the
/// current timestamp and rewrite placeholders.
///
/// On SQLite `{now}` becomes a `$slot` placeholder that [`bind_now`] fills, so
/// `slot` must be one past the statement's other parameters — the position
/// `bind_now` binds into. Reuse the same slot to write several columns from one
/// bind. On PostgreSQL `{now}` is `NOW()` and `slot` is unused.
#[cfg(feature = "postgres")]
pub(crate) fn now_sql(template: &str, _slot: usize) -> String {
    template.replace("{now}", "NOW()")
}

/// Render `template` for the active dialect. See the PostgreSQL variant.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub(crate) fn now_sql(template: &str, slot: usize) -> String {
    q(&template.replace("{now}", &format!("${slot}"))).into_owned()
}

/// Like [`now_sql`], but `{now}` is a naive `TIMESTAMP` — only for the two
/// `users` columns (see the module docs).
#[cfg(feature = "postgres")]
pub(crate) fn now_naive_sql(template: &str, _slot: usize) -> String {
    template.replace("{now}", "CURRENT_TIMESTAMP")
}

/// Like [`now_sql`], but `{now}` is a naive `TIMESTAMP`.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub(crate) fn now_naive_sql(template: &str, slot: usize) -> String {
    q(&template.replace("{now}", &format!("${slot}"))).into_owned()
}

/// Append the bind that [`now_sql`]'s `{now}` reserved: nothing on PostgreSQL,
/// `Utc::now()` on SQLite. Call it after the statement's other `.bind`s.
macro_rules! bind_now {
    ($query:expr) => {{
        #[cfg(feature = "postgres")]
        let query = $query;
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        let query = $query.bind(::chrono::Utc::now());
        query
    }};
}

/// [`bind_now`] for [`now_naive_sql`]: binds a `NaiveDateTime` on SQLite.
macro_rules! bind_now_naive {
    ($query:expr) => {{
        #[cfg(feature = "postgres")]
        let query = $query;
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        let query = $query.bind(::chrono::Utc::now().naive_utc());
        query
    }};
}

/// Run an async block through [`lock::with_write_retry`], so on SQLite it replays
/// past `SQLITE_BUSY`. The body must be replayable: it either owns its
/// transaction (rolled back on failure) or is one autocommit statement. Captures
/// are moved into a fresh future each attempt, so they must be `Copy`.
macro_rules! with_retry {
    ($($body:tt)*) => {
        $crate::db::lock::with_write_retry(|| ::std::boxed::Box::pin(async move { $($body)* })).await
    };
}

pub(crate) use bind_now;
pub(crate) use bind_now_naive;
pub(crate) use with_retry;

/// Adapt a PostgreSQL-flavoured statement to the compiled-in dialect. Identity on
/// PostgreSQL.
#[cfg(feature = "postgres")]
#[inline]
pub fn q(sql: &str) -> Cow<'_, str> {
    Cow::Borrowed(sql)
}

/// Rewrite `$N` placeholders to `?N` for SQLite.
///
/// Defence in depth, not required: sqlx-sqlite binds `$N` natively, so removing
/// this leaves queries working. `?N` is SQLite's documented form and avoids
/// depending on that internal.
///
/// Skips `$` inside single-/double-quoted strings and `--` / `/* */` comments so
/// only real placeholders are touched. Doubled quotes (`''`, `""`) escape, and a
/// `$` not followed by a digit (e.g. `$$`) is left alone.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn q(sql: &str) -> Cow<'_, str> {
    enum Mode {
        Sql,
        SingleQuoted,
        DoubleQuoted,
        LineComment,
        BlockComment,
    }

    if !sql.contains('$') {
        return Cow::Borrowed(sql);
    }

    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut mode = Mode::Sql;

    while let Some(c) = chars.next() {
        match mode {
            Mode::Sql => match c {
                '\'' => {
                    mode = Mode::SingleQuoted;
                    out.push(c);
                }
                '"' => {
                    mode = Mode::DoubleQuoted;
                    out.push(c);
                }
                '-' if chars.peek() == Some(&'-') => {
                    mode = Mode::LineComment;
                    out.push(c);
                    out.push(chars.next().expect("peeked"));
                }
                '/' if chars.peek() == Some(&'*') => {
                    mode = Mode::BlockComment;
                    out.push(c);
                    out.push(chars.next().expect("peeked"));
                }
                '$' if chars.peek().is_some_and(char::is_ascii_digit) => out.push('?'),
                _ => out.push(c),
            },
            Mode::SingleQuoted => {
                out.push(c);
                if c == '\'' {
                    mode = Mode::Sql;
                }
            }
            Mode::DoubleQuoted => {
                out.push(c);
                if c == '"' {
                    mode = Mode::Sql;
                }
            }
            Mode::LineComment => {
                out.push(c);
                if c == '\n' {
                    mode = Mode::Sql;
                }
            }
            Mode::BlockComment => {
                out.push(c);
                if c == '*' && chars.peek() == Some(&'/') {
                    out.push(chars.next().expect("peeked"));
                    mode = Mode::Sql;
                }
            }
        }
    }

    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::{now_naive_sql, now_sql, q};

    #[test]
    fn q_borrows_when_there_is_nothing_to_rewrite() {
        assert!(matches!(
            q("SELECT 1 FROM meetings"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    /// Real placeholders become `?N` on SQLite; a `$` inside a string literal,
    /// quoted identifier, comment, or `$$` is left alone.
    #[test]
    fn q_rewrites_only_real_placeholders() {
        let sql = concat!(
            "SELECT \"weird$1col\", 'a$1b' -- $1\n",
            "FROM meetings /* $1 */ WHERE id = $1 AND z = $$tag$$"
        );
        let expected = if cfg!(feature = "postgres") {
            sql.to_string()
        } else {
            sql.replace("id = $1", "id = ?1")
        };
        assert_eq!(q(sql), expected);
    }

    /// A doubled quote escapes rather than closes, so the `$1` inside stays put.
    #[test]
    fn q_treats_doubled_quotes_as_escapes() {
        let sql = "SELECT 'it''s $1', \"a\"\"b$1\" FROM meetings WHERE id = $1";
        let expected = if cfg!(feature = "postgres") {
            sql.to_string()
        } else {
            sql.replace("id = $1", "id = ?1")
        };
        assert_eq!(q(sql), expected);
    }

    #[test]
    fn now_sql_renders_the_dialects_clock() {
        if cfg!(feature = "postgres") {
            assert_eq!(
                now_sql("t = {now} WHERE id = $1", 2),
                "t = NOW() WHERE id = $1"
            );
            assert_eq!(now_naive_sql("t = {now}", 1), "t = CURRENT_TIMESTAMP");
        } else {
            assert_eq!(
                now_sql("t = {now} WHERE id = $1", 2),
                "t = ?2 WHERE id = ?1"
            );
            assert_eq!(now_naive_sql("t = {now}", 1), "t = ?1");
        }
    }
}
