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
//! Storage is managed through plain-text `web_sys` `sessionStorage` helpers
//! (see [`crate::context`]), which map to the browser's `sessionStorage` on
//! web (tab-scoped, discarded when the tab closes).  This ensures tokens
//! never outlive the session and are never persisted to disk.
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
    login_url, meeting_api_client, oauth_auth_url, oauth_client_id, oauth_prompt,
    oauth_redirect_url, oauth_scopes,
};
use crate::context::{read_session_storage, remove_session_storage, write_session_storage};
use crate::pkce::{self};
use anyhow::anyhow;
use gloo_utils::window;
use videocall_meeting_types::responses::ProfileResponse;

pub type UserProfile = ProfileResponse;

// ---------------------------------------------------------------------------
// sessionStorage key for id_token
// ---------------------------------------------------------------------------

/// Session-storage key for the provider id_token JWT.
///
/// Using the browser's `sessionStorage` (tab-scoped, discarded on tab close)
/// limits token lifetime to the current session and prevents the token from
/// persisting after the user closes the tab.
const ID_TOKEN_KEY: &str = "vc_id_token";

/// Session-storage key for the provider access token.
///
/// The access token is stored alongside the id_token.  It is sent as the
/// `Authorization: Bearer` credential on all meeting-api requests.  The id_token
/// is kept separately and used exclusively for reading user identity claims
/// (display name, user ID) — the access token is treated as opaque.
const ACCESS_TOKEN_KEY: &str = "vc_access_token";

/// Session-storage key for the provider refresh token.
///
/// Like the access/id tokens this is session-scoped (browser `sessionStorage`,
/// tab-scoped, discarded on tab close) and is **never persisted to disk**. It
/// is only present on PKCE deployments where the IdP grants `offline_access`;
/// on server-side-OAuth (cookie) deployments it is never set or read.
const REFRESH_TOKEN_KEY: &str = "vc_refresh_token";

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
    read_session_storage(GUEST_SESSION_ID_KEY)
}

/// Persist the guest session ID so re-joins reuse the same participant row.
pub fn store_guest_session_id(id: &str) {
    write_session_storage(GUEST_SESSION_ID_KEY, id);
}

/// Clear the guest session ID.
pub fn clear_guest_session_id() {
    remove_session_storage(GUEST_SESSION_ID_KEY);
}

// ---------------------------------------------------------------------------
// Token storage
// ---------------------------------------------------------------------------

/// Read the stored provider id_token from session-scoped storage.
///
/// On web this reads from the browser's `sessionStorage`.  On native it reads
/// from the in-memory session store.
pub fn get_stored_id_token() -> Option<String> {
    read_session_storage(ID_TOKEN_KEY)
}

/// Store the provider id_token in session-scoped storage.
pub fn store_id_token(token: &str) {
    write_session_storage(ID_TOKEN_KEY, token);
}

/// Clear the stored id_token from session-scoped storage.
pub fn clear_id_token() {
    remove_session_storage(ID_TOKEN_KEY);
    // Invalidate any in-flight refresh so its result is not written back.
    bump_auth_clear_epoch();
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
    read_session_storage(ACCESS_TOKEN_KEY)
}

/// Store the provider access token in session-scoped storage.
///
/// Called by the OAuth callback page after a successful token exchange.
pub fn store_access_token(token: &str) {
    write_session_storage(ACCESS_TOKEN_KEY, token);
}

/// Clear the stored access token from session-scoped storage.
///
/// Called on logout so subsequent requests are unauthenticated immediately,
/// even before the browser navigation to the logout endpoint completes.
pub fn clear_access_token() {
    remove_session_storage(ACCESS_TOKEN_KEY);
    // Invalidate any in-flight refresh so its result is not written back.
    bump_auth_clear_epoch();
}

// ---------------------------------------------------------------------------
// Auth clear-epoch (logout-resurrection guard)
// ---------------------------------------------------------------------------

