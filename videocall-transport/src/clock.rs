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

//! Global-agnostic monotonic clock.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

/// Resolve `performance` from whichever global this code is running in.
#[cfg(target_arch = "wasm32")]
pub fn resolve_performance() -> Option<web_sys::Performance> {
    if let Some(perf) = web_sys::window().and_then(|w| w.performance()) {
        return Some(perf);
    }
    js_sys::global()
        .dyn_into::<web_sys::WorkerGlobalScope>()
        .ok()
        .and_then(|scope| scope.performance())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn resolve_performance() -> Option<web_sys::Performance> {
    None
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static PERF: Option<web_sys::Performance> = resolve_performance();
}

/// Monotonic ms in this global's `performance.now()` domain.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> f64 {
    PERF.with(|perf| {
        perf.as_ref()
            .map(|p| p.now())
            .unwrap_or_else(js_sys::Date::now)
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// 0.0 where there is no `performance`, making [`convert_to_domain`] identity.
#[cfg(target_arch = "wasm32")]
pub fn time_origin_ms() -> f64 {
    PERF.with(|perf| perf.as_ref().map(|p| p.time_origin()).unwrap_or(0.0))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn time_origin_ms() -> f64 {
    0.0
}

thread_local! {
    static OWN_ORIGIN_MS: f64 = time_origin_ms();
}

/// This global's own time origin, read once.
pub fn own_time_origin_ms() -> f64 {
    OWN_ORIGIN_MS.with(|origin| *origin)
}

/// Express a stamp taken in one global in another global's domain.
pub fn convert_to_domain(stamp_ms: f64, from_origin_ms: f64, to_origin_ms: f64) -> f64 {
    stamp_ms + (from_origin_ms - to_origin_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_stamp_lands_where_the_main_thread_would_have_stamped_it() {
        let main_origin = 1_000_000.0;
        let worker_origin = 1_002_500.0;
        assert_eq!(
            convert_to_domain(40.0, worker_origin, main_origin),
            2540.0,
            "a worker stamp must move FORWARD by the origin delta, not backward"
        );
    }

    #[test]
    fn a_worker_that_booted_first_converts_backwards() {
        assert_eq!(convert_to_domain(3000.0, 500.0, 900.0), 2600.0);
    }

    #[test]
    fn equal_origins_are_the_identity() {
        assert_eq!(convert_to_domain(17.5, 42.0, 42.0), 17.5);
    }
}
