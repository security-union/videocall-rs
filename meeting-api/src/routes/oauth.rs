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

//! OAuth route handlers: login, callback, exchange, session, profile, logout.
//!
//! ## Authentication modes
//!
//! ### Legacy mode (`OAUTH_BROWSER_PKCE=false`, the default)
//!
//! `OAUTH_REDIRECT_URL` points at the **backend** `/login/callback` route.
//! After a successful token exchange the handler issues a signed session JWT
//! inside an `HttpOnly; Secure; SameSite=Lax` cookie named `session`.
//! JavaScript cannot read the cookie; the browser sends it automatically on
//! every subsequent request.  Existing deployments use this mode and require
//! no configuration change.
//!
//! ### Browser PKCE mode (`OAUTH_BROWSER_PKCE=true`)
//!
//! `OAUTH_REDIRECT_URL` points at the **dioxus-ui** `/auth/callback` route.
//! The flow is:
//!
//! 1. Browser navigates to `GET /login?returnTo=<url>` — the server generates
//!    PKCE + CSRF material, stores them in the DB, and redirects to the
//!    identity provider.
//!
//! 2. Provider redirects to the UI `/auth/callback` with `?code=...&state=...`.
//!
//! 3. The UI exchanges the code directly with the provider (public-client PKCE,
//!    no `client_secret` in the browser) and stores the returned tokens in
//!    `sessionStorage`.
//!
//! 4. The UI calls `POST /api/v1/user/register` with `Authorization: Bearer
//!    <id_token>` to upsert the user record on the backend.
//!
//! In browser PKCE mode **no session cookie is issued**.  If the provider
//! accidentally sends the code to the backend `/login/callback` route while
//! `OAUTH_BROWSER_PKCE=true`, the handler logs a warning and redirects
//! without setting a cookie so the deployment degrades gracefully.

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;
use url::Url;

use videocall_meeting_types::responses::{
    APIResponse, OAuthExchangeResponse, OAuthProviderConfigResponse, ProfileResponse,
};

use crate::auth::AuthUser;
use crate::db::oauth as db_oauth;
use crate::error::AppError;
use crate::oauth;
use crate::state::AppState;
use crate::token;

// ---------------------------------------------------------------------------
// Cookie helpers
// ---------------------------------------------------------------------------

