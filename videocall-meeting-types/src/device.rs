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

//! CLI device sign-in: wire types and pure, wasm-safe validators shared by
//! meeting-api, the app and the CLI, so every check has one implementation.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `iss` claim of every device JWT and registration JWS.
pub const CLI_ISSUER: &str = "videocall-cli";

/// `client_id` the CLI sends to `/cli/authorize`.
pub const CLI_CLIENT_ID: &str = "videocall-cli";

/// On the API origin; redirects with the query unchanged to
/// [`APP_AUTHORIZE_PATH`] on the app origin.
pub const CLI_AUTHORIZE_PATH: &str = "/cli/authorize";

/// Path of the consent page on the app origin.
pub const APP_AUTHORIZE_PATH: &str = "/devices/authorize";

/// Query parameter carrying the CLI's request to [`APP_AUTHORIZE_PATH`]:
/// `?request=<encode_authorize_request(raw query)>`. Base64url survives any
/// router or address-bar rewriting that would percent-decode the raw query.
pub const AUTHORIZE_REQUEST_PARAM: &str = "request";

/// Longest raw `/cli/authorize` query accepted, in bytes.
pub const MAX_AUTHORIZE_QUERY_LEN: usize = 2048;

/// The remote-login `redirect_uri`: the app shows the code to paste.
pub const CLI_REMOTE_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

pub const CODE_CHALLENGE_METHOD_S256: &str = "S256";

/// `POST`: the CLI's code exchange. `GET`: the user's devices.
/// `DELETE {DEVICES_PATH}/{id}`: revoke one.
pub const DEVICES_PATH: &str = "/api/v1/devices";

pub const DEVICE_AUTHORIZATIONS_PATH: &str = "/api/v1/devices/authorizations";

pub const DEVICES_ME_PATH: &str = "/api/v1/devices/me";

/// Lifetime the CLI uses when minting a device JWT (`exp - iat`).
pub const DEVICE_TOKEN_TTL_SECS: i64 = 120;

/// Largest `exp - iat` a server accepts on a device JWT or registration JWS.
pub const DEVICE_TOKEN_MAX_LIFETIME_SECS: i64 = 300;

/// Clock leeway, in seconds, a server allows on `exp` and on a future `iat`.
pub const DEVICE_TOKEN_LEEWAY_SECS: i64 = 60;

/// Device names are ASCII, at most this many bytes.
pub const DEVICE_NAME_MAX_LEN: usize = 64;

// `APIError::code` values of the browser device routes. A device JWT on any
// of them gets `BROWSER_SESSION_REQUIRED`; no session gets `UNAUTHORIZED`.

/// 400: a malformed body or an invalid field.
pub const INVALID_REQUEST: &str = "INVALID_REQUEST";
/// 400: a remote login approved without `confirm: true`.
pub const CONFIRMATION_REQUIRED: &str = "CONFIRMATION_REQUIRED";
/// 403: `Origin` missing, `null` or not an allowed browser origin.
pub const ORIGIN_NOT_ALLOWED: &str = "ORIGIN_NOT_ALLOWED";
/// 415: a POST body that is not `application/json`.
pub const UNSUPPORTED_MEDIA_TYPE: &str = "UNSUPPORTED_MEDIA_TYPE";
/// 409: the public key already belongs to a device.
pub const KEY_ALREADY_REGISTERED: &str = "KEY_ALREADY_REGISTERED";
/// 409: the user is at the live-device cap.
pub const TOO_MANY_DEVICES: &str = "TOO_MANY_DEVICES";
/// 404: no such device for this user (unknown, someone else's or revoked).
pub const DEVICE_NOT_FOUND: &str = "DEVICE_NOT_FOUND";
/// 503: device sign-in is not configured on this server.
pub const NOT_CONFIGURED: &str = "NOT_CONFIGURED";

/// A parsed `redirect_uri`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectUri {
    /// `http://127.0.0.1:{port}/callback`.
    Loopback { port: u16 },
    /// [`CLI_REMOTE_REDIRECT_URI`].
    Remote,
}

impl RedirectUri {
    pub fn loopback_port(self) -> Option<u16> {
        match self {
            RedirectUri::Loopback { port } => Some(port),
            RedirectUri::Remote => None,
        }
    }

    /// The canonical spelling, rebuilt from the parsed value.
    pub fn as_uri(self) -> String {
        match self {
            RedirectUri::Loopback { port } => format!("http://127.0.0.1:{port}/callback"),
            RedirectUri::Remote => CLI_REMOTE_REDIRECT_URI.to_string(),
        }
    }
}

