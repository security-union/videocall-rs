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

//! Device-JWT and registration-JWS verification on the server: the token
//! length limit, the `devices` row lookup and its state. The token format
//! and the signature checks are shared with the CLI in
//! [`videocall_meeting_types::device::crypto`].

use chrono::{DateTime, Duration, Utc};
use videocall_meeting_types::device::crypto;

use crate::db::devices::{self as db_devices, DeviceRow};
use crate::db::DbPool;

/// Which device states a caller accepts. A revoked device is never accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedStates {
    LiveApproved,
    /// Lets an expired device revoke itself.
    AnyNotRevoked,
}

#[derive(Debug)]
pub enum DeviceAuthError {
    /// The reason is for logs only and never goes on the wire.
    Invalid(&'static str),
    Db(sqlx::Error),
}

impl std::fmt::Display for DeviceAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceAuthError::Invalid(reason) => write!(f, "invalid device token: {reason}"),
            DeviceAuthError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

/// Verify a device JWT and return its row, checked against `allowed`.
pub async fn verify_device_jwt(
    db: &DbPool,
    token: &str,
    expected_aud: &str,
    now: DateTime<Utc>,
    allowed: AllowedStates,
) -> Result<DeviceRow, DeviceAuthError> {
    let kid = crypto::device_token_kid(token).map_err(DeviceAuthError::Invalid)?;
    let row = db_devices::find_device(db, &kid)
        .await
        .map_err(DeviceAuthError::Db)?
        .ok_or(DeviceAuthError::Invalid("unknown device"))?;
    crypto::verify_device_token(token, &kid, &row.public_key, expected_aud, now.timestamp())
        .map_err(DeviceAuthError::Invalid)?;
    if row.revoked_at.is_some() {
        return Err(DeviceAuthError::Invalid("revoked"));
    }
    if allowed == AllowedStates::LiveApproved && !row.is_live(now) {
        return Err(DeviceAuthError::Invalid("device expired"));
    }
    Ok(row)
}

/// Slide the idle expiry by the row's original idle span
/// (`idle_expires_at - last_used_at`), at most once an hour. Errors are
/// logged, not returned.
pub async fn touch_live_device(db: &DbPool, row: &DeviceRow, now: DateTime<Utc>) {
    if now - row.last_used_at < Duration::hours(1) {
        return;
    }
    let span = row.idle_expires_at - row.last_used_at;
    if span <= Duration::zero() {
        return;
    }
    if let Err(e) = db_devices::touch_device(db, &row.id, now, now + span).await {
        tracing::warn!(device_id = %row.id, "device touch failed: {e}");
    }
}

/// PKCE verifier: 43..=128 unreserved characters (RFC 7636 4.1).
pub fn is_valid_code_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_bounds() {
        assert!(is_valid_code_verifier(&format!("{}.~", "a".repeat(43))));
        assert!(!is_valid_code_verifier(&"a".repeat(42)));
        assert!(!is_valid_code_verifier(&format!("{}+", "a".repeat(43))));
    }
}
