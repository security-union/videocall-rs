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

//! Pure, host-testable policy for the meeting-timer feature (issue 2136).
//!
//! Everything in this module is plain Rust over injected clocks — no `web_sys`,
//! no signals, no timers. That is deliberate: the parts of this feature most
//! likely to be wrong are the LAST-WRITER-WINS comparator, the saturating
//! arithmetic, and the send cadence, and all three are only cheap to pin if they
//! are reachable from a plain `#[test]`. The Dioxus layer owns the actual
//! `Interval`s and calls in here for every decision.
//!
//! The wire contract these implement is specified on `MeetingTimerPacket` in
//! `protobuf/types/meeting_timer_packet.proto`; the rule numbers referenced
//! throughout are that document's.

use videocall_types::protos::meeting_timer_packet::MeetingTimerPacket;

/// Re-exported so every consumer of this module reaches the bound by ONE path.
/// The UI must clamp against it before sending (an over-cap packet is DROPPED by
/// the relay, not clamped), and a second import path is how a second, drifting
/// copy of the value gets introduced.
pub use videocall_types::limits::MEETING_TIMER_MAX_DURATION_MS;

/// How long a state may be older than the applied one and still be rejected as
/// reordering (wire contract rule 5).
///
/// THE UPPER BOUND IS THE WHOLE POINT, and it is why this is a window rather
/// than a plain `incoming < applied` comparison. Nothing on any of these paths
/// is in flight for five seconds, so a packet older than that is not reordering
/// — it is the host's wall clock stepping backwards (an NTP correction, a manual
/// change, a laptop resuming from sleep). An UNBOUNDED comparator would then
/// reject every state that host subsequently authors, INCLUDING THE CANCEL,
/// leaving a timer nobody can stop counting down to an audible expiry in every
/// participant's browser. That is the #2122 shape exactly: a guard written to
/// fail open that fails closed once clock arithmetic gets involved.
///
/// Bounding the rejection makes a large backwards step SELF-CORRECTING — the
/// next packet the host sends is simply applied — while still suppressing the
/// genuine reordering the WebTransport datagram path produces.
pub const MEETING_TIMER_LWW_REJECT_WINDOW_MS: u64 = 5_000;

/// Interval between heartbeat re-sends of a RUNNING timer (wire contract rule 1).
///
/// This one rule subsumes late-joiner delivery, reconnect recovery, and repair
/// of a packet lost on the unreliable QUIC datagram path — with no join-event
/// plumbing at all. It is affordable here, and was NOT affordable for raise-hand
/// (#2135), because only ONE session in the room can send this packet type: the
/// cost is O(1) per room rather than O(participants).
pub const MEETING_TIMER_HEARTBEAT_MS: u64 = 5_000;

/// Number of times each transition (start / extend / cancel) is re-sent
/// (wire contract rule 2).
///
/// A CANCEL is the case that forces this. Once `running` is false the host stops
/// heartbeating, so a cancel has no repair mechanism behind it — a single lost
/// packet would leave that peer counting down to an expiry sound the host
/// already called off. Rule 5 makes every repeat a no-op for anyone who already
/// got the first copy.
pub const MEETING_TIMER_TRANSITION_REPEATS: u8 = 3;

/// Spacing between the repeats of one transition burst.
pub const MEETING_TIMER_REPEAT_SPACING_MS: u64 = 1_000;

/// How long past `ends_at_ms` the host keeps heartbeating an expired timer.
///
/// Nothing clears `running` when a timer reaches zero — the state stays "running
/// until T" and only a CANCEL clears it — so without this bound the heartbeat is
/// the one unbounded term in the whole feature: a 5-minute timer nobody
/// cancelled in a 60-minute meeting re-announces for 55 more minutes, which at 20
/// participants is on the order of 11,000 relay deliveries carrying no news.
///
/// The window is not zero because "the timer just ran out" IS news to someone who
/// joins right after it does: for this long they still get the expired state, see
/// `0:00`, and get the visual Expired treatment. Past it the host goes quiet and a
/// late joiner simply sees no timer, which is the same thing they would see if the
/// host had left — an accepted residual this design already carries.
///
/// 60s rather than a few seconds because a late joiner also has to get through
/// connect + SESSION_ASSIGNED before its first heartbeat can arrive, and rather
/// than minutes because past a minute the information has no value.
pub const MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS: u64 = 60_000;

/// Trailing-edge debounce applied to host adjustments (wire contract rule 2b).
///
/// Three "+1 min" clicks in two seconds would otherwise be 3 transitions x 3
/// repeats + a heartbeat = 10 packets, over the relay's per-sender budget — and
/// the drops would land on the LAST burst, the one carrying the state that
/// actually matters. Coalescing to the SETTLED state loses nothing: every
/// intermediate value is one the host has already changed.
///
/// Sized against the budget rather than picked for feel: see
/// [`tests::worst_case_send_rate_stays_under_the_relay_budget`], which drives the
/// scheduler at its most expensive pattern and pins the result against
/// [`MEETING_TIMER_MAX_PER_WINDOW`] itself.
pub const MEETING_TIMER_DEBOUNCE_MS: u64 = 500;

/// The room-global timer state, as carried by `MeetingTimerPacket`.
///
/// `running` is an absolute LEVEL, never a toggle — re-sending the same value is
/// a harmless no-op for every consumer, which is what makes the heartbeat and
/// the transition repeats safe. A consumer must ASSIGN it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeetingTimerState {
    /// `true` = a timer is running and ends at `ends_at_ms`; `false` = no timer
    /// (never started, cancelled, or cleared).
    pub running: bool,
    /// Unix epoch ms at which the timer reaches zero. Host wall clock.
    pub ends_at_ms: u64,
    /// Total span the timer represents, INCLUDING any extension. Display only.
    pub duration_ms: u64,
    /// Unix epoch ms at which the host AUTHORED this state. Set once at the
    /// transition and preserved verbatim across every heartbeat and repeat.
    pub updated_at_ms: u64,
}

