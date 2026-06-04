/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! RECEIVE-side Performance settings panel + received-quality VU needles
//! (issue #989, simulcast P4/P5).
//!
//! This is the **downlink** half of the per-receiver simulcast feature. The user
//! caps the min/max simulcast layer they want to RECEIVE from peers, to save
//! THEIR download bandwidth. The bounds apply globally per media kind (to every
//! incoming peer of that kind) via
//! [`VideoCallClient::set_receive_layer_bounds`](videocall_client::VideoCallClient::set_receive_layer_bounds);
//! the per-(peer, kind) layer chooser auto-adapts within the range based on the
//! receiver's own network. The needles show the layer currently being received
//! (already post-clamp, so never above the user's max).
//!
//! # Layer-index convention — STRAIGHTFORWARD (not inverted)
//!
//! Unlike the send-side AQ panel (where tier index 0 was the *best* quality),
//! the receive model is direct: **layer 0 = LOWEST quality, higher index =
//! HIGHER quality.** So on the slider the LEFT thumb = min received quality =
//! lower index, the RIGHT thumb = max received quality = higher index. Per kind:
//! video `0..=2`, screen `0..=2`, audio `0..=1`.
//!
//! - The left thumb maps to `min: Some(idx)` only when it is OFF the left
//!   extreme (index 0); at the extreme it is `None` ("no lower bound").
//! - The right thumb maps to `max: Some(idx)` only when it is OFF the right
//!   extreme (top index); at the extreme it is `None` ("no upper bound").
//! - Both at extremes → `(None, None)` = full range = Auto.
//!
//! All of the label↔index mapping, thumb→(min,max) derivation, and needle math
//! is in pure, host-testable free functions (see `#[cfg(test)]`).
//!
//! # The needles (P5)
//!
//! Polling `received_layer_snapshot(kind)` and re-rendering the modal at 4 Hz
//! would be wasteful, so a single headless [`ReceivedQualityDriver`] runs one
//! `requestAnimationFrame`-throttled (~4 Hz) loop and writes each needle's
//! rotation + readout straight to the DOM by id (bypassing the Dioxus diff). The
//! needle-angle math is the pure [`needle_deg`].

use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::{PrefMediaKind, ReceivedLayerSnapshot};
use wasm_bindgen::JsCast;

// ── snapshot reader handle ─────────────────────────────────────────

/// A cloneable, `PartialEq`-able handle around the per-kind received-snapshot
/// reader closure.
///
/// Dioxus component props must be `PartialEq` (for memoized diffing), but a
/// `dyn Fn` is not comparable, so we wrap it and compare by `Rc` pointer
/// identity: two readers are "equal" iff they are the same allocation. Callers
/// build one stable reader per `Host` mount, so this never spuriously
/// re-renders. `Clone` is cheap (an `Rc` bump).
#[derive(Clone)]
pub struct ReceivedReader(pub Rc<dyn Fn(PrefMediaKind) -> Option<ReceivedLayerSnapshot>>);

impl ReceivedReader {
    /// A reader that always yields `None` (nothing received / test default).
    pub fn none() -> Self {
        ReceivedReader(Rc::new(|_| None))
    }

    fn read(&self, kind: PrefMediaKind) -> Option<ReceivedLayerSnapshot> {
        (self.0)(kind)
    }
}

impl PartialEq for ReceivedReader {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

// ── localStorage key + persisted shape ─────────────────────────────

/// `localStorage` key for the persisted receive-bounds preference. Follows the
/// `vc_`-prefixed convention used throughout `context.rs`.
pub const RECEIVE_PREF_KEY: &str = "vc_perf_receive_bounds";

/// One stream's persisted receive bound: min/max layer index (`None` = that end
/// unbounded) plus an explicit Auto flag. When `auto` is set the encoder bounds
/// are forced to `(None, None)` regardless of the stored indices (which are kept
/// so toggling Auto off restores the last manual range).
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

/// serde default for the `auto` flag (a fn because serde needs a path). Defaults
/// pre-`auto` persisted prefs to fully-Auto.
fn default_true() -> bool {
    true
}

/// The full persisted receive-bounds preference: one [`KindReceivePref`] per
/// media kind. Default = all-Auto (fully automatic; behaviour unchanged unless
/// the user opts in). `#[serde(default)]` per field gives back-compat for prefs
/// written by an older build that lacked a kind.
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

