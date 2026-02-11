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

//! JWT room access token generation.
//!
//! The Meeting Backend signs tokens with a shared secret; the Media Server
//! validates the signature and extracts the claims.

use chrono::Utc;
use jsonwebtoken::{encode, EncodingKey, Header};
use videocall_meeting_types::RoomAccessTokenClaims;

use crate::error::AppError;

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
