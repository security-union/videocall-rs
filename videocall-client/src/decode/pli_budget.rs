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

//! Per-receiver cross-sender proactive-PLI (keyframe-request) budget — issue #1479, option (b).
//!
//! # What this is
//!
//! A CLIENT-SIDE defense-in-depth ceiling on how many PROACTIVE keyframe requests
//! (PLIs) a single receiver emits ACROSS ALL of its senders within a sliding
//! window. The proactive path is the worker's keyframe-less stale-backlog
//! eviction (`videocall_codecs::jitter_buffer`): when a sender's stream freezes
//! with no buffered keyframe, the worker asks the main thread to fire a
//! `KEYFRAME_REQUEST` for that stream. Under a meeting-wide freeze wave, many
//! senders can fan that out simultaneously. This budget caps the cross-sender
//! total so a receiver cannot storm its own uplink / the relay.
//!
//! # Why it is benign (and why it does NOT weaken #1494)
//!
//! The AUTHORITATIVE limiter is the RELAY's per-receiver
//! `KEYFRAME_REQUEST_MAX_PER_SEC = 32` cap (server-side), which already coalesces
//! this receiver's PLIs across senders. This budget mirrors that 32/s exactly
//! (`KEYFRAME_REQUEST_MAX_PER_WINDOW`, `KEYFRAME_REQUEST_WINDOW_MS`) rather than
//! tightening it — the SERVER stays the binding limit, the client is a co-equal
//! shadow. In normal multi-sender recovery it is a NO-OP.
//!
//! It is layered ABOVE the #1494 per-sender backoff that lives in
//! `jitter_buffer.rs` (the `consecutive_proactive_keyframe_requests` exponential
//! backoff + the `awaiting_proactive_keyframe` arrival gate). That backoff still
//! paces EACH sender to at most ~1 proactive PLI per window. This budget only
//! sheds genuinely-redundant SAME-WINDOW cross-sender 2nd+ pokes once the global
//! cap is reached; it never touches a sender's first-in-window request, so it
//! cannot weaken #1494 or deny a lone frozen stream its #1494-paced recovery.
//!
//! # Wedge-proof guarantees (proven by the tests below)
//!
//! 1. **A lone frozen sender ALWAYS passes.** With only one sender contending it
//!    is always "first-in-window" at the #1494 cadence (>=1 window apart), so its
//!    every paced PLI is unconditionally allowed. Even if it somehow poked more
//!    than once per window, a lone sender can never reach the 32 global cap at the
//!    #1494 cadence (one fire per window << 32/window), so its 2nd+ pokes also pass
//!    via the under-cap branch. So a lone frozen stream NEVER loses a #1494-paced PLI.
//! 2. **A sender's FIRST request in a window is ALWAYS allowed**, unconditionally,
//!    regardless of the global cap. This is the wedge-proof core and simultaneously
//!    realizes the #1662 escalation exemption (see below).
//! 3. **Under contention at the cap, the STALEST contender is preserved.** A 2nd+
//!    poke that is staler than the freshest entry currently holding a slot evicts
//!    that fresher entry and takes its place; only a poke that is no staler than
//!    everything in the window is shed. The neediest (oldest-frozen) stream is
//!    never starved in favor of a fresher one.
//!
//! There is NO input under which a single starved stream is permanently denied
//! recovery: either it is first-in-window (unconditional allow), or — on a 2nd
//! poke — its growing staleness wins the priority compare. The wall-clock window
//! also guarantees self-healing: once a sender has been quiet for one window its
//! per-sender entry ages out and its next request is first-in-window again.
//!
//! # The #1662 escalation exemption
//!
//! The #1662 keyframe-less hold-ceiling escalation does NOT send a distinct
//! flagged PLI. Its flow is: the worker gates the escalation, the jitter buffer
//! `reset()`s, the deferred `reset_jitter_buffer()` rebuilds the `JitterBuffer`
//! fresh (resetting the per-sender #1494 backoff), and on the NEXT keyframe-less
//! eviction `post_request_keyframe_to_main` fires a NORMAL
//! `RequestKeyframeMessage`. So the escalation's recovery PLI is structurally a
//! routine proactive PLI at this budget gate. The escalation happens inside the
//! worker while the manager keeps the peer CONNECTED, so [`PliBudget::remove_sender`]
//! is NOT called on this path — the exemption rests on the wall-clock window
//! having aged out the sender's prior `last_allowed_ms` entry. The recovery PLI
//! therefore almost always arrives as that sender's FIRST-in-window request and is
//! unconditionally allowed (property #2). In the rare case the sender already had
//! an allow in the current <=1s window AND the global cap is full of staler
//! incumbents, the recovery PLI is delayed by at most one window — the wall-clock
//! window then ages out the prior entry, restoring unconditional first-in-window
//! allow — never permanently denied. This bounded sub-second delay matches the
//! relay's own 32/s coalescing, so the safety bar holds. Test `D` proves the
//! exemption (modeling the per-sender re-base via `remove_sender` for determinism).
//!
//! # Purity / testability
//!
//! [`PliBudget::allow`] is a PURE function of its inputs and the supplied
//! `now_ms` — it reads no clock and performs no side effects (no logging, no
//! diagnostics). The route closure in `peer_decode_manager.rs` owns all
//! side-effects: it calls `allow`, fires `emit_keyframe_request` on `Allow`, and
//! emits the throttled diagnostic on `Shed`. This keeps the whole algorithm
//! host-testable on the NATIVE target (not behind any `wasm32` cfg).

