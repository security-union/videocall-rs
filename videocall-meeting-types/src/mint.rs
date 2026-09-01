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

//! Room-access-token minting and relay lobby URL construction for the headless
//! in-repo tools (`bot`, `vcprobe`, `videocall-cli`).
//!
//! Browsers obtain a room token from the Meeting Backend after an OIDC login.
//! The headless tools have no browser: they present the same
//! [`RoomAccessTokenClaims`] shape signed with the relay's shared `JWT_SECRET`.

use crate::token::RoomAccessTokenClaims;
use jsonwebtoken::{encode, EncodingKey, Header};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use std::fmt;
use std::time::{SystemTime, SystemTimeError, UNIX_EPOCH};

/// Characters that must be percent-encoded in a URL path segment (RFC 3986).
const PATH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'/')
    .add(b'?')
    .add(b'#')
    .add(b'[')
    .add(b']')
    .add(b'@')
    .add(b'!')
    .add(b'$')
    .add(b'&')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b'+')
    .add(b',')
    .add(b';')
    .add(b'=')
    .add(b'%');

/// Failure to produce an authenticated lobby URL.
#[derive(Debug)]
pub enum MintError {
    /// The system clock is before the Unix epoch, so `exp` cannot be computed.
    Clock(SystemTimeError),
    /// JWT serialization or signing failed.
    Encode(jsonwebtoken::errors::Error),
    /// Neither a token nor a signing secret was configured, and the caller did
    /// not opt in to the deprecated unauthenticated path.
    NoCredential,
}

impl fmt::Display for MintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MintError::Clock(e) => write!(f, "system clock is before the Unix epoch: {e}"),
            MintError::Encode(e) => write!(f, "failed to sign room access token: {e}"),
            MintError::NoCredential => write!(
                f,
                "no room access token and no JWT secret configured — supply a token or a secret \
                 to join with `?token=`, or explicitly opt in to the deprecated unauthenticated \
                 `/lobby/{{user_id}}/{{room}}` path"
            ),
        }
    }
}

impl std::error::Error for MintError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MintError::Clock(e) => Some(e),
            MintError::Encode(e) => Some(e),
            MintError::NoCredential => None,
        }
    }
}

/// Sign a room access token that the relay's `decode_room_token` accepts.
pub fn mint_room_token(
    jwt_secret: &str,
    user_id: &str,
    room: &str,
    ttl_secs: u64,
) -> Result<String, MintError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(MintError::Clock)?
        .as_secs();

    let claims = RoomAccessTokenClaims {
        sub: user_id.to_string(),
        room: room.to_string(),
        room_join: true,
        is_host: false,
        is_guest: false,
        display_name: user_id.to_string(),
        observer: false,
        end_on_host_leave: true,
        exp: (now + ttl_secs) as i64,
        iss: RoomAccessTokenClaims::ISSUER.to_string(),
        typ: Some(RoomAccessTokenClaims::TOKEN_TYPE.to_string()),
    };

    // The relay treats the secret as raw UTF-8 bytes (not base64-decoded).
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .map_err(MintError::Encode)
}

/// How a headless tool authenticates its lobby connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LobbyAuth {
    /// A token minted elsewhere (e.g. copied from a browser session).
    Token(String),
    /// Mint a fresh token per connection attempt with the relay's shared secret.
    Secret { secret: String, ttl_secs: u64 },
    /// **DEPRECATED**: unauthenticated `/lobby/{user_id}/{room}`. Only reachable
    /// when a caller explicitly opts in, and rejected by the relay whenever
    /// `FEATURE_MEETING_MANAGEMENT` is on.
    DeprecatedPath,
}

/// Pick the authentication mode for a tool, preferring token auth.
///
/// `allow_deprecated_path` is consulted only after both credential sources are
/// found absent, so a configured secret can never be downgraded to an
/// unauthenticated join.
pub fn resolve_lobby_auth(
    token: Option<String>,
    secret: Option<String>,
    ttl_secs: u64,
    allow_deprecated_path: bool,
) -> Result<LobbyAuth, MintError> {
    if let Some(token) = token.filter(|t| !t.trim().is_empty()) {
        return Ok(LobbyAuth::Token(token));
    }
    if let Some(secret) = secret.filter(|s| !s.trim().is_empty()) {
        return Ok(LobbyAuth::Secret { secret, ttl_secs });
    }
    if allow_deprecated_path {
        return Ok(LobbyAuth::DeprecatedPath);
    }
    Err(MintError::NoCredential)
}

