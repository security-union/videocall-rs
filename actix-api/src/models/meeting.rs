use crate::db::get_connection_query;
use argon2::{
    password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
    Argon2,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::PgPool;
use std::error::Error;
use std::fmt;
use tracing::{error, info};

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
    pub password_hash: Option<String>,
    pub state: Option<String>,
    pub attendees: Option<JsonValue>,
    pub host_display_name: Option<String>,
}

#[derive(Debug)]
pub enum CreateMeetingError {
    MeetingExists,
    DatabaseError(String),
    HashError(String),
}

impl fmt::Display for CreateMeetingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CreateMeetingError::MeetingExists => write!(f, "Meeting already exists"),
            CreateMeetingError::DatabaseError(e) => write!(f, "Database error: {}", e),
            CreateMeetingError::HashError(e) => write!(f, "Hash error: {}", e),
        }
    }
}

impl Error for CreateMeetingError {}

impl Meeting {
    /// Get the start time of the meeting in milliseconds
    pub fn start_time_unix_ms(&self) -> i64 {
        self.started_at.timestamp_millis()
    }

    /// Get the current duration of the meeting in milliseconds
    pub fn current_durtion_ms(&self) -> i64 {
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
                password_hash: None,
                state: None,
                attendees: None,
                host_display_name: None,
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
            password_hash: None,
            state: None,
            attendees: None,
            host_display_name: None,
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
                    password_hash: None,
                    state: None,
                    attendees: None,
                    host_display_name: None,
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
            password_hash: None,
            state: None,
            attendees: None,
            host_display_name: None,
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
            INSERT INTO meetings (room_id, started_at, ended_at, creator_id)
            VALUES ($1, $2, NULL, $3)
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
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id
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
            SELECT id, room_id, started_at, ended_at, created_at, updated_at, deleted_at,
                   creator_id, password_hash, state, attendees, host_display_name
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
            SET ended_at = NOW(), updated_at = NOW()
            WHERE room_id = $1 AND ended_at IS NULL
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id
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

    /// Create a meeting via the API - checks for duplicates before insert
    /// This is different from create_async which uses upsert semantics
    pub async fn create_meeting_api(
        pool: &PgPool,
        room_id: &str,
        creator_id: &str,
        attendees: &[String],
        password: Option<&str>,
    ) -> Result<Self, CreateMeetingError> {
        // Hash password if provided
        let password_hash = match password {
            Some(pwd) if !pwd.is_empty() => {
                let salt = SaltString::generate(&mut OsRng);
                let argon2 = Argon2::default();
                let hash = argon2
                    .hash_password(pwd.as_bytes(), &salt)
                    .map_err(|e| CreateMeetingError::HashError(e.to_string()))?;
                Some(hash.to_string())
            }
            _ => None,
        };

        let attendees_json = serde_json::to_value(attendees)
            .map_err(|e| CreateMeetingError::DatabaseError(e.to_string()))?;

        let now = Utc::now();

        // First check if meeting exists (not deleted)
        let existing = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| CreateMeetingError::DatabaseError(e.to_string()))?;

        if existing.is_some() {
            return Err(CreateMeetingError::MeetingExists);
        }

        // Insert new meeting
        let meeting = sqlx::query_as::<_, Meeting>(
            r#"
            INSERT INTO meetings (room_id, started_at, creator_id, password_hash, state, attendees)
            VALUES ($1, $2, $3, $4, 'idle', $5)
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at,
                      creator_id, password_hash, state, attendees
            "#,
        )
        .bind(room_id)
        .bind(now)
        .bind(creator_id)
        .bind(&password_hash)
        .bind(&attendees_json)
        .fetch_one(pool)
        .await
        .map_err(|e| CreateMeetingError::DatabaseError(e.to_string()))?;

        info!(
            "Meeting '{}' created via API by '{}' (has_password: {})",
            room_id,
            creator_id,
            password_hash.is_some()
        );

        Ok(meeting)
    }

    /// List all active meetings (not deleted, not ended)
    pub async fn list_active_async(
        pool: &PgPool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Self>, Box<dyn Error + Send + Sync>> {
        let meetings = sqlx::query_as::<_, Meeting>(
            r#"
            SELECT id, room_id, started_at, ended_at, created_at, updated_at, deleted_at,
                   creator_id, password_hash, state, attendees, host_display_name
            FROM meetings
            WHERE deleted_at IS NULL AND ended_at IS NULL
            ORDER BY created_at DESC
            LIMIT $1 OFFSET $2
            "#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok(meetings)
    }

    /// Count all active meetings
    pub async fn count_active_async(pool: &PgPool) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM meetings WHERE deleted_at IS NULL AND ended_at IS NULL",
        )
        .fetch_one(pool)
        .await?;

        Ok(count)
    }

    /// List meetings owned by a specific user (excludes deleted, includes ended)
    pub async fn list_by_owner_async(
        pool: &PgPool,
        owner_email: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Self>, Box<dyn Error + Send + Sync>> {
        let meetings = sqlx::query_as::<_, Meeting>(
            r#"
            SELECT id, room_id, started_at, ended_at, created_at, updated_at, deleted_at,
                   creator_id, password_hash, state, attendees, host_display_name
            FROM meetings
            WHERE deleted_at IS NULL AND creator_id = $1
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(owner_email)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;

        Ok(meetings)
    }

    /// Count meetings owned by a specific user (excludes deleted, includes ended)
    pub async fn count_by_owner_async(
        pool: &PgPool,
        owner_email: &str,
    ) -> Result<i64, Box<dyn Error + Send + Sync>> {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM meetings WHERE deleted_at IS NULL AND creator_id = $1",
        )
        .bind(owner_email)
        .fetch_one(pool)
        .await?;

        Ok(count)
    }
}
