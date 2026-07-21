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
//!
//! Every write runs through [`with_retry!`], so on SQLite it replays past
//! `SQLITE_BUSY` instead of surfacing "database is locked" to the client (a
//! no-op on PostgreSQL). Replay is safe: a transaction is rolled back on
//! failure, an autocommit statement applied nothing, and the `status`
//! predicates make a replay that raced a real change return `None`.

use chrono::{DateTime, Utc};

use crate::db::{bind_now, lock, now_sql, q, with_retry, DbPool};

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

const PARTICIPANT_COLUMNS: &str = "id, meeting_id, user_id, status, is_host, is_required, \
    joined_at, admitted_at, left_at, created_at, updated_at, display_name";

/// Render a write statement: substitute `{cols}` and `{now}` (at `slot`), rewrite
/// placeholders. Pair with `bind_now!`.
fn stmt(template: &str, slot: usize) -> String {
    now_sql(&template.replace("{cols}", PARTICIPANT_COLUMNS), slot)
}

/// Insert or update a participant as host (admitted immediately).
pub async fn upsert_host(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    with_retry! {
        // `joined_at` / `created_at` are left to the column DEFAULTs (the DB clock).
        let sql = stmt(
            "INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
             VALUES ($1, $2, 'admitted', TRUE, $3, {now})
             ON CONFLICT (meeting_id, user_id)
             DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = {now}, updated_at = {now}, left_at = NULL,
                           display_name = COALESCE($3, meeting_participants.display_name)
             RETURNING {cols}",
            4,
        );
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name))
        .fetch_one(pool)
        .await
    }
}

/// Join a meeting as an attendee, honouring the current `waiting_room_enabled`.
///
/// Reads the flag under the write lock ([`lock::begin_write`]) so it cannot go
/// stale against a concurrent [`crate::db::meetings::update_waiting_room_enabled`],
/// then inserts as `waiting` or `admitted` accordingly. Returns
/// `(auto_admitted, row, waiting_room_enabled)`.
pub async fn join_attendee(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<(bool, ParticipantRow, bool), sqlx::Error> {
    with_retry! {
        let mut tx = lock::begin_write(pool).await?;

        let (waiting_room_enabled,): (bool,) =
            sqlx::query_as(&q(lock::SELECT_WAITING_ROOM_LOCKED))
                .bind(meeting_id)
                .fetch_one(&mut *tx)
                .await?;

        let template = if waiting_room_enabled {
            "INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name)
             VALUES ($1, $2, 'waiting', FALSE, $3)
             ON CONFLICT (meeting_id, user_id)
             DO UPDATE SET status = 'waiting', updated_at = {now}, left_at = NULL,
                           display_name = COALESCE($3, meeting_participants.display_name)
             RETURNING {cols}"
        } else {
            "INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
             VALUES ($1, $2, 'admitted', FALSE, $3, {now})
             ON CONFLICT (meeting_id, user_id)
             DO UPDATE SET status = 'admitted', admitted_at = {now}, updated_at = {now}, left_at = NULL,
                           display_name = COALESCE($3, meeting_participants.display_name)
             RETURNING {cols}"
        };
        let sql = stmt(template, 4);
        let row = bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name))
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok((!waiting_room_enabled, row, waiting_room_enabled))
    }
}

/// Get all participants in 'waiting' status for a meeting.
pub async fn get_waiting(
    pool: &DbPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    sqlx::query_as::<_, ParticipantRow>(&q(&format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'"
    )))
    .bind(meeting_id)
    .fetch_all(pool)
    .await
}

/// Get all admitted (active) participants in a meeting.
pub async fn get_admitted(
    pool: &DbPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    sqlx::query_as::<_, ParticipantRow>(&q(&format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'"
    )))
    .bind(meeting_id)
    .fetch_all(pool)
    .await
}

/// Get a single participant's status.
pub async fn get_status(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    sqlx::query_as::<_, ParticipantRow>(&q(&format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2"
    )))
    .bind(meeting_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// Admit a single waiting participant.
pub async fn admit(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    with_retry! {
        let sql = stmt(
            "UPDATE meeting_participants SET status = 'admitted', admitted_at = {now}, updated_at = {now}
             WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
             RETURNING {cols}",
            3,
        );
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql)
            .bind(meeting_id)
            .bind(user_id))
        .fetch_optional(pool)
        .await
    }
}

/// Admit all waiting participants at once.
pub async fn admit_all(pool: &DbPool, meeting_id: i32) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    with_retry! {
        let sql = stmt(
            "UPDATE meeting_participants SET status = 'admitted', admitted_at = {now}, updated_at = {now}
             WHERE meeting_id = $1 AND status = 'waiting'
             RETURNING {cols}",
            2,
        );
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql).bind(meeting_id))
            .fetch_all(pool)
            .await
    }
}

/// Reject a waiting participant.
pub async fn reject(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    with_retry! {
        let sql = stmt(
            "UPDATE meeting_participants SET status = 'rejected', updated_at = {now}
             WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
             RETURNING {cols}",
            3,
        );
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql)
            .bind(meeting_id)
            .bind(user_id))
        .fetch_optional(pool)
        .await
    }
}

/// Leave a meeting (set status to 'left').
pub async fn leave(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    with_retry! {
        let sql = stmt(
            "UPDATE meeting_participants SET status = 'left', left_at = {now}, updated_at = {now}
             WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
             RETURNING {cols}",
            3,
        );
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&sql)
            .bind(meeting_id)
            .bind(user_id))
        .fetch_optional(pool)
        .await
    }
}

/// Count admitted participants in a meeting.
pub async fn count_admitted(pool: &DbPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'",
    ))
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Count waiting participants in a meeting.
pub async fn count_waiting(pool: &DbPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'",
    ))
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

impl ParticipantRow {
    /// Convert a database row into the API response type, optionally attaching a
    /// `room_token` (only for the participant themselves).
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
