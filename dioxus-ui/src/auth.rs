// SPDX-License-Identifier: MIT OR Apache-2.0

//! Authentication module
//!
//! Handles OAuth session validation, user profile fetching, token storage,
//! logout, and — most importantly — **initiating the PKCE-based OIDC
//! authorization flow** directly from the browser.
//!
//! ## Login flow
//!
//! When authentication is required or requested the UI calls
//! [`start_oauth_flow`] (or the thin wrappers [`do_login`] /
//! [`redirect_to_login`]).  The function:
//!
//! 1. Resolves the provider's authorization endpoint URL and client ID from
//!    `window.__APP_CONFIG` (`oauthAuthUrl`, `oauthClientId`).  When
//!    `oauthAuthUrl` is absent (e.g. the backend resolved it via OIDC
//!    discovery but the value was not forwarded to the UI), it fetches
//!    `GET /api/v1/oauth/provider-config` and caches the result in
//!    `sessionStorage`.
//! 2. Generates a PKCE `code_verifier` / `code_challenge`, CSRF `state`, and
//!    OIDC `nonce` via `window.crypto.getRandomValues` (see [`crate::pkce`]).
//! 3. Saves the sensitive values in `sessionStorage` (tab-scoped, not
//!    persisted after the tab closes).
//! 4. Redirects the browser to the provider's authorization endpoint.
//!
//! The `client_secret` stays on the server; the browser only ever sees the
//! public `client_id` and the PKCE challenge.
//!
//! ## Token storage
//!
//! After the provider redirects back to `/auth/callback`, that page exchanges
//! the authorization code for tokens via `POST /api/v1/oauth/exchange`
//! (server-to-server, keeping the secret private) and stores the returned
//! `id_token` in `sessionStorage["vc_id_token"]`.
//!
//! Every call to [`meeting_api_client`](crate::constants::meeting_api_client)
//! reads the stored token and creates a client in `Bearer` mode.
//!
//! ## Logout
//!
//! [`logout`] clears the stored id_token and navigates the browser to the
//! meeting-api `/logout` endpoint so the provider session is also terminated
//! (RP-initiated logout via `end_session_endpoint` when configured).

use crate::constants::{
    login_url, logout_url, meeting_api_base_url, meeting_api_client, oauth_auth_url,
    oauth_client_id, oauth_enabled, oauth_redirect_url, oauth_scopes,
};
use crate::pkce::{self};
use anyhow::anyhow;
use gloo_utils::window;
use serde::Deserialize;
use videocall_meeting_types::responses::ProfileResponse;

pub type UserProfile = ProfileResponse;

// ---------------------------------------------------------------------------
// sessionStorage key for id_token
// ---------------------------------------------------------------------------

/// `sessionStorage` key for the provider id_token JWT.
///
/// Using `sessionStorage` (not `localStorage`) limits token lifetime to the
/// current browser tab and prevents the token from persisting after the user
/// closes the window.
const ID_TOKEN_KEY: &str = "vc_id_token";

/// `sessionStorage` key for the cached provider auth URL (set when the value
/// was obtained from `GET /api/v1/oauth/provider-config` rather than from
/// `window.__APP_CONFIG`).
const CACHED_AUTH_URL_KEY: &str = "vc_oauth_cached_auth_url";
const CACHED_CLIENT_ID_KEY: &str = "vc_oauth_cached_client_id";

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

/// Read the stored provider id_token from `sessionStorage`.
pub fn get_stored_id_token() -> Option<String> {
    window()
        .session_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(ID_TOKEN_KEY).ok().flatten())
        .filter(|t| !t.is_empty())
}

/// Store the provider id_token in `sessionStorage`.
pub fn store_id_token(token: &str) {
    if let Some(storage) = window().session_storage().ok().flatten() {
        let _ = storage.set_item(ID_TOKEN_KEY, token);
    }
}

