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

//! Integration tests for videofeed-reliability fixes:
//! - `build_room_subject()` NATS subject formatting
//! - PARTICIPANT_LEFT publishing on Leave and Disconnect
//! - CleanupFailedJoin removes stale room_members entries

use actix::{Actor, Handler};
use futures::StreamExt;
use protobuf::Message as ProtobufMessage;
use sec_api::{
    actors::chat_server::{ChatServer, CleanupFailedJoin, GetRoomMembers},
    messages::{
        server::{Connect, Disconnect, JoinRoom, Leave},
        session::Message,
    },
    models::build_room_subject,
};
use serial_test::serial;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

// =========================================================================
// Helpers
// =========================================================================

struct DummySession;
impl Actor for DummySession {
    type Context = actix::Context<Self>;
}
impl Handler<Message> for DummySession {
    type Result = ();
    fn handle(&mut self, _msg: Message, _ctx: &mut Self::Context) {}
}

// =========================================================================
// Unit tests: build_room_subject
// =========================================================================

#[test]
fn test_build_room_subject_basic() {
    assert_eq!(build_room_subject("my-room"), "room.my-room.*");
}

#[test]
fn test_build_room_subject_replaces_spaces() {
    assert_eq!(build_room_subject("my room"), "room.my_room.*");
}

#[test]
fn test_build_room_subject_empty_room() {
    assert_eq!(build_room_subject(""), "room..*");
}

// =========================================================================
// Integration test: Leave publishes PARTICIPANT_LEFT via NATS
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_leave_publishes_participant_left_via_nats() {
    use tokio::time::{sleep, Duration};

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat_server = ChatServer::new(nats_client.clone()).await.start();

    let dummy = DummySession.start();
    let session_id = 4001u64;
    let room = "test-leave-nats".to_string();
    let email = "leaver@example.com".to_string();

    // Subscribe to room system subject BEFORE join
    let system_subject = format!("room.{}.system", room.replace(' ', "_"));
    let mut sub = nats_client
        .subscribe(system_subject)
        .await
        .expect("Failed to subscribe to system subject");

    // Register and join
    chat_server
        .send(Connect {
            id: session_id,
            addr: dummy.recipient(),
        })
        .await
        .unwrap();
    chat_server
        .send(JoinRoom {
            session: session_id,
            room: room.clone(),
            user_id: email.clone(),
        })
        .await
        .unwrap()
        .unwrap();

    // Drain MEETING_STARTED messages
    sleep(Duration::from_millis(500)).await;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await {}

    // Send Leave
    chat_server
        .send(Leave {
            session: session_id,
            room: room.clone(),
            user_id: email.clone(),
        })
        .await
        .unwrap();

    // Wait for PARTICIPANT_LEFT to be published
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("Timed out waiting for PARTICIPANT_LEFT")
        .expect("NATS subscription ended");

    let wrapper = PacketWrapper::parse_from_bytes(&msg.payload).unwrap();
    assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
    assert_eq!(wrapper.email, email);
    assert_eq!(wrapper.session_id, session_id);

    let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
    assert_eq!(
        inner.event_type,
        MeetingEventType::PARTICIPANT_LEFT.into()
    );
    assert_eq!(inner.room_id, room);
}

// =========================================================================
// Integration test: Disconnect publishes PARTICIPANT_LEFT via NATS
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_disconnect_publishes_participant_left_via_nats() {
    use tokio::time::{sleep, Duration};

    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat_server = ChatServer::new(nats_client.clone()).await.start();

    let dummy = DummySession.start();
    let session_id = 4002u64;
    let room = "test-disconnect-nats".to_string();
    let email = "disconnecter@example.com".to_string();

    // Subscribe to room system subject BEFORE join
    let system_subject = format!("room.{}.system", room.replace(' ', "_"));
    let mut sub = nats_client
        .subscribe(system_subject)
        .await
        .expect("Failed to subscribe to system subject");

    // Register and join
    chat_server
        .send(Connect {
            id: session_id,
            addr: dummy.recipient(),
        })
        .await
        .unwrap();
    chat_server
        .send(JoinRoom {
            session: session_id,
            room: room.clone(),
            user_id: email.clone(),
        })
        .await
        .unwrap()
        .unwrap();

    // Drain MEETING_STARTED messages
    sleep(Duration::from_millis(500)).await;
    while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(100), sub.next()).await {}

    // Send Disconnect (instead of Leave)
    chat_server
        .send(Disconnect {
            session: session_id,
            room: room.clone(),
            user_id: email.clone(),
        })
        .await
        .unwrap();

    // Wait for PARTICIPANT_LEFT to be published
    let msg = tokio::time::timeout(Duration::from_secs(2), sub.next())
        .await
        .expect("Timed out waiting for PARTICIPANT_LEFT")
        .expect("NATS subscription ended");

    let wrapper = PacketWrapper::parse_from_bytes(&msg.payload).unwrap();
    assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
    assert_eq!(wrapper.email, email);
    assert_eq!(wrapper.session_id, session_id);

    let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
    assert_eq!(
        inner.event_type,
        MeetingEventType::PARTICIPANT_LEFT.into()
    );
    assert_eq!(inner.room_id, room);
}

// =========================================================================
// Integration test: CleanupFailedJoin removes room_members entry
// =========================================================================

#[actix_rt::test]
#[serial]
async fn test_cleanup_failed_join_removes_room_members() {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://nats:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .expect("Failed to connect to NATS");

    let chat_server = ChatServer::new(nats_client).await.start();

    let dummy = DummySession.start();
    let session_id = 4003u64;
    let room = "test-cleanup-room".to_string();

    // Register and join
    chat_server
        .send(Connect {
            id: session_id,
            addr: dummy.recipient(),
        })
        .await
        .unwrap();
    chat_server
        .send(JoinRoom {
            session: session_id,
            room: room.clone(),
            user_id: "cleanup-user@example.com".to_string(),
        })
        .await
        .unwrap()
        .unwrap();

    // Verify room_members has 1 entry
    let members = chat_server
        .send(GetRoomMembers { room: room.clone() })
        .await
        .unwrap();
    assert!(members.is_some(), "Room should have members after join");
    assert_eq!(members.unwrap().len(), 1);

    // Send CleanupFailedJoin directly
    chat_server
        .send(CleanupFailedJoin {
            session: session_id,
            room: room.clone(),
        })
        .await
        .unwrap();

    // Verify room_members is cleaned up (None since last member was removed)
    let members = chat_server
        .send(GetRoomMembers { room: room.clone() })
        .await
        .unwrap();
    assert!(
        members.is_none(),
        "Room should be removed after CleanupFailedJoin for last member"
    );
}
