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

    pub fn not_owner() -> Self {
        Self::new(StatusCode::FORBIDDEN, APIError::not_owner())
    }

    pub fn participant_not_found(email: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            APIError::participant_not_found(email),
        )
    }

    pub fn not_in_meeting() -> Self {
        Self::new(StatusCode::NOT_FOUND, APIError::not_in_meeting())
    }

    pub fn internal(detail: &str) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            APIError::internal_error(detail),
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
