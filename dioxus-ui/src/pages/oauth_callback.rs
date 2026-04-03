// SPDX-License-Identifier: MIT OR Apache-2.0

//! OAuth callback page (`/auth/callback`).
//!
//! The identity provider redirects here after the user authenticates.  The
//! URL carries the authorization `code` and `state` (CSRF token) as query
//! parameters captured by the Dioxus router and passed as `query_params`.
//!
//! ## Exchange flow
//!
//! This page performs the token exchange **directly with the identity
//! provider** (PKCE public-client flow, RFC 7636 + OAuth 2.0 for Browser-
//! Based Apps, draft-ietf-oauth-browser-based-apps):
//!
//! 1. Load PKCE state from `sessionStorage` (saved by
//!    [`crate::auth::start_oauth_flow`]).
//! 2. Validate the CSRF `state` parameter echoed back by the provider.
//! 3. Resolve the provider's token endpoint URL (config → OIDC well-known
//!    discovery → backend `/api/v1/oauth/provider-config` fallback).
//! 4. `POST {token_endpoint}` with
//!    `application/x-www-form-urlencoded`:
//!    `grant_type`, `code`, `redirect_uri`, `client_id`, `code_verifier`.
//!    **No `client_secret` is sent** — the PKCE verifier is the proof of
//!    possession for public clients.
//! 5. Parse and lightly validate the returned id_token payload:
//!    - `nonce` must match the stored nonce (anti-replay).
//!    - `exp` must be in the future.
//!    - `aud` must contain the configured `client_id`.
//! 6. Clear the one-time PKCE state from `sessionStorage`.
//! 7. Store the `id_token` in `sessionStorage["vc_id_token"]`.
//! 8. Call `POST /api/v1/user/register` on the meeting-api (Bearer auth) to
//!    upsert the user record (graceful — login still succeeds if this fails).
//! 9. Pre-populate the display name in `localStorage` from the token claims.
//! 10. Navigate to the stored `return_to` URL.
//!
//! ## Security notes
//!
//! * The id_token **signature** is not verified in the browser — that
//!   requires JWKS fetch + RSA/EC operations in WASM.  The meeting-api
//!   validates the signature via JWKS on every authenticated API call, which
//!   is the correct place for cryptographic token verification.
//! * The PKCE verifier exchange already binds this token response to this
//!   specific flow initiated by this browser tab (code injection protection).
//! * CSRF is validated client-side before the exchange is attempted.
//!
//! ## CORS requirement
//!
//! The provider's token endpoint must send CORS headers permitting the
//! browser request.  All major OIDC providers (Google, Okta, Keycloak,
//! Microsoft Entra) do this for public clients.  Providers that require a
//! `client_secret` even for PKCE (confidential clients) cannot use this
//! flow; use the backend `/api/v1/oauth/exchange` endpoint instead.

use crate::auth::{clear_pkce_state, load_pkce_state, store_access_token, store_id_token};
use crate::constants::{
    meeting_api_base_url, oauth_client_id, oauth_issuer, oauth_redirect_url, oauth_token_url,
};
use crate::context::{email_to_display_name, save_display_name_to_storage, validate_display_name};
use crate::id_token::decode_and_validate_id_token;
#[cfg(test)]
use crate::id_token::IdTokenClaims;
use crate::pkce::exchange_code_with_provider;
use dioxus::prelude::*;
use dioxus_sdk_storage::{SessionStorage, StorageBacking};
use gloo_utils::window;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Component state
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum CallbackStatus {
    Loading,
    Success,
    Error(String),
}

// ---------------------------------------------------------------------------
// Query-string helper
// ---------------------------------------------------------------------------

