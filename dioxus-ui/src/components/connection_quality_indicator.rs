// SPDX-License-Identifier: MIT OR Apache-2.0

//! The self-view tile's signal meter: the disc every peer tile carries, fed by
//! `connection_manager` RTT diagnostics. Hysteresis keeps the level steady.

use crate::components::attendants::action_bar_announce_text;
use crate::components::icons::signal_spark::SignalSparkIcon;
use crate::components::signal_quality::{
    build_spark_points, flat_spark_segment, prefers_reduced_motion, SignalLevel, SparkPaint,
    SPARK_MIN_POINTS, SPARK_POINTS,
};
use dioxus::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use videocall_diagnostics::{recv_loop_action, subscribe, MetricValue, RecvLoopAction};

// ---------------------------------------------------------------------------
// Thresholds & hysteresis constants
// ---------------------------------------------------------------------------

/// RTT at or above this value (ms) triggers [`QualityLevel::Warn`].
/// Deliberately higher than `RTT_FAIR_MS` (200ms) from adaptive_quality_constants
/// to avoid showing warnings that don't correspond to visible quality impact.
/// The AQ system degrades proactively; this indicator only fires when users
/// would notice degraded call quality.
const WARN_THRESHOLD_MS: f64 = 300.0;

/// RTT at or above this value (ms) triggers [`QualityLevel::Critical`].
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

/// Retained RTT samples. `connection_manager` reports at 1 Hz, so this is the
/// 10-second window the sparkline plots.
const SELF_RTT_HISTORY_LEN: usize = SPARK_POINTS;

/// The `connection_manager` cadence: `connection_controller` drives
/// `trigger_diagnostics_report` from an `Interval::new(1000, ..)`.
const RTT_SAMPLE_INTERVAL_MS: u32 = 1000;

/// Missed ticks before the meter stops claiming a level: two plus slack.
const fn stale_after_missed_ticks() -> u32 {
    (RTT_SAMPLE_INTERVAL_MS * 3) / RTT_SAMPLE_INTERVAL_MS
}

/// What the disc presents. `active_server_rtt` is emitted ONLY when Elected with
/// a fresh probe, and nothing else ages the history — hence `Stale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presented {
    Measuring,
    Live(QualityLevel),
    Stale,
}

/// Never having measured outranks having stopped; the LED covers cold start.
fn presented_state(sample_count: usize, missed_ticks: u32, level: QualityLevel) -> Presented {
    if sample_count == 0 {
        Presented::Measuring
    } else if missed_ticks >= stale_after_missed_ticks() {
        Presented::Stale
    } else if sample_count < SPARK_MIN_POINTS {
        Presented::Measuring
    } else {
        Presented::Live(level)
    }
}

/// RTT-to-quality breakpoints, interpolated between and clamped at both ends.
/// 300/500 ms are the thresholds and 0.90/0.75/0.50/0.25 are
/// `SignalLevel::from_quality` boundaries, so line and ring cannot disagree.
const RTT_QUALITY_BREAKPOINTS: [(f64, f64); 7] = [
    (0.0, 1.00),
    (100.0, 0.90),
    (200.0, 0.75),
    (300.0, 0.50),
    (500.0, 0.25),
    (1000.0, 0.05),
    (1500.0, 0.02),
];

fn rtt_to_quality(rtt_ms: f64) -> f64 {
    let points = &RTT_QUALITY_BREAKPOINTS;
    let first = points[0];
    let last = points[points.len() - 1];
    if rtt_ms.is_nan() || rtt_ms <= first.0 {
        return first.1;
    }
    if rtt_ms >= last.0 {
        return last.1;
    }
    for pair in points.windows(2) {
        let (x0, y0) = pair[0];
        let (x1, y1) = pair[1];
        if rtt_ms <= x1 {
            return y0 + (y1 - y0) * (rtt_ms - x0) / (x1 - x0);
        }
    }
    last.1
}

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

impl QualityLevel {
    fn signal_level(self) -> SignalLevel {
        match self {
            // Not `Excellent`: a healthy PEER reads `Good`.
            Self::Good => SignalLevel::Good,
            Self::Warn => SignalLevel::Fair,
            Self::Critical => SignalLevel::Bad,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Warn => "slow",
            Self::Critical => "poor",
        }
    }
}

