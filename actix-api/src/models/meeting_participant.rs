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
use std::fmt;
use tracing::info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "VARCHAR", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ParticipantStatus {
    Waiting,
    Admitted,
    Rejected,
    Left,
}

impl fmt::Display for ParticipantStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParticipantStatus::Waiting => write!(f, "waiting"),
            ParticipantStatus::Admitted => write!(f, "admitted"),
            ParticipantStatus::Rejected => write!(f, "rejected"),
            ParticipantStatus::Left => write!(f, "left"),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, sqlx::FromRow)]
pub struct MeetingParticipant {
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

#[derive(Debug)]
pub enum ParticipantError {
    NotFound,
    NotHost,
    MeetingNotFound,
    MeetingNotActive,
    AlreadyJoined,
    DatabaseError(String),
}

impl fmt::Display for ParticipantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParticipantError::NotFound => write!(f, "Participant not found"),
            ParticipantError::NotHost => write!(f, "Only the host can perform this action"),
            ParticipantError::MeetingNotFound => write!(f, "Meeting not found"),
            ParticipantError::MeetingNotActive => write!(f, "Meeting is not active"),
            ParticipantError::AlreadyJoined => write!(f, "Already joined this meeting"),
            ParticipantError::DatabaseError(e) => write!(f, "Database error: {}", e),
        }
    }
}

impl Error for ParticipantError {}

