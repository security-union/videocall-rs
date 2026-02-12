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

//! JWT token generation and validation.
//!
//! Two token types are issued by the Meeting Backend:
//!
//! - **Session token**: authenticates the user to the Meeting API. Delivered as
//!   an `HttpOnly` cookie so JavaScript cannot read it.
//! - **Room access token**: authorises a participant to join a specific room on
//!   the Media Server. Returned in the JSON response body.

use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use videocall_meeting_types::RoomAccessTokenClaims;

use crate::error::AppError;

// ---------------------------------------------------------------------------
// Session token
// ---------------------------------------------------------------------------

/// Claims embedded in a session JWT (stored in an HttpOnly cookie).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTokenClaims {
    /// User email (the identity principal).
    pub sub: String,
    /// Display name.
    pub name: String,
    /// Expiration (Unix timestamp).
    pub exp: i64,
    /// Issued-at (Unix timestamp).
    pub iat: i64,
    /// Issuer.
    pub iss: String,
}

impl SessionTokenClaims {
    pub const ISSUER: &'static str = "videocall-meeting-backend";
}

/// Create a signed session JWT for the given user.
///
/// The token is later set inside an `HttpOnly` cookie by the OAuth callback
/// handler so that the browser sends it automatically with every request.
pub fn generate_session_token(
    secret: &str,
    email: &str,
    name: &str,
    ttl_secs: i64,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = SessionTokenClaims {
        sub: email.to_string(),
        name: name.to_string(),
        exp: now + ttl_secs,
        iat: now,
        iss: SessionTokenClaims::ISSUER.to_string(),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!("Failed to sign session JWT: {e}");
        AppError::internal("failed to generate session token")
    })
}

/// Decode and validate a session JWT. Returns the claims on success.
pub fn decode_session_token(secret: &str, token: &str) -> Result<SessionTokenClaims, AppError> {
    let mut validation = Validation::default();
    validation.set_issuer(&[SessionTokenClaims::ISSUER]);

    decode::<SessionTokenClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(|e| {
        tracing::warn!("Session JWT validation failed: {e}");
        AppError::unauthorized_msg("invalid or expired session")
    })
}

// ---------------------------------------------------------------------------
// Room access token
// ---------------------------------------------------------------------------

/// Sign a room access token for the given participant.
pub fn generate_room_token(
    secret: &str,
    ttl_secs: i64,
    email: &str,
    room: &str,
    is_host: bool,
    display_name: &str,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = RoomAccessTokenClaims {
        sub: email.to_string(),
        room: room.to_string(),
        room_join: true,
        is_host,
        display_name: display_name.to_string(),
        exp: now + ttl_secs,
        iss: RoomAccessTokenClaims::ISSUER.to_string(),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!("Failed to sign JWT: {e}");
        AppError::internal("failed to generate room token")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use videocall_meeting_types::RoomAccessTokenClaims;

    const TEST_SECRET: &str = "super-secret-test-key";

    // -----------------------------------------------------------------------
    // Session token tests
    // -----------------------------------------------------------------------

    #[test]
    fn session_token_round_trips() {
        let token = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600)
            .expect("should sign");
        let claims = decode_session_token(TEST_SECRET, &token).expect("should decode");

        assert_eq!(claims.sub, "alice@test.com");
        assert_eq!(claims.name, "Alice");
        assert_eq!(claims.iss, SessionTokenClaims::ISSUER);
    }

    #[test]
    fn session_token_wrong_secret_fails() {
        let token = generate_session_token(TEST_SECRET, "a@b.com", "A", 3600).expect("should sign");
        let err = decode_session_token("wrong-secret", &token);
        assert!(err.is_err());
    }

    #[test]
    fn session_token_expired_fails() {
        // Use a TTL of -120s to exceed jsonwebtoken's default 60s leeway.
        let token = generate_session_token(TEST_SECRET, "a@b.com", "A", -120).expect("should sign");
        let err = decode_session_token(TEST_SECRET, &token);
        assert!(err.is_err());
    }

    #[test]
    fn session_token_has_iat() {
        let before = Utc::now().timestamp();
        let token = generate_session_token(TEST_SECRET, "a@b.com", "A", 3600).expect("should sign");
        let after = Utc::now().timestamp();

        let claims = decode_session_token(TEST_SECRET, &token).expect("should decode");
        assert!(claims.iat >= before);
        assert!(claims.iat <= after);
    }

    // -----------------------------------------------------------------------
    // Room access token tests
    // -----------------------------------------------------------------------

    #[test]
    fn token_round_trips_with_correct_claims() {
        let token =
            generate_room_token(TEST_SECRET, 600, "user@test.com", "room-42", true, "Alice")
                .expect("should sign");

        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let data = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(TEST_SECRET.as_bytes()),
            &validation,
        )
        .expect("should decode");

        assert_eq!(data.claims.sub, "user@test.com");
        assert_eq!(data.claims.room, "room-42");
        assert!(data.claims.is_host);
        assert_eq!(data.claims.display_name, "Alice");
        assert!(data.claims.room_join);
    }

    #[test]
    fn issuer_is_videocall_meeting_backend() {
        let token = generate_room_token(TEST_SECRET, 300, "a@b.com", "r", false, "Bob")
            .expect("should sign");

        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let data = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(TEST_SECRET.as_bytes()),
            &validation,
        )
        .expect("should decode");

        assert_eq!(data.claims.iss, "videocall-meeting-backend");
    }

    #[test]
    fn exp_is_now_plus_ttl() {
        let ttl = 900_i64;
        let before = Utc::now().timestamp();
        let token =
            generate_room_token(TEST_SECRET, ttl, "a@b.com", "r", false, "X").expect("should sign");
        let after = Utc::now().timestamp();

        let mut validation = Validation::default();
        validation.insecure_disable_signature_validation();
        validation.validate_exp = false;
        let data = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"ignored"),
            &validation,
        )
        .expect("should decode");

        assert!(data.claims.exp >= before + ttl);
        assert!(data.claims.exp <= after + ttl);
    }

    #[test]
    fn room_join_is_always_true() {
        let token =
            generate_room_token(TEST_SECRET, 60, "a@b.com", "r", false, "X").expect("should sign");

        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let data = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(TEST_SECRET.as_bytes()),
            &validation,
        )
        .expect("should decode");

        assert!(data.claims.room_join);
    }
}