/// Build a `Set-Cookie` header value for the session JWT.
///
/// Used by the legacy `GET /login/callback` handler when `OAUTH_BROWSER_PKCE`
/// is `false` (the default).  Attributes match OWASP session-cookie guidance:
/// `HttpOnly` (JavaScript cannot read it), `SameSite=Lax` (CSRF mitigation),
/// `Secure` when `cookie_secure` is `true` (HTTPS-only transmission),
/// and `Max-Age` for an explicit TTL so the browser expires it correctly.
fn build_session_cookie(
    name: &str,
    jwt: &str,
    ttl_secs: i64,
    domain: Option<&str>,
    secure: bool,
) -> String {
    let mut cookie = format!("{name}={jwt}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

/// Build a `Set-Cookie` header that clears the session cookie.
fn build_clear_session_cookie(name: &str, domain: Option<&str>, secure: bool) -> String {
    let mut cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    #[serde(rename = "returnTo")]
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub state: String,
    pub code: String,
}

/// GET /login?returnTo=<url>
///
/// Initiates the OAuth flow: generates PKCE + CSRF + nonce, stores in DB,
/// redirects to the identity provider.
///
/// `OAUTH_REDIRECT_URL` should be set to the UI's `/auth/callback` route so
/// the provider sends the authorization code to the frontend.
pub async fn login(
    State(state): State<AppState>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, AppError> {
    let oauth_cfg = state
        .oauth
        .as_ref()
        .ok_or_else(|| AppError::internal("OAuth not configured"))?;

    let csrf_token = CsrfToken::new_random();
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Generate a nonce for OIDC ID token binding (reuse oauth2's crypto RNG).
    let nonce = CsrfToken::new_random();

    // Sanitize return_to: allow relative paths and absolute URLs whose origin
    // is in the allowlist (after_login_url origin + ALLOWED_REDIRECT_URLS).
    let return_to = query.return_to.as_deref().and_then(|u| {
        validate_return_to(
            u,
            &oauth_cfg.after_login_url,
            &oauth_cfg.allowed_redirect_urls,
        )
    });
    let return_to = return_to.as_deref();

    db_oauth::store_oauth_request(
        &state.db,
        pkce_challenge.as_str(),
        pkce_verifier.secret(),
        csrf_token.secret(),
        return_to,
        Some(nonce.secret()),
    )
    .await?;

    let auth_url = oauth::build_auth_url(
        &oauth_cfg.auth_url,
        &oauth_cfg.client_id,
        &oauth_cfg.redirect_url,
        &oauth_cfg.scopes,
        pkce_challenge.as_str(),
        csrf_token.secret(),
        Some(nonce.secret()),
    );

    Ok(Redirect::to(&auth_url).into_response())
}

/// GET /login/callback?state=...&code=...
///
/// Handles the OAuth callback from the identity provider.
///
/// ## Legacy mode (`OAUTH_BROWSER_PKCE=false`, default)
///
/// Issues a signed session JWT inside an `HttpOnly` cookie after a successful
/// token exchange.  The browser sends the cookie automatically on every
/// subsequent request.  Use this mode when `OAUTH_REDIRECT_URL` still points
/// at the backend `/login/callback` route (all existing deployments).
///
/// ## Browser PKCE mode (`OAUTH_BROWSER_PKCE=true`)
///
/// Skips cookie issuance.  In this mode `OAUTH_REDIRECT_URL` should point at
/// the dioxus-ui `/auth/callback` route so the provider sends the code to the
/// browser, not here.  If the code arrives here despite the setting, the
/// handler logs a warning and redirects without a cookie so the deployment
/// degrades gracefully rather than silently.
pub async fn callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Result<Response, AppError> {
    let oauth_cfg = state
        .oauth
        .as_ref()
        .ok_or_else(|| AppError::internal("OAuth not configured"))?;

    let oauth_req = db_oauth::fetch_oauth_request(&state.db, &query.state)
        .await?
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                videocall_meeting_types::APIError::internal_error("invalid OAuth state"),
            )
        })?;

    let pkce_verifier = oauth_req
        .pkce_verifier
        .ok_or_else(|| AppError::internal("missing PKCE verifier"))?;

    let (token_response, mut claims) = oauth::exchange_code_for_claims(
        &oauth_cfg.redirect_url,
        &oauth_cfg.client_id,
        oauth_cfg.client_secret.as_deref(),
        &pkce_verifier,
        &oauth_cfg.token_url,
        &query.code,
        state.jwks_cache.as_deref(),
        oauth_cfg.issuer.as_deref(),
        oauth_req.nonce.as_deref(),
    )
    .await?;

    // If the ID token lacks an email claim, fall back to the UserInfo endpoint.
    if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
        if let Some(userinfo_url) = &oauth_cfg.userinfo_url {
            let user_info =
                oauth::fetch_userinfo(userinfo_url, &token_response.access_token).await?;
            if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
                claims.email = user_info.email;
            }
            if claims.name.is_empty() {
                if let Some(name) = user_info.name {
                    claims.name = name;
                }
            }
            if claims.given_name.is_none() {
                claims.given_name = user_info.given_name;
            }
            if claims.family_name.is_none() {
                claims.family_name = user_info.family_name;
            }
        }
    }

    let email = claims
        .email
        .as_ref()
        .filter(|e| !e.is_empty())
        .ok_or_else(|| AppError::internal("Email not available from ID token or UserInfo"))?
        .clone();

    let display_name = claims.display_name();

    db_oauth::upsert_user(
        &state.db,
        &email,
        &display_name,
        &token_response.access_token,
        token_response.refresh_token.as_deref(),
    )
    .await?;

    let redirect_url = match &oauth_req.return_to {
        Some(value) if value.starts_with("http://") || value.starts_with("https://") => {
            validate_return_to(
                value,
                &oauth_cfg.after_login_url,
                &oauth_cfg.allowed_redirect_urls,
            )
            .unwrap_or_else(|| oauth_cfg.after_login_url.clone())
        }
        Some(path) => {
            format!(
                "{}{}",
                oauth_cfg.after_login_url.trim_end_matches('/'),
                path
            )
        }
        None => oauth_cfg.after_login_url.clone(),
    };

    if oauth_cfg.browser_pkce {
        // Browser PKCE mode: the provider should have sent the code to the
        // UI's /auth/callback route, not here.  If it arrived here anyway,
        // redirect without a session cookie so the failure is loud in logs
        // but does not produce a confusing half-authenticated state.
        tracing::warn!(
            user_id = %email,
            display_name = %display_name,
            redirect_url = %redirect_url,
            "OAUTH_BROWSER_PKCE is enabled but the provider redirected to the \
             backend /login/callback route — no session cookie will be issued. \
             Verify that OAUTH_REDIRECT_URL is set to the dioxus-ui /auth/callback route.",
        );
        Ok(Redirect::to(&redirect_url).into_response())
    } else {
        // Legacy mode: issue a signed session JWT inside an HttpOnly cookie.
        let session_jwt = token::generate_session_token(
            &state.jwt_secret,
            &email,
            &display_name,
            state.session_ttl_secs,
        )?;
        let session_cookie = build_session_cookie(
            &state.cookie_name,
            &session_jwt,
            state.session_ttl_secs,
            state.cookie_domain.as_deref(),
            state.cookie_secure,
        );
        tracing::info!(
            user_id = %email,
            display_name = %display_name,
            redirect_url = %redirect_url,
            "OAuth login successful, redirecting with session cookie",
        );
        let mut response = Redirect::to(&redirect_url).into_response();
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&session_cookie)
                .map_err(|_| AppError::internal("failed to build session cookie header"))?,
        );
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Exchange endpoint — server-mediated PKCE (for confidential clients)
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v1/oauth/exchange`.
///
/// This endpoint performs a **server-mediated** token exchange: the server
/// calls the identity provider's token endpoint directly, keeping
/// `client_secret` private.  It supports two sub-modes:
///
/// ## Hybrid PKCE (caller-generated verifier)
///
/// The caller generates the PKCE verifier and nonce locally and sends them
/// here alongside the authorization code.  The server performs the exchange
/// using the caller-supplied verifier.  Intended for clients (e.g. native
/// apps, confidential providers) that need the server to hold `client_secret`
/// but also want PKCE protection.
///
/// > **Note:** The dioxus-ui does **not** use this path.  It exchanges tokens
/// > directly with the identity provider (public-client PKCE) and then calls
/// > `POST /api/v1/user/register`.  This mode exists for future native clients
/// > or providers that require a `client_secret` even for PKCE.
///
/// ```json
/// { "code": "…", "code_verifier": "…", "nonce": "…" }
/// ```
///
/// ## Server-side PKCE (server-generated verifier, legacy)
///
/// The backend generated the PKCE material during `GET /login` and stored it
/// in the DB keyed by the CSRF `state`.  The caller sends only
/// `{ "code": "…", "state": "…" }` and the server looks up the verifier.
///
/// ```json
/// { "code": "…", "state": "…" }
/// ```
///
/// Both forms are accepted; `code_verifier` takes precedence when non-empty.
#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    /// Authorization code received from the identity provider.
    pub code: String,
    /// PKCE code verifier generated by the UI.  Present in the client-side
    /// PKCE flow; takes precedence over `state` when non-empty.
    #[serde(default)]
    pub code_verifier: Option<String>,
    /// OIDC nonce generated by the UI.  When present it is validated against
    /// the `nonce` claim in the provider's id_token.
    #[serde(default)]
    pub nonce: Option<String>,
    /// CSRF state token used as a DB lookup key in the legacy server-side flow.
    /// Ignored when `code_verifier` is provided.
    #[serde(default)]
    pub state: Option<String>,
}

