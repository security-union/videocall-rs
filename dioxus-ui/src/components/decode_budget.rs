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
//! 2. `longtask` — main-thread long-task time per wall-clock second (a proxy
//!    for main-thread saturation / jank), as `Option<f64>`. It is `None` on
//!    browsers where the Long Tasks API is unavailable (WebKit/Safari + all iOS
//!    browsers — issue #1286), which is treated conservatively everywhere: a
//!    `None` can never confirm "idle / not busy" (so it never permits growth or
//!    recovery) but also never manufactures distress (so it never suppresses an
//!    FPS-driven protective step-down).
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

/// Absolute upper bound on simultaneously-decoded tiles for **mobile WebKit
/// (iOS)** devices, regardless of core count (issue #1286 / #1289).
///
/// First-guess value — pending a performance-reviewer pass. DO NOT treat as
/// final.
///
/// ## Why a hard mobile ceiling exists
///
/// On iOS the only main-thread-saturation signal we have (`longtask`) does not
/// exist (the Long Tasks API is WebKit-blind), and `render_fps` (rAF) measures
/// compositor paint, not video decode, so it stays healthy while the decode
/// pipeline backs up. With no valid backpressure signal, the adaptive loop
/// reads "healthy" and would otherwise grow the cap without bound. In #1289 a
/// 4-core iPhone ratcheted its cap 9 → 14 tiles until video froze and audio
/// backed up 1725 ms. This ceiling is the floor of last resort that binds even
/// when every signal reads healthy.
pub const IOS_DECODE_TILE_CEILING_ABS: usize = 6;

/// Pure, unit-testable device-class tile ceiling (issue #1286 / #1289).
///
/// Returns `Some(max_tiles)` when a device-class upper bound applies, or `None`
/// when the platform imposes no extra ceiling (the cap is then governed solely
/// by the existing `[MIN_CAP, min(natural, CANVAS_LIMIT)]` clamp + the adaptive
/// loop). The bound is intentionally a function of `cores` so a higher-end
/// phone is allowed a few more tiles than a 4-core handset, with a hard
/// absolute cap ([`IOS_DECODE_TILE_CEILING_ABS`]) on top.
///
/// ## Scope: iOS only, NOT all WebKit
///
/// The ceiling is gated on `is_ios` (mobile WebKit), NOT on desktop Safari. The
/// #1289 repro and the real decode-collapse risk are on phones; a Mac Pro
/// running desktop Safari should not be clamped to a phone-class tile budget.
/// Part A's longtask-blind conservatism already covers ALL WebKit (it only
/// *prevents* unjustified growth; it does not force a low ceiling), so desktop
/// Safari is protected from the ratchet without an aggressive absolute cap.
///
/// ## Tiers (first-guess — DO NOT treat as final)
///
/// - `is_ios == false` → `None` (no device ceiling).
/// - iOS, `cores <= 4` → 4 tiles. This is the #1289 class (a 4-core iPhone that
///   ratcheted to 14 and collapsed); 4 is WELL below that 14-tile failure
///   point and below the 9-tile point where the ratchet began.
/// - iOS, `cores in 5..=7` → 5 tiles.
/// - iOS, `cores >= 8` → [`IOS_DECODE_TILE_CEILING_ABS`] (6) tiles — the hard
///   absolute mobile cap; even a top-tier phone never decodes more.
///
/// `cores == 0` (navigator could not report a count) is treated as the most
/// conservative `<= 4` tier.
pub fn ios_decode_tile_ceiling(is_ios: bool, cores: u32) -> Option<usize> {
    if !is_ios {
        return None;
    }
    let tiles = match cores {
        0..=4 => 4,
        5..=7 => 5,
        _ => IOS_DECODE_TILE_CEILING_ABS,
    };
    // Never exceed the hard absolute mobile cap, and never drop below MIN_CAP.
    // `MIN_CAP` (1) < `IOS_DECODE_TILE_CEILING_ABS` (6) are both consts, so the
    // clamp bounds are well-ordered and `clamp` cannot panic.
    Some(tiles.clamp(MIN_CAP, IOS_DECODE_TILE_CEILING_ABS))
}

/// A single ~1 Hz quality sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetSample {
    /// Local render rate for this sampling window, if measured. `None` means
    /// the renderer produced no measurable FPS for this window (treated as
    /// missing, not as zero).
    pub render_fps: Option<f64>,
    /// Main-thread long-task time per wall-clock second for this window, if the
    /// Long Tasks API is available on this browser. `None` means **the signal
    /// is unavailable** (e.g. WebKit/Safari + all iOS browsers, where the
    /// `"longtask"` PerformanceObserver entry type is not implemented and the
    /// observer is never installed — issue #1286). It is distinct from
    /// `Some(0.0)`, which means the signal IS available and the main thread was
    /// genuinely idle this window. A `None` longtask is treated conservatively
    /// everywhere: it can NEVER confirm "idle / not busy" (so it never permits
    /// cap growth or recovery), but it also never *manufactures* distress (so
    /// it never suppresses an FPS-driven protective step-down). This mirrors the
    /// conservative handling of a `None` `render_fps`.
    ///
    /// TODO(#1024/#1025/#1020): WebKit/iOS has no main-thread-saturation signal
    /// at all today, so the controller flies blind on jank there and relies on
    /// the device-class tile ceiling (Part B / `ios_decode_tile_ceiling`) plus
    /// FPS alone. A real iOS-valid backpressure signal (decode-queue depth) is
    /// tracked by #1024/#1025/#1020; wire it in here when it lands.
    pub longtask: Option<f64>,
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

/// True if every one of the last `n` samples has a *present* `longtask`
/// reading at or above `threshold`. Used for sustained-severity detection (the
/// catastrophic tier is defined with a `>=` boundary per the design notes).
///
/// A `None` longtask (signal unavailable — WebKit/iOS, issue #1286) maps to
/// `false` for that sample, so it can never *manufacture* a severe down-trigger:
/// absence of evidence is not evidence of jank. The protective step-down still
/// fires off FPS alone (see [`decide_step`]); this only governs the *long-task*
/// severity tier.
fn longtask_sustained_at_or_above(samples: &[BudgetSample], n: usize, threshold: f64) -> bool {
    if n == 0 || samples.len() < n {
        return false;
    }
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask.map(|lt| lt >= threshold).unwrap_or(false))
}

/// True if every one of the last `n` samples has a *present* `longtask`
/// reading strictly above `threshold`. Used for sustained-busy detection.
///
/// A `None` longtask (signal unavailable — WebKit/iOS, issue #1286) maps to
/// `false` for that sample, so it never adds a busy-driven down-trigger. The
/// FPS-driven step-down is unaffected.
fn longtask_sustained_above(samples: &[BudgetSample], n: usize, threshold: f64) -> bool {
    if n == 0 || samples.len() < n {
        return false;
    }
    samples[samples.len() - n..]
        .iter()
        .all(|s| s.longtask.map(|lt| lt > threshold).unwrap_or(false))
}

