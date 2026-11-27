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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! The relay only accepts `/lobby?token=<JWT>`. The bot runs against
//! infrastructure you operate, so it signs its own room tokens with the
//! relay's `JWT_SECRET` instead of going through meeting-api's login. (The
//! only other minter, `meeting_api::token`, would pull axum and sqlx in.)

use anyhow::{anyhow, Context};
use jsonwebtoken::{EncodingKey, Header};
use videocall_meeting_types::token::RoomAccessTokenClaims;

/// The relay checks the token only when the session opens.
const TOKEN_TTL_SECS: i64 = 300;

#[derive(Clone)]
pub struct RoomTokenSigner {
    key: EncodingKey,
}

impl RoomTokenSigner {
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        if secret.is_empty() {
            return Err(anyhow!("JWT_SECRET is empty"));
        }
        Ok(Self {
            key: EncodingKey::from_secret(secret.as_bytes()),
        })
    }

    /// Read from the environment only, never from the YAML config, so the
    /// secret is not committed next to the client list.
    pub fn from_env() -> anyhow::Result<Self> {
        let secret = crate::constants::JWT_SECRET.clone().ok_or_else(|| {
            anyhow!(
                "JWT_SECRET is not set. The bot signs its own room tokens with the relay's secret."
            )
        })?;
        Self::new(&secret)
    }

    pub fn mint(&self, user_id: &str, room: &str) -> anyhow::Result<String> {
        let claims = RoomAccessTokenClaims {
            sub: user_id.to_string(),
            room: room.to_string(),
            room_join: true,
            is_host: false,
            display_name: user_id.to_string(),
            observer: false,
            exp: chrono::Utc::now().timestamp() + TOKEN_TTL_SECS,
            iss: RoomAccessTokenClaims::ISSUER.to_string(),
        };
        jsonwebtoken::encode(&Header::default(), &claims, &self.key)
            .context("failed to sign room token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation};

    #[test]
    fn minted_token_carries_relay_claims() {
        assert!(RoomTokenSigner::new("").is_err());
        let token = RoomTokenSigner::new("bot-test-secret")
            .and_then(|signer| signer.mint("bot001", "load-test"))
            .expect("mint");
        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let claims = jsonwebtoken::decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"bot-test-secret"),
            &validation,
        )
        .expect("token verifies with the same secret")
        .claims;
        assert_eq!(
            (claims.sub.as_str(), claims.room.as_str()),
            ("bot001", "load-test")
        );
        assert!(claims.room_join && !claims.observer && !claims.is_host);
    }
}
