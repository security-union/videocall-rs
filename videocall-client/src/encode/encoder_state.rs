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

#[cfg(test)]
mod tests {
    use super::pli_keyframe_allowed;

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
}