/// Remove the stored id_token from `sessionStorage`.
pub fn clear_id_token() {
    if let Some(storage) = window().session_storage().ok().flatten() {
        let _ = storage.remove_item(ID_TOKEN_KEY);
    }
}

// ---------------------------------------------------------------------------
// Session / profile
// ---------------------------------------------------------------------------

/// Check whether the current session is still valid.
///
/// **Fast-path:** when `oauthEnabled` is true and no id_token is stored,
/// returns `Err` immediately without a network round-trip.
pub async fn check_session() -> anyhow::Result<()> {
    if oauth_enabled().unwrap_or(false) && get_stored_id_token().is_none() {
        return Err(anyhow!("no id_token stored; OAuth authentication required"));
    }
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.check_session().await.map_err(|e| anyhow!("{e}"))
}

/// Fetch the authenticated user's display name and user ID.
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.get_profile().await.map_err(|e| anyhow!("{e}"))
}

// ---------------------------------------------------------------------------
// OAuth provider config resolution
// ---------------------------------------------------------------------------

/// Response body of `GET /api/v1/oauth/provider-config`.
#[derive(Debug, Deserialize)]
struct ProviderConfigResponse {
    success: bool,
    result: ProviderConfigResult,
}

#[derive(Debug, Deserialize)]
struct ProviderConfigResult {
    #[serde(default)]
    auth_url: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    scopes: String,
}

/// Resolved OAuth parameters needed to build the authorization URL.
#[derive(Debug, Clone)]
pub struct OAuthParams {
    pub auth_url: String,
    pub client_id: String,
    pub redirect_url: String,
    pub scopes: String,
}

/// Resolve the OAuth provider parameters the browser needs to build the
/// authorization URL.
///
/// Priority:
/// 1. `window.__APP_CONFIG` (values injected at container start from env vars).
/// 2. A previously cached value in `sessionStorage` (set by a prior call to
///    this function).
/// 3. `GET /api/v1/oauth/provider-config` — the meeting-api returns the
///    post-OIDC-discovery values.  The response is cached in `sessionStorage`
///    so subsequent flows within the same tab are free.
async fn resolve_oauth_params() -> Result<OAuthParams, String> {
    // The redirect URL is always the dioxus-ui `/auth/callback` route; we
    // derive it from config or fall back to constructing it from the current
    // origin.
    let redirect_url = oauth_redirect_url().unwrap_or_else(|| {
        window()
            .location()
            .origin()
            .map(|o| format!("{o}/auth/callback"))
            .unwrap_or_default()
    });

    let scopes = oauth_scopes();

    // --- Auth URL & Client ID ---
    // Try config.js first (fast, synchronous).
    if let (Some(auth_url), Some(client_id)) = (oauth_auth_url(), oauth_client_id()) {
        return Ok(OAuthParams {
            auth_url,
            client_id,
            redirect_url,
            scopes,
        });
    }

    // Try the in-tab cache to avoid a redundant fetch in the same session.
    if let Some(storage) = window().session_storage().ok().flatten() {
        let cached_url = storage
            .get_item(CACHED_AUTH_URL_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
        let cached_id = storage
            .get_item(CACHED_CLIENT_ID_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
        if let (Some(auth_url), Some(client_id)) = (cached_url, cached_id) {
            return Ok(OAuthParams {
                auth_url,
                client_id,
                redirect_url,
                scopes,
            });
        }
    }

    // Fall back to the backend discovery endpoint.
    let base =
        meeting_api_base_url().map_err(|e| format!("Cannot build provider-config URL: {e}"))?;
    let url = format!("{base}/api/v1/oauth/provider-config");

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Failed to fetch provider config: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Provider config endpoint returned HTTP {status}: {body}"
        ));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read provider config response: {e}"))?;

    let parsed: ProviderConfigResponse = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse provider config: {e} — body: {text}"))?;

    if !parsed.success || parsed.result.auth_url.is_empty() {
        return Err(
            "OAuth is not configured on the server, or OAUTH_AUTH_URL was not resolved. \
             Set OAUTH_AUTH_URL in the dioxus-ui environment."
                .to_string(),
        );
    }

    let result = parsed.result;

    // Cache for subsequent flows in the same tab.
    if let Some(storage) = window().session_storage().ok().flatten() {
        let _ = storage.set_item(CACHED_AUTH_URL_KEY, &result.auth_url);
        let _ = storage.set_item(CACHED_CLIENT_ID_KEY, &result.client_id);
    }

    let scopes = if !result.scopes.is_empty() {
        result.scopes
    } else {
        scopes
    };

    Ok(OAuthParams {
        auth_url: result.auth_url,
        client_id: result.client_id,
        redirect_url,
        scopes,
    })
}

