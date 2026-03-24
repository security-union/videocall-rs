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
    /// The `sub` claim does not match the user ID/identity in the connection URL.
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

impl TokenError {
    /// Whether this error represents a potentially tampered or forged token
    /// (as opposed to a benign expiration or missing token).
    pub fn is_suspicious(&self) -> bool {
        matches!(self, TokenError::Invalid(_))
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
