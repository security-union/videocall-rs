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

use std::sync::OnceLock;

/// Feature flags singleton, lazily initialized from environment variables.
#[derive(Debug, Clone)]
pub struct FeatureFlags {
    /// Enable database connections (PostgreSQL).
    /// Env: DATABASE_ENABLED=true
    pub database: bool,
}

impl FeatureFlags {
    /// Load feature flags from environment variables.
    fn from_env() -> Self {
        Self {
            database: read_bool_no_prefix("DATABASE_ENABLED"),
        }
    }

    /// Get the global feature flags instance.
    /// Lazily initialized on first call.
    pub fn global() -> &'static Self {
        static FLAGS: OnceLock<FeatureFlags> = OnceLock::new();
        FLAGS.get_or_init(FeatureFlags::from_env)
    }

    /// Check if database is enabled.
    #[inline]
    pub fn database_enabled() -> bool {
        Self::global().database
    }
}

/// Read a boolean environment variable without prefix (for legacy env vars like DATABASE_ENABLED).
/// Returns false if not set or not a truthy value.
fn read_bool_no_prefix(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bool_no_prefix_not_set() {
        std::env::remove_var("VC_FF_TEST_UNSET");
        assert!(!read_bool_no_prefix("VC_FF_TEST_UNSET"));
    }

    #[test]
    fn test_read_bool_no_prefix_truthy_values() {
        for (name, value) in [
            ("VC_FF_TEST_TRUE", "true"),
            ("VC_FF_TEST_ONE", "1"),
            ("VC_FF_TEST_YES", "yes"),
            ("VC_FF_TEST_TRUE_UPPER", "TRUE"),
        ] {
            std::env::set_var(name, value);
            assert!(read_bool_no_prefix(name), "{name}={value}");
            std::env::remove_var(name);
        }
    }

    #[test]
    fn test_read_bool_no_prefix_falsy_values() {
        for (name, value) in [
            ("VC_FF_TEST_FALSE", "false"),
            ("VC_FF_TEST_ZERO", "0"),
            ("VC_FF_TEST_RANDOM", "random"),
        ] {
            std::env::set_var(name, value);
            assert!(!read_bool_no_prefix(name), "{name}={value}");
            std::env::remove_var(name);
        }
    }
}
