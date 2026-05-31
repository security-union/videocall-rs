// SPDX-License-Identifier: MIT OR Apache-2.0

//! Adaptive decode-budget decision logic (issue #987, task 1a.1).
//!
//! This module is the pure, DOM-free, JS-free *foundation* for the adaptive
//! decode budget. It contains a single decision function, [`decide_step`],
//! that — given a short window of recent local quality samples plus the
//! current budget state — decides whether the number of simultaneously
//! decoded video tiles (the "cap") should be lowered, raised, or held.
//!
//! ## Why only two signals?
//!
//! The only quality signals that are actually populated client-side today are:
//!
//! 1. `render_fps` — sampled at ~1 Hz, the rate at which the local render loop
//!    is actually painting frames.
//! 2. `longtask_ms_per_sec` — main-thread long-task time per wall-clock second
//!    (a proxy for main-thread saturation / jank).
//!
//! `avg_decode_latency_ms` is intentionally NOT referenced: it is not populated
//! anywhere on the client, so keying on it would be keying on a constant.
//!
//! ## Design notes
//!
//! - **No clock, no DOM.** The caller supplies `now_ms`; this keeps the
//!   function fully deterministic and unit-testable.
//! - **Hysteresis.** The step-down and step-up FPS thresholds form a band
//!   (`FPS_STEP_DOWN` < `FPS_STEP_UP`) so the cap does not oscillate around a
//!   single boundary.
//! - **Sustain.** A single bad sample (e.g. a GC pause) must not move the cap;
//!   pressure/recovery must persist across a window before acting.
//! - **Cooldown.** After a step, no further step *in the same direction* is
//!   allowed until the relevant cooldown has elapsed. The cooldown is
//!   *asymmetric*: stepping down (relieving pressure) uses the shorter
//!   [`STEP_DOWN_COOLDOWN_MS`] so relief lands fast, while stepping up
//!   (re-adding load) uses the longer [`STEP_UP_COOLDOWN_MS`] so the cap does
//!   not eagerly climb back into a machine that is only briefly healthy.
//! - **Severity-proportional step-down.** Under *catastrophic* pressure
//!   (median FPS <= [`FPS_SEVERE`] or sustained long-task time >=
//!   [`LONGTASK_SEVERE_MS_PER_SEC`]) the cap drops by a quarter of its value at
//!   once ([`BudgetStep::Down`] carries the magnitude); mild pressure steps a
//!   single tile to avoid overshoot. Step-up is always a single tile.

/// FPS at or below which the local renderer is considered *under pressure*.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const FPS_STEP_DOWN: f64 = 24.0;

/// FPS at or above which the local renderer is considered *comfortably idle*
/// enough to add another tile.
///
/// Must be strictly above [`FPS_STEP_DOWN`] to provide a hysteresis band.
///
/// `render_fps` is sampled from the requestAnimationFrame loop and is therefore
/// *display-refresh-capped*: a perfectly healthy machine on a 30 Hz panel (or a
/// browser that throttles rAF to 30 fps for a background-ish tab) sits at ~30
/// fps and will never reach a higher target like 50. Keeping the step-up
/// threshold below the common 30 Hz floor (but comfortably above
/// [`FPS_STEP_DOWN`] = 24 to preserve the hysteresis band) ensures a fine
/// machine is allowed to recover its tiles instead of ratcheting the cap down
/// forever.
pub const FPS_STEP_UP: f64 = 30.0;

/// Median render FPS at or below which pressure is considered *catastrophic*,
/// triggering a multi-tile (proportional) step down instead of a single tile.
///
/// Well below [`FPS_STEP_DOWN`] so ordinary "mild" pressure still steps a single
/// tile (avoiding overshoot); only a genuinely collapsed renderer triggers the
/// proportional drop.
pub const FPS_SEVERE: f64 = 12.0;

/// Sustained long-task time per second at or above which main-thread saturation
/// is considered *catastrophic*, triggering a multi-tile (proportional) step
/// down instead of a single tile.
///
/// Well above [`LONGTASK_BUSY_MS_PER_SEC`] so ordinary "mild" jank still steps a
/// single tile.
pub const LONGTASK_SEVERE_MS_PER_SEC: f64 = 700.0;

/// Main-thread long-task time per second at or above which the main thread is
/// considered *busy* (jank), justifying a step down even if FPS looks okay.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const LONGTASK_BUSY_MS_PER_SEC: f64 = 250.0;

/// Main-thread long-task time per second below which the main thread is
/// considered *idle* enough to permit a step up.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const LONGTASK_IDLE_MS_PER_SEC: f64 = 80.0;