// ---------------------------------------------------------------------------
// Core flow
// ---------------------------------------------------------------------------

/// Start the PKCE-based OIDC authorization flow.
///
/// Generates fresh PKCE parameters, saves them in `sessionStorage`, and
/// redirects the browser to the provider's authorization endpoint.
///
/// `return_to` is the URL to navigate to after a successful login.  Pass
/// `None` to fall back to the home page.
///
/// This function is `async` because it may fetch the provider configuration
/// from `GET /api/v1/oauth/provider-config` when `OAUTH_AUTH_URL` is not set
/// in `window.__APP_CONFIG`.
pub async fn start_oauth_flow(return_to: Option<String>) {
    // 1. Resolve provider parameters.
    let params = match resolve_oauth_params().await {
        Ok(p) => p,
        Err(e) => {
            log::error!("Cannot start OAuth flow — provider config unavailable: {e}");
            return;
        }
    };

    // 2. Generate PKCE values.
    let pkce = pkce::generate_pkce_params();

    // 3. Persist in sessionStorage.
    pkce::save_pkce_state(&pkce, return_to.as_deref());

    // 4. Build the authorization URL.
    let auth_url = build_auth_url(&params, &pkce);

    log::info!(
        "Starting PKCE OIDC flow → {}",
        // Truncate the URL in logs to avoid leaking secrets (code_challenge is
        // safe, but the full URL can be long).
        &auth_url[..auth_url.len().min(120)]
    );

    // 5. Navigate.
    if let Err(e) = window().location().set_href(&auth_url) {
        log::error!("Failed to navigate to provider authorization URL: {e:?}");
    }
}

