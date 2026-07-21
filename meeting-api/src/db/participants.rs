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
//! # Why the `_stmt` / `_txn` split
//!
//! Every write here is a public wrapper around a private inner function, called
//! through [`crate::db::lock::with_write_retry`]. On PostgreSQL the wrapper is a
//! pass-through. On SQLite it absorbs `SQLITE_BUSY`, which a single writer can
//! still hit once `busy_timeout` expires — without it a busy database surfaces
//! to the API client as a bare "database is locked".
//!
//! Replaying is safe for both shapes. A `_txn` inner has already rolled its
//! transaction back before returning an error, and a `_stmt` inner is a single
//! autocommit statement that applied nothing if it failed. The status
//! predicates (`AND status = 'waiting'`) also make a replay that races a real
//! change return `None` rather than double-applying.

use chrono::{DateTime, Utc};

use crate::db::{bind_now, lock, now_expr, q, DbPool};

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

const PARTICIPANT_COLUMNS: &str = r#"
    id, meeting_id, user_id, status, is_host, is_required,
    joined_at, admitted_at, left_at, created_at, updated_at, display_name
"#;

/// Insert or update a participant as host (admitted immediately).
pub async fn upsert_host(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    lock::with_write_retry(|| Box::pin(upsert_host_stmt(pool, meeting_id, user_id, display_name)))
        .await
}

async fn upsert_host_stmt(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    // `joined_at` / `created_at` are left to the column DEFAULTs, which are the
    // database's clock on both backends.
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', TRUE, $3, {now})
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = {now}, updated_at = {now}, left_at = NULL,
                      display_name = COALESCE($3, meeting_participants.display_name)
        RETURNING {PARTICIPANT_COLUMNS}
        "#,
        now = now_expr(4)
    );
    let query = q(&query);
    bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id)
        .bind(display_name))
    .fetch_one(pool)
    .await
}

/// Atomically join a meeting as an attendee, respecting the current `waiting_room_enabled`
/// setting. Opens the transaction through [`lock::begin_write`] so the meeting row is
/// serialized against concurrent waiting room toggles via
/// [`crate::db::meetings::update_waiting_room_enabled`].
///
/// Returns `(auto_admitted, ParticipantRow, waiting_room_enabled)` where `auto_admitted`
/// is `true` when the participant was immediately admitted (waiting room disabled).
/// The third element is the `waiting_room_enabled` value observed under the write lock,
/// which avoids stale reads from a pre-transaction fetch.
pub async fn join_attendee(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<(bool, ParticipantRow, bool), sqlx::Error> {
    lock::with_write_retry(|| Box::pin(join_attendee_txn(pool, meeting_id, user_id, display_name)))
        .await
}

/// One attempt of [`join_attendee`]. Replayable: a failure rolls the transaction
/// back before returning.
async fn join_attendee_txn(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<(bool, ParticipantRow, bool), sqlx::Error> {
    let mut tx = lock::begin_write(pool).await?;

    // Read the flag under the write lock so a concurrent toggle cannot slip in
    // between this read and the insert below.
    let (waiting_room_enabled,): (bool,) = sqlx::query_as(&q(lock::SELECT_WAITING_ROOM_LOCKED))
        .bind(meeting_id)
        .fetch_one(&mut *tx)
        .await?;

    let row = if waiting_room_enabled {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name)
            VALUES ($1, $2, 'waiting', FALSE, $3)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'waiting', updated_at = {now}, left_at = NULL,
                          display_name = COALESCE($3, meeting_participants.display_name)
            RETURNING {PARTICIPANT_COLUMNS}
            "#,
            now = now_expr(4)
        );
        let query = q(&query);
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name))
        .fetch_one(&mut *tx)
        .await?
    } else {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
            VALUES ($1, $2, 'admitted', FALSE, $3, {now})
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'admitted', admitted_at = {now}, updated_at = {now}, left_at = NULL,
                          display_name = COALESCE($3, meeting_participants.display_name)
            RETURNING {PARTICIPANT_COLUMNS}
            "#,
            now = now_expr(4)
        );
        let query = q(&query);
        bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name))
        .fetch_one(&mut *tx)
        .await?
    };

    tx.commit().await?;
    Ok((!waiting_room_enabled, row, waiting_room_enabled))
}

/// Get all participants in 'waiting' status for a meeting.
pub async fn get_waiting(
    pool: &DbPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'"
    );
    sqlx::query_as::<_, ParticipantRow>(&q(&query))
        .bind(meeting_id)
        .fetch_all(pool)
        .await
}

/// Get all admitted (active) participants in a meeting.
pub async fn get_admitted(
    pool: &DbPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'"
    );
    sqlx::query_as::<_, ParticipantRow>(&q(&query))
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
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND user_id = $2"
    );
    sqlx::query_as::<_, ParticipantRow>(&q(&query))
        .bind(meeting_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// Admit a single participant.
pub async fn admit(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    lock::with_write_retry(|| Box::pin(admit_stmt(pool, meeting_id, user_id))).await
}

async fn admit_stmt(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = {now}, updated_at = {now}
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#,
        now = now_expr(3)
    );
    let query = q(&query);
    bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id))
    .fetch_optional(pool)
    .await
}

/// Admit all waiting participants at once.
pub async fn admit_all(pool: &DbPool, meeting_id: i32) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    lock::with_write_retry(|| Box::pin(admit_all_stmt(pool, meeting_id))).await
}

async fn admit_all_stmt(
    pool: &DbPool,
    meeting_id: i32,
) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = {now}, updated_at = {now}
        WHERE meeting_id = $1 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#,
        now = now_expr(2)
    );
    let query = q(&query);
    bind_now!(sqlx::query_as::<_, ParticipantRow>(&query).bind(meeting_id))
        .fetch_all(pool)
        .await
}

/// Reject a participant.
pub async fn reject(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    lock::with_write_retry(|| Box::pin(reject_stmt(pool, meeting_id, user_id))).await
}

async fn reject_stmt(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'rejected', updated_at = {now}
        WHERE meeting_id = $1 AND user_id = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#,
        now = now_expr(3)
    );
    let query = q(&query);
    bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id))
    .fetch_optional(pool)
    .await
}

/// Leave a meeting (set status to 'left').
pub async fn leave(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    lock::with_write_retry(|| Box::pin(leave_stmt(pool, meeting_id, user_id))).await
}

async fn leave_stmt(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'left', left_at = {now}, updated_at = {now}
        WHERE meeting_id = $1 AND user_id = $2 AND status IN ('admitted', 'waiting')
        RETURNING {PARTICIPANT_COLUMNS}
        "#,
        now = now_expr(3)
    );
    let query = q(&query);
    bind_now!(sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(user_id))
    .fetch_optional(pool)
    .await
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

// -- Conversions to API response types --

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
