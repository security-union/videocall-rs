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

//! Send-side state machine for the viewport control packet (HCL issue #988,
//! Phase 1c).
//!
//! The client decodes only the subset of peers the meeting layout actually
//! renders (the "active decode set", a set of relay session_ids). This module
//! relays that subset to the relay as a `VIEWPORT` control packet so the relay
//! can drop the VIDEO it would otherwise forward for peers the receiver is not
//! looking at, while ALWAYS forwarding AUDIO.
//!
//! ## Why a dedicated state machine
//!
//! Two real-time-conferencing hazards (see CLAUDE.md Change Impact Policy) make
//! a naive "send on every layout pass" approach dangerous:
//!
//! 1. **Storm avoidance.** Scrolling a large grid, or a reconnection wave that
//!    rebuilds every tile, can fire the layout callback dozens of times per
//!    second. Sending a control packet each time is exactly the O(n) fan-out
//!    the policy warns against. We therefore (a) only act on an *actual change*
//!    of the set and (b) DEBOUNCE so a burst of changes coalesces into one send
//!    once the layout settles.
//!
//! 2. **Fail-open recovery on reconnect.** On reconnect / re-election the
//!    client's session_id changes and the relay allocates a fresh, empty
//!    viewport (fail-open → the receiver gets every stream again). The client
//!    must re-send its current viewport after the connection comes back up or
//!    filtering silently never resumes. [`ViewportSender::reset_for_reconnect`]
//!    forces the next flush to send unconditionally.
//!
//! The pure change-detection / canonicalization logic lives here (and is unit
//! tested); the wasm-only timer + transport plumbing lives in
//! `video_call_client.rs`, which drives this state machine.

use std::collections::HashSet;

/// Debounce window for coalescing rapid active-decode-set changes before
/// emitting a single `VIEWPORT` packet. Long enough to absorb a scroll/relayout
/// burst, short enough that the relay starts filtering promptly once the layout
/// settles. See module docs for the storm-avoidance rationale.
pub(crate) const VIEWPORT_DEBOUNCE_MS: u32 = 300;

/// Tracks the most recently *requested* viewport and the most recently *sent*
/// viewport so we only emit a control packet when the set genuinely changes.
///
/// All comparisons are order-independent: sets are canonicalized to a sorted
/// `Vec<u64>` so `{1,2}` and `{2,1}` are treated as equal and never trigger a
/// redundant send.
#[derive(Debug, Default)]
pub(crate) struct ViewportSender {
    /// Canonical (sorted, deduped) form of the set last written to the wire.
    /// `None` means "nothing sent yet on the current connection" — the next
    /// flush will send unconditionally. Reset to `None` on reconnect so the
    /// relay's fresh empty viewport gets repopulated (fail-open recovery).
    last_sent: Option<Vec<u64>>,
    /// Canonical form of the most recent requested set, awaiting a debounced
    /// flush. `None` means no request is pending.
    pending: Option<Vec<u64>>,
}

