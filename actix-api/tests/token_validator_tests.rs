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

//! Integration tests for JWT room access token validation (moved from inline `#[cfg(test)]` module).

use chrono::Utc;
use jsonwebtoken::{EncodingKey, Header};
use sec_api::token_validator::{decode_room_token, validate_room_token, TokenError};
use videocall_meeting_types::token::RoomAccessTokenClaims;

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

// -- TokenError classification tests --

#[test]
fn expired_is_retryable_and_not_suspicious() {
    let err = TokenError::Expired;
    assert!(err.is_retryable());
    assert!(!err.is_suspicious());
    assert_eq!(err.client_message(), "token has expired");
}

#[test]
fn invalid_signature_is_suspicious_and_not_retryable() {
    let err = TokenError::Invalid("InvalidSignature".to_string());
    assert!(err.is_suspicious());
    assert!(!err.is_retryable());
    assert_eq!(
        err.client_message(),
        "We detected unusual activity from your browser. This incident has been logged."
    );
}

#[test]
fn room_join_denied_is_not_retryable_and_not_suspicious() {
    let err = TokenError::RoomJoinDenied;
    assert!(!err.is_retryable());
    assert!(!err.is_suspicious());
    assert_eq!(err.client_message(), "access denied");
}

#[test]
fn missing_is_not_retryable_and_not_suspicious() {
    let err = TokenError::Missing;
    assert!(!err.is_retryable());
    assert!(!err.is_suspicious());
    assert_eq!(err.client_message(), "access denied");
}

#[test]
fn room_mismatch_is_not_retryable_and_not_suspicious() {
    let err = TokenError::RoomMismatch {
        token_room: "a".to_string(),
        requested_room: "b".to_string(),
    };
    assert!(!err.is_retryable());
    assert!(!err.is_suspicious());
}

#[test]
fn identity_mismatch_is_not_retryable_and_not_suspicious() {
    let err = TokenError::IdentityMismatch {
        token_identity: "a@test.com".to_string(),
        requested_identity: "b@test.com".to_string(),
    };
    assert!(!err.is_retryable());
    assert!(!err.is_suspicious());
}

// -- End-to-end: decode produces correct classification --

#[test]
fn expired_token_error_is_retryable() {
    let token = make_token("alice@test.com", "room-1", true, -120);
    let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
    assert!(err.is_retryable());
    assert!(!err.is_suspicious());
}

#[test]
fn wrong_secret_error_is_suspicious() {
    let token = make_token("alice@test.com", "room-1", true, 600);
    let err = decode_room_token("wrong-secret", &token).unwrap_err();
    assert!(err.is_suspicious());
    assert!(!err.is_retryable());
}

#[test]
fn garbage_token_error_is_suspicious() {
    let err = decode_room_token(TEST_SECRET, "not.a.jwt").unwrap_err();
    assert!(err.is_suspicious());
    assert!(!err.is_retryable());
}
