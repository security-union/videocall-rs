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

//! Error types for the meeting API client.

use thiserror::Error;
use videocall_meeting_types::{APIError, APIResponse};

/// Errors returned by [`MeetingApiClient`](crate::MeetingApiClient) methods.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The session JWT is missing, expired, or invalid (HTTP 401).
    #[error("Not authenticated. Please log in.")]
    NotAuthenticated,

    /// The server denied access (HTTP 403).
    #[error("Access denied: {0}")]
    Forbidden(String),

    /// The requested resource was not found (HTTP 404).
    #[error("Not found: {0}")]
    NotFound(String),

    /// The meeting is not active (HTTP 400, code MEETING_NOT_ACTIVE).
    #[error("Meeting is not active. The host must join first.")]
    MeetingNotActive,

    /// A server error with status code and body.
    #[error("Server error ({status}): {body}")]
    ServerError { status: u16, body: String },

    /// An error from the device endpoints in their `{"error","error_description"}` shape.
    #[error("device API error ({status}): {error}")]
    Device {
        status: u16,
        error: String,
        error_description: String,
    },

    /// A network or transport error.
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// A configuration error (e.g. missing base URL).
    #[error("Configuration error: {0}")]
    Config(String),

    #[cfg(feature = "device-session")]
    #[error(transparent)]
    Key(#[from] videocall_meeting_types::device::crypto::KeyError),
}

impl ApiError {
    /// The meeting-api `APIError` in the response body, if it has the usual JSON shape.
    pub fn api_error(&self) -> Option<APIError> {
        let body = match self {
            Self::Forbidden(body) | Self::NotFound(body) | Self::ServerError { body, .. } => body,
            _ => return None,
        };
        let parsed: APIResponse<APIError> = serde_json::from_str(body).ok()?;
        Some(parsed.result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_error_reads_the_code() {
        let body = r#"{"success":false,"result":{"code":"MEETING_EXISTS","message":"m"}}"#;
        let taken = ApiError::ServerError {
            status: 409,
            body: body.into(),
        };
        assert_eq!(taken.api_error().unwrap().code, "MEETING_EXISTS");
        let html = ApiError::ServerError {
            status: 502,
            body: "<html>".into(),
        };
        assert!(html.api_error().is_none());
    }
}
