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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::error::Error;
use tracing::info;

/// Maximum number of attendees allowed per meeting
pub const MAX_ATTENDEES: usize = 100;

/// Pre-registered attendee for a meeting
/// Attendees are stored when the meeting is created via the Create Meeting API
#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct MeetingAttendee {
    pub id: i32,
    pub meeting_id: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
}

impl MeetingAttendee {
    /// Add multiple attendees to a meeting
    /// Returns the number of attendees added
    pub async fn add_attendees<'e, E>(
        executor: E,
        meeting_id: &str,
        user_ids: &[String],
    ) -> Result<usize, Box<dyn Error + Send + Sync>>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        if user_ids.is_empty() {
            return Ok(0);
        }

        if user_ids.len() > MAX_ATTENDEES {
            return Err(format!(
                "Too many attendees: {} exceeds maximum of {}",
                user_ids.len(),
                MAX_ATTENDEES
            )
            .into());
        }

        let mut query_builder =
            sqlx::QueryBuilder::new("INSERT INTO meeting_attendees (meeting_id, user_id) ");

        query_builder.push_values(user_ids.iter(), |mut b, user_id| {
            b.push_bind(meeting_id).push_bind(user_id);
        });

        query_builder.push(" ON CONFLICT (meeting_id, user_id) DO NOTHING");

        let result = query_builder.build().execute(executor).await?;

        let count = result.rows_affected() as usize;

        info!("Added {} attendees to meeting {}", count, meeting_id);
        Ok(count)
    }

    /// Get all attendees for a meeting
    pub async fn get_attendees(
        pool: &PgPool,
        meeting_id: &str,
    ) -> Result<Vec<Self>, Box<dyn Error + Send + Sync>> {
        let attendees = sqlx::query_as::<_, MeetingAttendee>(
            r#"
            SELECT id, meeting_id, user_id, created_at
            FROM meeting_attendees
            WHERE meeting_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await?;

        Ok(attendees)
    }

    /// Get attendee user IDs for a meeting
    pub async fn get_attendee_ids(
        pool: &PgPool,
        meeting_id: &str,
    ) -> Result<Vec<String>, Box<dyn Error + Send + Sync>> {
        let ids = sqlx::query_scalar::<_, String>(
            r#"
            SELECT user_id
            FROM meeting_attendees
            WHERE meeting_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await?;

        Ok(ids)
    }

    /// Check if a user is an attendee of a meeting
    pub async fn is_attendee(
        pool: &PgPool,
        meeting_id: &str,
        user_id: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM meeting_attendees
            WHERE meeting_id = $1 AND user_id = $2
            "#,
        )
        .bind(meeting_id)
        .bind(user_id)
        .fetch_one(pool)
        .await?;

        Ok(count > 0)
    }

    /// Get the count of attendees for a meeting
    pub async fn count_attendees(
        pool: &PgPool,
        meeting_id: &str,
    ) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM meeting_attendees
            WHERE meeting_id = $1
            "#,
        )
        .bind(meeting_id)
        .fetch_one(pool)
        .await?;

        Ok(count)
    }

    /// Remove an attendee from a meeting
    pub async fn remove_attendee(
        pool: &PgPool,
        meeting_id: &str,
        user_id: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query(
            r#"
            DELETE FROM meeting_attendees
            WHERE meeting_id = $1 AND user_id = $2
            "#,
        )
        .bind(meeting_id)
        .bind(user_id)
        .execute(pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Remove all attendees from a meeting
    pub async fn remove_all_attendees(
        pool: &PgPool,
        meeting_id: &str,
    ) -> Result<u64, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query(
            r#"
            DELETE FROM meeting_attendees
            WHERE meeting_id = $1
            "#,
        )
        .bind(meeting_id)
        .execute(pool)
        .await?;

        Ok(result.rows_affected())
    }
}
