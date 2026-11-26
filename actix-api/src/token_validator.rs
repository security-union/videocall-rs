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
//! parse JWT, verify HMAC signature, check `room_join == true` (or `observer`),
//! and take identity and room from the claims. The relay has no other way in.

use jsonwebtoken::{DecodingKey, Validation};
use regex::Regex;
use std::fmt;
use videocall_meeting_types::token::RoomAccessTokenClaims;

use crate::constants::VALID_ID_PATTERN;

lazy_static::lazy_static! {
    /// Compiled regex for validating room identifiers against NATS-safe characters.
    /// Only allows alphanumeric characters, underscores, and hyphens.
    static ref VALID_ID_RE: Regex = Regex::new(VALID_ID_PATTERN).expect("VALID_ID_PATTERN is a valid regex");
}

/// Errors that can occur during room token validation.
#[derive(Debug)]
pub enum TokenError {
    /// No token was provided. Every connection needs one.
    Missing,
    /// Token could not be decoded or signature is invalid.
    Invalid(String),
    /// Token has expired (`exp` claim is in the past).
    Expired,
    /// The `room_join` claim is `false`; participant is not authorized to join.
    RoomJoinDenied,
    /// A claim value contains characters that are unsafe for use in NATS subjects
    /// (dots, wildcards `*` / `>`, spaces, etc.). This indicates either a tampered
    /// JWT or a bug in the token-issuing service.
    UnsafeIdentifier { field: String, value: String },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing => write!(f, "room access token is required"),
            TokenError::Invalid(msg) => write!(f, "invalid token: {msg}"),
            TokenError::Expired => write!(f, "token has expired"),
            TokenError::RoomJoinDenied => write!(f, "token does not grant room join permission"),
            TokenError::UnsafeIdentifier { field, value } => write!(
                f,
                "token {field} '{value}' contains characters unsafe for NATS subjects"
            ),
        }
    }
}

impl std::error::Error for TokenError {}

impl TokenError {
    /// Whether this error represents a potentially tampered or forged token
    /// (as opposed to a benign expiration or missing token).
    pub fn is_suspicious(&self) -> bool {
        matches!(
            self,
            TokenError::Invalid(_) | TokenError::UnsafeIdentifier { .. }
        )
    }

    /// HTTP status used to refuse a connection with this error, on both
    /// transports (the WebSocket upgrade response and the WebTransport
    /// CONNECT response).
    ///
    /// - `401`: come back with a token. Either none was sent or it expired.
    /// - `403`: the token was read and refused; sending it again won't help.
    pub fn http_status(&self) -> u16 {
        match self {
            TokenError::Missing | TokenError::Expired => 401,
            _ => 403,
        }
    }

    /// The message that should be returned to the client.
    ///
    /// Suspicious errors (invalid signature, tampered tokens) get a generic
    /// security warning instead of leaking internal details.
    pub fn client_message(&self) -> &str {
        if self.is_suspicious() {
            "We detected unusual activity from your browser. This incident has been logged."
        } else {
            match self {
                TokenError::Expired => "token has expired",
                _ => "access denied",
            }
        }
    }

    /// Log this error at the appropriate severity level.
    ///
    /// Suspicious errors are logged at `warn!` (potential attack);
    /// everything else is `info!` (normal operational event).
    pub fn log(&self, transport: &str) {
        if self.is_suspicious() {
            tracing::warn!("{transport} connection rejected: {self}");
        } else {
            tracing::info!("{transport} connection rejected: {self}");
        }
    }
}

