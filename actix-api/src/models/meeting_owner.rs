use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tracing::info;

use crate::db::get_connection_query;

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct MeetingOwner {
    pub id: i32,
    pub meeting_id: String,
    pub user_id: String,
    pub delegated_by: Option<String>,
    pub delegated_at: Option<DateTime<Utc>>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MeetingOwner {
    pub async fn is_owner(
        pool: &PgPool,
        meeting_id: &str,
        user_id: &str,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM meeting_owners
            WHERE meeting_id = $1 AND user_id = $2 AND is_active = true
            "#,
        )
        .bind(meeting_id)
        .bind(user_id)
        .fetch_one(pool)
        .await?;

        Ok(count > 0)
    }

    pub async fn create<'e, E>(
        executor: E,
        meeting_id: &str,
        user_id: &str,
        delegated_by: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let now = Utc::now();
        let owner = sqlx::query_as::<_, MeetingOwner>(
            r#"
            INSERT INTO meeting_owners (meeting_id, user_id, delegated_by, delegated_at, is_active)
            VALUES ($1, $2, $3, $4, true)
            ON CONFLICT (meeting_id, user_id) DO UPDATE
            SET is_active = true, updated_at = NOW()
            RETURNING id, meeting_id, user_id, delegated_by, delegated_at, is_active, created_at, updated_at
            "#,
        )
        .bind(meeting_id)
        .bind(user_id)
        .bind(delegated_by)
        .bind(now)
        .fetch_one(executor)
        .await?;

        info!(
            "Meeting owner created: user {} for meeting {}",
            user_id, meeting_id
        );
        Ok(owner)
    }

    pub async fn get_owners(meeting_id: &str) -> Result<Vec<Self>, Box<dyn std::error::Error>> {
        let mut conn = get_connection_query()?;
        let query = conn.query(
            "SELECT * FROM meeting_owners WHERE meeting_id = $1 AND is_active = true ORDER BY created_at ASC",
            &[&meeting_id],
        )?;

        Ok(query
            .iter()
            .map(|row| MeetingOwner {
                id: row.get("id"),
                meeting_id: row.get("meeting_id"),
                user_id: row.get("user_id"),
                delegated_by: row.get("delegated_by"),
                delegated_at: row.get("delegated_at"),
                is_active: row.get("is_active"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }
}
