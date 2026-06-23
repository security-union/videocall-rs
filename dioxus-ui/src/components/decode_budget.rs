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
    /// `now_ms` of the most recent *layer-drop* apply (issue #1557). Used by
    /// [`cascade_action`] for settle timing: a Down edge escalates from
    /// lowering received layers to pausing tiles only once
    /// [`STEP_DOWN_COOLDOWN_MS`] has elapsed since this timestamp.
    pub last_layer_drop_ms: f64,
    /// Set from the last `apply_local_cpu_pressure_congestion()` that ACTUALLY RAN
    /// (`Some(stepped)`) (issue #1557); a contended/skipped apply (`None`) leaves
    /// this field UNCHANGED so the cascade does not advance. `stepped == false`
    /// means no peer chooser moved — every
    /// droppable/non-exempt received layer is already at base (this includes the
    /// degenerate case where the only connected peers are active speakers, who are
    /// exempt from layer-drop, so nothing was eligible to move). When
    /// `layers_at_floor == true`, further layer drops are a no-op and
    /// [`cascade_action`] may escalate to pausing tiles.
    pub layers_at_floor: bool,
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

/// The single source of truth for whether the pressured Hold arm may grow the
/// decode cap one tile this tick (issue #1558 emergency-growth gate).
///
/// The first three inputs are the pre-existing non-distress growth conditions
/// (`state.cap < target`, the up-cooldown has elapsed, and
/// [`non_distress_growth_qualifying`] is true). The fourth, `emergency_now`, is
/// the stage-4 protective EMERGENCY flag — i.e. [`protective_emergency_cap`]
/// would return `Some(MIN_CAP)` this tick (protective mode active AND audio
/// still growing past [`PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS`]).
///
/// ## Why `emergency_now` MUST veto growth (the flap this closes)
///
/// The non-distress growth gate looks ONLY at renderer FPS + long-tasks; it is
/// blind to audio. During a SUSTAINED audio-only emergency (renderer healthy,
/// jitter buffer past the emergency mark) the first three conditions stay true,
/// so without this veto the Hold arm raises the cap (e.g. 1→2) and re-arms the
/// cascade (clearing `layers_at_floor`) every up-cooldown. The emergency clamp
/// then re-slams the cap to `MIN_CAP`, but `layers_at_floor` was already
/// cleared — so the stage-3 encoder ceiling flips to `None` for a tick and the
/// local send-ladder un-sheds, only to re-shed once the cascade re-reaches
/// floor: a ~4s oscillation that burns encode CPU exactly when audio is
/// starving. Vetoing growth while `emergency_now` holds stops the fight at the
/// source: `layers_at_floor` stays stable through the emergency, so the encoder
/// ceiling stays applied. When audio recovers, `emergency_now` becomes false
/// and normal recovery/growth resumes unchanged — this veto does NOT block
/// legitimate recovery, only growth that fights an active emergency.
///
/// Pure / DOM-free / signal-free so the gate is host-unit-testable and shared
/// verbatim by the control loop and the `sim_tick` loop model.
pub fn non_distress_growth_allowed(
    cap_below_target: bool,
    up_cooldown_elapsed: bool,
    not_distressed: bool,
    emergency_now: bool,
) -> bool {
    cap_below_target && up_cooldown_elapsed && not_distressed && !emergency_now
}

/// The single source of truth for suppressing a recovery UP-step while the
/// protective EMERGENCY is active (issue #1558 emergency-growth gate, Up arm).
///
/// [`decide_step`]'s Up arm is gated on `recovery_qualifying`, which looks ONLY
/// at renderer FPS + long-tasks and is blind to audio. So during a sustained
/// audio-only emergency a healthy renderer can produce a `BudgetStep::Up` whose
/// arm would call `re_arm_cascade_after_recovery` — clearing `layers_at_floor`
/// — and the emergency clamp would re-slam the cap to `MIN_CAP` WITHOUT
/// restoring the floor flag, flipping the stage-3 encoder ceiling to `None` for
/// a tick (the same ~4s flap [`non_distress_growth_allowed`] guards on the Hold
/// path). When `emergency_now` is true this coerces `Up` to `Hold` BEFORE the
/// caller's step match, so the cap-raise + re-arm never run; the Hold arm's own
/// growth is then vetoed by [`non_distress_growth_allowed`]. `Down` and `Hold`
/// pass through unchanged (the emergency never blocks shedding, only growth).
///
/// When audio recovers, `emergency_now` is false and the Up-step passes through
/// untouched — so this does NOT block legitimate recovery, only the recovery
/// that fights an active emergency. Pure / host-unit-testable; shared verbatim
/// by the control loop and the `sim_tick_protective` loop model so removing the
/// veto here breaks both.
pub fn suppress_growth_step(step: BudgetStep, emergency_now: bool) -> BudgetStep {
    match step {
        BudgetStep::Up if emergency_now => BudgetStep::Hold,
        other => other,
    }
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
    // Thin wrapper: compute the sustain-window median once and delegate. The
    // control loop, which ALSO needs the median for its protective distress
    // predicate, calls `decide_step_with_median` directly to compute it exactly
    // once per tick (issue #1558 perf hoist — restores the #1001 single-median
    // contract); every other caller (tests, the un-pressured latch probe) uses
    // this convenience wrapper.
    let median = median_render_fps(samples, SUSTAIN_SAMPLES);
    decide_step_with_median(samples, state, natural_count, now_ms, median)
}

/// Core of [`decide_step`] with the sustain-window median (`median_render_fps`
/// over [`SUSTAIN_SAMPLES`]) supplied by the caller, so a caller that already
/// computed it does not re-run the `Vec`-alloc+sort a second time (issue #1558
/// perf hoist; preserves the #1001 "median computed once per tick" optimization
/// on the hot steady-state path). `median` MUST equal
/// `median_render_fps(samples, SUSTAIN_SAMPLES)` — same value, just not
/// recomputed; behaviour is otherwise identical to [`decide_step`].
pub fn decide_step_with_median(
    samples: &[BudgetSample],
    state: &BudgetState,
    natural_count: usize,
    now_ms: f64,
    median: Option<f64>,
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
    let median_fps = median;
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

/// The cascade decision produced by [`cascade_action`] — whether a Down edge
/// should LOWER received simulcast layers first or escalate to PAUSING (capping)
/// decoded tiles (issue #1557).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeAction {
    None,
    LowerLayer,
    PauseTiles,
}

/// Decide whether a Down edge should lower received simulcast layers FIRST or
/// proceed to pausing (capping) tiles. Pure: no clock, no DOM, no I/O — the
/// caller supplies the facts. Issue #1557: layer-drop precedes tile-pause; tiles
/// are paused only once layers are at floor AND a settle window has elapsed.
///
/// - `down_pressure`: a Down edge was decided this tick (decide_step Down OR the
///   presenter-extra latch).
/// - `layers_at_floor`: the last layer-drop apply reported no peer moved (all
///   received choosers already at base), i.e. lowering layers further is a no-op.
/// - `settle_elapsed`: STEP_DOWN_COOLDOWN_MS has elapsed since the last
///   layer-drop, so the layer reduction has had time to take effect before we
///   escalate to pausing tiles.
pub fn cascade_action(
    down_pressure: bool,
    layers_at_floor: bool,
    settle_elapsed: bool,
) -> CascadeAction {
    if !down_pressure {
        return CascadeAction::None;
    }
    if !layers_at_floor {
        return CascadeAction::LowerLayer;
    }
    if settle_elapsed {
        CascadeAction::PauseTiles
    } else {
        CascadeAction::LowerLayer
    }
}

/// True once the cascade settle window has elapsed since the most recent REAL
/// layer drop (issue #1557). Pure: the caller supplies `now` and the frozen
/// `last_layer_drop_ms`. Reuses [`STEP_DOWN_COOLDOWN_MS`] as the settle window.
pub fn settle_window_elapsed(now: f64, last_layer_drop_ms: f64) -> bool {
    (now - last_layer_drop_ms) >= STEP_DOWN_COOLDOWN_MS
}

/// Compute the next `last_layer_drop_ms` after an
/// `apply_local_cpu_pressure_congestion()` call (issue #1557).
///
/// CRITICAL: the settle clock advances to `now` ONLY when a layer ACTUALLY moved
/// (`stepped == true`). Once received layers are at floor the apply is a per-tick
/// no-op (`stepped == false`); if the clock were reset on those no-op ticks the
/// `now - last_layer_drop_ms` delta would be pinned at a single tick-gap forever
/// and [`settle_window_elapsed`] would NEVER return true — so the cascade could
/// never escalate from lowering layers to pausing tiles. Freezing the timestamp
/// at floor lets the settle window accumulate from the moment the floor was first
/// reached, which is the entire point of the tiered cascade.
pub fn next_layer_drop_ms(prev_last_layer_drop_ms: f64, now: f64, stepped: bool) -> f64 {
    if stepped {
        now
    } else {
        prev_last_layer_drop_ms
    }
}

/// Re-arm the tier-before-pause cascade when recovery raises the cap (issue
/// #1557). Called from BOTH the `BudgetStep::Up` arm and the non-distress
/// growth step in the `BudgetStep::Hold` arm (attendants.rs) — the two places a
/// pressure episode ends by growing the cap back toward natural while the
/// controller stays latched-pressured.
///
/// Clears `layers_at_floor` and re-anchors `last_layer_drop_ms = now` so the
/// NEXT Down edge re-enters at the LowerLayer stage and must re-earn the settle
/// window before pausing a tile. WITHOUT this, the next Down edge after any
/// recovery sees a STALE `layers_at_floor == true` plus a stale (far-in-the-past)
/// `last_layer_drop_ms`, so `cascade_action(true, true, settle_elapsed=true)`
/// routes straight to PauseTiles — pausing a tile while the choosers have already
/// re-grown received layers, inverting the feature on every cycle after the
/// first. This is the single source of truth for that reset; the unit test
/// `cascade_re_arms_after_recovery_so_next_down_lowers_layers_first` calls THIS
/// function so gutting it turns the test red.
pub fn re_arm_cascade_after_recovery(state: &mut BudgetState, now: f64) {
    state.layers_at_floor = false;
    state.last_layer_drop_ms = now;
}

