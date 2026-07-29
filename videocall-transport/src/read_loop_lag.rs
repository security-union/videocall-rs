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

//! Pure, host-testable summarizer for the incoming-datagram read-loop lag
//! metric (issue 2031).
//!
//! The WebTransport incoming-datagram reader
//! ([`crate::webtransport::WebTransportService::start_listening_incoming_datagrams`])
//! runs as a `spawn_local` task on the MAIN thread. When the main thread stalls
//! (a long task), that task cannot re-enter `.read()`, the browser's network
//! process age-drops the OLDEST queued datagrams, and audio is silently lost.
//! The gap between two successive `.read()` resolutions is therefore the direct
//! causal signal for reader starvation.
//!
//! This module holds ONLY the arithmetic — no `web_sys`, no wasm — so it can be
//! unit-tested natively (`cargo test -p videocall-transport --lib`). The wasm
//! read loop feeds it real timestamps; the tests feed synthetic ones.

/// Windowed max-gap summarizer for the datagram read loop.
///
/// Each time the read loop's `.read()` resolves, the loop calls [`record`] with
/// the current monotonic timestamp (ms). The struct keeps the LARGEST gap
/// observed between two consecutive resolutions since the last [`take_max_gap_ms`].
/// The health reporter drains it once per health interval (read-and-reset), so
/// the reported value is "the worst reader stall in the last reporting window".
///
/// [`record`]: DatagramReadLoopLagTracker::record
/// [`take_max_gap_ms`]: DatagramReadLoopLagTracker::take_max_gap_ms
#[derive(Debug)]
pub struct DatagramReadLoopLagTracker {
    /// Timestamp (ms) of the previous `.read()` resolution. `None` before the
    /// first resolution. This is INTENTIONALLY preserved across
    /// [`take_max_gap_ms`] so draining the window does not manufacture a phantom
    /// gap on the next `.read()` — the next gap is still measured from the real
    /// previous resolution.
    last_read_ms: Option<f64>,
    /// Largest gap (ms) seen since the last drain. Reset to 0 on
    /// [`take_max_gap_ms`].
    window_max_gap_ms: f64,
}

impl Default for DatagramReadLoopLagTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DatagramReadLoopLagTracker {
    /// A fresh tracker: no prior resolution, empty window. `const` so it can back
    /// a `static` behind a `Mutex` without a lazy initializer.
    pub const fn new() -> Self {
        Self {
            last_read_ms: None,
            window_max_gap_ms: 0.0,
        }
    }

    /// Record a `.read()` resolution at `now_ms` (a monotonic clock in ms).
    /// Returns the gap since the previous resolution (0.0 on the first call, and
    /// clamped at 0.0 so a non-monotonic clock step-back can never register a
    /// negative "gap"). Updates the windowed max as a side effect.
    pub fn record(&mut self, now_ms: f64) -> f64 {
        let gap = match self.last_read_ms {
            Some(prev) => (now_ms - prev).max(0.0),
            None => 0.0,
        };
        self.last_read_ms = Some(now_ms);
        if gap > self.window_max_gap_ms {
            self.window_max_gap_ms = gap;
        }
        gap
    }

    /// Read the current windowed max gap (ms) and reset the window to 0. The
    /// `last_read_ms` anchor is deliberately preserved (see the field doc), so
    /// the first `.read()` after a drain measures its gap from the true previous
    /// resolution, not from the drain instant.
    pub fn take_max_gap_ms(&mut self) -> f64 {
        let m = self.window_max_gap_ms;
        self.window_max_gap_ms = 0.0;
        m
    }

