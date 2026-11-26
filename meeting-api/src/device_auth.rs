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

//! Device-JWT and registration-JWS verification. The `kid` header selects
//! the `devices` row whose key verifies the signature. The algorithm is
//! pinned to EdDSA here, never taken from the header, so HS256 session and
//! room tokens can never be confused with device tokens.

use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::VerifyingKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use videocall_meeting_types::device::{
    self, DeviceTokenClaims, RegistrationClaims, CLI_ISSUER, DEVICE_TOKEN_LEEWAY_SECS,
    DEVICE_TOKEN_MAX_LIFETIME_SECS,
};

use crate::db::devices::{self as db_devices, DeviceRow};
use crate::db::DbPool;

/// Longest token accepted before any parsing.
const MAX_DEVICE_TOKEN_LEN: usize = 2048;

/// Which device states a caller accepts. A revoked device is never accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedStates {
    LiveApproved,
    /// Lets an expired device revoke itself.
    AnyNotRevoked,
}

#[derive(Debug)]
pub enum DeviceAuthError {
    /// The reason is for logs only and never goes on the wire.
    Invalid(&'static str),
    Db(sqlx::Error),
}

impl std::fmt::Display for DeviceAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceAuthError::Invalid(reason) => write!(f, "invalid device token: {reason}"),
            DeviceAuthError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

/// True if the unverified header says `alg: EdDSA`. Routes a bearer token to
/// the device path; proves nothing.
pub fn is_device_token(token: &str) -> bool {
    token.len() <= MAX_DEVICE_TOKEN_LEN
        && decode_header(token).is_ok_and(|header| header.alg == Algorithm::EdDSA)
}

/// EdDSA only, issuer `videocall-cli`, the exact audience. `exp` is checked
/// by [`check_token_times`] against the caller's `now` instead (jsonwebtoken
/// would read the system clock), so always pair the two.
fn device_validation(expected_aud: &str, require_sub: bool) -> Validation {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[CLI_ISSUER]);
    validation.set_audience(&[expected_aud]);
    if require_sub {
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    } else {
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    }
    validation.leeway = DEVICE_TOKEN_LEEWAY_SECS as u64;
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation
}

/// `exp` and `iat` within the leeway of `now`, and `exp - iat` in
/// `1..=DEVICE_TOKEN_MAX_LIFETIME_SECS`. Checked arithmetic, so extreme
/// claim values cannot wrap around the bounds.
pub fn check_token_times(iat: i64, exp: i64, now: DateTime<Utc>) -> Result<(), &'static str> {
    let now = now.timestamp();
    let earliest_exp = now
        .checked_sub(DEVICE_TOKEN_LEEWAY_SECS)
        .ok_or("clock overflow")?;
    if exp < earliest_exp {
        return Err("expired");
    }
    let latest_iat = now
        .checked_add(DEVICE_TOKEN_LEEWAY_SECS)
        .ok_or("clock overflow")?;
    if iat > latest_iat {
        return Err("iat in the future");
    }
    match exp.checked_sub(iat) {
        Some(lifetime) if (1..=DEVICE_TOKEN_MAX_LIFETIME_SECS).contains(&lifetime) => Ok(()),
        Some(_) => Err("lifetime out of range"),
        None => Err("lifetime overflow"),
    }
}

/// 43 base64url chars, a valid Edwards point, not a small-order (weak) key.
pub fn validate_public_key(encoded: &str) -> Option<[u8; 32]> {
    let raw = device::decode_public_key(encoded)?;
    let key = VerifyingKey::from_bytes(&raw).ok()?;
    (!key.is_weak()).then_some(raw)
}

