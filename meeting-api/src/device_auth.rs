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

//! CLI device keys: device-JWT verification, the registration JWS, and the
//! input validators of the `aws login`-style flow.
//!
//! A CLI device signs a short-lived EdDSA JWT for every request. The `kid`
//! header selects the `devices` row; the row's public key verifies the
//! signature. The algorithm is fixed here, never taken from the header, so
//! HS256 session/room tokens and device tokens can never be confused.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::VerifyingKey;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use videocall_meeting_types::device::{
    self, DeviceTokenClaims, RegistrationClaims, CLI_ISSUER, CLI_REMOTE_CODE_PATH,
    DEVICE_TOKEN_LEEWAY_SECS, DEVICE_TOKEN_MAX_LIFETIME_SECS,
};

use crate::db::devices::{self as db_devices, DeviceRow};
use crate::db::DbPool;

/// Longest token accepted before any parsing.
pub const MAX_DEVICE_TOKEN_LEN: usize = 2048;

/// Which device states a caller accepts. A revoked device is never accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedStates {
    /// Not revoked and inside its idle and absolute lifetime.
    LiveApproved,
    /// `DELETE /api/v1/devices/me`: any device that is not revoked, even an
    /// expired one, may revoke itself.
    AnyNotRevoked,
}

/// Why a device token was rejected.
#[derive(Debug)]
pub enum DeviceAuthError {
    /// The header `alg` is not EdDSA.
    NotADeviceToken,
    /// Rejected. The reason is for logs only and never goes on the wire.
    Invalid(&'static str),
    /// Database failure while looking up the device.
    Db(sqlx::Error),
}

impl std::fmt::Display for DeviceAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceAuthError::NotADeviceToken => write!(f, "not a device token"),
            DeviceAuthError::Invalid(reason) => write!(f, "invalid device token: {reason}"),
            DeviceAuthError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for DeviceAuthError {}

/// True if the unverified header says `alg: EdDSA`. Used to route a bearer
/// token to the device path; it proves nothing about the token.
pub fn is_device_token(token: &str) -> bool {
    token.len() <= MAX_DEVICE_TOKEN_LEN
        && decode_header(token).is_ok_and(|header| header.alg == Algorithm::EdDSA)
}

/// The [`Validation`] for device and registration tokens: EdDSA only, issuer
/// `videocall-cli`, the exact audience, 60 s leeway, no `nbf`. `require_sub`
/// is false only for the registration JWS.
pub fn device_validation(expected_aud: &str, require_sub: bool) -> Validation {
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.set_issuer(&[CLI_ISSUER]);
    validation.set_audience(&[expected_aud]);
    if require_sub {
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    } else {
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
    }
    validation.leeway = DEVICE_TOKEN_LEEWAY_SECS as u64;
    validation.validate_exp = true;
    validation.validate_nbf = false;
    validation
}

/// `iat` not in the future beyond the leeway, and a lifetime of at most
/// [`DEVICE_TOKEN_MAX_LIFETIME_SECS`].
pub fn check_token_times(iat: i64, exp: i64, now: DateTime<Utc>) -> Result<(), &'static str> {
    if iat > now.timestamp() + DEVICE_TOKEN_LEEWAY_SECS {
        return Err("iat in the future");
    }
    if exp - iat > DEVICE_TOKEN_MAX_LIFETIME_SECS {
        return Err("lifetime too long");
    }
    Ok(())
}

/// Decode a stored base64url public key to its 32 raw bytes.
pub fn public_key_raw(public_key_b64: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(public_key_b64).ok()?.try_into().ok()
}

/// Validate an encoded public key (contract 2.3): 43 base64url chars, 32
/// bytes, a valid Edwards point, not a small-order (weak) key.
pub fn validate_public_key(encoded: &str) -> Option<[u8; 32]> {
    if !device::is_valid_public_key_format(encoded) {
        return None;
    }
    let raw = public_key_raw(encoded)?;
    let key = VerifyingKey::from_bytes(&raw).ok()?;
    (!key.is_weak()).then_some(raw)
}

