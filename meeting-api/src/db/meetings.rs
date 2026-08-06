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
//! Shared across the PostgreSQL and SQLite backends. `$N` placeholders are
//! parsed natively by both sqlx drivers, so the query strings are identical;
//! the only backend-specific pieces come from [`crate::db`] helpers
//! ([`SQL_NOW`], [`begin_write_tx`], [`with_busy_retry`]).
//!
//! `updated_at` is written explicitly in every UPDATE. On PostgreSQL an
//! AFTER-UPDATE trigger would also maintain it (the double-write is harmless);
//! the SQLite migration drops that trigger because SQLite computes `RETURNING`
//! *before* AFTER-triggers fire, which would otherwise return a stale value.

use crate::db::{begin_write_tx, with_busy_retry, DbPool, MeetingRow, SQL_NOW};
use serde_json::Value as JsonValue;

/// Create a new meeting.
///
/// A plain INSERT: a duplicate active room violates the partial unique index
/// (`idx_meetings_room_id_unique_active`) and surfaces as a unique-violation
/// error that the caller handles.
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
    let query = format!(
        r#"
        INSERT INTO meetings (room_id, creator_id, started_at, password_hash, state, attendees, waiting_room_enabled)
        VALUES ($1, $2, {SQL_NOW}, $3, 'idle', $4, $5)
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#
    );
    sqlx::query_as::<_, MeetingRow>(&query)
        .bind(room_id)
        .bind(creator_id)
        .bind(password_hash)
        .bind(attendees)
        .bind(waiting_room_enabled)
        .fetch_one(pool)
        .await
}

/// Get a non-deleted meeting by room_id.
pub async fn get_by_room_id(
    pool: &DbPool,
    room_id: &str,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    sqlx::query_as::<_, MeetingRow>(
        r#"
        SELECT id, room_id, started_at, ended_at, created_at, updated_at,
               deleted_at, creator_id, password_hash, state, attendees, host_display_name,
               waiting_room_enabled
        FROM meetings
        WHERE room_id = $1 AND deleted_at IS NULL
        "#,
    )
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
    sqlx::query_as::<_, MeetingRow>(
        r#"
        SELECT id, room_id, started_at, ended_at, created_at, updated_at,
               deleted_at, creator_id, password_hash, state, attendees, host_display_name,
               waiting_room_enabled
        FROM meetings
        WHERE deleted_at IS NULL AND creator_id = $1
        ORDER BY created_at DESC
        LIMIT $2 OFFSET $3
        "#,
    )
    .bind(creator_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
}

/// Count meetings owned by `creator_id` (non-deleted).
pub async fn count_by_owner(pool: &DbPool, creator_id: &str) -> Result<i64, sqlx::Error> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM meetings WHERE deleted_at IS NULL AND creator_id = $1",
    )
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
    let query = format!(
        r#"
        UPDATE meetings
        SET deleted_at = {SQL_NOW}, updated_at = {SQL_NOW}
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#
    );
    sqlx::query_as::<_, MeetingRow>(&query)
        .bind(room_id)
        .bind(creator_id)
        .fetch_optional(pool)
        .await
}

/// Activate a meeting (set state to 'active').
pub async fn activate(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let query =
        format!("UPDATE meetings SET state = 'active', updated_at = {SQL_NOW} WHERE id = $1");
    sqlx::query(&query).bind(meeting_id).execute(pool).await?;
    Ok(())
}

/// End a meeting (set state to 'ended', set ended_at).
pub async fn end_meeting(pool: &DbPool, meeting_id: i32) -> Result<(), sqlx::Error> {
    let query = format!(
        "UPDATE meetings SET state = 'ended', ended_at = {SQL_NOW}, updated_at = {SQL_NOW} WHERE id = $1"
    );
    sqlx::query(&query).bind(meeting_id).execute(pool).await?;
    Ok(())
}

/// Update the cached host display name.
pub async fn set_host_display_name(
    pool: &DbPool,
    meeting_id: i32,
    display_name: &str,
) -> Result<(), sqlx::Error> {
    let query =
        format!("UPDATE meetings SET host_display_name = $1, updated_at = {SQL_NOW} WHERE id = $2");
    sqlx::query(&query)
        .bind(display_name)
        .bind(meeting_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Atomically update the waiting_room_enabled setting for a meeting.
/// When disabling the waiting room, auto-admits all currently waiting participants
/// within the same transaction to prevent race conditions.
///
/// PostgreSQL serializes concurrent joins via the `FOR UPDATE` row lock taken in
/// [`crate::db::participants::join_attendee`]; SQLite serializes via
/// `BEGIN IMMEDIATE` plus a busy retry (see [`with_busy_retry`]).
pub async fn update_waiting_room_enabled(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    enabled: bool,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    with_busy_retry(|| update_waiting_room_enabled_tx(pool, room_id, creator_id, enabled)).await
}

async fn update_waiting_room_enabled_tx(
    pool: &DbPool,
    room_id: &str,
    creator_id: &str,
    enabled: bool,
) -> Result<Option<MeetingRow>, sqlx::Error> {
    let mut tx = begin_write_tx(pool).await?;

    let update_meeting = format!(
        r#"
        UPDATE meetings
        SET waiting_room_enabled = $3, updated_at = {SQL_NOW}
        WHERE room_id = $1 AND creator_id = $2 AND deleted_at IS NULL
        RETURNING id, room_id, started_at, ended_at, created_at, updated_at,
                  deleted_at, creator_id, password_hash, state, attendees, host_display_name,
                  waiting_room_enabled
        "#
    );
    let updated = sqlx::query_as::<_, MeetingRow>(&update_meeting)
        .bind(room_id)
        .bind(creator_id)
        .bind(enabled)
        .fetch_optional(&mut *tx)
        .await?;

    // When disabling the waiting room, admit everyone currently waiting.
    if let Some(ref row) = updated {
        if !enabled {
            let admit_all = format!(
                "UPDATE meeting_participants SET status = 'admitted', admitted_at = {SQL_NOW}, updated_at = {SQL_NOW} \
                 WHERE meeting_id = $1 AND status = 'waiting'"
            );
            sqlx::query(&admit_all)
                .bind(row.id)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(updated)
}