/// The well-formed `kid` of an unverified EdDSA header. Proves nothing; it
/// only selects the row whose key [`verify_device_token`] uses.
pub fn device_token_kid(token: &str) -> Result<String, DeviceAuthError> {
    if token.len() > MAX_DEVICE_TOKEN_LEN {
        return Err(DeviceAuthError::Invalid("token too long"));
    }
    let header = decode_header(token).map_err(|_| DeviceAuthError::Invalid("header"))?;
    if header.alg != Algorithm::EdDSA {
        return Err(DeviceAuthError::Invalid("not EdDSA"));
    }
    header
        .kid
        .filter(|kid| device::is_valid_device_id(kid))
        .ok_or(DeviceAuthError::Invalid("missing or malformed kid"))
}

/// Verify a device JWT against the stored key of device `kid`, with no
/// database: pinned EdDSA signature and claims, `sub == kid`, time bounds.
pub fn verify_device_token(
    token: &str,
    kid: &str,
    public_key_b64: &str,
    expected_aud: &str,
    now: DateTime<Utc>,
) -> Result<DeviceTokenClaims, DeviceAuthError> {
    // Only ever build a DecodingKey from exactly 32 bytes: the EdDSA verifier
    // slices [..32] and would panic on a shorter key.
    let raw =
        device::decode_public_key(public_key_b64).ok_or(DeviceAuthError::Invalid("stored key"))?;
    let claims = decode::<DeviceTokenClaims>(
        token,
        &DecodingKey::from_ed_der(&raw),
        &device_validation(expected_aud, true),
    )
    .map_err(|_| DeviceAuthError::Invalid("signature or claims"))?
    .claims;

    if claims.sub != kid {
        return Err(DeviceAuthError::Invalid("sub does not match kid"));
    }
    check_token_times(claims.iat, claims.exp, now).map_err(DeviceAuthError::Invalid)?;
    Ok(claims)
}

/// Verify a device JWT and return its row, checked against `allowed`.
pub async fn verify_device_jwt(
    db: &DbPool,
    token: &str,
    expected_aud: &str,
    now: DateTime<Utc>,
    allowed: AllowedStates,
) -> Result<DeviceRow, DeviceAuthError> {
    let kid = device_token_kid(token)?;
    let row = db_devices::find_device(db, &kid)
        .await
        .map_err(DeviceAuthError::Db)?
        .ok_or(DeviceAuthError::Invalid("unknown device"))?;
    verify_device_token(token, &kid, &row.public_key, expected_aud, now)?;
    if row.revoked_at.is_some() {
        return Err(DeviceAuthError::Invalid("revoked"));
    }
    if allowed == AllowedStates::LiveApproved && !row.is_live(now) {
        return Err(DeviceAuthError::Invalid("device expired"));
    }
    Ok(row)
}

/// Slide the idle expiry by the row's original idle span
/// (`idle_expires_at - last_used_at`), at most once an hour. Errors are
/// logged, not returned.
pub async fn touch_live_device(db: &DbPool, row: &DeviceRow, now: DateTime<Utc>) {
    if now - row.last_used_at < Duration::hours(1) {
        return;
    }
    let span = row.idle_expires_at - row.last_used_at;
    if span <= Duration::zero() {
        return;
    }
    if let Err(e) = db_devices::touch_device(db, &row.id, now, now + span).await {
        tracing::warn!(device_id = %row.id, "device touch failed: {e}");
    }
}

/// Why a registration JWS was rejected (logged only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationError {
    Malformed,
    PublicKey,
    Signature,
    Times,
    Name,
}

#[derive(Deserialize)]
struct PublicKeyOnly {
    public_key: String,
}

