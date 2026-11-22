//! How the publisher's capacity is allowed to move, and why it is where it
//! is.
//!
//! One rule covers every way up that nothing measured: the ceiling on the
//! target never rises by more than one probe step `(1 + p)` per feedback
//! cycle `T`,
//! whatever lets it rise (a viewer with a ceiling leaves, a quiet viewer
//! comes back, a cut grows out, the link returns, the cap is raised).
//! Decreases apply at once. A rise a viewer measured (a report with a
//! ceiling, which only a measured busy period or clean window gives) is
//! taken at once: the path already showed it. It never undoes a relay cut,
//! which speaks for viewers that do not report, nor a quiet viewer's cut
//! while that viewer is still quiet. A step up shows in the reports one cycle later,
//! so one step per cycle is as fast as the publisher can learn whether it
//! was safe.
//!
//! One-bit evidence (the relay's CONGESTION packet, a viewer that goes
//! quiet) is a cut: the ceiling drops to the current target times
//! [`MIN_DRAIN_FRACTION`], never below the floor times that, and the same
//! rule brings it back. That is AIMD, halve on loss and grow by one probe
//! step per cycle, built from the policy's own numbers. Each source keeps
//! its own debounce in the publisher, so one never mutes another.

use crate::publisher::MIN_DRAIN_FRACTION;

/// What holds the capacity below what the reports alone would give.
/// For display: the UI can name the limit instead of calling everything a
/// relay limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitSource {
    /// Only the per-cycle rise: the reports allow more and the capacity is
    /// on its way up (a ceiling went away, the cap was raised).
    Rise,
    /// The relay's CONGESTION signal cut it.
    RelaySignal,
    /// A viewer that had reported went quiet while frames went out.
    QuietViewer,
    /// The link to the relay was lost; the rate in force is climbing back.
    LinkLost,
}

/// The ceiling on the target after the rise rule, as of the last update.
/// Pure: time is passed in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct PacedCapacity {
    /// When it was last updated; `None` before the first update.
    at_ms: Option<f64>,
    bps: f64,
    /// Why it is below what the reports allow, while it is.
    source: LimitSource,
}

impl PacedCapacity {
    pub(crate) fn new(start_bps: f64) -> Self {
        Self {
            at_ms: None,
            bps: start_bps,
            source: LimitSource::Rise,
        }
    }

    /// The most the capacity may be at `now_ms`: one probe step per cycle
    /// since the last update, and at most one step however long ago that
    /// was (nothing was learned while nothing was sent).
    fn allowed_bps(&self, now_ms: f64, cycle_ms: f64, probe_fraction: f64) -> f64 {
        let Some(at) = self.at_ms else {
            return self.bps;
        };
        let cycles = ((now_ms - at) / cycle_ms).clamp(0.0, 1.0);
        self.bps * (1.0 + probe_fraction).powf(cycles)
    }

    /// The capacity at `now_ms` when the reports alone give `raw_bps`.
    pub(crate) fn at(&self, now_ms: f64, raw_bps: f64, cycle_ms: f64, probe_fraction: f64) -> f64 {
        raw_bps.min(self.allowed_bps(now_ms, cycle_ms, probe_fraction))
    }

    /// What holds it at `now_ms`, if anything.
    pub(crate) fn source(
        &self,
        now_ms: f64,
        raw_bps: f64,
        cycle_ms: f64,
        probe_fraction: f64,
    ) -> Option<LimitSource> {
        (raw_bps > self.allowed_bps(now_ms, cycle_ms, probe_fraction)).then_some(self.source)
    }

    /// Moves the state to `now_ms`. Call it often (every frame): the rise
    /// accrues between updates.
    pub(crate) fn update(&mut self, now_ms: f64, raw_bps: f64, cycle_ms: f64, probe_fraction: f64) {
        let allowed = self.allowed_bps(now_ms, cycle_ms, probe_fraction);
        if raw_bps <= allowed {
            // The reports govern again; whatever cut it is over.
            self.source = LimitSource::Rise;
        }
        self.bps = raw_bps.min(allowed);
        self.at_ms = Some(self.at_ms.map_or(now_ms, |at| at.max(now_ms)));
    }

    /// What holds it, ignoring whether it binds right now.
    pub(crate) fn last_source(&self) -> LimitSource {
        self.source
    }

    /// A viewer measured more: the ceiling goes to `raw_bps` at `now_ms`
    /// at once, if that is higher.
    pub(crate) fn lift(&mut self, now_ms: f64, raw_bps: f64) {
        if raw_bps > self.bps {
            self.bps = raw_bps;
            self.source = LimitSource::Rise;
        }
        self.at_ms = Some(self.at_ms.map_or(now_ms, |at| at.max(now_ms)));
    }

    /// One-bit evidence of a limit at `now_ms`, with the capacity at
    /// `current_bps` and the target at `target_bps`: the capacity drops to
    /// `target * MIN_DRAIN_FRACTION` (never below `floor *
    /// MIN_DRAIN_FRACTION`), or stays where it is if that is already lower.
    pub(crate) fn cut(
        &mut self,
        now_ms: f64,
        current_bps: f64,
        target_bps: f64,
        floor_bps: f64,
        source: LimitSource,
    ) {
        let cut = (target_bps * MIN_DRAIN_FRACTION).max(floor_bps * MIN_DRAIN_FRACTION);
        self.hold(now_ms, current_bps.min(cut), source);
    }

    /// Holds the capacity at `bps` from `now_ms`, labelled `source`.
    pub(crate) fn hold(&mut self, now_ms: f64, bps: f64, source: LimitSource) {
        if bps.is_finite() {
            self.bps = bps;
            self.at_ms = Some(now_ms);
            self.source = source;
        }
    }
}
