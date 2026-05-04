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
use sqlx::PgPool;

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
    )
    .await
}

/// Create a new meeting with explicit waiting_room_enabled setting.
#[allow(clippy::too_many_arguments)]
pub async fn create_with_options(
    pool: &PgPool,
    room_id: &str,
    creator_id: &str,
    password_hash: Option<&str>,
    attendees: &JsonValue,
    waiting_room_enabled: bool,
    admitted_can_admit: bool,
    end_on_host_leave: bool,
    allow_guests: bool,
) -> Result<MeetingRow, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        INSERT INTO meetings (room_id, creator_id, started_at, password_hash, state, attendees, waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests)
        VALUES ($1, $2, NOW(), $3, 'idle', $4, $5, $6, $7, $8)
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests
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
    .fetch_one(pool)
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
               waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests
        FROM meetings
        WHERE room_id = $1 AND deleted_at IS NULL
        "#,
    )
    .bind(room_id)
    .fetch_optional(pool)
    .await
}

/// List meetings the user owns OR has participated in (non-deleted),
/// ordered by created_at DESC.
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
               m.waiting_room_enabled, m.admitted_can_admit, m.end_on_host_leave, m.allow_guests
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $1
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.user_id IS NOT NULL)
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

/// Count meetings the user owns OR has participated in (non-deleted).
pub async fn count_by_owner(pool: &PgPool, creator_id: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(DISTINCT m.id)
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $1
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.user_id IS NOT NULL)
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

/// Search non-deleted meetings the user owns OR has participated in,
/// matching a keyword against `room_id`, `state`, and `host_display_name`
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
               m.waiting_room_enabled, m.admitted_can_admit, m.end_on_host_leave, m.allow_guests
        FROM meetings m
        LEFT JOIN meeting_participants p ON p.meeting_id = m.id AND p.user_id = $2
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $2 OR p.user_id IS NOT NULL)
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

/// Count non-deleted meetings the user owns OR has participated in,
/// matching a keyword.
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
          AND (m.creator_id = $2 OR p.user_id IS NOT NULL)
          AND (m.room_id ILIKE $1 OR m.state ILIKE $1 OR m.host_display_name ILIKE $1)
        "#,
    )
    .bind(&pattern)
    .bind(creator_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Row returned from [`list_joined_by_user`] — a meeting the user has been
/// admitted into, with the join-time metadata used for ordering.
///
/// The `last_joined_at` value is `p.admitted_at`. The query filters on
/// `admitted_at IS NOT NULL`, so this column is always populated.
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
}

/// List meetings the user has been admitted into at least once, including
/// meetings they own. Ordered by `last_joined_at` descending, with `m.id DESC`
/// as a deterministic tiebreaker for same-microsecond admissions.
///
/// The filter `admitted_at IS NOT NULL` is the canonical "ever admitted" check:
/// `admitted_at` is set on every admission (initial waiting-room admit, host
/// upsert, or auto-admit when the waiting room is off) and is never cleared
/// when a participant leaves. Pure-`waiting` rows and waiting-then-rejected
/// rows have `admitted_at IS NULL` and are excluded.
pub async fn list_joined_by_user(
    pool: &PgPool,
    user_id: &str,
    limit: i64,
) -> Result<Vec<JoinedMeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, JoinedMeetingRow>(
        r#"
        SELECT m.id,
               m.room_id,
               m.started_at,
               m.ended_at,
               m.created_at,
               m.creator_id,
               m.password_hash,
               m.state,
               p.admitted_at AS last_joined_at
        FROM meetings m
        INNER JOIN meeting_participants p
            ON p.meeting_id = m.id AND p.user_id = $1
        WHERE m.deleted_at IS NULL
          AND p.admitted_at IS NOT NULL
        ORDER BY p.admitted_at DESC, m.id DESC
        LIMIT $2
        "#,
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Row returned from [`list_feed_for_user`] — the deduplicated home-feed
/// entry that backs `GET /api/v1/meetings/feed`.
///
/// Carries the meeting's settings + counts plus the join-time metadata used
/// for ordering. Counts are folded into the same SELECT (LEFT JOIN LATERAL)
/// so the route handler does not need to issue per-row queries to assemble
/// participant_count / waiting_count.
///
/// `last_active_at` is `COALESCE(p.last_admit, m.started_at, m.created_at)`
/// and is therefore always non-null. `started_at` may be earlier than
/// `last_active_at` when the user has joined a re-activated meeting since
/// the most recent activation refreshed `started_at`.
///
/// `ever_admitted` is `true` when the user has at least one
/// `meeting_participants` row with `admitted_at IS NOT NULL` — equivalent to
/// `p.last_admit IS NOT NULL`. The route handler uses it for nothing today
/// but it's exposed in case future call sites want a quick "has the user
/// actually joined this meeting before" check without going back to the DB.
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
    pub password_hash: Option<String>,
    pub allow_guests: bool,
    pub waiting_room_enabled: bool,
    pub end_on_host_leave: bool,
    pub admitted_can_admit: bool,
    pub last_active_at: DateTime<Utc>,
    pub ever_admitted: bool,
    pub participant_count: i64,
    pub waiting_count: i64,
}

