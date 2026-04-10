// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser-side id_token payload decoding and light validation.
//!
//! The browser cannot perform cryptographic signature verification (that
//! requires JWKS fetch + RSA/EC operations) — instead the meeting-api
//! validates the signature on every authenticated API call.  This module
//! handles the claims the browser *can* check without a key:
//!
//! | Check | Rationale |
//! |---|---|
//! | `nonce` | Anti-replay: binds this token to the PKCE flow started by this tab |
//! | `exp` | Rejects obviously stale tokens before they reach the server |
//! | `aud` | Confirms the token was issued for the configured `client_id` |

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use web_time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Claims struct
// ---------------------------------------------------------------------------

/// Claims we extract from the id_token payload.
///
/// Signature is **not** verified here — that is the meeting-api's
/// responsibility on every authenticated API call via JWKS.
#[derive(Debug, Deserialize)]
pub(crate) struct IdTokenClaims {
    #[serde(default)]
    pub sub: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub given_name: Option<String>,
    #[serde(default)]
    pub family_name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub exp: Option<u64>,
    /// `aud` may be a plain string (single audience) or a JSON array.
    #[serde(default)]
    pub aud: serde_json::Value,
}

impl IdTokenClaims {
    /// Return the best display name available from the token claims.
    ///
    /// Priority: `name` → `given_name family_name` → `email` →
    /// `preferred_username` → `sub`.
    pub(crate) fn display_name(&self) -> String {
        if let Some(ref n) = self.name {
            if !n.is_empty() {
                return n.clone();
            }
        }
        let given_family = match (&self.given_name, &self.family_name) {
            (Some(g), Some(f)) if !g.is_empty() => Some(format!("{g} {f}")),
            (Some(g), _) if !g.is_empty() => Some(g.clone()),
            _ => None,
        };
        if let Some(name) = given_family {
            return name;
        }
        if let Some(ref e) = self.email {
            if !e.is_empty() {
                return e.clone();
            }
        }
        if let Some(ref u) = self.preferred_username {
            if !u.is_empty() {
                return u.clone();
            }
        }
        self.sub.clone().unwrap_or_default()
    }

    /// Return the canonical user identifier: `email` when present, otherwise `sub`.
    pub(crate) fn user_id(&self) -> Option<String> {
        self.email
            .as_deref()
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .or_else(|| self.sub.clone())
    }

    /// Return `true` when the `aud` claim contains `client_id`.
    ///
    /// Per OIDC Core §2, `aud` must be a string or an array of strings.
    /// Any other JSON type (`Null`, `Bool`, `Number`, `Object`) or a missing
    /// `aud` field (which deserialises to `Null` via `#[serde(default)]`)
    /// returns `false` so the caller rejects the token.
    pub(crate) fn audience_contains(&self, client_id: &str) -> bool {
        match &self.aud {
            serde_json::Value::String(s) => s == client_id,
            serde_json::Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some(client_id)),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Decode and validate
// ---------------------------------------------------------------------------

/// Returns the current time as whole seconds since the Unix epoch.
///
/// Uses [`web_time`], which delegates to `js_sys::Date::now()` on `wasm32`
/// and to `std::time::SystemTime` on native targets.  Both the production
/// exp-check and the test suite therefore compile and run correctly on every
/// target without `#[cfg]` guards.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Decode and lightly validate the id_token payload.
///
/// Validates: `nonce` (anti-replay), `exp` (not expired), `aud` (audience).
/// Does **not** verify the cryptographic signature — that is performed by the
/// meeting-api JWKS check on every API call.
pub(crate) fn decode_and_validate_id_token(
    id_token: &str,
    expected_nonce: &str,
    client_id: &str,
) -> Result<IdTokenClaims, String> {
    // JWT is three base64url segments separated by `.`.
    let mut parts = id_token.splitn(3, '.');
    let _ = parts.next(); // header — skip
    let payload_b64 = parts
        .next()
        .ok_or("id_token has fewer than two dot-separated parts")?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("Failed to base64url-decode id_token payload: {e}"))?;

    let claims: IdTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|e| format!("Failed to parse id_token claims JSON: {e}"))?;

    // --- nonce (anti-replay) ---
    match &claims.nonce {
        Some(n) if n == expected_nonce => {}
        Some(n) => {
            return Err(format!(
                "id_token nonce mismatch: expected '{expected_nonce}', got '{n}'"
            ));
        }
        None => {
            return Err("id_token is missing the nonce claim".to_string());
        }
    }

