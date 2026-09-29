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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Escalation for repeated #1638 downlink sheds (#2726, contract E0-E16).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::actors::session_logic::{
    downlink_congested_epoch_now, downlink_epoch_is_active_at, DOWNLINK_EPOCH_NEVER,
};
use crate::constants::{
    WT_SHED_ESCALATION_DELIVERY_STALLED_ROUNDS, WT_SHED_ESCALATION_HOLD,
    WT_SHED_ESCALATION_MAX_ROUND_GAP, WT_SHED_ESCALATION_ROUND, WT_SHED_ESCALATION_STAGE1_ROUNDS,
    WT_SHED_ESCALATION_STAGE1_RUNWAY_ROUNDS, WT_SHED_ESCALATION_STAGE1_WINDOW,
    WT_SHED_ESCALATION_STAGE2_ROUNDS, WT_SHED_ESCALATION_STAGE2_WINDOW,
};

/// `label` is the `reason` on `relay_wt_session_closes_total` (#2726).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCloseReason {
    ShedRounds,
    /// Not one lane write completed across the run, so nothing at all — audio
    /// included — was accepted against this peer's flow-control credit.
    DeliveryStalled,
}

impl SessionCloseReason {
    pub fn label(self) -> &'static str {
        match self {
            SessionCloseReason::ShedRounds => "shed_rounds",
            SessionCloseReason::DeliveryStalled => "delivery_stalled",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationAction {
    Proceed,
    ArmedStage1,
    Close(SessionCloseReason),
}

type Round = (u64, u64);

struct Rounds {
    opened: VecDeque<Round>,
    /// Consecutive rounds whose write count matches their predecessor's: the
    /// span over which nothing reached the peer.
    stalled_run: usize,
    /// `None` until stage 1 arms, which is what stops a sparse burst series
    /// from ever closing a session.
    rounds_since_stage1: Option<usize>,
    stage1_booked: bool,
}

struct EscalationInner {
    rounds: Mutex<Rounds>,
    stage1_epoch: AtomicU64,
    closed: AtomicBool,
    /// Lane writes that COMPLETED: bytes quinn accepted against the peer's
    /// flow-control credit. Written by every lane, read once per round.
    writes_completed: AtomicU64,
}

/// One receiver's shed-escalation state (#2726), shared by its downlink lanes
/// and its transport actor's admission path; dropped with the session.
#[derive(Clone)]
pub struct DownlinkShedEscalation {
    inner: Arc<EscalationInner>,
}

impl Default for DownlinkShedEscalation {
    fn default() -> Self {
        Self::new()
    }
}

impl DownlinkShedEscalation {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(EscalationInner {
                rounds: Mutex::new(Rounds {
                    opened: VecDeque::new(),
                    stalled_run: 0,
                    rounds_since_stage1: None,
                    stage1_booked: false,
                }),
                stage1_epoch: AtomicU64::new(DOWNLINK_EPOCH_NEVER),
                closed: AtomicBool::new(false),
                writes_completed: AtomicU64::new(0),
            }),
        }
    }

