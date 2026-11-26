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

//! Device-key authentication types shared by `meeting-api` and
//! `videocall-cli`.
//!
//! A CLI device holds an Ed25519 private key. `videocall-cli auth login`
//! works like `aws login`: the browser opens `/cli/authorize` with a PKCE
//! challenge and the public key, the signed-in human approves on the consent
//! page, and the CLI exchanges the one-time code together with a self-signed
//! registration JWS (proof of possession) at `POST /api/v1/devices`. The
//! device is approved from then on and signs a short-lived JWT for every
//! request.
//!
//! This module carries only wire types and pure helpers. Key generation,
//! signing and verification live in the CLI and in
//! `meeting_api::device_auth`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// `iss` claim of every device JWT and registration JWS.
pub const CLI_ISSUER: &str = "videocall-cli";

/// `client_id` the CLI sends to `/cli/authorize`.
pub const CLI_CLIENT_ID: &str = "videocall-cli";

/// Path of the authorize endpoint on the API origin.
pub const CLI_AUTHORIZE_PATH: &str = "/cli/authorize";

/// Path of the remote-login `redirect_uri` on the API origin
/// (`{api origin}/cli/code`). The browser shows the code there and the human
/// pastes it into the terminal.
pub const CLI_REMOTE_CODE_PATH: &str = "/cli/code";

/// Path of the code exchange / registration endpoint.
pub const DEVICES_PATH: &str = "/api/v1/devices";

/// Path of the device's own status (`GET`) and self-revocation (`DELETE`).
pub const DEVICES_ME_PATH: &str = "/api/v1/devices/me";

/// Lifetime the CLI uses when minting a device JWT (`exp - iat`).
pub const DEVICE_TOKEN_TTL_SECS: i64 = 120;

/// Largest `exp - iat` a server accepts on a device JWT or registration JWS.
pub const DEVICE_TOKEN_MAX_LIFETIME_SECS: i64 = 300;

/// Clock leeway, in seconds, a server allows on `exp` and on a future `iat`.
pub const DEVICE_TOKEN_LEEWAY_SECS: i64 = 60;

/// Length of a base64url (no padding) encoded raw Ed25519 public key.
pub const PUBLIC_KEY_B64_LEN: usize = 43;

/// Length of a `device_id` (16 random bytes, base64url without padding).
pub const DEVICE_ID_LEN: usize = 22;

/// Maximum length of a device name, in bytes (the name is ASCII only).
pub const DEVICE_NAME_MAX_LEN: usize = 64;

/// Body of `POST /api/v1/devices`: exchange the one-time code for an
/// approved device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDeviceRequest {
    /// One-time code from the loopback callback or the remote Code page.
    pub code: String,
    /// PKCE verifier whose S256 challenge was sent to `/cli/authorize`.
    pub code_verifier: String,
    /// Exactly the `redirect_uri` sent to `/cli/authorize`.
    pub redirect_uri: String,
    /// The registration JWS ([`RegistrationClaims`]), signed by the new key.
    pub registration: String,
}

/// `201` response of `POST /api/v1/devices` (a bare JSON object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDeviceResponse {
    /// Server-assigned device id; the `kid` and `sub` of every device JWT.
    pub device_id: String,
    /// Server-computed [`fingerprint`] of the registered key.
    pub fingerprint: String,
    /// Device name, as stored.
    pub name: String,
    /// The account the device acts as.
    pub user_id: String,
    /// Display name of that account.
    pub user_name: String,
    /// Unix seconds; the device stops working if unused until then.
    pub idle_expires_at: i64,
    /// Unix seconds; the device stops working at this time regardless.
    pub abs_expires_at: i64,
}

/// State of a device as reported by `GET /api/v1/devices/me`. A device row
/// only exists once approved; revoked or expired devices get 401 instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    /// Approved and live; the device may call meeting-api as `user_id`.
    Approved,
}

impl DeviceStatus {
    /// The wire string (`"approved"`).
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceStatus::Approved => "approved",
        }
    }
}

