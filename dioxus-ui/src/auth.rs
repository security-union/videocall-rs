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
//! the authorization code **directly with the identity provider** (public-client
//! PKCE — no `client_secret` in the browser) and stores the returned tokens in
//! `sessionStorage` under `"vc_access_token"` and `"vc_id_token"`.
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
    login_url, logout_url, meeting_api_client, oauth_auth_url, oauth_client_id, oauth_prompt,
    oauth_redirect_url, oauth_scopes,
};
use crate::pkce::{self};
use anyhow::anyhow;
use dioxus_sdk_storage::{SessionStorage, StorageBacking};
use gloo_utils::window;
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

/// Session-storage key for the stable guest participant ID (`guest:<uuid>`)
/// that is reused across re-joins within the same browser tab.
const GUEST_SESSION_ID_KEY: &str = "vc_guest_session_id";

/// Read the stable guest session ID for the current tab, if any.
pub fn get_guest_session_id() -> Option<String> {
    SessionStorage::get::<Option<String>>(&GUEST_SESSION_ID_KEY.to_string()).flatten()
}

/// Persist the guest session ID so re-joins reuse the same participant row.
pub fn store_guest_session_id(id: &str) {
    SessionStorage::set(GUEST_SESSION_ID_KEY.to_string(), &Some(id.to_string()));
}

/// Clear the guest session ID.
pub fn clear_guest_session_id() {
    if let Ok(Some(storage)) = window().session_storage() {
        let _ = storage.remove_item(GUEST_SESSION_ID_KEY);
    }
}

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
    // Fast-path: skip the network call when a guest session ID is present,
    // meaning this tab is (or was) a guest — no OAuth session cookie exists.
    if get_guest_session_id().is_some() {
        clear_guest_session_id();
        return Err(anyhow!("guest session; no OAuth session cookie"));
    }
    if crate::constants::is_pkce_flow()
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
/// **Fast-path (OAuth):** returns the profile cached by the OAuth callback page,
/// but only when at least one token is still present.  If both tokens are absent
/// (e.g. after an explicit logout) the cache is stale and must not be returned.
///
/// > **Contract:** call [`check_session`] before this function on any protected
/// > page.  `check_session` validates the token server-side (detecting expiry)
/// > and redirects to login before this function is reached when the session is
/// > invalid.
///
/// **Fallback (no OAuth / no cached profile):** calls `GET /profile` on the
/// meeting-api.  This path is taken for legacy HMAC session JWT deployments or
/// in the unlikely event that the profile cache is absent despite a valid session.
pub async fn get_user_profile() -> anyhow::Result<UserProfile> {
    // Fast-path: cached profile from the OAuth callback page.
    //
    // Guard: only serve the cache when at least one token is present.  Tokens
    // and the profile cache are cleared together on logout; if both are absent
    // the user has signed out and the cached profile is stale.
    if crate::constants::is_pkce_flow()
        && (get_stored_access_token().is_some() || get_stored_id_token().is_some())
    {
        if let Some(profile) = get_stored_user_profile() {
            return Ok(profile);
        }
        // Token present but no cached profile — fall through to the API call.
    }

    // Fallback: ask the meeting-api.
    let client = meeting_api_client().map_err(|e| anyhow!("Config error: {e}"))?;
    client.get_profile().await.map_err(|e| anyhow!("{e}"))
}

