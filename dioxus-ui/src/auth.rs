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
//! `id_token` in session-scoped storage under the key `"vc_id_token"`.
//!
//! Storage is managed through [`dioxus_sdk_storage::SessionStorage`], which
//! maps to the browser's `sessionStorage` on web (tab-scoped, discarded when
//! the tab closes) and to an in-memory store on native platforms.  This
//! ensures tokens never outlive the session and are never persisted to disk.
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
use dioxus_sdk_storage::{SessionStorage, StorageBacking};
use gloo_utils::window;
use serde::Deserialize;
use videocall_meeting_types::responses::ProfileResponse;

pub type UserProfile = ProfileResponse;

// ---------------------------------------------------------------------------
// sessionStorage key for id_token
// ---------------------------------------------------------------------------

/// Session-storage key for the provider id_token JWT.
///
/// Using session-scoped storage (browser `sessionStorage` on web; in-memory
/// on native) limits token lifetime to the current session and prevents the
/// token from persisting after the user closes the tab or the app exits.
const ID_TOKEN_KEY: &str = "vc_id_token";

/// Session-storage key for the provider access token.
///
/// The access token is stored alongside the id_token.  It is sent as the
/// `Authorization: Bearer` credential on all meeting-api requests.  The id_token
/// is kept separately and used exclusively for reading user identity claims
/// (display name, user ID) — the access token is treated as opaque.
const ACCESS_TOKEN_KEY: &str = "vc_access_token";

/// Session-storage key for the user's canonical identifier extracted from the
/// validated id_token payload (email if present, otherwise `sub`).
const PROFILE_USER_ID_KEY: &str = "vc_profile_user_id";

/// Session-storage key for the user's display name extracted from the
/// validated id_token payload.
const PROFILE_DISPLAY_NAME_KEY: &str = "vc_profile_display_name";

/// Session-storage key for the cached provider auth URL (set when the value
/// was obtained from `GET /api/v1/oauth/provider-config` rather than from
/// `window.__APP_CONFIG`).
const CACHED_AUTH_URL_KEY: &str = "vc_oauth_cached_auth_url";
const CACHED_CLIENT_ID_KEY: &str = "vc_oauth_cached_client_id";

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

/// Read the stored provider id_token from session-scoped storage.
///
/// On web this reads from the browser's `sessionStorage`.  On native it reads
/// from the in-memory session store.
pub fn get_stored_id_token() -> Option<String> {
    SessionStorage::get::<Option<String>>(&ID_TOKEN_KEY.to_string())
        .flatten()
        .filter(|t| !t.is_empty())
}

/// Store the provider id_token in session-scoped storage.
pub fn store_id_token(token: &str) {
    SessionStorage::set(ID_TOKEN_KEY.to_string(), &Some(token.to_string()));
}

/// Clear the stored id_token from session-scoped storage.
pub fn clear_id_token() {
    SessionStorage::set(ID_TOKEN_KEY.to_string(), &None::<String>);
}

// ---------------------------------------------------------------------------
// Access-token storage
// ---------------------------------------------------------------------------

/// Read the stored provider access token from session-scoped storage.
///
/// Returns `None` when no OAuth exchange has been completed in the current
/// session or when the token was explicitly cleared.
///
/// The access token is **opaque** from the UI's perspective — it is passed
/// through as-is to the meeting-api and never decoded or inspected by the
/// browser.
pub fn get_stored_access_token() -> Option<String> {
    SessionStorage::get::<Option<String>>(&ACCESS_TOKEN_KEY.to_string())
        .flatten()
        .filter(|t| !t.is_empty())
}

/// Store the provider access token in session-scoped storage.
///
/// Called by the OAuth callback page after a successful token exchange.
pub fn store_access_token(token: &str) {
    SessionStorage::set(ACCESS_TOKEN_KEY.to_string(), &Some(token.to_string()));
}

/// Clear the stored access token from session-scoped storage.
///
/// Called on logout so subsequent requests are unauthenticated immediately,
/// even before the browser navigation to the logout endpoint completes.
pub fn clear_access_token() {
    SessionStorage::set(ACCESS_TOKEN_KEY.to_string(), &None::<String>);
}

// ---------------------------------------------------------------------------
// User profile cache
// ---------------------------------------------------------------------------

