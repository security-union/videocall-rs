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

use crate::publisher::{drain_target_bps, PublisherConfig};
use crate::signal::CongestionLimit;
use crate::viewer::Report;

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
/// - Capacity: the lowest among reports no older than `fresh_ms`; if none
///   is fresh, the lowest of all (capacity has memory). Never below
///   `floor_bps`.
/// - Backlog: the largest among fresh reports only; zero if none is fresh.
///
/// `None` without any report. One pass, no allocation.
pub fn combine_reports(
    reports: impl IntoIterator<Item = (f64, Report)>,
    fresh_ms: f64,
    floor_bps: f64,
) -> Option<Combined> {
    let mut any = false;
    let mut all_capacity = u64::MAX;
    let mut all_oldest = f64::NEG_INFINITY;
    let mut fresh_capacity: Option<u64> = None;
    let mut fresh_oldest = f64::NEG_INFINITY;
    // (age, ms) of the largest fresh backlog.
    let mut backlog = (0.0_f64, 0.0_f64);
    for (age, report) in reports {
        any = true;
        all_capacity = all_capacity.min(report.capacity_bps);
        all_oldest = all_oldest.max(age);
        if age <= fresh_ms {
            fresh_capacity =
                Some(fresh_capacity.map_or(report.capacity_bps, |c| c.min(report.capacity_bps)));
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
    let (capacity_bps, age_ms) = match fresh_capacity {
        Some(capacity) if backlog.1 > 0.0 => (capacity, backlog.0),
        Some(capacity) => (capacity, fresh_oldest),
        None => (all_capacity, all_oldest),
    };
    Some(Combined {
        report: Report {
            capacity_bps: capacity_bps.max(floor),
            backlog_ms: backlog.1,
        },
        age_ms,
    })
}

/// Publisher with one latest report per viewer, keyed by any viewer id, and
/// the relay's one-bit congestion limit for viewers that never report.
#[derive(Debug, Clone)]
pub struct MultiViewerPublisher<K: Ord> {
    config: PublisherConfig,
    latest: BTreeMap<K, (f64, Report)>,
    limit: CongestionLimit,
}

impl<K: Ord> MultiViewerPublisher<K> {
    /// Creates the publisher with no viewers.
    pub fn new(config: PublisherConfig) -> Self {
        Self {
            config,
            latest: BTreeMap::new(),
            limit: CongestionLimit::new(),
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

    /// A report from `viewer` arrived at `now_ms` (publisher clock).
    pub fn on_report(&mut self, viewer: K, now_ms: f64, report: Report) {
        self.latest.insert(viewer, (now_ms, report));
    }

    /// The viewer left: its report no longer counts.
    pub fn remove_viewer(&mut self, viewer: &K) {
        self.latest.remove(viewer);
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
            .is_some_and(|(received, _)| now_ms - received <= fresh_ms);
        if reported {
            return false;
        }
        let target = self.target_bps(now_ms, rtt_ms);
        let cycle = self.config.cycle_ms(rtt_ms);
        self.limit
            .on_signal(now_ms, target, cycle, self.config.floor_bps)
    }

    /// The congestion limit in force at `now_ms`, bps, if any.
    pub fn congestion_limit_bps(&self, now_ms: f64, rtt_ms: f64) -> Option<f64> {
        self.limit.limit_bps(
            now_ms,
            self.config.cycle_ms(rtt_ms),
            self.config.probe_fraction(rtt_ms),
            self.config.max_bps,
        )
    }

    /// The combined report at `now_ms` ([`combine_reports`]).
    pub fn combined(&self, now_ms: f64, rtt_ms: f64) -> Option<Combined> {
        combine_reports(
            self.latest
                .values()
                .map(|(received, report)| (now_ms - received, *report)),
            self.config.fresh_ms(rtt_ms),
            self.config.floor_bps,
        )
    }

    /// The capacity to choose the tier on ([`crate::choose_tier`]): the
    /// combined capacity, capped by the congestion limit; the limit alone
    /// when no viewer reports; `None` when there is neither.
    pub fn tier_capacity_bps(&self, now_ms: f64, rtt_ms: f64) -> Option<f64> {
        let limit = self.congestion_limit_bps(now_ms, rtt_ms);
        let capacity = self
            .combined(now_ms, rtt_ms)
            .map(|combined| combined.report.capacity_bps as f64);
        match (capacity, limit) {
            (Some(capacity), Some(limit)) => Some(capacity.min(limit)),
            (capacity, limit) => capacity.or(limit),
        }
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
