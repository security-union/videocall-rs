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

//! Request types for the Meeting Backend REST API.
//!
//! These types define the shape of request bodies and query parameters.
//! They are used by both the server (for deserialization) and clients
//! (for serialization).

use serde::{Deserialize, Serialize};

/// Placeholder substituted for every plaintext password field by the manual
/// [`std::fmt::Debug`] impls in this module.
///
/// Meeting passwords are user secrets. `#[derive(Debug)]` on a struct holding
/// one makes a single `tracing::debug!("{body:?}")` — or any panic message that
/// formats a `Result` containing the request — enough to write the plaintext
/// into a log aggregator forever. Every request type below that carries a
/// password therefore hand-rolls `Debug` and prints this marker instead.
///
/// Field *presence* is still shown (`Some("<redacted>")` vs `None`) because
/// that is what makes the impl useful for debugging, and presence is not a
/// secret: `has_password` is already public on every meeting listing.
const REDACTED: &str = "<redacted>";

/// Request body for `POST /api/v1/meetings`.
///
/// `Debug` is implemented manually to redact [`Self::password`]; see [`REDACTED`].
#[derive(Serialize, Deserialize, Clone)]
pub struct CreateMeetingRequest {
    /// Meeting identifier. Auto-generated (12 chars) if omitted.
    #[serde(default)]
    pub meeting_id: Option<String>,

    /// Pre-registered attendee emails (max 100).
    #[serde(default)]
    pub attendees: Vec<String>,

    /// Meeting password (hashed with Argon2 before storage).
    #[serde(default)]
    pub password: Option<String>,

    /// Whether the waiting room is enabled. Defaults to `true` on the server
    /// when omitted.
    #[serde(default)]
    pub waiting_room_enabled: Option<bool>,

    /// Whether admitted participants can also admit others from the waiting room.
    #[serde(default)]
    pub admitted_can_admit: Option<bool>,

    /// Whether the meeting ends for all when the host leaves. Defaults to `true`
    /// on the server when omitted.
    #[serde(default)]
    pub end_on_host_leave: Option<bool>,

    /// Whether guests (non-authenticated users) are allowed to join. Defaults
    /// to `false` on the server when omitted.
    #[serde(default)]
    pub allow_guests: Option<bool>,

    /// Whether the record button is shown to all admitted participants
    /// (not just the host).  Defaults to `false` on the server when omitted
    /// so the record button is a host-only affordance out of the box.
    #[serde(default)]
    pub recording_allowed_for_all: Option<bool>,

    /// Whether every admitted participant may SEND chat messages (not just the
    /// host/co-hosts).  Defaults to `true` on the server when omitted so normal
    /// meetings are unaffected; a host turns it OFF for all-hands-style meetings
    /// where only hosts should be able to post, and can flip it back ON live.
    #[serde(default)]
    pub chat_allowed_for_all: Option<bool>,
}

impl std::fmt::Debug for CreateMeetingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateMeetingRequest")
            .field("meeting_id", &self.meeting_id)
            .field("attendees", &self.attendees)
            .field("password", &self.password.as_ref().map(|_| REDACTED))
            .field("waiting_room_enabled", &self.waiting_room_enabled)
            .field("admitted_can_admit", &self.admitted_can_admit)
            .field("end_on_host_leave", &self.end_on_host_leave)
            .field("allow_guests", &self.allow_guests)
            .field("recording_allowed_for_all", &self.recording_allowed_for_all)
            .field("chat_allowed_for_all", &self.chat_allowed_for_all)
            .finish()
    }
}

/// Request body for `PATCH /api/v1/meetings/{meeting_id}`.
///
/// `Debug` is implemented manually to redact [`Self::password`]; see [`REDACTED`].
#[derive(Serialize, Deserialize, Clone)]
pub struct UpdateMeetingRequest {
    /// Toggle the waiting room on or off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_room_enabled: Option<bool>,

    /// Toggle whether admitted participants can admit others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_can_admit: Option<bool>,

    /// Toggle whether the meeting ends for all when the host leaves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_on_host_leave: Option<bool>,

    /// Toggle guest access on or off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_guests: Option<bool>,

    /// Toggle whether the record button is shown to all admitted participants.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_allowed_for_all: Option<bool>,

