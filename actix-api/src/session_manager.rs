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
//! Meeting lifecycle (create, end, host management) is handled by meeting-api.
//! The media server validates JWT room access tokens for authorization.
//! This module provides protobuf packet builders for real-time signaling.

use protobuf::Message as ProtoMessage;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::SYSTEM_USER_EMAIL;

/// Error type for session management operations
#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    /// User tried to use the reserved system email
    ReservedUserEmail,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::ReservedUserEmail => {
                write!(f, "Cannot use reserved system email as user ID")
            }
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
    pub session_id: u64,
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
/// Meeting lifecycle is managed by meeting-api; authorization via JWT.
/// No database dependency -- all meeting state lives in meeting-api.
#[derive(Debug, Clone)]
pub struct SessionManager;

impl SessionManager {
    pub fn new() -> Self {
        Self
    }

    /// Called when a user connects to a room.
    ///
    /// JWT validation happens at the handler level (ws_connect / WebTransport)
    /// before this method is called. By the time we get here the token has
    /// already been verified.
    ///
    /// # Errors
    /// Returns `SessionError::ReservedUserEmail` if user_id matches the system email.
    pub async fn start_session(
        &self,
        room_id: &str,
        user_id: &str,
        id: u64,
    ) -> Result<SessionStartResult, Box<dyn std::error::Error + Send + Sync>> {
        if user_id == SYSTEM_USER_EMAIL {
            return Err(Box::new(SessionError::ReservedUserEmail));
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        info!("Session started for {} in room {}", user_id, room_id);

        Ok(SessionStartResult {
            start_time_ms: now_ms,
            is_first_participant: true,
            creator_id: user_id.to_string(),
            session_id: id,
        })
    }

    /// Called when a user disconnects from a room.
    pub async fn end_session(
        &self,
        room_id: &str,
        user_id: &str,
    ) -> Result<SessionEndResult, Box<dyn std::error::Error + Send + Sync>> {
        info!("Session ended for {} in room {}", user_id, room_id);
        Ok(SessionEndResult::MeetingContinues { remaining_count: 0 })
    }

    /// Build SESSION_ASSIGNED packet: server sends immediately after connect.
    /// Client stores session_id for heartbeats, DIAGNOSTICS, self-packet filtering.
    pub fn build_session_assigned_packet(session_id: u64) -> Vec<u8> {
        let wrapper = PacketWrapper {
            packet_type: PacketType::SESSION_ASSIGNED.into(),
            email: SYSTEM_USER_EMAIL.to_string(),
            session_id,
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Build MEETING_STARTED packet to send to client (protobuf)
    pub fn build_meeting_started_packet(
        room_id: &str,
        start_time_ms: u64,
        creator_id: &str,
        session_id: u64,
    ) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_STARTED.into(),
            room_id: room_id.to_string(),
            start_time_ms,
            creator_id: creator_id.to_string(),
            session_id,
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            email: SYSTEM_USER_EMAIL.to_string(),
            session_id,
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
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_start_session_returns_result() {
        let manager = SessionManager::new();
        let result = manager
            .start_session("room-1", "alice", 12345)
            .await
            .unwrap();
        assert!(result.start_time_ms > 0);
        assert!(result.is_first_participant);
        assert_eq!(result.creator_id, "alice");
        assert_eq!(result.session_id, 12345);
    }

    #[tokio::test]
    async fn test_system_email_rejected() {
        let manager = SessionManager::new();
        let result = manager.start_session("room-1", SYSTEM_USER_EMAIL, 0).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("reserved system email"));
    }

    #[tokio::test]
    async fn test_end_session_returns_result() {
        let manager = SessionManager::new();
        let result = manager.end_session("room-1", "alice").await.unwrap();
        assert_eq!(
            result,
            SessionEndResult::MeetingContinues { remaining_count: 0 }
        );
    }

    #[tokio::test]
    async fn test_protobuf_packet_builders() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let meeting_started =
            SessionManager::build_meeting_started_packet("my-room", 1234567890, "alice", 98765);
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_started).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_STARTED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.start_time_ms, 1234567890);
        assert_eq!(inner.creator_id, "alice");
        assert_eq!(inner.session_id, 98765);

        let meeting_ended = SessionManager::build_meeting_ended_packet("my-room", "Host left");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_ended).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_ENDED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.message, "Host left");
    }
}
