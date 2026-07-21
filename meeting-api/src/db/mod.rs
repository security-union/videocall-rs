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
//! disagree about is funnelled through three small shims so that there is
//! exactly one copy of every statement:
//!
//! - [`q`] rewrites `$N` placeholders to `?N` for SQLite.
//! - [`now_expr`] / [`bind_now`] supply "now": the server's clock on
//!   PostgreSQL, a bound parameter on SQLite.
//! - [`lock`] provides the write-transaction primitives (`FOR UPDATE` on
//!   PostgreSQL, `BEGIN IMMEDIATE` + busy retry on SQLite).
//!
//! # Where "now" comes from
//!
//! **PostgreSQL keeps using its own clock**, exactly as it did before SQLite
//! was supported: every timestamp is written by `NOW()` / `CURRENT_TIMESTAMP`
//! evaluated server-side. This is not incidental. Binding a client timestamp
//! instead would key `list_by_owner`'s `ORDER BY created_at DESC` on API-pod
//! wall clock, so two replicas with NTP skew — or one backward NTP step — could
//! invert meetings created seconds apart. It would also let a row end up with
//! `updated_at < created_at`, since `updated_at` comes from a `BEFORE UPDATE`
//! trigger on the server while `created_at` would not.
//!
//! SQLite has no equivalent, so there [`now_expr`] emits a placeholder and
//! [`bind_now`] binds `chrono::Utc::now()`. That is safe for the ordering
//! concern because a SQLite deployment is a single file written by a single
//! process — there is no second clock to skew against.
//!
//! Bound `DateTime<Utc>` values serialize as RFC 3339, and the SQLite schema's
//! `DEFAULT`s are written to match, so a single column never mixes formats:
//! `datetime('now')` would render `2026-07-21 04:39:04` (space separator, no
//! sub-second, no offset), which does not sort lexicographically against
//! RFC 3339 and would corrupt `ORDER BY created_at DESC`.
//!
//! The one exception is `users.created_at` / `users.last_login`, the only
//! `TIMESTAMP`-without-time-zone columns in the schema. Those use
//! [`now_naive_expr`] / [`bind_now_naive`] and store a *naive* datetime
//! (`2026-07-21 09:06:06.635`), a second and deliberately different format.
//! Binding an aware `DateTime<Utc>` there would send a `TIMESTAMPTZ` that
//! PostgreSQL converts using the session time zone. Neither column is read
//! back or ordered on, so the format difference is inert.

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
/// - `synchronous(Normal)` is the standard WAL pairing: fsync at checkpoint
///   rather than at every commit. Under WAL this is still crash-safe — a power
///   loss can lose the most recent transactions but cannot corrupt the database.
/// - `busy_timeout` makes SQLite wait rather than immediately returning
///   `SQLITE_BUSY`; [`lock::with_write_retry`] is the backstop beyond it.
/// - `foreign_keys(true)` is mandatory. SQLite ignores foreign keys unless the
///   pragma is on, which would silently disable the `ON DELETE CASCADE` from
///   `meeting_participants` to `meetings`.
///
/// `create_if_missing` is deliberately **not** forced on. It is left to the URL
/// (`?mode=rwc`), so a typo in `DATABASE_URL` fails loudly at startup instead of
/// silently creating an empty, unmigrated database that then answers every
/// request with "no such table". Tests opt in explicitly.
///
/// `max_connections(5)` is safe despite SQLite's single-writer model: WAL keeps
/// readers off the write lock, and every write transaction starts with
/// `BEGIN IMMEDIATE` (see [`lock`]) so writers queue on the write lock instead
/// of deadlocking on a mid-transaction upgrade.
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

/// SQL expression producing the current `TIMESTAMPTZ`, for parameter slot `slot`.
///
/// PostgreSQL evaluates this server-side, so `slot` is unused and no
/// corresponding bind exists — see the module docs for why the database, not
/// the API pod, is the clock here.
#[cfg(feature = "postgres")]
#[inline]
pub fn now_expr(_slot: usize) -> Cow<'static, str> {
    Cow::Borrowed("NOW()")
}

/// SQL expression producing the current `TIMESTAMPTZ`, for parameter slot `slot`.
///
/// SQLite has no server clock to borrow, so this is a placeholder that
/// [`bind_now`] fills in. `slot` must be the 1-based position of the bind that
/// [`bind_now`] appends, i.e. one past the statement's other parameters. Repeat
/// the same `now_expr` value to reuse one bind across several columns.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
#[inline]
pub fn now_expr(slot: usize) -> Cow<'static, str> {
    Cow::Owned(format!("${slot}"))
}