/// Persist the user profile claims extracted from a validated id_token to
/// session-scoped storage.
///
/// Called by the OAuth callback page immediately after
/// `decode_and_validate_id_token` succeeds.  Storing the claims here lets
/// [`get_user_profile`] return a result synchronously (from cache) without a
/// network round-trip to `GET /profile`.
///
/// `user_id` is the canonical user identifier — email when present in the
/// token, otherwise the opaque `sub` claim.
/// `display_name` is the raw display name resolved from the token's `name`,
/// `given_name`/`family_name`, `email`, or `sub` claims in that order.
pub fn store_user_profile(user_id: &str, display_name: &str) {
    SessionStorage::set(PROFILE_USER_ID_KEY.to_string(), &Some(user_id.to_string()));
    SessionStorage::set(
        PROFILE_DISPLAY_NAME_KEY.to_string(),
        &Some(display_name.to_string()),
    );
}

/// Read the cached user profile from session-scoped storage.
///
/// Returns `Some(UserProfile)` when both `user_id` and `display_name` are
/// present (set by the OAuth callback page after a successful token exchange).
/// Returns `None` when no profile has been cached yet.
pub fn get_stored_user_profile() -> Option<UserProfile> {
    let user_id = SessionStorage::get::<Option<String>>(&PROFILE_USER_ID_KEY.to_string())
        .flatten()
        .filter(|s| !s.is_empty())?;
    let name = SessionStorage::get::<Option<String>>(&PROFILE_DISPLAY_NAME_KEY.to_string())
        .flatten()
        .unwrap_or_default();
    Some(UserProfile { user_id, name })
}

/// Clear the cached user profile from session-scoped storage.
///
/// Called on logout so stale profile data cannot be observed after the
/// user signs out.
pub fn clear_user_profile() {
    SessionStorage::set(PROFILE_USER_ID_KEY.to_string(), &None::<String>);
    SessionStorage::set(PROFILE_DISPLAY_NAME_KEY.to_string(), &None::<String>);
}

// ---------------------------------------------------------------------------
// Session / profile
// ---------------------------------------------------------------------------

/// Check whether the current session is still valid.
///
/// **Fast-path:** when `oauthEnabled` is true and neither the access token
/// nor the id_token is stored, the server will always return 401 — skip the
/// network round-trip and fail immediately.
pub async fn check_session() -> anyhow::Result<()> {
    if oauth_enabled().unwrap_or(false)
        && get_stored_access_token().is_none()
        && get_stored_id_token().is_none()
    {
        return Err(anyhow!("no OAuth token stored; authentication required"));
    }
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.check_session().await.map_err(|e| anyhow!("{e}"))
}

/// Return the authenticated user's profile.
///
/// **Fast-path (OAuth):** returns the profile cached in `sessionStorage` by
/// the OAuth callback page.  The callback stores the `user_id` and
/// `display_name` extracted from the validated id_token immediately after a
/// successful token exchange, so this function can return without any network
/// request.
///
/// **Fallback (no OAuth / no cached profile):** calls `GET /profile` on the
/// meeting-api.  This path is taken for deployments that do not use an
/// external OAuth provider (legacy HMAC session JWT mode) or in the unlikely
/// event that the cache is absent despite a valid session.
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    // Return the profile cached by the OAuth callback whenever it is present.
    // This avoids a network round-trip and works even before the first API
    // call completes.
    if let Some(profile) = get_stored_user_profile() {
        return Ok(profile);
    }

    // Fallback: ask the meeting-api.
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

    // Try the per-session cache to avoid a redundant fetch in the same session.
    let cached_url =
        SessionStorage::get::<Option<String>>(&CACHED_AUTH_URL_KEY.to_string()).flatten();
    let cached_id =
        SessionStorage::get::<Option<String>>(&CACHED_CLIENT_ID_KEY.to_string()).flatten();
    if let (Some(auth_url), Some(client_id)) = (cached_url, cached_id) {
        return Ok(OAuthParams {
            auth_url,
            client_id,
            redirect_url,
            scopes,
        });
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

    // Cache for subsequent flows in the same session.
    SessionStorage::set(
        CACHED_AUTH_URL_KEY.to_string(),
        &Some(result.auth_url.clone()),
    );
    SessionStorage::set(
        CACHED_CLIENT_ID_KEY.to_string(),
        &Some(result.client_id.clone()),
    );

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
    // Clear all session-scoped state before navigating away so subsequent
    // requests are immediately unauthenticated, even if the navigation is slow.
    clear_access_token();
    clear_id_token();
    clear_user_profile();
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
    // 1. Session-scoped storage (preferred — set by protected pages before navigating).
    let val = SessionStorage::get::<Option<String>>(&pkce::RETURN_TO_KEY.to_string())
        .flatten()
        .filter(|s| !s.is_empty());
    if val.is_some() {
        // Consume it — do_login owns the return_to from here.
        SessionStorage::set(pkce::RETURN_TO_KEY.to_string(), &None::<String>);
        return val;
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
