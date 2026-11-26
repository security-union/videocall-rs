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

//! Local room-token minting for the load-generator bot.
//!
//! The relay only accepts `/lobby?token=<JWT>`, where the JWT is a room access
//! token signed with the deployment's `JWT_SECRET`. The bot is meant for
//! infrastructure you operate, so it signs its own tokens with that secret
//! instead of going through meeting-api's login and admission flow.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context};
use jsonwebtoken::{EncodingKey, Header};
use videocall_meeting_types::token::RoomAccessTokenClaims;

/// Environment variable holding the relay's HMAC secret.
pub const JWT_SECRET_ENV: &str = "JWT_SECRET";

/// Token lifetime. The relay only checks the token when the session opens, so
/// this just has to outlive the CONNECT, with room for clock skew.
const TOKEN_TTL_SECS: i64 = 300;

/// Signs room access tokens with the deployment's `JWT_SECRET`.
#[derive(Clone)]
pub struct RoomTokenSigner {
    key: EncodingKey,
}

impl fmt::Debug for RoomTokenSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoomTokenSigner { .. }")
    }
}

impl RoomTokenSigner {
    /// Build a signer from a secret. An empty secret is refused.
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        if secret.is_empty() {
            return Err(anyhow!("{JWT_SECRET_ENV} is empty"));
        }
        Ok(Self {
            key: EncodingKey::from_secret(secret.as_bytes()),
        })
    }

    /// Build a signer from the `JWT_SECRET` environment variable.
    ///
    /// The secret is read from the environment only, never from the YAML
    /// config, so it doesn't end up committed next to the client list.
    pub fn from_env() -> anyhow::Result<Self> {
        let secret = std::env::var(JWT_SECRET_ENV).map_err(|_| {
            anyhow!(
                "{JWT_SECRET_ENV} is not set. The bot signs its own room tokens and needs \
                 the same secret as the relay you are pointing it at."
            )
        })?;
        Self::new(&secret)
    }

    /// Mint a room token for `user_id` in `room`.
    ///
    /// Uses the same claims type the relay validates, with the issuer it
    /// expects and `room_join = true`.
    pub fn mint(&self, user_id: &str, room: &str) -> anyhow::Result<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_secs() as i64;
        let claims = RoomAccessTokenClaims {
            sub: user_id.to_string(),
            room: room.to_string(),
            room_join: true,
            is_host: false,
            display_name: user_id.to_string(),
            observer: false,
            exp: now + TOKEN_TTL_SECS,
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
        let signer = RoomTokenSigner::new("bot-test-secret").expect("signer");
        let token = signer.mint("bot001", "load-test").expect("mint");

        let mut validation = Validation::default();
        validation.set_required_spec_claims(&["exp", "sub"]);
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        let decoded = jsonwebtoken::decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(b"bot-test-secret"),
            &validation,
        )
        .expect("token verifies with the same secret");

        assert_eq!(decoded.claims.sub, "bot001");
        assert_eq!(decoded.claims.room, "load-test");
        assert!(decoded.claims.room_join);
        assert!(!decoded.claims.observer);
    }

    #[test]
    fn empty_secret_is_refused() {
        assert!(RoomTokenSigner::new("").is_err());
    }
}