/// Verify the self-signed registration JWS: read `public_key` unverified,
/// validate it, then verify the token with that key. Returns the claims
/// (name normalized) and the raw key.
pub fn verify_registration(
    token: &str,
    audience: &str,
    now: DateTime<Utc>,
) -> Result<(RegistrationClaims, [u8; 32]), RegistrationError> {
    if token.len() > MAX_DEVICE_TOKEN_LEN {
        return Err(RegistrationError::Malformed);
    }
    let header = decode_header(token).map_err(|_| RegistrationError::Malformed)?;
    if header.alg != Algorithm::EdDSA {
        return Err(RegistrationError::Malformed);
    }
    let unverified = jsonwebtoken::dangerous::insecure_decode::<PublicKeyOnly>(token)
        .map_err(|_| RegistrationError::Malformed)?;
    let raw =
        validate_public_key(&unverified.claims.public_key).ok_or(RegistrationError::PublicKey)?;
    let mut claims = decode::<RegistrationClaims>(
        token,
        &DecodingKey::from_ed_der(&raw),
        &device_validation(audience, false),
    )
    .map_err(|_| RegistrationError::Signature)?
    .claims;
    if claims.public_key != unverified.claims.public_key {
        return Err(RegistrationError::Signature);
    }
    check_token_times(claims.iat, claims.exp, now).map_err(|_| RegistrationError::Times)?;
    claims.name = device::normalize_device_name(&claims.name).ok_or(RegistrationError::Name)?;
    Ok((claims, raw))
}

/// PKCE verifier: 43..=128 unreserved characters (RFC 7636 4.1).
pub fn is_valid_code_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

