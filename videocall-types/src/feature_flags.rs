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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

/// Environment variable prefix for feature flags
const ENV_PREFIX: &str = "FEATURE_";

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
}

impl FeatureFlags {
    /// Load feature flags from environment variables.
    fn from_env() -> Self {
        Self {
            meeting_management: read_bool_env("MEETING_MANAGEMENT"),
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

/// Read a boolean environment variable with the FEATURE_ prefix.
/// Returns false if not set or not a truthy value.
fn read_bool_env(name: &str) -> bool {
    let full_name = format!("{ENV_PREFIX}{name}");
    std::env::var(&full_name)
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bool_env_not_set() {
        // Ensure the env var is not set
        std::env::remove_var("FEATURE_TEST_FLAG");
        assert!(!read_bool_env("TEST_FLAG"));
    }

    #[test]
    fn test_read_bool_env_truthy_values() {
        std::env::set_var("FEATURE_TEST_TRUE", "true");
        assert!(read_bool_env("TEST_TRUE"));

        std::env::set_var("FEATURE_TEST_ONE", "1");
        assert!(read_bool_env("TEST_ONE"));

        std::env::set_var("FEATURE_TEST_YES", "yes");
        assert!(read_bool_env("TEST_YES"));

        std::env::set_var("FEATURE_TEST_TRUE_UPPER", "TRUE");
        assert!(read_bool_env("TEST_TRUE_UPPER"));

        // Cleanup
        std::env::remove_var("FEATURE_TEST_TRUE");
        std::env::remove_var("FEATURE_TEST_ONE");
        std::env::remove_var("FEATURE_TEST_YES");
        std::env::remove_var("FEATURE_TEST_TRUE_UPPER");
    }

    #[test]
    fn test_read_bool_env_falsy_values() {
        std::env::set_var("FEATURE_TEST_FALSE", "false");
        assert!(!read_bool_env("TEST_FALSE"));

        std::env::set_var("FEATURE_TEST_ZERO", "0");
        assert!(!read_bool_env("TEST_ZERO"));

        std::env::set_var("FEATURE_TEST_RANDOM", "random");
        assert!(!read_bool_env("TEST_RANDOM"));

        // Cleanup
        std::env::remove_var("FEATURE_TEST_FALSE");
        std::env::remove_var("FEATURE_TEST_ZERO");
        std::env::remove_var("FEATURE_TEST_RANDOM");
    }
}
