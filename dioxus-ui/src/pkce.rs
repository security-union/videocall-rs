// SPDX-License-Identifier: MIT OR Apache-2.0

//! PKCE (Proof Key for Code Exchange, RFC 7636) helpers for the OIDC flow.
//!
//! ## Responsibilities
//!
//! - Generate a cryptographically random `code_verifier`, derive the
//!   `code_challenge` (Base64url-SHA-256), and produce random `state` and
//!   `nonce` values using `getrandom` (which delegates to
//!   `window.crypto.getRandomValues` on WASM and the OS CSPRNG on native).
//! - Persist the generated values in session-scoped storage (browser
//!   `sessionStorage` on web; in-memory store on native) so they survive the
//!   redirect to the identity provider and can be retrieved by the
//!   `/auth/callback` page.
//! - Validate the CSRF `state` parameter echoed back by the provider.
//!
//! ## Storage backend
//!
//! Storage is managed through plain-text `web_sys` `sessionStorage` helpers
//! (see [`crate::context`]), which map to the browser's `sessionStorage` on
//! web (tab-scoped, discarded when the tab closes, inaccessible to
//! cross-origin scripts).  Values are stored as plain strings and are
//! human-readable in browser DevTools.  Clearing a key uses `removeItem`.
//!
//! ## Security properties
//!
//! | Value | Length | Encoding | Purpose |
//! |---|---|---|---|
//! | `code_verifier` | 32 bytes | Base64url (no padding) | PKCE verifier sent to token endpoint |
//! | `code_challenge` | SHA-256 of verifier | Base64url (no padding) | Sent in auth request |
//! | `state` | 16 bytes | hex | CSRF protection (validated in callback) |
//! | `nonce` | 16 bytes | hex | Binds id_token to this session |

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

use crate::context::{read_session_storage, remove_session_storage, write_session_storage};

// ---------------------------------------------------------------------------
// Storage keys
// ---------------------------------------------------------------------------

pub const PKCE_VERIFIER_KEY: &str = "vc_pkce_verifier";
pub const PKCE_STATE_KEY: &str = "vc_pkce_state";
pub const PKCE_NONCE_KEY: &str = "vc_pkce_nonce";
/// Pre-existing key — the URL to navigate to after a successful login.
pub const RETURN_TO_KEY: &str = "vc_oauth_return_to";

// ---------------------------------------------------------------------------
// Crypto primitives
// ---------------------------------------------------------------------------

/// Fill a buffer with cryptographically random bytes using `getrandom`, which
/// delegates to `window.crypto.getRandomValues` in a browser/WASM context.
///
/// # Panics
///
/// Panics when the underlying CSPRNG is unavailable (should never happen in a
/// modern browser or standard OS environment).
fn get_random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("CSPRNG unavailable");
    buf
}

/// Encode `bytes` as lowercase hexadecimal.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// PKCE value generation
// ---------------------------------------------------------------------------

/// Generate a PKCE `code_verifier`: 32 random bytes encoded as Base64url
/// without padding (43 characters, satisfying the RFC 7636 [A-Za-z0-9\-._~]
/// requirement when Base64url-encoded).
pub fn generate_code_verifier() -> String {
    URL_SAFE_NO_PAD.encode(get_random_bytes(32))
}

/// Derive the `code_challenge` from a verifier: `BASE64URL(SHA-256(verifier))`.
pub fn derive_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(hasher.finalize())
}

/// Generate a random CSRF `state` token (16 bytes → 32 hex characters).
pub fn generate_state() -> String {
    to_hex(&get_random_bytes(16))
}

/// Generate a random OIDC `nonce` (16 bytes → 32 hex characters).
pub fn generate_nonce() -> String {
    to_hex(&get_random_bytes(16))
}

// ---------------------------------------------------------------------------
// Bundled PKCE parameters
// ---------------------------------------------------------------------------

