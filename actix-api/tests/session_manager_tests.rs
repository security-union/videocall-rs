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

//! Integration tests for SessionManager (moved from inline `#[cfg(test)]` module).

use protobuf::Message as ProtoMessage;
use sec_api::session_manager::{SessionEndResult, SessionManager};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::user_id::to_user_id_bytes;
use videocall_types::SYSTEM_USER_ID;

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
    let packet = SessionManager::build_peer_joined_packet("my-room", "bob", 42, "Bob Smith");
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
}

#[tokio::test]
async fn test_build_peer_left_packet() {
    let packet = SessionManager::build_peer_left_packet("my-room", "alice", 99, "Alice Jones");
    let wrapper = PacketWrapper::parse_from_bytes(&packet).unwrap();
    assert_eq!(wrapper.packet_type, PacketType::MEETING.into());

    let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
    assert_eq!(inner.event_type, MeetingEventType::PARTICIPANT_LEFT.into());
    assert_eq!(inner.room_id, "my-room");
    assert_eq!(inner.target_user_id, to_user_id_bytes("alice"));
    assert_eq!(inner.session_id, 99);
}

/// Verify that build_peer_joined_packet and build_peer_left_packet are
/// structurally symmetric: same fields populated, only event_type differs.
/// Also verify outer wrapper session_id is 0 (system messages).
#[tokio::test]
async fn test_peer_joined_and_left_packets_are_symmetric() {
    let room = "symmetry-room";
    let user = "charlie";
    let sid = 77u64;
    let display = "Charlie Brown";

    let joined_bytes = SessionManager::build_peer_joined_packet(room, user, sid, display);
    let left_bytes = SessionManager::build_peer_left_packet(room, user, sid, display);

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
