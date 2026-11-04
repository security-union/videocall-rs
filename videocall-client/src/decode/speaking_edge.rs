// SPDX-License-Identifier: MIT OR Apache-2.0

//! Receiver-side speaking edges for one peer: the decoder VAD's raw per-frame
//! verdict, debounced and rate-limited into `[PEER_SPEAKING]` log lines.
//! `on` needs speech frames spanning the hold with no quiet gap of the hold or
//! longer (shorter dips are bridged).

/// A state must hold this long before it is logged. Also the quiet gap that
/// ends a speech episode, so sub-hold pauses between words do not split it.
pub(crate) const SPEAKING_EDGE_HOLD_MS: u64 = 300;

/// Minimum spacing between two logged edges for one peer, except
/// [`SpeakingEdgeFilter::force_off`].
pub(crate) const SPEAKING_EDGE_MIN_INTERVAL_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    PendingOn {
        since: u64,
        quiet_since: Option<u64>,
    },
    On,
    PendingOff {
        since: u64,
    },
}

/// A debounced speaking edge. `edge_ms` is the onset of the new state, not the
/// time the edge was confirmed or emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpeakingEdge {
    pub speaking: bool,
    pub edge_ms: u64,
}

impl SpeakingEdge {
    pub(crate) fn log_line(&self, peer_id: &str) -> String {
        let state = if self.speaking { "on" } else { "off" };
        format!(
            "[PEER_SPEAKING] speaking={state} peer={peer_id} edge_ms={}",
            self.edge_ms
        )
    }
}

#[derive(Debug)]
pub(crate) struct SpeakingEdgeFilter {
    phase: Phase,
    /// Onset of the current debounced state.
    edge_ms: u64,
    logged_speaking: bool,
    last_log_ms: Option<u64>,
    last_now_ms: u64,
}

impl SpeakingEdgeFilter {
    pub(crate) fn new() -> Self {
        Self {
            phase: Phase::Idle,
            edge_ms: 0,
            logged_speaking: false,
            last_log_ms: None,
            last_now_ms: 0,
        }
    }

    /// Feed one raw VAD verdict. Returns the edge to log, if any.
    ///
    /// An edge held back by the rate limit is not queued: the first call after
    /// the window opens emits the CURRENT debounced state if it still differs
    /// from the last logged one, so an edge that reverted inside the window
    /// collapses with its predecessor.
    pub(crate) fn observe(&mut self, speaking: bool, now_ms: u64) -> Option<SpeakingEdge> {
        self.rebase(now_ms);
        self.advance(speaking, now_ms);
        let debounced = matches!(self.phase, Phase::On | Phase::PendingOff { .. });
        if debounced == self.logged_speaking {
            return None;
        }
        if let Some(last) = self.last_log_ms {
            if now_ms.saturating_sub(last) < SPEAKING_EDGE_MIN_INTERVAL_MS {
                return None;
            }
        }
        Some(self.record(debounced, now_ms))
    }

    /// The stream stopped for good (mute, teardown): close a logged episode now,
    /// bypassing both the hold and the rate limit, since no later frame will.
    pub(crate) fn force_off(&mut self, now_ms: u64) -> Option<SpeakingEdge> {
        self.rebase(now_ms);
        self.edge_ms = match self.phase {
            Phase::PendingOff { since } => since,
            Phase::On => now_ms,
            Phase::Idle | Phase::PendingOn { .. } => self.edge_ms,
        };
        self.phase = Phase::Idle;
        if !self.logged_speaking {
            return None;
        }
        Some(self.record(false, now_ms))
    }

    /// Shift stored times back with a wall clock that stepped back, so a hold
    /// or the rate window is never measured across the step.
    fn rebase(&mut self, now_ms: u64) {
        let back = self.last_now_ms.saturating_sub(now_ms);
        self.last_now_ms = now_ms;
        if back == 0 {
            return;
        }
        let shift = |t: u64| t.saturating_sub(back);
        self.phase = match self.phase {
            Phase::PendingOn { since, quiet_since } => Phase::PendingOn {
                since: shift(since),
                quiet_since: quiet_since.map(shift),
            },
            Phase::PendingOff { since } => Phase::PendingOff {
                since: shift(since),
            },
            phase => phase,
        };
        self.edge_ms = shift(self.edge_ms);
        self.last_log_ms = self.last_log_ms.map(shift);
    }

    fn record(&mut self, speaking: bool, now_ms: u64) -> SpeakingEdge {
        self.logged_speaking = speaking;
        self.last_log_ms = Some(now_ms);
        SpeakingEdge {
            speaking,
            edge_ms: self.edge_ms,
        }
    }