/// True when the last [`SUSTAIN_SAMPLES`] samples qualify as *recovery*: a
/// healthy median FPS (>= [`FPS_STEP_UP`]) over the window AND a *confirmed*
/// idle main thread (every sample has a PRESENT `longtask` reading <
/// [`LONGTASK_IDLE_MS_PER_SEC`]) across that same window.
///
/// This is the SINGLE source of truth for the recovery condition. [`decide_step`]
/// uses it to gate the up-step, and the control loop in `attendants.rs` uses
/// the *same* function to decide whether to increment `direction_hold`. Keeping
/// them in one place prevents the two from silently drifting apart (which would
/// desync `direction_hold` accounting from the up-step gate).
///
/// A `None` longtask (signal unavailable — WebKit/iOS, issue #1286) means we
/// CANNOT confirm the main thread was idle, so it does NOT qualify for recovery
/// (no up-step / no `direction_hold` increment). This is the core of the #1286
/// fix on the recovery path: a blind `0.0` must not read as "idle".
///
/// `n` is the sustain window length; callers pass [`SUSTAIN_SAMPLES`].
pub fn recovery_qualifying(samples: &[BudgetSample], n: usize) -> bool {
    let fps_healthy = median_render_fps(samples, n)
        .map(|m| m >= FPS_STEP_UP)
        .unwrap_or(false);
    if !fps_healthy {
        return false;
    }
    // Confirmed idle for the entire window. A missing longtask reading cannot
    // confirm idle, so it disqualifies. `samples.len() >= n` is guaranteed by
    // the `median_render_fps` success above (it returns `None` otherwise).
    samples[samples.len() - n..].iter().all(|s| {
        s.longtask
            .map(|lt| lt < LONGTASK_IDLE_MS_PER_SEC)
            .unwrap_or(false)
    })
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
/// - every sample has a PRESENT `longtask` reading `< LONGTASK_BUSY_MS_PER_SEC`
///   (a `None`/unavailable reading does NOT qualify — see #1286 note below).
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
///
/// ## #1286: a missing (`None`) longtask reading BLOCKS growth
///
/// This gate's whole job is to confirm the machine is "not under measured
/// pressure" before it adds decode load. A `None` longtask (signal unavailable
/// — WebKit/iOS, where the Long Tasks API does not exist) means we CANNOT
/// confirm the main thread is not busy, so it must NOT qualify for growth.
/// Treating a blind `0.0` as "not busy" is exactly the inversion that let a
/// 4-core iPhone's cap ratchet 9→14 tiles and collapse (#1289). `render_fps`
/// alone cannot save us here: rAF measures compositor paint, not decode, and
/// stays healthy on WebKit even while the decode pipeline backs up. So when the
/// long-task signal is absent, this gate returns `false` and the cap can only
/// be held or stepped DOWN (off FPS) — never grown.
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
    samples[samples.len() - n..].iter().all(|s| {
        s.longtask
            .map(|lt| lt < LONGTASK_BUSY_MS_PER_SEC)
            .unwrap_or(false)
    })
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

/// Severe-tier label for a multi-tile (`magnitude > 1`) down-step, reproducing
/// [`decide_step`]'s catastrophic-pressure test EXACTLY so a support-triage log
/// is not misled by a single closing sample (issue #1000).
///
/// FPS uses the window median (`<= FPS_SEVERE`) and long-task uses the SUSTAINED
/// window check via the SAME [`longtask_sustained_at_or_above`] predicate (with
/// [`LONGTASK_SEVERE_MS_PER_SEC`]) that `decide_step` evaluates at its severity
/// branch — so the label can never silently drift from the step magnitude. Both
/// conditions may hold at once, hence the combined `fps+longtask_severe` label.
///
/// `(false, false)` is unreachable when called as intended (`decide_step` only
/// returns `magnitude > 1` when at least one severe condition holds); it is
/// labeled `unknown_severe` rather than asserted so logging can never panic. The
/// `severe_label_matches_decide_step_severity` test pins this agreement.
pub(crate) fn severe_label(samples: &[BudgetSample], median: Option<f64>) -> &'static str {
    let fps_severe = median.map(|m| m <= FPS_SEVERE).unwrap_or(false);
    let longtask_severe =
        longtask_sustained_at_or_above(samples, SUSTAIN_SAMPLES, LONGTASK_SEVERE_MS_PER_SEC);
    match (fps_severe, longtask_severe) {
        (true, true) => "fps+longtask_severe",
        (true, false) => "fps_severe",
        (false, true) => "longtask_severe",
        (false, false) => "unknown_severe",
    }
}

/// Resolve the **effective decode cap**: the number of on-screen tiles that may
/// actually decode video, given the current override mode and adaptive state.
///
/// This is the single source of truth for the three-mode actuator. Both the
/// render path (which uses the result to split tiles into video vs avatar) and
/// the telemetry producer (which reports it on the HEALTH packet) call this so
/// the reported value can never silently drift from what is on screen — the
/// drift risk flagged in the HCL #987 review. The arms are:
///
/// - **`Fixed(n)`** — a hard manual override: exactly `n` tiles, clamped into
///   `[MIN_CAP, min(natural, CANVAS_LIMIT)]`. The upper bound is floored at
///   `MIN_CAP` so the `clamp` never sees `max < min` when `natural` is 0
///   (0-peer layouts), and capped at [`CANVAS_LIMIT`] so a tampered
///   localStorage value can't exceed the canvas.
/// - **`Auto`, not pressured** — show every natural tile (capped at
///   [`CANVAS_LIMIT`]); the adaptive loop is idle so a capable machine decodes
///   all peers immediately, including staggered joins.
/// - **`Auto`, pressured** — the adaptive control loop owns the cap; return it
///   verbatim (`cap`).
///
/// `natural` is the uncapped layout tile count and `cap` is the loop-owned
/// adaptive cap (only consulted on the pressured-Auto path).
///
/// ## `device_ceiling` (issue #1286 / #1289)
///
/// `device_ceiling` is an optional HARD upper bound that binds on EVERY mode
/// (including `Auto`-unpressured, which otherwise returns the raw `natural`).
/// It is the single place the device-class tile ceiling is enforced for the
/// actuator, so the ceiling cannot be bypassed by any path. `None` means no
/// device ceiling on this platform. The bound is applied AFTER each mode's own
/// clamp and is floored at [`MIN_CAP`] so the local participant always decodes
/// at least one tile.
///
/// This function stays pure / DOM-free: the caller computes the ceiling (e.g.
/// via [`ios_decode_tile_ceiling`] with a cached `is_ios()` + core count) and
/// passes it in, so `effective_cap` remains fully unit-testable without a
/// browser.
pub fn effective_cap(
    override_mode: crate::context::DecodeBudgetOverride,
    pressured: bool,
    natural: usize,
    cap: usize,
    device_ceiling: Option<usize>,
) -> usize {
    use crate::context::DecodeBudgetOverride;

    let base = match override_mode {
        DecodeBudgetOverride::Fixed(n) => n.clamp(
            MIN_CAP,
            natural.clamp(MIN_CAP, crate::constants::CANVAS_LIMIT),
        ),
        // Issue #1466: `All` = "decode every natural tile". Like un-pressured
        // Auto it returns `natural` capped at `CANVAS_LIMIT`, and — crucially —
        // it IGNORES `pressured` (it never consults `cap`), so engaging `All`
        // reveals every tile on the next render WITHOUT having to clear the
        // pressured latch. The device_ceiling clamp below STILL binds (the
        // #1286 iOS hardware ceiling is NOT bypassed): "All" means all the
        // layout would show, still subject to the hardware ceiling.
        DecodeBudgetOverride::All => natural.min(crate::constants::CANVAS_LIMIT),
        DecodeBudgetOverride::Auto if !pressured => natural.min(crate::constants::CANVAS_LIMIT),
        DecodeBudgetOverride::Auto => cap,
    };

    // Device-class ceiling binds last, on every mode. Floor at MIN_CAP so a
    // tiny/zero ceiling can never starve the local view below one tile.
    match device_ceiling {
        Some(ceiling) => base.min(ceiling.max(MIN_CAP)),
        None => base,
    }
}

/// Expand the decoded-tile bucket to admit the user's explicit force-decode
/// (PLAY) requests, while STILL honouring the device-class ceiling (issues
/// #1466 / #1286).
///
/// ## Why expand at all
///
/// A user who taps PLAY on a decode-budget-paused tile is making an explicit
/// "decode this peer NOW" request. Before #1466 those requests were folded into
/// `active_decode_set` (so the peer's frames decoded) but the render scope still
/// sliced the visible/avatar buckets off the UN-expanded budget cap, so a
/// requested peer that ranked beyond the cap stayed rendered as a paused avatar
/// (`force_avatar = true`) even though it was being decoded — decode and render
/// DISAGREED. The fix is to RAISE the decoded count so each requested peer lands
/// in the visible bucket and renders live.
///
/// ## Why the device ceiling must still win (#1286)
///
/// On a phone the user must NOT be able to force more simultaneous decodes than
/// the hardware can sustain. The device ceiling (computed by
/// [`ios_decode_tile_ceiling`]) is the SAME hard upper bound `effective_cap`
/// enforces, and it binds LAST here too — identical clamp order to
/// `effective_cap` (canvas first, then device ceiling, floored at [`MIN_CAP`]),
/// so the two helpers can never disagree on the binding cap. If the user
/// requests more peers than the ceiling permits, we decode up to the ceiling
/// (the caller promotes requested peers first) and the excess requests stay
/// paused avatars — and, critically, are NOT placed in `active_decode_set`
/// (the caller intersects the merge with the decoded bucket), so decode and
/// render still agree.
///
/// ## Semantics
///
/// * `base_decoded` — the decode-budget ceiling BEFORE user requests (the
///   `effective_cap`-derived count the adaptive loop / Fixed / All produced).
/// * `requested_off_budget` — the number of DISTINCT user-requested peers that
///   are present in the tile list but ranked at/after `base_decoded` (i.e. the
///   ones not already decoded). Peers already inside `base_decoded` need no
///   expansion.
/// * `device_ceiling` — the optional #1286 hardware ceiling. `None` ⇒ no device
///   ceiling on this platform.
/// * `canvas_limit` — [`crate::constants::CANVAS_LIMIT`], the absolute canvas
///   cap, passed in so this stays pure / unit-testable.
///
/// Returns the EXPANDED decoded count. It can only GROW the bucket
/// (`>= base_decoded`): requesting peers never shrinks what was already
/// decoded. We raise to `base + requested`, then apply BOTH upper bounds —
/// `canvas_limit` and `device_ceiling.max(MIN_CAP)` (if `Some`) — and finally
/// floor back up to `base_decoded`. The two upper bounds are independent `min`s,
/// so their relative order is immaterial (`x.min(a).min(b) == x.min(b).min(a)`);
/// what matters is that both bind and that the `.max(base_decoded)` floor is
/// applied LAST.
///
/// The floor cannot lift the result back above either ceiling in production:
/// `base_decoded` was itself produced by `effective_cap` (which already applied
/// the same device ceiling and canvas cap), so `base <= min(canvas, ceiling)` on
/// entry. The floor therefore only bites on the `requested == 0` path (returning
/// exactly `base`) and as a defensive guarantee that we never return less than
/// `base`.
pub fn expand_decoded_for_requested(
    base_decoded: usize,
    requested_off_budget: usize,
    device_ceiling: Option<usize>,
    canvas_limit: usize,
) -> usize {
    // Raise the target to admit the off-budget requests.
    let target = base_decoded.saturating_add(requested_off_budget);
    // Canvas cap first (mirrors `effective_cap`'s per-mode `min(CANVAS_LIMIT)`).
    let target = target.min(canvas_limit);
    // Device-class ceiling binds last (mirrors `effective_cap`), floored at
    // MIN_CAP so a tiny/zero ceiling never starves the view below one tile.
    let target = match device_ceiling {
        Some(ceiling) => target.min(ceiling.max(MIN_CAP)),
        None => target,
    };
    // Never shrink below the pre-request base: PLAY can only expand the bucket.
    target.max(base_decoded)
}

