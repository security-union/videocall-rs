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
//! relay's `JWT_SECRET` instead of going through meeting-api's login, using
//! the same signer as meeting-api.

use anyhow::{anyhow, Context};
use videocall_meeting_types::token::generate_room_token;

/// The relay checks the token only when the session opens.
const TOKEN_TTL_SECS: i64 = 300;

#[derive(Clone)]
pub struct RoomTokenSigner {
    secret: String,
}

impl RoomTokenSigner {
    pub fn new(secret: &str) -> anyhow::Result<Self> {
        if secret.is_empty() {
            return Err(anyhow!("JWT_SECRET is empty"));
        }
        Ok(Self {
            secret: secret.to_string(),
        })
    }

    /// Read from the environment only, never from the YAML config, so the
    /// secret is not committed next to the client list.
    pub fn from_env() -> anyhow::Result<Self> {
        let secret = std::env::var("JWT_SECRET").map_err(|_| {
            anyhow!(
                "JWT_SECRET is not set. The bot signs its own room tokens with the relay's secret."
            )
        })?;
        Self::new(&secret)
    }

    pub fn mint(&self, user_id: &str, room: &str) -> anyhow::Result<String> {
        generate_room_token(&self.secret, TOKEN_TTL_SECS, user_id, room, false, user_id)
            .context("failed to sign room token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_meeting_types::device::base64url_decode;
    use videocall_meeting_types::RoomAccessTokenClaims;

    #[test]
    fn minted_token_carries_the_bot_claims() {
        assert!(RoomTokenSigner::new("").is_err());
        let token = RoomTokenSigner::new("bot-test-secret")
            .and_then(|signer| signer.mint("bot001", "load-test"))
            .expect("mint");
        let payload = token.split('.').nth(1).and_then(base64url_decode);
        // JSON is YAML, so the bot's YAML parser reads the payload.
        let claims: RoomAccessTokenClaims =
            serde_yaml::from_slice(&payload.expect("payload")).expect("claims");
        assert_eq!(
            (claims.sub.as_str(), claims.room.as_str()),
            ("bot001", "load-test")
        );
        assert!(!claims.is_host);
    }
}
