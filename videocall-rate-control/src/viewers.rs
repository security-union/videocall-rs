//! Several viewers: the publisher gets one report per viewer and has one
//! encoder.
//!
//! The rule lives in one function, [`combine_reports`], so it can be
//! swapped: act on the lowest reported capacity and the largest reported
//! backlog among viewers with a fresh report.
//!
//! **This lets one slow viewer lower the quality for everyone.** It is the
//! simplest rule that never overruns any viewer's path with a single
//! encoding; whether a meeting should instead drop a slow viewer to a lower
//! layer, or ignore it past some point, is an open product decision. A
//! viewer slower than the bottom tier's floor is clamped to that floor
//! ([`PublisherConfig::floor_bps`]): it cannot pull the room below the
//! bottom tier at half its frames.
//!
//! Capacity has memory, backlog is perishable: with no fresh report the
//! capacity is the lowest remembered one and the backlog is zero, so the
//! policy holds and never drains on old news.

use std::collections::BTreeMap;

use crate::actuator::{choose_tier, Tier};
use crate::publisher::{drain_target_bps, PublisherConfig};
use crate::signal::CongestionLimit;
use crate::viewer::{Report, CAPACITY_WINDOW_MS};

/// The report the publisher acts on, and the age the drain/probe/hold
/// policy should see for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Combined {
    /// Lowest capacity (at least the floor), largest fresh backlog.
    pub report: Report,
    /// Age of the report that set the backlog when there is one; else the
    /// oldest fresh report's; else (nothing fresh) the oldest report's, ms.
    pub age_ms: f64,
}

