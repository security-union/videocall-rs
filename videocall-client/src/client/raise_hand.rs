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

//! Pure, host-testable send-side policy for the raise-hand feature (issue 2135).
//!
//! Everything here is free of `web-sys`/DOM/clock dependencies — the caller
//! supplies `now_ms` — so the whole policy runs under plain native `#[test]`,
//! which is the only coverage that actually executes in this repo's CI for the
//! UI crate. The Dioxus side owns one [`RaiseHandAnnouncer`] and is a thin
//! driver: it applies the local level, asks for a decision, and either sends
//! immediately or arms a `gloo` timeout that calls [`RaiseHandAnnouncer::flush`].
//!
//! ## Why this is more than a throttle
//!
//! `RaiseHandPacket.raised` is an absolute LEVEL, not a toggle (see
//! `protobuf/types/raise_hand_packet.proto`). Three consequences shape this
//! module, and each is enforced by a test below:
//!
//! 1. **Re-sending is idempotent**, which is what makes the client-side
//!    re-announce protocol safe at all — the relay keeps no hand registry, so a
//!    hand raised before you joined only reaches you because the raiser re-sends
//!    its current level when it learns of you.
//! 2. **A lowered hand is every participant's default**, so a re-announce of
//!    `raised == false` is pure O(peers) waste and is never emitted. The one
//!    exception is a reconnect self-heal (see
//!    [`RaiseHandAnnouncer::invalidate_announced`]).
//! 3. **A dropped send loses a state transition the relay cannot repair.** So
//!    this scheduler COALESCES rather than DROPS: an over-rate request is
//!    deferred to the earliest in-budget instant and the *latest* level wins,
//!    instead of the reaction module's "reject the click" policy. A reaction is
//!    ephemeral — losing one loses a float. Losing a hand LOWER leaves a stale
//!    hand up in every peer's banner until the next re-announce.

/// Minimum spacing between two RAISE_HAND packets this client will put on the
/// wire, in milliseconds.
///
/// Sized STRICTLY under the relay's per-sender budget
/// ([`videocall_types::limits::RAISE_HAND_MAX_PER_WINDOW`] = 6 per
/// [`videocall_types::limits::RAISE_HAND_WINDOW_MS`] = 2000, which the relay's
/// limiter is built from). At 500 ms spacing at most 5 sends can land in any 2000 ms
/// tumbling window (`floor(2000 / 500) + 1`), leaving one slot of headroom, so
/// a well-behaved client can never trip the limiter — which matters more here
/// than for reactions because a dropped RAISE_HAND loses persistent state.
pub const RAISE_HAND_MIN_SEND_INTERVAL_MS: f64 = 500.0;

/// Debounce window applied to every RE-ANNOUNCE (issue 2135).
///
/// A 20-person join wave delivers 20 `PARTICIPANT_JOINED` callbacks in a few
/// hundred milliseconds. Sending one packet per callback would cost 20 sends per
/// raised hand and blow straight through the relay's 6/2000 ms budget. Deferring
/// EVERY re-announce by this window collapses the whole wave into exactly ONE
/// send (the second and later requests find a flush already armed and return
/// [`RaiseHandSend::Coalesced`]).
///
/// Deliberately larger than [`RAISE_HAND_MIN_SEND_INTERVAL_MS`] so the window,
/// not the rate gate, is what does the collapsing. The cost is that a late
/// joiner sees an already-raised hand up to ~750 ms after joining — below the
/// threshold at which a status badge feels late, and far cheaper than the
/// alternative of a rate-limiter drop that would leave the hand invisible to
/// that joiner until the NEXT join.
pub const RAISE_HAND_REANNOUNCE_COALESCE_MS: f64 = 750.0;

/// Hard ceiling on any deferral this scheduler will ask the caller to arm.
///
/// Defensive, not a policy knob: `now_ms` is a WALL clock (matching the
/// reactions self-throttle and `raised_at_ms` itself), so an NTP correction can
/// step it BACKWARD mid-call. Without a clamp, `deadline - now` after a jump of
/// minutes would arm a timer minutes out and freeze the local user's hand state
/// off the wire. Clamping bounds the worst case to one coalesce window; the
/// level is idempotent, so an early flush is always safe.
pub const RAISE_HAND_MAX_DEFER_MS: f64 = RAISE_HAND_REANNOUNCE_COALESCE_MS;

/// The `display_name` bytes a RAISE_HAND packet should carry for `raised`.
///
/// Empty on a LOWER, and that is a wire-size decision with a receiver-behaviour
/// justification, not a style one: the consumer's lower path
/// (`clear_raised_hand` in the UI) keys on the relay-stamped session id and drops
/// the roster entry — name and all — so a name sent with a LOWER is read by
/// nobody. Every lower was paying ~14-66 bytes on every participant's DOWNLINK
/// for a field that is discarded on arrival. This mirrors the `raised_at_ms`
/// normalisation in `send_raise_hand`: a field the consumer cannot use does not
/// go on the wire.
///
/// The relay is fine with the result — a bare LOWER serialises to an EMPTY inner
/// payload under proto3 default-elision, which `classify_packet` explicitly
/// accepts (see `test_2135_classify_wellformed_raise_hand_is_forwardable`).
///
/// `max_chars` caps by CHARACTERS, never bytes, so truncation can never split a
/// UTF-8 codepoint; the relay independently bounds the field in BYTES on ingress.
pub fn raise_hand_display_name_bytes(
    raised: bool,
    display_name: &str,
    max_chars: usize,
) -> Vec<u8> {
    if !raised {
        return Vec::new();
    }
    display_name
        .chars()
        .take(max_chars)
        .collect::<String>()
        .into_bytes()
}