/// Number of consecutive samples (at ~1 Hz) of sustained pressure required
/// before stepping down. Three samples ≈ 3 seconds.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const SUSTAIN_SAMPLES: usize = 3;

/// Number of *consecutive recovery-qualifying samples* (tracked via the
/// `direction_hold` counter, incremented once per ~1 Hz sample) required before
/// stepping up.
///
/// Note on counting: the control loop increments `direction_hold` *after*
/// `decide_step` reads it for the current sample, and `decide_step` fires the
/// up-step when `direction_hold >= RECOVERY_HOLD`. So the very first sample for
/// which `decide_step` sees `direction_hold == RECOVERY_HOLD` is the
/// `RECOVERY_HOLD`-th *prior* qualifying sample plus the current one — i.e. an
/// up-step needs `RECOVERY_HOLD + 1` consecutive qualifying samples in
/// practice. This is intentional (one extra sample of confirmation costs ~1 s
/// and biases the controller toward stability). [`recovery_qualifying`] is the
/// single source of truth the loop uses to decide whether to increment.
pub const RECOVERY_HOLD: u32 = 5;

/// Minimum milliseconds between two *down* steps. Shorter than
/// [`STEP_UP_COOLDOWN_MS`] so pressure relief lands fast (the user is actively
/// suffering jank), while still long enough that a single down-step's effect
/// can be measured before the next.
pub const STEP_DOWN_COOLDOWN_MS: f64 = 2000.0;

/// Minimum milliseconds between two *up* steps. Longer than
/// [`STEP_DOWN_COOLDOWN_MS`] so the cap does not eagerly re-add decode load into
/// a machine that is only briefly healthy.
pub const STEP_UP_COOLDOWN_MS: f64 = 4000.0;

/// Minimum allowed cap. The local participant always decodes at least one tile.
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
pub const MIN_CAP: usize = 1;

/// A single ~1 Hz quality sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetSample {
    /// Local render rate for this sampling window, if measured. `None` means
    /// the renderer produced no measurable FPS for this window (treated as
    /// missing, not as zero).
    pub render_fps: Option<f64>,
    /// Main-thread long-task time per wall-clock second for this window.
    pub longtask_ms_per_sec: f64,
}

/// Current adaptive-budget state, owned by the caller across ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetState {
    /// Current cap on simultaneously decoded tiles.
    pub cap: usize,
    /// `now_ms` timestamp of the most recent step (in either direction). Used
    /// for cooldown.
    pub last_step_ms: f64,
    /// Counter of consecutive samples spent holding in the *recovery*
    /// condition. Used to enforce [`RECOVERY_HOLD`] before stepping up.
    pub direction_hold: u32,
}

/// The decision produced by [`decide_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStep {
    /// Keep the cap where it is.
    Hold,
    /// Lower the cap by the carried number of tiles (the caller floors the
    /// result at [`MIN_CAP`]). The magnitude is `1` under mild pressure and
    /// `max(1, ceil(cap * 0.25))` under catastrophic pressure (see
    /// [`FPS_SEVERE`] / [`LONGTASK_SEVERE_MS_PER_SEC`]).
    Down(usize),
    /// Raise the cap by one (the caller caps the result at the natural layout
    /// count). Step-up is always a single tile.
    Up,
}

/// Median of the `render_fps` values over the last `n` samples.
///
/// Returns `None` if fewer than `n` samples are present, or if any of the last
/// `n` samples is missing its `render_fps` (a missing sample means we cannot
/// assert sustained behaviour over the window, so we decline to act).
///
/// Public so the control loop in `attendants.rs` can reuse the *exact* same
/// median definition rather than reimplementing it (which would silently drift
/// from `decide_step` if the algorithm ever changed).
pub fn median_render_fps(samples: &[BudgetSample], n: usize) -> Option<f64> {
    if n == 0 || samples.len() < n {
        return None;
    }
    let window = &samples[samples.len() - n..];
    let mut fps: Vec<f64> = Vec::with_capacity(n);
    for s in window {
        fps.push(s.render_fps?);
    }
    fps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = fps.len() / 2;
    let median = if fps.len().is_multiple_of(2) {
        (fps[mid - 1] + fps[mid]) / 2.0
    } else {
        fps[mid]
    };
    Some(median)
}

/// True if every one of the last `n` samples has `longtask_ms_per_sec`
/// at or above `threshold`. Used for sustained-severity detection (the
/// catastrophic tier is defined with a `>=` boundary per the design notes).
fn longtask_sustained_at_or_above(samples: &[BudgetSample], n: usize, threshold: f64) -> bool {
    if n == 0 || samples.len() < n {
        return false;
    }
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask_ms_per_sec >= threshold)
}

