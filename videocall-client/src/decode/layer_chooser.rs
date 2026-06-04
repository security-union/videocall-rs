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

//! Receiver-driven per-peer simulcast layer chooser (issue #989, Phase 2).
//!
//! For each remote VIDEO source the local client decodes, this module decides
//! which simulcast layer THIS receiver's OWN downlink can sustain, and adapts it
//! continuously and independently of the sender. A congested receiver pulls a
//! lower layer for the peers it struggles with; a receiver with headroom climbs
//! higher. The decision is purely local: it never touches the sender's encoder
//! and never affects what other receivers get.
//!
//! ## Why this is a separate, pure module
//!
//! The decision logic is pure arithmetic over per-peer receive signals, so it
//! lives here free of `web_sys` / wasm so it can be host-unit-tested
//! exhaustively (the hazards of a flapping or runaway selector on a real-time
//! call are exactly what the project's Change Impact Policy warns about). The
//! wasm-only glue — reading live per-peer loss/PLI rates and sending the
//! resulting `LAYER_PREFERENCE` packet — lives in `peer_decode_manager.rs` and
//! `video_call_client.rs`, which drive this state machine.
//!
//! ## Signals (THIS receiver's downlink for THIS source)
//!
//! The receive path already tracks, per peer-stream, on a ~1s rolling window
//! (`peer_decode_manager::SequenceTracker`):
//!   * `loss_per_sec` — packets that shifted off the reorder window unseen.
//!     Direct evidence the downlink is dropping this source's video.
//!   * `kf_per_sec` — keyframe-requests (PLI) this receiver emitted for the
//!     source. A receiver that cannot keep up freezes and storms PLIs, so a
//!     sustained PLI rate is a strong "can't sustain this layer" signal.
//!
//! Both rise under THIS receiver's congestion regardless of the sender's state,
//! which is exactly the property the feature requires. Throughput/decode-keep-up
//! is captured implicitly: a layer the downlink cannot carry manifests as loss
//! and PLIs; sustained CLEAN windows are the headroom signal that licenses a
//! step up.
//!
//! ## Availability learning
//!
//! The relay does not advertise which layers a source produces, so availability
//! is learned empirically: [`LayerAvailability`] records the distinct
//! `simulcast_layer_id`s observed from a source within a rolling window. The
//! chooser never targets a layer above the highest observed-available one.
//!
//! ## Hysteresis (anti-flap)
//!
//! Mirroring the spirit of the sender AQ (responsive down, conservative up):
//!   * **Down** is fast — a single bad window over threshold steps down (drop
//!     immediately when loss/PLI spikes).
//!   * **Up** requires `STEP_UP_CLEAN_WINDOWS` consecutive clean windows AND a
//!     dwell of at least [`LAYER_STEP_UP_DWELL_MS`] since the last change, so a
//!     brief lull cannot bait an immediate re-climb into a layer the downlink
//!     just proved it cannot carry.
//!
//! ## P4 seam (user receive thresholds)
//!
//! [`LayerChooser::choose`] returns the *raw* desired layer the downlink can
//! sustain. P4 will clamp that into `[user_min, user_max]` at the call site
//! (see [`clamp_to_user_range`]) without changing this module's logic.

/// Consecutive clean (sub-threshold) windows required before a step UP.
///
/// Conservative on the way up: the downlink must prove sustained headroom, not
/// just one lucky window, before we ask for a costlier layer. Three ~1s windows
/// ≈ 3s of clean reception, comparable to the sender AQ's step-up stabilization.
pub const STEP_UP_CLEAN_WINDOWS: u32 = 3;

/// Minimum dwell (ms) at the current layer before a step UP is allowed.
///
/// Belt-and-suspenders with [`STEP_UP_CLEAN_WINDOWS`]: even if windows roll
/// fast, we will not climb again until this much wall-clock has elapsed since
/// the last layer change, preventing rapid oscillation on a marginal link.
pub const LAYER_STEP_UP_DWELL_MS: u64 = 3000;