/// POST /api/v1/oauth/exchange
///
/// Server-mediated token exchange endpoint.  The server calls the identity
/// provider on behalf of the caller, keeping `client_secret` private.
///
/// > **Note:** The dioxus-ui does **not** call this endpoint.  It exchanges
/// > tokens directly with the identity provider (public-client PKCE) and then
/// > calls `POST /api/v1/user/register` to upsert the user record.  This
/// > endpoint exists for clients that need server-side exchange — e.g.
/// > confidential OIDC clients where the provider requires a `client_secret`
/// > even for PKCE, or native clients that cannot reach the provider directly.
///
/// Two exchange modes are accepted (see [`ExchangeRequest`]):
///
/// **Hybrid PKCE:** caller provides `code` + `code_verifier` + optional
/// `nonce`.  The server uses the caller-supplied verifier and skips the DB
/// lookup.
///
/// **Server-side PKCE (legacy):** caller provides `code` + `state`.  The
/// server atomically fetches-and-deletes the stored PKCE request from the DB,
/// validates CSRF, and uses the stored verifier.
///
/// In both cases:
/// - The token exchange is server-to-server (`client_secret` never leaves the
///   server).
/// - The id_token is validated via JWKS (signature, `exp`, `aud`, `iss`,
///   nonce when present).
/// - The user record is upserted.
/// - The id_token is returned in the JSON body; **no session cookie is set**.
pub async fn exchange(
    State(state): State<AppState>,
    Json(body): Json<ExchangeRequest>,
) -> Result<Response, AppError> {
    let oauth_cfg = state
        .oauth
        .as_ref()
        .ok_or_else(|| AppError::internal("OAuth not configured"))?;

    // --- Resolve PKCE verifier and expected nonce ---
    //
    // Hybrid PKCE flow: code_verifier was generated by the caller.
    // Server-side PKCE flow: code_verifier is stored in the DB, keyed by state.
    let (pkce_verifier, stored_nonce, return_to) =
        if let Some(verifier) = body.code_verifier.filter(|v| !v.is_empty()) {
            // New client-side flow: use the provided verifier directly.
            // `return_to` is not stored server-side in this flow (the UI keeps
            // it in sessionStorage).
            (verifier, None::<String>, None::<String>)
        } else {
            // Legacy server-side flow: look up the verifier from the DB.
            let csrf_state = body
                .state
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::new(
                        StatusCode::BAD_REQUEST,
                        videocall_meeting_types::APIError::internal_error(
                            "either code_verifier or state is required",
                        ),
                    )
                })?;

            let oauth_req = db_oauth::fetch_oauth_request(&state.db, csrf_state)
                .await?
                .ok_or_else(|| {
                    AppError::new(
                        StatusCode::BAD_REQUEST,
                        videocall_meeting_types::APIError::internal_error(
                            "invalid or already-used OAuth state; restart the login flow",
                        ),
                    )
                })?;

            let verifier = oauth_req.pkce_verifier.ok_or_else(|| {
                AppError::internal("missing PKCE verifier in stored OAuth request")
            })?;

            (verifier, oauth_req.nonce, oauth_req.return_to)
        };

    // The nonce to validate against the id_token: prefer the one supplied in
    // the request body (client-side flow), fall back to the one stored in the
    // DB (server-side flow).
    let expected_nonce: Option<&str> = body
        .nonce
        .as_deref()
        .filter(|n| !n.is_empty())
        .or(stored_nonce.as_deref());

    // --- Server-to-server token exchange ---
    let (token_response, mut claims) = oauth::exchange_code_for_claims(
        &oauth_cfg.redirect_url,
        &oauth_cfg.client_id,
        oauth_cfg.client_secret.as_deref(),
        &pkce_verifier,
        &oauth_cfg.token_url,
        &body.code,
        state.jwks_cache.as_deref(),
        oauth_cfg.issuer.as_deref(),
        expected_nonce,
    )
    .await?;

    // Fallback to UserInfo when the id_token lacks an email claim.
    if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
        if let Some(userinfo_url) = &oauth_cfg.userinfo_url {
            let user_info =
                oauth::fetch_userinfo(userinfo_url, &token_response.access_token).await?;
            if claims.email.as_ref().is_none_or(|e| e.is_empty()) {
                claims.email = user_info.email;
            }
            if claims.name.is_empty() {
                if let Some(name) = user_info.name {
                    claims.name = name;
                }
            }
            if claims.given_name.is_none() {
                claims.given_name = user_info.given_name;
            }
            if claims.family_name.is_none() {
                claims.family_name = user_info.family_name;
            }
        }
    }

    let display_name = claims.display_name();

    let email = claims
        .email
        .filter(|e| !e.is_empty())
        .ok_or_else(|| AppError::internal("Email not available from ID token or UserInfo"))?;

    db_oauth::upsert_user(
        &state.db,
        &email,
        &display_name,
        &token_response.access_token,
        token_response.refresh_token.as_deref(),
    )
    .await
    .unwrap_or_else(|e| {
        // A transient DB failure must not revoke a legitimately exchanged
        // token.  The client has already authenticated; the upsert is
        // bookkeeping.  Log and continue.
        tracing::warn!(
            user_id = %email,
            error = %e,
            "Failed to upsert user record after token exchange; \
             tokens are still returned"
        );
    });

    let id_token = token_response
        .id_token
        .ok_or_else(|| AppError::internal("id_token missing from provider token response"))?;

    tracing::info!(
        user_id = %email,
        display_name = %display_name,
        "OAuth token exchange successful",
    );

    let mut response = Json(APIResponse::ok(OAuthExchangeResponse {
        user_id: email,
        display_name,
        id_token,
        access_token: token_response.access_token,
        return_to,
    }))
    .into_response();
    // Raw tokens are returned in the body.  Instruct every cache (browser,
    // CDN, reverse proxy) never to store this response.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

