/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Unified Performance settings panel — SEND quality bounds (#961) AND RECEIVE
//! layer bounds (#989 simulcast), with live "Sending" and "Receiving" needles.
//!
//! Now that the call supports per-receiver simulcast, the Performance panel
//! exposes **two** controls per media kind (Video, Audio, Screen/content):
//!
//! - **Send** — bounds the quality/tiers this peer PUBLISHES (save MY uplink +
//!   CPU). Wires to `CameraEncoder::set_quality_tier_bounds` /
//!   `ScreenEncoder::set_quality_tier_bounds` via the parent. The send side is
//!   the #961 feature.
//! - **Receive** — bounds the simulcast layers this peer PULLS from others
//!   (save MY downlink). Wires to `VideoCallClient::set_receive_layer_bounds`
//!   via the parent. The receive side is the simulcast P4 feature.
//!
//! # The two index conventions (cross-wiring these is a bug)
//!
//! - **SEND / AQ tiers:** index `0` is the BEST tier (1080p / 50 kbps), the
//!   highest index is the WORST. The send "Max quality" selection = best allowed
//!   = the *lower* index = backend `*_best`; "Min quality" = worst allowed = the
//!   *higher* index = backend `*_worst`. All send logic lives at the top level of
//!   this module (the `*` free functions, [`PerformancePreference`], etc.).
//! - **RECEIVE layers:** index `0` is the LOWEST quality, higher index = HIGHER
//!   quality (the natural left→right slider order). All receive logic lives in
//!   the [`receive`] submodule ([`receive::ReceivePreference`] etc.) so its
//!   `RangeSel`/`span_text`/`bounds_to_thumbs` cannot be confused with the
//!   send-side ones.
//!
//! Both conventions render with the SAME left→right = increasing-quality slider,
//! so the visual is consistent; only the index→bound mapping differs, and it is
//! kept strictly separate by the module boundary.
//!
//! # The needles
//!
//! Each kind shows a **Sending** needle (from [`SnapshotReader`], the live
//! encoder snapshot) and a **Receiving** needle (from
//! [`receive::ReceivedReader`], the live `received_layer_snapshot`). Two headless
//! rAF drivers ([`QualityVuMeterDriver`] for send, [`receive::ReceivedQualityDriver`]
//! for receive) poll at ~4 Hz and write each needle's rotation + readout straight
//! to the DOM by id (bypassing the Dioxus diff). Send and receive needles use
//! DISTINCT DOM ids so the two drivers never fight over the same node.

use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::{LiveQualitySnapshot, PrefMediaKind, ScreenQualitySnapshot};
use wasm_bindgen::JsCast;

// Re-export the receive-side public API so call sites can `use
// performance_settings::{ReceivePreference, KindReceivePref, ReceivedReader, ...}`.
// (`RECEIVE_PREF_KEY` stays available as `receive::RECEIVE_PREF_KEY`.)
pub use receive::{
    load_receive_preference, save_receive_preference, KindReceivePref, ReceivePreference,
    ReceivedReader,
};

/// A cloneable, `PartialEq`-able handle around the live-snapshot reader closure.
///
/// Dioxus component props must be `PartialEq` (for memoized diffing), but a
/// `dyn Fn` is not comparable, so we wrap it and compare by `Rc` pointer
/// identity: two readers are "equal" iff they are the same allocation. Callers
/// build one stable reader per `Host` mount, so this never spuriously
/// re-renders. `Clone` is cheap (an `Rc` bump).
#[derive(Clone)]
pub struct SnapshotReader(pub Rc<dyn Fn() -> Option<LiveQualitySnapshot>>);

impl SnapshotReader {
    /// A reader that always yields `None` (encoder unavailable / test default).
    pub fn none() -> Self {
        SnapshotReader(Rc::new(|| None))
    }

    fn read(&self) -> Option<LiveQualitySnapshot> {
        (self.0)()
    }
}

impl PartialEq for SnapshotReader {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// Screen-share counterpart of [`SnapshotReader`]. Reads
/// `ScreenEncoder::live_screen_snapshot()`, which is already `Option` (`None`
/// while not sharing), so the meter's empty-state path applies directly.
#[derive(Clone)]
pub struct ScreenSnapshotReader(pub Rc<dyn Fn() -> Option<ScreenQualitySnapshot>>);

impl ScreenSnapshotReader {
    /// A reader that always yields `None` (not sharing / test default).
    pub fn none() -> Self {
        ScreenSnapshotReader(Rc::new(|| None))
    }

    fn read(&self) -> Option<ScreenQualitySnapshot> {
        (self.0)()
    }
}

impl PartialEq for ScreenSnapshotReader {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

// ── localStorage key + persisted shape (SEND) ─────────────────────

/// `localStorage` key for the persisted send-quality preference. Follows the
/// `vc_`-prefixed convention used throughout `context.rs`.
pub const PERFORMANCE_PREF_KEY: &str = "vc_performance_quality";

/// Load the persisted send-quality preference, falling back to all-Auto on any
/// failure (missing key, corrupt JSON, storage unavailable) and sanitizing any
/// stale out-of-range index against the current tier ladders.
pub fn load_performance_preference() -> PerformancePreference {
    crate::local_storage::load_json::<PerformancePreference>(
        PERFORMANCE_PREF_KEY,
        PerformancePreference::default(),
    )
    .sanitized(
        VIDEO_TIER_LABELS.len(),
        AUDIO_TIER_LABELS.len(),
        SCREEN_TIER_LABELS.len(),
    )
}

/// Persist the send-quality preference. Silently no-ops on storage failure.
pub fn save_performance_preference(pref: &PerformancePreference) {
    crate::local_storage::save_json(PERFORMANCE_PREF_KEY, pref);
}

/// User-selected adaptive-quality (SEND) tier bounds, persisted to `localStorage`.
///
/// Each field stores a **tier index** (not a label) so the serialized form is
/// stable even if display labels change, and is robust to a stored index that no
/// longer maps to a valid tier (see [`PerformancePreference::sanitized`]).
///
/// - `*_max` = best allowed tier = the lower index = backend `*_best`.
/// - `*_min` = worst allowed tier = the higher index = backend `*_worst`.
/// - `None` = Auto (no bound on that end).
///
/// Each stream additionally carries an explicit `*_auto` flag (default `true`).
/// When a stream is on Auto, [`preference_to_encoder_bounds`] emits `None/None`
/// for that stream **regardless of the stored thumb indices** — so the encoder
/// runs fully automatic. The thumb indices are still persisted so toggling Auto
/// off restores the user's last manual range. Default = all-Auto (fully
/// automatic; behaviour unchanged unless the user opts in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PerformancePreference {
    /// Best allowed video tier index (Max quality). `None` = extreme/no cap.
    #[serde(default)]
    pub video_max: Option<usize>,
    /// Worst allowed video tier index (Min quality). `None` = extreme/no floor.
    #[serde(default)]
    pub video_min: Option<usize>,
    /// Best allowed audio tier index (Max quality).
    #[serde(default)]
    pub audio_max: Option<usize>,
    /// Worst allowed audio tier index (Min quality).
    #[serde(default)]
    pub audio_min: Option<usize>,
    /// Best allowed screen-share tier index (Max quality).
    /// `#[serde(default)]` so prefs persisted before screen support still load.
    #[serde(default)]
    pub screen_max: Option<usize>,
    /// Worst allowed screen-share tier index (Min quality).
    #[serde(default)]
    pub screen_min: Option<usize>,
    /// Video stream on Auto (full automatic). Default `true`; defaults to `true`
    /// for prefs persisted before the flag existed so they remain fully Auto.
    #[serde(default = "default_true")]
    pub video_auto: bool,
    /// Audio stream on Auto. Default `true`.
    #[serde(default = "default_true")]
    pub audio_auto: bool,
    /// Screen-share stream on Auto. Default `true`.
    #[serde(default = "default_true")]
    pub screen_auto: bool,
}

/// serde default for the `*_auto` flags (a fn because serde needs a path).
fn default_true() -> bool {
    true
}

impl Default for PerformancePreference {
    fn default() -> Self {
        PerformancePreference {
            video_max: None,
            video_min: None,
            audio_max: None,
            audio_min: None,
            screen_max: None,
            screen_min: None,
            video_auto: true,
            audio_auto: true,
            screen_auto: true,
        }
    }
}

// ── tier ladders (label ↔ index, SEND) ─────────────────────────────
//
// The labels are a fixed product decision (8 video tiers, 4 audio tiers) and
// intentionally hard-coded here rather than derived from the AQ tier tables:
// the panel shows resolution/bitrate *labels* for the user, while the backend
// consumes indices. Keeping the ladder local makes the mapping pure and
// host-testable (the AQ tables live behind a wasm-only crate). The order MUST
// match `VIDEO_QUALITY_TIERS` / `AUDIO_QUALITY_TIERS` (index 0 = best).

/// Video tier labels, index 0 = best (1080p) … index 7 = worst (240p).
pub const VIDEO_TIER_LABELS: [&str; 8] = [
    "1080p", "900p", "720p", "540p", "480p", "360p", "270p", "240p",
];

/// Audio tier labels, index 0 = best (50 kbps) … index 3 = worst (16 kbps).
pub const AUDIO_TIER_LABELS: [&str; 4] = ["50 kbps", "32 kbps", "24 kbps", "16 kbps"];

/// Screen-share tier labels, index 0 = best (1080p) … index 2 = worst (low).
/// Order MUST match `SCREEN_QUALITY_TIERS` (index 0 = best).
pub const SCREEN_TIER_LABELS: [&str; 3] = ["1080p", "720p", "low"];

// ── encoder bounds + inversion (SEND) ──────────────────────────────

/// The four backend arguments for `CameraEncoder::set_quality_tier_bounds`,
/// already inverted from the user-facing Max/Min selections.
///
/// `*_best` = floor on the index (best allowed), `*_worst` = cap on the index
/// (worst allowed). See module docs for the inversion rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncoderQualityBounds {
    pub video_best: Option<usize>,
    pub video_worst: Option<usize>,
    pub audio_best: Option<usize>,
    pub audio_worst: Option<usize>,
    /// Screen-share bounds. Pushed to the separate `ScreenEncoder` (not the
    /// camera), but carried here so the UI computes all bounds in one place.
    pub screen_best: Option<usize>,
    pub screen_worst: Option<usize>,
}

/// Convert a [`PerformancePreference`] into the encoder's best/worst index
/// arguments.
///
/// Because quality is the inverse of the index:
/// - `*_max` (Max quality / best) maps **directly** to `*_best`.
/// - `*_min` (Min quality / worst) maps **directly** to `*_worst`.
///
/// The naming flips (max→best, min→worst) but the index values pass through
/// unchanged — the inversion is entirely conceptual (lower index = better). The
/// backend itself swap-normalizes an inverted pair, so an accidental
/// best>worst is still safe.
///
/// **Auto override:** when a stream's `*_auto` flag is set, that stream emits
/// `None/None` (fully automatic) regardless of the stored thumb indices — the
/// indices are retained only so toggling Auto off restores the last manual
/// range.
pub fn preference_to_encoder_bounds(pref: &PerformancePreference) -> EncoderQualityBounds {
    // Auto on a stream forces both ends to None (no bound).
    let gate = |is_auto: bool, v: Option<usize>| if is_auto { None } else { v };
    EncoderQualityBounds {
        video_best: gate(pref.video_auto, pref.video_max),
        video_worst: gate(pref.video_auto, pref.video_min),
        audio_best: gate(pref.audio_auto, pref.audio_max),
        audio_worst: gate(pref.audio_auto, pref.audio_min),
        screen_best: gate(pref.screen_auto, pref.screen_max),
        screen_worst: gate(pref.screen_auto, pref.screen_min),
    }
}

// ── dual-thumb range slider model (SEND) ───────────────────────────
//
// The control is a dual-thumb slider where **left→right = increasing quality**.
// A slider "position" is `0..=tier_count-1`:
//   - position 0           = far-LEFT  = WORST tier  = the highest tier *index*.
//   - position tier_count-1= far-RIGHT = BEST tier   = tier index 0.
// So `tier_index = (tier_count-1) - position`. The two thumbs are:
//   - `min_pos` (LEFT thumb)  = the worst quality the call may drop to.
//   - `max_pos` (RIGHT thumb) = the best quality the call may rise to.
// with the invariant `min_pos <= max_pos` (thumbs can't cross).
//
// "Auto" on an end = that thumb sitting at its extreme:
//   - LEFT thumb at position 0           → no minimum bound (`*_worst = None`).
//   - RIGHT thumb at position tier_count-1 → no maximum bound (`*_best = None`).

/// One stream's dual-thumb slider state, in slider-position space (not tier
/// index). `min_pos` is the left thumb, `max_pos` the right thumb;
/// `min_pos <= max_pos` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSel {
    pub min_pos: usize,
    pub max_pos: usize,
}

/// Convert a slider position to its tier index for a ladder of `tier_count`
/// tiers. Left (pos 0) = worst = highest index; right = best = index 0.
pub fn position_to_tier_index(position: usize, tier_count: usize) -> usize {
    let max_idx = tier_count.saturating_sub(1);
    max_idx.saturating_sub(position.min(max_idx))
}

