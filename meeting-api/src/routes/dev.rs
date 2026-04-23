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

//! Development-only auto-login endpoint.
//!
//! When the `DEV_USER` environment variable is set and OAuth is disabled,
//! `GET /api/v1/dev/auto-login` issues a signed session JWT cookie for
//! the configured dev identity and redirects to `/`.
//!
//! This eliminates the need to manually inject session cookies during
//! local development. The endpoint returns 404 when OAuth is enabled or
//! `DEV_USER` is not set, making it invisible in production.

use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

use crate::state::AppState;
use crate::token;

/// `GET /api/v1/dev/auto-login`
///
/// Issues a session cookie for the configured `DEV_USER` and redirects to `/`.
/// Returns 404 when `DEV_USER` is not configured or OAuth is enabled.
pub async fn auto_login(State(state): State<AppState>) -> Result<Response, StatusCode> {
    let dev_user = state.dev_user.as_ref().ok_or(StatusCode::NOT_FOUND)?;

    let session_jwt = token::generate_session_token(
        &state.jwt_secret,
        &dev_user.email,
        &dev_user.name,
        state.session_ttl_secs,
    )
    .map_err(|e| {
        tracing::error!("DEV_USER auto-login: failed to generate session JWT: {e:?}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let session_cookie = build_dev_session_cookie(
        &state.cookie_name,
        &session_jwt,
        state.session_ttl_secs,
        state.cookie_domain.as_deref(),
        state.cookie_secure,
    );

    tracing::info!(
        "DEV_USER auto-login: issued session for {} ({})",
        dev_user.name,
        dev_user.email
    );

    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    Ok(response)
}

/// Build a `Set-Cookie` header value for the dev session JWT.
///
/// Uses the same attributes as the OAuth callback cookie builder.
fn build_dev_session_cookie(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_session_cookie_format() {
        let cookie = build_dev_session_cookie("session", "my.jwt.tok", 3600, None, false);
        assert!(cookie.starts_with("session=my.jwt.tok;"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(!cookie.contains("Secure"));
        assert!(!cookie.contains("Domain="));
    }

    #[test]
    fn dev_session_cookie_with_secure_and_domain() {
        let cookie =
            build_dev_session_cookie("session", "tok", 3600, Some(".example.com"), true);
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("Domain=.example.com"));
    }
}
