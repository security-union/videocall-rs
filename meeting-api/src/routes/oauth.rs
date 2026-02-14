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

//! OAuth route handlers: login, callback, session, profile, logout.
//!
//! After a successful OAuth login the callback issues a **signed session JWT**
//! inside an `HttpOnly; Secure; SameSite=Lax` cookie named `session`.
//! JavaScript cannot read the cookie; the browser sends it automatically.

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;

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
fn build_session_cookie(jwt: &str, ttl_secs: i64, domain: Option<&str>, secure: bool) -> String {
    let mut cookie = format!("session={jwt}; Path=/; HttpOnly; SameSite=Lax; Max-Age={ttl_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

/// Build a `Set-Cookie` header that clears the `session` cookie.
fn build_clear_session_cookie(domain: Option<&str>, secure: bool) -> String {
    let mut cookie = "session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0".to_string();
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
/// Initiates the OAuth flow: generates PKCE + CSRF, stores in DB, redirects to
/// the identity provider.
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

    db_oauth::store_oauth_request(
        &state.db,
        pkce_challenge.as_str(),
        pkce_verifier.secret(),
        csrf_token.secret(),
        query.return_to.as_deref(),
    )
    .await?;

    let auth_url = oauth::build_auth_url(
        &oauth_cfg.auth_url,
        &oauth_cfg.client_id,
        &oauth_cfg.redirect_url,
        pkce_challenge.as_str(),
        csrf_token.secret(),
    );

    Ok(Redirect::to(&auth_url).into_response())
}

/// GET /login/callback?state=...&code=...
///
/// Handles the OAuth callback: exchanges the authorization code for tokens,
/// creates a signed session JWT, and sets it as an `HttpOnly` cookie.
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

    let (token_response, claims) = oauth::exchange_code_for_claims(
        &oauth_cfg.redirect_url,
        &oauth_cfg.client_id,
        &oauth_cfg.client_secret,
        &pkce_verifier,
        &oauth_cfg.token_url,
        &query.code,
    )
    .await?;

    db_oauth::upsert_user(
        &state.db,
        &claims.email,
        &claims.name,
        &token_response.access_token,
        token_response.refresh_token.as_deref(),
    )
    .await?;

    // --- Issue signed session JWT inside an HttpOnly cookie ---
    let session_jwt = token::generate_session_token(
        &state.jwt_secret,
        &claims.email,
        &claims.name,
        state.session_ttl_secs,
    )?;

    let redirect_url = oauth_req
        .return_to
        .unwrap_or_else(|| oauth_cfg.after_login_url.clone());

    let session_cookie = build_session_cookie(
        &session_jwt,
        state.session_ttl_secs,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );

    tracing::info!(
        "OAuth login successful for {} ({}), redirecting to {}",
        claims.name,
        claims.email,
        redirect_url
    );

    let mut response = Redirect::to(&redirect_url).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie).unwrap(),
    );
    Ok(response)
}

/// GET /session -- returns 200 if the session JWT is valid, 401 otherwise.
///
/// The `AuthUser` extractor validates the session JWT from the `session`
/// cookie (or `Authorization: Bearer` header).
pub async fn check_session(AuthUser { .. }: AuthUser) -> StatusCode {
    StatusCode::OK
}

/// GET /profile -- returns `{ "email": "...", "name": "..." }` from the
/// session JWT claims.
///
/// Because the session JWT embeds both email and display name, this endpoint
/// does not need a database query.
pub async fn get_profile(AuthUser { email, name }: AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "email": email, "name": name }))
}

/// GET /logout -- clears the session cookie.
pub async fn logout(State(state): State<AppState>) -> Response {
    let clear = build_clear_session_cookie(state.cookie_domain.as_deref(), state.cookie_secure);
    let mut response = StatusCode::OK.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, HeaderValue::from_str(&clear).unwrap());
    response
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