impl MeetingTimerState {
    /// The "no timer" state. Cancel is a transition to this, not its own message.
    pub fn cleared(updated_at_ms: u64) -> Self {
        Self {
            running: false,
            ends_at_ms: 0,
            duration_ms: 0,
            updated_at_ms,
        }
    }

    /// Build from an inbound packet, clamping `duration_ms` INDEPENDENTLY of the
    /// relay (the relay that validated this packet may be older or newer than the
    /// one this client reasons about — the same defense-in-depth REACTION applies
    /// to `custom_emoji`).
    pub fn from_packet(p: &MeetingTimerPacket) -> Self {
        Self {
            running: p.running,
            ends_at_ms: p.ends_at_ms,
            duration_ms: clamp_duration_ms(p.duration_ms),
            updated_at_ms: p.updated_at_ms,
        }
    }

    /// `started_at_ms = ends_at_ms - duration_ms`, SATURATING.
    ///
    /// A conforming relay rejects `ends_at_ms < duration_ms` at ingress so this
    /// cannot underflow from one — but a peer may be running an older relay, and
    /// an unguarded `u64` subtraction panics in a debug wasm build, which aborts
    /// the module and drops the whole call for that tab.
    pub fn started_at_ms(&self) -> u64 {
        self.ends_at_ms.saturating_sub(self.duration_ms)
    }
}

/// Clamp `duration_ms` to the wire-contract bound.
///
/// Deliberately CLAMPS rather than rejecting, unlike the relay: by the time a
/// packet reaches here it has already been accepted as the room's state, and
/// `duration_ms` is display-only (it drives a progress proportion, never the
/// countdown itself, which comes from `ends_at_ms` alone). Dropping the whole
/// state over a cosmetic field would discard a timer the room can otherwise
/// render correctly.
pub fn clamp_duration_ms(duration_ms: u64) -> u64 {
    duration_ms.min(MEETING_TIMER_MAX_DURATION_MS)
}

/// Last-writer-wins with a BOUNDED rejection window (wire contract rule 5).
///
/// Returns `true` when `incoming` should replace `applied`.
///
/// ```text
/// reject iff  0 < applied.updated_at_ms - incoming.updated_at_ms <= 5000
/// ```
///
/// FENCED AS `text` DELIBERATELY. Four-space indentation under `///` is a
/// Markdown indented code block, which rustdoc compiles AS RUST — so this line
/// of English was a failing doctest (`expected one of `!` or `::`, found `iff``)
/// that neither `cargo test --lib` nor `--tests` runs. Only
/// `cargo test --doc` catches it.
///
/// Read [`MEETING_TIMER_LWW_REJECT_WINDOW_MS`] before touching this: an
/// unbounded "older loses" rule wedges the feature under a backwards clock step.
pub fn should_apply(applied: Option<MeetingTimerState>, incoming: MeetingTimerState) -> bool {
    let Some(applied) = applied else {
        // Nothing applied yet — anything is newer than nothing. This is also the
        // late-joiner and post-reconnect path: the first heartbeat we see wins.
        return true;
    };
    let Some(older_by) = applied.updated_at_ms.checked_sub(incoming.updated_at_ms) else {
        // `incoming` is strictly newer (or the subtraction underflowed, which
        // means the same thing). Apply.
        return true;
    };
    if older_by == 0 {
        // Same authorship instant: a heartbeat or a repeat of the state we already
        // hold. Applying is a no-op by construction (the packet is a LEVEL), and
        // applying rather than rejecting keeps this comparator total — there is no
        // tie-break to get wrong.
        return true;
    }
    // Older by more than the window is a CLOCK STEP, not reordering. Apply it, or
    // the host loses control of the room's timer for as long as the step lasts.
    older_by > MEETING_TIMER_LWW_REJECT_WINDOW_MS
}

/// Milliseconds remaining, SATURATING at zero.
///
/// This underflows ROUTINELY and by design — every expired timer has
/// `now > ends_at_ms` — so the saturation is the normal path, not a guard against
/// something exotic.
pub fn remaining_ms(ends_at_ms: u64, now_ms: u64) -> u64 {
    ends_at_ms.saturating_sub(now_ms)
}

/// A countdown sampled ONCE against the wall clock and thereafter driven by a
/// MONOTONIC clock (wire contract rule 4).
///
/// The whole point is that host/viewer clock skew enters EXACTLY ONCE, at
/// receipt, instead of being re-applied on every tick. A countdown that
/// re-evaluated `Date.now()` each second would visibly jump or freeze when the
/// viewer's wall clock stepped (an NTP correction mid-meeting, a laptop waking);
/// this one cannot, because after construction it never reads the wall clock
/// again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CountdownSample {
    remaining_at_receipt_ms: u64,
    receipt_mono_ms: f64,
}

impl CountdownSample {
    /// Sample `ends_at_ms` against the wall clock once. `now_mono_ms` is a
    /// `performance.now()`-class reading whose ORIGIN is irrelevant — only
    /// differences against later readings from the same clock are used.
    pub fn sample(ends_at_ms: u64, now_wall_ms: u64, now_mono_ms: f64) -> Self {
        Self {
            remaining_at_receipt_ms: remaining_ms(ends_at_ms, now_wall_ms),
            receipt_mono_ms: now_mono_ms,
        }
    }