// ---------------------------------------------------------------------------
// OAuth provider config resolution
// ---------------------------------------------------------------------------

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
/// 1. `window.__APP_CONFIG` (injected at container start from env vars).
/// 2. `GET /api/v1/oauth/provider-config` via the shared
///    [`crate::provider_config::fetch_provider_config`] helper, which caches
///    the response in `sessionStorage` for the lifetime of the tab.
async fn resolve_oauth_params() -> Result<OAuthParams, String> {
    let redirect_url = oauth_redirect_url().unwrap_or_else(|| {
        window()
            .location()
            .origin()
            .map(|o| format!("{o}/auth/callback"))
            .unwrap_or_default()
    });

    let scopes = oauth_scopes();

    // Fast path: both values present in config.js.
    if let (Some(auth_url), Some(client_id)) = (oauth_auth_url(), oauth_client_id()) {
        return Ok(OAuthParams {
            auth_url,
            client_id,
            redirect_url,
            scopes,
        });
    }

    // Backend fallback — shared fetch + cache.
    let cfg = crate::provider_config::fetch_provider_config()
        .await
        .map_err(|e| format!("Provider config unavailable: {e}"))?;

    if cfg.auth_url.is_empty() || cfg.client_id.is_empty() {
        return Err(
            "OAuth is not configured on the server, or OAUTH_AUTH_URL was not resolved. \
             Set OAUTH_AUTH_URL in the dioxus-ui environment."
                .to_string(),
        );
    }

    let scopes = if !cfg.scopes.is_empty() {
        cfg.scopes
    } else {
        scopes
    };

    Ok(OAuthParams {
        auth_url: cfg.auth_url,
        client_id: cfg.client_id,
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
///
/// The optional `prompt` parameter is included only when `OAUTH_PROMPT` is
/// non-empty.  Omitting it when it is empty preserves compatibility with
/// providers that do not recognise non-standard prompt values.
fn build_auth_url(params: &OAuthParams, pkce: &pkce::PkceParams) -> String {
    let mut url = format!(
        "{auth_url}\
         ?response_type=code\
         &client_id={client_id}\
         &redirect_uri={redirect_uri}\
         &scope={scope}\
         &state={state}\
         &nonce={nonce}\
         &code_challenge={challenge}\
         &code_challenge_method=S256",
        auth_url = params.auth_url,
        client_id = urlencoding::encode(&params.client_id),
        redirect_uri = urlencoding::encode(&params.redirect_url),
        scope = urlencoding::encode(&params.scopes),
        state = pkce.state,
        nonce = pkce.nonce,
        challenge = pkce.code_challenge,
    );
    if let Some(prompt) = oauth_prompt() {
        url.push_str(&format!("&prompt={}", urlencoding::encode(&prompt)));
    }
    url
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

    if crate::constants::is_pkce_flow() {
        // Client-side PKCE: generate challenge and redirect directly to the provider.
        wasm_bindgen_futures::spawn_local(async move {
            start_oauth_flow(return_to).await;
        });
    } else {
        // Server-side OAuth (or OAuth disabled): redirect to the backend /login
        // endpoint which exchanges the authorization code using the client secret.
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

    if crate::constants::is_pkce_flow() {
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

/// Redirect the browser to the guest entry point for a meeting.
///
/// Called when an unauthenticated user tries to join a meeting that allows guests.
pub fn redirect_to_guest(meeting_id: &str) {
    let url = format!("/meeting/{meeting_id}/guest");
    if let Err(e) = window().location().set_href(&url) {
        log::error!("Failed to navigate to guest URL: {e:?}");
    }
}

/// When the user is not authenticated, check if the meeting allows guests
/// and redirect accordingly.
pub async fn handle_not_authenticated(meeting_id: &str) {
    let allow_guests = crate::meeting_api::get_meeting_guest_info(meeting_id)
        .await
        .map(|info| info.allow_guests)
        .unwrap_or(false);
    if allow_guests {
        redirect_to_guest(meeting_id);
    } else {
        redirect_to_login();
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
    // 1. Session-scoped storage (preferred — written by protected pages before
    //    they redirect to /login).
    //
    //    Read but do NOT clear here.  If start_oauth_flow fails before
    //    save_pkce_state is called (e.g. provider config unreachable), the
    //    value survives for the user's next login attempt.  It is overwritten
    //    by save_pkce_state on a successful redirect and cleared by
    //    clear_pkce_state after a completed exchange.
    let val = SessionStorage::get::<Option<String>>(&pkce::RETURN_TO_KEY.to_string())
        .flatten()
        .filter(|s| !s.is_empty());
    if val.is_some() {
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
