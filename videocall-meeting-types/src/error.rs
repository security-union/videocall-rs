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

//! API error types.
//!
//! Every failed API response is returned as `APIResponse<APIError>` with `success: false`.

use serde::{Deserialize, Serialize};

/// Structured error returned in the `result` field of a failed [`super::APIResponse`].
///
/// The `code` field is a machine-readable identifier (e.g. `"MEETING_NOT_FOUND"`).
/// The `message` field is a human-readable description suitable for display.
/// The `engineering_error` field carries debug-level detail (stack traces, DB errors)
/// that is useful during development but should be stripped or redacted in production.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct APIError {
    /// Machine-readable error code (e.g. `"UNAUTHORIZED"`, `"MEETING_NOT_FOUND"`).
    pub code: String,

    /// Human-readable error message.
    pub message: String,

    /// Optional engineering-level detail for debugging.
    /// Should be omitted or redacted in production responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engineering_error: Option<String>,
}

impl APIError {
    pub fn unauthorized() -> Self {
        Self {
            code: "UNAUTHORIZED".to_string(),
            message: "Authentication required.".to_string(),
            engineering_error: None,
        }
    }

    pub fn unauthorized_with_detail(detail: &str) -> Self {
        Self {
            code: "UNAUTHORIZED".to_string(),
            message: "Authentication required.".to_string(),
            engineering_error: Some(detail.to_string()),
        }
    }

    pub fn invalid_meeting_id(detail: &str) -> Self {
        Self {
            code: "INVALID_MEETING_ID".to_string(),
            message: format!("Invalid meeting ID: {detail}"),
            engineering_error: None,
        }
    }

    pub fn too_many_attendees(count: usize, max: usize) -> Self {
        Self {
            code: "TOO_MANY_ATTENDEES".to_string(),
            message: format!("Too many attendees: {count} provided, maximum is {max}"),
            engineering_error: None,
        }
    }

    pub fn meeting_exists(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_EXISTS".to_string(),
            message: format!("Meeting with ID '{meeting_id}' already exists"),
            engineering_error: None,
        }
    }

    pub fn meeting_not_found(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_NOT_FOUND".to_string(),
            message: format!("Meeting '{meeting_id}' not found"),
            engineering_error: None,
        }
    }

    pub fn meeting_not_active(meeting_id: &str) -> Self {
        Self {
            code: "MEETING_NOT_ACTIVE".to_string(),
            message: format!("Meeting '{meeting_id}' is not active. Host must join first."),
            engineering_error: None,
        }
    }

    pub fn not_host() -> Self {
        Self {
            code: "NOT_HOST".to_string(),
            message: "Only the meeting host can perform this action".to_string(),
            engineering_error: None,
        }
    }

    pub fn not_owner() -> Self {
        Self {
            code: "NOT_OWNER".to_string(),
            message: "Only the meeting owner can perform this action".to_string(),
            engineering_error: None,
        }
    }

    pub fn participant_not_found(email: &str) -> Self {
        Self {
            code: "PARTICIPANT_NOT_FOUND".to_string(),
            message: format!("Participant '{email}' not found in waiting room"),
            engineering_error: None,
        }
    }

    pub fn not_in_meeting() -> Self {
        Self {
            code: "NOT_IN_MEETING".to_string(),
            message: "You have not joined this meeting".to_string(),
            engineering_error: None,
        }
    }

    pub fn internal_error(detail: &str) -> Self {
        Self {
            code: "INTERNAL_ERROR".to_string(),
            message: "Internal server error".to_string(),
            engineering_error: Some(detail.to_string()),
        }
    }
}

impl std::fmt::Display for APIError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for APIError {}
