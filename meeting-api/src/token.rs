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
use videocall_meeting_types::{check_token_type, RoomAccessTokenClaims, TokenTypeCheck};

use crate::error::AppError;

// ---------------------------------------------------------------------------
// Session token
// ---------------------------------------------------------------------------

/// Claims embedded in a session JWT (stored in an HttpOnly cookie).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionTokenClaims {
    /// User ID (the identity principal).
    pub sub: String,
    /// Display name.
    pub name: String,
    /// Expiration (Unix timestamp).
    pub exp: i64,
    /// Issued-at (Unix timestamp).
    pub iat: i64,
    /// Original authentication time (Unix timestamp), used to enforce the
    /// absolute session lifetime across sliding refreshes.
    #[serde(default)]
    pub auth_time: Option<i64>,
    /// Issuer.
    pub iss: String,
    /// Token-type discriminator (#2411). `None` means the token was minted
    /// before this claim existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
}

impl SessionTokenClaims {
    pub const ISSUER: &'static str = "videocall-meeting-backend";

    /// The expected `typ` value; must differ from [`RoomAccessTokenClaims::TOKEN_TYPE`].
    pub const TOKEN_TYPE: &'static str = "session";
}

#[derive(Deserialize)]
struct UnverifiedSessionExpiration {
    exp: i64,
}

/// Read only a session JWT's expiration without verifying its signature.
///
/// SECURITY: this value is an untrusted performance hint. Callers may use it
/// only to skip refresh work for a fresh-looking token; authorization and every
/// re-mint must still use [`decode_session_token`].
pub(crate) fn decode_session_exp_unverified(token: &str) -> Option<i64> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let mut parts = token.split('.');
    let header = parts.next()?;
    let claims = parts.next()?;
    let signature = parts.next()?;
    if header.is_empty() || claims.is_empty() || signature.is_empty() || parts.next().is_some() {
        return None;
    }

    let claims = URL_SAFE_NO_PAD.decode(claims).ok()?;
    serde_json::from_slice::<UnverifiedSessionExpiration>(&claims)
        .ok()
        .map(|claims| claims.exp)
}

/// Compute the session-cookie TTL clamped to the absolute lifetime cap.
///
/// The cap invariant (#1966) — "no valid session cookie may exist past
/// `auth_time + absolute_max_secs`" — must hold at EVERY mint, not only on
/// sliding refresh. This is the single source of truth used by both the
/// initial login/dev mints (where `auth_time == now`, so the result is
/// `min(session_ttl, absolute_max)`) and the refresh middleware (where an older
/// `auth_time` shrinks the remaining budget). Returns a value `<= session_ttl`;
/// callers MUST treat `<= 0` as "do not mint / force re-login".
pub fn capped_session_ttl(
    session_ttl_secs: i64,
    auth_time: i64,
    absolute_max_secs: i64,
    now: i64,
) -> i64 {
    let absolute_deadline = auth_time.saturating_add(absolute_max_secs);
    session_ttl_secs.min(absolute_deadline.saturating_sub(now))
}