/// Loss rate (lost packets/sec) at or above which the chooser steps DOWN.
///
/// Sustained loss means the downlink is dropping this source's video; a lower
/// layer is cheaper and more resilient. Tuned conservatively so ordinary jitter
/// (the reorder window already tolerates reordering) does not trigger a drop.
pub const LOSS_STEP_DOWN_PER_SEC: f64 = 5.0;

/// Loss rate below which a window counts as "clean" for step-up accounting.
///
/// Strictly below the step-down threshold to create a neutral band
/// `[LOSS_CLEAN_PER_SEC, LOSS_STEP_DOWN_PER_SEC)` where the chooser neither
/// climbs nor drops — the hysteresis dead-zone that prevents flapping right at
/// the boundary.
pub const LOSS_CLEAN_PER_SEC: f64 = 1.0;

/// Keyframe-request (PLI) rate (per sec) at or above which the chooser steps
/// DOWN. A receiver that cannot keep up freezes and storms PLIs; treat that as
/// downlink congestion for this source independent of actual sequence loss.
pub const PLI_STEP_DOWN_PER_SEC: f64 = 2.0;

/// PLI rate below which a window counts as "clean" for step-up accounting.
pub const PLI_CLEAN_PER_SEC: f64 = 0.5;

/// A single window's receive-health sample for one source (THIS receiver's
/// downlink), as produced by the receive-side sequence tracker on ~1s rollover.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DownlinkSample {
    /// Windowed packet-loss rate for this source (lost packets/sec).
    pub loss_per_sec: f64,
    /// Windowed keyframe-request (PLI) rate this receiver emitted (per sec).
    pub kf_per_sec: f64,
}

impl DownlinkSample {
    /// Over the step-DOWN threshold on either signal → the downlink cannot
    /// sustain the current layer.
    fn is_congested(&self) -> bool {
        self.loss_per_sec >= LOSS_STEP_DOWN_PER_SEC || self.kf_per_sec >= PLI_STEP_DOWN_PER_SEC
    }

    /// Under the CLEAN threshold on BOTH signals → this window contributes to
    /// the sustained-headroom evidence required for a step up.
    fn is_clean(&self) -> bool {
        self.loss_per_sec < LOSS_CLEAN_PER_SEC && self.kf_per_sec < PLI_CLEAN_PER_SEC
    }
}

/// Tracks which simulcast layers a source is currently producing, learned
/// empirically from observed `simulcast_layer_id`s (issue #989, Phase 2).
///
/// The relay does not advertise availability, so this is the only source of
/// truth for "which layers can I even ask for". Layers are observed within a
/// rolling window so that a source that stops emitting a top layer (its sender
/// AQ shed it, Phase 1) is eventually forgotten and we stop targeting it.
#[derive(Debug, Clone)]
pub struct LayerAvailability {
    /// Last-seen timestamp (ms) per observed layer id. A layer is "available"
    /// while its last observation is within [`Self::window_ms`].
    last_seen_ms: std::collections::HashMap<u32, u64>,
    /// How long (ms) an unobserved layer remains considered available.
    window_ms: u64,
}

impl LayerAvailability {
    /// Default availability window. Generous relative to the sender's frame
    /// cadence so a momentary gap (a few dropped frames, a keyframe-only lull)
    /// does not retract a layer, but short enough that a genuinely-shed top
    /// layer is forgotten within a few seconds.
    pub const DEFAULT_WINDOW_MS: u64 = 4000;

    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW_MS)
    }

    pub fn with_window(window_ms: u64) -> Self {
        Self {
            last_seen_ms: std::collections::HashMap::new(),
            window_ms,
        }
    }

    /// Record that a packet tagged `layer_id` arrived from this source at `now`.
    pub fn observe(&mut self, layer_id: u32, now_ms: u64) {
        self.last_seen_ms.insert(layer_id, now_ms);
    }

    /// Highest layer id observed within the window as of `now`. Returns 0 when
    /// nothing has been observed recently (base-only / un-upgraded publisher),
    /// which is the bandwidth-safe default. Expired entries are pruned lazily.
    pub fn highest_available(&mut self, now_ms: u64) -> u32 {
        let window = self.window_ms;
        self.last_seen_ms
            .retain(|_, &mut seen| now_ms.saturating_sub(seen) <= window);
        self.last_seen_ms.keys().copied().max().unwrap_or(0)
    }
}

