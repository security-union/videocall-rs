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
//! (see [`clamp_to_user_range`]) without changing this module's logic. The clamp
//! is per-(peer, [`PrefMediaKind`]), so a user can cap screen and camera
//! independently.

/// Media kind a layer preference / chooser applies to (issue #989, Phase 3).
///
/// Camera VIDEO, SCREEN-share, and AUDIO of the same peer are independent
/// streams, each with its own availability, downlink health, and chosen layer.
/// This enum keys the per-(peer, kind) chooser state on the receiver and the
/// per-(peer, kind) entry in the `LAYER_PREFERENCE` packet. The discriminants
/// match the wire `PacketWrapper.MediaKind` / proto `EntryMediaKind`
/// (VIDEO=1, AUDIO=2, SCREEN=3) so mapping to the wire is a trivial cast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PrefMediaKind {
    /// Camera video (`MediaKind::VIDEO` == 1).
    Video = 1,
    /// Microphone audio (`MediaKind::AUDIO` == 2).
    Audio = 2,
    /// Screen share (`MediaKind::SCREEN` == 3).
    Screen = 3,
}

impl PrefMediaKind {
    /// The wire discriminant for the proto `EntryMediaKind` / `MediaKind`.
    pub fn wire_value(self) -> i32 {
        self as i32
    }
}

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

/// User-configured RECEIVE-side layer bounds for ONE media kind (issue #989,
/// Phase 4).
///
/// ## Layer index convention (IMPORTANT for the UI author)
/// Bounds are **simulcast LAYER indices**, where **0 = base = LOWEST quality**
/// and a HIGHER index = HIGHER quality. This is the *opposite* of the 8-tier
/// SEND index convention (where tier 0 is the *best*). Per kind:
///   * video  — layers `0..=2` (low / standard / hd)
///   * screen — layers `0..=2` (low / medium / high)
///   * audio  — layers `0..=1` (low / high)
///
/// ## Semantics
/// `min`/`max` are inclusive bounds applied to EVERY incoming peer of this kind
/// ("never receive any peer's video below `min` or above `max`"). `None` means
/// "no bound" (open end). The default `(None, None)` is the full range → pure
/// auto-adaptation, no clamping. Out-of-order bounds (`min > max`) are normalized
/// by [`clamp_to_user_range`] (defensive; the UI should never send them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindLayerBounds {
    /// Inclusive minimum layer index, or `None` for "no lower bound" (0).
    pub min: Option<u32>,
    /// Inclusive maximum layer index, or `None` for "no upper bound".
    pub max: Option<u32>,
}

impl KindLayerBounds {
    /// `true` when no bound is set on either end → the chooser runs unclamped.
    pub fn is_open(&self) -> bool {
        self.min.is_none() && self.max.is_none()
    }

    /// Clamp a chooser's desired layer into these bounds. An absent `min`
    /// defaults to 0 (base); an absent `max` defaults to `u32::MAX` (open). When
    /// both are absent this is the identity (pure auto).
    pub fn clamp(&self, desired: u32) -> u32 {
        if self.is_open() {
            return desired;
        }
        clamp_to_user_range(desired, self.min.unwrap_or(0), self.max.unwrap_or(u32::MAX))
    }
}

/// All three per-kind receive-layer bounds (issue #989, Phase 4). Default is
/// fully open (no clamping on any kind). Stored on the client and applied to
/// each per-(peer, kind) chooser's desired layer at the monitor-tick call site.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiveLayerBounds {
    pub video: KindLayerBounds,
    pub screen: KindLayerBounds,
    pub audio: KindLayerBounds,
}

impl ReceiveLayerBounds {
    /// The bounds for a given media kind.
    pub fn for_kind(&self, kind: PrefMediaKind) -> KindLayerBounds {
        match kind {
            PrefMediaKind::Video => self.video,
            PrefMediaKind::Screen => self.screen,
            PrefMediaKind::Audio => self.audio,
        }
    }

