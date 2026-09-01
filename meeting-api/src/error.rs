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

//! Application error type that implements Axum's `IntoResponse`.
//!
//! Every error is returned as `APIResponse<APIError>` with `success: false`,
//! paired with the appropriate HTTP status code.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use videocall_meeting_types::{APIError, APIResponse};

/// Application-level error that pairs an HTTP status code with an [`APIError`].
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub body: APIError,
}

impl AppError {
    pub fn new(status: StatusCode, body: APIError) -> Self {
        Self { status, body }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, APIError::unauthorized())
    }

    pub fn unauthorized_msg(detail: &str) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            APIError::unauthorized_with_detail(detail),
        )
    }

    pub fn invalid_input(detail: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, APIError::invalid_input(detail))
    }

    pub fn invalid_meeting_id(detail: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            APIError::invalid_meeting_id(detail),
        )
    }

    pub fn too_many_attendees(count: usize, max: usize) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            APIError::too_many_attendees(count, max),
        )
    }

    pub fn meeting_exists(meeting_id: &str) -> Self {
        Self::new(StatusCode::CONFLICT, APIError::meeting_exists(meeting_id))
    }

    pub fn meeting_not_found(meeting_id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            APIError::meeting_not_found(meeting_id),
        )
    }

    pub fn meeting_not_active(meeting_id: &str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            APIError::meeting_not_active(meeting_id),
        )
    }

    pub fn not_host() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::not_host())
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, APIError::bad_request(detail))
    }

    pub fn not_owner() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::not_owner())
    }

    pub fn participant_not_found(user_id: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            APIError::participant_not_found(user_id),
        )
    }

    pub fn not_in_meeting() -> Self {
        Self::new(StatusCode::NOT_FOUND, APIError::not_in_meeting())
    }

    pub fn joining_not_allowed() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::joining_not_allowed())
    }

    pub fn invalid_display_name() -> Self {
        Self::new(StatusCode::BAD_REQUEST, APIError::invalid_display_name())
    }

    pub fn rate_limit_exceeded() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            APIError::rate_limit_exceeded(),
        )
    }

    pub fn internal(detail: &str) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            APIError::internal_error(detail),
        )
    }

    pub fn guests_not_allowed() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::guests_not_allowed())
    }

    /// `403` — the meeting is password-protected and none was supplied.
    ///
    /// `403 Forbidden`, not `401 Unauthorized`: the caller's *identity* is
    /// established (session cookie for `/join`, none needed for `/join-guest`);
    /// what they lack is a resource-specific credential.
    ///
    /// A `401` would also be wrong operationally. `dioxus-ui`'s
    /// `with_refresh_retry` maps `401` to `ApiError::NotAuthenticated` and, when
    /// `is_pkce_flow()` is set, responds by refreshing the session and replaying
    /// the request **once** before surfacing the error. So a `401` here would
    /// cost a spurious token refresh plus a duplicate join attempt — a second
    /// Argon2 verification against the same failing password — on every wrong
    /// entry in PKCE mode. Not an unbounded loop; still work nobody asked for,
    /// on the exact path this change makes expensive.
    pub fn meeting_password_required() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::meeting_password_required())
    }

    /// `403` — the supplied meeting password did not verify (or the stored hash
    /// was unparseable, which is denied rather than ignored). See
    /// [`AppError::meeting_password_required`] for why this is `403`.
    pub fn invalid_meeting_password() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::invalid_meeting_password())
    }

    /// `429` — this `(client IP, meeting)` pair has burned its failed-password
    /// budget for the current window.
    pub fn too_many_password_attempts() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            APIError::too_many_password_attempts(),
        )
    }

    /// `503` — no Argon2 verification permit became available in time, so the
    /// request was shed instead of queueing without bound. Transient; the client
    /// may retry.
    pub fn verifier_overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            APIError::verifier_overloaded(),
        )
    }

    /// `503` — no Argon2 permit became available in time for a password *hash*.
    pub fn password_hasher_overloaded() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            APIError::password_hasher_overloaded(),
        )
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = APIResponse::error(self.body);
        (self.status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!("Database error: {err}");
        Self::internal(&err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;

    /// Consume the response body and deserialize it to `APIResponse<APIError>`.
    async fn read_error_body(resp: Response) -> (StatusCode, APIResponse<APIError>) {
        let status = resp.status();
        let bytes = Body::new(resp.into_body())
            .collect()
            .await
            .expect("collect body")
            .to_bytes();
        let parsed: APIResponse<APIError> =
            serde_json::from_slice(&bytes).expect("deserialize error body");
        (status, parsed)
    }

    #[tokio::test]
    async fn unauthorized_produces_401_with_correct_code() {
        let err = AppError::unauthorized();
        let resp = err.into_response();
        let (status, body) = read_error_body(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.success);
        assert_eq!(body.result.code, "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn meeting_not_found_produces_404() {
        let err = AppError::meeting_not_found("abc123");
        let resp = err.into_response();
        let (status, body) = read_error_body(resp).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!body.success);
        assert_eq!(body.result.code, "MEETING_NOT_FOUND");
    }

    #[tokio::test]
    async fn meeting_exists_produces_409() {
        let err = AppError::meeting_exists("dup");
        let resp = err.into_response();
        let (status, body) = read_error_body(resp).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body.result.code, "MEETING_EXISTS");
    }

    #[tokio::test]
    async fn not_owner_produces_403() {
        let err = AppError::not_owner();
        let resp = err.into_response();
        let (status, body) = read_error_body(resp).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body.result.code, "NOT_OWNER");
    }

    #[tokio::test]
    async fn internal_carries_engineering_error() {
        let err = AppError::internal("db exploded");
        let resp = err.into_response();
        let (status, body) = read_error_body(resp).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.result.code, "INTERNAL_ERROR");
        assert_eq!(
            body.result.engineering_error.as_deref(),
            Some("db exploded")
        );
    }
}
