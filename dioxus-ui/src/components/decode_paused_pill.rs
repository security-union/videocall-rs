// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persistent "N videos paused" pill + its appear/disappear debounce machine
//! (issue #1142, "FINAL DESIGN").
//!
//! ## The two surfaces and why they are mutually exclusive
//!
//! Issue #1142 ships TWO meeting-level affordances for the adaptive
//! decode-budget cap (see [`crate::components::decode_budget`]):
//!
//! 1. The **banner** ([`crate::components::decode_budget_banner::DecodeBudgetBanner`])
//!    — an *onset alert*. It is heavily anti-flapped (sustain / min-dwell /
//!    episode back-off) so it fires once when the cap first engages, announces
//!    "N tiles paused" via `aria-live=polite`, and then BACKS OFF: after a few
//!    episodes in a 2-minute window it suppresses itself so a chronically-capped
//!    call is not nagged by a re-appearing bar.
//!
//! 2. The **pill** (this module) — a *persistent level signpost*. Once the cap
//!    has been engaged long enough to be real (≥2 s of off-budget tiles) the
//!    pill appears and STAYS, on every tick, for as long as tiles are paused.
//!    It has NO back-off, NO episode counting, NO escalation, and NO dismiss
//!    latch: it is the always-available "this is the current state, here is the
//!    escape hatch" indicator. It is the inverse of the banner's back-off.
//!
//! These two must never co-exist on screen: when the banner is up, the pill
//! suppresses; when the banner backs off (or hides), the pill takes over and
//! holds the signpost. The spec encodes this as `pill_visible = eligible &&
//! !banner_visible`.
//!
//! The exclusivity is asymmetric in timing, and it is worth being precise:
//!
//! * **Co-existence is prevented IMMEDIATELY.** The pill's render gate reads the
//!   shared `banner_on_screen` signal REACTIVELY, so the instant the banner
//!   appears the pill re-renders on the SAME frame and suppresses itself. Both
//!   surfaces are never visible at once.
//! * **The reverse takeover (banner hides → pill re-appears) has up to ~1 s of
//!   latency** from the pill's 1 Hz poll, which is what advances the eligibility
//!   machine and flips `visible`. So when the banner leaves there can be a brief
//!   GAP where NEITHER surface shows — never an OVERLAP.
//!
//! In other words: the overlap direction is closed on the same frame; the gap
//! direction is bounded by one poll interval.
//!
//! ## How the pill knows whether the banner is visible (shared signal)
//!
//! The pill needs `banner_visible` — the banner's TRUE on-screen state. Rather
//! than approximate it, the common parent (`AttendantsComponent` in
//! `attendants.rs`) owns a single `Signal<bool>` (`banner_on_screen`). The
//! banner is the WRITER: every tick it publishes its real effective visibility
//! `damper_visible && !dismissed && avatar_count > 0` into that signal. The pill
//! is the READER on two paths: (1) its 1 Hz poll reads the signal via `.peek()`
//! to feed the eligibility machine's `banner_visible` suppression input, and (2)
//! its render gate reads the signal REACTIVELY so a banner appearance suppresses
//! the pill on the same frame. Because this is the banner's ACTUAL on-screen
//! state, all three ways the banner can leave the screen are honored exactly —
//! back-off, natural hide, AND the user dismissing it (the ✕ button) while tiles
//! are still paused (issue #1142 Gap 1).
//!
//! The guarantee this buys is precise (NOT "exactly mutually exclusive / zero
//! skew", which overstated it):
//!
//! * Co-existence is prevented IMMEDIATELY — the reactive render-gate read
//!   suppresses the pill on the SAME frame the banner appears.
//! * The reverse takeover (banner hides → pill re-appears) has up to ~1 s
//!   latency from the 1 Hz poll, so there is a brief GAP where NEITHER surface
//!   shows — never an OVERLAP.
//! * The 3 s disappear debounce (`PendingHide`) now actually governs the
//!   rendered component, because the retained nonzero count prevents a transient
//!   one-frame "0 videos paused" flash that previously made that tested
//!   behavior inert.
//!
//! There is no shadow approximation. The pill's own eligibility machine
//! ([`PillVisibility`]) still gates purely on `avatar_count`; `banner_on_screen`
//! gates only the final visible output.
//!
//! ## Why the eligibility machine is a pure struct, not Dioxus state
//!
//! Exactly as for the banner: `#[wasm_bindgen_test]` silently no-ops in this
//! crate's CI (a false-green), so a state machine validated only through a
//! rendered component would have NO real coverage. [`PillVisibility`] is a plain
//! `struct` with a single [`PillVisibility::tick`] transition
//! `(now_ms, avatar_count, banner_visible) -> visible`, exercised by ordinary
//! host `#[test]` cases. The Dioxus component is a thin driver.