use std::collections::{HashMap, VecDeque};

use crate::adaptive_quality_constants::{
    KEYFRAME_REQUEST_MAX_PER_WINDOW, KEYFRAME_REQUEST_WINDOW_MS,
};

/// Outcome of [`PliBudget::allow`].
///
/// `Allow` — fire the keyframe request. `Shed` — suppress it; `log` is `true`
/// at most once per [`KEYFRAME_REQUEST_WINDOW_MS`] per sender so the route
/// closure can emit a throttled `warn!` without the throttle decision leaking
/// any clock/side-effect into this pure type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PliBudgetDecision {
    /// Permit the proactive PLI.
    Allow,
    /// Suppress the proactive PLI. `log` indicates whether the caller should emit
    /// a throttled warning for this shed (true at most once per window per sender).
    Shed { log: bool },
}

/// One allowed proactive-PLI emission, retained in the global sliding window.
#[derive(Debug, Clone, Copy)]
struct AllowedEntry {
    /// Wall-clock time (ms) the request was allowed.
    timestamp_ms: u128,
    /// Sender session id (the relay/decode key).
    session_id: u64,
    /// Head-of-line backlog age (ms) carried with the request — the staleness
    /// priority key. Higher = staler = more deserving of a slot.
    head_age_ms: f64,
}

/// Per-receiver cross-sender proactive-PLI budget (issue #1479). See the module docs.
///
/// Keyed by `session_id` (`u64`) — the same key as `PeerDecodeManager::connected_peers`,
/// so the lifecycle hooks ([`remove_sender`](Self::remove_sender) / [`clear`](Self::clear))
/// line up with peer add/remove exactly.
#[derive(Debug, Default)]
pub struct PliBudget {
    /// Sliding window of ALLOWED emissions across ALL senders, oldest-first.
    /// Bounds the global cap and supplies the staleness-priority candidates.
    allowed: VecDeque<AllowedEntry>,
    /// `session_id -> last-allowed timestamp (ms)`. Drives the "first-in-window"
    /// determination (a sender absent here within the window is first-in-window).
    last_allowed_ms: HashMap<u64, u128>,
    /// `session_id -> last-logged-shed timestamp (ms)`. Throttles the shed warning
    /// to at most once per window per sender. Pure state; the actual log emission
    /// lives in the route closure keyed off [`PliBudgetDecision::Shed`]'s `log`.
    last_logged_ms: HashMap<u64, u128>,
}