    /// Forget the `last_read_ms` anchor so the NEXT [`record`] establishes a fresh
    /// baseline (gap 0). Called when a new read loop starts: this global tracker
    /// is shared across WebTransport sessions, so without the reset the first
    /// `.read()` of a reconnected session would measure a gap spanning the entire
    /// reconnect downtime — a transport-down interval masquerading as main-thread
    /// reader starvation. The accumulated `window_max_gap_ms` is preserved so a
    /// real stall recorded just before the reset is still reported.
    ///
    /// [`record`]: DatagramReadLoopLagTracker::record
    pub fn reset_anchor(&mut self) {
        self.last_read_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_record_has_no_gap() {
        let mut t = DatagramReadLoopLagTracker::new();
        // The first resolution has no predecessor, so it cannot define a gap.
        assert_eq!(t.record(1000.0), 0.0);
        assert_eq!(t.take_max_gap_ms(), 0.0);
    }

    #[test]
    fn tracks_max_gap_over_window() {
        let mut t = DatagramReadLoopLagTracker::new();
        t.record(0.0);
        assert_eq!(t.record(10.0), 10.0); // 10ms gap
        assert_eq!(t.record(210.0), 200.0); // 200ms stall — the worst
        assert_eq!(t.record(215.0), 5.0); // back to healthy
                                          // The window remembers the WORST gap, not the last one.
        assert_eq!(t.take_max_gap_ms(), 200.0);
    }

    #[test]
    fn take_resets_window_but_preserves_anchor() {
        let mut t = DatagramReadLoopLagTracker::new();
        t.record(0.0);
        t.record(100.0); // 100ms gap
        assert_eq!(t.take_max_gap_ms(), 100.0);
        // Window is drained...
        assert_eq!(t.take_max_gap_ms(), 0.0);
        // ...but the anchor persists: the next resolution's gap is measured from
        // t=100 (the real previous read), NOT from the drain instant. A read at
        // t=150 is therefore a 50ms gap, not 0.
        assert_eq!(t.record(150.0), 50.0);
        assert_eq!(t.take_max_gap_ms(), 50.0);
    }

    #[test]
    fn clamps_non_monotonic_step_back() {
        let mut t = DatagramReadLoopLagTracker::new();
        t.record(1000.0);
        // A clock that steps backward (should not happen with performance.now(),
        // but Date.now() can under NTP correction) must not report a negative gap.
        assert_eq!(t.record(900.0), 0.0);
        assert_eq!(t.take_max_gap_ms(), 0.0);
    }

    #[test]
    fn reset_anchor_prevents_cross_session_phantom_gap() {
        let mut t = DatagramReadLoopLagTracker::new();
        // Session 1: two reads 10ms apart, then the loop breaks (transport close).
        t.record(1000.0);
        t.record(1010.0);
        assert_eq!(t.take_max_gap_ms(), 10.0);
        // A long reconnect elapses (transport down ~30s), then session 2 starts.
        // Without reset_anchor, the first read below would measure a 30_000ms gap
        // from session 1's last read — a false "reader starvation" reading.
        t.reset_anchor();
        assert_eq!(
            t.record(31_010.0),
            0.0,
            "the first read of a new session must be a fresh baseline, not a reconnect gap"
        );
        assert_eq!(t.take_max_gap_ms(), 0.0);
        // Real gaps in session 2 are still measured normally.
        assert_eq!(t.record(31_110.0), 100.0);
    }

    #[test]
    fn reset_anchor_preserves_accumulated_window_max() {
        let mut t = DatagramReadLoopLagTracker::new();
        t.record(0.0);
        t.record(200.0); // a real 200ms stall just before the loop restarts
        t.reset_anchor();
        // The pre-reset stall is NOT erased — it is still reported on the next drain.
        assert_eq!(t.take_max_gap_ms(), 200.0);
    }

    #[test]
    fn window_max_survives_a_later_smaller_gap() {
        let mut t = DatagramReadLoopLagTracker::new();
        t.record(0.0);
        t.record(500.0); // 500ms stall
        for i in 1..=10 {
            // Ten healthy 5ms reads after the stall must not erase the recorded max.
            t.record(500.0 + (i as f64) * 5.0);
        }
        assert_eq!(t.take_max_gap_ms(), 500.0);
    }
}