    /// Remaining milliseconds at monotonic instant `now_mono_ms`, clamped at 0.
    ///
    /// GARBAGE CLOCK READINGS FAIL SAFE, and "safe" here specifically means NOT
    /// EXPIRING: expiry is audible for every participant in the room, so a
    /// spurious one is far worse than a countdown that briefly stops advancing.
    ///
    /// Rust's `as` cast from `f64` to `u64` SATURATES rather than wrapping, which
    /// already handles two of the three bad inputs without a branch: a negative
    /// `elapsed` (the caller mixed clocks) and `NaN` both cast to `0`, so the
    /// countdown simply does not advance. Only `+INFINITY` is dangerous — it
    /// casts to `u64::MAX`, which saturates `remaining` to zero and would beep
    /// the whole room. That is the single case this guard exists for.
    ///
    /// (An earlier revision of this comment claimed a negative cast would "wrap
    /// into an enormous u64 and ADD time". That is C's behaviour, not Rust's
    /// since 1.45, and the redundant `elapsed <= 0.0` term it justified has been
    /// removed — mutation testing caught the term as unkillable, which is what
    /// surfaced the wrong claim.)
    pub fn remaining_at(&self, now_mono_ms: f64) -> u64 {
        let elapsed = now_mono_ms - self.receipt_mono_ms;
        if !elapsed.is_finite() {
            return self.remaining_at_receipt_ms;
        }
        self.remaining_at_receipt_ms.saturating_sub(elapsed as u64)
    }

    /// Whether the countdown has reached zero at `now_mono_ms`.
    pub fn is_expired_at(&self, now_mono_ms: f64) -> bool {
        self.remaining_at(now_mono_ms) == 0
    }
}

/// One packet the scheduler wants sent, or nothing.
pub type ScheduledSend = Option<MeetingTimerState>;

/// Host-side send cadence: debounce -> transition repeat burst -> heartbeat
/// (wire contract rules 1, 2 and 2b).
///
/// A pure state machine over an injected `now_ms`. The Dioxus layer polls it from
/// a single interval and sends whatever it returns; nothing here touches a clock,
/// a timer, or the network.
///
/// `ends_at_ms` and `updated_at_ms` are stamped by the CALLER at the transition
/// and are preserved VERBATIM by every path through this type. The scheduler
/// never re-stamps them, which is what makes the LWW comparator on the receiving
/// side meaningful — a re-stamped heartbeat would defeat it entirely.
#[derive(Debug, Default)]
pub struct MeetingTimerScheduler {
    /// The state the host has settled on, once the debounce window closes.
    current: Option<MeetingTimerState>,
    /// A requested state whose debounce window is still open, with the instant
    /// the window last (re)opened.
    pending: Option<(MeetingTimerState, u64)>,
    /// Repeats still owed for the current state's transition burst.
    repeats_left: u8,
    /// Earliest instant the next repeat may go out.
    next_repeat_at_ms: u64,
    /// Earliest instant the next heartbeat may go out. Only consulted while
    /// `current` is running.
    next_heartbeat_at_ms: u64,
}

impl MeetingTimerScheduler {
    pub fn new() -> Self {
        Self::default()
    }

    /// The state this scheduler currently believes is the room's, if any.
    /// `None` before the first transition settles.
    pub fn current(&self) -> Option<MeetingTimerState> {
        self.current
    }

    /// The state this client is DRIVING — the pending (still-debouncing)
    /// transition if there is one, otherwise the settled current state.
    ///
    /// This, not [`current`](Self::current), is what a caller asking "am I the one
    /// announcing this timer?" wants. `current` only advances when `poll` closes
    /// the debounce window, so for the ~500-750ms after a host clicks a preset for
    /// the meeting's FIRST timer it is still `None` while the UI is already
    /// rendering that timer's local echo. A reconnect landing in that window made
    /// the host look like a mere viewer and dropped its own echo — and because the
    /// relay self-skips the sender, nothing would ever bring it back. (Extends
    /// were unaffected: `current` still held the previous running state.)
    pub fn driving(&self) -> Option<MeetingTimerState> {
        self.pending.map(|(state, _)| state).or(self.current)
    }

    /// Abandon everything this scheduler is announcing, without sending anything.
    ///
    /// For the case where this client LOSES the right to drive the timer — a
    /// transfer-host away from us. The relay drops a demoted ex-host's packets, so
    /// continuing to poll would be pure waste; worse, a packet slipping through
    /// the mirror-update window carries a stamp OLDER than the new host's, which
    /// the bounded LWW rule reads as a clock step and APPLIES, letting a demoted
    /// host briefly overwrite the real one's timer.
    ///
    /// Deliberately silent: a cancel would be a state transition this client is no
    /// longer entitled to author, and the room's timer is not ours to stop.
    pub fn stop(&mut self) {
        *self = Self::new();
    }

    /// Record a host transition (start, extend, or cancel).
    ///
    /// This OPENS (or reopens) the debounce window and DISCARDS any repeat burst
    /// still owed for a previous state — that is the coalescing rule 2b asks for.
    /// Discarding is safe precisely because the packet is a LEVEL: the settled
    /// state is the only one anyone needs, and an interrupted burst was carrying a
    /// value the host has since changed.
    pub fn request(&mut self, state: MeetingTimerState, now_ms: u64) {
        self.pending = Some((state, now_ms));
        self.repeats_left = 0;
    }