/// True if every one of the last `n` samples has `longtask_ms_per_sec`
/// strictly above `threshold`. Used for sustained-busy detection.
fn longtask_sustained_above(samples: &[BudgetSample], n: usize, threshold: f64) -> bool {
    if n == 0 || samples.len() < n {
        return false;
    }
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask_ms_per_sec > threshold)
}

/// True when the last [`SUSTAIN_SAMPLES`] samples qualify as *recovery*: a
/// healthy median FPS (>= [`FPS_STEP_UP`]) over the window AND an idle main
/// thread (every sample's `longtask_ms_per_sec` < [`LONGTASK_IDLE_MS_PER_SEC`])
/// across that same window.
///
/// This is the SINGLE source of truth for the recovery condition. [`decide_step`]
/// uses it to gate the up-step, and the control loop in `attendants.rs` uses
/// the *same* function to decide whether to increment `direction_hold`. Keeping
/// them in one place prevents the two from silently drifting apart (which would
/// desync `direction_hold` accounting from the up-step gate).
///
/// `n` is the sustain window length; callers pass [`SUSTAIN_SAMPLES`].
pub fn recovery_qualifying(samples: &[BudgetSample], n: usize) -> bool {
    let fps_healthy = median_render_fps(samples, n)
        .map(|m| m >= FPS_STEP_UP)
        .unwrap_or(false);
    if !fps_healthy {
        return false;
    }
    // Idle for the entire window. `samples.len() >= n` is guaranteed by the
    // `median_render_fps` success above (it returns `None` otherwise).
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask_ms_per_sec < LONGTASK_IDLE_MS_PER_SEC)
}

/// True when the last `n` samples are NOT exhibiting *measured step-down
/// pressure* — i.e. the machine has headroom to grow the cap toward `natural`
/// to accommodate newly joined peers.
///
/// This is the exact logical COMPLEMENT of [`decide_step`]'s step-DOWN
/// condition:
///
/// - median render FPS over the window `>= FPS_STEP_DOWN` (>= 24 — the distress
///   floor, which deliberately INCLUDES the 24-30 hysteresis band), AND
/// - every sample's `longtask_ms_per_sec` `< LONGTASK_BUSY_MS_PER_SEC`.
///
/// ## Why a separate, broader gate than [`recovery_qualifying`]?
///
/// [`recovery_qualifying`] (median FPS `>= FPS_STEP_UP` = 30 + idle long-task)
/// is the STRICT gate that re-grows tiles which were dropped by a *measured
/// down-step*; pairing it with `RECOVERY_HOLD` + the up-cooldown damps
/// oscillation around a pressure-induced trough.
///
/// But a perfectly healthy machine on a 30 Hz panel (or a throttled-but-fine
/// tab) reports ~29 fps and sits in the 24-30 band forever, so it can NEVER
/// satisfy the strict gate. Growing the cap to accommodate newly joined peers
/// must therefore use this broader non-distress gate, which includes the 24-30
/// band — otherwise such a machine is permanently trapped below `natural` (the
/// dead-band regression this fix removes).
///
/// Anti-oscillation is preserved by the CALLER, which rate-limits this growth
/// to one tile per [`STEP_UP_COOLDOWN_MS`] using the same `last_step_ms` that a
/// down-step refreshes: a machine that just dropped a tile under pressure cannot
/// re-add it until a full up-cooldown elapses with no further down-step, and a
/// flapping machine keeps tripping the down condition so the cooldown never
/// elapses.
///
/// Returns `false` for a short window (fewer than `n` samples) or any missing
/// `render_fps` in the window, mirroring [`decide_step`]'s conservative handling.
pub fn non_distress_growth_qualifying(samples: &[BudgetSample], n: usize) -> bool {
    if n == 0 || samples.len() < n {
        return false;
    }
    let fps_ok = median_render_fps(samples, n)
        .map(|m| m >= FPS_STEP_DOWN)
        .unwrap_or(false);
    if !fps_ok {
        return false;
    }
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask_ms_per_sec < LONGTASK_BUSY_MS_PER_SEC)
}

