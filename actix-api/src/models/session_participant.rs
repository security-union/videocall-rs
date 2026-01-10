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

//! Session participant tracking - DB as single source of truth
//!
//! All participant state lives in PostgreSQL. No in-memory HashMaps.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tracing::info;

/// Represents a participant in a meeting session
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionParticipant {
    pub id: i32,
    pub room_id: String,
    pub user_id: String,
    pub joined_at: DateTime<Utc>,
    pub left_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl SessionParticipant {
    /// Add a participant to a room (upsert - resets left_at if rejoining)
    pub async fn join(pool: &PgPool, room_id: &str, user_id: &str) -> Result<Self, sqlx::Error> {
        let participant = sqlx::query_as::<_, SessionParticipant>(
            r#"
            INSERT INTO session_participants (room_id, user_id, joined_at, left_at)
            VALUES ($1, $2, NOW(), NULL)
            ON CONFLICT (room_id, user_id) DO UPDATE
            SET joined_at = NOW(), left_at = NULL
            RETURNING id, room_id, user_id, joined_at, left_at, created_at
            "#,
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_one(pool)
        .await?;

        info!(
            "Participant {} joined room {} (id={})",
            user_id, room_id, participant.id
        );
        Ok(participant)
    }

    /// Mark a participant as left
    pub async fn leave(
        pool: &PgPool,
        room_id: &str,
        user_id: &str,
    ) -> Result<Option<Self>, sqlx::Error> {
        let participant = sqlx::query_as::<_, SessionParticipant>(
            r#"
            UPDATE session_participants
            SET left_at = NOW()
            WHERE room_id = $1 AND user_id = $2 AND left_at IS NULL
            RETURNING id, room_id, user_id, joined_at, left_at, created_at
            "#,
        )
        .bind(room_id)
        .bind(user_id)
        .fetch_optional(pool)
        .await?;

        if let Some(ref p) = participant {
            info!(
                "Participant {} left room {} (id={})",
                user_id, room_id, p.id
            );
        }
        Ok(participant)
    }

    /// Count active participants in a room
    pub async fn count_active(pool: &PgPool, room_id: &str) -> Result<i64, sqlx::Error> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) as count FROM session_participants
            WHERE room_id = $1 AND left_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_one(pool)
        .await?;

        Ok(row.0)
    }

    /// Check if this is the first participant (count was 0 before joining)
    pub async fn is_first_participant(pool: &PgPool, room_id: &str) -> Result<bool, sqlx::Error> {
        let count = Self::count_active(pool, room_id).await?;
        Ok(count == 1)
    }

    /// Clean up all participants in a room (when meeting ends)
    pub async fn leave_all(pool: &PgPool, room_id: &str) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE session_participants
            SET left_at = NOW()
            WHERE room_id = $1 AND left_at IS NULL
            "#,
        )
        .bind(room_id)
        .execute(pool)
        .await?;

        let affected = result.rows_affected();
        info!(
            "Marked {} participants as left in room {}",
            affected, room_id
        );
        Ok(affected)
    }
}