pub fn fingerprint_of(public_key_b64: &str) -> Option<String> {
    device::decode_public_key(public_key_b64).map(|raw| device::fingerprint(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    #[test]
    fn verifier_bounds() {
        assert!(is_valid_code_verifier(&format!("{}.~", "a".repeat(43))));
        assert!(!is_valid_code_verifier(&"a".repeat(42)));
        assert!(!is_valid_code_verifier(&format!("{}+", "a".repeat(43))));
    }

    #[test]
    fn token_time_bounds() {
        let now = Utc::now();
        let t = now.timestamp();
        assert!(check_token_times(t, t + 120, now).is_ok());
        assert!(check_token_times(t, t + 300, now).is_ok());
        assert!(check_token_times(t, t + 301, now).is_err());
        assert!(check_token_times(t + 60, t + 120, now).is_ok());
        assert!(check_token_times(t + 120, t + 240, now).is_err());
        // Overflow and non-positive lifetimes.
        assert!(check_token_times(i64::MIN, t + 120, now).is_err());
        assert!(check_token_times(i64::MIN, i64::MAX, now).is_err());
        assert!(check_token_times(t, t - 1, now).is_err());
        assert!(check_token_times(t, t, now).is_err());
        assert!(check_token_times(t, i64::MIN, now).is_err());
        assert!(check_token_times(t, t + 1, now).is_ok());
        // `exp` is checked against the given `now`, with the leeway.
        assert!(check_token_times(t - 150, t - 50, now).is_ok());
        assert!(check_token_times(t - 200, t - 61, now).is_err());
        assert!(check_token_times(t - 200, t - 100, now).is_err());
    }

    #[test]
    fn public_key_validation() {
        assert!(device::decode_public_key(&URL_SAFE_NO_PAD.encode([0u8; 31])).is_none());
        assert!(validate_public_key(&URL_SAFE_NO_PAD.encode([9u8; 31])).is_none());
        assert!(validate_public_key(&URL_SAFE_NO_PAD.encode([9u8; 33])).is_none());
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert!(validate_public_key(&URL_SAFE_NO_PAD.encode(identity)).is_none());
        assert!(validate_public_key(&URL_SAFE_NO_PAD.encode([0u8; 32])).is_none());
        let key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let encoded = URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
        assert!(validate_public_key(&encoded).is_some());
    }

    #[test]
    fn header_routing() {
        assert!(!is_device_token("garbage"));
        // {"alg":"HS256","typ":"JWT"}
        assert!(!is_device_token(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.e30.x"
        ));
        // {"alg":"EdDSA","typ":"JWT"}
        assert!(is_device_token(
            "eyJhbGciOiJFZERTQSIsInR5cCI6IkpXVCJ9.e30.x"
        ));
        // {"alg":"none"}
        assert!(!is_device_token("eyJhbGciOiJub25lIn0.e30."));
    }

    /// Everything `verify_device_jwt` checks except the row lookup and state.
    #[test]
    fn device_token_rejections() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        use ed25519_dalek::SigningKey;
        use jsonwebtoken::{encode, EncodingKey, Header};
        use serde_json::{json, Value};

        let key = SigningKey::from_bytes(&[7u8; 32]);
        let raw = key.verifying_key().to_bytes();
        let public_key = URL_SAFE_NO_PAD.encode(raw);
        let (kid, aud, now) = ("aB3_-aB3_-aB3_-aB3_-aB", "https://api.example", Utc::now());
        let t = now.timestamp();
        let claims = |edit: &dyn Fn(&mut Value)| {
            let mut claims = json!({"iss": CLI_ISSUER, "sub": kid, "aud": aud, "iat": t, "exp": t + 120, "jti": "j"});
            edit(&mut claims);
            claims
        };
        let sign = |key: &SigningKey, kid: Option<&str>, claims: Value| {
            let header = Header {
                kid: kid.map(str::to_string),
                ..Header::new(Algorithm::EdDSA)
            };
            let der = key.to_pkcs8_der().unwrap();
            encode(&header, &claims, &EncodingKey::from_ed_der(der.as_bytes())).unwrap()
        };
        let verify = |token: &str| {
            device_token_kid(token)
                .and_then(|kid| verify_device_token(token, &kid, &public_key, aud, now))
        };
        assert!(verify(&sign(&key, Some(kid), claims(&|_| {}))).is_ok());

        let hs256 = Header {
            kid: Some(kid.to_string()),
            ..Header::new(Algorithm::HS256)
        };
        let cases = [
            (
                "signed by another key",
                sign(
                    &SigningKey::from_bytes(&[8u8; 32]),
                    Some(kid),
                    claims(&|_| {}),
                ),
            ),
            (
                "aud mismatch",
                sign(
                    &key,
                    Some(kid),
                    claims(&|c| c["aud"] = json!("https://other.example")),
                ),
            ),
            (
                "kid != sub",
                sign(
                    &key,
                    Some(kid),
                    claims(&|c| c["sub"] = json!("x".repeat(22))),
                ),
            ),
            (
                "wrong issuer",
                sign(
                    &key,
                    Some(kid),
                    claims(&|c| c["iss"] = json!("someone-else")),
                ),
            ),
            (
                "expired",
                sign(
                    &key,
                    Some(kid),
                    claims(&|c| {
                        c["iat"] = json!(t - 400);
                        c["exp"] = json!(t - 280);
                    }),
                ),
            ),
            (
                "future iat",
                sign(
                    &key,
                    Some(kid),
                    claims(&|c| {
                        c["iat"] = json!(t + 120);
                        c["exp"] = json!(t + 240);
                    }),
                ),
            ),
            (
                "overlong lifetime",
                sign(&key, Some(kid), claims(&|c| c["exp"] = json!(t + 301))),
            ),
            ("malformed kid", sign(&key, Some("short"), claims(&|_| {}))),
            ("no kid", sign(&key, None, claims(&|_| {}))),
            (
                "HS256 with the public key",
                encode(&hs256, &claims(&|_| {}), &EncodingKey::from_secret(&raw)).unwrap(),
            ),
            (
                "alg none",
                format!(
                    "{}.{}.",
                    URL_SAFE_NO_PAD.encode(format!(r#"{{"alg":"none","kid":"{kid}"}}"#)),
                    URL_SAFE_NO_PAD.encode(claims(&|_| {}).to_string())
                ),
            ),
            ("oversized", "a".repeat(MAX_DEVICE_TOKEN_LEN + 1)),
        ];
        for (name, token) in cases {
            assert!(verify(&token).is_err(), "{name}");
        }
    }
}