impl MeetingParticipant {
    /// Host joins meeting - creates host participant and activates meeting
    pub async fn host_join(
        pool: &PgPool,
        room_id: &str,
        host_email: &str,
        display_name: Option<&str>,
    ) -> Result<Self, ParticipantError> {
        // Get meeting and verify host
        let meeting = sqlx::query_as::<_, (i32, Option<String>, Option<String>)>(
            r#"
            SELECT id, creator_id, state
            FROM meetings
            WHERE room_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        let (meeting_id, creator_id, _state) = meeting;

        // Verify this user is the host
        if creator_id.as_deref() != Some(host_email) {
            return Err(ParticipantError::NotHost);
        }

        // Activate the meeting and store host_display_name
        sqlx::query("UPDATE meetings SET state = 'active', started_at = NOW(), host_display_name = $2 WHERE id = $1")
            .bind(meeting_id)
            .bind(display_name)
            .execute(pool)
            .await
            .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        // Insert or update host participant with display_name
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            INSERT INTO meeting_participants (meeting_id, email, status, is_host, admitted_at, display_name)
            VALUES ($1, $2, 'admitted', TRUE, NOW(), $3)
            ON CONFLICT (meeting_id, email) DO UPDATE
            SET status = 'admitted', is_host = TRUE, admitted_at = COALESCE(meeting_participants.admitted_at, NOW()), display_name = COALESCE(EXCLUDED.display_name, meeting_participants.display_name), updated_at = NOW()
            RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            "#,
        )
        .bind(meeting_id)
        .bind(host_email)
        .bind(display_name)
        .fetch_one(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        info!(
            "Host '{}' (display: {:?}) joined meeting '{}'",
            host_email, display_name, room_id
        );
        Ok(participant)
    }

    /// Attendee requests to join meeting - enters wait room
    /// If the meeting doesn't exist, it will be created with the joining user as the owner
    pub async fn request_join(
        pool: &PgPool,
        room_id: &str,
        attendee_email: &str,
        display_name: Option<&str>,
    ) -> Result<Self, ParticipantError> {
        // Try to get existing meeting
        let meeting = sqlx::query_as::<_, (i32, Option<String>, Option<String>)>(
            r#"
            SELECT id, creator_id, state
            FROM meetings
            WHERE room_id = $1 AND deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        // If meeting doesn't exist, create it with this user as the owner
        let (meeting_id, creator_id, state) = match meeting {
            Some(m) => m,
            None => {
                info!(
                    "Meeting '{}' not found, creating with '{}' as owner",
                    room_id, attendee_email
                );
                // Create the meeting with the joining user as owner
                let now = Utc::now();
                let new_meeting = sqlx::query_as::<_, (i32, Option<String>, Option<String>)>(
                    r#"
                    INSERT INTO meetings (room_id, started_at, creator_id, state, attendees)
                    VALUES ($1, $2, $3, 'idle', '[]')
                    RETURNING id, creator_id, state
                    "#,
                )
                .bind(room_id)
                .bind(now)
                .bind(attendee_email)
                .fetch_one(pool)
                .await
                .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

                info!(
                    "Meeting '{}' created with '{}' as owner",
                    room_id, attendee_email
                );
                new_meeting
            }
        };

        // Check if user is the host - hosts are auto-admitted
        let is_host = creator_id.as_deref() == Some(attendee_email);
        let initial_status = if is_host { "admitted" } else { "waiting" };

        // If host is joining and meeting isn't active, activate it
        if is_host && state.as_deref() != Some("active") {
            return Self::host_join(pool, room_id, attendee_email, display_name).await;
        }

        // Check if meeting is active (for non-hosts)
        if !is_host && state.as_deref() != Some("active") {
            return Err(ParticipantError::MeetingNotActive);
        }

        // Check if already in the meeting
        let existing = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            SELECT id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            FROM meeting_participants
            WHERE meeting_id = $1 AND email = $2
            "#,
        )
        .bind(meeting_id)
        .bind(attendee_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if let Some(p) = existing {
            // If host is already admitted, return current status
            if p.is_host && p.status == "admitted" {
                return Ok(p);
            }
            // If non-host is currently waiting, return current status
            if !p.is_host && p.status == "waiting" {
                return Ok(p);
            }
            // For non-hosts who were previously admitted/rejected/left, put them back in waiting room
            // This ensures the waiting room works correctly when users rejoin
            let new_status = if p.is_host { "admitted" } else { "waiting" };
            // Clear admitted_at for non-hosts so they appear as fresh waiting room entries
            let clear_admitted = !p.is_host;
            let participant = sqlx::query_as::<_, MeetingParticipant>(
                r#"
                UPDATE meeting_participants
                SET status = $2, joined_at = NOW(), left_at = NULL,
                    admitted_at = CASE WHEN $4 THEN NULL ELSE admitted_at END,
                    display_name = COALESCE($3, display_name), updated_at = NOW()
                WHERE id = $1
                RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
                "#,
            )
            .bind(p.id)
            .bind(new_status)
            .bind(display_name)
            .bind(clear_admitted)
            .fetch_one(pool)
            .await
            .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

            info!(
                "Attendee '{}' re-requested to join meeting '{}' (status: {})",
                attendee_email, room_id, new_status
            );
            return Ok(participant);
        }

        // Insert new participant
        let admitted_at = if is_host { "NOW()" } else { "NULL" };
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            &format!(
                r#"
            INSERT INTO meeting_participants (meeting_id, email, status, is_host, admitted_at, display_name)
            VALUES ($1, $2, $3, $4, {}, $5)
            RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            "#,
                admitted_at
            ),
        )
        .bind(meeting_id)
        .bind(attendee_email)
        .bind(initial_status)
        .bind(is_host)
        .bind(display_name)
        .fetch_one(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        info!(
            "Attendee '{}' (display: {:?}) requested to join meeting '{}' (status: {})",
            attendee_email, display_name, room_id, initial_status
        );
        Ok(participant)
    }

    /// Get all waiting participants for a meeting (any admitted participant can view)
    pub async fn get_waiting(
        pool: &PgPool,
        room_id: &str,
        requester_email: &str,
    ) -> Result<Vec<Self>, ParticipantError> {
        // Get meeting
        let meeting_id = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        // Verify requester is admitted to the meeting
        let requester_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND email = $2",
        )
        .bind(meeting_id)
        .bind(requester_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if requester_status.as_deref() != Some("admitted") {
            return Err(ParticipantError::NotHost); // Reusing error - means "not authorized"
        }

        let participants = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            SELECT id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            FROM meeting_participants
            WHERE meeting_id = $1 AND status = 'waiting'
            ORDER BY joined_at ASC
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        Ok(participants)
    }

    /// Admit a participant (any admitted participant can admit others)
    pub async fn admit(
        pool: &PgPool,
        room_id: &str,
        requester_email: &str,
        attendee_email: &str,
    ) -> Result<Self, ParticipantError> {
        // Get meeting
        let meeting_id = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        // Verify requester is admitted to the meeting
        let requester_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND email = $2",
        )
        .bind(meeting_id)
        .bind(requester_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if requester_status.as_deref() != Some("admitted") {
            return Err(ParticipantError::NotHost); // Reusing error - means "not authorized"
        }

        // Admit the participant
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            UPDATE meeting_participants
            SET status = 'admitted', admitted_at = NOW(), updated_at = NOW()
            WHERE meeting_id = $1 AND email = $2 AND status = 'waiting'
            RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            "#,
        )
        .bind(meeting_id)
        .bind(attendee_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::NotFound)?;

        info!(
            "Participant '{}' admitted '{}' to meeting '{}'",
            requester_email, attendee_email, room_id
        );
        Ok(participant)
    }

    /// Reject a participant (any admitted participant can reject)
    pub async fn reject(
        pool: &PgPool,
        room_id: &str,
        requester_email: &str,
        attendee_email: &str,
    ) -> Result<Self, ParticipantError> {
        // Get meeting
        let meeting_id = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        // Verify requester is admitted to the meeting
        let requester_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND email = $2",
        )
        .bind(meeting_id)
        .bind(requester_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if requester_status.as_deref() != Some("admitted") {
            return Err(ParticipantError::NotHost); // Reusing error - means "not authorized"
        }

        // Reject the participant
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            UPDATE meeting_participants
            SET status = 'rejected', updated_at = NOW()
            WHERE meeting_id = $1 AND email = $2 AND status = 'waiting'
            RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            "#,
        )
        .bind(meeting_id)
        .bind(attendee_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::NotFound)?;

        info!(
            "Participant '{}' rejected '{}' from meeting '{}'",
            requester_email, attendee_email, room_id
        );
        Ok(participant)
    }

