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
//!
//! # `updated_at` on UPDATE
//!
//! Every `UPDATE` here sets `updated_at` explicitly, via [`crate::db::now_expr`].
//! On SQLite that is the only thing maintaining the column: the port has no
//! `updated_at` trigger, because SQLite evaluates `RETURNING` *before*
//! AFTER-triggers fire and a trigger-driven value would come back stale.
//!
//! On PostgreSQL the write is redundant — the `update_meetings_updated_at`
//! `BEFORE UPDATE` trigger overwrites it. It is not, however, a *divergent*
//! write: `NOW()` is `transaction_timestamp()`, so the statement and the trigger
//! that fires inside it produce the identical value. That is what lets the same
//! SQL serve both backends without changing what PostgreSQL stores. Do not
//! "optimize" these away — SQLite depends on them.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::db::{bind_now, lock, now_expr, q, DbPool};

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

/// Create a new meeting.
///
/// A `room_id` already in use by a live meeting violates the partial unique
/// index `idx_meetings_room_id_unique_active` and surfaces as a unique-violation
/// error for the caller to map; the index is partial, so a `room_id` becomes
/// available again once its meeting is soft-deleted.
pub async fn create(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    password_hash: Option<&str>,
    attendees: &JsonValue,
) -> Result<MeetingRow, sqlx::Error> {
    create_with_options(pool, room_id, creator_id, password_hash, attendees, true).await
}

/// Create a new meeting with explicit waiting_room_enabled setting.
pub async fn create_with_options(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    password_hash: Option<&str>,
    attendees: &JsonValue,
    waiting_room_enabled: bool,
) -> Result<MeetingRow, sqlx::Error> {
    // `created_at` / `updated_at` are left to the column DEFAULTs on both
    // backends, which is the database's clock in each case.
    let sql = format!(
        r#"
        INSERT INTO meetings (room_id, creator_id, started_at, password_hash, state, attendees, waiting_room_enabled)
        VALUES ($1, $2, {now}, $3, 'idle', $4, $5)
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#,
        now = now_expr(6)
    );
    let sql = q(&sql);
    bind_now!(sqlx::query_as::<_, MeetingRow>(&sql)
        .bind(room_id)
        .bind(creator_id)
        .bind(password_hash)
        .bind(attendees)
        .bind(waiting_room_enabled))
    .fetch_one(pool)
    .await
}

/// Get a non-deleted meeting by room_id.
pub async fn get_by_room_id(
    pool: &DbPool,
    room_id: &str,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(&q(r#"
        SELECT id, room_id, started_at, ended_at, created_at, updated_at,
               deleted_at, creator_id, password_hash, state, attendees, host_display_name,
               waiting_room_enabled
        FROM meetings
        WHERE room_id = $1 AND deleted_at IS NULL
        "#))
    .bind(room_id)
    .fetch_optional(pool)
    .await
}

/// List meetings owned by `creator_id` (non-deleted), ordered by created_at DESC.
pub async fn list_by_owner(
    pool: &DbPool,
    creator_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(&q(r#"
        SELECT id, room_id, started_at, ended_at, created_at, updated_at,
               deleted_at, creator_id, password_hash, state, attendees, host_display_name,
               waiting_room_enabled
        FROM meetings
        WHERE deleted_at IS NULL AND creator_id = $1
        ORDER BY created_at DESC
        LIMIT $2 OFFSET $3
        "#))
    .bind(creator_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Count meetings owned by `creator_id` (non-deleted).
pub async fn count_by_owner(pool: &DbPool, creator_id: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(&q(
        "SELECT COUNT(*) FROM meetings WHERE deleted_at IS NULL AND creator_id = $1",
    ))
    .bind(creator_id)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

/// Soft-delete a meeting (set `deleted_at`).
pub async fn soft_delete(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    let sql = format!(
        r#"
        UPDATE meetings
        SET deleted_at = {now}, updated_at = {now}
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#,
        now = now_expr(3)
    );
    let sql = q(&sql);
    bind_now!(sqlx::query_as::<_, MeetingRow>(&sql)
        .bind(room_id)
        .bind(creator_id))
    .fetch_optional(pool)
    .await
}

/// Activate a meeting (set state to 'active').
pub async fn activate(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE meetings SET state = 'active', updated_at = {now} WHERE id = $1",
        now = now_expr(2)
    );
    let sql = q(&sql);
    bind_now!(sqlx::query(&sql).bind(meeting_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// End a meeting (set state to 'ended', set ended_at).
pub async fn end_meeting(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE meetings SET state = 'ended', ended_at = {now}, updated_at = {now} WHERE id = $1",
        now = now_expr(2)
    );
    let sql = q(&sql);
    bind_now!(sqlx::query(&sql).bind(meeting_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// Update the cached host display name.
pub async fn set_host_display_name(
    pool: &DbPool,
    meeting_id: i32,
    display_name: &str,
) -> Result<(), sqlx::Error> {
    let sql = format!(
        "UPDATE meetings SET host_display_name = $1, updated_at = {now} WHERE id = $2",
        now = now_expr(3)
    );
    let sql = q(&sql);
    bind_now!(sqlx::query(&sql).bind(display_name).bind(meeting_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically update the waiting_room_enabled setting for a meeting.
/// When disabling the waiting room, auto-admits all currently waiting participants
/// within the same transaction to prevent race conditions.
///
/// Opens the transaction through [`lock::begin_write`] so it takes the write lock
/// up front, matching [`crate::db::participants::join_attendee`]. If only one of
/// the two were immediate the race would simply move to the other side.
pub async fn update_waiting_room_enabled(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    enabled: bool,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    lock::with_write_retry(|| {
        Box::pin(update_waiting_room_enabled_txn(
            pool, room_id, creator_id, enabled,
        ))
    })
    .await
}

/// One attempt of [`update_waiting_room_enabled`]. Replayable: a failure rolls
/// the transaction back before returning.
async fn update_waiting_room_enabled_txn(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    enabled: bool,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    let mut tx = lock::begin_write(pool).await?;

    let sql = format!(
        r#"
        UPDATE meetings
        SET waiting_room_enabled = $3, updated_at = {now}
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#,
        now = now_expr(4)
    );
    let sql = q(&sql);
    let updated = bind_now!(sqlx::query_as::<_, MeetingRow>(&sql)
        .bind(room_id)
        .bind(creator_id)
        .bind(enabled))
    .fetch_optional(&mut *tx)
    .await?;

    // When disabling the waiting room, admit everyone currently waiting.
    if let Some(ref row) = updated {
        if !enabled {
            let sql = format!(
                "UPDATE meeting_participants SET status = 'admitted', admitted_at = {now}, \
                 updated_at = {now} WHERE meeting_id = $1 AND status = 'waiting'",
                now = now_expr(2)
            );
            let sql = q(&sql);
            bind_now!(sqlx::query(&sql).bind(row.id))
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(updated)
}