const ANNOUNCE_CRITICAL: &str = "Your connection is poor.";
const ANNOUNCE_RECOVERED: &str = "Your connection is back to normal.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LevelEvent {
    Transition(QualityLevel),
    /// A reconnect: the connection the outstanding announcement described is gone.
    Discontinuity,
}

/// The live-region message, or `None`. Sole owner of the policy. Warn is silent
/// both ways; recovery keys off the latch, not a `Critical -> Good` edge,
/// because hysteresis can exit through Warn.
fn announcement_for(event: LevelEvent, announced_critical: &mut bool) -> Option<&'static str> {
    match event {
        // Retires the owed recovery WITHOUT speaking it.
        LevelEvent::Discontinuity => {
            *announced_critical = false;
            None
        }
        LevelEvent::Transition(QualityLevel::Critical) if !*announced_critical => {
            *announced_critical = true;
            Some(ANNOUNCE_CRITICAL)
        }
        LevelEvent::Transition(QualityLevel::Good) if *announced_critical => {
            *announced_critical = false;
            Some(ANNOUNCE_RECOVERED)
        }
        LevelEvent::Transition(_) => None,
    }
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct RttSample {
    /// A wall clock, not a monotonic one.
    timestamp_ms: u64,
    rtt_ms: f64,
}

struct SelfRttHistory {
    samples: VecDeque<RttSample>,
}

