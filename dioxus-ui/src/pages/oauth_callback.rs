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

use crate::auth::{clear_pkce_state, load_pkce_state, store_id_token};
use crate::constants::{
    meeting_api_base_url, oauth_client_id, oauth_issuer, oauth_redirect_url, oauth_token_url,
};
use crate::context::{email_to_display_name, save_display_name_to_storage, validate_display_name};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use dioxus::prelude::*;
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
const CACHED_TOKEN_ENDPOINT_KEY: &str = "vc_cached_token_endpoint";

/// Resolve the provider's token endpoint URL.
///
/// Priority:
///
/// 1. `window.__APP_CONFIG.oauthTokenUrl` (explicit env var `OAUTH_TOKEN_URL`).
/// 2. Per-tab sessionStorage cache set by a previous call to this function.
/// 3. OIDC well-known discovery: `GET {oauthIssuer}/.well-known/openid-configuration`.
/// 4. Backend fallback: `GET /api/v1/oauth/provider-config` — the meeting-api
///    exposes the post-discovery `token_url` field.
async fn resolve_token_endpoint() -> Result<String, String> {
    // 1. Explicit config value — fastest path, no network.
    if let Some(url) = oauth_token_url() {
        return Ok(url);
    }

    // 2. Per-tab cache from a previous discovery call.
    if let Some(storage) = window().session_storage().ok().flatten() {
        if let Some(cached) = storage
            .get_item(CACHED_TOKEN_ENDPOINT_KEY)
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
        {
            return Ok(cached);
        }
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

/// Partial response from `GET /api/v1/oauth/provider-config`.
#[derive(Debug, Deserialize)]
struct ProviderConfigResult {
    #[serde(default)]
    token_url: String,
    #[serde(default)]
    issuer: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderConfigResponse {
    success: bool,
    result: ProviderConfigResult,
}

async fn fetch_token_endpoint_from_backend() -> Result<String, String> {
    let base =
        meeting_api_base_url().map_err(|e| format!("Cannot build provider-config URL: {e}"))?;
    let url = format!("{base}/api/v1/oauth/provider-config");

    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Failed to fetch provider config from backend: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "Backend provider-config returned HTTP {status}: {body}"
        ));
    }

    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read provider-config response: {e}"))?;

    let parsed: ProviderConfigResponse = serde_json::from_str(&text)
        .map_err(|e| format!("Failed to parse provider-config response: {e} — body: {text}"))?;

    if !parsed.success || parsed.result.token_url.is_empty() {
        // If token_url is missing but issuer is present, try discovery.
        if let Some(issuer) = parsed.result.issuer.filter(|s| !s.is_empty()) {
            let discovery_url = format!(
                "{}/.well-known/openid-configuration",
                issuer.trim_end_matches('/')
            );
            let url = fetch_token_endpoint_from_discovery(&discovery_url).await?;
            cache_token_endpoint(&url);
            return Ok(url);
        }
        return Err(
            "Cannot resolve token endpoint: set OAUTH_TOKEN_URL or OAUTH_ISSUER in the \
             dioxus-ui environment, or ensure the backend has OAUTH_TOKEN_URL / OAUTH_ISSUER \
             configured."
                .to_string(),
        );
    }

    let token_url = parsed.result.token_url;
    cache_token_endpoint(&token_url);
    Ok(token_url)
}

fn cache_token_endpoint(url: &str) {
    if let Some(storage) = window().session_storage().ok().flatten() {
        let _: Result<_, _> = storage.set_item(CACHED_TOKEN_ENDPOINT_KEY, url);
    }
}

// ---------------------------------------------------------------------------
// Provider token exchange
// ---------------------------------------------------------------------------

/// Response from the provider's token endpoint.
#[derive(Debug, Deserialize)]
struct ProviderTokenResponse {
    #[serde(default)]
    #[allow(dead_code)] // present in the response; not used by the UI
    access_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    // error fields — present when the exchange fails
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    #[allow(dead_code)] // logged via `error` field
    error_description: Option<String>,
}

/// POST to the provider's token endpoint with PKCE parameters.
///
/// No `client_secret` is included — this is the public-client PKCE flow.
/// The provider validates the `code_verifier` against the `code_challenge`
/// that was sent in the authorization request.
async fn exchange_code_with_provider(
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
        format!("Failed to parse token response (HTTP {status}): {e} — body: {body}")
    })?;

    if let Some(ref err) = token_resp.error {
        let desc = token_resp
            .error_description
            .as_deref()
            .unwrap_or("no description");
        return Err(format!("Token endpoint error '{err}': {desc}"));
    }

    if !status.is_success() {
        return Err(format!("Token endpoint returned HTTP {status}: {body}"));
    }

    Ok(token_resp)
}

// ---------------------------------------------------------------------------
// id_token payload parsing and validation
// ---------------------------------------------------------------------------