/// Create a signed session JWT for the given user.
///
/// The token is later set inside an `HttpOnly` cookie by the OAuth callback
/// handler so that the browser sends it automatically with every request.
pub fn generate_session_token(
    secret: &str,
    user_id: &str,
    name: &str,
    ttl_secs: i64,
    auth_time: i64,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = SessionTokenClaims {
        sub: user_id.to_string(),
        name: name.to_string(),
        exp: now + ttl_secs,
        iat: now,
        auth_time: Some(auth_time),
        iss: SessionTokenClaims::ISSUER.to_string(),
        typ: Some(SessionTokenClaims::TOKEN_TYPE.to_string()),
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
///
/// `previous_secret` is the verify-only predecessor key (#2455). It is tried
/// only after `secret` fails, and it is never handed to
/// [`generate_session_token`] — every mint signs with `secret` alone. Pass
/// `None` to accept the current key only.
pub fn decode_session_token(
    secret: &str,
    previous_secret: Option<&str>,
    token: &str,
) -> Result<SessionTokenClaims, AppError> {
    let mut validation = Validation::default();
    validation.set_issuer(&[SessionTokenClaims::ISSUER]);

    let verify = |key: &str| {
        decode::<SessionTokenClaims>(
            token,
            &DecodingKey::from_secret(key.as_bytes()),
            &validation,
        )
        .map(|data| data.claims)
    };

    let claims = match verify(secret) {
        Ok(claims) => claims,
        Err(current_err) => match previous_secret.map(verify) {
            Some(Ok(claims)) => {
                tracing::info!("accepted session cookie signed with the previous key (#2455)");
                claims
            }
            _ => {
                tracing::warn!("Session JWT validation failed: {current_err}");
                return Err(AppError::unauthorized_msg("invalid or expired session"));
            }
        },
    };

    require_token_type(
        claims.typ.as_deref(),
        SessionTokenClaims::TOKEN_TYPE,
        "session",
    )?;

    Ok(claims)
}

/// Enforce the `typ` discriminator (#2411).
pub(crate) fn require_token_type(
    typ: Option<&str>,
    expected: &str,
    kind: &str,
) -> Result<(), AppError> {
    match check_token_type(typ, expected) {
        TokenTypeCheck::Match => Ok(()),
        TokenTypeCheck::Legacy => {
            tracing::warn!("accepted {kind} token with no 'typ' claim (pre-#2411 mint)");
            Ok(())
        }
        TokenTypeCheck::Mismatch { found } => {
            tracing::warn!("rejected token presented as {kind}: typ '{found}' != '{expected}'");
            Err(AppError::unauthorized_msg("token type mismatch"))
        }
    }
}

/// Decode any guest JWT - either an observer token or a room-access token.
/// This allows the `/leave-guest` endpoint to accept both the observer token
/// (issued while waiting) and the room token (issued after admission), so guests can always cleanly leave.
pub fn decode_guest_token(secret: &str, token: &str) -> Result<RoomAccessTokenClaims, AppError> {
    let mut validation = Validation::default();
    validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);

    let claims = decode::<RoomAccessTokenClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|data| data.claims)
    .map_err(|e| {
        tracing::warn!("Guest JWT validation failed: {e}");
        AppError::unauthorized_msg("invalid or expired guest token")
    })?;

    require_token_type(
        claims.typ.as_deref(),
        RoomAccessTokenClaims::TOKEN_TYPE,
        "guest room",
    )?;

    if !claims.is_guest {
        return Err(AppError::unauthorized_msg("token is not a guest token"));
    }

    Ok(claims)
}

// ---------------------------------------------------------------------------
// Room access token
// ---------------------------------------------------------------------------

