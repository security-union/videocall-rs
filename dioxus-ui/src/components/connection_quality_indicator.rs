// SPDX-License-Identifier: MIT OR Apache-2.0

//! Persistent connection quality warning indicator for the self-view tile.
//!
//! Subscribes to RTT diagnostics from the connection manager and displays
//! a compact signal-bars badge when the round-trip time exceeds warning
//! thresholds. Hysteresis logic prevents the indicator from strobing on
//! marginal connections — it only activates after several consecutive
//! high-RTT samples and only deactivates after several consecutive
//! low-RTT samples.

use crate::components::icons::signal_bars::SignalBarsIcon;
use dioxus::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use videocall_diagnostics::{recv_loop_action, subscribe, MetricValue, RecvLoopAction};

// ---------------------------------------------------------------------------
// Thresholds & hysteresis constants
// ---------------------------------------------------------------------------

/// RTT at or above this value (ms) triggers the "Slow connection" warning.
/// Deliberately higher than `RTT_FAIR_MS` (200ms) from adaptive_quality_constants
/// to avoid showing warnings that don't correspond to visible quality impact.
/// The AQ system degrades proactively; this indicator only fires when users
/// would notice degraded call quality.
const WARN_THRESHOLD_MS: f64 = 300.0;

/// RTT at or above this value (ms) triggers the "Poor connection" warning.
/// Deliberately higher than `RTT_POOR_MS` (400ms) from adaptive_quality_constants
/// for the same reason as `WARN_THRESHOLD_MS` above.
const CRITICAL_THRESHOLD_MS: f64 = 500.0;

/// Number of consecutive samples above a threshold required to **enter** that
/// warning state.
const ENTER_COUNT: u32 = 3;

/// Number of consecutive samples below a threshold required to **exit** that
/// warning state.
const EXIT_COUNT: u32 = 5;

/// If the gap between two consecutive diagnostic samples exceeds this duration,
/// reset hysteresis state. This handles reconnects, re-elections, and network
/// drops — any scenario where the connection context has fundamentally changed
/// and stale hysteresis counters would cause the indicator to persist
/// incorrectly.  The threshold (10 seconds) exceeds the election period (~2s)
/// plus probing, so normal 1 Hz samples never trigger a false reset.
const SAMPLE_GAP_RESET_MS: u64 = 10_000;

// ---------------------------------------------------------------------------
// Quality level
// ---------------------------------------------------------------------------

/// Discrete quality levels derived from RTT with hysteresis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QualityLevel {
    /// RTT < WARN_THRESHOLD — no indicator shown.
    Good,
    /// WARN_THRESHOLD <= RTT < CRITICAL_THRESHOLD — amber warning.
    Warn,
    /// RTT >= CRITICAL_THRESHOLD — red warning.
    Critical,
}

// ---------------------------------------------------------------------------
// Sample ordering / gap classification
// ---------------------------------------------------------------------------

/// What the diagnostics loop should do with an incoming RTT sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleAction {
    /// The event is older than the last accepted one (replay / reorder).
    /// Ignore it entirely and leave all state untouched.
    Skip,
    /// Contiguous sample — feed it straight to the hysteresis state.
    Accept,
    /// The gap since the last sample exceeded `SAMPLE_GAP_RESET_MS`
    /// (reconnect / re-election). Reset the hysteresis state, then process
    /// the sample normally.
    Reset,
}