/// All values generated for one PKCE authorization request.
#[derive(Debug, Clone)]
pub struct PkceParams {
    pub code_verifier: String,
    pub code_challenge: String,
    pub state: String,
    pub nonce: String,
}

/// Generate a complete set of PKCE parameters for a new authorization request.
pub fn generate_pkce_params() -> PkceParams {
    let code_verifier = generate_code_verifier();
    let code_challenge = derive_code_challenge(&code_verifier);
    let state = generate_state();
    let nonce = generate_nonce();
    PkceParams {
        code_verifier,
        code_challenge,
        state,
        nonce,
    }
}

// ---------------------------------------------------------------------------
// Session-storage persistence
// ---------------------------------------------------------------------------

/// Persist the PKCE parameters and optional `return_to` URL in the browser's
/// `sessionStorage` as plain-text strings so they survive the redirect to the
/// provider.
///
/// Existing values are overwritten — each call to `start_oauth_flow` starts a
/// fresh PKCE session.
pub fn save_pkce_state(params: &PkceParams, return_to: Option<&str>) {
    write_session_storage(PKCE_VERIFIER_KEY, &params.code_verifier);
    write_session_storage(PKCE_STATE_KEY, &params.state);
    write_session_storage(PKCE_NONCE_KEY, &params.nonce);
    if let Some(rt) = return_to {
        write_session_storage(RETURN_TO_KEY, rt);
    } else {
        // Clear any stale return_to from a previous attempt.
        remove_session_storage(RETURN_TO_KEY);
    }
}

/// Load the saved PKCE state from session-scoped storage.
///
/// Returns `None` when any required key is missing (e.g. the user opened a
/// fresh tab directly on `/auth/callback` without going through the login
/// flow, or the session was cleared).
pub fn load_pkce_state() -> Option<SavedPkceState> {
    let verifier = read_session_storage(PKCE_VERIFIER_KEY)?;
    let state = read_session_storage(PKCE_STATE_KEY)?;
    let nonce = read_session_storage(PKCE_NONCE_KEY)?;
    let return_to = read_session_storage(RETURN_TO_KEY);
    Some(SavedPkceState {
        code_verifier: verifier,
        state,
        nonce,
        return_to,
    })
}

/// Clear all PKCE keys from session-scoped storage.
///
/// Called by the callback page after successfully exchanging the code so the
/// one-time values cannot be replayed.
pub fn clear_pkce_state() {
    remove_session_storage(PKCE_VERIFIER_KEY);
    remove_session_storage(PKCE_STATE_KEY);
    remove_session_storage(PKCE_NONCE_KEY);
    remove_session_storage(RETURN_TO_KEY);
}

// ---------------------------------------------------------------------------
// Provider token exchange (browser-side PKCE flow)
// ---------------------------------------------------------------------------

/// Summarize a provider response body for error-path logging WITHOUT shipping a
/// live token.
///
/// On PKCE/Ascend deployments the browser console logs are uploaded to a
/// collector. A malformed-but-token-bearing provider response logged verbatim
/// could therefore exfiltrate a live access/id/refresh token. This returns only
/// the byte length and a short Debug-escaped prefix (control chars escaped),
/// which is enough to debug a parse/status failure without dumping the token.
///
/// The prefix is capped deliberately short (32 chars): on an error-shaped
/// response that nonetheless front-loads a token field, a longer window could
/// expose a usable token prefix. 32 chars is enough to recognise a JSON error
/// envelope (`{"error":"invalid_grant"...`) or an HTML gateway page while
/// surfacing at most a non-sensitive JWT header fragment.
fn redact_body(body: &str) -> String {
    format!(
        "{} bytes, prefix: {:?}",
        body.len(),
        body.chars().take(32).collect::<String>()
    )
}

/// Response from the identity provider's token endpoint.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct ProviderTokenResponse {
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    // Error fields — present when the provider rejects the exchange.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // surfaced through the `error` message
    pub error_description: Option<String>,
}

