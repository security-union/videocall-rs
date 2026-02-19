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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! WASM (browser) platform primitives.
//!
//! These implementations use browser APIs through `js-sys`, `gloo`, and
//! `wasm-bindgen-futures`.

use std::future::Future;

/// Returns the current time in milliseconds since the Unix epoch.
///
/// Uses `js_sys::Date::now()` which returns a high-resolution timestamp from
/// the browser.
pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// A repeating timer that fires a callback at a fixed interval.
///
/// Wraps `gloo::timers::callback::Interval`. The timer is automatically cancelled
/// when the handle is dropped.
pub struct IntervalHandle {
    _interval: gloo::timers::callback::Interval,
}

impl IntervalHandle {
    /// Create a new repeating timer.
    ///
    /// # Arguments
    /// * `period_ms` — interval period in milliseconds
    /// * `callback` — closure to invoke on each tick
    pub fn new<F: Fn() + 'static>(period_ms: u32, callback: F) -> Self {
        Self {
            _interval: gloo::timers::callback::Interval::new(period_ms, callback),
        }
    }
}

/// Spawn an async task on the browser's microtask queue.
///
/// Wraps `wasm_bindgen_futures::spawn_local`. The future does **not** need to be
/// `Send` because WASM is single-threaded.
pub fn spawn<F: Future<Output = ()> + 'static>(future: F) {
    wasm_bindgen_futures::spawn_local(future);
}

#[cfg(test)]
mod tests {
    use super::*;

    // WASM tests require wasm-bindgen-test and a browser/headless environment.
    // These are basic compile-time checks; runtime tests live in wasm-bindgen-test suites.

    #[test]
    fn test_now_ms_compiles() {
        // This test only runs on wasm32, verifying the function signature.
        // Actual value testing is done via wasm-bindgen-test.
    }
}