    // --- exp ---
    if let Some(exp) = claims.exp {
        if now_secs() > exp {
            return Err(format!("id_token has expired (exp={exp})"));
        }
    }

    // --- aud ---
    if !claims.audience_contains(client_id) {
        return Err(format!(
            "id_token audience does not contain the configured client_id '{client_id}'"
        ));
    }

    Ok(claims)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A far-future expiry (year ~2286) used wherever tests need a token that
    /// is not expired.  A hardcoded constant keeps tests deterministic and
    /// removes any dependency on `js_sys` or a live clock.
    const FAR_FUTURE_EXP: u64 = 9_999_999_999;

    fn make_jwt_payload(claims: serde_json::Value) -> String {
        let json = serde_json::to_string(&claims).unwrap();
        let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
        format!("eyJhbGciOiJSUzI1NiJ9.{encoded}.fakesig")
    }

    #[test]
    fn valid_claims_decode_successfully() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "email": "user@example.com",
            "nonce": "testnonce",
            "exp": FAR_FUTURE_EXP,
            "aud": "my-client-id",
        }));
        let claims = decode_and_validate_id_token(&token, "testnonce", "my-client-id");
        assert!(claims.is_ok(), "should decode valid claims");
        let c = claims.unwrap();
        assert_eq!(c.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn wrong_nonce_rejected() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "correct-nonce",
            "exp": FAR_FUTURE_EXP,
            "aud": "client",
        }));
        let result = decode_and_validate_id_token(&token, "wrong-nonce", "client");
        assert!(result.is_err(), "wrong nonce must be rejected");
    }

    #[test]
    fn expired_token_rejected() {
        let past_exp = 1_000_000u64; // long expired
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": past_exp,
            "aud": "client",
        }));
        let result = decode_and_validate_id_token(&token, "n", "client");
        assert!(result.is_err(), "expired token must be rejected");
    }

    #[test]
    fn wrong_audience_rejected() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": "other-client",
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "wrong audience must be rejected");
    }

    #[test]
    fn array_audience_accepted_when_client_id_present() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": ["my-client", "other-client"],
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_ok(), "client_id in array aud should be accepted");
    }

    #[test]
    fn array_audience_rejected_when_client_id_absent() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": ["other-client", "another-client"],
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(
            result.is_err(),
            "client_id absent from array aud must be rejected"
        );
    }

    #[test]
    fn null_audience_rejected() {
        // `aud` absent from JSON deserialises to Null via #[serde(default)];
        // an explicit null has the same effect.  Both must be rejected.
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": null,
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "null aud must be rejected");
    }

    #[test]
    fn missing_audience_field_rejected() {
        // When the `aud` key is entirely absent, serde deserialises it as
        // Null (the Default for serde_json::Value), which must also be rejected.
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "missing aud must be rejected");
    }

    #[test]
    fn boolean_audience_rejected() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": true,
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "boolean aud must be rejected");
    }

    #[test]
    fn numeric_audience_rejected() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": 42,
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "numeric aud must be rejected");
    }

    #[test]
    fn object_audience_rejected() {
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": FAR_FUTURE_EXP,
            "aud": {"client": "my-client"},
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "object aud must be rejected");
    }

    #[test]
    fn display_name_prefers_name_claim() {
        let claims = IdTokenClaims {
            sub: Some("sub".into()),
            email: Some("e@e.com".into()),
            name: Some("Full Name".into()),
            given_name: Some("First".into()),
            family_name: Some("Last".into()),
            preferred_username: None,
            nonce: None,
            exp: None,
            aud: serde_json::Value::Null,
        };
        assert_eq!(claims.display_name(), "Full Name");
    }

    #[test]
    fn display_name_falls_back_to_given_family() {
        let claims = IdTokenClaims {
            sub: Some("sub".into()),
            email: Some("e@e.com".into()),
            name: None,
            given_name: Some("First".into()),
            family_name: Some("Last".into()),
            preferred_username: None,
            nonce: None,
            exp: None,
            aud: serde_json::Value::Null,
        };
        assert_eq!(claims.display_name(), "First Last");
    }

    #[test]
    fn display_name_falls_back_to_email() {
        let claims = IdTokenClaims {
            sub: Some("sub".into()),
            email: Some("user@example.com".into()),
            name: None,
            given_name: None,
            family_name: None,
            preferred_username: None,
            nonce: None,
            exp: None,
            aud: serde_json::Value::Null,
        };
        assert_eq!(claims.display_name(), "user@example.com");
    }
}
