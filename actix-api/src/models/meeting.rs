use crate::db::get_connection_query;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::default::Default;
use std::error::Error;
use tracing::{error, info};

/// Meeting state as per requirements
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, sqlx::Type, Default)]
#[sqlx(type_name = "VARCHAR", rename_all = "snake_case")]
pub enum MeetingState {
    #[serde(rename = "idle")]
    #[default]
    Idle,
    #[serde(rename = "active")]
    Active,
}

impl std::fmt::Display for MeetingState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeetingState::Idle => write!(f, "idle"),
            MeetingState::Active => write!(f, "active"),
        }
    }
}

impl From<String> for MeetingState {
    fn from(s: String) -> Self {
        match s.as_str() {
            "active" => MeetingState::Active,
            _ => MeetingState::Idle,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct Meeting {
    pub id: i32,
    pub room_id: String,
    pub started_at: DateTime<Utc>, // When the meeting started
    pub ended_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub creator_id: Option<String>,
    pub meeting_title: Option<String>,
    pub password_hash: Option<String>,
    pub waiting_room_enabled: Option<bool>,
    pub meeting_status: Option<String>,
}

impl Meeting {
    /// Get the start time of the meeting in milliseconds
    pub fn start_time_unix_ms(&self) -> i64 {
        self.started_at.timestamp_millis()
    }

    /// Get the current duration of the meeting in milliseconds
    pub fn current_duration_ms(&self) -> i64 {
        match self.ended_at {
            Some(end) => (end - self.started_at).num_milliseconds(),
            None => (Utc::now() - self.started_at).num_milliseconds(),
        }
    }

    /// Check if the meeting is active
    pub fn is_active(&self) -> bool {
        self.ended_at.is_none()
    }

    /// Create or update a meeting with a start time
    pub async fn create(
        room_id: &str,
        started_at: DateTime<Utc>,
        creator_id: Option<String>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let room_id = room_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn = get_connection_query()?;
            let row = conn.query_one(
            "
                INSERT INTO meetings (room_id, started_at, ended_at, creator_id) 
                VALUES ($1, $2, NULL, $3) 
                ON CONFLICT (room_id) DO UPDATE 
                SET started_at = EXCLUDED.started_at, updated_at = NOW() 
                RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id",
                &[&room_id, &started_at, &creator_id],
            )?;

            Ok(Meeting {
                id: row.get("id"),
                room_id: row.get("room_id"),
                started_at: row.get("started_at"),
                ended_at: row.get("ended_at"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
                deleted_at: row.get("deleted_at"),
                creator_id: row.get("creator_id"),
                meeting_title: row.get("meeting_title"),
                password_hash: row.get("password_hash"),
                waiting_room_enabled: row.get("waiting_room_enabled"),
                meeting_status: Some("idle".to_string())

            })
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?
    }

    /// End a meeting by setting ended_at timestamp
    pub fn end_meeting(room_id: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut conn = get_connection_query()?;
        let row = conn.query_one(
            "UPDATE meetings 
             SET ended_at = NOW(), updated_at = NOW() 
             WHERE room_id = $1 AND ended_at IS NULL
             RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id",
            &[&room_id],
        )?;

        error!(
            "Meeting {} ended at: {:?}",
            room_id,
            row.get::<_, Option<DateTime<Utc>>>("ended_at")
        );

        Ok(Meeting {
            id: row.get("id"),
            room_id: row.get("room_id"),
            started_at: row.get("started_at"),
            ended_at: row.get("ended_at"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            deleted_at: row.get("deleted_at"),
            creator_id: row.get("creator_id"),
            meeting_title: row.get("meeting_title"),
            password_hash: row.get("password_hash"),
            waiting_room_enabled: row.get("waiting_room_enabled"),
            meeting_status: Some("ended".to_string()),
        })
    }

    /// Get meeting by room_id
    pub async fn get_by_room_id(
        room_id: &str,
    ) -> Result<Option<Self>, Box<dyn Error + Send + Sync>> {
        let room_id = room_id.to_string();

        tokio::task::spawn_blocking(move || {
            let mut conn = get_connection_query()?;
            let rows = conn.query(
                "SELECT id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id 
                FROM meetings 
                WHERE room_id = $1 AND deleted_at IS NULL",
                &[&room_id],
            )?;

            if rows.is_empty() {
                Ok(None)
            } else {
                let row = &rows[0];
                Ok(Some(Meeting {
                    id: row.get("id"),
                    room_id: row.get("room_id"),
                    started_at: row.get("started_at"),
                    ended_at: row.get("ended_at"),
                    created_at: row.get("created_at"),
                    updated_at: row.get("updated_at"),
                    deleted_at: row.get("deleted_at"),
                    creator_id: row.get("creator_id"),
                    meeting_status: row.get("meeting_status"),
                    meeting_title: row.get("meeting_title"),
                    password_hash: row.get("password_hash"),
                    waiting_room_enabled: row.get("waiting_room_enabled"),
                }))
            }
        })
        .await
        .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)?
    }

    /// Soft delete a meeting
    pub fn delete_by_room_id(room_id: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut conn = get_connection_query()?;
        let row = conn.query_one(
            "UPDATE meetings 
             SET deleted_at = NOW()
             WHERE room_id = $1
             RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id",
            &[&room_id],
        )?;

        Ok(Meeting {
            id: row.get("id"),
            room_id: row.get("room_id"),
            started_at: row.get("started_at"),
            ended_at: row.get("ended_at"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
            deleted_at: row.get("deleted_at"),
            creator_id: row.get("creator_id"),
            meeting_title: row.get("meeting_title"),
            password_hash: row.get("password_hash"),
            waiting_room_enabled: row.get("waiting_room_enabled"),
            meeting_status: row.get("meeting_status"),
        })
    }

    /// Get or create a meeting - used when first user joins
    pub async fn get_or_create(room_id: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match Self::get_by_room_id(room_id).await? {
            Some(meeting) => Ok(meeting),
            None => {
                let now = Utc::now();
                Self::create(room_id, now, None).await
            }
        }
    }

    pub fn get_meeting_start_time(
        room_id: &str,
    ) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
        let mut conn = get_connection_query()?;
        let rows = conn.query(
            "SELECT started_at FROM meetings WHERE room_id = $1 AND ended_at IS NULL",
            &[&room_id],
        )?;

        if rows.is_empty() {
            Ok(None)
        } else {
            let started_at: DateTime<Utc> = rows[0].get("started_at");
            Ok(Some(started_at.timestamp_millis()))
        }
    }

    // =========================================================================
    // Async sqlx methods - preferred for new code
    // =========================================================================

    /// Create a meeting using sqlx (async, no spawn_blocking)
    pub async fn create_async(
        pool: &PgPool,
        room_id: &str,
        creator_id: Option<&str>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let now = Utc::now();
        let meeting = sqlx::query_as::<_, Meeting>(
            r#"
            INSERT INTO meetings (room_id, started_at, ended_at, creator_id, meeting_status)
            VALUES ($1, $2, NULL, $3, 'not_started')
            ON CONFLICT (room_id) DO UPDATE
            SET started_at = CASE
                WHEN meetings.ended_at IS NOT NULL THEN EXCLUDED.started_at
                ELSE meetings.started_at
            END,
            ended_at = NULL,
            creator_id = CASE
                WHEN meetings.ended_at IS NOT NULL THEN EXCLUDED.creator_id
                ELSE meetings.creator_id
            END,
            updated_at = NOW()
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id,
                      meeting_title, password_hash, waiting_room_enabled, meeting_status
            "#,
        )
        .bind(room_id)
        .bind(now)
        .bind(creator_id)
        .fetch_one(pool)
        .await?;

        info!(
            "Meeting created/updated for room {} by {:?}",
            room_id, creator_id
        );
        Ok(meeting)
    }

    /// Get meeting by room_id using sqlx (async, no spawn_blocking)
    pub async fn get_by_room_id_async(
        pool: &PgPool,
        room_id: &str,
    ) -> Result<Option<Self>, Box<dyn Error + Send + Sync>> {
        let meeting = sqlx::query_as::<_, Meeting>(
            r#"
            SELECT id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id,
                   meeting_title, password_hash, waiting_room_enabled, meeting_status
            FROM meetings
            WHERE room_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await?;

        Ok(meeting)
    }

    /// End a meeting using sqlx (async, no spawn_blocking)
    pub async fn end_meeting_async(
        pool: &PgPool,
        room_id: &str,
    ) -> Result<Option<Self>, Box<dyn Error + Send + Sync>> {
        let meeting = sqlx::query_as::<_, Meeting>(
            r#"
            UPDATE meetings
            SET ended_at = NOW(), updated_at = NOW(), meeting_status = 'ended'
            WHERE room_id = $1 AND ended_at IS NULL
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id,
                      meeting_title, password_hash, waiting_room_enabled, meeting_status
            "#,
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await?;

        if let Some(ref m) = meeting {
            info!("Meeting {} ended at: {:?}", room_id, m.ended_at);
        }
        Ok(meeting)
    }

    /// Check if a meeting with the given room_id already exists
    pub async fn exists_async(
        pool: &PgPool,
        room_id: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let result = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*) FROM meetings WHERE room_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_one(pool)
        .await?;

        Ok(result > 0)
    }

    /// Create a new meeting via the Create Meeting API
    /// This creates the meeting metadata at request time (not at start time)
    /// The meeting starts in 'not_started' state
    pub async fn create_meeting_api<'e, E>(
        executor: E,
        room_id: &str,
        host_id: &str,
        password_hash: Option<&str>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>>
    where
        E: sqlx::Executor<'e, Database = sqlx::Postgres>,
    {
        let now = Utc::now();
        let meeting = sqlx::query_as::<_, Meeting>(
            r#"
            INSERT INTO meetings (room_id, started_at, creator_id, password_hash, meeting_status)
            VALUES ($1, $2, $3, $4, 'not_started')
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id,
                      meeting_title, password_hash, waiting_room_enabled, meeting_status
            "#,
        )
        .bind(room_id)
        .bind(now)
        .bind(host_id)
        .bind(password_hash)
        .fetch_one(executor)
        .await?;

        info!(
            "Meeting created via API for room {} by host {}",
            room_id, host_id
        );
        Ok(meeting)
    }
}