    /// Poll for a packet to send. Returns at most ONE state per call; the caller
    /// is expected to poll on a short interval.
    pub fn poll(&mut self, now_ms: u64) -> ScheduledSend {
        // 1. Close the debounce window if it has elapsed, and arm the burst.
        if let Some((state, since)) = self.pending {
            if now_ms.saturating_sub(since) >= MEETING_TIMER_DEBOUNCE_MS {
                self.pending = None;
                self.current = Some(state);
                self.repeats_left = MEETING_TIMER_TRANSITION_REPEATS;
                self.next_repeat_at_ms = now_ms;
            } else {
                // Window still open. Deliberately fall through to the heartbeat
                // below rather than returning: an in-flight adjustment must not
                // suppress the heartbeat for the state the room currently holds.
            }
        }

        let current = self.current?;

        // 2. Transition repeats take priority over the heartbeat.
        if self.repeats_left > 0 && now_ms >= self.next_repeat_at_ms {
            self.repeats_left -= 1;
            self.next_repeat_at_ms = now_ms.saturating_add(MEETING_TIMER_REPEAT_SPACING_MS);
            // Any send defers the heartbeat: the room just heard this state, so a
            // heartbeat on its heels would be pure waste against the budget.
            self.next_heartbeat_at_ms = now_ms.saturating_add(MEETING_TIMER_HEARTBEAT_MS);
            return Some(current);
        }

        // 3. Heartbeat, but ONLY while running AND only until shortly past zero.
        //    A cancelled timer is repeated by the burst above and then goes quiet —
        //    there is nothing to keep alive, and heartbeating "no timer" forever
        //    would spend budget to say nothing.
        //
        //    The EXPIRY BOUND is the second half of that, and without it this was
        //    the only unbounded term in the feature: nothing clears `running` when
        //    a timer reaches zero, so a 5-minute timer nobody cancelled in a
        //    60-minute meeting kept re-announcing for 55 more minutes — ~600
        //    wasted sends, and at 20 participants ~11,400 wasted relay deliveries,
        //    all to tell a room something it has already been told and already
        //    heard. The grace window keeps the late-joiner guarantee intact for
        //    the period where "the timer just ran out" is still news.
        if current.running
            && now_ms >= self.next_heartbeat_at_ms
            && now_ms
                <= current
                    .ends_at_ms
                    .saturating_add(MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS)
        {
            self.next_heartbeat_at_ms = now_ms.saturating_add(MEETING_TIMER_HEARTBEAT_MS);
            return Some(current);
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The relay's own budget constants, imported HERE rather than at module level
    // so a non-test build carries no unused import. The point of reaching for the
    // real constants (instead of literals) is that lowering the relay's budget
    // must FAIL the cadence test rather than silently un-check the property.
    use videocall_types::limits::{MEETING_TIMER_MAX_PER_WINDOW, MEETING_TIMER_WINDOW_MS};

    fn st(running: bool, ends: u64, dur: u64, updated: u64) -> MeetingTimerState {
        MeetingTimerState {
            running,
            ends_at_ms: ends,
            duration_ms: dur,
            updated_at_ms: updated,
        }
    }

    // ---------------------------------------------------------------------
    // Rule 5 — last-writer-wins with a BOUNDED rejection window
    // ---------------------------------------------------------------------

    #[test]
    fn first_state_is_always_applied() {
        assert!(
            should_apply(None, st(true, 1_000, 500, 42)),
            "with nothing applied there is nothing to compare against; the first \
             heartbeat a late joiner or a reconnected client sees must win"
        );
    }

    #[test]
    fn strictly_newer_state_is_applied() {
        let applied = st(true, 1_000, 500, 10_000);
        assert!(should_apply(Some(applied), st(false, 0, 0, 10_001)));
    }

    #[test]
    fn state_older_by_less_than_the_window_is_rejected_as_reordering() {
        let applied = st(false, 0, 0, 20_000);
        // A START that lost the race to the CANCEL on the unordered datagram path.
        assert!(
            !should_apply(Some(applied), st(true, 99_999, 60_000, 19_000)),
            "a packet older by 1s is reordering on the QUIC datagram path and must \
             NOT resurrect a timer the host already cancelled"
        );
    }

    #[test]
    fn state_older_by_exactly_the_window_is_still_rejected() {
        let applied = st(false, 0, 0, 20_000);
        assert!(
            !should_apply(
                Some(applied),
                st(
                    true,
                    99_999,
                    60_000,
                    20_000 - MEETING_TIMER_LWW_REJECT_WINDOW_MS
                )
            ),
            "the window is inclusive at its upper edge (reject iff older_by <= 5000)"
        );
    }

    /// THE test for this comparator. A host wall clock that steps BACKWARDS must
    /// not be able to wedge the room's timer.
    #[test]
    fn state_older_than_the_window_is_applied_because_it_is_a_clock_step() {
        // Realistic epoch-ms stamps: an hour-long backwards step must stay positive.
        const NOW: u64 = 1_700_000_000_000;
        let applied = st(true, NOW + 600_000, 600_000, NOW);
        // Host clock jumps back an hour; every subsequent state it authors carries
        // a much older stamp.
        let after_step = st(false, 0, 0, NOW - 3_600_000);
        assert!(
            should_apply(Some(applied), after_step),
            "older by more than the window is a CLOCK STEP, not reordering. An \
             unbounded comparator would reject this CANCEL and every state after \
             it, leaving a timer nobody can stop counting down to an audible \
             expiry in every participant's browser (the #2122 fail-closed shape)"
        );
    }

    #[test]
    fn a_repeat_of_the_applied_state_is_applied_as_a_no_op() {
        let applied = st(true, 1_000, 500, 777);
        assert!(
            should_apply(Some(applied), applied),
            "an equal stamp is a heartbeat or repeat of what we hold; applying is a \
             no-op by construction and keeps the comparator total"
        );
    }

    // ---------------------------------------------------------------------
    // Saturating arithmetic
    // ---------------------------------------------------------------------

    #[test]
    fn remaining_saturates_instead_of_underflowing_for_an_expired_timer() {
        assert_eq!(
            remaining_ms(1_000, 5_000),
            0,
            "every expired timer has now > ends_at_ms, so this underflows on the \
             NORMAL path; an unguarded u64 subtraction panics in a debug wasm \
             build and aborts the module, dropping the whole call for that tab"
        );
    }

    #[test]
    fn remaining_is_exact_while_running() {
        assert_eq!(remaining_ms(5_000, 1_250), 3_750);
    }

    #[test]
    fn started_at_saturates_when_a_nonconforming_peer_sends_ends_before_duration() {
        // A conforming relay rejects this at ingress, but a peer may be on an older one.
        let s = st(true, 1_000, 60_000, 1);
        assert_eq!(
            s.started_at_ms(),
            0,
            "ends_at_ms < duration_ms must saturate, not underflow into ~1.8e19"
        );
    }

    #[test]
    fn started_at_is_exact_for_a_conforming_packet() {
        assert_eq!(st(true, 300_000, 120_000, 1).started_at_ms(), 180_000);
    }

    #[test]
    fn duration_is_clamped_independently_of_the_relay() {
        assert_eq!(
            clamp_duration_ms(MEETING_TIMER_MAX_DURATION_MS + 1),
            MEETING_TIMER_MAX_DURATION_MS,
            "a peer may be running an older or newer relay than the one this \
             client reasons about, so the client clamps for itself"
        );
        assert_eq!(clamp_duration_ms(60_000), 60_000);
    }

    #[test]
    fn from_packet_clamps_duration() {
        let p = MeetingTimerPacket {
            running: true,
            ends_at_ms: 10_000,
            duration_ms: u64::MAX,
            updated_at_ms: 5,
            ..Default::default()
        };
        assert_eq!(
            MeetingTimerState::from_packet(&p).duration_ms,
            MEETING_TIMER_MAX_DURATION_MS
        );
    }

    // ---------------------------------------------------------------------
    // Rule 4 — sample once, then count down monotonically
    // ---------------------------------------------------------------------

    #[test]
    fn countdown_counts_down_on_the_monotonic_clock() {
        let s = CountdownSample::sample(10_000, 4_000, 1_000.0);
        assert_eq!(s.remaining_at(1_000.0), 6_000);
        assert_eq!(s.remaining_at(3_500.0), 3_500);
        assert_eq!(s.remaining_at(7_000.0), 0);
    }

    /// Rule 4, stated as the two properties that actually distinguish this design
    /// from the naive one (re-evaluating `Date.now()` every tick):
    ///
    ///   a. skew enters EXACTLY ONCE — two viewers whose wall clocks differ by
    ///      `k` sample remainders differing by exactly `k`, and that offset is
    ///      constant thereafter rather than growing; and
    ///   b. after receipt the countdown advances by the MONOTONIC delta alone.
    ///
    /// Together these are what make a post-receipt wall-clock step (an NTP
    /// correction, a laptop waking) unable to jump or freeze a running countdown.
    #[test]
    fn countdown_samples_skew_once_and_then_advances_only_on_the_monotonic_clock() {
        // Same timer, same monotonic origin, two viewers 3s apart on the wall clock.
        let accurate = CountdownSample::sample(10_000, 4_000, 1_000.0);
        let skewed = CountdownSample::sample(10_000, 4_000 - 3_000, 1_000.0);

        let at_receipt = skewed.remaining_at(1_000.0) - accurate.remaining_at(1_000.0);
        assert_eq!(at_receipt, 3_000, "the skew must appear in full at receipt");

        // 4 seconds of monotonic time later the offset is UNCHANGED -- skew does
        // not accumulate, because neither sample reads the wall clock again.
        let later = skewed.remaining_at(5_000.0) - accurate.remaining_at(5_000.0);
        assert_eq!(
            later, at_receipt,
            "skew must be sampled ONCE, not re-applied every tick"
        );

        // ...and each viewer's own countdown advanced by exactly the monotonic delta.
        assert_eq!(
            accurate.remaining_at(1_000.0) - accurate.remaining_at(5_000.0),
            4_000
        );
    }

    #[test]
    fn countdown_clamps_at_zero_and_never_renders_negative() {
        let s = CountdownSample::sample(1_000, 0, 0.0);
        assert_eq!(s.remaining_at(999_999.0), 0);
        assert!(s.is_expired_at(999_999.0));
        assert!(!s.is_expired_at(500.0));
    }

    /// Every degenerate clock reading must FAIL SAFE, and here "safe" means NOT
    /// EXPIRING — expiry is audible for the whole room, so a spurious one is far
    /// worse than a countdown that briefly stops advancing.
    ///
    /// The three inputs are covered by two different mechanisms, and the test
    /// asserts both together because a reader of `remaining_at` needs the whole
    /// contract in one place:
    ///   * negative and NaN are safe by the SATURATING `f64 as u64` cast, with no
    ///     branch involved; and
    ///   * `+INFINITY` is safe only because of the explicit `is_finite` guard —
    ///     it would otherwise cast to `u64::MAX` and saturate `remaining` to 0.
    ///
    /// MUTATION PROOF: delete the `is_finite` guard and the infinity assertions
    /// go red. (The negative/NaN assertions are pinned by the language rather
    /// than by a branch of ours; they are kept as a behavioural pin, and this
    /// note records that no mutation of OUR code can kill them.)
    #[test]
    fn a_degenerate_monotonic_reading_never_triggers_a_spurious_expiry() {
        let s = CountdownSample::sample(10_000, 0, 5_000.0);
        // Negative delta: the caller mixed clocks. Must not ADD time.
        assert_eq!(s.remaining_at(4_000.0), 10_000);
        assert!(!s.is_expired_at(4_000.0));
        // NaN and +INFINITY: neither may expire the timer.
        assert_eq!(s.remaining_at(f64::NAN), 10_000);
        assert_eq!(s.remaining_at(f64::INFINITY), 10_000);
        assert!(!s.is_expired_at(f64::NAN));
        assert!(!s.is_expired_at(f64::INFINITY));
    }

    // ---------------------------------------------------------------------
    // Rules 1, 2, 2b — the send scheduler
    // ---------------------------------------------------------------------

    /// Drain every packet the scheduler wants across `[0, until_ms)`, polling at
    /// `step_ms`. Returns `(instant, state)` pairs.
    fn drain(
        s: &mut MeetingTimerScheduler,
        until_ms: u64,
        step_ms: u64,
    ) -> Vec<(u64, MeetingTimerState)> {
        let mut out = Vec::new();
        let mut t = 0;
        while t < until_ms {
            if let Some(state) = s.poll(t) {
                out.push((t, state));
            }
            t += step_ms;
        }
        out
    }

    #[test]
    fn nothing_is_sent_before_the_debounce_window_closes() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 60_000, 60_000, 100), 0);
        assert_eq!(s.poll(0), None);
        assert_eq!(s.poll(MEETING_TIMER_DEBOUNCE_MS - 1), None);
        assert!(
            s.poll(MEETING_TIMER_DEBOUNCE_MS).is_some(),
            "the burst arms exactly when the debounce window closes"
        );
    }

    #[test]
    fn a_transition_is_repeated_three_times_one_second_apart() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 60_000, 60_000, 100), 0);
        let sent = drain(&mut s, 4_000, 100);
        assert_eq!(
            sent.len(),
            MEETING_TIMER_TRANSITION_REPEATS as usize,
            "a transition must go out exactly {MEETING_TIMER_TRANSITION_REPEATS} times before the heartbeat takes over"
        );
        assert_eq!(sent[0].0, 500);
        assert_eq!(sent[1].0, 1_500);
        assert_eq!(sent[2].0, 2_500);
    }

    #[test]
    fn ends_at_and_updated_at_are_preserved_verbatim_across_every_repeat() {
        let mut s = MeetingTimerScheduler::new();
        let original = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        s.request(original, 0);
        let sent = drain(&mut s, 20_000, 100);
        assert!(sent.len() > MEETING_TIMER_TRANSITION_REPEATS as usize);
        for (at, state) in &sent {
            assert_eq!(
                state.ends_at_ms, original.ends_at_ms,
                "ends_at_ms was re-stamped on the send at t={at}; recomputing it on a \
                 re-send leaks a host clock step into a timer viewers already sampled"
            );
            assert_eq!(
                state.updated_at_ms, original.updated_at_ms,
                "updated_at_ms was re-stamped on the send at t={at}; that destroys the \
                 ordering guarantee the LWW comparator depends on"
            );
        }
    }

    #[test]
    fn a_running_timer_heartbeats_after_the_burst() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 600_000, 600_000, 100), 0);
        let sent = drain(&mut s, 20_000, 100);
        // burst at 500/1500/2500, then heartbeats every 5s from the last send.
        let heartbeats: Vec<u64> = sent
            .iter()
            .map(|(t, _)| *t)
            .filter(|t| *t > 2_500)
            .collect();
        assert_eq!(
            heartbeats,
            vec![7_500, 12_500, 17_500],
            "a running timer must re-announce every ~{MEETING_TIMER_HEARTBEAT_MS}ms — this is what \
             makes a late joiner and a reconnected client converge with no join-event plumbing"
        );
    }

    /// A cancel must be repeated and then go QUIET. Heartbeating "no timer"
    /// forever would spend the budget to say nothing.
    /// An EXPIRED timer must stop heartbeating. Without this bound the heartbeat
    /// is the only unbounded term in the feature: nothing clears `running` at
    /// zero, so a 5-minute timer nobody cancelled in a 60-minute meeting
    /// re-announces for 55 more minutes — hundreds of sends, thousands of relay
    /// deliveries at scale, all carrying news the room already has.
    ///
    /// MUTATION PROOF: delete the `now_ms <= ends_at + grace` term and the second
    /// assertion goes red.
    #[test]
    fn an_expired_timer_stops_heartbeating_after_the_grace_window() {
        let mut s = MeetingTimerScheduler::new();
        // Ends at t = 10_000; the run below covers well past the grace window.
        s.request(st(true, 10_000, 10_000, 1), 0);
        let sent = drain(
            &mut s,
            10_000 + MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS + 30_000,
            100,
        );

        let after_expiry: Vec<u64> = sent
            .iter()
            .map(|(t, _)| *t)
            .filter(|t| *t > 10_000)
            .collect();
        assert!(
            !after_expiry.is_empty(),
            "the grace window must still deliver the expired state -- 'the timer \
             just ran out' is news to someone who joins right after it does"
        );
        let last = *after_expiry.last().unwrap();
        assert!(
            last <= 10_000 + MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS,
            "last heartbeat was at {last}ms but the timer ended at 10000ms and the \
             grace window is {MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS}ms; an \
             unbounded heartbeat re-announces an expired timer for the rest of the \
             meeting"
        );
    }

    /// The grace window is not zero: a client that joins just after a timer runs
    /// out still learns about it, sees `0:00`, and gets the Expired treatment.
    #[test]
    fn an_expired_timer_still_heartbeats_inside_the_grace_window() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 10_000, 10_000, 1), 0);
        let sent = drain(
            &mut s,
            10_000 + MEETING_TIMER_EXPIRED_HEARTBEAT_GRACE_MS,
            100,
        );
        assert!(
            sent.iter().any(|(t, _)| *t > 10_000),
            "the expired state must still be announced inside the grace window"
        );
    }

    #[test]
    fn a_cancelled_timer_is_repeated_then_stops() {
        let mut s = MeetingTimerScheduler::new();
        s.request(MeetingTimerState::cleared(100), 0);
        let sent = drain(&mut s, 60_000, 100);
        assert_eq!(
            sent.len(),
            MEETING_TIMER_TRANSITION_REPEATS as usize,
            "a cancel gets the repeat burst (its only repair — there is no heartbeat \
             behind it) and then nothing at all"
        );
        assert!(sent.iter().all(|(_, s)| !s.running));
    }

    #[test]
    fn rapid_adjustments_are_coalesced_into_one_burst_for_the_settled_state() {
        let mut s = MeetingTimerScheduler::new();
        // Three "+1 min" clicks inside two seconds.
        s.request(st(true, 60_000, 60_000, 100), 0);
        assert_eq!(s.poll(100), None);
        s.request(st(true, 120_000, 120_000, 200), 200);
        assert_eq!(s.poll(300), None);
        s.request(st(true, 180_000, 180_000, 400), 400);

        let sent = drain(&mut s, 4_000, 100);
        assert_eq!(
            sent.len(),
            MEETING_TIMER_TRANSITION_REPEATS as usize,
            "three clicks in 400ms must produce ONE burst, not three (3 transitions \
             x 3 repeats + a heartbeat = 10 packets would exceed the relay budget, \
             and the drops would land on the last burst — the one that matters)"
        );
        for (_, state) in &sent {
            assert_eq!(
                state.ends_at_ms, 180_000,
                "the burst must carry the SETTLED state; every intermediate value is \
                 one the host has already changed"
            );
        }
    }

    /// Worst count of sends in any window of the relay's width.
    fn worst_window(sends: &[u64]) -> usize {
        sends
            .iter()
            .map(|&start| {
                sends
                    .iter()
                    .filter(|&&x| x >= start && x < start + MEETING_TIMER_WINDOW_MS)
                    .count()
            })
            .max()
            .unwrap_or(0)
    }

    /// The budget pin. Drives the scheduler at its most expensive legal pattern
    /// and asserts against [`MEETING_TIMER_MAX_PER_WINDOW`] ITSELF — not a copy —
    /// so lowering the relay's budget fails this test instead of silently
    /// un-checking the property.
    ///
    /// THE DRIVER SHAPE IS THE WHOLE TEST, and the obvious one is wrong. An
    /// earlier version re-`request`ed on a fixed cadence and polled at the SAME
    /// instant. `request` re-opens the debounce window at `now`, so the elapsed
    /// time any poll ever saw was 0, the window never closed, `current` stayed
    /// `None`, and `poll` returned `None` on every call — leaving `sends` empty,
    /// `worst_window` at 0 via `unwrap_or(0)`, and `assert!(0 < 8)` passing
    /// against a scheduler that did nothing. It survived making `poll` return
    /// `None` unconditionally, i.e. it pinned nothing at all.
    ///
    /// Two defences against that recurring, and both are load-bearing:
    ///   * the driver re-requests only AFTER a send lands, which is also the
    ///     genuinely most expensive pattern — every burst is restarted at the
    ///     first packet, so the debounce never gets to coalesce anything; and
    ///   * the assertions below require the run to have PRODUCED traffic and to
    ///     have produced overlapping traffic, so a scheduler that emits nothing
    ///     fails instead of trivially passing.
    #[test]
    fn worst_case_send_rate_stays_under_the_relay_budget() {
        let mut s = MeetingTimerScheduler::new();
        let mut sends: Vec<u64> = Vec::new();
        let mut stamp = 1u64;
        let mut re_request_at: Option<u64> = None;

        s.request(st(true, 600_000, 600_000, stamp), 0);

        let mut t = 0u64;
        while t < 60_000 {
            if re_request_at == Some(t) {
                stamp += 1;
                s.request(st(true, 600_000, 600_000, stamp), t);
                re_request_at = None;
            }
            if s.poll(t).is_some() {
                sends.push(t);
                // Re-request as soon as the host possibly could after seeing its
                // own transition go out. This restarts the burst at its most
                // expensive point and is what maximises the sustained rate.
                re_request_at = Some(t + 10);
            }
            t += 10;
        }

        assert!(
            !sends.is_empty(),
            "the driver produced NO sends, so this measured nothing — the previous \
             version of this test failed exactly this way and still passed"
        );
        let worst = worst_window(&sends);
        assert!(
            worst >= 2,
            "worst window was {worst}; the driver is supposed to force OVERLAPPING \
             sends, so anything below 2 means it is not exercising the budget at all"
        );
        assert!(
            worst < MEETING_TIMER_MAX_PER_WINDOW as usize,
            "worst-case client send rate was {worst} per {MEETING_TIMER_WINDOW_MS}ms but the relay \
             forwards only {MEETING_TIMER_MAX_PER_WINDOW}; exceeding it loses state the relay \
             cannot repair, and a dropped CANCEL leaves the room counting down to an \
             audible expiry the host already called off"
        );
    }

    /// The other adversarial shape: let each burst COMPLETE, then immediately
    /// start another. Cheaper than the interrupting driver above (the burst's own
    /// ~1s spacing throttles it), but it is the pattern a host clicking "+1 min"
    /// repeatedly actually produces, so it is worth pinning separately.
    #[test]
    fn a_repeated_extend_pattern_stays_under_the_relay_budget() {
        let mut s = MeetingTimerScheduler::new();
        let mut sends: Vec<u64> = Vec::new();
        let mut stamp = 1u64;
        let mut burst_sends = 0u8;

        s.request(st(true, 600_000, 600_000, stamp), 0);

        let mut t = 0u64;
        while t < 60_000 {
            if s.poll(t).is_some() {
                sends.push(t);
                burst_sends += 1;
                if burst_sends == MEETING_TIMER_TRANSITION_REPEATS {
                    burst_sends = 0;
                    stamp += 1;
                    s.request(st(true, 600_000, 600_000, stamp), t);
                }
            }
            t += 10;
        }

        assert!(!sends.is_empty(), "the driver produced NO sends");
        let worst = worst_window(&sends);
        assert!(
            worst >= 2,
            "worst window was {worst}; not exercising the budget"
        );
        assert!(
            worst < MEETING_TIMER_MAX_PER_WINDOW as usize,
            "repeated-extend send rate was {worst} per {MEETING_TIMER_WINDOW_MS}ms, over the \
             relay's {MEETING_TIMER_MAX_PER_WINDOW}"
        );
    }

    #[test]
    fn a_normal_start_then_heartbeat_is_far_under_budget() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 600_000, 600_000, 1), 0);
        let sent = drain(&mut s, 30_000, 100);
        let worst = sent
            .iter()
            .map(|&(start, _)| {
                sent.iter()
                    .filter(|(x, _)| *x >= start && *x < start + MEETING_TIMER_WINDOW_MS)
                    .count()
            })
            .max()
            .unwrap_or(0);
        assert!(
            worst <= 2,
            "the ordinary shape (one burst, then heartbeats) should sit at ~2 per window, got {worst}"
        );
    }

    /// While an adjustment is DEBOUNCING, the room still holds the previous state,
    /// so that state must keep heartbeating. If the open window suppressed the
    /// heartbeat, a host who opened the duration picker and hesitated would
    /// silently stop keeping the running timer alive, and any client that joined
    /// or reconnected during the hesitation would see no timer at all.
    #[test]
    fn a_heartbeat_for_the_held_state_still_fires_while_an_adjustment_debounces() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 600_000, 600_000, 1), 0);
        let _ = drain(&mut s, 3_000, 100); // burst at 500/1500/2500 -> heartbeat due 7500

        // Host begins adjusting at 7200; the debounce window is open across 7500.
        s.request(st(true, 900_000, 900_000, 2), 7_200);
        assert_eq!(
            s.current().map(|c| c.updated_at_ms),
            Some(1),
            "an un-settled adjustment must not become the current state yet"
        );

        let due = s.poll(7_500);
        assert_eq!(
            due.map(|c| c.updated_at_ms),
            Some(1),
            "the heartbeat that came due mid-window must go out for the state the \
             room actually holds, not be swallowed by the pending adjustment"
        );

        // Once the window closes the new state takes over and gets its own burst.
        let settled = s.poll(7_700);
        assert_eq!(settled.map(|c| c.ends_at_ms), Some(900_000));
    }

    /// A transition that is still DEBOUNCING must already count as "driving".
    ///
    /// MUTATION PROOF: make `driving()` return `self.current` and the first
    /// assertion goes red. That is the exact bug this method was added for — a
    /// host reconnecting inside the debounce window dropped its own local echo of
    /// the meeting's first timer, permanently, because the relay self-skips the
    /// sender and nothing re-establishes it.
    #[test]
    fn a_debouncing_transition_counts_as_driving_before_it_settles() {
        let mut s = MeetingTimerScheduler::new();
        let started = st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000);
        s.request(started, 0);
        assert_eq!(
            s.driving(),
            Some(started),
            "a request inside its debounce window is still this client's timer; \
             `current()` alone reports None here and would make the host look \
             like a viewer"
        );
        assert_eq!(s.current(), None, "control: `current` has NOT settled yet");

        // Once the window closes both agree.
        let _ = s.poll(MEETING_TIMER_DEBOUNCE_MS);
        assert_eq!(s.driving(), Some(started));
        assert_eq!(s.current(), Some(started));
    }

    /// A demoted ex-host must go QUIET, and silently.
    ///
    /// MUTATION PROOF: make `stop()` a no-op and the post-stop poll still returns
    /// a heartbeat -> red.
    #[test]
    fn stop_abandons_the_timer_without_announcing_anything() {
        let mut s = MeetingTimerScheduler::new();
        s.request(st(true, 1_700_000_300_000, 300_000, 1_700_000_000_000), 0);
        let _ = drain(&mut s, 3_000, 100);
        assert!(s.current().is_some(), "control: it was driving a timer");

        s.stop();
        assert_eq!(s.driving(), None);
        assert!(
            drain(&mut s, 60_000, 100).is_empty(),
            "a demoted ex-host must stop announcing entirely -- the relay drops its \
             packets, and any that slip through carry a stamp older than the new \
             host's, which the bounded LWW rule applies as a clock step"
        );
    }

    #[test]
    fn scheduler_has_nothing_to_send_before_any_transition() {
        let mut s = MeetingTimerScheduler::new();
        assert_eq!(s.poll(0), None);
        assert_eq!(s.poll(100_000), None);
        assert_eq!(s.current(), None);
    }
}