/// Why a send is being requested. The two triggers differ ONLY in urgency —
/// both put the same absolute level on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaiseHandTrigger {
    /// The local user pressed the raise/lower control: a real state transition
    /// the room does not know about yet. Sent as soon as the rate gate allows.
    LocalToggle,
    /// A peer joined, or we reconnected / were re-elected, so someone in the
    /// room may not know our current level. Always debounced by
    /// [`RAISE_HAND_REANNOUNCE_COALESCE_MS`] so a join wave costs one send.
    ReAnnounce,
}

/// What the caller must do with a [`RaiseHandAnnouncer::request`] /
/// [`RaiseHandAnnouncer::flush`] result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RaiseHandSend {
    /// Serialize and send the current level NOW, then call
    /// [`RaiseHandAnnouncer::note_sent`] with the same `now_ms`.
    Now,
    /// Nothing to put on the wire: the room already knows this exact level and
    /// no re-announce is outstanding.
    Skip,
    /// Arm a ONE-SHOT timer for `delay_ms` that calls
    /// [`RaiseHandAnnouncer::flush`]. Only ever returned when no flush is
    /// already armed, so the caller never holds two timers at once.
    Defer { delay_ms: f64 },
    /// A flush timer is already armed and will carry this request too. Do
    /// nothing — this is the join-wave collapse.
    Coalesced,
}

impl RaiseHandSend {
    /// The deferral in whole milliseconds, or `None` for every non-deferring
    /// variant.
    ///
    /// This is the ACTUAL conversion the Dioxus driver performs before calling
    /// `gloo_timers::callback::Timeout::new` (which takes a `u32` of
    /// milliseconds), lifted here so the saturating cast is production code
    /// covered by a host test rather than an untested `as u32` at the call site.
    /// An `f64 -> u32` cast saturates in Rust, but a NEGATIVE input would cast to
    /// `0` only by that same saturation, so the clamp is written explicitly.
    pub fn delay_millis(self) -> Option<u32> {
        match self {
            RaiseHandSend::Defer { delay_ms } => Some(delay_ms.max(0.0) as u32),
            _ => None,
        }
    }
}

/// The local participant's raise-hand send state machine (issue 2135).
///
/// Owns three separable facts and keeps them consistent:
///   * `raised` — the level the LOCAL user has chosen (what the UI renders
///     optimistically, without waiting for the wire);
///   * `announced` — the level the ROOM was last told, so a redundant send can
///     be skipped and a stale one repaired;
///   * `raised_at_ms` — the wall-clock instant of the false→true edge, stamped
///     ONCE and preserved verbatim across every re-announce (a re-stamp would
///     move the raiser to the back of every peer's queue whenever anyone joins).
#[derive(Debug, Clone)]
pub struct RaiseHandAnnouncer {
    /// The local level. `true` = this participant's hand is up.
    raised: bool,
    /// Wall-clock ms of the most recent false→true transition. `0` while
    /// lowered (the proto documents `raised_at_ms` as meaningless then).
    raised_at_ms: u64,
    /// The level the room was last TOLD. `None` means "unknown" — only reachable
    /// through [`Self::invalidate_announced`] after a reconnect.
    ///
    /// Seeded to `Some(false)` rather than `None` because a participant that has
    /// never sent anything IS correctly rendered as lowered by every peer: that
    /// is the proto3 default and the UI's initial state. Seeding `None` would
    /// make the very first peer-join of every cold start emit a pointless
    /// `raised = false` packet.
    announced: Option<bool>,
    /// Set when a re-announce is outstanding: some peer may not have heard our
    /// (raised) level yet, even though `announced == Some(true)`. Cleared by a
    /// send. Never set while lowered.
    reannounce_wanted: bool,
    /// Whether this participant has raised its hand at least once this session.
    /// Gates the reconnect self-heal so the overwhelmingly common
    /// never-raised-a-hand participant costs zero packets per reconnect.
    ever_raised: bool,
    /// `now_ms` of the last packet handed to the transport.
    last_send_ms: Option<f64>,
    /// Absolute `now_ms` deadline of the armed flush timer, if any.
    pending_deadline_ms: Option<f64>,
}

impl Default for RaiseHandAnnouncer {
    fn default() -> Self {
        Self::new()
    }
}

impl RaiseHandAnnouncer {
    /// A fresh announcer: hand down, room correctly believes it is down.
    pub fn new() -> Self {
        Self {
            raised: false,
            raised_at_ms: 0,
            announced: Some(false),
            reannounce_wanted: false,
            ever_raised: false,
            last_send_ms: None,
            pending_deadline_ms: None,
        }
    }

    /// The local level (what the UI renders for itself, immediately on click).
    pub fn is_raised(&self) -> bool {
        self.raised
    }