use dioxus::prelude::*;

use crate::context::{save_decode_budget_override, DecodeBudgetCtx, DecodeBudgetOverride};

// ──────────────────────────────────────────────────────────────────────────
// Tunable thresholds.
//
// FIRST-GUESS values pending real-world measurement, matching the honesty
// posture of the banner's threshold comments (and #1159 / `decode_budget.rs`).
// Named constants so a future performance/UX pass can tune them in one place.
// DO NOT treat any of these as final.
// ──────────────────────────────────────────────────────────────────────────

/// How long off-budget tiles must be **continuously** present before the pill
/// is allowed to appear. A short blip (a GC pause, one slow frame) must not
/// flash the pill. Shorter than the banner is unnecessary — the pill is the
/// steady-state signpost, not the urgent alert.
///
/// Tunable first-guess, pending real-world measurement.
pub const PILL_APPEAR_MS: f64 = 2_000.0;

/// How long the off-budget tile count must be **continuously** zero before an
/// already-shown pill hides. Damps the hide edge so a one-frame recovery does
/// not blink the pill off and back on. There is deliberately NO min-dwell and
/// NO back-off: the pill is allowed to track the real state closely on the way
/// down, it just refuses to chatter.
///
/// Tunable first-guess, pending real-world measurement.
pub const PILL_DISAPPEAR_MS: f64 = 3_000.0;

// ──────────────────────────────────────────────────────────────────────────
// Pure eligibility state machine.
// ──────────────────────────────────────────────────────────────────────────

/// Coarse phase of the eligibility machine. Exact edge timing lives in the
/// timestamp fields of [`PillVisibility`]; this enum just records which arm of
/// the machine we are in. It is driven PURELY by `avatar_count` streaks —
/// `banner_visible` never touches it (see the steady-state-persistence contract
/// in [`PillVisibility::tick`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No off-budget tiles; pill not eligible.
    Hidden,
    /// `avatar_count > 0` is present; counting up to [`PILL_APPEAR_MS`] before
    /// becoming eligible.
    PendingShow,
    /// Off-budget tiles sustained long enough — the pill is ELIGIBLE (it will
    /// show iff the banner is not visible).
    Eligible,
    /// Eligible but `avatar_count` has dropped to zero; counting up to
    /// [`PILL_DISAPPEAR_MS`] of continuous zero before going back to `Hidden`.
    PendingHide,
}

/// Pure, DOM-free, clock-free appear/disappear debounce for the decode-paused
/// pill.
///
/// Drive it by calling [`PillVisibility::tick`] with the current wall clock, the
/// off-budget tile count, and whether the banner is currently visible. It
/// returns whether the pill should be visible *right now*
/// (`eligible && !banner_visible`).
///
/// The struct holds no `Instant`/`Date` — the caller supplies `now_ms` — so the
/// whole machine is deterministic and exercised by plain `#[test]` (see the test
/// module), which is the only coverage that actually runs in this crate's CI.
///
/// Mirrors [`crate::components::decode_budget_banner::BannerDamper`]'s
/// philosophy but deliberately drops every back-off / episode / escalation /
/// min-dwell mechanism: the pill is the persistent level signpost, so it must
/// NOT go silent under sustained pressure.
#[derive(Debug, Clone)]
pub struct PillVisibility {
    phase: Phase,
    /// When the current PendingShow streak of sustained off-budget tiles began.
    /// Reset whenever `avatar_count` drops to zero while pending.
    pending_show_since_ms: f64,
    /// When `avatar_count` most recently became continuously zero while
    /// `Eligible`. `None` means tiles are currently present (or we are not in
    /// `PendingHide`).
    zero_since_ms: Option<f64>,
    /// The exact value the LAST [`PillVisibility::tick`] returned
    /// (`eligible && !banner_visible`). [`PillVisibility::is_visible`] reports
    /// THIS, not the phase, so the banner-suppression gate is reflected in
    /// `is_visible` between ticks.
    last_visible: bool,
}

