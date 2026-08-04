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

    /// The host has left and joining is no longer allowed (HTTP 403, code JOINING_NOT_ALLOWED).
    #[error("The host has left and no one can admit new participants. New participants cannot join this meeting.")]
    JoiningNotAllowed,
    /// The meeting does not permit guest (unauthenticated) participants
    /// (HTTP 403, code GUESTS_NOT_ALLOWED).
    #[error("Guests are not allowed in this meeting. The meeting host must enable guest access.")]
    GuestsNotAllowed,

    /// The meeting is password-protected and the join request carried no
    /// password (HTTP 403, code MEETING_PASSWORD_REQUIRED). Issue #1613.
    ///
    /// The caller should prompt for a password and retry the same join call
    /// with it. Retrying without one will always fail the same way.
    #[error("This meeting requires a password.")]
    MeetingPasswordRequired,

    /// The per-user display-name rename budget is spent (HTTP 429, code
    /// RATE_LIMIT_EXCEEDED).
    ///
    /// Not a password error, but it lands on the password path: `join_meeting`
    /// runs the rename limiter before the password gate and only when the
    /// request carries a `display_name`, so a client that sends one sees this
    /// instead of [`Self::TooManyPasswordAttempts`] once its budget is spent.
    /// Both reject before any Argon2 work. A caller that supplied a password
    /// should treat this as a back-off, not as a verdict on what was typed.
    #[error("Too many requests. Please wait a moment and try again.")]
    RateLimitExceeded,

    /// Too many failed meeting-password attempts from this client for this
    /// meeting (HTTP 429, code TOO_MANY_PASSWORD_ATTEMPTS). Issue #1613.
    ///
    /// Scoped to a `(client IP, meeting)` pair server-side, so it never means
    /// "somebody else locked this meeting". The caller should keep the password
    /// prompt on screen with a back-off message rather than falling through to
    /// a generic error card — the user's next action is still "type the
    /// password", just not yet.
    ///
    /// **The supplied password was NOT evaluated.** `consume_attempt` rejects
    /// before `verify_offloaded`, so this says nothing about whether the value
    /// was correct — a UI must not discard what the user typed on this code.
    #[error("Too many incorrect password attempts. Please wait a minute and try again.")]
    TooManyPasswordAttempts,

    /// The server shed the request rather than queue it behind its bounded
    /// password verifier (HTTP 503, code VERIFIER_OVERLOADED). Issue #1613.
    ///
    /// Transient and safe to retry; like [`Self::TooManyPasswordAttempts`] the
    /// user's next action is still to submit a password.
    ///
    /// **The supplied password was NOT evaluated** — the request was shed while
    /// waiting for a verification permit, so a UI must not discard what the user
    /// typed on this code either.
    #[error("The server is busy verifying meeting passwords. Please try again.")]
    VerifierOverloaded,

    /// The supplied meeting password was rejected (HTTP 403, code
    /// INVALID_MEETING_PASSWORD). Issue #1613.
    ///
    /// The caller should re-prompt. Note the server returns this for an
    /// unparseable stored hash too — deliberately indistinguishable from a
    /// wrong password on the wire.
    #[error("Incorrect meeting password.")]
    InvalidMeetingPassword,

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