    /// The effective encoder `(min, max)` for a kind: `(None, None)` when on Auto
    /// (full range), otherwise the stored indices. This is exactly what gets
    /// pushed to `set_receive_layer_bounds`.
    pub fn effective_bounds(&self, kind: PrefMediaKind) -> (Option<u32>, Option<u32>) {
        let s = self.for_kind(kind);
        if s.auto {
            (None, None)
        } else {
            (s.min, s.max)
        }
    }

    /// Clamp any stored index outside `[0, layer_count(kind)-1]` back to `None`,
    /// defending against a pref written by a build with a different ladder size.
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
/// failure (missing key, corrupt JSON, storage unavailable) and sanitizing any
/// stale out-of-range index.
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

// ── per-kind layer ladders (label ↔ index, receive convention) ─────
//
// Labels are a fixed product decision and intentionally hard-coded here (the UI
// shows user-facing labels; the backend consumes layer indices). Order is
// LOWEST-first to match the receive convention: index 0 = lowest quality (left
// thumb), top index = highest quality (right thumb).

/// Video receive layer labels, index 0 = lowest (360p) … 2 = highest (720p).
///
/// These mirror `videocall_aq::simulcast_layers(3)` = `[low, standard, hd]`,
/// lowest-first: low = 640×360 (360p), standard = 960×540 (540p), hd = 1280×720
/// (720p). The middle "540p" is correct — `simulcast_layers(3)[1]` is the
/// "standard" tier at 960×540 (#1079 reviewer confirmation).
pub const VIDEO_LAYER_LABELS: [&str; 3] = ["360p", "540p", "720p"];

/// Screen receive layer labels, index 0 = lowest … 2 = highest.
pub const SCREEN_LAYER_LABELS: [&str; 3] = ["low", "medium", "high"];

/// Audio receive layer labels, index 0 = low (24k) … 1 = high (50k). 2 stops.
pub const AUDIO_LAYER_LABELS: [&str; 2] = ["low (24k)", "high (50k)"];

/// The labels for a media kind.
pub fn labels_for(kind: PrefMediaKind) -> &'static [&'static str] {
    match kind {
        PrefMediaKind::Video => &VIDEO_LAYER_LABELS,
        PrefMediaKind::Screen => &SCREEN_LAYER_LABELS,
        PrefMediaKind::Audio => &AUDIO_LAYER_LABELS,
    }
}

/// The number of layers in a kind's ladder (video/screen 3, audio 2).
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

// ── dual-thumb range slider model (receive: left=low index) ────────
//
// A slider "position" IS the layer index directly (no inversion): position 0 =
// far-LEFT = lowest layer; position top = far-RIGHT = highest layer. The two
// thumbs are `min_pos` (left, min received quality) and `max_pos` (right, max),
// with `min_pos <= max_pos` (thumbs can't cross).
//
// "No bound" on an end = that thumb sitting at its extreme:
//   - LEFT thumb at index 0   → `min = None` (no lower bound).
//   - RIGHT thumb at top index → `max = None` (no upper bound).

/// One stream's dual-thumb slider state, in layer-index space. `min_pos` is the
/// left thumb, `max_pos` the right thumb; `min_pos <= max_pos` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSel {
    pub min_pos: u32,
    pub max_pos: u32,
}

/// Derive a kind's slider thumbs from its stored `(min, max)` bounds.
///
/// `None` (no bound) on an end places that thumb at its extreme:
/// - `min = None` → left thumb at index 0.
/// - `max = None` → right thumb at the top index.
///
/// The result always satisfies `min_pos <= max_pos`.
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

/// Derive a kind's `(min, max)` bounds from its slider thumbs.
///
/// A thumb at its extreme means "no bound" (`None`) on that end:
/// - left thumb at index 0   → `min = None`.
/// - right thumb at top index → `max = None`.
///
/// Otherwise the position IS the layer index. Both extremes → `(None, None)`
/// (full range = Auto).
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

/// Move the LEFT (min) thumb to `new_min_pos`, never letting it pass the right
/// thumb. Pure (no-cross guard).
pub fn set_min_thumb(sel: RangeSel, new_min_pos: u32) -> RangeSel {
    RangeSel {
        min_pos: new_min_pos.min(sel.max_pos),
        max_pos: sel.max_pos,
    }
}