/// Inverse of [`position_to_tier_index`]: tier index → slider position.
pub fn tier_index_to_position(tier_index: usize, tier_count: usize) -> usize {
    let max_idx = tier_count.saturating_sub(1);
    max_idx.saturating_sub(tier_index.min(max_idx))
}

/// Derive a stream's slider thumbs from its persisted (best, worst) bounds.
///
/// `None` (Auto) on an end places that thumb at its extreme:
/// - `best = None`  → right thumb fully right (position `tier_count-1`).
/// - `worst = None` → left thumb fully left (position 0).
///
/// The result always satisfies `min_pos <= max_pos`.
pub fn bounds_to_thumbs(best: Option<usize>, worst: Option<usize>, tier_count: usize) -> RangeSel {
    let max_idx = tier_count.saturating_sub(1);
    // Right thumb (best quality). Auto = far right.
    let max_pos = match best {
        Some(idx) => tier_index_to_position(idx, tier_count),
        None => max_idx,
    };
    // Left thumb (worst quality). Auto = far left.
    let min_pos = match worst {
        Some(idx) => tier_index_to_position(idx, tier_count),
        None => 0,
    };
    // Enforce the no-cross invariant defensively.
    if min_pos > max_pos {
        RangeSel {
            min_pos: max_pos,
            max_pos,
        }
    } else {
        RangeSel { min_pos, max_pos }
    }
}

/// Derive a stream's (best, worst) bounds from its slider thumbs.
///
/// A thumb at its extreme means "Auto" for that end (`None`):
/// - right thumb at far right (`max_pos == tier_count-1`) → `best = None`.
/// - left thumb at far left (`min_pos == 0`)              → `worst = None`.
///
/// Otherwise the position maps back to a tier index. Returns `(best, worst)`.
pub fn thumbs_to_bounds(sel: RangeSel, tier_count: usize) -> (Option<usize>, Option<usize>) {
    let max_idx = tier_count.saturating_sub(1);
    let best = if sel.max_pos >= max_idx {
        None
    } else {
        Some(position_to_tier_index(sel.max_pos, tier_count))
    };
    let worst = if sel.min_pos == 0 {
        None
    } else {
        Some(position_to_tier_index(sel.min_pos, tier_count))
    };
    (best, worst)
}

/// Move the LEFT (min / worst) thumb to `new_min_pos`, never letting it pass the
/// right thumb. Returns the corrected [`RangeSel`]. Pure (no-cross guard).
pub fn set_min_thumb(sel: RangeSel, new_min_pos: usize) -> RangeSel {
    let min_pos = new_min_pos.min(sel.max_pos);
    RangeSel {
        min_pos,
        max_pos: sel.max_pos,
    }
}

/// Move the RIGHT (max / best) thumb to `new_max_pos`, never letting it pass the
/// left thumb. Returns the corrected [`RangeSel`].
pub fn set_max_thumb(sel: RangeSel, new_max_pos: usize) -> RangeSel {
    let max_pos = new_max_pos.max(sel.min_pos);
    RangeSel {
        min_pos: sel.min_pos,
        max_pos,
    }
}

/// The full-range (both extremes) selection for a ladder = fully Auto.
///
/// Test-only: production drives Auto via the explicit `*_auto` flag +
/// [`PerformancePreference::set_video_auto`] et al. rather than snapping thumbs,
/// but this is a handy fixture for the slider round-trip tests.
#[cfg(test)]
pub fn auto_thumbs(tier_count: usize) -> RangeSel {
    RangeSel {
        min_pos: 0,
        max_pos: tier_count.saturating_sub(1),
    }
}

impl PerformancePreference {
    /// Return a copy with any out-of-range index collapsed to `None` (Auto).
    ///
    /// Defends against a `localStorage` value written by a future/older build
    /// with a different number of tiers: a stored index that no longer maps to a
    /// valid tier silently falls back to Auto rather than producing an
    /// out-of-bounds bound. `video_len` / `audio_len` / `screen_len` are the
    /// current tier counts.
    pub fn sanitized(self, video_len: usize, audio_len: usize, screen_len: usize) -> Self {
        let clamp = |v: Option<usize>, len: usize| v.filter(|&i| i < len);
        PerformancePreference {
            video_max: clamp(self.video_max, video_len),
            video_min: clamp(self.video_min, video_len),
            audio_max: clamp(self.audio_max, audio_len),
            audio_min: clamp(self.audio_min, audio_len),
            screen_max: clamp(self.screen_max, screen_len),
            screen_min: clamp(self.screen_min, screen_len),
            // The auto flags carry no index, so they pass through unchanged.
            video_auto: self.video_auto,
            audio_auto: self.audio_auto,
            screen_auto: self.screen_auto,
        }
    }

    /// `true` when adaptation is pinned to a single tier ("Fixed" badge): the
    /// stream is NOT on Auto and both bounds are set to the same tier. A stream
    /// on Auto is never "fixed" (it is fully automatic).
    pub fn video_is_fixed(&self) -> bool {
        !self.video_auto && matches!((self.video_max, self.video_min), (Some(a), Some(b)) if a == b)
    }

    /// See [`Self::video_is_fixed`].
    pub fn audio_is_fixed(&self) -> bool {
        !self.audio_auto && matches!((self.audio_max, self.audio_min), (Some(a), Some(b)) if a == b)
    }

    /// See [`Self::video_is_fixed`].
    pub fn screen_is_fixed(&self) -> bool {
        !self.screen_auto
            && matches!((self.screen_max, self.screen_min), (Some(a), Some(b)) if a == b)
    }

    /// Toggle the video stream's Auto flag. Turning Auto ON snaps both thumbs to
    /// the extremes (bounds → `None/None`). Turning Auto OFF leaves the stored
    /// thumb indices (which are extremes/`None` until the user drags). Pure.
    pub fn set_video_auto(mut self, on: bool) -> Self {
        self.video_auto = on;
        if on {
            self.video_max = None;
            self.video_min = None;
        }
        self
    }

    /// See [`Self::set_video_auto`].
    pub fn set_audio_auto(mut self, on: bool) -> Self {
        self.audio_auto = on;
        if on {
            self.audio_max = None;
            self.audio_min = None;
        }
        self
    }

    /// See [`Self::set_video_auto`].
    pub fn set_screen_auto(mut self, on: bool) -> Self {
        self.screen_auto = on;
        if on {
            self.screen_max = None;
            self.screen_min = None;
        }
        self
    }

    /// Return a copy with the video stream's bounds replaced by those derived
    /// from `sel` (slider-position space). A thumb drag implies manual mode, so
    /// this also clears the Auto flag. Pure.
    pub fn with_video_thumbs(mut self, sel: RangeSel) -> Self {
        let (best, worst) = thumbs_to_bounds(sel, VIDEO_TIER_LABELS.len());
        self.video_max = best;
        self.video_min = worst;
        self.video_auto = false;
        self
    }

    /// Return a copy with the audio stream's bounds replaced by those derived
    /// from `sel`. Clears the Auto flag. Pure.
    pub fn with_audio_thumbs(mut self, sel: RangeSel) -> Self {
        let (best, worst) = thumbs_to_bounds(sel, AUDIO_TIER_LABELS.len());
        self.audio_max = best;
        self.audio_min = worst;
        self.audio_auto = false;
        self
    }

    /// Return a copy with the screen stream's bounds replaced by those derived
    /// from `sel`. Clears the Auto flag. Pure.
    pub fn with_screen_thumbs(mut self, sel: RangeSel) -> Self {
        let (best, worst) = thumbs_to_bounds(sel, SCREEN_TIER_LABELS.len());
        self.screen_max = best;
        self.screen_min = worst;
        self.screen_auto = false;
        self
    }
}

/// Concrete span text for the slider readout: always renders both thumb
/// positions as tier labels (e.g. `"240p – 1080p"` for the full ladder),
/// regardless of Auto state — it describes what the slider visibly shows, not
/// the encoder bound semantics. When both thumbs sit on the same tier it
/// collapses to a single label. Pure so it is host-tested.
pub fn span_text(sel: RangeSel, labels: &[&str]) -> String {
    let label_at = |pos: usize| position_label(pos, labels);
    let worst = label_at(sel.min_pos); // left thumb = worst end
    let best = label_at(sel.max_pos); // right thumb = best end
    if sel.min_pos == sel.max_pos {
        worst.to_string()
    } else {
        format!("{worst} – {best}")
    }
}

// ── VU meter needle math (SEND) ────────────────────────────────────

/// Sweep range of the analog needle, in degrees. The needle swings from
/// `-MAX_NEEDLE_DEG` (worst / left) to `+MAX_NEEDLE_DEG` (best / right) across
/// the tier ladder, like a classic cassette-deck VU meter.
pub const MAX_NEEDLE_DEG: f32 = 50.0;

/// Convert a tier index within a ladder of `tier_count` tiers into the needle
/// rotation angle in degrees.
///
/// Index 0 (best) → `+MAX_NEEDLE_DEG` (needle pegged right); the worst index →
/// `-MAX_NEEDLE_DEG` (needle pegged left). A single-tier ladder centers the
/// needle. Out-of-range indices are clamped, so this never produces NaN and is
/// safe to feed straight into a CSS transform.
///
/// Pure + host-tested.
pub fn tier_to_needle_deg(index: usize, tier_count: usize) -> f32 {
    if tier_count <= 1 {
        return 0.0;
    }
    let max_idx = tier_count - 1;
    let clamped = index.min(max_idx);
    // 0.0 at best (index 0) … 1.0 at worst (index max_idx).
    let frac = clamped as f32 / max_idx as f32;
    // Best → +MAX, worst → -MAX.
    MAX_NEEDLE_DEG - frac * (2.0 * MAX_NEEDLE_DEG)
}

/// Format the video readout line for the meter: `{w}x{h}·{fps}fps·{kbps}kbps`.
/// Pure so the readout text is host-tested.
pub fn format_video_readout(snap: &LiveQualitySnapshot) -> String {
    format!(
        "{}x{}·{}fps·{}kbps",
        snap.video_width, snap.video_height, snap.video_fps, snap.video_ideal_kbps
    )
}

/// Format the audio readout line for the meter: `{kbps} kbps`.
pub fn format_audio_readout(snap: &LiveQualitySnapshot) -> String {
    format!("{} kbps", snap.audio_kbps)
}

/// Format the screen readout line: `{w}x{h}·{fps}fps·{kbps}kbps` (same shape as
/// video). Pure.
pub fn format_screen_readout(snap: &ScreenQualitySnapshot) -> String {
    format!(
        "{}x{}·{}fps·{}kbps",
        snap.width, snap.height, snap.fps, snap.ideal_kbps
    )
}

/// Empty-state needle angle: rested at the left peg (worst end) so a camera-off
/// gauge reads as "no signal" rather than a frozen mid-scale value.
pub const EMPTY_NEEDLE_DEG: f32 = -MAX_NEEDLE_DEG;

/// Empty-state readout for the video gauge (camera off / no snapshot).
pub const VIDEO_EMPTY_READOUT: &str = "Camera off";
/// Empty-state readout for the audio gauge (no snapshot).
pub const AUDIO_EMPTY_READOUT: &str = "Idle";
/// Empty-state readout for the screen gauge (not sharing / no snapshot).
pub const SCREEN_EMPTY_READOUT: &str = "Not sharing";

/// All three SEND gauges' render state: needle angle + readout text. Pure so the
/// snapshot→gauge mapping (including the empty-state reset) is host-testable.
#[derive(Debug, Clone, PartialEq)]
pub struct GaugeState {
    pub video_deg: f32,
    pub audio_deg: f32,
    pub screen_deg: f32,
    pub video_text: String,
    pub audio_text: String,
    pub screen_text: String,
}

/// Map the optional live SEND snapshots to all three gauges' render state.
///
/// `Some` → live needle angles + numeric readouts. `None` on an input (encoder
/// unavailable — camera turned off, or screen not sharing) → that gauge's needle
/// resets to the left peg with a placeholder readout, so a stopped stream never
/// freezes on a stale reading. The video/audio gauges share one
/// `LiveQualitySnapshot`; the screen gauge has its own already-`Option`
/// `ScreenQualitySnapshot`. Single source of truth for both first paint and the
/// rAF loop.
pub fn gauge_state_from_snapshot(
    va: Option<&LiveQualitySnapshot>,
    screen: Option<&ScreenQualitySnapshot>,
) -> GaugeState {
    let (video_deg, audio_deg, video_text, audio_text) = match va {
        Some(s) => (
            tier_to_needle_deg(s.video_tier_index, VIDEO_TIER_LABELS.len()),
            tier_to_needle_deg(s.audio_tier_index, AUDIO_TIER_LABELS.len()),
            format_video_readout(s),
            format_audio_readout(s),
        ),
        None => (
            EMPTY_NEEDLE_DEG,
            EMPTY_NEEDLE_DEG,
            VIDEO_EMPTY_READOUT.to_string(),
            AUDIO_EMPTY_READOUT.to_string(),
        ),
    };
    let (screen_deg, screen_text) = match screen {
        Some(s) => (
            tier_to_needle_deg(s.tier_index, SCREEN_TIER_LABELS.len()),
            format_screen_readout(s),
        ),
        None => (EMPTY_NEEDLE_DEG, SCREEN_EMPTY_READOUT.to_string()),
    };
    GaugeState {
        video_deg,
        audio_deg,
        screen_deg,
        video_text,
        audio_text,
        screen_text,
    }
}

