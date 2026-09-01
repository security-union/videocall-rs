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
 */

//! Screen-share uplink bitrate governor (issue #2343). Resolution stays pinned;
//! only the bit budget adapts. [`ScreenBaselineKbps`] is a CEILING that a
//! [`ScreenUplinkGovernor`] pulls DOWN, never up.

use crate::constants::{screen_bitrate_kbps_for, SCREEN_MIN_BITRATE_KBPS};

// ── Ladder ──────────────────────────────────────────────────────────────────
pub const SCREEN_BACKOFF_STEP_NUM: u32 = 13;
pub const SCREEN_BACKOFF_STEP_DEN: u32 = 20; // 0.65 per step
pub const SCREEN_BACKOFF_MAX_STEP: u8 = 5;

// ── Governor tuning (all ms; u64 so the const asserts are integer) ──────────
pub const SCREEN_UPLINK_RELIEF_MS: u64 = 100;
pub const SCREEN_UPLINK_PRESSURE_MS: u64 = 300;
pub const SCREEN_UPLINK_PRESSURE_DWELL_MS: u64 = 500;
pub const SCREEN_UPLINK_GAP_MS: u64 = 1_000;
pub const SCREEN_BACKOFF_STEP_DOWN_MIN_INTERVAL_MS: u64 = 2_000;
pub const SCREEN_BACKOFF_RECOVER_QUIET_MS: u64 = 8_000;
pub const SCREEN_BACKOFF_RECOVER_INTERVAL_MS: u64 = 8_000;
pub const SCREEN_BACKOFF_PROBE_INTERVAL_MS: u64 = 30_000;

const _: () = assert!(
    SCREEN_BACKOFF_STEP_NUM < SCREEN_BACKOFF_STEP_DEN,
    "a screen backoff step must REDUCE the geometry baseline, never raise it"
);
const _: () = assert!(SCREEN_UPLINK_RELIEF_MS < SCREEN_UPLINK_PRESSURE_MS);
const _: () =
    assert!(SCREEN_BACKOFF_RECOVER_INTERVAL_MS > SCREEN_BACKOFF_STEP_DOWN_MIN_INTERVAL_MS);

/// Geometry-derived budget for the CURRENT capture, kbps. `for_geometry` is the
/// ONLY constructor, so no tier constant can become one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScreenBaselineKbps(u32);

impl ScreenBaselineKbps {
    pub const fn for_geometry(width: u32, height: u32, fps: u32) -> Self {
        Self(screen_bitrate_kbps_for(width, height, fps))
    }