impl PliBudget {
    /// Create an empty budget.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decide whether a proactive PLI for `sender_session_id` (carrying
    /// `head_age_ms`) may be emitted at `now_ms`.
    ///
    /// PURE: depends only on the arguments and the budget's accumulated state, and
    /// performs no I/O. The caller supplies `now_ms` (does not read a clock here)
    /// so host tests are deterministic.
    ///
    /// Algorithm (sliding window, staleness-prioritized; see module docs for the
    /// wedge proof):
    /// 1. Prune all maps/the deque of entries older than one window.
    /// 2. If the sender has no entry within the window, it is **first-in-window**:
    ///    ALLOW unconditionally and record (wedge-proof core + #1662 exemption).
    /// 3. Otherwise (a 2nd+ poke this window): if below the global cap, ALLOW and
    ///    record. If at the cap, apply **staleness priority** — if this request is
    ///    strictly staler than the freshest entry currently holding a slot, evict
    ///    that freshest entry and take its place; otherwise SHED.
    pub fn allow(
        &mut self,
        sender_session_id: u64,
        head_age_ms: f64,
        now_ms: u128,
    ) -> PliBudgetDecision {
        self.prune(now_ms);

        // first-in-window iff the sender has no surviving (post-prune) entry.
        let sender_first_in_window = !self.last_allowed_ms.contains_key(&sender_session_id);

        if sender_first_in_window {
            // Property #2: a sender's first-in-window request is ALWAYS allowed,
            // regardless of the global cap. This is the wedge-proof core and the
            // #1662 escalation exemption (the post-reset recovery PLI almost always
            // lands here once the wall-clock window has aged out the sender's prior
            // entry; a rare same-window collision delays it by at most one window).
            // It is NEVER subject to staleness shedding.
            self.record_allow(sender_session_id, head_age_ms, now_ms);
            return PliBudgetDecision::Allow;
        }

        // 2nd+ poke from a sender that already fired this window — the only
        // shed-eligible case.
        if self.allowed.len() < KEYFRAME_REQUEST_MAX_PER_WINDOW {
            // Below the global cap: there is spare room, so allow it. (A lone
            // sender can never reach the cap by itself at the #1494 cadence, so
            // this is the branch that keeps a lone frozen stream's bursts flowing
            // — property #1.)
            self.record_allow(sender_session_id, head_age_ms, now_ms);
            return PliBudgetDecision::Allow;
        }

        // At the global cap. Apply staleness priority: preserve the stalest
        // contenders, shed the freshest. Find the freshest entry currently in the
        // window (the smallest head_age_ms). If THIS request is strictly staler,
        // it preempts that fresher slot; otherwise it sheds.
        if let Some((freshest_idx, freshest_age)) = self.freshest_entry() {
            if head_age_ms > freshest_age {
                // Preempt: refund the freshest slot and take it.
                self.allowed.remove(freshest_idx);
                self.record_allow(sender_session_id, head_age_ms, now_ms);
                return PliBudgetDecision::Allow;
            }
        }

        // Shed. Throttle the log to at most once per window per sender.
        let should_log = match self.last_logged_ms.get(&sender_session_id) {
            Some(&last) => now_ms.saturating_sub(last) >= KEYFRAME_REQUEST_WINDOW_MS as u128,
            None => true,
        };
        if should_log {
            self.last_logged_ms.insert(sender_session_id, now_ms);
        }
        PliBudgetDecision::Shed { log: should_log }
    }

    /// Drop all per-sender state for `session_id` — the per-sender last-allowed
    /// entry, that sender's last-logged entry, AND every global-deque entry for
    /// that sender. Called on the manager's two single-peer-removal paths
    /// (`run_peer_monitor` heartbeat-timeout, `delete_peer_at`) so a rejoining
    /// session under the same id is never throttled by its prior life; bulk
    /// teardown (`clear_all_peers`) uses [`PliBudget::clear`] instead.
    /// (Note: the #1662 escalation does NOT take a removal path — the peer stays
    /// connected — so its recovery PLI relies on the wall-clock window aging out
    /// the prior entry, not on this call; see the module-level exemption docs.)
    pub fn remove_sender(&mut self, session_id: u64) {
        self.last_allowed_ms.remove(&session_id);
        self.last_logged_ms.remove(&session_id);
        self.allowed.retain(|e| e.session_id != session_id);
    }

