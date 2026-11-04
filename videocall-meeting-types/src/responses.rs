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

use crate::announcement::AnnouncementOverride;

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
    #[serde(default)]
    pub recording_allowed_for_all: bool,
    #[serde(default = "default_true")]
    pub chat_allowed_for_all: bool,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_sound_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_sound_override: AnnouncementOverride,
    #[serde(default)]
    pub co_hosts: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CoHostEntry {
    pub user_id: String,
    pub persistent: bool,
    pub is_present_host: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `false` for a present host with no co-host entry (e.g. a transfer target).
    #[serde(default = "default_true")]
    pub designated: bool,
    /// Won't re-grant the host role until the next instance.
    #[serde(default)]
    pub suspended: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListCoHostsResponse {
    pub co_hosts: Vec<CoHostEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CoHostSuggestion {
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Response payload for `GET /api/v1/meetings/{meeting_id}/co-hosts/suggestions`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListCoHostSuggestionsResponse {
    pub suggestions: Vec<CoHostSuggestion>,
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
    #[serde(default)]
    pub recording_allowed_for_all: bool,
    #[serde(default = "default_true")]
    pub chat_allowed_for_all: bool,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_sound_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_sound_override: AnnouncementOverride,
    #[serde(default)]
    pub viewer_is_owner: bool,
    #[serde(default)]
    pub viewer_can_edit_options: bool,
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
    /// The cached host display name captured on first participant join.
    /// `None` until any participant (typically the host) has joined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_display_name: Option<String>,
    /// The host's `user_id` (i.e. `creator_id`). Canonical alias of `host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host_user_id: Option<String>,
    /// Server-computed; the authoritative ownership signal.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_owner: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_co_host: bool,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub participant_count: i64,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub waiting_count: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_password: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_guests: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recording_allowed_for_all: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub chat_allowed_for_all: bool,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_sound_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_sound_override: AnnouncementOverride,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub waiting_room_enabled: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub admitted_can_admit: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub end_on_host_leave: bool,
    /// Unix timestamp in milliseconds of the **authenticated user's** most
    /// recent admission to this meeting — the MAX `admitted_at` across the
    /// requesting user's `meeting_participants` rows.
    ///
    /// Strictly user-scoped: it is THIS user's last-attended time, never any
    /// other participant's. Distinct from `last_active_at`, which falls back
    /// to meeting-level `started_at`/`created_at` and is therefore not
    /// user-scoped. `None` when the user has never been admitted (e.g. they
    /// own the meeting but never joined). Used client-side for the "sort by
    /// last attended" option and the "attended within last 7d / 30d / 6mo"
    /// filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_last_attended_at: Option<i64>,
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
    #[serde(default)]
    pub recording_allowed_for_all: bool,
    #[serde(default = "default_true")]
    pub chat_allowed_for_all: bool,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_sound_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_message_override: AnnouncementOverride,
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_sound_override: AnnouncementOverride,
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
    /// `true` only while the participant is admitted, a relay-reported session
    /// has been recorded live since their last REST join or admit, and their
    /// presence lease is fresh. `false` means not confirmed live.
    #[serde(default)]
    pub in_call: bool,
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
    /// Meeting-level: whether the record button is shown to all admitted
    /// participants. Defaults to `false` so older API responses (that lack
    /// the field) hide the record button from non-hosts.
    #[serde(default)]
    pub recording_allowed_for_all: bool,
    /// Meeting-level: whether every admitted participant may SEND chat
    /// messages. Defaults to `true` so older API responses (that lack the
    /// field) and normal meetings keep chat open for everyone; a host turns
    /// it off to make chat host-only for an all-hands-style meeting.
    #[serde(default = "default_true")]
    pub chat_allowed_for_all: bool,
    /// Meeting-level: override of showing a message when someone joins.
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_message_override: AnnouncementOverride,
    /// Meeting-level: override of playing a sound when someone joins.
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub entry_sound_override: AnnouncementOverride,
    /// Meeting-level: override of showing a message when someone leaves.
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_message_override: AnnouncementOverride,
    /// Meeting-level: override of playing a sound when someone leaves.
    #[serde(default, skip_serializing_if = "AnnouncementOverride::is_participant")]
    pub exit_sound_override: AnnouncementOverride,
}

/// Returns `true`; used as the serde `default` for meeting-policy booleans
/// that are true when absent from older API responses.
fn default_true() -> bool {
    true
}

/// Returns `true` when the value is `true`; paired with `default_true` as the
/// `skip_serializing_if` predicate so default-valued (true) fields are omitted
/// on the wire.
fn is_true(v: &bool) -> bool {
    *v
}

/// Returns `true` when the count is zero; the `skip_serializing_if` predicate
/// paired with `#[serde(default)]` so zero counts are omitted on the wire.
fn is_zero_i64(v: &i64) -> bool {
    *v == 0
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the skip-iff-equals-default invariant on [`MeetingFeedSummary`]:
    /// every scalar field that carries `#[serde(default = D, skip_serializing_if = P)]`
    /// must use a predicate `P` that is true EXACTLY when the value equals `D`,
    /// so `absent <=> value == D` and serialization round-trips losslessly.
    ///
    /// It specifically exercises the default-true trap: `waiting_room_enabled`
    /// and `end_on_host_leave` default to `true`, so they MUST be skipped only
    /// when `true` (`is_true`) and emitted when `false`. Pairing `default_true`
    /// with a skip-when-false predicate would silently resurrect a genuine
    /// `false` as `true` on the wire — this test would catch that.
    #[test]
    fn meeting_feed_summary_round_trips_both_polarities() {
        // MUTATION-MEANINGFUL property: if someone changed
        // `waiting_room_enabled` to `skip_serializing_if = "std::ops::Not::not"`
        // (skip-when-false) while keeping `default = "default_true"`, then the
        // all-non-default instance below (which sets `waiting_room_enabled =
        // false`) would serialize WITHOUT the field. On deserialize the missing
        // field would fall back to `default_true` (i.e. `true`), so the
        // round-trip `from_str(to_string(original)) == original` assertion in
        // step 3 would FAIL (false != true). The same reasoning applies to
        // `end_on_host_leave`. This is what pins the predicate to the default.

        // 1. All-default instance: every scalar holds its schema default,
        // including the three default-TRUE policy flags. Option fields are a mix.
        let all_default = MeetingFeedSummary {
            meeting_id: "meeting-default".to_string(),
            state: "idle".to_string(),
            last_active_at: 1_000,
            created_at: 900,
            started_at: None,
            ended_at: None,
            host: Some("host@example.com".to_string()),
            host_display_name: None,
            host_user_id: Some("host@example.com".to_string()),
            is_owner: false,
            is_co_host: false,
            participant_count: 0,
            waiting_count: 0,
            has_password: false,
            allow_guests: false,
            recording_allowed_for_all: false,
            chat_allowed_for_all: true, // default-true!
            entry_message_override: AnnouncementOverride::Participant,
            entry_sound_override: AnnouncementOverride::Participant,
            exit_message_override: AnnouncementOverride::Participant,
            exit_sound_override: AnnouncementOverride::Participant,
            waiting_room_enabled: true, // default-true!
            admitted_can_admit: false,
            end_on_host_leave: true, // default-true!
            user_last_attended_at: None,
        };

        // 2. All-non-default instance: every bool flipped from its default,
        // counts non-zero, every Option populated.
        let all_non_default = MeetingFeedSummary {
            meeting_id: "meeting-non-default".to_string(),
            state: "active".to_string(),
            last_active_at: 2_000,
            created_at: 1_500,
            started_at: Some(1_600),
            ended_at: Some(1_900),
            host: Some("owner@example.com".to_string()),
            host_display_name: Some("Owner Name".to_string()),
            host_user_id: Some("owner@example.com".to_string()),
            is_owner: true,
            is_co_host: true,
            participant_count: 5,
            waiting_count: 3,
            has_password: true,
            allow_guests: true,
            recording_allowed_for_all: true,
            chat_allowed_for_all: false, // flipped from default-true
            entry_message_override: AnnouncementOverride::On,
            entry_sound_override: AnnouncementOverride::Off,
            exit_message_override: AnnouncementOverride::Off,
            exit_sound_override: AnnouncementOverride::On,
            waiting_room_enabled: false, // flipped from default-true
            admitted_can_admit: true,
            end_on_host_leave: false, // flipped from default-true
            user_last_attended_at: Some(1_950),
        };

        // 3. Both instances must round-trip losslessly through JSON.
        let json_default = serde_json::to_string(&all_default).unwrap();
        let round_default: MeetingFeedSummary = serde_json::from_str(&json_default).unwrap();
        assert_eq!(round_default, all_default);

        let json_non_default = serde_json::to_string(&all_non_default).unwrap();
        let round_non_default: MeetingFeedSummary =
            serde_json::from_str(&json_non_default).unwrap();
        assert_eq!(round_non_default, all_non_default);

        // 4. For the all-default instance, the default-valued scalar fields
        // must be OMITTED from the wire (bandwidth win for the home feed).
        assert!(!json_default.contains("\"is_owner\""));
        assert!(!json_default.contains("\"is_co_host\""));
        assert!(!json_default.contains("\"participant_count\""));
        assert!(!json_default.contains("\"waiting_room_enabled\""));
        assert!(!json_default.contains("\"end_on_host_leave\""));
        assert!(!json_default.contains("\"has_password\""));
        assert!(!json_default.contains("_override\""), "{json_default}");

        // 5. For the all-non-default instance, the now-non-default fields must
        // be PRESENT. `waiting_room_enabled` is `false` (non-default for a
        // default-true field) so it MUST be emitted; `is_owner` is `true`.
        assert!(json_non_default.contains("\"waiting_room_enabled\""));
        assert!(json_non_default.contains("\"is_co_host\""));
        assert!(json_non_default.contains("\"is_owner\""));
        assert!(json_non_default.contains(r#""exit_sound_override":"on""#));
    }

    const LEGACY_PARTICIPANT_STATUS: &str =
        r#"{"user_id":"u@example.com","status":"admitted","is_host":false,"joined_at":1}"#;

    const LEGACY_MEETING_INFO: &str = r#"{"meeting_id":"m","state":"active","host":"h",
        "has_password":false,"waiting_room_enabled":true,"admitted_can_admit":false,
        "end_on_host_leave":true,"participant_count":1,"waiting_count":0,"started_at":1}"#;

    const LEGACY_CREATE_MEETING: &str = r#"{"meeting_id":"m","host":"h","created_at":1,
        "state":"idle","attendees":[],"has_password":false,"waiting_room_enabled":true,
        "admitted_can_admit":false,"end_on_host_leave":true}"#;

    const LEGACY_MEETING_SUMMARY: &str = r#"{"meeting_id":"m","state":"idle",
        "has_password":false,"created_at":1,"participant_count":0,"started_at":1,
        "waiting_count":0,"waiting_room_enabled":true,"admitted_can_admit":false,
        "end_on_host_leave":true}"#;

    #[test]
    fn responses_from_a_server_without_overrides_read_participant() {
        let participant = [AnnouncementOverride::Participant; 4];

        let s: ParticipantStatusResponse = serde_json::from_str(LEGACY_PARTICIPANT_STATUS).unwrap();
        let got = [
            s.entry_message_override,
            s.entry_sound_override,
            s.exit_message_override,
            s.exit_sound_override,
        ];
        assert_eq!(got, participant, "ParticipantStatusResponse");

        let m: MeetingInfoResponse = serde_json::from_str(LEGACY_MEETING_INFO).unwrap();
        let got = [
            m.entry_message_override,
            m.entry_sound_override,
            m.exit_message_override,
            m.exit_sound_override,
        ];
        assert_eq!(got, participant, "MeetingInfoResponse");

        let c: CreateMeetingResponse = serde_json::from_str(LEGACY_CREATE_MEETING).unwrap();
        let got = [
            c.entry_message_override,
            c.entry_sound_override,
            c.exit_message_override,
            c.exit_sound_override,
        ];
        assert_eq!(got, participant, "CreateMeetingResponse");

        let l: MeetingSummary = serde_json::from_str(LEGACY_MEETING_SUMMARY).unwrap();
        let got = [
            l.entry_message_override,
            l.entry_sound_override,
            l.exit_message_override,
            l.exit_sound_override,
        ];
        assert_eq!(got, participant, "MeetingSummary");
    }

    const OVERRIDE_FIELDS: [&str; 4] = [
        "entry_message_override",
        "entry_sound_override",
        "exit_message_override",
        "exit_sound_override",
    ];

    fn wire_overrides<T: Serialize>(v: &T) -> serde_json::Map<String, serde_json::Value> {
        let serde_json::Value::Object(map) = serde_json::to_value(v).unwrap() else {
            panic!("a response must serialize to an object");
        };
        map.into_iter()
            .filter(|(k, _)| k.ends_with("_override"))
            .collect()
    }

    macro_rules! assert_only_non_participant_overrides_serialize {
        ($ty:ty, $legacy:expr) => {
            let base: $ty = serde_json::from_str($legacy).unwrap();
            let at_default = wire_overrides(&base);
            assert!(at_default.is_empty(), "{}: {at_default:?}", stringify!($ty));
            for (i, field) in OVERRIDE_FIELDS.iter().enumerate() {
                for value in [AnnouncementOverride::On, AnnouncementOverride::Off] {
                    let mut v = base.clone();
                    let slots = [
                        &mut v.entry_message_override,
                        &mut v.entry_sound_override,
                        &mut v.exit_message_override,
                        &mut v.exit_sound_override,
                    ];
                    *slots[i] = value;
                    let wire = wire_overrides(&v);
                    let keys: Vec<&String> = wire.keys().collect();
                    assert_eq!(keys, [field], "{} {field}={value:?}", stringify!($ty));
                    assert_eq!(wire[*field], value.as_str());
                    let back: $ty =
                        serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
                    let mut want = [AnnouncementOverride::Participant; 4];
                    want[i] = value;
                    let got = [
                        back.entry_message_override,
                        back.entry_sound_override,
                        back.exit_message_override,
                        back.exit_sound_override,
                    ];
                    assert_eq!(got, want, "{} {field}={value:?}", stringify!($ty));
                }
            }
        };
    }

    #[test]
    fn overrides_reach_the_wire_only_when_not_participant() {
        assert_only_non_participant_overrides_serialize!(
            ParticipantStatusResponse,
            LEGACY_PARTICIPANT_STATUS
        );
        assert_only_non_participant_overrides_serialize!(MeetingInfoResponse, LEGACY_MEETING_INFO);
        assert_only_non_participant_overrides_serialize!(
            CreateMeetingResponse,
            LEGACY_CREATE_MEETING
        );
        assert_only_non_participant_overrides_serialize!(MeetingSummary, LEGACY_MEETING_SUMMARY);
    }

    #[test]
    fn a_response_carrying_an_unknown_override_still_deserializes() {
        let mut body: serde_json::Value = serde_json::from_str(LEGACY_PARTICIPANT_STATUS).unwrap();
        body["entry_sound_override"] = "chime_only".into();
        body["exit_message_override"] = "off".into();
        let s: ParticipantStatusResponse =
            serde_json::from_value(body).expect("an unknown override must not fail the response");
        assert_eq!(s.entry_sound_override, AnnouncementOverride::Participant);
        assert_eq!(s.exit_message_override, AnnouncementOverride::Off);
    }

    #[test]
    fn participant_status_without_in_call_deserializes_as_not_in_call() {
        let legacy =
            r#"{"user_id":"u@example.com","status":"admitted","is_host":false,"joined_at":1}"#;
        let parsed: ParticipantStatusResponse =
            serde_json::from_str(legacy).expect("a response without in_call must deserialize");
        assert!(!parsed.in_call);

        let live = ParticipantStatusResponse {
            in_call: true,
            ..parsed
        };
        let wire = serde_json::to_value(&live).expect("serialize");
        assert_eq!(wire["in_call"], serde_json::Value::Bool(true));
    }
}