/// Move keyboard focus to the element with `id`, if present. Used to return
/// focus to the "?" button after its popover closes via Escape.
fn focus_element_by_id_local(id: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

// ── DOM helpers for the throttled needle update (shared) ───────────

/// Write `style.transform = rotate(<deg>deg)` to the needle element by id.
///
/// This is the per-frame DOM write that drives the analog needle without
/// triggering a Dioxus re-render (mirrors the pre-join mic meter's direct
/// `style.width` write). No-ops if the element is missing.
fn write_needle_rotation(needle_id: &str, deg: f32) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(needle_id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el
            .style()
            .set_property("transform", &format!("rotate({deg}deg)"));
    }
}

/// Write the live numeric readout text to a meter readout element by id.
fn write_readout_text(readout_id: &str, text: &str) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(readout_id))
    {
        el.set_text_content(Some(text));
    }
}

// ── public testids (also referenced by e2e) ───────────────────────
//
// SEND controls keep the original #961 testids so the existing
// performance-settings e2e selectors continue to resolve. RECEIVE controls use
// the `perf-recv-*` namespace (defined in the `receive` submodule) so the two
// sets never collide within one unified section.

/// Dual-thumb SEND range slider thumbs (the two overlaid `<input type="range">`).
pub const TESTID_VIDEO_RANGE_MIN: &str = "perf-video-range-min";
pub const TESTID_VIDEO_RANGE_MAX: &str = "perf-video-range-max";
pub const TESTID_AUDIO_RANGE_MIN: &str = "perf-audio-range-min";
pub const TESTID_AUDIO_RANGE_MAX: &str = "perf-audio-range-max";
pub const TESTID_SCREEN_RANGE_MIN: &str = "perf-screen-range-min";
pub const TESTID_SCREEN_RANGE_MAX: &str = "perf-screen-range-max";
/// Per-stream SEND "Auto" toggle buttons.
pub const TESTID_VIDEO_AUTO: &str = "perf-video-auto";
pub const TESTID_AUDIO_AUTO: &str = "perf-audio-auto";
pub const TESTID_SCREEN_AUTO: &str = "perf-screen-auto";
/// SEND ("Sending") needle gauges.
pub const TESTID_VU_VIDEO: &str = "perf-vu-video";
pub const TESTID_VU_AUDIO: &str = "perf-vu-audio";
pub const TESTID_VU_SCREEN: &str = "perf-vu-screen";

const VIDEO_NEEDLE_ID: &str = "perf-vu-video-needle";
const AUDIO_NEEDLE_ID: &str = "perf-vu-audio-needle";
const SCREEN_NEEDLE_ID: &str = "perf-vu-screen-needle";
const VIDEO_READOUT_ID: &str = "perf-vu-video-readout";
const AUDIO_READOUT_ID: &str = "perf-vu-audio-readout";
const SCREEN_READOUT_ID: &str = "perf-vu-screen-readout";

// ── components ────────────────────────────────────────────────────

/// A single analog VU needle gauge with a live numeric readout below it.
///
/// The arc + ticks are static SVG; only the `<line>` needle and the readout
/// text node are mutated at runtime (by the rAF drivers) via direct DOM writes,
/// so this component itself never re-renders per frame. Shared by both the
/// "Sending" and "Receiving" needles (distinct ids per instance).
#[component]
fn VuGauge(
    /// Stable testid / aria target for the gauge container.
    testid: &'static str,
    /// Id of the rotating needle `<line>` element.
    needle_id: &'static str,
    /// Id of the readout text element.
    readout_id: &'static str,
    /// Accessible label, e.g. "Sending video" / "Receiving video".
    label: &'static str,
    /// Initial needle rotation (degrees) for first paint before the loop ticks.
    initial_deg: f32,
    /// Initial readout text.
    initial_readout: String,
) -> Element {
    rsx! {
        div {
            class: "perf-vu-gauge",
            "data-testid": testid,
            "aria-label": label,
            svg {
                class: "perf-vu-svg",
                view_box: "0 0 120 78",
                width: "120",
                height: "78",
                "aria-hidden": "true",
                // Outer arc (the dial face).
                path {
                    class: "perf-vu-arc",
                    d: "M 14 64 A 50 50 0 0 1 106 64",
                    fill: "none",
                    stroke_width: "2",
                }
                // Tick marks across the sweep (drawn as short radial lines).
                for i in 0..7 {
                    {
                        // Evenly space 7 ticks from -50deg..+50deg.
                        let frac = i as f32 / 6.0;
                        let deg = -MAX_NEEDLE_DEG + frac * (2.0 * MAX_NEEDLE_DEG);
                        let rad = (deg - 90.0) * std::f32::consts::PI / 180.0;
                        let (cx, cy) = (60.0_f32, 64.0_f32);
                        let (r0, r1) = (44.0_f32, 50.0_f32);
                        let x1 = cx + r0 * rad.cos();
                        let y1 = cy + r0 * rad.sin();
                        let x2 = cx + r1 * rad.cos();
                        let y2 = cy + r1 * rad.sin();
                        rsx! {
                            line {
                                class: "perf-vu-tick",
                                x1: "{x1}", y1: "{y1}", x2: "{x2}", y2: "{y2}",
                                stroke_width: "1.5",
                            }
                        }
                    }
                }
                // Needle: pivots at (60,64), points up; rotated at runtime.
                line {
                    id: needle_id,
                    class: "perf-vu-needle",
                    x1: "60", y1: "64", x2: "60", y2: "20",
                    stroke_width: "2.5",
                    stroke_linecap: "round",
                    style: "transform: rotate({initial_deg}deg);",
                }
                // Needle hub.
                circle { class: "perf-vu-hub", cx: "60", cy: "64", r: "4" }
            }
            div { class: "perf-vu-label", "{label}" }
            // The readout is the accessible value: announced to screen readers
            // via aria-live when the quality changes. (The SVG gauge above is
            // decorative — aria-hidden — so the readout is the sole SR source.)
            div {
                id: readout_id,
                class: "perf-vu-readout",
                role: "status",
                "aria-live": "polite",
                "aria-label": "{label}",
                "{initial_readout}"
            }
        }
    }
}

/// Headless driver for the three SEND VU needles. Renders **nothing** — it only
/// owns the single ~4 Hz `requestAnimationFrame` polling loop that reads
/// `live_quality_snapshot()` / `live_screen_snapshot()` and writes the needle
/// rotations + readouts straight to the DOM nodes **by id** (so the gauges can
/// live anywhere in the tree, e.g. each inside its own threshold section).
///
/// Direct DOM writes mean no per-frame re-render. The loop self-cancels when the
/// driver unmounts (the `use_drop` clears the closure cell).
#[component]
fn QualityVuMeterDriver(
    /// Reads the current video/audio live snapshot. `None` → those gauges reset
    /// to the empty state ("Camera off" / "Idle").
    read_snapshot: SnapshotReader,
    /// Reads the current screen-share live snapshot. `None` (not sharing) → the
    /// screen gauge shows "Not sharing".
    read_screen_snapshot: ScreenSnapshotReader,
) -> Element {
    // Shared cell holds the rAF closure so it can reschedule itself, and so the
    // component's `use_drop` can drop it on unmount (stopping the loop).
    type RafCell = Rc<std::cell::RefCell<Option<wasm_bindgen::closure::Closure<dyn FnMut()>>>>;
    let cb: RafCell = use_hook(|| Rc::new(std::cell::RefCell::new(None)));

    // Start the throttled rAF loop once on mount.
    {
        let cb = cb.clone();
        let reader = read_snapshot.clone();
        let screen_reader = read_screen_snapshot.clone();
        use_hook(move || {
            let cb_clone = cb.clone();
            // Last-write throttle: only touch the DOM ~4x/sec.
            let last_ms = Rc::new(std::cell::Cell::new(0.0_f64));
            let closure = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
                let now = web_sys::window()
                    .and_then(|w| w.performance())
                    .map(|p| p.now())
                    .unwrap_or(0.0);
                if now - last_ms.get() >= 250.0 {
                    last_ms.set(now);
                    // Always write — including the `None`/stopped cases, which
                    // reset the needle to the left peg and show the placeholder
                    // readout. Without this branch a stream stopped *after* the
                    // panel opened would freeze the needle on a stale value.
                    let snap = reader.read();
                    let screen_snap = screen_reader.read();
                    let state = gauge_state_from_snapshot(snap.as_ref(), screen_snap.as_ref());
                    write_needle_rotation(VIDEO_NEEDLE_ID, state.video_deg);
                    write_needle_rotation(AUDIO_NEEDLE_ID, state.audio_deg);
                    write_needle_rotation(SCREEN_NEEDLE_ID, state.screen_deg);
                    write_readout_text(VIDEO_READOUT_ID, &state.video_text);
                    write_readout_text(AUDIO_READOUT_ID, &state.audio_text);
                    write_readout_text(SCREEN_READOUT_ID, &state.screen_text);
                }
                // Reschedule only while the cell still holds the closure (i.e. the
                // component is still mounted). `use_drop` clears it to stop us.
                if let (Some(win), Some(c)) = (web_sys::window(), cb_clone.borrow().as_ref()) {
                    let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
                }
            }) as Box<dyn FnMut()>);

            *cb.borrow_mut() = Some(closure);
            if let (Some(win), Some(c)) = (web_sys::window(), cb.borrow().as_ref()) {
                let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
            }
        });
    }

    // Stop the loop and drop the closure when the driver unmounts.
    {
        let cb = cb.clone();
        use_drop(move || {
            *cb.borrow_mut() = None;
        });
    }

    rsx! {}
}

/// Tier-index → aria-valuetext label for a slider thumb at `position`.
fn position_label<'a>(position: usize, labels: &[&'a str]) -> &'a str {
    let idx = position_to_tier_index(position, labels.len());
    labels.get(idx).copied().unwrap_or("?")
}

/// A discrete dual-thumb SEND-quality range slider for one stream.
///
/// Implemented as two overlaid native `<input type="range">` elements — this
/// keeps full keyboard operability (arrow keys step by one tier) and native
/// focus handling for free, while CSS stacks them into a single track. The
/// thumbs cannot cross: the left (min/worst) thumb is clamped to `<=` the right
/// (max/best) thumb on every change. Left→right is increasing quality, so the
/// rightmost stop is the best tier (index 0).
#[component]
fn DualRangeSlider(
    /// Stable id prefix, e.g. "perf-video" / "perf-audio".
    id_prefix: &'static str,
    /// testid for the min (left) thumb.
    min_testid: &'static str,
    /// testid for the max (right) thumb.
    max_testid: &'static str,
    /// Accessible noun for the stream, e.g. "video" / "audio".
    stream_noun: &'static str,
    /// Tier labels, index 0 = best … last = worst.
    labels: Vec<&'static str>,
    /// Current thumbs in slider-position space.
    sel: RangeSel,
    /// Called with the corrected [`RangeSel`] whenever a thumb moves.
    on_change: EventHandler<RangeSel>,
) -> Element {
    let max_pos = labels.len().saturating_sub(1);
    let min_value = sel.min_pos;
    let max_value = sel.max_pos;
    let min_id = format!("{id_prefix}-range-min");
    let max_id = format!("{id_prefix}-range-max");
    let min_valuetext = position_label(sel.min_pos, &labels).to_string();
    let max_valuetext = position_label(sel.max_pos, &labels).to_string();

    // Fill highlight between the thumbs (percent of track).
    let (fill_left, fill_right) = if max_pos == 0 {
        (0.0_f32, 100.0_f32)
    } else {
        (
            sel.min_pos as f32 / max_pos as f32 * 100.0,
            sel.max_pos as f32 / max_pos as f32 * 100.0,
        )
    };

    rsx! {
        div { class: "perf-range",
            // Worst-end (left) tier label.
            span { class: "perf-range-end-label", "{labels.last().copied().unwrap_or(\"\")}" }
            div { class: "perf-range-track-wrap",
                div { class: "perf-range-track",
                    div {
                        class: "perf-range-fill",
                        style: "left: {fill_left}%; right: {100.0 - fill_right}%;",
                    }
                }
                input {
                    id: "{min_id}",
                    class: "perf-range-input perf-range-input-min",
                    "data-testid": min_testid,
                    r#type: "range",
                    min: "0",
                    max: "{max_pos}",
                    step: "1",
                    value: "{min_value}",
                    "aria-label": "Minimum {stream_noun} send quality",
                    "aria-valuetext": "{min_valuetext}",
                    oninput: move |evt| {
                        if let Ok(p) = evt.value().parse::<usize>() {
                            on_change.call(set_min_thumb(sel, p));
                        }
                    },
                }
                input {
                    id: "{max_id}",
                    class: "perf-range-input perf-range-input-max",
                    "data-testid": max_testid,
                    r#type: "range",
                    min: "0",
                    max: "{max_pos}",
                    step: "1",
                    value: "{max_value}",
                    "aria-label": "Maximum {stream_noun} send quality",
                    "aria-valuetext": "{max_valuetext}",
                    oninput: move |evt| {
                        if let Ok(p) = evt.value().parse::<usize>() {
                            on_change.call(set_max_thumb(sel, p));
                        }
                    },
                }
            }
            // Best-end (right) tier label.
            span { class: "perf-range-end-label", "{labels.first().copied().unwrap_or(\"\")}" }
        }
    }
}