impl Default for PillVisibility {
    fn default() -> Self {
        Self::new()
    }
}

impl PillVisibility {
    /// A fresh machine in the hidden phase with no history.
    pub fn new() -> Self {
        Self {
            phase: Phase::Hidden,
            pending_show_since_ms: 0.0,
            zero_since_ms: None,
            last_visible: false,
        }
    }

    /// Whether the pill is currently visible — EXACTLY what the last
    /// [`PillVisibility::tick`] returned (`eligible && !banner_visible`), NOT
    /// merely whether the phase is `Eligible`. Stored in `last_visible` so the
    /// banner-suppression gate is honoured between ticks.
    pub fn is_visible(&self) -> bool {
        self.last_visible
    }

    /// Advance the machine to `now_ms` and return whether the pill is visible.
    ///
    /// `avatar_count` is the off-budget tile count; `banner_visible` is whether
    /// the meeting banner is currently up. The output is
    /// `eligible && !banner_visible`.
    ///
    /// ## Steady-state-persistence contract (the inverse of the banner)
    ///
    /// `banner_visible` gates ONLY the final returned/stored output. It does
    /// NOT touch the eligibility phase or the appear/disappear timers — those
    /// run PURELY on `avatar_count`. Consequences, all verified by the tests:
    ///
    /// * When the banner later hides, an already-`Eligible` pill appears
    ///   IMMEDIATELY on that same tick — there is NO re-debounce, because the
    ///   eligibility streak was never disturbed by the banner.
    /// * A chronically-capped machine whose banner has backed off shows the
    ///   pill on EVERY tick, forever. There is no back-off here; once eligible
    ///   and the banner is down, the pill stays up for as long as
    ///   `avatar_count > 0` persists.
    pub fn tick(&mut self, now_ms: f64, avatar_count: usize, banner_visible: bool) -> bool {
        let present = avatar_count > 0;

        // ── Eligibility machine: driven ONLY by `avatar_count`. ──────────────
        match self.phase {
            Phase::Hidden => {
                if present {
                    self.phase = Phase::PendingShow;
                    self.pending_show_since_ms = now_ms;
                }
            }
            Phase::PendingShow => {
                if !present {
                    // Tiles cleared before the appear debounce completed —
                    // abandon the pending show.
                    self.phase = Phase::Hidden;
                } else if (now_ms - self.pending_show_since_ms) >= PILL_APPEAR_MS {
                    self.phase = Phase::Eligible;
                    self.zero_since_ms = None;
                }
            }
            Phase::Eligible => {
                if present {
                    // Still capped: any nascent zero-streak resets.
                    self.zero_since_ms = None;
                } else {
                    // Off-budget tiles just hit zero: begin the disappear
                    // debounce.
                    self.phase = Phase::PendingHide;
                    self.zero_since_ms = Some(now_ms);
                }
            }
            Phase::PendingHide => {
                if present {
                    // A blip back to >0 before the disappear debounce completed:
                    // stay eligible and reset the zero-streak. The NEXT zero
                    // edge must re-sustain a fresh full PILL_DISAPPEAR_MS.
                    self.phase = Phase::Eligible;
                    self.zero_since_ms = None;
                } else {
                    let zero_start = *self.zero_since_ms.get_or_insert(now_ms);
                    if (now_ms - zero_start) >= PILL_DISAPPEAR_MS {
                        self.phase = Phase::Hidden;
                        self.zero_since_ms = None;
                    }
                }
            }
        }

        // ── Final output gate. `banner_visible` suppresses the visible pill but
        //    leaves the eligibility phase untouched (above), so the pill can pop
        //    immediately when the banner hides. `PendingHide` is still "showing"
        //    (it is eligible, just counting down), so it counts as eligible. ──
        let eligible = matches!(self.phase, Phase::Eligible | Phase::PendingHide);
        self.last_visible = eligible && !banner_visible;
        // Return through `is_visible` (which reads `last_visible`) — the same
        // idiom the banner's `tick` uses — so `tick` and `is_visible` are
        // guaranteed to agree and `is_visible` is exercised by the lib build.
        self.is_visible()
    }
}

