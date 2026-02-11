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

//! SessionManager - shared session lifecycle logic for WebSocket and WebTransport
//!
//! Tracks active connections via SessionParticipant in PostgreSQL.
//! Meeting lifecycle (create, end, host management) is handled by meeting-api.
//! The media server validates JWT room access tokens for authorization.

use crate::models::session_participant::SessionParticipant;
use protobuf::Message as ProtoMessage;
use sqlx::PgPool;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::{FeatureFlags, SYSTEM_USER_EMAIL};

/// Error type for session management operations
#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    /// User tried to use the reserved system email
    ReservedUserEmail,
    /// Database or other internal error
    Internal(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::ReservedUserEmail => {
                write!(f, "Cannot use reserved system email as user ID")
            }
            SessionError::Internal(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Result of starting a session
#[derive(Debug, Clone)]
pub struct SessionStartResult {
    pub start_time_ms: u64,
    pub is_first_participant: bool,
    /// The user_id of the connecting user (host/creator info comes from JWT)
    pub creator_id: String,
}

/// Result of ending a session
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEndResult {
    /// Regular participant left, meeting continues
    MeetingContinues { remaining_count: i64 },
    /// Host left, meeting ended for everyone (determined by JWT role in future)
    HostEndedMeeting,
    /// Last participant left, meeting ended
    LastParticipantLeft,
}

/// SessionManager handles session lifecycle for both WebSocket and WebTransport.
/// Tracks active connections in PostgreSQL via SessionParticipant.
/// Meeting lifecycle is managed by meeting-api; authorization via JWT.
#[derive(Debug, Clone)]
pub struct SessionManager {
    pool: Option<PgPool>,
}

impl SessionManager {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self { pool }
    }

    /// Called when a user connects to a room.
    /// Records participant in DB.
    ///
    /// When FEATURE_MEETING_MANAGEMENT is disabled, returns defaults without DB operations.
    ///
    /// # Errors
    /// Returns `SessionError::ReservedUserEmail` if user_id matches the system email.
    pub async fn start_session(
        &self,
        room_id: &str,
        user_id: &str,
    ) -> Result<SessionStartResult, Box<dyn std::error::Error + Send + Sync>> {
        // Reject reserved system email (always enforced)
        if user_id == SYSTEM_USER_EMAIL {
            return Err(Box::new(SessionError::ReservedUserEmail));
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        // Feature flag check - return defaults if disabled
        if !FeatureFlags::meeting_management_enabled() {
            return Ok(SessionStartResult {
                start_time_ms: now_ms,
                is_first_participant: true,
                creator_id: user_id.to_string(),
            });
        }

        // Check if database pool is available
        let pool = match &self.pool {
            Some(p) => p,
            None => {
                return Ok(SessionStartResult {
                    start_time_ms: now_ms,
                    is_first_participant: true,
                    creator_id: user_id.to_string(),
                });
            }
        };

        // Record participant join in DB
        SessionParticipant::join(pool, room_id, user_id).await?;

        // Check if this is the first participant
        let is_first_participant = SessionParticipant::is_first_participant(pool, room_id).await?;

        if is_first_participant {
            info!(
                "First participant {} joined room {}, session started",
                user_id, room_id
            );
        }

        Ok(SessionStartResult {
            start_time_ms: now_ms,
            is_first_participant,
            creator_id: user_id.to_string(),
        })
    }

    /// Called when a user disconnects from a room.
    /// Returns what action was taken.
    ///
    /// When FEATURE_MEETING_MANAGEMENT is disabled, returns no-op result without DB operations.
    pub async fn end_session(
        &self,
        room_id: &str,
        user_id: &str,
    ) -> Result<SessionEndResult, Box<dyn std::error::Error + Send + Sync>> {
        // Feature flag check - return no-op if disabled
        if !FeatureFlags::meeting_management_enabled() {
            return Ok(SessionEndResult::MeetingContinues { remaining_count: 0 });
        }

        // Check if database pool is available
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(SessionEndResult::MeetingContinues { remaining_count: 0 }),
        };

        // Mark participant as left in DB
        SessionParticipant::leave(pool, room_id, user_id).await?;

        // Get remaining count from DB
        let remaining_count = SessionParticipant::count_active(pool, room_id).await?;

        if remaining_count == 0 {
            info!(
                "Last participant {} left room {} - session ended",
                user_id, room_id
            );
            Ok(SessionEndResult::LastParticipantLeft)
        } else {
            info!(
                "Participant {} left room {}, {} remaining",
                user_id, room_id, remaining_count
            );
            Ok(SessionEndResult::MeetingContinues { remaining_count })
        }
    }

    /// Get current participant count for a room (from DB)
    ///
    /// When FEATURE_MEETING_MANAGEMENT is disabled, returns 0.
    pub async fn get_participant_count(&self, room_id: &str) -> i64 {
        if !FeatureFlags::meeting_management_enabled() {
            return 0;
        }
        match &self.pool {
            Some(pool) => SessionParticipant::count_active(pool, room_id)
                .await
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Build MEETING_STARTED packet to send to client (protobuf)
    pub fn build_meeting_started_packet(
        room_id: &str,
        start_time_ms: u64,
        creator_id: &str,
    ) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_STARTED.into(),
            room_id: room_id.to_string(),
            start_time_ms,
            creator_id: creator_id.to_string(),
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            email: SYSTEM_USER_EMAIL.to_string(),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };

        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Build MEETING_ENDED packet to send to clients (protobuf)
    pub fn build_meeting_ended_packet(room_id: &str, message: &str) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_ENDED.into(),
            room_id: room_id.to_string(),
            message: message.to_string(),
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            email: SYSTEM_USER_EMAIL.to_string(),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };

        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Get the database pool (for passing to other components)
    pub fn pool(&self) -> Option<&PgPool> {
        self.pool.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ==========================================================================
    // Integration tests - require DATABASE_URL environment variable
    // Run with: make tests_run
    // These tests use #[serial] to prevent race conditions on global feature flags.
    // ==========================================================================

    async fn get_test_pool() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
        PgPool::connect(&database_url)
            .await
            .expect("Failed to connect to test database")
    }

    async fn cleanup_test_room(pool: &PgPool, room_id: &str) {
        let _ = sqlx::query("DELETE FROM session_participants WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
    }

    fn setup_meeting_management_enabled() {
        FeatureFlags::set_meeting_management_override(true);
    }

    fn teardown_meeting_management() {
        FeatureFlags::clear_meeting_management_override();
    }

    // ==========================================================================
    // TEST 1: First user joins - is_first_participant is true
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_first_user_is_first_participant() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-first-1";

        cleanup_test_room(&pool, room_id).await;

        assert_eq!(manager.get_participant_count(room_id).await, 0);

        let result = manager.start_session(room_id, "alice").await.unwrap();

        assert_eq!(manager.get_participant_count(room_id).await, 1);
        assert!(
            result.is_first_participant,
            "Alice should be marked as first participant"
        );
        assert!(result.start_time_ms > 0, "Start time should be set");

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 2: Others join - is_first_participant is false
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_others_join_not_first() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-join-2";

        cleanup_test_room(&pool, room_id).await;

        let alice_result = manager.start_session(room_id, "alice").await.unwrap();
        assert!(alice_result.is_first_participant);
        assert_eq!(manager.get_participant_count(room_id).await, 1);

        let bob_result = manager.start_session(room_id, "bob").await.unwrap();
        assert!(
            !bob_result.is_first_participant,
            "Bob should NOT be first participant"
        );
        assert_eq!(manager.get_participant_count(room_id).await, 2);

        let charlie_result = manager.start_session(room_id, "charlie").await.unwrap();
        assert!(
            !charlie_result.is_first_participant,
            "Charlie should NOT be first participant"
        );
        assert_eq!(manager.get_participant_count(room_id).await, 3);

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 3: Everyone leaving - last participant triggers LastParticipantLeft
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_everyone_leaving_ends_session() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-leave-3";

        cleanup_test_room(&pool, room_id).await;

        let _ = manager.start_session(room_id, "alice").await.unwrap();
        let _ = manager.start_session(room_id, "bob").await.unwrap();
        let _ = manager.start_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 3);

        let charlie_leave = manager.end_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 2);
        assert_eq!(
            charlie_leave,
            SessionEndResult::MeetingContinues { remaining_count: 2 }
        );

        let bob_leave = manager.end_session(room_id, "bob").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 1);
        assert_eq!(
            bob_leave,
            SessionEndResult::MeetingContinues { remaining_count: 1 }
        );

        let alice_leave = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 0);
        assert_eq!(alice_leave, SessionEndResult::LastParticipantLeft);

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 4: Multiple rooms are isolated
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_multiple_rooms_isolated() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));

        cleanup_test_room(&pool, "room-a-iso").await;
        cleanup_test_room(&pool, "room-b-iso").await;
        cleanup_test_room(&pool, "room-c-iso").await;

        let _ = manager.start_session("room-a-iso", "alice").await.unwrap();
        let _ = manager.start_session("room-b-iso", "bob").await.unwrap();
        let _ = manager
            .start_session("room-a-iso", "charlie")
            .await
            .unwrap();

        assert_eq!(manager.get_participant_count("room-a-iso").await, 2);
        assert_eq!(manager.get_participant_count("room-b-iso").await, 1);
        assert_eq!(manager.get_participant_count("room-c-iso").await, 0);

        let result = manager.end_session("room-a-iso", "charlie").await.unwrap();
        assert_eq!(
            result,
            SessionEndResult::MeetingContinues { remaining_count: 1 }
        );
        assert_eq!(manager.get_participant_count("room-a-iso").await, 1);
        assert_eq!(manager.get_participant_count("room-b-iso").await, 1);

        cleanup_test_room(&pool, "room-a-iso").await;
        cleanup_test_room(&pool, "room-b-iso").await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 5: Protobuf packet builders work correctly
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_protobuf_packet_builders() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let meeting_started =
            SessionManager::build_meeting_started_packet("my-room", 1234567890, "alice");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_started).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_STARTED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.start_time_ms, 1234567890);
        assert_eq!(inner.creator_id, "alice");

        let meeting_ended = SessionManager::build_meeting_ended_packet("my-room", "Host left");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_ended).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_ENDED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.message, "Host left");
    }

    // ==========================================================================
    // TEST 6: Reserved system email is rejected
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_system_email_rejected() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-system-6";

        cleanup_test_room(&pool, room_id).await;

        let result = manager.start_session(room_id, SYSTEM_USER_EMAIL).await;
        assert!(result.is_err(), "Should reject reserved system email");

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("reserved system email"),
            "Error message should mention reserved system email: {err}"
        );

        FeatureFlags::set_meeting_management_override(true);
        assert_eq!(
            manager.get_participant_count(room_id).await,
            0,
            "No participant should be added for reserved email"
        );
        FeatureFlags::clear_meeting_management_override();

        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 7: Feature flag OFF - start_session returns defaults without DB ops
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_off_start_session_returns_defaults() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-ff-off-7";

        cleanup_test_room(&pool, room_id).await;

        FeatureFlags::set_meeting_management_override(false);

        let result = manager.start_session(room_id, "alice").await.unwrap();
        assert!(result.start_time_ms > 0);
        assert!(result.is_first_participant);

        FeatureFlags::set_meeting_management_override(true);
        assert_eq!(
            manager.get_participant_count(room_id).await,
            0,
            "No participant should be in DB when FF is off"
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 8: Feature flag OFF - end_session returns no-op
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_off_end_session_returns_noop() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-ff-off-8";

        cleanup_test_room(&pool, room_id).await;

        FeatureFlags::set_meeting_management_override(false);

        let result = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(
            result,
            SessionEndResult::MeetingContinues { remaining_count: 0 }
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 9: Feature flag ON - participant tracking works
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_on_participant_tracking() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(Some(pool.clone()));
        let room_id = "test-room-ff-on-9";

        cleanup_test_room(&pool, room_id).await;

        FeatureFlags::set_meeting_management_override(true);

        let result = manager.start_session(room_id, "alice").await.unwrap();
        assert!(result.is_first_participant);
        assert!(result.start_time_ms > 0);

        assert_eq!(manager.get_participant_count(room_id).await, 1);

        let end_result = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(end_result, SessionEndResult::LastParticipantLeft);

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }
}
