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

    pub fn invalid_input(detail: &str) -> Self {
        Self {
            code: "INVALID_INPUT".to_string(),
            message: detail.to_string(),
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

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self {
            code: "BAD_REQUEST".to_string(),
            message: detail.into(),
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

    pub fn joining_not_allowed() -> Self {
        Self {
            code: "JOINING_NOT_ALLOWED".to_string(),
            message: "The host has left and no one can admit new participants. New participants cannot join this meeting.".to_string(),
            engineering_error: None,
        }
    }

    pub fn invalid_display_name() -> Self {
        Self {
            code: "INVALID_DISPLAY_NAME".to_string(),
            // Use a static message to avoid echoing user-supplied characters back in
            // the HTTP response body (SG4: don't reflect invalid chars to clients).
            message: "Display name is invalid. Allowed: letters, numbers, spaces, hyphens, \
                      underscores, and apostrophes (max 50 characters)."
                .to_string(),
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

    pub fn rate_limit_exceeded() -> Self {
        Self {
            code: "RATE_LIMIT_EXCEEDED".to_string(),
            message: "Too many rename requests. Please wait before trying again.".to_string(),
            engineering_error: None,
        }
    }

    pub fn guests_not_allowed() -> Self {
        Self {
            code: "GUESTS_NOT_ALLOWED".to_string(),
            message: "Unable to join this meeting as a guest.".to_string(),
            engineering_error: None,
        }
    }

    /// The meeting is password-protected and the join request carried no
    /// password (issue #1613). Distinct from [`Self::invalid_meeting_password`]
    /// so a client can tell "prompt the user" from "the user typed it wrong".
    ///
    /// Disclosure note: both codes reveal only that the meeting has a password,
    /// which `has_password` already publishes on every meeting listing
    /// (`GET /api/v1/meetings`, `/feed`, `/joined`, `/{meeting_id}`) — so
    /// splitting them leaks nothing that an enumerating caller cannot read
    /// directly, and it avoids forcing every client into a blind retry loop.
    pub fn meeting_password_required() -> Self {
        Self {
            code: "MEETING_PASSWORD_REQUIRED".to_string(),
            message: "This meeting requires a password.".to_string(),
            engineering_error: None,
        }
    }

    /// The supplied meeting password did not verify against the stored hash —
    /// or the stored hash could not be parsed at all, in which case the join is
    /// denied rather than allowed through (fail closed, issue #1613).
    ///
    /// The message is deliberately identical for both causes: a caller must not
    /// be able to distinguish "wrong password" from "this meeting's stored hash
    /// is corrupt", since the latter would advertise a record an attacker could
    /// otherwise probe for. Operators get the real cause from the server log.
    pub fn invalid_meeting_password() -> Self {
        Self {
            code: "INVALID_MEETING_PASSWORD".to_string(),
            message: "Incorrect meeting password.".to_string(),
            engineering_error: None,
        }
    }

    /// Too many failed password attempts from this client for this meeting
    /// (issue #1613). Scoped to a `(client IP, meeting)` pair, so it never locks
    /// a meeting for anyone but the client that burned the budget.
    pub fn too_many_password_attempts() -> Self {
        Self {
            code: "TOO_MANY_PASSWORD_ATTEMPTS".to_string(),
            message: "Too many incorrect password attempts. Please wait a minute and try again."
                .to_string(),
            engineering_error: None,
        }
    }

    /// The server is at its bounded capacity for concurrent password
    /// verifications and shed this request rather than queueing it without
    /// limit (issue #1613). Transient and safe to retry.
    pub fn verifier_overloaded() -> Self {
        Self {
            code: "VERIFIER_OVERLOADED".to_string(),
            message: "The server is busy verifying meeting passwords. Please try again."
                .to_string(),
            engineering_error: None,
        }
    }
}

impl std::fmt::Display for APIError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for APIError {}
