//! Publisher side: the drain/probe/hold policy that turns the latest report
//! `(capacity C, backlog Q)` and its age into the encoder target.
//!
//! - Feedback cycle `T = I + RTT` (`I` the viewers' report period, `RTT` the
//!   round trip the caller measures): how long any decision runs before a
//!   periodic report can show its effect.
//! - Backlog: send `C * max(f, 1 - Q / T)` for `max(T, Q / (1 - f))`, which
//!   drains `Q` at full depth in one cycle when it can, and at `f * C`
//!   otherwise ([`MIN_DRAIN_FRACTION`] `= f`); then hold `C` until the next
//!   report. A backlog is perishable: it is acted on only while its report
//!   is fresh, so the drain never outlives `T + I`.
//! - No backlog and a fresh report (`age <= T + I`, one missed report
//!   tolerated): probe at `C * (1 + p)`, `p = θ / T` with `θ` the
//!   publisher's own frame interval. A wrong probe builds at most one frame
//!   interval of queue (the viewer's detection threshold) per cycle, against
//!   `(1 - f) * T` the drain clears per cycle.
//! - Stale report: hold `C`, never probe and never drain on old news.
//!   Capacity has memory, backlog does not.
//! - `C` is never taken below [`PublisherConfig::floor_bps`] (the bottom
//!   tier's floor), so no report can ask for less than the bottom tier at
//!   `f` of its frames.
//! - A report without a capacity (the path never queued: what it delivered
//!   is only a lower bound) does not lower anything: the target is the
//!   maximum. An encoder that later fills a narrower path makes a queue,
//!   and the queue's report brings the target down.
//! - Never above the configured maximum; before any report, the start rate.

use crate::viewer::Report;

/// Lowest fraction of the reported capacity a drain may send: the trade-off
/// between drain time and picture. A drain at `f * C` clears a backlog `Q` in
/// `Q / (1 - f)`; `f = 1/2` costs at most half the bitrate and clears any
/// backlog in twice its own length.
pub const MIN_DRAIN_FRACTION: f64 = 0.5;

/// What the publisher knows about itself and its viewers' report period.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PublisherConfig {
    /// Encoder target before any report, bps.
    pub start_bps: f64,
    /// Configured maximum, bps.
    pub max_bps: f64,
    /// Its own frame interval (1000 / camera fps), ms.
    pub frame_interval_ms: f64,
    /// The viewers' report period, ms.
    pub report_interval_ms: f64,
    /// Lowest capacity acted on, bps: the bottom tier's floor
    /// ([`crate::Tier::floor_bps`] of the last tier). A report below it is
    /// a viewer one encoding cannot serve; it is treated as this.
    pub floor_bps: f64,
}

impl PublisherConfig {
    /// Feedback cycle `T = I + RTT`, ms.
    pub fn cycle_ms(&self, rtt_ms: f64) -> f64 {
        self.report_interval_ms + rtt_ms.max(0.0)
    }

    /// Probe step `p = θ / T` per cycle. Also the tier-up margin
    /// ([`crate::actuator::choose_tier`]).
    pub fn probe_fraction(&self, rtt_ms: f64) -> f64 {
        self.frame_interval_ms / self.cycle_ms(rtt_ms)
    }

    /// A report is fresh while `age <= T + I` (one missed report tolerated).
    pub fn fresh_ms(&self, rtt_ms: f64) -> f64 {
        self.cycle_ms(rtt_ms) + self.report_interval_ms
    }
}

/// The policy itself: the target for a report of this age, bps. Pure.
pub fn drain_target_bps(config: &PublisherConfig, report: Report, age_ms: f64, rtt_ms: f64) -> f64 {
    let floor = config.floor_bps.min(config.max_bps);
    let capacity = match report.capacity_bps {
        Some(bps) => (bps as f64).max(floor),
        // No ceiling seen: nothing here bounds the rate, so the policy works
        // from the maximum (a backlog still drains from it).
        // `MultiViewerPublisher` always passes its own paced capacity.
        None => config.max_bps,
    };
    let cycle = config.cycle_ms(rtt_ms);
    let backlog = report.backlog_ms;
    let fresh = age_ms <= config.fresh_ms(rtt_ms);
    let target = if !fresh {
        capacity
    } else if backlog > 0.0 {
        let drain_ms = cycle.max(backlog / (1.0 - MIN_DRAIN_FRACTION));
        if age_ms < drain_ms {
            capacity * (1.0 - backlog / cycle).max(MIN_DRAIN_FRACTION)
        } else {
            capacity
        }
    } else {
        capacity * (1.0 + config.probe_fraction(rtt_ms))
    };
    target.min(config.max_bps)
}

/// Single-viewer publisher: keeps the latest report and answers the target.
/// For several viewers use [`crate::viewers::MultiViewerPublisher`].
#[derive(Debug, Clone)]
pub struct Publisher {
    config: PublisherConfig,
    latest: Option<(f64, Report)>,
}

impl Publisher {
    /// Creates the publisher.
    pub fn new(config: PublisherConfig) -> Self {
        Self {
            config,
            latest: None,
        }
    }

    /// Its configuration.
    pub fn config(&self) -> &PublisherConfig {
        &self.config
    }

    /// Replaces the configuration and keeps the reports (camera restart,
    /// frame-rate or cap change): the target goes on from what the viewers
    /// said, not from the start rate.
    pub fn reconfigure(&mut self, config: PublisherConfig) {
        self.config = config;
    }

    /// A report arrived at `now_ms` (publisher clock).
    pub fn on_report(&mut self, now_ms: f64, report: Report) {
        self.latest = Some((now_ms, report));
    }

    /// The latest report and when it arrived, if any.
    pub fn latest(&self) -> Option<(f64, Report)> {
        self.latest
    }

    /// Encoder target at `now_ms`, bps, given the current round-trip
    /// estimate to the viewer. Call it at every frame and every report.
    pub fn target_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        match self.latest {
            Some((received, report)) => {
                drain_target_bps(&self.config, report, now_ms - received, rtt_ms)
            }
            None => self.config.start_bps.min(self.config.max_bps),
        }
    }
}