    /// NOT an acknowledgement: credit exists only because the peer's
    /// application read, so a frozen count means it stopped reading.
    pub fn note_write_completed(&self) {
        self.inner.writes_completed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn writes_completed(&self) -> u64 {
        self.inner.writes_completed.load(Ordering::Relaxed)
    }

    pub fn record_shed(&self) -> EscalationAction {
        self.record_shed_at(downlink_congested_epoch_now(), self.writes_completed())
    }

    /// Stage 1 is evaluated BEFORE stage 2, and stage 2 also requires stage 1 to
    /// have been armed for [`WT_SHED_ESCALATION_STAGE1_RUNWAY_ROUNDS`] rounds.
    pub fn record_shed_at(&self, now: u64, writes_completed: u64) -> EscalationAction {
        let mut rounds = self
            .inner
            .rounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(&(last, _)) = rounds.opened.back() {
            if now.saturating_sub(last) < millis(WT_SHED_ESCALATION_ROUND) {
                return EscalationAction::Proceed;
            }
        }
        let previous_writes = rounds.opened.back().map(|&(_, w)| w);
        rounds.opened.push_back((now, writes_completed));
        let stage2_span = millis(WT_SHED_ESCALATION_STAGE2_WINDOW);
        while rounds
            .opened
            .front()
            .is_some_and(|&(at, _)| now.saturating_sub(at) > stage2_span)
        {
            rounds.opened.pop_front();
        }
        rounds.stalled_run = match previous_writes {
            Some(previous) if previous == writes_completed => rounds.stalled_run + 1,
            _ => 1,
        };

        // Decay BEFORE the stamp, or the stamp would always look held.
        if !self.stage1_active_at(now) {
            rounds.stage1_booked = false;
            rounds.rounds_since_stage1 = None;
        }
        let stage1_span = millis(WT_SHED_ESCALATION_STAGE1_WINDOW);
        let recent = rounds
            .opened
            .iter()
            .filter(|&&(at, _)| now.saturating_sub(at) <= stage1_span)
            .count();
        let mut armed_now = false;
        if recent >= WT_SHED_ESCALATION_STAGE1_ROUNDS {
            self.inner.stage1_epoch.store(now, Ordering::Relaxed);
            if !rounds.stage1_booked {
                rounds.stage1_booked = true;
                rounds.rounds_since_stage1 = Some(0);
                armed_now = true;
            }
        }
        if !armed_now {
            if let Some(since) = rounds.rounds_since_stage1.as_mut() {
                *since += 1;
            }
        }
        if armed_now {
            return EscalationAction::ArmedStage1;
        }

        if rounds
            .rounds_since_stage1
            .is_none_or(|since| since < WT_SHED_ESCALATION_STAGE1_RUNWAY_ROUNDS)
        {
            return EscalationAction::Proceed;
        }

        let run = sustained_run(&rounds.opened);
        let stalled = rounds.stalled_run >= WT_SHED_ESCALATION_DELIVERY_STALLED_ROUNDS;
        let stage2 = run >= WT_SHED_ESCALATION_STAGE2_ROUNDS
            || (stalled && run >= WT_SHED_ESCALATION_DELIVERY_STALLED_ROUNDS);
        if !stage2 {
            return EscalationAction::Proceed;
        }
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return EscalationAction::Proceed;
        }
        EscalationAction::Close(if stalled {
            SessionCloseReason::DeliveryStalled
        } else {
            SessionCloseReason::ShedRounds
        })
    }

    pub fn camera_video_is_shed(&self) -> bool {
        self.stage1_active_at(downlink_congested_epoch_now())
    }

    pub fn stage1_active_at(&self, now: u64) -> bool {
        downlink_epoch_is_active_at(
            self.inner.stage1_epoch.load(Ordering::Relaxed),
            WT_SHED_ESCALATION_HOLD,
            now,
        )
    }

    pub fn rounds_recorded(&self) -> usize {
        self.inner
            .rounds
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .opened
            .len()
    }

    pub fn session_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }
}

/// A SUSTAINED wedge: rounds back from the newest while each gap is at most
/// [`WT_SHED_ESCALATION_MAX_ROUND_GAP`].
fn sustained_run(opened: &VecDeque<Round>) -> usize {
    let max_gap = millis(WT_SHED_ESCALATION_MAX_ROUND_GAP);
    let mut run = 0;
    let mut newer: Option<u64> = None;
    for &(at, _) in opened.iter().rev() {
        if newer.is_some_and(|next| next.saturating_sub(at) > max_gap) {
            break;
        }
        run += 1;
        newer = Some(at);
    }
    run
}

