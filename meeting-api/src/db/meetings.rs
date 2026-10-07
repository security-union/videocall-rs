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

//! Meeting table queries.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;
use sqlx::{PgConnection, PgPool};

use crate::password::PasswordUpdate;

/// The terminal `state` value. A meeting that has ended stays ended forever
/// (until soft-deleted) — presence can never resurrect it.
pub const STATE_ENDED: &str = "ended";
/// The "someone is currently present" state.
pub const STATE_ACTIVE: &str = "active";
/// The "exists but nobody is currently present" state.
pub const STATE_IDLE: &str = "idle";

/// Derive the displayed meeting state: `ended` wins if raw state is `ended`, else `active` iff `participant_count > 0`, else `idle`.
pub fn display_state(raw_state: Option<&str>, participant_count: i64) -> String {
    match raw_state {
        Some(STATE_ENDED) => STATE_ENDED.to_string(),
        _ if participant_count > 0 => STATE_ACTIVE.to_string(),
        _ => STATE_IDLE.to_string(),
    }
}

/// Row returned from the `meetings` table.
#[derive(Debug, Clone, sqlx::FromRow)]
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
    pub admitted_can_admit: bool,
    pub end_on_host_leave: bool,
    pub allow_guests: bool,
    /// Whether the record button is shown to all admitted participants (not
    /// just the host).  Defaults to `false`: only the host sees the record
    /// button unless explicitly opened up.  `POST .../recordings` refuses a
    /// non-host lease while it is off; capture itself stays client-side.
    pub recording_allowed_for_all: bool,
    /// Whether every admitted participant may SEND chat messages (not just the
    /// host/co-hosts).  Defaults to `true`, so normal meetings keep chat open
    /// for everyone; a host turns it off for an all-hands-style meeting so only
    /// hosts can post, and can flip it back on live.  This is a client
    /// UI-visibility gate on the send affordance, not a server-side
    /// access-control enforcement.
    pub chat_allowed_for_all: bool,
}

/// Create a new meeting. Uses INSERT ... ON CONFLICT to handle the partial unique index.
pub async fn create(
    pool: &PgPool,
    room_id: &str,
    creator_id: &str,
    password_hash: Option<&str>,
    attendees: &JsonValue,
) -> Result<MeetingRow, sqlx::Error> {
    create_with_options(
        pool,
        room_id,
        creator_id,
        password_hash,
        attendees,
        true,
        false,
        true,
        false,
        false,
        true,
    )
    .await
}

/// Create a new meeting with explicit waiting_room_enabled setting.
#[allow(clippy::too_many_arguments)]
pub async fn create_with_options<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    room_id: &str,
    creator_id: &str,
    password_hash: Option<&str>,
    attendees: &JsonValue,
    waiting_room_enabled: bool,
    admitted_can_admit: bool,
    end_on_host_leave: bool,
    allow_guests: bool,
    recording_allowed_for_all: bool,
    chat_allowed_for_all: bool,
) -> Result<MeetingRow, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        INSERT INTO meetings (room_id, creator_id, started_at, password_hash, state, attendees, waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests, recording_allowed_for_all, chat_allowed_for_all)
        VALUES ($1, $2, NOW(), $3, 'idle', $4, $5, $6, $7, $8, $9, $10)
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests, recording_allowed_for_all, chat_allowed_for_all
        "#,
    )
    .bind(room_id)
    .bind(creator_id)
    .bind(password_hash)
    .bind(attendees)
    .bind(waiting_room_enabled)
    .bind(admitted_can_admit)
    .bind(end_on_host_leave)
    .bind(allow_guests)
    .bind(recording_allowed_for_all)
    .bind(chat_allowed_for_all)
    .fetch_one(executor)
    .await
}

/// Get a non-deleted meeting by room_id.
pub async fn get_by_room_id(
    pool: &PgPool,
    room_id: &str,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        SELECT id, room_id, started_at, ended_at, created_at, updated_at,
               deleted_at, creator_id, password_hash, state, attendees, host_display_name,
               waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests, recording_allowed_for_all, chat_allowed_for_all
        FROM meetings
        WHERE room_id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(room_id)
    .fetch_optional(pool)
    .await
}

