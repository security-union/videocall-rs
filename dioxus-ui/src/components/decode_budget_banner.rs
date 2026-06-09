// SPDX-License-Identifier: MIT OR Apache-2.0

//! Meeting-level "videos paused" banner + its anti-flap damper (issue #1142,
//! Phase 1).
//!
//! When the adaptive decode-budget controller (see [`crate::components::decode_budget`])
//! caps how many tiles decode video, off-budget peers render as avatars. That is
//! the right behaviour for CPU survival, but without a meeting-level affordance
//! the user has no idea *why* some tiles went to avatars or that they can opt
//! back in. This module supplies:
//!
//! 1. [`DecodeBudgetBanner`] — a slim, dismissible glass bar pinned top-centre of
//!    the grid area. It announces how many videos are paused and offers a
//!    one-click "Show all videos" escape hatch that flips the decode-budget
//!    override to `Fixed(natural)` (reusing the exact machinery the appearance
//!    settings panel uses).
//!
//! 2. [`BannerDamper`] — a **pure, host-testable** state machine that decides
//!    *whether* the banner should be visible right now, given the live pressure
//!    signal and wall-clock time. All anti-flap policy lives here, deliberately
//!    SEPARATE from `decode_budget.rs` (which owns the cap *actuator*): cosmetic
//!    gating must never perturb the actuator, and the actuator's thresholds must
//!    never silently double as banner thresholds.
//!
//! ## Why the damper is a pure struct, not Dioxus state
//!
//! `#[wasm_bindgen_test]` silently no-ops in this crate's CI (false-green), so a
//! state machine validated only through a rendered component would have *no*
//! real coverage. By making the damper a plain `struct` with a single
//! [`BannerDamper::tick`] transition function `(now_ms, pressured, avatar_count)
//! -> visible`, the entire policy is exercised by ordinary `#[test]` cases that
//! run on the host. The Dioxus component is a thin driver: it calls `tick` each
//! render/poll and renders iff it returns `true`.

use dioxus::prelude::*;

use crate::context::{save_decode_budget_override, DecodeBudgetCtx, DecodeBudgetOverride};

// ──────────────────────────────────────────────────────────────────────────
// Tunable thresholds.
//
// All of these are FIRST-GUESS values pending real-world measurement, matching
// the honesty posture of #1159 / the `decode_budget.rs` threshold comments. They
// are intentionally named constants so a future performance/UX pass can tune
// them in one place. DO NOT treat any of these as final.
// ──────────────────────────────────────────────────────────────────────────

/// How long pressure must be **continuously** present before the banner is
/// allowed to appear. Prevents a sub-second blip (a GC pause, one slow frame)
/// from flashing the banner.
///
/// Tunable, pending real-world measurement.
pub const SHOW_SUSTAIN_MS: f64 = 2_000.0;

/// Minimum time the banner stays visible once shown, even if pressure clears
/// immediately. Stops the banner from vanishing the instant the user's eyes
/// reach it.
///
/// Tunable, pending real-world measurement.
pub const MIN_DWELL_MS: f64 = 5_000.0;

/// How long pressure must be **continuously** clear before an already-visible
/// banner auto-hides. Combined with [`MIN_DWELL_MS`] (whichever is later wins),
/// this damps the hide edge so a brief recovery does not blink the banner off
/// and back on.
///
/// Tunable, pending real-world measurement.
pub const HIDE_SUSTAIN_MS: f64 = 3_000.0;

/// Rolling window over which repeated show events are counted for back-off.
///
/// Tunable, pending real-world measurement.
pub const EPISODE_WINDOW_MS: f64 = 120_000.0;

/// Number of show "episodes" within [`EPISODE_WINDOW_MS`] that trips back-off.
/// The Nth episode (this many in the window) is the one that engages it.
///
/// Tunable, pending real-world measurement.
pub const EPISODE_BACKOFF_THRESHOLD: usize = 3;

