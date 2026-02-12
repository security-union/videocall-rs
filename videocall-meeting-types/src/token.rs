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
///   "exp": 1707004800,
///   "iss": "videocall-meeting-backend"
/// }
/// ```
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RoomAccessTokenClaims {
    /// Participant's email (unique identity).
    pub sub: String,

    /// The room/meeting ID the participant is authorized to join.
    pub room: String,

    /// Must be `true` for the Media Server to accept the connection.
    pub room_join: bool,

    /// Whether this participant is the meeting host.
    pub is_host: bool,

    /// Participant's chosen display name for this meeting.
    pub display_name: String,

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
