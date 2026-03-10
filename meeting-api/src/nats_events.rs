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

//! NATS event publishers for meeting lifecycle notifications.
//!
//! Each function accepts `Option<&async_nats::Client>` and is a no-op when
//! NATS is not configured (graceful degradation).

use protobuf::Message;
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

/// System email used as the sender for server-generated NATS packets.
const SYSTEM_EMAIL: &str = "system-&^%$#@!";

/// Build a `PacketWrapper` containing a serialized `MeetingPacket`.
fn build_meeting_wrapper(meeting_packet: &MeetingPacket) -> Vec<u8> {
    let wrapper = PacketWrapper {
        packet_type: PacketType::MEETING.into(),
        email: SYSTEM_EMAIL.to_string(),
        data: meeting_packet.write_to_bytes().unwrap_or_default(),
        ..Default::default()
    };
    wrapper.write_to_bytes().unwrap_or_default()
}

/// Sanitize a room ID for use in NATS subjects by replacing special characters.
fn sanitize_room_id(room_id: &str) -> String {
    room_id
        .chars()
        .map(|c| match c {
            '.' | '*' | '>' | ' ' | '\t' | '\n' | '\r' => '_',
            _ => c,
        })
        .collect()
}

/// NATS subject for system messages in a room.
fn room_system_subject(room_id: &str) -> String {
    format!("room.{}.system", sanitize_room_id(room_id))
}

/// Publish a serialized packet to NATS subject. Logs errors but never fails.
async fn publish(nats: &async_nats::Client, subject: String, payload: Vec<u8>) {
    if let Err(e) = nats.publish(subject.clone(), payload.into()).await {
        tracing::error!("Failed to publish NATS event to {subject}: {e}");
    }
}

/// Publish `MEETING_ACTIVATED` when the host activates/starts a meeting.
pub async fn publish_meeting_activated(nats: Option<&async_nats::Client>, room_id: &str) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::MEETING_ACTIVATED.into(),
        room_id: room_id.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published MEETING_ACTIVATED for room {room_id}");
}

/// Publish `PARTICIPANT_ADMITTED` when a participant is admitted from the waiting room.
///
/// The room token is NOT included in the broadcast. The admitted client must
/// fetch its token via HTTP after receiving this notification.
pub async fn publish_participant_admitted(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_email: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_ADMITTED.into(),
        room_id: room_id.to_string(),
        target_email: target_email.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published PARTICIPANT_ADMITTED for {target_email} in room {room_id}");
}

/// Publish `PARTICIPANT_REJECTED` when a participant is rejected from the waiting room.
pub async fn publish_participant_rejected(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_email: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_REJECTED.into(),
        room_id: room_id.to_string(),
        target_email: target_email.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published PARTICIPANT_REJECTED for {target_email} in room {room_id}");
}

/// Publish `WAITING_ROOM_UPDATED` when the waiting room list changes.
pub async fn publish_waiting_room_updated(nats: Option<&async_nats::Client>, room_id: &str) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::WAITING_ROOM_UPDATED.into(),
        room_id: room_id.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published WAITING_ROOM_UPDATED for room {room_id}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
    use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;

    #[test]
    fn test_build_meeting_activated_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_ACTIVATED.into(),
            room_id: "test-room".to_string(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        assert_eq!(wrapper.packet_type, PacketType::MEETING.into());
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::MEETING_ACTIVATED.into());
        assert_eq!(inner.room_id, "test-room");
    }

    #[test]
    fn test_build_participant_admitted_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_ADMITTED.into(),
            room_id: "test-room".to_string(),
            target_email: "alice@example.com".to_string(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_ADMITTED.into()
        );
        assert_eq!(inner.target_email, "alice@example.com");
        assert!(
            inner.room_token.is_empty(),
            "room_token must not be broadcast via NATS"
        );
    }

    #[test]
    fn test_build_participant_rejected_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_REJECTED.into(),
            room_id: "test-room".to_string(),
            target_email: "bob@example.com".to_string(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_REJECTED.into()
        );
        assert_eq!(inner.target_email, "bob@example.com");
    }

    #[test]
    fn test_build_waiting_room_updated_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::WAITING_ROOM_UPDATED.into(),
            room_id: "test-room".to_string(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::WAITING_ROOM_UPDATED.into()
        );
    }

    #[test]
    fn test_room_system_subject() {
        assert_eq!(room_system_subject("my-room"), "room.my-room.system");
        assert_eq!(
            room_system_subject("room with spaces"),
            "room.room_with_spaces.system"
        );
    }

    #[test]
    fn test_sanitize_room_id() {
        assert_eq!(sanitize_room_id("simple"), "simple");
        assert_eq!(sanitize_room_id("has spaces"), "has_spaces");
        assert_eq!(sanitize_room_id("has.dots"), "has_dots");
        assert_eq!(sanitize_room_id("has*stars"), "has_stars");
        assert_eq!(sanitize_room_id("has>gt"), "has_gt");
        assert_eq!(sanitize_room_id("a.b*c>d e"), "a_b_c_d_e");
    }

    #[tokio::test]
    async fn test_nats_none_is_noop() {
        // All publish functions should be no-ops when nats is None.
        publish_meeting_activated(None, "room").await;
        publish_participant_admitted(None, "room", "user@test.com").await;
        publish_participant_rejected(None, "room", "user@test.com").await;
        publish_waiting_room_updated(None, "room").await;
    }
}