/// Move the RIGHT (max) thumb to `new_max_pos`, never letting it pass the left
/// thumb. Pure.
pub fn set_max_thumb(sel: RangeSel, new_max_pos: u32) -> RangeSel {
    RangeSel {
        min_pos: sel.min_pos,
        max_pos: new_max_pos.max(sel.min_pos),
    }
}

/// Concrete span text for the slider readout: always renders both thumb
/// positions as layer labels (e.g. `"360p – 720p"` for the full video ladder),
/// regardless of Auto state — it describes what the slider visibly shows. When
/// both thumbs sit on the same layer it collapses to one label. Pure.
pub fn span_text(kind: PrefMediaKind, sel: RangeSel) -> String {
    let low = index_label(kind, sel.min_pos);
    let high = index_label(kind, sel.max_pos);
    if sel.min_pos == sel.max_pos {
        low.to_string()
    } else {
        format!("{low} – {high}")
    }
}

// ── needle math + readout ──────────────────────────────────────────

/// Sweep range of the analog needle, in degrees. The needle swings from
/// `-MAX_NEEDLE_DEG` (lowest layer / left) to `+MAX_NEEDLE_DEG` (highest layer /
/// right), like a classic cassette-deck VU meter.
pub const MAX_NEEDLE_DEG: f32 = 50.0;

/// Empty-state needle angle: rested at the left peg (lowest end) so a
/// not-receiving gauge reads as "no signal" rather than a frozen mid-scale value.
pub const EMPTY_NEEDLE_DEG: f32 = -MAX_NEEDLE_DEG;

/// Empty-state readout shown when nothing of a kind is being received.
pub const EMPTY_READOUT: &str = "Not receiving";