    /// Get participant status in a meeting
    pub async fn get_status(
        pool: &PgPool,
        room_id: &str,
        email: &str,
    ) -> Result<Option<Self>, ParticipantError> {
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            SELECT mp.id, mp.meeting_id, mp.email, mp.status, mp.is_host, mp.is_required,
                   mp.joined_at, mp.admitted_at, mp.left_at, mp.created_at, mp.updated_at, mp.display_name
            FROM meeting_participants mp
            JOIN meetings m ON mp.meeting_id = m.id
            WHERE m.room_id = $1 AND mp.email = $2 AND m.deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        Ok(participant)
    }

    /// Get all admitted participants for a meeting
    pub async fn get_admitted(pool: &PgPool, room_id: &str) -> Result<Vec<Self>, ParticipantError> {
        let meeting_id = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        let participants = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            SELECT id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            FROM meeting_participants
            WHERE meeting_id = $1 AND status = 'admitted'
            ORDER BY admitted_at ASC
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        Ok(participants)
    }

    /// Leave a meeting
    pub async fn leave(
        pool: &PgPool,
        room_id: &str,
        email: &str,
    ) -> Result<Option<Self>, ParticipantError> {
        let participant = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            UPDATE meeting_participants mp
            SET status = 'left', left_at = NOW(), updated_at = NOW()
            FROM meetings m
            WHERE mp.meeting_id = m.id AND m.room_id = $1 AND mp.email = $2 AND m.deleted_at IS NULL
            RETURNING mp.id, mp.meeting_id, mp.email, mp.status, mp.is_host, mp.is_required,
                      mp.joined_at, mp.admitted_at, mp.left_at, mp.created_at, mp.updated_at, mp.display_name
            "#,
        )
        .bind(room_id)
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if let Some(ref p) = participant {
            info!("Participant '{}' left meeting '{}'", email, room_id);

            // Check if meeting should end
            let should_end = if p.is_host {
                // Host leaving always ends the meeting
                true
            } else {
                // Check if any admitted participants remain
                let remaining = sqlx::query_scalar::<_, i64>(
                    r#"
                    SELECT COUNT(*)
                    FROM meeting_participants mp
                    JOIN meetings m ON mp.meeting_id = m.id
                    WHERE m.room_id = $1 AND mp.status = 'admitted' AND m.deleted_at IS NULL
                    "#,
                )
                .bind(room_id)
                .fetch_one(pool)
                .await
                .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

                remaining == 0
            };

            if should_end {
                sqlx::query(
                    "UPDATE meetings SET state = 'ended', ended_at = NOW() WHERE room_id = $1",
                )
                .bind(room_id)
                .execute(pool)
                .await
                .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

                if p.is_host {
                    info!("Meeting '{}' ended because host left", room_id);
                } else {
                    info!("Meeting '{}' ended because all participants left", room_id);
                }
            }
        }