/// Camera-on vs camera-off partition for the decode-budget tile list
/// (issue #1465).
///
/// A camera-OFF peer produces zero video to decode, so it must NOT consume a
/// decode-budget slot and must NOT be rendered with the dashed off-budget
/// outline (it would look "paused"/sheddable when there is nothing to shed).
/// This pure helper is the single source of truth for that split: callers pass
/// the candidate peers as `(tile_id, camera_on)` pairs (already in their final
/// display order), and it returns `(camera_on_real, camera_off_real)`.
///
/// Only peers in `camera_on_real` (plus any mock placeholders the caller
/// appends separately) feed the decode-budget split; `camera_off_real` peers
/// render as plain avatars outside the budget. Order within each output is the
/// input order (stable), so the caller controls deterministic rendering.
///
/// Kept pure / DOM-free / signal-free so it is host-unit-testable: the caller
/// resolves each peer's live `camera_on` (via `is_video_enabled_for_peer`)
/// before calling.
pub fn partition_camera_tiles(peers: &[(String, bool)]) -> (Vec<String>, Vec<String>) {
    let mut camera_on_real = Vec::with_capacity(peers.len());
    let mut camera_off_real = Vec::with_capacity(peers.len());
    for (tile_id, camera_on) in peers {
        if *camera_on {
            camera_on_real.push(tile_id.clone());
        } else {
            camera_off_real.push(tile_id.clone());
        }
    }
    (camera_on_real, camera_off_real)
}

/// Merge the user's explicit "force-decode this peer" requests (issue #1466)
/// into the active decode set — but ONLY for peers that actually landed in the
/// decoded bucket this render.
///
/// The per-tile PLAY button (rendered on a decode-budget-PAUSED tile) toggles a
/// peer's `session_id` into `UserRequestedDecodeCtx`. The render scope reacts by
/// EXPANDING the decoded bucket (via [`expand_decoded_for_requested`]) and
/// promoting requested peers into the visible/decoded slice, clamped by the
/// device ceiling (#1286) and the canvas/displayed limits. Some requests may not
/// fit — when the user asks for more peers than the device ceiling (or the grid)
/// can decode, the excess requests STAY paused avatars and must NOT be decoded.
///
/// This helper enforces the decode⇄render invariant at the merge point: it
/// inserts a requested id ONLY when that id is present in `decoded_bucket` (the
/// session_ids of the tiles actually rendering live video this frame —
/// `visible_tiles` for the normal grid, `ss_decoded_tiles` for screen share).
/// A requested peer that did not get a decoded slot is skipped, so it never
/// appears in `active_decode_set` while rendering as a paused avatar. Non-numeric
/// ids (e.g. `"mock-0"`) parse-fail and are silently skipped — EXACTLY mirroring
/// how `active_decode_set` is seeded from the decoded bucket
/// (`filter_map(|id| id.parse::<u64>().ok())`), so a mock or malformed id can
/// never poison the set.
///
/// In practice the decoded bucket already SEEDS `active_decode_set`, so every id
/// this helper inserts is already present — the call is a redundant-but-explicit
/// guard (defense in depth) that documents and pins the invariant. It is a union
/// (existing entries are preserved) and idempotent (re-merging the same requests
/// is a no-op), so calling it once per render is safe.
///
/// Kept pure / DOM-free / signal-free so it is host-unit-testable; the caller
/// passes `active` (the in-progress set), a borrow of the requested-id set, and
/// a borrow of the decoded-bucket id set.
pub fn merge_user_requested_decode(
    active: &mut std::collections::HashSet<u64>,
    requested: &std::collections::HashSet<String>,
    decoded_bucket: &std::collections::HashSet<u64>,
) {
    for id in requested {
        if let Ok(session_id) = id.parse::<u64>() {
            // Only force-decode a requested peer that actually got a decoded
            // slot this render (decode⇄render must agree — #1466).
            if decoded_bucket.contains(&session_id) {
                active.insert(session_id);
            }
        }
    }
}

/// Promote user-requested ("PLAY") peers that are still ranked beyond the
/// decoded window INWARD into decoded slots, so they render live instead of
/// decoded-but-shown-paused (issues #1466 / #1286).
///
/// `all_tiles` is the unified, display-ordered tile list. `visible_tile_count`
/// is the (already-expanded) decoded-window size; `displayed_tile_count` is the
/// number of real grid cells (everything past it folds into the +N badge).
/// `requested` is the set of force-decode `session_id`s. `pinned_slot`, if
/// `Some`, is the decoded-slot index the pinned peer occupies after the pin
/// swap — the cursor skips it so a promotion never evicts the pin.
///
/// ## Bounded by `displayed_tile_count` (PR #1467 review B1)
///
/// Only peers in the DISPLAYED off-budget window
/// `[visible_tile_count, displayed_tile_count)` are eligible to promote. A
/// requested peer in the true-overflow region (`idx >= displayed_tile_count`)
/// is NOT swapped inward: doing so would pull a true-overflow peer onto the grid
/// and evict a previously-displayed tile OUT to `idx >= displayed_tile_count`,
/// where neither the off-budget `avatar_tiles` slice (capped at
/// `displayed_tile_count`) nor the `camera_off_tiles` group renders it — the
/// evicted peer would silently vanish from the grid while the +N badge count
/// stayed unchanged. Bounding the eligible range to `displayed_tile_count`
/// guarantees every displaced tile lands back in the renderable
/// `[visible_tile_count, displayed_tile_count)` range, and true-overflow
/// requests genuinely stay in the +N badge (they get no decoded slot, so the
/// phase-4 merge keeps them out of `active_decode_set` and decode⇄render still
/// agree).
///
/// Kept pure / DOM-free / signal-free so it is host-unit-testable; the caller
/// resolves `pinned_slot` (via `client.get_peer_user_id`) before calling.
pub fn promote_requested_into_decoded(
    all_tiles: &mut [String],
    visible_tile_count: usize,
    displayed_tile_count: usize,
    requested: &std::collections::HashSet<String>,
    pinned_slot: Option<usize>,
) {
    if visible_tile_count == 0 || visible_tile_count >= all_tiles.len() || requested.is_empty() {
        return;
    }
    // Cursor: the next free decoded slot, walking down from the last one.
    // `isize` so the "ran out of slots" boundary (-1) is representable.
    let mut next_free_slot: isize = visible_tile_count as isize - 1;
    // Collect the indices to promote first (an immutable borrow), then perform
    // the swaps, so we never alias `all_tiles`. `take(displayed_tile_count)`
    // bounds eligibility to the renderable window — see the B1 note above.
    let promote_indices: Vec<usize> = all_tiles
        .iter()
        .enumerate()
        .take(displayed_tile_count)
        .skip(visible_tile_count)
        .filter(|(_, tile_id)| requested.contains(*tile_id))
        .map(|(idx, _)| idx)
        .collect();
    for idx in promote_indices {
        // Advance the cursor past the pinned slot (never evict the pin) and
        // stop if we have exhausted the decoded slots.
        while next_free_slot >= 0 && Some(next_free_slot as usize) == pinned_slot {
            next_free_slot -= 1;
        }
        if next_free_slot < 0 {
            break;
        }
        all_tiles.swap(next_free_slot as usize, idx);
        next_free_slot -= 1;
    }
}