/// POST the authorization code to the provider's token endpoint.
///
/// This is the **public-client** PKCE exchange: no `client_secret` is sent.
/// The provider validates the `code_verifier` against the `code_challenge`
/// that was included in the original authorization request.
///
/// ## CORS requirement
///
/// The provider's token endpoint must include CORS headers that allow the
/// browser origin.  All major OIDC providers (Google, Okta, Keycloak,
/// Microsoft Entra) do this for public clients.  Providers that require a
/// `client_secret` even for PKCE (confidential clients) cannot use this
/// flow — use `POST /api/v1/oauth/exchange` instead.
pub(crate) async fn exchange_code_with_provider(
    token_endpoint: &str,
    code: &str,
    code_verifier: &str,
    client_id: &str,
    redirect_uri: &str,
) -> Result<ProviderTokenResponse, String> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", code_verifier),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
    ];

    let resp = reqwest::Client::new()
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            format!(
                "Token exchange request to {token_endpoint} failed: {e}. \
                 Ensure the provider allows CORS requests from this origin."
            )
        })?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read token response body: {e}"))?;

    let token_resp: ProviderTokenResponse = serde_json::from_str(&body).map_err(|e| {
        log::error!(
            "Failed to parse token response (HTTP {status}): {e} — body: {}",
            redact_body(&body)
        );
        format!(
            "The identity provider returned an unexpected response (HTTP {status}). \
             Please try again."
        )
    })?;

    if let Some(ref err) = token_resp.error {
        let desc = token_resp
            .error_description
            .as_deref()
            .unwrap_or("no description");
        return Err(format!("Token endpoint error '{err}': {desc}"));
    }

    if !status.is_success() {
        log::error!(
            "Token endpoint returned HTTP {status}: {}",
            redact_body(&body)
        );
        return Err(format!(
            "Sign-in failed: the identity provider returned HTTP {status}. \
             Please try again."
        ));
    }

    Ok(token_resp)
}

/// Outcome classification for a refresh-token grant, so the caller can tell a
/// dead token apart from a recoverable transport failure.
pub(crate) enum RefreshError {
    /// Provider definitively rejected the grant (invalid_grant, or a 4xx with
    /// an error body) — the refresh token is dead and must be cleared.
    Rejected(String),
    /// Could not reach/parse the provider (network, CORS, 5xx, bad JSON) — the
    /// token may still be valid; the caller must NOT clear it.
    Transient(String),
}

/// Exchange a `refresh_token` for a fresh access/id token at the provider's
/// token endpoint.
///
/// This is the **public-client** refresh grant: no `client_secret` is sent
/// (mirrors [`exchange_code_with_provider`]). It is used by the PKCE flow when
/// the provider bearer expires mid-session. The same CORS requirement applies —
/// see [`exchange_code_with_provider`] for details.
///
/// Errors are classified as [`RefreshError::Rejected`] (the token is dead) vs
/// [`RefreshError::Transient`] (recoverable — the caller must NOT clear the
/// token). No error payload ever carries a token value.
pub(crate) async fn refresh_with_provider(
    token_endpoint: &str,
    refresh_token: &str,
    client_id: &str,
) -> Result<ProviderTokenResponse, RefreshError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];

    let resp = reqwest::Client::new()
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .map_err(|e| {
            // Network / CORS failure — the token may still be valid.
            RefreshError::Transient(format!(
                "Token refresh request to {token_endpoint} failed: {e}. \
                 Ensure the provider allows CORS requests from this origin."
            ))
        })?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        // Body-read failure — transport-level, treat as transient.
        RefreshError::Transient(format!("Failed to read token response body: {e}"))
    })?;

    let token_resp: ProviderTokenResponse = serde_json::from_str(&body).map_err(|e| {
        log::error!(
            "Failed to parse token response (HTTP {status}): {e} — body: {}",
            redact_body(&body)
        );
        // Unparseable body — could be a transient gateway/HTML error page; the
        // token may still be valid, so do not treat this as a rejection.
        RefreshError::Transient(format!(
            "The identity provider returned an unexpected response (HTTP {status}). \
             Please try again."
        ))
    })?;

    // An explicit `error` body is always a definitive rejection, regardless of
    // the HTTP status — check it BEFORE the status-class split below.
    if let Some(ref err) = token_resp.error {
        let desc = token_resp
            .error_description
            .as_deref()
            .unwrap_or("no description");
        return Err(RefreshError::Rejected(format!(
            "Token endpoint error '{err}': {desc}"
        )));
    }

    if !status.is_success() {
        log::error!(
            "Token endpoint returned HTTP {status}: {}",
            redact_body(&body)
        );
        if status.is_server_error() {
            // 5xx: server-side and retryable — the token may still be valid.
            return Err(RefreshError::Transient(format!(
                "Token refresh failed: the identity provider returned HTTP {status} \
                 (server-side; will retry)."
            )));
        }
        // 4xx (without an explicit error body): treat as a definitive rejection.
        return Err(RefreshError::Rejected(format!(
            "Token refresh failed: the identity provider returned HTTP {status}."
        )));
    }

    Ok(token_resp)
}