        Ok(participant)
    }

    /// Admit all waiting participants (any admitted participant can admit all)
    pub async fn admit_all(
        pool: &PgPool,
        room_id: &str,
        requester_email: &str,
    ) -> Result<Vec<Self>, ParticipantError> {
        // Get meeting
        let meeting_id = sqlx::query_scalar::<_, i32>(
            "SELECT id FROM meetings WHERE room_id = $1 AND deleted_at IS NULL",
        )
        .bind(room_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?
        .ok_or(ParticipantError::MeetingNotFound)?;

        // Verify requester is admitted to the meeting
        let requester_status = sqlx::query_scalar::<_, String>(
            "SELECT status FROM meeting_participants WHERE meeting_id = $1 AND email = $2",
        )
        .bind(meeting_id)
        .bind(requester_email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        if requester_status.as_deref() != Some("admitted") {
            return Err(ParticipantError::NotHost); // Reusing error - means "not authorized"
        }

        // Admit all waiting participants
        let participants = sqlx::query_as::<_, MeetingParticipant>(
            r#"
            UPDATE meeting_participants
            SET status = 'admitted', admitted_at = NOW(), updated_at = NOW()
            WHERE meeting_id = $1 AND status = 'waiting'
            RETURNING id, meeting_id, email, status, is_host, is_required, joined_at, admitted_at, left_at, created_at, updated_at, display_name
            "#,
        )
        .bind(meeting_id)
        .fetch_all(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        let count = participants.len();
        info!(
            "Participant '{}' admitted {} participants to meeting '{}'",
            requester_email, count, room_id
        );
        Ok(participants)
    }

    /// Count admitted participants for a meeting
    pub async fn count_admitted(pool: &PgPool, room_id: &str) -> Result<i64, ParticipantError> {
        let count = sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM meeting_participants mp
            JOIN meetings m ON mp.meeting_id = m.id
            WHERE m.room_id = $1 AND mp.status = 'admitted' AND m.deleted_at IS NULL
            "#,
        )
        .bind(room_id)
        .fetch_one(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        Ok(count)
    }

    /// Get the display name for a participant by their email (user ID)
    /// This is used to identify the meeting host by their user ID rather than display name
    pub async fn get_display_name_by_email(
        pool: &PgPool,
        meeting_id: i32,
        email: &str,
    ) -> Result<Option<String>, ParticipantError> {
        let display_name = sqlx::query_scalar::<_, String>(
            "SELECT display_name FROM meeting_participants WHERE meeting_id = $1 AND email = $2",
        )
        .bind(meeting_id)
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_err(|e| ParticipantError::DatabaseError(e.to_string()))?;

        Ok(display_name)
    }
}
