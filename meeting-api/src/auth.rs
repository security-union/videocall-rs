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

//! Axum extractor that authenticates the user via a signed session JWT.
//!
//! The JWT can be delivered in two ways (checked in order):
//!
//! 1. `Cookie: session=<JWT>` -- set by the OAuth callback as `HttpOnly`.
//! 2. `Authorization: Bearer <JWT>` -- for non-browser clients.
//!
//! The extractor validates the JWT signature using the shared secret from
//! [`AppState`](crate::state::AppState) and extracts the user's email from
//! the `sub` claim.

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, StatusCode},
};
use videocall_meeting_types::APIError;

use crate::error::AppError;
use crate::state::AppState;
use crate::token;

/// Extractor that resolves the authenticated user from a signed session JWT
/// (cookie or Bearer header).
///
/// Usage in a handler:
/// ```ignore
/// async fn my_handler(AuthUser { email, .. }: AuthUser) { ... }
/// ```
#[derive(Debug)]
pub struct AuthUser {
    pub email: String,
    pub name: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = extract_session_token(parts)
            .ok_or_else(|| AppError::new(StatusCode::UNAUTHORIZED, APIError::unauthorized()))?;

        let claims = token::decode_session_token(&state.jwt_secret, &token)?;

        Ok(AuthUser {
            email: claims.sub,
            name: claims.name,
        })
    }
}

/// Extract the raw session JWT from the request.
///
/// Checks (in order):
/// 1. `Cookie: session=<jwt>`
/// 2. `Authorization: Bearer <jwt>`
fn extract_session_token(parts: &Parts) -> Option<String> {
    // 1. Try the `session` cookie.
    if let Some(cookie_header) = parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for pair in cookie_header.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix("session=") {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    // 2. Fall back to `Authorization: Bearer <token>`.
    if let Some(auth) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::generate_session_token;
    use axum::http::Request;
    use sqlx::postgres::PgPoolOptions;

    const TEST_SECRET: &str = "test-secret-for-auth-tests";

    fn make_test_state() -> AppState {
        // connect_lazy creates a pool handle without actually connecting.
        // The URL is never used because no queries are executed in unit tests.
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
            cookie_domain: None,
            cookie_secure: false,
        }
    }

    async fn extract_with_cookie(cookie: Option<&str>) -> Result<AuthUser, AppError> {
        let state = make_test_state();
        let mut builder = Request::builder().uri("/test").method("GET");
        if let Some(val) = cookie {
            builder = builder.header(header::COOKIE, val);
        }
        let req = builder.body(()).unwrap();
        let (mut parts, _) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, &state).await
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
        assert_eq!(auth.email, "alice@test.com");
    }

    #[tokio::test]
    async fn valid_bearer_token_returns_auth_user() {
        let jwt = generate_session_token(TEST_SECRET, "bob@test.com", "Bob", 3600).unwrap();
        let auth = extract_with_bearer(&jwt).await.expect("should succeed");
        assert_eq!(auth.email, "bob@test.com");
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
        assert_eq!(auth.email, "cookie@test.com");
    }

    #[tokio::test]
    async fn session_cookie_among_other_cookies() {
        let jwt = generate_session_token(TEST_SECRET, "multi@test.com", "Multi", 3600).unwrap();
        let auth = extract_with_cookie(Some(&format!("lang=en; session={jwt}; theme=dark")))
            .await
            .expect("should find session in middle");
        assert_eq!(auth.email, "multi@test.com");
    }
}