// ---------------------------------------------------------------------------
// State type returned by load_pkce_state
// ---------------------------------------------------------------------------

/// PKCE values retrieved from session-scoped storage in the callback page.
#[derive(Debug, Clone)]
pub struct SavedPkceState {
    /// The original code verifier — sent to the token endpoint.
    pub code_verifier: String,
    /// The CSRF state token — must match the `state` query parameter in the
    /// callback URL.
    pub state: String,
    /// The nonce sent in the authorization request — validated against the
    /// id_token.
    pub nonce: String,
    /// Where to navigate after a successful login (may be `None`).
    pub return_to: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests (pure-Rust parts only — crypto calls require a browser environment)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_challenge_is_deterministic() {
        // The same verifier must always produce the same challenge.
        let verifier = "dGhpcyBpcyBhIHRlc3QgdmVyaWZpZXIgc3RyaW5n";
        let c1 = derive_code_challenge(verifier);
        let c2 = derive_code_challenge(verifier);
        assert_eq!(c1, c2);
        // Must be non-empty Base64url without '=' padding.
        assert!(!c1.is_empty());
        assert!(!c1.contains('='));
    }

    #[test]
    fn code_challenge_differs_from_verifier() {
        let verifier = "dGhpcyBpcyBhIHRlc3QgdmVyaWZpZXIgc3RyaW5n";
        let challenge = derive_code_challenge(verifier);
        assert_ne!(verifier, challenge);
    }

    #[test]
    fn to_hex_produces_lowercase_pairs() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(to_hex(&[0xab, 0xcd]), "abcd");
    }

    #[test]
    fn redact_body_bounds_output_and_omits_full_body() {
        // A token-bearing JSON body well over the 32-char prefix cap. The secret
        // is a long, unique run that must NOT survive into the redacted output.
        let secret = "x".repeat(200);
        let big = format!("{{\"access_token\":\"{secret}\",\"token_type\":\"Bearer\"}}");

        let redacted = redact_body(&big);

        // The full body / full secret must never appear in the redacted output.
        // This test FAILS if redact_body is reverted to logging the raw body
        // (e.g. `format!("{body}")`), which would emit the entire token.
        assert!(
            !redacted.contains(&secret),
            "redacted output must not contain the full secret run"
        );
        assert!(
            !redacted.contains(&big),
            "redacted output must not contain the full body"
        );

        // Output is bounded: the 32-char prefix cap keeps it far smaller than the
        // ~225-char input. (A raw `format!("{body}")` would be >= big.len().)
        assert!(
            redacted.len() < big.len(),
            "redacted output ({}) must be shorter than the input ({})",
            redacted.len(),
            big.len()
        );
    }
}
