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
    use crate::config::DevUser;
    use sqlx::postgres::PgPoolOptions;

    const TEST_SECRET: &str = "test-secret-for-dev-auto-login";

    /// Build a minimal `AppState` suitable for `auto_login` handler tests.
    /// Uses `connect_lazy` so no database connection is established (no
    /// queries are executed inside the handler).
    fn make_state(dev_user: Option<DevUser>) -> AppState {
        let db = PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy("postgres://localhost/unused")
            .expect("lazy pool creation should not fail");
        AppState {
            db,
            jwt_secret: TEST_SECRET.to_string(),
            token_ttl_secs: 600,
            session_ttl_secs: 3600,
            oauth: None,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: "session".to_string(),
            cookie_secure: false,
            nats: None,
            service_version_urls: Vec::new(),
            http_client: reqwest::Client::new(),
            display_name_rate_limiter: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            display_name_rate_limiter_ops: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            search: None,
            display_name_rate_limit_disabled: false,
            dev_user,
        }
    }

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
        let cookie = build_dev_session_cookie("session", "tok", 3600, Some(".example.com"), true);
        assert!(cookie.contains("; Secure"));
        assert!(cookie.contains("Domain=.example.com"));
    }

    /// Production-safety invariant: when `DEV_USER` is unset, the endpoint
    /// MUST return 404 so it is invisible to clients. This is the only thing
    /// preventing a misconfigured production deploy (with `DEV_USER` accidentally
    /// scrubbed but the route still mounted) from exposing auto-login.
    #[tokio::test]
    async fn auto_login_returns_404_when_dev_user_unset() {
        let state = make_state(None);
        let result = auto_login(axum::extract::State(state)).await;
        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    /// Happy path: when `DEV_USER` is set, the endpoint returns a 303-or-302
    /// redirect to "/" with a Set-Cookie carrying the configured cookie name.
    #[tokio::test]
    async fn auto_login_returns_redirect_with_session_cookie_when_dev_user_set() {
        let state = make_state(Some(DevUser {
            email: "dev@local.test".to_string(),
            name: "Dev User".to_string(),
        }));
        let response = auto_login(axum::extract::State(state))
            .await
            .expect("handler should succeed when dev_user is Some");

        // axum's Redirect::to defaults to 303 See Other.
        assert!(
            response.status().is_redirection(),
            "expected redirect status, got {}",
            response.status()
        );

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie header must be present")
            .to_str()
            .expect("Set-Cookie value should be valid UTF-8");
        assert!(
            set_cookie.starts_with("session="),
            "Set-Cookie should use the configured cookie name, got: {set_cookie}"
        );
        assert!(set_cookie.contains("HttpOnly"));
        assert!(set_cookie.contains("SameSite=Lax"));
    }

    /// Decoded session JWT carries the configured DEV_USER identity.
    /// Guarantees the auto-login user can't be silently swapped for a
    /// different principal (e.g. an attacker-controlled email) by the handler.
    #[tokio::test]
    async fn auto_login_session_jwt_decodes_to_configured_dev_user() {
        let state = make_state(Some(DevUser {
            email: "alice@example.test".to_string(),
            name: "Alice Example".to_string(),
        }));
        let response = auto_login(axum::extract::State(state))
            .await
            .expect("handler should succeed when dev_user is Some");

        let set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("Set-Cookie header must be present")
            .to_str()
            .unwrap();
        // Extract the JWT from "session=<jwt>; ..."
        let jwt = set_cookie
            .strip_prefix("session=")
            .and_then(|rest| rest.split(';').next())
            .expect("cookie should start with session=<jwt>; ...");

        let claims = token::decode_session_token(TEST_SECRET, jwt)
            .expect("session JWT issued by handler should decode with the configured secret");
        assert_eq!(claims.sub, "alice@example.test");
        assert_eq!(claims.name, "Alice Example");
    }
}