/// Decide the next budget step from recent samples and current state.
///
/// `samples` is most-recent-last (~the last 3–5 @ 1 Hz). `natural_count` is the
/// uncapped layout tile count (the cap will never be raised above it).
/// `now_ms` is a monotonic-ish timestamp supplied by the caller for cooldown
/// arithmetic.
///
/// Returns [`BudgetStep::Hold`] when there is insufficient data, during
/// cooldown, or when no threshold is crossed. This function is pure: it reads
/// no clock and touches no DOM, so it is fully unit-testable.
pub fn decide_step(
    samples: &[BudgetSample],
    state: &BudgetState,
    natural_count: usize,
    now_ms: f64,
) -> BudgetStep {
    // Not enough data to assert sustained behaviour either way.
    if samples.len() < SUSTAIN_SAMPLES {
        return BudgetStep::Hold;
    }

    // Asymmetric cooldown: down-steps may re-fire sooner than up-steps.
    let down_cooldown_elapsed = (now_ms - state.last_step_ms) >= STEP_DOWN_COOLDOWN_MS;
    let up_cooldown_elapsed = (now_ms - state.last_step_ms) >= STEP_UP_COOLDOWN_MS;

    // ---- Step DOWN: sustained pressure ------------------------------------
    //
    // Either the median render FPS over the sustain window is below the
    // step-down threshold, OR the main thread has been busy (long tasks) for
    // the entire sustain window. A single bad sample cannot trip this because
    // the FPS check uses a median and the long-task check requires *all*
    // samples in the window to be busy.
    let median_fps = median_render_fps(samples, SUSTAIN_SAMPLES);
    let fps_low = median_fps.map(|m| m < FPS_STEP_DOWN).unwrap_or(false);
    let longtask_busy =
        longtask_sustained_above(samples, SUSTAIN_SAMPLES, LONGTASK_BUSY_MS_PER_SEC);

    if fps_low || longtask_busy {
        // Only step down if we are not already at the floor and not cooling
        // down from a prior down-step.
        if state.cap > MIN_CAP && down_cooldown_elapsed {
            // Severity: catastrophic pressure (collapsed FPS or extreme
            // sustained jank) drops a quarter of the cap at once for fast
            // relief; mild pressure steps a single tile to avoid overshoot.
            let fps_severe = median_fps.map(|m| m <= FPS_SEVERE).unwrap_or(false);
            let longtask_severe = longtask_sustained_at_or_above(
                samples,
                SUSTAIN_SAMPLES,
                LONGTASK_SEVERE_MS_PER_SEC,
            );
            let magnitude = if fps_severe || longtask_severe {
                ((state.cap as f64 * 0.25).ceil() as usize).max(1)
            } else {
                1
            };
            return BudgetStep::Down(magnitude);
        }
        return BudgetStep::Hold;
    }

    // ---- Step UP: sustained recovery --------------------------------------
    //
    // Recovery (healthy median FPS + idle main thread across the sustain
    // window) is computed by the shared `recovery_qualifying` helper — the
    // same function the control loop uses to drive `direction_hold`. The actual
    // step only fires once that recovery condition has been held for
    // RECOVERY_HOLD samples (tracked by the caller in `direction_hold`).
    if recovery_qualifying(samples, SUSTAIN_SAMPLES) {
        if state.direction_hold >= RECOVERY_HOLD && state.cap < natural_count && up_cooldown_elapsed
        {
            return BudgetStep::Up;
        }
        return BudgetStep::Hold;
    }

    BudgetStep::Hold
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample with explicit FPS and a comfortably-idle main thread.
    fn fps_sample(fps: f64) -> BudgetSample {
        BudgetSample {
            render_fps: Some(fps),
            longtask_ms_per_sec: 0.0,
        }
    }

    /// A sample with healthy FPS but a given long-task load.
    fn longtask_sample(longtask: f64) -> BudgetSample {
        BudgetSample {
            render_fps: Some(60.0),
            longtask_ms_per_sec: longtask,
        }
    }

    fn state_with_cap(cap: usize) -> BudgetState {
        BudgetState {
            cap,
            // Far enough in the past that cooldown has elapsed for any
            // reasonable `now_ms` used in the tests.
            last_step_ms: 0.0,
            direction_hold: 0,
        }
    }

    // Far enough in the past that BOTH the down and up cooldowns have elapsed.
    const PAST_COOLDOWN: f64 = STEP_UP_COOLDOWN_MS + 1.0;

    #[test]
    fn step_down_on_sustained_low_fps() {
        // Mild pressure (between FPS_SEVERE and FPS_STEP_DOWN) → single tile.
        let mid = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let samples = [fps_sample(mid), fps_sample(mid), fps_sample(mid)];
        let state = state_with_cap(5);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Down(1)
        );
    }

    #[test]
    fn catastrophic_fps_steps_down_multiple_tiles() {
        // Median FPS at/below FPS_SEVERE → proportional drop of ceil(cap*0.25).
        let samples = [
            fps_sample(FPS_SEVERE - 2.0),
            fps_sample(FPS_SEVERE),
            fps_sample(FPS_SEVERE - 1.0),
        ];
        let state = state_with_cap(20);
        // ceil(20 * 0.25) = 5.
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Down(5)
        );
    }

    #[test]
    fn extreme_longtask_steps_down_multiple_tiles() {
        // FPS is healthy, but sustained long-task time is catastrophic →
        // proportional drop.
        let samples = [
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC + 10.0),
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC + 20.0),
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC),
        ];
        let state = state_with_cap(12);
        // ceil(12 * 0.25) = 3.
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Down(3)
        );
    }

    #[test]
    fn mild_pressure_is_single_tile_down() {
        // Just below FPS_STEP_DOWN but well above FPS_SEVERE, and long-tasks are
        // busy-but-not-severe → single tile, never proportional.
        let mild = FPS_STEP_DOWN - 1.0;
        let samples = [
            BudgetSample {
                render_fps: Some(mild),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC + 10.0,
            },
            BudgetSample {
                render_fps: Some(mild),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC + 10.0,
            },
            BudgetSample {
                render_fps: Some(mild),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC + 10.0,
            },
        ];
        let state = state_with_cap(20);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Down(1)
        );
    }

    #[test]
    fn no_step_on_single_bad_sample_amid_good() {
        // One dreadful frame surrounded by healthy frames: the median stays
        // high, so no step down.
        let samples = [fps_sample(60.0), fps_sample(5.0), fps_sample(60.0)];
        let state = state_with_cap(5);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn step_up_only_after_recovery_hold() {
        let samples = [fps_sample(60.0), fps_sample(58.0), fps_sample(59.0)];

        // Recovery is healthy but has not been held long enough yet.
        let mut state = state_with_cap(3);
        state.direction_hold = RECOVERY_HOLD - 1;
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );

        // Held long enough -> step up.
        state.direction_hold = RECOVERY_HOLD;
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Up
        );
    }

    #[test]
    fn cooldown_blocks_rapid_down_resteps() {
        // Mild pressure so the step is a single tile (severity tested elsewhere).
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let samples = [fps_sample(mild), fps_sample(mild), fps_sample(mild)];
        let mut state = state_with_cap(5);
        // Last step happened "now"; down-cooldown not yet elapsed.
        state.last_step_ms = 1_000.0;
        let now = state.last_step_ms + STEP_DOWN_COOLDOWN_MS - 1.0;
        assert_eq!(decide_step(&samples, &state, 9, now), BudgetStep::Hold);

        // Once the down-cooldown elapses, the same pressure does step down.
        let now = state.last_step_ms + STEP_DOWN_COOLDOWN_MS;
        assert_eq!(decide_step(&samples, &state, 9, now), BudgetStep::Down(1));
    }

    #[test]
    fn down_cooldown_is_shorter_than_up_cooldown() {
        // At a time past the down-cooldown but before the up-cooldown, a
        // down-step is allowed (relief is prioritised) while an up-step is not.
        const { assert!(STEP_DOWN_COOLDOWN_MS < STEP_UP_COOLDOWN_MS) };
        let between = (STEP_DOWN_COOLDOWN_MS + STEP_UP_COOLDOWN_MS) / 2.0;

        // Down path: pressure, just past the down-cooldown → steps down.
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let down_samples = [fps_sample(mild), fps_sample(mild), fps_sample(mild)];
        let down_state = state_with_cap(5);
        assert_eq!(
            decide_step(&down_samples, &down_state, 9, between),
            BudgetStep::Down(1)
        );

        // Up path: recovery held, but only the down-cooldown has elapsed → Hold.
        let up_samples = [
            fps_sample(FPS_STEP_UP + 10.0),
            fps_sample(FPS_STEP_UP + 10.0),
            fps_sample(FPS_STEP_UP + 10.0),
        ];
        let mut up_state = state_with_cap(3);
        up_state.direction_hold = RECOVERY_HOLD;
        assert_eq!(
            decide_step(&up_samples, &up_state, 9, between),
            BudgetStep::Hold
        );
    }

    #[test]
    fn cooldown_blocks_rapid_up_resteps() {
        let samples = [fps_sample(60.0), fps_sample(60.0), fps_sample(60.0)];
        let mut state = state_with_cap(3);
        state.direction_hold = RECOVERY_HOLD;
        state.last_step_ms = 1_000.0;
        let now = state.last_step_ms + STEP_UP_COOLDOWN_MS - 1.0;
        assert_eq!(decide_step(&samples, &state, 9, now), BudgetStep::Hold);

        let now = state.last_step_ms + STEP_UP_COOLDOWN_MS;
        assert_eq!(decide_step(&samples, &state, 9, now), BudgetStep::Up);
    }

    #[test]
    fn result_never_exceeds_natural_count() {
        // Healthy + recovery held, but cap already at the natural count.
        let samples = [fps_sample(60.0), fps_sample(60.0), fps_sample(60.0)];
        let mut state = state_with_cap(4);
        state.direction_hold = RECOVERY_HOLD;
        assert_eq!(
            decide_step(&samples, &state, 4, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn result_never_below_min_cap() {
        // Sustained pressure, but already at the floor: no further down step.
        let samples = [fps_sample(5.0), fps_sample(5.0), fps_sample(5.0)];
        let state = state_with_cap(MIN_CAP);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn longtask_driven_step_down_even_when_fps_ok() {
        // FPS is perfectly healthy (60), but the main thread is saturated with
        // long tasks across the whole window -> step down.
        let samples = [
            longtask_sample(LONGTASK_BUSY_MS_PER_SEC + 50.0),
            longtask_sample(LONGTASK_BUSY_MS_PER_SEC + 60.0),
            longtask_sample(LONGTASK_BUSY_MS_PER_SEC + 70.0),
        ];
        let state = state_with_cap(5);
        // Busy-but-not-severe long-tasks → single tile.
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Down(1)
        );
    }

    #[test]
    fn single_longtask_spike_does_not_step_down() {
        // Only one busy sample in the window: long-task check requires ALL
        // samples busy, so no step.
        let samples = [
            longtask_sample(0.0),
            longtask_sample(LONGTASK_BUSY_MS_PER_SEC + 100.0),
            longtask_sample(0.0),
        ];
        let state = state_with_cap(5);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn empty_slice_holds() {
        let state = state_with_cap(5);
        assert_eq!(decide_step(&[], &state, 9, PAST_COOLDOWN), BudgetStep::Hold);
    }

    #[test]
    fn short_slice_holds() {
        let samples = [fps_sample(5.0), fps_sample(5.0)]; // < SUSTAIN_SAMPLES
        let state = state_with_cap(5);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn missing_fps_in_window_does_not_step_on_fps() {
        // A missing FPS reading in the window means we can't assert sustained
        // low FPS; with idle long-tasks this should Hold (no FPS-driven down,
        // no recovery-driven up because FPS median is unavailable).
        let samples = [
            fps_sample(10.0),
            BudgetSample {
                render_fps: None,
                longtask_ms_per_sec: 0.0,
            },
            fps_sample(10.0),
        ];
        let state = state_with_cap(5);
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn recovery_qualifying_matches_step_up_gate() {
        // Healthy FPS + idle main thread → qualifies.
        let healthy = [
            fps_sample(FPS_STEP_UP + 5.0),
            fps_sample(FPS_STEP_UP + 5.0),
            fps_sample(FPS_STEP_UP + 5.0),
        ];
        assert!(recovery_qualifying(&healthy, SUSTAIN_SAMPLES));

        // FPS in the dead band (below FPS_STEP_UP) → does not qualify.
        let mid = (FPS_STEP_DOWN + FPS_STEP_UP) / 2.0;
        let deadband = [fps_sample(mid), fps_sample(mid), fps_sample(mid)];
        assert!(!recovery_qualifying(&deadband, SUSTAIN_SAMPLES));

        // Healthy FPS but a busy main thread in the window → does not qualify.
        let busy = [
            BudgetSample {
                render_fps: Some(FPS_STEP_UP + 5.0),
                longtask_ms_per_sec: LONGTASK_IDLE_MS_PER_SEC + 1.0,
            },
            fps_sample(FPS_STEP_UP + 5.0),
            fps_sample(FPS_STEP_UP + 5.0),
        ];
        assert!(!recovery_qualifying(&busy, SUSTAIN_SAMPLES));

        // Too few samples → does not qualify.
        assert!(!recovery_qualifying(&healthy[..1], SUSTAIN_SAMPLES));
    }

    #[test]
    fn non_distress_growth_qualifying_includes_2430_band() {
        // 29 fps (in the 24-30 band) with idle long-task: does NOT qualify for
        // the STRICT recovery gate (needs >= 30)...
        let band = [fps_sample(29.0), fps_sample(29.0), fps_sample(29.0)];
        assert!(!recovery_qualifying(&band, SUSTAIN_SAMPLES));
        // ...but DOES qualify for the broader non-distress growth gate, which is
        // exactly the dead-band fix (HCL #987 review FIX 1).
        assert!(non_distress_growth_qualifying(&band, SUSTAIN_SAMPLES));

        // 60 fps idle also qualifies.
        let fast = [fps_sample(60.0), fps_sample(60.0), fps_sample(60.0)];
        assert!(non_distress_growth_qualifying(&fast, SUSTAIN_SAMPLES));
    }

    #[test]
    fn non_distress_growth_is_complement_of_step_down() {
        // Below the distress floor (< FPS_STEP_DOWN): does NOT qualify for growth.
        let low = [
            fps_sample(FPS_STEP_DOWN - 1.0),
            fps_sample(FPS_STEP_DOWN - 1.0),
            fps_sample(FPS_STEP_DOWN - 1.0),
        ];
        assert!(!non_distress_growth_qualifying(&low, SUSTAIN_SAMPLES));

        // Busy main thread (>= LONGTASK_BUSY_MS_PER_SEC) even with healthy FPS:
        // does NOT qualify (it is the down-pressure condition).
        let busy = [
            BudgetSample {
                render_fps: Some(60.0),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC,
            },
            BudgetSample {
                render_fps: Some(60.0),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC,
            },
            BudgetSample {
                render_fps: Some(60.0),
                longtask_ms_per_sec: LONGTASK_BUSY_MS_PER_SEC,
            },
        ];
        assert!(!non_distress_growth_qualifying(&busy, SUSTAIN_SAMPLES));

        // Short / incomplete window: declines to act.
        assert!(!non_distress_growth_qualifying(&low[..1], SUSTAIN_SAMPLES));
        let missing = [
            fps_sample(29.0),
            BudgetSample {
                render_fps: None,
                longtask_ms_per_sec: 0.0,
            },
            fps_sample(29.0),
        ];
        assert!(!non_distress_growth_qualifying(&missing, SUSTAIN_SAMPLES));
    }

    // ── Control-loop growth/anti-oscillation simulation ──────────────────────
    //
    // The async control loop in `attendants.rs` is not itself unit-testable
    // (signals + diagnostics bus), but its per-tick cap arithmetic is built
    // entirely from the pure helpers in this module. `sim_tick` reproduces that
    // arithmetic EXACTLY (down-step, strict-recovery up-step via `decide_step`,
    // and the non-distress growth gate in the Hold arm) so the key invariants
    // can be asserted deterministically. Keep this in lockstep with the loop.
    fn sim_tick(samples: &[BudgetSample], state: &mut BudgetState, natural: usize, now: f64) {
        let step = decide_step(samples, state, natural, now);
        if recovery_qualifying(samples, SUSTAIN_SAMPLES) {
            state.direction_hold = state.direction_hold.saturating_add(1);
        } else {
            state.direction_hold = 0;
        }
        match step {
            BudgetStep::Down(magnitude) => {
                state.cap = state.cap.saturating_sub(magnitude).max(MIN_CAP);
                state.last_step_ms = now;
                state.direction_hold = 0;
            }
            BudgetStep::Up => {
                state.cap = (state.cap + 1).min(natural.max(MIN_CAP));
                state.last_step_ms = now;
                state.direction_hold = 0;
            }
            BudgetStep::Hold => {
                let target = natural.max(MIN_CAP);
                let up_cooldown_elapsed = (now - state.last_step_ms) >= STEP_UP_COOLDOWN_MS;
                if state.cap < target
                    && up_cooldown_elapsed
                    && non_distress_growth_qualifying(samples, SUSTAIN_SAMPLES)
                {
                    state.cap += 1;
                    state.last_step_ms = now;
                }
            }
        }
    }

    #[test]
    fn healthy_29fps_30hz_panel_converges_to_natural_no_deadband() {
        // INVARIANT: a healthy machine reporting steady 29 fps (in the 24-30
        // band) with idle long-tasks reaches and HOLDS cap == natural. This is
        // the regression guard: under the old strict-recovery-only climb it
        // would have been trapped at MIN_CAP forever. (HCL #987 review FIX 1.)
        let natural = 12;
        // Worst case for the fix: start at MIN_CAP (e.g. seed had no natural yet
        // and a transient nudged it). Growth must still reach natural.
        let mut state = BudgetState {
            cap: MIN_CAP,
            last_step_ms: 0.0,
            direction_hold: 0,
        };
        let steady29 = [fps_sample(29.0); 5];
        // Advance the clock one up-cooldown per tick so the gate can fire each
        // step (matches the loop's one-tile-per-up-cooldown rate limit).
        let mut now = STEP_UP_COOLDOWN_MS;
        for _ in 0..(natural * 2) {
            sim_tick(&steady29, &mut state, natural, now);
            now += STEP_UP_COOLDOWN_MS;
        }
        assert_eq!(
            state.cap, natural,
            "29 fps healthy machine must reach natural"
        );

        // And it HOLDS there (never overshoots, never oscillates).
        for _ in 0..5 {
            sim_tick(&steady29, &mut state, natural, now);
            now += STEP_UP_COOLDOWN_MS;
            assert_eq!(state.cap, natural);
        }
    }

    #[test]
    fn stepped_down_machine_does_not_regrow_on_single_good_sample() {
        // INVARIANT (anti-oscillation): a machine that dropped a tile under real
        // pressure does not instantly re-add it on the next good sample. Growth
        // is rate-limited by the up-cooldown off the SAME last_step_ms the
        // down-step refreshed.
        let natural = 12;
        let mut state = BudgetState {
            cap: 8,
            last_step_ms: 0.0,
            direction_hold: 0,
        };
        // Real pressure: sustained mild-low FPS -> a down-step. `t_down` is past
        // the down-cooldown (last_step_ms == 0) so the step actually fires.
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let low = [fps_sample(mild); 5];
        let t_down = STEP_DOWN_COOLDOWN_MS + 1.0;
        sim_tick(&low, &mut state, natural, t_down);
        assert_eq!(state.cap, 7, "single mild down-step");
        assert_eq!(state.last_step_ms, t_down);

        // Immediately healthy again, but only a fraction of the up-cooldown has
        // elapsed: the cap must NOT grow yet.
        let healthy = [fps_sample(29.0); 5];
        sim_tick(
            &healthy,
            &mut state,
            natural,
            t_down + STEP_UP_COOLDOWN_MS - 1.0,
        );
        assert_eq!(state.cap, 7, "must not re-grow before up-cooldown elapses");

        // Once the up-cooldown elapses, a single tile may be re-added.
        sim_tick(&healthy, &mut state, natural, t_down + STEP_UP_COOLDOWN_MS);
        assert_eq!(state.cap, 8, "one tile back after the up-cooldown");
    }

    #[test]
    fn new_peer_growth_tracks_natural_promptly_when_not_distressed() {
        // A non-distressed machine at the natural count: when natural grows
        // (new peer joins), the cap follows it up one tile per up-cooldown.
        let mut state = BudgetState {
            cap: 5,
            last_step_ms: 0.0,
            direction_hold: 0,
        };
        let healthy = [fps_sample(29.0); 5];
        // natural jumps 5 -> 8 (three peers joined).
        let natural = 8;
        let mut now = STEP_UP_COOLDOWN_MS;
        for _ in 0..3 {
            sim_tick(&healthy, &mut state, natural, now);
            now += STEP_UP_COOLDOWN_MS;
        }
        assert_eq!(state.cap, natural);
    }

    #[test]
    fn first_down_step_starts_from_natural_not_stale_seed() {
        // Pressured-latch model (HCL #987 review FIX 1 + FIX 2): while NOT
        // pressured, the control loop keeps `state.cap` synced to the displayed
        // `natural` count, so the FIRST pressure-driven down-step's magnitude is
        // computed against what is actually on screen — never a stale MIN_CAP
        // seed. This pins that contract at the pure layer: with cap == natural
        // and catastrophic FPS, the proportional drop is ceil(natural * 0.25),
        // and applying it lands at natural - that magnitude (floored at MIN_CAP).
        let natural = 20;
        // Loop syncs state.cap to natural before pressure hits.
        let state = state_with_cap(natural);
        let severe = [
            fps_sample(FPS_SEVERE - 1.0),
            fps_sample(FPS_SEVERE),
            fps_sample(FPS_SEVERE - 2.0),
        ];
        let step = decide_step(&severe, &state, natural, PAST_COOLDOWN);
        // ceil(20 * 0.25) = 5.
        assert_eq!(step, BudgetStep::Down(5));
        if let BudgetStep::Down(magnitude) = step {
            // This mirrors the loop's first-down-step application off `natural`.
            let applied = natural.saturating_sub(magnitude).max(MIN_CAP);
            assert_eq!(applied, 15, "first down-step lands at natural - magnitude");
        }
    }

    #[test]
    fn hysteresis_band_holds_between_thresholds() {
        // Median FPS sits in the dead-band (above STEP_DOWN, below STEP_UP):
        // neither step fires regardless of recovery hold.
        let mid = (FPS_STEP_DOWN + FPS_STEP_UP) / 2.0;
        let samples = [fps_sample(mid), fps_sample(mid), fps_sample(mid)];
        let mut state = state_with_cap(5);
        state.direction_hold = RECOVERY_HOLD;
        assert_eq!(
            decide_step(&samples, &state, 9, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }
}