impl Default for LayerAvailability {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-peer layer-selection state machine (issue #989, Phase 2).
///
/// Owns the current selected layer plus the hysteresis bookkeeping (consecutive
/// clean-window count and the timestamp of the last change). One instance per
/// remote source; they are fully independent so a struggling source does not
/// affect a healthy one.
#[derive(Debug, Clone)]
pub struct LayerChooser {
    /// Currently selected layer for this source (== the decode guard value and
    /// the layer requested from the relay).
    current: u32,
    /// Consecutive clean windows accumulated toward a step up.
    clean_windows: u32,
    /// Timestamp (ms) of the last layer change, for the step-up dwell guard.
    last_change_ms: u64,
}

impl LayerChooser {
    /// Construct a chooser starting at the **base layer (0)** — the
    /// bandwidth-safe default. A freshly-joined peer whose layers we have not
    /// yet learned therefore requests only the base layer, and climbs as the
    /// downlink proves capacity AND higher layers are observed available.
    pub fn new(now_ms: u64) -> Self {
        Self {
            current: 0,
            clean_windows: 0,
            last_change_ms: now_ms,
        }
    }

    /// The currently-selected layer (decode-guard value + relay request).
    pub fn current(&self) -> u32 {
        self.current
    }

    /// Fold one downlink window sample into the decision and return the new
    /// desired layer for this source.
    ///
    /// `highest_available` is the cap learned empirically by
    /// [`LayerAvailability`]; the chooser never targets above it (and clamps a
    /// previously-higher selection down when a top layer disappears).
    ///
    /// Behavior:
    ///   * **Down (fast):** a single congested window steps down one layer and
    ///     resets the clean-window counter (floored at base 0).
    ///   * **Up (conservative):** requires [`STEP_UP_CLEAN_WINDOWS`] consecutive
    ///     clean windows AND [`LAYER_STEP_UP_DWELL_MS`] dwell since the last
    ///     change, then climbs one layer toward `highest_available`.
    ///   * **Neutral band:** a window that is neither congested nor clean holds
    ///     the layer and resets the clean streak (no progress toward climbing,
    ///     but no drop either).
    pub fn choose(&mut self, sample: DownlinkSample, highest_available: u32, now_ms: u64) -> u32 {
        // Availability can only shrink our target: if the top layer we were on
        // is no longer being produced, drop to the highest still-available one
        // immediately (it is no longer decodable anyway).
        if self.current > highest_available {
            self.set_layer(highest_available, now_ms);
            return self.current;
        }

        if sample.is_congested() {
            // Responsive step-down: drop one layer now, reset the climb streak.
            if self.current > 0 {
                self.set_layer(self.current - 1, now_ms);
            }
            self.clean_windows = 0;
            return self.current;
        }

        if sample.is_clean() {
            self.clean_windows = self.clean_windows.saturating_add(1);
            let dwell_ok = now_ms.saturating_sub(self.last_change_ms) >= LAYER_STEP_UP_DWELL_MS;
            let streak_ok = self.clean_windows >= STEP_UP_CLEAN_WINDOWS;
            if dwell_ok && streak_ok && self.current < highest_available {
                self.set_layer(self.current + 1, now_ms);
                // Require a fresh streak before the NEXT climb so we ascend one
                // rung per sustained-headroom period, not all at once.
                self.clean_windows = 0;
            }
            return self.current;
        }

        // Neutral band (between clean and congested): hold, but the streak
        // breaks so we do not climb on intermittent marginal windows.
        self.clean_windows = 0;
        self.current
    }

    /// Apply a layer change and reset the dwell/clean bookkeeping.
    fn set_layer(&mut self, layer: u32, now_ms: u64) {
        if layer != self.current {
            self.current = layer;
            self.last_change_ms = now_ms;
        }
    }
}

/// Clamp a chooser's desired layer into a user-configured receive range
/// (issue #989, Phase 4 seam).
///
/// P2 calls this with the full `[0, u32::MAX]` range (a no-op). P4 will pass the
/// user's `[min, max]` so the automatic selection is bounded by an explicit
/// preference without changing the chooser's adaptation logic. Kept here, pure
/// and tested, so P4 is a one-line wiring change at the call site.
pub fn clamp_to_user_range(desired: u32, user_min: u32, user_max: u32) -> u32 {
    desired.clamp(user_min.min(user_max), user_max.max(user_min))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clean window (well under both clean thresholds).
    fn clean() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: 0.0,
        }
    }