    /// Empty all budget state. Called on `clear_all_peers` (connection drop): all
    /// senders leave, so the whole budget resets.
    pub fn clear(&mut self) {
        self.allowed.clear();
        self.last_allowed_ms.clear();
        self.last_logged_ms.clear();
    }

    /// Drop everything older than one window from the deque and both maps.
    /// Wall-clock based, so a reconnect / cold-start / tab-resume that jumps
    /// `now_ms` forward by more than a window empties the budget naturally.
    fn prune(&mut self, now_ms: u128) {
        let window = KEYFRAME_REQUEST_WINDOW_MS as u128;
        let cutoff = now_ms.saturating_sub(window);
        // Drop entries at/older than the cutoff. The deque is oldest-first, so pop
        // from the front while stale.
        while let Some(front) = self.allowed.front() {
            if front.timestamp_ms <= cutoff {
                self.allowed.pop_front();
            } else {
                break;
            }
        }
        self.last_allowed_ms.retain(|_, &mut ts| ts > cutoff);
        self.last_logged_ms.retain(|_, &mut ts| ts > cutoff);
    }

    /// Record an allowed emission: push to the global window and refresh the
    /// per-sender last-allowed timestamp.
    fn record_allow(&mut self, session_id: u64, head_age_ms: f64, now_ms: u128) {
        self.allowed.push_back(AllowedEntry {
            timestamp_ms: now_ms,
            session_id,
            head_age_ms,
        });
        self.last_allowed_ms.insert(session_id, now_ms);
    }

