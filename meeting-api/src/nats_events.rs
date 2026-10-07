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
use videocall_types::protos::meeting_packet::{MeetingPacket, RecordingEntry, RecordingState};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::validation::is_valid_meeting_id;
use videocall_types::SYSTEM_USER_ID;

/// NATS subject carrying per-meeting policy flag changes. Only relays
/// predating #2702 read it, to refresh the end-on-host-leave decision they
/// made themselves.
pub const MEETING_SETTINGS_UPDATE_SUBJECT: &str = "internal.meeting_settings_updated";

/// Subject on which a relay predating #2702 reports a meeting it ended itself;
/// consumed (`state='ended'`) so the DB agrees if such a relay is rolled back to.
pub const MEETING_ENDED_BY_HOST_SUBJECT: &str = "internal.meeting_ended_by_host";

/// NATS subject consumed by `meeting-api` to write `state='idle'` to the
/// `meetings` table when `actix-api` detects that a room became empty (the last
/// present participant disconnected/left) for a meeting that did NOT end.
/// Defines the presence-driven everyone-left → idle transition.
///
/// Each relay binary fires this ONCE when its own in-memory copy of the room
/// empties, never per-disconnect. The consumer's `db_meetings::set_idle` only
/// idles an `active` meeting with nobody present in the DB, so another
/// binary's participants keep it active and an `ended` meeting stays ended.
///
/// The corresponding publisher lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_BECAME_EMPTY_SUBJECT`).
pub const MEETING_BECAME_EMPTY_SUBJECT: &str = "internal.meeting_became_empty";

/// NATS subject on which each relay reports, in order, every participant
/// session it starts or stops counting as present (issue #2702). Consumed by
/// [`crate::nats_consumers::apply_participant_presence`]: the DB roster, the
/// idle transition, end-on-host-leave and promote-on-connect all follow it.
/// Relays also publish the pre-#2702 `internal.participant_left` /
/// `internal.participant_present` subjects, which this service ignores.
pub const PARTICIPANT_PRESENCE_SUBJECT: &str = "internal.participant_presence";

/// NATS subject for fanning out per-participant host-flag changes to every
/// `actix-api` chat_server instance. The chat_server caches each member's
/// `is_host` at JoinRoom time from the JWT, so without this fanout a
/// mid-meeting transfer-host would not take effect in the in-memory presence
/// map until the affected user reconnected — and the relay's host-only packet
/// gate reads that cached flag.
///
/// JSON over an internal subject, mirroring
/// [`MEETING_SETTINGS_UPDATE_SUBJECT`]. The corresponding consumer lives in
/// `actix-api/src/actors/chat_server.rs` (search for
/// `MEETING_HOST_CHANGE_SUBJECT`).
pub const MEETING_HOST_CHANGE_SUBJECT: &str = "internal.meeting_host_changed";

/// Payload published on [`MEETING_SETTINGS_UPDATE_SUBJECT`]: the full set of
/// post-update policy flags, never a partial delta.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MeetingSettingsUpdatePayload {
    pub room_id: String,
    pub end_on_host_leave: bool,
    pub admitted_can_admit: bool,
    pub waiting_room_enabled: bool,
    pub allow_guests: bool,
    #[serde(default)]
    pub recording_allowed_for_all: bool,
}

/// Payload consumed on [`MEETING_ENDED_BY_HOST_SUBJECT`].
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

/// Payload consumed on [`PARTICIPANT_PRESENCE_SUBJECT`]: relay session
/// `session_id` of `user_id` in `room_id` became present, or left.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ParticipantPresencePayload {
    pub room_id: String,
    pub user_id: String,
    pub session_id: u64,
    pub present: bool,
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

/// NATS subject for system messages in a room, or `None` (with a warning) when
/// `room_id` is not a valid meeting ID.
fn room_system_subject(room_id: &str) -> Option<String> {
    if is_valid_meeting_id(room_id) {
        Some(format!("room.{room_id}.system"))
    } else {
        tracing::warn!("Not publishing room system event: invalid room id {room_id:?}");
        None
    }
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
    tracing::debug!("Published MEETING_ACTIVATED for room {room_id}");
}