/// Escalating back-off cooldowns. Each time back-off re-engages while still
/// "hot" (see [`ESCALATION_RESET_MS`]), the next-longer cooldown is used; the
/// final entry is the cap and repeats. Back-off is NEVER permanent — after the
/// cooldown elapses the banner is allowed again.
///
/// Tunable, pending real-world measurement.
pub const BACKOFF_COOLDOWNS_MS: [f64; 3] = [60_000.0, 120_000.0, 240_000.0];

/// How long the damper must stay quiet (no new episode, not in back-off) before
/// the escalation level resets to the base cooldown. Escalation does NOT reset
/// the instant a single cooldown ends — only sustained quiet relaxes it.
///
/// Tunable, pending real-world measurement.
pub const ESCALATION_RESET_MS: f64 = 300_000.0;

// ──────────────────────────────────────────────────────────────────────────
// Pure damper state machine.
// ──────────────────────────────────────────────────────────────────────────

/// Coarse phase of the damper. The exact edge timing is carried in the
/// timestamp fields of [`BannerDamper`]; this enum just records which arm of the
/// machine we are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Banner not shown and pressure not (yet) sustained.
    Idle,
    /// Pressure is present; counting up to [`SHOW_SUSTAIN_MS`] before showing.
    PendingShow,
    /// Banner visible.
    Shown,
    /// Banner suppressed by episode back-off until `backoff_until_ms`.
    BackOff,
}

/// Pure, DOM-free, clock-free anti-flap damper for the decode-budget banner.
///
/// Drive it by calling [`BannerDamper::tick`] with the current wall clock, the
/// live pressure signal, and the off-budget avatar count. It returns whether the
/// banner should be visible *right now*. All hysteresis and back-off policy is
/// internal, so the Dioxus component never has to reason about timing.
///
/// The struct holds no `Instant`/`Date` — the caller supplies `now_ms` — so the
/// whole machine is deterministic and exercised by plain `#[test]` (see the test
/// module), which is the only coverage that actually runs in this crate's CI.
#[derive(Debug, Clone)]
pub struct BannerDamper {
    phase: Phase,
    /// When the current PendingShow streak of sustained pressure began. Reset
    /// whenever pressure drops while pending.
    pending_show_since_ms: f64,
    /// When the banner was last shown (entered `Shown`). Used for [`MIN_DWELL_MS`].
    shown_since_ms: f64,
    /// When pressure most recently became continuously clear while `Shown`.
    /// `None` means pressure is currently present (or we are not Shown).
    clear_since_ms: Option<f64>,
    /// Timestamps of recent HIDDEN→SHOWN transitions, pruned to the rolling
    /// [`EPISODE_WINDOW_MS`] window. Length is bounded by the show cadence, which
    /// the hysteresis itself rate-limits, so this never grows large.
    episodes: Vec<f64>,
    /// Wall-clock time until which back-off suppresses the banner.
    backoff_until_ms: f64,
    /// Current escalation index into [`BACKOFF_COOLDOWNS_MS`].
    escalation: usize,
    /// Time of the last "activity" (episode start or back-off engage) used to
    /// decide when sustained quiet should reset [`Self::escalation`].
    last_activity_ms: f64,
}

impl Default for BannerDamper {
    fn default() -> Self {
        Self::new()
    }
}

