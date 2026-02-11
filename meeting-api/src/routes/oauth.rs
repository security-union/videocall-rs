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

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
    Json,
};
use oauth2::{CsrfToken, PkceCodeChallenge};
use serde::Deserialize;

use crate::db::oauth as db_oauth;
use crate::error::AppError;
use crate::oauth;
use crate::state::AppState;

/// Cookie max-age in seconds (~10 years, matching the existing behavior).
const COOKIE_MAX_AGE_SECS: i64 = 87600 * 3600;

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

fn build_set_cookie(name: &str, value: &str, domain: Option<&str>) -> String {
    let mut cookie = format!("{name}={value}; Path=/; SameSite=Lax; Max-Age={COOKIE_MAX_AGE_SECS}");
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

fn build_clear_cookie(name: &str, domain: Option<&str>) -> String {
    let mut cookie = format!("{name}=; Path=/; Max-Age=0");
    if let Some(d) = domain {
        cookie.push_str(&format!("; Domain={d}"));
    }
    cookie
}

/// GET /login?returnTo=<url>
///
/// Initiates the OAuth flow: generates PKCE + CSRF, stores in DB, redirects to Google.
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
/// Handles the OAuth callback: exchanges code for tokens, sets session cookies, redirects.
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

    let redirect_url = oauth_req
        .return_to
        .unwrap_or_else(|| oauth_cfg.after_login_url.clone());

    let domain = state.cookie_domain.as_deref();
    let email_cookie = build_set_cookie("email", &claims.email, domain);
    let name_cookie = build_set_cookie("name", &claims.name, domain);

    tracing::info!(
        "OAuth login successful for {} ({}), redirecting to {}",
        claims.name,
        claims.email,
        redirect_url
    );

    let mut response = Redirect::to(&redirect_url).into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&email_cookie).unwrap(),
    );
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&name_cookie).unwrap(),
    );
    Ok(response)
}

/// GET /session -- returns 200 if email cookie is present, 401 otherwise.
pub async fn check_session(headers: axum::http::HeaderMap) -> Result<StatusCode, StatusCode> {
    let has_session = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|c| {
            c.split(';').any(|p| {
                let p = p.trim();
                p.starts_with("email=") && p.len() > "email=".len()
            })
        })
        .unwrap_or(false);

    if has_session {
        Ok(StatusCode::OK)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// GET /profile -- returns { "email": "...", "name": "..." } from cookies.
pub async fn get_profile(
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let cookies = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let email = extract_cookie_value(cookies, "email").ok_or(StatusCode::UNAUTHORIZED)?;
    let name = extract_cookie_value(cookies, "name").unwrap_or_else(|| email.clone());

    Ok(Json(serde_json::json!({ "email": email, "name": name })))
}

/// GET /logout -- clears session cookies.
pub async fn logout(State(state): State<AppState>) -> Response {
    let domain = state.cookie_domain.as_deref();
    let email_clear = build_clear_cookie("email", domain);
    let name_clear = build_clear_cookie("name", domain);

    let mut response = StatusCode::OK.into_response();
    let headers = response.headers_mut();
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&email_clear).unwrap(),
    );
    headers.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&name_clear).unwrap(),
    );
    response
}

fn extract_cookie_value(cookies: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for pair in cookies.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(&prefix) {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}