    fn advance(&mut self, speaking: bool, now: u64) {
        let held = |t: u64| now.saturating_sub(t) >= SPEAKING_EDGE_HOLD_MS;
        self.phase = match (self.phase, speaking) {
            (Phase::Idle, false) => Phase::Idle,
            (Phase::Idle, true) => Phase::PendingOn {
                since: now,
                quiet_since: None,
            },
            (
                Phase::PendingOn {
                    quiet_since: Some(q),
                    ..
                },
                _,
            ) if held(q) => {
                if speaking {
                    Phase::PendingOn {
                        since: now,
                        quiet_since: None,
                    }
                } else {
                    Phase::Idle
                }
            }
            (Phase::PendingOn { since, .. }, true) if held(since) => {
                self.edge_ms = since;
                Phase::On
            }
            (Phase::PendingOn { since, .. }, true) => Phase::PendingOn {
                since,
                quiet_since: None,
            },
            (Phase::PendingOn { since, quiet_since }, false) => Phase::PendingOn {
                since,
                quiet_since: quiet_since.or(Some(now)),
            },
            (Phase::On, true) | (Phase::PendingOff { .. }, true) => Phase::On,
            (Phase::On, false) => Phase::PendingOff { since: now },
            (Phase::PendingOff { since }, false) if held(since) => {
                self.edge_ms = since;
                Phase::Idle
            }
            (pending_off @ Phase::PendingOff { .. }, false) => pending_off,
        };
    }

    #[cfg(test)]
    pub(crate) fn is_pending_on(&self) -> bool {
        matches!(self.phase, Phase::PendingOn { .. })
    }

    #[cfg(test)]
    pub(crate) fn logged_speaking(&self) -> bool {
        self.logged_speaking
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: u64 = 10;

    /// Feed `speaking` every 10 ms over `[from, to)`, collecting emitted edges.
    fn run(f: &mut SpeakingEdgeFilter, speaking: bool, from: u64, to: u64) -> Vec<SpeakingEdge> {
        (from..to)
            .step_by(FRAME_MS as usize)
            .filter_map(|t| f.observe(speaking, t))
            .collect()
    }

    fn on(edge_ms: u64) -> SpeakingEdge {
        SpeakingEdge {
            speaking: true,
            edge_ms,
        }
    }

    fn off(edge_ms: u64) -> SpeakingEdge {
        SpeakingEdge {
            speaking: false,
            edge_ms,
        }
    }

    #[test]
    fn a_sub_hold_blip_logs_nothing() {
        let mut f = SpeakingEdgeFilter::new();
        let mut edges = run(&mut f, true, 0, 250);
        edges.extend(run(&mut f, false, 250, 3_000));
        assert_eq!(edges, vec![]);
    }

    #[test]
    fn a_sustained_edge_logs_once_with_its_onset() {
        let mut f = SpeakingEdgeFilter::new();
        let edges = run(&mut f, true, 1_000, 5_000);
        assert_eq!(edges, vec![on(1_000)]);
    }

    #[test]
    fn on_is_confirmed_exactly_at_the_hold() {
        let mut f = SpeakingEdgeFilter::new();
        assert_eq!(f.observe(true, 0), None);
        assert_eq!(f.observe(true, 299), None);
        assert_eq!(f.observe(true, 300), Some(on(0)));
    }

    #[test]
    fn on_to_off_needs_the_hold_and_pauses_shorter_than_it_do_not_split() {
        let mut f = SpeakingEdgeFilter::new();
        let mut edges = run(&mut f, true, 0, 2_000);
        edges.extend(run(&mut f, false, 2_000, 2_290));
        edges.extend(run(&mut f, true, 2_290, 3_000));
        assert_eq!(edges, vec![on(0)], "a 290 ms pause must not log off");

        edges.extend(run(&mut f, false, 3_000, 3_290));
        assert_eq!(edges, vec![on(0)]);
        edges.extend(run(&mut f, false, 3_290, 3_310));
        assert_eq!(edges, vec![on(0), off(3_000)]);
    }

    /// A raw VAD dip inside the pending window is bridged; a quiet gap of the
    /// full hold cancels the pending episode before the next speech frame.
    #[test]
    fn pending_on_bridges_short_dips_and_cancels_on_a_held_gap() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 0);
        run(&mut f, false, 10, 300);
        assert_eq!(f.observe(true, 300), Some(on(0)));