/// The multi-viewer rule. `reports` are `(age_ms, report)`, one per viewer.
///
/// - Capacity: the lowest ceiling among reports no older than `fresh_ms`;
///   if none is fresh, the lowest of all (capacity has memory). A report
///   without a capacity (that path never queued) sets no ceiling, so it
///   never pulls the capacity down; `None` when no report has one. Never
///   below `floor_bps`.
/// - Backlog: the largest among fresh reports only; zero if none is fresh.
///
/// `None` without any report. One pass, no allocation.
pub fn combine_reports(
    reports: impl IntoIterator<Item = (f64, Report)>,
    fresh_ms: f64,
    floor_bps: f64,
) -> Option<Combined> {
    fn lower(held: Option<u64>, new: Option<u64>) -> Option<u64> {
        match (held, new) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
    let mut any = false;
    let mut any_fresh = false;
    let mut all_capacity: Option<u64> = None;
    let mut all_oldest = f64::NEG_INFINITY;
    let mut fresh_capacity: Option<u64> = None;
    let mut fresh_oldest = f64::NEG_INFINITY;
    // (age, ms) of the largest fresh backlog.
    let mut backlog = (0.0_f64, 0.0_f64);
    for (age, report) in reports {
        any = true;
        all_capacity = lower(all_capacity, report.capacity_bps);
        all_oldest = all_oldest.max(age);
        if age <= fresh_ms {
            any_fresh = true;
            fresh_capacity = lower(fresh_capacity, report.capacity_bps);
            fresh_oldest = fresh_oldest.max(age);
            if report.backlog_ms > backlog.1 {
                backlog = (age, report.backlog_ms);
            }
        }
    }
    if !any {
        return None;
    }
    // Saturating float-to-int cast; a floor is never negative.
    let floor = floor_bps.max(0.0) as u64;
    let (capacity_bps, age_ms) = if !any_fresh {
        (all_capacity, all_oldest)
    } else if backlog.1 > 0.0 {
        (fresh_capacity, backlog.0)
    } else {
        (fresh_capacity, fresh_oldest)
    };
    Some(Combined {
        report: Report {
            capacity_bps: capacity_bps.map(|bps| bps.max(floor)),
            backlog_ms: backlog.1,
        },
        age_ms,
    })
}

/// One viewer's latest report and its silence bookkeeping.
#[derive(Debug, Clone, Copy)]
struct ViewerState {
    received_ms: f64,
    report: Report,
    /// When its silence last cut the rate.
    silence_cut_ms: f64,
}

/// Publisher with one latest report per viewer, keyed by any viewer id, and
/// a one-bit congestion limit fed by the relay's CONGESTION signal, by
/// viewers that go quiet, and by a lost link.
#[derive(Debug, Clone)]
pub struct MultiViewerPublisher<K: Ord> {
    config: PublisherConfig,
    latest: BTreeMap<K, ViewerState>,
    limit: CongestionLimit,
    /// The same cut and growth, fed by viewers that went quiet; separate
    /// from `limit` because it ends when they report again.
    silence: CongestionLimit,
    /// Start of the current unbroken run of sent frames.
    sending_since_ms: f64,
    /// The latest frame sent.
    last_sent_ms: f64,
    /// The latest tier change made by [`Self::next_tier`].
    last_tier_change_ms: f64,
}

impl<K: Ord> MultiViewerPublisher<K> {
    /// Creates the publisher with no viewers.
    pub fn new(config: PublisherConfig) -> Self {
        Self {
            config,
            latest: BTreeMap::new(),
            limit: CongestionLimit::new(),
            silence: CongestionLimit::new(),
            sending_since_ms: f64::NEG_INFINITY,
            last_sent_ms: f64::NEG_INFINITY,
            last_tier_change_ms: f64::NEG_INFINITY,
        }
    }

    /// Its configuration.
    pub fn config(&self) -> &PublisherConfig {
        &self.config
    }

    /// Replaces the configuration and keeps every report and the congestion
    /// limit. Use it on a camera restart, a frame-rate change or a new cap
    /// instead of building a new publisher, so the target goes on from the
    /// remembered capacity instead of jumping back to the start rate.
    pub fn reconfigure(&mut self, config: PublisherConfig) {
        self.config = config;
    }

    /// A new camera frame interval (1000 / fps), keeping everything else.
    pub fn set_frame_interval_ms(&mut self, frame_interval_ms: f64) {
        self.config.frame_interval_ms = frame_interval_ms;
    }

    /// Lowest ceiling among all reports, any age.
    fn lowest_ceiling_bps(&self) -> Option<u64> {
        self.latest
            .values()
            .filter_map(|state| state.report.capacity_bps)
            .min()
    }

    /// The reports are about to show no ceiling at all, so the policy will
    /// say "the maximum". The rate gets there by probing, not by a leap:
    /// what was in force becomes a limit that grows by the probe step per
    /// cycle (unless a limit is already pacing the rate).
    fn open_from(&mut self, now_ms: f64, held_bps: f64) {
        if self.limit.is_unset() {
            self.limit.hold(now_ms, held_bps);
        }
    }

    /// A report from `viewer` arrived at `now_ms` (publisher clock).
    ///
    /// A report without a capacity (the path never queued) sets no ceiling.
    /// When no report has one, the target probes up from where it was (the
    /// start rate, or the ceiling that just went away) by one probe step
    /// per cycle until a queue shows or it reaches the maximum.
    pub fn on_report(&mut self, viewer: K, now_ms: f64, report: Report) {
        if report.capacity_bps.is_none() {
            let before = self.lowest_ceiling_bps();
            let others = self
                .latest
                .iter()
                .filter(|(key, _)| **key != viewer)
                .filter_map(|(_, state)| state.report.capacity_bps)
                .min();
            if others.is_none() {
                let held = match (self.latest.is_empty(), before) {
                    (true, _) => Some(self.config.start_bps),
                    (false, Some(ceiling)) => Some(ceiling as f64),
                    (false, None) => None,
                };
                if let Some(held) = held {
                    self.open_from(now_ms, held.min(self.config.max_bps));
                }
            }
        }
        self.latest.insert(
            viewer,
            ViewerState {
                received_ms: now_ms,
                report,
                silence_cut_ms: f64::NEG_INFINITY,
            },
        );
        self.end_silence_if_over();
    }

    /// A quiet viewer's cut stands in for the report it is not sending; once
    /// every viewer that went quiet has reported again (or left), the reports
    /// say how much each takes and the stand-in goes.
    fn end_silence_if_over(&mut self) {
        if self
            .latest
            .values()
            .all(|state| state.silence_cut_ms == f64::NEG_INFINITY)
        {
            self.silence.clear();
        }
    }

    /// The viewer left at `now_ms`: its report no longer counts. If it was
    /// the last one with a ceiling, the rest probe up from that ceiling (see
    /// [`Self::on_report`]).
    pub fn remove_viewer(&mut self, viewer: &K, now_ms: f64) {
        let before = self.lowest_ceiling_bps();
        self.latest.remove(viewer);
        self.end_silence_if_over();
        if let (Some(ceiling), None, false) =
            (before, self.lowest_ceiling_bps(), self.latest.is_empty())
        {
            self.open_from(now_ms, (ceiling as f64).min(self.config.max_bps));
        }
    }

    /// Viewers with a report.
    pub fn viewers(&self) -> usize {
        self.latest.len()
    }

    /// The relay's CONGESTION signal arrived at `now_ms`. `from` is the
    /// congested receiver when the caller can tell; a receiver with a fresh
    /// report is ignored (its reports already say how much it takes, and the
    /// limit must not fight them). Returns whether the limit was cut.
    pub fn on_congestion_signal(&mut self, now_ms: f64, rtt_ms: f64, from: Option<&K>) -> bool {
        let fresh_ms = self.config.fresh_ms(rtt_ms);
        let reported = from
            .and_then(|viewer| self.latest.get(viewer))
            .is_some_and(|state| now_ms - state.received_ms <= fresh_ms);
        if reported {
            return false;
        }
        self.cut(now_ms, rtt_ms)
    }

    /// How long a viewer that has reported may stay quiet while frames go
    /// out before its silence counts as congestion, ms: its report going
    /// stale (`T + I`, [`PublisherConfig::fresh_ms`]) plus one gap hold
    /// ([`CAPACITY_WINDOW_MS`]), the longest a healthy viewer goes without
    /// news (it sends nothing while frames are held behind a lost one).
    pub fn silence_ms(&self, rtt_ms: f64) -> f64 {
        self.config.fresh_ms(rtt_ms) + CAPACITY_WINDOW_MS
    }

    /// A camera frame was handed to the transport at `now_ms`. Call it for
    /// every frame sent (not for skipped ones). This is where a viewer that
    /// has reported and then went quiet while frames kept going out is
    /// noticed: its frames are not being released, which is congestion, so
    /// the congestion limit is cut as for the relay's signal, at most once
    /// per [`Self::silence_ms`] of continued silence per viewer. Quiet time
    /// counts only while frames are being sent: a run of sent frames starts
    /// over after a gap longer than [`CAPACITY_WINDOW_MS`] (camera off), and
    /// viewers that left or never reported are not in the map. Returns
    /// whether the limit was cut.
    pub fn on_frame_sent(&mut self, now_ms: f64, rtt_ms: f64) -> bool {
        if now_ms - self.last_sent_ms > CAPACITY_WINDOW_MS {
            self.sending_since_ms = now_ms;
        }
        self.last_sent_ms = self.last_sent_ms.max(now_ms);
        let silence_ms = self.silence_ms(rtt_ms);
        let since = self.sending_since_ms;
        let mut quiet = false;
        for state in self.latest.values_mut() {
            let quiet_from = state.received_ms.max(since).max(state.silence_cut_ms);
            if now_ms - quiet_from > silence_ms {
                state.silence_cut_ms = now_ms;
                quiet = true;
            }
        }
        if !quiet {
            return false;
        }
        let target = self.target_bps(now_ms, rtt_ms);
        let cycle = self.config.cycle_ms(rtt_ms);
        self.silence
            .on_signal(now_ms, target, cycle, self.config.floor_bps)
    }

    /// The link to the relay was lost and every viewer is gone with it. The
    /// rate does not snap back to the start rate: what was being sent
    /// becomes a congestion limit that grows back by the probe step per
    /// cycle, exactly as after a relay signal, until reports return or it
    /// reaches the maximum. Call it instead of rebuilding or reconfiguring
    /// the publisher.
    pub fn on_link_lost(&mut self, now_ms: f64, rtt_ms: f64) {
        let held = self.target_bps(now_ms, rtt_ms);
        self.latest.clear();
        self.silence.clear();
        self.limit.hold(now_ms, held);
    }

    fn cut(&mut self, now_ms: f64, rtt_ms: f64) -> bool {
        let target = self.target_bps(now_ms, rtt_ms);
        let cycle = self.config.cycle_ms(rtt_ms);
        self.limit
            .on_signal(now_ms, target, cycle, self.config.floor_bps)
    }

    /// The congestion limit in force at `now_ms`, bps, if any: the lower of
    /// the relay / link-lost limit and the quiet-viewer limit.
    pub fn congestion_limit_bps(&self, now_ms: f64, rtt_ms: f64) -> Option<f64> {
        let cycle = self.config.cycle_ms(rtt_ms);
        let probe = self.config.probe_fraction(rtt_ms);
        let max = self.config.max_bps;
        let relay = self.limit.limit_bps(now_ms, cycle, probe, max);
        let silence = self.silence.limit_bps(now_ms, cycle, probe, max);
        match (relay, silence) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The combined report at `now_ms` ([`combine_reports`]).
    pub fn combined(&self, now_ms: f64, rtt_ms: f64) -> Option<Combined> {
        combine_reports(
            self.latest
                .values()
                .map(|state| (now_ms - state.received_ms, state.report)),
            self.config.fresh_ms(rtt_ms),
            self.config.floor_bps,
        )
    }

    /// The capacity to choose the tier on ([`crate::choose_tier`]): the
    /// combined ceiling, or the maximum when the reports show none (no path
    /// has queued), never above the maximum (a capacity beyond anything
    /// that could be sent means nothing), capped by the congestion limit;
    /// the limit alone when no viewer reports; `None` when there is neither.
    pub fn tier_capacity_bps(&self, now_ms: f64, rtt_ms: f64) -> Option<f64> {
        let max = self.config.max_bps;
        let limit = self.congestion_limit_bps(now_ms, rtt_ms);
        let capacity = self.combined(now_ms, rtt_ms).map(|combined| {
            combined
                .report
                .capacity_bps
                .map_or(max, |bps| (bps as f64).min(max))
        });
        match (capacity, limit) {
            (Some(capacity), Some(limit)) => Some(capacity.min(limit)),
            (capacity, limit) => capacity.or(limit),
        }
    }

    /// The tier to use at `now_ms`, from `current` (an index into `tiers`,
    /// top first): [`crate::choose_tier`] on [`Self::tier_capacity_bps`],
    /// stepping down at once but up at most one rung per feedback cycle
    /// (a step up shows in the reports only a cycle later). Safe to call on
    /// every frame. Returns `current` when there is no capacity at all.
    pub fn next_tier(&mut self, tiers: &[Tier], current: usize, now_ms: f64, rtt_ms: f64) -> usize {
        let Some(capacity) = self.tier_capacity_bps(now_ms, rtt_ms) else {
            return current;
        };
        let probe = self.config.probe_fraction(rtt_ms);
        let next = choose_tier(tiers, current, capacity, probe);
        let paced = now_ms - self.last_tier_change_ms >= self.config.cycle_ms(rtt_ms);
        if next == current || (next < current && !paced) {
            return current;
        }
        self.last_tier_change_ms = now_ms;
        next
    }

    /// Encoder target at `now_ms`, bps. `rtt_ms`: pass the largest of the
    /// viewers' round trips (a longer cycle probes more gently).
    pub fn target_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        let policy = match self.combined(now_ms, rtt_ms) {
            Some(combined) => {
                drain_target_bps(&self.config, combined.report, combined.age_ms, rtt_ms)
            }
            None => self.config.start_bps.min(self.config.max_bps),
        };
        self.congestion_limit_bps(now_ms, rtt_ms)
            .map_or(policy, |limit| policy.min(limit))
    }
}
