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

//! Server-registered in-app recordings (#2856): lease secrets, the request
//! guards, and the fixed rejection reasons.

use std::fmt;
use std::time::Duration;

use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, HeaderValue, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use videocall_meeting_types::APIError;

use crate::error::AppError;
use crate::rate_limit::KeyedRateLimiter;
use crate::state::AppState;

/// Seconds after the last renewal at which a lease has expired.
pub const LEASE_SECS: i64 = 90;
/// Most active non-host leases per meeting.
pub const MEETING_CAP: i64 = 5;
/// Permitted register attempts per meeting per [`CHURN_WINDOW_SECS`], across replicas, past
/// which non-hosts are refused.
pub const CHURN_CAP: f64 = 20.0;
pub const CHURN_WINDOW_SECS: f64 = 60.0;

/// The bearer credential for one lease. Only its SHA-256 is stored.
pub struct LeaseSecret(String);

impl LeaseSecret {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn from_wire(raw: &str) -> Self {
        Self(raw.trim().to_owned())
    }

    pub fn hash(&self) -> Vec<u8> {
        Sha256::digest(self.0.as_bytes()).to_vec()
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LeaseSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LeaseSecret([REDACTED])")
    }
}

impl fmt::Display for LeaseSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Serialize for LeaseSecret {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Why a recording request was refused. The code is the whole response detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    NotPermitted,
    Guest,
    NotAdmitted,
    MeetingEnded,
    UserCap,
    MeetingCap,
    RateLimited,
    BadOrigin,
}

impl Rejection {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotPermitted => "not_permitted",
            Self::Guest => "guest",
            Self::NotAdmitted => "not_admitted",
            Self::MeetingEnded => "meeting_ended",
            Self::UserCap => "user_cap",
            Self::MeetingCap => "meeting_cap",
            Self::RateLimited => "rate_limited",
            Self::BadOrigin => "bad_origin",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::UserCap | Self::MeetingCap => StatusCode::CONFLICT,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            _ => StatusCode::FORBIDDEN,
        }
    }
}

impl From<Rejection> for AppError {
    fn from(reason: Rejection) -> Self {
        tracing::debug!(reason = reason.code(), "recording request rejected");
        AppError::new(
            reason.status(),
            APIError {
                code: reason.code().to_uppercase(),
                message: "Recording request refused.".to_string(),
                engineering_error: None,
            },
        )
    }
}

/// Per-replica recording limits and the allowed request origins.
#[derive(Debug)]
pub struct RecordingGuard {
    allowed_origins: Vec<String>,
    dev_mode: bool,
    pub register_limiter: KeyedRateLimiter,
    pub stop_ip_limiter: KeyedRateLimiter,
    pub stop_global_limiter: KeyedRateLimiter,
}

impl RecordingGuard {
    pub fn new(allowed_origins: Vec<String>, dev_mode: bool) -> Self {
        let minute = Duration::from_secs(60);
        Self {
            allowed_origins,
            dev_mode,
            register_limiter: KeyedRateLimiter::new(6, minute),
            stop_ip_limiter: KeyedRateLimiter::new(30, minute),
            stop_global_limiter: KeyedRateLimiter::new(600, minute),
        }
    }

    /// An empty list allows any origin only in dev mode, as the CORS layer does.
    fn origin_allowed(&self, origin: Option<&HeaderValue>) -> bool {
        if self.allowed_origins.is_empty() {
            return self.dev_mode;
        }
        origin
            .and_then(|o| o.to_str().ok())
            .is_some_and(|o| self.allowed_origins.iter().any(|a| a == o))
    }
}

impl Default for RecordingGuard {
    fn default() -> Self {
        Self::new(Vec::new(), false)
    }
}

/// Requires an allowed `Origin` on any request carrying a session cookie,
/// whichever credential then authenticates it. Must precede `AuthUser`.
pub struct CookieOrigin;

impl FromRequestParts<AppState> for CookieOrigin {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let has_session_cookie = parts
            .headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|h| {
                crate::auth::session_cookie_candidates(h, &state.cookie_name)
                    .next()
                    .is_some()
            });
        if has_session_cookie
            && !state
                .recording
                .origin_allowed(parts.headers.get(header::ORIGIN))
        {
            return Err(Rejection::BadOrigin.into());
        }
        Ok(CookieOrigin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_secret_formatting_is_redacted() {
        let secret = LeaseSecret::generate();
        assert_eq!(secret.expose().len(), 43);
        for rendered in [format!("{secret:?}"), format!("{secret}")] {
            assert!(!rendered.contains(secret.expose()), "{rendered}");
            assert!(rendered.contains("REDACTED"), "{rendered}");
        }
    }

    #[test]
    #[tracing_test::traced_test]
    fn register_rejection_is_logged_at_debug() {
        let err = AppError::from(Rejection::NotPermitted);
        assert_eq!(err.status, StatusCode::FORBIDDEN);
        assert_eq!(err.body.code, "NOT_PERMITTED");
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .filter(|l| l.contains("recording request rejected"))
                .collect::<Vec<_>>()
                .as_slice()
            {
                [line] if line.contains("DEBUG") && line.contains("reason=\"not_permitted\"") => {
                    Ok(())
                }
                other => Err(format!("{other:?}")),
            }
        });
    }
}
