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
    pub email: String,
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
    id, meeting_id, email, status, is_host, is_required,
    joined_at, admitted_at, left_at, created_at, updated_at, display_name
"#;

/// Insert or update a participant as host (admitted immediately).
pub async fn upsert_host(
    pool: &PgPool,
    meeting_id: i32,
    email: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, email, status, is_host, display_name, admitted_at)
        VALUES ($1, $2, 'admitted', TRUE, $3, NOW())
        ON CONFLICT (meeting_id, email)
        DO UPDATE SET status = 'admitted', is_host = TRUE, admitted_at = NOW(),
                      display_name = COALESCE($3, meeting_participants.display_name)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
        .bind(display_name)
        .fetch_one(pool)
        .await
}

/// Insert a participant in 'waiting' status (or update if re-joining).
pub async fn upsert_attendee(
    pool: &PgPool,
    meeting_id: i32,
    email: &str,
    display_name: Option<&str>,
) -> Result<ParticipantRow, sqlx::Error> {
    let query = format!(
        r#"
        INSERT INTO meeting_participants (meeting_id, email, status, is_host, display_name)
        VALUES ($1, $2, 'waiting', FALSE, $3)
        ON CONFLICT (meeting_id, email)
        DO UPDATE SET status = 'waiting', left_at = NULL,
                      display_name = COALESCE($3, meeting_participants.display_name)
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
        .bind(display_name)
        .fetch_one(pool)
        .await
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
    email: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        "SELECT {PARTICIPANT_COLUMNS} FROM meeting_participants WHERE meeting_id = $1 AND email = $2"
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
        .fetch_optional(pool)
        .await
}

/// Admit a single participant.
pub async fn admit(
    pool: &PgPool,
    meeting_id: i32,
    email: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'admitted', admitted_at = NOW()
        WHERE meeting_id = $1 AND email = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
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
    email: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'rejected'
        WHERE meeting_id = $1 AND email = $2 AND status = 'waiting'
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
        .fetch_optional(pool)
        .await
}

/// Leave a meeting (set status to 'left').
pub async fn leave(
    pool: &PgPool,
    meeting_id: i32,
    email: &str,
) -> Result<Option<ParticipantRow>, sqlx::Error> {
    let query = format!(
        r#"
        UPDATE meeting_participants
        SET status = 'left', left_at = NOW()
        WHERE meeting_id = $1 AND email = $2 AND status IN ('admitted', 'waiting')
        RETURNING {PARTICIPANT_COLUMNS}
        "#
    );
    sqlx::query_as::<_, ParticipantRow>(&query)
        .bind(meeting_id)
        .bind(email)
        .fetch_optional(pool)
        .await
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
            email: self.email,
            display_name: self.display_name,
            status: self.status,
            is_host: self.is_host,
            joined_at: self.joined_at.timestamp(),
            admitted_at: self.admitted_at.map(|t| t.timestamp()),
            room_token,
        }
    }
}