/// List meetings the user owns OR has been admitted into, deduplicated to
/// one row per meeting. Powers `GET /api/v1/meetings/feed`.
///
/// ## Membership predicate
///
/// A meeting `m` appears in the feed when either:
///   - `m.creator_id = user_id` (the user owns it), regardless of whether
///     they have ever joined; or
///   - the user has at least one `meeting_participants` row for `m` with
///     `admitted_at IS NOT NULL` — i.e. they were actually admitted at some
///     point. Pure-`waiting` rows (`admitted_at IS NULL`) are excluded.
///
/// ## Ordering
///
/// `last_active_at = COALESCE(p.last_admit, m.started_at, m.created_at)`,
/// descending. `m.id DESC` is the deterministic tiebreaker for rows that
/// share the same `last_active_at` (e.g. two meetings activated in the same
/// microsecond on a busy host).
///
/// ## Folded counts
///
/// `participant_count` (rows with `status = 'admitted'`) and `waiting_count`
/// (rows with `status = 'waiting'`) are computed inside the same query via
/// LEFT JOIN LATERAL subqueries so the route handler issues exactly one
/// round-trip regardless of feed length. Status semantics match the legacy
/// `db_participants::count_admitted` / `count_waiting` so the
/// /feed counts are byte-for-byte identical to the per-row helpers.
pub async fn list_feed_for_user(
    pool: &PgPool,
    user_id: &str,
    limit: i64,
) -> Result<Vec<FeedMeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, FeedMeetingRow>(
        r#"
        SELECT m.id,
               m.room_id,
               m.state,
               m.created_at,
               m.started_at,
               m.ended_at,
               m.creator_id,
               m.password_hash,
               m.allow_guests,
               m.waiting_room_enabled,
               m.end_on_host_leave,
               m.admitted_can_admit,
               COALESCE(p.last_admit, m.started_at, m.created_at) AS last_active_at,
               (p.last_admit IS NOT NULL) AS ever_admitted,
               COALESCE(pc.admitted_count, 0) AS participant_count,
               COALESCE(wc.waiting_count, 0) AS waiting_count
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
            FROM meeting_participants
            WHERE meeting_id = m.id
              AND status = 'admitted'
        ) pc ON TRUE
        LEFT JOIN LATERAL (
            SELECT COUNT(*) AS waiting_count
            FROM meeting_participants
            WHERE meeting_id = m.id
              AND status = 'waiting'
        ) wc ON TRUE
        WHERE m.deleted_at IS NULL
          AND (m.creator_id = $1 OR p.last_admit IS NOT NULL)
        ORDER BY last_active_at DESC, m.id DESC
        LIMIT $2
        "#,
    )
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
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests
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
pub async fn activate(pool: &PgPool, meeting_id: i32) -> Result<(), sqlx::Error> {
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
    .execute(pool)
    .await?;
    Ok(())
}

/// End a meeting (set state to 'ended', set ended_at if not already set).
///
/// Idempotent at the SQL level: `state <> 'ended'` short-circuits zero-row
/// UPDATEs on re-fire, and `COALESCE(ended_at, NOW())` preserves the original
/// `ended_at` so the "when did this meeting end" signal is stable across
/// duplicate triggers (e.g. NATS re-subscribe after disconnect, or multi-replica
/// fan-out without a queue group). Callers do not inspect rows-affected, so the
/// no-op second call is intentionally indistinguishable from a fresh end.
pub async fn end_meeting(pool: &PgPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE meetings \
         SET state = 'ended', ended_at = COALESCE(ended_at, NOW()) \
         WHERE id = $1 AND state <> 'ended'",
    )
    .bind(meeting_id)
    .execute(pool)
    .await?;
    Ok(())
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

/// Atomically update the waiting_room_enabled, admitted_can_admit, end_on_host_leave, and allow_guests settings for a meeting.
/// When disabling the waiting room, auto-admits all currently waiting participants
/// within the same transaction to prevent race conditions.
pub async fn update_meeting_settings(
    pool: &PgPool,
    room_id: &str,
    creator_id: &str,
    waiting_room_enabled: Option<bool>,
    admitted_can_admit: Option<bool>,
    end_on_host_leave: Option<bool>,
    allow_guests: Option<bool>,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    let updated = sqlx::query_as::<_, MeetingRow>(
        r#"
        UPDATE meetings
        SET waiting_room_enabled = COALESCE($3, waiting_room_enabled),
            admitted_can_admit = COALESCE($4, admitted_can_admit),
            end_on_host_leave = COALESCE($5, end_on_host_leave),
            allow_guests = COALESCE($6, allow_guests)
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled, admitted_can_admit, end_on_host_leave, allow_guests
        "#,
    )
    .bind(room_id)
    .bind(creator_id)
    .bind(waiting_room_enabled)
    .bind(admitted_can_admit)
    .bind(end_on_host_leave)
    .bind(allow_guests)
    .fetch_optional(&mut *tx)
    .await?;

    // When disabling the waiting room, admit everyone currently waiting.
    if let Some(ref row) = updated {
        if waiting_room_enabled == Some(false) {
            sqlx::query(
                "UPDATE meeting_participants SET status = 'admitted', admitted_at = NOW() \
                 WHERE meeting_id = $1 AND status = 'waiting'",
            )
            .bind(row.id)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::escape_like;

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