/// SQL expression producing the current naive `TIMESTAMP`, for parameter `slot`.
///
/// Only `users.created_at` / `users.last_login` need this; see the module docs.
#[cfg(feature = "postgres")]
#[inline]
pub fn now_naive_expr(_slot: usize) -> Cow<'static, str> {
    Cow::Borrowed("CURRENT_TIMESTAMP")
}

/// SQL expression producing the current naive `TIMESTAMP`, for parameter `slot`.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
#[inline]
pub fn now_naive_expr(slot: usize) -> Cow<'static, str> {
    Cow::Owned(format!("${slot}"))
}

/// Bind the value that [`now_expr`] reserved a placeholder for.
///
/// Expands to nothing on PostgreSQL, where `NOW()` needs no parameter, and to
/// `.bind(Utc::now())` on SQLite. Always call it *after* the statement's other
/// `.bind`s so the appended parameter lands in the slot `now_expr` was given.
macro_rules! bind_now {
    ($query:expr) => {{
        #[cfg(feature = "postgres")]
        let query = $query;
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        let query = $query.bind(::chrono::Utc::now());
        query
    }};
}

/// Bind the value that [`now_naive_expr`] reserved a placeholder for.
macro_rules! bind_now_naive {
    ($query:expr) => {{
        #[cfg(feature = "postgres")]
        let query = $query;
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        let query = $query.bind(::chrono::Utc::now().naive_utc());
        query
    }};
}

pub(crate) use bind_now;
pub(crate) use bind_now_naive;

/// Adapt a PostgreSQL-flavoured statement to the compiled-in dialect.
///
/// On PostgreSQL this is the identity function and borrows the input.
#[cfg(feature = "postgres")]
#[inline]
pub fn q(sql: &str) -> Cow<'_, str> {
    Cow::Borrowed(sql)
}

/// Adapt a PostgreSQL-flavoured statement to the compiled-in dialect: on SQLite,
/// rewrite `$N` placeholders to `?N`.
///
/// **This rewrite is defence in depth, not a requirement.** SQLite accepts `$1`
/// as a parameter name in its own right, and sqlx-sqlite's binder explicitly
/// parses the `$NNN` form (`sqlx-sqlite/src/arguments.rs`), so the statements
/// would bind correctly even if this function were the identity. It exists
/// because `?N` is SQLite's documented numbered-parameter syntax and relying on
/// an sqlx implementation detail across upgrades is a bet with no upside. If
/// this function is ever removed, the queries are expected to keep working —
/// verify that rather than assuming it, but do not treat `q` as load-bearing.
///
/// `$N -> ?N` preserves the numbering, so bind order is unchanged and a
/// statement may reference the same parameter more than once.
///
/// The scanner skips anywhere a `$` could legitimately appear verbatim: single-
/// quoted literals, `"double-quoted identifiers"`, `-- line comments` and
/// `/* block comments */`. Doubled quotes (`''`, `""`) close and immediately
/// reopen, which lands on the correct state either way. A `$` not followed by a
/// digit — PostgreSQL dollar-quoting, `$$` — is left alone.
#[cfg(all(feature = "sqlite", not(feature = "postgres")))]
pub fn q(sql: &str) -> Cow<'_, str> {
    #[derive(PartialEq)]
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

    #[test]
    fn never_rewrites_inside_quoted_identifiers_or_comments() {
        let sql = concat!(
            "SELECT \"weird$1col\" -- trailing $1 note\n",
            "FROM meetings /* block $1 note */ WHERE id = $1"
        );
        let expected = if cfg!(feature = "postgres") {
            sql.to_string()
        } else {
            sql.replace("WHERE id = $1", "WHERE id = ?1")
        };
        assert_eq!(q(sql), expected);
    }

    /// A doubled quote is an escape, not a close, so the scanner must still be
    /// inside the literal afterwards — otherwise the `$1` here gets rewritten.
    #[test]
    fn handles_doubled_quote_escapes() {
        let sql = "SELECT 'it''s $1 fine', \"a\"\"b$1\" FROM meetings WHERE id = $1";
        let expected = if cfg!(feature = "postgres") {
            sql.to_string()
        } else {
            sql.replace("WHERE id = $1", "WHERE id = ?1")
        };
        assert_eq!(q(sql), expected);
    }

    /// The whole point of [`super::now_expr`]: PostgreSQL uses its own clock and
    /// consumes no parameter slot, SQLite gets a placeholder for [`super::bind_now`].
    #[test]
    fn now_expr_matches_the_dialects_clock_strategy() {
        if cfg!(feature = "postgres") {
            assert_eq!(super::now_expr(6), "NOW()");
            assert_eq!(super::now_naive_expr(5), "CURRENT_TIMESTAMP");
        } else {
            assert_eq!(super::now_expr(6), "$6");
            assert_eq!(super::now_naive_expr(5), "$5");
        }
    }
}
