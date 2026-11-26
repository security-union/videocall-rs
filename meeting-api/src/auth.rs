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

//! Axum extractors that authenticate the caller.
//!
//! - [`AuthUser`]: browser session only. The signed HS256 session JWT comes
//!   from `Cookie: <name>=<JWT>` (set by the OAuth callback as `HttpOnly`) or
//!   from `Authorization: Bearer <JWT>`. A device JWT (header `alg: EdDSA`)
//!   gets 403 `BROWSER_SESSION_REQUIRED`. This is the default for every route.
//! - [`CliOrBrowserUser`]: additionally accepts a device JWT as bearer,
//!   verified against the `devices` table. Used only on get meeting, join and
//!   status.
//!
//! The algorithm decides the path and is then pinned by the verifier: HS256
//! tokens only ever reach the session decoder, EdDSA tokens only ever reach
//! [`crate::device_auth::verify_device_jwt`].

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use videocall_meeting_types::APIError;

use crate::device_auth::{self, AllowedStates, DeviceAuthError};
use crate::error::AppError;
use crate::state::AppState;
use crate::token;

/// How the caller authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVia {
    /// Browser session JWT (cookie or bearer).
    Browser,
    /// Device JWT of a live, approved CLI device.
    Cli,
}

/// Extractor for a browser session (signed HS256 session JWT in the cookie or
/// the bearer header).
///
/// Usage in a handler:
/// ```ignore
/// async fn my_handler(AuthUser { user_id, .. }: AuthUser) { ... }
/// ```
#[derive(Debug)]
pub struct AuthUser {
    pub user_id: String,
    pub name: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // The cookie only ever carries a session JWT.
        if let Some(token) = extract_cookie_token(&parts.headers, &state.cookie_name) {
            let claims = token::decode_session_token(&state.jwt_secret, &token)?;
            return Ok(AuthUser {
                user_id: claims.sub,
                name: claims.name,
            });
        }
        let token = extract_bearer_token(&parts.headers).ok_or_else(AppError::unauthorized)?;
        if device_auth::is_device_token(&token) {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                APIError::browser_session_required(),
            ));
        }
        let claims = token::decode_session_token(&state.jwt_secret, &token)?;
        Ok(AuthUser {
            user_id: claims.sub,
            name: claims.name,
        })
    }
}

/// Extractor for routes a CLI device may call (get meeting, join, status).
///
/// A browser session resolves exactly like [`AuthUser`]. A bearer whose header
/// says `alg: EdDSA` is verified as a device JWT with
/// `aud = DEVICE_TOKEN_AUDIENCE` and must belong to a live, approved device;
/// `user_id` and `name` then come from the device row.
#[derive(Debug)]
pub struct CliOrBrowserUser {
    pub user_id: String,
    pub name: String,
    pub via: AuthVia,
}

impl FromRequestParts<AppState> for CliOrBrowserUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let device_token = extract_cookie_token(&parts.headers, &state.cookie_name)
            .is_none()
            .then(|| extract_bearer_token(&parts.headers))
            .flatten()
            .filter(|token| device_auth::is_device_token(token));

        let Some(token) = device_token else {
            let user = AuthUser::from_request_parts(parts, state)
                .await
                .map_err(IntoResponse::into_response)?;
            return Ok(CliOrBrowserUser {
                user_id: user.user_id,
                name: user.name,
                via: AuthVia::Browser,
            });
        };

        let Some(audience) = state.device.audience.as_deref() else {
            tracing::info!("device token rejected: DEVICE_TOKEN_AUDIENCE is not configured");
            return Err(device_unauthorized());
        };
        let now = chrono::Utc::now();
        match device_auth::verify_device_jwt(
            &state.db,
            &token,
            audience,
            now,
            AllowedStates::LiveApproved,
        )
        .await
        {
            Ok(row) => {
                device_auth::touch_live_device(&state.db, &row, now).await;
                Ok(CliOrBrowserUser {
                    user_id: row.user_id,
                    name: row.user_name,
                    via: AuthVia::Cli,
                })
            }
            Err(DeviceAuthError::Db(e)) => Err(AppError::from(e).into_response()),
            Err(err) => {
                let kid = jsonwebtoken::decode_header(&token)
                    .ok()
                    .and_then(|h| h.kid)
                    .filter(|kid| videocall_meeting_types::device::is_valid_device_id(kid));
                tracing::info!(device_id = kid.as_deref().unwrap_or("-"), "{err}");
                Err(device_unauthorized())
            }
        }
    }
}

