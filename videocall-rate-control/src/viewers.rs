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
use crate::signal::{LimitSource, PacedCapacity};
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
    /// Its current silence has already cut the rate (one cut per episode).
    quiet_cut: bool,
}

/// Publisher with one latest report per viewer, keyed by any viewer id.
///
/// It owns one invariant for going up: the capacity the policy acts on
/// never rises by more than one probe step per feedback cycle, whatever
/// lets it rise; decreases apply at once (see [`crate::signal`]). One-bit
/// evidence (the relay's CONGESTION signal, a viewer that goes quiet) cuts
/// that capacity, and the same rule brings it back.
#[derive(Debug, Clone)]
pub struct MultiViewerPublisher<K: Ord> {
    config: PublisherConfig,
    latest: BTreeMap<K, ViewerState>,
    /// The paced ceiling on the target.
    capacity: PacedCapacity,
    /// The last relay signal taken (its own debounce).
    relay_signal_ms: f64,
    /// The link was lost and no report has come since.
    link_lost: bool,
    /// A report with a measured ceiling arrived since the last update.
    measured_since_update: bool,
    /// Start of the current unbroken run of sent frames.
    sending_since_ms: f64,
    /// The latest frame sent.
    last_sent_ms: f64,
    /// The latest tier change made by [`Self::next_tier`].
    last_tier_change_ms: f64,
}

impl<K: Ord> MultiViewerPublisher<K> {
    /// Creates the publisher with no viewers, at the start rate.
    pub fn new(config: PublisherConfig) -> Self {
        Self {
            config,
            latest: BTreeMap::new(),
            capacity: PacedCapacity::new(config.start_bps.min(config.max_bps)),
            relay_signal_ms: f64::NEG_INFINITY,
            link_lost: false,
            measured_since_update: false,
            sending_since_ms: f64::NEG_INFINITY,
            last_sent_ms: f64::NEG_INFINITY,
            last_tier_change_ms: f64::NEG_INFINITY,
        }
    }

    /// Its configuration.
    pub fn config(&self) -> &PublisherConfig {
        &self.config
    }

    /// Replaces the configuration and keeps every report and the paced
    /// capacity. Use it on a camera restart, a frame-rate change or a new
    /// cap instead of building a new publisher: the target goes on from
    /// where it was. A lower cap applies at once, a higher one is climbed
    /// to one probe step per cycle.
    pub fn reconfigure(&mut self, config: PublisherConfig) {
        self.config = config;
    }

    /// A new camera frame interval (1000 / fps), keeping everything else.
    pub fn set_frame_interval_ms(&mut self, frame_interval_ms: f64) {
        self.config.frame_interval_ms = frame_interval_ms;
    }

    /// A report from `viewer` arrived at `now_ms` (publisher clock). A
    /// report without a capacity (that path never queued) sets no ceiling.
    /// It also ends the viewer's silence, if it was quiet.
    ///
    /// A report with a ceiling is a measurement, and a rise it shows is
    /// taken at the next update (the next sent frame) instead of one probe
    /// step per cycle. What one viewer can do with it is bounded: the
    /// ceiling acted on is the lowest among the reports, so a huge one
    /// (hostile or mistaken) lifts nothing past the others; alone, it is
    /// bounded by the maximum, and its own next backlog report brings the
    /// target down at once. It never undoes a relay cut, which speaks for
    /// viewers that do not report.
    pub fn on_report(&mut self, viewer: K, now_ms: f64, report: Report) {
        self.measured_since_update |= report.capacity_bps.is_some();
        self.latest.insert(
            viewer,
            ViewerState {
                received_ms: now_ms,
                report,
                quiet_cut: false,
            },
        );
        self.link_lost = false;
    }

    /// The viewer left: its report no longer counts.
    pub fn remove_viewer(&mut self, viewer: &K) {
        self.latest.remove(viewer);
    }

    /// Viewers with a report.
    pub fn viewers(&self) -> usize {
        self.latest.len()
    }