// ---------------------------------------------------------------------------
// Provider config endpoint
// ---------------------------------------------------------------------------

/// GET /api/v1/oauth/provider-config
///
/// Returns the OAuth provider parameters the browser needs to build the PKCE
/// authorization URL **and** to perform the token exchange directly with the
/// provider.  No authentication is required — all returned values are public.
///
/// The dioxus-ui calls this endpoint when `OAUTH_AUTH_URL`, `OAUTH_TOKEN_URL`,
/// or `OAUTH_CLIENT_ID` are not set in `window.__APP_CONFIG` (e.g. when only
/// `OAUTH_ISSUER` was provided and the endpoints were resolved via OIDC
/// discovery at server start).
pub async fn provider_config(
    State(state): State<AppState>,
) -> Json<APIResponse<OAuthProviderConfigResponse>> {
    match &state.oauth {
        None => Json(APIResponse::ok(OAuthProviderConfigResponse {
            enabled: false,
            auth_url: String::new(),
            client_id: String::new(),
            scopes: String::new(),
            token_url: String::new(),
            issuer: None,
        })),
        Some(cfg) => Json(APIResponse::ok(OAuthProviderConfigResponse {
            enabled: true,
            auth_url: cfg.auth_url.clone(),
            client_id: cfg.client_id.clone(),
            scopes: cfg.scopes.clone(),
            token_url: cfg.token_url.clone(),
            issuer: cfg.issuer.clone(),
        })),
    }
}

// ---------------------------------------------------------------------------
// User registration endpoint
// ---------------------------------------------------------------------------

/// POST /api/v1/user/register
///
/// Called by the dioxus-ui `/auth/callback` page **after** performing the
/// token exchange directly with the identity provider (PKCE public-client
/// flow).  The browser presents the id_token as `Authorization: Bearer
/// <id_token>`; this endpoint:
///
/// 1. Validates the token via the `AuthUser` extractor (JWKS signature check,
///    `exp`, `aud`, `iss`).
/// 2. Upserts the user record using only the identity claims from the token —
///    no provider access/refresh tokens are stored.
/// 3. Returns `{ user_id, name }` so the browser can populate UI state.
///
/// This endpoint is also safe to call repeatedly (idempotent within a
/// session) because the upsert only updates `name` and `last_login`.
pub async fn register_user(
    State(state): State<AppState>,
    AuthUser { user_id, name }: AuthUser,
) -> Result<Json<APIResponse<ProfileResponse>>, AppError> {
    db_oauth::register_user_from_token(&state.db, &user_id, &name).await?;

    tracing::info!(
        user_id = %user_id,
        name = %name,
        "User registered from id_token",
    );

    Ok(Json(APIResponse::ok(ProfileResponse { user_id, name })))
}

// ---------------------------------------------------------------------------
// Session / profile / logout
// ---------------------------------------------------------------------------

/// GET /session -- returns 200 if the session JWT is valid, 401 otherwise.
///
/// When OAuth/JWKS is configured the `AuthUser` extractor validates the
/// provider id_token supplied as `Authorization: Bearer <id_token>`.
pub async fn check_session(AuthUser { .. }: AuthUser) -> StatusCode {
    StatusCode::OK
}

/// GET /profile -- returns the authenticated user's profile.
///
/// Because the `AuthUser` extractor populates fields directly from the
/// validated token (session JWT or provider id_token), this endpoint never
/// needs a database query.
pub async fn get_profile(
    AuthUser { user_id, name }: AuthUser,
) -> Json<APIResponse<ProfileResponse>> {
    Json(APIResponse::ok(ProfileResponse { user_id, name }))
}