/// Convert a decoded `layer_index` within a `layer_count`-layer ladder into the
/// needle rotation angle in degrees.
///
/// Layer 0 (lowest) → `-MAX_NEEDLE_DEG` (needle pegged left); the top layer →
/// `+MAX_NEEDLE_DEG` (needle pegged right). A single-layer ladder centers the
/// needle. Out-of-range indices are clamped, so this never produces NaN and is
/// safe to feed straight into a CSS transform. Pure.
pub fn needle_deg(layer_index: u32, layer_count: u32) -> f32 {
    if layer_count <= 1 {
        return 0.0;
    }
    let max_idx = layer_count - 1;
    let clamped = layer_index.min(max_idx);
    // position = layer_index / (layer_count - 1), in 0.0..=1.0 (0 = lowest).
    let frac = clamped as f32 / max_idx as f32;
    // Lowest → -MAX, highest → +MAX.
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

/// One gauge's render state: needle angle + readout text. Pure so the
/// snapshot→gauge mapping (including the not-receiving reset) is host-testable.
#[derive(Debug, Clone, PartialEq)]
pub struct GaugeState {
    pub deg: f32,
    pub text: String,
}

/// Map an optional received snapshot to a gauge's render state. `Some` → live
/// needle angle + readout; `None` → needle reset to the left peg + the
/// "Not receiving" placeholder (so a stopped stream never freezes on a stale
/// reading). Single source of truth for both first paint and the rAF loop.
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

// ── DOM helpers (direct-write needle update) ───────────────────────

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

/// Write `style.transform = rotate(<deg>deg)` to the needle element by id. This
/// is the per-tick DOM write that drives the analog needle without triggering a
/// Dioxus re-render. No-ops if the element is missing.
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

pub const TESTID_VIDEO_RANGE_MIN: &str = "perf-video-range-min";
pub const TESTID_VIDEO_RANGE_MAX: &str = "perf-video-range-max";
pub const TESTID_AUDIO_RANGE_MIN: &str = "perf-audio-range-min";
pub const TESTID_AUDIO_RANGE_MAX: &str = "perf-audio-range-max";
pub const TESTID_SCREEN_RANGE_MIN: &str = "perf-screen-range-min";
pub const TESTID_SCREEN_RANGE_MAX: &str = "perf-screen-range-max";
pub const TESTID_VIDEO_AUTO: &str = "perf-video-auto";
pub const TESTID_AUDIO_AUTO: &str = "perf-audio-auto";
pub const TESTID_SCREEN_AUTO: &str = "perf-screen-auto";
pub const TESTID_VIDEO_HELP: &str = "perf-video-help";
pub const TESTID_AUDIO_HELP: &str = "perf-audio-help";
pub const TESTID_SCREEN_HELP: &str = "perf-screen-help";
pub const TESTID_VU_VIDEO: &str = "perf-vu-video";
pub const TESTID_VU_AUDIO: &str = "perf-vu-audio";
pub const TESTID_VU_SCREEN: &str = "perf-vu-screen";

const VIDEO_NEEDLE_ID: &str = "perf-vu-video-needle";
const AUDIO_NEEDLE_ID: &str = "perf-vu-audio-needle";
const SCREEN_NEEDLE_ID: &str = "perf-vu-screen-needle";
const VIDEO_READOUT_ID: &str = "perf-vu-video-readout";
const AUDIO_READOUT_ID: &str = "perf-vu-audio-readout";
const SCREEN_READOUT_ID: &str = "perf-vu-screen-readout";

/// DOM ids for a kind's needle + readout (used by both the gauge markup and the
/// headless driver).
fn needle_ids(kind: PrefMediaKind) -> (&'static str, &'static str) {
    match kind {
        PrefMediaKind::Video => (VIDEO_NEEDLE_ID, VIDEO_READOUT_ID),
        PrefMediaKind::Audio => (AUDIO_NEEDLE_ID, AUDIO_READOUT_ID),
        PrefMediaKind::Screen => (SCREEN_NEEDLE_ID, SCREEN_READOUT_ID),
    }
}

// ── components ────────────────────────────────────────────────────

/// A single analog VU needle gauge with a live numeric readout below it.
///
/// The arc + ticks are static SVG; only the `<line>` needle and the readout text
/// node are mutated at runtime (by [`ReceivedQualityDriver`]'s rAF loop) via
/// direct DOM writes, so this component itself never re-renders per frame.
#[component]
fn VuGauge(
    testid: &'static str,
    needle_id: &'static str,
    readout_id: &'static str,
    label: &'static str,
    initial_deg: f32,
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
                // Dial arc.
                path {
                    class: "perf-vu-arc",
                    d: "M 14 64 A 50 50 0 0 1 106 64",
                    fill: "none",
                    stroke_width: "2",
                }
                // Tick marks across the sweep.
                for i in 0..7 {
                    {
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
                circle { class: "perf-vu-hub", cx: "60", cy: "64", r: "4" }
            }
            div { class: "perf-vu-label", "{label}" }
            // The readout is the accessible value: announced to screen readers
            // via aria-live when the received quality changes.
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

/// Headless driver for the three received-quality needles. Renders **nothing** —
/// it only owns the single ~4 Hz `requestAnimationFrame` loop that reads
/// `received_layer_snapshot(kind)` per kind and writes the needle rotations +
/// readouts straight to the DOM nodes **by id** (so the gauges can live anywhere
/// in the tree, e.g. each inside its own threshold section). No per-frame
/// re-render. The loop self-cancels when the driver unmounts.
#[component]
fn ReceivedQualityDriver(reader: ReceivedReader) -> Element {
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
                        // Always write — including the `None`/not-receiving case,
                        // which resets the needle to the left peg + placeholder,
                        // so a stream stopped after the panel opened never freezes.
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
            }) as Box<dyn FnMut()>);

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
///
/// Two overlaid native `<input type="range">` elements (full keyboard / focus
/// for free), stacked into one track by CSS. The thumbs cannot cross. Left→right
/// is increasing received quality (index 0 = lowest, top = highest). Always
/// interactive (never disabled) — Auto is conveyed by the green toggle + thumbs
/// pinned at the extremes, not by disabling.
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

    // Fill highlight between the thumbs (percent of track).
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
            // Lowest-end (left) label.
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
            // Highest-end (right) label.
            span { class: "perf-range-end-label", "{index_label(kind, top)}" }
        }
    }
}