impl ViewportSender {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Canonicalize a session-id set into a sorted, deduped vector so that
    /// equality checks and on-wire payloads are deterministic regardless of
    /// `HashSet` iteration order.
    fn canonicalize(set: &HashSet<u64>) -> Vec<u64> {
        let mut v: Vec<u64> = set.iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// Record a newly-requested active decode set.
    ///
    /// Returns `true` if this represents a change versus what is already
    /// pending *and* versus what was last sent — i.e. a debounced flush should
    /// be (re)scheduled. Returns `false` when the request is a no-op (the set
    /// matches the last value we already sent or already have pending), so the
    /// caller can skip arming a timer entirely. This is the storm-avoidance
    /// fast path: repeated identical layout passes cost nothing.
    pub(crate) fn record(&mut self, set: &HashSet<u64>) -> bool {
        let canonical = Self::canonicalize(set);

        // No-op if it matches what's already queued to send.
        if self.pending.as_ref() == Some(&canonical) {
            return false;
        }
        // No-op if it matches what we last sent and nothing else is pending.
        if self.pending.is_none() && self.last_sent.as_ref() == Some(&canonical) {
            return false;
        }

        self.pending = Some(canonical);
        true
    }

    /// Consume the pending request if (and only if) it differs from the last
    /// value sent on the wire. Returns the canonical session-id vector to put
    /// on the wire, or `None` if there is nothing new to send.
    ///
    /// On success the pending value is promoted to `last_sent` and cleared.
    /// Called when the debounce timer fires and on reconnect re-send.
    pub(crate) fn take_if_changed(&mut self) -> Option<Vec<u64>> {
        let pending = self.pending.take()?;
        if self.last_sent.as_ref() == Some(&pending) {
            // Settled back to the already-sent value during the debounce
            // window; nothing to do.
            return None;
        }
        self.last_sent = Some(pending.clone());
        Some(pending)
    }

    /// Forget what was last sent so the next flush sends unconditionally, and
    /// re-arm the current set as pending. Call on (re)connect: the relay just
    /// allocated a fresh empty viewport for the new session_id, so we must
    /// repopulate it even though the local set did not change.
    ///
    /// Returns `true` if there is a set to re-send (i.e. we had previously
    /// established a viewport this page-load), so the caller can flush
    /// immediately; `false` when nothing has ever been set.
    pub(crate) fn reset_for_reconnect(&mut self) -> bool {
        // Whatever we believe the relay knows is now stale.
        let previous = self.last_sent.take();
        // Re-queue the best-known current set: prefer an already-pending value,
        // otherwise fall back to the last value we had sent.
        if self.pending.is_none() {
            self.pending = previous;
        }
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[u64]) -> HashSet<u64> {
        ids.iter().copied().collect()
    }

    #[test]
    fn first_record_is_a_change() {
        let mut s = ViewportSender::new();
        assert!(s.record(&set(&[1, 2, 3])));
    }

    #[test]
    fn record_is_order_independent() {
        let mut s = ViewportSender::new();
        assert!(s.record(&set(&[3, 1, 2])));
        assert_eq!(s.take_if_changed(), Some(vec![1, 2, 3]));
        // Same members, different insertion order -> not a change.
        assert!(!s.record(&set(&[2, 3, 1])));
        assert_eq!(s.take_if_changed(), None);
    }

    #[test]
    fn duplicate_pending_record_is_noop() {
        let mut s = ViewportSender::new();
        assert!(s.record(&set(&[1, 2])));
        // Recording the same set again before flushing should not re-arm.
        assert!(!s.record(&set(&[1, 2])));
    }

    #[test]
    fn take_sends_then_dedups_same_set() {
        let mut s = ViewportSender::new();
        s.record(&set(&[10, 20]));
        assert_eq!(s.take_if_changed(), Some(vec![10, 20]));
        // Nothing pending now.
        assert_eq!(s.take_if_changed(), None);
        // Re-request the identical set: no change, nothing to send.
        assert!(!s.record(&set(&[10, 20])));
        assert_eq!(s.take_if_changed(), None);
    }

    #[test]
    fn coalesces_rapid_changes_into_latest() {
        let mut s = ViewportSender::new();
        // A burst of changes during a scroll; only the final settled value
        // should be sent when the debounce fires.
        assert!(s.record(&set(&[1])));
        assert!(s.record(&set(&[1, 2])));
        assert!(s.record(&set(&[1, 2, 3])));
        assert_eq!(s.take_if_changed(), Some(vec![1, 2, 3]));
    }

    #[test]
    fn settle_back_to_sent_value_is_noop() {
        let mut s = ViewportSender::new();
        s.record(&set(&[1, 2]));
        assert_eq!(s.take_if_changed(), Some(vec![1, 2]));
        // Change then change back before the timer fires.
        s.record(&set(&[1, 2, 3]));
        s.record(&set(&[1, 2]));
        // Net effect is the already-sent value -> nothing on the wire.
        assert_eq!(s.take_if_changed(), None);
    }

    #[test]
    fn empty_set_is_a_valid_distinct_value() {
        let mut s = ViewportSender::new();
        s.record(&set(&[1, 2]));
        assert_eq!(s.take_if_changed(), Some(vec![1, 2]));
        // Clearing the viewport (e.g. everything scrolled off) is a real
        // change that must be sent.
        assert!(s.record(&set(&[])));
        assert_eq!(s.take_if_changed(), Some(vec![]));
    }

    #[test]
    fn reconnect_forces_resend_of_current_set() {
        let mut s = ViewportSender::new();
        s.record(&set(&[7, 8]));
        assert_eq!(s.take_if_changed(), Some(vec![7, 8]));
        // Reconnect: relay forgot our viewport. Same local set, but we MUST
        // re-send it so filtering resumes.
        assert!(s.reset_for_reconnect());
        assert_eq!(s.take_if_changed(), Some(vec![7, 8]));
    }

    #[test]
    fn reconnect_with_nothing_set_is_noop() {
        let mut s = ViewportSender::new();
        assert!(!s.reset_for_reconnect());
        assert_eq!(s.take_if_changed(), None);
    }

    #[test]
    fn reconnect_prefers_pending_over_last_sent() {
        let mut s = ViewportSender::new();
        s.record(&set(&[1]));
        assert_eq!(s.take_if_changed(), Some(vec![1]));
        // A new set was requested but not yet flushed when reconnect happens.
        s.record(&set(&[1, 2]));
        assert!(s.reset_for_reconnect());
        // The newest pending value wins, and it sends unconditionally.
        assert_eq!(s.take_if_changed(), Some(vec![1, 2]));
    }
}