/// Parse a CLI `redirect_uri`. Accepted spellings, exactly (anything else is
/// rejected, never normalized):
/// - `http://127.0.0.1:{port}/callback` with `port` in 1024..=65535 written
///   without leading zeros. Not `localhost` (DNS can point it elsewhere), not
///   `[::1]`, no userinfo, query or fragment.
/// - [`CLI_REMOTE_REDIRECT_URI`].
pub fn parse_redirect_uri(raw: &str) -> Option<RedirectUri> {
    if raw == CLI_REMOTE_REDIRECT_URI {
        return Some(RedirectUri::Remote);
    }
    let port_str = raw
        .strip_prefix("http://127.0.0.1:")?
        .strip_suffix("/callback")?;
    if port_str.is_empty()
        || port_str.len() > 5
        || port_str.starts_with('0')
        || !port_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    (port >= 1024).then_some(RedirectUri::Loopback { port })
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

/// Body of `POST /api/v1/devices/authorizations`: the `/cli/authorize`
/// query parameters plus `confirm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateDeviceAuthorizationRequest {
    pub redirect_uri: String,
    pub code_challenge: String,
    pub code_challenge_method: String,
    /// Ignored for a remote login.
    pub state: String,
    pub public_key: String,
    pub device_name: String,
    /// Must be `true` for a remote login; ignored for loopback.
    #[serde(default)]
    pub confirm: bool,
}

/// `result` of a successful `POST /api/v1/devices/authorizations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CreateDeviceAuthorizationResponse {
    Loopback { redirect_to: String },
    Remote { code: String, expires_in: i64 },
}

/// One live device in `GET /api/v1/devices`. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceListItem {
    pub id: String,
    pub name: String,
    pub fingerprint: String,
    pub created_at: i64,
    /// Updated at most once an hour.
    pub last_used_at: i64,
    /// The earlier of the idle and absolute expiry.
    pub expires_at: i64,
}

/// Body of `POST /api/v1/devices`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDeviceRequest {
    pub code: String,
    pub code_verifier: String,
    /// Exactly the approved `redirect_uri`.
    pub redirect_uri: String,
    /// The registration JWS ([`RegistrationClaims`]), signed by the new key.
    pub registration: String,
}

/// `201` body of `POST /api/v1/devices`. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDeviceResponse {
    /// The `kid` and `sub` of every device JWT.
    pub device_id: String,
    pub fingerprint: String,
    pub name: String,
    pub user_id: String,
    pub user_name: String,
    pub idle_expires_at: i64,
    pub abs_expires_at: i64,
}

/// `200` body of `GET /api/v1/devices/me`: the calling device is approved
/// and live (otherwise it gets 401). Times are Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatusResponse {
    pub device_id: String,
    pub name: String,
    pub fingerprint: String,
    pub user_id: String,
    pub user_name: String,
    pub created_at: i64,
    /// Updated at most once an hour.
    pub last_used_at: i64,
    pub idle_expires_at: i64,
    pub abs_expires_at: i64,
}

/// Error body of the CLI routes: `error` is `invalid_request`,
/// `invalid_grant`, `invalid_token`, `key_already_registered`,
/// `too_many_devices`, `not_configured` or `server_error`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceErrorResponse {
    pub error: String,
    pub error_description: String,
}

/// Claims of the device JWT signed for every request. Header:
/// `{"alg":"EdDSA","typ":"JWT","kid":"<device_id>"}`; `iss` is
/// [`CLI_ISSUER`], `sub` equals `kid`, `aud` is the exact API origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTokenClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
}

/// Claims of the self-signed registration JWS (proof of possession), header
/// without `kid`. `public_key` (43-char base64url raw Ed25519 key) and `name`
/// must equal the approved ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationClaims {
    pub iss: String,
    pub aud: String,
    pub iat: i64,
    pub exp: i64,
    pub public_key: String,
    pub name: String,
}