/// A self-contained "?" help popover button (shared by send + receive rows).
///
/// `open_help` is the shared single-open signal keyed by `key_id`. Opening one
/// closes any other (since they all share the signal).
#[component]
fn HelpPopover(
    /// Unique key for this popover within the shared `open_help` signal.
    key_id: &'static str,
    help_testid: &'static str,
    help_label: &'static str,
    help_body: &'static str,
    open_help: Signal<Option<&'static str>>,
) -> Element {
    let mut open_help = open_help;
    let help_open = open_help() == Some(key_id);
    let help_popover_id = format!("{key_id}-help-popover");
    let help_btn_id = format!("{key_id}-help-btn");

    rsx! {
        div { class: "perf-help",
            button {
                id: "{help_btn_id}",
                r#type: "button",
                class: "perf-help-button",
                "data-testid": help_testid,
                "aria-label": help_label,
                "aria-haspopup": "dialog",
                "aria-expanded": if help_open { "true" } else { "false" },
                "aria-controls": "{help_popover_id}",
                onclick: move |e: MouseEvent| {
                    e.stop_propagation();
                    if help_open {
                        open_help.set(None);
                    } else {
                        open_help.set(Some(key_id));
                    }
                },
                onkeydown: move |evt: KeyboardEvent| {
                    if evt.key() == Key::Escape && help_open {
                        evt.stop_propagation();
                        open_help.set(None);
                    }
                },
                "?"
            }
            if help_open {
                // Transparent full-viewport scrim: any click outside the popover
                // closes it (touch-friendly outside-click).
                div {
                    class: "perf-help-scrim",
                    "aria-hidden": "true",
                    onclick: move |e: MouseEvent| {
                        e.stop_propagation();
                        open_help.set(None);
                    },
                }
                div {
                    id: "{help_popover_id}",
                    class: "perf-help-popover",
                    role: "dialog",
                    "aria-label": help_label,
                    onclick: move |e: MouseEvent| e.stop_propagation(),
                    onkeydown: {
                        let help_btn_id = help_btn_id.clone();
                        move |evt: KeyboardEvent| {
                            if evt.key() == Key::Escape {
                                evt.stop_propagation();
                                open_help.set(None);
                                focus_element_by_id_local(&help_btn_id);
                            }
                        }
                    },
                    p { class: "perf-help-popover-text", "{help_body}" }
                }
            }
        }
    }
}

/// One stream kind's unified section: a header (kind title) plus a **Receive**
/// row and a **Send** row, each with its own needle gauge, dual-thumb slider,
/// Auto toggle, Fixed badge, "?" help popover and live range text.
///
/// Receive uses the natural index convention (0 = lowest) and bounds the layers
/// pulled from peers; Send uses the inverted tier convention (0 = best) and
/// bounds what this peer publishes. They are wired to independent callbacks and
/// distinct testids / needle ids so they never cross.
#[allow(clippy::too_many_arguments)]
#[component]
fn KindSection(
    kind: PrefMediaKind,
    title: &'static str,
    stream_noun: &'static str,
    /// Send id prefix, e.g. "perf-video".
    send_id_prefix: &'static str,
    send_min_testid: &'static str,
    send_max_testid: &'static str,
    send_auto_testid: &'static str,
    send_fixed_testid: &'static str,
    send_help_testid: &'static str,
    send_vu_testid: &'static str,
    send_vu_needle_id: &'static str,
    send_vu_readout_id: &'static str,
    send_vu_label: &'static str,
    send_vu_initial_deg: f32,
    send_vu_initial_readout: String,
    send_labels: Vec<&'static str>,
    send_best: Option<usize>,
    send_worst: Option<usize>,
    send_is_fixed: bool,
    send_is_auto: bool,
    /// Receive needle first-paint values + the kind's persisted receive sub-pref.
    recv_vu_initial_deg: f32,
    recv_vu_initial_readout: String,
    recv_sub: KindReceivePref,
    /// Shared single-open help signal (opening any popover closes the others).
    open_help: Signal<Option<&'static str>>,
    on_send_change: EventHandler<RangeSel>,
    on_send_auto_toggle: EventHandler<bool>,
    on_recv_change: EventHandler<KindReceivePref>,
) -> Element {
    let send_sel = bounds_to_thumbs(send_best, send_worst, send_labels.len());
    let send_range_str = span_text(send_sel, &send_labels);
    let send_auto_class = if send_is_auto {
        "perf-auto-button is-active"
    } else {
        "perf-auto-button"
    };

    rsx! {
        div { class: "perf-kind-group",
            div { class: "perf-kind-header",
                span { class: "perf-kind-title", "{title}" }
            }

            // ── RECEIVE row (save MY downlink) ──
            receive::ReceiveRow {
                kind,
                stream_noun,
                vu_initial_deg: recv_vu_initial_deg,
                vu_initial_readout: recv_vu_initial_readout,
                sub: recv_sub,
                open_help,
                on_change: move |sub: KindReceivePref| on_recv_change.call(sub),
            }

            // ── SEND row (save MY uplink / CPU) ──
            div { class: "perf-stream-group perf-send-row",
                div { class: "perf-stream-header",
                    span { class: "perf-stream-title", "Send" }
                    HelpPopover {
                        key_id: send_id_prefix,
                        help_testid: send_help_testid,
                        help_label: send_vu_label,
                        help_body: "Sets the best (right) and worst (left) quality this device PUBLISHES, to save your upload bandwidth and CPU. The app adapts within these limits based on your network. Auto uses the full range.",
                        open_help,
                    }
                    if send_is_fixed {
                        span {
                            class: "perf-fixed-badge",
                            "data-testid": send_fixed_testid,
                            title: "Send quality is pinned to a single tier — this stream won't adapt",
                            "aria-label": "{stream_noun} send quality pinned to a single tier — adaptation disabled",
                            "Fixed"
                        }
                    }
                    button {
                        r#type: "button",
                        class: send_auto_class,
                        "data-testid": send_auto_testid,
                        "aria-pressed": if send_is_auto { "true" } else { "false" },
                        "aria-label": "Automatic {stream_noun} send quality",
                        title: if send_is_auto {
                            "Automatic (full range) — click to set manual send limits"
                        } else {
                            "Manual limits — click for fully automatic send quality"
                        },
                        onclick: move |_| on_send_auto_toggle.call(!send_is_auto),
                        "Auto"
                    }
                }
                div { class: "perf-stream-body",
                    VuGauge {
                        testid: send_vu_testid,
                        needle_id: send_vu_needle_id,
                        readout_id: send_vu_readout_id,
                        label: send_vu_label,
                        initial_deg: send_vu_initial_deg,
                        initial_readout: send_vu_initial_readout,
                    }
                    div { class: "perf-stream-controls",
                        DualRangeSlider {
                            id_prefix: send_id_prefix,
                            min_testid: send_min_testid,
                            max_testid: send_max_testid,
                            stream_noun,
                            labels: send_labels.clone(),
                            sel: send_sel,
                            on_change: move |s: RangeSel| on_send_change.call(s),
                        }
                        p {
                            class: "perf-range-value",
                            "data-testid": "{send_id_prefix}-range-value",
                            "aria-live": "polite",
                            "Sending: {send_range_str}"
                        }
                    }
                }
            }
        }
    }
}

