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
    pub waiting_room_enabled: bool,
    pub admitted_can_admit: bool,
    pub end_on_host_leave: bool,
    #[serde(default)]
    pub allow_guests: bool,
}

/// Response payload for `GET /api/v1/meetings/{meeting_id}`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeetingInfoResponse {
    pub meeting_id: String,
    pub state: String,
    pub host: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    /// The host's user_id (i.e. `creator_id`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_user_id: Option<String>,
    pub has_password: bool,
    pub waiting_room_enabled: bool,
    pub admitted_can_admit: bool,
    pub end_on_host_leave: bool,
    pub participant_count: i64,
    pub waiting_count: i64,
    /// Unix timestamp in milliseconds.
    pub started_at: i64,
    /// Unix timestamp in milliseconds, or `null` if still active/idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub your_status: Option<ParticipantStatusResponse>,
    #[serde(default)]
    pub allow_guests: bool,
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
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
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
    pub waiting_room_enabled: bool,
    pub admitted_can_admit: bool,
    pub end_on_host_leave: bool,
    #[serde(default)]
    pub allow_guests: bool,
}

/// Participant status returned by join, status, admit, reject, and leave endpoints.
///
/// This is the canonical shape for any per-participant response. Fields that
/// are not applicable for a given status are set to `null`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ParticipantStatusResponse {
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub status: String,
    pub is_host: bool,
    /// Whether this participant joined as an unauthenticated guest.
    #[serde(default)]
    pub is_guest: bool,
    /// Unix timestamp in seconds when the participant joined/entered the waiting room.
    pub joined_at: i64,
    /// Unix timestamp in seconds when the participant was admitted, or `null`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_at: Option<i64>,
    /// Signed JWT room access token. Present only when `status` is `"admitted"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_token: Option<String>,
    /// Signed JWT observer token. Present when `status` is `"waiting"` or
    /// `"waiting_for_meeting"`, allowing the client to open a read-only
    /// connection for push notifications without polling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observer_token: Option<String>,
    /// Meeting-level: whether the waiting room is enabled. Present in join/status responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_room_enabled: Option<bool>,
    /// Meeting-level: whether admitted participants can also admit others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub admitted_can_admit: Option<bool>,
    /// Meeting-level: whether the meeting ends for all when the host leaves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_on_host_leave: Option<bool>,
    /// Meeting-level: the host's display name. Present in join/status responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    /// Meeting-level: the host's user_id (i.e. `creator_id`). Present in join/status responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_user_id: Option<String>,
    /// Meeting-level: whether guests (unauthenticated users) are allowed to join.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_guests: Option<bool>,
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

/// Response payload for `GET /api/v1/meetings/{meeting_id}/guest-info` (public, no auth).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MeetingGuestInfoResponse {
    pub allow_guests: bool,
}

/// Response payload for `GET /profile`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProfileResponse {
    pub user_id: String,
    pub name: String,
}

/// Response payload for `GET /api/v1/oauth/provider-config`.
///
/// Contains the OAuth provider parameters the browser needs to initiate a
/// PKCE authorization request and, when `token_url` is present, to perform
/// the token exchange directly with the provider.  All fields are public —
/// `client_id`, `auth_url`, and `token_url` are intentionally exposed.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OAuthProviderConfigResponse {
    /// `true` when an OAuth provider is configured on the server.
    pub enabled: bool,
    /// The provider's authorization endpoint URL.
    pub auth_url: String,
    /// The OAuth client ID (public — safe to expose to browsers).
    pub client_id: String,
    /// Space-separated OAuth scopes (e.g. `"openid email profile"`).
    pub scopes: String,
    /// The provider's token endpoint URL.  The browser uses this to exchange
    /// the authorization code directly with the provider (PKCE public-client
    /// flow, no `client_secret` required).
    ///
    /// May be empty when the server configuration does not expose the token
    /// URL (e.g. only `OAUTH_ISSUER` is set and discovery populated
    /// `token_url` internally but the response field is not surfaced).
    pub token_url: String,
    /// OIDC issuer URL (e.g. `https://accounts.google.com`).  The browser
    /// can use this to perform its own OIDC discovery when `token_url` is
    /// empty.
    pub issuer: Option<String>,
}

/// Response payload for `POST /api/v1/oauth/exchange`.
///
/// Returned when the server performs a token exchange on the caller's behalf
/// (server-mediated PKCE path).  Store `access_token` (preferred) or
/// `id_token` in session-scoped storage and present it as
/// `Authorization: Bearer <token>` on subsequent meeting-api requests.
/// No session cookie is issued.
///
/// ## Not used by the dioxus-ui
///
/// The dioxus-ui `/auth/callback` page exchanges tokens **directly with the
/// identity provider** (public-client PKCE) and calls
/// `POST /api/v1/user/register` instead — it never receives this type.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OAuthExchangeResponse {
    /// The user's canonical identifier (email address from the id_token).
    pub user_id: String,
    /// Display name resolved from the provider id_token or UserInfo endpoint.
    pub display_name: String,
    /// The raw id_token JWT from the identity provider.  The client stores
    /// this and presents it as `Authorization: Bearer <id_token>` on all
    /// subsequent meeting-api requests.
    pub id_token: String,
    /// The provider access token.  Returned for completeness; most clients
    /// only need the id_token for meeting-api authentication.
    pub access_token: String,
    /// Where to navigate after successful authentication.  Set by the legacy
    /// server-side PKCE flow; `None` in the new client-side PKCE flow (the UI
    /// reads `return_to` from `sessionStorage["vc_oauth_return_to"]` instead).
    pub return_to: Option<String>,
}
