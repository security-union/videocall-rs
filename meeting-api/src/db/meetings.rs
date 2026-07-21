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
//! Every `UPDATE` sets `updated_at` explicitly. On SQLite that is the only thing
//! maintaining the column (no `updated_at` trigger, because SQLite evaluates
//! `RETURNING` before AFTER-triggers). On PostgreSQL the `BEFORE UPDATE` trigger
//! overwrites it with the same `transaction_timestamp()`, so the write is
//! redundant but not divergent — do not drop it, SQLite depends on it.

use chrono::{DateTime, Utc};
use serde_json::Value as JsonValue;

use crate::db::{bind_now, lock, now_sql, q, with_retry, DbPool};

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

const MEETING_COLUMNS: &str = "id, room_id, started_at, ended_at, created_at, updated_at, \
    deleted_at, creator_id, password_hash, state, attendees, host_display_name, \
    waiting_room_enabled";

/// Render a write statement: substitute `{cols}` and `{now}` (at `slot`), rewrite
/// placeholders. Pair with `bind_now!`.
fn stmt(template: &str, slot: usize) -> String {
    now_sql(&template.replace("{cols}", MEETING_COLUMNS), slot)
}

/// Create a new meeting.
///
/// A `room_id` already live violates `idx_meetings_room_id_unique_active` and
/// surfaces as a unique violation; the index is partial, so a `room_id` frees up
/// once its meeting is soft-deleted.
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
    // `created_at` / `updated_at` are left to the column DEFAULTs (the DB clock).
    let sql = stmt(
        "INSERT INTO meetings (room_id, creator_id, started_at, password_hash, state, attendees, waiting_room_enabled)
         VALUES ($1, $2, {now}, $3, 'idle', $4, $5)
         RETURNING {cols}",
        6,
    );
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
    sqlx::query_as::<_, MeetingRow>(&q(&format!(
        "SELECT {MEETING_COLUMNS} FROM meetings WHERE room_id = $1 AND deleted_at IS NULL"
    )))
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
    sqlx::query_as::<_, MeetingRow>(&q(&format!(
        "SELECT {MEETING_COLUMNS} FROM meetings WHERE deleted_at IS NULL AND creator_id = $1 \
         ORDER BY created_at DESC LIMIT $2 OFFSET $3"
    )))
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
    let sql = stmt(
        "UPDATE meetings SET deleted_at = {now}, updated_at = {now}
         WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
         RETURNING {cols}",
        3,
    );
    bind_now!(sqlx::query_as::<_, MeetingRow>(&sql)
        .bind(room_id)
        .bind(creator_id))
    .fetch_optional(pool)
    .await
}

/// Activate a meeting (set state to 'active').
pub async fn activate(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let sql = now_sql(
        "UPDATE meetings SET state = 'active', updated_at = {now} WHERE id = $1",
        2,
    );
    bind_now!(sqlx::query(&sql).bind(meeting_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// End a meeting (set state to 'ended', set ended_at).
pub async fn end_meeting(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let sql = now_sql(
        "UPDATE meetings SET state = 'ended', ended_at = {now}, updated_at = {now} WHERE id = $1",
        2,
    );
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
    let sql = now_sql(
        "UPDATE meetings SET host_display_name = $1, updated_at = {now} WHERE id = $2",
        3,
    );
    bind_now!(sqlx::query(&sql).bind(display_name).bind(meeting_id))
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically update `waiting_room_enabled`, admitting everyone waiting when it
/// is turned off — in one transaction so a concurrent join cannot strand.
///
/// Takes the write lock up front via [`lock::begin_write`], matching
/// [`crate::db::participants::join_attendee`]; only one side being immediate
/// would move the race, not close it.
pub async fn update_waiting_room_enabled(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    enabled: bool,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    with_retry! {
        let mut tx = lock::begin_write(pool).await?;

        let sql = stmt(
            "UPDATE meetings SET waiting_room_enabled = $3, updated_at = {now}
             WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
             RETURNING {cols}",
            4,
        );
        let updated = bind_now!(sqlx::query_as::<_, MeetingRow>(&sql)
            .bind(room_id)
            .bind(creator_id)
            .bind(enabled))
        .fetch_optional(&mut *tx)
        .await?;

        if let Some(ref row) = updated {
            if !enabled {
                let sql = now_sql(
                    "UPDATE meeting_participants SET status = 'admitted', admitted_at = {now}, \
                     updated_at = {now} WHERE meeting_id = $1 AND status = 'waiting'",
                    2,
                );
                bind_now!(sqlx::query(&sql).bind(row.id))
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        Ok(updated)
    }
}
