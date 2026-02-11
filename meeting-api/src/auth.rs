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

//! Axum extractor that pulls the authenticated user's email from the `email` cookie.

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, StatusCode},
};
use videocall_meeting_types::APIError;

use crate::error::AppError;

/// Extractor that resolves the authenticated user's email from the `email` cookie.
///
/// Usage in a handler:
/// ```ignore
/// async fn my_handler(AuthUser(email): AuthUser) { ... }
/// ```
#[derive(Debug)]
pub struct AuthUser(pub String);

impl<S: Send + Sync> FromRequestParts<S> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        for pair in cookie_header.split(';') {
            let pair = pair.trim();
            if let Some(value) = pair.strip_prefix("email=") {
                let email = value.trim();
                if !email.is_empty() {
                    return Ok(AuthUser(email.to_string()));
                }
            }
        }

        Err(AppError::new(
            StatusCode::UNAUTHORIZED,
            APIError::unauthorized(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    /// Helper: run the AuthUser extractor against a request with the given cookie header.
    async fn extract_auth(cookie_header: Option<&str>) -> Result<AuthUser, AppError> {
        let mut builder = Request::builder().uri("/test").method("GET");
        if let Some(val) = cookie_header {
            builder = builder.header(header::COOKIE, val);
        }
        let req = builder.body(()).unwrap();
        let (mut parts, _body) = req.into_parts();
        AuthUser::from_request_parts(&mut parts, &()).await
    }

    #[tokio::test]
    async fn valid_email_cookie_returns_auth_user() {
        let result = extract_auth(Some("email=user@example.com")).await;
        let auth = result.expect("should succeed");
        assert_eq!(auth.0, "user@example.com");
    }

    #[tokio::test]
    async fn missing_cookie_header_returns_unauthorized() {
        let err = extract_auth(None).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
        assert_eq!(err.body.code, "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn empty_email_value_returns_unauthorized() {
        let err = extract_auth(Some("email=")).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn email_not_first_cookie_still_found() {
        let result = extract_auth(Some("session=abc123; email=alice@test.com; lang=en")).await;
        let auth = result.expect("should find email in middle");
        assert_eq!(auth.0, "alice@test.com");
    }

    #[tokio::test]
    async fn whitespace_around_cookie_value_is_trimmed() {
        let result = extract_auth(Some("email=  bob@test.com  ")).await;
        let auth = result.expect("should trim whitespace");
        assert_eq!(auth.0, "bob@test.com");
    }
}
