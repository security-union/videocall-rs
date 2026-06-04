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

//! Clock abstraction for the adaptive-quality subsystem.
//!
//! The browser path uses `js_sys::Date::now()`, while native consumers
//! (e.g. the load-test bot) need `std::time::SystemTime`. This trait lets the
//! same AQ code run on both targets, and also lets tests inject a deterministic
//! clock so tier-transition tests are not flaky.
//!
//! Callers typically hold `Arc<dyn Clock>` because the underlying controller
//! and manager types are shared across tasks.

use std::sync::Arc;

/// Source of monotonic-ish wall-clock time for adaptive-quality logic.
///
/// `now_ms()` returns milliseconds since some consistent epoch. The absolute
/// value is not meaningful — the AQ code only ever compares two samples from
/// the same clock, so it is safe for different `Clock` implementations to use
/// different epochs.
pub trait Clock: Send + Sync + 'static {
    /// Return the current time, in milliseconds.
    fn now_ms(&self) -> f64;
}

// ---------------------------------------------------------------------------
// Native clock — `std::time::SystemTime`.
// ---------------------------------------------------------------------------

/// Native `Clock` backed by `std::time::SystemTime::UNIX_EPOCH`.
///
/// Not available under `wasm32` targets because `SystemTime::now()` panics
/// in browser Wasm; use [`JsDateClock`] instead.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct SystemClock;

#[cfg(not(target_arch = "wasm32"))]
impl Clock for SystemClock {
    fn now_ms(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time is before UNIX_EPOCH")
            .as_secs_f64()
            * 1000.0
    }
}

// ---------------------------------------------------------------------------
// Browser clock — `js_sys::Date::now`.
// ---------------------------------------------------------------------------

/// Browser `Clock` backed by `js_sys::Date::now`.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Default)]
pub struct JsDateClock;

#[cfg(target_arch = "wasm32")]
impl Clock for JsDateClock {
    fn now_ms(&self) -> f64 {
        js_sys::Date::now()
    }
}

/// Return the default `Clock` implementation for the current target.
///
/// - On `wasm32`, returns a [`JsDateClock`].
/// - On native, returns a [`SystemClock`].
pub fn default_clock() -> Arc<dyn Clock> {
    #[cfg(target_arch = "wasm32")]
    {
        Arc::new(JsDateClock)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Arc::new(SystemClock)
    }
}

// ---------------------------------------------------------------------------
// Test clock — deterministic, advance by hand.
// ---------------------------------------------------------------------------

/// Deterministic `Clock` used by unit tests.
///
/// `TestClock` stores its current time in an `AtomicU64` so the same instance
/// can be shared (via `Arc<dyn Clock>`) across the controller and the test
/// while still being advanced from the test side.
#[derive(Debug)]
pub struct TestClock {
    ms: std::sync::atomic::AtomicU64,
}

impl TestClock {
    /// Construct a new `TestClock` starting at `start_ms` milliseconds.
    pub fn new(start_ms: u64) -> Self {
        Self {
            ms: std::sync::atomic::AtomicU64::new(start_ms),
        }
    }

    /// Advance the clock by `delta` milliseconds.
    pub fn advance_ms(&self, delta: u64) {
        self.ms
            .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
    }

    /// Set the clock to a specific time (milliseconds).
    pub fn set_ms(&self, value: u64) {
        self.ms.store(value, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> f64 {
        self.ms.load(std::sync::atomic::Ordering::SeqCst) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    #[test]
    fn test_default_clock_returns_positive_time() {
        let clock = default_clock();
        assert!(clock.now_ms() > 0.0);
    }

    #[test]
    fn test_test_clock_starts_at_given_time() {
        let clock = TestClock::new(1000);
        assert_eq!(clock.now_ms(), 1000.0);
    }

    #[test]
    fn test_test_clock_advances() {
        let clock = TestClock::new(1000);
        clock.advance_ms(500);
        assert_eq!(clock.now_ms(), 1500.0);
    }

    #[test]
    fn test_test_clock_set() {
        let clock = TestClock::new(1000);
        clock.set_ms(9999);
        assert_eq!(clock.now_ms(), 9999.0);
    }
}