impl BannerDamper {
    /// A fresh damper in the idle phase with no history.
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            pending_show_since_ms: 0.0,
            shown_since_ms: 0.0,
            clear_since_ms: None,
            episodes: Vec::new(),
            backoff_until_ms: 0.0,
            escalation: 0,
            last_activity_ms: f64::NEG_INFINITY,
        }
    }

    /// Whether the banner is currently visible (the last `tick` result).
    pub fn is_visible(&self) -> bool {
        matches!(self.phase, Phase::Shown)
    }

    /// Drop episode timestamps older than the rolling window relative to `now`.
    fn prune_episodes(&mut self, now_ms: f64) {
        let cutoff = now_ms - EPISODE_WINDOW_MS;
        self.episodes.retain(|&t| t >= cutoff);
    }

    /// Reset escalation back to the base cooldown once the damper has been quiet
    /// (no new episode, not actively backed off) for [`ESCALATION_RESET_MS`].
    fn maybe_reset_escalation(&mut self, now_ms: f64) {
        if self.escalation > 0
            && now_ms >= self.backoff_until_ms
            && (now_ms - self.last_activity_ms) >= ESCALATION_RESET_MS
        {
            self.escalation = 0;
        }
    }

    /// Advance the machine to `now_ms` and return whether the banner is visible.
    ///
    /// `pressured` is the raw decode-budget pressure flag and `avatar_count` is
    /// the off-budget tile count. The machine triggers only when BOTH hold
    /// (`pressured && avatar_count > 0`): a banner that announces "N videos
    /// paused" is meaningless with N == 0, so the count is a real input to the
    /// trigger, not a cosmetic knob. The caller passes the two raw signals
    /// straight through — the `> 0` gate lives HERE so it is covered by the
    /// host tests (see `zero_avatar_count_never_shows_even_when_pressured`),
    /// which `#[wasm_bindgen_test]` could not protect in this crate's CI.
    pub fn tick(&mut self, now_ms: f64, pressured: bool, avatar_count: usize) -> bool {
        let triggered = pressured && avatar_count > 0;

        self.prune_episodes(now_ms);
        self.maybe_reset_escalation(now_ms);

        match self.phase {
            Phase::Idle => {
                if triggered {
                    self.phase = Phase::PendingShow;
                    self.pending_show_since_ms = now_ms;
                }
            }
            Phase::PendingShow => {
                if !triggered {
                    // Pressure blipped away before sustaining — abandon the show.
                    self.phase = Phase::Idle;
                } else if (now_ms - self.pending_show_since_ms) >= SHOW_SUSTAIN_MS {
                    // Sustained long enough. This is a HIDDEN→SHOWN edge: record
                    // the episode, then check whether it trips back-off.
                    self.register_episode(now_ms);
                    if self.episodes.len() >= EPISODE_BACKOFF_THRESHOLD {
                        self.enter_backoff(now_ms);
                    } else {
                        self.phase = Phase::Shown;
                        self.shown_since_ms = now_ms;
                        self.clear_since_ms = None;
                    }
                }
            }
            Phase::Shown => {
                if triggered {
                    // Still pressured: clear-streak resets.
                    self.clear_since_ms = None;
                } else {
                    // Pressure clear: start (or continue) the clear streak.
                    let clear_start = *self.clear_since_ms.get_or_insert(now_ms);
                    let dwell_satisfied = (now_ms - self.shown_since_ms) >= MIN_DWELL_MS;
                    let clear_satisfied = (now_ms - clear_start) >= HIDE_SUSTAIN_MS;
                    if dwell_satisfied && clear_satisfied {
                        self.phase = Phase::Idle;
                        self.clear_since_ms = None;
                    }
                }
            }
            Phase::BackOff => {
                if now_ms >= self.backoff_until_ms {
                    // Cooldown elapsed. Re-evaluate from idle on this same tick so
                    // a still-pressured machine begins a fresh PendingShow streak
                    // (it must re-sustain; back-off never silently re-shows).
                    self.phase = Phase::Idle;
                    if triggered {
                        self.phase = Phase::PendingShow;
                        self.pending_show_since_ms = now_ms;
                    }
                }
            }
        }

        self.is_visible()
    }

    /// Record a HIDDEN→SHOWN transition for back-off accounting.
    fn register_episode(&mut self, now_ms: f64) {
        self.episodes.push(now_ms);
        self.last_activity_ms = now_ms;
    }

    /// Engage back-off: suppress the banner for the current escalation's
    /// cooldown, then escalate (capped at the last entry) for the next time.
    fn enter_backoff(&mut self, now_ms: f64) {
        let idx = self.escalation.min(BACKOFF_COOLDOWNS_MS.len() - 1);
        let cooldown = BACKOFF_COOLDOWNS_MS[idx];
        self.phase = Phase::BackOff;
        self.backoff_until_ms = now_ms + cooldown;
        self.clear_since_ms = None;
        self.last_activity_ms = now_ms;
        // Escalate for *next* engage; saturate at the final (capped) entry.
        self.escalation = (self.escalation + 1).min(BACKOFF_COOLDOWNS_MS.len() - 1);
        // The episode that tripped back-off is "spent": clear the window so the
        // post-cooldown count starts fresh and back-off does not immediately
        // re-trip on the very next show.
        self.episodes.clear();
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Dioxus component (thin driver over the pure damper).
// ──────────────────────────────────────────────────────────────────────────

/// The slim "videos paused" banner.
///
/// Props are the live render-scope signals from `attendants.rs`. The component
/// owns the [`BannerDamper`] and a `dismissed` latch; it renders the bar only
/// when the damper says visible AND the user has not dismissed it for the
/// current episode.
#[component]
pub fn DecodeBudgetBanner(
    /// Raw decode-budget pressure flag (`decode_budget_pressured`).
    pressured: bool,
    /// Number of off-budget avatar tiles (`avatar_tile_count`).
    avatar_count: usize,
    /// The uncapped natural tile count (`total_tiles ∩ CANVAS_LIMIT` is applied
    /// downstream by `decode_budget::effective_cap`). "Show all videos" sets the
    /// override to `Fixed(natural)`.
    natural: usize,
) -> Element {
    // The damper persists across renders. `use_signal` gives us a mutable,
    // render-surviving cell without re-running the constructor each render.
    let mut damper = use_signal(BannerDamper::new);

    // `visible` is the damper's authoritative output, stored in a signal so the
    // 1 Hz poll (below) can update it and trigger a re-render even when no prop
    // changed. The render body reads this — NOT a tick computed inline — so the
    // poll and the render agree on a single source of truth.
    let mut visible = use_signal(|| false);

    // Per-episode dismissal latch: when the user hits ✕ we hide the banner until
    // the damper has fully reset to hidden (a NEW episode re-arms it). Storing the
    // dismissal as a bool that we clear on the hidden edge keeps "dismiss this
    // one" from becoming "dismiss forever".
    let mut dismissed = use_signal(|| false);

    let mut decode_budget_ctx = use_context::<DecodeBudgetCtx>();

    // Mirror the live trigger props into signals the poll can read. Props are
    // captured by value when the `use_future` closure is created, so without this
    // mirror the poll would tick the damper with a STALE trigger forever. By
    // writing the current props into signals every render (only when they
    // change, to avoid a write-triggered render loop) the 1 Hz poll always sees
    // the latest pressure/avatar/natural values from attendants.rs.
    let mut trigger_pressured = use_signal(|| pressured);
    let mut trigger_avatar = use_signal(|| avatar_count);
    if *trigger_pressured.peek() != pressured {
        trigger_pressured.set(pressured);
    }
    if *trigger_avatar.peek() != avatar_count {
        trigger_avatar.set(avatar_count);
    }

    // Single 1 Hz driver. It is the ONLY place that advances the damper, so the
    // wall-clock edges (sustain → show, min-dwell, hide-clear, back-off cooldown,
    // escalation reset) fire on a steady cadence regardless of whether the parent
    // re-renders. It reads the live trigger through the mirror signals above and
    // publishes the result into `visible`, which re-renders this component. The
    // tick runs at the TOP of the loop (before the 1 s sleep) so the first tick
    // fires immediately on mount; the banner still cannot appear before the show
    // path's SHOW_SUSTAIN_MS (2 s) of sustained pressure has elapsed.
    use_future(move || async move {
        loop {
            let now = js_sys::Date::now();
            let p = *trigger_pressured.peek();
            let a = *trigger_avatar.peek();
            let v = damper.write().tick(now, p, a);
            if *visible.peek() != v {
                visible.set(v);
                // Reset the per-episode dismissal once the banner is no longer
                // visible, so the next distinct episode can show it again.
                if !v && *dismissed.peek() {
                    dismissed.set(false);
                }
            }
            gloo_timers::future::TimeoutFuture::new(1_000).await;
        }
    });

    let show_banner = visible() && !dismissed();

    if !show_banner {
        return rsx! {};
    }

    let count = avatar_count;
    let plural = if count == 1 { "video" } else { "videos" };
    let full_text =
        format!("{count} {plural} paused — your device is keeping up with audio first.");
    // Compact phrase shown ≤639px in place of the long text. "N paused" (not a
    // lone digit) keeps minimal context on touch, where the hover-only tooltip /
    // long text are unavailable.
    let compact_text = format!("{count} paused");
    let aria = format!(
        "{count} {plural} paused to keep the call smooth. Click Show all videos to override."
    );

    rsx! {
        div {
            class: "decode-budget-banner",
            "data-testid": "decode-budget-banner",
            role: "status",
            "aria-live": "polite",
            "aria-label": "{aria}",

            span { class: "decode-budget-banner-icon", aria_hidden: "true",
                // Lightning bolt — matches the "⚡" affordance in the copy
                // without relying on emoji font rendering.
                svg {
                    width: "16",
                    height: "16",
                    view_box: "0 0 24 24",
                    fill: "currentColor",
                    stroke: "none",
                    polygon { points: "13 2 3 14 12 14 11 22 21 10 12 10 13 2" }
                }
            }

            // Full message on wider viewports; compact "N paused" on mobile (CSS
            // hides the long text and reveals `.decode-budget-banner-count`
            // ≤639px). The aria-label on the root still carries the full message,
            // so this short form is aria-hidden to avoid double announcement.
            span { class: "decode-budget-banner-text", "{full_text}" }
            span { class: "decode-budget-banner-count", aria_hidden: "true", "{compact_text}" }

            button {
                r#type: "button",
                class: "decode-budget-banner-action",
                "data-testid": "decode-budget-show-all",
                // Convey the trade-off: forcing all tiles to decode can stutter
                // on a device that is already CPU-bound (which is why the budget
                // paused them). `title` for pointer users; aria-label mirrors it
                // so screen-reader users hear the same caveat as the visible
                // button name plus the consequence.
                title: "Show all videos (may stutter on a busy device)",
                "aria-label": "Show all videos — may stutter on a busy device",
                onclick: move |_| {
                    // One-click escape hatch: pin the decode budget to the natural
                    // tile count so EVERY present peer decodes. This is the exact
                    // path the appearance settings panel uses — set the shared
                    // context signal AND persist to localStorage — so the render
                    // scope's `effective_cap(Fixed(natural), …)` re-reveals all
                    // tiles on the next frame and the choice survives reloads.
                    let target = DecodeBudgetOverride::Fixed(natural.max(1));
                    decode_budget_ctx.0.set(target);
                    save_decode_budget_override(target);
                    // Hide immediately; the override clears pressure so the damper
                    // will settle to Idle on subsequent ticks.
                    dismissed.set(true);
                },
                "Show all videos"
            }

            button {
                r#type: "button",
                class: "decode-budget-banner-dismiss",
                "data-testid": "decode-budget-dismiss",
                "aria-label": "Dismiss",
                onclick: move |_| dismissed.set(true),
                // ✕ glyph as an inline SVG (crisp, theme-tinted).
                svg {
                    width: "14",
                    height: "14",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2.5",
                    stroke_linecap: "round",
                    line { x1: "18", y1: "6", x2: "6", y2: "18" }
                    line { x1: "6", y1: "6", x2: "18", y2: "18" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Sanity: the escalation array is non-empty (indexing in `enter_backoff`
    // relies on `len() - 1`).
    const _: () = assert!(!BACKOFF_COOLDOWNS_MS.is_empty());

    // ── Contract pins ────────────────────────────────────────────────────────
    //
    // The product contract (Jay's #1142 requirement) names exact thresholds:
    // 2 s show-sustain, 5 s min-dwell, 3 s hide-clear, 3-per-2-min episodes, and
    // 60 → 120 → 240 s escalating back-off. These pins assert each CONSTANT
    // equals its documented literal. The literal is the external source of
    // truth (the spec), not the constant itself, so editing a constant away from
    // its agreed value breaks the pin. The behavioural tests below independently
    // use literal millisecond values, so a mutation that changed BOTH a constant
    // and tried to "track" it in tests would still trip these pins.

    #[test]
    fn thresholds_match_documented_contract() {
        assert_eq!(SHOW_SUSTAIN_MS, 2_000.0, "show-sustain is 2s per #1142");
        assert_eq!(MIN_DWELL_MS, 5_000.0, "min dwell is 5s per #1142");
        assert_eq!(
            HIDE_SUSTAIN_MS, 3_000.0,
            "hide-clear sustain is 3s per #1142"
        );
        assert_eq!(
            EPISODE_WINDOW_MS, 120_000.0,
            "episode window is 2min per #1142"
        );
        assert_eq!(
            EPISODE_BACKOFF_THRESHOLD, 3,
            "3 episodes in the window trips back-off per #1142"
        );
        assert_eq!(
            BACKOFF_COOLDOWNS_MS,
            [60_000.0, 120_000.0, 240_000.0],
            "back-off escalates 60→120→240s per #1142"
        );
        assert_eq!(
            ESCALATION_RESET_MS, 300_000.0,
            "escalation resets after ~5min quiet per #1142"
        );
    }

    /// Drive the damper across a span of steady input, stepping `step_ms` per
    /// tick. Helper to keep quiet/escalation tests readable.
    fn run_steady(
        d: &mut BannerDamper,
        start_ms: f64,
        ticks: usize,
        step_ms: f64,
        pressured: bool,
        avatar: usize,
    ) {
        let mut now = start_ms;
        for _ in 0..ticks {
            d.tick(now, pressured, avatar);
            now += step_ms;
        }
    }

    // ── Layer 1: hysteresis (show-sustain / min-dwell / hide-clear) ───────────
    //
    // All times below are LITERALS encoding the contract (2000/5000/3000 ms), so
    // they fail if the state machine's edge logic breaks regardless of how a
    // constant is named.

    #[test]
    fn fast_flicker_within_sustain_never_shows() {
        // Pressure present for 1500ms (< 2000ms show-sustain), then clears, on a
        // loop. The banner must NEVER show because the sustain window never
        // completes uninterrupted.
        let mut d = BannerDamper::new();
        let mut now = 0.0;
        for _ in 0..20 {
            assert!(!d.tick(now, true, 3), "blip on must not show");
            now += 1_500.0; // 1.5s < 2s sustain
            assert!(!d.tick(now, false, 0), "blip cleared must not show");
            now += 1_000.0;
        }
        assert!(!d.is_visible());
    }

    #[test]
    fn sustained_pressure_shows_at_2000ms_not_before() {
        let mut d = BannerDamper::new();
        assert!(!d.tick(0.0, true, 4), "t=0: pressure just started");
        assert!(
            !d.tick(1_999.0, true, 4),
            "t=1999ms (<2000) must stay hidden"
        );
        assert!(
            d.tick(2_000.0, true, 4),
            "t=2000ms: 2s sustain elapsed -> shows"
        );
    }

    #[test]
    fn min_dwell_5000ms_holds_banner_even_if_pressure_clears_at_once() {
        let mut d = BannerDamper::new();
        d.tick(0.0, true, 4);
        assert!(d.tick(2_000.0, true, 4), "shown at 2000ms");
        // Pressure clears right after showing. Even though 3s hide-clear would
        // elapse, the 5s min dwell (from shown_at=2000ms) holds it up.
        // shown_at + 3000ms hide-sustain = 5000ms, but dwell ends at 7000ms.
        assert!(
            d.tick(5_000.0, false, 0),
            "clear-sustained at 5000ms but dwell (ends 7000ms) keeps it up"
        );
        assert!(
            d.tick(6_999.0, false, 0),
            "1ms before dwell end (7000ms) still visible"
        );
    }

    #[test]
    fn hides_only_after_dwell_and_3000ms_sustained_clear() {
        let mut d = BannerDamper::new();
        d.tick(0.0, true, 4);
        assert!(d.tick(2_000.0, true, 4), "shown at 2000ms");
        // Clear begins at 10_000ms — well past the 7000ms dwell end, so dwell is
        // not the binding edge; the 3000ms clear-sustain is.
        assert!(d.tick(10_000.0, false, 0), "clear just started, still up");
        assert!(
            d.tick(12_999.0, false, 0),
            "1ms before 3000ms clear-sustain -> still up"
        );
        assert!(
            !d.tick(13_000.0, false, 0),
            "3000ms clear-sustain elapsed (and dwell long met) -> hides"
        );
    }

    #[test]
    fn brief_recovery_during_shown_resets_clear_streak() {
        let mut d = BannerDamper::new();
        d.tick(0.0, true, 4);
        assert!(d.tick(2_000.0, true, 4), "shown at 2000ms");
        // Past dwell. Clear for 2000ms (< 3000ms), then pressure returns.
        assert!(d.tick(10_000.0, false, 0), "clear streak begins at 10000ms");
        assert!(
            d.tick(12_000.0, true, 4),
            "pressure back at 12000ms (only 2000ms clear) -> stays up"
        );
        // The clear streak reset: a FULL fresh 3000ms of clear is now required.
        assert!(
            d.tick(13_000.0, false, 0),
            "new clear streak begins at 13000ms"
        );
        assert!(
            d.tick(15_999.0, false, 0),
            "1ms before the new 3000ms streak completes -> still up"
        );
        assert!(
            !d.tick(16_000.0, false, 0),
            "new 3000ms clear streak complete -> hides"
        );
    }

    // ── Layer 2: episode back-off (3 per 2min, 60→120→240s) ───────────────────

    /// Show then fully hide the banner once, starting at `start_ms`. Returns the
    /// time just after the hide. Each call is one full "episode." Uses literal
    /// offsets matching the contract: show at +2000ms, clear at +(dwell), hide at
    /// +(dwell + 3000ms clear).
    fn one_episode(d: &mut BannerDamper, start_ms: f64) -> f64 {
        d.tick(start_ms, true, 3); // pressure begins
        let shown = start_ms + 2_000.0;
        d.tick(shown, true, 3); // shows
                                // Clear begins after the 5000ms dwell so dwell is satisfied; then a
                                // full 3000ms of clear hides it.
        let clear_begin = shown + 5_000.0;
        d.tick(clear_begin, false, 0);
        let hide_at = clear_begin + 3_000.0;
        d.tick(hide_at, false, 0); // hides
        hide_at + 10.0
    }

    #[test]
    fn third_episode_in_2min_window_engages_backoff() {
        let mut d = BannerDamper::new();
        let t = one_episode(&mut d, 0.0);
        assert!(!d.is_visible(), "episode 1 hidden");
        let t = one_episode(&mut d, t);
        assert!(!d.is_visible(), "episode 2 hidden");
        // All three show-edges fall within 2min (each episode is ~10s).
        assert!(t < 120_000.0, "third show-edge within the 2min window");
        // Third episode: the show edge should trip back-off (suppressed).
        d.tick(t, true, 3);
        let third_show = t + 2_000.0;
        assert!(
            !d.tick(third_show, true, 3),
            "3rd episode in the 2min window engages back-off (suppressed)"
        );
    }

    #[test]
    fn episodes_outside_window_do_not_accumulate() {
        // Two episodes, then a THIRD more than 2min after the first: the first
        // has aged out of the window, so the third shows normally (no back-off).
        let mut d = BannerDamper::new();
        let _ = one_episode(&mut d, 0.0); // episode 1 at ~t=0
        let _ = one_episode(&mut d, 10_000.0); // episode 2 at ~t=10s
                                               // Episode 3 starts at t=130_000ms (>120s after episode 1's show-edge at
                                               // 2000ms, so only episodes 2 and 3 are in the window: 2 < 3, no back-off).
        d.tick(130_000.0, true, 3);
        assert!(
            d.tick(132_000.0, true, 3),
            "only 2 episodes in the rolling window -> shows, no back-off"
        );
    }

    /// Drive two quiet episodes then a third whose show-edge engages back-off.
    /// Returns the back-off engage time.
    fn engage_backoff(d: &mut BannerDamper, base: f64) -> f64 {
        let mut t = base;
        for _ in 0..2 {
            t = one_episode(d, t);
        }
        d.tick(t, true, 3);
        let engage = t + 2_000.0;
        d.tick(engage, true, 3);
        engage
    }

    #[test]
    fn backoff_cooldown_escalates_60_then_120_then_240s() {
        let mut d = BannerDamper::new();

        // First back-off: 60s. Suppressed at +59.999s, the machine has NOT left
        // back-off before +60s.
        let e1 = engage_backoff(&mut d, 0.0);
        d.tick(e1 + 59_999.0, false, 0);
        assert!(
            matches!(d.phase, Phase::BackOff),
            "still backed off before 60s"
        );
        // Leave back-off after 60s.
        d.tick(e1 + 60_001.0, false, 0);
        assert!(
            !matches!(d.phase, Phase::BackOff),
            "out of back-off after 60s"
        );

        // Second back-off should use 120s (escalated). At +60s it must STILL be
        // backed off (would NOT be, if it had stayed at 60s).
        let e2 = engage_backoff(&mut d, e1 + 60_001.0 + 100.0);
        d.tick(e2 + 60_000.0, false, 0);
        assert!(
            matches!(d.phase, Phase::BackOff),
            "second back-off escalated to 120s, so 60s is not enough"
        );
        d.tick(e2 + 119_999.0, false, 0);
        assert!(
            matches!(d.phase, Phase::BackOff),
            "still backed off before 120s"
        );
        d.tick(e2 + 120_001.0, false, 0);
        assert!(!matches!(d.phase, Phase::BackOff), "out after 120s");

        // Third back-off should use 240s (the cap). At +120s still backed off.
        let e3 = engage_backoff(&mut d, e2 + 120_001.0 + 100.0);
        d.tick(e3 + 120_000.0, false, 0);
        assert!(
            matches!(d.phase, Phase::BackOff),
            "third back-off escalated to 240s, so 120s is not enough"
        );
        d.tick(e3 + 239_999.0, false, 0);
        assert!(
            matches!(d.phase, Phase::BackOff),
            "still backed off before 240s"
        );
        d.tick(e3 + 240_001.0, false, 0);
        assert!(!matches!(d.phase, Phase::BackOff), "out after the 240s cap");
    }

    #[test]
    fn sustained_quiet_resets_escalation_to_base_60s() {
        let mut d = BannerDamper::new();
        // First back-off -> escalation advances.
        let e1 = engage_backoff(&mut d, 0.0);
        let after1 = e1 + 60_001.0;
        // Stay quiet (no pressure) past the 5min escalation-reset horizon.
        run_steady(&mut d, after1, 6, 60_000.0, false, 0); // ~6min of quiet
                                                           // Next back-off must use the BASE 60s again: at +60.001s it is OUT.
        let base = after1 + 300_000.0 + 5_000.0;
        let e2 = engage_backoff(&mut d, base);
        d.tick(e2 + 60_001.0, false, 0);
        assert!(
            !matches!(d.phase, Phase::BackOff),
            "escalation reset to base 60s after sustained quiet"
        );
    }

    #[test]
    fn backoff_is_never_permanent() {
        let mut d = BannerDamper::new();
        let engage = engage_backoff(&mut d, 0.0);
        assert!(matches!(d.phase, Phase::BackOff), "engaged");
        // Hold steady pressure across the 60s cooldown; the machine re-enters
        // PendingShow and shows again after a fresh 2000ms sustain.
        d.tick(engage + 60_001.0, true, 3); // leaves back-off -> PendingShow
        assert!(
            d.tick(engage + 60_001.0 + 2_000.0, true, 3),
            "back-off is temporary: shows again after cooldown + re-sustain"
        );
    }

    #[test]
    fn zero_avatar_count_never_shows_even_when_pressured() {
        let mut d = BannerDamper::new();
        run_steady(&mut d, 0.0, 10, 2_000.0, true, 0);
        assert!(!d.is_visible(), "pressured but 0 avatars must never show");
    }
}