/// The unified Performance settings panel body: per kind (Video, Audio,
/// Screen/content) a Receive control + a Send control, each with its own live
/// needle. Two headless rAF drivers update the "Sending" and "Receiving"
/// needles independently.
///
/// `pref` (send) + `receive_pref` are the current persisted preferences
/// (controlled by the parent). On any change the panel derives the new bounds
/// and calls the matching callback; the parent persists it and pushes it to the
/// encoder (send) or client (receive). The panel is otherwise stateless.
#[component]
pub fn PerformanceSettingsPanel(
    // SEND side (#961).
    pref: PerformancePreference,
    on_change: EventHandler<PerformancePreference>,
    read_snapshot: SnapshotReader,
    read_screen_snapshot: ScreenSnapshotReader,
    // RECEIVE side (#989 simulcast).
    receive_pref: ReceivePreference,
    on_receive_change: EventHandler<(PrefMediaKind, KindReceivePref)>,
    received_reader: ReceivedReader,
) -> Element {
    let video_fixed = pref.video_is_fixed();
    let audio_fixed = pref.audio_is_fixed();
    let screen_fixed = pref.screen_is_fixed();

    // First-paint SEND gauge values (before the rAF driver ticks). The same pure
    // mapper drives the live loop, so first paint and live updates agree.
    let initial = read_snapshot.read();
    let initial_screen = read_screen_snapshot.read();
    let g = gauge_state_from_snapshot(initial.as_ref(), initial_screen.as_ref());

    // First-paint RECEIVE gauge values.
    let rgv = receive::gauge_state(received_reader.read(PrefMediaKind::Video).as_ref());
    let rga = receive::gauge_state(received_reader.read(PrefMediaKind::Audio).as_ref());
    let rgs = receive::gauge_state(received_reader.read(PrefMediaKind::Screen).as_ref());

    // Which popover (if any) is currently open. `None` = all closed. Shared
    // across every section/row so opening one closes the others.
    let open_help: Signal<Option<&'static str>> = use_signal(|| None);

    rsx! {
        h3 { class: "settings-section-title", "Performance" }
        p { class: "settings-section-description",
            "Per stream: limit what you RECEIVE from others (saves your download) and "
            "what you SEND to others (saves your upload + CPU). Each control adapts "
            "within its range; the needles show what's flowing right now."
        }

        // Headless drivers: one ~4 Hz rAF loop each, updating the Sending and
        // Receiving needles by id. They render nothing; gauges live in the
        // sections below.
        QualityVuMeterDriver { read_snapshot, read_screen_snapshot }
        receive::ReceivedQualityDriver { reader: received_reader }

        // ── Video ──
        KindSection {
            kind: PrefMediaKind::Video,
            title: "Video",
            stream_noun: "video",
            send_id_prefix: "perf-video",
            send_min_testid: TESTID_VIDEO_RANGE_MIN,
            send_max_testid: TESTID_VIDEO_RANGE_MAX,
            send_auto_testid: TESTID_VIDEO_AUTO,
            send_fixed_testid: "perf-video-fixed-badge",
            send_help_testid: "perf-video-help",
            send_vu_testid: TESTID_VU_VIDEO,
            send_vu_needle_id: VIDEO_NEEDLE_ID,
            send_vu_readout_id: VIDEO_READOUT_ID,
            send_vu_label: "Sending video",
            send_vu_initial_deg: g.video_deg,
            send_vu_initial_readout: g.video_text.clone(),
            send_labels: VIDEO_TIER_LABELS.to_vec(),
            send_best: pref.video_max,
            send_worst: pref.video_min,
            send_is_fixed: video_fixed,
            send_is_auto: pref.video_auto,
            recv_vu_initial_deg: rgv.deg,
            recv_vu_initial_readout: rgv.text.clone(),
            recv_sub: receive_pref.video,
            open_help,
            on_send_change: move |sel: RangeSel| on_change.call(pref.with_video_thumbs(sel)),
            on_send_auto_toggle: move |on: bool| on_change.call(pref.set_video_auto(on)),
            on_recv_change: move |sub: KindReceivePref| {
                on_receive_change.call((PrefMediaKind::Video, sub));
            },
        }

        // ── Audio ──
        KindSection {
            kind: PrefMediaKind::Audio,
            title: "Audio",
            stream_noun: "audio",
            send_id_prefix: "perf-audio",
            send_min_testid: TESTID_AUDIO_RANGE_MIN,
            send_max_testid: TESTID_AUDIO_RANGE_MAX,
            send_auto_testid: TESTID_AUDIO_AUTO,
            send_fixed_testid: "perf-audio-fixed-badge",
            send_help_testid: "perf-audio-help",
            send_vu_testid: TESTID_VU_AUDIO,
            send_vu_needle_id: AUDIO_NEEDLE_ID,
            send_vu_readout_id: AUDIO_READOUT_ID,
            send_vu_label: "Sending audio",
            send_vu_initial_deg: g.audio_deg,
            send_vu_initial_readout: g.audio_text.clone(),
            send_labels: AUDIO_TIER_LABELS.to_vec(),
            send_best: pref.audio_max,
            send_worst: pref.audio_min,
            send_is_fixed: audio_fixed,
            send_is_auto: pref.audio_auto,
            recv_vu_initial_deg: rga.deg,
            recv_vu_initial_readout: rga.text.clone(),
            recv_sub: receive_pref.audio,
            open_help,
            on_send_change: move |sel: RangeSel| on_change.call(pref.with_audio_thumbs(sel)),
            on_send_auto_toggle: move |on: bool| on_change.call(pref.set_audio_auto(on)),
            on_recv_change: move |sub: KindReceivePref| {
                on_receive_change.call((PrefMediaKind::Audio, sub));
            },
        }

        // ── Screen / shared content ──
        KindSection {
            kind: PrefMediaKind::Screen,
            title: "Screen Share",
            stream_noun: "screen share",
            send_id_prefix: "perf-screen",
            send_min_testid: TESTID_SCREEN_RANGE_MIN,
            send_max_testid: TESTID_SCREEN_RANGE_MAX,
            send_auto_testid: TESTID_SCREEN_AUTO,
            send_fixed_testid: "perf-screen-fixed-badge",
            send_help_testid: "perf-screen-help",
            send_vu_testid: TESTID_VU_SCREEN,
            send_vu_needle_id: SCREEN_NEEDLE_ID,
            send_vu_readout_id: SCREEN_READOUT_ID,
            send_vu_label: "Sending screen",
            send_vu_initial_deg: g.screen_deg,
            send_vu_initial_readout: g.screen_text.clone(),
            send_labels: SCREEN_TIER_LABELS.to_vec(),
            send_best: pref.screen_max,
            send_worst: pref.screen_min,
            send_is_fixed: screen_fixed,
            send_is_auto: pref.screen_auto,
            recv_vu_initial_deg: rgs.deg,
            recv_vu_initial_readout: rgs.text.clone(),
            recv_sub: receive_pref.screen,
            open_help,
            on_send_change: move |sel: RangeSel| on_change.call(pref.with_screen_thumbs(sel)),
            on_send_auto_toggle: move |on: bool| on_change.call(pref.set_screen_auto(on)),
            on_recv_change: move |sub: KindReceivePref| {
                on_receive_change.call((PrefMediaKind::Screen, sub));
            },
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// RECEIVE side (simulcast P4/P5). Layer-index convention is DIRECT: index 0 =
// LOWEST quality, higher = HIGHER. Kept in its own module so its RangeSel /
// span_text / bounds_to_thumbs cannot be confused with the inverted send-side
// ones above.
// ══════════════════════════════════════════════════════════════════════════
pub mod receive {
    use super::{write_needle_rotation, write_readout_text};
    use dioxus::prelude::*;
    use std::rc::Rc;
    use videocall_client::{PrefMediaKind, ReceivedLayerSnapshot};
    use wasm_bindgen::JsCast;

    /// A cloneable, `PartialEq`-able handle around the per-kind received-snapshot
    /// reader closure (see [`super::SnapshotReader`] for the pattern).
    #[derive(Clone)]
    pub struct ReceivedReader(pub Rc<dyn Fn(PrefMediaKind) -> Option<ReceivedLayerSnapshot>>);

    impl ReceivedReader {
        /// A reader that always yields `None` (nothing received / test default).
        pub fn none() -> Self {
            ReceivedReader(Rc::new(|_| None))
        }

        pub(super) fn read(&self, kind: PrefMediaKind) -> Option<ReceivedLayerSnapshot> {
            (self.0)(kind)
        }
    }

    impl PartialEq for ReceivedReader {
        fn eq(&self, other: &Self) -> bool {
            Rc::ptr_eq(&self.0, &other.0)
        }
    }

    // ── localStorage key + persisted shape ─────────────────────────

    /// `localStorage` key for the persisted receive-bounds preference.
    pub const RECEIVE_PREF_KEY: &str = "vc_perf_receive_bounds";

    /// One stream's persisted receive bound: min/max layer index (`None` = that
    /// end unbounded) plus an explicit Auto flag. When `auto` is set the encoder
    /// bounds are forced to `(None, None)` regardless of the stored indices
    /// (which are kept so toggling Auto off restores the last manual range).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct KindReceivePref {
        /// Inclusive minimum received layer index, or `None` for no lower bound.
        #[serde(default)]
        pub min: Option<u32>,
        /// Inclusive maximum received layer index, or `None` for no upper bound.
        #[serde(default)]
        pub max: Option<u32>,
        /// Whether this stream is on Auto (full range). Default `true`.
        #[serde(default = "default_true")]
        pub auto: bool,
    }

    impl Default for KindReceivePref {
        fn default() -> Self {
            KindReceivePref {
                min: None,
                max: None,
                auto: true,
            }
        }
    }

    /// serde default for the `auto` flag (a fn because serde needs a path).
    fn default_true() -> bool {
        true
    }

    /// The full persisted receive-bounds preference: one [`KindReceivePref`] per
    /// media kind. Default = all-Auto. `#[serde(default)]` per field gives
    /// back-compat for prefs written by an older build that lacked a kind.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
    pub struct ReceivePreference {
        #[serde(default)]
        pub video: KindReceivePref,
        #[serde(default)]
        pub audio: KindReceivePref,
        #[serde(default)]
        pub screen: KindReceivePref,
    }

    impl ReceivePreference {
        /// The per-kind sub-preference.
        pub fn for_kind(&self, kind: PrefMediaKind) -> KindReceivePref {
            match kind {
                PrefMediaKind::Video => self.video,
                PrefMediaKind::Audio => self.audio,
                PrefMediaKind::Screen => self.screen,
            }
        }

        /// Return a copy with one kind's sub-preference replaced.
        pub fn with_kind(mut self, kind: PrefMediaKind, sub: KindReceivePref) -> Self {
            match kind {
                PrefMediaKind::Video => self.video = sub,
                PrefMediaKind::Audio => self.audio = sub,
                PrefMediaKind::Screen => self.screen = sub,
            }
            self
        }

        /// The effective encoder `(min, max)` for a kind: `(None, None)` when on
        /// Auto (full range), otherwise the stored indices. This is exactly what
        /// gets pushed to `set_receive_layer_bounds`.
        pub fn effective_bounds(&self, kind: PrefMediaKind) -> (Option<u32>, Option<u32>) {
            let s = self.for_kind(kind);
            if s.auto {
                (None, None)
            } else {
                (s.min, s.max)
            }
        }

        /// Clamp any stored index outside `[0, layer_count(kind)-1]` back to
        /// `None`, defending against a pref written by a build with a different
        /// ladder size.
        pub fn sanitized(self) -> Self {
            let fix = |kind: PrefMediaKind, sub: KindReceivePref| {
                let top = top_index(kind);
                KindReceivePref {
                    min: sub.min.filter(|&i| i <= top),
                    max: sub.max.filter(|&i| i <= top),
                    auto: sub.auto,
                }
            };
            ReceivePreference {
                video: fix(PrefMediaKind::Video, self.video),
                audio: fix(PrefMediaKind::Audio, self.audio),
                screen: fix(PrefMediaKind::Screen, self.screen),
            }
        }
    }

    /// Load the persisted receive preference, falling back to all-Auto on any
    /// failure and sanitizing any stale out-of-range index.
    pub fn load_receive_preference() -> ReceivePreference {
        crate::local_storage::load_json::<ReceivePreference>(
            RECEIVE_PREF_KEY,
            ReceivePreference::default(),
        )
        .sanitized()
    }

    /// Persist the receive preference. Silently no-ops on storage failure.
    pub fn save_receive_preference(pref: &ReceivePreference) {
        crate::local_storage::save_json(RECEIVE_PREF_KEY, pref);
    }

    // ── per-kind layer ladders (label ↔ index, receive convention) ─
    //
    // Order is LOWEST-first: index 0 = lowest quality (left thumb), top index =
    // highest quality (right thumb).

    /// Video receive layer labels, index 0 = lowest (360p) … 2 = highest (720p).
    pub const VIDEO_LAYER_LABELS: [&str; 3] = ["360p", "540p", "720p"];

    /// Screen receive layer labels, index 0 = lowest … 2 = highest.
    pub const SCREEN_LAYER_LABELS: [&str; 3] = ["low", "medium", "high"];

    /// Audio receive layer labels, index 0 = low (24k) … 2 = high (50k).
    /// Three rungs to match the publisher's audio ladder (#1082).
    pub const AUDIO_LAYER_LABELS: [&str; 3] = ["low (24k)", "mid (32k)", "high (50k)"];

    /// The labels for a media kind.
    pub fn labels_for(kind: PrefMediaKind) -> &'static [&'static str] {
        match kind {
            PrefMediaKind::Video => &VIDEO_LAYER_LABELS,
            PrefMediaKind::Screen => &SCREEN_LAYER_LABELS,
            PrefMediaKind::Audio => &AUDIO_LAYER_LABELS,
        }
    }

    /// The number of layers in a kind's ladder.
    pub fn layer_count(kind: PrefMediaKind) -> u32 {
        labels_for(kind).len() as u32
    }

    /// The top (highest-quality) layer index for a kind: `layer_count - 1`.
    pub fn top_index(kind: PrefMediaKind) -> u32 {
        layer_count(kind).saturating_sub(1)
    }

    /// Map a layer index to its label for a kind, or `"?"` if out of range.
    pub fn index_label(kind: PrefMediaKind, index: u32) -> &'static str {
        labels_for(kind).get(index as usize).copied().unwrap_or("?")
    }

    // ── dual-thumb range slider model (receive: left=low index) ────

    /// One stream's dual-thumb slider state, in layer-index space. `min_pos` is
    /// the left thumb, `max_pos` the right thumb; `min_pos <= max_pos` always.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RangeSel {
        pub min_pos: u32,
        pub max_pos: u32,
    }

    /// Derive a kind's slider thumbs from its stored `(min, max)` bounds.
    pub fn bounds_to_thumbs(kind: PrefMediaKind, min: Option<u32>, max: Option<u32>) -> RangeSel {
        let top = top_index(kind);
        let min_pos = min.unwrap_or(0).min(top);
        let max_pos = max.unwrap_or(top).min(top);
        if min_pos > max_pos {
            RangeSel {
                min_pos: max_pos,
                max_pos,
            }
        } else {
            RangeSel { min_pos, max_pos }
        }
    }

    /// Derive a kind's `(min, max)` bounds from its slider thumbs. A thumb at its
    /// extreme means "no bound" (`None`) on that end. Both extremes →
    /// `(None, None)` (full range = Auto).
    pub fn thumbs_to_bounds(kind: PrefMediaKind, sel: RangeSel) -> (Option<u32>, Option<u32>) {
        let top = top_index(kind);
        let min = if sel.min_pos == 0 {
            None
        } else {
            Some(sel.min_pos.min(top))
        };
        let max = if sel.max_pos >= top {
            None
        } else {
            Some(sel.max_pos)
        };
        (min, max)
    }

    /// Move the LEFT (min) thumb to `new_min_pos`, never past the right thumb.
    pub fn set_min_thumb(sel: RangeSel, new_min_pos: u32) -> RangeSel {
        RangeSel {
            min_pos: new_min_pos.min(sel.max_pos),
            max_pos: sel.max_pos,
        }
    }

    /// Move the RIGHT (max) thumb to `new_max_pos`, never past the left thumb.
    pub fn set_max_thumb(sel: RangeSel, new_max_pos: u32) -> RangeSel {
        RangeSel {
            min_pos: sel.min_pos,
            max_pos: new_max_pos.max(sel.min_pos),
        }
    }

    /// Concrete span text for the slider readout. Pure.
    pub fn span_text(kind: PrefMediaKind, sel: RangeSel) -> String {
        let low = index_label(kind, sel.min_pos);
        let high = index_label(kind, sel.max_pos);
        if sel.min_pos == sel.max_pos {
            low.to_string()
        } else {
            format!("{low} – {high}")
        }
    }

    // ── needle math + readout ──────────────────────────────────────

    /// Sweep range of the analog needle, in degrees.
    pub const MAX_NEEDLE_DEG: f32 = super::MAX_NEEDLE_DEG;

    /// Empty-state needle angle: rested at the left peg (lowest end).
    pub const EMPTY_NEEDLE_DEG: f32 = -MAX_NEEDLE_DEG;

    /// Empty-state readout shown when nothing of a kind is being received.
    pub const EMPTY_READOUT: &str = "Not receiving";

    /// Convert a decoded `layer_index` within a `layer_count`-layer ladder into
    /// the needle rotation angle. Layer 0 (lowest) → `-MAX` (left); top → `+MAX`
    /// (right). Single-layer centers; out-of-range clamps. Pure.
    pub fn needle_deg(layer_index: u32, layer_count: u32) -> f32 {
        if layer_count <= 1 {
            return 0.0;
        }
        let max_idx = layer_count - 1;
        let clamped = layer_index.min(max_idx);
        let frac = clamped as f32 / max_idx as f32;
        -MAX_NEEDLE_DEG + frac * (2.0 * MAX_NEEDLE_DEG)
    }

    /// Format the readout line for a received snapshot. Video/screen show
    /// `"L{i+1}/{n} · {w}x{h}"`; audio shows `"L{i+1}/{n} · {kbps} kbps"`. Pure.
    pub fn format_readout(snap: &ReceivedLayerSnapshot) -> String {
        let layer = snap.layer_index + 1;
        match snap.kind {
            PrefMediaKind::Audio => {
                format!("L{layer}/{} · {} kbps", snap.layer_count, snap.kbps)
            }
            _ => format!(
                "L{layer}/{} · {}x{}",
                snap.layer_count, snap.width, snap.height
            ),
        }
    }

    /// One gauge's render state: needle angle + readout text. Pure.
    #[derive(Debug, Clone, PartialEq)]
    pub struct GaugeState {
        pub deg: f32,
        pub text: String,
    }

    /// Map an optional received snapshot to a gauge's render state. `Some` →
    /// live; `None` → needle reset to the left peg + "Not receiving".
    pub fn gauge_state(snap: Option<&ReceivedLayerSnapshot>) -> GaugeState {
        match snap {
            Some(s) => GaugeState {
                deg: needle_deg(s.layer_index, s.layer_count),
                text: format_readout(s),
            },
            None => GaugeState {
                deg: EMPTY_NEEDLE_DEG,
                text: EMPTY_READOUT.to_string(),
            },
        }
    }

    // ── public testids (receive namespace — distinct from send) ────

    pub const TESTID_VIDEO_RANGE_MIN: &str = "perf-recv-video-range-min";
    pub const TESTID_VIDEO_RANGE_MAX: &str = "perf-recv-video-range-max";
    pub const TESTID_AUDIO_RANGE_MIN: &str = "perf-recv-audio-range-min";
    pub const TESTID_AUDIO_RANGE_MAX: &str = "perf-recv-audio-range-max";
    pub const TESTID_SCREEN_RANGE_MIN: &str = "perf-recv-screen-range-min";
    pub const TESTID_SCREEN_RANGE_MAX: &str = "perf-recv-screen-range-max";
    pub const TESTID_VIDEO_AUTO: &str = "perf-recv-video-auto";
    pub const TESTID_AUDIO_AUTO: &str = "perf-recv-audio-auto";
    pub const TESTID_SCREEN_AUTO: &str = "perf-recv-screen-auto";
    pub const TESTID_VIDEO_HELP: &str = "perf-recv-video-help";
    pub const TESTID_AUDIO_HELP: &str = "perf-recv-audio-help";
    pub const TESTID_SCREEN_HELP: &str = "perf-recv-screen-help";
    pub const TESTID_VU_VIDEO: &str = "perf-vu-recv-video";
    pub const TESTID_VU_AUDIO: &str = "perf-vu-recv-audio";
    pub const TESTID_VU_SCREEN: &str = "perf-vu-recv-screen";

    const VIDEO_NEEDLE_ID: &str = "perf-vu-recv-video-needle";
    const AUDIO_NEEDLE_ID: &str = "perf-vu-recv-audio-needle";
    const SCREEN_NEEDLE_ID: &str = "perf-vu-recv-screen-needle";
    const VIDEO_READOUT_ID: &str = "perf-vu-recv-video-readout";
    const AUDIO_READOUT_ID: &str = "perf-vu-recv-audio-readout";
    const SCREEN_READOUT_ID: &str = "perf-vu-recv-screen-readout";

    /// DOM ids for a kind's receive needle + readout.
    fn needle_ids(kind: PrefMediaKind) -> (&'static str, &'static str) {
        match kind {
            PrefMediaKind::Video => (VIDEO_NEEDLE_ID, VIDEO_READOUT_ID),
            PrefMediaKind::Audio => (AUDIO_NEEDLE_ID, AUDIO_READOUT_ID),
            PrefMediaKind::Screen => (SCREEN_NEEDLE_ID, SCREEN_READOUT_ID),
        }
    }

    /// Per-kind static metadata for the receive row (testids + labels).
    struct RecvMeta {
        min_testid: &'static str,
        max_testid: &'static str,
        auto_testid: &'static str,
        fixed_testid: &'static str,
        help_testid: &'static str,
        vu_testid: &'static str,
        vu_label: &'static str,
        help_body: &'static str,
        id_prefix: &'static str,
    }

    fn recv_meta(kind: PrefMediaKind) -> RecvMeta {
        match kind {
            PrefMediaKind::Video => RecvMeta {
                min_testid: TESTID_VIDEO_RANGE_MIN,
                max_testid: TESTID_VIDEO_RANGE_MAX,
                auto_testid: TESTID_VIDEO_AUTO,
                fixed_testid: "perf-recv-video-fixed-badge",
                help_testid: TESTID_VIDEO_HELP,
                vu_testid: TESTID_VU_VIDEO,
                vu_label: "Receiving video",
                help_body: "Limits the camera-video quality you pull from other participants, to save YOUR download bandwidth. The call picks a layer within this range based on your network. The needle shows what you're receiving now.",
                id_prefix: "perf-recv-video",
            },
            PrefMediaKind::Audio => RecvMeta {
                min_testid: TESTID_AUDIO_RANGE_MIN,
                max_testid: TESTID_AUDIO_RANGE_MAX,
                auto_testid: TESTID_AUDIO_AUTO,
                fixed_testid: "perf-recv-audio-fixed-badge",
                help_testid: TESTID_AUDIO_HELP,
                vu_testid: TESTID_VU_AUDIO,
                vu_label: "Receiving audio",
                help_body: "Limits the audio quality you pull from other participants. The call adapts within this range. Audio is already low-bandwidth, so the download savings here are small.",
                id_prefix: "perf-recv-audio",
            },
            PrefMediaKind::Screen => RecvMeta {
                min_testid: TESTID_SCREEN_RANGE_MIN,
                max_testid: TESTID_SCREEN_RANGE_MAX,
                auto_testid: TESTID_SCREEN_AUTO,
                fixed_testid: "perf-recv-screen-fixed-badge",
                help_testid: TESTID_SCREEN_HELP,
                vu_testid: TESTID_VU_SCREEN,
                vu_label: "Receiving shared content",
                help_body: "Limits the quality of screen / shared-content you pull from others, to save YOUR download bandwidth. Independent of camera video; adapts on its own within the range.",
                id_prefix: "perf-recv-screen",
            },
        }
    }

    // ── components ─────────────────────────────────────────────────

    /// Headless driver for the three received-quality needles. Renders nothing;
    /// owns one ~4 Hz rAF loop reading `received_layer_snapshot(kind)` per kind
    /// and writing the needle rotations + readouts straight to the DOM by id.
    #[component]
    pub fn ReceivedQualityDriver(reader: ReceivedReader) -> Element {
        type RafCell = Rc<std::cell::RefCell<Option<wasm_bindgen::closure::Closure<dyn FnMut()>>>>;
        let cb: RafCell = use_hook(|| Rc::new(std::cell::RefCell::new(None)));

        {
            let cb = cb.clone();
            let reader = reader.clone();
            use_hook(move || {
                let cb_clone = cb.clone();
                let last_ms = Rc::new(std::cell::Cell::new(0.0_f64));
                let closure = wasm_bindgen::closure::Closure::wrap(Box::new(move || {
                    let now = web_sys::window()
                        .and_then(|w| w.performance())
                        .map(|p| p.now())
                        .unwrap_or(0.0);
                    if now - last_ms.get() >= 250.0 {
                        last_ms.set(now);
                        for kind in [
                            PrefMediaKind::Video,
                            PrefMediaKind::Audio,
                            PrefMediaKind::Screen,
                        ] {
                            let snap = reader.read(kind);
                            let state = gauge_state(snap.as_ref());
                            let (needle_id, readout_id) = needle_ids(kind);
                            write_needle_rotation(needle_id, state.deg);
                            write_readout_text(readout_id, &state.text);
                        }
                    }
                    if let (Some(win), Some(c)) = (web_sys::window(), cb_clone.borrow().as_ref()) {
                        let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
                    }
                })
                    as Box<dyn FnMut()>);

                *cb.borrow_mut() = Some(closure);
                if let (Some(win), Some(c)) = (web_sys::window(), cb.borrow().as_ref()) {
                    let _ = win.request_animation_frame(c.as_ref().unchecked_ref());
                }
            });
        }

        {
            let cb = cb.clone();
            use_drop(move || {
                *cb.borrow_mut() = None;
            });
        }

        rsx! {}
    }

    /// A discrete dual-thumb received-quality range slider for one kind.
    #[component]
    fn DualRangeSlider(
        kind: PrefMediaKind,
        id_prefix: &'static str,
        min_testid: &'static str,
        max_testid: &'static str,
        stream_noun: &'static str,
        sel: RangeSel,
        on_change: EventHandler<RangeSel>,
    ) -> Element {
        let top = top_index(kind);
        let min_id = format!("{id_prefix}-range-min");
        let max_id = format!("{id_prefix}-range-max");
        let min_valuetext = index_label(kind, sel.min_pos).to_string();
        let max_valuetext = index_label(kind, sel.max_pos).to_string();

        let (fill_left, fill_right) = if top == 0 {
            (0.0_f32, 100.0_f32)
        } else {
            (
                sel.min_pos as f32 / top as f32 * 100.0,
                sel.max_pos as f32 / top as f32 * 100.0,
            )
        };

        rsx! {
            div { class: "perf-range",
                span { class: "perf-range-end-label", "{index_label(kind, 0)}" }
                div { class: "perf-range-track-wrap",
                    div { class: "perf-range-track",
                        div {
                            class: "perf-range-fill",
                            style: "left: {fill_left}%; right: {100.0 - fill_right}%;",
                        }
                    }
                    input {
                        id: "{min_id}",
                        class: "perf-range-input perf-range-input-min",
                        "data-testid": min_testid,
                        r#type: "range",
                        min: "0",
                        max: "{top}",
                        step: "1",
                        value: "{sel.min_pos}",
                        "aria-label": "Minimum received {stream_noun} quality",
                        "aria-valuetext": "{min_valuetext}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse::<u32>() {
                                on_change.call(set_min_thumb(sel, p));
                            }
                        },
                    }
                    input {
                        id: "{max_id}",
                        class: "perf-range-input perf-range-input-max",
                        "data-testid": max_testid,
                        r#type: "range",
                        min: "0",
                        max: "{top}",
                        step: "1",
                        value: "{sel.max_pos}",
                        "aria-label": "Maximum received {stream_noun} quality",
                        "aria-valuetext": "{max_valuetext}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse::<u32>() {
                                on_change.call(set_max_thumb(sel, p));
                            }
                        },
                    }
                }
                span { class: "perf-range-end-label", "{index_label(kind, top)}" }
            }
        }
    }

    /// One kind's RECEIVE row: needle gauge + dual-thumb slider + Auto/help/Fixed
    /// header + live range text. Used inside [`super::KindSection`].
    #[component]
    pub fn ReceiveRow(
        kind: PrefMediaKind,
        stream_noun: &'static str,
        vu_initial_deg: f32,
        vu_initial_readout: String,
        sub: KindReceivePref,
        open_help: Signal<Option<&'static str>>,
        on_change: EventHandler<KindReceivePref>,
    ) -> Element {
        let meta = recv_meta(kind);
        let sel = bounds_to_thumbs(kind, sub.min, sub.max);
        let range_str = span_text(kind, sel);
        // Fixed = manual (not Auto) AND both thumbs collapsed to one layer.
        let is_fixed = !sub.auto && sel.min_pos == sel.max_pos;
        let auto_button_class = if sub.auto {
            "perf-auto-button is-active"
        } else {
            "perf-auto-button"
        };
        let (vu_needle_id, vu_readout_id) = needle_ids(kind);

        rsx! {
            div { class: "perf-stream-group perf-recv-row",
                div { class: "perf-stream-header",
                    span { class: "perf-stream-title", "Receive" }
                    super::HelpPopover {
                        key_id: meta.id_prefix,
                        help_testid: meta.help_testid,
                        help_label: meta.vu_label,
                        help_body: meta.help_body,
                        open_help,
                    }
                    if is_fixed {
                        span {
                            class: "perf-fixed-badge",
                            "data-testid": meta.fixed_testid,
                            title: "Pinned to a single layer — this stream won't adapt",
                            "aria-label": "received {stream_noun} quality pinned to a single layer",
                            "Fixed"
                        }
                    }
                    button {
                        r#type: "button",
                        class: auto_button_class,
                        "data-testid": meta.auto_testid,
                        "aria-pressed": if sub.auto { "true" } else { "false" },
                        "aria-label": "Automatic received {stream_noun} quality",
                        title: if sub.auto {
                            "Automatic (full range) — click to set manual receive limits"
                        } else {
                            "Manual limits — click for fully automatic receive quality"
                        },
                        onclick: move |_| {
                            let next = if sub.auto {
                                KindReceivePref { auto: false, ..sub }
                            } else {
                                KindReceivePref { min: None, max: None, auto: true }
                            };
                            on_change.call(next);
                        },
                        "Auto"
                    }
                }
                div { class: "perf-stream-body",
                    super::VuGauge {
                        testid: meta.vu_testid,
                        needle_id: vu_needle_id,
                        readout_id: vu_readout_id,
                        label: meta.vu_label,
                        initial_deg: vu_initial_deg,
                        initial_readout: vu_initial_readout,
                    }
                    div { class: "perf-stream-controls",
                        DualRangeSlider {
                            kind,
                            id_prefix: meta.id_prefix,
                            min_testid: meta.min_testid,
                            max_testid: meta.max_testid,
                            stream_noun,
                            sel,
                            on_change: move |s: RangeSel| {
                                let (min, max) = thumbs_to_bounds(kind, s);
                                on_change.call(KindReceivePref { min, max, auto: false });
                            },
                        }
                        p {
                            class: "perf-range-value",
                            "data-testid": "{meta.id_prefix}-range-value",
                            "aria-live": "polite",
                            "Receiving up to: {range_str}"
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn layer_counts_and_top_index_per_kind() {
            // Audio is now 3 rungs to match the publisher ladder (#1082).
            assert_eq!(layer_count(PrefMediaKind::Video), 3);
            assert_eq!(layer_count(PrefMediaKind::Screen), 3);
            assert_eq!(layer_count(PrefMediaKind::Audio), 3);
            assert_eq!(top_index(PrefMediaKind::Video), 2);
            assert_eq!(top_index(PrefMediaKind::Audio), 2);
        }

        #[test]
        fn index_label_receive_convention_not_inverted() {
            // index 0 = LOWEST quality (left), top index = HIGHEST (right).
            assert_eq!(index_label(PrefMediaKind::Video, 0), "360p");
            assert_eq!(index_label(PrefMediaKind::Video, 2), "720p");
            assert_eq!(index_label(PrefMediaKind::Screen, 0), "low");
            assert_eq!(index_label(PrefMediaKind::Screen, 2), "high");
            assert_eq!(index_label(PrefMediaKind::Audio, 0), "low (24k)");
            assert_eq!(index_label(PrefMediaKind::Audio, 1), "mid (32k)");
            assert_eq!(index_label(PrefMediaKind::Audio, 2), "high (50k)");
            assert_eq!(index_label(PrefMediaKind::Audio, 5), "?");
        }

        #[test]
        fn thumbs_both_extremes_is_none_none() {
            let sel = RangeSel {
                min_pos: 0,
                max_pos: 2,
            };
            assert_eq!(thumbs_to_bounds(PrefMediaKind::Video, sel), (None, None));
            let sel_a = RangeSel {
                min_pos: 0,
                max_pos: 2,
            };
            assert_eq!(thumbs_to_bounds(PrefMediaKind::Audio, sel_a), (None, None));
        }

        #[test]
        fn thumbs_each_extreme_individually_is_none_on_that_end() {
            let sel = RangeSel {
                min_pos: 0,
                max_pos: 1,
            };
            assert_eq!(thumbs_to_bounds(PrefMediaKind::Video, sel), (None, Some(1)));
            let sel2 = RangeSel {
                min_pos: 1,
                max_pos: 2,
            };
            assert_eq!(
                thumbs_to_bounds(PrefMediaKind::Video, sel2),
                (Some(1), None)
            );
        }

        #[test]
        fn bounds_to_thumbs_round_trips() {
            for &(min, max) in &[
                (None, None),
                (Some(1u32), None),
                (None, Some(1u32)),
                (Some(1u32), Some(1u32)),
            ] {
                let sel = bounds_to_thumbs(PrefMediaKind::Video, min, max);
                assert!(sel.min_pos <= sel.max_pos);
                assert_eq!(thumbs_to_bounds(PrefMediaKind::Video, sel), (min, max));
            }
        }

        #[test]
        fn bounds_to_thumbs_places_extremes_correctly() {
            let sel = bounds_to_thumbs(PrefMediaKind::Video, None, None);
            assert_eq!(
                sel,
                RangeSel {
                    min_pos: 0,
                    max_pos: 2
                }
            );
            // Audio also 3 rungs now → 0..2.
            let sel_a = bounds_to_thumbs(PrefMediaKind::Audio, None, None);
            assert_eq!(
                sel_a,
                RangeSel {
                    min_pos: 0,
                    max_pos: 2
                }
            );
        }

        #[test]
        fn thumbs_cannot_cross() {
            let sel = RangeSel {
                min_pos: 1,
                max_pos: 2,
            };
            assert_eq!(
                set_min_thumb(sel, 5),
                RangeSel {
                    min_pos: 2,
                    max_pos: 2
                }
            );
            assert_eq!(
                set_max_thumb(sel, 0),
                RangeSel {
                    min_pos: 1,
                    max_pos: 1
                }
            );
        }

        #[test]
        fn span_text_renders_concrete_endpoints() {
            assert_eq!(
                span_text(
                    PrefMediaKind::Video,
                    RangeSel {
                        min_pos: 0,
                        max_pos: 2
                    }
                ),
                "360p – 720p"
            );
            assert_eq!(
                span_text(
                    PrefMediaKind::Video,
                    RangeSel {
                        min_pos: 1,
                        max_pos: 1
                    }
                ),
                "540p"
            );
            assert_eq!(
                span_text(
                    PrefMediaKind::Audio,
                    RangeSel {
                        min_pos: 0,
                        max_pos: 2
                    }
                ),
                "low (24k) – high (50k)"
            );
        }

        #[test]
        fn default_preference_is_all_auto() {
            let pref = ReceivePreference::default();
            for kind in [
                PrefMediaKind::Video,
                PrefMediaKind::Audio,
                PrefMediaKind::Screen,
            ] {
                let s = pref.for_kind(kind);
                assert!(s.auto);
                assert_eq!(s.min, None);
                assert_eq!(s.max, None);
                assert_eq!(pref.effective_bounds(kind), (None, None));
            }
        }

        #[test]
        fn auto_flag_forces_none_bounds_regardless_of_indices() {
            let sub = KindReceivePref {
                min: Some(1),
                max: Some(1),
                auto: true,
            };
            let pref = ReceivePreference::default().with_kind(PrefMediaKind::Video, sub);
            assert_eq!(pref.effective_bounds(PrefMediaKind::Video), (None, None));
        }

        #[test]
        fn auto_off_emits_stored_bounds() {
            let sub = KindReceivePref {
                min: None,
                max: Some(1),
                auto: false,
            };
            let pref = ReceivePreference::default().with_kind(PrefMediaKind::Screen, sub);
            assert_eq!(
                pref.effective_bounds(PrefMediaKind::Screen),
                (None, Some(1))
            );
            assert_eq!(pref.effective_bounds(PrefMediaKind::Video), (None, None));
        }

        #[test]
        fn sanitized_drops_out_of_range_indices_keeps_auto() {
            let stale = ReceivePreference {
                video: KindReceivePref {
                    min: Some(9),
                    max: Some(1),
                    auto: false,
                },
                audio: KindReceivePref {
                    min: Some(0),
                    max: Some(7),
                    auto: false,
                },
                screen: KindReceivePref::default(),
            };
            let clean = stale.sanitized();
            assert_eq!(clean.video.min, None); // 9 > top(2) dropped
            assert_eq!(clean.video.max, Some(1)); // kept
            assert!(!clean.video.auto);
            assert_eq!(clean.audio.min, Some(0)); // kept (0 <= top(2))
            assert_eq!(clean.audio.max, None); // 7 > top(2) dropped
        }

        #[test]
        fn needle_deg_lowest_pegs_left_highest_pegs_right() {
            assert_eq!(needle_deg(0, 3), -MAX_NEEDLE_DEG);
            assert_eq!(needle_deg(2, 3), MAX_NEEDLE_DEG);
            assert!(needle_deg(1, 3).abs() < 0.001);
            assert_eq!(needle_deg(0, 2), -MAX_NEEDLE_DEG);
            assert_eq!(needle_deg(1, 2), MAX_NEEDLE_DEG);
        }

        #[test]
        fn needle_deg_clamps_and_single_layer_centers() {
            assert_eq!(needle_deg(99, 3), MAX_NEEDLE_DEG);
            assert_eq!(needle_deg(0, 1), 0.0);
            assert_eq!(needle_deg(0, 0), 0.0);
        }

        #[test]
        fn format_readout_video_and_audio_shapes() {
            let v = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Video,
                layer_index: 1,
                layer_count: 3,
                width: 960,
                height: 540,
                kbps: 900,
            };
            assert_eq!(format_readout(&v), "L2/3 · 960x540");
            let a = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Audio,
                layer_index: 0,
                layer_count: 3,
                width: 0,
                height: 0,
                kbps: 24,
            };
            assert_eq!(format_readout(&a), "L1/3 · 24 kbps");
            let s = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Screen,
                layer_index: 2,
                layer_count: 3,
                width: 1920,
                height: 1080,
                kbps: 2500,
            };
            assert_eq!(format_readout(&s), "L3/3 · 1920x1080");
        }

        #[test]
        fn gauge_state_live_and_none() {
            let snap = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Video,
                layer_index: 2,
                layer_count: 3,
                width: 1280,
                height: 720,
                kbps: 1500,
            };
            let st = gauge_state(Some(&snap));
            assert_eq!(st.deg, MAX_NEEDLE_DEG);
            assert_eq!(st.text, "L3/3 · 1280x720");
            let empty = gauge_state(None);
            assert_eq!(empty.deg, EMPTY_NEEDLE_DEG);
            assert_eq!(empty.deg, -MAX_NEEDLE_DEG);
            assert_eq!(empty.text, "Not receiving");
        }

        #[test]
        fn preference_json_round_trips() {
            let pref = ReceivePreference {
                video: KindReceivePref {
                    min: Some(1),
                    max: None,
                    auto: false,
                },
                audio: KindReceivePref::default(),
                screen: KindReceivePref {
                    min: None,
                    max: Some(1),
                    auto: false,
                },
            };
            let json = serde_json::to_string(&pref).unwrap();
            let back: ReceivePreference = serde_json::from_str(&json).unwrap();
            assert_eq!(pref, back);
        }

        #[test]
        fn json_without_auto_or_kinds_defaults_to_auto() {
            let legacy = r#"{"video":{"min":1,"max":2}}"#;
            let p: ReceivePreference = serde_json::from_str(legacy).unwrap();
            assert!(p.video.auto);
            assert_eq!(p.effective_bounds(PrefMediaKind::Video), (None, None));
            assert!(p.audio.auto);
            assert!(p.screen.auto);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_tier_index_round_trips() {
        // 8 video tiers: position 7 (far right) = best = tier index 0;
        // position 0 (far left) = worst = tier index 7.
        assert_eq!(position_to_tier_index(7, 8), 0);
        assert_eq!(position_to_tier_index(0, 8), 7);
        assert_eq!(position_to_tier_index(3, 8), 4);
        for pos in 0..8 {
            let idx = position_to_tier_index(pos, 8);
            assert_eq!(tier_index_to_position(idx, 8), pos);
        }
        // 4 audio tiers.
        assert_eq!(position_to_tier_index(3, 4), 0);
        assert_eq!(position_to_tier_index(0, 4), 3);
    }

    /// Build a manual (non-Auto for all streams) preference with the given
    /// bounds, so the bound-inversion assertions aren't masked by the Auto gate.
    fn manual(
        video: (Option<usize>, Option<usize>),
        audio: (Option<usize>, Option<usize>),
        screen: (Option<usize>, Option<usize>),
    ) -> PerformancePreference {
        PerformancePreference {
            video_max: video.0,
            video_min: video.1,
            audio_max: audio.0,
            audio_min: audio.1,
            screen_max: screen.0,
            screen_min: screen.1,
            video_auto: false,
            audio_auto: false,
            screen_auto: false,
        }
    }

    #[test]
    fn inversion_max_is_best_min_is_worst() {
        // Max quality (best) → *_best (lower index, a floor);
        // Min quality (worst) → *_worst (higher index, a cap).
        let pref = manual((Some(0), Some(5)), (Some(0), Some(3)), (Some(0), Some(2)));
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b.video_best, Some(0));
        assert_eq!(b.video_worst, Some(5));
        assert_eq!(b.audio_best, Some(0));
        assert_eq!(b.audio_worst, Some(3));
        assert_eq!(b.screen_best, Some(0));
        assert_eq!(b.screen_worst, Some(2));
    }

    #[test]
    fn inversion_auto_ends_pass_none() {
        let pref = manual((None, Some(7)), (Some(1), None), (None, Some(2)));
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b.video_best, None);
        assert_eq!(b.video_worst, Some(7));
        assert_eq!(b.audio_best, Some(1));
        assert_eq!(b.audio_worst, None);
        assert_eq!(b.screen_best, None);
        assert_eq!(b.screen_worst, Some(2));
    }

    #[test]
    fn auto_flag_forces_none_bounds_regardless_of_indices() {
        // All streams Auto but with stale index fields set: bounds must be
        // None/None for every stream.
        let pref = PerformancePreference {
            video_max: Some(0),
            video_min: Some(5),
            audio_max: Some(0),
            audio_min: Some(3),
            screen_max: Some(0),
            screen_min: Some(2),
            video_auto: true,
            audio_auto: true,
            screen_auto: true,
        };
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b, EncoderQualityBounds::default());
    }

    #[test]
    fn toggling_auto_off_emits_thumb_derived_bounds() {
        let pref = manual((Some(1), Some(5)), (None, None), (Some(0), Some(2)));
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b.video_best, Some(1));
        assert_eq!(b.video_worst, Some(5));
        assert_eq!(b.audio_best, None);
        assert_eq!(b.audio_worst, None);
        assert_eq!(b.screen_best, Some(0));
        assert_eq!(b.screen_worst, Some(2));
    }

    #[test]
    fn set_auto_helpers_toggle_and_reset_thumbs() {
        let manual_pref = manual((Some(1), Some(5)), (Some(0), Some(2)), (Some(0), Some(2)));
        let v_auto = manual_pref.set_video_auto(true);
        assert!(v_auto.video_auto);
        assert_eq!(v_auto.video_max, None);
        assert_eq!(v_auto.video_min, None);
        assert_eq!(v_auto.audio_max, Some(0));
        let s_manual = manual_pref.set_screen_auto(false);
        assert!(!s_manual.screen_auto);
        assert_eq!(s_manual.screen_max, Some(0));
        assert_eq!(s_manual.screen_min, Some(2));
    }

    #[test]
    fn thumbs_both_extremes_is_auto() {
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 0,
                max_pos: 7,
            },
            8,
        );
        assert_eq!(best, None);
        assert_eq!(worst, None);
        let pref = PerformancePreference::default().with_video_thumbs(auto_thumbs(8));
        assert_eq!(pref.video_max, None);
        assert_eq!(pref.video_min, None);
    }

    #[test]
    fn thumbs_each_extreme_individually_is_none_on_that_end() {
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 2,
                max_pos: 7,
            },
            8,
        );
        assert_eq!(best, None);
        assert_eq!(worst, Some(5));
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 0,
                max_pos: 5,
            },
            8,
        );
        assert_eq!(best, Some(2));
        assert_eq!(worst, None);
    }

    #[test]
    fn thumbs_mid_range_pair_maps_to_both_bounds() {
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 2,
                max_pos: 6,
            },
            8,
        );
        assert_eq!(best, Some(1));
        assert_eq!(worst, Some(5));
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 1,
                max_pos: 2,
            },
            4,
        );
        assert_eq!(best, Some(1));
        assert_eq!(worst, Some(2));
    }

    #[test]
    fn bounds_to_thumbs_round_trips_through_thumbs_to_bounds() {
        for &(best, worst) in &[
            (None, None),
            (Some(1), None),
            (None, Some(5)),
            (Some(1), Some(5)),
            (Some(3), Some(3)),
        ] {
            let sel = bounds_to_thumbs(best, worst, 8);
            assert!(sel.min_pos <= sel.max_pos);
            assert_eq!(thumbs_to_bounds(sel, 8), (best, worst));
        }
    }

    #[test]
    fn extreme_tier_bounds_canonicalize_to_auto() {
        let sel = bounds_to_thumbs(Some(0), Some(7), 8);
        assert_eq!(
            sel,
            RangeSel {
                min_pos: 0,
                max_pos: 7
            }
        );
        assert_eq!(thumbs_to_bounds(sel, 8), (None, None));
    }

    #[test]
    fn min_thumb_cannot_pass_max_thumb() {
        let sel = RangeSel {
            min_pos: 1,
            max_pos: 4,
        };
        assert_eq!(
            set_min_thumb(sel, 6),
            RangeSel {
                min_pos: 4,
                max_pos: 4
            }
        );
        assert_eq!(
            set_min_thumb(sel, 3),
            RangeSel {
                min_pos: 3,
                max_pos: 4
            }
        );
    }

    #[test]
    fn max_thumb_cannot_pass_min_thumb() {
        let sel = RangeSel {
            min_pos: 3,
            max_pos: 6,
        };
        assert_eq!(
            set_max_thumb(sel, 1),
            RangeSel {
                min_pos: 3,
                max_pos: 3
            }
        );
        assert_eq!(
            set_max_thumb(sel, 5),
            RangeSel {
                min_pos: 3,
                max_pos: 5
            }
        );
    }

    #[test]
    fn with_thumbs_writes_inverted_bounds_and_clears_auto() {
        let p = PerformancePreference::default().with_video_thumbs(RangeSel {
            min_pos: 2,
            max_pos: 6,
        });
        assert_eq!(p.video_max, Some(1)); // best
        assert_eq!(p.video_min, Some(5)); // worst
        assert!(!p.video_auto);
        assert!(p.audio_auto);
        let p2 = p.with_audio_thumbs(auto_thumbs(4));
        assert_eq!(p2.audio_max, None);
        assert_eq!(p2.audio_min, None);
        assert!(!p2.audio_auto);
    }

    #[test]
    fn span_text_renders_concrete_endpoints_including_full_ladder() {
        let v = &VIDEO_TIER_LABELS;
        assert_eq!(
            span_text(
                RangeSel {
                    min_pos: 0,
                    max_pos: 7
                },
                v
            ),
            "240p – 1080p"
        );
        assert_eq!(
            span_text(
                RangeSel {
                    min_pos: 2,
                    max_pos: 6
                },
                v
            ),
            "360p – 900p"
        );
        assert_eq!(
            span_text(
                RangeSel {
                    min_pos: 3,
                    max_pos: 3
                },
                v
            ),
            "480p"
        );
        assert_eq!(
            span_text(
                RangeSel {
                    min_pos: 0,
                    max_pos: 3
                },
                &AUDIO_TIER_LABELS
            ),
            "16 kbps – 50 kbps"
        );
    }

    #[test]
    fn auto_on_preference_yields_full_span_thumbs() {
        let pref = PerformancePreference::default(); // all auto
        assert!(pref.video_auto);
        let sel = bounds_to_thumbs(pref.video_max, pref.video_min, VIDEO_TIER_LABELS.len());
        assert_eq!(
            sel,
            RangeSel {
                min_pos: 0,
                max_pos: VIDEO_TIER_LABELS.len() - 1
            }
        );
        assert_eq!(span_text(sel, &VIDEO_TIER_LABELS), "240p – 1080p");
    }

    #[test]
    fn default_preference_is_all_auto() {
        let pref = PerformancePreference::default();
        assert_eq!(pref.video_max, None);
        assert_eq!(pref.video_min, None);
        assert_eq!(pref.audio_max, None);
        assert_eq!(pref.audio_min, None);
        assert_eq!(pref.screen_max, None);
        assert_eq!(pref.screen_min, None);
        assert!(pref.video_auto);
        assert!(pref.audio_auto);
        assert!(pref.screen_auto);
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b, EncoderQualityBounds::default());
    }

    #[test]
    fn sanitized_drops_stale_indices_and_keeps_auto_flags() {
        let stale = PerformancePreference {
            video_max: Some(10),
            video_min: Some(3),
            audio_max: Some(9),
            audio_min: Some(2),
            screen_max: Some(5),
            screen_min: Some(1),
            video_auto: false,
            audio_auto: false,
            screen_auto: false,
        };
        let clean = stale.sanitized(
            VIDEO_TIER_LABELS.len(),
            AUDIO_TIER_LABELS.len(),
            SCREEN_TIER_LABELS.len(),
        );
        assert_eq!(clean.video_max, None);
        assert_eq!(clean.video_min, Some(3));
        assert_eq!(clean.audio_max, None);
        assert_eq!(clean.audio_min, Some(2));
        assert_eq!(clean.screen_max, None);
        assert_eq!(clean.screen_min, Some(1));
        assert!(!clean.video_auto);
        assert!(!clean.audio_auto);
        assert!(!clean.screen_auto);
    }

    #[test]
    fn fixed_detection() {
        let fixed = manual((Some(2), Some(2)), (Some(1), Some(3)), (Some(1), Some(1)));
        assert!(fixed.video_is_fixed());
        assert!(!fixed.audio_is_fixed());
        assert!(fixed.screen_is_fixed());
        let auto_equal = PerformancePreference {
            video_max: Some(2),
            video_min: Some(2),
            video_auto: true,
            ..Default::default()
        };
        assert!(!auto_equal.video_is_fixed());
        let partial = PerformancePreference {
            video_max: None,
            video_min: Some(2),
            video_auto: false,
            ..Default::default()
        };
        assert!(!partial.video_is_fixed());
    }

    #[test]
    fn needle_deg_best_pegs_right_worst_pegs_left() {
        assert_eq!(tier_to_needle_deg(0, 8), MAX_NEEDLE_DEG);
        assert_eq!(tier_to_needle_deg(7, 8), -MAX_NEEDLE_DEG);
        let mid = tier_to_needle_deg(3, 7);
        assert!(mid.abs() < 0.001, "mid={mid}");
    }

    #[test]
    fn needle_deg_clamps_out_of_range_and_single_tier() {
        assert_eq!(tier_to_needle_deg(99, 8), -MAX_NEEDLE_DEG);
        assert_eq!(tier_to_needle_deg(0, 1), 0.0);
        assert_eq!(tier_to_needle_deg(0, 0), 0.0);
    }

    #[test]
    fn readout_formatting() {
        let snap = LiveQualitySnapshot {
            video_tier_index: 0,
            video_width: 1280,
            video_height: 720,
            video_fps: 30,
            video_ideal_kbps: 1500,
            audio_tier_index: 1,
            audio_kbps: 32,
            target_bitrate_kbps: 1234.0,
        };
        assert_eq!(format_video_readout(&snap), "1280x720·30fps·1500kbps");
        assert_eq!(format_audio_readout(&snap), "32 kbps");
    }

    #[test]
    fn gauge_state_live_snapshot_maps_to_needles_and_readouts() {
        let snap = LiveQualitySnapshot {
            video_tier_index: 0,
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            video_ideal_kbps: 2500,
            audio_tier_index: 3,
            audio_kbps: 16,
            target_bitrate_kbps: 2000.0,
        };
        let screen = ScreenQualitySnapshot {
            tier_index: 1,
            width: 1280,
            height: 720,
            fps: 15,
            ideal_kbps: 1200,
            target_bitrate_kbps: 1100,
        };
        let st = gauge_state_from_snapshot(Some(&snap), Some(&screen));
        assert_eq!(st.video_deg, MAX_NEEDLE_DEG);
        assert_eq!(st.audio_deg, -MAX_NEEDLE_DEG);
        assert_eq!(st.video_text, "1920x1080·30fps·2500kbps");
        assert_eq!(st.audio_text, "16 kbps");
        assert!(st.screen_deg.abs() < 0.001, "screen_deg={}", st.screen_deg);
        assert_eq!(st.screen_text, "1280x720·15fps·1200kbps");
    }

    #[test]
    fn gauge_state_none_resets_to_empty_state() {
        let st = gauge_state_from_snapshot(None, None);
        assert_eq!(st.video_deg, EMPTY_NEEDLE_DEG);
        assert_eq!(st.audio_deg, EMPTY_NEEDLE_DEG);
        assert_eq!(st.screen_deg, EMPTY_NEEDLE_DEG);
        assert_eq!(st.video_deg, -MAX_NEEDLE_DEG);
        assert_eq!(st.video_text, "Camera off");
        assert_eq!(st.audio_text, "Idle");
        assert_eq!(st.screen_text, "Not sharing");
    }

    #[test]
    fn screen_gauge_none_shows_not_sharing_with_live_va() {
        let va = LiveQualitySnapshot {
            video_tier_index: 2,
            video_width: 1280,
            video_height: 720,
            video_fps: 30,
            video_ideal_kbps: 1500,
            audio_tier_index: 0,
            audio_kbps: 50,
            target_bitrate_kbps: 1400.0,
        };
        let st = gauge_state_from_snapshot(Some(&va), None);
        assert_eq!(st.video_text, "1280x720·30fps·1500kbps");
        assert_eq!(st.screen_deg, EMPTY_NEEDLE_DEG);
        assert_eq!(st.screen_text, "Not sharing");
    }

    #[test]
    fn screen_thumbs_map_over_three_tiers() {
        assert_eq!(
            thumbs_to_bounds(
                RangeSel {
                    min_pos: 0,
                    max_pos: 2
                },
                3
            ),
            (None, None)
        );
        assert_eq!(
            thumbs_to_bounds(
                RangeSel {
                    min_pos: 0,
                    max_pos: 1
                },
                3
            ),
            (Some(1), None)
        );
        assert_eq!(
            thumbs_to_bounds(
                RangeSel {
                    min_pos: 1,
                    max_pos: 2
                },
                3
            ),
            (None, Some(1))
        );
        assert_eq!(
            thumbs_to_bounds(
                RangeSel {
                    min_pos: 1,
                    max_pos: 1
                },
                3
            ),
            (Some(1), Some(1))
        );
        let p = PerformancePreference::default().with_screen_thumbs(RangeSel {
            min_pos: 1,
            max_pos: 1,
        });
        assert_eq!(p.screen_max, Some(1));
        assert_eq!(p.screen_min, Some(1));
        assert!(!p.screen_auto);
    }

    #[test]
    fn preference_json_round_trips_with_screen_and_auto() {
        let pref = PerformancePreference {
            video_max: Some(0),
            video_min: Some(7),
            audio_max: None,
            audio_min: Some(3),
            screen_max: Some(0),
            screen_min: Some(2),
            video_auto: false,
            audio_auto: true,
            screen_auto: false,
        };
        let json = serde_json::to_string(&pref).unwrap();
        let back: PerformancePreference = serde_json::from_str(&json).unwrap();
        assert_eq!(pref, back);
    }

    #[test]
    fn json_without_auto_fields_defaults_to_auto_true() {
        let legacy = r#"{"video_max":2,"video_min":5}"#;
        let p: PerformancePreference = serde_json::from_str(legacy).unwrap();
        assert!(p.video_auto);
        assert!(p.audio_auto);
        assert!(p.screen_auto);
        assert_eq!(
            preference_to_encoder_bounds(&p),
            EncoderQualityBounds::default()
        );
    }
}
