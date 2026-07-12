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

//! Realtime-first presentation coalescing (issue #1783).
//!
//! Decoded frames cross from the decoder worker to the main thread one `postMessage` at a time and,
//! before this, were painted synchronously on receipt — so a burst of late frames (WS/TCP after
//! congestion) was replayed back-to-back as a fast-forward "catch-up flash," leaving video behind
//! real time while neteq holds audio at live. For a realtime conference, being *current* beats
//! playing *every* frame: a burst should resolve to the newest frame, not a replay.
//!
//! [`LatestFrameMailbox`] is the coalescing policy, kept as a pure, transport- and
//! browser-agnostic type so it is unit-testable off the wasm-only render path. It holds at most one
//! pending frame. The render owner (`videocall-client`'s `VideoPeerDecoder`) drains every decoded
//! frame from the worker→main queue and `offer`s it here; if a newer frame arrives before the
//! held one is presented, the older one is *displaced* (returned to the caller to release — closing
//! the `VideoFrame` is mandatory to free GPU memory) and the newest is held. A single
//! `requestAnimationFrame`-scheduled paint then `take`s whatever is newest and draws it, so an
//! N-frame burst becomes one draw (an instant jump to live) instead of N.
//!
//! Steady state (≤1 offer per paint interval) is behaviorally unchanged: each offered frame is the
//! only one pending when the paint fires, so it is presented untouched — no frame is ever displaced.
//!
//! Both a peer's camera decoder and its screen-share decoder are `VideoPeerDecoder` instances, so
//! they share this coalescing; screen share jumping to the newest frame on a burst is intended —
//! screen content is state-valued (the latest frame is the truth), not a motion stream where every
//! intermediate frame carries information.
//!
//! ## Accounting
//!
//! This type does NOT touch the `frames_painted` ACK the worker uses for `paint_lag_ms`
//! (`jitter_buffer::paint_lag_ms`). That counter is incremented one step *upstream*, as each frame
//! is drained from the worker→main `postMessage` queue (`decoder/wasm.rs`), which happens BEFORE
//! the frame is offered here. So a frame coalesced away (displaced) by this mailbox has already
//! been counted as drained/consumed — `emitted - painted` stays the true in-flight backlog and the
//! backpressure metric cannot misread because of coalescing. The counters below
//! (`offered`/`dropped`/`taken`) are the mailbox's OWN bookkeeping, used to make the coalescing
//! policy assertable in tests; they are independent of the wire ACK.
//!
//! [`paint_lag_ms`]: crate::jitter_buffer::paint_lag_ms

/// A latest-wins presentation mailbox holding at most one pending frame (issue #1783).
///
/// Generic over the frame handle `T` so the policy is exercised natively in tests with a plain
/// sentinel (e.g. a `u32` sequence id) while production instantiates it with the browser
/// `web_sys::VideoFrame`. The type never releases a frame itself (it cannot know how — `close()`
/// lives on `VideoFrame`); the caller releases every frame this returns from [`offer`](Self::offer)
/// (displaced) and every frame it takes from [`take`](Self::take) but chooses not to present.
#[derive(Debug)]
pub struct LatestFrameMailbox<T> {
    /// The single newest frame awaiting presentation, if any.
    pending: Option<T>,
    /// Total frames offered over this mailbox's lifetime.
    offered: u64,
    /// Frames displaced by a newer offer (coalesced away, never presented). Each was returned to
    /// the caller by [`offer`](Self::offer) to be released.
    dropped: u64,
    /// Frames removed for presentation via [`take`](Self::take).
    taken: u64,
}

impl<T> Default for LatestFrameMailbox<T> {
    fn default() -> Self {
        Self {
            pending: None,
            offered: 0,
            dropped: 0,
            taken: 0,
        }
    }
}

impl<T> LatestFrameMailbox<T> {
    /// Create an empty mailbox.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer a newly-decoded frame for presentation, holding it as the newest.
    ///
    /// If a frame was already pending (it has not been presented yet), it is *displaced* and
    /// returned so the caller can release it — for a `web_sys::VideoFrame` the caller MUST call
    /// `close()` on the returned frame to free GPU memory. Returns `None` in the steady-state case
    /// where nothing was pending, so a single offer passes straight through untouched.
    #[must_use = "the displaced frame must be released (e.g. VideoFrame::close) to free GPU memory"]
    pub fn offer(&mut self, frame: T) -> Option<T> {
        self.offered += 1;
        let displaced = self.pending.replace(frame);
        if displaced.is_some() {
            self.dropped += 1;
        }
        displaced
    }

    /// Remove and return the newest pending frame for presentation, or `None` if the mailbox is
    /// empty. The caller presents (or, if presentation is suppressed, releases) the returned frame.
    pub fn take(&mut self) -> Option<T> {
        let frame = self.pending.take();
        if frame.is_some() {
            self.taken += 1;
        }
        frame
    }

    /// Whether a frame is currently pending presentation.
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Total frames offered over this mailbox's lifetime.
    pub fn offered_count(&self) -> u64 {
        self.offered
    }