    /// Toggle whether every admitted participant may send chat messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_allowed_for_all: Option<bool>,

    /// Set the meeting password to this plaintext value, Argon2-hashed
    /// server-side. `None` leaves it untouched; `""` is rejected. Mutually
    /// exclusive with [`Self::remove_password`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// Remove the meeting's password. `Some(false)` and `None` both mean
    /// "leave it alone".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_password: Option<bool>,
}

impl std::fmt::Debug for UpdateMeetingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateMeetingRequest")
            .field("waiting_room_enabled", &self.waiting_room_enabled)
            .field("admitted_can_admit", &self.admitted_can_admit)
            .field("end_on_host_leave", &self.end_on_host_leave)
            .field("allow_guests", &self.allow_guests)
            .field("recording_allowed_for_all", &self.recording_allowed_for_all)
            .field("chat_allowed_for_all", &self.chat_allowed_for_all)
            .field("password", &self.password.as_ref().map(|_| REDACTED))
            .field("remove_password", &self.remove_password)
            .finish()
    }
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/join`.
///
/// `Debug` is implemented manually to redact [`Self::password`]; see [`REDACTED`].
#[derive(Serialize, Deserialize, Clone)]
pub struct JoinMeetingRequest {
    /// Display name shown in the meeting UI.
    #[serde(default)]
    pub display_name: Option<String>,

    /// Plaintext meeting password, verified server-side against the meeting's
    /// stored Argon2 hash (issue #1613).
    ///
    /// `Option` so that pre-#1613 callers keep compiling and keep working
    /// against meetings that have no password. It is **required in practice**
    /// whenever the target meeting reports `has_password: true` and the caller
    /// is not the meeting owner — the server rejects such a join with
    /// `403 MEETING_PASSWORD_REQUIRED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for JoinMeetingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinMeetingRequest")
            .field("display_name", &self.display_name)
            .field("password", &self.password.as_ref().map(|_| REDACTED))
            .finish()
    }
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/join-guest`.
///
/// `Debug` is implemented manually to redact [`Self::password`]; see [`REDACTED`].
#[derive(Serialize, Deserialize, Clone)]
pub struct GuestJoinRequest {
    /// Display name shown in the meeting UI. Must be provided by the caller.
    pub display_name: String,
    /// Optional stable guest identifier persisted in the client's sessionStorage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guest_session_id: Option<String>,

    /// Plaintext meeting password, verified server-side against the meeting's
    /// stored Argon2 hash (issue #1613). A guest is never the meeting owner, so
    /// this is required whenever the meeting has a password.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for GuestJoinRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuestJoinRequest")
            .field("display_name", &self.display_name)
            .field("guest_session_id", &self.guest_session_id)
            .field("password", &self.password.as_ref().map(|_| REDACTED))
            .finish()
    }
}

/// Request body for `PUT /api/v1/meetings/{meeting_id}/display-name`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateDisplayNameRequest {
    /// New display name for the participant.
    pub display_name: String,
    /// Optional session_id of the renaming tab. When provided, the server
    /// scopes the rename and its `PARTICIPANT_DISPLAY_NAME_CHANGED` broadcast
    /// to this single session, so sibling tabs of the same authenticated
    /// user keep their own display names (HCL issue #828 follow-up). When
    /// omitted (legacy clients), the server falls back to renaming every
    /// session that shares the caller's `user_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/admit`
/// and `POST /api/v1/meetings/{meeting_id}/reject`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AdmitRequest {
    /// User ID of the participant to admit or reject.
    pub user_id: String,
}

/// Query parameters for `GET /api/v1/meetings`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListMeetingsQuery {
    /// Maximum number of meetings to return (1-100). Defaults to 20.
    #[serde(default = "default_limit")]
    pub limit: i64,

    /// Number of meetings to skip for pagination. Defaults to 0.
    #[serde(default)]
    pub offset: i64,

    /// Search query for meeting ID, state, or host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
}

fn default_limit() -> i64 {
    20
}

impl Default for ListMeetingsQuery {
    fn default() -> Self {
        Self {
            limit: default_limit(),
            offset: 0,
            q: None,
        }
    }
}

/// Query parameters for `GET /api/v1/meetings/joined`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListJoinedMeetingsQuery {
    /// Maximum number of joined meetings to return. Defaults to 5 when omitted.
    /// Server clamps to a maximum of 50; negative values return 400 INVALID_INPUT.
    #[serde(default = "default_joined_limit")]
    pub limit: i64,
}

fn default_joined_limit() -> i64 {
    5
}

impl Default for ListJoinedMeetingsQuery {
    fn default() -> Self {
        Self {
            limit: default_joined_limit(),
        }
    }
}

/// Query parameters for `GET /api/v1/meetings/feed`.
///
/// `limit` defaults to and is clamped at 200. Negative values are rejected
/// with `400 INVALID_INPUT`. The 200-row cap is intentional — datasets larger
/// than that should be reached via the search modal
/// (`GET /api/v1/meetings?q=...`) rather than expanding the home-feed payload.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListFeedQuery {
    /// Maximum number of meetings to return. Defaults to 200 when omitted.
    /// Server clamps to a maximum of 200; negative values return 400 INVALID_INPUT.
    #[serde(default = "default_feed_limit")]
    pub limit: i64,
}

fn default_feed_limit() -> i64 {
    200
}

impl Default for ListFeedQuery {
    fn default() -> Self {
        Self {
            limit: default_feed_limit(),
        }
    }
}

/// Query parameters for `POST /api/v1/meetings/{meeting_id}/mute`
/// and `POST /api/v1/meetings/{meeting_id}/mute-all`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MuteParticipantRequest {
    /// User ID of the participant to ask to mute.
    pub user_id: String,
}

/// Query parameters for `POST /api/v1/meetings/{meeting_id}/disable-video`
/// and `POST /api/v1/meetings/{meeting_id}/disable-video-all`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DisableVideoParticipantRequest {
    /// User ID of the participant to ask to disable video.
    pub user_id: String,
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/kick`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KickParticipantRequest {
    /// User ID of the participant to remove from the meeting.
    pub user_id: String,
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/transfer-host`.
///
/// Atomically promotes the target to host and demotes the issuing host in a
/// single transaction.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TransferHostRequest {
    /// User ID of the admitted participant to transfer host to.
    pub user_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "hunter2-do-not-log-me";

    /// Every request type carrying a plaintext password must keep it out of its
    /// `Debug` output. Replacing any of these hand-written impls with
    /// `#[derive(Debug)]` makes these assertions fail, which is the point:
    /// `Debug` is what a stray `tracing::debug!("{body:?}")` or a panic message
    /// reaches for, and a password in a log line survives long after the
    /// request does.
    #[test]
    fn debug_never_prints_a_plaintext_password() {
        let create = CreateMeetingRequest {
            meeting_id: Some("standup".into()),
            attendees: vec!["a@example.com".into()],
            password: Some(SECRET.into()),
            waiting_room_enabled: None,
            admitted_can_admit: None,
            end_on_host_leave: None,
            allow_guests: None,
            recording_allowed_for_all: None,
            chat_allowed_for_all: None,
        };
        let join = JoinMeetingRequest {
            display_name: Some("Alice".into()),
            password: Some(SECRET.into()),
        };
        let guest = GuestJoinRequest {
            display_name: "Guesty".into(),
            guest_session_id: Some("guest:abc".into()),
            password: Some(SECRET.into()),
        };
        let update = UpdateMeetingRequest {
            waiting_room_enabled: None,
            admitted_can_admit: None,
            end_on_host_leave: None,
            allow_guests: None,
            recording_allowed_for_all: None,
            chat_allowed_for_all: None,
            password: Some(SECRET.into()),
            remove_password: None,
        };

        for rendered in [
            format!("{create:?}"),
            format!("{join:?}"),
            format!("{create:#?}"),
            format!("{join:#?}"),
            format!("{guest:?}"),
            format!("{guest:#?}"),
            format!("{update:?}"),
            format!("{update:#?}"),
        ] {
            assert!(
                !rendered.contains(SECRET),
                "Debug output leaked the plaintext password: {rendered}"
            );
            assert!(
                rendered.contains(REDACTED),
                "Debug output should mark the redacted field: {rendered}"
            );
        }
    }

    /// The non-secret fields must still be visible — a `Debug` impl that hides
    /// everything is useless and would quietly get reverted.
    #[test]
    fn debug_still_shows_the_non_secret_fields() {
        let join = JoinMeetingRequest {
            display_name: Some("Alice".into()),
            password: Some(SECRET.into()),
        };
        let rendered = format!("{join:?}");
        assert!(rendered.contains("Alice"), "{rendered}");
        assert!(rendered.contains("JoinMeetingRequest"), "{rendered}");
    }

    /// An absent password renders as `None`, not as a redaction marker, so
    /// "no password was sent" stays distinguishable from "one was sent".
    #[test]
    fn debug_distinguishes_absent_from_redacted() {
        let none = JoinMeetingRequest {
            display_name: None,
            password: None,
        };
        let rendered = format!("{none:?}");
        assert!(
            !rendered.contains(REDACTED),
            "a None password must not render as redacted: {rendered}"
        );
        assert!(rendered.contains("password: None"), "{rendered}");
    }

    /// `password` must be omitted from the serialized body when absent, so
    /// pre-#1613 clients and post-#1613 clients that have no password to send
    /// put the identical bytes on the wire.
    #[test]
    fn absent_password_is_omitted_from_the_wire() {
        let join = JoinMeetingRequest {
            display_name: Some("Alice".into()),
            password: None,
        };
        let wire = serde_json::to_string(&join).expect("serializing a join request");
        assert_eq!(wire, r#"{"display_name":"Alice"}"#);

        let guest = GuestJoinRequest {
            display_name: "Guesty".into(),
            guest_session_id: None,
            password: None,
        };
        let wire = serde_json::to_string(&guest).expect("serializing a guest join request");
        assert_eq!(wire, r#"{"display_name":"Guesty"}"#);
    }

    /// A pre-#1613 body (no `password` key at all) must still deserialize —
    /// old clients keep working against meetings that have no password.
    #[test]
    fn legacy_bodies_without_a_password_field_still_deserialize() {
        let join: JoinMeetingRequest =
            serde_json::from_str(r#"{"display_name":"Alice"}"#).expect("legacy join body");
        assert_eq!(join.display_name.as_deref(), Some("Alice"));
        assert!(join.password.is_none());

        let join: JoinMeetingRequest = serde_json::from_str("{}").expect("empty join body");
        assert!(join.password.is_none());

        let guest: GuestJoinRequest =
            serde_json::from_str(r#"{"display_name":"Guesty"}"#).expect("legacy guest join body");
        assert!(guest.password.is_none());
    }

    /// And a body that does carry one round-trips it verbatim — no trimming,
    /// no case folding, no length cap on the way in.
    #[test]
    fn supplied_password_round_trips_verbatim() {
        let raw = r#"{"display_name":"Alice","password":"  Mixed CASE ☂ "}"#;
        let join: JoinMeetingRequest = serde_json::from_str(raw).expect("join body with password");
        assert_eq!(join.password.as_deref(), Some("  Mixed CASE ☂ "));
    }

    #[test]
    fn update_body_carries_set_and_clear() {
        let set: UpdateMeetingRequest =
            serde_json::from_str(r#"{"password":"s3cret"}"#).expect("update body setting one");
        assert_eq!(set.password.as_deref(), Some("s3cret"));
        assert_eq!(set.remove_password, None);

        let clear: UpdateMeetingRequest =
            serde_json::from_str(r#"{"remove_password":true}"#).expect("update body clearing one");
        assert_eq!(clear.password, None);
        assert_eq!(clear.remove_password, Some(true));
    }

    #[test]
    fn update_body_omits_the_password_keys_when_unused() {
        let toggle_only = UpdateMeetingRequest {
            waiting_room_enabled: Some(false),
            admitted_can_admit: None,
            end_on_host_leave: None,
            allow_guests: None,
            recording_allowed_for_all: None,
            chat_allowed_for_all: None,
            password: None,
            remove_password: None,
        };
        let wire = serde_json::to_string(&toggle_only).expect("serializing an update request");
        assert_eq!(wire, r#"{"waiting_room_enabled":false}"#);
    }

    #[test]
    fn legacy_update_bodies_still_deserialize() {
        let legacy: UpdateMeetingRequest =
            serde_json::from_str(r#"{"allow_guests":true}"#).expect("legacy update body");
        assert_eq!(legacy.allow_guests, Some(true));
        assert!(legacy.password.is_none());
        assert!(legacy.remove_password.is_none());
    }
}