/// Decode and validate a JWT room access token, extracting claims.
///
/// This is the validation function for the only connection endpoint
/// (`/lobby?token=<JWT>` on both WebSocket and WebTransport). The identity
/// (user_id) and room are extracted from the token claims themselves; there
/// are no URL path parameters to cross-check.
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

    // Reject room names containing NATS-unsafe characters (dots, wildcards,
    // spaces, etc.). A room like "foo.>" would let an attacker subscribe to
    // arbitrary NATS subjects. Only alphanumeric, underscore, and hyphen are
    // allowed. We do NOT validate `sub` (email) here because email addresses
    // naturally contain `.` and `@`; the `sub` field is not interpolated raw
    // into NATS subjects the same way `room` is.
    if !VALID_ID_RE.is_match(&claims.room) {
        return Err(TokenError::UnsafeIdentifier {
            field: "room".to_string(),
            value: claims.room,
        });
    }

    // Allow connection if the token grants room join permission OR is an
    // observer token. Observers have `room_join: false` but `observer: true`
    // as a defense-in-depth measure — they can connect to receive push
    // notifications but cannot participate in the room media exchange.
    if !claims.room_join && !claims.observer {
        return Err(TokenError::RoomJoinDenied);
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
            observer: false,
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
    fn decode_room_join_false_non_observer_fails() {
        let token = make_token("alice@test.com", "room-1", false, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(matches!(result, Err(TokenError::RoomJoinDenied)));
    }

    #[test]
    fn decode_observer_token_with_room_join_false_succeeds() {
        let now = Utc::now().timestamp();
        let claims = RoomAccessTokenClaims {
            sub: "observer@test.com".to_string(),
            room: "room-1".to_string(),
            room_join: false,
            is_host: false,
            display_name: "Observer".to_string(),
            observer: true,
            exp: now + 600,
            iss: RoomAccessTokenClaims::ISSUER.to_string(),
        };
        let token = jsonwebtoken::encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
        )
        .unwrap();
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(result.is_ok());
        let decoded = result.unwrap();
        assert!(decoded.observer);
        assert!(!decoded.room_join);
    }

    #[test]
    fn decode_garbage_token_fails() {
        let result = decode_room_token(TEST_SECRET, "not.a.jwt");
        assert!(matches!(result, Err(TokenError::Invalid(_))));
    }

    // -- TokenError classification tests --

    #[test]
    fn expired_is_401_and_not_suspicious() {
        let err = TokenError::Expired;
        assert_eq!(err.http_status(), 401);
        assert!(!err.is_suspicious());
        assert_eq!(err.client_message(), "token has expired");
    }

    #[test]
    fn invalid_signature_is_suspicious_and_403() {
        let err = TokenError::Invalid("InvalidSignature".to_string());
        assert_eq!(err.http_status(), 403);
        assert!(err.is_suspicious());
        assert_eq!(
            err.client_message(),
            "We detected unusual activity from your browser. This incident has been logged."
        );
    }

    #[test]
    fn room_join_denied_is_403_and_not_suspicious() {
        let err = TokenError::RoomJoinDenied;
        assert_eq!(err.http_status(), 403);
        assert!(!err.is_suspicious());
        assert_eq!(err.client_message(), "access denied");
    }

    #[test]
    fn http_status_is_401_for_missing_or_expired_and_403_otherwise() {
        assert_eq!(TokenError::Missing.http_status(), 401);
        assert_eq!(TokenError::Expired.http_status(), 401);
        assert_eq!(TokenError::Invalid("x".to_string()).http_status(), 403);
        assert_eq!(TokenError::RoomJoinDenied.http_status(), 403);
        assert_eq!(
            TokenError::UnsafeIdentifier {
                field: "room".to_string(),
                value: "a.b".to_string(),
            }
            .http_status(),
            403
        );
    }

    #[test]
    fn missing_is_401_and_not_suspicious() {
        let err = TokenError::Missing;
        assert_eq!(err.http_status(), 401);
        assert!(!err.is_suspicious());
        assert_eq!(err.client_message(), "access denied");
    }

    // -- End-to-end: decode produces correct classification --

    #[test]
    fn expired_token_error_is_401() {
        let token = make_token("alice@test.com", "room-1", true, -120);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert_eq!(err.http_status(), 401);
        assert!(!err.is_suspicious());
    }

    #[test]
    fn wrong_secret_error_is_suspicious() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let err = decode_room_token("wrong-secret", &token).unwrap_err();
        assert!(err.is_suspicious());
    }

    #[test]
    fn garbage_token_error_is_suspicious() {
        let err = decode_room_token(TEST_SECRET, "not.a.jwt").unwrap_err();
        assert!(err.is_suspicious());
    }

    // -- NATS subject injection tests --

    #[test]
    fn room_with_dots_is_rejected() {
        let token = make_token("alice@test.com", "room.sub.topic", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
        assert!(err.is_suspicious());
    }

    #[test]
    fn room_with_wildcard_star_is_rejected() {
        let token = make_token("alice@test.com", "room-*", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    fn room_with_wildcard_gt_is_rejected() {
        let token = make_token("alice@test.com", "room->", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    fn room_with_spaces_is_rejected() {
        let token = make_token("alice@test.com", "room name", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    fn room_with_valid_chars_is_accepted() {
        let token = make_token("alice@test.com", "My_Room-123", true, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().room, "My_Room-123");
    }

    #[test]
    fn unsafe_identifier_error_is_suspicious_and_403() {
        let err = TokenError::UnsafeIdentifier {
            field: "room".to_string(),
            value: "evil.>".to_string(),
        };
        assert_eq!(err.http_status(), 403);
        assert!(err.is_suspicious());
        assert_eq!(
            err.client_message(),
            "We detected unusual activity from your browser. This incident has been logged."
        );
    }
}
