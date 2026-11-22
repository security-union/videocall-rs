//! One-bit congestion signal: the relay's CONGESTION packet.
//!
//! Viewers that never report (older web clients, and the native clients,
//! which send no diagnostics) still sit behind the relay. When a receiver's
//! outbound queue overflows the relay drops and tells the publisher, one
//! bit, no numbers. [`CongestionLimit`] turns that into a cap on the target
//! with nothing but the quantities the policy already has: on a signal the
//! limit is the current target times [`MIN_DRAIN_FRACTION`]; every feedback
//! cycle `T` without a signal it rises by the probe step `(1 + p)`; it caps
//! the target and the capacity used for tier choice, and disappears once it
//! reaches the configured maximum. That is AIMD: halve on loss, grow by one
//! probe step per cycle.
//!
//! Signals less than one cycle after the last accepted one are ignored: the
//! cut has not had time to show, and the relay sends one signal per
//! congested receiver.

use crate::publisher::MIN_DRAIN_FRACTION;

/// The limit set by the relay's congestion signal. Pure: time passed in.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CongestionLimit {
    /// `(when the last signal was accepted, the limit it set)`.
    cut: Option<(f64, f64)>,
}

impl CongestionLimit {
    /// No limit.
    pub fn new() -> Self {
        Self::default()
    }

    /// A congestion signal at `now_ms` while the publisher's target is
    /// `target_bps`. `cycle_ms` is the feedback cycle `T`, `floor_bps` the
    /// publisher's capacity floor (the limit never goes below
    /// `floor * MIN_DRAIN_FRACTION`, the drain floor). Returns whether the
    /// signal was taken (false within one cycle of the last one).
    pub fn on_signal(
        &mut self,
        now_ms: f64,
        target_bps: f64,
        cycle_ms: f64,
        floor_bps: f64,
    ) -> bool {
        if self.cut.is_some_and(|(at, _)| now_ms - at < cycle_ms) || !target_bps.is_finite() {
            return false;
        }
        let limit = (target_bps * MIN_DRAIN_FRACTION).max(floor_bps * MIN_DRAIN_FRACTION);
        self.cut = Some((now_ms, limit));
        true
    }

    /// The limit at `now_ms`, bps, or `None` once it has grown to `max_bps`
    /// (or without any signal). Grows by `(1 + probe_fraction)` per whole
    /// cycle since the last accepted signal.
    pub fn limit_bps(
        &self,
        now_ms: f64,
        cycle_ms: f64,
        probe_fraction: f64,
        max_bps: f64,
    ) -> Option<f64> {
        let (at, limit) = self.cut?;
        let cycles = ((now_ms - at) / cycle_ms)
            .floor()
            .clamp(0.0, f64::from(i32::MAX));
        // `cycles` is a whole number in i32 range; a huge one overflows to
        // infinity, which is past any maximum.
        let grown = limit * (1.0 + probe_fraction).powi(cycles as i32);
        (grown < max_bps).then_some(grown)
    }

    /// Forgets the signal.
    pub fn clear(&mut self) {
        self.cut = None;
    }
}
