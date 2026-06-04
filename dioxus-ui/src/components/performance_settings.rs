/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Performance settings panel + real-time quality VU meter (issue #961).
//!
//! This module is the UI half of the user-configurable adaptive-quality (AQ)
//! tier bounds feature. It lets the user pin a **max** (best allowed) and
//! **min** (worst allowed) quality for both video and audio, or leave either
//! end on **Auto**. It also renders two analog cassette-deck-style VU needles
//! that track the encoder's *current* quality tier in real time.
//!
//! # The tier-index inversion (the whole feature)
//!
//! Quality is the **inverse** of the tier index used by the AQ crate:
//! index `0` is the BEST tier (video 1080p / audio 50 kbps) and the highest
//! index is the WORST. Therefore:
//!
//! - The user's **Max quality** selection = the *best* allowed tier = the
//!   **lower** index = passed to the backend as `*_best` (a floor on the index;
//!   adaptation never climbs to a smaller index / higher quality).
//! - The user's **Min quality** selection = the *worst* allowed tier = the
//!   **higher** index = passed to the backend as `*_worst` (a cap on the index;
//!   adaptation never drops to a larger index / lower quality).
//! - **Auto** on an end → `None` for that argument.
//!
//! All of the label↔index mapping and the Max/Min→best/worst conversion lives
//! in pure, host-testable free functions (see the `#[cfg(test)]` block) so the
//! inversion logic is covered by `cargo test` with no browser APIs.
//!
//! # The VU meter
//!
//! Polling [`LiveQualitySnapshot`] on every render and re-rendering the whole
//! modal at 4 Hz would be wasteful, so the meter follows the same pattern as the
//! pre-join mic meter: a `requestAnimationFrame`-throttled loop writes the
//! needle rotation directly to the DOM via `style.transform`, bypassing Dioxus's
//! diff. The needle-angle math is a pure function ([`tier_to_needle_deg`]).

use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::{LiveQualitySnapshot, ScreenQualitySnapshot};
use wasm_bindgen::JsCast;

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

// ── localStorage key + persisted shape ────────────────────────────

/// `localStorage` key for the persisted performance preference. Follows the
/// `vc_`-prefixed convention used throughout `context.rs`.
pub const PERFORMANCE_PREF_KEY: &str = "vc_performance_quality";

/// Load the persisted performance preference, falling back to all-Auto on any
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

/// Persist the performance preference. Silently no-ops on storage failure.
pub fn save_performance_preference(pref: &PerformancePreference) {
    crate::local_storage::save_json(PERFORMANCE_PREF_KEY, pref);
}

/// User-selected adaptive-quality tier bounds, persisted to `localStorage`.
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

// ── tier ladders (label ↔ index) ──────────────────────────────────
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

// ── encoder bounds + inversion ─────────────────────────────────────

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

// ── dual-thumb range slider model ──────────────────────────────────
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

// ── VU meter needle math ──────────────────────────────────────────

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

/// All three gauges' render state: needle angle + readout text. Pure so the
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

/// Map the optional live snapshots to all three gauges' render state.
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

// ── DOM helpers for the throttled needle update ───────────────────

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

/// Dual-thumb range slider thumbs (the two overlaid `<input type="range">`).
pub const TESTID_VIDEO_RANGE_MIN: &str = "perf-video-range-min";
pub const TESTID_VIDEO_RANGE_MAX: &str = "perf-video-range-max";
pub const TESTID_AUDIO_RANGE_MIN: &str = "perf-audio-range-min";
pub const TESTID_AUDIO_RANGE_MAX: &str = "perf-audio-range-max";
pub const TESTID_SCREEN_RANGE_MIN: &str = "perf-screen-range-min";
pub const TESTID_SCREEN_RANGE_MAX: &str = "perf-screen-range-max";
/// Per-stream "Auto" reset buttons (slide both thumbs to the extremes).
pub const TESTID_VIDEO_AUTO: &str = "perf-video-auto";
pub const TESTID_AUDIO_AUTO: &str = "perf-audio-auto";
pub const TESTID_SCREEN_AUTO: &str = "perf-screen-auto";
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
/// text node are mutated at runtime (by [`QualityVuMeterDriver`]'s rAF loop) via
/// direct DOM writes, so this component itself never re-renders per frame.
#[component]
fn VuGauge(
    /// Stable testid / aria target for the gauge container.
    testid: &'static str,
    /// Id of the rotating needle `<line>` element.
    needle_id: &'static str,
    /// Id of the readout text element.
    readout_id: &'static str,
    /// Accessible label, e.g. "Video quality" / "Audio quality".
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

/// Headless driver for the three VU needles. Renders **nothing** — it only owns
/// the single ~4 Hz `requestAnimationFrame` polling loop that reads
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

/// A discrete dual-thumb quality range slider for one stream.
///
/// Implemented as two overlaid native `<input type="range">` elements — this
/// keeps full keyboard operability (arrow keys step by one tier) and native
/// focus handling for free, while CSS stacks them into a single track. The
/// thumbs cannot cross: the left (min/worst) thumb is clamped to `<=` the right
/// (max/best) thumb on every change. Left→right is increasing quality, so the
/// rightmost stop is the best tier (index 0).
///
/// Each thumb carries its own `aria-label` and an `aria-valuetext` of the tier
/// label it currently points at, so screen readers announce the quality (not a
/// bare number).
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
                    "aria-label": "Minimum {stream_noun} quality",
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
                    "aria-label": "Maximum {stream_noun} quality",
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

/// One stream's threshold section: its live VU needle gauge alongside the
/// dual-thumb range slider, with the header (Fixed badge + Auto toggle) and the
/// live range text.
///
/// **Auto** is an explicit per-stream toggle (`aria-pressed`): when active
/// (default), the slider stays **fully interactive** with both thumbs pinned at
/// the extremes (full ladder span) and the encoder bounds are `None/None`. The
/// only Auto-on cues are the green button + the thumbs at the ends — the slider
/// is never disabled or dimmed. Clicking the button toggles Auto via
/// `on_auto_toggle` (ON snaps thumbs back to the extremes); dragging a thumb
/// inward reports the new range via `on_change`, which turns Auto OFF.
#[allow(clippy::too_many_arguments)]
#[component]
fn ThresholdGroup(
    title: &'static str,
    stream_noun: &'static str,
    id_prefix: &'static str,
    min_testid: &'static str,
    max_testid: &'static str,
    auto_testid: &'static str,
    fixed_testid: &'static str,
    /// "?" help button testid + its accessible label + the explanation copy.
    help_testid: &'static str,
    help_label: &'static str,
    help_body: &'static str,
    /// VU gauge identifiers + first-paint values for this stream's needle.
    vu_testid: &'static str,
    vu_needle_id: &'static str,
    vu_readout_id: &'static str,
    vu_label: &'static str,
    vu_initial_deg: f32,
    vu_initial_readout: String,
    labels: Vec<&'static str>,
    /// Current bounds: (best index, worst index), `None` = extreme on that end.
    best: Option<usize>,
    worst: Option<usize>,
    is_fixed: bool,
    /// Whether this stream is currently on Auto.
    is_auto: bool,
    /// Which section's help popover is open (shared so opening one closes others).
    /// Keyed by `id_prefix`.
    open_help: Signal<Option<&'static str>>,
    on_change: EventHandler<RangeSel>,
    /// Toggle the Auto flag; the arg is the new desired state.
    on_auto_toggle: EventHandler<bool>,
) -> Element {
    let sel = bounds_to_thumbs(best, worst, labels.len());
    // The readout always describes the concrete slider span (full ladder while
    // Auto, since both thumbs sit at the extremes), not the encoder semantics.
    let range_str = span_text(sel, &labels);
    let auto_button_class = if is_auto {
        "perf-auto-button is-active"
    } else {
        "perf-auto-button"
    };
    let mut open_help = open_help;
    let help_open = open_help() == Some(id_prefix);
    let help_popover_id = format!("{id_prefix}-help-popover");
    let help_btn_id = format!("{id_prefix}-help-btn");

    rsx! {
        div { class: "perf-stream-group",
            div { class: "perf-stream-header",
                span { class: "perf-stream-title", "{title}" }
                // "?" help button + popover.
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
                                open_help.set(Some(id_prefix));
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
                        // Transparent full-viewport scrim: any click outside the
                        // popover closes it (touch-friendly outside-click).
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
                            // Keep clicks inside from reaching the scrim.
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
                if is_fixed {
                    span {
                        class: "perf-fixed-badge",
                        "data-testid": fixed_testid,
                        title: "Quality is pinned to a single tier — the call won't adapt this stream",
                        "aria-label": "{stream_noun} quality pinned to a single tier — adaptation disabled for this stream",
                        "Fixed"
                    }
                }
                button {
                    r#type: "button",
                    class: auto_button_class,
                    "data-testid": auto_testid,
                    "aria-pressed": if is_auto { "true" } else { "false" },
                    "aria-label": "Automatic {stream_noun} quality",
                    title: if is_auto {
                        "Automatic (full range) — click to set manual quality limits"
                    } else {
                        "Manual limits — click for fully automatic quality"
                    },
                    onclick: move |_| on_auto_toggle.call(!is_auto),
                    "Auto"
                }
            }
            // Needle gauge (left) beside the slider + range text (right).
            div { class: "perf-stream-body",
                VuGauge {
                    testid: vu_testid,
                    needle_id: vu_needle_id,
                    readout_id: vu_readout_id,
                    label: vu_label,
                    initial_deg: vu_initial_deg,
                    initial_readout: vu_initial_readout,
                }
                div { class: "perf-stream-controls",
                    DualRangeSlider {
                        id_prefix,
                        min_testid,
                        max_testid,
                        stream_noun,
                        labels: labels.clone(),
                        sel,
                        on_change: move |s: RangeSel| on_change.call(s),
                    }
                    p {
                        class: "perf-range-value",
                        "data-testid": "{id_prefix}-range-value",
                        "aria-live": "polite",
                        "Range: {range_str}"
                    }
                }
            }
        }
    }
}

/// The Performance settings panel body: the live VU meters (top), then the
/// per-stream dual-thumb quality-threshold sliders.
///
/// `pref` is the current persisted preference (controlled by the parent). When
/// the user moves a thumb or presses Auto, the panel derives the new bounds and
/// calls `on_change`; the parent persists it and pushes it to the encoder. The
/// panel is otherwise stateless.
#[component]
pub fn PerformanceSettingsPanel(
    pref: PerformancePreference,
    on_change: EventHandler<PerformancePreference>,
    read_snapshot: SnapshotReader,
    read_screen_snapshot: ScreenSnapshotReader,
) -> Element {
    let video_fixed = pref.video_is_fixed();
    let audio_fixed = pref.audio_is_fixed();
    let screen_fixed = pref.screen_is_fixed();

    // First-paint gauge values (before the rAF driver ticks). The same pure
    // mapper drives the live loop, so first paint and live updates agree.
    let initial = read_snapshot.read();
    let initial_screen = read_screen_snapshot.read();
    let g = gauge_state_from_snapshot(initial.as_ref(), initial_screen.as_ref());

    // Which popover (if any) is currently open. `None` = all closed. Shared
    // across the three sections so opening one closes the others.
    let open_help: Signal<Option<&'static str>> = use_signal(|| None);

    rsx! {
        h3 { class: "settings-section-title", "Performance" }
        p { class: "settings-section-description",
            "Each stream is on Auto (fully automatic) by default. Turn Auto off to "
            "bound the range with the dual thumbs: the left thumb is the lowest it "
            "may drop to, the right thumb the highest it may rise to."
        }

        // Headless driver: owns the single ~4 Hz rAF loop that updates all three
        // needles by id. Renders nothing; the gauges live in the sections below.
        QualityVuMeterDriver { read_snapshot, read_screen_snapshot }

        // ── Video Thresholds ──
        ThresholdGroup {
            title: "Video Thresholds",
            stream_noun: "video",
            id_prefix: "perf-video",
            min_testid: TESTID_VIDEO_RANGE_MIN,
            max_testid: TESTID_VIDEO_RANGE_MAX,
            auto_testid: TESTID_VIDEO_AUTO,
            fixed_testid: "perf-video-fixed-badge",
            help_testid: "perf-video-help",
            help_label: "About video quality",
            help_body: "Sets the best (right) and worst (left) video quality the call may use. The app automatically adapts resolution and frame rate between these limits based on your network. Auto uses the full range.",
            vu_testid: TESTID_VU_VIDEO,
            vu_needle_id: VIDEO_NEEDLE_ID,
            vu_readout_id: VIDEO_READOUT_ID,
            vu_label: "Video quality",
            vu_initial_deg: g.video_deg,
            vu_initial_readout: g.video_text.clone(),
            labels: VIDEO_TIER_LABELS.to_vec(),
            best: pref.video_max,
            worst: pref.video_min,
            is_fixed: video_fixed,
            is_auto: pref.video_auto,
            open_help,
            on_change: move |sel: RangeSel| {
                on_change.call(pref.with_video_thumbs(sel));
            },
            on_auto_toggle: move |on: bool| {
                on_change.call(pref.set_video_auto(on));
            },
        }

        // ── Audio Thresholds ──
        ThresholdGroup {
            title: "Audio Thresholds",
            stream_noun: "audio",
            id_prefix: "perf-audio",
            min_testid: TESTID_AUDIO_RANGE_MIN,
            max_testid: TESTID_AUDIO_RANGE_MAX,
            auto_testid: TESTID_AUDIO_AUTO,
            fixed_testid: "perf-audio-fixed-badge",
            help_testid: "perf-audio-help",
            help_label: "About audio quality",
            help_body: "Sets the best and worst audio quality (bitrate) the call may use. Audio only steps down once video is already at its lowest quality, so these limits mainly matter under heavy congestion.",
            vu_testid: TESTID_VU_AUDIO,
            vu_needle_id: AUDIO_NEEDLE_ID,
            vu_readout_id: AUDIO_READOUT_ID,
            vu_label: "Audio quality",
            vu_initial_deg: g.audio_deg,
            vu_initial_readout: g.audio_text.clone(),
            labels: AUDIO_TIER_LABELS.to_vec(),
            best: pref.audio_max,
            worst: pref.audio_min,
            is_fixed: audio_fixed,
            is_auto: pref.audio_auto,
            open_help,
            on_change: move |sel: RangeSel| {
                on_change.call(pref.with_audio_thumbs(sel));
            },
            on_auto_toggle: move |on: bool| {
                on_change.call(pref.set_audio_auto(on));
            },
        }

        // ── Screen Share Thresholds ──
        ThresholdGroup {
            title: "Screen Share Thresholds",
            stream_noun: "screen share",
            id_prefix: "perf-screen",
            min_testid: TESTID_SCREEN_RANGE_MIN,
            max_testid: TESTID_SCREEN_RANGE_MAX,
            auto_testid: TESTID_SCREEN_AUTO,
            fixed_testid: "perf-screen-fixed-badge",
            help_testid: "perf-screen-help",
            help_label: "About shared-content quality",
            help_body: "Sets the best and worst quality for screen / shared-content sharing. This is independent of your camera video — shared content adapts on its own between these limits.",
            vu_testid: TESTID_VU_SCREEN,
            vu_needle_id: SCREEN_NEEDLE_ID,
            vu_readout_id: SCREEN_READOUT_ID,
            vu_label: "Screen quality",
            vu_initial_deg: g.screen_deg,
            vu_initial_readout: g.screen_text.clone(),
            labels: SCREEN_TIER_LABELS.to_vec(),
            best: pref.screen_max,
            worst: pref.screen_min,
            is_fixed: screen_fixed,
            is_auto: pref.screen_auto,
            open_help,
            on_change: move |sel: RangeSel| {
                on_change.call(pref.with_screen_thumbs(sel));
            },
            on_auto_toggle: move |on: bool| {
                on_change.call(pref.set_screen_auto(on));
            },
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
        // Same indices, but auto OFF → the stored bounds flow through.
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
        // Turning Auto ON clears that stream's bounds; OFF leaves them.
        let manual_pref = manual((Some(1), Some(5)), (Some(0), Some(2)), (Some(0), Some(2)));
        let v_auto = manual_pref.set_video_auto(true);
        assert!(v_auto.video_auto);
        assert_eq!(v_auto.video_max, None);
        assert_eq!(v_auto.video_min, None);
        // Audio/screen untouched by the video toggle.
        assert_eq!(v_auto.audio_max, Some(0));
        // Turning Auto OFF flips the flag but keeps any stored bounds as-is.
        let s_manual = manual_pref.set_screen_auto(false);
        assert!(!s_manual.screen_auto);
        assert_eq!(s_manual.screen_max, Some(0));
        assert_eq!(s_manual.screen_min, Some(2));
    }

    #[test]
    fn thumbs_both_extremes_is_auto() {
        // Video: left thumb at 0 (far left) + right thumb at 7 (far right) =
        // fully Auto = no bounds.
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 0,
                max_pos: 7,
            },
            8,
        );
        assert_eq!(best, None);
        assert_eq!(worst, None);
        // And that is exactly the default-Auto preference.
        let pref = PerformancePreference::default().with_video_thumbs(auto_thumbs(8));
        assert_eq!(pref.video_max, None);
        assert_eq!(pref.video_min, None);
    }

    #[test]
    fn thumbs_each_extreme_individually_is_none_on_that_end() {
        // Right thumb at far right → best=None; left thumb pulled in to pos 2
        // (tier index 5) → worst=Some(5).
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 2,
                max_pos: 7,
            },
            8,
        );
        assert_eq!(best, None);
        assert_eq!(worst, Some(5));
        // Left thumb at far left → worst=None; right thumb pulled in to pos 5
        // (tier index 2) → best=Some(2).
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
        // min_pos=2 (tier 5 / 360p), max_pos=6 (tier 1 / 900p).
        let (best, worst) = thumbs_to_bounds(
            RangeSel {
                min_pos: 2,
                max_pos: 6,
            },
            8,
        );
        assert_eq!(best, Some(1));
        assert_eq!(worst, Some(5));
        // Audio mid-range: min_pos=1 (tier 2 / 24k), max_pos=2 (tier 1 / 32k).
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
        // Canonical (non-extreme) bounds round-trip exactly.
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
        // Pinning best to the very top tier (index 0) is indistinguishable from
        // "no cap" on a left→right slider — the right thumb is fully right either
        // way — so it canonicalizes to None (Auto). Likewise worst = the lowest
        // tier (index 7) canonicalizes to None.
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
        // Try to drag min past max → clamped to max.
        assert_eq!(
            set_min_thumb(sel, 6),
            RangeSel {
                min_pos: 4,
                max_pos: 4
            }
        );
        // Valid move.
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
        // Try to drag max below min → clamped to min.
        assert_eq!(
            set_max_thumb(sel, 1),
            RangeSel {
                min_pos: 3,
                max_pos: 3
            }
        );
        // Valid move.
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
        // Drag video to min_pos=2 / max_pos=6 → worst=Some(5), best=Some(1), and
        // dragging implies manual mode (video_auto cleared).
        let p = PerformancePreference::default().with_video_thumbs(RangeSel {
            min_pos: 2,
            max_pos: 6,
        });
        assert_eq!(p.video_max, Some(1)); // best
        assert_eq!(p.video_min, Some(5)); // worst
        assert!(!p.video_auto);
        // Audio still on default Auto (untouched by the video drag).
        assert!(p.audio_auto);
        // Dragging audio to extremes → both None, but auto cleared (manual).
        let p2 = p.with_audio_thumbs(auto_thumbs(4));
        assert_eq!(p2.audio_max, None);
        assert_eq!(p2.audio_min, None);
        assert!(!p2.audio_auto);
    }

    #[test]
    fn span_text_renders_concrete_endpoints_including_full_ladder() {
        let v = &VIDEO_TIER_LABELS;
        // Full ladder (both thumbs at extremes = Auto-on visual) → concrete span.
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
        // Partial manual range.
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
        // Both thumbs together → single label (pinned). pos 3 → tier index
        // 7-3=4 → "480p".
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
        // Audio full ladder.
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
        // An Auto-on stream's bounds are None/None, which derive to thumbs at the
        // extremes (full ladder) — what the always-interactive slider shows while
        // Auto is on. No "disabled" concept exists anymore.
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
        // All three streams default to Auto = true.
        assert!(pref.video_auto);
        assert!(pref.audio_auto);
        assert!(pref.screen_auto);
        let b = preference_to_encoder_bounds(&pref);
        assert_eq!(b, EncoderQualityBounds::default());
    }

    #[test]
    fn sanitized_drops_stale_indices_and_keeps_auto_flags() {
        // A value stored by a hypothetical larger build, restored here: an index
        // past the current ladder falls back to None; the auto flags pass through.
        let stale = PerformancePreference {
            video_max: Some(10),
            video_min: Some(3),
            audio_max: Some(9),
            audio_min: Some(2),
            screen_max: Some(5), // out of range for 3 screen tiers
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
        assert_eq!(clean.video_max, None); // dropped
        assert_eq!(clean.video_min, Some(3)); // kept
        assert_eq!(clean.audio_max, None); // dropped
        assert_eq!(clean.audio_min, Some(2)); // kept
        assert_eq!(clean.screen_max, None); // dropped (>=3)
        assert_eq!(clean.screen_min, Some(1)); // kept
                                               // Flags preserved.
        assert!(!clean.video_auto);
        assert!(!clean.audio_auto);
        assert!(!clean.screen_auto);
    }

    #[test]
    fn fixed_detection() {
        // Manual (auto off) + both ends equal → fixed.
        let fixed = manual((Some(2), Some(2)), (Some(1), Some(3)), (Some(1), Some(1)));
        assert!(fixed.video_is_fixed());
        assert!(!fixed.audio_is_fixed());
        assert!(fixed.screen_is_fixed());
        // A stream on Auto is never "fixed" even with equal stored indices.
        let auto_equal = PerformancePreference {
            video_max: Some(2),
            video_min: Some(2),
            video_auto: true,
            ..Default::default()
        };
        assert!(!auto_equal.video_is_fixed());
        // One end at extreme (None) is never "fixed".
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
        // 8 video tiers: index 0 (best) → +MAX, index 7 (worst) → -MAX.
        assert_eq!(tier_to_needle_deg(0, 8), MAX_NEEDLE_DEG);
        assert_eq!(tier_to_needle_deg(7, 8), -MAX_NEEDLE_DEG);
        // Midpoint is ~0.
        let mid = tier_to_needle_deg(3, 7);
        assert!(mid.abs() < 0.001, "mid={mid}");
    }

    #[test]
    fn needle_deg_clamps_out_of_range_and_single_tier() {
        // Out-of-range index clamps to the worst (no NaN / overshoot).
        assert_eq!(tier_to_needle_deg(99, 8), -MAX_NEEDLE_DEG);
        // Single-tier ladder centers the needle.
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
            video_tier_index: 0, // best → +MAX
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            video_ideal_kbps: 2500,
            audio_tier_index: 3, // worst → -MAX (4-tier ladder)
            audio_kbps: 16,
            target_bitrate_kbps: 2000.0,
        };
        let screen = ScreenQualitySnapshot {
            tier_index: 1, // middle of 3 → ~0deg
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
        // Screen middle tier (index 1 of 3) sits at the needle center.
        assert!(st.screen_deg.abs() < 0.001, "screen_deg={}", st.screen_deg);
        assert_eq!(st.screen_text, "1280x720·15fps·1200kbps");
    }

    #[test]
    fn gauge_state_none_resets_to_empty_state() {
        // Camera off + not sharing: all needles peg left and readouts show their
        // own placeholders — never a frozen / stale reading.
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
        // Camera/mic live but screen not sharing: only the screen gauge resets.
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
        // 3 screen tiers: both extremes → None/None (Auto).
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
        // Right thumb in to pos 1 (tier 1 / 720p) → best=Some(1), worst=None.
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
        // Left thumb in to pos 1 (tier 1) → best=None, worst=Some(1).
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
        // Mid pin both at pos 1 (tier 1 / 720p) → best=worst=Some(1).
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
        // with_screen_thumbs writes the inverted bounds + clears auto.
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
        // A pref persisted before the auto flags existed must load as fully Auto.
        let legacy = r#"{"video_max":2,"video_min":5}"#;
        let p: PerformancePreference = serde_json::from_str(legacy).unwrap();
        assert!(p.video_auto);
        assert!(p.audio_auto);
        assert!(p.screen_auto);
        // And since auto is on, bounds resolve to None despite the stored indices.
        assert_eq!(
            preference_to_encoder_bounds(&p),
            EncoderQualityBounds::default()
        );
    }
}
