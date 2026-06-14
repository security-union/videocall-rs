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

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

//
// EncoderState struct contains state variables that are common among the encoders, and the logic
// for working with them.
//

#[derive(Clone)]
pub struct EncoderState {
    pub(super) enabled: Arc<AtomicBool>,
    pub(super) selected: Option<String>,
    pub(super) switching: Arc<AtomicBool>,
}

impl EncoderState {
    pub fn new() -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(false)),
            selected: None,
            switching: Arc::new(AtomicBool::new(false)),
        }
    }

    // Sets the enabled bit to a given value, returning true if it was a change.
    pub fn set_enabled(&mut self, value: bool) -> bool {
        if value != self.enabled.load(Ordering::Acquire) {
            self.enabled.store(value, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn select(&mut self, device: String) -> bool {
        self.selected = Some(device);
        if self.is_enabled() {
            self.switching.store(true, Ordering::Release);
            true
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        self.enabled.store(false, Ordering::Release);
        self.switching.store(false, Ordering::Release);
    }
}

/// Pure PLI-coalescing decision shared by every publisher encoder (issue #1287
/// for camera, #1312/#1322 for screen). Host-testable single source of truth.
///
/// Returns `true` iff a PLI-driven forced keyframe is allowed to be emitted now,
/// given when the last keyframe was emitted (`last_keyframe_emit_ms`) and the
/// per-encoder cooldown window. The last keyframe may be periodic OR forced —
/// either one is broadcast to the whole room and satisfies all pending PLIs, so
/// both reset the window. `None` ⇒ no keyframe emitted yet ⇒ always allowed. The
/// `>=` makes the boundary inclusive: a PLI exactly `cooldown_ms` after the last
/// keyframe fires.
///
/// The cooldown VALUE differs per encoder (camera 250ms, screen 2000ms — screen
/// content tolerates more aggressive coalescing) but the *decision* is identical,
/// so it lives here once. Callers must PEEK the request flag (`load`, not `swap`)
/// and clear it only when a keyframe is actually emitted, so a PLI that lands
/// mid-cooldown stays pending and is honored the instant the window expires
/// rather than being dropped.
///
/// This is the ONLY place the comparison lives so a host unit test pins it
/// (mutating `>=`→`>`, inverting the comparison, or dropping the `None` guard all
/// make the test fail).
pub(super) fn pli_keyframe_allowed(
    now_ms: f64,
    last_keyframe_emit_ms: Option<f64>,
    cooldown_ms: f64,
) -> bool {
    match last_keyframe_emit_ms {
        Some(last) => now_ms - last >= cooldown_ms,
        None => true,
    }
}

/// Per-frame inputs to [`keyframe_tick_decision`]. All atomic reads/swaps are done
/// by the caller (the encode loop) — this struct carries the already-loaded values
/// so the decision itself is pure and host-testable.
#[derive(Clone, Copy, Debug)]
pub(super) struct KeyframeTickInput {
    /// `performance.now()` for this frame.
    pub now_ms: f64,
    /// `force_keyframe.load()` — a PLI is pending (a receiver requested recovery).
    /// The caller PEEKS this with `load` (not `swap`) so a request landing
    /// mid-cooldown stays pending until the window expires.
    pub pli_pending: bool,
    /// This frame falls on the periodic GOP boundary (`frame % interval == 0`).
    pub is_periodic: bool,
    /// The reconnect/re-election cooldown-reset edge was observed this frame
    /// (issue #1311). The caller `.swap(false)`-consumes the reset atom and passes
    /// the result here; when `true`, the stale pre-transition keyframe timestamp is
    /// cleared so the first post-transition PLI is not coalesced away.
    pub cooldown_reset: bool,
    /// When the last keyframe (periodic OR forced) was emitted; `None` until the
    /// first one goes out.
    pub last_keyframe_emit_ms: Option<f64>,
    /// Per-encoder PLI coalescing window (camera 250ms, screen 2000ms).
    pub cooldown_ms: f64,
}

/// The decision returned by [`keyframe_tick_decision`]. The encode loop applies the
/// side effects (set the encoder `key_frame` option, clear the `force_keyframe`
/// atom, write back `last_keyframe_emit_ms`, log on a forced emit).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct KeyframeTickDecision {
    /// Emit a keyframe this frame (periodic OR an allowed forced PLI). Fed to
    /// `VideoEncoderEncodeOptions::set_key_frame`.
    pub want_keyframe: bool,
    /// The keyframe is being forced by a PLI (vs. a periodic GOP keyframe). Drives
    /// the "forcing keyframe (PLI)" log line; the encode loop logs ONLY when this is
    /// `true` (issue #1347) — never on a held/coalesced PLI.
    pub pli_forced: bool,
    /// The `last_keyframe_emit_ms` clock AFTER this tick. Reflects both the #1311
    /// reset (cleared to `None` when `cooldown_reset`) and the emit (set to `now_ms`
    /// when `want_keyframe`). The caller MUST write this back into its loop-local
    /// `last_keyframe_emit_ms`.
    pub last_keyframe_emit_ms: Option<f64>,
    /// Clear the `force_keyframe` request atom (`store(false)`). True iff a keyframe
    /// is emitted this frame — ANY keyframe (periodic or forced) is broadcast to the
    /// whole room and satisfies every pending PLI. Clearing ONLY on emit is what lets
    /// a mid-cooldown request survive to be honored at window expiry (issue #1322).
    pub clear_force_keyframe: bool,
}

/// The single source of truth for the publisher-side per-frame keyframe decision,
/// shared by BOTH the camera and screen encode loops. Pure (no atomics, no clock):
/// the caller does the `swap`/`load`/`store` around it and passes the loaded values
/// in via [`KeyframeTickInput`].
///
/// Folds together every per-frame keyframe rule so a mutation to any of them breaks
/// the host tests that pin it:
///
/// 1. **#1311 cooldown reset** — when `cooldown_reset` is set (a reconnect or
///    re-election just happened), the stale pre-transition keyframe timestamp is
///    cleared FIRST, so the gate below sees `None` and the first post-transition PLI
///    emits immediately instead of being coalesced away.
/// 2. **#1287/#1312/#1322 PLI coalescer** — a forced keyframe is honored only when
///    `pli_pending` AND [`pli_keyframe_allowed`] (outside the cooldown window). A
///    pending PLI inside the window is NOT cleared here (`clear_force_keyframe` is
///    false), so it stays pending and fires the instant the window expires.
/// 3. **Periodic GOP** — an interval-boundary frame always emits, ungated by the
///    cooldown, and (like any keyframe) satisfies and clears a pending PLI.
///
/// `last_keyframe_emit_ms` in the returned decision is the post-tick clock the caller
/// must write back: cleared to `None` on a #1311 reset, set to `now_ms` on an emit.
pub(super) fn keyframe_tick_decision(input: KeyframeTickInput) -> KeyframeTickDecision {
    // 1. #1311: a reconnect/re-election cleared the cooldown clock so the first
    //    post-transition PLI is not coalesced away by a stale timestamp.
    let mut last_keyframe_emit_ms = if input.cooldown_reset {
        None
    } else {
        input.last_keyframe_emit_ms
    };

    // 2. #1287/#1312/#1322: a PLI is forced only when pending AND outside the
    //    cooldown window. PEEK semantics live in the caller (it `load`s, not
    //    `swap`s); the held-PLI survival comes from clearing the atom only on emit.
    let pli_forced = input.pli_pending
        && pli_keyframe_allowed(input.now_ms, last_keyframe_emit_ms, input.cooldown_ms);

    // 3. Periodic GOP keyframe is never gated by the cooldown.
    let want_keyframe = input.is_periodic || pli_forced;

    // ANY keyframe (re)starts the cooldown window and satisfies every pending PLI.
    if want_keyframe {
        last_keyframe_emit_ms = Some(input.now_ms);
    }

    KeyframeTickDecision {
        want_keyframe,
        pli_forced,
        last_keyframe_emit_ms,
        clear_force_keyframe: want_keyframe,
    }
}

#[cfg(test)]
mod tests {
    use super::{keyframe_tick_decision, pli_keyframe_allowed, KeyframeTickInput};

    /// Build a [`KeyframeTickInput`] with the cooldown-reset edge OFF (the common
    /// steady-state case). Tests that exercise #1311 set `cooldown_reset` explicitly.
    fn tick(
        now_ms: f64,
        pli_pending: bool,
        is_periodic: bool,
        last_keyframe_emit_ms: Option<f64>,
        cooldown_ms: f64,
    ) -> KeyframeTickInput {
        KeyframeTickInput {
            now_ms,
            pli_pending,
            is_periodic,
            cooldown_reset: false,
            last_keyframe_emit_ms,
            cooldown_ms,
        }
    }

    /// Pins the shared single source of truth for the publisher-side PLI emit
    /// coalescer used by BOTH camera and screen encoders.
    ///
    /// Mutations these assertions catch:
    ///  * dropping the `None` guard (the first forced keyframe would be blocked) — case 1
    ///  * inverting the comparison (`>=`→`<`) — every `Some` case flips and FAILS
    ///  * swapping `>=`→`>` — the exact-boundary case (249 suppress, 250 allow) flips
    #[test]
    fn pli_keyframe_allowed_pins_cooldown_boundary() {
        let cd = 250.0;

        // No keyframe emitted yet (None) → the first PLI is always allowed.
        assert!(
            pli_keyframe_allowed(0.0, None, cd),
            "the first forced keyframe (no prior keyframe) must always be allowed"
        );
        // 249ms after the last keyframe (< 250ms) → SUPPRESS (still in cooldown).
        assert!(
            !pli_keyframe_allowed(249.0, Some(0.0), cd),
            "a PLI 249ms after the last keyframe must be coalesced (suppressed)"
        );
        // Exactly 250ms → ALLOW (pins the inclusive `>=`; a `>` mutation fails here).
        assert!(
            pli_keyframe_allowed(250.0, Some(0.0), cd),
            "a PLI exactly one cooldown after the last keyframe must be allowed (>= is inclusive)"
        );
        // Well past the window → ALLOW.
        assert!(
            pli_keyframe_allowed(1_000.0, Some(250.0), cd),
            "a PLI long after the window must be allowed"
        );
    }

    /// Pins the per-frame keyframe decision that BOTH the camera and screen encode
    /// loops call (the production loops call `keyframe_tick_decision` directly, so a
    /// mutation to the real decision logic breaks this test). Pins, per frame:
    ///  * a periodic-GOP frame ALWAYS emits and is never gated by the cooldown
    ///    (dropping `is_periodic` from `want_keyframe` flips the first assertion);
    ///  * a periodic emit clears the request atom and (re)starts the cooldown clock;
    ///  * a pending PLI INSIDE the cooldown window is NOT emitted and NOT cleared
    ///    (so it stays pending — issue #1322; clearing it here would flip the
    ///    `clear_force_keyframe == false` assertion);
    ///  * a pending PLI at/after the window emits, is flagged `pli_forced` (drives
    ///    the #1347 emit-only log), clears the atom, and restarts the clock.
    #[test]
    fn keyframe_tick_decision_coalesces_and_holds_pli() {
        let cd = 2_000.0;

        // t=0: periodic GOP keyframe. Emits regardless of cooldown (no prior emit
        // here, but the periodic path is ungated either way) and is NOT a PLI force.
        let d = keyframe_tick_decision(tick(0.0, false, true, None, cd));
        assert!(d.want_keyframe, "periodic GOP frame must emit a keyframe");
        assert!(!d.pli_forced, "a periodic keyframe is not a PLI force");
        assert!(
            d.clear_force_keyframe,
            "any keyframe satisfies pending PLIs → clear the request atom"
        );
        assert_eq!(
            d.last_keyframe_emit_ms,
            Some(0.0),
            "an emit restarts the cooldown clock at now"
        );

        // t=500: a PLI is pending but we are 500ms into a 2000ms cooldown → HELD.
        let d = keyframe_tick_decision(tick(500.0, true, false, Some(0.0), cd));
        assert!(
            !d.want_keyframe,
            "a PLI inside the cooldown window must be coalesced (not emitted)"
        );
        assert!(!d.pli_forced, "a coalesced PLI is not a forced emit");
        assert!(
            !d.clear_force_keyframe,
            "issue #1322: a held PLI must stay pending — do NOT clear the request atom \
             mid-cooldown"
        );
        assert_eq!(
            d.last_keyframe_emit_ms,
            Some(0.0),
            "a non-emit must not touch the cooldown clock"
        );

        // t=2000: the window expires (>= cooldown). The held PLI fires.
        let d = keyframe_tick_decision(tick(2_000.0, true, false, Some(0.0), cd));
        assert!(d.want_keyframe, "a held PLI must fire at window expiry");
        assert!(
            d.pli_forced,
            "the window-expiry emit is PLI-forced (drives the #1347 emit-only log)"
        );
        assert!(
            d.clear_force_keyframe,
            "the forced emit clears the request atom"
        );
        assert_eq!(
            d.last_keyframe_emit_ms,
            Some(2_000.0),
            "the forced emit restarts the cooldown clock"
        );
    }

    /// Issue #1311: pins that the cooldown-reset edge (armed by a reconnect or
    /// re-election) un-gates the FIRST post-transition PLI inside the decision
    /// itself — the camera AND screen loops get this behavior from the single
    /// `keyframe_tick_decision` source of truth.
    ///
    /// The CONTROL arm (`cooldown_reset: false`) pins that the stale timestamp
    /// genuinely WOULD suppress, so the reset arm is a true behavioral difference,
    /// not a vacuous assertion. The RESET arm fails if the `cooldown_reset` →
    /// `last_keyframe_emit_ms = None` clear is removed from the decision (the
    /// mutation this test guards), AND the one-shot follow-up frame pins that the
    /// reset does not stick (the next mid-cooldown PLI is coalesced again).
    #[test]
    fn keyframe_tick_decision_reset_unblocks_first_post_reconnect_pli() {
        let cd = 2_000.0;
        let pre_emit = 1_000.0;
        // The first post-transition frame arrives only 33ms later — deep inside the
        // 2000ms window — with a PLI pending.
        let first_after = pre_emit + 33.0;

        // CONTROL: no reset. The stale timestamp suppresses the PLI (pins the window
        // is real, so the reset arm proves a genuine difference).
        let control = keyframe_tick_decision(tick(first_after, true, false, Some(pre_emit), cd));
        assert!(
            !control.want_keyframe,
            "control: a PLI 33ms after the last keyframe must be coalesced when no \
             reconnect reset is armed"
        );

        // RESET ARM: a reconnect/re-election armed the cooldown reset. The SAME PLI on
        // the SAME early frame now EMITS. Removing the `cooldown_reset` clear makes
        // this `want_keyframe` false and FAILS.
        let reset = keyframe_tick_decision(KeyframeTickInput {
            now_ms: first_after,
            pli_pending: true,
            is_periodic: false,
            cooldown_reset: true,
            last_keyframe_emit_ms: Some(pre_emit),
            cooldown_ms: cd,
        });
        assert!(
            reset.want_keyframe,
            "after a reconnect/re-election reset, the first PLI must emit immediately \
             even {}ms < cooldown ({}ms) since the last keyframe",
            first_after - pre_emit,
            cd
        );
        assert!(reset.pli_forced, "the un-gated emit is PLI-forced");
        assert_eq!(
            reset.last_keyframe_emit_ms,
            Some(first_after),
            "the emit restarts the cooldown clock at the post-reset emit time"
        );

        // One-shot: the reset is a per-frame edge (the caller `.swap(false)`-consumed
        // it), so the NEXT early frame — still inside the cooldown of the keyframe we
        // just emitted, reset NOT re-armed — is coalesced again. The reset does not
        // stick and disable the coalescer.
        let next = keyframe_tick_decision(tick(
            first_after + 33.0,
            true,
            false,
            reset.last_keyframe_emit_ms,
            cd,
        ));
        assert!(
            !next.want_keyframe,
            "after the one-shot reset is consumed, the coalescer resumes suppressing \
             PLIs inside the cooldown window"
        );
    }
}
