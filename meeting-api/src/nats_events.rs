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

/// NATS subject consumed by `meeting-api` to write `state='idle'` to the
/// `meetings` table when `actix-api` detects that a room became empty (the last
/// present participant disconnected/left) for a meeting that did NOT end.
/// Defines the presence-driven everyone-left → idle transition.
///
/// `actix-api` fires this ONCE per room-becomes-empty (when its in-memory
/// per-room member count reaches zero), never per-disconnect, so the consumer
/// is not subjected to an O(n) storm during a mass-disconnect. The consumer's
/// `db_meetings::set_idle` guards on `state='active'`, so it is a no-op on an
/// already-ended (terminal) or already-idle meeting — making the end-vs-idle
/// race safe in either ordering.
///
/// The corresponding publisher lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_BECAME_EMPTY_SUBJECT`).
pub const MEETING_BECAME_EMPTY_SUBJECT: &str = "internal.meeting_became_empty";

/// NATS subject for fanning out per-participant host-flag changes to every
/// `actix-api` chat_server instance. The chat_server caches each member's
/// `is_host` at JoinRoom time from the JWT, so without this fanout a
/// mid-meeting transfer-host would not take effect in the in-memory presence
/// map until the affected user reconnected — and the host-leave→end continuity
/// check reads that cached flag.
///
/// JSON over an internal subject, mirroring
/// [`MEETING_SETTINGS_UPDATE_SUBJECT`]. The corresponding consumer lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_HOST_CHANGE_SUBJECT`).
pub const MEETING_HOST_CHANGE_SUBJECT: &str = "internal.meeting_host_changed";

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

/// Payload consumed on [`MEETING_BECAME_EMPTY_SUBJECT`].
///
/// Sent by chat_server when the last present participant leaves a room whose
/// meeting did not end. The `meeting-api` consumer looks up the meeting by
/// `room_id` and transitions its DB row to `state='idle'` via
/// `db_meetings::set_idle`. Mirrors [`MeetingEndedByHostPayload`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingBecameEmptyPayload {
    pub room_id: String,
}

/// Payload published on [`MEETING_HOST_CHANGE_SUBJECT`].
///
/// Carries a single per-user host-flag delta so chat_server can update the
/// `is_host` field on every `RoomMemberInfo` (across all of that user's
/// sessions) in the room without a DB round-trip. `is_host` is the
/// post-change authoritative value (`true` on grant/transfer-target, `false`
/// on revoke/transfer-source).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingHostChangePayload {
    pub room_id: String,
    pub user_id: String,
    pub is_host: bool,
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