    pub const fn kbps(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ScreenPressureStep(u8);

impl ScreenPressureStep {
    pub const HEALTHY: Self = Self(0);

    pub const fn from_raw(step: u8) -> Self {
        Self(step)
    }

    pub const fn raw(self) -> u8 {
        self.0
    }
}

/// The bitrate the SCREEN encoder is configured at. NO public constructor —
/// `screen_effective_bitrate_kbps` is the only producer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScreenTargetKbps(u32);

impl ScreenTargetKbps {
    pub const fn kbps(self) -> u32 {
        self.0
    }
}

/// Monotone downward by construction: the trailing `.min(baseline)` admits no
/// input, at any step, for which this exceeds the geometry budget.
pub fn screen_effective_bitrate_kbps(
    baseline: ScreenBaselineKbps,
    step: ScreenPressureStep,
) -> ScreenTargetKbps {
    let mut budget = baseline.kbps() as u64;
    for _ in 0..step.raw().min(SCREEN_BACKOFF_MAX_STEP) {
        budget = budget * SCREEN_BACKOFF_STEP_NUM as u64 / SCREEN_BACKOFF_STEP_DEN as u64;
    }
    ScreenTargetKbps(
        (budget as u32)
            .max(SCREEN_MIN_BITRATE_KBPS)
            .min(baseline.kbps()),
    )
}

/// True when one more pressure step would not lower the target — the max-step
/// clamp and the `SCREEN_MIN_BITRATE_KBPS` clamp, without enumerating either.
pub fn screen_bitrate_at_floor(baseline: ScreenBaselineKbps, step: ScreenPressureStep) -> bool {
    let next = ScreenPressureStep::from_raw(step.raw().saturating_add(1));
    screen_effective_bitrate_kbps(baseline, next) == screen_effective_bitrate_kbps(baseline, step)
}

/// Socket backlog expressed in milliseconds of video at the GEOMETRY BASELINE.
/// The baseline, never the governed target: the actuator must not move its own
/// sensor. `screen_bitrate_kbps_for` floors at 500, so the divisor is never 0.
pub const fn queued_ms_for(buffered_bytes: u64, baseline: ScreenBaselineKbps) -> u64 {
    buffered_bytes.saturating_mul(8) / baseline.kbps() as u64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenUplinkSample {
    /// WebSocket: live `bufferedAmount` (bytes, SHARED socket) + the cumulative
    /// screen-attributed #1921 freshness-gate drop counter.
    Buffered { bytes: u64, gate_drops: u64 },
    /// WebTransport: cumulative screen-unistream drop + ready-stall counters.
    StreamEvents { events: u64 },
    /// The transport cannot report on this tick — no elected connection, or a
    /// borrowed controller cell. Treated exactly like an unobserved interval.
    Unobservable,
}

#[derive(Debug, Clone)]
pub struct ScreenUplinkGovernor {
    step: ScreenPressureStep,
    last_sample_ms: Option<f64>,
    last_step_change_ms: Option<f64>,
    last_pressure_ms: Option<f64>,
    pressure_since_ms: Option<f64>,
    gate_drops_seen: u64,
    stream_events_seen: u64,
    probes_fired: u32,
}

impl Default for ScreenUplinkGovernor {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenUplinkGovernor {
    pub fn new() -> Self {
        Self {
            step: ScreenPressureStep::HEALTHY,
            last_sample_ms: None,
            last_step_change_ms: None,
            last_pressure_ms: None,
            pressure_since_ms: None,
            gate_drops_seen: 0,
            stream_events_seen: 0,
            probes_fired: 0,
        }
    }

    pub fn step(&self) -> ScreenPressureStep {
        self.step
    }

    pub fn probes_fired(&self) -> u32 {
        self.probes_fired
    }

    /// Read the current target without advancing the governor.
    pub fn target_for(&self, baseline: ScreenBaselineKbps) -> ScreenTargetKbps {
        screen_effective_bitrate_kbps(baseline, self.step)
    }

    /// No bit budget left to shed, counting the one step the probe below releases
    /// under unbroken pressure — that dip is not recovered headroom.
    pub fn at_floor(&self, baseline: ScreenBaselineKbps) -> bool {
        let probe_floor = ScreenPressureStep::from_raw(self.step.raw().saturating_add(1));
        screen_bitrate_at_floor(baseline, probe_floor)
    }

    /// `None` until the first step change, which makes every interval gate
    /// vacuous — and the probe moot, since a step above HEALTHY implies one.
    fn step_change_age(&self, now_ms: f64) -> Option<f64> {
        self.last_step_change_ms.map(|t| now_ms - t)
    }

    fn change_step(&mut self, raw: u8, now_ms: f64) {
        self.step = ScreenPressureStep(raw);
        self.last_step_change_ms = Some(now_ms);
    }

    /// A stored stamp AHEAD of `now_ms` means the wall clock stepped backwards
    /// (NTP). Every elapsed gate would then read negative and deny for the whole
    /// rewind, so clamp: one conservative interval, never a stall.
    fn clamp_clocks_to(&mut self, now_ms: f64) {
        self.last_step_change_ms = self.last_step_change_ms.map(|t| t.min(now_ms));
        self.last_pressure_ms = self.last_pressure_ms.map(|t| t.min(now_ms));
        self.pressure_since_ms = self.pressure_since_ms.map(|t| t.min(now_ms));
    }

    /// `None` means the interval carried no usable signal: the first sample, a
    /// tab the browser stopped ticking, a backwards clock, or a transport that
    /// cannot report. Neither pressure nor relief — over an unobserved interval
    /// a cumulative counter delta is not a rate.
    fn classify(
        &mut self,
        now_ms: f64,
        sample: ScreenUplinkSample,
        baseline: ScreenBaselineKbps,
    ) -> Option<(bool, bool)> {
        let dt = self.last_sample_ms.map(|prev| now_ms - prev);
        let observed = matches!(dt, Some(d) if (0.0..=SCREEN_UPLINK_GAP_MS as f64).contains(&d));
        self.last_sample_ms = Some(now_ms);

        match sample {
            ScreenUplinkSample::Buffered { bytes, gate_drops } => {
                let prev = std::mem::replace(&mut self.gate_drops_seen, gate_drops);
                let queued = queued_ms_for(bytes, baseline);
                let dropped = gate_drops.saturating_sub(prev) > 0;
                observed.then_some((
                    queued >= SCREEN_UPLINK_PRESSURE_MS || dropped,
                    queued <= SCREEN_UPLINK_RELIEF_MS && !dropped,
                ))
            }
            ScreenUplinkSample::StreamEvents { events } => {
                let prev = std::mem::replace(&mut self.stream_events_seen, events);
                let moved = events.saturating_sub(prev) > 0;
                observed.then_some((moved, !moved))
            }
            ScreenUplinkSample::Unobservable => {
                // No counter to re-seed: the NEXT delta spans this window too.
                self.last_sample_ms = None;
                None
            }
        }
    }

    pub fn observe(
        &mut self,
        now_ms: f64,
        sample: ScreenUplinkSample,
        baseline: ScreenBaselineKbps,
    ) -> ScreenTargetKbps {
        self.clamp_clocks_to(now_ms);

        match self.classify(now_ms, sample, baseline) {
            // Hold, and make the dwell be re-earned from observed samples.
            None => self.pressure_since_ms = None,
            Some((over, relief)) => {
                if over {
                    self.last_pressure_ms = Some(now_ms);
                    let since = *self.pressure_since_ms.get_or_insert(now_ms);
                    if self.step.raw() < SCREEN_BACKOFF_MAX_STEP
                        && now_ms - since >= SCREEN_UPLINK_PRESSURE_DWELL_MS as f64
                        && self.step_change_age(now_ms).is_none_or(|age| {
                            age >= SCREEN_BACKOFF_STEP_DOWN_MIN_INTERVAL_MS as f64
                        })
                    {
                        self.change_step(self.step.raw() + 1, now_ms);
                    }
                } else {
                    self.pressure_since_ms = None;
                    let quiet = self
                        .last_pressure_ms
                        .is_none_or(|t| now_ms - t >= SCREEN_BACKOFF_RECOVER_QUIET_MS as f64);
                    if relief
                        && self.step.raw() > 0
                        && quiet
                        && self
                            .step_change_age(now_ms)
                            .is_none_or(|age| age >= SCREEN_BACKOFF_RECOVER_INTERVAL_MS as f64)
                    {
                        self.change_step(self.step.raw() - 1, now_ms);
                    }
                }
            }
        }

        // Time-bounded exit, on the ONE path every sample flows through:
        // holding across an unobserved interval cannot postpone the probe.
        if self.step.raw() > 0
            && self
                .step_change_age(now_ms)
                .is_some_and(|age| age >= SCREEN_BACKOFF_PROBE_INTERVAL_MS as f64)
        {
            self.change_step(self.step.raw() - 1, now_ms);
            self.probes_fired += 1;
        }

        self.target_for(baseline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::SCREEN_QUALITY_TIERS;

    const TICK_MS: f64 = 150.0;

    fn pressure(bytes: u64) -> ScreenUplinkSample {
        ScreenUplinkSample::Buffered {
            bytes,
            gate_drops: 0,
        }
    }

    fn ladder(width: u32, height: u32) -> Vec<u32> {
        let baseline = ScreenBaselineKbps::for_geometry(width, height, 10);
        (0..=SCREEN_BACKOFF_MAX_STEP)
            .map(|s| {
                screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(s)).kbps()
            })
            .collect()
    }

    /// Advance under unbroken pressure to `target_step`; returns the next tick.
    fn drive_to_step(
        g: &mut ScreenUplinkGovernor,
        baseline: ScreenBaselineKbps,
        target_step: u8,
        start_ms: f64,
    ) -> f64 {
        let mut t = start_ms;
        while g.step().raw() < target_step {
            assert!(
                t - start_ms < 60_000.0,
                "descent stalled below step {target_step}"
            );
            g.observe(t, pressure(400_000), baseline);
            t += TICK_MS;
        }
        t
    }

    #[test]
    fn healthy_screen_target_is_the_geometry_baseline_not_the_tier_ceiling() {
        assert!(
            screen_bitrate_kbps_for(2560, 1440, 10) < SCREEN_QUALITY_TIERS[0].max_bitrate_kbps,
            "the tier ceiling must exceed the native-geometry budget for this test to bind"
        );

        for (w, h) in [(2560, 1440), (1920, 1080), (1400, 700), (1280, 720)] {
            let baseline = ScreenBaselineKbps::for_geometry(w, h, 10);
            assert_eq!(
                screen_effective_bitrate_kbps(baseline, ScreenPressureStep::HEALTHY).kbps(),
                screen_bitrate_kbps_for(w, h, 10),
                "healthy {w}x{h} must be exactly the geometry baseline"
            );
            for step in 0..=(SCREEN_BACKOFF_MAX_STEP + 3) {
                let got =
                    screen_effective_bitrate_kbps(baseline, ScreenPressureStep::from_raw(step));
                assert!(
                    got.kbps() <= baseline.kbps(),
                    "step {step} at {w}x{h} raised the budget ({} > {})",
                    got.kbps(),
                    baseline.kbps()
                );
            }
        }
    }

    #[test]
    fn each_backoff_step_lowers_the_budget_and_the_floor_binds_on_a_small_share() {
        let native = ladder(2560, 1440);
        assert_eq!(native, vec![4423, 2874, 1868, 1214, 789, 512]);
        for pair in native.windows(2) {
            assert!(pair[1] < pair[0], "step did not reduce: {pair:?}");
        }
        assert!(native.iter().all(|&k| k >= SCREEN_MIN_BITRATE_KBPS));

        let small = ladder(1400, 700);
        assert_eq!(small, vec![1176, 764, 500, 500, 500, 500]);
        assert_eq!(small[2], SCREEN_MIN_BITRATE_KBPS);

        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        assert_eq!(
            screen_effective_bitrate_kbps(
                baseline,
                ScreenPressureStep::from_raw(SCREEN_BACKOFF_MAX_STEP + 7)
            ),
            screen_effective_bitrate_kbps(
                baseline,
                ScreenPressureStep::from_raw(SCREEN_BACKOFF_MAX_STEP)
            )
        );
    }

    #[test]
    fn at_floor_flips_on_the_first_step_the_budget_stops_moving() {
        // Native: the max-step clamp binds. Small: the min-bitrate clamp, earlier.
        for ((w, h), floor_step) in [((2560u32, 1440u32), 5u8), ((1400, 700), 2)] {
            let baseline = ScreenBaselineKbps::for_geometry(w, h, 10);
            assert_eq!(
                ladder(w, h)[floor_step as usize],
                ladder(w, h)[SCREEN_BACKOFF_MAX_STEP as usize],
                "{w}x{h}: step {floor_step} is not where the ladder flattens"
            );
            for step in 0..=(SCREEN_BACKOFF_MAX_STEP + 2) {
                assert_eq!(
                    screen_bitrate_at_floor(baseline, ScreenPressureStep::from_raw(step)),
                    step >= floor_step,
                    "{w}x{h} step {step}"
                );
            }
        }
    }

    #[test]
    fn a_governor_reports_at_floor_only_once_its_descent_bottoms_out() {
        // 1440p flattens at step 5, 1080p at 4; reported one probe step above.
        for ((w, h), reported_from) in [((2560u32, 1440u32), 4u8), ((1920, 1080), 3)] {
            let baseline = ScreenBaselineKbps::for_geometry(w, h, 10);
            let mut g = ScreenUplinkGovernor::new();
            assert!(!g.at_floor(baseline), "{w}x{h}: healthy is not at floor");

            let mut t = 0.0;
            for step in 1..=SCREEN_BACKOFF_MAX_STEP {
                t = drive_to_step(&mut g, baseline, step, t);
                assert_eq!(
                    g.at_floor(baseline),
                    step >= reported_from,
                    "{w}x{h} step {step}"
                );
            }

            while g.step().raw() > 0 {
                g.observe(t, pressure(0), baseline);
                t += TICK_MS;
            }
            assert!(!g.at_floor(baseline), "{w}x{h}: recovery must not latch");
        }
    }

    #[test]
    fn sustained_congestion_never_retracts_the_at_floor_state_signal() {
        for (w, h) in [(2560u32, 1440u32), (1920, 1080)] {
            let baseline = ScreenBaselineKbps::for_geometry(w, h, 10);
            let mut g = ScreenUplinkGovernor::new();
            let bottom = drive_to_step(&mut g, baseline, SCREEN_BACKOFF_MAX_STEP, 0.0);

            let mut t = bottom;
            while t <= bottom + 4.0 * SCREEN_BACKOFF_PROBE_INTERVAL_MS as f64 {
                g.observe(t, pressure(400_000), baseline);
                assert!(
                    g.at_floor(baseline),
                    "{w}x{h} retracted at +{}ms on step {}",
                    t - bottom,
                    g.step().raw()
                );
                t += TICK_MS;
            }
            assert!(g.probes_fired() >= 2, "{w}x{h}: no probe window crossed");
        }
    }

    #[test]
    fn dwell_and_min_interval_bound_the_descent_to_one_step_per_interval() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        assert_eq!(queued_ms_for(200_000, baseline), 361);

        let mut g = ScreenUplinkGovernor::new();
        let mut steps_at = std::collections::HashMap::new();
        let mut t = 0.0;
        while t <= 3_000.0 {
            g.observe(t, pressure(200_000), baseline);
            steps_at.insert(t as u64, g.step().raw());
            t += TICK_MS;
        }

        assert_eq!(steps_at[&0], 0, "the gap-guard seed must not cut");
        assert_eq!(steps_at[&600], 0, "dwell not yet met at 600ms");
        assert_eq!(steps_at[&750], 1, "first cut once the 500ms dwell is met");
        assert_eq!(steps_at[&2700], 1, "min-interval must hold step 1");
        assert_eq!(steps_at[&2850], 2);
    }

    #[test]
    fn intermittent_relief_still_recovers_and_cannot_wedge() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();
        let quiet_start = drive_to_step(&mut g, baseline, SCREEN_BACKOFF_MAX_STEP, 0.0);
        assert_eq!(g.step().raw(), SCREEN_BACKOFF_MAX_STEP);

        let mut healed_at = None;
        for i in 1..=600 {
            let elapsed = i as f64 * TICK_MS;
            let sample = if (elapsed as u64).is_multiple_of(12_000) {
                pressure(400_000)
            } else if i % 2 == 0 {
                pressure(0)
            } else {
                pressure(120_000)
            };
            g.observe(quiet_start + elapsed, sample, baseline);
            if g.step() == ScreenPressureStep::HEALTHY && healed_at.is_none() {
                healed_at = Some(elapsed);
            }
        }

        let healed_at = healed_at.expect("intermittent relief never returned the step to HEALTHY");
        assert!(
            healed_at <= 90_000.0,
            "recovery took {healed_at}ms, over the 90s bound"
        );
        assert_eq!(g.step(), ScreenPressureStep::HEALTHY);
    }

    #[test]
    fn the_wall_clock_probe_releases_a_step_under_unbroken_pressure() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();

        let mut probe_ticks = 0;
        let mut t = 0.0;
        while t <= 200_000.0 {
            let before_step = g.step().raw();
            let before_probes = g.probes_fired();
            g.observe(t, pressure(400_000), baseline);
            if g.probes_fired() > before_probes {
                probe_ticks += 1;
                assert!(
                    g.step().raw() < before_step,
                    "a probe tick must release a step ({before_step} -> {})",
                    g.step().raw()
                );
            }
            t += TICK_MS;
        }

        assert_eq!(probe_ticks, g.probes_fired());
        assert!(
            (4..=8).contains(&g.probes_fired()),
            "probes fired: {}",
            g.probes_fired()
        );
    }

    #[test]
    fn a_suspended_tab_and_a_backward_clock_neither_cut_nor_recover() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);

        let mut g = ScreenUplinkGovernor::new();
        let suspend_at = drive_to_step(&mut g, baseline, 3, 0.0);
        let resume_at = suspend_at + 300_000.0;
        g.observe(resume_at, pressure(0), baseline);
        assert_eq!(
            (g.step().raw(), g.probes_fired()),
            (2, 1),
            "an unobserved interval may move a step only through the probe"
        );
        let mut t = resume_at + TICK_MS;
        while t < resume_at + 4_000.0 {
            g.observe(t, pressure(0), baseline);
            t += TICK_MS;
        }
        assert_eq!(g.step().raw(), 2, "recovery must wait for observed quiet");
        while t <= resume_at + 8_400.0 {
            g.observe(t, pressure(0), baseline);
            t += TICK_MS;
        }
        assert_eq!(g.step().raw(), 1, "one step released after 8s of quiet");

        let mut g = ScreenUplinkGovernor::new();
        let hidden_at = drive_to_step(&mut g, baseline, 1, 0.0);
        let mut t = hidden_at;
        while t <= hidden_at + 20_000.0 {
            g.observe(t, pressure(400_000), baseline);
            assert_eq!(g.step().raw(), 1, "an unobserved interval must not cut");
            t += 1_500.0;
        }

        let mut g = ScreenUplinkGovernor::new();
        let now = drive_to_step(&mut g, baseline, 3, 1_000_000.0);
        let rewound = now - 60_000.0;
        g.observe(rewound, pressure(0), baseline);
        assert_eq!(
            g.step().raw(),
            3,
            "a backwards clock step must neither cut nor release"
        );
        let mut t = rewound + TICK_MS;
        while t <= rewound + 60_000.0 {
            g.observe(t, pressure(0), baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step(),
            ScreenPressureStep::HEALTHY,
            "forward ticks from the rewound clock must still recover"
        );
    }

    #[test]
    fn a_hidden_tab_whose_every_tick_is_a_gap_still_walks_back_to_healthy() {
        const HIDDEN_TICK_MS: f64 = 1_500.0;
        assert!(
            HIDDEN_TICK_MS > SCREEN_UPLINK_GAP_MS as f64,
            "the premise is that every hidden-tab tick reads as unobserved"
        );

        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();
        let hidden_at = drive_to_step(&mut g, baseline, SCREEN_BACKOFF_MAX_STEP, 0.0);

        let mut healed_at = None;
        let mut t = hidden_at;
        while t <= hidden_at + 300_000.0 {
            let before = g.step().raw();
            g.observe(t, pressure(0), baseline);
            assert!(
                g.step().raw() <= before,
                "an unobserved interval must never cut a step"
            );
            if g.step() == ScreenPressureStep::HEALTHY && healed_at.is_none() {
                healed_at = Some(t - hidden_at);
            }
            t += HIDDEN_TICK_MS;
        }

        let healed_at =
            healed_at.expect("a hidden tab pinned the share at its floor for the whole session");
        assert!(
            healed_at
                <= (SCREEN_BACKOFF_MAX_STEP as f64 + 1.0) * SCREEN_BACKOFF_PROBE_INTERVAL_MS as f64,
            "recovery took {healed_at}ms, over the probe-paced bound"
        );
        assert_eq!(g.probes_fired(), SCREEN_BACKOFF_MAX_STEP as u32);
    }

    #[test]
    fn a_gap_makes_the_pressure_dwell_be_re_earned() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();
        g.observe(0.0, pressure(400_000), baseline);
        g.observe(TICK_MS, pressure(400_000), baseline);
        g.observe(2.0 * TICK_MS, pressure(400_000), baseline);

        g.observe(1_500.0, pressure(400_000), baseline);
        g.observe(1_650.0, pressure(400_000), baseline);
        assert_eq!(
            g.step().raw(),
            0,
            "the first observed sample after a gap must not inherit the pre-gap dwell"
        );

        let mut t = 1_800.0;
        while t <= 2_400.0 {
            g.observe(t, pressure(400_000), baseline);
            t += TICK_MS;
        }
        assert_eq!(g.step().raw(), 1, "a re-earned dwell must still cut");
    }