/// Parse one query parameter from a raw query string (with or without leading
/// `?`).  Values are percent-decoded.
fn parse_query_param(search: &str, key: &str) -> Option<String> {
    let search = search.trim_start_matches('?');
    for pair in search.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some(k), Some(v)) = (parts.next(), parts.next()) {
            if k == key {
                return urlencoding::decode(v)
                    .ok()
                    .map(|s| s.into_owned())
                    .filter(|s| !s.is_empty());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Token endpoint resolution
// ---------------------------------------------------------------------------

/// OIDC discovery document (subset — only the fields we need).
#[derive(Debug, Deserialize)]
struct OidcDiscovery {
    token_endpoint: String,
}

/// Cached sessionStorage key for the discovered token endpoint URL.
/// Avoids re-fetching the discovery document on every callback within the
/// same browser tab.
/// Cached session-storage key for the discovered token endpoint URL.
/// Avoids re-fetching the discovery document on every callback within the
/// same session.
const CACHED_TOKEN_ENDPOINT_KEY: &str = "vc_cached_token_endpoint";

/// Resolve the provider's token endpoint URL.
///
/// Priority:
///
/// 1. `window.__APP_CONFIG.oauthTokenUrl` (explicit env var `OAUTH_TOKEN_URL`).
/// 2. Per-session cache set by a previous call to this function (browser
///    `sessionStorage` on web; in-memory on native).
/// 3. OIDC well-known discovery: `GET {oauthIssuer}/.well-known/openid-configuration`.
/// 4. Backend fallback: `GET /api/v1/oauth/provider-config` — the meeting-api
///    exposes the post-discovery `token_url` field.
async fn resolve_token_endpoint() -> Result<String, String> {
    // 1. Explicit config value — fastest path, no network.
    if let Some(url) = oauth_token_url() {
        return Ok(url);
    }

    // 2. Per-session cache from a previous discovery call.
    if let Some(cached) =
        SessionStorage::get::<Option<String>>(&CACHED_TOKEN_ENDPOINT_KEY.to_string())
            .flatten()
            .filter(|s| !s.is_empty())
    {
        return Ok(cached);
    }

    // 3. OIDC well-known discovery.
    if let Some(issuer) = oauth_issuer() {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        match fetch_token_endpoint_from_discovery(&discovery_url).await {
            Ok(url) => {
                cache_token_endpoint(&url);
                return Ok(url);
            }
            Err(e) => {
                log::warn!("OIDC discovery failed ({discovery_url}): {e}; trying backend fallback");
            }
        }
    }

    // 4. Backend fallback: meeting-api already ran discovery and stores the
    //    resolved token_url.
    fetch_token_endpoint_from_backend().await
}

async fn fetch_token_endpoint_from_discovery(discovery_url: &str) -> Result<String, String> {
    let resp = reqwest::get(discovery_url)
        .await
        .map_err(|e| format!("OIDC discovery request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("OIDC discovery returned HTTP {status}: {body}"));
    }

    let doc: OidcDiscovery = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse OIDC discovery document: {e}"))?;

    if doc.token_endpoint.is_empty() {
        return Err("OIDC discovery document has an empty token_endpoint".to_string());
    }

    Ok(doc.token_endpoint)
}

async fn fetch_token_endpoint_from_backend() -> Result<String, String> {
    // Delegate to the shared provider-config fetch (handles its own cache).
    let cfg = crate::provider_config::fetch_provider_config().await?;

    if !cfg.token_url.is_empty() {
        cache_token_endpoint(&cfg.token_url);
        return Ok(cfg.token_url);
    }

    // token_url empty but issuer present — try OIDC well-known discovery.
    if let Some(issuer) = cfg.issuer.filter(|s| !s.is_empty()) {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let url = fetch_token_endpoint_from_discovery(&discovery_url).await?;
        cache_token_endpoint(&url);
        return Ok(url);
    }

    Err(
        "Cannot resolve token endpoint: set OAUTH_TOKEN_URL or OAUTH_ISSUER in the \
         dioxus-ui environment, or ensure the backend has OAUTH_TOKEN_URL / OAUTH_ISSUER \
         configured."
            .to_string(),
    )
}

fn cache_token_endpoint(url: &str) {
    SessionStorage::set(
        CACHED_TOKEN_ENDPOINT_KEY.to_string(),
        &Some(url.to_string()),
    );
}

// ---------------------------------------------------------------------------
// Provider token exchange and id_token validation
// ---------------------------------------------------------------------------
//
// `exchange_code_with_provider` and `ProviderTokenResponse` live in
// `crate::pkce` and are imported at the top of this file.
//
// `IdTokenClaims` and `decode_and_validate_id_token` live in
// `crate::id_token` and are imported at the top of this file.

// ---------------------------------------------------------------------------
// Backend user registration (graceful)
// ---------------------------------------------------------------------------

/// POST /api/v1/user/register — upserts the user record on the meeting-api.
///
/// The **access token** is forwarded as the Bearer credential.  The meeting-api
/// validates it via JWKS (same key set as for id_tokens) and upserts the user
/// row using whatever identity claims are present (`email` or `sub`).
///
/// Failure is logged but does not abort the login flow.
async fn register_user_with_backend(access_token: &str) {
    let base_url = match meeting_api_base_url() {
        Ok(u) => u,
        Err(e) => {
            log::warn!("Skipping user registration: cannot resolve meeting-api URL: {e}");
            return;
        }
    };
    let url = format!("{base_url}/api/v1/user/register");

    match reqwest::Client::new()
        .post(&url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {access_token}"),
        )
        .header(reqwest::header::CONTENT_LENGTH, "0")
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            log::info!("User registration with backend succeeded");
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            log::warn!("User registration returned HTTP {status}: {body}");
        }
        Err(e) => {
            log::warn!("User registration request failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Post-login navigation
// ---------------------------------------------------------------------------

fn do_post_login_navigate(return_to: Option<String>) {
    match return_to {
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
            if let Err(e) = window().location().set_href(&url) {
                log::error!("post-login navigation failed: {e:?}");
                let _ = window().location().set_href("/");
            }
        }
        Some(path) if path.starts_with('/') => {
            if let Err(e) = window().location().set_href(&path) {
                log::error!("post-login path navigation failed: {e:?}");
                let _ = window().location().set_href("/");
            }
        }
        _ => {
            let _ = window().location().set_href("/");
        }
    }
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/// OAuth callback page — mounted at `/auth/callback`.
///
/// Performs the token exchange directly with the identity provider, validates
/// the id_token, stores it in `sessionStorage`, and navigates to the post-
/// login destination.
#[component]
pub fn OAuthCallback(query_params: String) -> Element {
    let mut status = use_signal(|| CallbackStatus::Loading);
    // Capture query_params by value so the async task can own it.
    let params_snapshot = query_params.clone();

    use_effect(move || {
        let params = params_snapshot.clone();
        spawn(async move {
            if let Err(e) = run_callback(params).await {
                status.set(CallbackStatus::Error(e));
            } else {
                status.set(CallbackStatus::Success);
            }
        });
    });

    rsx! {
        div {
            style: "display: flex; align-items: center; justify-content: center; \
                    height: 100vh; background: #0D131F; color: #fff; \
                    font-family: system-ui, sans-serif;",
            match status() {
                CallbackStatus::Loading => rsx! {
                    div { style: "text-align: center;",
                        div {
                            style: "width: 48px; height: 48px; \
                                    border: 4px solid rgba(255,255,255,0.2); \
                                    border-top-color: #7928CA; border-radius: 50%; \
                                    animation: spin 0.8s linear infinite; \
                                    margin: 0 auto 24px;",
                        }
                        p {
                            style: "color: rgba(255,255,255,0.7); font-size: 1rem;",
                            "Completing sign-in\u{2026}"
                        }
                        style {
                            "@keyframes spin {{ to {{ transform: rotate(360deg); }} }}"
                        }
                    }
                },
                CallbackStatus::Success => rsx! {
                    div { style: "text-align: center;",
                        p { style: "color: #4ade80; font-size: 1.2rem;", "Sign-in successful!" }
                        p {
                            style: "color: rgba(255,255,255,0.5); font-size: 0.9rem;",
                            "Redirecting\u{2026}"
                        }
                    }
                },
                CallbackStatus::Error(msg) => rsx! {
                    div {
                        style: "text-align: center; max-width: 480px; padding: 2rem;",
                        div { style: "font-size: 2rem; margin-bottom: 1rem;", "\u{26a0}\u{fe0f}" }
                        h2 {
                            style: "margin-bottom: 0.75rem; font-size: 1.25rem;",
                            "Sign-in failed"
                        }
                        p {
                            style: "color: rgba(255,255,255,0.65); font-size: 0.9rem; \
                                    margin-bottom: 1.5rem; line-height: 1.5;",
                            "{msg}"
                        }
                        button {
                            style: "background: #7928CA; color: #fff; border: none; \
                                    padding: 0.6rem 1.5rem; border-radius: 8px; \
                                    cursor: pointer; font-size: 1rem;",
                            onclick: move |_| {
                                if let Err(e) = window().location().set_href("/login") {
                                    log::error!("Failed to navigate to /login: {e:?}");
                                }
                            },
                            "Try again"
                        }
                    }
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core callback logic (extracted so errors can be returned cleanly)
// ---------------------------------------------------------------------------

async fn run_callback(query_params: String) -> Result<(), String> {
    // ── 1. Parse code + state from query string ──────────────────────────
    let code = parse_query_param(&query_params, "code")
        .ok_or_else(|| "Missing 'code' parameter in callback URL.".to_string())?;
    let url_state = parse_query_param(&query_params, "state")
        .ok_or_else(|| "Missing 'state' parameter in callback URL.".to_string())?;

    // ── 2. Load PKCE state from sessionStorage ────────────────────────────
    let pkce_state = load_pkce_state().ok_or_else(|| {
        "No PKCE session state found in sessionStorage. \
         This tab may have been opened directly on the callback URL. \
         Please sign in again."
            .to_string()
    })?;

    // ── 3. CSRF state validation ──────────────────────────────────────────
    if url_state != pkce_state.state {
        clear_pkce_state();
        log::error!(
            "CSRF state mismatch: url_state={url_state} stored={}",
            pkce_state.state
        );
        return Err("Sign-in failed: the state parameter did not match. \
             This may indicate a CSRF attempt or an expired session. \
             Please sign in again."
            .to_string());
    }

    // ── 4. Resolve the provider's token endpoint ──────────────────────────
    let token_endpoint = resolve_token_endpoint().await?;

    // ── 5. Resolve client_id and redirect_uri ────────────────────────────
    let client_id = oauth_client_id().ok_or_else(|| {
        "OAUTH_CLIENT_ID is not set in window.__APP_CONFIG. \
         Set it in the dioxus-ui environment or via /api/v1/oauth/provider-config."
            .to_string()
    })?;

    let redirect_uri = oauth_redirect_url().unwrap_or_else(|| {
        window()
            .location()
            .origin()
            .map(|o| format!("{o}/auth/callback"))
            .unwrap_or_default()
    });

    // ── 6. Exchange the code with the provider ────────────────────────────
    let token_resp = exchange_code_with_provider(
        &token_endpoint,
        &code,
        &pkce_state.code_verifier,
        &client_id,
        &redirect_uri,
    )
    .await;

    // ── 7. Consume PKCE state (one-time use, clear regardless of outcome) ─
    clear_pkce_state();

    let token_resp = token_resp?;

    let id_token = token_resp
        .id_token
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "The provider did not return an id_token. \
             Ensure the 'openid' scope is requested and the provider supports OIDC."
                .to_string()
        })?;

    // The access token may be absent for some providers (though standard OIDC
    // always returns one).  Log a warning but continue — the id_token alone
    // can still be used as a fallback Bearer credential.
    let access_token = token_resp.access_token.filter(|s| !s.is_empty());
    if access_token.is_none() {
        log::warn!(
            "Provider did not return an access_token; \
             the id_token will be used as Bearer fallback"
        );
    }

    // ── 8. Decode and validate the id_token payload ───────────────────────
    // The id_token is decoded solely to extract user identity claims
    // (display name, user_id).  It is NOT sent to the meeting-api — the
    // access_token is used for all API authentication.
    let claims = decode_and_validate_id_token(&id_token, &pkce_state.nonce, &client_id)?;

    // ── 9. Store both tokens ──────────────────────────────────────────────
    // id_token  → user identity (email, display name) decoded client-side.
    // access_token → Bearer credential forwarded to the meeting-api as-is.
    store_id_token(&id_token);
    if let Some(ref at) = access_token {
        store_access_token(at);
    }

    // ── 10. Upsert the user record on the backend (graceful) ─────────────
    // Use the access_token as Bearer; fall back to id_token when absent.
    let bearer_for_registration = access_token.as_deref().unwrap_or(&id_token);
    register_user_with_backend(bearer_for_registration).await;

    // ── 11. Update the local display name and cache the user profile ─────
    let raw_display_name = claims.display_name();
    let user_id = claims.user_id().unwrap_or_default();

    // Cache the profile claims extracted from the validated id_token so that
    // get_user_profile() can return them immediately without a network call.
    crate::auth::store_user_profile(&user_id, &raw_display_name);

    if !raw_display_name.is_empty() {
        let display_name = if raw_display_name.contains('@') {
            email_to_display_name(&raw_display_name)
        } else {
            raw_display_name.clone()
        };
        if let Ok(valid) = validate_display_name(&display_name) {
            save_display_name_to_storage(&valid);
        }
    }

    log::info!(
        "OAuth callback complete for user '{}' (display: '{}')",
        user_id,
        raw_display_name,
    );

    // ── 12. Navigate to return_to ─────────────────────────────────────────
    // Brief pause so the success state renders.
    gloo_timers::future::TimeoutFuture::new(50).await;
    do_post_login_navigate(pkce_state.return_to);

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_query_param ---

    #[test]
    fn extracts_code() {
        assert_eq!(
            parse_query_param("code=abc123&state=xyz", "code"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn extracts_state() {
        assert_eq!(
            parse_query_param("code=abc&state=xyz789", "state"),
            Some("xyz789".to_string())
        );
    }

    #[test]
    fn returns_none_for_missing_key() {
        assert_eq!(parse_query_param("code=abc123", "state"), None);
    }

    #[test]
    fn handles_empty_search() {
        assert_eq!(parse_query_param("", "code"), None);
    }

    #[test]
    fn strips_leading_question_mark() {
        assert_eq!(
            parse_query_param("?code=abc&state=def", "state"),
            Some("def".to_string())
        );
    }

    #[test]
    fn percent_decodes_value() {
        assert_eq!(
            parse_query_param("state=hello%20world", "state"),
            Some("hello world".to_string())
        );
    }

    // Tests for `parse_query_param` only.
    // Tests for `decode_and_validate_id_token` and `IdTokenClaims` live
    // in `crate::id_token` (see `src/id_token.rs`).
}