thread_local! {
    static AUTH_CLEAR_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn auth_clear_epoch() -> u64 {
    AUTH_CLEAR_EPOCH.with(|e| e.get())
}

/// Invalidate any in-flight [`refresh_access_token`] so a refresh POST that
/// resolves AFTER a token-clear/logout does NOT write tokens back into
/// sessionStorage.
///
/// `refresh_access_token` snapshots this epoch before its network POST and
/// compares it again after the POST resolves; a mismatch means a token-clear
/// (logout calls `clear_*_token`, each of which bumps this) raced the in-flight
/// refresh, so the result is discarded instead of being persisted. Without this,
/// a logout that races an in-flight refresh would silently re-authenticate the
/// user (the logout-resurrection hazard) until a hard reload.
pub fn bump_auth_clear_epoch() {
    AUTH_CLEAR_EPOCH.with(|e| e.set(e.get().wrapping_add(1)));
}

// ---------------------------------------------------------------------------
// Refresh-token storage (PKCE flow only)
// ---------------------------------------------------------------------------

/// Read the stored provider refresh token from session-scoped storage.
///
/// Returns `None` when the IdP did not grant `offline_access`, when no OAuth
/// exchange has been completed in this session, or when it was cleared.
pub fn get_stored_refresh_token() -> Option<String> {
    read_session_storage(REFRESH_TOKEN_KEY)
}

/// Store the provider refresh token in session-scoped storage.
///
/// Called by the OAuth callback page after a successful exchange and on each
/// successful refresh that rotates the token.
pub fn store_refresh_token(token: &str) {
    write_session_storage(REFRESH_TOKEN_KEY, token);
}

/// Clear the stored refresh token from session-scoped storage.
pub fn clear_refresh_token() {
    remove_session_storage(REFRESH_TOKEN_KEY);
    // Invalidate any in-flight refresh so its result is not written back.
    bump_auth_clear_epoch();
}

/// Attempt to refresh the provider access/id token using the stored refresh
/// token (PKCE flow only).
///
/// On success the new access token (and rotated id/refresh tokens, when the
/// provider returns them) are written back to session storage, and the new
/// Bearer credential is returned. Mirrors the access-token-preferred,
/// id-token-fallback ordering used by
/// [`meeting_api_client`](crate::constants::meeting_api_client).
///
/// No token VALUE is ever logged — only attempt/success/failure markers and
/// the provider's (token-free) error string on failure.
pub async fn refresh_access_token() -> Result<String, String> {
    let refresh_token =
        get_stored_refresh_token().ok_or_else(|| "no refresh token stored".to_string())?;
    let token_endpoint = crate::constants::oauth_token_url()
        .ok_or_else(|| "OAUTH_TOKEN_URL not configured".to_string())?;
    let client_id = crate::constants::oauth_client_id()
        .ok_or_else(|| "OAUTH_CLIENT_ID not configured".to_string())?;

    log::info!("PKCE token refresh attempt");

    // Snapshot the clear-epoch BEFORE the network POST. The matching compare
    // happens strictly AFTER the await resolves (see the logout-resurrection
    // guard below) — there is no RefCell borrow held across this await.
    let start_epoch = auth_clear_epoch();

    let resp = match pkce::refresh_with_provider(&token_endpoint, &refresh_token, &client_id).await
    {
        Ok(r) => r,
        // `e` in both arms is the inner pkce-layer error string, which never
        // contains the refresh_token value (verified: only the redacted
        // status/length and the token-free provider error message are formatted
        // into it).
        Err(crate::pkce::RefreshError::Rejected(e)) => {
            // Definitive provider rejection (invalid_grant / 4xx with error body):
            // the refresh token is dead. Clear it so subsequent 401 waves skip
            // straight to the login redirect instead of re-POSTing a doomed grant.
            log::warn!("PKCE token refresh rejected by provider: {e}");
            clear_refresh_token();
            return Err(e);
        }
        Err(crate::pkce::RefreshError::Transient(e)) => {
            // Network / CORS / 5xx / parse failure: the token may still be valid.
            // Do NOT clear it — this request falls through to NotAuthenticated (login
            // redirect for THIS request), but the token survives so the next 401 wave
            // can retry. Important on the flaky-network profile this app targets.
            log::warn!("PKCE token refresh failed (transient); token retained: {e}");
            return Err(e);
        }
    };

    // Logout-resurrection guard: if any token-clear (logout calls clear_*_token,
    // each of which bumps the epoch) happened while this refresh POST was in
    // flight, DROP the result instead of writing live tokens back into
    // sessionStorage — otherwise a logout that races an in-flight refresh would
    // silently re-authenticate the user until a hard reload.
    if auth_clear_epoch() != start_epoch {
        return Err("auth cleared during refresh; discarding result".into());
    }

    let new_access = resp.access_token.filter(|s| !s.is_empty());
    // Bind the id_token (if any) before any move so it can serve as the Bearer
    // fallback in the return value below.
    let new_id = resp.id_token.filter(|s| !s.is_empty());
    if let Some(ref it) = new_id {
        store_id_token(it);
    }
    if let Some(ref at) = new_access {
        store_access_token(at);
    }
    // Token rotation: providers (e.g. Okta) may return a new refresh_token that
    // supersedes the old one. Persist it so the next refresh uses the current
    // credential.
    if let Some(rt) = resp.refresh_token.filter(|s| !s.is_empty()) {
        store_refresh_token(&rt);
    }

    log::info!("PKCE token refresh succeeded");

    if let Some(at) = new_access {
        Ok(at)
    } else if let Some(it) = new_id {
        Ok(it)
    } else {
        Err("refresh response contained no token".to_string())
    }
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
    write_session_storage(PROFILE_USER_ID_KEY, user_id);
    write_session_storage(PROFILE_DISPLAY_NAME_KEY, display_name);
}

/// Read the cached user profile from session-scoped storage.
///
/// Returns `Some(UserProfile)` when both `user_id` and `display_name` are
/// present (set by the OAuth callback page after a successful token exchange).
/// Returns `None` when no profile has been cached yet.
pub fn get_stored_user_profile() -> Option<UserProfile> {
    let user_id = read_session_storage(PROFILE_USER_ID_KEY)?;
    let name = read_session_storage(PROFILE_DISPLAY_NAME_KEY).unwrap_or_default();
    Some(UserProfile { user_id, name })
}

/// Clear the cached user profile from session-scoped storage.
///
/// Called on logout so stale profile data cannot be observed after the
/// user signs out.
pub fn clear_user_profile() {
    remove_session_storage(PROFILE_USER_ID_KEY);
    remove_session_storage(PROFILE_DISPLAY_NAME_KEY);
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
    // Guest fast-path: skip the network call when this tab was (or is) a
    // guest — but only when no OAuth session could exist.  When PKCE-based
    // OAuth is active and tokens are present the user has logged in *after*
    // a guest session; bail-out would incorrectly reject a valid session.
    if get_guest_session_id().is_some() && crate::constants::is_pkce_flow() {
        let has_oauth_tokens =
            get_stored_access_token().is_some() || get_stored_id_token().is_some();
        if !has_oauth_tokens {
            clear_guest_session_id();
            return Err(anyhow!("guest session; no OAuth session cookie"));
        }
        // If OAuth tokens exist, fall through to normal check
        clear_guest_session_id();
    } else if get_guest_session_id().is_some() {
        // Server-side OAuth: a backend session cookie may still be valid;
        // clear the stale guest marker and fall through to the network check.
        clear_guest_session_id();
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

    let scopes = ensure_offline_access(oauth_scopes());

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

    // Re-apply the PKCE offline_access append: the backend-provided scopes have
    // not passed through `ensure_offline_access` yet.
    let scopes = ensure_offline_access(if !cfg.scopes.is_empty() {
        cfg.scopes
    } else {
        scopes
    });

    Ok(OAuthParams {
        auth_url: cfg.auth_url,
        client_id: cfg.client_id,
        redirect_url,
        scopes,
    })
}

/// Append `offline_access` to the OAuth scopes when running the PKCE flow, so
/// the IdP issues a refresh_token enabling [`refresh_access_token`].
///
/// The append is **unconditional for PKCE** — an operator cannot opt out via
/// `OAUTH_SCOPES`. This is deliberate: refresh support is required for PKCE
/// deployments to survive a mid-meeting bearer expiry. If the IdP ignores or
/// rejects `offline_access`, no refresh_token is issued and the flow degrades
/// GRACEFULLY to the existing login-redirect on 401 — that is acceptable.
///
/// For cookie (server-side OAuth) mode this is a no-op: `resolve_oauth_params`
/// is only reached in PKCE mode (do_login/redirect_to_login gate
/// `start_oauth_flow` on `is_pkce_flow()`), so the inner guard is
/// belt-and-suspenders.
fn ensure_offline_access(scopes: String) -> String {
    if !crate::constants::is_pkce_flow() {
        return scopes;
    }
    append_offline_access(scopes)
}

/// Pure helper: append `offline_access` to a space-delimited scope string
/// unless it is already present as a whole-word element.
///
/// Factored out of [`ensure_offline_access`] so the dedup/append/empty logic
/// can be unit-tested on the host target without the runtime
/// `is_pkce_flow()` config read. This is the entire body of the former
/// PKCE-true branch — moving it here is behavior-preserving.
///
/// - empty input -> `"offline_access"`
/// - already present (whole word) -> returned unchanged (no duplicate)
/// - otherwise -> ` offline_access` appended
///
/// Membership is word-boundary, not substring: `offline_access_extra` does
/// NOT count as present, so the scope is still appended.
fn append_offline_access(scopes: String) -> String {
    // Word-boundary membership: split on whitespace and look for an exact
    // `offline_access` element, not a substring.
    if scopes.split_whitespace().any(|s| s == "offline_access") {
        scopes
    } else if scopes.is_empty() {
        "offline_access".to_string()
    } else {
        format!("{scopes} offline_access")
    }
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
    redirect_not_authenticated(meeting_id, allow_guests);
}

/// Redirect based on a pre-fetched `allow_guests` value (no extra network
/// call). Use this when guest info has already been fetched concurrently.
pub fn redirect_not_authenticated(meeting_id: &str, allow_guests: bool) {
    if allow_guests {
        redirect_to_guest(meeting_id);
    } else {
        redirect_to_login();
    }
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
    let val = read_session_storage(pkce::RETURN_TO_KEY);
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

// ---------------------------------------------------------------------------
// Tests (host-target, pure-Rust parts only)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // `append_offline_access` is the pure dedup/append/empty body factored out
    // of `ensure_offline_access` (which gates on the runtime `is_pkce_flow()`
    // config read and so cannot be unit-tested in isolation on the host).

    #[test]
    fn append_offline_access_on_empty_string() {
        // Empty input must yield exactly the scope, with no leading space.
        // FAILS if the `is_empty()` branch is removed (the `format!` arm would
        // produce " offline_access" with a leading space).
        assert_eq!(append_offline_access(String::new()), "offline_access");
    }

    #[test]
    fn append_offline_access_appends_when_absent() {
        // FAILS if the append arm is broken (e.g. returns scopes unchanged).
        assert_eq!(
            append_offline_access("openid email profile".to_string()),
            "openid email profile offline_access"
        );
    }

    #[test]
    fn append_offline_access_dedups_when_present() {
        // Already present as a whole word -> returned unchanged, no duplicate.
        // FAILS if the dedup guard is removed: the append arm would produce
        // "openid offline_access email offline_access".
        assert_eq!(
            append_offline_access("openid offline_access email".to_string()),
            "openid offline_access email"
        );
    }

    #[test]
    fn append_offline_access_word_boundary_not_substring() {
        // `offline_access_extra` contains the substring but is NOT a whole-word
        // match, so the scope must still be appended.
        // FAILS if membership is changed to a substring check (e.g.
        // `scopes.contains("offline_access")`), which would wrongly skip the
        // append and return the input unchanged.
        assert_eq!(
            append_offline_access("openid offline_access_extra".to_string()),
            "openid offline_access_extra offline_access"
        );
    }
}