/// First 8 bytes of `SHA-256(raw key)` as upper-case hex in four
/// dash-separated groups, e.g. `9F3C-21AB-77D0-4E18`.
pub fn fingerprint(public_key_raw: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let digest = Sha256::digest(public_key_raw);
    let mut out = String::with_capacity(19);
    for (index, byte) in digest[..8].iter().enumerate() {
        if index > 0 && index % 2 == 0 {
            out.push('-');
        }
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Trim ASCII whitespace from a device name and validate it against
/// `^[A-Za-z0-9][A-Za-z0-9 ._()@:+-]{0,63}$`. ASCII only by design: no
/// control, format, bidi or homoglyph characters.
pub fn normalize_device_name(input: &str) -> Option<String> {
    let name = input.trim_matches(|c: char| c.is_ascii_whitespace());
    let bytes = name.as_bytes();
    let first = *bytes.first()?;
    if bytes.len() > DEVICE_NAME_MAX_LEN || !first.is_ascii_alphanumeric() {
        return None;
    }
    let allowed = |b: &u8| b.is_ascii_alphanumeric() || b" ._()@:+-".contains(b);
    bytes.iter().all(allowed).then(|| name.to_string())
}

fn is_base64url_of_len(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// 22 base64url chars (16 random bytes).
pub fn is_valid_device_id(value: &str) -> bool {
    is_base64url_of_len(value, 22)
}

/// 43 base64url chars (a raw 32-byte key). The Ed25519 checks are
/// server-side.
pub fn is_valid_public_key_format(value: &str) -> bool {
    is_base64url_of_len(value, 43)
}

/// Decode a public key to its 32 raw bytes (canonical spelling only).
pub fn decode_public_key(encoded: &str) -> Option<[u8; 32]> {
    if !is_valid_public_key_format(encoded) {
        return None;
    }
    base64url_decode(encoded)?.try_into().ok()
}

/// Wrap the raw `/cli/authorize` query for the app (no validation).
pub fn encode_authorize_request(raw_query: &str) -> String {
    base64url_encode(raw_query.as_bytes())
}

/// Unwrap [`encode_authorize_request`]: strict base64url without padding,
/// valid UTF-8, at most [`MAX_AUTHORIZE_QUERY_LEN`] bytes.
pub fn decode_authorize_request(request: &str) -> Option<String> {
    if request.len() > MAX_AUTHORIZE_QUERY_LEN.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64url_decode(request)?;
    if bytes.len() > MAX_AUTHORIZE_QUERY_LEN {
        return None;
    }
    String::from_utf8(bytes).ok()
}

const BASE64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// RFC 4648 base64url without padding.
fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let group = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(BASE64URL[(group >> (18 - 6 * i)) as usize & 63] as char);
        }
    }
    out
}

