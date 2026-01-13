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
//! All state lives in PostgreSQL. No in-memory HashMaps.
//! This ensures consistency across multiple server instances.

use crate::models::meeting::Meeting;
use crate::models::session_participant::SessionParticipant;
use protobuf::Message as ProtoMessage;
use sqlx::PgPool;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info};
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
    /// The user_id of the meeting creator/host
    pub creator_id: String,
}

/// Result of ending a session
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEndResult {
    /// Regular participant left, meeting continues
    MeetingContinues { remaining_count: i64 },
    /// Host left, meeting ended for everyone
    HostEndedMeeting,
    /// Last participant left, meeting ended
    LastParticipantLeft,
}

/// SessionManager handles session lifecycle for both WebSocket and WebTransport.
/// All state is persisted in PostgreSQL - no in-memory state.
#[derive(Debug, Clone)]
pub struct SessionManager {
    pool: Option<PgPool>,
}

impl SessionManager {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self { pool }
    }

    /// Called when a user connects to a room.
    /// Records participant in DB and starts meeting if first participant.
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

        // Feature flag check - return defaults if disabled
        if !FeatureFlags::meeting_management_enabled() {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
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
                // No database - return defaults
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
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

        // Get or create meeting
        let (start_time_ms, creator_id) = if is_first_participant {
            info!(
                "First participant {} joined room {}, starting meeting",
                user_id, room_id
            );
            let meeting = Meeting::create_async(pool, room_id, Some(user_id)).await?;
            (
                meeting.start_time_unix_ms() as u64,
                meeting.creator_id.unwrap_or_else(|| user_id.to_string()),
            )
        } else {
            // Get existing meeting info (start time and creator)
            match Meeting::get_by_room_id_async(pool, room_id).await? {
                Some(meeting) => (
                    meeting.start_time_unix_ms() as u64,
                    meeting.creator_id.unwrap_or_else(|| user_id.to_string()),
                ),
                None => {
                    // Edge case: meeting doesn't exist but participants do
                    // This shouldn't happen but handle it gracefully
                    let meeting = Meeting::create_async(pool, room_id, Some(user_id)).await?;
                    (
                        meeting.start_time_unix_ms() as u64,
                        meeting.creator_id.unwrap_or_else(|| user_id.to_string()),
                    )
                }
            }
        };

        Ok(SessionStartResult {
            start_time_ms,
            is_first_participant,
            creator_id,
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

        // Check if user is host before marking them as left
        let is_host = self.is_host(room_id, user_id).await;

        // Mark participant as left in DB
        SessionParticipant::leave(pool, room_id, user_id).await?;

        // Get remaining count from DB
        let remaining_count = SessionParticipant::count_active(pool, room_id).await?;

        // Determine what to do
        if is_host && remaining_count > 0 {
            // Host left but others remain - end meeting for everyone
            info!(
                "Host {} left room {} with {} remaining - ending meeting",
                user_id, room_id, remaining_count
            );
            // Mark all remaining participants as left
            SessionParticipant::leave_all(pool, room_id).await?;
            // End meeting in DB
            if let Err(e) = Meeting::end_meeting_async(pool, room_id).await {
                error!("Error ending meeting: {}", e);
            }
            Ok(SessionEndResult::HostEndedMeeting)
        } else if remaining_count == 0 {
            // Last person left
            info!(
                "Last participant {} left room {} - ending meeting",
                user_id, room_id
            );
            if let Err(e) = Meeting::end_meeting_async(pool, room_id).await {
                error!("Error ending meeting: {}", e);
            }
            Ok(SessionEndResult::LastParticipantLeft)
        } else {
            // Regular participant left, meeting continues
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

    /// Check if a user is the host/creator of a room
    ///
    /// When FEATURE_MEETING_MANAGEMENT is disabled, returns false.
    pub async fn is_host(&self, room_id: &str, user_id: &str) -> bool {
        if !FeatureFlags::meeting_management_enabled() {
            return false;
        }
        let pool = match &self.pool {
            Some(p) => p,
            None => return false,
        };
        match Meeting::get_by_room_id_async(pool, room_id).await {
            Ok(Some(meeting)) => meeting.creator_id.as_deref() == Some(user_id),
            _ => false,
        }
    }

    /// Get meeting info for a room
    pub async fn get_meeting_info(
        &self,
        room_id: &str,
    ) -> Result<Option<Meeting>, Box<dyn std::error::Error + Send + Sync>> {
        match &self.pool {
            Some(pool) => Meeting::get_by_room_id_async(pool, room_id).await,
            None => Ok(None),
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
        // Clean up any existing test data
        let _ = sqlx::query("DELETE FROM session_participants WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
        let _ = sqlx::query("DELETE FROM meetings WHERE room_id = $1")
            .bind(room_id)
            .execute(pool)
            .await;
    }

    /// Setup for tests that require meeting management enabled
    fn setup_meeting_management_enabled() {
        FeatureFlags::set_meeting_management_override(true);
    }

    /// Teardown for tests - clear FF override
    fn teardown_meeting_management() {
        FeatureFlags::clear_meeting_management_override();
    }

    // ==========================================================================
    // TEST 1: Meeting Creation - First user creates a meeting
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_meeting_creation_first_user_creates_meeting() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-create-1";

        cleanup_test_room(&pool, room_id).await;

        // Before anyone joins, room should have 0 participants
        assert_eq!(manager.get_participant_count(room_id).await, 0);

        // First user (alice) joins - this should create the meeting
        let result = manager.start_session(room_id, "alice").await.unwrap();

        // Verify participant count is now 1
        assert_eq!(manager.get_participant_count(room_id).await, 1);

        // First participant flag should be true
        assert!(
            result.is_first_participant,
            "Alice should be marked as first participant"
        );

        // Start time should be set
        assert!(result.start_time_ms > 0, "Meeting start time should be set");

        // Verify meeting exists in DB
        let meeting = manager.get_meeting_info(room_id).await.unwrap();
        assert!(meeting.is_some(), "Meeting should exist in DB");
        assert_eq!(
            meeting.unwrap().creator_id,
            Some("alice".to_string()),
            "Alice should be the creator"
        );

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 2: Others Join Meeting - Subsequent users join existing meeting
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_others_can_join_meeting() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-join-2";

        cleanup_test_room(&pool, room_id).await;

        // Alice creates the meeting
        let alice_result = manager.start_session(room_id, "alice").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 1);
        assert!(alice_result.is_first_participant);

        // Bob joins the existing meeting
        let bob_result = manager.start_session(room_id, "bob").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 2);
        assert!(
            !bob_result.is_first_participant,
            "Bob should NOT be first participant"
        );

        // Charlie joins too
        let charlie_result = manager.start_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 3);
        assert!(
            !charlie_result.is_first_participant,
            "Charlie should NOT be first participant"
        );

        // All should have same start time
        assert_eq!(
            alice_result.start_time_ms, bob_result.start_time_ms,
            "Alice and Bob should have same start time"
        );
        assert_eq!(
            bob_result.start_time_ms, charlie_result.start_time_ms,
            "Bob and Charlie should have same start time"
        );

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 2b: Joining user gets correct creator_id (not their own ID)
    // ==========================================================================
    // This test verifies the fix for the bug where non-first participants
    // would receive their own user_id as creator_id in the MEETING_STARTED packet.
    #[tokio::test]
    #[serial]
    async fn test_joining_user_gets_correct_creator_id() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-creator-id";

        cleanup_test_room(&pool, room_id).await;

        // Alice creates the meeting - she should be the creator
        let alice_result = manager.start_session(room_id, "alice").await.unwrap();
        assert!(alice_result.is_first_participant);
        assert_eq!(
            alice_result.creator_id, "alice",
            "First participant should get their own ID as creator"
        );

        // Bob joins - he should get Alice's ID as the creator, NOT his own
        let bob_result = manager.start_session(room_id, "bob").await.unwrap();
        assert!(!bob_result.is_first_participant);
        assert_eq!(
            bob_result.creator_id, "alice",
            "Second participant should get first participant (alice) as creator, not their own ID (bob)"
        );

        // Charlie joins - he should also get Alice's ID as the creator
        let charlie_result = manager.start_session(room_id, "charlie").await.unwrap();
        assert!(!charlie_result.is_first_participant);
        assert_eq!(
            charlie_result.creator_id, "alice",
            "Third participant should get first participant (alice) as creator, not their own ID (charlie)"
        );

        // All participants should have the same creator_id
        assert_eq!(
            alice_result.creator_id, bob_result.creator_id,
            "Alice and Bob should have same creator_id"
        );
        assert_eq!(
            bob_result.creator_id, charlie_result.creator_id,
            "Bob and Charlie should have same creator_id"
        );

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 3: Everyone Leaving - Last participant ends the meeting
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_everyone_leaving_ends_meeting() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-leave-3";

        cleanup_test_room(&pool, room_id).await;

        // Setup: 3 users join
        let _ = manager.start_session(room_id, "alice").await.unwrap();
        let _ = manager.start_session(room_id, "bob").await.unwrap();
        let _ = manager.start_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 3);

        // Charlie leaves - meeting continues
        let charlie_leave = manager.end_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 2);
        assert_eq!(
            charlie_leave,
            SessionEndResult::MeetingContinues { remaining_count: 2 },
            "Meeting should continue with 2 remaining"
        );

        // Bob leaves - meeting continues
        let bob_leave = manager.end_session(room_id, "bob").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 1);
        assert_eq!(
            bob_leave,
            SessionEndResult::MeetingContinues { remaining_count: 1 },
            "Meeting should continue with 1 remaining"
        );

        // Alice (last person) leaves - meeting ends
        let alice_leave = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 0);
        assert_eq!(
            alice_leave,
            SessionEndResult::LastParticipantLeft,
            "Meeting should end when last participant leaves"
        );

        // Verify meeting is ended in DB
        let meeting = manager.get_meeting_info(room_id).await.unwrap();
        assert!(
            meeting.is_some() && meeting.unwrap().ended_at.is_some(),
            "Meeting should be marked as ended"
        );

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 4: Host leaves - Meeting ends for everyone
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_host_leaving_ends_meeting() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-host-4";

        cleanup_test_room(&pool, room_id).await;

        // Alice creates and is host
        let _ = manager.start_session(room_id, "alice").await.unwrap();
        let _ = manager.start_session(room_id, "bob").await.unwrap();
        let _ = manager.start_session(room_id, "charlie").await.unwrap();
        assert_eq!(manager.get_participant_count(room_id).await, 3);

        // Verify alice is host
        assert!(manager.is_host(room_id, "alice").await);
        assert!(!manager.is_host(room_id, "bob").await);

        // Alice (host) leaves while others are still in the room
        let alice_leave = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(
            alice_leave,
            SessionEndResult::HostEndedMeeting,
            "Host leaving should end meeting"
        );

        // All participants should be marked as left
        assert_eq!(
            manager.get_participant_count(room_id).await,
            0,
            "All participants should be gone"
        );

        cleanup_test_room(&pool, room_id).await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 5: Multiple rooms are isolated
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_multiple_rooms_isolated() {
        setup_meeting_management_enabled();

        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());

        cleanup_test_room(&pool, "room-a-iso").await;
        cleanup_test_room(&pool, "room-b-iso").await;
        cleanup_test_room(&pool, "room-c-iso").await;

        // Users join different rooms
        let _ = manager.start_session("room-a-iso", "alice").await.unwrap();
        let _ = manager.start_session("room-b-iso", "bob").await.unwrap();
        let _ = manager
            .start_session("room-a-iso", "charlie")
            .await
            .unwrap();

        // Verify counts are isolated
        assert_eq!(manager.get_participant_count("room-a-iso").await, 2);
        assert_eq!(manager.get_participant_count("room-b-iso").await, 1);
        assert_eq!(manager.get_participant_count("room-c-iso").await, 0);

        // Charlie (non-host) leaves room-a-iso - meeting should continue
        let result = manager.end_session("room-a-iso", "charlie").await.unwrap();
        assert_eq!(
            result,
            SessionEndResult::MeetingContinues { remaining_count: 1 }
        );
        assert_eq!(manager.get_participant_count("room-a-iso").await, 1);
        assert_eq!(manager.get_participant_count("room-b-iso").await, 1); // unaffected

        cleanup_test_room(&pool, "room-a-iso").await;
        cleanup_test_room(&pool, "room-b-iso").await;
        teardown_meeting_management();
    }

    // ==========================================================================
    // TEST 6: Protobuf packet builders work correctly
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_protobuf_packet_builders() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        // Test MEETING_STARTED packet
        let meeting_started =
            SessionManager::build_meeting_started_packet("my-room", 1234567890, "alice");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_started).unwrap();
        assert_eq!(
            wrapper.packet_type,
            PacketType::MEETING.into(),
            "Should be MEETING packet type"
        );
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_STARTED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.start_time_ms, 1234567890);
        assert_eq!(inner.creator_id, "alice");

        // Test MEETING_ENDED packet
        let meeting_ended = SessionManager::build_meeting_ended_packet("my-room", "Host left");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_ended).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_ENDED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.message, "Host left");
    }

    // ==========================================================================
    // TEST 7: Reserved system email is rejected
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_system_email_rejected() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-system-7";

        cleanup_test_room(&pool, room_id).await;

        // Attempt to join with reserved system email should fail
        let result = manager.start_session(room_id, SYSTEM_USER_EMAIL).await;
        assert!(result.is_err(), "Should reject reserved system email");

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("reserved system email"),
            "Error message should mention reserved system email: {err}"
        );

        // Verify no participant was added (with FF enabled for this check)
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
    // TEST 8: Feature flag OFF - start_session returns defaults without DB ops
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_off_start_session_returns_defaults() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-ff-off-8";

        cleanup_test_room(&pool, room_id).await;

        // Disable feature flag
        FeatureFlags::set_meeting_management_override(false);

        // start_session should return defaults without touching DB
        let result = manager.start_session(room_id, "alice").await.unwrap();

        // Should return a valid start time (current time)
        assert!(result.start_time_ms > 0, "Should have a start time");
        assert!(
            result.is_first_participant,
            "Should default to first participant"
        );

        // Enable FF to verify no DB records were created
        FeatureFlags::set_meeting_management_override(true);
        assert_eq!(
            manager.get_participant_count(room_id).await,
            0,
            "No participant should be in DB when FF is off"
        );
        let meeting = manager.get_meeting_info(room_id).await.unwrap();
        assert!(
            meeting.is_none(),
            "No meeting should be created when FF is off"
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 9: Feature flag OFF - end_session returns no-op
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_off_end_session_returns_noop() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-ff-off-9";

        cleanup_test_room(&pool, room_id).await;

        // Disable feature flag
        FeatureFlags::set_meeting_management_override(false);

        // end_session should return no-op result
        let result = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(
            result,
            SessionEndResult::MeetingContinues { remaining_count: 0 },
            "Should return no-op result when FF is off"
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 10: Feature flag ON - normal behavior with DB operations
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_on_normal_behavior() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-ff-on-10";

        cleanup_test_room(&pool, room_id).await;

        // Enable feature flag explicitly
        FeatureFlags::set_meeting_management_override(true);

        // start_session should create DB records
        let result = manager.start_session(room_id, "alice").await.unwrap();
        assert!(result.is_first_participant);
        assert!(result.start_time_ms > 0);

        // Verify DB records exist
        assert_eq!(
            manager.get_participant_count(room_id).await,
            1,
            "Participant should be in DB"
        );
        let meeting = manager.get_meeting_info(room_id).await.unwrap();
        assert!(meeting.is_some(), "Meeting should be created in DB");
        assert_eq!(
            meeting.unwrap().creator_id,
            Some("alice".to_string()),
            "Alice should be creator"
        );

        // end_session should update DB
        let end_result = manager.end_session(room_id, "alice").await.unwrap();
        assert_eq!(
            end_result,
            SessionEndResult::LastParticipantLeft,
            "Should end meeting when last participant leaves"
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }

    // ==========================================================================
    // TEST 11: Feature flag OFF - is_host returns false
    // ==========================================================================
    #[tokio::test]
    #[serial]
    async fn test_feature_flag_off_is_host_returns_false() {
        let pool = get_test_pool().await;
        let manager = SessionManager::new(pool.clone());
        let room_id = "test-room-ff-host-11";

        cleanup_test_room(&pool, room_id).await;

        // First create a meeting with FF on
        FeatureFlags::set_meeting_management_override(true);
        let _ = manager.start_session(room_id, "alice").await.unwrap();
        assert!(
            manager.is_host(room_id, "alice").await,
            "Alice should be host"
        );

        // Now disable FF - is_host should return false
        FeatureFlags::set_meeting_management_override(false);
        assert!(
            !manager.is_host(room_id, "alice").await,
            "is_host should return false when FF is off"
        );

        FeatureFlags::clear_meeting_management_override();
        cleanup_test_room(&pool, room_id).await;
    }
}
