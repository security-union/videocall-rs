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

//! Room access token (JWT) claims.
//!
//! The room access token is a signed JWT (HMAC-SHA256) that authorizes a client
//! to connect to the Media Server for a specific room. The Meeting Backend signs
//! the token; the Media Server validates the signature and extracts the claims.

use serde::{Deserialize, Serialize};

/// JWT payload for a room access token.
///
/// This is the **only** credential the Media Server accepts for connection.
/// The token is issued by the Meeting Backend when a participant is admitted.
///
/// # Example payload
///
/// ```json
/// {
///   "sub": "user@example.com",
///   "room": "standup-2024",
///   "room_join": true,
///   "is_host": true,
///   "display_name": "Alice",
///   "observer": false,
///   "exp": 1707004800,
///   "iss": "videocall-meeting-backend"
/// }
/// ```
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RoomAccessTokenClaims {
    /// Participant's user_id (unique identity).
    pub sub: String,

    /// The room/meeting ID the participant is authorized to join.
    pub room: String,

    /// Must be `true` for the Media Server to accept the connection.
    pub room_join: bool,

    /// Whether this participant is the meeting host.
    pub is_host: bool,

    /// Participant's chosen display name for this meeting.
    pub display_name: String,

    /// Whether this token grants observer-only access (no media publishing).
    /// Observer tokens are issued to participants waiting for meeting activation
    /// or waiting-room admission so they can receive push notifications.
    #[serde(default)]
    pub observer: bool,

    /// Whether the meeting ends for all participants when the host leaves.
    /// Defaults to `true` for backward compatibility with older tokens that
    /// lack this claim.
    #[serde(default = "default_true")]
    pub end_on_host_leave: bool,

    /// Expiration timestamp (Unix seconds).
    /// Token is rejected after this time.
    pub exp: i64,

    /// Issuer identifier. Always `"videocall-meeting-backend"`.
    pub iss: String,
}

impl RoomAccessTokenClaims {
    /// The expected issuer value for tokens produced by the Meeting Backend.
    pub const ISSUER: &'static str = "videocall-meeting-backend";
}

fn default_true() -> bool {
    true
}
