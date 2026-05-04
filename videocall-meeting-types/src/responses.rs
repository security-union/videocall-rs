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
    /// Unix timestamp in milliseconds when the meeting was created.
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

/// Response payload for `GET /api/v1/meetings/joined`.
///
/// Returns the meetings the authenticated user has previously been admitted into,
/// ordered by their most recent admission time (descending). Includes both
/// meetings the user owns and meetings they joined as a non-owner.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListJoinedMeetingsResponse {
    /// Meetings the user has joined, ordered by `last_joined_at` descending.
    pub meetings: Vec<JoinedMeetingSummary>,
    /// Total count of joined meetings returned (equal to `meetings.len()`).
    /// Capped by the request's `limit`; not a true unbounded count.
    pub total: i64,
}

/// Single meeting entry inside [`ListJoinedMeetingsResponse`].
///
/// All timestamps are Unix epoch milliseconds. The `last_joined_at` field is
/// the timestamp used for ordering — `admitted_at` when present, falling back
/// to `joined_at` for legacy rows where `admitted_at` was never refreshed.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct JoinedMeetingSummary {
    pub meeting_id: String,
    /// Meeting state: `"active"`, `"idle"`, or `"ended"`.
    pub state: String,
    /// Unix timestamp in milliseconds when the meeting started.
    pub started_at: i64,
    /// Unix timestamp in milliseconds when the meeting ended, or `null` if
    /// still active/idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub participant_count: i64,
    pub waiting_count: i64,
    pub has_password: bool,
    /// `true` when the authenticated user is the meeting owner (creator).
    pub is_owner: bool,
    /// Unix timestamp in milliseconds when the meeting was first created.
    /// Immutable — set at INSERT and never updated.
    pub created_at: i64,
    /// Unix timestamp in milliseconds — the timestamp used for ordering.
    /// Computed as `COALESCE(admitted_at, joined_at)` so re-admissions float
    /// to the top while legacy rows that were never re-admitted still sort
    /// by their original join time.
    pub last_joined_at: i64,
}

/// Response payload for `GET /api/v1/meetings/feed`.
///
/// Returns the union of meetings the authenticated user owns or has been
/// admitted into, deduplicated to one row per meeting and ordered by
/// `last_active_at` descending. Powers the home page meeting list.
///
/// ## Why one endpoint instead of two
///
/// Earlier the home page called both `GET /api/v1/meetings` (owned + joined,
/// but missing `is_owner` on each row) and `GET /api/v1/meetings/joined`
/// (which carried `is_owner` correctly). The frontend assumed everything in
/// the first list was owned and rendered the Owner pill plus edit/delete
/// affordances unconditionally. This endpoint fixes that by returning a
/// single deduplicated feed where every row has an authoritative,
/// server-computed `is_owner` flag.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListFeedResponse {
    /// Meetings the user owns OR has been admitted into, ordered by
    /// `last_active_at` descending. Capped at 200 rows — see
    /// `MeetingFeedSummary::last_active_at` for ordering semantics.
    pub meetings: Vec<MeetingFeedSummary>,
}

/// Single meeting entry inside [`ListFeedResponse`].
///
/// All timestamps are Unix epoch milliseconds. `is_owner` is set by the
/// server: it compares `creator_id` against the authenticated user's id and
/// is the only safe source of truth for ownership-gated UI affordances such
/// as the Owner pill, edit, delete, and end-meeting controls.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MeetingFeedSummary {
    pub meeting_id: String,
    /// Meeting state: `"active"`, `"idle"`, or `"ended"`.
    pub state: String,
    /// Unix timestamp in milliseconds — the timestamp used for ordering.
    /// Computed server-side as
    /// `COALESCE(p.admitted_at_max, m.started_at, m.created_at)` so:
    /// - For meetings the user has been admitted into, this is the most
    ///   recent admission time (so re-admissions float to the top).
    /// - For owned-but-never-joined meetings this falls back to
    ///   `started_at` (which the meeting-api refreshes on every
    ///   `idle/ended -> active` transition).
    /// - For idle meetings that have never been activated this falls back
    ///   to `created_at`.
    pub last_active_at: i64,
    /// Unix timestamp in milliseconds when the meeting was first created.
    /// Immutable — set at INSERT and never updated.
    pub created_at: i64,
    /// Unix timestamp in milliseconds when the meeting most recently
    /// transitioned to `active`. `None` only when the meeting has never been
    /// activated (still in `idle`); otherwise refreshed on each
    /// `idle/ended -> active` transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    /// Unix timestamp in milliseconds when the meeting ended, or `None` if
    /// still active/idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    /// The meeting creator's `user_id` (display only). Use `is_owner` for
    /// any ownership-based authorization decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `true` when `creator_id == authenticated_user_id`.
    ///
    /// **Server-computed.** This is the authoritative trust signal the UI
    /// must use to decide whether to render owner-only affordances
    /// (Owner pill, edit, delete, end-meeting). Do not infer ownership from
    /// any other field.
    pub is_owner: bool,
    pub participant_count: i64,
    pub waiting_count: i64,
    pub has_password: bool,
    pub allow_guests: bool,
    pub waiting_room_enabled: bool,
    pub admitted_can_admit: bool,
    pub end_on_host_leave: bool,
}

/// Single meeting entry inside [`ListMeetingsResponse`].
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct MeetingSummary {
    pub meeting_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub state: String,
    pub has_password: bool,
    /// Unix timestamp in milliseconds when the meeting was created.
    pub created_at: i64,
    pub participant_count: i64,
    /// Unix timestamp in milliseconds when the meeting most recently
    /// transitioned to `active`. Refreshed on every `idle/ended -> active`
    /// transition; equal to `created_at` for meetings that were activated
    /// once on creation and never re-activated.
    pub started_at: i64,
    /// Unix timestamp in milliseconds when the meeting ended, or `null` if
    /// still active/idle.
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
    #[serde(default = "default_true")]
    pub waiting_room_enabled: bool,
    /// Meeting-level: whether admitted participants can also admit others.
    #[serde(default)]
    pub admitted_can_admit: bool,
    /// Meeting-level: whether the meeting ends for all when the host leaves.
    #[serde(default = "default_true")]
    pub end_on_host_leave: bool,
    /// Meeting-level: the host's display name. Present in join/status responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    /// Meeting-level: the host's user_id (i.e. `creator_id`). Present in join/status responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_user_id: Option<String>,
    /// Meeting-level: whether guests (unauthenticated users) are allowed to join.
    #[serde(default)]
    pub allow_guests: bool,
}

/// Returns `true`; used as the serde `default` for meeting-policy booleans
/// that are true when absent from older API responses.
fn default_true() -> bool {
    true
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