    /// Frames coalesced away (displaced by a newer offer) — never presented.
    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }

    /// Frames removed for presentation via [`take`](Self::take).
    pub fn taken_count(&self) -> u64 {
        self.taken
    }

    /// Frames that have fully left the mailbox — either coalesced away or taken for presentation.
    /// After N offers and one take with no interleaving, this is N (N-1 dropped + 1 taken): every
    /// offered frame is accounted for. Equals `offered_count()` minus a still-pending frame.
    pub fn consumed_count(&self) -> u64 {
        self.dropped + self.taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Burst coalescing: N offers with no interleaved take must present ONLY the newest, and every
    /// offered frame must be accounted as consumed (N-1 displaced + 1 taken == N).
    ///
    /// Mutation sensitivity: if `offer` stopped coalescing (e.g. kept the FIRST frame, or queued
    /// all frames FIFO and `take` returned the oldest), the `take() == Some(newest)` assertion
    /// fails; if it stopped counting displaced frames, `consumed_count() == n` fails. This is the
    /// test the issue requires to fail on the un-fixed (paint-every-frame) code.
    #[test]
    fn burst_of_n_presents_only_newest_and_accounts_all() {
        let n: u32 = 8;
        let mut mailbox: LatestFrameMailbox<u32> = LatestFrameMailbox::new();

        // Offer frames 1..=n. Only the first offer finds an empty slot; each later offer displaces
        // the previous still-pending frame, which the caller would close.
        for seq in 1..=n {
            let displaced = mailbox.offer(seq);
            if seq == 1 {
                assert_eq!(
                    displaced, None,
                    "first offer into an empty mailbox displaces nothing"
                );
            } else {
                assert_eq!(
                    displaced,
                    Some(seq - 1),
                    "each later offer displaces exactly the previous (older) frame"
                );
            }
        }

        assert!(mailbox.has_pending());
        // Newest wins: the single presented frame is the last offered.
        assert_eq!(
            mailbox.take(),
            Some(n),
            "only the newest frame is presented"
        );
        assert!(!mailbox.has_pending(), "take empties the mailbox");

        // All N offered frames are accounted for: N-1 coalesced away + 1 presented.
        assert_eq!(mailbox.dropped_count(), u64::from(n - 1));
        assert_eq!(mailbox.taken_count(), 1);
        assert_eq!(mailbox.consumed_count(), u64::from(n));
        assert_eq!(mailbox.offered_count(), u64::from(n));
    }

    /// Steady state, single offer per take: the frame passes through untouched (never displaced),
    /// exactly reproducing the pre-#1783 paint-every-frame behavior for the ≤1-pending case.
    #[test]
    fn single_offer_passes_through_unchanged() {
        let mut mailbox: LatestFrameMailbox<u32> = LatestFrameMailbox::new();

        let displaced = mailbox.offer(42);
        assert_eq!(displaced, None, "a lone offer displaces nothing");
        assert_eq!(
            mailbox.take(),
            Some(42),
            "the lone frame is presented as-is"
        );

        assert_eq!(mailbox.dropped_count(), 0, "steady state drops no frames");
        assert_eq!(mailbox.taken_count(), 1);
        assert_eq!(mailbox.consumed_count(), 1);
    }

    /// Interleaved offer/take (steady-state cadence): every frame is delivered, in order, with no
    /// coalescing — the paint keeps up so nothing is ever displaced.
    #[test]
    fn interleaved_offer_take_delivers_every_frame_in_order() {
        let mut mailbox: LatestFrameMailbox<u32> = LatestFrameMailbox::new();

        for seq in 1..=5u32 {
            let displaced = mailbox.offer(seq);
            assert_eq!(
                displaced, None,
                "paint kept up: nothing to displace at seq {seq}"
            );
            assert_eq!(mailbox.take(), Some(seq), "each frame delivered in order");
            assert!(!mailbox.has_pending());
        }

        assert_eq!(
            mailbox.dropped_count(),
            0,
            "no coalescing when paint keeps up"
        );
        assert_eq!(mailbox.taken_count(), 5);
        assert_eq!(mailbox.consumed_count(), 5);
    }

    /// A partial burst (2 offers) then a take, repeated, still presents only the newest of each
    /// clump and accounts every frame — guards the common "two frames land in one frame interval"
    /// jitter case, not just large bursts.
    #[test]
    fn pairwise_bursts_present_newest_of_each_pair() {
        let mut mailbox: LatestFrameMailbox<u32> = LatestFrameMailbox::new();

        // Pair (1,2): 2 is newest.
        assert_eq!(mailbox.offer(1), None);
        assert_eq!(mailbox.offer(2), Some(1), "2 displaces 1");
        assert_eq!(mailbox.take(), Some(2));

        // Pair (3,4): 4 is newest.
        assert_eq!(mailbox.offer(3), None);
        assert_eq!(mailbox.offer(4), Some(3), "4 displaces 3");
        assert_eq!(mailbox.take(), Some(4));

        assert_eq!(mailbox.dropped_count(), 2, "one displaced per pair");
        assert_eq!(mailbox.taken_count(), 2);
        assert_eq!(mailbox.consumed_count(), 4);
    }

    /// `take` on an empty mailbox is a no-op that returns `None` and counts nothing — the rAF paint
    /// firing with no held frame (e.g. after the frame was already taken) must not miscount.
    #[test]
    fn take_on_empty_is_noop() {
        let mut mailbox: LatestFrameMailbox<u32> = LatestFrameMailbox::new();
        assert_eq!(mailbox.take(), None);
        assert_eq!(mailbox.taken_count(), 0);
        assert_eq!(mailbox.consumed_count(), 0);
        assert!(!mailbox.has_pending());
    }
}