/// `MEETING_ENDED` message: the last present host left, or the owner
/// explicitly ended the meeting from settings. Both read the same to a
/// participant still on the call, so they share one message.
pub const HOST_LEFT_MESSAGE: &str = "The host has ended the meeting";

/// Publish `MEETING_ENDED` to every client in a room so they show the
/// meeting-ended overlay and disconnect. Used whenever meeting-api ends a
/// meeting a client might still be connected to: the last present host
/// leaving with `end_on_host_leave=true` (REST `/leave`, or a transport
/// departure reported on [`PARTICIPANT_PRESENCE_SUBJECT`]), and an explicit
/// owner `POST .../end`. Same packet as
/// `SessionManager::build_meeting_ended_packet` in the relay.
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
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
    let subject = room_system_subject(room_id).ok_or("room id is not a valid meeting ID")?;
    nats.publish(subject, bytes.into()).await?;
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
    let subject = room_system_subject(room_id).ok_or("room id is not a valid meeting ID")?;
    nats.publish(subject, bytes.into()).await?;
    tracing::debug!("Published HOST_DISABLE_VIDEO for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\"");
    Ok(())
}

/// `RECORDING_STATE` for `snapshot`; carries no user id or secret.
pub fn recording_state_packet(
    room_id: &str,
    snapshot: &crate::db::recordings::Snapshot,
) -> Vec<u8> {
    let state = RecordingState {
        version: snapshot.version.max(0) as u64,
        entries: snapshot
            .entries
            .iter()
            .map(|(recording_id, revoked)| RecordingEntry {
                recording_id: recording_id.as_bytes().to_vec(),
                revoked: *revoked,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    build_meeting_wrapper(&MeetingPacket {
        event_type: MeetingEventType::RECORDING_STATE.into(),
        room_id: room_id.to_string(),
        recording_epoch: snapshot.epoch,
        recording_state: protobuf::MessageField::some(state),
        ..Default::default()
    })
}

/// Publish `snapshot` on `room_id`'s system subject.
pub async fn publish_recording_state(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    snapshot: &crate::db::recordings::Snapshot,
) {
    let (Some(nats), Some(subject)) = (nats, room_system_subject(room_id)) else {
        return;
    };
    publish(nats, subject, recording_state_packet(room_id, snapshot)).await;
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
    let subject = room_system_subject(room_id).ok_or("room id is not a valid meeting ID")?;
    nats.publish(subject, bytes.into()).await?;
    tracing::debug!("Published PARTICIPANT_KICKED for room {room_id} target=\"{target_user_id}\"");
    Ok(())
}

/// Publish a host-kick revocation on [`PARTICIPANT_KICKED_SUBJECT`] for every
/// relay to enforce (#2934). Server-internal, unlike [`publish_host_kick`].
///
/// [`PARTICIPANT_KICKED_SUBJECT`]: videocall_meeting_types::kick::PARTICIPANT_KICKED_SUBJECT
pub async fn publish_kick_revocation(
    nats: Option<&async_nats::Client>,
    payload: &videocall_meeting_types::kick::ParticipantKickedPayload,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(nats) = nats else { return Ok(()) };
    let bytes = serde_json::to_vec(payload)?;
    nats.publish(
        videocall_meeting_types::kick::PARTICIPANT_KICKED_SUBJECT,
        bytes.into(),
    )
    .await?;
    tracing::debug!(
        "Published kick revocation for room {} user=\"{}\" through={}",
        payload.room_id,
        payload.user_id,
        payload.revoke_iat_through
    );
    Ok(())
}

/// Publish `HOST_GRANTED` to tell every client a participant was promoted to
/// host (a transfer-host target, or a co-host granted or joining).
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
    let subject = room_system_subject(room_id).ok_or("room id is not a valid meeting ID")?;
    nats.publish(subject, bytes.into()).await?;
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
    let subject = room_system_subject(room_id).ok_or("room id is not a valid meeting ID")?;
    nats.publish(subject, bytes.into()).await?;
    tracing::debug!(
        "Published HOST_REVOKED for room {room_id} target=\"{target_user_id}\" host=\"{host_user_id}\""
    );
    Ok(())
}

/// Announce a real host-role change for `user_id`: `HOST_GRANTED` or
/// `HOST_REVOKED` to clients, plus the [`MEETING_HOST_CHANGE_SUBJECT`] fanout
/// to every relay. Publish failures are logged, not returned.
pub async fn announce_host_change(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    user_id: &str,
    changed_by: &str,
    is_host: bool,
) {
    let result = if is_host {
        publish_host_granted(nats, room_id, user_id, changed_by).await
    } else {
        publish_host_revoked(nats, room_id, user_id, changed_by).await
    };
    if let Err(e) = result {
        tracing::error!(
            "NATS publish failed for host change (user={user_id}, is_host={is_host}) in room {room_id}: {e}"
        );
    }
    publish_internal_host_change(
        nats,
        &MeetingHostChangePayload {
            room_id: room_id.to_string(),
            user_id: user_id.to_string(),
            is_host,
        },
    )
    .await;
}

/// [`announce_host_change`] (`is_host = false`) for each host a new meeting
/// instance demoted.
pub async fn announce_demotions(
    nats: Option<&async_nats::Client>,
    room_id: &str,
    user_ids: &[String],
    changed_by: &str,
) {
    for user_id in user_ids {
        announce_host_change(nats, room_id, user_id, changed_by, false).await;
    }
}

/// Publish a server-internal [`MEETING_HOST_CHANGE_SUBJECT`] event so every
/// `actix-api` chat_server instance updates the cached `is_host` flag for the
/// affected user across all of their sessions in the room. No-op when NATS is
/// not configured.
///
/// Distinct from [`publish_host_granted`] / [`publish_host_revoked`]: those
/// tell **clients** about the change; this one tells **servers** to refresh
/// their in-memory presence map.
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
    let Some(subject) = room_system_subject(room_id) else {
        return;
    };
    publish(nats, subject, bytes).await;
    tracing::debug!("Published MEETING_SETTINGS_UPDATED for room {room_id}");
}

/// Publish a server-internal [`MEETING_SETTINGS_UPDATE_SUBJECT`] event with
/// the post-update authoritative flag values. Distinct from
/// [`publish_meeting_settings_updated`], which tells **clients** to re-fetch.
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
             waiting_room_enabled={}, allow_guests={}, recording_allowed_for_all={})",
            MEETING_SETTINGS_UPDATE_SUBJECT,
            payload.room_id,
            payload.end_on_host_leave,
            payload.admitted_can_admit,
            payload.waiting_room_enabled,
            payload.allow_guests,
            payload.recording_allowed_for_all
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
    fn test_room_system_subject_uses_valid_ids_unchanged() {
        assert_eq!(
            room_system_subject("my-room").as_deref(),
            Some("room.my-room.system")
        );
        assert_eq!(
            room_system_subject("victim_room").as_deref(),
            Some("room.victim_room.system")
        );
        assert_eq!(
            room_system_subject("abc123def456").as_deref(),
            Some("room.abc123def456.system")
        );
        assert_eq!(
            room_system_subject("a~b").as_deref(),
            Some("room.a~b.system")
        );
    }

    #[test]
    fn test_room_system_subject_refuses_invalid_ids() {
        for room_id in [
            "victim.room",
            "victim room",
            "victim*room",
            "victim>room",
            "victim\troom",
            "victim\nroom",
            "..",
            "",
            "room.>",
            "caf\u{e9}",
        ] {
            assert_eq!(room_system_subject(room_id), None, "{room_id:?}");
        }
    }

    #[test]
    fn test_participant_presence_payload_json_wire_format() {
        let wire = r#"{"room_id":"test-room","user_id":"ghost@example.com","session_id":18446744073709551615,"present":false}"#;
        let payload: ParticipantPresencePayload = serde_json::from_str(wire).unwrap();
        assert_eq!(
            payload,
            ParticipantPresencePayload {
                room_id: "test-room".to_string(),
                user_id: "ghost@example.com".to_string(),
                session_id: u64::MAX,
                present: false,
            }
        );
        assert_eq!(serde_json::to_string(&payload).unwrap(), wire);
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