/// GET /logout -- clears the legacy session cookie and optionally initiates
/// RP-initiated logout at the provider's `end_session_endpoint`.
///
/// When OAuth/JWKS is configured there is no server-side session to terminate
/// (the id_token lives in the browser's `sessionStorage`).  The client should
/// discard the stored id_token before navigating to this endpoint.  If an
/// `end_session_endpoint` is configured the browser is redirected there so
/// that the provider's session is also terminated.
pub async fn logout(State(state): State<AppState>) -> Result<Response, AppError> {
    let clear = build_clear_session_cookie(
        &state.cookie_name,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );

    let mut response = if let Some(end_session_url) = state
        .oauth
        .as_ref()
        .and_then(|o| o.end_session_endpoint.as_deref())
    {
        let oauth_cfg = state.oauth.as_ref().expect("oauth is Some");
        let redirect_url = build_end_session_url(end_session_url, oauth_cfg)?;
        tracing::info!(
            end_session_url = %redirect_url,
            "Initiating RP-initiated logout via provider end-session endpoint",
        );
        Redirect::to(&redirect_url).into_response()
    } else {
        StatusCode::OK.into_response()
    };

    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear)
            .map_err(|_| AppError::internal("failed to build clear cookie header"))?,
    );
    Ok(response)
}

/// Build the provider's end-session URL for RP-initiated logout.
fn build_end_session_url(
    end_session_url: &str,
    oauth_cfg: &crate::config::OAuthConfig,
) -> Result<String, AppError> {
    let mut url = Url::parse(end_session_url)
        .map_err(|e| AppError::internal(&format!("Invalid end_session_endpoint URL: {e}")))?;
    {
        let mut params = url.query_pairs_mut();
        params.append_pair("client_id", &oauth_cfg.client_id);
        if let Some(ref after_logout_url) = oauth_cfg.after_logout_url {
            params.append_pair("post_logout_redirect_uri", after_logout_url);
        }
    }
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Front-channel logout
// ---------------------------------------------------------------------------

/// Query parameters sent by the OIDC provider to the front-channel logout URI.
#[derive(Debug, Deserialize)]
pub struct FrontChannelLogoutQuery {
    pub iss: Option<String>,
    pub sid: Option<String>,
}

/// GET /logout/frontchannel
///
/// OIDC front-channel logout endpoint (OpenID Connect Front-Channel Logout 1.0).
///
/// The identity provider loads this URL in a hidden iframe when the End-User
/// logs out at the provider or via another relying party.  Clears the legacy
/// session cookie (a no-op when none was set) and returns `200 OK` without a
/// redirect so the provider's iframe can process the response.
pub async fn frontchannel_logout(
    State(state): State<AppState>,
    Query(query): Query<FrontChannelLogoutQuery>,
) -> Result<Response, AppError> {
    // ── 1. Validate iss; derive the Content-Security-Policy value ─────────
    //
    // When an issuer is configured:
    //   - `iss` is REQUIRED.  Omitting it must not bypass issuer validation —
    //     the previous outer `if let Some(iss_param)` guard silently accepted
    //     requests with no `iss` parameter even when an issuer was configured.
    //   - `iss` must exactly match the configured issuer.
    //   - On success, `frame-ancestors <issuer_origin>` restricts iframe
    //     embedding to the legitimate provider only.
    //
    // When no issuer (or no OAuth config) is present, `iss` is not validated
    // and `frame-ancestors 'none'` is emitted — no origin has a legitimate
    // reason to embed this endpoint.
    let frame_ancestors: String = if let Some(ref oauth_cfg) = state.oauth {
        if let Some(ref configured_issuer) = oauth_cfg.issuer {
            match query.iss.as_deref() {
                None => {
                    tracing::warn!("Front-channel logout rejected: iss parameter absent");
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        videocall_meeting_types::APIError::internal_error(
                            "iss parameter is required when an issuer is configured",
                        ),
                    ));
                }
                Some(iss_param) if iss_param != configured_issuer.as_str() => {
                    tracing::warn!(
                        iss_received = %iss_param,
                        iss_expected = %configured_issuer,
                        "Front-channel logout rejected: issuer mismatch",
                    );
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        videocall_meeting_types::APIError::internal_error(
                            "iss parameter does not match configured issuer",
                        ),
                    ));
                }
                Some(_) => {
                    // iss matched — restrict framing to the provider's origin.
                    // `origin()` serialises to "null" for opaque origins; fall
                    // back to 'none' rather than emitting `frame-ancestors null`.
                    Url::parse(configured_issuer)
                        .ok()
                        .and_then(|u| {
                            let origin = u.origin().unicode_serialization();
                            (origin != "null").then(|| format!("frame-ancestors {origin}"))
                        })
                        .unwrap_or_else(|| "frame-ancestors 'none'".to_string())
                }
            }
        } else {
            // OAuth configured but no known issuer — cannot determine the
            // legitimate embedder.
            "frame-ancestors 'none'".to_string()
        }
    } else {
        // No OAuth configured — no provider should be embedding this page.
        "frame-ancestors 'none'".to_string()
    };

    tracing::info!(
        sid = query.sid.as_deref().unwrap_or("<none>"),
        iss = query.iss.as_deref().unwrap_or("<none>"),
        "Processing OIDC front-channel logout",
    );

    let clear = build_clear_session_cookie(
        &state.cookie_name,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );
    let mut response = StatusCode::OK.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear)
            .map_err(|_| AppError::internal("failed to build clear cookie header"))?,
    );
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&frame_ancestors)
            .unwrap_or_else(|_| HeaderValue::from_static("frame-ancestors 'none'")),
    );
    Ok(response)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Validate and sanitize a `returnTo` value.