/// Promote a pinned peer ranked beyond the decoded window INWARD into the last
/// decoded slot, so it renders live video instead of decoded-but-shown-paused
/// (HCL #987 review FIX 7). A pinned peer is force-added to `active_decode_set`
/// (phase 3) regardless of rank, so without this swap an off-budget pin would be
/// decoded yet rendered as a "Video paused" avatar — wasted decode AND a
/// misleading UI.
///
/// `all_tiles` is the unified, display-ordered tile list. `visible_tile_count`
/// is the decoded-window size; `displayed_tile_count` is the number of real grid
/// cells (everything past it folds into the +N badge). `pinned_idx` is the
/// pinned peer's index in `all_tiles` (the caller resolves it via
/// `client.get_peer_user_id`, which is not host-testable, so it is passed in).
///
/// ## Bounded by `displayed_tile_count` (issue #1470)
///
/// Only a pin in the DISPLAYED off-budget window
/// `[visible_tile_count, displayed_tile_count)` is swapped inward. A pin in the
/// true-overflow region (`pinned_idx >= displayed_tile_count` — e.g. a pinned,
/// silent, late-joiner in a meeting whose camera-ON + mock tiles exceed
/// `layout_limit`) is NOT promoted: swapping it inward would evict the peer at
/// `visible_tile_count - 1` OUT to `pinned_idx >= displayed_tile_count`, where
/// neither the off-budget `avatar_tiles` slice (capped at `displayed_tile_count`)
/// nor the `camera_off_tiles` group renders it — the evicted peer would silently
/// vanish from the grid while the +N badge count stayed unchanged. This is the
/// exact defect bounded on the PLAY path in
/// [`promote_requested_into_decoded`] (PR #1467 review B1); the pin path shares
/// the mechanism and is bounded identically here. A true-overflow pin correctly
/// stays in the +N badge with no decoded slot — consistent with the
/// POST-EXPANSION INVARIANT documented for the PLAY path. (Phase 3 still
/// force-adds it to `active_decode_set` by user_id regardless of rank; that is a
/// distinct concern — admitting the pin's decode — tracked separately and
/// unchanged here.)
///
/// Kept pure / DOM-free / signal-free so it is host-unit-testable.
pub fn promote_pinned_into_decoded(
    all_tiles: &mut [String],
    visible_tile_count: usize,
    displayed_tile_count: usize,
    pinned_idx: usize,
) {
    if visible_tile_count == 0 || visible_tile_count >= all_tiles.len() {
        return;
    }
    // Only promote a pin in the displayed off-budget window. A pin already inside
    // the decoded window (`< visible_tile_count`) needs no swap; a pin in true
    // overflow (`>= displayed_tile_count`) must not evict a displayed tile
    // off-grid — see the B1 note above.
    if pinned_idx >= visible_tile_count && pinned_idx < displayed_tile_count {
        all_tiles.swap(visible_tile_count - 1, pinned_idx);
    }
}

