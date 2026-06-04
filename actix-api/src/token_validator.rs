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
use regex::Regex;
use std::fmt;
use videocall_meeting_types::token::RoomAccessTokenClaims;

use crate::constants::VALID_ID_PATTERN;
use crate::metrics::AUTH_REJECTIONS_TOTAL;

lazy_static::lazy_static! {
    /// Compiled regex for validating room identifiers against NATS-safe characters.
    /// Only allows alphanumeric characters, underscores, and hyphens.
    static ref VALID_ID_RE: Regex = Regex::new(VALID_ID_PATTERN).expect("VALID_ID_PATTERN is a valid regex");
}

/// Errors that can occur during room token validation.
#[derive(Debug)]
pub enum TokenError {
    /// No token was provided but one is required (FF=on).
    Missing,
    /// HMAC signature verification failed. Strong signal of tampering or a
    /// secret-rotation drift between the token-issuing service and the relay.
    InvalidSignature,
    /// A claim required by `Validation::set_required_spec_claims` is absent
    /// (e.g. `exp` or `sub`). The wrapped string is the missing claim name.
    MissingClaim(String),
    /// The token doesn't have the JWT shape (header.payload.signature triplet),
    /// failed Base64/UTF-8/JSON decoding, or otherwise can't be parsed.
    Malformed(String),
    /// Catch-all for `jsonwebtoken::ErrorKind` variants that don't map cleanly
    /// onto the more specific buckets above (e.g. invalid issuer / audience /
    /// algorithm). Preserves backward compat with callers that match on
    /// `Invalid(_)` from the legacy single-variant API.
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
    /// The `sub` claim does not match the user ID/identity in the connection URL.
    IdentityMismatch {
        token_identity: String,
        requested_identity: String,
    },
    /// A claim value contains characters that are unsafe for use in NATS subjects
    /// (dots, wildcards `*` / `>`, spaces, etc.). This indicates either a tampered
    /// JWT or a bug in the token-issuing service.
    UnsafeIdentifier { field: String, value: String },
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing => write!(f, "room access token is required"),
            TokenError::InvalidSignature => {
                write!(f, "invalid token: signature verification failed")
            }
            TokenError::MissingClaim(claim) => {
                write!(f, "invalid token: missing required claim '{claim}'")
            }
            TokenError::Malformed(msg) => write!(f, "invalid token: {msg}"),
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
            TokenError::InvalidSignature
                | TokenError::Malformed(_)
                | TokenError::Invalid(_)
                | TokenError::UnsafeIdentifier { .. }
        )
    }

    /// Whether the client should receive a 401 (retry with a fresh token) or
    /// 403 (do not retry, access denied).
    ///
    /// Only `Expired` is retryable; everything else is a hard denial.
    pub fn is_retryable(&self) -> bool {
        matches!(self, TokenError::Expired)
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

    /// Stable Prometheus label for this rejection reason.
    ///
    /// Returned values are intentionally constrained — see the cardinality
    /// note on `videocall_auth_rejections_total` in `metrics.rs`. Keep the
    /// set bounded.
    pub fn reason(&self) -> &'static str {
        match self {
            TokenError::Expired => "token_expired",
            TokenError::InvalidSignature => "invalid_signature",
            TokenError::MissingClaim(_) => "missing_claim",
            TokenError::Malformed(_) => "malformed",
            // `Missing`, `Invalid`, `RoomJoinDenied`, `RoomMismatch`,
            // `IdentityMismatch`, `UnsafeIdentifier` all roll up to `other`.
            // Splitting them further would inflate cardinality with little
            // alerting value (they're all hard-deny operational events).
            _ => "other",
        }
    }

    /// Increment `videocall_auth_rejections_total{reason=...}` for this error.
    ///
    /// Called from the centralized validation entry points so every JWT
    /// rejection is recorded regardless of which transport (WS or WT) the
    /// client was attempting to use.
    pub fn record_rejection(&self) {
        AUTH_REJECTIONS_TOTAL
            .with_label_values(&[self.reason()])
            .inc();
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
/// This is the primary validation function for the **token-based** connection
/// endpoint (`GET /lobby?token=<JWT>`). The identity (user_id) and room are
/// extracted from the token claims themselves -- there are no URL path
/// parameters to cross-check.
///
/// Checks:
/// 1. Signature is valid (HMAC-SHA256)
/// 2. Token is not expired (`exp`)
/// 3. Issuer matches `RoomAccessTokenClaims::ISSUER`
/// 4. `room_join` is `true`
pub fn decode_room_token(secret: &str, token: &str) -> Result<RoomAccessTokenClaims, TokenError> {
    decode_room_token_inner(secret, token).inspect_err(TokenError::record_rejection)
}

/// Inner decode implementation. Separated from the public entry point so
/// `decode_room_token` can wrap every error path with a single
/// `record_rejection` call without scattering counter increments through
/// every `Err(...)` site.
fn decode_room_token_inner(secret: &str, token: &str) -> Result<RoomAccessTokenClaims, TokenError> {
    let decoding_key = DecodingKey::from_secret(secret.as_bytes());

    let mut validation = Validation::default();
    validation.set_required_spec_claims(&["exp", "sub"]);
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
    validation.validate_exp = true;

    let token_data =
        jsonwebtoken::decode::<RoomAccessTokenClaims>(token, &decoding_key, &validation).map_err(
            |e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => TokenError::Expired,
                jsonwebtoken::errors::ErrorKind::InvalidSignature => TokenError::InvalidSignature,
                jsonwebtoken::errors::ErrorKind::MissingRequiredClaim(claim) => {
                    TokenError::MissingClaim(claim.clone())
                }
                // Shape / decode failures: the bytes don't even look like a JWT
                // we can interpret. Bucket as `malformed`.
                jsonwebtoken::errors::ErrorKind::InvalidToken
                | jsonwebtoken::errors::ErrorKind::Base64(_)
                | jsonwebtoken::errors::ErrorKind::Json(_)
                | jsonwebtoken::errors::ErrorKind::Utf8(_) => TokenError::Malformed(e.to_string()),
                // Everything else (InvalidIssuer, InvalidAudience, InvalidAlgorithm,
                // Crypto, key-format issues) maps to the catch-all `Invalid`
                // bucket which `reason()` reports as `other`.
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

/// Validate a JWT room access token against expected room and identity.
///
/// Validate a JWT room access token against expected room and identity.
///
/// **DEPRECATED**: This function is used by the legacy `GET /lobby/{user_id}/{room}`
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
    // Delegate to the un-counted inner decoder so that mismatches detected
    // *here* (RoomMismatch / IdentityMismatch) are still recorded by the
    // single `record_rejection` call below — without double-counting decode
    // failures that already incremented the counter inside `decode_room_token`.
    validate_room_token_inner(secret, token, expected_room, expected_email)
        .inspect_err(TokenError::record_rejection)
}

fn validate_room_token_inner(
    secret: &str,
    token: &str,
    expected_room: &str,
    expected_email: &str,
) -> Result<RoomAccessTokenClaims, TokenError> {
    let claims = decode_room_token_inner(secret, token)?;

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
    use serial_test::serial;

    // Phase 8b note: every test that calls `decode_room_token` or
    // `validate_room_token` mutates the global `AUTH_REJECTIONS_TOTAL` counter.
    // Tests that read deltas of that counter only get deterministic results if
    // every counter-mutating test in this module shares one serial lock — so
    // every relevant test below carries `#[serial(token_validator_counter)]`.

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
            is_guest: false,
            end_on_host_leave: false,
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
    #[serial(token_validator_counter)]
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
    #[serial(token_validator_counter)]
    fn decode_expired_token_fails() {
        // Use -120 to exceed jsonwebtoken's default 60-second leeway
        let token = make_token("alice@test.com", "room-1", true, -120);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(matches!(result, Err(TokenError::Expired)));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn decode_wrong_secret_fails() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = decode_room_token("wrong-secret", &token);
        // Phase 8b: HMAC mismatches are now classified as InvalidSignature so
        // they can be alerted on independently from generic malformed tokens.
        assert!(matches!(result, Err(TokenError::InvalidSignature)));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn decode_room_join_false_non_observer_fails() {
        let token = make_token("alice@test.com", "room-1", false, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(matches!(result, Err(TokenError::RoomJoinDenied)));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn decode_observer_token_with_room_join_false_succeeds() {
        let now = Utc::now().timestamp();
        let claims = RoomAccessTokenClaims {
            sub: "observer@test.com".to_string(),
            room: "room-1".to_string(),
            room_join: false,
            is_host: false,
            display_name: "Observer".to_string(),
            observer: true,
            is_guest: false,
            end_on_host_leave: false,
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
    #[serial(token_validator_counter)]
    fn decode_garbage_token_fails() {
        let result = decode_room_token(TEST_SECRET, "not.a.jwt");
        // Phase 8b: shape/decode failures are now classified as `Malformed`
        // so they can be alerted on independently from signature mismatches.
        assert!(matches!(result, Err(TokenError::Malformed(_))));
    }

    // -- validate_room_token tests (deprecated, with URL cross-check) --

    #[test]
    #[serial(token_validator_counter)]
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
    #[serial(token_validator_counter)]
    fn room_mismatch_fails() {
        let token = make_token("alice@test.com", "room-A", true, 600);
        let result = validate_room_token(TEST_SECRET, &token, "room-B", "alice@test.com");
        assert!(matches!(result, Err(TokenError::RoomMismatch { .. })));
    }

    #[test]
    #[serial(token_validator_counter)]
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
    #[serial(token_validator_counter)]
    fn expired_token_error_is_retryable() {
        let token = make_token("alice@test.com", "room-1", true, -120);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(err.is_retryable());
        assert!(!err.is_suspicious());
    }

    #[test]
    #[serial(token_validator_counter)]
    fn wrong_secret_error_is_suspicious() {
        let token = make_token("alice@test.com", "room-1", true, 600);
        let err = decode_room_token("wrong-secret", &token).unwrap_err();
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
    }

    #[test]
    #[serial(token_validator_counter)]
    fn garbage_token_error_is_suspicious() {
        let err = decode_room_token(TEST_SECRET, "not.a.jwt").unwrap_err();
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
    }

    // -- NATS subject injection tests --

    #[test]
    #[serial(token_validator_counter)]
    fn room_with_dots_is_rejected() {
        let token = make_token("alice@test.com", "room.sub.topic", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
    }

    #[test]
    #[serial(token_validator_counter)]
    fn room_with_wildcard_star_is_rejected() {
        let token = make_token("alice@test.com", "room-*", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn room_with_wildcard_gt_is_rejected() {
        let token = make_token("alice@test.com", "room->", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn room_with_spaces_is_rejected() {
        let token = make_token("alice@test.com", "room name", true, 600);
        let err = decode_room_token(TEST_SECRET, &token).unwrap_err();
        assert!(matches!(err, TokenError::UnsafeIdentifier { .. }));
    }

    #[test]
    #[serial(token_validator_counter)]
    fn room_with_valid_chars_is_accepted() {
        let token = make_token("alice@test.com", "My_Room-123", true, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().room, "My_Room-123");
    }

    #[test]
    fn unsafe_identifier_error_is_suspicious_and_not_retryable() {
        let err = TokenError::UnsafeIdentifier {
            field: "room".to_string(),
            value: "evil.>".to_string(),
        };
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
        assert_eq!(
            err.client_message(),
            "We detected unusual activity from your browser. This incident has been logged."
        );
    }

    // -- Phase 8b: granular variant classification (TELEM-7) --

    #[test]
    fn invalid_signature_variant_is_suspicious_and_not_retryable() {
        let err = TokenError::InvalidSignature;
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
        assert_eq!(
            err.client_message(),
            "We detected unusual activity from your browser. This incident has been logged."
        );
    }

    #[test]
    fn missing_claim_variant_is_not_suspicious_and_not_retryable() {
        // A missing-claim error means the token-issuer is misconfigured;
        // it's not a forgery signal so we don't treat it as suspicious.
        let err = TokenError::MissingClaim("exp".to_string());
        assert!(!err.is_suspicious());
        assert!(!err.is_retryable());
    }

    #[test]
    fn malformed_variant_is_suspicious() {
        let err = TokenError::Malformed("bad json".to_string());
        assert!(err.is_suspicious());
        assert!(!err.is_retryable());
    }

    // -- Phase 8b: TokenError::reason() label mapping (TELEM-7) --

    #[test]
    fn reason_token_expired() {
        assert_eq!(TokenError::Expired.reason(), "token_expired");
    }

    #[test]
    fn reason_invalid_signature() {
        assert_eq!(TokenError::InvalidSignature.reason(), "invalid_signature");
    }

    #[test]
    fn reason_missing_claim() {
        assert_eq!(
            TokenError::MissingClaim("exp".to_string()).reason(),
            "missing_claim"
        );
    }

    #[test]
    fn reason_malformed() {
        assert_eq!(
            TokenError::Malformed("bad header".to_string()).reason(),
            "malformed"
        );
    }

    #[test]
    fn reason_other_buckets_remaining_variants() {
        // Catch-all reason for variants that don't map to a more specific
        // alerting bucket. Keep this list tight so the cardinality of the
        // `reason` label stays bounded.
        assert_eq!(TokenError::Missing.reason(), "other");
        assert_eq!(
            TokenError::Invalid("InvalidIssuer".to_string()).reason(),
            "other"
        );
        assert_eq!(TokenError::RoomJoinDenied.reason(), "other");
        assert_eq!(
            TokenError::RoomMismatch {
                token_room: "a".to_string(),
                requested_room: "b".to_string(),
            }
            .reason(),
            "other"
        );
        assert_eq!(
            TokenError::IdentityMismatch {
                token_identity: "a@test.com".to_string(),
                requested_identity: "b@test.com".to_string(),
            }
            .reason(),
            "other"
        );
        assert_eq!(
            TokenError::UnsafeIdentifier {
                field: "room".to_string(),
                value: "evil.>".to_string()
            }
            .reason(),
            "other"
        );
    }

    // -- Phase 8b: AUTH_REJECTIONS_TOTAL counter wiring (AUTH-3) --
    //
    // The Prometheus counter is a process-global singleton. To get
    // deterministic deltas, every test in this module that calls
    // `decode_room_token` / `validate_room_token` carries the same
    // `#[serial(token_validator_counter)]` attribute, so at most one such
    // test runs at a time within this process.

    fn auth_counter(reason: &str) -> f64 {
        crate::metrics::AUTH_REJECTIONS_TOTAL
            .with_label_values(&[reason])
            .get()
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_token_expired_on_decode() {
        let before = auth_counter("token_expired");
        let token = make_token("alice@test.com", "room-1", true, -120);
        let _ = decode_room_token(TEST_SECRET, &token);
        let after = auth_counter("token_expired");
        assert_eq!(
            after - before,
            1.0,
            "expired-token rejection should increment token_expired counter exactly once"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_invalid_signature_on_decode() {
        let before = auth_counter("invalid_signature");
        let token = make_token("alice@test.com", "room-1", true, 600);
        let _ = decode_room_token("wrong-secret", &token);
        let after = auth_counter("invalid_signature");
        assert_eq!(
            after - before,
            1.0,
            "wrong-secret rejection should increment invalid_signature counter exactly once"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_malformed_on_garbage() {
        let before = auth_counter("malformed");
        let _ = decode_room_token(TEST_SECRET, "not.a.jwt");
        let after = auth_counter("malformed");
        assert_eq!(
            after - before,
            1.0,
            "garbage-token rejection should increment malformed counter exactly once"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_missing_claim_via_record_rejection() {
        // The `MissingClaim` variant is structurally unreachable from
        // `decode_room_token` with the current `RoomAccessTokenClaims`
        // struct (every field is required by serde, so a missing `sub` /
        // `exp` produces a `Json` error before jsonwebtoken's validate()
        // step runs). The variant is preserved so that future loosening
        // of the struct (`Option<String>`) immediately starts producing
        // `MissingClaim` instead of being misclassified as `malformed`.
        // This test exercises the wiring directly: build the variant,
        // call `record_rejection`, assert the labeled counter increments.
        let before = auth_counter("missing_claim");
        let err = TokenError::MissingClaim("sub".to_string());
        err.record_rejection();
        let after = auth_counter("missing_claim");
        assert_eq!(
            after - before,
            1.0,
            "MissingClaim::record_rejection should increment the missing_claim counter exactly once"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_other_for_room_join_denied() {
        let before = auth_counter("other");
        let token = make_token("alice@test.com", "room-1", false, 600);
        let _ = decode_room_token(TEST_SECRET, &token);
        let after = auth_counter("other");
        assert_eq!(
            after - before,
            1.0,
            "room_join=false rejection should increment `other` counter exactly once"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_does_not_increment_on_success() {
        // Regression guard: `inspect_err` must NOT fire on Ok(_). We sum
        // every reason label so that a wrongly-tagged increment anywhere in
        // the labelspace would still be detected.
        let reasons = [
            "token_expired",
            "invalid_signature",
            "missing_claim",
            "malformed",
            "other",
        ];
        let before: f64 = reasons.iter().map(|r| auth_counter(r)).sum();
        let token = make_token("alice@test.com", "room-1", true, 600);
        let result = decode_room_token(TEST_SECRET, &token);
        let after: f64 = reasons.iter().map(|r| auth_counter(r)).sum();
        assert!(result.is_ok());
        assert_eq!(
            after, before,
            "successful decode must not increment any auth-rejection counter"
        );
    }

    #[test]
    #[serial(token_validator_counter)]
    fn auth_counter_records_other_for_validate_room_token_mismatch() {
        // `validate_room_token` (deprecated path) detects RoomMismatch /
        // IdentityMismatch *after* a successful inner decode. Make sure the
        // counter still records those rejections — without double-counting
        // the inner decode.
        let before_other = auth_counter("other");
        let before_signature = auth_counter("invalid_signature");
        let token = make_token("alice@test.com", "room-A", true, 600);
        let result = validate_room_token(TEST_SECRET, &token, "room-B", "alice@test.com");
        let after_other = auth_counter("other");
        let after_signature = auth_counter("invalid_signature");
        assert!(matches!(result, Err(TokenError::RoomMismatch { .. })));
        assert_eq!(
            after_other - before_other,
            1.0,
            "RoomMismatch should bump `other` once"
        );
        assert_eq!(
            after_signature, before_signature,
            "RoomMismatch must not bump `invalid_signature`"
        );
    }
}