/// One kind's threshold section: its live needle gauge beside the dual-thumb
/// range slider, with the header (Fixed badge + Auto toggle + "?" help popover)
/// and the live range text.
///
/// **Auto** is an explicit per-kind toggle (`aria-pressed`): when active
/// (default) the slider stays fully interactive with both thumbs pinned at the
/// extremes (full range) and the encoder bounds are `(None, None)`. The only
/// Auto-on cue is the green button + thumbs at the ends. Clicking the button
/// toggles Auto; dragging a thumb inward turns Auto off.
#[allow(clippy::too_many_arguments)]
#[component]
fn ThresholdGroup(
    kind: PrefMediaKind,
    title: &'static str,
    stream_noun: &'static str,
    id_prefix: &'static str,
    min_testid: &'static str,
    max_testid: &'static str,
    auto_testid: &'static str,
    fixed_testid: &'static str,
    help_testid: &'static str,
    help_label: &'static str,
    help_body: &'static str,
    vu_testid: &'static str,
    vu_label: &'static str,
    vu_initial_deg: f32,
    vu_initial_readout: String,
    sub: KindReceivePref,
    open_help: Signal<Option<&'static str>>,
    on_change: EventHandler<KindReceivePref>,
) -> Element {
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
                        // Transparent full-viewport scrim: any outside click closes.
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
                if is_fixed {
                    span {
                        class: "perf-fixed-badge",
                        "data-testid": fixed_testid,
                        title: "Pinned to a single layer — this stream won't adapt",
                        "aria-label": "received {stream_noun} quality pinned to a single layer",
                        "Fixed"
                    }
                }
                button {
                    r#type: "button",
                    class: auto_button_class,
                    "data-testid": auto_testid,
                    "aria-pressed": if sub.auto { "true" } else { "false" },
                    "aria-label": "Automatic received {stream_noun} quality",
                    title: if sub.auto {
                        "Automatic (full range) — click to set manual receive limits"
                    } else {
                        "Manual limits — click for fully automatic receive quality"
                    },
                    onclick: move |_| {
                        let next = if sub.auto {
                            // Turn Auto OFF: keep current (extreme) thumbs as manual.
                            KindReceivePref { auto: false, ..sub }
                        } else {
                            // Turn Auto ON: snap to full range (bounds cleared).
                            KindReceivePref { min: None, max: None, auto: true }
                        };
                        on_change.call(next);
                    },
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
                        kind,
                        id_prefix,
                        min_testid,
                        max_testid,
                        stream_noun,
                        sel,
                        on_change: move |s: RangeSel| {
                            let (min, max) = thumbs_to_bounds(kind, s);
                            // A drag implies manual mode.
                            on_change.call(KindReceivePref { min, max, auto: false });
                        },
                    }
                    p {
                        class: "perf-range-value",
                        "data-testid": "{id_prefix}-range-value",
                        "aria-live": "polite",
                        "Receiving up to: {range_str}"
                    }
                }
            }
        }
    }
}

