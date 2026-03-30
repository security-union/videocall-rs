// SPDX-License-Identifier: MIT OR Apache-2.0

//! PKCE (Proof Key for Code Exchange, RFC 7636) helpers for the browser-side
//! OIDC flow.
//!
//! ## Responsibilities
//!
//! - Generate a cryptographically random `code_verifier`, derive the
//!   `code_challenge` (Base64url-SHA-256), and produce random `state` and
//!   `nonce` values using `window.crypto.getRandomValues`.
//! - Persist the generated values in `sessionStorage` so they survive the
//!   redirect to the identity provider and can be retrieved by the
//!   `/auth/callback` page.
//! - Validate the CSRF `state` parameter echoed back by the provider.
//!
//! ## Security properties
//!
//! | Value | Length | Encoding | Purpose |
//! |---|---|---|---|
//! | `code_verifier` | 32 bytes | Base64url (no padding) | PKCE verifier sent to token endpoint |
//! | `code_challenge` | SHA-256 of verifier | Base64url (no padding) | Sent in auth request |
//! | `state` | 16 bytes | hex | CSRF protection (validated in callback) |
//! | `nonce` | 16 bytes | hex | Binds id_token to this session |
//!
//! All values are stored in `sessionStorage` (tab-scoped, not persisted after
//! the tab closes, inaccessible to cross-origin scripts).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use gloo_utils::window;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// sessionStorage keys
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
/// Panics when the Web Crypto API is unavailable (should never happen in a
/// modern browser).
fn get_random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("window.crypto.getRandomValues failed");
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
// sessionStorage persistence
// ---------------------------------------------------------------------------

/// Persist the PKCE parameters and optional `return_to` URL in
/// `sessionStorage` so they survive the redirect to the provider.
///
/// Existing values are overwritten — each call to `start_oauth_flow` starts a
/// fresh PKCE session.
pub fn save_pkce_state(params: &PkceParams, return_to: Option<&str>) {
    let Some(storage) = window().session_storage().ok().flatten() else {
        log::error!("sessionStorage unavailable — PKCE state cannot be saved");
        return;
    };
    let _ = storage.set_item(PKCE_VERIFIER_KEY, &params.code_verifier);
    let _ = storage.set_item(PKCE_STATE_KEY, &params.state);
    let _ = storage.set_item(PKCE_NONCE_KEY, &params.nonce);
    if let Some(rt) = return_to {
        let _ = storage.set_item(RETURN_TO_KEY, rt);
    } else {
        // Clear a stale return_to from a previous attempt.
        let _ = storage.remove_item(RETURN_TO_KEY);
    }
}

/// Load the saved PKCE state from `sessionStorage`.
///
/// Returns `None` when any required key is missing (e.g. the user opened a
/// fresh tab directly on `/auth/callback` without going through the login
/// flow).
pub fn load_pkce_state() -> Option<SavedPkceState> {
    let storage = window().session_storage().ok().flatten()?;
    let verifier = storage.get_item(PKCE_VERIFIER_KEY).ok().flatten()?;
    let state = storage.get_item(PKCE_STATE_KEY).ok().flatten()?;
    let nonce = storage.get_item(PKCE_NONCE_KEY).ok().flatten()?;
    let return_to = storage.get_item(RETURN_TO_KEY).ok().flatten();
    Some(SavedPkceState {
        code_verifier: verifier,
        state,
        nonce,
        return_to,
    })
}

/// Remove all PKCE keys from `sessionStorage`.
///
/// Called by the callback page after successfully exchanging the code so the
/// one-time values cannot be replayed.
pub fn clear_pkce_state() {
    let Some(storage) = window().session_storage().ok().flatten() else {
        return;
    };
    let _ = storage.remove_item(PKCE_VERIFIER_KEY);
    let _ = storage.remove_item(PKCE_STATE_KEY);
    let _ = storage.remove_item(PKCE_NONCE_KEY);
    let _ = storage.remove_item(RETURN_TO_KEY);
}

// ---------------------------------------------------------------------------
// State type returned by load_pkce_state
// ---------------------------------------------------------------------------

/// PKCE values retrieved from `sessionStorage` in the callback page.
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
}