/// 401 for a rejected device token: the usual `UNAUTHORIZED` body plus
/// `WWW-Authenticate: Bearer error="invalid_token"`. Never says why.
pub(crate) fn device_unauthorized() -> Response {
    let mut response = AppError::unauthorized().into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer error=\"invalid_token\""),
    );
    response
}

/// Extract the session JWT from the configured session cookie.
pub(crate) fn extract_cookie_token(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE).and_then(|v| v.to_str().ok())?;
    let prefix = format!("{cookie_name}=");
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix(prefix.as_str()) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Extract a token from `Authorization: Bearer <token>`.
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?;
    let token = auth.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::generate_session_token;
    use axum::http::Request;
    const TEST_SECRET: &str = "test-secret-for-auth-tests";

    fn make_test_state() -> AppState {
        make_state_with_cookie_name("session")
    }

    fn make_state_with_cookie_name(name: &str) -> AppState {
        // connect_lazy creates a pool handle without actually connecting.
        // The URL is never used because no queries are executed in unit tests.
        #[cfg(feature = "postgres")]
        let db = {
            use sqlx::postgres::PgPoolOptions;
            PgPoolOptions::new()
                .max_connections(1)
                .connect_lazy("postgres://localhost/unused")
                .expect("lazy pool creation should not fail")
        };
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        let db = {
            use sqlx::sqlite::SqlitePoolOptions;
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_lazy("sqlite::memory:")
                .expect("lazy pool creation should not fail")
        };
        AppState {
            db,
            jwt_secret: TEST_SECRET.to_string(),
            token_ttl_secs: 600,
            session_ttl_secs: 3600,
            oauth: None,
            jwks_cache: None,
            cookie_domain: None,
            cookie_name: name.to_string(),
            cookie_secure: false,
            nats: None,
            service_version_urls: Vec::new(),
            http_client: reqwest::Client::new(),
            device: crate::config::DeviceAuthConfig::default(),
        }
    }

    async fn extract_with_cookie(cookie: Option<&str>) -> Result<AuthUser, AppError> {
        let state = make_test_state();
        extract_with_cookie_and_state(cookie, &state).await
    }

    async fn extract_with_cookie_and_state(
        cookie: Option<&str>,
        state: &AppState,
    ) -> Result<AuthUser, AppError> {
        let mut builder = Request::builder().uri("/test").method("GET");
        if let Some(val) = cookie {
            builder = builder.header(header::COOKIE, val);
        }
        let req = builder.body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, state).await
    }

    async fn extract_with_bearer(token: &str) -> Result<AuthUser, AppError> {
        let state = make_test_state();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, &state).await
    }

    #[tokio::test]
    async fn valid_session_cookie_returns_auth_user() {
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "alice@test.com");
    }

    #[tokio::test]
    async fn valid_bearer_token_returns_auth_user() {
        let jwt = generate_session_token(TEST_SECRET, "bob@test.com", "Bob", 3600).unwrap();
        let auth = extract_with_bearer(&jwt).await.expect("should succeed");
        assert_eq!(auth.user_id, "bob@test.com");
        assert_eq!(auth.name, "Bob");
    }

    #[tokio::test]
    async fn valid_cookie_extracts_name() {
        let jwt =
            generate_session_token(TEST_SECRET, "alice@test.com", "Alice Wonder", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "alice@test.com");
        assert_eq!(auth.name, "Alice Wonder");
    }

    #[tokio::test]
    async fn expired_bearer_token_returns_unauthorized() {
        let jwt = generate_session_token(TEST_SECRET, "a@b.com", "A", -120).unwrap();
        let err = extract_with_bearer(&jwt).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_bearer_token_returns_unauthorized() {
        let err = extract_with_bearer("not-a-valid-jwt").await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_secret_bearer_token_returns_unauthorized() {
        let jwt = generate_session_token("wrong-secret", "a@b.com", "A", 3600).unwrap();
        let err = extract_with_bearer(&jwt).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_bearer_token_returns_unauthorized() {
        let err = extract_with_bearer("").await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn missing_credentials_returns_unauthorized() {
        let err = extract_with_cookie(None).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_jwt_returns_unauthorized() {
        let err = extract_with_cookie(Some("session=not-a-valid-jwt"))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn expired_jwt_returns_unauthorized() {
        let jwt = generate_session_token(TEST_SECRET, "a@b.com", "A", -120).unwrap();
        let err = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_secret_returns_unauthorized() {
        let jwt = generate_session_token("different-secret", "a@b.com", "A", 3600).unwrap();
        let err = extract_with_cookie(Some(&format!("session={jwt}")))
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_takes_precedence_over_bearer() {
        let cookie_jwt =
            generate_session_token(TEST_SECRET, "cookie@test.com", "Cookie", 3600).unwrap();
        let bearer_jwt =
            generate_session_token(TEST_SECRET, "bearer@test.com", "Bearer", 3600).unwrap();

        let state = make_test_state();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::COOKIE, format!("session={cookie_jwt}"))
            .header(header::AUTHORIZATION, format!("Bearer {bearer_jwt}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("should succeed");
        assert_eq!(auth.user_id, "cookie@test.com");
    }

    #[tokio::test]
    async fn session_cookie_among_other_cookies() {
        let jwt = generate_session_token(TEST_SECRET, "multi@test.com", "Multi", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("lang=en; session={jwt}; theme=dark")))
            .await
            .expect("should find session in middle");
        assert_eq!(auth.user_id, "multi@test.com");
    }

    // -----------------------------------------------------------------------
    // Custom cookie name tests (PR preview collision fix)
    // -----------------------------------------------------------------------

    /// PR preview API configured with "pr1-session" accepts a pr1-session= cookie.
    #[tokio::test]
    async fn custom_cookie_name_is_accepted() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let auth = extract_with_cookie_and_state(Some(&format!("pr1-session={jwt}")), &state)
            .await
            .expect("pr1-session cookie should be accepted");
        assert_eq!(auth.user_id, "alice@test.com");
    }

    /// Core regression test: PR preview API configured with "pr1-session" must
    /// reject a "session=" cookie — exactly what the production API sets with
    /// Domain=.videocall.rs, which the browser would otherwise send to
    /// pr1-api.sandbox.videocall.rs causing a 401.
    #[tokio::test]
    async fn production_session_cookie_rejected_by_preview_api() {
        let state = make_state_with_cookie_name("pr1-session");
        let production_jwt =
            generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        // Even with a valid JWT, the wrong cookie name must be rejected.
        let err = extract_with_cookie_and_state(Some(&format!("session={production_jwt}")), &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    /// Slot isolation: pr2-session= is rejected when the API expects pr1-session=.
    #[tokio::test]
    async fn different_slot_cookie_rejected() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "alice@test.com", "Alice", 3600).unwrap();
        let err = extract_with_cookie_and_state(Some(&format!("pr2-session={jwt}")), &state)
            .await
            .unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    /// Custom cookie name is found correctly when mixed with other cookies,
    /// including a same-named-prefix cookie that should not match.
    #[tokio::test]
    async fn custom_cookie_name_among_other_cookies() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "multi@test.com", "Multi", 3600).unwrap();
        // "session" appears as a prefix of "pr1-session" in the cookie header —
        // verify we match the full name and don't accidentally split on it.
        let auth = extract_with_cookie_and_state(
            Some(&format!(
                "lang=en; session=garbage; pr1-session={jwt}; theme=dark"
            )),
            &state,
        )
        .await
        .expect("should find pr1-session and ignore session=garbage");
        assert_eq!(auth.user_id, "multi@test.com");
    }

    /// Bearer token still works regardless of cookie_name configuration.
    #[tokio::test]
    async fn bearer_works_with_custom_cookie_name() {
        let state = make_state_with_cookie_name("pr1-session");
        let jwt = generate_session_token(TEST_SECRET, "bob@test.com", "Bob", 3600).unwrap();
        let req = Request::builder()
            .uri("/test")
            .method("GET")
            .header(header::AUTHORIZATION, format!("Bearer {jwt}"))
            .body(())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let auth = AuthUser::from_request_parts(&mut parts, &state)
            .await
            .expect("bearer should work regardless of cookie_name");
        assert_eq!(auth.user_id, "bob@test.com");
    }
}
