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

/// Request body for `POST /api/v1/meetings`.
#[derive(Debug, Serialize, Deserialize, Clone)]
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
}

/// Request body for `PATCH /api/v1/meetings/{meeting_id}`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateMeetingRequest {
    /// Toggle the waiting room on or off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_room_enabled: Option<bool>,

    /// Toggle whether admitted participants can admit others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_can_admit: Option<bool>,
}

/// Request body for `POST /api/v1/meetings/{meeting_id}/join`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JoinMeetingRequest {
    /// Display name shown in the meeting UI.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Request body for `PUT /api/v1/meetings/{meeting_id}/display-name`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UpdateDisplayNameRequest {
    /// New display name for the participant.
    pub display_name: String,
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