/// Classify an incoming sample against the last-accepted timestamp watermark,
/// advancing the watermark for every sample the caller will act on.
///
/// The watermark must be monotonic. The diagnostics bus makes no ordering
/// guarantee, so a replayed or reordered event can arrive with a timestamp
/// older than the last accepted one. Such an event returns [`SampleAction::Skip`]
/// and the watermark is left untouched, because processing it would corrupt gap
/// detection twice over: a backwards delta cannot express a real gap (the
/// un-fixed code used `saturating_sub`, which floored it to 0, so the stale
/// event read as a normal contiguous sample), *and* storing its timestamp would
/// rewind the watermark — making the next genuine sample look like a
/// multi-second gap and spuriously reset the hysteresis counters.
///
/// The skip window is bounded by `SAMPLE_GAP_RESET_MS`, symmetrically with the
/// forward-gap check: `videocall_diagnostics::now_ms` is `Date::now` on wasm, a
/// wall clock, so timestamps can also move backwards because the clock itself
/// was stepped (NTP correction, sleep/resume). Skipping *every* older event
/// would then stall the indicator until the clock caught back up — and a single
/// far-future timestamp would stall it permanently, since every later sample
/// would be older than the watermark. A backwards jump too large to be a
/// reorder is therefore treated like any other discontinuity: re-seed the
/// watermark and reset, which is self-healing on the very next sample.
///
/// A `last_sample_ts_ms` of `0` is the "no sample seen yet" sentinel: the first
/// sample is always accepted and only seeds the watermark.
fn classify_sample(evt_ts_ms: u64, last_sample_ts_ms: &mut u64) -> SampleAction {
    if *last_sample_ts_ms == 0 {
        *last_sample_ts_ms = evt_ts_ms;
        return SampleAction::Accept;
    }

    if evt_ts_ms < *last_sample_ts_ms {
        if *last_sample_ts_ms - evt_ts_ms <= SAMPLE_GAP_RESET_MS {
            // Replay / reorder: drop it, leaving the watermark untouched.
            return SampleAction::Skip;
        }
        // The clock moved, not the sample order — re-seed and start fresh.
        *last_sample_ts_ms = evt_ts_ms;
        return SampleAction::Reset;
    }

    // The branch above established `evt_ts_ms >= *last_sample_ts_ms`, so this
    // subtraction cannot underflow.
    let action = if evt_ts_ms - *last_sample_ts_ms > SAMPLE_GAP_RESET_MS {
        SampleAction::Reset
    } else {
        SampleAction::Accept
    };
    *last_sample_ts_ms = evt_ts_ms;
    action
}

// ---------------------------------------------------------------------------
// Hysteresis state (kept in a use_hook RefCell, not a signal, because we
// only want to trigger a re-render when the *displayed* level changes)
// ---------------------------------------------------------------------------

struct HysteresisState {
    /// Current displayed quality level.
    level: QualityLevel,
    /// Consecutive samples at or above WARN_THRESHOLD.
    above_warn_count: u32,
    /// Consecutive samples below WARN_THRESHOLD.
    below_warn_count: u32,
    /// Consecutive samples at or above CRITICAL_THRESHOLD.
    above_critical_count: u32,
    /// Consecutive samples below CRITICAL_THRESHOLD.
    below_critical_count: u32,
}

impl HysteresisState {
    fn new() -> Self {
        Self {
            level: QualityLevel::Good,
            above_warn_count: 0,
            below_warn_count: 0,
            above_critical_count: 0,
            below_critical_count: 0,
        }
    }

    /// Reset all counters and level to the initial state.
    fn reset(&mut self) {
        *self = Self::new();
    }

