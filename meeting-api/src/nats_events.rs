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
use serde::{Deserialize, Serialize};
use videocall_types::protos::meeting_packet::meeting_packet::MeetingEventType;
use videocall_types::protos::meeting_packet::MeetingPacket;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::SYSTEM_USER_ID;

/// NATS subject for fanning out per-meeting policy flag changes to every
/// `actix-api` chat_server instance. The chat_server caches these flags at
/// JoinRoom time from the JWT, so without this fanout a mid-meeting PATCH
/// would not take effect until the host reconnected.
///
/// Mirrors the public `MEETING_SETTINGS_UPDATED` protobuf event but uses
/// JSON over a separate internal subject so the wire format for clients
/// stays untouched. The corresponding consumer lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_SETTINGS_UPDATE_SUBJECT`).
pub const MEETING_SETTINGS_UPDATE_SUBJECT: &str = "internal.meeting_settings_updated";

/// NATS subject consumed by `meeting-api` to write `state='ended'` to the
/// `meetings` table when `actix-api` broadcasts MEETING_ENDED on a host
/// disconnect with `end_on_host_leave=true`. Mirrors the REST POST /leave
/// flow's `db_meetings::end_meeting` call so the meetings list stays
/// consistent with the broadcast clients receive.
///
/// The corresponding publisher lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_ENDED_BY_HOST_SUBJECT`).
pub const MEETING_ENDED_BY_HOST_SUBJECT: &str = "internal.meeting_ended_by_host";

/// Payload published on [`MEETING_SETTINGS_UPDATE_SUBJECT`].
///
/// Carries the four per-meeting policy flags so chat_server can refresh
/// its full `RoomPolicy` snapshot without a DB round-trip. All flags are
/// always populated — the consumer overwrites the cache wholesale rather
/// than merging field-by-field.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingSettingsUpdatePayload {
    pub room_id: String,
    pub end_on_host_leave: bool,
    pub admitted_can_admit: bool,
    pub waiting_room_enabled: bool,
    pub allow_guests: bool,
}

/// Payload consumed on [`MEETING_ENDED_BY_HOST_SUBJECT`].
///
/// Sent by chat_server when the host-leave broadcast fires. The `meeting-api`
/// consumer looks up the meeting by `room_id` and transitions its DB row
/// to `state='ended'`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingEndedByHostPayload {
    pub room_id: String,
}

/// Build a `PacketWrapper` containing a serialized `MeetingPacket`.
fn build_meeting_wrapper(meeting_packet: &MeetingPacket) -> Vec<u8> {
    let wrapper = PacketWrapper {
        packet_type: PacketType::MEETING.into(),
        user_id: SYSTEM_USER_ID.as_bytes().to_vec(),
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
    target_user_id: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_ADMITTED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published PARTICIPANT_ADMITTED for {target_user_id} in room {room_id}");
}

/// Publish `PARTICIPANT_REJECTED` when a participant is rejected from the waiting room.
pub async fn publish_participant_rejected(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_REJECTED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published PARTICIPANT_REJECTED for {target_user_id} in room {room_id}");
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

/// Publish `PARTICIPANT_DISPLAY_NAME_CHANGED` when a participant updates their display name.
pub async fn publish_participant_display_name_changed(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    new_display_name: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        display_name: new_display_name.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!(
        "Published PARTICIPANT_DISPLAY_NAME_CHANGED for {target_user_id} in room {room_id}: {}",
        new_display_name
    );
}

/// Publish `MEETING_SETTINGS_UPDATED` when meeting settings change.
pub async fn publish_meeting_settings_updated(nats: Option<&async_nats::Client>, room_id: &str) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::MEETING_SETTINGS_UPDATED.into(),
        room_id: room_id.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published MEETING_SETTINGS_UPDATED for room {room_id}");
}

/// Publish a server-internal [`MEETING_SETTINGS_UPDATE_SUBJECT`] event so
/// every `actix-api` chat_server instance refreshes its in-memory
/// `room_policy` cache. Caller passes the post-update authoritative flag
/// values (typically the `MeetingRow` fields after a successful
/// `update_meeting_settings` call).
///
/// Distinct from [`publish_meeting_settings_updated`]: that one tells
/// **clients** to re-fetch settings via REST, this one tells **servers**
/// to refresh their cache without any DB lookup.
pub async fn publish_internal_meeting_settings_update(
    nats: Option<&async_nats::Client>,
    payload: &MeetingSettingsUpdatePayload,
) {
    let Some(nats) = nats else { return };
    let bytes = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                "Failed to serialize MeetingSettingsUpdatePayload for {}: {e}",
                payload.room_id
            );
            return;
        }
    };
    if let Err(e) = nats
        .publish(MEETING_SETTINGS_UPDATE_SUBJECT, bytes.into())
        .await
    {
        tracing::error!(
            "Failed to publish {} for {}: {e}",
            MEETING_SETTINGS_UPDATE_SUBJECT,
            payload.room_id
        );
    } else {
        tracing::debug!(
            "Published {} for room {} (end_on_host_leave={}, admitted_can_admit={}, \
             waiting_room_enabled={}, allow_guests={})",
            MEETING_SETTINGS_UPDATE_SUBJECT,
            payload.room_id,
            payload.end_on_host_leave,
            payload.admitted_can_admit,
            payload.waiting_room_enabled,
            payload.allow_guests
        );
    }
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
            target_user_id: "alice@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_ADMITTED.into()
        );
        assert_eq!(
            inner.target_user_id,
            "alice@example.com".as_bytes().to_vec()
        );
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
            target_user_id: "bob@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_REJECTED.into()
        );
        assert_eq!(inner.target_user_id, "bob@example.com".as_bytes().to_vec());
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
    fn test_build_meeting_settings_updated_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::MEETING_SETTINGS_UPDATED.into(),
            room_id: "test-room".to_string(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::MEETING_SETTINGS_UPDATED.into()
        );
        assert_eq!(inner.room_id, "test-room");
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
        publish_meeting_settings_updated(None, "room").await;
    }
}