fn millis(d: std::time::Duration) -> u64 {
    d.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::WT_MAX_DOWNLINK_STREAMS;

    const ROUND_MS: u64 = 1_000;

    fn delivering(round: u64) -> u64 {
        100 + round
    }

    fn run_rounds(
        e: &DownlinkShedEscalation,
        rounds: u64,
        writes: impl Fn(u64) -> u64,
    ) -> Vec<(u64, EscalationAction)> {
        (1..=rounds)
            .map(|round| (round, e.record_shed_at(round * ROUND_MS, writes(round))))
            .collect()
    }

    fn first_close(actions: &[(u64, EscalationAction)]) -> Option<(u64, SessionCloseReason)> {
        actions.iter().find_map(|&(round, action)| match action {
            EscalationAction::Close(reason) => Some((round, reason)),
            _ => None,
        })
    }

    #[test]
    fn a_fresh_receiver_is_unescalated_and_unclosed() {
        let e = DownlinkShedEscalation::new();
        assert!(
            !e.stage1_active_at(10_000),
            "a session that has never shed must not shed camera video"
        );
        assert!(!e.session_closed(), "a fresh session is not closed");
    }

    #[test]
    fn three_rounds_in_the_window_arm_stage_one_once_per_episode() {
        let e = DownlinkShedEscalation::new();
        assert_eq!(
            e.record_shed_at(1_000, delivering(1)),
            EscalationAction::Proceed
        );
        assert_eq!(
            e.record_shed_at(2_000, delivering(2)),
            EscalationAction::Proceed,
            "two rounds is below the stage-1 threshold"
        );
        assert!(!e.stage1_active_at(2_000), "and must not shed camera video");
        assert_eq!(
            e.record_shed_at(3_000, delivering(3)),
            EscalationAction::ArmedStage1,
            "the third round inside the window arms stage 1"
        );
        assert!(
            e.stage1_active_at(3_000),
            "stage 1 must shed this receiver's camera video"
        );
        assert_eq!(
            e.record_shed_at(4_000, delivering(4)),
            EscalationAction::Proceed,
            "a fourth round re-stamps the hold but must not book a second episode"
        );
    }

    /// BITES: drop the coalescing guard — 48 sheds become 48 rounds.
    #[test]
    fn a_whole_map_shed_is_one_round_not_one_per_lane() {
        let e = DownlinkShedEscalation::new();
        let lanes = WT_MAX_DOWNLINK_STREAMS as u64;
        for lane in 0..lanes {
            let action = e.record_shed_at(1_000 + lane * (ROUND_MS - 1) / lanes, delivering(1));
            assert_eq!(
                action,
                EscalationAction::Proceed,
                "lane {lane} of one whole-map shed must not escalate on its own"
            );
        }
        assert!(
            !e.session_closed(),
            "one congestion event must never close a session"
        );
        assert!(
            !e.stage1_active_at(2_000),
            "nor arm stage 1: that is one round, not {WT_MAX_DOWNLINK_STREAMS}"
        );
    }

    #[test]
    fn ten_rounds_in_the_window_close_the_session_exactly_once() {
        let e = DownlinkShedEscalation::new();
        let mut closes = Vec::new();
        for round in 1..=WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 {
            if let EscalationAction::Close(reason) =
                e.record_shed_at(round * ROUND_MS, delivering(round))
            {
                closes.push(reason);
            }
        }
        assert_eq!(
            closes,
            vec![SessionCloseReason::ShedRounds],
            "exactly one close, attributed to the round threshold"
        );
        assert!(e.session_closed());
        let closed_at = WT_SHED_ESCALATION_STAGE2_ROUNDS as u64;
        let after: Vec<EscalationAction> = (1..=closed_at)
            .map(|round| e.record_shed_at((closed_at + round) * ROUND_MS, 7))
            .collect();
        assert!(
            !after
                .iter()
                .any(|a| matches!(a, EscalationAction::Close(_))),
            "a second lane must not close an already-closed session: {after:?}",
        );
    }

    /// BITES: evaluate stage 2 first, or drop the runway gate.
    #[test]
    fn a_delivery_stall_closes_only_after_stage_one_has_had_its_runway() {
        let e = DownlinkShedEscalation::new();
        let actions = run_rounds(&e, WT_SHED_ESCALATION_STAGE2_ROUNDS as u64, |_| 7);
        let expected = WT_SHED_ESCALATION_STAGE1_ROUNDS as u64
            + WT_SHED_ESCALATION_STAGE1_RUNWAY_ROUNDS as u64;
        assert_eq!(
            first_close(&actions),
            Some((expected, SessionCloseReason::DeliveryStalled)),
            "a stalled receiver closes at round {expected}, on its own reason",
        );
        assert!(
            actions[..expected as usize - 1]
                .iter()
                .all(|(_, a)| !matches!(a, EscalationAction::Close(_))),
            "and not one round earlier: {actions:?}",
        );
    }

    #[test]
    fn a_delivering_receiver_holds_the_ordinary_threshold() {
        let e = DownlinkShedEscalation::new();
        let actions = run_rounds(&e, WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 - 1, delivering);
        assert_eq!(
            first_close(&actions),
            None,
            "a receiver still taking bytes must reach the full round bar: {actions:?}",
        );
        assert!(!e.session_closed());
    }

    #[test]
    fn five_transient_bursts_never_reach_the_stage_two_bar() {
        let e = DownlinkShedEscalation::new();
        let gap = millis(WT_SHED_ESCALATION_MAX_ROUND_GAP) + ROUND_MS;
        for burst in 0..5u64 {
            let at = ROUND_MS + burst * (gap + ROUND_MS);
            e.record_shed_at(at, 7);
            e.record_shed_at(at + ROUND_MS, 7);
        }
        assert!(
            !e.session_closed(),
            "ten rounds spread over five bursts is a lossy link, not a wedge",
        );
    }

    #[test]
    fn sparse_rounds_never_close_because_stage_one_never_arms() {
        let e = DownlinkShedEscalation::new();
        let spacing = millis(WT_SHED_ESCALATION_STAGE1_WINDOW) - ROUND_MS;
        for round in 1..=8u64 {
            e.record_shed_at(round * spacing, 7);
        }
        assert!(
            !e.stage1_active_at(8 * spacing),
            "one round per stage-1 window never arms the remedy",
        );
        assert!(
            !e.session_closed(),
            "and a session must never close with the remedy never tried",
        );
    }

    /// BITES: stamp `stage1_epoch` only when not already held.
    #[test]
    fn a_still_wedging_receiver_keeps_stage_one_from_its_latest_round() {
        let e = DownlinkShedEscalation::new();
        let rounds = WT_SHED_ESCALATION_STAGE2_ROUNDS as u64 - 1;
        for round in 1..=rounds {
            e.record_shed_at(round * ROUND_MS, delivering(round));
        }
        assert!(
            !e.session_closed(),
            "one round below the close threshold must not close",
        );
        assert!(
            e.stage1_active_at(rounds * ROUND_MS + millis(WT_SHED_ESCALATION_HOLD)),
            "the hold must run from the LAST round, not from the arm",
        );
    }

    #[test]
    fn stage_one_decays_after_the_hold_and_re_arms_as_a_new_episode() {
        let e = DownlinkShedEscalation::new();
        for round in 1..=3u64 {
            e.record_shed_at(round * ROUND_MS, delivering(round));
        }
        let armed_at = 3 * ROUND_MS;
        assert!(e.stage1_active_at(armed_at + millis(WT_SHED_ESCALATION_HOLD)));
        assert!(
            !e.stage1_active_at(armed_at + millis(WT_SHED_ESCALATION_HOLD) + 1),
            "stage 1 must decay on its own once the hold elapses with no round"
        );

        let later = armed_at + millis(WT_SHED_ESCALATION_STAGE2_WINDOW) + ROUND_MS;
        let mut actions = Vec::new();
        for round in 0..3u64 {
            actions.push(e.record_shed_at(later + round * ROUND_MS, delivering(20 + round)));
        }
        assert_eq!(
            actions.last(),
            Some(&EscalationAction::ArmedStage1),
            "a recovered receiver that wedges again books a NEW stage-1 episode"
        );
    }

    #[test]
    fn rounds_spread_wider_than_the_stage_one_window_never_arm_it() {
        let e = DownlinkShedEscalation::new();
        let spacing = millis(WT_SHED_ESCALATION_STAGE1_WINDOW) / 2 + ROUND_MS;
        for round in 1..=6u64 {
            let action = e.record_shed_at(round * spacing, delivering(round));
            assert_eq!(
                action,
                EscalationAction::Proceed,
                "an isolated shed every {spacing}ms is not sustained wedging"
            );
        }
        assert!(!e.stage1_active_at(6 * spacing));
    }
}