    #[test]
    fn an_unobservable_transport_is_not_a_quiet_webtransport_stream() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut quiet_wt = ScreenUplinkGovernor::new();
        let mut blind = ScreenUplinkGovernor::new();
        let from = drive_to_step(&mut quiet_wt, baseline, 3, 0.0);
        assert_eq!(drive_to_step(&mut blind, baseline, 3, 0.0), from);

        let mut t = from;
        while t <= from + 20_000.0 {
            quiet_wt.observe(t, ScreenUplinkSample::StreamEvents { events: 0 }, baseline);
            blind.observe(t, ScreenUplinkSample::Unobservable, baseline);
            t += TICK_MS;
        }
        assert!(
            quiet_wt.step().raw() < 3,
            "a WT stream with no drops or stalls must recover"
        );
        assert_eq!(
            blind.step().raw(),
            3,
            "a transport that cannot report must hold, not recover"
        );
    }

    #[test]
    fn an_unobservable_sample_holds_the_step_without_advancing_the_pressure_clock() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();
        let blind_from = drive_to_step(&mut g, baseline, 3, 0.0);

        let mut t = blind_from;
        while t < blind_from + 10_000.0 {
            g.observe(t, ScreenUplinkSample::Unobservable, baseline);
            assert_eq!(
                g.step().raw(),
                3,
                "a blind window must neither cut nor recover"
            );
            t += TICK_MS;
        }

        g.observe(t, pressure(0), baseline);
        assert_eq!(g.step().raw(), 3);
        g.observe(t + TICK_MS, pressure(0), baseline);
        assert_eq!(
            g.step().raw(),
            2,
            "quiet observed after the blind window must be credited from the last REAL pressure"
        );
    }

    #[test]
    fn stream_event_deltas_are_pressure_and_their_absence_is_relief() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();

        let mut events = 4_000u64;
        let mut t = 0.0;
        while t <= 3_000.0 {
            g.observe(t, ScreenUplinkSample::StreamEvents { events }, baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step(),
            ScreenPressureStep::HEALTHY,
            "a static counter is not pressure"
        );

        while t <= 20_000.0 {
            events += 1;
            g.observe(t, ScreenUplinkSample::StreamEvents { events }, baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step().raw(),
            SCREEN_BACKOFF_MAX_STEP,
            "a rising unistream drop/stall counter must walk the ladder down"
        );

        let quiet_from = t;
        while t <= quiet_from + 60_000.0 {
            g.observe(t, ScreenUplinkSample::StreamEvents { events }, baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step(),
            ScreenPressureStep::HEALTHY,
            "a counter that stopped moving must recover the whole ladder"
        );
    }

    #[test]
    fn a_sample_inside_the_deadband_is_neither_pressure_nor_relief() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mid = queued_ms_for(120_000, baseline);
        assert!(
            mid > SCREEN_UPLINK_RELIEF_MS && mid < SCREEN_UPLINK_PRESSURE_MS,
            "{mid}ms is not strictly inside the deadband"
        );

        let mut g = ScreenUplinkGovernor::new();
        let deadband_from = drive_to_step(&mut g, baseline, 3, 0.0);
        let mut t = deadband_from;
        while t <= deadband_from + 25_000.0 {
            g.observe(t, pressure(120_000), baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step().raw(),
            3,
            "a deadband sample must neither cut nor recover"
        );

        g.observe(t, pressure(0), baseline);
        assert_eq!(
            g.step().raw(),
            2,
            "the run above must have been held by the deadband, not by a gate"
        );
    }

    #[test]
    fn a_rising_gate_drop_counter_is_pressure_even_when_the_backlog_looks_calm() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        assert!(queued_ms_for(0, baseline) <= SCREEN_UPLINK_RELIEF_MS);

        let mut g = ScreenUplinkGovernor::new();
        let mut t = drive_to_step(&mut g, baseline, 3, 0.0);

        let mut drops = 0u64;
        while t <= 20_000.0 {
            drops += 1;
            g.observe(
                t,
                ScreenUplinkSample::Buffered {
                    bytes: 0,
                    gate_drops: drops,
                },
                baseline,
            );
            t += TICK_MS;
        }
        assert_eq!(
            g.step().raw(),
            SCREEN_BACKOFF_MAX_STEP,
            "gate drops on an empty socket must still walk the ladder down"
        );

        // Same empty socket, counter now still: this must read as relief.
        while t <= 30_000.0 {
            g.observe(
                t,
                ScreenUplinkSample::Buffered {
                    bytes: 0,
                    gate_drops: drops,
                },
                baseline,
            );
            t += TICK_MS;
        }
        assert_eq!(
            g.step().raw(),
            SCREEN_BACKOFF_MAX_STEP - 1,
            "the descent above was the counter, not the byte level"
        );
    }

    #[test]
    fn relief_cannot_release_a_step_until_the_post_pressure_quiet_window_elapses() {
        let baseline = ScreenBaselineKbps::for_geometry(2560, 1440, 10);
        let mut g = ScreenUplinkGovernor::new();
        let mut t = drive_to_step(&mut g, baseline, 3, 0.0);

        // Age the step-change clock past the recover interval on deadband
        // samples, so the quiet gate is the only one that can hold the release.
        while t <= 15_000.0 {
            g.observe(t, pressure(120_000), baseline);
            t += TICK_MS;
        }
        let pressure_at = t;
        g.observe(pressure_at, pressure(400_000), baseline);
        assert_eq!(g.step().raw(), 3, "one sample cannot meet the dwell");

        t = pressure_at + TICK_MS;
        while t < pressure_at + SCREEN_BACKOFF_RECOVER_QUIET_MS as f64 {
            g.observe(t, pressure(0), baseline);
            t += TICK_MS;
        }
        assert_eq!(
            g.step().raw(),
            3,
            "relief inside the quiet window must not release a step"
        );

        while t <= pressure_at + SCREEN_BACKOFF_RECOVER_QUIET_MS as f64 + TICK_MS {
            g.observe(t, pressure(0), baseline);
            t += TICK_MS;
        }
        assert_eq!(g.step().raw(), 2, "the release lands once quiet elapses");
    }
}