/// `200` response of `GET /api/v1/devices/me` (a bare JSON object).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatusResponse {
    pub device_id: String,
    pub status: DeviceStatus,
    pub name: String,
    pub fingerprint: String,
    pub user_id: String,
    pub user_name: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds; when the human approved it.
    pub decided_at: i64,
    /// Unix seconds; updated at most once an hour.
    pub last_used_at: i64,
    /// Unix seconds.
    pub idle_expires_at: i64,
    /// Unix seconds.
    pub abs_expires_at: i64,
}

/// JSON error body of the device API endpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceErrorResponse {
    /// Machine-readable code: `invalid_request`, `invalid_grant`,
    /// `invalid_token`, `key_already_registered`, `too_many_devices`,
    /// `device_auth_disabled`, `server_error`.
    pub error: String,
    /// Static human-readable description.
    pub error_description: String,
}

/// Claims of the device JWT the CLI signs for every request after
/// registration. Header: `{"alg":"EdDSA","typ":"JWT","kid":"<device_id>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceTokenClaims {
    /// Always [`CLI_ISSUER`].
    pub iss: String,
    /// The `device_id`; must equal the header `kid`.
    pub sub: String,
    /// Exact API origin.
    pub aud: String,
    /// Unix seconds.
    pub iat: i64,
    /// Unix seconds; `iat + DEVICE_TOKEN_TTL_SECS` when minted by the CLI.
    pub exp: i64,
    /// 16 random bytes, base64url.
    pub jti: String,
}

/// Claims of the self-signed registration JWS (proof of possession).
/// Header: `{"alg":"EdDSA","typ":"JWT"}` with no `kid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationClaims {
    /// Always [`CLI_ISSUER`].
    pub iss: String,
    /// Exact API origin.
    pub aud: String,
    /// Unix seconds.
    pub iat: i64,
    /// Unix seconds.
    pub exp: i64,
    /// Raw 32-byte Ed25519 public key, base64url without padding (43 chars).
    /// Must equal the `public_key` sent to `/cli/authorize`.
    pub public_key: String,
    /// Device name; must equal the `device_name` sent to `/cli/authorize`.
    pub name: String,
}

/// Fingerprint of a raw Ed25519 public key, shown in the terminal and on the
/// consent page: the first 8 bytes of `SHA-256(pk)` as upper-case hex in
/// four dash-separated groups of four, e.g. `9F3C-21AB-77D0-4E18`.
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

/// True if `value` is exactly `len` characters of `[A-Za-z0-9_-]`.
fn is_base64url_of_len(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// True if `value` has the shape of a `device_id` (22 base64url chars).
pub fn is_valid_device_id(value: &str) -> bool {
    is_base64url_of_len(value, DEVICE_ID_LEN)
}

/// True if `value` has the shape of an encoded public key (43 base64url
/// chars). Decoding and the Ed25519 checks happen server-side.
pub fn is_valid_public_key_format(value: &str) -> bool {
    is_base64url_of_len(value, PUBLIC_KEY_B64_LEN)
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
    fn id_and_key_shapes() {
        assert!(is_valid_device_id("aB3_-aB3_-aB3_-aB3_-aB"));
        assert!(!is_valid_device_id("aB3_-aB3_-aB3_-aB3_-a"));
        assert!(!is_valid_device_id("aB3_-aB3_-aB3_-aB3_-a="));
        assert!(is_valid_public_key_format(&"A".repeat(43)));
        assert!(!is_valid_public_key_format(&"A".repeat(44)));
        assert!(!is_valid_public_key_format(&format!("{}+", "A".repeat(42))));
    }

    #[test]
    fn wire_format() {
        assert_eq!(
            serde_json::to_string(&DeviceStatus::Approved).unwrap(),
            "\"approved\""
        );
        let err: DeviceErrorResponse =
            serde_json::from_str(r#"{"error":"invalid_grant","error_description":"x"}"#).unwrap();
        assert_eq!(err.error, "invalid_grant");
        let req: RegisterDeviceRequest = serde_json::from_str(
            r#"{"code":"c","code_verifier":"v","redirect_uri":"r","registration":"j"}"#,
        )
        .unwrap();
        assert_eq!(req.redirect_uri, "r");
    }
}