/// List meetings the user owns, has participated in, or is a live co-host of
/// (non-deleted), ordered by created_at DESC. The co-host branch depends on
/// `meeting_co_hosts_user_id_idx` (see that migration) to stay an index
/// lookup rather than a full-table scan. [`search_by_owner`] shares this
/// query shape and the same index.
pub async fn list_by_owner(
    pool: &PgPool,
    creator_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        SELECT DISTINCT m.id, m.room_id, m.started_at, m.ended_at, m.created_at, m.updated_at,
               m.deleted_at, m.creator_id, m.password_hash, m.state, m.attendees, m.host_display_name,
               m.waiting_room_enabled, m.admitted_can_admit, m.end_on_host_leave, m.allow_guests, m.recording_allowed_for_all, m.chat_allowed_for_all
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $1
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.user_id IS NOT NULL
               OR EXISTS (SELECT 1 FROM meeting_co_hosts c
                          WHERE c.meeting_id = m.id AND c.user_id = LOWER($1) AND NOT c.suspended))
        ORDER BY m.created_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(creator_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Count meetings the user owns, has participated in, or is a live co-host
/// of (non-deleted). See [`list_by_owner`].
pub async fn count_by_owner(pool: &PgPool, creator_id: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(DISTINCT m.id)
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $1
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.user_id IS NOT NULL
               OR EXISTS (SELECT 1 FROM meeting_co_hosts c
                          WHERE c.meeting_id = m.id AND c.user_id = LOWER($1) AND NOT c.suspended))
        "#,
    )
    .bind(creator_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Escape the LIKE-special characters `%`, `_`, and `\` in user-supplied
/// search input so they're treated as literals inside the `ILIKE` pattern.
///
/// Without this, a query of `%` would match everything, and `_` would match
/// any single character — either giving callers access to rows they haven't
/// searched for (low-severity info disclosure when combined with the
/// participant JOIN's ACL predicate) and producing confusing result sets.
/// The default Postgres escape character is `\`, so we double-escape
/// backslashes before the metacharacter escapes so literal backslashes in
/// user input survive untouched.
fn escape_like(input: &str) -> String {
    input
        .replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}

/// Search non-deleted meetings the user owns, has participated in, or is a
/// live co-host of (issue #2702 round 11 — see [`list_by_owner`]), matching
/// a keyword against `room_id`, `state`, and `host_display_name`
/// (case-insensitive).
pub async fn search_by_owner(
    pool: &PgPool,
    creator_id: &str,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MeetingRow>, sqlx::Error> {
    let pattern = format!("%{}%", escape_like(q));
    sqlx::query_as::<_, MeetingRow>(
        r#"
        SELECT DISTINCT m.id, m.room_id, m.started_at, m.ended_at, m.created_at, m.updated_at,
               m.deleted_at, m.creator_id, m.password_hash, m.state, m.attendees, m.host_display_name,
               m.waiting_room_enabled, m.admitted_can_admit, m.end_on_host_leave, m.allow_guests, m.recording_allowed_for_all, m.chat_allowed_for_all
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $2
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $2 OR p.user_id IS NOT NULL
               OR EXISTS (SELECT 1 FROM meeting_co_hosts c
                          WHERE c.meeting_id = m.id AND c.user_id = LOWER($2) AND NOT c.suspended))
          AND (m.room_id ILIKE $1 OR m.state ILIKE $1 OR m.host_display_name ILIKE $1)
        ORDER BY m.created_at DESC
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(&pattern)
    .bind(creator_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Count non-deleted meetings the user owns, has participated in, or is a
/// live co-host of, matching a keyword. See [`search_by_owner`].
pub async fn count_search_by_owner(
    pool: &PgPool,
    creator_id: &str,
    q: &str,
) -> Result<i64, sqlx::Error> {
    let pattern = format!("%{}%", escape_like(q));
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(DISTINCT m.id)
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $2
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $2 OR p.user_id IS NOT NULL
               OR EXISTS (SELECT 1 FROM meeting_co_hosts c
                          WHERE c.meeting_id = m.id AND c.user_id = LOWER($2) AND NOT c.suspended))
          AND (m.room_id ILIKE $1 OR m.state ILIKE $1 OR m.host_display_name ILIKE $1)
        "#,
    )
    .bind(&pattern)
    .bind(creator_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Row returned from [`list_joined_by_user`].
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct JoinedMeetingRow {
    pub id: i32,
    pub room_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub creator_id: Option<String>,
    pub password_hash: Option<String>,
    pub state: Option<String>,
    pub last_joined_at: DateTime<Utc>,
    pub participant_count: i64,
    pub waiting_count: i64,
}

/// List meetings the user has been admitted into at least once (including owned), ordered by `last_joined_at` DESC. No live-co-host branch: the contract is join history.
pub async fn list_joined_by_user(
    pool: &PgPool,
    user_id: &str,
    limit: i64,
    healthy: bool,
) -> Result<Vec<JoinedMeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, JoinedMeetingRow>(&format!(
        r#"
        SELECT m.id,
               m.room_id,
               m.started_at,
               m.ended_at,
               m.created_at,
               m.creator_id,
               m.password_hash,
               m.state,
               p.admitted_at AS last_joined_at,
               COALESCE(pc.admitted_count, 0) AS participant_count,
               COALESCE(wc.waiting_count, 0) AS waiting_count
        FROM meetings m
        INNER JOIN meeting_participants p
            ON p.meeting_id = m.id AND p.user_id = $1
        LEFT JOIN LATERAL (
            SELECT COUNT(*) AS admitted_count
            FROM meeting_participants mp
            WHERE mp.meeting_id = m.id
              AND {present}
        ) pc ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*) AS waiting_count
            FROM meeting_participants wp
            WHERE wp.meeting_id = m.id
              AND {waiting}
        ) wc ON TRUE
        WHERE m.deleted_at IS NULL
          AND p.admitted_at IS NOT NULL
        ORDER BY p.admitted_at DESC, m.id DESC
        LIMIT $2
        "#,
        present = crate::db::participants::present_sql("mp", healthy),
        waiting = crate::db::participants::waiting_sql("wp")
    ))
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Row returned from [`list_feed_for_user`], backing `GET /api/v1/meetings/feed`.
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct FeedMeetingRow {
    pub id: i32,
    pub room_id: String,
    pub state: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub creator_id: Option<String>,
    pub host_display_name: Option<String>,
    pub password_hash: Option<String>,
    pub allow_guests: bool,
    pub recording_allowed_for_all: bool,
    pub chat_allowed_for_all: bool,
    pub waiting_room_enabled: bool,
    pub end_on_host_leave: bool,
    pub admitted_can_admit: bool,
    pub last_active_at: DateTime<Utc>,
    pub ever_admitted: bool,
    /// The requesting user's most recent admission, or `None` if never admitted.
    pub last_admit: Option<DateTime<Utc>>,
    pub participant_count: i64,
    pub waiting_count: i64,
    /// Whether the requesting user holds a live co-host entry for this meeting.
    pub is_co_host: bool,
}

/// List meetings the user owns, has been admitted into, or is a live
/// co-host of, deduplicated to one row per meeting and ordered by
/// `last_active_at` DESC (`m.id DESC` tiebreaker). Powers
/// `GET /api/v1/meetings/feed`. `participant_count` / `waiting_count` /
/// `is_co_host` are folded in via `LEFT JOIN LATERAL` so the route handler
/// issues one round-trip regardless of feed length.
///
/// `is_co_host` is computed once, via the same `LEFT JOIN LATERAL` the WHERE
/// clause tests for co-host membership, so the two can't disagree. That
/// LATERAL forces a per-row nested loop, which `meeting_co_hosts_pkey`
/// already serves — unlike [`list_by_owner`], this query needs no
/// additional index.
pub async fn list_feed_for_user(
    pool: &PgPool,
    user_id: &str,
    limit: i64,
    healthy: bool,
) -> Result<Vec<FeedMeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedMeetingRow>(&format!(
        r#"
        SELECT m.id,
               m.room_id,
               m.state,
               m.created_at,
               m.started_at,
               m.ended_at,
               m.creator_id,
               m.host_display_name,
               m.password_hash,
               m.allow_guests,
               m.recording_allowed_for_all,
               m.chat_allowed_for_all,
               m.waiting_room_enabled,
               m.end_on_host_leave,
               m.admitted_can_admit,
               COALESCE(p.last_admit, m.started_at, m.created_at) AS last_active_at,
               (p.last_admit IS NOT NULL) AS ever_admitted,
               p.last_admit AS last_admit,
               COALESCE(pc.admitted_count, 0) AS participant_count,
               COALESCE(wc.waiting_count, 0) AS waiting_count,
               COALESCE(ch.is_co_host, FALSE) AS is_co_host
        FROM meetings m
        LEFT JOIN LATERAL (
            SELECT MAX(admitted_at) AS last_admit
            FROM meeting_participants
            WHERE meeting_id = m.id
              AND user_id = $1
              AND admitted_at IS NOT NULL
        ) p ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*) AS admitted_count
            FROM meeting_participants mp
            WHERE mp.meeting_id = m.id
              AND {present}
        ) pc ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*) AS waiting_count
            FROM meeting_participants wp
            WHERE wp.meeting_id = m.id
              AND {waiting}
        ) wc ON TRUE
        LEFT JOIN LATERAL (
            SELECT TRUE AS is_co_host
            FROM meeting_co_hosts c
            WHERE c.meeting_id = m.id AND c.user_id = LOWER($1) AND NOT c.suspended
            LIMIT 1
        ) ch ON TRUE
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.last_admit IS NOT NULL OR ch.is_co_host)
        ORDER BY last_active_at DESC, m.id DESC
        LIMIT $2
        "#,
        present = crate::db::participants::present_sql("mp", healthy),
        waiting = crate::db::participants::waiting_sql("wp")
    ))
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Soft-delete a meeting (set `deleted_at`).
pub async fn soft_delete(
    pool: &PgPool,
    room_id: &str,
    creator_id: &str,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        UPDATE meetings
        SET deleted_at = NOW()
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests, recording_allowed_for_all, chat_allowed_for_all
        "#,
    )
    .bind(room_id)
    .bind(creator_id)
    .fetch_optional(pool)
    .await
}

/// Activate a meeting (set state to 'active').
///
/// On a fresh activation (transitioning from `idle` or `ended`) this also
/// refreshes `started_at = NOW()` and clears `ended_at = NULL` so the row
/// reflects the most recent activation. When the meeting is already
/// `active` the call is idempotent — no timestamps are touched.
pub async fn activate<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    meeting_id: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        UPDATE meetings
        SET state = 'active',
            started_at = CASE WHEN state IN ('idle', 'ended') THEN NOW() ELSE started_at END,
            ended_at   = CASE WHEN state IN ('idle', 'ended') THEN NULL  ELSE ended_at   END
        WHERE id = $1
        "#,
    )
    .bind(meeting_id)
    .execute(executor)
    .await?;
    Ok(())
}

/// End a meeting, demote non-creator hosts, and reset co-host entries. Idempotent: a re-fire is a no-op.
pub async fn end_meeting(pool: &PgPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    end_meeting_in(&mut tx, meeting_id).await?;
    tx.commit().await
}

/// [`end_meeting`] on the caller's transaction.
pub async fn end_meeting_in(conn: &mut PgConnection, meeting_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meetings \
         SET state = 'ended', ended_at = COALESCE(ended_at, NOW()) \
         WHERE id = $1 AND state <> 'ended'",
    )
    .bind(meeting_id)
    .execute(&mut *conn)
    .await?;
    crate::db::participants::clear_non_creator_hosts(&mut *conn, meeting_id).await?;
    // Suspensions stay through End; only a real new instance lifts them.
    crate::db::co_hosts::clear_instance_only_entries(conn, meeting_id).await
}

/// How [`start_instance`] left a meeting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// Nothing written: active with someone present, or nothing to activate.
    Unchanged,
    /// Idle with someone present: set active, still the same instance.
    Resumed,
    /// Ended, or nobody present: a new instance started, demoting these non-owner hosts.
    NewInstance { demoted: Vec<String> },
}

impl Activation {
    /// Whether the meeting went from not active to active.
    pub fn activated(&self) -> bool {
        !matches!(self, Activation::Unchanged)
    }

    /// The non-owner hosts a new instance demoted.
    pub fn demoted(&self) -> &[String] {
        match self {
            Activation::NewInstance { demoted } => demoted,
            _ => &[],
        }
    }
}

/// Activate the locked meeting; a new instance starts only from `ended` or when nobody is present.
pub(crate) async fn start_instance_in(
    conn: &mut PgConnection,
    meeting_id: i32,
    healthy: bool,
) -> Result<Activation, sqlx::Error> {
    let state: Option<String> =
        sqlx::query_scalar("SELECT state FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *conn)
            .await?;
    let ended = state.as_deref() == Some(STATE_ENDED);
    if !ended && crate::db::participants::any_present(&mut *conn, meeting_id, healthy).await? {
        if state.as_deref() == Some(STATE_ACTIVE) {
            return Ok(Activation::Unchanged);
        }
        sqlx::query("UPDATE meetings SET state = 'active' WHERE id = $1")
            .bind(meeting_id)
            .execute(&mut *conn)
            .await?;
        return Ok(Activation::Resumed);
    }
    let demoted = crate::db::participants::clear_non_creator_hosts(&mut *conn, meeting_id).await?;
    // Ascending-id lock order avoids deadlock with `record_heartbeat`'s CTE.
    sqlx::query(
        "WITH victims AS ( \
            SELECT id FROM meeting_participants \
            WHERE meeting_id = $1 AND status = 'admitted' AND left_at IS NULL \
            ORDER BY id FOR UPDATE \
         ) \
         UPDATE meeting_participants mp \
         SET status = 'left', left_at = NOW(), live_session_id = 0 \
         FROM victims WHERE mp.id = victims.id",
    )
    .bind(meeting_id)
    .execute(&mut *conn)
    .await?;
    crate::db::co_hosts::reset_for_new_instance(&mut *conn, meeting_id).await?;
    sqlx::query(
        "UPDATE meetings SET state = 'active', started_at = NOW(), ended_at = NULL WHERE id = $1",
    )
    .bind(meeting_id)
    .execute(&mut *conn)
    .await?;
    Ok(Activation::NewInstance { demoted })
}

/// Activate a meeting (see [`start_instance_in`]).
pub async fn start_instance(
    pool: &PgPool,
    meeting_id: i32,
    healthy: bool,
) -> Result<Activation, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let activation = start_instance_in(&mut tx, meeting_id, healthy).await?;
    tx.commit().await?;
    Ok(activation)
}

/// Admit the creator, activating the meeting. Their first join of the
/// CURRENT instance makes them host; a rejoin of an instance they already
/// joined leaves their host flag alone (a transfer may have moved it).
///
/// "First join of this instance" means `admitted_at` is absent or older than
/// `started_at` — true whether the instance is brand new or one a co-host
/// already started, since both stamp `admitted_at`/`started_at` from the
/// same transaction's `NOW()`.
pub async fn owner_join(
    pool: &PgPool,
    meeting_id: i32,
    creator_id: &str,
    display_name: Option<&str>,
    healthy: bool,
) -> Result<(crate::db::participants::ParticipantRow, Activation), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let activation = start_instance_in(&mut tx, meeting_id, healthy).await?;

    let joined_this_instance: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM meeting_participants p
            JOIN meetings m ON m.id = p.meeting_id
            WHERE p.meeting_id = $1 AND p.user_id = $2
              AND p.admitted_at IS NOT NULL AND p.admitted_at >= m.started_at
        )",
    )
    .bind(meeting_id)
    .bind(creator_id)
    .fetch_one(&mut *tx)
    .await?;

    let row = if joined_this_instance {
        crate::db::participants::admit_creator_preserve_host(
            &mut *tx,
            meeting_id,
            creator_id,
            display_name,
        )
        .await?
    } else {
        if let Some(dn) = display_name {
            sqlx::query(
                "UPDATE meetings SET host_display_name = $1 \
                 WHERE id = $2 AND COALESCE(host_display_name, '') = ''",
            )
            .bind(dn)
            .bind(meeting_id)
            .execute(&mut *tx)
            .await?;
        }
        crate::db::participants::upsert_host(&mut *tx, meeting_id, creator_id, display_name).await?
    };
    tx.commit().await?;
    Ok((row, activation))
}

/// Transition an un-ended meeting to `idle` once every participant has left. Returns whether it went idle.
pub async fn set_idle(pool: &PgPool, meeting_id: i32, healthy: bool) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let active: Option<bool> = sqlx::query_scalar(
        "SELECT state IS NOT DISTINCT FROM 'active' FROM meetings WHERE id = $1 FOR UPDATE",
    )
    .bind(meeting_id)
    .fetch_optional(&mut *tx)
    .await?;
    if active != Some(true)
        || crate::db::participants::any_present(&mut *tx, meeting_id, healthy).await?
    {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query("UPDATE meetings SET state = 'idle' WHERE id = $1")
        .bind(meeting_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

/// Update the cached host display name.
pub async fn set_host_display_name(
    pool: &PgPool,
    meeting_id: i32,
    display_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE meetings SET host_display_name = $1 WHERE id = $2")
        .bind(display_name)
        .bind(meeting_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Outcome of [`update_meeting_settings`]. `auto_admitted_user_ids` names the
/// participants the same transaction moved from `waiting` to `admitted` — the
/// caller's only handle for pushing that news to them (issue #2262).
pub struct SettingsUpdate {
    pub row: MeetingRow,
    pub auto_admitted_user_ids: Vec<String>,
}

/// The `($9, $10)` pair the password `CASE` in [`update_meeting_settings`]
/// consumes: `(clear it, the hash to write)`.
fn password_binds(update: &PasswordUpdate) -> (bool, Option<&str>) {
    match update {
        PasswordUpdate::Unchanged => (false, None),
        PasswordUpdate::Set(hash) => (false, Some(hash.as_str())),
        PasswordUpdate::Clear => (true, None),
    }
}

/// Atomically update meeting settings and password; auto-admits waiting participants if the waiting room is disabled. Authorization is the caller's job.
#[allow(clippy::too_many_arguments)]
pub async fn update_meeting_settings(
    pool: &PgPool,
    room_id: &str,
    waiting_room_enabled: Option<bool>,
    admitted_can_admit: Option<bool>,
    end_on_host_leave: Option<bool>,
    allow_guests: Option<bool>,
    recording_allowed_for_all: Option<bool>,
    chat_allowed_for_all: Option<bool>,
    password: &PasswordUpdate,
) -> Result<Option<SettingsUpdate>, sqlx::Error> {
    let (clear_password, new_password_hash) = password_binds(password);

    let mut tx = pool.begin().await?;

    let updated = sqlx::query_as::<_, MeetingRow>(
        r#"
        UPDATE meetings
        SET waiting_room_enabled = COALESCE($2, waiting_room_enabled),
            admitted_can_admit = COALESCE($3, admitted_can_admit),
            end_on_host_leave = COALESCE($4, end_on_host_leave),
            allow_guests = COALESCE($5, allow_guests),
            recording_allowed_for_all = COALESCE($6, recording_allowed_for_all),
            chat_allowed_for_all = COALESCE($7, chat_allowed_for_all),
            password_hash = CASE WHEN $8 THEN NULL ELSE COALESCE($9, password_hash) END
        WHERE room_id = $1 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests, recording_allowed_for_all, chat_allowed_for_all
        "#,
    )
    .bind(room_id)
    .bind(waiting_room_enabled)
    .bind(admitted_can_admit)
    .bind(end_on_host_leave)
    .bind(allow_guests)
    .bind(recording_allowed_for_all)
    .bind(chat_allowed_for_all)
    .bind(clear_password)
    .bind(new_password_hash)
    .fetch_optional(&mut *tx)
    .await?;

    // When disabling the waiting room, admit everyone `waiting_sql` counts as waiting.
    let mut auto_admitted_user_ids = Vec::new();
    if let Some(ref row) = updated {
        if waiting_room_enabled == Some(false) {
            auto_admitted_user_ids = sqlx::query_scalar::<_, String>(&format!(
                "UPDATE meeting_participants p \
                 SET status = 'admitted', admitted_at = NOW(), live_session_id = 0, \
                     presence_seen_at = NULL \
                 WHERE p.meeting_id = $1 AND {} RETURNING p.user_id",
                crate::db::participants::waiting_sql("p")
            ))
            .bind(row.id)
            .fetch_all(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(updated.map(|row| SettingsUpdate {
        row,
        auto_admitted_user_ids,
    }))
}

#[cfg(test)]
mod tests {
    use super::{display_state, escape_like, password_binds, PasswordUpdate};

    // ── The password `CASE` binds (issue #2207) ──────────────────────────

    #[test]
    fn an_unchanged_password_binds_no_clear_and_no_hash() {
        assert_eq!(password_binds(&PasswordUpdate::Unchanged), (false, None));
    }

    #[test]
    fn clearing_binds_the_flag_and_no_hash() {
        assert_eq!(password_binds(&PasswordUpdate::Clear), (true, None));
    }

    #[test]
    fn setting_binds_the_hash_with_the_clear_flag_off() {
        let update = PasswordUpdate::Set("$argon2id$v=19$fake".to_string());
        assert_eq!(
            password_binds(&update),
            (false, Some("$argon2id$v=19$fake"))
        );
    }

    // ── display_state: the `idle ⟺ zero present` invariant (issue #1628) ──────

    #[test]
    fn display_state_idle_iff_zero_present() {
        // With nobody present, every non-ended raw state displays as idle.
        assert_eq!(display_state(Some("idle"), 0), "idle");
        assert_eq!(display_state(Some("active"), 0), "idle");
        assert_eq!(display_state(None, 0), "idle");
    }

    #[test]
    fn display_state_present_participants_never_idle() {
        // The core fix: a meeting with >=1 present participant is NEVER idle,
        // even if the raw column lags at 'idle' (stuck after a transport-only
        // reconnect that re-activated nobody, or a brief column/presence skew).
        assert_eq!(display_state(Some("idle"), 1), "active");
        assert_eq!(display_state(Some("idle"), 5), "active");
        assert_eq!(display_state(Some("active"), 1), "active");
        // Also covers the ">1 present but shown idle" symptom from the issue.
        assert_eq!(display_state(Some("idle"), 2), "active");
        assert_eq!(display_state(None, 3), "active");
    }

    #[test]
    fn display_state_ended_is_terminal_even_with_present_rows() {
        // `ended` always wins, even against an in-flight roster write that still
        // counts a present participant when end_meeting lands — the end-vs-
        // presence race must resolve to ended, never resurrect to active.
        assert_eq!(display_state(Some("ended"), 0), "ended");
        assert_eq!(display_state(Some("ended"), 1), "ended");
        assert_eq!(display_state(Some("ended"), 9), "ended");
    }

    #[test]
    fn escape_like_neutralises_percent_and_underscore() {
        // Raw `%` / `_` would be treated as wildcards and match more than the
        // caller intended; escaped they should match the literal character.
        assert_eq!(escape_like("%"), r"\%");
        assert_eq!(escape_like("_"), r"\_");
        assert_eq!(escape_like("ab%cd_ef"), r"ab\%cd\_ef");
    }

    #[test]
    fn escape_like_preserves_plain_characters() {
        assert_eq!(escape_like(""), "");
        assert_eq!(escape_like("standup2024"), "standup2024");
        assert_eq!(escape_like("my-meeting_id"), r"my-meeting\_id");
    }

    #[test]
    fn escape_like_escapes_backslash_before_metacharacters() {
        // Must double-escape `\` first so user-provided `\` survives and
        // doesn't accidentally escape the `%` we add around the query later.
        assert_eq!(escape_like(r"\"), r"\\");
        assert_eq!(escape_like(r"a\b"), r"a\\b");
        // A user typing a raw backslash followed by a percent must still
        // match a literal backslash-percent, not an escaped-percent pattern.
        assert_eq!(escape_like(r"\%"), r"\\\%");
    }
}