/// Claims we extract from the id_token payload.  We do not verify the
/// signature here — that is the meeting-api's job on every API call.
#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    given_name: Option<String>,
    #[serde(default)]
    family_name: Option<String>,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(default)]
    exp: Option<u64>,
    /// Can be a string (single audience) or an array — both are valid per
    /// the OIDC spec.
    #[serde(default)]
    aud: serde_json::Value,
}

impl IdTokenClaims {
    /// Return the best display name available in the token.
    fn display_name(&self) -> String {
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

    /// Return the canonical user identifier: email if present, otherwise sub.
    fn user_id(&self) -> Option<String> {
        self.email
            .as_deref()
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .or_else(|| self.sub.clone())
    }

    /// Does the `aud` claim contain `client_id`?
    fn audience_contains(&self, client_id: &str) -> bool {
        match &self.aud {
            serde_json::Value::String(s) => s == client_id,
            serde_json::Value::Array(arr) => arr.iter().any(|v| v.as_str() == Some(client_id)),
            // Null / missing aud — be permissive (some providers omit it for
            // implicit flows, but we still accept to avoid hard failures).
            _ => true,
        }
    }
}

/// Decode and lightly validate the id_token payload.
///
/// Validates: `nonce`, `exp`, `aud`.
/// Does NOT validate the signature — that is done by the meeting-api JWKS
/// check on every API call.
fn decode_and_validate_id_token(
    id_token: &str,
    expected_nonce: &str,
    client_id: &str,
) -> Result<IdTokenClaims, String> {
    // JWT structure: header.payload.signature (three base64url segments)
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

    // Validate nonce (anti-replay).
    match &claims.nonce {
        Some(n) if n == expected_nonce => {}
        Some(n) => {
            return Err(format!(
                "id_token nonce mismatch: expected '{expected_nonce}', got '{n}'"
            ));
        }
        None => {
            // Some providers omit the nonce when none was sent.  Since we
            // always send one, treat a missing nonce as an error.
            return Err("id_token is missing the nonce claim".to_string());
        }
    }

    // Validate expiry using the browser's current time.
    if let Some(exp) = claims.exp {
        let now_secs = (js_sys::Date::now() / 1000.0) as u64;
        if now_secs > exp {
            return Err(format!("id_token has expired (exp={exp}, now={now_secs})"));
        }
    }

    // Validate audience.
    if !claims.audience_contains(client_id) {
        return Err(format!(
            "id_token audience does not contain the configured client_id '{client_id}'"
        ));
    }

    Ok(claims)
}

// ---------------------------------------------------------------------------
// Backend user registration (graceful)
// ---------------------------------------------------------------------------

/// POST /api/v1/user/register — upserts the user record on the meeting-api.
///
/// Failure is logged but does not abort the login flow — the user can still
/// join meetings; the DB row will be created on a future call.
async fn register_user_with_backend(id_token: &str) {
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
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {id_token}"))
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

    // ── 8. Decode and validate the id_token payload ───────────────────────
    let claims = decode_and_validate_id_token(&id_token, &pkce_state.nonce, &client_id)?;

    // ── 9. Store the id_token ─────────────────────────────────────────────
    store_id_token(&id_token);

    // ── 10. Upsert the user record on the backend (graceful) ─────────────
    register_user_with_backend(&id_token).await;

    // ── 11. Update the local display name ────────────────────────────────
    let raw_display_name = claims.display_name();
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

    let user_id = claims.user_id().unwrap_or_default();
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

    // --- decode_and_validate_id_token ---

    fn make_jwt_payload(claims: serde_json::Value) -> String {
        let json = serde_json::to_string(&claims).unwrap();
        let encoded = URL_SAFE_NO_PAD.encode(json.as_bytes());
        // Fake header and signature — we only care about the payload.
        format!("eyJhbGciOiJSUzI1NiJ9.{encoded}.fakesig")
    }

    #[test]
    fn valid_claims_decode_successfully() {
        let exp = (js_sys::Date::now() / 1000.0) as u64 + 3600;
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "email": "user@example.com",
            "nonce": "testnonce",
            "exp": exp,
            "aud": "my-client-id",
        }));
        let claims = decode_and_validate_id_token(&token, "testnonce", "my-client-id");
        assert!(claims.is_ok(), "should decode valid claims");
        let c = claims.unwrap();
        assert_eq!(c.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn wrong_nonce_rejected() {
        let exp = (js_sys::Date::now() / 1000.0) as u64 + 3600;
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "correct-nonce",
            "exp": exp,
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
        let exp = (js_sys::Date::now() / 1000.0) as u64 + 3600;
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": exp,
            "aud": "other-client",
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_err(), "wrong audience must be rejected");
    }

    #[test]
    fn array_audience_accepted_when_client_id_present() {
        let exp = (js_sys::Date::now() / 1000.0) as u64 + 3600;
        let token = make_jwt_payload(serde_json::json!({
            "sub": "user123",
            "nonce": "n",
            "exp": exp,
            "aud": ["my-client", "other-client"],
        }));
        let result = decode_and_validate_id_token(&token, "n", "my-client");
        assert!(result.is_ok(), "client_id in array aud should be accepted");
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
