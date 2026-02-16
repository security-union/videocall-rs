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

//! Response types for the Meeting Backend REST API.
//!
//! Every endpoint returns an [`APIResponse<T>`] envelope:
//! - On success: `{ "success": true,  "result": <T> }`
//! - On failure: `{ "success": false, "result": <APIError> }`

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Generic envelope
// ---------------------------------------------------------------------------

/// Top-level API response envelope.
///
/// All Meeting Backend endpoints wrap their payload in this structure so that
/// clients always see a consistent `{ "success", "result" }` shape.
///
/// # Success example
///
/// ```json
/// { "success": true, "result": { "meeting_id": "standup-2024", ... } }
/// ```
///
/// # Error example
///
/// ```json
/// { "success": false, "result": { "code": "MEETING_NOT_FOUND", "message": "..." } }
/// ```
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct APIResponse<A: Serialize> {
    pub success: bool,
    pub result: A,
}

impl<A: Serialize> APIResponse<A> {
    /// Wrap a successful result.
    pub fn ok(result: A) -> Self {
        Self {
            success: true,
            result,
        }
    }
}

impl APIResponse<crate::error::APIError> {
    /// Wrap an error result.
    pub fn error(err: crate::error::APIError) -> Self {
        Self {
            success: false,
            result: err,
        }
    }
}

// ---------------------------------------------------------------------------
// Endpoint-specific response payloads
// ---------------------------------------------------------------------------

/// Response payload for `POST /api/v1/meetings` (201 Created).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateMeetingResponse {
    pub meeting_id: String,
    pub host: String,
    /// Unix timestamp in seconds when the meeting was created.
    pub created_at: i64,
    pub state: String,
    pub attendees: Vec<String>,
    pub has_password: bool,
}

/// Response payload for `GET /api/v1/meetings/{meeting_id}`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeetingInfoResponse {
    pub meeting_id: String,
    pub state: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    pub has_password: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub your_status: Option<ParticipantStatusResponse>,
}

/// Response payload for `GET /api/v1/meetings`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListMeetingsResponse {
    pub meetings: Vec<MeetingSummary>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

/// Single meeting entry inside [`ListMeetingsResponse`].
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeetingSummary {
    pub meeting_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub state: String,
    pub has_password: bool,
    /// Unix timestamp in seconds when the meeting was created.
    pub created_at: i64,
    pub participant_count: i64,
    /// Unix timestamp in seconds when the meeting started.
    /// Same as `created_at` for meetings that were activated immediately.
    pub started_at: i64,
    /// Unix timestamp in seconds when the meeting ended, or `null` if still active/idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// Number of participants currently in the waiting room.
    pub waiting_count: i64,
}

/// Participant status returned by join, status, admit, reject, and leave endpoints.
///
/// This is the canonical shape for any per-participant response. Fields that
/// are not applicable for a given status are set to `null`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ParticipantStatusResponse {
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: String,
    pub is_host: bool,
    /// Unix timestamp in seconds when the participant joined/entered the waiting room.
    pub joined_at: i64,
    /// Unix timestamp in seconds when the participant was admitted, or `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_at: Option<i64>,
    /// Signed JWT room access token. Present only when `status` is `"admitted"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_token: Option<String>,
}

/// Response payload for `GET /api/v1/meetings/{meeting_id}/waiting`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WaitingRoomResponse {
    pub meeting_id: String,
    pub waiting: Vec<ParticipantStatusResponse>,
}

/// Response payload for `POST /api/v1/meetings/{meeting_id}/admit-all`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AdmitAllResponse {
    pub admitted_count: usize,
    pub admitted: Vec<ParticipantStatusResponse>,
}

/// Response payload for `DELETE /api/v1/meetings/{meeting_id}`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeleteMeetingResponse {
    pub message: String,
}

/// Response payload for `GET /profile`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProfileResponse {
    pub email: String,
    pub name: String,
}