    /// The relay's CONGESTION signal arrived at `now_ms`; returns whether
    /// it cut the capacity. `from` is the congested receiver when the
    /// caller can tell. A receiver whose fresh report has a ceiling is
    /// ignored: its reports already say how much it takes. One whose fresh
    /// report has no ceiling has said nothing about a limit, so the relay's
    /// drop is the only evidence and counts. Signals within one cycle of
    /// the last relay signal taken are ignored (the cut has not had time to
    /// show, and the relay sends one per congested receiver).
    pub fn on_congestion_signal(&mut self, now_ms: f64, rtt_ms: f64, from: Option<&K>) -> bool {
        self.update(now_ms, rtt_ms);
        let fresh_ms = self.config.fresh_ms(rtt_ms);
        let trusted = from
            .and_then(|viewer| self.latest.get(viewer))
            .is_some_and(|state| {
                now_ms - state.received_ms <= fresh_ms && state.report.capacity_bps.is_some()
            });
        if trusted || now_ms - self.relay_signal_ms < self.config.cycle_ms(rtt_ms) {
            return false;
        }
        self.relay_signal_ms = now_ms;
        self.cut(now_ms, rtt_ms, LimitSource::RelaySignal);
        true
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
    /// every frame sent (not for skipped ones): it moves the paced capacity
    /// on, and it is where a viewer that reported and then went quiet is
    /// noticed. Such a silence cuts the capacity once per episode: one
    /// halving stands for one missing report, because silence is often not
    /// this publisher's path (a backgrounded phone, the viewer's own uplink
    /// dropping its reports). The episode ends with its next report; the
    /// rise rule then governs the climb. Quiet time counts only while
    /// frames are being sent: a run of sent frames starts over after a gap
    /// longer than [`CAPACITY_WINDOW_MS`] (camera off), and viewers that
    /// left or never reported are not in the map. Returns whether it cut.
    pub fn on_frame_sent(&mut self, now_ms: f64, rtt_ms: f64) -> bool {
        self.update(now_ms, rtt_ms);
        if now_ms - self.last_sent_ms > CAPACITY_WINDOW_MS {
            self.sending_since_ms = now_ms;
        }
        self.last_sent_ms = self.last_sent_ms.max(now_ms);
        let silence_ms = self.silence_ms(rtt_ms);
        let since = self.sending_since_ms;
        let mut quiet = false;
        for state in self.latest.values_mut() {
            if !state.quiet_cut && now_ms - state.received_ms.max(since) > silence_ms {
                state.quiet_cut = true;
                quiet = true;
            }
        }
        if quiet {
            self.cut(now_ms, rtt_ms, LimitSource::QuietViewer);
        }
        quiet
    }

    /// The link to the relay was lost and every viewer is gone with it.
    /// The rate does not snap back to the start rate: the target in force
    /// is held and climbs one probe step per cycle until reports return or
    /// it reaches the maximum. Call it instead of rebuilding or
    /// reconfiguring the publisher.
    pub fn on_link_lost(&mut self, now_ms: f64, rtt_ms: f64) {
        let held = self.target_bps(now_ms, rtt_ms);
        self.latest.clear();
        self.link_lost = true;
        self.capacity.hold(now_ms, held, LimitSource::LinkLost);
    }

    fn cut(&mut self, now_ms: f64, rtt_ms: f64, source: LimitSource) {
        let target = self.target_bps(now_ms, rtt_ms);
        let current = self.ceiling_bps(now_ms, rtt_ms);
        self.capacity
            .cut(now_ms, current, target, self.config.floor_bps, source);
    }

    /// The capacity the reports alone give: the lowest ceiling, or the
    /// maximum when no report has one (no path has queued); never above the
    /// maximum (a capacity beyond anything that could be sent means
    /// nothing). With no report: the start rate, or the maximum after a
    /// lost link (the held rate climbs back).
    fn raw_capacity_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        let max = self.config.max_bps;
        match self.combined(now_ms, rtt_ms) {
            Some(combined) => combined
                .report
                .capacity_bps
                .map_or(max, |bps| (bps as f64).min(max)),
            None if self.link_lost => max,
            None => self.config.start_bps.min(max),
        }
    }