/// Strict base64url: alphabet only, no padding, and only the canonical
/// spelling (unused trailing bits zero), so every byte string has exactly
/// one accepted encoding.
fn base64url_decode(encoded: &str) -> Option<Vec<u8>> {
    if encoded.len() % 4 == 1 {
        return None;
    }
    let (mut acc, mut bits, mut out) = (0u32, 0, Vec::with_capacity(encoded.len() * 3 / 4));
    for byte in encoded.bytes() {
        acc = (acc << 6) | BASE64URL.iter().position(|&c| c == byte)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (acc == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_vectors() {
        // SHA-256 of 32 zero bytes = 66687aadf862bd77…
        assert_eq!(fingerprint(&[0u8; 32]), "6668-7AAD-F862-BD77");
        // SHA-256 of 32 0x01 bytes = 72cd6e8422c407fb…
        assert_eq!(fingerprint(&[1u8; 32]), "72CD-6E84-22C4-07FB");
        assert_eq!(fingerprint(&[7u8; 32]).len(), 19);
    }

    #[test]
    fn device_name_rules() {
        assert_eq!(
            normalize_device_name("  videocall-cli on pi4 ").as_deref(),
            Some("videocall-cli on pi4")
        );
        assert_eq!(normalize_device_name("a").as_deref(), Some("a"));
        assert!(normalize_device_name("host (lab) user@box:1+x._").is_some());
        assert!(normalize_device_name(&"a".repeat(64)).is_some());
        assert_eq!(normalize_device_name(&"a".repeat(65)), None);
        assert_eq!(normalize_device_name(""), None);
        assert_eq!(normalize_device_name("   "), None);
        assert_eq!(normalize_device_name("-leading"), None);
        assert_eq!(normalize_device_name("caf\u{e9}"), None);
        assert_eq!(normalize_device_name("evil\u{202e}gpj"), None);
        assert_eq!(normalize_device_name("tab\there"), None);
        assert_eq!(normalize_device_name("nul\0"), None);
        assert_eq!(normalize_device_name("<script>"), None);
    }

    #[test]
    fn redirect_uri_accepts_canonical_loopback_and_remote() {
        for (raw, parsed) in [
            (
                "http://127.0.0.1:1024/callback",
                RedirectUri::Loopback { port: 1024 },
            ),
            (
                "http://127.0.0.1:65535/callback",
                RedirectUri::Loopback { port: 65535 },
            ),
            (CLI_REMOTE_REDIRECT_URI, RedirectUri::Remote),
        ] {
            assert_eq!(parse_redirect_uri(raw), Some(parsed));
            assert_eq!(parsed.as_uri(), raw);
        }
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
            "http://[::1]:53682/callback",
            "http://::1:53682/callback",
            " http://127.0.0.1:53682/callback",
            "http://127.0.0.1:53682/callback ",
            "http://127.0.0.1:53682/../callback",
            "http://127.0.0.1.evil.com:53682/callback",
            "urn:ietf:wg:oauth:2.0:oob:auto",
            "URN:IETF:WG:OAUTH:2.0:OOB",
            "https://api.videocall.rs/cli/code",
        ] {
            assert_eq!(parse_redirect_uri(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn state_and_challenge_bounds() {
        assert!(is_valid_client_state(&"a".repeat(16)));
        assert!(is_valid_client_state(&"~!".repeat(64)));
        assert!(!is_valid_client_state(&"a".repeat(15)));
        assert!(!is_valid_client_state(&"a".repeat(129)));
        assert!(!is_valid_client_state(&format!("{} ", "a".repeat(20))));
        assert!(!is_valid_client_state(&format!("{}\u{e9}", "a".repeat(20))));
        assert!(is_valid_code_challenge(&"A".repeat(43)));
        assert!(is_valid_code_challenge(&"A".repeat(128)));
        assert!(!is_valid_code_challenge(&"A".repeat(42)));
        assert!(!is_valid_code_challenge(&"A".repeat(129)));
        assert!(!is_valid_code_challenge(&format!("{}=", "A".repeat(43))));
        assert!(!is_valid_code_challenge(&format!("{}.", "A".repeat(43))));
    }

    #[test]
    fn id_and_key_shapes() {
        assert!(is_valid_device_id("aB3_-aB3_-aB3_-aB3_-aB"));
        assert!(!is_valid_device_id("aB3_-aB3_-aB3_-aB3_-a"));
        assert!(!is_valid_device_id("aB3_-aB3_-aB3_-aB3_-a="));
        assert!(is_valid_public_key_format(&"A".repeat(43)));
        assert!(!is_valid_public_key_format(&"A".repeat(44)));
        assert!(!is_valid_public_key_format(&format!("{}+", "A".repeat(42))));
        // 32 bytes of 0xFF: 42 x '_' then '8' (spare bits zero); '_' is not.
        let canonical = format!("{}8", "_".repeat(42));
        assert_eq!(decode_public_key(&canonical), Some([0xFF; 32]));
        assert_eq!(decode_public_key(&"_".repeat(43)), None);
        assert_eq!(decode_public_key(&"A".repeat(43)), Some([0; 32]));
        assert_eq!(decode_public_key(&"A".repeat(42)), None);
    }

    #[test]
    fn base64url_codec() {
        // RFC 4648 section 10, base64url without padding.
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode_authorize_request(raw), encoded);
            assert_eq!(decode_authorize_request(encoded).as_deref(), Some(raw));
        }
        assert_eq!(base64url_encode(&[0xfb, 0xff]), "-_8");
        for bad in ["Zg==", "Zm9v+", "Zm9v/", "Zh", "Zm9", "Z", "Zm9vY", "Zm 9v"] {
            assert_eq!(base64url_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn authorize_request_round_trip() {
        for raw in [
            "device_name=C%2B%2B+rig&state=a%2Bb%25c",
            "response_type=code&redirect_uri=http%3A%2F%2F127.0.0.1%3A53682%2Fcallback&x=%3Cscript%3E#",
            "",
            &"a".repeat(MAX_AUTHORIZE_QUERY_LEN),
        ] {
            let request = encode_authorize_request(raw);
            assert!(request.bytes().all(|b| BASE64URL.contains(&b)));
            assert_eq!(decode_authorize_request(&request).as_deref(), Some(raw));
        }
        let too_long = encode_authorize_request(&"a".repeat(MAX_AUTHORIZE_QUERY_LEN + 1));
        assert_eq!(decode_authorize_request(&too_long), None);
        assert_eq!(
            decode_authorize_request(&base64url_encode(&[0xff, 0xfe])),
            None
        );
    }

    #[test]
    fn wire_format() {
        let approve: CreateDeviceAuthorizationRequest = serde_json::from_str(
            r#"{"redirect_uri":"r","code_challenge":"c","code_challenge_method":"S256",
                "state":"s","public_key":"k","device_name":"n"}"#,
        )
        .unwrap();
        assert!(!approve.confirm, "confirm defaults to false");
        let remote: CreateDeviceAuthorizationResponse =
            serde_json::from_str(r#"{"code":"c","expires_in":300}"#).unwrap();
        assert!(matches!(
            remote,
            CreateDeviceAuthorizationResponse::Remote { .. }
        ));
        let loopback: CreateDeviceAuthorizationResponse =
            serde_json::from_str(r#"{"redirect_to":"u"}"#).unwrap();
        assert!(matches!(
            loopback,
            CreateDeviceAuthorizationResponse::Loopback { .. }
        ));
    }
}
