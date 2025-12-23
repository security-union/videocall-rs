use crate::{
    constants::DEFAULT_SALT, db::get_connection_query, models::meeting_owner::MeetingOwner,
};
use bcrypt::{hash, verify};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::error::Error;
use tracing::{error, info};

#[derive(Debug, Serialize, Deserialize, Clone)]
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
    pub waiting_room_enabled: bool,
    pub meeting_status: String, // our db enum values are 'not_started', 'active', 'ended'
}

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
    pub fn create(
        room_id: &str,
        started_at: DateTime<Utc>,
        creator_id: Option<String>,
        meeting_title: Option<String>,
        password_hash: Option<String>,
        waiting_room_enabled: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let mut conn = get_connection_query()?;

        let password_hash = if let Some(pwd) = password_hash {
            Some(hash(pwd, DEFAULT_SALT)?)
        } else {
            None
        };

        let meeting_status = if password_hash.is_some() || waiting_room_enabled {
            "not_started"
        } else {
            "active"
        };

        let started_at = Utc::now();

        let row = conn.query_one(
            "
            INSERT INTO meetings (room_id, started_at, ended_at, creator_id, meeting_title, password_hash, waiting_room_enabled, meeting_status) 
            VALUES ($1, $2, NULL, $3, $4, $5, $6, $7) 
            ON CONFLICT (room_id) DO UPDATE 
            SET updated_at = NOW() 
            RETURNING id, room_id, started_at, ended_at, created_at, updated_at, deleted_at, creator_id, meeting_title, password_hash, waiting_room_enabled, meeting_status",
            &[&room_id, &started_at, &creator_id, &meeting_title, &password_hash, &waiting_room_enabled, &meeting_status    ],
        )?;

        let meeting = Meeting {
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
        };

        if let Some(ref creator) = meeting.creator_id {
            match MeetingOwner::create(&meeting.room_id, creator, None) {
                Ok(_) => {
                    info!("Meeting owner created: {}", creator);
                }
                Err(e) => {
                    error!("Failed to create meeting owner: {}", e);
                }
            }
        }

        Ok(meeting)
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
            meeting_status: row.get("meeting_status"),
        })
    }

    /// Get meeting by room_id
    pub fn get_by_room_id(room_id: &str) -> Result<Option<Self>, Box<dyn Error + Send + Sync>> {
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
                meeting_title: row.get("meeting_title"),
                password_hash: row.get("password_hash"),
                waiting_room_enabled: row.get("waiting_room_enabled"),
                meeting_status: row.get("meeting_status"),
            }))
        }
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
    pub fn get_or_create(room_id: &str) -> Result<Self, Box<dyn Error + Send + Sync>> {
        match Self::get_by_room_id(room_id)? {
            Some(meeting) => Ok(meeting),
            None => {
                let now = Utc::now();
                Self::create(room_id, now, None, None, None, false)
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

    pub fn verify_password(&self, password: &str) -> Result<bool, Box<dyn Error + Send + Sync>> {
        match &self.password_hash {
            Some(hash) => Ok(verify(password, hash)?),
            None => Ok(true),
        }
    }

    /// Check if the meeting requires a password
    pub fn requires_password(&self) -> bool {
        self.password_hash.is_some()
    }
}
