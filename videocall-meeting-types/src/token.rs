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

/// Sign a room access token for the given participant (HS256 with the
/// relay's `secret`, valid `ttl_secs`).
#[cfg(feature = "room-token")]
pub fn generate_room_token(
    secret: &str,
    ttl_secs: i64,
    user_id: &str,
    room: &str,
    is_host: bool,
    display_name: &str,
) -> Result<String, jsonwebtoken::errors::Error> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let claims = RoomAccessTokenClaims {
        sub: user_id.to_string(),
        room: room.to_string(),
        room_join: true,
        is_host,
        display_name: display_name.to_string(),
        observer: false,
        exp: now + ttl_secs,
        iss: RoomAccessTokenClaims::ISSUER.to_string(),
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )
}

#[cfg(all(test, feature = "room-token"))]
mod tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};

    #[test]
    fn room_token_round_trips_with_relay_claims() {
        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
        };
        let before = now();
        let token = generate_room_token("secret", 900, "user@test.com", "room-42", true, "Alice")
            .expect("should sign");
        let after = now();

        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let claims = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"secret"),
            &validation,
        )
        .expect("should decode")
        .claims;
        assert_eq!(claims.sub, "user@test.com");
        assert_eq!(claims.room, "room-42");
        assert_eq!(claims.display_name, "Alice");
        assert_eq!(claims.iss, "videocall-meeting-backend");
        assert!(claims.is_host && claims.room_join && !claims.observer);
        assert!((before + 900..=after + 900).contains(&claims.exp));
        assert!(decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"wrong"),
            &validation
        )
        .is_err());
    }
}
