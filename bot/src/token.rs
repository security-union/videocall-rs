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

//! JWT token minting for bot clients.
//!
//! Produces tokens compatible with the Media Server's `decode_room_token()`
//! validator (HMAC-SHA256, issuer = "videocall-meeting-backend").

use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// JWT claims matching `RoomAccessTokenClaims` in videocall-meeting-types.
#[derive(Debug, Serialize, Deserialize)]
pub struct RoomAccessTokenClaims {
    pub sub: String,
    pub room: String,
    pub room_join: bool,
    pub is_host: bool,
    pub display_name: String,
    pub observer: bool,
    pub exp: i64,
    pub iss: String,
}

/// Mint a JWT for a bot client to join a room.
pub fn mint_token(
    jwt_secret: &str,
    user_id: &str,
    meeting_id: &str,
    ttl_secs: u64,
) -> anyhow::Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    let claims = RoomAccessTokenClaims {
        sub: user_id.to_string(),
        room: meeting_id.to_string(),
        room_join: true,
        is_host: false,
        display_name: user_id.to_string(),
        observer: false,
        exp: (now + ttl_secs) as i64,
        iss: "videocall-meeting-backend".to_string(),
    };

    // The secret is used as raw UTF-8 bytes (not base64-decoded), matching
    // the server's JwtDecoder which also treats the secret string as-is.
    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )?;

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::{mint_token, RoomAccessTokenClaims};
    use jsonwebtoken::{decode, DecodingKey, Validation};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn mint_token_sets_expected_claims() {
        let token = mint_token("secret", "bot-1", "room-123", 300).unwrap();
        let decoded = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"secret"),
            &Validation::default(),
        )
        .unwrap();

        assert_eq!(decoded.claims.sub, "bot-1");
        assert_eq!(decoded.claims.room, "room-123");
        assert!(decoded.claims.room_join);
        assert!(!decoded.claims.is_host);
        assert_eq!(decoded.claims.display_name, "bot-1");
        assert!(!decoded.claims.observer);
        assert_eq!(decoded.claims.iss, "videocall-meeting-backend");
    }

    #[test]
    fn mint_token_applies_ttl_to_expiry() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let token = mint_token("secret", "bot-1", "room-123", 120).unwrap();
        let decoded = decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"secret"),
            &Validation::default(),
        )
        .unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        assert!(decoded.claims.exp >= before + 120);
        assert!(decoded.claims.exp <= after + 120);
    }
}
