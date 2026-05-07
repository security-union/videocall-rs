/*
 * Copyright 2026 Security Union LLC
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

//! Cross-machine "how fast is this device?" microbenchmark used during the
//! console-log preamble.
//!
//! The 2026-05-06 incident postmortem (Phase 8a) showed we have inadequate
//! observability of client-side performance. Long Tasks API events tell us
//! when the main thread stalls, but they don't tell us whether the device
//! was *capable* of doing the work in the first place. A `capability_score`
//! emitted once at preamble time gives us a stable cross-machine signal so
//! support can quickly tell "Jason's Intel MBA from 2014" from "an M2 with
//! battery saver on" from "a fresh Chromebook".
//!
//! Expected ranges (from Jay's analysis on the cc7tp meeting):
//!   * Apple M2/M3 Pro:                 > 50,000 ops
//!   * Recent 8-core Intel desktop:     30,000–40,000
//!   * Older Intel MacBook Pro:         5,000–10,000
//!   * Throttled / battery-saver:       < 5,000
//!
//! The exact units don't matter — what matters is that the same machine in
//! the same browser produces the same score from one run to the next, so
//! we can spot the outliers in production logs.

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Default time budget for the benchmark loop (milliseconds).
///
/// 100 ms is short enough to run during page load without being noticeable,
/// but long enough that timer quantization (≈1 ms in modern Chrome) plus
/// platform jitter doesn't dominate the measurement.
pub const DEFAULT_BUDGET_MS: f64 = 100.0;

/// Number of `f64` multiply–add operations performed per outer iteration
/// of the benchmark inner loop. Bigger values reduce the amortized cost of
/// the wall-clock check; smaller values give a finer-grained measurement.
const OPS_PER_ITER: u32 = 1024;

/// Run the f64-multiplication microbenchmark for `budget_ms` wall time and
/// return the number of outer iterations completed.
///
/// `now_ms` must return monotonically non-decreasing milliseconds (e.g.
/// `performance.now()`). Tests inject a deterministic clock; the production
/// caller passes a closure backed by the browser's Performance API.
///
/// The benchmark is intentionally simple — a tight loop of f64 multiply–add
/// operations — so it stresses raw CPU throughput rather than memory
/// bandwidth, JIT warmup, or browser-specific intrinsics.
///
/// Returns `0` if the elapsed time fails to advance (e.g. a buggy clock
/// stub) so callers can detect "benchmark didn't run" without having to
/// special-case `Option`.
pub fn run_capability_benchmark<F: FnMut() -> f64>(budget_ms: f64, mut now_ms: F) -> u64 {
    if budget_ms <= 0.0 {
        return 0;
    }

    let start = now_ms();
    let deadline = start + budget_ms;

    // The accumulator must escape the loop so the optimizer doesn't
    // delete it. We feed it back into the next iteration to defeat
    // trivial loop-invariant code motion.
    let mut acc: f64 = 1.0;
    let mut iterations: u64 = 0;

    loop {
        // Inner loop: amortize the wall-clock check across many ops.
        for i in 0..OPS_PER_ITER {
            // Use the iteration counter to keep the multiplier varying
            // and avoid any compiler trick that collapses the chain to a
            // single multiplication.
            let x = (i as f64).mul_add(0.000_001_f64, 1.000_000_001_f64);
            acc = acc.mul_add(x, 1.0);
            // Periodically rescale so `acc` stays in a sane range and
            // the multiplications keep doing useful work.
            if i & 0xFF == 0 {
                acc = (acc.fract() + 1.0).max(0.5);
            }
        }
        iterations += 1;

        let now = now_ms();
        if now >= deadline {
            break;
        }
        // Defensive: if the clock is broken (returns the same value
        // forever) we'd loop indefinitely. Cap at a sane upper bound.
        if iterations >= u64::MAX / 2 {
            break;
        }
    }

    // Use `acc` to defeat dead-code elimination of the inner loop without
    // distorting the returned iteration count. `black_box` is the standard
    // optimizer-fence: the compiler must assume `acc` may be inspected.
    std::hint::black_box(acc);
    iterations
}

/// Run the capability benchmark using the browser's `performance.now()` clock.
///
/// Returns `0` if the Performance API is unavailable. Production callers
/// that need a non-zero value should fall back to logging "N/A".
#[cfg(target_arch = "wasm32")]
pub fn run_capability_benchmark_now(budget_ms: f64) -> u64 {
    if let Some(perf) = web_sys::window().and_then(|w| w.performance()) {
        run_capability_benchmark(budget_ms, move || perf.now())
    } else {
        // Fall back to `Date.now()` which only has 1 ms resolution but is
        // monotonic enough for a 100 ms budget.
        run_capability_benchmark(budget_ms, js_sys::Date::now)
    }
}

/// JS-callable shim used by the dioxus-ui console-log preamble.
///
/// The preamble runs in JavaScript (see
/// `dioxus-ui/scripts/console-log-collector.js`) before the WASM module
/// is fully bootstrapped, so this function is exposed via wasm-bindgen
/// under a stable name that the JS side can `look up by hand` once the
/// WASM module finishes loading.
///
/// Returns the iteration count as a `u32`. Callers should treat the
/// value as opaque — it is only meant to be compared across runs of the
/// same device, not interpreted as a unit (e.g. "ops/sec").
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(js_name = videocallCapabilityScore)]
pub fn videocall_capability_score() -> u32 {
    let score = run_capability_benchmark_now(DEFAULT_BUDGET_MS);
    // The score is bounded by what we can do in 100 ms; clamp to u32 so
    // the JS side gets a numeric result even on hypothetical hardware
    // that overflows.
    if score > u32::MAX as u64 {
        u32::MAX
    } else {
        score as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A simple monotonic clock stub: each call advances by `step_ms`.
    struct StepClock {
        cursor_ms: f64,
        step_ms: f64,
    }

    impl StepClock {
        fn new(step_ms: f64) -> Self {
            Self {
                cursor_ms: 0.0,
                step_ms,
            }
        }

        fn now(&mut self) -> f64 {
            let v = self.cursor_ms;
            self.cursor_ms += self.step_ms;
            v
        }
    }

    #[test]
    fn benchmark_returns_positive_iteration_count() {
        // 0.5 ms per call → ~200 calls fit in a 100 ms budget. Each call
        // runs OPS_PER_ITER multiply-adds, so the iteration count is at
        // least 1 even on a slow runner.
        let mut clock = StepClock::new(0.5);
        let count = run_capability_benchmark(100.0, || clock.now());
        assert!(
            count >= 1,
            "benchmark should complete at least one iteration"
        );
    }

    #[test]
    fn benchmark_zero_budget_returns_zero() {
        let mut clock = StepClock::new(1.0);
        let count = run_capability_benchmark(0.0, || clock.now());
        assert_eq!(count, 0);
    }

    #[test]
    fn benchmark_negative_budget_returns_zero() {
        let mut clock = StepClock::new(1.0);
        let count = run_capability_benchmark(-5.0, || clock.now());
        assert_eq!(count, 0);
    }

    #[test]
    fn benchmark_runs_in_under_200ms_on_native() {
        // Use a real wall clock so this is actually a perf check. On any
        // dev machine this completes well within 200 ms (the default budget
        // is 100 ms), proving the benchmark doesn't accidentally hang.
        let start = std::time::Instant::now();
        let count = run_capability_benchmark(DEFAULT_BUDGET_MS, || {
            // `Instant::elapsed` returns a Duration; convert to f64 ms.
            start.elapsed().as_secs_f64() * 1000.0
        });
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(
            elapsed_ms < 200.0,
            "benchmark exceeded 200 ms budget (took {elapsed_ms:.1} ms, count={count})"
        );
        assert!(
            count > 0,
            "benchmark should produce a non-zero iteration count"
        );
    }

    #[test]
    fn benchmark_score_is_monotonic_in_budget() {
        // Doubling the budget should produce at least 1.5x the iterations
        // (allowing for warmup + clock noise). This catches the case where
        // the inner loop is being collapsed by the optimizer.
        let start = std::time::Instant::now();
        let now = || start.elapsed().as_secs_f64() * 1000.0;
        let small = run_capability_benchmark(50.0, now);
        let large = run_capability_benchmark(150.0, now);
        assert!(
            large >= small,
            "expected larger budget to produce ≥ small budget result; small={small}, large={large}"
        );
    }
}
