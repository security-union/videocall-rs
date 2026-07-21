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
//! Shared across the PostgreSQL and SQLite backends. `$N` placeholders are
//! parsed natively by both sqlx drivers, so the query strings are identical;
//! the only backend-specific pieces come from [`crate::db`] helpers
//! ([`SQL_NOW`], [`FOR_UPDATE`], [`begin_write_tx`], [`with_busy_retry`]).
//!
//! `updated_at` is written explicitly in every UPDATE / `DO UPDATE`. On
//! PostgreSQL an AFTER-UPDATE trigger would also maintain it (harmless double
//! write); the SQLite migration drops that trigger because SQLite computes
//! `RETURNING` before AFTER-triggers fire.

use crate::db::{begin_write_tx, with_busy_retry, DbPool, ParticipantRow, FOR_UPDATE, SQL_NOW};

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
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', TRUE, $3, {SQL_NOW})
        ON CONFLICT (meeting_id, user_id)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = {SQL_NOW}, left_at = NULL,
                      updated_at = {SQL_NOW},
                      display_name = COALESCE($3, meeting_participants.display_name)
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

/// Atomically join a meeting as an attendee, respecting the current
/// `waiting_room_enabled` setting.
///
/// Returns `(auto_admitted, ParticipantRow, waiting_room_enabled)` where
/// `auto_admitted` is `true` when the participant was immediately admitted
/// (waiting room disabled). The third element is the `waiting_room_enabled`
/// value observed under the write lock, which avoids stale reads from a
/// pre-transaction fetch.
///
/// The join is serialized against concurrent waiting-room toggles
/// ([`crate::db::meetings::update_waiting_room_enabled`]): PostgreSQL locks the
/// meeting row with `FOR UPDATE`; SQLite takes the database write lock up front
/// via `BEGIN IMMEDIATE` and retries on `SQLITE_BUSY` (see [`with_busy_retry`]).
pub async fn join_attendee(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<(bool, ParticipantRow, bool), sqlx::Error> {
    with_busy_retry(|| join_attendee_tx(pool, meeting_id, user_id, display_name)).await
}

async fn join_attendee_tx(
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
    display_name: Option<&str>,
) -> Result<(bool, ParticipantRow, bool), sqlx::Error> {
    let mut tx = begin_write_tx(pool).await?;

    // Lock the meeting row to serialize against concurrent waiting room toggles.
    // On SQLite `FOR_UPDATE` is empty (the BEGIN IMMEDIATE write lock suffices).
    let lock_select =
        format!("SELECT waiting_room_enabled FROM meetings WHERE id = $1{FOR_UPDATE}");
    let (waiting_room_enabled,): (bool,) = sqlx::query_as(&lock_select)
        .bind(meeting_id)
        .fetch_one(&mut *tx)
        .await?;

    let row = if waiting_room_enabled {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name)
            VALUES ($1, $2, 'waiting', FALSE, $3)
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'waiting', left_at = NULL, updated_at = {SQL_NOW},
                          display_name = COALESCE($3, meeting_participants.display_name)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
            .fetch_one(&mut *tx)
            .await?
    } else {
        let query = format!(
            r#"
            INSERT INTO meeting_participants (meeting_id, user_id, status, is_host, display_name, admitted_at)
            VALUES ($1, $2, 'admitted', FALSE, $3, {SQL_NOW})
            ON CONFLICT (meeting_id, user_id)
            DO UPDATE SET status = 'admitted', admitted_at = {SQL_NOW}, left_at = NULL,
                          updated_at = {SQL_NOW},
                          display_name = COALESCE($3, meeting_participants.display_name)
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        );
        sqlx::query_as::<_, ParticipantRow>(&query)
            .bind(meeting_id)
            .bind(user_id)
            .bind(display_name)
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
    sqlx::query_as::<_, ParticipantRow>(&query)
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
    sqlx::query_as::<_, ParticipantRow>(&query)
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
    sqlx::query_as::<_, ParticipantRow>(&query)
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
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = {SQL_NOW}, updated_at = {SQL_NOW}
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
pub async fn admit_all(pool: &DbPool, meeting_id: i32) -> Result<Vec<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = {SQL_NOW}, updated_at = {SQL_NOW}
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
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'rejected', updated_at = {SQL_NOW}
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
    pool: &DbPool,
    meeting_id: i32,
    user_id: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'left', left_at = {SQL_NOW}, updated_at = {SQL_NOW}
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

/// Count admitted participants in a meeting.
pub async fn count_admitted(pool: &DbPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'admitted'",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Count waiting participants in a meeting.
pub async fn count_waiting(pool: &DbPool, meeting_id: i32) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meeting_participants WHERE meeting_id = $1 AND status = 'waiting'",
    )
    .bind(meeting_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}