    /// Set (or clear) the bounds for a given media kind.
    pub fn set_kind(&mut self, kind: PrefMediaKind, min: Option<u32>, max: Option<u32>) {
        let b = KindLayerBounds { min, max };
        match kind {
            PrefMediaKind::Video => self.video = b,
            PrefMediaKind::Screen => self.screen = b,
            PrefMediaKind::Audio => self.audio = b,
        }
    }
}

/// A real-time snapshot of the simulcast layer this receiver is CURRENTLY
/// decoding for one media kind, for the P5 quality needles (issue #989, Phase 4).
///
/// This reflects the **post-clamp** selected layer (what is actually decoded),
/// so it can never exceed the user's `max` bound — matching the needle's stated
/// expectation. `width`/`height` (and `kbps`) are resolved from the per-kind
/// layer ladder via [`received_layer_snapshot`]. `fps` is left `None` here
/// (the ladder's target fps is a publisher hint, not the received rate; the UI
/// already has received-fps elsewhere). Cheap to construct and poll per render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceivedLayerSnapshot {
    /// Which media kind this snapshot describes.
    pub kind: PrefMediaKind,
    /// The currently-decoded layer index (0 = base/lowest).
    pub layer_index: u32,
    /// Total layers available in this kind's ladder for `layer_count` layers
    /// (e.g. how many distinct layers the source ladder defines). Lets the UI
    /// render "layer 1 of 3".
    pub layer_count: u32,
    /// Resolution of the decoded layer in pixels (0 for audio).
    pub width: u32,
    pub height: u32,
    /// Approximate bitrate of the decoded layer in kbps, from the ladder.
    pub kbps: u32,
}

/// Audio simulcast bitrates (kbps) by layer, lowest-first (issue #989, Phase 3c
/// / 4). Mirrors the publisher's 2-layer model (low 24 / high 50). Kept here so
/// the snapshot resolver has no dependency on the encoder module.
const AUDIO_LAYER_KBPS: &[u32] = &[24, 50];

/// Number of simulcast layers the ladder defines for a media kind (issue #989):
/// video/screen = 3, audio = 2. Single source of truth for the per-kind ladder
/// size used by the snapshot resolver and the availability-id clamp.
pub fn max_layers_for_kind(kind: PrefMediaKind) -> u32 {
    match kind {
        PrefMediaKind::Video | PrefMediaKind::Screen => 3,
        PrefMediaKind::Audio => 2,
    }
}

/// Clamp a raw incoming `simulcast_layer_id` to the highest valid layer index
/// for `kind` (issue #989, security follow-up). The layer id rides OUTSIDE the
/// AEAD seal, so a malicious publisher could cycle arbitrary/unbounded ids; if
/// fed straight into [`LayerAvailability::observe`] each unique id would add a
/// distinct map entry, inflating availability cardinality between prunes.
/// Clamping to `[0, max_layers_for_kind - 1]` bounds the map to the ladder size
/// regardless of what arrives on the wire, with no effect on honest publishers
/// (whose ids are already in range).
pub fn clamp_observed_layer_id(kind: PrefMediaKind, raw_layer_id: u32) -> u32 {
    raw_layer_id.min(max_layers_for_kind(kind).saturating_sub(1))
}

