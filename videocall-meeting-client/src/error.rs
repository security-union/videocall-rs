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

    /// A network or transport error.
    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    /// A configuration error (e.g. missing base URL).
    #[error("Configuration error: {0}")]
    Config(String),
}