/// Pure render decision for the pill, given the machine output and live inputs.
/// `machine_visible` is `PillVisibility::is_visible()` (the 1 Hz machine's
/// gated output). `banner_on_screen` is the banner's published true on-screen
/// state. `avatar_count` is the live off-budget count. `last_nonzero` is the
/// retained last-nonzero count. Returns `Some(display_count)` if the pill
/// should render (with that count), else `None`.
///
/// Encodes the two round-2 fixes: (1) the banner suppresses the pill
/// (`banner_on_screen` => None), so the surfaces never co-exist; (2) during
/// the machine's PendingHide debounce `avatar_count` is 0 but `machine_visible`
/// is still true, so the displayed count falls back to `last_nonzero` instead
/// of hiding — letting the 3 s disappear-debounce actually govern the
/// component rather than the live count front-running it.
pub fn pill_render_decision(
    machine_visible: bool,
    banner_on_screen: bool,
    avatar_count: usize,
    last_nonzero: usize,
) -> Option<usize> {
    let effective_count = if avatar_count > 0 {
        avatar_count
    } else {
        last_nonzero
    };
    let show = machine_visible && !banner_on_screen && effective_count > 0;
    show.then_some(effective_count)
}

// ──────────────────────────────────────────────────────────────────────────
// Dioxus component (thin driver over the pure machine).
// ──────────────────────────────────────────────────────────────────────────