    /// Feed a new RTT sample and return `Some(new_level)` only when the
    /// displayed level should change. Returns `None` if the level is unchanged.
    fn update(&mut self, rtt_ms: f64) -> Option<QualityLevel> {
        // --- Critical threshold tracking ---
        if rtt_ms >= CRITICAL_THRESHOLD_MS {
            self.above_critical_count = self.above_critical_count.saturating_add(1);
            self.below_critical_count = 0;
        } else {
            self.below_critical_count = self.below_critical_count.saturating_add(1);
            self.above_critical_count = 0;
        }

        // --- Warn threshold tracking ---
        if rtt_ms >= WARN_THRESHOLD_MS {
            self.above_warn_count = self.above_warn_count.saturating_add(1);
            self.below_warn_count = 0;
        } else {
            self.below_warn_count = self.below_warn_count.saturating_add(1);
            self.above_warn_count = 0;
        }

        // --- Determine new level ---
        let new_level = match self.level {
            QualityLevel::Good => {
                if self.above_critical_count >= ENTER_COUNT {
                    QualityLevel::Critical
                } else if self.above_warn_count >= ENTER_COUNT {
                    QualityLevel::Warn
                } else {
                    QualityLevel::Good
                }
            }
            QualityLevel::Warn => {
                if self.above_critical_count >= ENTER_COUNT {
                    QualityLevel::Critical
                } else if self.below_warn_count >= EXIT_COUNT {
                    QualityLevel::Good
                } else {
                    QualityLevel::Warn
                }
            }
            QualityLevel::Critical => {
                // Check warn exit first: if RTT has been below the warn threshold
                // for EXIT_COUNT samples, skip Warn and go directly to Good.
                // This is intentional — if conditions are genuinely good (not just
                // below critical), there's no reason to pause at Warn.
                if self.below_warn_count >= EXIT_COUNT {
                    QualityLevel::Good
                } else if self.below_critical_count >= EXIT_COUNT {
                    QualityLevel::Warn
                } else {
                    QualityLevel::Critical
                }
            }
        };

        if new_level != self.level {
            self.level = new_level;
            Some(new_level)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/// Renders a persistent connection quality warning on the self-view tile.
///
/// Subscribes to `connection_manager` diagnostics events and applies
/// hysteresis to avoid indicator strobe on marginal connections.
#[component]
pub fn ConnectionQualityIndicator() -> Element {
    // Displayed quality level (drives rendering).
    let mut quality = use_signal(|| QualityLevel::Good);
    // Latest raw RTT for the title/aria-label tooltip. Using a Signal so
    // the tooltip text stays current while the indicator is visible. It is
    // read only AFTER the Good-state early return below, so the 1 Hz signal
    // updates do not re-render the component while it renders nothing.
    let mut raw_rtt_ms = use_signal(|| 0.0_f64);
    // Track whether we are in the "exiting" transition (fading out).
    let mut exiting = use_signal(|| false);
    // The last non-Good level, so the exit animation renders the correct
    // prior state (e.g., Critical fades out with 1 red bar, not 2 amber).
    let mut exit_level = use_signal(|| QualityLevel::Warn);
    // Generation counter to invalidate stale exit timers.  Incremented on
    // every level transition; the timer closure captures the generation at
    // scheduling time and no-ops if it has changed.
    let exit_generation: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));

    // Hysteresis state — stored in a RefCell so it survives across renders
    // without triggering re-renders on every sample.
    let hysteresis: Rc<RefCell<HysteresisState>> =
        use_hook(|| Rc::new(RefCell::new(HysteresisState::new())));

    // Subscribe to diagnostics events.
    {
        let hysteresis = hysteresis.clone();
        let exit_gen = exit_generation.clone();
        use_effect(move || {
            let hysteresis = hysteresis.clone();
            let exit_gen = exit_gen.clone();
            spawn(async move {
                let mut rx = subscribe();
                // Track the timestamp of the last processed sample so we can
                // detect reconnection / re-election gaps and reset hysteresis.
                let mut last_sample_ts_ms: u64 = 0;
                loop {
                    // Issue 2174: a bare `while let Ok(..)` here died permanently
                    // on the first `Overflowed`, which is recoverable — see
                    // `videocall_diagnostics::recv_loop_action`. The RTT pill then
                    // froze on whatever quality it last showed.
                    let evt = match rx.recv().await {
                        Ok(evt) => evt,
                        Err(e) => match recv_loop_action(&e) {
                            RecvLoopAction::Continue => continue,
                            RecvLoopAction::Break => break,
                        },
                    };
                    if evt.subsystem != "connection_manager" {
                        continue;
                    }
                    // Skip per-server events (only the main event carries active_server_rtt).
                    if evt.stream_id.is_some() {
                        continue;
                    }
                    // Extract active_server_rtt from the event metrics.
                    let mut rtt: Option<f64> = None;
                    for m in &evt.metrics {
                        if m.name == "active_server_rtt" {
                            if let MetricValue::F64(v) = &m.value {
                                rtt = Some(*v);
                            }
                        }
                    }
                    let Some(rtt_val) = rtt else {
                        continue;
                    };

                    // Classify the sample against the timestamp watermark:
                    // out-of-order replays are dropped, and gaps that indicate a
                    // transport reconnect or re-election reset the (now stale)
                    // hysteresis state so the indicator starts fresh with the
                    // new connection. `classify_sample` owns the watermark
                    // advance, so a skipped event cannot rewind it.
                    match classify_sample(evt.ts_ms, &mut last_sample_ts_ms) {
                        SampleAction::Skip => continue,
                        SampleAction::Reset => {
                            hysteresis.borrow_mut().reset();
                            // If the indicator was visible, immediately hide it.
                            // The next sample will re-evaluate from a clean state.
                            if quality() != QualityLevel::Good {
                                let gen = exit_gen.get().wrapping_add(1);
                                exit_gen.set(gen);
                                quality.set(QualityLevel::Good);
                                exiting.set(false);
                            }
                        }
                        SampleAction::Accept => {}
                    }

                    raw_rtt_ms.set(rtt_val);

                    if let Some(new_level) = hysteresis.borrow_mut().update(rtt_val) {
                        // Bump generation to invalidate any pending exit timer.
                        let gen = exit_gen.get().wrapping_add(1);
                        exit_gen.set(gen);

                        if new_level == QualityLevel::Good {
                            // Start the exit animation — the CSS fade-out plays
                            // while we keep rendering the last non-Good state.
                            exiting.set(true);
                            let mut q = quality;
                            let mut ex = exiting;
                            let exit_gen_clone = exit_gen.clone();
                            gloo_timers::callback::Timeout::new(500, move || {
                                // Only complete the exit if no new transition
                                // happened while we were fading out.
                                if exit_gen_clone.get() == gen {
                                    q.set(QualityLevel::Good);
                                    ex.set(false);
                                }
                            })
                            .forget();
                        } else {
                            exiting.set(false);
                            exit_level.set(new_level);
                            quality.set(new_level);
                        }
                    }
                }
            });
        });
    }

    let level = quality();
    let is_exiting = exiting();

    // Do not render anything in the Good state (after exit animation completes).
    if level == QualityLevel::Good && !is_exiting {
        return rsx! {};
    }

    // Read the RTT only past the early return. A Dioxus scope re-subscribes
    // from scratch on every render (`ReactiveContext::reset_and_run_in`), so a
    // read that does not execute drops the subscription: in the Good state the
    // 1 Hz `raw_rtt_ms.set` above cannot wake this component, and when it does
    // become visible the read re-subscribes and the tooltip tracks live again.
    let rtt = raw_rtt_ms();

    // During exit animation, render the last non-Good state so the fade-out
    // shows the correct icon/label (e.g., red 1-bar for Critical, not amber).
    let display_level = if level == QualityLevel::Good {
        exit_level()
    } else {
        level
    };

    let (bar_level, label, rtt_int) = match display_level {
        QualityLevel::Good => (2u8, "Slow connection", rtt as u32),
        QualityLevel::Warn => (2, "Slow connection", rtt as u32),
        QualityLevel::Critical => (1, "Poor connection", rtt as u32),
    };

    let visible_class = if is_exiting {
        "connection-quality-indicator exiting"
    } else {
        "connection-quality-indicator visible"
    };

    let title_text = format!("RTT: {rtt_int}ms");
    let aria_text = match display_level {
        QualityLevel::Good | QualityLevel::Warn => {
            format!("Connection quality: slow, round trip time {rtt_int} milliseconds")
        }
        QualityLevel::Critical => {
            format!("Connection quality: poor, round trip time {rtt_int} milliseconds")
        }
    };

    rsx! {
        div {
            class: "{visible_class}",
            role: "status",
            "aria-live": "polite",
            "aria-label": "{aria_text}",
            title: "{title_text}",
            // aria-hidden on children prevents screen readers from
            // double-announcing: the outer div's aria-label is the
            // single announcement source.
            span { "aria-hidden": "true", SignalBarsIcon { level: bar_level } }
            span { class: "connection-quality-label", "aria-hidden": "true", "{label}" }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hysteresis_enter_warn() {
        let mut state = HysteresisState::new();
        // Two samples above warn — not enough yet.
        assert_eq!(state.update(350.0), None);
        assert_eq!(state.update(310.0), None);
        // Third consecutive sample triggers warn.
        assert_eq!(state.update(400.0), Some(QualityLevel::Warn));
    }

    #[test]
    fn hysteresis_exit_warn_requires_five() {
        let mut state = HysteresisState::new();
        // Enter warn state.
        state.update(350.0);
        state.update(350.0);
        state.update(350.0);
        assert_eq!(state.level, QualityLevel::Warn);

        // Four samples below — not enough to exit.
        for _ in 0..4 {
            assert_eq!(state.update(100.0), None);
        }
        // Fifth sample exits.
        assert_eq!(state.update(100.0), Some(QualityLevel::Good));
    }

    #[test]
    fn hysteresis_enter_critical() {
        let mut state = HysteresisState::new();
        state.update(600.0);
        state.update(700.0);
        assert_eq!(state.update(550.0), Some(QualityLevel::Critical));
    }

    #[test]
    fn hysteresis_critical_to_warn() {
        let mut state = HysteresisState::new();
        // Enter critical.
        state.update(600.0);
        state.update(700.0);
        state.update(550.0);
        assert_eq!(state.level, QualityLevel::Critical);

        // Drop below critical but stay above warn.
        for _ in 0..4 {
            assert_eq!(state.update(350.0), None);
        }
        assert_eq!(state.update(350.0), Some(QualityLevel::Warn));
    }

    #[test]
    fn hysteresis_no_strobe_on_boundary() {
        let mut state = HysteresisState::new();
        // Alternate above/below warn threshold — should never trigger.
        for _ in 0..20 {
            assert_eq!(state.update(310.0), None);
            assert_eq!(state.update(290.0), None);
        }
        assert_eq!(state.level, QualityLevel::Good);
    }

    #[test]
    fn hysteresis_resets_after_gap() {
        let mut state = HysteresisState::new();
        // Enter critical state.
        state.update(600.0);
        state.update(700.0);
        state.update(550.0);
        assert_eq!(state.level, QualityLevel::Critical);
        // All counters are non-zero.
        assert!(state.above_critical_count > 0);

        // Simulate a reconnect / re-election gap by resetting.
        state.reset();

        // State should be fully clean — equivalent to a fresh HysteresisState.
        assert_eq!(state.level, QualityLevel::Good);
        assert_eq!(state.above_warn_count, 0);
        assert_eq!(state.below_warn_count, 0);
        assert_eq!(state.above_critical_count, 0);
        assert_eq!(state.below_critical_count, 0);

        // After reset, hysteresis re-evaluates from scratch. A single good
        // sample should not trigger any level change.
        assert_eq!(state.update(50.0), None);
        assert_eq!(state.level, QualityLevel::Good);

        // And it takes the full ENTER_COUNT consecutive bad samples to
        // re-enter a warning state — stale counters are gone.
        assert_eq!(state.update(350.0), None);
        assert_eq!(state.update(350.0), None);
        assert_eq!(state.update(350.0), Some(QualityLevel::Warn));
    }

    // -----------------------------------------------------------------------
    // Sample ordering / gap classification
    // -----------------------------------------------------------------------

    #[test]
    fn classify_sample_first_sample_is_never_skipped() {
        // The `0` sentinel means "no sample seen yet" — whatever the bus
        // hands us first seeds the watermark and is accepted.
        let mut watermark = 0;
        assert_eq!(
            classify_sample(1_000_000, &mut watermark),
            SampleAction::Accept
        );
        assert_eq!(watermark, 1_000_000);

        // Even a huge first timestamp must not be read as a reconnect gap.
        let mut fresh = 0;
        assert_eq!(
            classify_sample(u64::MAX, &mut fresh),
            SampleAction::Accept,
            "first sample must never be classified as a gap reset"
        );
    }

    #[test]
    fn classify_sample_accepts_normal_forward_step() {
        let mut watermark = 100_000;
        assert_eq!(
            classify_sample(101_000, &mut watermark),
            SampleAction::Accept
        );
        assert_eq!(watermark, 101_000);
    }

    #[test]
    fn classify_sample_accepts_equal_timestamp() {
        // A duplicate timestamp is a zero-length gap, not a backwards jump:
        // it is still processed, and the watermark stays where it was.
        let mut watermark = 100_000;
        assert_eq!(
            classify_sample(100_000, &mut watermark),
            SampleAction::Accept
        );
        assert_eq!(watermark, 100_000);
    }

    #[test]
    fn classify_sample_resets_beyond_gap_threshold() {
        let mut watermark = 100_000;
        let past_gap = 100_000 + SAMPLE_GAP_RESET_MS + 1;
        assert_eq!(
            classify_sample(past_gap, &mut watermark),
            SampleAction::Reset
        );
        assert_eq!(
            watermark, past_gap,
            "a reset sample still advances the watermark"
        );

        // Exactly at the threshold is still contiguous — the check is `>`.
        let mut boundary = 100_000;
        assert_eq!(
            classify_sample(100_000 + SAMPLE_GAP_RESET_MS, &mut boundary),
            SampleAction::Accept
        );
    }

    #[test]
    fn classify_sample_skips_backwards_jump_without_rewinding_watermark() {
        let mut watermark = 100_000;
        // A replayed / reordered event from 8s earlier — inside the reorder
        // window, so it is dropped rather than treated as a clock step.
        assert_eq!(classify_sample(92_000, &mut watermark), SampleAction::Skip);
        assert_eq!(
            watermark, 100_000,
            "a skipped event must not rewind the watermark"
        );

        // The boundary is inclusive: a backwards jump of exactly
        // SAMPLE_GAP_RESET_MS is still a reorder, not a clock step.
        let mut boundary = 100_000;
        assert_eq!(
            classify_sample(100_000 - SAMPLE_GAP_RESET_MS, &mut boundary),
            SampleAction::Skip
        );
        assert_eq!(boundary, 100_000);
    }

    #[test]
    fn classify_sample_large_backwards_step_reseeds_instead_of_wedging() {
        // `now_ms` is a wall clock on wasm, so it can be stepped backwards by
        // an NTP correction. Skipping every older event would stall the
        // indicator for the whole duration of the step, and a single far-future
        // timestamp would stall it forever. Instead the watermark re-seeds.
        let mut watermark = 100_000;
        let stepped_back = 100_000 - SAMPLE_GAP_RESET_MS - 1;
        assert_eq!(
            classify_sample(stepped_back, &mut watermark),
            SampleAction::Reset
        );
        assert_eq!(
            watermark, stepped_back,
            "a clock step must re-seed the watermark, not leave it in the future"
        );

        // Self-healing: the very next sample on the new clock is contiguous.
        assert_eq!(
            classify_sample(stepped_back + 1_000, &mut watermark),
            SampleAction::Accept,
            "the indicator must resume immediately, not wedge behind a stale watermark"
        );
    }

    #[test]
    fn classify_sample_replay_does_not_trigger_a_spurious_reset_later() {
        // This is the composite failure the guard exists to prevent. Without
        // it, the stale event at 92_000 reads as a 0-length gap (the un-fixed
        // code's `saturating_sub` floored the backwards delta) AND rewinds the
        // watermark to 92_000 — so the next genuine sample at 103_000 looks
        // like an 11s gap and wipes the hysteresis counters even though the
        // connection never dropped.
        let mut watermark = 0;

        assert_eq!(
            classify_sample(100_000, &mut watermark),
            SampleAction::Accept
        );
        // The classification of the replay itself is pinned by
        // `classify_sample_skips_backwards_jump_without_rewinding_watermark`;
        // what this test pins is the effect it has on the *next* sample.
        let _ = classify_sample(92_000, &mut watermark);
        assert_eq!(
            classify_sample(103_000, &mut watermark),
            SampleAction::Accept,
            "the sample following a replay must stay contiguous, not reset hysteresis"
        );
        assert_eq!(watermark, 103_000);
    }
}