/// True when exactly ONE real-peer tile is displayed across ALL THREE render
/// groups combined: decoded video tiles (`visible`), off-budget avatar tiles
/// (`avatar`), and camera-off avatar tiles (`camera_off`) (issues #1465, #508).
///
/// ## Why a cross-group sum and not `visible == 1`
///
/// Before the #1465 partition, the only way to have one on-screen tile was
/// `visible_tiles.len() == 1`, so the lone-peer full-bleed presentation (the
/// #508 single-peer view: one remote peer filling the tile) keyed off that.
/// The #1465 partition split camera-OFF real peers OUT of `visible`/`avatar`
/// into their own `camera_off` group, so `visible == 1` no longer implies the
/// peer is alone on screen — a camera-on peer and a camera-off peer can render
/// side by side (`visible == 1`, `camera_off == 1`). Both the visible-tiles
/// full-bleed rule and the camera-off-tiles full-bleed rule must therefore key
/// off the TOTAL displayed real-peer tiles, which is exactly this sum.
///
/// ## #1465 no-cap byte-identity invariant
///
/// With exactly one camera-ON peer and zero camera-off peers, the budget cap is
/// inactive: `visible == 1`, `avatar == 0`, `camera_off == 0`, so the sum is 1
/// and this returns `true` — the lone peer is full-bleed exactly as before the
/// partition. (`avatar` is empty unless a budget cap is active; `camera_off` is
/// empty when every peer is camera-on — see `partition_camera_tiles`.)
pub fn is_sole_real_tile(visible: usize, avatar: usize, camera_off: usize) -> bool {
    visible + avatar + camera_off == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample with explicit FPS and a comfortably-idle main thread (a
    /// *present* `Some(0.0)` long-task reading — i.e. a Chromium browser where
    /// the Long Tasks API IS available and reported genuine idleness this
    /// window). Distinct from `None` ("signal unavailable"), exercised by the
    /// #1286 tests below.
    fn fps_sample(fps: f64) -> BudgetSample {
        BudgetSample {
            render_fps: Some(fps),
            longtask: Some(0.0),
        }
    }

    /// A sample with healthy FPS but a given (present) long-task load.
    fn longtask_sample(longtask: f64) -> BudgetSample {
        BudgetSample {
            render_fps: Some(60.0),
            longtask: Some(longtask),
        }
    }

    /// A sample with explicit FPS but **no** long-task signal (`None`),
    /// modelling a WebKit/iOS browser where the Long Tasks API is unavailable
    /// (issue #1286). The control loop must treat this as "cannot confirm idle /
    /// not busy" — never as a healthy `0.0`.
    fn fps_blind_longtask_sample(fps: f64) -> BudgetSample {
        BudgetSample {
            render_fps: Some(fps),
            longtask: None,
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

    /// #1000: `severe_label` must agree with `decide_step`'s ACTUAL severity
    /// outcome for the same window. `severe_label` is only consulted on a
    /// multi-tile down-step, so the contract is: it returns a concrete severe
    /// tier IFF `decide_step` (under a down-eligible state) returns `Down(m)`
    /// with `m > 1`, and the specific tier matches WHICH condition fired. Both
    /// derive fps-severe / longtask-severe from the SAME `FPS_SEVERE` and
    /// `longtask_sustained_at_or_above(LONGTASK_SEVERE_MS_PER_SEC)` inputs, so
    /// this pins them together: changing `decide_step`'s severity
    /// predicate/threshold OR `severe_label`'s branch logic in isolation breaks
    /// the agreement and fails this test (the drift #1000 was filed to close).
    #[test]
    fn severe_label_matches_decide_step_severity() {
        // Down-eligible: cap well above MIN_CAP, cooldown long elapsed, so the
        // severity branch is reachable and `magnitude` reflects the true tier
        // (mild = 1; severe = ceil(cap*0.25) > 1 at cap = 20).
        let state = state_with_cap(20);
        let natural = 9;

        let catastrophic_fps = [
            fps_sample(FPS_SEVERE - 1.0),
            fps_sample(FPS_SEVERE),
            fps_sample(FPS_SEVERE - 2.0),
        ];
        let extreme_longtask = [
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC + 10.0),
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC),
            longtask_sample(LONGTASK_SEVERE_MS_PER_SEC + 5.0),
        ];
        let combined = [
            BudgetSample {
                render_fps: Some(FPS_SEVERE - 1.0),
                longtask: Some(LONGTASK_SEVERE_MS_PER_SEC + 10.0),
            },
            BudgetSample {
                render_fps: Some(FPS_SEVERE),
                longtask: Some(LONGTASK_SEVERE_MS_PER_SEC),
            },
            BudgetSample {
                render_fps: Some(FPS_SEVERE - 2.0),
                longtask: Some(LONGTASK_SEVERE_MS_PER_SEC + 1.0),
            },
        ];
        // Mild: FPS below step-down but ABOVE severe, main thread idle → Down(1).
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let mild_pressure = [fps_sample(mild), fps_sample(mild), fps_sample(mild)];
        // Healthy: no pressure → Hold.
        let healthy = [fps_sample(60.0), fps_sample(58.0), fps_sample(60.0)];
        // Boundary (#1316): window median EXACTLY == FPS_SEVERE. The FPS-severe predicate is
        // inclusive (`m <= FPS_SEVERE`) in BOTH decide_step and severe_label, so median ==
        // FPS_SEVERE IS severe. None of the other cases sits a median exactly on the threshold, so
        // without this one a one-sided edit of EITHER function's comparison from `<=` to `<` would
        // still pass (the #1000 drift class). With it: under `<` that function flips median==12 to
        // not-severe, breaking either the label assertion or the label/decide_step agreement.
        let fps_at_severe_boundary = [
            fps_sample(FPS_SEVERE),
            fps_sample(FPS_SEVERE),
            fps_sample(FPS_SEVERE),
        ];

        let cases: [(&str, &[BudgetSample], &str); 6] = [
            ("catastrophic_fps", &catastrophic_fps, "fps_severe"),
            (
                "fps_at_severe_boundary",
                &fps_at_severe_boundary,
                "fps_severe",
            ),
            ("extreme_longtask", &extreme_longtask, "longtask_severe"),
            ("combined", &combined, "fps+longtask_severe"),
            ("mild_pressure", &mild_pressure, "unknown_severe"),
            ("healthy", &healthy, "unknown_severe"),
        ];

        for (name, samples, expected_label) in cases {
            let median = median_render_fps(samples, SUSTAIN_SAMPLES);
            let label = severe_label(samples, median);
            assert_eq!(
                label, expected_label,
                "severe_label classification for {name}"
            );

            // Lock the boolean against decide_step's REAL magnitude: a severe
            // (non-"unknown") label IFF decide_step returns a multi-tile down-step.
            let step = decide_step(samples, &state, natural, PAST_COOLDOWN);
            let decide_severe = matches!(step, BudgetStep::Down(m) if m > 1);
            let label_severe = label != "unknown_severe";
            assert_eq!(
                decide_severe, label_severe,
                "{name}: severe_label ({label}) disagrees with decide_step ({step:?})"
            );
        }
    }

    #[test]
    fn mild_pressure_is_single_tile_down() {
        // Just below FPS_STEP_DOWN but well above FPS_SEVERE, and long-tasks are
        // busy-but-not-severe → single tile, never proportional.
        let mild = FPS_STEP_DOWN - 1.0;
        let samples = [
            BudgetSample {
                render_fps: Some(mild),
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC + 10.0),
            },
            BudgetSample {
                render_fps: Some(mild),
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC + 10.0),
            },
            BudgetSample {
                render_fps: Some(mild),
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC + 10.0),
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
                longtask: Some(0.0),
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
                longtask: Some(LONGTASK_IDLE_MS_PER_SEC + 1.0),
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
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC),
            },
            BudgetSample {
                render_fps: Some(60.0),
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC),
            },
            BudgetSample {
                render_fps: Some(60.0),
                longtask: Some(LONGTASK_BUSY_MS_PER_SEC),
            },
        ];
        assert!(!non_distress_growth_qualifying(&busy, SUSTAIN_SAMPLES));

        // Short / incomplete window: declines to act.
        assert!(!non_distress_growth_qualifying(&low[..1], SUSTAIN_SAMPLES));
        let missing = [
            fps_sample(29.0),
            BudgetSample {
                render_fps: None,
                longtask: Some(0.0),
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

    // ── effective_cap: the shared three-mode actuator ────────────────────────
    //
    // These pin the contract that the render path and the telemetry producer
    // share (HCL #987 review FIX): the reported decode-budget cap must equal
    // what is actually rendered, across all three modes plus the clamps.
    use crate::constants::CANVAS_LIMIT;
    use crate::context::DecodeBudgetOverride;

    #[test]
    fn effective_cap_auto_unpressured_is_natural_capped_at_canvas() {
        // Un-pressured Auto shows every natural tile, bounded by CANVAS_LIMIT.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 5, 99, None),
            5
        );
        assert_eq!(
            effective_cap(
                DecodeBudgetOverride::Auto,
                false,
                CANVAS_LIMIT + 7,
                99,
                None
            ),
            CANVAS_LIMIT,
            "natural above the canvas limit is capped at CANVAS_LIMIT"
        );
        // `cap` is irrelevant on the un-pressured path.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 4, 1, None),
            4
        );
    }

    #[test]
    fn effective_cap_auto_pressured_uses_loop_cap() {
        // Pressured Auto: the control loop owns the cap; return it verbatim.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, true, 12, 3, None),
            3
        );
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, true, 12, 1, None),
            1
        );
    }

    #[test]
    fn effective_cap_fixed_clamps_into_natural_and_canvas() {
        // Fixed(n) is bounded above by min(natural, CANVAS_LIMIT)...
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Fixed(6), false, 10, 99, None),
            6
        );
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Fixed(9), false, 4, 99, None),
            4,
            "Fixed cannot exceed the natural tile count"
        );
        assert_eq!(
            effective_cap(
                DecodeBudgetOverride::Fixed(CANVAS_LIMIT + 5),
                false,
                CANVAS_LIMIT + 5,
                99,
                None
            ),
            CANVAS_LIMIT,
            "Fixed is clamped to CANVAS_LIMIT even when natural is larger"
        );
        // Pressured flag is ignored under a Fixed override.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Fixed(6), true, 10, 2, None),
            6
        );
    }

    #[test]
    fn effective_cap_all_is_natural_capped_subject_to_ceiling() {
        // Issue #1466: `All` decodes every natural tile, capped at CANVAS_LIMIT,
        // and IGNORES `pressured` (so engaging All reveals all tiles without
        // clearing the latch). The independent expected literals pin each clamp.
        // No ceiling: returns the raw natural (12).
        assert_eq!(
            effective_cap(DecodeBudgetOverride::All, false, 12, 99, None),
            12,
            "All with no ceiling returns the natural tile count"
        );
        // Pressured is ignored under All (same as Fixed): still 12, not `cap`.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::All, true, 12, 3, None),
            12,
            "All ignores the pressured flag and the loop-owned cap"
        );
        // The #1286 device ceiling STILL binds on All — it is NOT a ceiling
        // bypass. With natural 12 and a 4-tile ceiling, the effective cap is 4.
        // MUTATION THAT BREAKS IT: making the All arm skip the device_ceiling
        // clamp (returning natural unconditionally) yields 12 here.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::All, false, 12, 99, Some(4)),
            4,
            "All is still clamped by the iOS device ceiling (issue 1286)"
        );
        // Natural above the canvas limit is capped at CANVAS_LIMIT.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::All, false, CANVAS_LIMIT + 5, 99, None),
            CANVAS_LIMIT,
            "All never exceeds CANVAS_LIMIT"
        );
    }

    #[test]
    fn effective_cap_floors_at_min_cap_for_zero_peers() {
        // 0-peer layout: the clamp upper bound is floored at MIN_CAP so the
        // result is never below MIN_CAP and `clamp` never sees max < min.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 0, 0, None),
            0,
            "un-pressured Auto reports the raw natural (0) — caller floors at render"
        );
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Fixed(4), false, 0, 0, None),
            MIN_CAP,
            "Fixed never returns below MIN_CAP even with 0 natural tiles"
        );
    }

    // ── #1286 Part A: longtask-blind (None) handling ─────────────────────────
    //
    // These pin the core inversion fix: on a browser where the Long Tasks API is
    // unavailable (WebKit/iOS), `longtask` is `None` and the controller must
    // treat it as "cannot confirm not-busy / idle" — never as a healthy `0.0`.
    // Each test would PASS again (i.e. fail to protect) if `BudgetSample.longtask`
    // were reverted to a bare `f64` defaulting the blind case to `0.0`, because
    // `0.0 < LONGTASK_BUSY/IDLE` would re-permit growth.

    #[test]
    fn blind_longtask_with_healthy_fps_does_not_grow() {
        // SOURCE-OF-TRUTH PINNED: `non_distress_growth_qualifying` must return
        // false when longtask is None, even at a healthy 60 fps. MUTATION THAT
        // BREAKS IT: mapping `None` -> "not busy" (e.g. `.unwrap_or(true)` or a
        // bare-f64 `0.0`) in `non_distress_growth_qualifying` makes this pass the
        // gate and the assert fails. This is the #1289 iPhone ratchet.
        let blind = [fps_blind_longtask_sample(60.0); 5];
        assert!(
            !non_distress_growth_qualifying(&blind, SUSTAIN_SAMPLES),
            "blind (None) longtask must NOT qualify for growth even at 60 fps"
        );

        // And `decide_step` must NOT return Up on the blind+healthy window: a
        // None longtask cannot satisfy `recovery_qualifying`, so even with the
        // recovery hold maxed out there is no up-step. MUTATION: making
        // `recovery_qualifying` treat None as idle returns Up here.
        let mut state = state_with_cap(5);
        state.direction_hold = RECOVERY_HOLD;
        assert_ne!(
            decide_step(&blind, &state, 12, PAST_COOLDOWN),
            BudgetStep::Up,
            "blind longtask must never produce an Up step"
        );
        // Specifically, with no FPS pressure it Holds (neither grows nor shrinks).
        assert_eq!(
            decide_step(&blind, &state, 12, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    #[test]
    fn blind_longtask_with_low_fps_still_steps_down() {
        // PROTECTIVE STEP-DOWN MUST SURVIVE: a None longtask must never SUPPRESS
        // an FPS-driven down-step. With sustained low FPS and a blind longtask,
        // `decide_step` must still return Down. MUTATION THAT BREAKS IT: making
        // the down path require a present/non-None longtask (e.g. gating
        // `fps_low` on longtask) would yield Hold here and the assert fails.
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let blind_low = [fps_blind_longtask_sample(mild); 5];
        let state = state_with_cap(8);
        assert_eq!(
            decide_step(&blind_low, &state, 12, PAST_COOLDOWN),
            BudgetStep::Down(1),
            "FPS-driven protective step-down must fire even when longtask is blind"
        );
    }

    #[test]
    fn blind_longtask_does_not_manufacture_severe_or_busy() {
        // A None longtask must never ADD a long-task-driven down-trigger: the
        // sustained-busy / sustained-severe helpers map None -> false per sample.
        // MUTATION: mapping None -> true (e.g. treating absence as "above
        // threshold") would make these fire spuriously.
        let blind = [fps_blind_longtask_sample(60.0); SUSTAIN_SAMPLES];
        assert!(!longtask_sustained_above(
            &blind,
            SUSTAIN_SAMPLES,
            LONGTASK_BUSY_MS_PER_SEC
        ));
        assert!(!longtask_sustained_at_or_above(
            &blind,
            SUSTAIN_SAMPLES,
            LONGTASK_SEVERE_MS_PER_SEC
        ));
        // Healthy FPS + blind longtask => no down-trigger at all => Hold.
        let state = state_with_cap(8);
        assert_eq!(
            decide_step(&blind, &state, 12, PAST_COOLDOWN),
            BudgetStep::Hold
        );
    }

    // ── #1286 Part B: device-class tile ceiling ──────────────────────────────

    #[test]
    fn ios_ceiling_4_core_is_below_1289_ratchet_target() {
        // SOURCE-OF-TRUTH PINNED: #1289 was a 4-core iPhone whose cap ratcheted
        // to 14 tiles and collapsed. The ceiling for that device class must be
        // STRICTLY below 14 (and below the 9-tile point where the ratchet began).
        // MUTATION THAT BREAKS IT: raising the `0..=4` tier or
        // IOS_DECODE_TILE_CEILING_ABS at/above 14, or returning None for iOS.
        let c = ios_decode_tile_ceiling(true, 4).expect("iOS must have a ceiling");
        assert!(
            c < 14,
            "iOS 4-core ceiling must be below the issue 1289 ratchet target of 14"
        );
        assert!(
            c < 9,
            "iOS 4-core ceiling must be below the 9-tile ratchet start"
        );
        assert_eq!(c, 4, "first-guess 4-core tier");
        // cores == 0 (unknown) falls in the most-conservative tier.
        assert_eq!(ios_decode_tile_ceiling(true, 0), Some(4));
    }

    #[test]
    fn ios_ceiling_tiers_and_absolute_cap() {
        // Higher-core phones get a few more tiles, but never above the hard
        // absolute mobile cap. MUTATION: changing the tiers/abs cap fails these.
        assert_eq!(ios_decode_tile_ceiling(true, 6), Some(5));
        assert_eq!(
            ios_decode_tile_ceiling(true, 16),
            Some(IOS_DECODE_TILE_CEILING_ABS)
        );
        assert!(
            IOS_DECODE_TILE_CEILING_ABS < 9,
            "abs mobile cap below ratchet start"
        );
    }

    #[test]
    fn non_ios_gets_no_device_ceiling() {
        // SOURCE-OF-TRUTH PINNED: desktop/Android (is_ios == false) must get
        // None — no phone-class clamp. MUTATION: returning Some(_) for non-iOS
        // (e.g. dropping the `if !is_ios` guard) fails this.
        assert_eq!(ios_decode_tile_ceiling(false, 4), None);
        assert_eq!(ios_decode_tile_ceiling(false, 32), None);
    }

    #[test]
    fn effective_cap_device_ceiling_binds_below_growth() {
        // The ceiling must clamp the actuator on EVERY mode, including the
        // un-pressured Auto path that otherwise returns the raw natural. With a
        // 12-tile natural and a 4-tile iOS ceiling, the effective cap is 4 — even
        // though all signals are "healthy" and growth would otherwise reach 12.
        // MUTATION THAT BREAKS IT: not applying `device_ceiling` in
        // `effective_cap` (returning `base`) yields 12 and the assert fails.
        let ceiling = ios_decode_tile_ceiling(true, 4); // Some(4)
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 12, 99, ceiling),
            4,
            "un-pressured Auto must be clamped by the device ceiling"
        );
        // Pressured Auto: loop-owned cap of 9 (the #1289 ratchet value) is
        // clamped to 4 by the ceiling.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, true, 12, 9, ceiling),
            4,
            "pressured loop cap above the ceiling is clamped down"
        );
        // Fixed override above the ceiling is also clamped.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Fixed(10), false, 12, 99, ceiling),
            4
        );
        // A ceiling >= the base is a no-op.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 3, 99, Some(4)),
            3,
            "ceiling above the natural count does not lower the cap"
        );
        // No ceiling (None) leaves the cap at natural — non-iOS path.
        assert_eq!(
            effective_cap(DecodeBudgetOverride::Auto, false, 12, 99, None),
            12
        );
    }

    // ── issue #1465: camera-off peers are excluded from the decode-budget set ──

    /// Camera-OFF peers must be split out of the budget population (they have no
    /// video to decode), while camera-ON peers stay in — preserving input order
    /// in each bucket.
    ///
    /// MUTATION SENSITIVITY: this is the load-bearing #1465 assertion. If
    /// `partition_camera_tiles` is reverted to "include camera-off peers in the
    /// budget" — e.g. by pushing every peer into `camera_on_real` (dropping the
    /// `if *camera_on` branch), or by flipping the branch — then `b` ("bob",
    /// camera off) lands in `camera_on_real` and/or `camera_off_real` is empty,
    /// and BOTH asserts below fail. There are no `X == X` tautologies here: the
    /// expected vectors are independent literals, not derived from the output.
    #[test]
    fn partition_excludes_camera_off_from_budget() {
        let peers = vec![
            ("alice".to_string(), true),
            ("bob".to_string(), false),
            ("carol".to_string(), true),
            ("dave".to_string(), false),
        ];
        let (on, off) = partition_camera_tiles(&peers);
        assert_eq!(
            on,
            vec!["alice".to_string(), "carol".to_string()],
            "only camera-ON peers feed the decode budget"
        );
        assert_eq!(
            off,
            vec!["bob".to_string(), "dave".to_string()],
            "camera-OFF peers are partitioned out of the budget population"
        );
    }

    /// When every peer is camera-ON, the camera-off bucket is empty and the
    /// budget population is unchanged (the #1465 no-cap byte-identity invariant:
    /// nothing is diverted away from `all_tiles`).
    #[test]
    fn partition_all_camera_on_leaves_budget_intact() {
        let peers = vec![("alice".to_string(), true), ("bob".to_string(), true)];
        let (on, off) = partition_camera_tiles(&peers);
        assert_eq!(on, vec!["alice".to_string(), "bob".to_string()]);
        assert!(
            off.is_empty(),
            "no camera-off peers ⇒ empty group ⇒ budget population identical to pre-1465"
        );
    }

    // ── issue #1466: user-requested force-decode merge ────────────────────────

    use std::collections::HashSet;

    /// Build a `HashSet<u64>` decoded-bucket from a slice of ids (test helper).
    fn bucket(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    /// A numeric requested id that IS in the decoded bucket is inserted.
    ///
    /// MUTATION SENSITIVITY: if `merge_user_requested_decode` dropped the
    /// `.parse::<u64>()`/insert (made a no-op) the set stays empty and this
    /// fails. The expected value (123) is an independent literal.
    #[test]
    fn merge_inserts_requested_id_in_decoded_bucket() {
        let mut active: HashSet<u64> = HashSet::new();
        let mut requested: HashSet<String> = HashSet::new();
        requested.insert("123".to_string());
        merge_user_requested_decode(&mut active, &requested, &bucket(&[123]));
        assert!(active.contains(&123), "decoded requested id is force-added");
        assert_eq!(active.len(), 1, "exactly the one decoded id was inserted");
    }

    /// A requested id that did NOT get a decoded slot (not in the decoded
    /// bucket — e.g. it exceeded the device ceiling) must NOT be force-decoded.
    /// This is the #1466/#1286 decode⇄render invariant: a paused avatar is never
    /// in `active_decode_set`.
    ///
    /// MUTATION SENSITIVITY: if the helper dropped the `decoded_bucket.contains`
    /// gate (reverting to the old unconditional insert), id 555 would be added
    /// and this fails.
    #[test]
    fn merge_skips_requested_id_not_in_decoded_bucket() {
        let mut active: HashSet<u64> = HashSet::new();
        let mut requested: HashSet<String> = HashSet::new();
        requested.insert("555".to_string());
        // Decoded bucket holds a DIFFERENT peer (777), so 555 was not decoded.
        merge_user_requested_decode(&mut active, &requested, &bucket(&[777]));
        assert!(
            active.is_empty(),
            "an un-decoded requested peer must not enter active_decode_set"
        );
    }

    /// Non-numeric ids (mock placeholders, garbage) are silently skipped —
    /// matching the `filter_map(parse::<u64>)` discipline of `active_decode_set`.
    ///
    /// MUTATION SENSITIVITY: if the helper inserted on parse failure (or used a
    /// default/hash of the string), `active` would be non-empty and this fails.
    #[test]
    fn merge_skips_non_numeric_requested_id() {
        let mut active: HashSet<u64> = HashSet::new();
        let mut requested: HashSet<String> = HashSet::new();
        requested.insert("mock-0".to_string());
        requested.insert("abc".to_string());
        // Even if the (impossible) parse succeeded, the bucket is empty.
        merge_user_requested_decode(&mut active, &requested, &HashSet::new());
        assert!(active.is_empty(), "non-numeric ids must not be inserted");
    }

    /// The merge is a UNION with existing entries (pre-seeded ids survive) and is
    /// idempotent (merging twice yields the same set).
    ///
    /// MUTATION SENSITIVITY: if the helper cleared/replaced `active` instead of
    /// inserting, the pre-seeded 999 would vanish and the first assert fails.
    /// The idempotency assert pins that a second merge adds nothing new.
    #[test]
    fn merge_is_union_and_idempotent() {
        let mut active: HashSet<u64> = HashSet::new();
        active.insert(999);
        let mut requested: HashSet<String> = HashSet::new();
        requested.insert("123".to_string());

        merge_user_requested_decode(&mut active, &requested, &bucket(&[123]));
        assert!(
            active.contains(&999),
            "pre-existing entry preserved (union)"
        );
        assert!(active.contains(&123), "requested entry added");
        let after_first: HashSet<u64> = active.clone();

        // Idempotent: a second identical merge changes nothing.
        merge_user_requested_decode(&mut active, &requested, &bucket(&[123]));
        assert_eq!(active, after_first, "re-merging the same set is a no-op");
    }

    /// An empty requested set leaves `active` untouched.
    ///
    /// MUTATION SENSITIVITY: if the helper mutated `active` regardless of the
    /// requested set (e.g. cleared it) this fails.
    #[test]
    fn merge_empty_requested_leaves_active_unchanged() {
        let mut active: HashSet<u64> = HashSet::new();
        active.insert(7);
        active.insert(42);
        let requested: HashSet<String> = HashSet::new();
        merge_user_requested_decode(&mut active, &requested, &bucket(&[7, 42]));
        let mut expected: HashSet<u64> = HashSet::new();
        expected.insert(7);
        expected.insert(42);
        assert_eq!(active, expected, "empty request set is a no-op");
    }

    // ── issue #1466 / #1286: expand_decoded_for_requested ─────────────────────

    /// Zero requested off-budget peers returns the base unchanged (no expansion).
    ///
    /// MUTATION SENSITIVITY: if the helper added a constant or used a wrong base,
    /// the exact `3` literal fails.
    #[test]
    fn expand_zero_requested_returns_base() {
        assert_eq!(
            expand_decoded_for_requested(3, 0, None, CANVAS_LIMIT),
            3,
            "no requests ⇒ bucket unchanged"
        );
    }

    /// Requests within an absent ceiling expand the bucket by exactly the
    /// off-budget count.
    ///
    /// MUTATION SENSITIVITY: base=1 + requested=2 must be exactly 3. Using `+1`,
    /// `*`, or dropping the `saturating_add` of `requested_off_budget` all break
    /// this exact literal.
    #[test]
    fn expand_within_no_ceiling_adds_requested() {
        assert_eq!(
            expand_decoded_for_requested(1, 2, None, CANVAS_LIMIT),
            3,
            "base 1 + 2 requested ⇒ 3 decoded (no device ceiling)"
        );
    }

    /// The device ceiling BINDS even when the requests would push past it
    /// (#1286): base 1 + 6 requested, ceiling 4 ⇒ 4. The ceiling wins.
    ///
    /// MUTATION SENSITIVITY: if the device-ceiling clamp were dropped, the result
    /// would be 7; if it clamped before the canvas/expansion or used the wrong
    /// operand, it would not be exactly 4.
    #[test]
    fn expand_device_ceiling_binds() {
        assert_eq!(
            expand_decoded_for_requested(1, 6, Some(4), CANVAS_LIMIT),
            4,
            "device ceiling 4 caps the expansion regardless of request count"
        );
    }

    /// The device ceiling wins even when `base` already equals it: base 4 +
    /// 3 requested, ceiling 4 ⇒ 4 (no growth past the hardware cap).
    ///
    /// MUTATION SENSITIVITY: dropping the ceiling clamp yields 7; applying the
    /// `.max(base)` floor in the wrong order is still 4 here, but the
    /// `expand_device_ceiling_binds` case above catches a missing clamp.
    #[test]
    fn expand_device_ceiling_wins_when_base_equals_ceiling() {
        assert_eq!(
            expand_decoded_for_requested(4, 3, Some(4), CANVAS_LIMIT),
            4,
            "cannot expand past the device ceiling even from base == ceiling"
        );
    }

    /// The canvas limit binds when there is no device ceiling: a huge request
    /// count is capped at CANVAS_LIMIT.
    ///
    /// MUTATION SENSITIVITY: dropping the canvas clamp yields base+1000; the
    /// exact `CANVAS_LIMIT` literal pins the clamp.
    #[test]
    fn expand_canvas_limit_binds() {
        assert_eq!(
            expand_decoded_for_requested(2, 1000, None, CANVAS_LIMIT),
            CANVAS_LIMIT,
            "the absolute canvas limit caps the expansion when no device ceiling"
        );
    }

    /// Never shrinks below base: a tiny (but `Some`) ceiling below `base` is
    /// re-floored to `base` by `.max(base_decoded)`. (In production `base` is
    /// already `<= ceiling` because `effective_cap` applied the same ceiling, so
    /// this floor only ever bites on the degenerate test input.)
    ///
    /// MUTATION SENSITIVITY: if the `.max(base_decoded)` floor were dropped, this
    /// would return 2 (the ceiling), not 5.
    #[test]
    fn expand_never_shrinks_below_base() {
        assert_eq!(
            expand_decoded_for_requested(5, 0, Some(2), CANVAS_LIMIT),
            5,
            "PLAY can only expand; result is never below base even under a tiny ceiling"
        );
    }

    /// A zero device ceiling is floored to MIN_CAP (1) by `ceiling.max(MIN_CAP)`
    /// before binding, so the bucket never collapses to zero. base=1 keeps the
    /// `.max(base)` from masking the floor.
    ///
    /// MUTATION SENSITIVITY: if `ceiling.max(MIN_CAP)` were `ceiling` alone, the
    /// `min` would force 0 and the `.max(base=1)` would mask it to 1 anyway —
    /// so to pin the MIN_CAP floor specifically, use base 0: a 0 ceiling must
    /// still yield MIN_CAP (1), not 0.
    #[test]
    fn expand_min_cap_floor_on_zero_ceiling() {
        assert_eq!(
            expand_decoded_for_requested(0, 3, Some(0), CANVAS_LIMIT),
            MIN_CAP,
            "a zero device ceiling is floored to MIN_CAP, never zero"
        );
    }

    /// `is_sole_real_tile` is the shared full-bleed predicate for the normal
    /// grid (issues #1465, #508): a peer renders full-bleed IFF it is the only
    /// real-peer tile across ALL THREE groups. The lone peer can live in ANY
    /// single group (a camera-on decoded tile, an off-budget avatar tile, or a
    /// camera-off avatar tile), so all three single-group cases must be true and
    /// every multi-tile / empty combination must be false.
    ///
    /// MUTATION SENSITIVITY: the expected booleans are independent literals, not
    /// derived from the function. Weakening `== 1` to `<= 1` flips `(0,0,0)`
    /// from false → true; dropping any term from the sum (e.g. ignoring
    /// `camera_off`) flips `(0,0,1)` from true → false or `(1,0,1)` from false →
    /// true. Each such mutation breaks at least one assertion below.
    #[test]
    fn is_sole_real_tile_only_when_total_is_one() {
        // Exactly one tile in any single group → sole.
        assert!(is_sole_real_tile(1, 0, 0), "lone camera-on decoded tile");
        assert!(is_sole_real_tile(0, 1, 0), "lone off-budget avatar tile");
        assert!(is_sole_real_tile(0, 0, 1), "lone camera-off avatar tile");

        // Two or more across any groups → not sole.
        assert!(!is_sole_real_tile(2, 0, 0), "two decoded tiles");
        assert!(
            !is_sole_real_tile(1, 1, 0),
            "one decoded + one avatar = two tiles"
        );
        assert!(
            !is_sole_real_tile(1, 0, 1),
            "one camera-on + one camera-off = two tiles (the issue-1465 mixed case)"
        );

        // Zero tiles → not sole.
        assert!(!is_sole_real_tile(0, 0, 0), "no tiles is not a sole tile");
    }

    fn tiles(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn req(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// B1 REGRESSION (PR #1467 review): a requested peer ranked in the
    /// true-overflow region (`idx >= displayed_tile_count`) must NOT be promoted,
    /// and no previously-displayed tile may be pushed past `displayed_tile_count`.
    ///
    /// Setup: 10 tiles, `visible_tile_count = 4`, `displayed_tile_count = 8`.
    /// Peer "p9" sits at index 9 (true overflow, beyond the 8 grid cells) and is
    /// the only PLAY request. The bounded promotion must leave the list untouched.
    ///
    /// MUTATION SENSITIVITY: with the bug (`take(displayed_tile_count)` dropped, or
    /// `.take()` widened to `all_tiles.len()`), "p9" is swapped into slot 3 and the
    /// slot-3 peer "p3" is evicted to index 9 (off the grid). The assertions on
    /// both the decoded window AND index 9 fail under that mutation.
    #[test]
    fn promote_skips_true_overflow_request_and_keeps_displaced_renderable() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]);
        let before = all.clone();
        promote_requested_into_decoded(&mut all, 4, 8, &req(&["p9"]), None);
        // True-overflow request was ignored: list is completely unchanged.
        assert_eq!(
            all, before,
            "a request at idx 9 (>= displayed_tile_count=8) must not be promoted"
        );
        // No previously-displayed tile was pushed into the overflow region.
        assert_eq!(
            all[9], "p9",
            "true-overflow peer stays at its overflow index"
        );
        assert_eq!(
            &all[0..4],
            &["p0", "p1", "p2", "p3"],
            "the decoded window is undisturbed"
        );
    }

    /// A requested peer INSIDE the displayed off-budget window
    /// (`visible <= idx < displayed`) IS promoted into the decoded slice — the
    /// bound must not be so tight that it suppresses legitimate promotions.
    ///
    /// Setup: same 10 tiles, `visible = 4`, `displayed = 8`. "p6" (index 6, inside
    /// the displayed window) is requested. It must move into the last decoded slot
    /// (index 3), and the displaced "p3" must remain renderable (idx < 8).
    ///
    /// MUTATION SENSITIVITY: if the promotion were dropped entirely the list would
    /// be unchanged and the `all[3] == "p6"` assertion fails; if the displaced tile
    /// went past index 8 the `displaced index < 8` assertion fails.
    #[test]
    fn promote_admits_in_window_request() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]);
        promote_requested_into_decoded(&mut all, 4, 8, &req(&["p6"]), None);
        assert_eq!(
            all[3], "p6",
            "in-window request promoted into last decoded slot"
        );
        let displaced = all.iter().position(|t| t == "p3").unwrap();
        assert!(
            displaced < 8,
            "the displaced tile stays in the renderable window, not the +N overflow (was {displaced})"
        );
    }

    /// Screen-share configuration (issue #1472): the SS panel renders ALL tiles (no +N
    /// overflow badge), so it calls the helper with `displayed_tile_count == all_tiles.len()`.
    /// In that configuration the true-overflow region `[displayed_tile_count, len)` is empty,
    /// so the `.take(displayed_tile_count)` eligibility bound is a no-op and EVERY off-budget
    /// requested peer must be promoted into the decoded window — identical to the old inline SS
    /// loop that had no `.take()` at all. This pins the equivalence the #1472 DRY refactor
    /// relies on.
    ///
    /// MUTATION SENSITIVITY: if the helper's `.take(displayed_tile_count)` were tightened to a
    /// smaller bound (e.g. a hard-coded grid size < len), the deepest requested peer "p5" would
    /// fall in a spurious overflow region and NOT be promoted — `all[0] == "p5"` would fail.
    #[test]
    fn promote_with_displayed_eq_len_admits_all_offbudget_requests() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5"]);
        let len = all.len();
        // visible (ss_budget) = 2, displayed = len (no +N region). Request the two deepest
        // off-budget peers; both must be pulled into the decoded window [0, 2).
        promote_requested_into_decoded(&mut all, 2, len, &req(&["p4", "p5"]), None);
        // Cursor walks down from slot 1: first eligible (p4) → slot 1, next (p5) → slot 0.
        assert_eq!(
            all[1], "p4",
            "first off-budget request promoted into slot 1"
        );
        assert_eq!(
            all[0], "p5",
            "second off-budget request promoted into slot 0 (no overflow region excludes it)"
        );
    }

    /// The promotion cursor skips the pinned slot so a PLAY promotion never evicts
    /// the pinned peer. With `visible = 3`, `displayed = 6`, pin at slot 2, and
    /// "p4" requested, the request must land in slot 1 (slot 2 is skipped) and the
    /// pinned tile at slot 2 must be preserved.
    ///
    /// MUTATION SENSITIVITY: dropping the `pinned_slot` skip swaps "p4" into slot 2,
    /// evicting the pin — the `all[2] == "p2"` assertion fails.
    #[test]
    fn promote_skips_pinned_slot() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5"]);
        promote_requested_into_decoded(&mut all, 3, 6, &req(&["p4"]), Some(2));
        assert_eq!(all[2], "p2", "pinned slot is never evicted by a promotion");
        assert_eq!(
            all[1], "p4",
            "request lands in the next free slot below the pin"
        );
    }

    /// Empty request set is a no-op (the unpressured / no-PLAY path).
    #[test]
    fn promote_empty_request_is_noop() {
        let mut all = tiles(&["p0", "p1", "p2", "p3"]);
        let before = all.clone();
        promote_requested_into_decoded(&mut all, 2, 4, &HashSet::new(), None);
        assert_eq!(all, before, "no requests ⇒ list unchanged");
    }

    /// #1470: a pinned peer ranked in the true-overflow region
    /// (`pinned_idx >= displayed_tile_count`) must NOT be promoted, and no
    /// previously-displayed tile may be pushed off the grid past
    /// `displayed_tile_count`. Mirrors
    /// `promote_skips_true_overflow_request_and_keeps_displaced_renderable` for the
    /// PLAY path.
    ///
    /// Setup: 10 tiles, `visible_tile_count = 4`, `displayed_tile_count = 8`. The
    /// pin sits at index 9 (true overflow, beyond the 8 grid cells). The bounded
    /// swap must leave the list untouched.
    ///
    /// MUTATION SENSITIVITY: with the bug (the `pinned_idx < displayed_tile_count`
    /// bound dropped, i.e. the old unconditional `idx >= visible_tile_count` swap),
    /// the pin at 9 is swapped into slot 3 and the slot-3 peer "p3" is evicted to
    /// index 9 (off the grid). Both the decoded-window and index-9 assertions fail
    /// under that mutation.
    #[test]
    fn promote_pinned_skips_true_overflow_and_keeps_displaced_renderable() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]);
        let before = all.clone();
        promote_pinned_into_decoded(&mut all, 4, 8, 9);
        assert_eq!(
            all, before,
            "a pin at idx 9 (>= displayed_tile_count=8) must not be promoted"
        );
        assert_eq!(
            all[9], "p9",
            "true-overflow pin stays at its overflow index"
        );
        assert_eq!(
            &all[0..4],
            &["p0", "p1", "p2", "p3"],
            "the decoded window is undisturbed"
        );
    }

    /// #1470: a pin INSIDE the displayed off-budget window
    /// (`visible <= pinned_idx < displayed`) IS swapped into the last decoded slot
    /// — the bound must not be so tight that it suppresses the legitimate promotion
    /// the swap exists for.
    ///
    /// Setup: same 10 tiles, `visible = 4`, `displayed = 8`. The pin sits at index 6
    /// (inside the displayed window). It must move into the last decoded slot
    /// (index 3), and the displaced "p3" must remain renderable (idx < 8).
    ///
    /// MUTATION SENSITIVITY: if the swap were dropped the list is unchanged and the
    /// `all[3] == "p6"` assertion fails; if the displaced tile went past index 8 the
    /// `displaced < 8` assertion fails.
    #[test]
    fn promote_pinned_admits_in_window() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9"]);
        promote_pinned_into_decoded(&mut all, 4, 8, 6);
        assert_eq!(
            all[3], "p6",
            "in-window pin promoted into last decoded slot"
        );
        let displaced = all.iter().position(|t| t == "p3").unwrap();
        assert!(
            displaced < 8,
            "the displaced tile stays in the renderable window, not the +N overflow (was {displaced})"
        );
    }

    /// #1470: a pin ALREADY inside the decoded window (`pinned_idx <
    /// visible_tile_count`) needs no swap and must be left in place.
    ///
    /// MUTATION SENSITIVITY: if the lower bound (`pinned_idx >= visible_tile_count`)
    /// were dropped, the pin at slot 1 would swap with slot 3
    /// (`visible_tile_count - 1`), reordering the decoded window — the
    /// `all == before` assertion fails.
    #[test]
    fn promote_pinned_already_decoded_is_noop() {
        let mut all = tiles(&["p0", "p1", "p2", "p3", "p4", "p5"]);
        let before = all.clone();
        promote_pinned_into_decoded(&mut all, 4, 6, 1);
        assert_eq!(
            all, before,
            "a pin already in the decoded window is left untouched"
        );
    }
}