/// Build the relay lobby URL for `auth`.
///
/// The [`LobbyAuth::Secret`] arm mints on every call, so callers that reconnect
/// should call this per attempt rather than reusing an earlier URL.
pub fn build_lobby_url(
    base_url: &str,
    auth: &LobbyAuth,
    user_id: &str,
    room: &str,
) -> Result<String, MintError> {
    let base = base_url.trim_end_matches('/');
    Ok(match auth {
        LobbyAuth::Token(token) => format!("{base}/lobby?token={token}"),
        LobbyAuth::Secret { secret, ttl_secs } => {
            let token = mint_room_token(secret, user_id, room, *ttl_secs)?;
            format!("{base}/lobby?token={token}")
        }
        LobbyAuth::DeprecatedPath => {
            let user = utf8_percent_encode(user_id, PATH_SEGMENT_ENCODE_SET);
            let room = utf8_percent_encode(room, PATH_SEGMENT_ENCODE_SET);
            format!("{base}/lobby/{user}/{room}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{check_token_type, TokenTypeCheck};
    use jsonwebtoken::{decode, DecodingKey, Validation};

    fn decode_claims(token: &str, secret: &[u8]) -> RoomAccessTokenClaims {
        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        decode::<RoomAccessTokenClaims>(token, &DecodingKey::from_secret(secret), &validation)
            .expect("relay-shaped token should decode")
            .claims
    }

    #[test]
    fn mint_room_token_sets_the_claims_the_relay_requires() {
        let token = mint_room_token("secret", "bot-1", "room-123", 300).unwrap();
        let claims = decode_claims(&token, b"secret");

        assert_eq!(claims.sub, "bot-1");
        assert_eq!(claims.room, "room-123");
        assert!(claims.room_join);
        assert!(!claims.is_host);
        assert!(!claims.is_guest);
        assert!(!claims.observer);
        assert_eq!(claims.display_name, "bot-1");
        assert_eq!(claims.iss, RoomAccessTokenClaims::ISSUER);
    }

    #[test]
    fn minted_token_passes_the_media_servers_type_gate() {
        let token = mint_room_token("secret", "bot-1", "room-123", 300).unwrap();
        let claims = decode_claims(&token, b"secret");

        assert_eq!(
            check_token_type(claims.typ.as_deref(), RoomAccessTokenClaims::TOKEN_TYPE),
            TokenTypeCheck::Match,
            "a minted room token must carry typ; Legacy here increments \
             videocall_legacy_token_type_accepted_total forever and typ can never be required"
        );
    }

    #[test]
    fn mint_room_token_matches_the_deprecated_paths_end_on_host_leave() {
        let token = mint_room_token("secret", "bot-1", "room-123", 300).unwrap();
        assert!(decode_claims(&token, b"secret").end_on_host_leave);
    }

    #[test]
    fn mint_room_token_applies_ttl_to_expiry() {
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let token = mint_room_token("secret", "bot-1", "room-123", 120).unwrap();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let exp = decode_claims(&token, b"secret").exp;
        assert!(exp >= before + 120);
        assert!(exp <= after + 120);
    }

    #[test]
    fn resolve_lobby_auth_prefers_an_explicit_token() {
        let auth =
            resolve_lobby_auth(Some("pre-minted".into()), Some("secret".into()), 60, true).unwrap();
        assert_eq!(auth, LobbyAuth::Token("pre-minted".into()));
    }

    #[test]
    fn resolve_lobby_auth_prefers_a_secret_over_the_deprecated_path() {
        let auth = resolve_lobby_auth(None, Some("secret".into()), 60, true).unwrap();
        assert_eq!(
            auth,
            LobbyAuth::Secret {
                secret: "secret".into(),
                ttl_secs: 60
            }
        );
    }

    #[test]
    fn resolve_lobby_auth_fails_loudly_without_a_credential() {
        let err = resolve_lobby_auth(None, None, 60, false).unwrap_err();
        assert!(matches!(err, MintError::NoCredential));
    }

    #[test]
    fn resolve_lobby_auth_treats_blank_credentials_as_absent() {
        let err = resolve_lobby_auth(Some("  ".into()), Some("".into()), 60, false).unwrap_err();
        assert!(matches!(err, MintError::NoCredential));

        let auth = resolve_lobby_auth(Some(" ".into()), Some(" ".into()), 60, true).unwrap();
        assert_eq!(auth, LobbyAuth::DeprecatedPath);
    }

    #[test]
    fn resolve_lobby_auth_returns_the_deprecated_path_only_on_opt_in() {
        let auth = resolve_lobby_auth(None, None, 60, true).unwrap();
        assert_eq!(auth, LobbyAuth::DeprecatedPath);
    }

    #[test]
    fn build_lobby_url_puts_a_minted_token_in_the_query() {
        let auth = LobbyAuth::Secret {
            secret: "secret".into(),
            ttl_secs: 60,
        };
        let url = build_lobby_url("https://relay.example.com/", &auth, "bot-1", "room-1").unwrap();

        let token = url
            .strip_prefix("https://relay.example.com/lobby?token=")
            .unwrap_or_else(|| panic!("expected an authenticated lobby URL, got {url}"));
        let claims = decode_claims(token, b"secret");
        assert_eq!(claims.sub, "bot-1");
        assert_eq!(claims.room, "room-1");
    }

    #[test]
    fn build_lobby_url_preserves_a_base_path_and_port() {
        let auth = LobbyAuth::Token("tok".into());
        let url = build_lobby_url("https://relay.example.com:4443/base", &auth, "u", "r").unwrap();
        assert_eq!(url, "https://relay.example.com:4443/base/lobby?token=tok");
    }

    #[test]
    fn build_lobby_url_encodes_deprecated_path_segments() {
        let url = build_lobby_url(
            "https://relay.example.com",
            &LobbyAuth::DeprecatedPath,
            "user/admin",
            "room?id=1#top",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://relay.example.com/lobby/user%2Fadmin/room%3Fid%3D1%23top"
        );
    }
}
