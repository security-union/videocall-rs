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
use videocall_types::user_id::to_user_id_bytes;
use videocall_types::SYSTEM_USER_ID;

/// Error type for session management operations
#[derive(Debug, Clone, PartialEq)]
pub enum SessionError {
    /// User tried to use the reserved system user ID
    ReservedUserId,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::ReservedUserId => {
                write!(f, "Cannot use reserved system user ID")
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
    /// Returns `SessionError::ReservedUserId` if user_id matches the system user ID.
    pub async fn start_session(
        &self,
        room_id: &str,
        user_id: &str,
        id: u64,
    ) -> Result<SessionStartResult, Box<dyn std::error::Error + Send + Sync>> {
        if user_id == SYSTEM_USER_ID {
            return Err(Box::new(SessionError::ReservedUserId));
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
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
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
    ) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_STARTED.into(),
            room_id: room_id.to_string(),
            start_time_ms,
            creator_id: to_user_id_bytes(creator_id),
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };

        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Build PARTICIPANT_JOINED packet to notify peers about a new session joining the room.
    ///
    /// The `display_name` field carries the participant's display name so the
    /// client can show friendly toast messages. The `is_guest` flag is sourced
    /// from the authenticated JWT `is_guest` claim.
    pub fn build_peer_joined_packet(
        room_id: &str,
        user_id: &str,
        session_id: u64,
        display_name: &str,
        is_guest: bool,
    ) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_JOINED.into(),
            room_id: room_id.to_string(),
            message: format!("{} has joined the meeting", user_id),
            target_user_id: to_user_id_bytes(user_id),
            session_id,
            display_name: display_name.as_bytes().to_vec(),
            is_guest,
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };

        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Build PARTICIPANT_LEFT packet to notify remaining peers about a departed session.
    pub fn build_peer_left_packet(
        room_id: &str,
        user_id: &str,
        session_id: u64,
        display_name: &str,
        is_guest: bool,
    ) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_LEFT.into(),
            room_id: room_id.to_string(),
            message: format!("{} has left the meeting", user_id),
            target_user_id: to_user_id_bytes(user_id),
            session_id,
            display_name: display_name.as_bytes().to_vec(),
            is_guest,
            ..Default::default()
        };

        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };

        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Build PARTICIPANT_LIST_REQUEST packet.
    pub fn build_participant_list_request(room_id: &str, requester_session: u64) -> Vec<u8> {
        let meeting_packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_LIST_REQUEST.into(),
            room_id: room_id.to_string(),
            session_id: requester_session,
            ..Default::default()
        };
        let wrapper = PacketWrapper {
            packet_type: PacketType::MEETING.into(),
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
            data: meeting_packet.write_to_bytes().unwrap_or_default(),
            ..Default::default()
        };
        wrapper.write_to_bytes().unwrap_or_default()
    }

    /// Resolve the `(subject, bytes)` a joiner should publish to ask existing
    /// peers to re-announce themselves, or `None` for observer sessions (which
    /// never request a peer list).
    ///
    /// The request goes to the room system subject (`room.{room}.system`); the
    /// reply comes back addressed to `requester_session`.
    pub fn participant_list_request_publication(
        observer: bool,
        room_id: &str,
        requester_session: u64,
    ) -> Option<(String, Vec<u8>)> {
        if observer {
            return None;
        }
        let subject = format!("room.{}.system", room_id.replace(' ', "_"));
        let bytes = Self::build_participant_list_request(room_id, requester_session);
        Some((subject, bytes))
    }

    /// Resolve the `(subject, bytes)` a peer should publish in reply to a
    /// PARTICIPANT_LIST_REQUEST, or `None` when no reply should be sent.
    ///
    /// The reply is a PARTICIPANT_JOINED for the responding peer, addressed to
    /// the requester's per-session subject (`room.{room}.{requester}`) so the
    /// `handle_msg` unicast filter delivers it only to that requester.
    ///
    /// Returns `None` when:
    /// * the responder is not yet `Active` (`responder_is_active == false`) —
    ///   only elected connections announce themselves; or
    /// * the requester is on THIS server instance (`requester_is_local`) — the
    ///   in-memory existing-member replay in JoinRoom already delivered the
    ///   PARTICIPANT_JOINED directly, so a NATS reply would duplicate it.
    #[allow(clippy::too_many_arguments)]
    pub fn rebroadcast_reply_publication(
        room_id: &str,
        user_id: &str,
        display_name: &str,
        responder_session: u64,
        requester_session: u64,
        is_guest: bool,
        responder_is_active: bool,
        requester_is_local: bool,
    ) -> Option<(String, Vec<u8>)> {
        if !responder_is_active || requester_is_local {
            return None;
        }
        let subject = format!("room.{}.{}", room_id.replace(' ', "_"), requester_session);
        let bytes = Self::build_peer_joined_packet(
            room_id,
            user_id,
            responder_session,
            display_name,
            is_guest,
        );
        Some((subject, bytes))
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
            user_id: to_user_id_bytes(SYSTEM_USER_ID),
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
    async fn test_system_user_id_rejected() {
        let manager = SessionManager::new();
        let result = manager.start_session("room-1", SYSTEM_USER_ID, 0).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("reserved system user ID"));
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
            SessionManager::build_meeting_started_packet("my-room", 1234567890, "alice");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_started).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_STARTED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.start_time_ms, 1234567890);
        assert_eq!(inner.creator_id, to_user_id_bytes("alice"));

        let meeting_ended = SessionManager::build_meeting_ended_packet("my-room", "Host left");
        let wrapper = PacketWrapper::parse_from_bytes(&meeting_ended).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_ENDED.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.message, "Host left");
    }

    #[tokio::test]
    async fn test_build_peer_joined_packet() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let packet =
            SessionManager::build_peer_joined_packet("my-room", "bob", 42, "Bob Smith", false);
        let wrapper = PacketWrapper::parse_from_bytes(&packet).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        assert_eq!(wrapper.user_id, to_user_id_bytes(SYSTEM_USER_ID));

        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_JOINED.into()
        );
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.target_user_id, to_user_id_bytes("bob"));
        assert_eq!(inner.session_id, 42);
        assert!(inner.message.contains("bob"));
        assert_eq!(inner.display_name, "Bob Smith".as_bytes().to_vec());
        assert!(!inner.is_guest);

        // Guest path: is_guest = true propagates end-to-end.
        let packet_guest = SessionManager::build_peer_joined_packet(
            "my-room",
            "guest:1234",
            43,
            "Guest Bob",
            true,
        );
        let wrapper_guest = PacketWrapper::parse_from_bytes(&packet_guest).unwrap();
        let inner_guest = MeetingPacket::parse_from_bytes(&wrapper_guest.data).unwrap();
        assert!(inner_guest.is_guest);
    }

    #[tokio::test]
    async fn test_build_peer_left_packet() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let packet =
            SessionManager::build_peer_left_packet("my-room", "alice", 99, "Alice Jones", false);
        let wrapper = PacketWrapper::parse_from_bytes(&packet).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());

        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::PARTICIPANT_LEFT.into());
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.target_user_id, to_user_id_bytes("alice"));
        assert_eq!(inner.session_id, 99);
    }

    #[tokio::test]
    async fn test_build_participant_list_request() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        // The requesting joiner's session is carried in `session_id` so peers
        // can ignore their own request and address the reply back to it.
        let requester_session = 4242u64;
        let packet = SessionManager::build_participant_list_request("my-room", requester_session);

        let wrapper = PacketWrapper::parse_from_bytes(&packet).unwrap();
        // Must be a server-authoritative MEETING packet; `classify_packet`
        // drops client-originated MEETING packets, so clients cannot forge it.
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        assert_eq!(wrapper.user_id, to_user_id_bytes(SYSTEM_USER_ID));

        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_LIST_REQUEST.into()
        );
        assert_eq!(inner.room_id, "my-room");
        assert_eq!(inner.session_id, requester_session);
    }

    #[tokio::test]
    async fn test_participant_list_request_publication() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        // Observer sessions never request a peer list.
        assert!(
            SessionManager::participant_list_request_publication(true, "my-room", 7).is_none(),
            "observers must not publish a PARTICIPANT_LIST_REQUEST"
        );

        // Non-observers publish to the room system subject (with spaces in the
        // room id sanitized to underscores) and carry their session as requester.
        let (subject, bytes) =
            SessionManager::participant_list_request_publication(false, "my room", 7)
                .expect("non-observer must produce a publication");
        assert_eq!(subject, "room.my_room.system");

        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_LIST_REQUEST.into()
        );
        assert_eq!(inner.session_id, 7);
    }

    #[tokio::test]
    async fn test_rebroadcast_reply_publication() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        // Not-yet-Active responder → no reply (only elected connections announce).
        assert!(
            SessionManager::rebroadcast_reply_publication(
                "my-room",
                "alice@x.com",
                "Alice",
                10,
                20,
                false,
                /*active*/ false,
                /*requester_local*/ false,
            )
            .is_none(),
            "a non-Active responder must not reply"
        );

        // Local requester → no reply (the in-memory replay already delivered it;
        // a NATS reply would duplicate on the client). This is the short-circuit.
        assert!(
            SessionManager::rebroadcast_reply_publication(
                "my-room",
                "alice@x.com",
                "Alice",
                10,
                20,
                false,
                /*active*/ true,
                /*requester_local*/ true,
            )
            .is_none(),
            "a local requester must be served by the in-memory replay, not a NATS reply"
        );

        // Active responder + remote requester → reply addressed to the
        // requester's per-session subject, carrying the responder's
        // PARTICIPANT_JOINED.
        let (subject, bytes) = SessionManager::rebroadcast_reply_publication(
            "my room",
            "alice@x.com",
            "Alice",
            10,
            20,
            true,
            /*active*/ true,
            /*requester_local*/ false,
        )
        .expect("Active responder + remote requester must produce a reply");
        // Subject targets the REQUESTER (20), not the responder (10), with the
        // room id sanitized.
        assert_eq!(subject, "room.my_room.20");

        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_JOINED.into()
        );
        // The packet announces the RESPONDER's session (10) and identity.
        assert_eq!(inner.session_id, 10);
        assert_eq!(inner.target_user_id, to_user_id_bytes("alice@x.com"));
        assert!(inner.is_guest, "is_guest must propagate into the reply");
    }

    /// Verify that build_peer_joined_packet and build_peer_left_packet are
    /// structurally symmetric: same fields populated, only event_type differs.
    /// Also verify outer wrapper session_id is 0 (system messages).
    #[tokio::test]
    async fn test_peer_joined_and_left_packets_are_symmetric() {
        use videocall_types::protos::meeting_packet::MeetingPacket;
        use videocall_types::protos::packet_wrapper::PacketWrapper;

        let room = "symmetry-room";
        let user = "charlie";
        let sid = 77u64;
        let display = "Charlie Brown";

        let joined_bytes =
            SessionManager::build_peer_joined_packet(room, user, sid, display, false);
        let left_bytes = SessionManager::build_peer_left_packet(room, user, sid, display, false);

        let joined_wrapper = PacketWrapper::parse_from_bytes(&joined_bytes).unwrap();
        let left_wrapper = PacketWrapper::parse_from_bytes(&left_bytes).unwrap();

        // Both wrappers should be system messages with session_id 0
        assert_eq!(
            joined_wrapper.session_id, 0,
            "PARTICIPANT_JOINED wrapper session_id should be 0"
        );
        assert_eq!(
            left_wrapper.session_id, 0,
            "PARTICIPANT_LEFT wrapper session_id should be 0"
        );
        assert_eq!(joined_wrapper.user_id, to_user_id_bytes(SYSTEM_USER_ID));
        assert_eq!(left_wrapper.user_id, to_user_id_bytes(SYSTEM_USER_ID));
        assert_eq!(joined_wrapper.packet_type, left_wrapper.packet_type);

        let joined_inner = MeetingPacket::parse_from_bytes(&joined_wrapper.data).unwrap();
        let left_inner = MeetingPacket::parse_from_bytes(&left_wrapper.data).unwrap();

        // Same fields populated
        assert_eq!(joined_inner.room_id, left_inner.room_id);
        assert_eq!(joined_inner.target_user_id, left_inner.target_user_id);
        assert_eq!(joined_inner.session_id, left_inner.session_id);
        assert_eq!(joined_inner.session_id, sid);
        // display_name carried via dedicated display_name field
        assert_eq!(joined_inner.display_name, display.as_bytes().to_vec());
        assert_eq!(left_inner.display_name, display.as_bytes().to_vec());

        // Only event_type and message differ
        assert_eq!(
            joined_inner.event_type,
            MeetingEventType::PARTICIPANT_JOINED.into()
        );
        assert_eq!(
            left_inner.event_type,
            MeetingEventType::PARTICIPANT_LEFT.into()
        );
        assert!(joined_inner.message.contains("joined"));
        assert!(left_inner.message.contains("left"));
    }
}