/// The RECEIVE Performance settings panel body: three per-kind threshold
/// sections (each with its received-quality needle), driven by one headless
/// rAF loop.
///
/// `pref` is the current persisted preference (controlled by the parent). On any
/// change the panel derives the new per-kind sub-preference and calls
/// `on_change`; the parent persists it and pushes the effective bounds to the
/// client. The panel is otherwise stateless.
#[component]
pub fn PerformanceSettingsPanel(
    pref: ReceivePreference,
    on_change: EventHandler<(PrefMediaKind, KindReceivePref)>,
    reader: ReceivedReader,
) -> Element {
    // First-paint gauge values (before the rAF driver ticks).
    let gv = gauge_state(reader.read(PrefMediaKind::Video).as_ref());
    let ga = gauge_state(reader.read(PrefMediaKind::Audio).as_ref());
    let gs = gauge_state(reader.read(PrefMediaKind::Screen).as_ref());

    // Which section's help popover is open. Shared so opening one closes others.
    let open_help: Signal<Option<&'static str>> = use_signal(|| None);

    rsx! {
        h3 { class: "settings-section-title", "Performance" }
        p { class: "settings-section-description",
            "Limit the quality you receive from other participants to save your "
            "download bandwidth. Each stream auto-adapts within the range based on "
            "your network; the needle shows what you're currently receiving."
        }

        // Headless driver: one ~4 Hz rAF loop updates all three needles by id.
        ReceivedQualityDriver { reader }

        // ── Video Thresholds ──
        ThresholdGroup {
            kind: PrefMediaKind::Video,
            title: "Video Thresholds",
            stream_noun: "video",
            id_prefix: "perf-video",
            min_testid: TESTID_VIDEO_RANGE_MIN,
            max_testid: TESTID_VIDEO_RANGE_MAX,
            auto_testid: TESTID_VIDEO_AUTO,
            fixed_testid: "perf-video-fixed-badge",
            help_testid: TESTID_VIDEO_HELP,
            help_label: "About received video quality",
            help_body: "Limits the camera-video quality you pull from other participants, to save YOUR download bandwidth. The call automatically picks a layer within this range based on your network. The needle shows what you're receiving now.",
            vu_testid: TESTID_VU_VIDEO,
            vu_label: "Received video",
            vu_initial_deg: gv.deg,
            vu_initial_readout: gv.text.clone(),
            sub: pref.video,
            open_help,
            on_change: move |sub: KindReceivePref| {
                on_change.call((PrefMediaKind::Video, sub));
            },
        }

        // ── Audio Thresholds ──
        ThresholdGroup {
            kind: PrefMediaKind::Audio,
            title: "Audio Thresholds",
            stream_noun: "audio",
            id_prefix: "perf-audio",
            min_testid: TESTID_AUDIO_RANGE_MIN,
            max_testid: TESTID_AUDIO_RANGE_MAX,
            auto_testid: TESTID_AUDIO_AUTO,
            fixed_testid: "perf-audio-fixed-badge",
            help_testid: TESTID_AUDIO_HELP,
            help_label: "About received audio quality",
            help_body: "Limits the audio quality you pull from other participants. The call adapts within this range based on your network. Note: audio is already low-bandwidth, so the download savings here are small.",
            vu_testid: TESTID_VU_AUDIO,
            vu_label: "Received audio",
            vu_initial_deg: ga.deg,
            vu_initial_readout: ga.text.clone(),
            sub: pref.audio,
            open_help,
            on_change: move |sub: KindReceivePref| {
                on_change.call((PrefMediaKind::Audio, sub));
            },
        }

        // ── Shared content Thresholds ──
        ThresholdGroup {
            kind: PrefMediaKind::Screen,
            title: "Shared content Thresholds",
            stream_noun: "shared content",
            id_prefix: "perf-screen",
            min_testid: TESTID_SCREEN_RANGE_MIN,
            max_testid: TESTID_SCREEN_RANGE_MAX,
            auto_testid: TESTID_SCREEN_AUTO,
            fixed_testid: "perf-screen-fixed-badge",
            help_testid: TESTID_SCREEN_HELP,
            help_label: "About received shared-content quality",
            help_body: "Limits the quality of screen / shared-content you pull from other participants, to save YOUR download bandwidth. This is independent of camera video and adapts on its own within the range.",
            vu_testid: TESTID_VU_SCREEN,
            vu_label: "Received shared content",
            vu_initial_deg: gs.deg,
            vu_initial_readout: gs.text.clone(),
            sub: pref.screen,
            open_help,
            on_change: move |sub: KindReceivePref| {
                on_change.call((PrefMediaKind::Screen, sub));
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_counts_and_top_index_per_kind() {
        assert_eq!(layer_count(PrefMediaKind::Video), 3);
        assert_eq!(layer_count(PrefMediaKind::Screen), 3);
        assert_eq!(layer_count(PrefMediaKind::Audio), 2);
        assert_eq!(top_index(PrefMediaKind::Video), 2);
        assert_eq!(top_index(PrefMediaKind::Audio), 1);
    }

    #[test]
    fn index_label_receive_convention_not_inverted() {
        // index 0 = LOWEST quality (left), top index = HIGHEST (right).
        assert_eq!(index_label(PrefMediaKind::Video, 0), "360p");
        assert_eq!(index_label(PrefMediaKind::Video, 1), "540p");
        assert_eq!(index_label(PrefMediaKind::Video, 2), "720p");
        assert_eq!(index_label(PrefMediaKind::Screen, 0), "low");
        assert_eq!(index_label(PrefMediaKind::Screen, 2), "high");
        assert_eq!(index_label(PrefMediaKind::Audio, 0), "low (24k)");
        assert_eq!(index_label(PrefMediaKind::Audio, 1), "high (50k)");
        // Out of range → "?".
        assert_eq!(index_label(PrefMediaKind::Audio, 5), "?");
    }

    #[test]
    fn thumbs_both_extremes_is_none_none() {
        // Video full ladder: left at 0, right at top (2) → no bounds = Auto.
        let sel = RangeSel {
            min_pos: 0,
            max_pos: 2,
        };
        assert_eq!(thumbs_to_bounds(PrefMediaKind::Video, sel), (None, None));
        // Audio full ladder.
        let sel_a = RangeSel {
            min_pos: 0,
            max_pos: 1,
        };
        assert_eq!(thumbs_to_bounds(PrefMediaKind::Audio, sel_a), (None, None));
    }

    #[test]
    fn thumbs_each_extreme_individually_is_none_on_that_end() {
        // Left at 0, right pulled in to 1 → min None, max Some(1).
        let sel = RangeSel {
            min_pos: 0,
            max_pos: 1,
        };
        assert_eq!(thumbs_to_bounds(PrefMediaKind::Video, sel), (None, Some(1)));
        // Left pulled in to 1, right at top 2 → min Some(1), max None.
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
    fn thumbs_interior_pair_maps_to_both_bounds() {
        // For video, the only interior-on-both-ends case collapses min=max=1.
        let sel = RangeSel {
            min_pos: 1,
            max_pos: 1,
        };
        assert_eq!(
            thumbs_to_bounds(PrefMediaKind::Video, sel),
            (Some(1), Some(1))
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
        // None/None → thumbs at extremes (0..top).
        let sel = bounds_to_thumbs(PrefMediaKind::Video, None, None);
        assert_eq!(
            sel,
            RangeSel {
                min_pos: 0,
                max_pos: 2
            }
        );
        // Audio None/None → 0..1.
        let sel_a = bounds_to_thumbs(PrefMediaKind::Audio, None, None);
        assert_eq!(
            sel_a,
            RangeSel {
                min_pos: 0,
                max_pos: 1
            }
        );
    }

    #[test]
    fn thumbs_cannot_cross() {
        let sel = RangeSel {
            min_pos: 1,
            max_pos: 2,
        };
        // Drag min past max → clamps to max.
        assert_eq!(
            set_min_thumb(sel, 5),
            RangeSel {
                min_pos: 2,
                max_pos: 2
            }
        );
        // Drag max below min → clamps to min.
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
        // Full video ladder.
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
        // Partial range.
        assert_eq!(
            span_text(
                PrefMediaKind::Video,
                RangeSel {
                    min_pos: 0,
                    max_pos: 1
                }
            ),
            "360p – 540p"
        );
        // Collapsed to single layer.
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
        // Audio full ladder.
        assert_eq!(
            span_text(
                PrefMediaKind::Audio,
                RangeSel {
                    min_pos: 0,
                    max_pos: 1
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
            // Effective bounds = full range.
            assert_eq!(pref.effective_bounds(kind), (None, None));
        }
    }

    #[test]
    fn auto_flag_forces_none_bounds_regardless_of_indices() {
        // Auto on but with stale stored indices → effective bounds are None/None.
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
        // Other kinds remain Auto.
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
        assert!(!clean.video.auto); // flag preserved
        assert_eq!(clean.audio.min, Some(0)); // kept (0 <= top(1))
        assert_eq!(clean.audio.max, None); // 7 > top(1) dropped
    }

    #[test]
    fn needle_deg_lowest_pegs_left_highest_pegs_right() {
        // Receive convention: layer 0 (lowest) → -MAX, top → +MAX.
        assert_eq!(needle_deg(0, 3), -MAX_NEEDLE_DEG);
        assert_eq!(needle_deg(2, 3), MAX_NEEDLE_DEG);
        // Middle ~ 0.
        assert!(needle_deg(1, 3).abs() < 0.001);
        // Audio: layer 0 → -MAX, 1 → +MAX.
        assert_eq!(needle_deg(0, 2), -MAX_NEEDLE_DEG);
        assert_eq!(needle_deg(1, 2), MAX_NEEDLE_DEG);
    }

    #[test]
    fn needle_deg_clamps_and_single_layer_centers() {
        assert_eq!(needle_deg(99, 3), MAX_NEEDLE_DEG); // clamped to top
        assert_eq!(needle_deg(0, 1), 0.0); // single layer centers
        assert_eq!(needle_deg(0, 0), 0.0); // degenerate
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
            layer_count: 2,
            width: 0,
            height: 0,
            kbps: 24,
        };
        assert_eq!(format_readout(&a), "L1/2 · 24 kbps");
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
        // None → empty state: needle left peg + "Not receiving".
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
        // A pref persisted before the `auto` flag / a missing kind must load as
        // fully-Auto (serde defaults).
        let legacy = r#"{"video":{"min":1,"max":2}}"#;
        let p: ReceivePreference = serde_json::from_str(legacy).unwrap();
        // video.auto defaulted true → effective bounds None/None despite indices.
        assert!(p.video.auto);
        assert_eq!(p.effective_bounds(PrefMediaKind::Video), (None, None));
        // Missing audio/screen → default Auto.
        assert!(p.audio.auto);
        assert!(p.screen.auto);
    }
}
