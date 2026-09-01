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
///   "iss": "videocall-meeting-backend",
///   "typ": "room_access"
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

    /// Whether this participant joined as an unauthenticated guest.
    #[serde(default)]
    pub is_guest: bool,

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

    /// Token-type discriminator (#2411); `None` predates the claim. See [`check_token_type`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
}

impl RoomAccessTokenClaims {
    /// The expected issuer value for tokens produced by the Meeting Backend.
    pub const ISSUER: &'static str = "videocall-meeting-backend";

    /// The expected `typ` value; `meeting-api`'s `SessionTokenClaims` uses `"session"`.
    pub const TOKEN_TYPE: &'static str = "room_access";
}

fn default_true() -> bool {
    true
}

/// Outcome of comparing a token's `typ` claim against the expected type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenTypeCheck {
    Match,
    Legacy,
    Mismatch { found: String },
}

/// Classify a token's `typ` claim against `expected`.
pub fn check_token_type(typ: Option<&str>, expected: &str) -> TokenTypeCheck {
    match typ {
        None => TokenTypeCheck::Legacy,
        Some(found) if found == expected => TokenTypeCheck::Match,
        Some(found) => TokenTypeCheck::Mismatch {
            found: found.to_string(),
        },
    }
}

/// Prefix used for guest participant user IDs: `"guest:{uuid}"`.
///
/// Guest `sub` claims in [`RoomAccessTokenClaims`] always start with this
/// prefix, so any code that receives a user ID can distinguish guests from
/// authenticated users without inspecting the `is_guest` flag.
pub const GUEST_USER_ID_PREFIX: &str = "guest:";

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: &str = RoomAccessTokenClaims::TOKEN_TYPE;

    #[test]
    fn check_token_type_classifies_all_three_outcomes() {
        assert_eq!(
            check_token_type(Some(EXPECTED), EXPECTED),
            TokenTypeCheck::Match
        );
        assert_eq!(check_token_type(None, EXPECTED), TokenTypeCheck::Legacy);
        assert_eq!(
            check_token_type(Some("session"), EXPECTED),
            TokenTypeCheck::Mismatch {
                found: "session".to_string()
            }
        );
    }

    #[test]
    fn room_and_session_type_values_differ() {
        assert_ne!(RoomAccessTokenClaims::TOKEN_TYPE, "session");
    }

    #[test]
    fn absent_typ_deserialises_as_legacy() {
        let legacy = r#"{
            "sub": "alice@test.com",
            "room": "room-1",
            "room_join": true,
            "is_host": false,
            "display_name": "Alice",
            "exp": 1707004800,
            "iss": "videocall-meeting-backend"
        }"#;

        let claims: RoomAccessTokenClaims = serde_json::from_str(legacy).expect("must deserialise");
        assert_eq!(claims.typ, None);
        assert_eq!(
            check_token_type(claims.typ.as_deref(), EXPECTED),
            TokenTypeCheck::Legacy
        );
    }

    #[test]
    fn none_typ_is_omitted_from_the_wire() {
        let claims = RoomAccessTokenClaims {
            sub: "alice@test.com".to_string(),
            room: "room-1".to_string(),
            room_join: true,
            is_host: false,
            is_guest: false,
            display_name: "Alice".to_string(),
            observer: false,
            end_on_host_leave: true,
            exp: 1707004800,
            iss: RoomAccessTokenClaims::ISSUER.to_string(),
            typ: None,
        };

        assert!(!serde_json::to_string(&claims).unwrap().contains("typ"));
    }
}