/// Publish `MEETING_ENDED` to every client in a room so they show the
/// meeting-ended overlay and disconnect. Used by the REST `/leave` path when
/// the meeting OWNER (creator, while still host) leaves with
/// `end_on_host_leave=true`: that path ends the meeting immediately when the
/// host leaves, so the transport-layer host-leave broadcast (which only
/// fires when no host remains) would not notify clients. Mirrors the packet the
/// transport path builds via `SessionManager::build_meeting_ended_packet`.
pub async fn publish_meeting_ended(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    message: &str,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::MEETING_ENDED.into(),
        room_id: room_id.to_string(),
        message: message.to_string(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!("Published MEETING_ENDED for room {room_id}");
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
///
/// When `session_id` is `Some(sid)`, the broadcast packet carries that session
/// identifier so the chat_server consumer and peer clients scope the rename to
/// the originating tab only — not every session sharing the renaming user's
/// `user_id` (HCL issue #828 follow-up). When `None`, the proto field is left
/// at its default (`0`), which both peers and the chat_server handler interpret
/// as the legacy "rename every session of this user" path.
pub async fn publish_participant_display_name_changed(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    new_display_name: &str,
    session_id: Option<u64>,
) {
    let Some(nats) = nats else { return };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        display_name: new_display_name.as_bytes().to_vec(),
        session_id: session_id.unwrap_or(0),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    publish(nats, room_system_subject(room_id), bytes).await;
    tracing::debug!(
        "Published PARTICIPANT_DISPLAY_NAME_CHANGED for {target_user_id} in room {room_id} \
         (session_id={}): {}",
        session_id.unwrap_or(0),
        new_display_name
    );
}

/// Publish `HOST_MUTE_PARTICIPANT` for one participant — or, with an empty
/// `target_user_id`, for every participant in the room (mute-all).
///
/// `host_user_id` is the authenticated issuing host's `user_id`. It is carried
/// on the broadcast `MeetingPacket` via `creator_id` (UTF-8 bytes) so clients
/// can exclude the host's own tile from a force-off on the mute-all path. On
/// the targeted path it is harmless extra context (clients only consult it for
/// the broadcast variant where `target_user_id` is empty). When `creator_id`
/// is empty the frontend falls back to the slower heartbeat-driven path.
pub async fn publish_host_mute(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    host_user_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let packet = MeetingPacket {
        event_type: MeetingEventType::HOST_MUTE_PARTICIPANT.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        creator_id: host_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    nats.publish(room_system_subject(room_id), bytes.into())
        .await?;
    tracing::debug!(
        "Published HOST_MUTE_PARTICIPANT for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\""
    );
    Ok(())
}

/// Publish `HOST_DISABLE_VIDEO` for one participant — or, with an empty
/// `target_user_id`, for every participant in the room (disable-video-all).
///
/// `host_user_id` is the authenticated issuing host's `user_id`. It is carried
/// on the broadcast `MeetingPacket` via `creator_id` (UTF-8 bytes) so clients
/// can exclude the host's own tile from a force-off on the disable-video-all
/// path. On the targeted path it is harmless extra context (clients only
/// consult it for the broadcast variant where `target_user_id` is empty). When
/// `creator_id` is empty the frontend falls back to the slower
/// heartbeat-driven path.
pub async fn publish_host_disable_video(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    host_user_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let packet = MeetingPacket {
        event_type: MeetingEventType::HOST_DISABLE_VIDEO.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        creator_id: host_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    nats.publish(room_system_subject(room_id), bytes.into())
        .await?;
    tracing::debug!("Published HOST_DISABLE_VIDEO for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\"");
    Ok(())
}

/// Publish `PARTICIPANT_KICKED` to tell one participant they have been removed.
pub async fn publish_host_kick(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let packet = MeetingPacket {
        event_type: MeetingEventType::PARTICIPANT_KICKED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    nats.publish(room_system_subject(room_id), bytes.into())
        .await?;
    tracing::debug!("Published PARTICIPANT_KICKED for room {room_id} target=\"{target_user_id}\"");
    Ok(())
}

/// Publish `HOST_GRANTED` to tell every client a participant was promoted to
/// host (the promotion half of a transfer-host).
pub async fn publish_host_granted(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    host_user_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let packet = MeetingPacket {
        event_type: MeetingEventType::HOST_GRANTED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        creator_id: host_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    nats.publish(room_system_subject(room_id), bytes.into())
        .await?;
    tracing::debug!(
        "Published HOST_GRANTED for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\""
    );
    Ok(())
}

/// Publish `HOST_REVOKED` to tell every client a participant's host privileges
/// were removed (demote, or the demotion half of a transfer).
pub async fn publish_host_revoked(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    target_user_id: &str,
    host_user_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let packet = MeetingPacket {
        event_type: MeetingEventType::HOST_REVOKED.into(),
        room_id: room_id.to_string(),
        target_user_id: target_user_id.as_bytes().to_vec(),
        creator_id: host_user_id.as_bytes().to_vec(),
        ..Default::default()
    };
    let bytes = build_meeting_wrapper(&packet);
    nats.publish(room_system_subject(room_id), bytes.into())
        .await?;
    tracing::debug!(
        "Published HOST_REVOKED for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\""
    );
    Ok(())
}

/// Publish a server-internal [`MEETING_HOST_CHANGE_SUBJECT`] event so every
/// `actix-api` chat_server instance updates the cached `is_host` flag for the
/// affected user across all of their sessions in the room. No-op when NATS is
/// not configured.
///
/// Distinct from [`publish_host_granted`] / [`publish_host_revoked`]: those
/// tell **clients** about the change; this one tells **servers** to refresh
/// their in-memory presence map so the host-leave continuity check stays
/// correct without a DB lookup.
pub async fn publish_internal_host_change(
    nats: Option<&async_nats::Client>,
    payload: &MeetingHostChangePayload,
) {
    let Some(nats) = nats else { return };
    let bytes = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                "Failed to serialize MeetingHostChangePayload for {}: {e}",
                payload.room_id
            );
            return;
        }
    };
    if let Err(e) = nats
        .publish(MEETING_HOST_CHANGE_SUBJECT, bytes.into())
        .await
    {
        tracing::error!(
            "Failed to publish {} for {} (user={}): {e}",
            MEETING_HOST_CHANGE_SUBJECT,
            payload.room_id,
            payload.user_id
        );
    } else {
        tracing::debug!(
            "Published {} for room {} (user={}, is_host={})",
            MEETING_HOST_CHANGE_SUBJECT,
            payload.room_id,
            payload.user_id,
            payload.is_host
        );
    }
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
    fn test_build_host_mute_packet_targeted() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_MUTE_PARTICIPANT.into(),
            room_id: "test-room".to_string(),
            target_user_id: "carol@example.com".as_bytes().to_vec(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::HOST_MUTE_PARTICIPANT.into()
        );
        assert_eq!(
            inner.target_user_id,
            "carol@example.com".as_bytes().to_vec()
        );
        // The issuing host's user_id rides on `creator_id` so clients can
        // exclude the host tile from a force-off (HCL issue #1036). Populated
        // on the targeted path too for API uniformity.
        assert_eq!(inner.creator_id, "host@example.com".as_bytes().to_vec());
        assert_eq!(inner.room_id, "test-room");
    }

    #[test]
    fn test_build_host_mute_packet_all_participants() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_MUTE_PARTICIPANT.into(),
            room_id: "test-room".to_string(),
            target_user_id: Vec::new(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::HOST_MUTE_PARTICIPANT.into()
        );
        assert!(
            inner.target_user_id.is_empty(),
            "mute-all uses empty target_user_id as the broadcast marker"
        );
        // On the mute-all broadcast, `creator_id` carrying the host's user_id
        // is what lets every client exclude the host's own tile from the
        // force-off and take the fast path (HCL issue #1036).
        assert_eq!(
            inner.creator_id,
            "host@example.com".as_bytes().to_vec(),
            "mute-all must carry the issuing host's user_id in creator_id"
        );
    }

    #[test]
    fn test_build_host_disable_video_packet_targeted() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_DISABLE_VIDEO.into(),
            room_id: "test-room".to_string(),
            target_user_id: "dan@example.com".as_bytes().to_vec(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::HOST_DISABLE_VIDEO.into()
        );
        assert_eq!(inner.target_user_id, "dan@example.com".as_bytes().to_vec());
        // The issuing host's user_id rides on `creator_id` (HCL issue #1036).
        assert_eq!(inner.creator_id, "host@example.com".as_bytes().to_vec());
        assert_eq!(inner.room_id, "test-room");
    }

    #[test]
    fn test_build_host_disable_video_packet_all_participants() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_DISABLE_VIDEO.into(),
            room_id: "test-room".to_string(),
            target_user_id: Vec::new(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::HOST_DISABLE_VIDEO.into()
        );
        assert!(
            inner.target_user_id.is_empty(),
            "disable-video-all uses empty target_user_id as the broadcast marker"
        );
        // On the disable-video-all broadcast, `creator_id` carrying the host's
        // user_id is what lets every client exclude the host's own tile from
        // the force-off and take the fast path (HCL issue #1036).
        assert_eq!(
            inner.creator_id,
            "host@example.com".as_bytes().to_vec(),
            "disable-video-all must carry the issuing host's user_id in creator_id"
        );
    }

    #[test]
    fn test_build_participant_display_name_changed_packet_with_session_id() {
        // When `meeting-api` is told a rename came from a specific session
        // (HCL issue #828 follow-up), the broadcast packet MUST carry the
        // same `session_id` so chat_server and downstream peers can scope
        // the rename to a single tab instead of every session of the user.
        let packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into(),
            room_id: "test-room".to_string(),
            target_user_id: "tony@example.com".as_bytes().to_vec(),
            display_name: "Antonio (tab A)".as_bytes().to_vec(),
            session_id: 4242,
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.event_type,
            MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into()
        );
        assert_eq!(inner.target_user_id, "tony@example.com".as_bytes().to_vec());
        assert_eq!(inner.display_name, "Antonio (tab A)".as_bytes().to_vec());
        assert_eq!(
            inner.session_id, 4242,
            "session_id must be preserved on the wire so peers can scope the rename to one tab"
        );
        assert_eq!(inner.room_id, "test-room");
    }

    #[test]
    fn test_build_participant_display_name_changed_packet_legacy_no_session_id() {
        // Legacy callers don't supply `session_id`. The proto-3 default `0`
        // is the agreed sentinel for the user-id-wide rename path and MUST
        // be preserved verbatim — both chat_server and peer clients depend
        // on this exact value to fall back to the pre-#828 behaviour.
        let packet = MeetingPacket {
            event_type: MeetingEventType::PARTICIPANT_DISPLAY_NAME_CHANGED.into(),
            room_id: "test-room".to_string(),
            target_user_id: "legacy@example.com".as_bytes().to_vec(),
            display_name: "Legacy".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(
            inner.session_id, 0,
            "legacy callers must produce session_id=0 so consumers fall back to user-id-wide rename"
        );
    }

    #[test]
    fn test_build_host_granted_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_GRANTED.into(),
            room_id: "test-room".to_string(),
            target_user_id: "eve@example.com".as_bytes().to_vec(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::HOST_GRANTED.into());
        assert_eq!(inner.target_user_id, "eve@example.com".as_bytes().to_vec());
        // Issuing host rides on creator_id, mirroring the mute/disable events.
        assert_eq!(inner.creator_id, "host@example.com".as_bytes().to_vec());
        assert_eq!(inner.room_id, "test-room");
    }

    #[test]
    fn test_build_host_revoked_packet() {
        let packet = MeetingPacket {
            event_type: MeetingEventType::HOST_REVOKED.into(),
            room_id: "test-room".to_string(),
            target_user_id: "frank@example.com".as_bytes().to_vec(),
            creator_id: "host@example.com".as_bytes().to_vec(),
            ..Default::default()
        };
        let bytes = build_meeting_wrapper(&packet);
        let wrapper = PacketWrapper::parse_from_bytes(&bytes).unwrap();
        let inner = MeetingPacket::parse_from_bytes(&wrapper.data).unwrap();
        assert_eq!(inner.event_type, MeetingEventType::HOST_REVOKED.into());
        assert_eq!(
            inner.target_user_id,
            "frank@example.com".as_bytes().to_vec()
        );
        assert_eq!(inner.creator_id, "host@example.com".as_bytes().to_vec());
        assert_eq!(inner.room_id, "test-room");
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
        publish_meeting_ended(None, "room", "ended").await;
        publish_participant_admitted(None, "room", "user@test.com").await;
        publish_participant_rejected(None, "room", "user@test.com").await;
        publish_waiting_room_updated(None, "room").await;
        publish_meeting_settings_updated(None, "room").await;
        let _ = publish_host_mute(None, "room", "user@test.com", "host@test.com").await;
        let _ = publish_host_mute(None, "room", "", "host@test.com").await;
        let _ = publish_host_disable_video(None, "room", "user@test.com", "host@test.com").await;
        let _ = publish_host_disable_video(None, "room", "", "host@test.com").await;
        let _ = publish_host_granted(None, "room", "user@test.com", "host@test.com").await;
        let _ = publish_host_revoked(None, "room", "user@test.com", "host@test.com").await;
        publish_internal_host_change(
            None,
            &MeetingHostChangePayload {
                room_id: "room".to_string(),
                user_id: "user@test.com".to_string(),
                is_host: true,
            },
        )
        .await;
    }
}
