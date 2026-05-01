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

//! Meeting participant table queries.

use chrono::{DateTime, Utc};
use sqlx::PgPool;

/// Row returned from the `meeting_participants` table.
#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct ParticipantRow {
    pub id: i32,
    pub meeting_id: i32,
    pub user_id: String,
    pub status: String,
    pub is_host: bool,
    pub is_guest: bool,
    pub is_required: bool,
    pub joined_at: DateTime<Utc>,
    pub admitted_at: Option<DateTime<Utc>>,
    pub left_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub display_name: Option<String>,
}

const PARTICIPANT_COLUMNS: &str = r#"
    id, meeting_id, user_id, status, is_host, is_guest, is_required,
    joined_at, admitted_at, left_at, created_at, updated_at, display_name
"#;

/// Insert or update a participant as host (admitted immediately).
///
/// **Display-name reconciliation policy on rejoin:** when a row already exists
/// for `(meeting_id, user_id)` with a non-empty `display_name`, the existing
/// value is preserved — the request's `display_name` does NOT overwrite it.
/// This is intentional: rejoin must never silently rename a participant.
/// Mid-meeting renames go through the rate-limited
/// [`crate::routes::participants::update_display_name`] endpoint.
///
/// The `NULLIF(..., '')` rewrites an empty-string existing value to `NULL` so
/// the `COALESCE` falls through to the request's value — empty-string is
/// treated as "no name set yet" (the legitimate first-time case where a
/// follow-up rejoin should be allowed to fill it in).
///
/// See issue #502 for the bug this prevents (manually-typed name "Antonio"
/// being silently overwritten by an OAuth-derived "Tony" on back-then-rejoin).
pub async fn upsert_host(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', TRUE, FALSE, $3, NOW())
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = NOW(), left_at = NULL,
                      display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_one(pool)
        .await
}

/// Atomically join a meeting as an attendee, respecting the current `waiting_room_enabled`
/// setting. Locks the meeting row with `FOR UPDATE` to serialize against concurrent
/// waiting room toggles via `update_meeting_settings`.
///
/// When `check_host_gone_for` is `Some(creator_id)`, verifies within the same transaction
/// that the host is still admitted. Returns `Ok(None)` if the host has left — callers
/// should respond with a "joining not allowed" error. This closes the TOCTOU window
/// that arises when the check is performed outside the transaction.
///
/// Returns `Ok(Some((auto_admitted, row, waiting_room_enabled)))` on success, where
/// `auto_admitted` is `true` when the participant was immediately admitted (waiting room
/// disabled). The third element is the `waiting_room_enabled` value observed under the
/// row lock.
pub async fn join_attendee(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
    check_host_gone_for: Option<&str>,
    is_guest: bool,
) -> Result<Option<(bool, ParticipantRow, bool)>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // Lock the meeting row to serialize against concurrent waiting room toggles.
    let (waiting_room_enabled,): (bool,) =
        sqlx::query_as("SELECT waiting_room_enabled FROM meetings WHERE id = $1 FOR UPDATE")
            .bind(meeting_id)
            .fetch_one(&mut *tx)
            .await?;

    // If requested, verify within the same transaction that the host has not left.
    // Doing this outside the transaction creates a TOCTOU race: two concurrent
    // requests can both pass the pre-transaction check, then both insert into a
    // meeting where no one can admit them.
    if let Some(creator_id) = check_host_gone_for {
        let host_status: Option<(String,)> = sqlx::query_as(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2",
        )
        .bind(meeting_id)
        .bind(creator_id)
        .fetch_optional(&mut *tx)
        .await?;

        let host_is_gone = host_status.map(|(s,)| s != "admitted").unwrap_or(true);
        if host_is_gone {
            tx.rollback().await?;
            return Ok(None);
        }
    }

    // Display-name reconciliation policy on rejoin: see [`upsert_host`] for
    // the rationale. The same `COALESCE(NULLIF(...), $3)` shape applies to
    // both branches below — non-empty existing names beat the request's
    // value so rejoin never silently renames a participant. Issue #502.
    let row = if waiting_room_enabled {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name)
            VALUES ($1, $2, 'waiting', FALSE, $4, $3)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'waiting', left_at = NULL,
                          display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
            .bind(is_guest)
            .fetch_one(&mut *tx)
            .await?
    } else {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, is_guest, display_name, admitted_at)
            VALUES ($1, $2, 'admitted', FALSE, $4, $3, NOW())
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'admitted', admitted_at = NOW(), left_at = NULL,
                          display_name = COALESCE(NULLIF(meeting_participants.display_name, ''), $3)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
            .bind(is_guest)
            .fetch_one(&mut *tx)
            .await?
    };

    tx.commit().await?;
    Ok(Some((!waiting_room_enabled, row, waiting_room_enabled)))
}