impl SelfRttHistory {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(SELF_RTT_HISTORY_LEN),
        }
    }

    fn push(&mut self, sample: RttSample) {
        if self.samples.len() >= SELF_RTT_HISTORY_LEN {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }

    fn clear(&mut self) {
        self.samples.clear();
    }

    fn quality_series(&self) -> Vec<(f64, f64)> {
        self.samples
            .iter()
            .map(|s| (s.timestamp_ms as f64, rtt_to_quality(s.rtt_ms)))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleEffect {
    Ignored,
    Recorded {
        reset: bool,
        level_change: Option<QualityLevel>,
    },
}

fn fold_sample(
    hysteresis: &mut HysteresisState,
    history: &mut SelfRttHistory,
    last_sample_ts_ms: &mut u64,
    sample: RttSample,
) -> SampleEffect {
    let reset = match classify_sample(sample.timestamp_ms, last_sample_ts_ms) {
        SampleAction::Skip => return SampleEffect::Ignored,
        SampleAction::Reset => {
            hysteresis.reset();
            history.clear();
            true
        }
        SampleAction::Accept => false,
    };

    history.push(sample);
    let level_change = hysteresis.update(sample.rtt_ms);

    SampleEffect::Recorded {
        reset,
        level_change,
    }
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn ConnectionQualityIndicator(on_open_diagnostics: EventHandler<()>) -> Element {
    // Displayed quality level (drives rendering).
    let mut quality = use_signal(|| QualityLevel::Good);
    let mut raw_rtt_ms = use_signal(|| 0.0_f64);
    let mut announcement = use_signal(String::new);
    let mut announce_nonce = use_signal(|| 0_u32);
    // Flipped only when the verdict changes, so a healthy call is not woken.
    let mut stale = use_signal(|| false);
    let missed_ticks: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));
    let announced_critical: Rc<Cell<bool>> = use_hook(|| Rc::new(Cell::new(false)));

    // Hysteresis state — stored in a RefCell so it survives across renders
    // without triggering re-renders on every sample.
    let hysteresis: Rc<RefCell<HysteresisState>> =
        use_hook(|| Rc::new(RefCell::new(HysteresisState::new())));

    let rtt_history: Rc<RefCell<SelfRttHistory>> =
        use_hook(|| Rc::new(RefCell::new(SelfRttHistory::new())));

    let mut sample_counter = use_signal(|| 0_u32);

    // Subscribe to diagnostics events.
    {
        let hysteresis = hysteresis.clone();
        let rtt_history = rtt_history.clone();
        let announced = announced_critical.clone();
        let missed = missed_ticks.clone();
        use_effect(move || {
            let hysteresis = hysteresis.clone();
            let rtt_history = rtt_history.clone();
            let announced = announced.clone();
            let missed = missed.clone();
            spawn(async move {
                let mut rx = subscribe();
                // Track the timestamp of the last processed sample so we can
                // detect reconnection / re-election gaps and reset hysteresis.
                let mut last_sample_ts_ms: u64 = 0;
                loop {
                    // Issue 2174: a bare `while let Ok(..)` here died permanently
                    // on the first `Overflowed`, which is recoverable — see
                    // `videocall_diagnostics::recv_loop_action`. The meter then
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
                        // The main event ticks in every election state, so its
                        // arrival without the metric IS the staleness clock.
                        let n = missed.get().saturating_add(1);
                        missed.set(n);
                        if n >= stale_after_missed_ticks() && !*stale.peek() {
                            stale.set(true);
                        }
                        continue;
                    };
                    missed.set(0);
                    if *stale.peek() {
                        stale.set(false);
                    }

                    let effect = fold_sample(
                        &mut hysteresis.borrow_mut(),
                        &mut rtt_history.borrow_mut(),
                        &mut last_sample_ts_ms,
                        RttSample {
                            timestamp_ms: evt.ts_ms,
                            rtt_ms: rtt_val,
                        },
                    );
                    let (reset, level_change) = match effect {
                        SampleEffect::Ignored => continue,
                        SampleEffect::Recorded {
                            reset,
                            level_change,
                        } => (reset, level_change),
                    };

                    let mut latched = announced.get();
                    let mut msg = None;

                    if reset {
                        msg = announcement_for(LevelEvent::Discontinuity, &mut latched);
                        if quality() != QualityLevel::Good {
                            quality.set(QualityLevel::Good);
                        }
                    }

                    raw_rtt_ms.set(rtt_val);
                    let prev_count = *sample_counter.peek();
                    sample_counter.set(prev_count.wrapping_add(1));

                    if let Some(new_level) = level_change {
                        quality.set(new_level);
                        msg = announcement_for(LevelEvent::Transition(new_level), &mut latched);
                    }

                    announced.set(latched);
                    if let Some(msg) = msg {
                        // Nonce on every write, repeats included: `diff_vtext`
                        // skips an identical value, so a repeat would never
                        // announce (issue 1765).
                        announcement.set(msg.to_string());
                        let next = announce_nonce.peek().wrapping_add(1);
                        announce_nonce.set(next);
                    }
                }
            });
        });
    }

    let level = quality();
    let rtt = raw_rtt_ms();
    let _ = sample_counter();
    // Read so the staleness flip wakes this scope; the Cell cannot.
    let _ = stale();

    let series = rtt_history.borrow().quality_series();
    let sample_count = series.len();
    let rtt_int = rtt as u32;

    // The RTT lives in `title` only: that is the accessible DESCRIPTION,
    // announced on focus, never on mutation.
    let (signal_level, state_attr, aria_text, title_text) =
        match presented_state(sample_count, missed_ticks.get(), level) {
            Presented::Measuring => (
                SignalLevel::Unmeasured,
                "measuring",
                "Your connection: measuring. Open diagnostics.".to_string(),
                "Connection: measuring…".to_string(),
            ),
            Presented::Stale => (
                SignalLevel::Unmeasured,
                "stale",
                "Your connection: not measured. Open diagnostics.".to_string(),
                "Connection: not measured — no recent samples".to_string(),
            ),
            Presented::Live(level) => {
                let word = level.word();
                (
                    level.signal_level(),
                    "measured",
                    format!("Your connection: {word}. Open diagnostics."),
                    format!("Connection: {word} — RTT {rtt_int} ms"),
                )
            }
        };
    // `stale`/`reduce` are PERSISTENT so they get the readout; `measuring` is
    // transient, and only `state_attr` tells it from `stale`.
    let plot = matches!(state_attr, "measured") && !prefers_reduced_motion();
    let paint = SparkPaint {
        segments: if plot {
            build_spark_points(&series)
        } else if matches!(state_attr, "measuring") {
            Vec::new()
        } else {
            vec![flat_spark_segment(signal_level)]
        },
        level: signal_level,
        sample_count,
        latency_ms: rtt.trunc(),
    };

    let announce = action_bar_announce_text(&announcement(), announce_nonce());

    rsx! {
        button {
            class: "signal-indicator",
            "data-testid": "self-signal-indicator",
            "data-signal-state": "{state_attr}",
            "data-signal-level": "{signal_level.bars()}",
            "data-signal-lost": "false",
            "data-signal-samples": "{sample_count}",
            "aria-label": "{aria_text}",
            title: "{title_text}",
            // stop_propagation: a tile-overlay control, not a grid click — it
            // must not light-dismiss an open side panel (issue 1790).
            onclick: move |e: MouseEvent| {
                e.stop_propagation();
                on_open_diagnostics.call(());
            },
            // A spec handle only: the self disc repaints through RSX.
            SignalSparkIcon { paint, spark_id: "self".to_string() }
        }
        span {
            class: "visually-hidden",
            role: "status",
            "aria-live": "polite",
            "aria-atomic": "true",
            "{announce}"
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

    fn sample(timestamp_ms: u64, rtt_ms: f64) -> RttSample {
        RttSample {
            timestamp_ms,
            rtt_ms,
        }
    }

    struct Folder {
        hysteresis: HysteresisState,
        history: SelfRttHistory,
        watermark: u64,
    }

    impl Folder {
        fn seeded_at(watermark: u64) -> Self {
            Self {
                hysteresis: HysteresisState::new(),
                history: SelfRttHistory::new(),
                watermark,
            }
        }

        fn fold(&mut self, timestamp_ms: u64, rtt_ms: f64) -> SampleEffect {
            fold_sample(
                &mut self.hysteresis,
                &mut self.history,
                &mut self.watermark,
                sample(timestamp_ms, rtt_ms),
            )
        }
    }

    #[test]
    fn history_evicts_the_oldest_sample_at_capacity() {
        let mut history = SelfRttHistory::new();
        let overflow = 5_u64;
        for i in 0..(SELF_RTT_HISTORY_LEN as u64 + overflow) {
            history.push(sample(1_000 + i, i as f64));
        }

        assert_eq!(
            history.samples.len(),
            SELF_RTT_HISTORY_LEN,
            "the ring must stay bounded, not grow with the call count"
        );
        assert_eq!(
            history.samples.front().copied(),
            Some(sample(1_000 + overflow, overflow as f64)),
            "the oldest surviving sample must be the first one not evicted"
        );
        assert_eq!(
            history.samples.back().copied(),
            Some(sample(
                1_000 + SELF_RTT_HISTORY_LEN as u64 + overflow - 1,
                (SELF_RTT_HISTORY_LEN as u64 + overflow - 1) as f64
            )),
            "the newest sample must be retained"
        );
    }

    #[test]
    fn fold_sample_records_each_accepted_sample() {
        let mut folder = Folder::seeded_at(100_000);

        assert_eq!(
            folder.fold(100_000, 120.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: None
            }
        );
        assert_eq!(
            folder.fold(101_000, 140.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: None
            }
        );

        assert_eq!(
            folder.history.samples.iter().copied().collect::<Vec<_>>(),
            vec![sample(100_000, 120.0), sample(101_000, 140.0)],
            "every accepted sample must land in the history, in arrival order"
        );
    }

    #[test]
    fn fold_sample_reports_the_level_change_that_enters_warn() {
        let mut folder = Folder::seeded_at(100_000);

        assert_eq!(
            folder.fold(100_000, 350.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: None
            }
        );
        assert_eq!(
            folder.fold(101_000, 350.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: None
            }
        );
        assert_eq!(
            folder.fold(102_000, 350.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: Some(QualityLevel::Warn)
            },
            "the ENTER_COUNT-th consecutive high sample must surface the transition"
        );
    }

    #[test]
    fn fold_sample_ignores_a_replay_without_touching_any_state() {
        let mut folder = Folder::seeded_at(100_000);
        folder.fold(100_000, 350.0);
        folder.fold(101_000, 350.0);

        assert_eq!(
            folder.fold(93_000, 900.0),
            SampleEffect::Ignored,
            "an in-window backwards timestamp is a replay, not a new reading"
        );
        assert_eq!(
            folder.history.samples.len(),
            2,
            "a replayed sample must not be recorded"
        );
        assert_eq!(
            folder.hysteresis.above_critical_count, 0,
            "a replayed sample must not feed hysteresis"
        );
        assert_eq!(
            folder.watermark, 101_000,
            "a replayed sample must not rewind the watermark"
        );
    }

    #[test]
    fn fold_sample_discards_pre_discontinuity_history_on_a_gap() {
        let mut folder = Folder::seeded_at(100_000);
        folder.fold(100_000, 120.0);
        folder.fold(101_000, 130.0);
        folder.fold(102_000, 140.0);
        assert_eq!(folder.history.samples.len(), 3);

        let after_gap = 102_000 + SAMPLE_GAP_RESET_MS + 1;
        assert_eq!(
            folder.fold(after_gap, 200.0),
            SampleEffect::Recorded {
                reset: true,
                level_change: None
            }
        );
        assert_eq!(
            folder.history.samples.iter().copied().collect::<Vec<_>>(),
            vec![sample(after_gap, 200.0)],
            "samples from before a reconnect / clock step must not be graphed \
             alongside samples from after it"
        );
    }

    #[test]
    fn fold_sample_gap_clears_hysteresis_so_one_bad_sample_cannot_re_trigger() {
        let mut folder = Folder::seeded_at(100_000);
        folder.fold(100_000, 600.0);
        folder.fold(101_000, 600.0);
        assert_eq!(
            folder.fold(102_000, 600.0),
            SampleEffect::Recorded {
                reset: false,
                level_change: Some(QualityLevel::Critical)
            }
        );

        let after_gap = 102_000 + SAMPLE_GAP_RESET_MS + 1;
        assert_eq!(
            folder.fold(after_gap, 600.0),
            SampleEffect::Recorded {
                reset: true,
                level_change: None
            },
            "a wiped hysteresis cannot re-enter Critical on a single sample"
        );
        assert_eq!(folder.hysteresis.level, QualityLevel::Good);
        assert_eq!(folder.hysteresis.above_critical_count, 1);
    }

    #[test]
    fn rtt_quality_lands_exactly_on_every_breakpoint() {
        for (rtt, q) in RTT_QUALITY_BREAKPOINTS {
            let got = rtt_to_quality(rtt);
            assert!(
                (got - q).abs() < 1e-9,
                "breakpoint {rtt}ms should plot at {q}, got {got}"
            );
        }
    }

    #[test]
    fn rtt_quality_puts_the_warn_threshold_on_the_reference_line() {
        assert_eq!(rtt_to_quality(WARN_THRESHOLD_MS), 0.5);
        assert!(rtt_to_quality(CRITICAL_THRESHOLD_MS) < 0.5);
    }

    #[test]
    fn rtt_quality_interpolates_between_breakpoints() {
        assert!((rtt_to_quality(250.0) - 0.625).abs() < 1e-9);
        assert!((rtt_to_quality(750.0) - 0.15).abs() < 1e-9);
    }

    #[test]
    fn rtt_quality_is_monotonic_and_never_leaves_the_plot_box() {
        let mut prev = f64::INFINITY;
        for step in 0..400 {
            let q = rtt_to_quality(step as f64 * 10.0);
            assert!(q <= prev, "quality must never rise as RTT rises");
            assert!((0.02..=1.0).contains(&q), "q={q} escaped the plot box");
            prev = q;
        }
    }

    #[test]
    fn rtt_quality_clamps_outside_the_table_and_survives_nan() {
        assert_eq!(rtt_to_quality(-50.0), 1.0);
        assert_eq!(rtt_to_quality(0.0), 1.0);
        assert_eq!(rtt_to_quality(9_000.0), 0.02);
        assert_eq!(
            rtt_to_quality(f64::NAN),
            1.0,
            "a NaN must resolve here, not reach the SVG as a NaN coordinate"
        );
    }

    #[test]
    fn each_hysteresis_level_gets_its_own_ring() {
        assert_eq!(QualityLevel::Good.signal_level(), SignalLevel::Good);
        assert_eq!(QualityLevel::Warn.signal_level(), SignalLevel::Fair);
        assert_eq!(QualityLevel::Critical.signal_level(), SignalLevel::Bad);

        let colors = [
            QualityLevel::Good.signal_level().level_color(),
            QualityLevel::Warn.signal_level().level_color(),
            QualityLevel::Critical.signal_level().level_color(),
        ];
        assert_ne!(colors[0], colors[1]);
        assert_ne!(colors[1], colors[2]);
        assert_ne!(colors[0], colors[2]);
    }

    fn announce(level: QualityLevel, latched: &mut bool) -> Option<&'static str> {
        announcement_for(LevelEvent::Transition(level), latched)
    }

    #[test]
    fn a_link_that_stops_reporting_stops_claiming_a_level() {
        let live = presented_state(6, 0, QualityLevel::Good);
        assert_eq!(live, Presented::Live(QualityLevel::Good));

        assert_eq!(
            presented_state(6, stale_after_missed_ticks(), QualityLevel::Good),
            Presented::Stale
        );
        assert_eq!(
            presented_state(6, stale_after_missed_ticks() - 1, QualityLevel::Good),
            live,
            "one tick short of the bound must not flip it"
        );
    }

    #[test]
    fn cold_start_reads_measuring_rather_than_stale() {
        assert_eq!(
            presented_state(0, stale_after_missed_ticks() * 4, QualityLevel::Good),
            Presented::Measuring
        );
        assert_eq!(
            presented_state(SPARK_MIN_POINTS - 1, 0, QualityLevel::Good),
            Presented::Measuring,
            "a partial history is still measuring"
        );
    }

    #[test]
    fn only_critical_is_announced_and_only_once() {
        let mut latched = false;
        assert_eq!(announce(QualityLevel::Warn, &mut latched), None);
        assert!(!latched, "warn is not actionable and must stay silent");

        assert_eq!(
            announce(QualityLevel::Critical, &mut latched),
            Some(ANNOUNCE_CRITICAL)
        );
        assert_eq!(
            announce(QualityLevel::Critical, &mut latched),
            None,
            "a second critical transition must not re-announce"
        );
    }

    #[test]
    fn a_discontinuity_retires_the_owed_recovery_without_speaking_it() {
        let mut latched = false;
        announce(QualityLevel::Critical, &mut latched);
        assert!(latched);

        assert_eq!(
            announcement_for(LevelEvent::Discontinuity, &mut latched),
            None,
            "a reconnect must not announce anything about the old connection"
        );
        assert!(
            !latched,
            "the owed recovery must be retired, not carried over"
        );

        assert_eq!(announce(QualityLevel::Good, &mut latched), None);
    }

    #[test]
    fn recovery_is_announced_even_when_it_exits_through_warn() {
        let mut latched = false;
        announce(QualityLevel::Critical, &mut latched);
        assert_eq!(announce(QualityLevel::Warn, &mut latched), None);
        assert_eq!(
            announce(QualityLevel::Good, &mut latched),
            Some(ANNOUNCE_RECOVERED)
        );
        assert!(!latched);
    }

    #[test]
    fn a_connection_that_was_never_critical_never_announces_recovery() {
        let mut latched = false;
        assert_eq!(announce(QualityLevel::Warn, &mut latched), None);
        assert_eq!(
            announce(QualityLevel::Good, &mut latched),
            None,
            "nothing was announced, so there is nothing to take back"
        );
    }

    #[test]
    fn quality_series_maps_every_retained_sample_through_the_rtt_curve() {
        let mut history = SelfRttHistory::new();
        history.push(sample(1_000, 0.0));
        history.push(sample(2_000, WARN_THRESHOLD_MS));
        history.push(sample(3_000, 9_000.0));

        let series = history.quality_series();
        assert_eq!(
            series,
            vec![(1_000.0, 1.0), (2_000.0, 0.5), (3_000.0, 0.02)],
            "the plotted series must be the retained RTTs run through rtt_to_quality"
        );
    }
}