    /// Index + head-age of the FRESHEST entry currently in the window (the one
    /// with the smallest `head_age_ms`), or `None` if the window is empty. Ties
    /// resolve to the earliest such entry (first encountered).
    fn freshest_entry(&self) -> Option<(usize, f64)> {
        let mut best: Option<(usize, f64)> = None;
        for (idx, e) in self.allowed.iter().enumerate() {
            match best {
                Some((_, best_age)) if e.head_age_ms >= best_age => {}
                _ => best = Some((idx, e.head_age_ms)),
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    //! Native, mutation-sensitive tests for the production [`PliBudget`] (issue #1479).
    //!
    //! Each test drives the PRODUCTION `PliBudget::allow` / `remove_sender` /
    //! `clear` — never a re-implementation — and is paired with a named source
    //! mutation that MUST flip it red (recorded in the PR/agent report).
    use super::*;
    use crate::adaptive_quality_constants::KEYFRAME_REQUEST_MAX_PER_WINDOW;

    const NOW: u128 = 10_000;

    /// Fill the global window to exactly the cap using `cap` DISTINCT senders'
    /// first-in-window allows (all unconditionally allowed). Returns the budget at cap.
    fn budget_at_cap(now_ms: u128) -> PliBudget {
        let mut b = PliBudget::new();
        for s in 0..KEYFRAME_REQUEST_MAX_PER_WINDOW as u64 {
            // distinct senders, moderate staleness; all first-in-window => Allow.
            assert_eq!(
                b.allow(s, 500.0, now_ms),
                PliBudgetDecision::Allow,
                "distinct sender {s} first-in-window must be allowed"
            );
        }
        assert_eq!(b.allowed.len(), KEYFRAME_REQUEST_MAX_PER_WINDOW);
        b
    }

    // ---------------------------------------------------------------------
    // A) Sheds excess SAME-WINDOW cross-sender 2nd+ pokes past the cap.
    //    Mutation: flip `< KEYFRAME_REQUEST_MAX_PER_WINDOW` to `<=` (or remove the
    //    cap check) => the 2nd poke is allowed and this fails.
    // ---------------------------------------------------------------------
    #[test]
    fn sheds_excess_same_window_pokes_past_cap() {
        let mut b = budget_at_cap(NOW);
        // A sender already in the window (sender 0) pokes a SECOND time, at LOW
        // staleness (fresher than everything in the window, which is all 500.0).
        // The window is at cap and this poke is not staler than the freshest, so
        // it must SHED.
        let decision = b.allow(0, 100.0, NOW);
        assert!(
            matches!(decision, PliBudgetDecision::Shed { .. }),
            "a low-staleness 2nd poke at the global cap must be shed, got {decision:?}"
        );
        // The cap was not exceeded.
        assert_eq!(b.allowed.len(), KEYFRAME_REQUEST_MAX_PER_WINDOW);
    }

    // ---------------------------------------------------------------------
    // B) NEVER sheds a lone starved stream's #1494-paced baseline, AND a
    //    first-in-window sender is allowed even when the window is already at cap
    //    from OTHER senders (the load-bearing wedge-proof property).
    //    Mutation: remove the first-in-window unconditional allow (force
    //    `sender_first_in_window = false`) => the at-cap first-in-window assert
    //    below fails (the new sender would hit the shed-eligible path at cap).
    // ---------------------------------------------------------------------
    #[test]
    fn never_sheds_lone_starved_streams_paced_baseline() {
        let mut b = PliBudget::new();
        // One sender, #1494-paced at the 1000ms window cadence across many windows.
        // Each call is first-in-window after the prior entry ages out -> always Allow.
        let mut t = NOW;
        for i in 0..20u128 {
            assert_eq!(
                b.allow(7, 800.0, t),
                PliBudgetDecision::Allow,
                "lone sender's window-spaced PLI #{i} must always be allowed"
            );
            t += KEYFRAME_REQUEST_WINDOW_MS as u128;
        }

        // Also: a lone sender's 2nd poke 100ms later (no other contenders, so the
        // global deque holds only 1-2 entries, far below the cap) must also Allow.
        let mut b2 = PliBudget::new();
        assert_eq!(b2.allow(9, 800.0, NOW), PliBudgetDecision::Allow);
        assert_eq!(
            b2.allow(9, 850.0, NOW + 100),
            PliBudgetDecision::Allow,
            "a lone sender's quick 2nd poke far below the cap must be allowed"
        );

        // LOAD-BEARING: fill the window to cap with OTHER senders (ids 0..cap),
        // then a NEW, never-seen-this-window sender (`cap`) freezes. It is
        // first-in-window, so it MUST be allowed even though the global cap is
        // already reached — otherwise a freshly-frozen stream could be denied its
        // very first recovery PLI. This is the assertion the first-in-window
        // unconditional-allow uniquely protects. Use a LOW staleness so this can
        // ONLY pass via the first-in-window rule (it would lose any staleness
        // priority compare against the 500.0 incumbents).
        let mut b3 = budget_at_cap(NOW);
        let new_sender = KEYFRAME_REQUEST_MAX_PER_WINDOW as u64; // distinct from 0..cap
        assert_eq!(
            b3.allow(new_sender, 1.0, NOW),
            PliBudgetDecision::Allow,
            "a first-in-window sender must be allowed even at the global cap (wedge-proof)"
        );
    }

    // ---------------------------------------------------------------------
    // C) Under contention AT CAP, sheds the FRESHEST and passes the STALEST.
    //    Mutation: remove the staleness compare (always shed at cap on 2nd poke)
    //    => the stale 2nd poke would shed and this fails.
    // ---------------------------------------------------------------------
    #[test]
    fn at_cap_prioritizes_stalest_and_sheds_freshest() {
        let mut b = budget_at_cap(NOW); // every entry has head_age 500.0
                                        // A STALE 2nd-poke (sender 0 already in window) at high staleness must
                                        // PREEMPT the freshest 500.0 slot and be allowed.
        assert_eq!(
            b.allow(0, 5000.0, NOW),
            PliBudgetDecision::Allow,
            "a stalest-yet 2nd poke at the cap must preempt a fresher slot"
        );
        assert_eq!(b.allowed.len(), KEYFRAME_REQUEST_MAX_PER_WINDOW);

        // A FRESH 2nd-poke (sender 1 already in window) below the stalest contender
        // must SHED. After the preempt above the window contains one 5000.0 entry
        // and the rest 500.0; a fresh 50.0 poke is no staler than the 500.0 floor.
        let decision = b.allow(1, 50.0, NOW);
        assert!(
            matches!(decision, PliBudgetDecision::Shed { .. }),
            "a fresh 2nd poke at the cap must be shed, got {decision:?}"
        );
    }

    // ---------------------------------------------------------------------
    // D) #1662 escalation PLI is NEVER (permanently) shed.
    //    In production the escalation keeps the peer connected, so the recovery PLI
    //    almost always arrives as that sender's first-in-window request once the
    //    wall-clock window has aged out its prior entry (a rare same-window collision
    //    delays it by at most one window, never permanently). This test models that
    //    aged-out / re-based state DETERMINISTICALLY via remove_sender (the same state
    //    the wall-clock prune reaches, and the call the manager makes on every
    //    peer-removal path), so the first-in-window exemption is exercised directly.
    //    Mutation: make remove_sender NOT drop the per-sender entry => the post-
    //    reset request would be treated as a 2nd poke and shed; this fails.
    // ---------------------------------------------------------------------
    #[test]
    fn escalation_recovery_pli_is_never_shed() {
        let mut b = budget_at_cap(NOW); // senders 0..cap, every incumbent head_age 500.0
                                        // Sender X was already in the window. The #1662 reset re-bases its buffer;
                                        // model the per-sender re-base via remove_sender (matches the manager's
                                        // teardown-path cleanup).
        let x = 0u64;
        b.remove_sender(x);
        // CRITICAL: remove_sender(X) also frees X's deque slot (32 -> 31). REFILL that
        // slot with a fresh DISTINCT sender's first-in-window allow so the global window
        // is back at EXACTLY the cap. Otherwise X's re-add would be rescued by the
        // under-cap branch and never exercise the first-in-window path — leaving the
        // `last_allowed_ms.remove` line in remove_sender unguarded (the reviewer's finding).
        let filler = KEYFRAME_REQUEST_MAX_PER_WINDOW as u64 + 1; // distinct from 0..cap and X
        assert_eq!(b.allow(filler, 500.0, NOW), PliBudgetDecision::Allow);
        assert_eq!(b.allowed.len(), KEYFRAME_REQUEST_MAX_PER_WINDOW);
        // X's post-reset recovery PLI is now first-in-window => unconditional Allow.
        // Use a LOW staleness (1.0, fresher than every 500.0 incumbent) so this can
        // ONLY pass via the first-in-window rule — at the cap, a staleness preempt would
        // lose against the incumbents. If `remove_sender` failed to drop X's per-sender
        // entry, X would be a 2nd poke at cap and this fresh request would SHED.
        assert_eq!(
            b.allow(x, 1.0, NOW),
            PliBudgetDecision::Allow,
            "the #1662 post-reset recovery PLI must always be allowed (first-in-window)"
        );
    }

    // ---------------------------------------------------------------------
    // E) Lifecycle: remove_sender makes a re-add first-in-window even at the cap;
    //    clear empties everything.
    //    Mutation: make remove_sender / clear no-ops => the re-add would be a 2nd
    //    poke shed (remove_sender) / the post-clear call would be at cap (clear)
    //    and these asserts fail.
    // ---------------------------------------------------------------------
    #[test]
    fn lifecycle_remove_sender_and_clear_reset_state() {
        // remove_sender: a sender removed and re-adding is first-in-window again.
        let mut b = budget_at_cap(NOW); // senders 0..cap, every incumbent head_age 500.0
        let s = 3u64;
        // Establish s as already-in-window with a 2nd poke being shed first (fresh 10.0
        // at cap -> Shed; this does NOT touch last_allowed_ms — s was allowed in
        // budget_at_cap).
        assert!(matches!(
            b.allow(s, 10.0, NOW),
            PliBudgetDecision::Shed { .. }
        ));
        b.remove_sender(s);
        // CRITICAL: remove_sender(s) frees s's deque slot (32 -> 31). REFILL it with a
        // fresh DISTINCT sender so the window is back at EXACTLY the cap, forcing s's
        // re-add to pass ONLY via the first-in-window path (which depends on
        // last_allowed_ms NOT containing s). Without the refill the under-cap branch
        // would rescue the re-add and leave the `last_allowed_ms.remove` line unguarded.
        let filler = KEYFRAME_REQUEST_MAX_PER_WINDOW as u64 + 1; // distinct from 0..cap and s
        assert_eq!(b.allow(filler, 500.0, NOW), PliBudgetDecision::Allow);
        assert_eq!(b.allowed.len(), KEYFRAME_REQUEST_MAX_PER_WINDOW);
        // s's re-add at LOW staleness (10.0, fresher than the 500.0 incumbents) can only
        // pass via first-in-window — a staleness preempt would lose. If remove_sender
        // failed to drop s from last_allowed_ms, this would Shed.
        assert_eq!(
            b.allow(s, 10.0, NOW),
            PliBudgetDecision::Allow,
            "after remove_sender, the sender's next request is first-in-window (even at cap)"
        );

        // clear: post-clear, every sender is first-in-window even though the window
        // had been at cap.
        let mut b2 = budget_at_cap(NOW);
        b2.clear();
        assert_eq!(b2.allowed.len(), 0, "clear empties the global window");
        assert_eq!(
            b2.allow(0, 1.0, NOW),
            PliBudgetDecision::Allow,
            "after clear, any sender's next request is first-in-window"
        );
    }

    // ---------------------------------------------------------------------
    // Window self-heal: an entry older than the window is pruned, so a sender that
    // has been quiet for >1 window is first-in-window again (reconnect/tab-resume).
    // ---------------------------------------------------------------------
    #[test]
    fn stale_entries_age_out_of_the_window() {
        let mut b = PliBudget::new();
        assert_eq!(b.allow(1, 500.0, NOW), PliBudgetDecision::Allow);
        // Same sender, more than a window later: the prior entry ages out, so this
        // is first-in-window again.
        let later = NOW + KEYFRAME_REQUEST_WINDOW_MS as u128 + 1;
        assert_eq!(
            b.allow(1, 500.0, later),
            PliBudgetDecision::Allow,
            "after a quiet window the sender is first-in-window again"
        );
        assert_eq!(
            b.allowed.len(),
            1,
            "the aged-out entry must be pruned, leaving only the fresh one"
        );
    }

    // ---------------------------------------------------------------------
    // Shed-log throttle: at most one log per window per sender, but a later window
    // re-arms it.
    // ---------------------------------------------------------------------
    #[test]
    fn shed_log_is_throttled_per_sender_per_window() {
        let mut b = budget_at_cap(NOW);
        // Two fresh 2nd pokes from the same in-window sender within one window: the
        // first shed logs, the second does not.
        let first = b.allow(0, 1.0, NOW);
        let second = b.allow(0, 1.0, NOW + 10);
        assert_eq!(first, PliBudgetDecision::Shed { log: true });
        assert_eq!(second, PliBudgetDecision::Shed { log: false });

        // A poke more than a window later re-arms the log (and is first-in-window,
        // so it does not actually shed — drive a fresh at-cap state instead).
        let mut b2 = budget_at_cap(NOW);
        assert_eq!(b2.allow(0, 1.0, NOW), PliBudgetDecision::Shed { log: true });
        // Rebuild an at-cap window one full window later and shed again -> logs anew.
        let t2 = NOW + KEYFRAME_REQUEST_WINDOW_MS as u128 + 1;
        let mut b3 = budget_at_cap(t2);
        assert_eq!(
            b3.allow(0, 1.0, t2),
            PliBudgetDecision::Shed { log: true },
            "a later-window shed re-arms the per-sender log"
        );
    }
}