/// Verify a device JWT and return the device row.
///
/// Length cap; unverified header must be EdDSA with a well-formed `kid`;
/// primary-key lookup; signature, issuer, audience and expiry with the
/// algorithm pinned to EdDSA; `sub == kid`; `iat`/lifetime bounds; then the
/// row state against `allowed`.
pub async fn verify_device_jwt(
    db: &DbPool,
    token: &str,
    expected_aud: &str,
    now: DateTime<Utc>,
    allowed: AllowedStates,
) -> Result<DeviceRow, DeviceAuthError> {
    if token.len() > MAX_DEVICE_TOKEN_LEN {
        return Err(DeviceAuthError::Invalid("token too long"));
    }
    let header = decode_header(token).map_err(|_| DeviceAuthError::NotADeviceToken)?;
    if header.alg != Algorithm::EdDSA {
        return Err(DeviceAuthError::NotADeviceToken);
    }
    let kid = header
        .kid
        .filter(|kid| device::is_valid_device_id(kid))
        .ok_or(DeviceAuthError::Invalid("missing or malformed kid"))?;

    let row = db_devices::find_device(db, &kid)
        .await
        .map_err(DeviceAuthError::Db)?
        .ok_or(DeviceAuthError::Invalid("unknown device"))?;

    // Only ever build a DecodingKey from exactly 32 bytes: the EdDSA verifier
    // slices [..32] and would panic on a shorter key.
    let raw = public_key_raw(&row.public_key).ok_or(DeviceAuthError::Invalid("stored key"))?;
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

/// Why a registration JWS was rejected (logged; one static description on
/// the wire).
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

/// Verify the self-signed registration JWS (contract 2.5): read
/// `public_key` unverified, validate it, then verify the whole token with
/// that key and the pinned [`device_validation`]. Returns the claims (name
/// normalized) and the raw key.
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

/// Where `/cli/consent` sends the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectTarget {
    /// `http://{host}:{port}/callback` on the user's own machine.
    Loopback { host: String, port: u16 },
    /// `{api origin}/cli/code`: the browser shows the code to paste.
    Remote,
}

impl RedirectTarget {
    /// The canonical `redirect_uri` spelling for this target.
    pub fn redirect_uri(&self, api_origin: &str) -> String {
        match self {
            RedirectTarget::Loopback { host, port } => format!("http://{host}:{port}/callback"),
            RedirectTarget::Remote => format!("{api_origin}{CLI_REMOTE_CODE_PATH}"),
        }
    }
}

/// Parse a `redirect_uri` sent to `/cli/authorize`.
///
/// Accepted spellings, exactly (anything else is rejected, never
/// normalized):
/// - `http://127.0.0.1:{port}/callback` or `http://[::1]:{port}/callback`
///   with `port` in 1024..=65535 written without leading zeros. Not
///   `localhost` (DNS can point it elsewhere), no userinfo, query or fragment.
/// - `{api_origin}/cli/code` (remote login).
pub fn parse_redirect_uri(raw: &str, api_origin: &str) -> Option<RedirectTarget> {
    if raw == format!("{api_origin}{CLI_REMOTE_CODE_PATH}") {
        return Some(RedirectTarget::Remote);
    }
    let rest = raw.strip_prefix("http://")?;
    let (host, rest) = match rest.strip_prefix("127.0.0.1:") {
        Some(rest) => ("127.0.0.1", rest),
        None => ("[::1]", rest.strip_prefix("[::1]:")?),
    };
    let port_str = rest.strip_suffix("/callback")?;
    if port_str.is_empty()
        || port_str.len() > 5
        || port_str.starts_with('0')
        || !port_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port < 1024 {
        return None;
    }
    Some(RedirectTarget::Loopback {
        host: host.to_string(),
        port,
    })
}