    /// The stamp to put in `RaiseHandPacket.raised_at_ms`. `0` while lowered.
    pub fn raised_at_ms(&self) -> u64 {
        self.raised_at_ms
    }

    /// Whether a flush timer is currently armed (the caller must hold exactly
    /// one live timer whenever this is `true`).
    pub fn has_pending_flush(&self) -> bool {
        self.pending_deadline_ms.is_some()
    }

    /// Apply an absolute level. `wall_now_ms` is `Date.now()`; it is read ONLY
    /// on the false→true edge.
    ///
    /// Idempotent by construction — setting the level it already holds changes
    /// nothing and, crucially, does NOT re-stamp `raised_at_ms`. That matters
    /// beyond tidiness: a UI that re-applies its own level on a re-render (or a
    /// double-fired click) must not silently move the local user to the back of
    /// the room's raise order.
    pub fn set_level(&mut self, raised: bool, wall_now_ms: f64) {
        if raised == self.raised {
            return;
        }
        self.raised = raised;
        if raised {
            // Stamp ONCE, here, at the false→true edge. Every subsequent send
            // (including every re-announce) reuses this value verbatim.
            self.raised_at_ms = wall_now_ms.max(0.0) as u64;
            self.ever_raised = true;
        } else {
            self.raised_at_ms = 0;
            // Nothing to re-announce once the hand is down: peers render a
            // lowered participant from the default, so the outstanding
            // re-announce is void. (The LOWER itself still goes out — that is
            // an `announced != raised` mismatch, not a re-announce.)
            self.reannounce_wanted = false;
        }
    }

    /// Forget what the room was told, so the next re-announce repairs it.
    ///
    /// Call on `on_connected` after a RECONNECT / re-election. Two distinct
    /// desyncs are possible across a transport gap and both are repaired by this
    /// one call, because it makes `announced` unequal to any level:
    ///   * we were RAISED and genuinely departed, so peers cleared us on
    ///     `PARTICIPANT_LEFT` and must be told again; and
    ///   * we LOWERED while the transport was down, so the packet went nowhere
    ///     and peers that survived the blip still show our hand up. Rule 2
    ///     ("never re-announce lowered") would otherwise suppress that repair
    ///     forever — this is exactly why `request` consults `announced` and not
    ///     `raised` alone.
    ///
    /// No-op for a participant that has never raised a hand this session: the
    /// room's default view of it cannot be stale, so invalidating would cost one
    /// wasted broadcast per reconnect for every participant in the meeting —
    /// precisely the O(peers) waste rule 2 exists to prevent.
    pub fn invalidate_announced(&mut self) {
        if !self.ever_raised {
            return;
        }
        self.announced = None;
    }

    /// Is there anything worth putting on the wire right now?
    ///
    /// Two independent reasons, and the second is why `announced` alone is not
    /// enough: a hand that is already announced as raised must STILL be re-sent
    /// when a new peer appears, because that peer never heard the original.
    fn has_work(&self) -> bool {
        self.announced != Some(self.raised) || (self.raised && self.reannounce_wanted)
    }

    /// Earliest `now_ms` at which the rate gate permits another send.
    fn earliest_send_ms(&self, now_ms: f64) -> f64 {
        match self.last_send_ms {
            Some(t) => (t + RAISE_HAND_MIN_SEND_INTERVAL_MS).max(f64::MIN),
            // Never sent: allowed immediately.
            None => now_ms,
        }
    }

    /// Decide what to do about a send request at `now_ms` (a wall clock, e.g.
    /// `Date.now()`).
    ///
    /// For [`RaiseHandTrigger::LocalToggle`] the caller must have already
    /// applied the new level via [`Self::set_level`].
    pub fn request(&mut self, trigger: RaiseHandTrigger, now_ms: f64) -> RaiseHandSend {
        if trigger == RaiseHandTrigger::ReAnnounce {
            // Rule 2 — "never re-announce a LOWERED hand" — is enforced by the
            // `self.raised &&` HERE together with the matching `self.raised &&`
            // in `has_work`, and by nothing else. Stated explicitly because an
            // earlier revision of this function ALSO carried an
            // `if !self.raised && self.announced == Some(false) { return Skip }`
            // early-return that read like the rule's implementation but was
            // provably dead: in exactly that state `has_work()` is already false,
            // so the guard could never change an outcome. It was removed rather
            // than kept as "defense in depth", because a guard that cannot fire
            // is not defense — it is a claim no test can hold to account (the
            // mutation that deleted it killed no test).
            //
            // The rule's ONE exception falls out of `has_work` for free: when the
            // room's view of us is UNKNOWN (`announced == None`, i.e. after a
            // reconnect), `announced != Some(false)` holds even for a lowered
            // hand, so a LOWER lost in the transport gap is repaired exactly once
            // and then goes quiet.
            if self.raised {
                self.reannounce_wanted = true;
            }
        }

        // Rule 3, the join-wave collapse: one armed flush carries every request
        // that arrives while it is pending, whatever the trigger. A LocalToggle
        // that lands mid-wave therefore waits out the remaining window (bounded
        // by RAISE_HAND_REANNOUNCE_COALESCE_MS) instead of shortening it — the
        // UI has already rendered the user's own hand optimistically, so the
        // only cost is peer-visible latency, and holding ONE timer at a time is
        // what makes the "no stale timer can fire against a later state"
        // invariant trivially true.
        //
        // Checked BEFORE `has_work()` deliberately: while a flush is armed, the
        // FLUSH owns the decision (it re-reads the level when it fires), so this
        // must not answer `Skip` — which would tell a caller that nothing is
        // outstanding while a timer is in fact about to fire. A rapid toggle that
        // momentarily returns to the announced level is exactly that case.
        if self.pending_deadline_ms.is_some() {
            return RaiseHandSend::Coalesced;
        }

        if !self.has_work() {
            return RaiseHandSend::Skip;
        }

        let earliest = self.earliest_send_ms(now_ms);
        let target = match trigger {
            RaiseHandTrigger::LocalToggle => earliest,
            RaiseHandTrigger::ReAnnounce => {
                earliest.max(now_ms + RAISE_HAND_REANNOUNCE_COALESCE_MS)
            }
        };

        if target <= now_ms {
            RaiseHandSend::Now
        } else {
            let delay_ms = (target - now_ms).min(RAISE_HAND_MAX_DEFER_MS);
            // Store the CLAMPED deadline so `flush`'s re-check agrees with the
            // timer the caller actually arms (an unclamped deadline after a
            // backward wall-clock step would make every flush re-defer).
            self.pending_deadline_ms = Some(now_ms + delay_ms);
            RaiseHandSend::Defer { delay_ms }
        }
    }