    /// A congested window (over the loss step-down threshold).
    fn congested() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: LOSS_STEP_DOWN_PER_SEC + 1.0,
            kf_per_sec: 0.0,
        }
    }

    /// A neutral window (in the dead-zone: above clean, below step-down).
    fn neutral() -> DownlinkSample {
        DownlinkSample {
            loss_per_sec: (LOSS_CLEAN_PER_SEC + LOSS_STEP_DOWN_PER_SEC) / 2.0,
            kf_per_sec: 0.0,
        }
    }

    /// Drive `n` clean windows spaced `dt_ms` apart starting at `start_ms`,
    /// returning the final timestamp used.
    fn feed_clean(c: &mut LayerChooser, avail: u32, start_ms: u64, n: u32, dt_ms: u64) -> u64 {
        let mut t = start_ms;
        for _ in 0..n {
            c.choose(clean(), avail, t);
            t += dt_ms;
        }
        t
    }

    #[test]
    fn starts_at_base_layer() {
        let c = LayerChooser::new(0);
        assert_eq!(c.current(), 0);
    }

    #[test]
    fn sustained_good_downlink_climbs_to_top_available() {
        // 3 layers available (0,1,2). Sustained clean windows with adequate
        // dwell must climb all the way to the top, one rung at a time.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        // Each window 1100ms apart so dwell (3000ms) is satisfied after the
        // 3-clean-window streak that licenses each climb.
        let mut t = 1000u64;
        // Climb 0 -> 1: need 3 clean windows AND dwell since last change.
        for _ in 0..20 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2, "sustained headroom must reach top layer");
    }

    #[test]
    fn loss_spike_steps_down_fast() {
        // Climb to top, then a single congested window must drop immediately.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let mut t = 1000u64;
        for _ in 0..20 {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2);
        // One bad window → down one rung, right now (no dwell required).
        let after = c.choose(congested(), avail, t);
        assert_eq!(after, 1, "a single congested window must step down at once");
    }

    #[test]
    fn hysteresis_prevents_flap_on_neutral_windows() {
        // From base, climb to 1, then alternate neutral windows: neutral never
        // climbs (streak resets) and never drops, so the layer is stable.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let t = feed_clean(&mut c, avail, 1000, 4, 1100);
        assert_eq!(c.current(), 1, "should have climbed exactly one rung");
        let mut t = t;
        for _ in 0..10 {
            let l = c.choose(neutral(), avail, t);
            assert_eq!(l, 1, "neutral windows must hold the current layer");
            t += 1100;
        }
    }

    #[test]
    fn only_base_available_stays_base() {
        // Availability cap of 0 (base-only / un-upgraded publisher): no amount
        // of clean headroom may climb above base.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..20 {
            let l = c.choose(clean(), 0, t);
            assert_eq!(l, 0, "cannot climb above the only available layer");
            t += 1100;
        }
    }

    #[test]
    fn step_up_requires_sustained_headroom() {
        // Fewer than STEP_UP_CLEAN_WINDOWS clean windows must NOT climb.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let mut t = 1000u64;
        for _ in 0..(STEP_UP_CLEAN_WINDOWS - 1) {
            c.choose(clean(), avail, t);
            t += 1100;
        }
        assert_eq!(
            c.current(),
            0,
            "must not climb before the clean-window streak is met"
        );
    }

    #[test]
    fn step_up_requires_dwell_even_with_streak() {
        // Enough clean windows but bunched within the dwell period (small dt):
        // the dwell guard must still block the climb.
        let mut c = LayerChooser::new(1000);
        let avail = 2;
        // 5 clean windows only 100ms apart → streak satisfied but only 400ms
        // dwell elapsed, under LAYER_STEP_UP_DWELL_MS.
        let mut t = 1000u64;
        for _ in 0..5 {
            c.choose(clean(), avail, t);
            t += 100;
        }
        assert_eq!(
            c.current(),
            0,
            "dwell guard must block a climb even with a clean streak"
        );
    }

    #[test]
    fn availability_shrink_drops_selection_immediately() {
        // On the top layer, the source stops producing it (availability drops
        // to 1): the chooser must drop to the highest still-available layer at
        // once, regardless of downlink health.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..20 {
            c.choose(clean(), 2, t);
            t += 1100;
        }
        assert_eq!(c.current(), 2);
        let after = c.choose(clean(), 1, t);
        assert_eq!(after, 1, "must drop to highest available when top vanishes");
    }

    #[test]
    fn never_drops_below_base() {
        // Repeated congestion at base must floor at 0, never underflow.
        let mut c = LayerChooser::new(0);
        let mut t = 1000u64;
        for _ in 0..10 {
            let l = c.choose(congested(), 2, t);
            assert_eq!(l, 0, "base layer is the floor");
            t += 1100;
        }
    }

    #[test]
    fn pli_storm_steps_down_independent_of_loss() {
        // High PLI rate with zero sequence loss must still step down — a
        // receiver that cannot keep up freezes and storms PLIs.
        let mut c = LayerChooser::new(0);
        let avail = 2;
        let t = feed_clean(&mut c, avail, 1000, 20, 1100);
        assert_eq!(c.current(), 2);
        let pli_only = DownlinkSample {
            loss_per_sec: 0.0,
            kf_per_sec: PLI_STEP_DOWN_PER_SEC + 1.0,
        };
        assert_eq!(c.choose(pli_only, avail, t), 1, "PLI storm must step down");
    }

    #[test]
    fn per_peer_independence() {
        // Two choosers: one fed congestion, one fed clean headroom. They must
        // diverge — the struggling peer drops, the healthy peer climbs.
        let mut bad = LayerChooser::new(0);
        let mut good = LayerChooser::new(0);
        let avail = 2;
        // Prime both to the top via clean headroom.
        let mut t = feed_clean(&mut bad, avail, 1000, 20, 1100);
        t = feed_clean(&mut good, avail, 1000, 20, 1100).max(t);
        assert_eq!(bad.current(), 2);
        assert_eq!(good.current(), 2);
        // Now diverge: bad gets congestion, good stays clean.
        bad.choose(congested(), avail, t);
        good.choose(clean(), avail, t + 5000); // dwell satisfied, already at top
        assert_eq!(bad.current(), 1, "struggling peer drops");
        assert_eq!(good.current(), 2, "healthy peer holds the top");
    }

    #[test]
    fn availability_window_forgets_unseen_layers() {
        let mut a = LayerAvailability::with_window(1000);
        a.observe(0, 100);
        a.observe(1, 100);
        a.observe(2, 100);
        assert_eq!(a.highest_available(100), 2);
        // Re-observe only the base within the window; layers 1,2 expire.
        a.observe(0, 1200);
        assert_eq!(
            a.highest_available(1200),
            0,
            "unseen top layers must expire out of availability"
        );
    }

    #[test]
    fn availability_defaults_to_base_when_nothing_seen() {
        let mut a = LayerAvailability::new();
        assert_eq!(a.highest_available(0), 0);
    }

    #[test]
    fn clamp_to_user_range_is_noop_on_full_range() {
        assert_eq!(clamp_to_user_range(2, 0, u32::MAX), 2);
        assert_eq!(clamp_to_user_range(0, 0, u32::MAX), 0);
    }

    #[test]
    fn clamp_to_user_range_bounds_p4_preference() {
        // P4: clamp desired into [min, max].
        assert_eq!(clamp_to_user_range(2, 0, 1), 1, "clamped down to user max");
        assert_eq!(clamp_to_user_range(0, 1, 2), 1, "clamped up to user min");
        // Defensive: inverted bounds are normalized, never panic.
        assert_eq!(clamp_to_user_range(5, 2, 1), 2);
    }
}
