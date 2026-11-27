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

//! Feature flags for videocall-rs.
//!
//! Flags are loaded lazily from environment variables on first access.
//! Add new flags here as the project evolves.

use crate::constants::{DATABASE_ENABLED, FEATURE_MEETING_MANAGEMENT};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Override states for testing
const OVERRIDE_NONE: u8 = 0;
const OVERRIDE_TRUE: u8 = 1;
const OVERRIDE_FALSE: u8 = 2;

/// Test override for meeting_management flag
static MEETING_MANAGEMENT_OVERRIDE: AtomicU8 = AtomicU8::new(OVERRIDE_NONE);

/// Feature flags singleton, lazily initialized from environment variables.
#[derive(Debug, Clone)]
pub struct FeatureFlags {
    /// Enable meeting lifecycle management (creation, tracking, host controls).
    /// Env: FEATURE_MEETING_MANAGEMENT=true
    pub meeting_management: bool,

    /// Enable database connections (PostgreSQL).
    /// Env: DATABASE_ENABLED=true
    pub database: bool,
}

impl FeatureFlags {
    /// Load feature flags from environment variables.
    fn from_env() -> Self {
        Self {
            meeting_management: *FEATURE_MEETING_MANAGEMENT,
            database: *DATABASE_ENABLED,
        }
    }

    /// Get the global feature flags instance.
    /// Lazily initialized on first call.
    pub fn global() -> &'static Self {
        static FLAGS: OnceLock<FeatureFlags> = OnceLock::new();
        FLAGS.get_or_init(FeatureFlags::from_env)
    }

    /// Check if meeting management is enabled.
    /// Respects test overrides if set.
    #[inline]
    pub fn meeting_management_enabled() -> bool {
        match MEETING_MANAGEMENT_OVERRIDE.load(Ordering::SeqCst) {
            OVERRIDE_TRUE => true,
            OVERRIDE_FALSE => false,
            _ => Self::global().meeting_management,
        }
    }

    /// Check if database is enabled.
    #[inline]
    pub fn database_enabled() -> bool {
        Self::global().database
    }

    /// Override meeting_management flag for testing.
    /// Call `clear_meeting_management_override()` to restore normal behavior.
    ///
    /// Only available with the `testing` feature enabled.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_meeting_management_override(enabled: bool) {
        let value = if enabled {
            OVERRIDE_TRUE
        } else {
            OVERRIDE_FALSE
        };
        MEETING_MANAGEMENT_OVERRIDE.store(value, Ordering::SeqCst);
    }

    /// Clear the meeting_management override, restoring env-based behavior.
    ///
    /// Only available with the `testing` feature enabled.
    #[cfg(any(test, feature = "testing"))]
    pub fn clear_meeting_management_override() {
        MEETING_MANAGEMENT_OVERRIDE.store(OVERRIDE_NONE, Ordering::SeqCst);
    }
}

/// Parse a flag's raw environment value.
/// Returns false if not set or not a truthy value.
pub(crate) fn parse_bool_flag(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_flag_not_set() {
        assert!(!parse_bool_flag(None));
    }

    #[test]
    fn test_parse_bool_flag_truthy_values() {
        assert!(parse_bool_flag(Some("true")));
        assert!(parse_bool_flag(Some("1")));
        assert!(parse_bool_flag(Some("yes")));
        assert!(parse_bool_flag(Some("TRUE")));
    }

    #[test]
    fn test_parse_bool_flag_falsy_values() {
        assert!(!parse_bool_flag(Some("false")));
        assert!(!parse_bool_flag(Some("0")));
        assert!(!parse_bool_flag(Some("random")));
    }
}