/// The persistent "N videos paused" pill.
///
/// Props are the live render-scope signals from `attendants.rs`. The component
/// owns a single pure state object — a [`PillVisibility`] eligibility machine
/// that gates on `avatar_count`. The `banner_visible` suppression input is NOT
/// approximated locally: the banner publishes its TRUE on-screen visibility into
/// the shared `banner_on_screen` signal (owned by the common parent) and the
/// pill reads it on two paths — its 1 Hz poll (via `.peek()`) drives the
/// machine, and its render gate reads it REACTIVELY so a banner appearance
/// suppresses the pill on the SAME frame. That makes dismiss, back-off, and
/// natural-hide all honored: co-existence is prevented immediately, while the
/// reverse takeover (banner hides → pill re-appears) is bounded by the ~1 s poll
/// interval — a brief gap, never an overlap (see the module docs).
///
/// Unlike the banner, the pill has NO `dismissed` latch — it is persistent.
#[component]
pub fn DecodePausedPill(
    /// Number of off-budget avatar tiles — the live count of paused-video tiles
    /// the user actually sees (the same value the banner receives).
    avatar_count: usize,
    /// The uncapped natural tile count. Carries the count named in the action
    /// button's accessible label ("Show all N videos"); the action itself sets
    /// the override to `All`, which tracks the live count.
    natural: usize,
    /// The banner's TRUE on-screen visibility, published by the sibling
    /// [`crate::components::decode_budget_banner::DecodeBudgetBanner`] into a
    /// shared `Signal<bool>` owned by `attendants.rs`. The pill reads it as
    /// `banner_visible`, so the banner being on screen — whether engaged,
    /// dismissed by the user, or naturally hidden — is honored exactly and the
    /// two surfaces are mutually exclusive (issue #1142 Gap 1).
    banner_on_screen: Signal<bool>,
) -> Element {
    // The eligibility machine persists across renders. `use_signal` gives a
    // mutable, render-surviving cell without re-running the constructor each
    // render.
    let mut pill = use_signal(PillVisibility::new);

    // `visible` is the pill's authoritative output, stored in a signal so the
    // 1 Hz poll (below) can update it and trigger a re-render even when no prop
    // changed. The render body reads this — NOT a tick computed inline — so the
    // poll and the render agree on a single source of truth.
    let mut visible = use_signal(|| false);

    let mut decode_budget_ctx = use_context::<DecodeBudgetCtx>();

    // Mirror the live `avatar_count` prop into a signal the poll can read. Props
    // are captured by value when the `use_future` closure is created, so without
    // this mirror the poll would tick with a STALE count forever. Writing the
    // current prop into a signal every render (only when it changes, to avoid a
    // write-triggered render loop) keeps the 1 Hz poll seeing the latest avatar
    // value from attendants.rs. `banner_on_screen` needs NO mirror — it is a
    // `Signal<bool>` (a live handle), so the poll reads the parent's latest value
    // directly.
    let mut trigger_avatar = use_signal(|| avatar_count);
    if *trigger_avatar.peek() != avatar_count {
        trigger_avatar.set(avatar_count);
    }

    // Retained last-nonzero count. While the machine is in its 3 s `PendingHide`
    // debounce the live `avatar_count` can momentarily read 0 (a recovery blip)
    // even though the pill should keep showing the count it was last displaying.
    // Holding the last nonzero value here keeps the rendered label stable through
    // that debounce instead of flashing "0 videos paused".
    let mut last_nonzero_count = use_signal(|| 0usize);

    // Single 1 Hz driver. It is the ONLY place that advances the eligibility
    // machine, so the wall-clock edges (appear / disappear debounce) fire on a
    // steady cadence regardless of parent re-renders. The tick runs at the TOP
    // of the loop (before the 1 s sleep) so the first tick fires immediately on
    // mount; the pill still cannot appear before PILL_APPEAR_MS (2 s) of
    // sustained off-budget tiles has elapsed.
    use_future(move || async move {
        loop {
            let now = js_sys::Date::now();
            let a = *trigger_avatar.peek();
            // The banner publishes its TRUE on-screen visibility into
            // `banner_on_screen` (dismiss-aware); read it live here as the
            // machine's suppression input. (The render gate ALSO reads the same
            // signal reactively, which is what closes the co-existence window on
            // the same frame; this poll-side read bounds the reverse takeover to
            // ~1 s.)
            let banner_visible = *banner_on_screen.peek();
            let v = pill.write().tick(now, a, banner_visible);
            if *visible.peek() != v {
                visible.set(v);
            }
            gloo_timers::future::TimeoutFuture::new(1_000).await;
        }
    });

    // Retain the latest nonzero live count (only-on-change, peek-guarded so this
    // write never re-triggers the render that performed it). When the live count
    // is 0 during a `PendingHide` blip we fall back to this retained value below.
    if avatar_count > 0 && *last_nonzero_count.peek() != avatar_count {
        last_nonzero_count.set(avatar_count);
    }

    // The render decision goes through the pure `pill_render_decision` fn so it
    // is pinned by host tests rather than living only in this component body.
    // The reactive reads STAY here and are passed by value: `visible()` is the
    // 1 Hz machine's gated output, and `banner_on_screen()` is read REACTIVELY
    // (NOT `.peek()`) — when the banner appears mid-poll-interval the pill
    // re-renders on the SAME frame and the gate suppresses it, so the two
    // surfaces never co-exist. The pure fn folds in the retained-count fallback
    // (so the rendered label holds the last nonzero value through the machine's
    // 3 s PendingHide debounce instead of front-running it when the live count
    // momentarily hits 0) and the `!banner_on_screen` suppression gate, and
    // returns the display count to render (or `None` to hide).
    let decision = pill_render_decision(
        visible(),
        banner_on_screen(),
        avatar_count,
        *last_nonzero_count.peek(),
    );
    let Some(count) = decision else {
        return rsx! {};
    };
    // Factual, present-tense, noun = "videos". NEVER imply a network cause —
    // the device paused decoding, the network did not drop frames.
    let video_word = if count == 1 { "video" } else { "videos" };
    let full_text = format!("{count} {video_word} paused");
    // Compact phrase shown ≤639px in place of the full label.
    let compact_text = format!("{count} paused");
    // Issue #1142: accessible label for the action, naming the natural tile
    // count so screen-reader users hear how many videos "Show all" reveals.
    let show_all_aria = format!("Show all {natural} videos — may stutter on a busy device");

    rsx! {
        div {
            class: "decode-paused-pill",
            "data-testid": "decode-paused-pill",
            role: "status",
            // Announce-once posture: the banner already announced onset via
            // aria-live=polite. The pill is a persistent visual signpost, so
            // `aria-live=off` prevents screen-reader spam as the count changes
            // while peers join/leave.
            "aria-live": "off",

            span { class: "decode-paused-pill-icon", aria_hidden: "true",
                // Pause glyph — two rounded bars (NOT the banner's lightning
                // bolt; the pill signals "paused", not "under pressure").
                svg {
                    width: "16",
                    height: "16",
                    view_box: "0 0 24 24",
                    fill: "currentColor",
                    stroke: "none",
                    rect { x: "6", y: "5", width: "4", height: "14", rx: "1" }
                    rect { x: "14", y: "5", width: "4", height: "14", rx: "1" }
                }
            }

            // Full label on wider viewports; compact "N paused" on mobile (CSS
            // hides the long label and reveals `.decode-paused-pill-count`
            // ≤639px). The compact form is aria-hidden so it does not
            // double-announce.
            span { class: "decode-paused-pill-label", "{full_text}" }
            span { class: "decode-paused-pill-count", aria_hidden: "true", "{compact_text}" }

            button {
                r#type: "button",
                class: "decode-paused-pill-action",
                "data-testid": "decode-paused-pill-show-all",
                // Convey the trade-off: forcing all tiles to decode can stutter
                // on a device that is already CPU-bound. `title` for pointer
                // users; `aria-label` mirrors it with the count for AT.
                title: "Show all videos (may stutter on a busy device)",
                "aria-label": "{show_all_aria}",
                onclick: move |_| {
                    // One-click escape hatch: pin the decode budget to `All`
                    // (issue #1466) so EVERY present peer decodes — and stays
                    // decoded as peers join, since `All` tracks the live natural
                    // count rather than a frozen `Fixed(n)`. This is the exact
                    // path the banner and the appearance settings panel use —
                    // set the shared context signal AND persist to localStorage
                    // — so the render scope's `effective_cap(All, …)` re-reveals
                    // all tiles on the next frame and the choice survives
                    // reloads. The pill needs NO dismiss: once the override
                    // drives `avatar_count` to 0, the machine settles to Hidden
                    // on later ticks on its own.
                    let target = DecodeBudgetOverride::All;
                    decode_budget_ctx.0.set(target);
                    save_decode_budget_override(target);
                },
                span { class: "decode-paused-pill-action-icon", aria_hidden: "true",
                    // Right-pointing play triangle (same glyph as the per-tile
                    // `.decode-play-overlay`).
                    svg {
                        width: "14",
                        height: "14",
                        view_box: "0 0 24 24",
                        fill: "currentColor",
                        stroke: "none",
                        polygon { points: "8 5 19 12 8 19 8 5" }
                    }
                }
                "Show all"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are plain `#[test]` host tests. `#[wasm_bindgen_test]` silently
    // no-ops in this crate's CI (a false-green), so the host `#[test]` cases
    // below are the ONLY real coverage — same posture as the banner's tests.

    // ── Contract pin ─────────────────────────────────────────────────────────
    //
    // The product contract (#1142 FINAL DESIGN) names exact debounces: ~2 s
    // appear, ~3 s disappear. This pins each CONSTANT to its documented literal.
    // The literal is the external source of truth (the spec), not the constant
    // itself, so editing a constant away from its agreed value breaks the pin.
    // The behavioural tests below independently use literal millisecond values.

    #[test]
    fn thresholds_match_documented_contract() {
        assert_eq!(PILL_APPEAR_MS, 2_000.0, "appear debounce is 2s per #1142");
        assert_eq!(
            PILL_DISAPPEAR_MS, 3_000.0,
            "disappear debounce is 3s per #1142"
        );
    }

    /// Drive the machine across a span of steady input, stepping `step_ms` per
    /// tick. Returns whether the pill was visible on the FINAL tick. Helper to
    /// keep the steady-state test readable (mirrors the banner's `run_steady`).
    fn run_steady(
        p: &mut PillVisibility,
        start_ms: f64,
        ticks: usize,
        step_ms: f64,
        avatar: usize,
        banner_visible: bool,
    ) -> bool {
        let mut now = start_ms;
        let mut last = false;
        for _ in 0..ticks {
            last = p.tick(now, avatar, banner_visible);
            now += step_ms;
        }
        last
    }

    // ── Appear debounce (2000ms) ──────────────────────────────────────────────

    #[test]
    fn appear_debounce_2000ms_then_shows() {
        // avatar_count > 0, banner not visible. < 2000ms must never show;
        // sustained ≥ 2000ms shows. Literal 2000 encodes the contract.
        let mut p = PillVisibility::new();
        assert!(!p.tick(0.0, 3, false), "t=0: tiles just went off-budget");
        assert!(
            !p.tick(1_999.0, 3, false),
            "t=1999ms (<2000) must stay hidden"
        );
        assert!(
            p.tick(2_000.0, 3, false),
            "t=2000ms: 2s appear debounce elapsed -> shows"
        );
        assert!(p.is_visible(), "is_visible reflects the last tick output");
    }

    #[test]
    fn appear_streak_resets_on_blip_to_zero() {
        // A drop to zero before 2000ms abandons the pending show; the appear
        // debounce must restart from scratch.
        let mut p = PillVisibility::new();
        assert!(!p.tick(0.0, 3, false), "pending show begins");
        assert!(!p.tick(1_000.0, 0, false), "blip to zero before 2000ms");
        // Fresh streak from 1500ms; at 1500+1999 it is still < 2000ms sustained.
        assert!(
            !p.tick(1_500.0, 3, false),
            "new pending show begins at 1500ms"
        );
        assert!(
            !p.tick(3_499.0, 3, false),
            "only 1999ms of the new streak -> still hidden"
        );
        assert!(
            p.tick(3_500.0, 3, false),
            "new 2000ms streak complete -> shows"
        );
    }

    // ── Disappear debounce (3000ms) ───────────────────────────────────────────

    #[test]
    fn disappear_debounce_3000ms_holds_then_hides() {
        // Once shown, avatar_count -> 0 stays visible until 3000ms continuous
        // zero elapses, then hides.
        let mut p = PillVisibility::new();
        p.tick(0.0, 3, false);
        assert!(p.tick(2_000.0, 3, false), "shown at 2000ms");
        // Zero begins at 10_000ms.
        assert!(p.tick(10_000.0, 0, false), "zero just started -> still up");
        assert!(
            p.tick(12_999.0, 0, false),
            "1ms before 3000ms zero-sustain -> still up"
        );
        assert!(
            !p.tick(13_000.0, 0, false),
            "3000ms continuous zero elapsed -> hides"
        );
        assert!(!p.is_visible(), "is_visible reflects the hide");
    }

    #[test]
    fn disappear_streak_resets_on_blip_back_to_nonzero() {
        // A blip back to >0 before 3000ms resets the disappear streak: the pill
        // stays visible and the NEXT zero edge must re-sustain a fresh 3000ms.
        let mut p = PillVisibility::new();
        p.tick(0.0, 3, false);
        assert!(p.tick(2_000.0, 3, false), "shown at 2000ms");
        assert!(p.tick(10_000.0, 0, false), "zero streak begins at 10000ms");
        assert!(
            p.tick(12_000.0, 3, false),
            "tiles back at 12000ms (only 2000ms zero) -> stays up, streak resets"
        );
        // The disappear streak restarted: a FULL fresh 3000ms zero is now needed.
        assert!(
            p.tick(13_000.0, 0, false),
            "new zero streak begins at 13000ms"
        );
        assert!(
            p.tick(15_999.0, 0, false),
            "1ms before the new 3000ms streak completes -> still up"
        );
        assert!(
            !p.tick(16_000.0, 0, false),
            "new 3000ms zero streak complete -> hides"
        );
    }

    // ── Banner suppression + immediate-takeover (the KEY steady-state test) ────

    #[test]
    fn banner_suppresses_pill_then_releases_immediately() {
        // While banner_visible=true the pill output is false even when eligible.
        let mut p = PillVisibility::new();
        // Become eligible while the banner is up: the eligibility streak runs on
        // avatar_count regardless of banner state.
        assert!(!p.tick(0.0, 3, true), "banner up: suppressed at t=0");
        assert!(
            !p.tick(2_000.0, 3, true),
            "eligible at 2000ms but banner up -> still suppressed"
        );
        assert!(
            !p.tick(5_000.0, 3, true),
            "still eligible, banner still up -> suppressed"
        );
        assert!(
            !p.is_visible(),
            "is_visible is the suppressed output, not the phase"
        );
        // Banner flips to false on this tick: the already-eligible pill must
        // appear IMMEDIATELY, with NO re-debounce.
        assert!(
            p.tick(6_000.0, 3, false),
            "banner hides -> already-eligible pill appears on the SAME tick"
        );
        assert!(p.is_visible(), "now visible");
    }

    // ── Steady-state persistence (the inverse of the banner's back-off) ────────

    #[test]
    fn steady_state_persistence_no_backoff_over_minutes() {
        // Drive avatar_count>0, banner_visible=false steadily for > 2 minutes
        // and assert the pill is visible on EVERY tick after the appear
        // debounce. It must NOT back off or go silent (the inverse of the
        // banner's `third_episode_in_2min_window_engages_backoff`).
        let mut p = PillVisibility::new();
        // Become eligible (2s of sustained tiles).
        assert!(!p.tick(0.0, 4, false));
        assert!(p.tick(2_000.0, 4, false), "shown after appear debounce");
        // Now tick once per second for ~3 minutes and assert it never drops.
        let mut now = 3_000.0;
        for i in 0..180 {
            assert!(
                p.tick(now, 4, false),
                "pill must stay visible at tick {i} (t={now}ms) — no back-off"
            );
            now += 1_000.0;
        }
        assert!(
            p.is_visible(),
            "still visible after >3min of steady pressure"
        );
    }

    #[test]
    fn steady_state_persistence_via_run_steady_60s_steps() {
        // Same persistence guarantee sampled at the cadence-agnostic 60s step
        // (mirrors the banner test's `run_steady` usage). Eligible first, then
        // coarse 60s steps across ~3 min: still visible on the final tick.
        let mut p = PillVisibility::new();
        p.tick(0.0, 2, false);
        assert!(p.tick(2_000.0, 2, false), "eligible + shown");
        let last = run_steady(&mut p, 62_000.0, 4, 60_000.0, 2, false);
        assert!(last, "no back-off: visible on the final tick ~4min in");
    }

    // ── Pure render decision (round-2 banner-suppression + retained count) ─────

    #[test]
    fn pill_render_decision_suppressed_while_banner_on_screen() {
        // banner_on_screen=true must force None even with a live count and an
        // eligible machine. Pins the `!banner_on_screen` term of the gate.
        assert_eq!(pill_render_decision(true, true, 3, 3), None);
    }

    #[test]
    fn pill_render_decision_hidden_when_machine_not_visible() {
        // machine_visible=false => None regardless of count. Pins the
        // `machine_visible` term.
        assert_eq!(pill_render_decision(false, false, 3, 0), None);
    }

    #[test]
    fn pill_render_decision_shows_live_count() {
        // Live count wins while > 0 (last_nonzero ignored).
        assert_eq!(pill_render_decision(true, false, 5, 2), Some(5));
    }

    #[test]
    fn pill_render_decision_holds_last_nonzero_during_pendinghide() {
        // THE mutation guard for the retained-count fix: during PendingHide the
        // live count reads 0 but the machine is still visible, so the displayed
        // count falls back to last_nonzero. If `effective_count` were reverted to
        // raw `avatar_count`, the `> 0` gate would fail and this would be None.
        assert_eq!(pill_render_decision(true, false, 0, 3), Some(3));
    }

    #[test]
    fn pill_render_decision_none_before_any_pressure() {
        // No live count and nothing retained => nothing to show.
        assert_eq!(pill_render_decision(true, false, 0, 0), None);
    }

    #[test]
    fn pill_render_decision_banner_beats_retained() {
        // Suppression beats the retained-count fallback: banner up => None even
        // though last_nonzero would otherwise drive a Some.
        assert_eq!(pill_render_decision(true, true, 0, 3), None);
    }
}