/// Resolve a [`ReceivedLayerSnapshot`] for `kind` at the given decoded
/// `layer_index`, mapping the layer to its resolution/bitrate via the per-kind
/// ladder (issue #989, Phase 4). `layer_count` is the number of layers the
/// source ladder is producing (>= 1). Pure + panic-safe: `layer_index` and
/// `layer_count` are clamped into range, so the 1-layer (flag-off) default
/// always yields a valid layer-0 snapshot.
pub fn received_layer_snapshot(
    kind: PrefMediaKind,
    layer_index: u32,
    layer_count: u32,
) -> ReceivedLayerSnapshot {
    // Clamp the ladder size to the supported range for this kind, and the index
    // into [0, count-1], so a degenerate input can never panic the resolver.
    let max_layers = max_layers_for_kind(kind);
    let audio = matches!(kind, PrefMediaKind::Audio);
    let count = layer_count.clamp(1, max_layers);
    let idx = layer_index.min(count.saturating_sub(1));

    if audio {
        let kbps = AUDIO_LAYER_KBPS
            .get(idx as usize)
            .copied()
            .unwrap_or(AUDIO_LAYER_KBPS[0]);
        return ReceivedLayerSnapshot {
            kind,
            layer_index: idx,
            layer_count: count,
            width: 0,
            height: 0,
            kbps,
        };
    }

    // Video / screen: resolve from the AQ ladder (lowest-first, index == layer).
    let tiers = match kind {
        PrefMediaKind::Screen => {
            crate::adaptive_quality_constants::simulcast_screen_layers(count as usize)
        }
        _ => crate::adaptive_quality_constants::simulcast_layers(count as usize),
    };
    let tier = tiers
        .get(idx as usize)
        .or_else(|| tiers.first())
        .expect("ladder is non-empty for count >= 1");
    ReceivedLayerSnapshot {
        kind,
        layer_index: idx,
        layer_count: count,
        width: tier.max_width,
        height: tier.max_height,
        kbps: tier.ideal_bitrate_kbps,
    }
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

    // -----------------------------------------------------------------
    // Phase 4: KindLayerBounds / ReceiveLayerBounds
    // -----------------------------------------------------------------

    #[test]
    fn kind_bounds_default_is_open_and_identity() {
        let b = KindLayerBounds::default();
        assert!(b.is_open(), "default bounds are fully open");
        // Open bounds are the identity → pure auto, no clamping.
        for d in 0..=2 {
            assert_eq!(b.clamp(d), d);
        }
    }

    #[test]
    fn kind_bounds_max_clamps_down() {
        let b = KindLayerBounds {
            min: None,
            max: Some(1),
        };
        assert!(!b.is_open());
        assert_eq!(b.clamp(2), 1, "desired above max is clamped down");
        assert_eq!(b.clamp(1), 1);
        assert_eq!(b.clamp(0), 0, "below max is untouched");
    }

    #[test]
    fn kind_bounds_min_clamps_up() {
        let b = KindLayerBounds {
            min: Some(1),
            max: None,
        };
        assert_eq!(b.clamp(0), 1, "desired below min is clamped up");
        assert_eq!(b.clamp(2), 2);
    }

    #[test]
    fn kind_bounds_pin_to_single_layer() {
        // min == max pins every peer to exactly that layer.
        let b = KindLayerBounds {
            min: Some(1),
            max: Some(1),
        };
        assert_eq!(b.clamp(0), 1);
        assert_eq!(b.clamp(2), 1);
    }

    #[test]
    fn receive_bounds_per_kind_independent() {
        let mut rb = ReceiveLayerBounds::default();
        rb.set_kind(PrefMediaKind::Video, Some(0), Some(0)); // video pinned to base
        rb.set_kind(PrefMediaKind::Screen, None, Some(2)); // screen open up to 2
        assert_eq!(rb.for_kind(PrefMediaKind::Video).clamp(2), 0);
        assert_eq!(rb.for_kind(PrefMediaKind::Screen).clamp(2), 2);
        // Audio untouched → open.
        assert!(rb.for_kind(PrefMediaKind::Audio).is_open());
    }

    // -----------------------------------------------------------------
    // Phase 4: received_layer_snapshot layer→resolution mapping
    // -----------------------------------------------------------------

    #[test]
    fn snapshot_video_maps_layer_to_ladder_resolution() {
        // 3-layer video ladder, top layer (2) = 1280x720 hd.
        let s = received_layer_snapshot(PrefMediaKind::Video, 2, 3);
        assert_eq!(s.kind, PrefMediaKind::Video);
        assert_eq!(s.layer_index, 2);
        assert_eq!(s.layer_count, 3);
        assert_eq!((s.width, s.height), (1280, 720));
        assert!(s.kbps > 0);
        // Base layer (0) = lowest resolution.
        let base = received_layer_snapshot(PrefMediaKind::Video, 0, 3);
        assert_eq!((base.width, base.height), (640, 360));
        assert!(base.kbps < s.kbps, "base bitrate < top bitrate");
    }

    #[test]
    fn snapshot_screen_top_layer_is_1080p() {
        let s = received_layer_snapshot(PrefMediaKind::Screen, 2, 3);
        assert_eq!((s.width, s.height), (1920, 1080));
    }

    #[test]
    fn snapshot_audio_has_no_resolution_and_kbps_by_layer() {
        let low = received_layer_snapshot(PrefMediaKind::Audio, 0, 2);
        assert_eq!((low.width, low.height), (0, 0));
        assert_eq!(low.kbps, 24);
        let high = received_layer_snapshot(PrefMediaKind::Audio, 1, 2);
        assert_eq!(high.kbps, 50);
        assert_eq!(high.layer_count, 2);
    }

    #[test]
    fn snapshot_is_panic_safe_on_out_of_range() {
        // Degenerate inputs are clamped, never panic.
        let s = received_layer_snapshot(PrefMediaKind::Video, 99, 99);
        assert_eq!(s.layer_count, 3, "ladder size capped to 3 for video");
        assert_eq!(s.layer_index, 2, "index clamped to count-1");
        // Audio capped to 2.
        let a = received_layer_snapshot(PrefMediaKind::Audio, 99, 99);
        assert_eq!(a.layer_count, 2);
        assert_eq!(a.layer_index, 1);
    }

    #[test]
    fn snapshot_single_layer_default_is_base() {
        // 1-layer (flag-off) default: layer 0 / base, valid for every kind.
        for kind in [
            PrefMediaKind::Video,
            PrefMediaKind::Screen,
            PrefMediaKind::Audio,
        ] {
            let s = received_layer_snapshot(kind, 0, 1);
            assert_eq!(s.layer_index, 0);
            assert_eq!(s.layer_count, 1);
        }
    }

    // -----------------------------------------------------------------
    // Security follow-up: clamp_observed_layer_id bounds availability cardinality
    // -----------------------------------------------------------------

    #[test]
    fn max_layers_for_kind_matches_ladders() {
        assert_eq!(max_layers_for_kind(PrefMediaKind::Video), 3);
        assert_eq!(max_layers_for_kind(PrefMediaKind::Screen), 3);
        assert_eq!(max_layers_for_kind(PrefMediaKind::Audio), 2);
    }

    #[test]
    fn clamp_observed_layer_id_caps_to_ladder() {
        // In-range ids pass through; out-of-range ids clamp to the top index.
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 0), 0);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 2), 2);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, 3), 2);
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Video, u32::MAX), 2);
        // Audio caps at index 1.
        assert_eq!(clamp_observed_layer_id(PrefMediaKind::Audio, 5), 1);
    }

    #[test]
    fn clamped_observe_bounds_availability_cardinality() {
        // Simulate an attacker cycling many UNIQUE out-of-range layer ids: with
        // the clamp, availability can never hold more than the ladder size, and
        // highest_available never exceeds the top index — no inflation between
        // prunes. (Without the clamp this map would grow to ~1000 entries.)
        let mut avail = LayerAvailability::new();
        let now = 1_000u64;
        for raw in 0u32..1000 {
            let clamped = clamp_observed_layer_id(PrefMediaKind::Video, raw);
            avail.observe(clamped, now);
        }
        // highest_available also prunes; with all observations at `now` it is the
        // top ladder index, not some giant attacker value.
        assert_eq!(
            avail.highest_available(now),
            2,
            "clamped observe keeps availability within the 3-layer video ladder"
        );
    }
}