    /// The armed timer fired: decide what to send NOW, re-evaluated against the
    /// CURRENT level (which may have flipped while the timer ran).
    ///
    /// This re-evaluation is the reason a coalesced LocalToggle is never lost:
    /// the request that armed the timer does not capture a level, so whatever
    /// the user settled on is what goes on the wire.
    ///
    /// Returns ONLY [`RaiseHandSend::Now`] or [`RaiseHandSend::Skip`] — never
    /// another `Defer`, and that is deliberate on two independent grounds:
    ///
    /// * **It would be wrong.** The rate gate is about REAL elapsed time, and
    ///   `setTimeout` measures real time, not the wall clock. The timer was armed
    ///   for exactly the number of milliseconds needed to reach
    ///   `last_send + RAISE_HAND_MIN_SEND_INTERVAL_MS`, so by the time it fires
    ///   that interval HAS elapsed — whatever `now_ms` claims. Re-gating on a
    ///   wall clock that has since stepped BACKWARD would refuse a send the relay
    ///   would have accepted, and could do so repeatedly.
    /// * **It would be unsafe to act on.** The driver holds the live `Timeout` so
    ///   unmount cancels it; re-arming from inside the callback would replace
    ///   (and therefore DROP) the `Closure` that is currently executing.
    pub fn flush(&mut self, now_ms: f64) -> RaiseHandSend {
        self.pending_deadline_ms = None;
        if !self.has_work() {
            return RaiseHandSend::Skip;
        }
        // Record the send time from the caller's clock like every other path, so
        // the NEXT request's gate is measured from here.
        let _ = now_ms;
        RaiseHandSend::Now
    }