/// OAuth `state` from the CLI: 16..=128 visible ASCII characters.
pub fn is_valid_client_state(state: &str) -> bool {
    (16..=128).contains(&state.len()) && state.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// PKCE S256 challenge: 43..=128 base64url characters.
pub fn is_valid_code_challenge(challenge: &str) -> bool {
    (43..=128).contains(&challenge.len())
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// PKCE verifier: 43..=128 unreserved characters (RFC 7636 4.1).
pub fn is_valid_code_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// The consent page path for a request id.
pub fn consent_path(request_id: &str) -> String {
    format!("/cli/consent?request={request_id}")
}

/// The closed set of post-login return targets on this server's own origin:
/// `{api_origin}/cli/consent?request=<22-char id>` and `{api_origin}/devices`.
/// `raw` must equal the canonical spelling exactly; the returned value is
/// rebuilt from the parsed parts. Nothing else on the API origin qualifies.
pub fn device_return_target(raw: &str, api_origin: &str) -> Option<String> {
    let path = raw.strip_prefix(api_origin)?;
    let rebuilt = if path == "/devices" {
        "/devices".to_string()
    } else {
        let id = path.strip_prefix("/cli/consent?request=")?;
        if !device::is_valid_device_id(id) {
            return None;
        }
        consent_path(id)
    };
    (path == rebuilt).then(|| format!("{api_origin}{rebuilt}"))
}

/// A fresh opaque 22-char id (16 CSPRNG bytes, base64url); used for devices
/// and login requests.
pub fn generate_device_id() -> String {
    crate::secrets::new_id()
}

/// Fingerprint of a stored base64url public key. `None` if it is malformed.
pub fn fingerprint_of(public_key_b64: &str) -> Option<String> {
    public_key_raw(public_key_b64).map(|raw| device::fingerprint(&raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    const API: &str = "https://api.videocall.rs";

    fn loopback(host: &str, port: u16) -> Option<RedirectTarget> {
        Some(RedirectTarget::Loopback {
            host: host.into(),
            port,
        })
    }

    #[test]
    fn redirect_uri_accepts_canonical_loopback_and_remote() {
        assert_eq!(
            parse_redirect_uri("http://127.0.0.1:53682/callback", API),
            loopback("127.0.0.1", 53682)
        );
        assert_eq!(
            parse_redirect_uri("http://[::1]:1024/callback", API),
            loopback("[::1]", 1024)
        );
        assert_eq!(
            parse_redirect_uri("http://127.0.0.1:65535/callback", API),
            loopback("127.0.0.1", 65535)
        );
        assert_eq!(
            parse_redirect_uri("https://api.videocall.rs/cli/code", API),
            Some(RedirectTarget::Remote)
        );
        let target = loopback("[::1]", 4000).unwrap();
        assert_eq!(target.redirect_uri(API), "http://[::1]:4000/callback");
        assert_eq!(
            RedirectTarget::Remote.redirect_uri(API),
            "https://api.videocall.rs/cli/code"
        );
    }

    #[test]
    fn redirect_uri_rejects_everything_else() {
        for bad in [
            "",
            "http://localhost:53682/callback",
            "https://127.0.0.1:53682/callback",
            "HTTP://127.0.0.1:53682/callback",
            "http://127.0.0.1/callback",
            "http://127.0.0.1:/callback",
            "http://127.0.0.1:80/callback",
            "http://127.0.0.1:1023/callback",
            "http://127.0.0.1:65536/callback",
            "http://127.0.0.1:099999/callback",
            "http://127.0.0.1:01024/callback",
            "http://127.0.0.1:+1024/callback",
            "http://127.0.0.1:53682/callback/",
            "http://127.0.0.1:53682/Callback",
            "http://127.0.0.1:53682/callback?x=1",
            "http://127.0.0.1:53682/callback#frag",
            "http://127.0.0.1:53682/cb",
            "http://127.0.0.1:53682",
            "http://user@127.0.0.1:53682/callback",
            "http://127.0.0.2:53682/callback",
            "http://127.1:53682/callback",
            "http://0x7f.0.0.1:53682/callback",
            "http://[::ffff:127.0.0.1]:53682/callback",
            "http://[0:0:0:0:0:0:0:1]:53682/callback",
            "http://::1:53682/callback",
            " http://127.0.0.1:53682/callback",
            "http://127.0.0.1:53682/callback ",
            "http://127.0.0.1:53682/../callback",
            "http://127.0.0.1.evil.com:53682/callback",
            "https://evil.example/cli/code",
            "https://api.videocall.rs/cli/code/",
            "https://api.videocall.rs/cli/code?x",
            "https://api.videocall.rs:443/cli/code",
            "http://api.videocall.rs/cli/code",
            "https://api.videocall.rs.evil.com/cli/code",
        ] {
            assert_eq!(parse_redirect_uri(bad, API), None, "{bad:?}");
        }
    }

    #[test]
    fn state_challenge_and_verifier_bounds() {
        assert!(is_valid_client_state(&"a".repeat(16)));
        assert!(is_valid_client_state(&"~!".repeat(64)));
        assert!(!is_valid_client_state(&"a".repeat(15)));
        assert!(!is_valid_client_state(&"a".repeat(129)));
        assert!(!is_valid_client_state(&format!("{} ", "a".repeat(20))));
        assert!(!is_valid_client_state(&format!("{}\u{e9}", "a".repeat(20))));
        assert!(is_valid_code_challenge(&"A".repeat(43)));
        assert!(!is_valid_code_challenge(&"A".repeat(42)));
        assert!(!is_valid_code_challenge(&format!("{}=", "A".repeat(43))));
        assert!(!is_valid_code_challenge(&format!("{}.", "A".repeat(43))));
        assert!(is_valid_code_verifier(&format!("{}.~", "a".repeat(43))));
        assert!(!is_valid_code_verifier(&"a".repeat(42)));
        assert!(!is_valid_code_verifier(&format!("{}+", "a".repeat(43))));
    }

    #[test]
    fn return_targets_are_a_closed_set() {
        let id = "aB3_-aB3_-aB3_-aB3_-aB";
        assert_eq!(
            device_return_target(&format!("{API}/cli/consent?request={id}"), API),
            Some(format!("{API}/cli/consent?request={id}"))
        );
        assert_eq!(
            device_return_target(&format!("{API}/devices"), API),
            Some(format!("{API}/devices"))
        );
        for bad in [
            format!("{API}/"),
            API.to_string(),
            format!("{API}/login"),
            format!("{API}/logout"),
            format!("{API}/profile"),
            format!("{API}/api/v1/meetings"),
            format!("{API}/devices/"),
            format!("{API}/devices?x=1"),
            format!("{API}/devices#x"),
            format!("{API}/devices/abc/revoke"),
            format!("{API}/device"),
            format!("{API}//evil.example"),
            format!("{API}/device@evil.example"),
            format!("{API}/device/../x"),
            format!("{API}/cli/consent/../../login"),
            format!("{API}/cli/consent?request=short"),
            format!("{API}/cli/consent?request={id}&x=1"),
            format!("{API}/cli/consent?request={id}#x"),
            format!("{API}/cli/consent?request={id}x"),
            format!("{API}/cli/consent"),
            format!("{API}/cli/code"),
            format!("{API}/cli/authorize"),
            format!("{API}.evil.example/devices"),
            format!("{API}:443/devices"),
            format!("{API}@evil.example/devices"),
            "/devices".to_string(),
            format!("/cli/consent?request={id}"),
            "//evil.example/devices".to_string(),
            "https://evil.example/devices".to_string(),
            "http://api.videocall.rs/devices".to_string(),
        ] {
            assert_eq!(device_return_target(&bad, API), None, "{bad:?}");
        }
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
    }

    #[test]
    fn public_key_validation() {
        assert!(public_key_raw(&URL_SAFE_NO_PAD.encode([0u8; 31])).is_none());
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

    #[test]
    fn ids_are_22_chars() {
        assert!(device::is_valid_device_id(&generate_device_id()));
    }
}