/// Sign a room access token for the given participant.
// Keep this signature stable across multiple call sites; grouping into a struct
// would be a broader refactor with no behavioral benefit.
#[allow(clippy::too_many_arguments)]
pub fn generate_room_token(
    secret: &str,
    ttl_secs: i64,
    user_id: &str,
    room: &str,
    is_host: bool,
    display_name: &str,
    end_on_host_leave: bool,
    is_guest: bool,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = RoomAccessTokenClaims {
        sub: user_id.to_string(),
        room: room.to_string(),
        room_join: true,
        is_host,
        is_guest,
        display_name: display_name.to_string(),
        observer: false,
        end_on_host_leave,
        exp: now + ttl_secs,
        iss: RoomAccessTokenClaims::ISSUER.to_string(),
        typ: Some(RoomAccessTokenClaims::TOKEN_TYPE.to_string()),
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

/// Observer token TTL: 30 minutes. Users may wait a long time in the lobby
/// for the host to start the meeting or admit them.
const OBSERVER_TOKEN_TTL_SECS: i64 = 1800;

/// Sign an observer token for a participant who is waiting for meeting
/// activation or waiting-room admission. The token grants read-only access
/// to the media server so the client can receive push notifications
/// (e.g. MEETING_ACTIVATED, PARTICIPANT_ADMITTED) without polling.
pub fn generate_observer_token(
    secret: &str,
    user_id: &str,
    room: &str,
    display_name: &str,
    is_guest: bool,
) -> Result<String, AppError> {
    let now = Utc::now().timestamp();
    let claims = RoomAccessTokenClaims {
        sub: user_id.to_string(),
        room: room.to_string(),
        room_join: false,
        is_host: false,
        is_guest,
        display_name: display_name.to_string(),
        observer: true,
        end_on_host_leave: true,
        exp: now + OBSERVER_TOKEN_TTL_SECS,
        iss: RoomAccessTokenClaims::ISSUER.to_string(),
        typ: Some(RoomAccessTokenClaims::TOKEN_TYPE.to_string()),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| {
        tracing::error!("Failed to sign observer JWT: {e}");
        AppError::internal("failed to generate observer token")
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
    fn capped_session_ttl_clamps_to_absolute_max_at_initial_mint() {
        // #1966: the cap must bind at INITIAL mint (auth_time == now), not only
        // on sliding refresh. When session_ttl exceeds the absolute max, the
        // minted TTL is clamped to the max — otherwise the first cookie would
        // outlive the cap (the codex-review P1). This is the single helper both
        // the login/dev mints and the refresh middleware call.
        let now = 1_000_000;
        // session_ttl (10y) >> absolute_max (7d) → clamp to absolute_max.
        assert_eq!(
            capped_session_ttl(315_360_000, now, 604_800, now),
            604_800,
            "initial mint must clamp to the absolute cap, not the raw session TTL"
        );
        // session_ttl <= absolute_max → unchanged.
        assert_eq!(capped_session_ttl(28_800, now, 604_800, now), 28_800);
        // Mid-slide: an older auth_time shrinks the remaining budget.
        assert_eq!(
            capped_session_ttl(28_800, now - 600_000, 604_800, now),
            4_800,
            "remaining budget = auth_time + max - now"
        );
        // Past the cap → non-positive (callers force re-login).
        assert!(capped_session_ttl(28_800, now - 604_801, 604_800, now) <= 0);

        // ttl == max is INERT sliding (#1966 review): a fresh mint (auth_time
        // == now) yields ttl, but any later re-mint lands on the SAME deadline
        // (auth_time + max), so the session never extends. Deploys must set
        // max > ttl for sliding to do anything. This assertion documents the
        // constraint so a config that collapses them is a conscious choice.
        assert_eq!(
            capped_session_ttl(28_800, now, 28_800, now),
            28_800,
            "ttl==max: fresh mint gets ttl, but re-mint can't extend (inert sliding)"
        );
        assert_eq!(
            capped_session_ttl(28_800, now - 3_600, 28_800, now),
            25_200,
            "ttl==max, 1h in: re-mint lands on the original deadline, not now+ttl"
        );
    }

    #[test]
    fn capped_session_ttl_saturates_absolute_deadline() {
        let auth_time = i64::MAX - 10;
        let now = i64::MAX - 20;

        assert_eq!(
            capped_session_ttl(3_600, auth_time, 100, now),
            20,
            "overflowing auth_time + absolute_max must saturate at i64::MAX"
        );
    }

    #[test]
    fn session_token_round_trips() {
        let token = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600, 1234)
            .expect("should sign");
        let claims = decode_session_token(TEST_SECRET, None, &token).expect("should decode");

        assert_eq!(claims.sub, "alice@test.com");
        assert_eq!(claims.name, "Alice");
        assert_eq!(claims.auth_time, Some(1234));
        assert_eq!(claims.iss, SessionTokenClaims::ISSUER);
    }

    #[test]
    fn session_token_wrong_secret_fails() {
        let token =
            generate_session_token(TEST_SECRET, "a@b.com", "A", 3600, 1234).expect("should sign");
        let err = decode_session_token("wrong-secret", None, &token);
        assert!(err.is_err());
    }

    #[test]
    fn session_token_expired_fails() {
        // Use a TTL of -120s to exceed jsonwebtoken's default 60s leeway.
        let token =
            generate_session_token(TEST_SECRET, "a@b.com", "A", -120, 1234).expect("should sign");
        let err = decode_session_token(TEST_SECRET, None, &token);
        assert!(err.is_err());
    }

    #[test]
    fn session_token_has_iat() {
        let before = Utc::now().timestamp();
        let token =
            generate_session_token(TEST_SECRET, "a@b.com", "A", 3600, 1234).expect("should sign");
        let after = Utc::now().timestamp();

        let claims = decode_session_token(TEST_SECRET, None, &token).expect("should decode");
        assert!(claims.iat >= before);
        assert!(claims.iat <= after);
    }

    fn sign_raw(claims: &serde_json::Value) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(TEST_SECRET.as_bytes()),
        )
        .expect("should sign")
    }

    fn session_shaped(typ: Option<&str>) -> serde_json::Value {
        let now = Utc::now().timestamp();
        let mut claims = serde_json::json!({
            "sub": "attacker@example.com",
            "name": "Attacker",
            "iat": now,
            "exp": now + 3600,
            "iss": SessionTokenClaims::ISSUER,
        });
        if let Some(typ) = typ {
            claims["typ"] = serde_json::json!(typ);
        }
        claims
    }

    fn room_shaped(typ: Option<&str>) -> serde_json::Value {
        let now = Utc::now().timestamp();
        let mut claims = serde_json::json!({
            "sub": "guest:abc-123",
            "room": "victim-room",
            "room_join": true,
            "is_host": true,
            "is_guest": true,
            "display_name": "Attacker",
            "exp": now + 3600,
            "iss": RoomAccessTokenClaims::ISSUER,
        });
        if let Some(typ) = typ {
            claims["typ"] = serde_json::json!(typ);
        }
        claims
    }

    #[test]
    fn a_room_typed_token_is_refused_by_the_session_path_on_the_discriminator() {
        let token = sign_raw(&session_shaped(Some(RoomAccessTokenClaims::TOKEN_TYPE)));

        let mut validation = Validation::default();
        validation.set_issuer(&[SessionTokenClaims::ISSUER]);
        decode::<SessionTokenClaims>(
            &token,
            &DecodingKey::from_secret(TEST_SECRET.as_bytes()),
            &validation,
        )
        .expect("the blob must satisfy SessionTokenClaims, else this tests serde, not typ");

        let err = decode_session_token(TEST_SECRET, None, &token)
            .expect_err("a room-typed token must not be accepted as a session cookie");
        assert_eq!(
            err.body.engineering_error.as_deref(),
            Some("token type mismatch")
        );
    }

    #[test]
    fn the_same_session_blob_with_the_session_typ_is_accepted() {
        // Control: byte-identical apart from `typ`.
        let claims = decode_session_token(
            TEST_SECRET,
            None,
            &sign_raw(&session_shaped(Some(SessionTokenClaims::TOKEN_TYPE))),
        )
        .expect("must accept");

        assert_eq!(claims.sub, "attacker@example.com");
    }

    #[test]
    fn a_legacy_session_token_without_typ_is_still_accepted() {
        let claims = decode_session_token(TEST_SECRET, None, &sign_raw(&session_shaped(None)))
            .expect("pre-#2411 cookies must keep working");

        assert_eq!(claims.typ, None);
    }

    #[test]
    fn a_session_typed_token_is_refused_by_the_room_path_on_the_discriminator() {
        let token = sign_raw(&room_shaped(Some(SessionTokenClaims::TOKEN_TYPE)));

        let mut validation = Validation::default();
        validation.set_issuer(&[RoomAccessTokenClaims::ISSUER]);
        decode::<RoomAccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(TEST_SECRET.as_bytes()),
            &validation,
        )
        .expect("the blob must satisfy RoomAccessTokenClaims, else this tests serde, not typ");

        let err = decode_guest_token(TEST_SECRET, &token)
            .expect_err("a session-typed token must not be accepted as a room token");
        assert_eq!(
            err.body.engineering_error.as_deref(),
            Some("token type mismatch")
        );
    }

    #[test]
    fn the_same_room_blob_with_the_room_typ_is_accepted() {
        let claims = decode_guest_token(
            TEST_SECRET,
            &sign_raw(&room_shaped(Some(RoomAccessTokenClaims::TOKEN_TYPE))),
        )
        .expect("must accept");

        assert_eq!(claims.sub, "guest:abc-123");
    }

    #[test]
    fn a_legacy_guest_room_token_without_typ_is_still_accepted() {
        let claims = decode_guest_token(TEST_SECRET, &sign_raw(&room_shaped(None)))
            .expect("pre-#2411 room tokens must keep working");

        assert_eq!(claims.typ, None);
    }

    #[test]
    fn every_mint_path_stamps_its_own_typ() {
        let session = generate_session_token(TEST_SECRET, "a@b.com", "A", 3600, 1234).unwrap();
        assert_eq!(
            decode_session_token(TEST_SECRET, None, &session)
                .unwrap()
                .typ,
            Some(SessionTokenClaims::TOKEN_TYPE.to_string())
        );

        let room =
            generate_room_token(TEST_SECRET, 600, "guest:1", "r", false, "G", true, true).unwrap();
        assert_eq!(
            decode_guest_token(TEST_SECRET, &room).unwrap().typ,
            Some(RoomAccessTokenClaims::TOKEN_TYPE.to_string())
        );

        let observer = generate_observer_token(TEST_SECRET, "guest:1", "r", "G", true).unwrap();
        assert_eq!(
            decode_guest_token(TEST_SECRET, &observer).unwrap().typ,
            Some(RoomAccessTokenClaims::TOKEN_TYPE.to_string())
        );
    }
    // -----------------------------------------------------------------------
    // Verify-only previous-key window (#2455)
    // -----------------------------------------------------------------------

    const PREVIOUS_SECRET: &str = "outgoing-session-key";

    fn sign_raw_with(secret: &str, claims: &serde_json::Value) -> String {
        encode(
            &Header::default(),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("should sign")
    }

    #[test]
    fn a_previous_key_cookie_verifies_only_when_the_window_is_configured() {
        let token = generate_session_token(PREVIOUS_SECRET, "alice@test.com", "Alice", 3600, 1234)
            .expect("should sign");

        assert!(
            decode_session_token(TEST_SECRET, None, &token).is_err(),
            "with no window configured, a predecessor-key cookie must be rejected"
        );

        let claims = decode_session_token(TEST_SECRET, Some(PREVIOUS_SECRET), &token)
            .expect("with the window configured, a predecessor-key cookie must verify");
        assert_eq!(claims.sub, "alice@test.com");
    }

    #[test]
    fn an_open_window_admits_only_the_one_configured_previous_key() {
        let forged = generate_session_token("some-unrelated-key", "mallory@test.com", "M", 3600, 1)
            .expect("should sign");

        assert!(
            decode_session_token(TEST_SECRET, Some(PREVIOUS_SECRET), &forged).is_err(),
            "an open window must not widen acceptance beyond the configured predecessor"
        );
    }

    #[test]
    fn a_previous_key_cookie_is_still_subject_to_the_typ_gate() {
        let err = decode_session_token(
            TEST_SECRET,
            Some(PREVIOUS_SECRET),
            &sign_raw_with(
                PREVIOUS_SECRET,
                &session_shaped(Some(RoomAccessTokenClaims::TOKEN_TYPE)),
            ),
        )
        .expect_err("a room-typed token must not ride in through the previous key");
        assert_eq!(
            err.body.engineering_error.as_deref(),
            Some("token type mismatch")
        );

        // Control: same blob and same predecessor key, session `typ`.
        let claims = decode_session_token(
            TEST_SECRET,
            Some(PREVIOUS_SECRET),
            &sign_raw_with(
                PREVIOUS_SECRET,
                &session_shaped(Some(SessionTokenClaims::TOKEN_TYPE)),
            ),
        )
        .expect("must accept");
        assert_eq!(claims.sub, "attacker@example.com");
    }

    // -----------------------------------------------------------------------
    // Room access token tests
    // -----------------------------------------------------------------------

    #[test]
    fn token_round_trips_with_correct_claims() {
        let token = generate_room_token(
            TEST_SECRET,
            600,
            "user@test.com",
            "room-42",
            true,
            "Alice",
            true,
            false,
        )
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
        let token =
            generate_room_token(TEST_SECRET, 300, "a@b.com", "r", false, "Bob", false, false)
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
        let token = generate_room_token(TEST_SECRET, ttl, "a@b.com", "r", false, "X", false, false)
            .expect("should sign");
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
        let token = generate_room_token(TEST_SECRET, 60, "a@b.com", "r", false, "X", false, false)
            .expect("should sign");

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