/// The cap to PIN on a [`CascadeAction::LowerLayer`] outcome (issue #1557 BLOCKER
/// fix). A LowerLayer edge drops received simulcast layers but pauses NO tile, so
/// the cap must equal the displayed (natural) tile count — clamped into
/// `[MIN_CAP, CANVAS_LIMIT]` and then bounded by the device-class decode ceiling
/// (iOS / #1286) exactly as the un-pressured sync does. This is deliberately the
/// SAME clamp as the not-pressured path and explicitly does NOT apply
/// `presenter_cap_ceiling` (that clamp sheds tiles and belongs only to the
/// PauseTiles arm).
///
/// REGRESSION GUARD: without writing this value into `decode_budget_cap` on the
/// LowerLayer edge, the render's `effective_cap` would read the stale Auto seed
/// (`MIN_CAP`) and pause N-1 of N tiles on the FIRST Down edge — the exact
/// inversion of #1557. The unit test `lower_layer_pins_cap_to_natural_no_pause`
/// pins that this returns the full natural cap (no pause) while a PauseTiles step
/// returns strictly fewer.
pub fn lower_layer_cap(natural: usize, device_ceiling: Option<usize>) -> usize {
    let cap = natural.clamp(MIN_CAP, crate::constants::CANVAS_LIMIT);
    match device_ceiling {
        Some(ceiling) => cap.min(ceiling.max(MIN_CAP)),
        None => cap,
    }
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

/// Decide whether a decode-budget override transition should discard the
/// per-tile force-decode (PLAY) requests (issue #1466 / #1471).
///
/// Returning to `Auto` from ANY non-`Auto` state (`Fixed`/`All`) means "let the
/// adaptive loop decide again", so stale PLAY requests must NOT keep peers
/// pinned-decoded across the switch. Every OTHER transition keeps them: `All`
/// and `Fixed(n)` are explicit manual modes where an existing PLAY request is
/// still meaningful, and an `Auto`→`Auto` non-transition must not clear (it
/// would let a single spurious re-render wipe the user's requests).
///
/// Pure / DOM-free / signal-free so the return-to-Auto DECISION is host-testable
/// — the `use_effect` in `attendants.rs` calls this and only clears the
/// `user_requested_decode` signal when it returns `true`. This pins the
/// *decision* — which transitions clear — that the inline `.clear()` could not:
/// `should_clear_..._tests` fails if the predicate is mutated. That decision was
/// the subtle part (#1471 named the risk of a mutation flipping which
/// transitions clear).
///
/// SCOPE (honest): the unit test does NOT cover the effect→helper *wiring* — a
/// Dioxus `use_effect` body needs a runtime — so deleting the
/// `user_requested_decode.write().clear()` call in `attendants.rs` would still
/// pass the unit test. That call is a single line gated directly by this
/// helper's result; it has no host test. An end-to-end return-to-Auto assertion
/// would need the multi-publisher budget-shed setup (see
/// `decode-budget-play-button.spec.ts`), which is not added here.
pub fn should_clear_force_decode_on_override_change(
    previous: crate::context::DecodeBudgetOverride,
    current: crate::context::DecodeBudgetOverride,
) -> bool {
    use crate::context::DecodeBudgetOverride::Auto;
    previous != Auto && current == Auto
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

/// Merge the pinned peer into the active decode set — but ONLY when it actually
/// landed in the decoded bucket this render (issue #1489).
///
/// Phase 3 of `active_decode_set` construction force-adds the pinned peer. The
/// pin-swap ([`promote_pinned_into_decoded`]) earlier in the render moves a pin
/// ranked in the displayed off-budget window `[visible_tile_count,
/// displayed_tile_count)` INWARD into a decoded slot, so a promotable pin is
/// already in `decoded_bucket` and passes this gate. But a pin in the
/// true-overflow region (`pinned_idx >= displayed_tile_count`) is deliberately
/// NOT promoted (it would evict a displayed tile off-grid — #1470), so it stays
/// in the +N badge with no decoded slot. Force-adding such a pin would decode it
/// while it renders in no grid bucket — the "decode but show nothing" waste
/// #1489 removes. Gating the insert on `decoded_bucket` membership keeps phase 3
/// in agreement with render, EXACTLY mirroring the phase-4 PLAY-path
/// [`merge_user_requested_decode`].
///
/// `pinned_session_id` is the pinned peer's resolved `session_id` (the caller
/// maps the user_id-keyed pin to a session_id via `client.get_peer_user_id`,
/// which is not host-testable, so it is passed in). Kept pure / DOM-free /
/// signal-free so the decode⇄render invariant is host-unit-testable.
pub fn merge_pinned_decode(
    active: &mut std::collections::HashSet<u64>,
    pinned_session_id: u64,
    decoded_bucket: &std::collections::HashSet<u64>,
) {
    // Only force-decode the pin when it actually got a decoded slot this render
    // (decode⇄render must agree — #1489). A true-overflow pin (#1470) has no
    // decoded slot and so must not be decoded while rendering in no grid bucket.
    if decoded_bucket.contains(&pinned_session_id) {
        active.insert(pinned_session_id);
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
/// (HCL #987 review FIX 7). A pin promoted into the decoded window is force-added
/// to `active_decode_set` (phase 3, intersected with the decoded bucket — see the
/// note below and [`merge_pinned_decode`]); without this swap an off-budget pin in
/// the displayed window would be decoded yet rendered as a "Video paused" avatar
/// — wasted decode AND a misleading UI.
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
/// POST-EXPANSION INVARIANT documented for the PLAY path. Phase 3 then
/// intersects the pin's decode admission with the decoded bucket (issue #1489 —
/// [`merge_pinned_decode`]), so a true-overflow pin that got no decoded slot here
/// is NOT decoded either: decode and render agree (neither decoded nor shown).
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

/// Presenter-aware decode-shed factor (issue #1559).
///
/// While the LOCAL user is screen-sharing, the sharer's CPU is split between the
/// heavy screen ENCODE (which since #1554 seeds the screen ladder at two rungs
/// including the 1080p `high` rung) and every concurrent WebCodecs peer-video
/// DECODE. In a large meeting (~15 peers) the decode load starves the screen
/// encoder, so the shared screen's bitrate/FPS collapses (~3x worse than a
/// 7-peer call on the same machine — the #1562 audit). Freeing CPU from peer
/// decodes is the higher-leverage lever than per-peer resolution because decode
/// cost scales with the NUMBER of concurrent decodes, and the decode budget
/// counts tiles.
///
/// This factor sets HOW HARD a presenter sheds once pressure is measured: while
/// sharing, the presenter's pressured-cap ceiling starts from
/// `ceil(natural * FACTOR)` and is then bounded ABOVE by
/// [`PRESENTER_RESIDUAL_FLOOR`] (both floored at [`MIN_CAP`] — see
/// [`presenter_cap_ceiling`]). At `0.5` the fraction keeps SMALL meetings gentle
/// (e.g. 7-peer → `ceil(6 * 0.5)` = 3), while the residual-floor `min` handles
/// LARGE meetings where a pure fraction would still leave too many concurrent
/// decodes to unstarve the encoder.
///
/// Tuned per the #1559 performance review (NOT a bare first guess): the goal is a
/// BOUNDED residual peer-decode count, targeting at-or-below the healthy peer
/// baseline the screen encoder already ran fine alongside (a ~7-peer call's ~6
/// decodes). A pure `0.5` fraction misses this for large meetings — a 15-peer
/// call (natural ≈ 14) would land at `ceil(14 * 0.5)` = 7 residual decodes, still
/// AT/ABOVE the borderline that was starving the encoder, and a 30-peer call at
/// 15 — so the absolute [`PRESENTER_RESIDUAL_FLOOR`] cap is what actually delivers
/// the fix at scale. The factor is retained for the gentle small-meeting taper.
pub const PRESENTER_SHED_FACTOR: f64 = 0.5;

/// Absolute upper bound on the number of peer tiles a PRESENTER (local user
/// screen-sharing) decodes while pressured, regardless of meeting size
/// (issue #1559).
///
/// This is the lever that actually frees the screen encoder in LARGE meetings:
/// the [`PRESENTER_SHED_FACTOR`] fraction alone does not bound the absolute
/// residual decode count (a 30-peer call at `0.5` still decodes 15 peers), so the
/// presenter ceiling is the `min` of the fraction AND this floor. With `5`, a
/// 15-peer sharer sheds to 5 concurrent peer decodes and a 30-peer sharer also to
/// 5 — at or below the healthy ~6-decode baseline the encoder ran fine alongside,
/// so it can recover its CPU. `5` = the active speaker (kept decoded inside this
/// floor by `promote_speakers`) plus a few visible peers, so the UX cost of the
/// lower floor is small: the presenter still sees the talker and a handful of
/// participants while non-speaker thumbnails are shed.
///
/// Tuned per the #1559 performance review (bounded residual decode count, target
/// at-or-below the healthy peer baseline) — NOT a bare first guess.
pub const PRESENTER_RESIDUAL_FLOOR: usize = 5;

/// The FPS step-down threshold to use this tick, biased by whether the local
/// user is screen-sharing (issue #1559).
///
/// Returns the normal [`FPS_STEP_DOWN`] (24) when NOT sharing, and the higher
/// [`FPS_STEP_UP`] (30) when sharing. Raising the step-down threshold to 30 while
/// sharing makes the controller treat the *entire* 24-30 hysteresis band as
/// pressure FOR A PRESENTER, so the budget steps down SOONER (it sheds peer
/// tiles at a milder FPS dip than it would for a non-presenter). This is the
/// "step down sooner" half of presenter-aware shedding.
///
/// It is still PRESSURE-GATED: a presenter whose FPS stays comfortably above 30
/// (`>= FPS_STEP_UP`) is never under measured pressure, so this threshold never
/// trips and a powerful device sharing in a small meeting keeps decoding all
/// peers. The threshold only changes WHICH fps level counts as pressure; it does
/// not manufacture pressure on a healthy machine.
///
/// Pure / DOM-free / signal-free so the bias is host-unit-testable; the caller
/// resolves `sharing` from `screen_share_state().is_sharing()`.
pub fn presenter_step_down_fps(sharing: bool) -> f64 {
    if sharing {
        FPS_STEP_UP
    } else {
        FPS_STEP_DOWN
    }
}

/// True when a PRESENTER (local user screen-sharing) is under *measured* decode
/// pressure that warrants extra peer-tile shedding, evaluated over the sustain
/// window (issue #1559).
///
/// This is the presenter "step down sooner" trigger. It fires when ALL of:
///
/// - `sharing` is true (the local user is screen-sharing — otherwise this is
///   never a presenter and the function returns `false`), AND
/// - the median render FPS over the sustain window is BELOW the presenter
///   step-down threshold ([`presenter_step_down_fps`] = [`FPS_STEP_UP`] = 30 while
///   sharing), i.e. the presenter is in or below the 24-30 band that the normal
///   (`< FPS_STEP_DOWN` = 24) trigger would NOT yet treat as pressure.
///
/// It is the COMPLEMENT-aware companion to [`decide_step`]'s own step-down
/// trigger: the normal trigger (`median < FPS_STEP_DOWN` OR sustained longtask)
/// still fires independently; this ADDS the milder 24-30 band as a presenter-only
/// down-trigger. The caller composes the two with OR (so the normal path is never
/// weakened) and only ACTS on the result while `sharing` — when sharing stops the
/// function returns `false` and the controller reverts to the normal trigger.
///
/// Crucially it is PRESSURE-GATED: a presenter at a healthy `>= 30` fps does NOT
/// satisfy `median < presenter_step_down_fps(true)`, so a powerful device sharing
/// in a small meeting is never shed by this path.
///
/// Returns `false` for a short/incomplete window (mirroring [`decide_step`]'s
/// conservative handling) or any missing `render_fps` in the window.
///
/// Pure / DOM-free / signal-free so the trigger is host-unit-testable.
pub fn presenter_extra_shed_pressure(samples: &[BudgetSample], sharing: bool) -> bool {
    if !sharing {
        return false;
    }
    median_render_fps(samples, SUSTAIN_SAMPLES)
        .map(|m| m < presenter_step_down_fps(true))
        .unwrap_or(false)
}

/// The presenter-aware *pressured cap ceiling*: an optional hard upper bound on
/// the loop-owned decode cap that binds ONLY while the local user is
/// screen-sharing AND the budget is already in its pressured state (issue #1559).
///
/// While `sharing` is true, returns
/// `Some( min( ceil(natural * PRESENTER_SHED_FACTOR), PRESENTER_RESIDUAL_FLOOR )
/// .max(MIN_CAP) )` — the "lower floor" half of presenter-aware shedding. The
/// `min` is what makes this effective at SCALE: the fraction tapers small
/// meetings gently, while [`PRESENTER_RESIDUAL_FLOOR`] hard-bounds the absolute
/// residual peer-decode count so a 15- or 30-peer sharer is shed to the same
/// small number (≈ the healthy baseline the encoder ran fine alongside), not a
/// large fraction of a large meeting. Clamping the loop-owned cap to this ceiling
/// frees peer-decode CPU for the screen encoder by shedding the lowest-priority
/// (non-speaker / off-screen) peer thumbnails first; the active-speaker exemption
/// is preserved by the caller's `promote_speakers`, which runs against the
/// resulting (lower) `visible_tile_count` and swaps active speakers INTO the
/// decoded window before the visible/avatar split, displacing only NON-speaking
/// visible tiles — so the talker stays decoded inside the residual floor.
///
/// ## Worked sizes (with FACTOR = 0.5, RESIDUAL_FLOOR = 5)
///
/// - 6-peer meeting → `min(ceil(6 * 0.5)=3, 5)` = 3 (the fraction wins; gentle).
/// - 14-peer meeting → `min(ceil(14 * 0.5)=7, 5)` = 5 (the floor wins; the fix).
/// - 30-peer meeting → `min(ceil(30 * 0.5)=15, 5)` = 5 (the floor wins; bounded).
///
/// While `sharing` is false, returns `None` (no presenter ceiling) so the cap
/// recovers to its normal behaviour and re-grows via the existing non-distress
/// growth path. No leaked state: the ceiling is recomputed from the live
/// `sharing` flag every tick.
///
/// ## Pressure-gating is the CALLER's responsibility
///
/// This helper does not itself inspect the pressure signals — the CALLER applies
/// it only on the pressured-cap path (after `decode_budget_pressured` has
/// latched, or on the latch edge), so a presenter whose device is NOT pressured
/// never has this ceiling bind: a powerful machine sharing in a small meeting
/// keeps decoding all peers. The factor only governs HOW HARD an already-pressured
/// presenter sheds.
///
/// Pure / DOM-free / signal-free so the ceiling is host-unit-testable; the caller
/// resolves `sharing` from `screen_share_state().is_sharing()`.
pub fn presenter_cap_ceiling(natural: usize, sharing: bool) -> Option<usize> {
    if !sharing {
        return None;
    }
    // Fractional taper for small meetings, hard-bounded by the absolute residual
    // floor for large meetings (the latter is what actually unstarves the encoder
    // at scale — see PRESENTER_RESIDUAL_FLOOR). Floored at MIN_CAP so a presenter
    // always decodes at least one tile (the active speaker). `MIN_CAP` (1) <=
    // `PRESENTER_RESIDUAL_FLOOR` (5) — both consts — so the clamp bounds are
    // well-ordered and `clamp` cannot panic; it is exactly
    // `fraction.min(RESIDUAL_FLOOR).max(MIN_CAP)`.
    let fraction = (natural as f64 * PRESENTER_SHED_FACTOR).ceil() as usize;
    Some(fraction.clamp(MIN_CAP, PRESENTER_RESIDUAL_FLOOR))
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

// ─────────────────────────────────────────────────────────────────────────────
// Issue #1558: protective mode (audio-first, speaker-priority degradation).
//
// This is a THIN control layer that COMPOSES the existing #1557 cascade
// (`cascade_action` → layers-then-pause) with two new emergency stages, gated by
// a single broader "client in distress" predicate and a latched, hysteretic
// protective-mode flag. It deliberately reuses every existing primitive:
//
//   - Detection (item 1): a broader predicate than `decide_step`'s FPS/longtask
//     down-trigger — adds audio-buffer growth, a low-capability + crowded-meeting
//     combination, and (when wired) a sustained NetEQ accelerate rate. It reuses
//     `median_render_fps` for the FPS sub-signal so its definition cannot drift.
//   - Latch (item 2): `tick_protective_mode` mirrors the budget loop's asymmetric
//     hysteresis (a short ON sustain window, a longer OFF recovery window) — it
//     does NOT add a second control loop; the caller drives it from the SAME 1 Hz
//     budget tick.
//   - Stages 1-2 (cascade layers→pause): UNCHANGED — `cascade_action` already
//     lowers received layers then pauses non-speaker tiles, with the active
//     speaker exempt and audio never paused. Protective mode does not reimplement
//     these; it sits ON TOP and only adds stages 3-4.
//   - Stage 3 (encoder self-shed, item 5): `protective_encoder_layer_ceiling`
//     computes the LOCAL encoder send-layer ceiling (3→2→1) to free CPU for
//     decode. The actuator is the EXISTING per-encoder `set_user_layer_ceiling`
//     (the AQ `user_layer_ceiling_cap`, a top-side `min` that never fights the
//     encoder's own backpressure controller); the caller composes it with the
//     user's persisted ceiling via `min` so neither clobbers the other.
//   - Stage 4 (EMERGENCY non-speaker pause, item 5): `protective_emergency_cap`
//     forces the decode cap to the floor (speaker only) when audio is STILL
//     growing past threshold after stages 1-3. The active speaker is preserved by
//     the caller's existing `promote_speakers`, exactly as the cascade's
//     PauseTiles arm already is.
//
// AUDIO IS NEVER DEGRADED (item 3): no stage touches the audio decode path; the
// emergency stage sheds VIDEO decode precisely to protect audio. The active
// speaker's video is the LAST thing degraded (item 4): it is exempt from
// layer-drop and pause (the cascade exemption) and from the emergency cap (which
// floors at MIN_CAP = the speaker tile) and the encoder self-shed never touches a
// REMOTE speaker (it only caps the LOCAL send ladder).
//
// All thresholds are FIRST-GUESS and marked tunable, mirroring the #987/#1557
// convention.
// ─────────────────────────────────────────────────────────────────────────────

/// Median render FPS at or below which the local client is considered *in
/// distress* for protective mode. Deliberately LOWER than [`FPS_STEP_DOWN`] (24):
/// the decode-budget cascade already reacts to the 24-band; protective mode is a
/// stronger, audio-protecting response reserved for a genuinely collapsing
/// renderer (15 fps is visibly stuttering, not merely throttled).
///
/// First-guess value — tunable, pending a performance-reviewer pass.
pub const PROTECTIVE_FPS_DISTRESS: f64 = 15.0;

/// Sustained long-task time per second at or above which the main thread is
/// considered *in distress* for protective mode. The issue frames this as
/// "longtask > ~10 in 5s"; expressed as a per-second rate over the sustain
/// window this is a heavy-jank threshold well above [`LONGTASK_BUSY_MS_PER_SEC`]
/// (250) — the renderer is spending most of each second blocked.
///
/// First-guess value — tunable, pending a performance-reviewer pass.
pub const PROTECTIVE_LONGTASK_DISTRESS_MS_PER_SEC: f64 = 500.0;

/// Per-peer audio-buffer depth (ms) above which the client is considered *in
/// distress*: the audio jitter buffer is backing up because decode/main-thread
/// pressure is starving audio playout. This is the PRIMARY audio-protection
/// trigger — when any peer's `audio_buffer_ms` exceeds this, protective mode must
/// shed video work to give audio CPU back.
///
/// First-guess value (the issue's "> 500ms") — tunable.
pub const PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS: f64 = 500.0;

/// Per-peer audio-buffer depth (ms) above which the EMERGENCY stage (stage 4)
/// fires: audio is STILL growing past a higher water mark even after stages 1-3
/// (layers → pause → encoder self-shed). At this point ALL non-speaker video
/// decode is paused to protect audio. Strictly above
/// [`PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS`] so the emergency is a distinct, worse
/// condition than mere entry-level distress (hysteresis between stages).
///
/// First-guess value — tunable.
pub const PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS: f64 = 800.0;

/// Sustained NetEQ accelerate operations per second above which audio is being
/// time-compressed to keep up — a sign of accumulating audio lag (issue #1299
/// class). The issue's "> ~10/s sustained".
///
/// NOTE (deferred sub-signal): `accelerate_per_sec` is NOT currently broadcast on
/// the `videocall_diagnostics` bus the budget loop subscribes to — it lives only
/// inside the NetEQ worker stats JSON consumed by `health_reporter.rs`. The
/// predicate therefore accepts it as an `Option<f64>` and the caller threads
/// `None` until a bus emit is added; the threshold is defined here so the wiring
/// is a one-line change when the signal lands. See the report's "deferred signal"
/// note.
///
/// First-guess value — tunable.
pub const PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC: f64 = 10.0;

/// Capability-benchmark score below which the device is considered *low-cap*. The
/// score is the opaque iteration count from
/// [`videocall_client::capability::videocall_capability_score`] (higher = faster
/// device). Combined with [`PROTECTIVE_PARTICIPANT_COUNT_DISTRESS`]: a slow device
/// in a small meeting is fine, but a slow device in a CROWDED meeting is a
/// structural distress condition (it will never keep up), so protective mode
/// engages proactively.
///
/// First-guess value (the issue's "< 3000") — tunable; the score is device- and
/// build-relative, so this MUST be re-tuned against field data.
pub const PROTECTIVE_CAP_SCORE_DISTRESS: u32 = 3000;

/// Participant count above which a low-capability device
/// ([`PROTECTIVE_CAP_SCORE_DISTRESS`]) is considered structurally distressed. The
/// count is OTHER peers (not including self), matching
/// [`videocall_client::VideoCallClient::peer_count`].
///
/// First-guess value (the issue's "> 8") — tunable.
pub const PROTECTIVE_PARTICIPANT_COUNT_DISTRESS: usize = 8;

/// Consecutive ~1 Hz samples of sustained distress required before protective
/// mode LATCHES ON. Short (a few seconds) so audio protection lands promptly once
/// distress is real, but > 1 so a single bad sample (a GC pause) never flips it.
///
/// First-guess value — tunable. Mirrors [`SUSTAIN_SAMPLES`] in spirit.
pub const PROTECTIVE_ENTER_SUSTAIN: u32 = 3;

/// Consecutive ~1 Hz samples of sustained *clear* (no distress) required before
/// protective mode LATCHES OFF. Deliberately LONGER than
/// [`PROTECTIVE_ENTER_SUSTAIN`] — asymmetric hysteresis, exactly like the budget
/// loop's shorter down-cooldown vs longer up-cooldown: relief (entering
/// protection) lands fast, recovery (leaving it) is conservative so the client
/// does not thrash in and out of protective mode on a flapping machine.
///
/// First-guess value — tunable.
pub const PROTECTIVE_EXIT_RECOVERY: u32 = 8;

/// The aggregated per-tick distress signals for [`in_distress`]. Kept as a plain
/// struct of already-resolved scalars so the predicate is pure / host-testable —
/// the caller resolves each field from its live source (the budget-loop sample
/// window, `client.peer_count()`, the cached capability score, the per-peer
/// audio-buffer max observed on the diagnostics bus) before calling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DistressSignals {
    /// Median render FPS over the sustain window, if measurable (`None` ⇒ the
    /// FPS sub-trigger cannot fire — absence is never distress).
    pub median_fps: Option<f64>,
    /// Sustained long-task time per second over the window, if the Long Tasks
    /// API is available (`None` on WebKit/iOS ⇒ the longtask sub-trigger cannot
    /// fire, mirroring `decide_step`'s conservative None handling).
    pub longtask_ms_per_sec: Option<f64>,
    /// The MAXIMUM `audio_buffer_ms` observed across all peers this window, if
    /// any audio-buffer reading was seen (`None` ⇒ no reading ⇒ no trigger).
    pub max_peer_audio_buffer_ms: Option<f64>,
    /// Sustained NetEQ accelerate operations per second, if available (`None`
    /// today — deferred bus signal; see
    /// [`PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC`]).
    pub neteq_accelerate_per_sec: Option<f64>,
    /// The device capability-benchmark score, if known (`None` ⇒ the
    /// low-cap+crowded sub-trigger cannot fire).
    pub cap_score: Option<u32>,
    /// Number of OTHER peers in the meeting (not including self).
    pub participant_count: usize,
}

impl DistressSignals {
    /// A fully-clear signal set (no distress on any axis). The control loop builds
    /// `DistressSignals` directly from live readings, so this constructor is used
    /// only as the truth-table baseline in tests; gated accordingly to avoid a
    /// dead-code warning in the wasm build.
    #[cfg(test)]
    pub fn clear() -> Self {
        DistressSignals {
            median_fps: Some(60.0),
            longtask_ms_per_sec: Some(0.0),
            max_peer_audio_buffer_ms: Some(0.0),
            neteq_accelerate_per_sec: Some(0.0),
            cap_score: Some(u32::MAX),
            participant_count: 0,
        }
    }
}

/// The single, pure "client in distress" predicate (issue #1558 item 1).
///
/// Returns `true` when ANY of the following independent triggers holds:
///
/// 1. **Collapsed renderer** — `median_fps < `[`PROTECTIVE_FPS_DISTRESS`].
/// 2. **Saturated main thread** — `longtask_ms_per_sec >= `
///    [`PROTECTIVE_LONGTASK_DISTRESS_MS_PER_SEC`].
/// 3. **Audio backing up** — `max_peer_audio_buffer_ms > `
///    [`PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS`] (the primary audio-protection
///    trigger).
/// 4. **Audio time-compressing** — `neteq_accelerate_per_sec >= `
///    [`PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC`] (deferred; `None` today).
/// 5. **Low-cap + crowded** — `cap_score < `[`PROTECTIVE_CAP_SCORE_DISTRESS`]
///    AND `participant_count > `[`PROTECTIVE_PARTICIPANT_COUNT_DISTRESS`].
///
/// Every `Option` sub-signal is conservative: a `None` (signal unavailable /
/// unmeasured) can NEVER manufacture distress — absence of evidence is not
/// evidence of distress. This mirrors `decide_step`'s `None`-longtask handling
/// and keeps protective mode from flapping on a browser where a given signal is
/// simply unobtainable.
///
/// Pure / DOM-free / signal-free so the predicate is host-unit-testable.
pub fn in_distress(s: DistressSignals) -> bool {
    let fps_collapsed = s
        .median_fps
        .map(|m| m < PROTECTIVE_FPS_DISTRESS)
        .unwrap_or(false);
    let main_thread_saturated = s
        .longtask_ms_per_sec
        .map(|lt| lt >= PROTECTIVE_LONGTASK_DISTRESS_MS_PER_SEC)
        .unwrap_or(false);
    let audio_backing_up = s
        .max_peer_audio_buffer_ms
        .map(|buf| buf > PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS)
        .unwrap_or(false);
    let audio_compressing = s
        .neteq_accelerate_per_sec
        .map(|acc| acc >= PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC)
        .unwrap_or(false);
    let low_cap_and_crowded = s
        .cap_score
        .map(|score| {
            score < PROTECTIVE_CAP_SCORE_DISTRESS
                && s.participant_count > PROTECTIVE_PARTICIPANT_COUNT_DISTRESS
        })
        .unwrap_or(false);

    fps_collapsed
        || main_thread_saturated
        || audio_backing_up
        || audio_compressing
        || low_cap_and_crowded
}

/// Latched protective-mode state, owned by the caller across ~1 Hz ticks (issue
/// #1558 item 2). Mirrors the asymmetric hysteresis of the budget loop: an `enter`
/// streak and an `exit` streak, with `active` flipped only when the relevant
/// streak crosses its (asymmetric) sustain threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProtectiveModeState {
    /// True while protective mode is engaged.
    pub active: bool,
    /// Consecutive distress samples observed while NOT yet active (drives ON).
    pub enter_streak: u32,
    /// Consecutive clear samples observed while active (drives OFF).
    pub exit_streak: u32,
}

/// The transition produced by one [`tick_protective_mode`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectiveTransition {
    /// No change to `active` this tick.
    None,
    /// Protective mode just turned ON (the metric/log entry edge).
    Entered,
    /// Protective mode just turned OFF (the metric/log exit edge).
    Exited,
}

/// Advance the protective-mode latch by one ~1 Hz tick (issue #1558 item 2).
///
/// `distressed` is this tick's [`in_distress`] result. The latch is asymmetric:
///
/// - While NOT active: a distress sample grows `enter_streak`; a clear sample
///   resets it to 0. Once `enter_streak` reaches [`PROTECTIVE_ENTER_SUSTAIN`],
///   protective mode latches ON and returns [`ProtectiveTransition::Entered`].
/// - While active: a clear sample grows `exit_streak`; a distress sample resets
///   it to 0. Once `exit_streak` reaches [`PROTECTIVE_EXIT_RECOVERY`] (a LONGER
///   window), protective mode latches OFF and returns
///   [`ProtectiveTransition::Exited`].
///
/// A single bad sample can never flip the latch (both thresholds are > 1), and a
/// single GOOD sample can never drop it (the exit streak must accumulate the full
/// recovery window). The two streaks are kept separate so leaving one state
/// always re-arms the other from 0.
///
/// Pure / DOM-free / signal-free so the latch is host-unit-testable; the caller
/// owns the [`ProtectiveModeState`] across ticks and reacts to the returned
/// transition (emit the metric/log, apply/clear the encoder ceiling).
pub fn tick_protective_mode(
    state: &mut ProtectiveModeState,
    distressed: bool,
) -> ProtectiveTransition {
    if !state.active {
        if distressed {
            state.enter_streak = state.enter_streak.saturating_add(1);
        } else {
            state.enter_streak = 0;
        }
        if state.enter_streak >= PROTECTIVE_ENTER_SUSTAIN {
            state.active = true;
            state.enter_streak = 0;
            state.exit_streak = 0;
            return ProtectiveTransition::Entered;
        }
        ProtectiveTransition::None
    } else {
        if distressed {
            state.exit_streak = 0;
        } else {
            state.exit_streak = state.exit_streak.saturating_add(1);
        }
        if state.exit_streak >= PROTECTIVE_EXIT_RECOVERY {
            state.active = false;
            state.enter_streak = 0;
            state.exit_streak = 0;
            return ProtectiveTransition::Exited;
        }
        ProtectiveTransition::None
    }
}

/// The LOCAL encoder send-layer ceiling protective mode requests this tick — the
/// step-3 "encoder self-shed" (issue #1558 item 5). Dropping the local encoder's
/// active layers (3→2→1) frees encode CPU so the decode pipeline (and audio) can
/// keep up; the active speaker's video is unaffected because this caps only the
/// LOCAL send ladder, never a remote decode.
///
/// Returns:
/// - `None` when protective mode is NOT active OR has not yet reached the
///   encoder-shed stage — the encoder runs its normal (user/auto) ceiling.
/// - `Some(ceiling)` (a layer COUNT, floored at 1 so the base layer is always
///   published) when protective mode is active AND stages 1-2 have reached their
///   floor (`cascade_at_floor`) — i.e. received layers are at base and tiles are
///   already being paused, so the next lever is to shed the LOCAL send ladder.
///
/// `cascade_at_floor` is the caller's `state.layers_at_floor` from the #1557
/// cascade: the encoder self-shed is gated behind the cascade reaching floor+pause
/// so the levers fire in order (received layers → pause → encoder shed), exactly
/// as the issue's progressive sequence requires. While active and at floor the
/// ceiling steps DOWN as audio pressure persists, governed by `severity`:
///
/// - `severity == 0` ⇒ `Some(2)` (drop the top of a 3-layer ladder).
/// - `severity >= 1` ⇒ `Some(1)` (base-only — maximum CPU relief).
///
/// The ceiling NEVER drops below 1 (the base layer always publishes, so a peer is
/// never left with no video of the local user). The actuator (`set_user_layer_ceiling`)
/// composes this with the user's persisted ceiling via `min`, and the encoder's
/// AQ applies it as a top-side `min` that cannot fight its own backpressure
/// controller — so this never competes with the encoder's own congestion control.
///
/// Pure / DOM-free / signal-free so the lever is host-unit-testable.
pub fn protective_encoder_layer_ceiling(
    active: bool,
    cascade_at_floor: bool,
    severity: u32,
) -> Option<u32> {
    if !active || !cascade_at_floor {
        return None;
    }
    if severity >= 1 {
        Some(1)
    } else {
        Some(2)
    }
}

/// The EMERGENCY decode cap protective mode forces this tick — the step-4
/// "pause ALL non-speaker video decode" lever (issue #1558 item 5).
///
/// Returns `Some(MIN_CAP)` ONLY when protective mode is active AND audio is STILL
/// growing past [`PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS`] after stages 1-3 — i.e.
/// every cheaper lever (received layers, tile pause, encoder self-shed) has fired
/// and audio is still losing. Flooring the cap at [`MIN_CAP`] leaves exactly ONE
/// decoded tile, which the caller's `promote_speakers` fills with the active
/// speaker — so the speaker's video survives the emergency and every other
/// non-speaker tile is paused to protect audio.
///
/// Returns `None` (no emergency clamp) otherwise, so the cap recovers via the
/// normal cascade/growth path once audio drains — the stage reverses on recovery.
///
/// `max_peer_audio_buffer_ms` is the caller's observed per-peer audio-buffer max
/// (the SAME signal `in_distress` uses); passing it explicitly keeps this pure.
///
/// Pure / DOM-free / signal-free so the lever is host-unit-testable.
pub fn protective_emergency_cap(
    active: bool,
    max_peer_audio_buffer_ms: Option<f64>,
) -> Option<usize> {
    if !active {
        return None;
    }
    let still_growing = max_peer_audio_buffer_ms
        .map(|buf| buf > PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS)
        .unwrap_or(false);
    if still_growing {
        Some(MIN_CAP)
    } else {
        None
    }
}

/// Compose the protective-mode encoder ceiling with the user's persisted "layers
/// published" preference into the EFFECTIVE ceiling to apply via the encoder's
/// `set_user_layer_ceiling` (issue #1558 item 5, stage 3 actuation).
///
/// Both inputs are layer COUNTS in the `Option<u32>` convention used by the
/// encoder (`None` = no cap / Auto / full ladder):
///
/// - `user_pref`: the user's chosen send-layer ceiling (perf panel), or `None`.
/// - `protective`: protective mode's requested shed ceiling
///   ([`protective_encoder_layer_ceiling`]), or `None` when not shedding.
///
/// The result is the more restrictive of the two — `min` when both are `Some`,
/// the present one when exactly one is `Some`, and `None` when both are `None`.
/// This guarantees neither clobbers the other: protective mode can only LOWER the
/// effective ceiling below the user's choice, and when protective mode releases
/// (`protective == None`) the result reverts to the user's preference alone (full
/// reversibility). Each `Some` is already `>= 1` from its producer; the
/// encoder/AQ floors the active count at 1 regardless.
///
/// Pure / DOM-free / signal-free so the composition is host-unit-testable.
pub fn compose_encoder_ceiling(user_pref: Option<u32>, protective: Option<u32>) -> Option<u32> {
    match (user_pref, protective) {
        (Some(u), Some(p)) => Some(u.min(p)),
        (Some(u), None) => Some(u),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    }
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
            last_layer_drop_ms: 0.0,
            layers_at_floor: false,
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

    // ---- Issue #1557: cascade_action (layer-drop before tile-pause) ----

    #[test]
    fn cascade_layer_drop_precedes_pause() {
        // Under Down pressure, while received layers are NOT yet at floor, the
        // cascade always lowers layers first — REGARDLESS of the settle window.
        // A tile cannot be paused while there is still a layer to drop.
        assert_eq!(
            cascade_action(true, false, false),
            CascadeAction::LowerLayer,
            "down pressure + layers not at floor (no settle) must lower a layer"
        );
        assert_eq!(
            cascade_action(true, false, true),
            CascadeAction::LowerLayer,
            "down pressure + layers not at floor must lower a layer even after settle"
        );
        // MUTATION: deleting the `if !layers_at_floor` guard would let the
        // (true, false, true) case fall through to PauseTiles — this fails.
    }

    #[test]
    fn cascade_pause_only_after_floor_and_settle() {
        // Once layers are at floor, pausing tiles is gated on the settle window:
        // not elapsed → keep cascading on layers (a harmless idempotent nudge);
        // elapsed → escalate to pausing a tile.
        assert_eq!(
            cascade_action(true, true, false),
            CascadeAction::LowerLayer,
            "at floor but settle window not elapsed must NOT pause a tile yet"
        );
        assert_eq!(
            cascade_action(true, true, true),
            CascadeAction::PauseTiles,
            "at floor AND settle elapsed escalates to pausing a tile"
        );
        // MUTATION: inverting or removing the `if settle_elapsed` branch flips
        // one of these two outcomes — this fails.
    }

    #[test]
    fn cascade_no_pressure_is_none() {
        // Without a Down edge there is no cascade at all — neither layer-drop
        // nor tile-pause, irrespective of floor/settle facts.
        assert_eq!(
            cascade_action(false, true, true),
            CascadeAction::None,
            "no down pressure must be None even at floor + settled"
        );
        assert_eq!(
            cascade_action(false, false, false),
            CascadeAction::None,
            "no down pressure must be None regardless of facts"
        );
        // MUTATION: removing the `if !down_pressure` early return would make the
        // first case PauseTiles and the second LowerLayer — this fails.
    }

    #[test]
    fn lower_layer_pins_cap_to_natural_no_pause() {
        // #1557 BLOCKER guard: on a LowerLayer outcome the cap MUST equal the full
        // displayed (natural) tile count — NO tile paused — while a PauseTiles step
        // returns strictly fewer. This pins the cap-write wiring fix: if a future
        // regression drops the cap on the LowerLayer arm (the original bug, where
        // the stale `MIN_CAP` Auto seed paused N-1 of N tiles on the first Down
        // edge), `lower_layer_cap` would no longer return `natural` and this fails.
        let natural = 6usize;
        let lower = lower_layer_cap(natural, None);
        assert_eq!(
            lower, natural,
            "LowerLayer must keep the full natural cap (no tile paused)"
        );
        // A single-tile PauseTiles step on the same natural sheds at least one tile;
        // the LowerLayer cap must be strictly larger (it pauses nothing).
        let paused_cap = natural.saturating_sub(1).max(MIN_CAP);
        assert!(
            lower > paused_cap,
            "LowerLayer cap ({lower}) must exceed a PauseTiles step cap ({paused_cap}) — \
             LowerLayer pauses no tile"
        );
        // Natural is clamped into [MIN_CAP, CANVAS_LIMIT]; a 0-peer layout floors at
        // MIN_CAP (the local participant always decodes at least one tile).
        assert_eq!(
            lower_layer_cap(0, None),
            MIN_CAP,
            "a 0-peer natural floors at MIN_CAP, never below"
        );
        // The device-class decode ceiling (iOS / #1286) clamps the LowerLayer cap
        // exactly as the un-pressured sync does. MUTATION: dropping the ceiling
        // clamp would return `natural` (6) here instead of the ceiling (3).
        assert_eq!(
            lower_layer_cap(natural, Some(3)),
            3,
            "device decode ceiling must clamp the LowerLayer cap"
        );
        // The ceiling itself never drops below MIN_CAP.
        assert_eq!(
            lower_layer_cap(natural, Some(0)),
            MIN_CAP,
            "a 0 device ceiling still floors at MIN_CAP"
        );
    }

    #[test]
    fn cascade_recovery_is_out_of_band_never_raises() {
        // Recovery (un-pausing tiles and re-growing received layers) is NOT
        // routed through `cascade_action`: tile un-pause is the control loop's
        // existing Up arm (cap RAISE), and received-layer re-grow happens via
        // the choosers' clean-window recovery on the monitor tick
        // (`layer_chooser.rs::choose`). The recovery rate-limit regression guard
        // is the existing test `stepped_down_machine_does_not_regrow_on_single_good_sample`.
        //
        // This test pins that out-of-band-ness structurally: across the FULL 8-row
        // truth table, `cascade_action` only ever yields None / LowerLayer /
        // PauseTiles — there is NO "raise" variant. The exhaustive match means
        // that if a `CascadeAction::Raise` were ever added without re-routing
        // recovery through here, this test would fail to COMPILE (non-exhaustive
        // match), forcing the author to revisit the design.
        let table = [
            (false, false, false),
            (false, false, true),
            (false, true, false),
            (false, true, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
            (true, true, true),
        ];
        for (down_pressure, layers_at_floor, settle_elapsed) in table {
            let action = cascade_action(down_pressure, layers_at_floor, settle_elapsed);
            // Exhaustive match: a future "raise" variant cannot be silently
            // ignored — it would break compilation here.
            match action {
                CascadeAction::None | CascadeAction::LowerLayer | CascadeAction::PauseTiles => {}
            }
        }
    }

    #[test]
    fn settle_clock_freezes_at_floor_so_pause_becomes_reachable() {
        // REGRESSION (#1557 blocker): once received layers are at floor, the
        // per-tick `apply_local_cpu_pressure_congestion()` is a no-op
        // (`stepped == false`). If the settle clock were advanced on those no-op
        // ticks, the `now - last_layer_drop_ms` delta would be pinned at one
        // tick-gap forever and `settle_window_elapsed` would NEVER fire — so the
        // cascade could never escalate from LowerLayer to PauseTiles under steady
        // ~1 Hz pressure. `next_layer_drop_ms` freezes the timestamp at floor so
        // the window accumulates.
        //
        // Simulate the steady ~1 Hz Down loop AT FLOOR (every tick stepped=false).
        let tick = 1000.0; // ~1 Hz telemetry cadence
                           // The floor was first reached at this timestamp; the clock starts here.
        let mut last_layer_drop_ms = 5000.0_f64;
        let mut now = last_layer_drop_ms;

        // Tick at the floor moment itself: not yet elapsed.
        assert!(
            !settle_window_elapsed(now, last_layer_drop_ms),
            "settle cannot be elapsed at the instant the floor is reached"
        );

        // Advance the loop tick-by-tick with stepped=false (at floor). With the
        // freeze, the timestamp must NOT move, so the window keeps growing.
        let mut paused = false;
        for _ in 0..5 {
            now += tick;
            // The loop calls this every Down tick to (not) advance the clock.
            last_layer_drop_ms = next_layer_drop_ms(last_layer_drop_ms, now, false);
            // Frozen: the timestamp stays at the moment the floor was reached.
            assert_eq!(
                last_layer_drop_ms, 5000.0,
                "at floor (stepped=false) the settle clock must stay frozen"
            );
            if settle_window_elapsed(now, last_layer_drop_ms) {
                paused = true;
            }
        }
        // STEP_DOWN_COOLDOWN_MS (2000ms) is 2 ticks; PauseTiles is reachable.
        assert!(
            paused,
            "after the settle window elapses at floor the cascade reaches PauseTiles"
        );

        // MUTATION CHECK: make `next_layer_drop_ms` return `now` unconditionally
        // (the original blocker) and `last_layer_drop_ms` tracks `now` each tick,
        // so `now - last_layer_drop_ms == 0` forever — `paused` stays false and
        // this test fails.
    }

    #[test]
    fn settle_clock_advances_on_a_real_layer_drop() {
        // Counterpart: while layers are STILL dropping (stepped=true) the clock
        // tracks `now`, so the settle window restarts on each real drop — the
        // cascade keeps lowering layers (not pausing) until the floor is reached.
        let mut last_layer_drop_ms = 5000.0_f64;
        let now = 5000.0 + 3.0 * STEP_DOWN_COOLDOWN_MS; // well past the window
        last_layer_drop_ms = next_layer_drop_ms(last_layer_drop_ms, now, true);
        assert_eq!(
            last_layer_drop_ms, now,
            "a real layer drop (stepped=true) advances the settle clock to now"
        );
        assert!(
            !settle_window_elapsed(now, last_layer_drop_ms),
            "right after a real drop the settle window has NOT elapsed yet"
        );
        // MUTATION CHECK: freeze unconditionally (return prev) and the first
        // assert fails — a real drop would not advance the clock.
    }

    #[test]
    fn cascade_re_arms_after_recovery_so_next_down_lowers_layers_first() {
        // REGRESSION (#1557 blocker): `state.layers_at_floor` must be cleared on
        // recovery, else the SECOND Down edge after any recovery pauses a tile
        // BEFORE re-dropping the re-grown received layers — inverting the feature
        // on every pressure cycle after the first.
        //
        // The attendants cascade arms (Up / Hold / Site A / Site B) live in
        // attendants.rs and are NOT reachable from this decode_budget unit harness
        // (which models `decide_step` on a `BudgetState`). So this test models the
        // recovery transition by calling `re_arm_cascade_after_recovery` — the SAME
        // pure function the `BudgetStep::Up` arm and the Hold-arm non-distress
        // growth step call. Gutting that function (removing the `layers_at_floor`
        // reset) turns this test RED, so the assertion is coupled to the real
        // source of truth, not to an inline copy.
        let mut state = state_with_cap(5);

        // --- End of episode 1: received layers at floor, settle window elapsed.
        // This is the steady at-floor state the loop reaches after dropping every
        // received layer under sustained Down pressure. A tile-pause is CORRECT
        // here (this is the whole point of the cascade's second stage).
        state.layers_at_floor = true;
        state.last_layer_drop_ms = 0.0; // floor reached long ago
        let pause_now = 10_000.0;
        assert!(
            settle_window_elapsed(pause_now, state.last_layer_drop_ms),
            "by end of episode 1 the settle window has long elapsed"
        );
        assert_eq!(
            cascade_action(
                true,
                state.layers_at_floor,
                settle_window_elapsed(pause_now, state.last_layer_drop_ms),
            ),
            CascadeAction::PauseTiles,
            "at the end of episode 1 (at floor + settled) the cascade pauses a tile"
        );

        // --- Recovery: the Up arm (or Hold-growth) raises the cap and RE-ARMS the
        // cascade. Model that exact transition via the shared source of truth.
        let recovery_now = 10_000.0;
        re_arm_cascade_after_recovery(&mut state, recovery_now);
        assert!(
            !state.layers_at_floor,
            "recovery must clear layers_at_floor so the next Down re-drops layers first"
        );
        assert_eq!(
            state.last_layer_drop_ms, recovery_now,
            "recovery re-anchors the settle clock to now"
        );

        // --- Episode 2, first Down edge, 1s after recovery (< STEP_DOWN_COOLDOWN_MS
        // = 2000ms, so the settle window is NOT yet elapsed). The cascade MUST lower
        // a received layer first, NOT pause a tile. Without the Up-arm reset,
        // layers_at_floor would still be true AND last_layer_drop_ms still 0.0, so
        // settle_window_elapsed(11_000.0, 0.0) == true and the cascade would route
        // to PauseTiles — the bug.
        let down_now = recovery_now + 1_000.0;
        assert!(
            !settle_window_elapsed(down_now, state.last_layer_drop_ms),
            "1s after the re-armed drop the settle window has NOT elapsed"
        );
        assert_eq!(
            cascade_action(
                true,
                state.layers_at_floor,
                settle_window_elapsed(down_now, state.last_layer_drop_ms),
            ),
            CascadeAction::LowerLayer,
            "the first Down after recovery must lower a received layer, not pause a tile"
        );

        // --- Clock-independent pin: even if the settle window WERE already elapsed,
        // clearing layers_at_floor alone routes the first post-recovery Down to
        // LowerLayer. This isolates "clearing layers_at_floor is what saves us" from
        // the settle-clock re-anchor, so the test fails specifically if the
        // `layers_at_floor = false` line is removed from
        // `re_arm_cascade_after_recovery` (even if last_layer_drop_ms were left set).
        assert_eq!(
            cascade_action(true, state.layers_at_floor, true),
            CascadeAction::LowerLayer,
            "clearing layers_at_floor forces LowerLayer on the first post-recovery Down \
             regardless of the settle clock"
        );

        // MUTATION CHECK: delete `state.layers_at_floor = false;` from
        // `re_arm_cascade_after_recovery` and the two post-recovery LowerLayer
        // assertions flip to PauseTiles — this test fails.
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

    #[test]
    fn non_distress_growth_allowed_vetoed_by_emergency() {
        // All three pre-existing growth conditions satisfied → growth allowed only
        // while NOT in an emergency. This pins the `!emergency_now` veto directly:
        // if the clause is removed from `non_distress_growth_allowed`, the
        // emergency case below returns `true` and this assertion fails.
        assert!(
            non_distress_growth_allowed(true, true, true, false),
            "all conditions met, no emergency → growth allowed"
        );
        assert!(
            !non_distress_growth_allowed(true, true, true, true),
            "an active emergency MUST veto growth even when every other condition qualifies"
        );
        // The veto does not paper over the ordinary gating: any missing pre-existing
        // condition still blocks growth regardless of the emergency flag.
        assert!(!non_distress_growth_allowed(false, true, true, false));
        assert!(!non_distress_growth_allowed(true, false, true, false));
        assert!(!non_distress_growth_allowed(true, true, false, false));
    }

    #[test]
    fn suppress_growth_step_coerces_up_to_hold_only_under_emergency() {
        // Up is suppressed (-> Hold) ONLY while the emergency is active; Down and
        // Hold always pass through, and Up passes through when not in emergency.
        // Pins the Up-arm veto: dropping `emergency_now` from the guard makes the
        // emergency case return `Up` and this assertion fails.
        assert_eq!(
            suppress_growth_step(BudgetStep::Up, true),
            BudgetStep::Hold,
            "Up MUST be coerced to Hold during an active emergency"
        );
        assert_eq!(
            suppress_growth_step(BudgetStep::Up, false),
            BudgetStep::Up,
            "Up passes through when there is no emergency (recovery is allowed)"
        );
        // Shedding is never blocked, and a Hold is unchanged, in either state.
        assert_eq!(
            suppress_growth_step(BudgetStep::Down(3), true),
            BudgetStep::Down(3)
        );
        assert_eq!(
            suppress_growth_step(BudgetStep::Down(3), false),
            BudgetStep::Down(3)
        );
        assert_eq!(
            suppress_growth_step(BudgetStep::Hold, true),
            BudgetStep::Hold
        );
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
            last_layer_drop_ms: 0.0,
            layers_at_floor: false,
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
            last_layer_drop_ms: 0.0,
            layers_at_floor: false,
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
            last_layer_drop_ms: 0.0,
            layers_at_floor: false,
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

    // ── Protective-mode emergency interaction (issue #1558) ───────────────────
    //
    // `sim_tick_protective` reproduces the FULL per-tick protective interaction
    // of the `attendants.rs` control loop on the PRESSURED path — the emergency
    // growth gate, the emergency cap clamp, and the resulting stage-3 encoder
    // ceiling — built ENTIRELY from the SAME pure helpers the loop calls
    // (`decide_step`, `non_distress_growth_allowed`, `re_arm_cascade_after_recovery`,
    // `protective_emergency_cap`, `protective_encoder_layer_ceiling`). Because the
    // growth gate goes through the shared `non_distress_growth_allowed`, removing
    // the `!emergency_now` veto from that one function breaks BOTH the loop and
    // this model — so the test below pins the real source of truth, not a copy.
    //
    // Returns the encoder ceiling the loop would PUBLISH this tick (the value the
    // `ProtectiveModeReport` carries and `Host` actuates).
    struct ProtSim {
        cap: usize,
        layers_at_floor: bool,
        last_layer_drop_ms: f64,
        last_step_ms: f64,
        direction_hold: u32,
        severity: u32,
    }

    fn sim_tick_protective(
        sim: &mut ProtSim,
        samples: &[BudgetSample],
        natural: usize,
        now: f64,
        protective_active: bool,
        audio_buffer_ms: Option<f64>,
    ) -> Option<u32> {
        // `emergency_now` is exactly `protective_emergency_cap(...).is_some()` —
        // the loop reuses the same flag for the growth veto and the clamp.
        let emergency_now = protective_emergency_cap(protective_active, audio_buffer_ms).is_some();

        let mut state = BudgetState {
            cap: sim.cap,
            last_step_ms: sim.last_step_ms,
            direction_hold: sim.direction_hold,
            last_layer_drop_ms: sim.last_layer_drop_ms,
            layers_at_floor: sim.layers_at_floor,
        };

        // The loop owns `direction_hold`: +1 per recovery-qualifying tick, reset
        // to 0 when recovery breaks. Modelled so `decide_step` can reach its Up
        // arm (which needs `direction_hold >= RECOVERY_HOLD`) under healthy FPS.
        if recovery_qualifying(samples, SUSTAIN_SAMPLES) {
            state.direction_hold = state.direction_hold.saturating_add(1);
        } else {
            state.direction_hold = 0;
        }

        let median = median_render_fps(samples, SUSTAIN_SAMPLES);
        // SHARED Up-arm gate: coerce Up->Hold under the emergency exactly as the
        // loop does. Removing the veto from `suppress_growth_step` lets the Up arm
        // below clear `layers_at_floor` and flips the ceiling — failing this test.
        let step = suppress_growth_step(
            decide_step_with_median(samples, &state, natural, now, median),
            emergency_now,
        );
        match step {
            BudgetStep::Down(magnitude) => {
                state.cap = state.cap.saturating_sub(magnitude).max(MIN_CAP);
                state.last_step_ms = now;
            }
            BudgetStep::Up => {
                re_arm_cascade_after_recovery(&mut state, now);
                state.cap = (state.cap + 1).min(natural.max(MIN_CAP));
                state.last_step_ms = now;
                state.direction_hold = 0;
            }
            BudgetStep::Hold => {
                let target = natural.max(MIN_CAP);
                let up_cooldown_elapsed = (now - state.last_step_ms) >= STEP_UP_COOLDOWN_MS;
                let not_distressed = non_distress_growth_qualifying(samples, SUSTAIN_SAMPLES);
                // The SHARED Hold-growth gate — the other single source of truth.
                if non_distress_growth_allowed(
                    state.cap < target,
                    up_cooldown_elapsed,
                    not_distressed,
                    emergency_now,
                ) {
                    state.cap += 1;
                    state.last_step_ms = now;
                    re_arm_cascade_after_recovery(&mut state, now);
                }
            }
        }

        // Stage-4 emergency clamp, applied LAST exactly as the loop does.
        if let Some(emergency_cap) = protective_emergency_cap(protective_active, audio_buffer_ms) {
            state.cap = state.cap.min(emergency_cap.max(MIN_CAP));
        }

        // Encoder severity escalation + ceiling recompute (the loop's lines that
        // build the published `ProtectiveModeReport`). NOTE the loop reads
        // `layers_at_floor` at the TOP of the tick (before the step), so its
        // published ceiling lags any clear by one tick; we recompute here off the
        // END-of-tick `layers_at_floor` instead. Both sample the SAME flag, so the
        // invariant under test — "growth must never clear `layers_at_floor` during
        // the emergency" — is pinned identically: a clear at end-of-tick N is
        // exactly what would drive the loop's tick-(N+1) ceiling=None publish.
        sim.severity = if protective_active && state.layers_at_floor {
            sim.severity.saturating_add(1)
        } else {
            0
        };
        let ceiling = protective_encoder_layer_ceiling(
            protective_active,
            state.layers_at_floor,
            sim.severity.saturating_sub(1),
        );

        // Commit state back for the next tick.
        sim.cap = state.cap;
        sim.layers_at_floor = state.layers_at_floor;
        sim.last_layer_drop_ms = state.last_layer_drop_ms;
        sim.last_step_ms = state.last_step_ms;
        sim.direction_hold = state.direction_hold;
        ceiling
    }

    #[test]
    fn sustained_audio_emergency_keeps_encoder_shed_no_flap() {
        // REGRESSION (issue #1558 emergency-growth gate). During a SUSTAINED
        // audio-only emergency — renderer HEALTHY (29 fps, in the non-distress
        // growth band, so `non_distress_growth_qualifying` is TRUE) but the jitter
        // buffer is past the emergency mark for many ticks — the stage-3 encoder
        // ceiling MUST stay SHED (`Some(..)`) and `layers_at_floor` MUST stay true
        // across the whole window. It must NOT oscillate to `None` (full send
        // ladder) every up-cooldown.
        //
        // Without the `!emergency_now` veto in `non_distress_growth_allowed`, the
        // Hold arm grows the cap each up-cooldown and re-arms the cascade (clearing
        // `layers_at_floor`); the emergency clamp re-slams the cap to MIN_CAP but
        // does NOT restore the floor flag, so the encoder ceiling flips to `None`
        // for that tick — the ~4s flap this test guards against.
        let natural = 8;
        // Enter AT FLOOR with the encoder already shedding (the state stages 1-3
        // leave behind before stage 4 fires).
        let mut sim = ProtSim {
            cap: MIN_CAP,
            layers_at_floor: true,
            last_layer_drop_ms: 0.0,
            // Far in the past so the up-cooldown is ELAPSED on the very first tick
            // — the worst case for the flap (growth would fire immediately).
            last_step_ms: -STEP_UP_COOLDOWN_MS,
            direction_hold: 0,
            severity: 1,
        };
        // Audio sustained past the emergency water mark for every tick.
        let audio = Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS + 300.0);
        let mut now = 0.0;

        // A single tick assertion reused across both renderer profiles below.
        let assert_no_flap = |sim: &mut ProtSim,
                              samples: &[BudgetSample],
                              now: f64,
                              tick: usize,
                              profile: &str| {
            let ceiling = sim_tick_protective(sim, samples, natural, now, true, audio);
            // The encoder ceiling stays SHED across the entire emergency window.
            assert!(
                ceiling.is_some(),
                "{profile} tick {tick}: encoder ceiling flapped to None (full send-ladder un-shed) during a sustained audio emergency"
            );
            // The cascade floor flag is never cleared by growth/recovery fighting
            // the emergency, and the cap is held at the speaker-only floor.
            assert!(
                sim.layers_at_floor,
                "{profile} tick {tick}: layers_at_floor was cleared mid-emergency (the root cause of the encoder-ceiling flap)"
            );
            assert_eq!(
                sim.cap, MIN_CAP,
                "{profile} tick {tick}: emergency cap must hold the decode budget at the speaker-only floor"
            );
        };

        // PHASE 1 — healthy renderer at 29 fps (the non-distress GROWTH band): the
        // Hold-arm growth gate would fire every up-cooldown if not vetoed. Pins the
        // `non_distress_growth_allowed` veto.
        let band29 = [fps_sample(29.0); 5];
        for tick in 0..12 {
            now += STEP_UP_COOLDOWN_MS;
            assert_no_flap(&mut sim, &band29, now, tick, "growth-band");
        }

        // PHASE 2 — healthy renderer at 60 fps (the strict-RECOVERY band, >= 30):
        // `decide_step` reaches its Up arm once `direction_hold >= RECOVERY_HOLD`.
        // Pins the Up-arm `if !emergency_now` gate (a DIFFERENT code path from the
        // Hold-growth gate). If the Up arm were not gated it would call
        // `re_arm_cascade_after_recovery` and clear the floor flag, flipping the
        // ceiling to None.
        let fast = [fps_sample(60.0); 5];
        for tick in 0..12 {
            now += STEP_UP_COOLDOWN_MS;
            assert_no_flap(&mut sim, &fast, now, tick, "recovery-band");
        }

        // RECOVERY: once audio drains below the emergency mark, `emergency_now`
        // clears and normal growth/recovery resumes — proving the veto did NOT
        // permanently block recovery (it only suppressed growth/recovery that
        // FOUGHT the emergency). With FPS healthy and direction_hold warm, the Up
        // arm now fires and the cascade re-arms.
        let drained = Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS - 50.0);
        now += STEP_UP_COOLDOWN_MS;
        let _ = sim_tick_protective(&mut sim, &fast, natural, now, true, drained);
        assert!(
            sim.cap > MIN_CAP || !sim.layers_at_floor,
            "after audio drains, growth/recovery must resume (cap grows or cascade re-arms) — the veto must not block legitimate recovery"
        );
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
        const {
            assert!(
                IOS_DECODE_TILE_CEILING_ABS < 9,
                "abs mobile cap below ratchet start"
            )
        };
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

    // ── issue #1466/#1471: "Back to automatic" clears force-decode requests ──

    /// Returning to `Auto` from a non-`Auto` override (the Settings picker's
    /// "Back to automatic" toggle, or selecting Auto in the picker) MUST clear the
    /// per-tile force-decode (PLAY) requests; every other transition keeps them.
    ///
    /// MUTATION SENSITIVITY: the production effect only clears
    /// `user_requested_decode` when this returns `true`. If the clear were dropped
    /// (the bug #1471 names — an inline `.clear()` that no test caught), or if the
    /// condition were inverted/widened, the asserts below flip. Each arm pins a
    /// distinct transition, so a one-sided mutation (e.g. always-true, or dropping
    /// the `previous != Auto` half) fails at least one assert.
    #[test]
    fn back_to_auto_clears_force_decode_requests_only_on_return_to_auto() {
        use DecodeBudgetOverride::{All, Auto, Fixed};

        // Return to Auto from a manual mode → CLEAR.
        assert!(
            should_clear_force_decode_on_override_change(Fixed(5), Auto),
            "Fixed -> Auto must clear force-decode requests"
        );
        assert!(
            should_clear_force_decode_on_override_change(All, Auto),
            "All -> Auto must clear force-decode requests"
        );

        // Leaving Auto for a manual mode, or moving between manual modes → KEEP
        // (a PLAY request is still meaningful in an explicit manual mode).
        assert!(
            !should_clear_force_decode_on_override_change(Auto, Fixed(5)),
            "Auto -> Fixed must NOT clear"
        );
        assert!(
            !should_clear_force_decode_on_override_change(Auto, All),
            "Auto -> All must NOT clear"
        );
        assert!(
            !should_clear_force_decode_on_override_change(Fixed(5), All),
            "Fixed -> All must NOT clear"
        );
        assert!(
            !should_clear_force_decode_on_override_change(All, Fixed(3)),
            "All -> Fixed must NOT clear"
        );

        // Non-transitions → KEEP. Auto -> Auto especially must not clear, or a
        // spurious re-render would wipe the user's requests.
        assert!(
            !should_clear_force_decode_on_override_change(Auto, Auto),
            "Auto -> Auto (no change) must NOT clear"
        );
        assert!(
            !should_clear_force_decode_on_override_change(Fixed(5), Fixed(5)),
            "Fixed -> same Fixed (no change) must NOT clear"
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

    // ── issue #1489: pinned-peer decode-admission merge ───────────────────────

    /// A pin that got a decoded slot (it IS in the decoded bucket) is force-added.
    /// This is the promotable-pin case (`[visible_tile_count,
    /// displayed_tile_count)`), where the pin-swap already placed it in
    /// `decoded_bucket`.
    ///
    /// MUTATION SENSITIVITY: if `merge_pinned_decode` dropped the insert (made a
    /// no-op) the set stays empty and this fails. The literal (321) is
    /// independent.
    #[test]
    fn merge_pinned_inserts_pin_in_decoded_bucket() {
        let mut active: HashSet<u64> = HashSet::new();
        merge_pinned_decode(&mut active, 321, &bucket(&[321]));
        assert!(active.contains(&321), "a decoded pin is force-added");
        assert_eq!(active.len(), 1, "exactly the one decoded pin was inserted");
    }

    /// A true-overflow pin (#1470) that got NO decoded slot — it is not in the
    /// decoded bucket — must NOT be force-decoded. This is the #1489
    /// decode⇄render invariant: a pin rendered in the +N badge (no grid bucket)
    /// is never decoded-but-invisible.
    ///
    /// MUTATION SENSITIVITY: if the helper dropped the `decoded_bucket.contains`
    /// gate (reverting to the old unconditional `active.insert(pin)`), id 555
    /// would be added and this fails — this is the exact regression #1489 fixes.
    #[test]
    fn merge_pinned_skips_pin_not_in_decoded_bucket() {
        let mut active: HashSet<u64> = HashSet::new();
        // Decoded bucket holds a DIFFERENT peer (777); the pin (555) is in the
        // true-overflow +N badge with no decoded slot.
        merge_pinned_decode(&mut active, 555, &bucket(&[777]));
        assert!(
            active.is_empty(),
            "an off-grid (true-overflow) pin must not enter active_decode_set"
        );
    }

    /// The merge is a UNION (pre-seeded ids survive) and idempotent.
    ///
    /// MUTATION SENSITIVITY: if the helper cleared/replaced `active`, the
    /// pre-seeded 999 would vanish and the first assert fails.
    #[test]
    fn merge_pinned_is_union_and_idempotent() {
        let mut active: HashSet<u64> = HashSet::new();
        active.insert(999);
        merge_pinned_decode(&mut active, 321, &bucket(&[321]));
        assert!(
            active.contains(&999),
            "pre-existing entry preserved (union)"
        );
        assert!(active.contains(&321), "decoded pin added");
        let after_first: HashSet<u64> = active.clone();
        merge_pinned_decode(&mut active, 321, &bucket(&[321]));
        assert_eq!(active, after_first, "re-merging the same pin is a no-op");
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

    // ── issue #1559: presenter-aware decode shedding ─────────────────────────
    //
    // While the local user is screen-sharing, the budget must shed peer decodes
    // MORE aggressively under pressure to free CPU for the screen encoder, but
    // ONLY when actually pressured (a powerful device sharing in a small meeting
    // keeps decoding peers). These pin the two pure levers — the lowered
    // step-down FPS threshold (`presenter_step_down_fps` /
    // `presenter_extra_shed_pressure`, "step down sooner") and the pressured-cap
    // ceiling (`presenter_cap_ceiling`, "lower floor") — plus a loop-mirroring
    // simulation that proves the cap lands LOWER while sharing and recovers when
    // sharing stops.

    /// The presenter step-down FPS threshold is raised to FPS_STEP_UP while
    /// sharing (so the 24-30 band counts as pressure) and is the normal
    /// FPS_STEP_DOWN otherwise.
    ///
    /// MUTATION SENSITIVITY: if `presenter_step_down_fps` ignored `sharing` (e.g.
    /// always returned FPS_STEP_DOWN) the first assert fails; the `<` ordering
    /// assert pins that sharing genuinely raises the bar (sheds sooner). Both
    /// expected values are independent constants, not derived from the output.
    #[test]
    fn presenter_step_down_threshold_is_higher_while_sharing() {
        assert_eq!(
            presenter_step_down_fps(true),
            FPS_STEP_UP,
            "sharing widens the pressure zone up to FPS_STEP_UP"
        );
        assert_eq!(
            presenter_step_down_fps(false),
            FPS_STEP_DOWN,
            "not sharing uses the normal step-down threshold"
        );
        assert!(
            presenter_step_down_fps(false) < presenter_step_down_fps(true),
            "sharing must lower the FPS bar for shedding (step down sooner)"
        );
    }

    /// `presenter_extra_shed_pressure` fires in the mild 24-30 band ONLY while
    /// sharing, and never on a healthy >= 30 presenter (pressure-gated) nor when
    /// not sharing.
    ///
    /// MUTATION SENSITIVITY: removing the presenter bias (returning `false`
    /// unconditionally, or gating on the normal `< FPS_STEP_DOWN`) makes the
    /// `band_sharing` assert fail. Removing the `!sharing` guard makes the
    /// `band_not_sharing` assert fail. Using `<=` on a healthy machine would make
    /// the `healthy_sharing` assert fail.
    #[test]
    fn presenter_extra_shed_pressure_fires_in_band_only_while_sharing() {
        // Median in the 24-30 band (29 fps): a presenter IS pressured here.
        let band = [fps_sample(29.0), fps_sample(29.0), fps_sample(29.0)];
        assert!(
            presenter_extra_shed_pressure(&band, true),
            "a sharing presenter in the 24-30 band is under extra-shed pressure"
        );
        // ...but NOT a presenter (not sharing) — the normal trigger handles 24-30
        // as the hysteresis band, no extra shed.
        assert!(
            !presenter_extra_shed_pressure(&band, false),
            "not sharing ⇒ the 24-30 band is the normal hysteresis band, no extra shed"
        );
        // A healthy >= 30 presenter is NOT pressured (the whole point of
        // pressure-gating: a powerful device sharing keeps decoding peers).
        let healthy = [
            fps_sample(FPS_STEP_UP + 5.0),
            fps_sample(FPS_STEP_UP + 5.0),
            fps_sample(FPS_STEP_UP + 5.0),
        ];
        assert!(
            !presenter_extra_shed_pressure(&healthy, true),
            "a healthy >= 30 presenter is not pressured ⇒ no extra shed (pressure-gated)"
        );
        // Boundary: median EXACTLY at FPS_STEP_UP is NOT below it ⇒ not pressured.
        let boundary = [
            fps_sample(FPS_STEP_UP),
            fps_sample(FPS_STEP_UP),
            fps_sample(FPS_STEP_UP),
        ];
        assert!(
            !presenter_extra_shed_pressure(&boundary, true),
            "median == FPS_STEP_UP is the recovery floor, not pressure"
        );
        // Short window declines to act.
        assert!(!presenter_extra_shed_pressure(&band[..1], true));
    }

    /// `presenter_cap_ceiling` returns a LOWER, ABSOLUTELY-BOUNDED cap while
    /// sharing — `min(ceil(natural * FACTOR), PRESENTER_RESIDUAL_FLOOR)` floored
    /// at MIN_CAP — and `None` when not sharing (full recovery, no leaked state).
    ///
    /// MUTATION SENSITIVITY:
    /// - Removing the `PRESENTER_RESIDUAL_FLOOR` `min` (reverting to the pure
    ///   fraction) makes the LARGE-meeting asserts fail: `natural=14` would yield
    ///   `ceil(14*0.5)=7` not `5`, and `natural=30` would yield `15` not `5`.
    /// - Ignoring `sharing` and returning `Some(_)` while NOT sharing fails the
    ///   `not_sharing` assert (`None`).
    /// - Dropping the fraction (always returning the floor) fails the SMALL-meeting
    ///   assert: `natural=6` would yield `5` not `3` (the fraction must win there).
    ///
    /// All expected values are independent literals, not derived from the output.
    #[test]
    fn presenter_cap_ceiling_sheds_while_sharing_and_recovers_on_stop() {
        // Not sharing ⇒ no ceiling (recovery / non-presenter behaviour).
        assert_eq!(
            presenter_cap_ceiling(14, false),
            None,
            "not sharing ⇒ no presenter ceiling (cap recovers to normal behaviour)"
        );

        // LARGE meeting (the #1559 worst case): natural=14 ⇒ the residual FLOOR
        // (5) wins over the fraction (ceil(14*0.5)=7). This is the effectiveness
        // fix: 5 residual decodes ≈ the healthy baseline the encoder ran fine
        // alongside, NOT 7 (which was still at/above the starving borderline).
        let large = presenter_cap_ceiling(14, true).expect("sharing ⇒ a ceiling");
        assert!(
            large < 14,
            "a sharing presenter's pressured-cap ceiling must be below natural so peer tiles shed"
        );
        assert_eq!(
            large, 5,
            "large meeting sheds to the residual floor (5), not the fraction (7)"
        );

        // VERY LARGE meeting: natural=30 ⇒ still bounded to the floor (5), NOT
        // ceil(30*0.5)=15. A pure fraction would leave a large meeting starving
        // the encoder; the absolute floor is what bounds it regardless of size.
        assert_eq!(
            presenter_cap_ceiling(30, true),
            Some(5),
            "very large meeting is bounded to the residual floor (5), not 15"
        );

        // SMALL meeting: natural=6 ⇒ the FRACTION (ceil(6*0.5)=3) wins over the
        // floor (5), keeping small meetings gentle (3 < 5, the min picks 3).
        assert_eq!(
            presenter_cap_ceiling(6, true),
            Some(3),
            "small meeting uses the gentle fraction (3), the floor does not bind"
        );

        // Tiny meetings never drop below MIN_CAP (the presenter always decodes the
        // active speaker).
        assert_eq!(
            presenter_cap_ceiling(1, true),
            Some(MIN_CAP),
            "ceiling is floored at MIN_CAP (a presenter always decodes >= 1 tile)"
        );
        assert_eq!(
            presenter_cap_ceiling(0, true),
            Some(MIN_CAP),
            "0-natural ceiling is floored at MIN_CAP, never 0"
        );
    }

    /// END-TO-END (loop-mirroring): under the SAME measured pressure, a SHARING
    /// presenter's cap lands STRICTLY LOWER than a non-sharing user's, and when
    /// sharing stops the cap recovers back toward natural. This reproduces the
    /// loop's pressured-path arithmetic: apply `decide_step`, then clamp to the
    /// presenter ceiling while sharing (the post-step clamp the loop performs).
    ///
    /// MUTATION SENSITIVITY: deleting the presenter-ceiling clamp (the #1559
    /// lever) makes `shared_cap == not_shared_cap`, so the `<` assert fails. This
    /// is the load-bearing presenter-bias assertion.
    fn presenter_sim_pressured_cap(
        samples: &[BudgetSample],
        start_cap: usize,
        natural: usize,
        sharing: bool,
        now: f64,
    ) -> usize {
        let mut state = BudgetState {
            cap: start_cap,
            last_step_ms: 0.0,
            direction_hold: 0,
            last_layer_drop_ms: 0.0,
            layers_at_floor: false,
        };
        // Pressured path: apply decide_step's step.
        match decide_step(samples, &state, natural, now) {
            BudgetStep::Down(m) => state.cap = state.cap.saturating_sub(m).max(MIN_CAP),
            BudgetStep::Up => state.cap = (state.cap + 1).min(natural.max(MIN_CAP)),
            BudgetStep::Hold => {}
        }
        // Presenter post-step clamp (the loop's #1559 lever).
        if let Some(ceiling) = presenter_cap_ceiling(natural, sharing) {
            state.cap = state.cap.min(ceiling.max(MIN_CAP));
        }
        state.cap
    }

    #[test]
    fn sharing_presenter_sheds_more_under_pressure_and_recovers_on_stop() {
        let natural = 14;
        // Mild measured pressure (24-30 band-ish, below FPS_STEP_DOWN so the
        // normal down-step also fires): both users step down by one tile, but the
        // sharing presenter is ALSO clamped to the presenter ceiling.
        let mild = (FPS_SEVERE + FPS_STEP_DOWN) / 2.0;
        let pressure = [fps_sample(mild), fps_sample(mild), fps_sample(mild)];
        // Both start at a high cap (already pressured, settled near natural).
        let start = natural - 1; // 13

        let not_shared =
            presenter_sim_pressured_cap(&pressure, start, natural, false, PAST_COOLDOWN);
        let shared = presenter_sim_pressured_cap(&pressure, start, natural, true, PAST_COOLDOWN);

        assert!(
            shared < not_shared,
            "under the same pressure a sharing presenter sheds MORE tiles than a non-sharing user (shared={shared}, not_shared={not_shared})"
        );
        // The presenter cap equals the presenter ceiling. natural=14 ⇒ the residual
        // floor (5) binds over the fraction (ceil(14*0.5)=7), so the shed lands at 5.
        assert_eq!(
            shared, 5,
            "the sharing presenter cap is clamped to the presenter ceiling (residual floor)"
        );

        // RECOVER ON STOP: once sharing stops, the presenter ceiling no longer
        // binds — a machine sitting ABOVE the old ceiling (cap 9 > 5) is NOT
        // dragged back down. With healthy samples `decide_step` Holds (no
        // down-step) and `sharing=false` ⇒ `presenter_cap_ceiling` is `None`, so
        // the clamp leaves the cap at 9.
        //
        // SCOPE (honest): this sim models the Hold arm as a no-op, so it pins ONLY
        // that the ceiling no longer drags the cap down once sharing stops — it
        // does NOT exercise the loop's non-distress GROWTH re-step (that lives in
        // the pressured-Hold arm of the control loop, not this pure helper). The
        // re-grow toward natural is covered by the existing growth-sim tests.
        //
        // MUTATION SENSITIVITY: if the presenter clamp ignored `sharing` (always
        // bound the ceiling) the cap would be dragged to 5 and this fails.
        let healthy = [fps_sample(29.0), fps_sample(29.0), fps_sample(29.0)];
        let above_ceiling = 9; // > presenter ceiling 5, < natural 14
        let recovered =
            presenter_sim_pressured_cap(&healthy, above_ceiling, natural, false, PAST_COOLDOWN);
        assert_eq!(
            recovered, above_ceiling,
            "once sharing stops the presenter ceiling no longer drags the cap down (recovered={recovered})"
        );
        // And the CONTRAST: with the SAME healthy window but still sharing, the
        // cap IS clamped back to the presenter ceiling (the lever still binds
        // while sharing). This pins that recovery is gated on `sharing`, not on
        // the sample health.
        let still_sharing =
            presenter_sim_pressured_cap(&healthy, above_ceiling, natural, true, PAST_COOLDOWN);
        assert_eq!(
            still_sharing, 5,
            "while STILL sharing, a healthy cap above the ceiling is clamped back to it"
        );
    }

    // ── issue #1558: protective mode (audio-first, speaker-priority) ──────────
    //
    // These pin the FOUR pure levers protective mode adds on top of the #1557
    // cascade: the broader `in_distress` predicate (each trigger flips it
    // independently; all-clear is false), the asymmetric latch
    // (`tick_protective_mode`: enters on sustained distress, exits on sustained
    // recovery, never flaps on a single sample), the encoder self-shed
    // (`protective_encoder_layer_ceiling`: gated on the cascade reaching floor,
    // stepping 2→1 by severity, never below the base layer, and `None` when
    // inactive so the user's ceiling rules), and the emergency cap
    // (`protective_emergency_cap`: floors at MIN_CAP — the speaker tile — only
    // while audio is still growing, `None` otherwise so it reverses on recovery).

    /// `in_distress` truth table: each trigger flips the predicate INDEPENDENTLY,
    /// the all-clear set is false, and every `None` sub-signal is conservative
    /// (cannot manufacture distress).
    ///
    /// MUTATION SENSITIVITY: each case isolates ONE trigger. Removing or inverting
    /// any single condition in `in_distress` (e.g. flipping the audio `>` to `<`,
    /// dropping the cap_score+participant AND, or weakening the fps `<`) fails the
    /// matching case while the all-clear case stays green — so a one-sided mutation
    /// is always caught. The expected booleans are independent literals.
    #[test]
    fn in_distress_each_trigger_flips_independently() {
        // All-clear ⇒ false. This is the anchor: every other case perturbs ONE
        // axis of this baseline.
        assert!(
            !in_distress(DistressSignals::clear()),
            "a fully-clear signal set must NOT be distress"
        );

        // 1. Collapsed renderer: median FPS below the distress floor.
        let mut fps = DistressSignals::clear();
        fps.median_fps = Some(PROTECTIVE_FPS_DISTRESS - 1.0);
        assert!(in_distress(fps), "median fps below the floor is distress");
        // Boundary: EXACTLY at the floor is NOT below it ⇒ not distress.
        let mut fps_boundary = DistressSignals::clear();
        fps_boundary.median_fps = Some(PROTECTIVE_FPS_DISTRESS);
        assert!(
            !in_distress(fps_boundary),
            "median fps exactly at the floor is not below it ⇒ not distress"
        );
        // A None median can never trigger.
        let mut fps_none = DistressSignals::clear();
        fps_none.median_fps = None;
        assert!(
            !in_distress(fps_none),
            "an unmeasured (None) median fps cannot manufacture distress"
        );

        // 2. Saturated main thread: sustained longtask at/above the threshold.
        let mut lt = DistressSignals::clear();
        lt.longtask_ms_per_sec = Some(PROTECTIVE_LONGTASK_DISTRESS_MS_PER_SEC);
        assert!(in_distress(lt), "longtask at the threshold is distress");
        // A None longtask (WebKit/iOS) can never trigger.
        let mut lt_none = DistressSignals::clear();
        lt_none.longtask_ms_per_sec = None;
        assert!(
            !in_distress(lt_none),
            "an unavailable (None) longtask cannot manufacture distress"
        );

        // 3. Audio backing up: any peer's buffer above the distress water mark.
        let mut audio = DistressSignals::clear();
        audio.max_peer_audio_buffer_ms = Some(PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS + 1.0);
        assert!(
            in_distress(audio),
            "audio buffer above the mark is distress"
        );
        // Boundary: exactly AT the mark is not ABOVE it ⇒ not distress.
        let mut audio_boundary = DistressSignals::clear();
        audio_boundary.max_peer_audio_buffer_ms = Some(PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS);
        assert!(
            !in_distress(audio_boundary),
            "audio buffer exactly at the mark is not above it ⇒ not distress"
        );

        // 4. Audio time-compressing: sustained NetEQ accelerate (deferred signal,
        // but the predicate honours it when present).
        let mut acc = DistressSignals::clear();
        acc.neteq_accelerate_per_sec = Some(PROTECTIVE_NETEQ_ACCELERATE_DISTRESS_PER_SEC);
        assert!(
            in_distress(acc),
            "neteq accelerate at the threshold is distress"
        );

        // 5. Low-cap AND crowded: BOTH halves required (the AND).
        let mut both = DistressSignals::clear();
        both.cap_score = Some(PROTECTIVE_CAP_SCORE_DISTRESS - 1);
        both.participant_count = PROTECTIVE_PARTICIPANT_COUNT_DISTRESS + 1;
        assert!(
            in_distress(both),
            "low cap_score AND crowded meeting is distress"
        );
        // Low cap but small meeting ⇒ NOT distress (the AND requires crowd).
        let mut low_cap_small = DistressSignals::clear();
        low_cap_small.cap_score = Some(PROTECTIVE_CAP_SCORE_DISTRESS - 1);
        low_cap_small.participant_count = PROTECTIVE_PARTICIPANT_COUNT_DISTRESS;
        assert!(
            !in_distress(low_cap_small),
            "a low-cap device in a small meeting is not distress (AND requires crowd)"
        );
        // Crowded but capable ⇒ NOT distress (the AND requires low cap).
        let mut capable_crowded = DistressSignals::clear();
        capable_crowded.cap_score = Some(PROTECTIVE_CAP_SCORE_DISTRESS);
        capable_crowded.participant_count = PROTECTIVE_PARTICIPANT_COUNT_DISTRESS + 10;
        assert!(
            !in_distress(capable_crowded),
            "a capable device in a crowded meeting is not distress (AND requires low cap)"
        );
    }

    /// The protective-mode latch ENTERS only after a sustained distress run,
    /// EXITS only after a (longer) sustained clear run, and NEVER flips on a
    /// single bad/good sample — asymmetric hysteresis, mirroring the budget loop.
    ///
    /// MUTATION SENSITIVITY: the `>=` thresholds and the asymmetric streak
    /// accounting are pinned. If `tick_protective_mode` were mutated to flip on a
    /// single sample (e.g. `enter_streak >= 1`), the `single_bad_sample` assertion
    /// fails; if the exit window were shortened to the enter window, the
    /// asymmetry assertion fails; if the streaks did not reset on the opposite
    /// sample, the `flap` assertions fail. All expected values are independent.
    #[test]
    fn protective_latch_enters_and_exits_with_asymmetric_hysteresis() {
        // Asymmetry is the whole design point — pin it structurally so a future
        // edit that equalises the windows fails to compile-time-check the intent.
        const { assert!(PROTECTIVE_EXIT_RECOVERY > PROTECTIVE_ENTER_SUSTAIN) };

        let mut st = ProtectiveModeState::default();
        assert!(!st.active, "starts inactive");

        // A single distress sample does NOT enter (sustain > 1).
        assert_eq!(
            tick_protective_mode(&mut st, true),
            ProtectiveTransition::None,
            "one distress sample must not enter protective mode"
        );
        assert!(!st.active);

        // A clear sample BEFORE the enter window completes resets the streak — no
        // flap-in on alternating samples.
        assert_eq!(
            tick_protective_mode(&mut st, false),
            ProtectiveTransition::None
        );
        assert_eq!(st.enter_streak, 0, "a clear sample resets the enter streak");

        // Now drive a FULL sustained distress run: the entry edge fires exactly on
        // the PROTECTIVE_ENTER_SUSTAIN-th consecutive distress sample.
        let mut entered_on = None;
        for i in 1..=PROTECTIVE_ENTER_SUSTAIN {
            let t = tick_protective_mode(&mut st, true);
            if t == ProtectiveTransition::Entered {
                entered_on = Some(i);
            }
        }
        assert_eq!(
            entered_on,
            Some(PROTECTIVE_ENTER_SUSTAIN),
            "protective mode enters exactly on the sustain-th distress sample"
        );
        assert!(st.active, "latched ON after the sustain window");

        // While active, a SINGLE clear sample does NOT exit (recovery > 1)...
        assert_eq!(
            tick_protective_mode(&mut st, false),
            ProtectiveTransition::None,
            "one clear sample must not exit protective mode"
        );
        assert!(st.active, "still active after one clear sample");
        // ...and a distress sample mid-recovery RESETS the exit streak (no
        // flap-out): drive a few clears, interrupt, and confirm no exit yet.
        tick_protective_mode(&mut st, false); // exit_streak grows
        assert_eq!(
            tick_protective_mode(&mut st, true),
            ProtectiveTransition::None,
            "a distress sample mid-recovery must not exit"
        );
        assert_eq!(
            st.exit_streak, 0,
            "a distress sample resets the exit streak (no flap-out)"
        );
        assert!(st.active, "still active after the interrupted recovery");

        // Now a FULL sustained clear run: the exit edge fires exactly on the
        // PROTECTIVE_EXIT_RECOVERY-th consecutive clear sample — the LONGER window.
        let mut exited_on = None;
        for i in 1..=PROTECTIVE_EXIT_RECOVERY {
            let t = tick_protective_mode(&mut st, false);
            if t == ProtectiveTransition::Exited {
                exited_on = Some(i);
            }
        }
        assert_eq!(
            exited_on,
            Some(PROTECTIVE_EXIT_RECOVERY),
            "protective mode exits exactly on the recovery-th clear sample (longer window)"
        );
        assert!(!st.active, "latched OFF after the recovery window");
    }

    /// Stage 3 (encoder self-shed): the LOCAL send-layer ceiling is `None` unless
    /// protective mode is active AND the #1557 cascade has reached its floor; once
    /// gated it steps 2→1 by severity and NEVER drops below the base layer (1).
    /// The active speaker's video is never affected (this caps only the LOCAL
    /// send ladder).
    ///
    /// MUTATION SENSITIVITY: dropping the `!active` guard makes the inactive case
    /// return Some; dropping the `!cascade_at_floor` guard makes the not-at-floor
    /// case return Some (firing the encoder shed BEFORE the cheaper levers —
    /// out of order); collapsing the severity branch makes the 2-vs-1 assertion
    /// fail. Each is caught.
    #[test]
    fn protective_encoder_shed_is_gated_and_floored_at_base_layer() {
        // Inactive ⇒ no encoder ceiling (the user/auto ceiling rules).
        assert_eq!(
            protective_encoder_layer_ceiling(false, true, 5),
            None,
            "inactive protective mode must not cap the encoder"
        );
        // Active but cascade NOT at floor ⇒ no encoder shed yet (levers in order:
        // received layers + tile pause must reach floor FIRST).
        assert_eq!(
            protective_encoder_layer_ceiling(true, false, 5),
            None,
            "encoder shed must wait until the cascade reaches floor (ordered levers)"
        );
        // Active + at floor, mild severity ⇒ drop the top layer (3→2).
        assert_eq!(
            protective_encoder_layer_ceiling(true, true, 0),
            Some(2),
            "first encoder shed drops the top layer to 2"
        );
        // Active + at floor, escalated severity ⇒ base-only (2→1) for max relief.
        assert_eq!(
            protective_encoder_layer_ceiling(true, true, 1),
            Some(1),
            "escalated encoder shed drops to base-only (1)"
        );
        // Never below the base layer regardless of how high severity climbs.
        assert_eq!(
            protective_encoder_layer_ceiling(true, true, 99),
            Some(1),
            "the encoder ceiling never drops below the base layer (1)"
        );
    }

    /// Stage 4 (EMERGENCY non-speaker pause): the decode cap is forced to MIN_CAP
    /// (the speaker tile) ONLY while protective mode is active AND audio is STILL
    /// growing past the EMERGENCY water mark; otherwise `None` so the stage
    /// reverses on recovery. The active speaker survives because the caller's
    /// `promote_speakers` fills the single MIN_CAP slot.
    ///
    /// MUTATION SENSITIVITY: dropping the `!active` guard fires the emergency
    /// while inactive; flipping the `>` to `<` (or using the lower DISTRESS mark
    /// instead of the EMERGENCY mark) fires it too early. The boundary case pins
    /// the exact threshold. Expected values are independent literals.
    #[test]
    fn protective_emergency_floors_cap_only_while_audio_still_growing() {
        // Inactive ⇒ never an emergency clamp, even with a huge buffer.
        assert_eq!(
            protective_emergency_cap(false, Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS + 500.0)),
            None,
            "inactive protective mode never forces the emergency cap"
        );
        // Active but audio NOT past the emergency mark ⇒ no emergency (stages 1-3
        // are expected to be holding the line; the cap recovers normally).
        assert_eq!(
            protective_emergency_cap(true, Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS)),
            None,
            "audio exactly at the emergency mark is not ABOVE it ⇒ no emergency yet"
        );
        // Active AND audio still growing past the emergency mark ⇒ floor the cap
        // at MIN_CAP (one tile, filled by the active speaker).
        assert_eq!(
            protective_emergency_cap(true, Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS + 1.0)),
            Some(MIN_CAP),
            "active + audio still growing floors the cap to the speaker tile"
        );
        // A None buffer reading cannot trigger the emergency (conservative).
        assert_eq!(
            protective_emergency_cap(true, None),
            None,
            "an unmeasured audio buffer cannot trigger the emergency pause"
        );
        // The EMERGENCY mark is strictly above the DISTRESS mark — the two stages
        // are distinct (entry-level distress vs the worse emergency). Pin it so a
        // future edit cannot collapse them into one.
        const { assert!(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS > PROTECTIVE_AUDIO_BUFFER_DISTRESS_MS) };
    }

    /// SPEAKER EXEMPTION through the encoder-shed AND emergency stages
    /// (issue #1558 item 4): a loop-mirroring simulation proves that across the
    /// full degradation sequence — cascade floor → encoder self-shed → emergency
    /// pause — the decode cap NEVER drops below MIN_CAP (the slot the active
    /// speaker occupies via `promote_speakers`), and the LOCAL encoder shed caps
    /// only the send ladder (it returns a layer COUNT, never a remote-decode
    /// instruction). The speaker's REMOTE video therefore survives every stage.
    ///
    /// MUTATION SENSITIVITY: if `protective_emergency_cap` returned `Some(0)` (or
    /// the floor were dropped) the `cap >= MIN_CAP` assertion fails — that is the
    /// speaker being starved. If the encoder shed returned 0, the
    /// `encoder_ceiling >= 1` assertion fails — the base layer being dropped, so a
    /// peer would lose all video of the local user.
    #[test]
    fn speaker_survives_encoder_shed_and_emergency_stages() {
        // Drive the worst stage: active, cascade at floor, escalated severity,
        // audio past the emergency mark.
        let active = true;
        let cascade_at_floor = true;
        let severity = 9;
        let audio = Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS + 200.0);

        let encoder_ceiling = protective_encoder_layer_ceiling(active, cascade_at_floor, severity)
            .expect("at floor + active ⇒ an encoder ceiling");
        assert!(
            encoder_ceiling >= 1,
            "the encoder self-shed never drops the base send layer (peer keeps video of local user)"
        );

        let emergency_cap = protective_emergency_cap(active, audio)
            .expect("active + audio still growing ⇒ the emergency cap");
        assert!(
            emergency_cap >= MIN_CAP,
            "the emergency decode cap never starves the active-speaker slot (>= MIN_CAP)"
        );
        assert_eq!(
            emergency_cap, MIN_CAP,
            "the emergency cap is exactly the speaker tile (one decoded tile)"
        );
    }

    /// RECOVERY REVERSES THE STAGES (issue #1558 item 5): as audio drains and the
    /// cascade leaves floor, the emergency cap releases FIRST (audio recovers
    /// before the buffer fully clears), then the encoder shed releases when the
    /// cascade leaves floor, and finally the latch exits after the recovery
    /// window — all levers return to their inactive (`None`) state in reverse
    /// order. This pins the reversibility contract.
    ///
    /// MUTATION SENSITIVITY: if any lever failed to release on its clearing
    /// condition (e.g. `protective_emergency_cap` ignored the buffer and stayed
    /// `Some`), the matching `assert_eq!(..., None)` fails.
    #[test]
    fn protective_stages_reverse_on_recovery() {
        // Stage 4 releases the instant audio drops to/below the emergency mark,
        // even while still active and at floor — the most aggressive lever is the
        // first to relax.
        assert_eq!(
            protective_emergency_cap(true, Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS - 1.0)),
            None,
            "emergency cap releases as soon as audio drains below the emergency mark"
        );
        // Stage 3 releases when the cascade leaves floor (received layers re-grow
        // ⇒ cascade_at_floor=false), even while still active.
        assert_eq!(
            protective_encoder_layer_ceiling(true, false, 9),
            None,
            "encoder shed releases when the cascade leaves floor (layers re-grow)"
        );
        // And when the latch finally exits, BOTH levers are unconditionally off.
        assert_eq!(
            protective_encoder_layer_ceiling(false, true, 9),
            None,
            "latch off ⇒ encoder shed off regardless of cascade state"
        );
        assert_eq!(
            protective_emergency_cap(false, Some(PROTECTIVE_AUDIO_BUFFER_EMERGENCY_MS + 999.0)),
            None,
            "latch off ⇒ emergency cap off regardless of audio buffer"
        );
    }

    /// The encoder-ceiling composition (stage 3 actuation) takes the MORE
    /// restrictive of the user's persisted ceiling and protective mode's request,
    /// and reverts to the user's preference alone when protective mode releases —
    /// so neither clobbers the other and the shed is fully reversible.
    ///
    /// MUTATION SENSITIVITY: replacing the `min` with `max` (or dropping the user
    /// term) fails the `both_some` assertion; returning `protective` on the
    /// `(Some, None)` arm (the release arm) fails the reversibility assertion —
    /// the case that proves protective mode does not strand the user's choice.
    #[test]
    fn compose_encoder_ceiling_takes_more_restrictive_and_reverts_on_release() {
        // Both present ⇒ the lower (more restrictive) wins.
        assert_eq!(
            compose_encoder_ceiling(Some(3), Some(1)),
            Some(1),
            "protective shed (1) is more restrictive than the user ceiling (3)"
        );
        assert_eq!(
            compose_encoder_ceiling(Some(1), Some(2)),
            Some(1),
            "the user's lower ceiling (1) wins over a gentler protective shed (2)"
        );
        // User pref only (protective released) ⇒ the user's ceiling alone — this is
        // the REVERSIBILITY case (protective mode never strands the user's choice).
        assert_eq!(
            compose_encoder_ceiling(Some(2), None),
            Some(2),
            "on protective release the effective ceiling reverts to the user's preference"
        );
        // Protective only (user has no cap) ⇒ protective's shed binds.
        assert_eq!(
            compose_encoder_ceiling(None, Some(1)),
            Some(1),
            "protective shed binds when the user set no ceiling"
        );
        // Neither ⇒ no cap (full ladder / Auto).
        assert_eq!(
            compose_encoder_ceiling(None, None),
            None,
            "no user cap and no protective shed ⇒ no ceiling (full ladder)"
        );
    }
}