        let mut g = SpeakingEdgeFilter::new();
        g.observe(true, 0);
        run(&mut g, false, 10, 320);
        assert_eq!(g.observe(true, 320), None, "gap from 10 to 310 ended it");
        assert!(g.is_pending_on());
        assert_eq!(g.observe(true, 620), Some(on(320)));
    }

    #[test]
    fn speech_between_dips_restarts_the_quiet_gap() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 0);
        f.observe(false, 10);
        run(&mut f, true, 20, 280);
        run(&mut f, false, 280, 330);
        assert_eq!(f.observe(true, 330), Some(on(0)));
    }

    #[test]
    fn a_stalled_frame_after_a_held_gap_starts_a_new_onset() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 0);
        f.observe(false, 10);
        assert_eq!(f.observe(true, 400), None);
        assert_eq!(f.observe(true, 699), None);
        assert_eq!(f.observe(true, 700), Some(on(400)));
    }

    #[test]
    fn the_window_opens_exactly_one_second_after_the_last_line() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 0);
        assert_eq!(f.observe(true, 300), Some(on(0)));
        f.observe(false, 400);
        assert_eq!(f.observe(false, 700), None);
        assert_eq!(f.observe(false, 1_299), None);
        assert_eq!(f.observe(false, 1_300), Some(off(400)));
    }

    #[test]
    fn a_step_back_in_pending_on_shifts_the_onset() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 10_000);
        assert_eq!(run(&mut f, true, 5_000, 5_310), vec![on(5_000)]);
    }

    #[test]
    fn a_step_back_in_pending_on_shifts_the_quiet_gap() {
        let mut f = SpeakingEdgeFilter::new();
        f.observe(true, 10_000);
        run(&mut f, false, 10_010, 10_200);
        run(&mut f, false, 5_000, 5_130);
        assert_eq!(run(&mut f, true, 5_130, 5_500), vec![on(5_130)]);
    }

    #[test]
    fn a_step_back_in_pending_off_shifts_the_quiet_onset() {
        let mut f = SpeakingEdgeFilter::new();
        run(&mut f, true, 10_000, 10_400);
        f.observe(false, 10_500);
        assert_eq!(run(&mut f, false, 5_000, 5_800), vec![]);
        assert_eq!(f.observe(false, 5_800), Some(off(5_000)));
    }

    #[test]
    fn a_step_back_shifts_the_onset_of_a_deferred_edge() {
        let mut f = SpeakingEdgeFilter::new();
        run(&mut f, true, 10_000, 10_310);
        run(&mut f, false, 10_500, 10_890);
        assert_eq!(f.observe(false, 10_490), None);
        assert_eq!(run(&mut f, false, 10_500, 11_000), vec![off(10_110)]);
    }

    #[test]
    fn force_off_after_a_step_back_reports_the_shifted_onset() {
        let mut f = SpeakingEdgeFilter::new();
        run(&mut f, true, 10_000, 10_400);
        f.observe(false, 10_500);
        assert_eq!(f.force_off(5_000), Some(off(5_000)));
    }

    #[test]
    fn a_rate_limited_edge_is_deferred_to_the_window_not_dropped() {
        let mut f = SpeakingEdgeFilter::new();
        let mut edges = run(&mut f, true, 0, 300);
        assert_eq!(edges, vec![]);
        edges.extend(run(&mut f, true, 300, 500));
        assert_eq!(edges, vec![on(0)], "on emitted at 300");

        // Off confirmed at 800, inside the window that opens at 1300.
        edges.extend(run(&mut f, false, 500, 1_300));
        assert_eq!(edges, vec![on(0)]);
        edges.extend(run(&mut f, false, 1_300, 1_310));
        assert_eq!(edges, vec![on(0), off(500)]);
    }

    #[test]
    fn an_edge_that_reverts_inside_the_window_collapses() {
        let mut f = SpeakingEdgeFilter::new();
        let mut edges = run(&mut f, true, 0, 500);
        edges.extend(run(&mut f, false, 500, 850));
        edges.extend(run(&mut f, true, 850, 2_500));
        assert_eq!(edges, vec![on(0)], "off at 800 and on at 1150 collapse");
    }

    #[test]
    fn the_window_bounds_lines_per_peer() {
        let mut f = SpeakingEdgeFilter::new();
        let mut emitted_at = Vec::new();
        for t in (0..20_000).step_by(FRAME_MS as usize) {
            let speaking = (t / 600) % 2 == 0;
            if f.observe(speaking, t).is_some() {
                emitted_at.push(t);
            }
        }
        assert!(emitted_at.len() >= 10, "{emitted_at:?}");
        for w in emitted_at.windows(2) {
            assert!(w[1] - w[0] >= 1_000, "{emitted_at:?}");
        }
    }

    #[test]
    fn force_off_closes_a_logged_episode_immediately() {
        let mut f = SpeakingEdgeFilter::new();
        run(&mut f, true, 0, 400);
        run(&mut f, false, 400, 450);
        assert_eq!(
            f.force_off(450),
            Some(off(400)),
            "onset is when quiet began"
        );
        assert_eq!(f.force_off(460), None);
    }

    #[test]
    fn force_off_without_a_logged_on_logs_nothing() {
        let mut f = SpeakingEdgeFilter::new();
        assert_eq!(f.force_off(0), None);
        run(&mut f, true, 0, 200);
        assert_eq!(f.force_off(200), None);
        assert_eq!(
            run(&mut f, true, 200, 400),
            vec![],
            "pending state was cleared"
        );
    }

    #[test]
    fn a_wall_clock_step_back_does_not_wedge_the_filter() {
        let mut f = SpeakingEdgeFilter::new();
        assert_eq!(run(&mut f, true, 10_000, 10_400), vec![on(10_000)]);
        assert_eq!(f.observe(false, 5_000), None);
        assert_eq!(run(&mut f, false, 5_010, 6_400), vec![off(5_000)]);
    }

    #[test]
    fn log_line_format() {
        assert_eq!(
            on(1_759_000_000_123).log_line("42"),
            "[PEER_SPEAKING] speaking=on peer=42 edge_ms=1759000000123"
        );
        assert_eq!(
            off(7).log_line("42"),
            "[PEER_SPEAKING] speaking=off peer=42 edge_ms=7"
        );
    }
}
