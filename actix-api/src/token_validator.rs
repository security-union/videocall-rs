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

//! JWT room access token validation.
//!
//! Validates tokens issued by the Meeting Backend (meeting-api) before allowing
//! a client to connect to the Media Server. Mirrors the approach used by LiveKit:
//! parse JWT, verify HMAC signature, check `room_join == true`, and ensure the
//! room and identity match the connection request.

use jsonwebtoken::{DecodingKey, Validation};
use std::fmt;
use videocall_meeting_types::token::RoomAccessTokenClaims;

/// Errors that can occur during room token validation.
#[derive(Debug)]
pub enum TokenError {
    /// No token was provided but one is required (FF=on).
    Missing,
    /// Token could not be decoded or signature is invalid.
    Invalid(String),
    /// Token has expired (`exp` claim is in the past).
    Expired,
    /// The `room_join` claim is `false`; participant is not authorized to join.
    RoomJoinDenied,
    /// The `room` claim does not match the room in the connection URL.
    RoomMismatch {
        token_room: String,
        requested_room: String,
    },
    /// The `sub` claim does not match the email/identity in the connection URL.
    IdentityMismatch {
        token_identity: String,
        requested_identity: String,
    },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing => write!(f, "room access token is required"),
            TokenError::Invalid(msg) => write!(f, "invalid token: {msg}"),
            TokenError::Expired => write!(f, "token has expired"),
            TokenError::RoomJoinDenied => write!(f, "token does not grant room join permission"),
            TokenError::RoomMismatch {
                token_room,
                requested_room,
            } => write!(
                f,
                "token room '{token_room}' does not match requested room '{requested_room}'"
            ),
            TokenError::IdentityMismatch {
                token_identity,
                requested_identity,
            } => write!(
                f,
                "token identity '{token_identity}' does not match '{requested_identity}'"
            ),
        }
    }
}

impl std::error::Error for TokenError {}

/// Decode and validate a JWT room access token, extracting claims.
///
/// This is the primary validation function for the **token-based** connection
/// endpoint (`GET /lobby?token=<JWT>`). The identity (email) and room are
/// extracted from the token claims themselves -- there are no URL path
/// parameters to cross-check.
///
/// Checks:
/// 1. Signature is valid (HMAC-SHA256)
/// 2. Token is not expired (`exp`)
/// 3. Issuer matches `RoomAccessTokenClaims::ISSUER`
/// 4. `room_join` is `true`
pub fn decode_room_token(secret: &str, token: &str) -> Result<RoomAccessTokenClaims, TokenError> {
    let decoding_key = DecodingKey::from_secret(secret.as_bytes());

    let mut validation = Validation::default();
    validation.set_required_spec_claims(&["exp", "sub"]);
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    validation.validate_exp = true;

    let token_data =
        jsonwebtoken::decode::<RoomAccessTokenClaims>(token, &decoding_key, &validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                _ => TokenError::Invalid(e.to_string()),
            },
        )?;

    let claims = token_data.claims;

    if !claims.room_join {
        return Err(TokenError::RoomJoinDenied);
    }

    Ok(claims)
}

/// Validate a JWT room access token against expected room and identity.
///
/// Validate a JWT room access token against expected room and identity.
///
/// **DEPRECATED**: This function is used by the legacy `GET /lobby/{email}/{room}`
/// endpoint. Prefer [`decode_room_token`] for the new token-based endpoint where
/// identity and room are extracted from the JWT claims directly.
///
/// Decodes the token with HMAC-SHA256 using `secret`, then checks:
/// 1. Signature is valid
/// 2. Token is not expired (`exp`)
/// 3. Issuer matches `RoomAccessTokenClaims::ISSUER`
/// 4. `room_join` is `true`
/// 5. `room` matches `expected_room`
/// 6. `sub` matches `expected_email`
pub fn validate_room_token(
    secret: &str,
    token: &str,
    expected_room: &str,
    expected_email: &str,
) -> Result<RoomAccessTokenClaims, TokenError> {
    let claims = decode_room_token(secret, token)?;

    if claims.room != expected_room {
        return Err(TokenError::RoomMismatch {
            token_room: claims.room.clone(),
            requested_room: expected_room.to_string(),
        });
    }

    if claims.sub != expected_email {
        return Err(TokenError::IdentityMismatch {
            token_identity: claims.sub.clone(),
            requested_identity: expected_email.to_string(),
        });
    }

    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use jsonwebtoken::{EncodingKey, Header};

    const TEST_SECRET: &str = "test-secret-for-unit-tests";

    fn make_token(email: &str, room: &str, room_join: bool, exp_offset_secs: i64) -> String {
        let now = Utc::now().timestamp();
        let claims = RoomAccessTokenClaims {
            sub: email.to_string(),
            room: room.to_string(),
            room_join,
            is_host: false,
            display_name: email.to_string(),
            exp: now + exp_offset_secs,
            iss: RoomAccessTokenClaims::ISSUER.to_string(),
        };
        jsonwebtoken::encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
        )
        .unwrap()
    }

    // -- decode_room_token tests (primary, no URL cross-check) --

    #[test]
    fn decode_valid_token_extracts_claims() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(result.is_ok());
        let claims = result.unwrap();
        assert_eq!(claims.sub, "alice@test.com");
        assert_eq!(claims.room, "room-1");
        assert!(claims.room_join);
    }

    #[test]
    fn decode_expired_token_fails() {
        // Use -120 to exceed jsonwebtoken's default 60-second leeway
        let token = make_token("alice@test.com", "room-1", true, -120);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(matches!(result, Err(TokenError::Expired)));
    }

    #[test]
    fn decode_wrong_secret_fails() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = decode_room_token("wrong-secret", &token);
        assert!(matches!(result, Err(TokenError::Invalid(_))));
    }

    #[test]
    fn decode_room_join_false_fails() {
        let token = make_token("alice@test.com", "room-1", false, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(matches!(result, Err(TokenError::RoomJoinDenied)));
    }

    #[test]
    fn decode_garbage_token_fails() {
        let result = decode_room_token(TEST_SECRET, "not.a.jwt");
        assert!(matches!(result, Err(TokenError::Invalid(_))));
    }

    // -- validate_room_token tests (deprecated, with URL cross-check) --

    #[test]
    fn valid_token_succeeds() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = validate_room_token(TEST_SECRET, &token, "room-1", "alice@test.com");
        assert!(result.is_ok());
        let claims = result.unwrap();
        assert_eq!(claims.sub, "alice@test.com");
        assert_eq!(claims.room, "room-1");
        assert!(claims.room_join);
    }

    #[test]
    fn room_mismatch_fails() {
        let token = make_token("alice@test.com", "room-A", true, 600);
        let result = validate_room_token(TEST_SECRET, &token, "room-B", "alice@test.com");
        assert!(matches!(result, Err(TokenError::RoomMismatch { .. })));
    }

    #[test]
    fn identity_mismatch_fails() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = validate_room_token(TEST_SECRET, &token, "room-1", "bob@test.com");
        assert!(matches!(result, Err(TokenError::IdentityMismatch { .. })));
    }
}