/// Build the authorization URL by appending required OIDC / PKCE query
/// parameters to the provider's `auth_url`.
fn build_auth_url(params: &OAuthParams, pkce: &pkce::PkceParams) -> String {
    format!(
        "{auth_url}\
         ?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &scope={scope}\
         &state={state}\
         &nonce={nonce}\
         &code_challenge={challenge}\
         &code_challenge_method=S256\
         &prompt=select_account",
        auth_url = params.auth_url,
        client_id = urlencoding::encode(&params.client_id),
        redirect_uri = urlencoding::encode(&params.redirect_url),
        scope = urlencoding::encode(&params.scopes),
        state = pkce.state,
        nonce = pkce.nonce,
        challenge = pkce.code_challenge,
    )
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Navigate to the backend OAuth login endpoint (legacy) or directly start
/// the client-side PKCE flow (when the provider auth URL is available).
///
/// This function is a **synchronous** wrapper: it spawns the async
/// [`start_oauth_flow`] internally so it can be called from Dioxus event
/// handlers and `use_effect` closures without `await`.
///
/// `return_to` is resolved in priority order:
/// 1. `sessionStorage["vc_oauth_return_to"]` (set by callers before navigating
///    to `/login`).
/// 2. The current URL search string (e.g. `/login?returnTo=…`).
/// 3. The current page origin root.
pub fn do_login() {
    // Determine `return_to` synchronously before the async task runs.
    let return_to = resolve_return_to_for_do_login();

    if oauth_enabled().unwrap_or(false) {
        // New flow: generate PKCE and redirect directly to the provider.
        wasm_bindgen_futures::spawn_local(async move {
            start_oauth_flow(return_to).await;
        });
    } else {
        // OAuth is not configured — navigate to the backend /login endpoint as
        // a fallback (the backend will return an error, but this path should
        // never be reached in a correctly configured deployment).
        let url = match build_legacy_login_url(return_to.as_deref()) {
            Ok(u) => u,
            Err(e) => {
                log::error!("Failed to build login URL: {e}");
                return;
            }
        };
        let _ = window().location().set_href(&url);
    }
}

/// Redirect the browser to the OAuth login flow, encoding the **current page
/// URL** as the `return_to` destination.
///
/// This is the function called by protected pages (meeting page, settings
/// page) when an unauthenticated request is detected.
///
/// Like [`do_login`] this is a synchronous wrapper around [`start_oauth_flow`]
/// and can be called from any context.
pub fn redirect_to_login() {
    let return_to = window().location().href().ok().filter(|s| !s.is_empty());

    if oauth_enabled().unwrap_or(false) {
        wasm_bindgen_futures::spawn_local(async move {
            start_oauth_flow(return_to).await;
        });
    } else {
        let url = match build_legacy_login_url(return_to.as_deref()) {
            Ok(u) => u,
            Err(e) => {
                log::error!("Failed to build login URL: {e}");
                return;
            }
        };
        let _ = window().location().set_href(&url);
    }
}

// ---------------------------------------------------------------------------
// Logout
// ---------------------------------------------------------------------------

/// Navigate the browser to the meeting-api `/logout` endpoint, clearing the
/// stored id_token first.
///
/// Any `303` redirect to the OIDC provider's `end_session_endpoint` is
/// followed as a real page load (terminating the provider session too).
pub fn logout() -> Result<(), String> {
    clear_id_token();
    let url = logout_url()?;
    window()
        .location()
        .set_href(&url)
        .map_err(|e| format!("Navigation to logout URL failed: {e:?}"))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the `returnTo` value used by [`do_login`].
fn resolve_return_to_for_do_login() -> Option<String> {
    // 1. sessionStorage (preferred — set by protected pages before navigating).
    if let Some(storage) = window().session_storage().ok().flatten() {
        let val = storage
            .get_item(pkce::RETURN_TO_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty());
        if val.is_some() {
            // Consume it — do_login owns the return_to from here.
            let _ = storage.remove_item(pkce::RETURN_TO_KEY);
            return val;
        }
    }

    // 2. URL search string (e.g. direct navigation to `/login?returnTo=…`).
    if let Ok(search) = window().location().search() {
        if !search.is_empty() {
            // Re-parse to extract the returnTo value properly.
            let decoded = search.trim_start_matches('?');
            for pair in decoded.split('&') {
                let mut parts = pair.splitn(2, '=');
                if let (Some("returnTo"), Some(v)) = (parts.next(), parts.next()) {
                    if let Ok(decoded) = urlencoding::decode(v) {
                        return Some(decoded.into_owned());
                    }
                }
            }
        }
    }

    // 3. Origin root.
    window().location().origin().ok().map(|o| format!("{o}/"))
}

/// Build the legacy backend `/login` URL with an optional `returnTo` param.
/// Used when OAuth is not configured (fallback path).
fn build_legacy_login_url(return_to: Option<&str>) -> Result<String, String> {
    let base = login_url()?;
    match return_to {
        Some(rt) => Ok(format!("{base}?returnTo={}", urlencoding::encode(rt))),
        None => Ok(base),
    }
}

/// Re-export so callers that import from `auth` get the state type directly.
pub use crate::pkce::SavedPkceState;

/// Load the PKCE state saved by [`start_oauth_flow`].
pub fn load_pkce_state() -> Option<SavedPkceState> {
    pkce::load_pkce_state()
}

/// Clear all PKCE state from `sessionStorage`.
/// Delegates to [`pkce::clear_pkce_state`].
pub fn clear_pkce_state() {
    pkce::clear_pkce_state();
}