    /// Record that a packet carrying the CURRENT level was handed to the
    /// transport at `now_ms`. Must be called for every [`RaiseHandSend::Now`].
    pub fn note_sent(&mut self, now_ms: f64) {
        self.last_send_ms = Some(now_ms);
        self.announced = Some(self.raised);
        self.reannounce_wanted = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client's own ceiling must stay STRICTLY under the relay's.
    ///
    /// The budget is READ FROM `videocall_types::limits` — the same constants
    /// `actix-api`'s rate limiter is built from (it re-exports them) — so this is
    /// a real cross-crate pin in BOTH directions: lowering
    /// `RAISE_HAND_MIN_SEND_INTERVAL_MS` into the budget fails it, and so does
    /// tightening the RELAY's budget down onto our interval.
    ///
    /// This test previously declared its own `RELAY_WINDOW_MS` /
    /// `RELAY_MAX_PER_WINDOW` locals that hand-copied the actix-api literals,
    /// while its doc claimed to be "expressed against the relay's literals". It
    /// was not: a copy cannot observe a change to the thing it copied, so the
    /// half of the property that matters most — the relay tightening under us —
    /// was unpinned, and no mutation of `actix-api` could ever turn it red.
    ///
    /// ADVERSARIAL (mutation), both directions:
    ///   * set `RAISE_HAND_MIN_SEND_INTERVAL_MS` to 300.0 → `max_in_relay_window`
    ///     becomes 7, not < 6 → red.
    ///   * set `videocall_types::limits::RAISE_HAND_MAX_PER_WINDOW` to 5 → our 5
    ///     is not < 5 → red. (Under the old copy: still green.)
    #[test]
    fn self_interval_stays_strictly_under_the_relay_budget() {
        use videocall_types::limits::{RAISE_HAND_MAX_PER_WINDOW, RAISE_HAND_WINDOW_MS};

        // Worst case: a send exactly on the window start, then one every
        // interval, so the count in a closed window is floor(w / i) + 1.
        let max_in_relay_window =
            (RAISE_HAND_WINDOW_MS as f64 / RAISE_HAND_MIN_SEND_INTERVAL_MS).floor() as u32 + 1;
        assert!(
            max_in_relay_window < RAISE_HAND_MAX_PER_WINDOW,
            "client may emit up to {max_in_relay_window} per {RAISE_HAND_WINDOW_MS}ms, \
             which is not strictly under the relay's {RAISE_HAND_MAX_PER_WINDOW}",
        );
    }

    // ===================================================================
    // Wire payload: the cosmetic name rides only on a RAISE.
    // ===================================================================

    /// A LOWER carries NO display name. The receiver's lower path keys on the
    /// relay-stamped session id and drops the roster entry outright, so the name
    /// is discarded on arrival — sending it spent bytes on every participant's
    /// downlink for a field nobody reads.
    ///
    /// ADVERSARIAL (mutation): delete the `if !raised { return Vec::new(); }`
    /// guard → the LOWER carries "Antonio" → red.
    #[test]
    fn a_lower_carries_no_display_name() {
        assert!(
            raise_hand_display_name_bytes(false, "Antonio", 64).is_empty(),
            "a LOWER must not put a name the consumer discards on the wire",
        );
        assert_eq!(
            raise_hand_display_name_bytes(true, "Antonio", 64),
            b"Antonio".to_vec(),
            "a RAISE still carries it — that is the pre-join cache-race fallback",
        );
    }

    /// The cap counts CHARACTERS, not bytes, so truncation can never split a
    /// UTF-8 codepoint and hand the relay an invalid string.
    ///
    /// ADVERSARIAL (mutation): change `.chars().take(max)` to a byte slice
    /// `&display_name[..max]` → panics mid-codepoint on this input → red.
    #[test]
    fn the_name_cap_never_splits_a_codepoint() {
        // 3 bytes per char, so a byte-based cap of 4 would land mid-codepoint.
        let name = "あ".repeat(10);
        let out = raise_hand_display_name_bytes(true, &name, 4);
        assert_eq!(out.len(), 12, "4 chars x 3 bytes");
        assert_eq!(
            String::from_utf8(out).expect("must remain valid UTF-8"),
            "ああああ",
        );
    }

    // ===================================================================
    // Rule 4: `raised_at_ms` is stamped once and preserved verbatim.
    // ===================================================================

    /// ADVERSARIAL (mutation): move the `self.raised_at_ms = ...` assignment out
    /// of the `if raised` arm so it runs on every `set_level` → the second
    /// `set_level(true, ...)` re-stamps → red.
    #[test]
    fn raised_at_ms_is_stamped_once_and_preserved_across_reannounce() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 1_000.0);
        assert_eq!(a.raised_at_ms(), 1_000);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 1_000.0),
            RaiseHandSend::Now
        );
        a.note_sent(1_000.0);

        // A redundant re-apply of the SAME level must not re-stamp.
        a.set_level(true, 9_999.0);
        assert_eq!(
            a.raised_at_ms(),
            1_000,
            "re-applying the level must not re-stamp"
        );

        // Neither may any number of re-announces.
        for t in [5_000.0, 30_000.0, 120_000.0] {
            let d = a.request(RaiseHandTrigger::ReAnnounce, t);
            assert!(matches!(d, RaiseHandSend::Defer { .. }), "got {d:?}");
            assert_eq!(
                a.flush(t + RAISE_HAND_REANNOUNCE_COALESCE_MS),
                RaiseHandSend::Now
            );
            a.note_sent(t + RAISE_HAND_REANNOUNCE_COALESCE_MS);
            assert_eq!(
                a.raised_at_ms(),
                1_000,
                "re-announce must preserve raised_at_ms verbatim (t={t})"
            );
        }
    }

    /// Lowering clears the stamp (the proto documents it as meaningless while
    /// lowered), and a LATER raise gets a NEW stamp — it goes to the back of the
    /// room's queue, which is the correct semantic.
    ///
    /// ADVERSARIAL (mutation): drop the `self.raised_at_ms = 0;` in the lowered
    /// arm → the first assert reads 1_000 → red.
    #[test]
    fn lowering_clears_the_stamp_and_a_later_raise_restamps() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 1_000.0);
        a.set_level(false, 2_000.0);
        assert_eq!(a.raised_at_ms(), 0, "lowered hands carry no stamp");
        a.set_level(true, 3_000.0);
        assert_eq!(a.raised_at_ms(), 3_000, "a fresh raise takes a fresh stamp");
    }

    // ===================================================================
    // Rule 2: never re-announce a lowered hand.
    // ===================================================================

    /// ADVERSARIAL (mutation): rule 2 is DOUBLY guarded — by the `self.raised &&`
    /// around `reannounce_wanted = true` in `request`, and by the `self.raised &&`
    /// in `has_work`. Either alone still suppresses the send, so the mutation
    /// that kills this test must remove BOTH: make the `request` assignment
    /// unconditional AND change `has_work`'s `(self.raised &&
    /// self.reannounce_wanted)` to `self.reannounce_wanted`. Verified: the
    /// compound mutation turns all 20 `Skip`s into `Defer`s.
    #[test]
    fn reannounce_while_lowered_is_never_sent() {
        let mut a = RaiseHandAnnouncer::new();
        // A 20-person join wave against a participant who never raised a hand.
        for i in 0..20 {
            assert_eq!(
                a.request(RaiseHandTrigger::ReAnnounce, i as f64 * 40.0),
                RaiseHandSend::Skip,
                "a lowered hand must cost ZERO packets per joining peer",
            );
        }
        assert!(!a.has_pending_flush(), "and must not even arm a timer");
    }

    /// A LOWER is a real transition and IS broadcast — once.
    ///
    /// ADVERSARIAL (mutation): make `has_work` return `self.raised &&
    /// self.reannounce_wanted` only → the LOWER is never sent → red.
    #[test]
    fn lowering_broadcasts_once_then_goes_quiet() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);

        a.set_level(false, 1_000.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 1_000.0),
            RaiseHandSend::Now,
            "the LOWER transition must reach the room"
        );
        a.note_sent(1_000.0);

        // Now the room knows; nothing more is owed to anyone.
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 5_000.0),
            RaiseHandSend::Skip
        );
        assert_eq!(
            a.request(RaiseHandTrigger::ReAnnounce, 5_000.0),
            RaiseHandSend::Skip
        );
    }

    // ===================================================================
    // Rule 3: coalesce a join wave into ONE send.
    // ===================================================================

    /// ADVERSARIAL (mutation): delete the `if self.pending_deadline_ms.is_some()
    /// { return Coalesced; }` early-return → each of the 20 joins asks for its
    /// own Defer → the `Coalesced` count assert goes to 0 → red.
    #[test]
    fn a_join_wave_collapses_to_exactly_one_send() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);

        // 20 peers join over 400ms (a realistic wave).
        let mut defers = 0;
        let mut coalesced = 0;
        for i in 0..20 {
            match a.request(RaiseHandTrigger::ReAnnounce, 10_000.0 + i as f64 * 20.0) {
                RaiseHandSend::Defer { .. } => defers += 1,
                RaiseHandSend::Coalesced => coalesced += 1,
                other => panic!("unexpected {other:?} for join #{i}"),
            }
        }
        assert_eq!(defers, 1, "exactly ONE timer is armed for the whole wave");
        assert_eq!(coalesced, 19, "every other join rides that timer");

        // And the single flush produces exactly one packet for the whole wave.
        assert_eq!(a.flush(10_750.0), RaiseHandSend::Now);
        a.note_sent(10_750.0);
        assert!(
            !a.has_pending_flush(),
            "the wave is fully served by that one send — no timer is left armed"
        );
    }

    /// A join arriving AFTER the wave's flush is a genuinely new peer that never
    /// heard us, so it earns its own (debounced) send. This is the
    /// counterexample that keeps the coalescing above from being over-read as
    /// "one re-announce per meeting".
    ///
    /// ADVERSARIAL (mutation): drop the `self.reannounce_wanted = true;`
    /// assignment in `request` → `has_work()` is false (the level is already
    /// announced) → the late joiner never learns the hand is up → red.
    #[test]
    fn a_peer_joining_after_the_wave_still_gets_its_own_reannounce() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);
        // A first wave, served.
        assert!(matches!(
            a.request(RaiseHandTrigger::ReAnnounce, 10_000.0),
            RaiseHandSend::Defer { .. }
        ));
        assert_eq!(a.flush(10_750.0), RaiseHandSend::Now);
        a.note_sent(10_750.0);
        // A latecomer 30s later — the level is already `announced`, so ONLY the
        // re-announce flag can make this send happen.
        assert!(
            matches!(
                a.request(RaiseHandTrigger::ReAnnounce, 40_000.0),
                RaiseHandSend::Defer { .. }
            ),
            "a peer that joined after the wave must still be told"
        );
    }

    /// A re-announce is ALWAYS debounced, even when the rate gate would allow an
    /// immediate send — that is what makes the wave collapse work.
    ///
    /// ADVERSARIAL (mutation): change the `ReAnnounce` arm's target to plain
    /// `earliest` → the first join sends immediately and the wave no longer
    /// collapses onto one timer → red.
    #[test]
    fn reannounce_is_debounced_even_when_the_rate_gate_is_open() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);
        // 60s later the rate gate is wide open, yet the re-announce still waits.
        assert_eq!(
            a.request(RaiseHandTrigger::ReAnnounce, 60_000.0),
            RaiseHandSend::Defer {
                delay_ms: RAISE_HAND_REANNOUNCE_COALESCE_MS
            },
        );
    }

    // ===================================================================
    // Rule 7 / issue-2135 design: coalesce, never drop.
    // ===================================================================

    /// Rapid toggling must never LOSE the final level — the distinguishing
    /// property versus the reactions self-throttle, which silently drops.
    ///
    /// ADVERSARIAL (mutation): make `flush` capture and send the level that was
    /// current when the timer was armed (e.g. by returning `Skip` when
    /// `announced == Some(self.raised)` is false in the other direction) → the
    /// final `Now` disappears or carries the wrong level → red.
    #[test]
    fn rapid_toggling_coalesces_to_the_final_level_and_never_drops_it() {
        let mut a = RaiseHandAnnouncer::new();
        // Raise -> sent immediately.
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);

        // Lower 50ms later: inside the min interval, so it must DEFER, not drop.
        a.set_level(false, 50.0);
        let d = a.request(RaiseHandTrigger::LocalToggle, 50.0);
        assert_eq!(d, RaiseHandSend::Defer { delay_ms: 450.0 });

        // The user re-raises and re-lowers while the timer runs.
        a.set_level(true, 120.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 120.0),
            RaiseHandSend::Coalesced
        );
        a.set_level(false, 300.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 300.0),
            RaiseHandSend::Coalesced
        );

        // The flush sends the FINAL level (lowered), which the room did not know.
        assert_eq!(a.flush(500.0), RaiseHandSend::Now);
        assert!(!a.is_raised(), "the packet carries the final level");
        a.note_sent(500.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 501.0),
            RaiseHandSend::Skip
        );
    }

    /// A coalesced burst that ends back where it started sends NOTHING extra —
    /// the level is idempotent, so there is nothing to tell anyone.
    ///
    /// ADVERSARIAL (mutation): make `has_work` return `true` unconditionally →
    /// the flush emits a redundant packet → red.
    #[test]
    fn a_toggle_burst_returning_to_the_announced_level_sends_nothing() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);

        a.set_level(false, 50.0);
        assert!(matches!(
            a.request(RaiseHandTrigger::LocalToggle, 50.0),
            RaiseHandSend::Defer { .. }
        ));
        a.set_level(true, 100.0); // back to raised, which the room already has
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 100.0),
            RaiseHandSend::Coalesced
        );

        assert_eq!(
            a.flush(500.0),
            RaiseHandSend::Skip,
            "nothing changed from the room's point of view"
        );
    }

    /// The rate gate is real: two toggles inside the min interval cannot both
    /// reach the wire immediately.
    ///
    /// ADVERSARIAL (mutation): make `earliest_send_ms` return `now_ms`
    /// unconditionally → the second toggle returns `Now` → red.
    #[test]
    fn a_second_send_inside_the_min_interval_is_deferred_not_sent() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);
        a.set_level(false, RAISE_HAND_MIN_SEND_INTERVAL_MS - 1.0);
        assert_eq!(
            a.request(
                RaiseHandTrigger::LocalToggle,
                RAISE_HAND_MIN_SEND_INTERVAL_MS - 1.0
            ),
            RaiseHandSend::Defer { delay_ms: 1.0 },
        );
    }

    /// At exactly the min interval the gate opens.
    #[test]
    fn a_send_at_exactly_the_min_interval_is_allowed() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);
        a.set_level(false, RAISE_HAND_MIN_SEND_INTERVAL_MS);
        assert_eq!(
            a.request(
                RaiseHandTrigger::LocalToggle,
                RAISE_HAND_MIN_SEND_INTERVAL_MS
            ),
            RaiseHandSend::Now,
        );
    }

    // ===================================================================
    // Reconnect / re-election self-heal.
    // ===================================================================

    /// After a reconnect, a still-raised hand must be re-announced with its
    /// ORIGINAL stamp so late joiners order the queue correctly.
    ///
    /// ADVERSARIAL (mutation): add `if self.raised { self.raised_at_ms = now_ms
    /// as u64; }` to `note_sent` — the "just re-stamp whenever we send" mistake a
    /// naive implementation makes. The final assertion then reads 60_750 instead
    /// of 1_234 → red.
    ///
    /// The `note_sent` call after the flush is load-bearing to THAT: an earlier
    /// revision of this test asserted the stamp without it, so it never exercised
    /// the send path at all and the mutation survived. Drive the FULL cycle
    /// (request → flush → note_sent) exactly as the driver does.
    #[test]
    fn reconnect_reannounces_a_raised_hand_with_the_original_stamp() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 1_234.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 1_234.0),
            RaiseHandSend::Now
        );
        a.note_sent(1_234.0);

        a.invalidate_announced();
        let d = a.request(RaiseHandTrigger::ReAnnounce, 60_000.0);
        assert!(matches!(d, RaiseHandSend::Defer { .. }), "got {d:?}");
        assert_eq!(a.flush(60_750.0), RaiseHandSend::Now);
        a.note_sent(60_750.0);
        assert!(a.is_raised());
        assert_eq!(
            a.raised_at_ms(),
            1_234,
            "a reconnect must NOT re-stamp — late joiners order on this value"
        );
    }

    /// The desync rule 2 would otherwise wedge forever: the user LOWERED while
    /// the transport was down, so peers that survived the blip still show the
    /// hand up. The reconnect must repair it exactly once, then go quiet.
    ///
    /// ADVERSARIAL (mutation): make `invalidate_announced` a no-op (or have the
    /// `ReAnnounce` guard test `!self.raised` alone, ignoring `announced`) → the
    /// repair returns `Skip` → red.
    #[test]
    fn reconnect_repairs_a_lower_lost_in_the_transport_gap() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 0.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 0.0),
            RaiseHandSend::Now
        );
        a.note_sent(0.0);

        // The user lowers; the packet is handed to a dead transport (we still
        // note it, because the client cannot know it was dropped).
        a.set_level(false, 1_000.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 1_000.0),
            RaiseHandSend::Now
        );
        a.note_sent(1_000.0);

        // Reconnect: the room's view of us is now UNKNOWN.
        a.invalidate_announced();
        let d = a.request(RaiseHandTrigger::ReAnnounce, 5_000.0);
        assert!(
            matches!(d, RaiseHandSend::Defer { .. }),
            "the lost LOWER must be repaired after a reconnect, got {d:?}"
        );
        assert_eq!(a.flush(5_750.0), RaiseHandSend::Now);
        assert!(
            !a.is_raised(),
            "the repair carries the CURRENT (lowered) level"
        );
        a.note_sent(5_750.0);

        // Repaired once — subsequent joins are free again.
        assert_eq!(
            a.request(RaiseHandTrigger::ReAnnounce, 6_000.0),
            RaiseHandSend::Skip
        );
    }

    /// A participant that never raised a hand costs ZERO packets per reconnect,
    /// even in a 50-person meeting reconnecting after a network blip.
    ///
    /// ADVERSARIAL (mutation): delete the `if !self.ever_raised { return; }`
    /// early-return in `invalidate_announced` → the post-reconnect join emits a
    /// pointless `raised = false` packet → red.
    #[test]
    fn reconnect_costs_nothing_for_a_participant_who_never_raised() {
        let mut a = RaiseHandAnnouncer::new();
        a.invalidate_announced();
        assert_eq!(
            a.request(RaiseHandTrigger::ReAnnounce, 1_000.0),
            RaiseHandSend::Skip,
            "a never-raised participant must not broadcast on reconnect",
        );
    }

    // ===================================================================
    // Defensive: clock steps.
    // ===================================================================

    /// A backward wall-clock step (NTP correction) must not arm a timer minutes
    /// into the future and freeze the local hand off the wire.
    ///
    /// ADVERSARIAL (mutation): drop the `.min(RAISE_HAND_MAX_DEFER_MS)` clamp →
    /// `delay_ms` becomes 600_000 → red.
    #[test]
    fn a_backward_clock_step_cannot_arm_an_unbounded_timer() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 600_000.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 600_000.0),
            RaiseHandSend::Now
        );
        a.note_sent(600_000.0);
        // Wall clock steps back 10 minutes.
        a.set_level(false, 0.0);
        match a.request(RaiseHandTrigger::LocalToggle, 0.0) {
            RaiseHandSend::Defer { delay_ms } => assert!(
                delay_ms <= RAISE_HAND_MAX_DEFER_MS,
                "deferral {delay_ms}ms must be clamped to {RAISE_HAND_MAX_DEFER_MS}ms",
            ),
            other => panic!("expected a clamped Defer, got {other:?}"),
        }
    }

    /// `flush` NEVER re-defers, even when the wall clock has stepped backward so
    /// far that the rate gate would appear unsatisfied.
    ///
    /// Two independent reasons, both load-bearing (see `flush`'s doc):
    /// `setTimeout` measures REAL time so the interval has genuinely elapsed;
    /// and re-arming would force the driver to replace — and therefore DROP —
    /// the `Closure` currently executing.
    ///
    /// ADVERSARIAL (mutation): reinstate a `Defer` branch in `flush` → the
    /// `assert_eq!(..., Now)` goes red, and `has_pending_flush()` becomes true.
    #[test]
    fn flush_never_re_defers_even_after_a_backward_clock_step() {
        let mut a = RaiseHandAnnouncer::new();
        a.set_level(true, 10_000.0);
        assert_eq!(
            a.request(RaiseHandTrigger::LocalToggle, 10_000.0),
            RaiseHandSend::Now
        );
        a.note_sent(10_000.0);
        a.set_level(false, 10_100.0);
        assert!(matches!(
            a.request(RaiseHandTrigger::LocalToggle, 10_100.0),
            RaiseHandSend::Defer { .. }
        ));
        // The wall clock steps back before the timer fires. The timer itself was
        // armed for the correct REAL duration, so the send must proceed.
        assert_eq!(a.flush(10_050.0), RaiseHandSend::Now);
        assert!(
            !a.has_pending_flush(),
            "flush must leave NO timer armed — the driver never re-arms from \
             inside the callback it is running in"
        );
    }

    /// `delay_millis` is the conversion the Dioxus driver actually calls before
    /// arming its `gloo` timeout, so this covers the production cast rather than
    /// a parallel copy of it.
    ///
    /// ADVERSARIAL (mutation): drop the `.max(0.0)` clamp → the negative case
    /// still yields 0 via saturation, so instead make it `Some(delay_ms as u32)`
    /// AND return `Some(0)` for `Now` → the `None` assertions go red.
    #[test]
    fn delay_millis_maps_only_the_defer_variant() {
        assert_eq!(
            RaiseHandSend::Defer { delay_ms: 450.0 }.delay_millis(),
            Some(450)
        );
        assert_eq!(RaiseHandSend::Now.delay_millis(), None);
        assert_eq!(RaiseHandSend::Skip.delay_millis(), None);
        assert_eq!(RaiseHandSend::Coalesced.delay_millis(), None);
        // A negative delay (only reachable through a caller bug) becomes an
        // immediate flush rather than a wrapped, effectively-infinite timer.
        assert_eq!(
            RaiseHandSend::Defer { delay_ms: -5.0 }.delay_millis(),
            Some(0)
        );
    }
}