/// Get all participants in 'waiting' status for a meeting.
pub async fn get_waiting(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Get all admitted (active) participants in a meeting.
pub async fn get_admitted(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Get a single participant's status.
pub async fn get_status(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Admit a single participant.
pub async fn admit(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Admit all waiting participants at once.
pub async fn admit_all(pool: &PgPool, meeting_id: i32) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW()
        WHERE meeting_id = $1 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Reject a participant.
pub async fn reject(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'rejected'
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Leave a meeting (set status to 'left').
pub async fn leave(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'left', left_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Update a participant's display name.
pub async fn update_display_name(
    pool: &PgPool,
    meeting_id: i32,
    user_id: &str,
    display_name: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET display_name = $3, updated_at = NOW()
        WHERE meeting_id = $1 AND user_id = $2
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name)
        .fetch_optional(pool)
        .await
}

/// Load the participant roster for a SearchV2 index push.
///
/// Returns one row per `admitted` or `waiting` participant.  We include
/// `waiting` users so they become searchable as soon as they enter the
/// waiting room — they're already visible to Postgres-side searches via
/// [`crate::db::meetings::list_by_owner`], which JOINs on any participant
/// row regardless of status.
///
/// Shape is mapped into [`crate::search::ParticipantAcl`], which is the
/// minimal subset needed for both the top-level `participants` / `acls`
/// arrays and the richer `documentObject.participants` entries.
///
/// Ordering: host first (for stable creator-first ACL lists), then by
/// admission time to keep doc diffs predictable as the roster evolves.
pub async fn list_for_search(
    pool: &PgPool,
    meeting_id: i32,
) -> Result<Vec<crate::search::ParticipantAcl>, sqlx::Error> {
    // Module-private row struct keeps the query typed without leaking a
    // tuple signature across fn boundaries (and pleases
    // `clippy::type_complexity`).  joined_at is NOT NULL in the schema;
    // admitted_at is nullable until the participant is admitted from the
    // waiting room.
    #[derive(sqlx::FromRow)]
    struct SearchParticipantRow {
        user_id: String,
        display_name: Option<String>,
        is_host: bool,
        status: String,
        joined_at: DateTime<Utc>,
        admitted_at: Option<DateTime<Utc>>,
    }

    let rows: Vec<SearchParticipantRow> = sqlx::query_as(
        r#"
        SELECT user_id, display_name, is_host, status, joined_at, admitted_at
        FROM meeting_participants
        WHERE meeting_id = $1
          AND status IN ('admitted', 'waiting')
        ORDER BY is_host DESC, admitted_at NULLS LAST, created_at
        "#,
    )
    .bind(meeting_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| crate::search::ParticipantAcl {
            user_id: r.user_id,
            display_name: r.display_name,
            is_host: r.is_host,
            status: r.status,
            joined_at: Some(r.joined_at),
            admitted_at: r.admitted_at,
        })
        .collect())
}

/// Count admitted participants in a meeting.
pub async fn count_admitted(pool: &PgPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Count waiting participants in a meeting.
pub async fn count_waiting(pool: &PgPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// -- Conversions to API response types --

impl ParticipantRow {
    /// Convert a database row into the API response type.
    /// Optionally attach a `room_token` (only for the participant themselves).
    pub fn into_participant_status(
        self,
        room_token: Option<String>,
    ) -> videocall_meeting_types::responses::ParticipantStatusResponse {
        videocall_meeting_types::responses::ParticipantStatusResponse {
            is_guest: self.is_guest,
            user_id: self.user_id,
            display_name: self.display_name,
            status: self.status,
            is_host: self.is_host,
            joined_at: self.joined_at.timestamp(),
            admitted_at: self.admitted_at.map(|t| t.timestamp()),
            room_token,
            observer_token: None,
            waiting_room_enabled: None,
            admitted_can_admit: None,
            end_on_host_leave: None,
            host_display_name: None,
            host_user_id: None,
            allow_guests: None,
        }
    }
}