///
/// Accepts:
/// - Relative paths starting with `/` (but not `//`).
/// - Absolute `http(s)://` URLs whose origin matches `after_login_url` or any
///   entry in `allowed_redirect_urls`.
///
/// Returns `Some(sanitized_value)` on success, `None` on rejection.
fn validate_return_to(
    raw: &str,
    after_login_url: &str,
    allowed_redirect_urls: &[String],
) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.starts_with('/') {
        if trimmed.starts_with("//") {
            tracing::warn!(return_to = trimmed, "rejected protocol-relative returnTo");
            return None;
        }
        return Some(trimmed.to_string());
    }

    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        tracing::warn!(
            return_to = trimmed,
            "rejected returnTo with disallowed scheme"
        );
        return None;
    }

    let parsed = match Url::parse(trimmed) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(return_to = trimmed, error = %e, "rejected unparseable returnTo URL");
            return None;
        }
    };
    let candidate_origin = parsed.origin().unicode_serialization();

    if let Ok(base) = Url::parse(after_login_url) {
        if base.origin().unicode_serialization() == candidate_origin {
            return Some(trimmed.to_string());
        }
    }

    for allowed in allowed_redirect_urls {
        if let Ok(allowed_url) = Url::parse(allowed) {
            if allowed_url.origin().unicode_serialization() == candidate_origin {
                return Some(trimmed.to_string());
            }
        }
    }

    tracing::warn!(
        return_to = trimmed,
        origin = candidate_origin,
        "rejected returnTo: origin not in allowlist"
    );
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const AFTER_LOGIN: &str = "http://localhost:80";
    const ALLOWED: &[&str] = &["http://localhost:3001", "https://app.videocall.rs"];

    fn allowed() -> Vec<String> {
        ALLOWED.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn relative_path_accepted() {
        assert_eq!(
            validate_return_to("/meeting/123", AFTER_LOGIN, &allowed()),
            Some("/meeting/123".to_string())
        );
    }

    #[test]
    fn relative_root_accepted() {
        assert_eq!(
            validate_return_to("/", AFTER_LOGIN, &allowed()),
            Some("/".to_string())
        );
    }

    #[test]
    fn protocol_relative_rejected() {
        assert_eq!(
            validate_return_to("//evil.com/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn absolute_url_matching_after_login_origin() {
        assert_eq!(
            validate_return_to("http://localhost:80/meeting/1", AFTER_LOGIN, &allowed()),
            Some("http://localhost:80/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_matching_allowed_list() {
        assert_eq!(
            validate_return_to("http://localhost:3001/meeting/1", AFTER_LOGIN, &allowed()),
            Some("http://localhost:3001/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_https_allowed() {
        assert_eq!(
            validate_return_to(
                "https://app.videocall.rs/meeting/1",
                AFTER_LOGIN,
                &allowed()
            ),
            Some("https://app.videocall.rs/meeting/1".to_string())
        );
    }

    #[test]
    fn absolute_url_disallowed_origin() {
        assert_eq!(
            validate_return_to("http://evil.com/steal", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn javascript_scheme_rejected() {
        assert_eq!(
            validate_return_to("javascript:alert(1)", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn empty_string_rejected() {
        assert_eq!(validate_return_to("", AFTER_LOGIN, &allowed()), None);
    }

    #[test]
    fn port_mismatch_rejected() {
        assert_eq!(
            validate_return_to("http://localhost:9999/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn scheme_mismatch_rejected() {
        assert_eq!(
            validate_return_to("https://localhost:80/foo", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn data_scheme_rejected() {
        assert_eq!(
            validate_return_to("data:text/html,<h1>hi</h1>", AFTER_LOGIN, &allowed()),
            None
        );
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            validate_return_to("  /meeting/1  ", AFTER_LOGIN, &allowed()),
            Some("/meeting/1".to_string())
        );
    }

    #[test]
    fn empty_allowed_list_still_checks_after_login() {
        assert_eq!(
            validate_return_to("http://localhost:80/meeting/1", AFTER_LOGIN, &[]),
            Some("http://localhost:80/meeting/1".to_string())
        );
        assert_eq!(
            validate_return_to("http://localhost:3001/meeting/1", AFTER_LOGIN, &[]),
            None
        );
    }

    // --- build_clear_session_cookie ---

    #[test]
    fn clear_cookie_uses_name() {
        let cookie = build_clear_session_cookie("session", None, false);
        assert!(cookie.starts_with("session=;"));
    }

    #[test]
    fn clear_cookie_custom_name() {
        let cookie = build_clear_session_cookie("pr1-session", None, false);
        assert!(cookie.starts_with("pr1-session=;"));
    }

    #[test]
    fn clear_cookie_sets_max_age_zero() {
        let cookie = build_clear_session_cookie("session", None, false);
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn clear_cookie_domain_appended() {
        let cookie = build_clear_session_cookie("session", Some(".videocall.rs"), false);
        assert!(cookie.contains("Domain=.videocall.rs"));
    }

    // ---------------------------------------------------------------------------
    // build_end_session_url
    // ---------------------------------------------------------------------------

    fn minimal_oauth_config(
        end_session_endpoint: Option<String>,
        after_logout_url: Option<String>,
    ) -> crate::config::OAuthConfig {
        crate::config::OAuthConfig {
            client_id: "test-client".to_string(),
            client_secret: None,
            redirect_url: "https://app.example.com/auth/callback".to_string(),
            issuer: Some("https://provider.example.com".to_string()),
            auth_url: "https://provider.example.com/auth".to_string(),
            token_url: "https://provider.example.com/token".to_string(),
            jwks_url: None,
            userinfo_url: None,
            scopes: "openid email profile".to_string(),
            after_login_url: "https://app.example.com/".to_string(),
            allowed_redirect_urls: vec![],
            end_session_endpoint,
            after_logout_url,
            browser_pkce: false,
            resource_server_audience: None,
        }
    }

    // --- build_session_cookie ---

    #[test]
    fn session_cookie_contains_name_and_jwt() {
        let cookie = build_session_cookie("session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("session=my.jwt.token;"));
    }

    #[test]
    fn session_cookie_custom_name() {
        let cookie = build_session_cookie("pr1-session", "my.jwt.token", 3600, None, false);
        assert!(cookie.starts_with("pr1-session=my.jwt.token;"));
        // Must not be mistakable for a plain "session=" cookie.
        assert!(!cookie.starts_with("session="));
    }

    #[test]
    fn session_cookie_includes_required_attributes() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
    }

    #[test]
    fn session_cookie_secure_flag_added_when_true() {
        let cookie = build_session_cookie("session", "tok", 3600, None, true);
        assert!(cookie.contains("; Secure"));
    }

    #[test]
    fn session_cookie_no_secure_flag_when_false() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn session_cookie_domain_appended() {
        let cookie =
            build_session_cookie("session", "tok", 3600, Some(".sandbox.videocall.rs"), false);
        assert!(cookie.contains("Domain=.sandbox.videocall.rs"));
    }

    #[test]
    fn session_cookie_no_domain_when_none() {
        let cookie = build_session_cookie("session", "tok", 3600, None, false);
        assert!(!cookie.contains("Domain="));
    }

    #[test]
    fn build_end_session_url_includes_client_id() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/logout", &cfg).unwrap();
        assert!(
            url.contains("client_id=test-client"),
            "expected client_id in URL, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_includes_post_logout_redirect_uri_when_set() {
        let cfg = minimal_oauth_config(
            Some("https://provider.example.com/logout".to_string()),
            Some("https://app.example.com/after-logout".to_string()),
        );
        let url = build_end_session_url("https://provider.example.com/logout", &cfg).unwrap();
        assert!(
            url.contains("post_logout_redirect_uri="),
            "expected post_logout_redirect_uri in URL, got: {url}"
        );
        assert!(
            url.contains("app.example.com"),
            "expected redirect URI host in URL, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_omits_post_logout_redirect_uri_when_unset() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/end_session", &cfg).unwrap();
        assert!(
            !url.contains("post_logout_redirect_uri"),
            "should not contain post_logout_redirect_uri, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_preserves_existing_query_params() {
        let cfg = minimal_oauth_config(None, None);
        let url = build_end_session_url("https://provider.example.com/logout?realm=master", &cfg)
            .unwrap();
        assert!(
            url.contains("realm=master"),
            "existing query param should be preserved, got: {url}"
        );
        assert!(
            url.contains("client_id="),
            "client_id should be appended, got: {url}"
        );
    }

    #[test]
    fn build_end_session_url_rejects_invalid_base_url() {
        let cfg = minimal_oauth_config(None, None);
        let result = build_end_session_url("not-a-valid-url", &cfg);
        assert!(result.is_err(), "invalid base URL should produce an error");
    }

    // ---------------------------------------------------------------------------
    // Handler tests
    // ---------------------------------------------------------------------------

    use crate::state::AppState;
    use axum::body::Body as AxumBody;
    use sqlx::postgres::PgPoolOptions;

    fn make_handler_state(oauth: Option<crate::config::OAuthConfig>) -> AppState {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool creation should not fail");
        AppState {
            db,
            jwt_secret: "test-secret".to_string(),
            token_ttl_secs: 60,
            session_ttl_secs: 3600,
            oauth,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: "session".to_string(),
            cookie_secure: false,
            nats: None,
            service_version_urls: vec![],
            http_client: reqwest::Client::new(),
            display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            search: None,
            // OAuth handler tests exercise the OAuth-configured path; make
            // sure the anonymous fallback is off so behaviour matches prod.
            allow_anonymous: false,
        }
    }

    fn oauth_cfg_with_end_session(
        end_session_endpoint: &str,
        after_logout_url: Option<&str>,
    ) -> crate::config::OAuthConfig {
        minimal_oauth_config(
            Some(end_session_endpoint.to_string()),
            after_logout_url.map(|s| s.to_string()),
        )
    }

    #[tokio::test]
    async fn logout_returns_200_when_no_end_session_endpoint() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "should clear cookie, got: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn logout_redirects_when_end_session_endpoint_is_configured() {
        use tower::ServiceExt;
        let cfg = oauth_cfg_with_end_session(
            "https://provider.example.com/logout",
            Some("https://app.example.com/after-logout"),
        );
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .expect("Location header must be present")
            .to_str()
            .unwrap();
        assert!(
            location.starts_with("https://provider.example.com/logout"),
            "Location should point to provider end-session endpoint: {location}"
        );
        assert!(
            location.contains("client_id=test-client"),
            "client_id must be present: {location}"
        );
        assert!(
            location.contains("post_logout_redirect_uri="),
            "post_logout_redirect_uri must be present: {location}"
        );
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "cookie should be cleared alongside redirect: {set_cookie}"
        );
    }

    #[tokio::test]
    async fn logout_redirect_omits_post_logout_redirect_uri_when_not_configured() {
        use tower::ServiceExt;
        let cfg = oauth_cfg_with_end_session("https://provider.example.com/logout", None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route("/logout", axum::routing::get(logout))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !location.contains("post_logout_redirect_uri"),
            "post_logout_redirect_uri should be absent: {location}"
        );
    }

    // --- frontchannel_logout ---

    #[tokio::test]
    async fn frontchannel_logout_returns_200_and_clears_cookie() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            set_cookie.contains("Max-Age=0"),
            "should clear cookie: {set_cookie}"
        );
        assert!(
            resp.headers().get(header::LOCATION).is_none(),
            "front-channel logout must not redirect"
        );
    }

    #[tokio::test]
    async fn frontchannel_logout_accepts_matching_iss_param() {
        use tower::ServiceExt;
        let cfg = minimal_oauth_config(None, None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fprovider.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frontchannel_logout_rejects_mismatched_iss_param() {
        use tower::ServiceExt;
        let cfg = minimal_oauth_config(None, None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fevil.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn frontchannel_logout_skips_iss_validation_when_no_oauth_configured() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fanyone.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frontchannel_logout_accepts_sid_param() {
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?sid=abc123session")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn frontchannel_logout_rejects_absent_iss_when_issuer_configured() {
        // NEW: when an issuer is configured, omitting `iss` must return 400.
        // Previously the outer `if let Some(iss_param)` guard accepted absent
        // `iss` silently, bypassing issuer validation entirely.
        use tower::ServiceExt;
        let cfg = minimal_oauth_config(None, None);
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel") // no iss parameter
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "absent iss must be rejected when issuer is configured"
        );
    }

    #[tokio::test]
    async fn frontchannel_logout_csp_frame_ancestors_set_to_provider_origin() {
        // When iss matches the configured issuer, the response must carry
        // `frame-ancestors <issuer_origin>` so only that provider can embed
        // the endpoint in an iframe.
        use tower::ServiceExt;
        let cfg = minimal_oauth_config(None, None);
        // minimal_oauth_config sets issuer = "https://provider.example.com"
        let state = make_handler_state(Some(cfg));
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel?iss=https%3A%2F%2Fprovider.example.com")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .expect("Content-Security-Policy header must be present")
            .to_str()
            .unwrap();
        assert_eq!(
            csp, "frame-ancestors https://provider.example.com",
            "CSP must restrict framing to the configured issuer's origin"
        );
    }

    #[tokio::test]
    async fn frontchannel_logout_csp_frame_ancestors_none_without_oauth() {
        // When OAuth is not configured, `frame-ancestors 'none'` must be set
        // so no origin can embed the endpoint in an iframe.
        use tower::ServiceExt;
        let state = make_handler_state(None);
        let app = axum::Router::new()
            .route(
                "/logout/frontchannel",
                axum::routing::get(frontchannel_logout),
            )
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/logout/frontchannel")
            .body(AxumBody::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .expect("Content-Security-Policy header must be present")
            .to_str()
            .unwrap();
        assert_eq!(
            csp, "frame-ancestors 'none'",
            "CSP must deny all framing when OAuth is not configured"
        );
    }

    // --- exchange: Cache-Control ---

    /// Drive the real `exchange` handler end-to-end against a local mock token
    /// endpoint and assert that the response carries `Cache-Control: no-store`.
    ///
    /// The mock server returns a pre-built JWT whose claims are decoded without
    /// JWKS signature verification (`jwks_cache: None`); this isolates the
    /// test from network I/O while still exercising the full handler code path
    /// that builds the response and inserts the header.
    #[tokio::test]
    async fn exchange_response_carries_no_store_cache_control() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        use tower::ServiceExt;

        // Build a fake JWT whose payload has the required claims.  With
        // `jwks_cache: None` the handler calls `decode_id_token_claims_unverified`,
        // which only base64-decodes the payload — no signature check.
        let now = chrono::Utc::now().timestamp();
        let payload_json = serde_json::json!({
            "sub":   "user@example.com",
            "email": "user@example.com",
            "name":  "Test User",
            "exp":   now + 3600,
            "iat":   now,
        })
        .to_string();
        let header_b64 = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        let fake_id_token = format!("{header_b64}.{payload_b64}.fakesig");

        // Build the JSON the mock token endpoint will return.
        let token_body = serde_json::json!({
            "access_token": "fake-access-token",
            "token_type":   "Bearer",
            "id_token":     fake_id_token,
        })
        .to_string();

        // Spawn a minimal Axum server that acts as the OAuth token endpoint.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = listener.local_addr().unwrap();
        let token_body_clone = token_body.clone();
        let mock_router = axum::Router::new().route(
            "/token",
            axum::routing::post(move || {
                let body = token_body_clone.clone();
                async move {
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, mock_router).await.ok();
        });

        // Configure the exchange handler to call our local mock token endpoint.
        // No JWKS cache → unverified decode path; no issuer check.
        let mut cfg = minimal_oauth_config(None, None);
        cfg.token_url = format!("http://{mock_addr}/token");
        cfg.issuer = None;
        let state = make_handler_state(Some(cfg));

        let app = axum::Router::new()
            .route("/api/v1/oauth/exchange", axum::routing::post(exchange))
            .with_state(state);

        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/api/v1/oauth/exchange")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({"code": "test-code", "code_verifier": "test-verifier"})
                    .to_string(),
            ))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();

        // The handler must succeed and set Cache-Control: no-store.
        assert_eq!(resp.status(), StatusCode::OK, "exchange must return 200");
        let cache_control = resp
            .headers()
            .get(header::CACHE_CONTROL)
            .expect("Cache-Control header must be present on exchange response")
            .to_str()
            .unwrap();
        assert_eq!(
            cache_control, "no-store",
            "exchange response must carry Cache-Control: no-store"
        );
    }
}