    /// The most the target could be at `now_ms` on the reports alone: the
    /// policy's answer with no backlog (one probe step above a fresh
    /// capacity, the capacity itself when stale or without reports).
    fn raw_ceiling_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        let capacity = self.raw_capacity_bps(now_ms, rtt_ms);
        match self.combined(now_ms, rtt_ms) {
            Some(combined) => drain_target_bps(
                &self.config,
                Report {
                    capacity_bps: Some(capacity.round() as u64),
                    backlog_ms: 0.0,
                },
                combined.age_ms,
                rtt_ms,
            ),
            None => capacity,
        }
    }

    /// The paced ceiling on the target at `now_ms`.
    fn ceiling_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        self.capacity.at(
            now_ms,
            self.raw_ceiling_bps(now_ms, rtt_ms),
            self.config.cycle_ms(rtt_ms),
            self.config.probe_fraction(rtt_ms),
        )
    }

    fn update(&mut self, now_ms: f64, rtt_ms: f64) {
        let raw = self.raw_ceiling_bps(now_ms, rtt_ms);
        let cycle = self.config.cycle_ms(rtt_ms);
        let probe = self.config.probe_fraction(rtt_ms);
        if std::mem::take(&mut self.measured_since_update) && self.measurement_may_lift() {
            self.capacity.lift(now_ms, raw);
        }
        self.capacity.update(now_ms, raw, cycle, probe);
    }

    /// Whether a measured rise may be taken at once: not over a relay cut
    /// (it speaks for viewers that do not report), nor over a quiet cut
    /// while a viewer is still quiet (its own report must end it).
    fn measurement_may_lift(&self) -> bool {
        match self.capacity.last_source() {
            LimitSource::RelaySignal => false,
            LimitSource::QuietViewer => self.latest.values().all(|state| !state.quiet_cut),
            LimitSource::Rise | LimitSource::LinkLost => true,
        }
    }

    /// The capacity the tier choice acts on at `now_ms`, bps: what the
    /// reports give, cut by one-bit evidence, never above the paced ceiling
    /// on the target (so it rises at most one probe step per cycle too).
    pub fn capacity_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        self.raw_capacity_bps(now_ms, rtt_ms)
            .min(self.ceiling_bps(now_ms, rtt_ms))
    }

    /// What holds [`Self::capacity_bps`] below what the reports give, and
    /// that capacity, bps; `None` when the reports govern.
    pub fn limit(&self, now_ms: f64, rtt_ms: f64) -> Option<(LimitSource, f64)> {
        let source = self.capacity.source(
            now_ms,
            self.raw_ceiling_bps(now_ms, rtt_ms),
            self.config.cycle_ms(rtt_ms),
            self.config.probe_fraction(rtt_ms),
        )?;
        Some((source, self.capacity_bps(now_ms, rtt_ms)))
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

    /// The tier to use at `now_ms`, from `current` (an index into `tiers`,
    /// top first). Call it on every frame; start from 0 and the first call
    /// drops to the start rate's tier.
    ///
    /// With reports, [`crate::choose_tier`] on [`Self::capacity_bps`]: a
    /// measured capacity is noisy, so a step up needs one probe step of room
    /// above the upper floor. With no report, the capacity is the
    /// publisher's own number (the start rate, or a cut growing back), with
    /// no noise to absorb, so the tier is simply the highest whose floor it
    /// affords. Without that, a start rate just above a floor (980 kbps
    /// over 960) would never climb back to its own tier after a relay cut
    /// in a room of viewers that never report, and the encoder bitrate,
    /// clamped to the tier, would sit on the wrong rung for the rest of the
    /// call. Either way it steps down at once and up at most one rung per
    /// feedback cycle (a step up shows in the reports only a cycle later).
    pub fn next_tier(&mut self, tiers: &[Tier], current: usize, now_ms: f64, rtt_ms: f64) -> usize {
        self.update(now_ms, rtt_ms);
        let capacity = self.capacity_bps(now_ms, rtt_ms);
        let wanted = if self.latest.is_empty() {
            tiers
                .iter()
                .position(|tier| tier.floor_bps() <= capacity)
                .unwrap_or(tiers.len().saturating_sub(1))
        } else {
            choose_tier(tiers, current, capacity, self.config.probe_fraction(rtt_ms))
        };
        let next = if wanted < current {
            current - 1
        } else {
            wanted
        };
        let paced = now_ms - self.last_tier_change_ms >= self.config.cycle_ms(rtt_ms);
        if next == current || (next < current && !paced) {
            return current;
        }
        self.last_tier_change_ms = now_ms;
        next
    }

    /// Encoder target at `now_ms`, bps: the drain/probe/hold policy on the
    /// reports, never above the paced ceiling. Rising toward the ceiling is
    /// free (a drain ending returns to a capacity already measured); the
    /// ceiling itself rises at most one probe step per cycle, and one-bit
    /// evidence cuts it. With no report the target is the ceiling: the
    /// start rate, or a cut or lost link climbing back. `rtt_ms`: pass the
    /// largest of the viewers' round trips (a longer cycle probes more
    /// gently).
    pub fn target_bps(&self, now_ms: f64, rtt_ms: f64) -> f64 {
        let ceiling = self.ceiling_bps(now_ms, rtt_ms);
        let Some(combined) = self.combined(now_ms, rtt_ms) else {
            return ceiling;
        };
        // The policy on the reported capacity as is: a drain from a capacity
        // above the cap is a drain from that capacity, not from the cap.
        drain_target_bps(&self.config, combined.report, combined.age_ms, rtt_ms).min(ceiling)
    }
}
