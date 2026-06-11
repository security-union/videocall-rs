/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Unified Performance settings panel — SEND quality bounds (#961) AND RECEIVE
//! layer bounds (#989 simulcast), with live "Sending" and "Receiving" bar-meters.
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
//! # The bar-meters
//!
//! Each kind shows a **Sending** bar-meter (from [`SnapshotReader`], the live
//! encoder snapshot) and a **Receiving** bar-meter (from
//! [`receive::ReceivedReader`], the live `received_layer_snapshot`). Each meter is
//! four vertical bars whose lit count is a level `0..=4`, plus a one-line readout.
//! Two headless rAF drivers ([`QualityVuMeterDriver`] for send,
//! [`receive::ReceivedQualityDriver`] for receive) poll at ~4 Hz and write each
//! meter's `data-level` attribute + readout text straight to the DOM by id
//! (bypassing the Dioxus diff). Send and receive meters use DISTINCT DOM ids so
//! the two drivers never fight over the same node.

use dioxus::prelude::*;
use std::rc::Rc;
use videocall_client::{
    DegradeReason, LiveQualitySnapshot, PeerReceiveDiag, PrefMediaKind, QualityState,
    ReceivedLayerSnapshot, ScreenQualitySnapshot, SimulcastSendSnapshot,
};
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

// The Receive | Send direction toggle was removed in the #1095 redesign: both
// directions now render side-by-side in each per-kind card, so there is no longer
// a single "active direction" to track. (`Direction` enum + its testids deleted.)

// ── live diagnostics (issue #1095 observability) ───────────────────
//
// A collapsible "Live diagnostics" disclosure at the bottom of the panel surfaces
// the real simulcast/AQ state: the effective layer setting, the layers being
// SENT (+ per-layer bitrate/resolution), and what is being RECEIVED per peer.
// All values come from the live encoder/client accessors via `DiagnosticsReader`;
// the disclosure re-renders (throttled, only while open) so a variable-length
// per-peer list can be shown without per-id DOM writes.

/// The effective simulcast layer setting, for the diagnostics summary line.
///
/// `effective = min(flag, capability)`. Video/screen share the CPU capability
/// ceiling; audio has its own. Pure data so the formatter is host-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SimulcastSummary {
    /// `experimentalSimulcastMaxLayers` runtime flag value.
    pub flag: u32,
    /// Device CPU capability ceiling for video/screen.
    pub video_capability: u32,
    /// Device capability ceiling for audio (its own ladder size).
    pub audio_capability: u32,
    /// Effective video/screen layers = `min(flag, video_capability)`.
    pub effective_video: u32,
    /// Effective audio layers = `min(flag, audio_capability)`.
    pub effective_audio: u32,
}

/// Format the simulcast effective-setting summary line, e.g.
/// `"Video/Screen: 3 layers (flag 3 × device cap 3) · Audio: 3 layers"`. When a
/// kind is effectively single-layer it reads "off (1 layer)". Pure / host-tested.
pub fn format_simulcast_summary(s: &SimulcastSummary) -> String {
    let kind_str = |effective: u32, cap: u32| -> String {
        if effective <= 1 {
            "off (1 layer)".to_string()
        } else {
            format!("{effective} layers (flag {} × device cap {})", s.flag, cap)
        }
    };
    format!(
        "Video/Screen: {} · Audio: {}",
        kind_str(s.effective_video, s.video_capability),
        kind_str(s.effective_audio, s.audio_capability),
    )
}

/// Format one SEND simulcast layer line, e.g. `"Low · 640×360 · 400 kbps"`. The
/// `layer_id` is the internal 0-based id; `count` is the ladder size. The
/// DISPLAYED label is the quality name (Low/Medium/High) via
/// [`layer_quality_label`] — the internal id stays 0-based for e2e/protobuf. Pure.
pub fn format_send_layer(
    layer_id: u32,
    count: u32,
    width: u32,
    height: u32,
    bitrate_kbps: u32,
) -> String {
    let name = layer_quality_label(layer_id, count, false);
    format!("{name} · {width}×{height} · {bitrate_kbps} kbps")
}

/// Format the SEND simulcast header for a kind, e.g.
/// `"3 of 3 layers active"` (active vs effective). For single-stream it reads
/// `"Single layer"` (capitalized to match the footer's display copy). Pure /
/// host-tested.
pub fn format_send_header(snap: &SimulcastSendSnapshot) -> String {
    if !snap.simulcast_active {
        "Single layer".to_string()
    } else {
        format!(
            "{} of {} layers active",
            snap.active_layers, snap.effective_layers
        )
    }
}

/// Format one RECEIVE per-kind line for a peer, e.g. `"video M · 2/3 · 1280×720"`
/// or `"audio L · 1/3 · 24 kbps"`. The quality LETTER (Low/Med/High → L/M/H) is
/// followed by the 1-based position/total. Returns `None` when the kind is not
/// flowing. Pure / host-tested.
pub fn format_peer_kind_line(
    kind_label: &str,
    snap: Option<&ReceivedLayerSnapshot>,
) -> Option<String> {
    let s = snap?;
    let letter = layer_quality_label(s.layer_index, s.layer_count, true);
    let detail = if matches!(s.kind, PrefMediaKind::Audio) {
        format!("{} kbps", s.kbps)
    } else {
        format!("{}×{}", s.width, s.height)
    };
    Some(format!(
        "{kind_label} {letter} · {}/{} · {detail}",
        s.layer_index + 1,
        s.layer_count
    ))
}

/// A cloneable, `PartialEq`-able handle around the live diagnostics readers
/// (issue #1095). Mirrors [`SnapshotReader`]: compared by `Rc` pointer identity,
/// built once per `Host` mount, so it never spuriously re-renders the panel.
///
/// Bundles the four reads the diagnostics disclosure needs: the effective-setting
/// summary, the SEND simulcast snapshot for video and (optional) screen, and the
/// per-peer RECEIVE diagnostics.
#[derive(Clone)]
pub struct DiagnosticsReader {
    /// The effective simulcast setting (flag × capability), captured at mount.
    pub summary: SimulcastSummary,
    /// Reads the camera's live SEND simulcast snapshot (`None` while the camera
    /// is off — gated on `prev_video_enabled`, mirroring the quality meter and
    /// the screen path; otherwise stale "N of M layers active" would render with
    /// the camera disabled, since the encoder atomics aren't reset on stop).
    pub send_video: Rc<dyn Fn() -> Option<SimulcastSendSnapshot>>,
    /// Reads the screen's live SEND simulcast snapshot (`None` while not sharing).
    pub send_screen: Rc<dyn Fn() -> Option<SimulcastSendSnapshot>>,
    /// Reads the per-peer RECEIVE diagnostics.
    pub per_peer_receive: Rc<dyn Fn() -> Vec<PeerReceiveDiag>>,
}

impl DiagnosticsReader {
    /// An inert reader (single-layer summary, empty snapshots) for tests / when
    /// diagnostics aren't wired.
    pub fn none() -> Self {
        DiagnosticsReader {
            summary: SimulcastSummary::default(),
            send_video: Rc::new(|| None),
            send_screen: Rc::new(|| None),
            per_peer_receive: Rc::new(Vec::new),
        }
    }
}

impl PartialEq for DiagnosticsReader {
    fn eq(&self, other: &Self) -> bool {
        // Summary is `Copy`/`Eq`; the closures are compared by allocation
        // identity (one stable set per Host mount).
        self.summary == other.summary
            && Rc::ptr_eq(&self.send_video, &other.send_video)
            && Rc::ptr_eq(&self.send_screen, &other.send_screen)
            && Rc::ptr_eq(&self.per_peer_receive, &other.per_peer_receive)
    }
}

/// Everything [`PerformanceSettingsPanel`] needs that only `Host` can supply,
/// bundled so it can travel from `Host` (which owns the encoders + preference
/// signals) to the Diagnostics drawer, which is a SIBLING of `Host` in the
/// attendants tree and cannot reach those internals directly (#1131 unify).
///
/// Before the merge the panel lived inside `Host → DeviceSettingsModal`, so it
/// received these as ordinary props. Now the panel mounts inside the Diagnostics
/// drawer, so `Host` builds ONE handle per mount and publishes it through a sink
/// signal (mirroring the existing `publish_diagnostics_reader` mechanism); the
/// attendants forward it into `Diagnostics`, which reads the preference signals
/// and hands the panel its existing value props.
///
/// The two preference fields are `Signal`s (not values): the panel's slider
/// positions are reactive, and a `Signal` read subscribes whatever component
/// reads it — even across the component-tree boundary — so the controls stay
/// live without re-plumbing the panel's value-typed props. Preference edits are
/// user-driven (not tick-rate), so reading them in the drawer body does not
/// reintroduce the per-tick re-render the scoped 250 ms ticks avoid (#1128).
///
/// `Clone` is cheap (`Copy` signals + `Rc` bumps). `PartialEq` compares the
/// signals by identity (`Signal: Eq`) and the closures/readers by `Rc` pointer,
/// so a stable per-`Host`-mount handle never spuriously re-renders the drawer.
#[derive(Clone)]
pub struct PerfControlsHandle {
    /// Persisted SEND quality-bounds preference (drives the send slider thumbs).
    pub performance_preference: Signal<PerformancePreference>,
    /// Persisted RECEIVE layer-bounds preference (drives the receive thumbs).
    pub receive_preference: Signal<ReceivePreference>,
    /// Apply a changed SEND preference (persist + push to encoders).
    pub on_change: Rc<dyn Fn(PerformancePreference)>,
    /// Apply a changed RECEIVE preference for one kind (persist + push to client).
    pub on_receive_change: Rc<dyn Fn((PrefMediaKind, KindReceivePref))>,
    /// Live camera SEND quality snapshot reader (for the "Sending" video meter).
    pub read_snapshot: SnapshotReader,
    /// Live screen SEND quality snapshot reader (for the "Sending" screen meter).
    pub read_screen_snapshot: ScreenSnapshotReader,
    /// Per-kind RECEIVE-layer snapshot reader (for the "Receiving" meters).
    pub received_reader: ReceivedReader,
    /// Live simulcast/AQ diagnostics for the per-card summary lines + strip.
    pub diagnostics_reader: DiagnosticsReader,
    /// Effective VIDEO ladder depth (`min(flag, CPU capability)`).
    pub video_layer_max: usize,
    /// Effective SCREEN ladder depth (shares the video CPU capability ceiling).
    pub screen_layer_max: usize,
    /// Effective AUDIO ladder depth (NOT CPU-clamped — audio encode is cheap).
    pub audio_layer_max: usize,
}

impl PartialEq for PerfControlsHandle {
    fn eq(&self, other: &Self) -> bool {
        // Signals compare by identity (`Signal: Eq`); the closures/readers by
        // allocation identity (one stable set per Host mount); the layer-max
        // counts by value (deterministic per session).
        self.performance_preference == other.performance_preference
            && self.receive_preference == other.receive_preference
            && Rc::ptr_eq(&self.on_change, &other.on_change)
            && Rc::ptr_eq(&self.on_receive_change, &other.on_receive_change)
            && self.read_snapshot == other.read_snapshot
            && self.read_screen_snapshot == other.read_screen_snapshot
            && self.received_reader == other.received_reader
            && self.diagnostics_reader == other.diagnostics_reader
            && self.video_layer_max == other.video_layer_max
            && self.screen_layer_max == other.screen_layer_max
            && self.audio_layer_max == other.audio_layer_max
    }
}

/// testid for the global effective-setting strip (one line under the intro).
pub const TESTID_SIMULCAST_STRIP: &str = "perf-simulcast-strip";

// The expandable per-row diagnostics (the send ladder + per-peer receive
// breakdown, with the `{id_prefix}-diag*` testids) moved OUT of this panel into
// the Diagnostics panel's "Simulcast layers" section (#1095 redesign). The panel
// now keeps only the always-visible per-card SUMMARY line per side.

/// Format the SLIM global simulcast strip line (issue #1095 redesign): compact
/// copy `"Simulcast: 3 layers (device cap 3)"`, or `"Simulcast: off"` when the
/// effective video layer count is 1. The full `flag N × device cap N` text lives
/// in the strip's `title`/aria (via [`format_simulcast_summary`]). Pure.
///
/// The strip describes the VIDEO/SCREEN effective layers (the headline setting);
/// audio's separate count is in the full text. Uses `effective_video` since
/// video/screen share the CPU ceiling and are the dominant cost.
pub fn format_simulcast_summary_compact(s: &SimulcastSummary) -> String {
    if s.effective_video <= 1 {
        "Simulcast: off".to_string()
    } else {
        format!(
            "Simulcast: {} layers (device cap {})",
            s.effective_video, s.video_capability
        )
    }
}

/// Sum of the ACTIVE layers' target bitrates (kbps) for a SEND snapshot — the
/// total uplink the simulcast publish currently costs. `0` in single-stream mode
/// (the per-layer bitrate atomics are empty then). Pure / host-tested.
pub fn format_send_total_kbps(snap: &SimulcastSendSnapshot) -> u32 {
    snap.layers
        .iter()
        .take(snap.active_layers as usize)
        .map(|l| l.bitrate_kbps)
        .sum()
}

/// Short resolution label for a SEND layer chip, e.g. `360×640`→`"360p"`. Uses
/// the SHORTER dimension as the "p" value so portrait and landscape both read
/// sensibly. Falls back to `"{w}×{h}"` if a dimension is 0. Pure / host-tested.
pub fn format_send_layer_short(width: u32, height: u32) -> String {
    if width == 0 || height == 0 {
        return format!("{width}×{height}");
    }
    format!("{}p", width.min(height))
}

/// Compact bitrate label: `400`→`"400k"`, `1400`→`"1.4M"`, `2500`→`"2.5M"`.
/// Sub-1000 kbps render as `"{n}k"`; ≥1000 as `"{x.y}M"` (one decimal, trailing
/// `.0` trimmed → `"2M"`). Pure / host-tested.
pub fn format_kbps_compact(kbps: u32) -> String {
    if kbps < 1000 {
        return format!("{kbps}k");
    }
    let mbps = kbps as f32 / 1000.0;
    // One decimal, then trim a trailing ".0" so 2000 -> "2M" not "2.0M".
    let s = format!("{mbps:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}M")
}

/// Position→quality label for simulcast tiers (#1222). `index` is the 0-based
/// ladder position; `count` the ladder size; `compact` returns the single letter
/// for space-constrained chips. Degenerate: count<=1 → "Single" (compact "1").
/// Top index (`count-1`) is always High; everything between 0 and top is Medium.
/// Pure / host-testable.
pub fn layer_quality_label(index: u32, count: u32, compact: bool) -> &'static str {
    if count <= 1 {
        return if compact { "1" } else { "Single" };
    }
    if count == 2 {
        return match (index, compact) {
            (0, false) => "Low",
            (0, true) => "L",
            (_, false) => "High",
            (_, true) => "H",
        };
    }
    let top = count - 1;
    match (index, compact) {
        (0, false) => "Low",
        (0, true) => "L",
        (i, false) if i >= top => "High",
        (i, true) if i >= top => "H",
        (_, false) => "Medium",
        (_, true) => "M",
    }
}

/// Format the RECEIVE per-kind layer spread across peers by quality LETTER, e.g.
/// `"L–H"`, or a single letter `"H"` when every peer is on the same layer.
/// `layers` is the list of 0-based `layer_index` values; `count` is the ladder
/// size for the kind (the basis for the quality letters). Empty → empty string
/// (caller renders the "not receiving" state). Pure / host-tested.
pub fn format_receive_spread(layers: &[u32], count: u32) -> String {
    let Some(&first) = layers.first() else {
        return String::new();
    };
    let lo = layers.iter().copied().min().unwrap_or(first);
    let hi = layers.iter().copied().max().unwrap_or(first);
    let lo_letter = layer_quality_label(lo, count, true);
    let hi_letter = layer_quality_label(hi, count, true);
    if lo == hi {
        hi_letter.to_string()
    } else {
        format!("{lo_letter}\u{2013}{hi_letter}")
    }
}

/// Total active uplink as a human "Mbps" string, e.g. 2600→`"2.6 Mbps"`,
/// 400→`"0.4 Mbps"`. Used in the SEND per-row summary line. Pure / host-tested.
pub fn format_mbps(kbps: u32) -> String {
    format!("{:.1} Mbps", kbps as f32 / 1000.0)
}

// ── per-card summary lines (#1095 redesign, §3 copy) ───────────────
//
// One always-visible summary line per side, under each slider. Templates carry
// the spec's literal phrasings; live numbers are filled from the snapshots. Pure
// so the copy is a host-tested source of truth (a wording change breaks a test).

/// VIDEO SEND summary, e.g. `"Sending 3 of 3 layers · 540p–720p"`. Camera off
/// (`snap` is `None`) → `"Camera — off"`. Single-stream → `"Sending single
/// layer · {res}"` (the one adaptive layer's short res, when known). The res
/// span uses the SEND snapshot's per-layer short resolutions across the EFFECTIVE
/// (offered) layers, lowest→highest. Pure / host-tested.
pub fn format_video_send_summary(snap: Option<&SimulcastSendSnapshot>) -> String {
    let Some(s) = snap else {
        return "Camera — off".to_string();
    };
    if !s.simulcast_active {
        return match s.layers.first() {
            Some(l) => format!(
                "Sending single layer · {}",
                format_send_layer_short(l.width, l.height)
            ),
            None => "Sending single layer".to_string(),
        };
    }
    let span = send_layer_res_span(s);
    if span.is_empty() {
        format!(
            "Sending {} of {} layers",
            s.active_layers, s.effective_layers
        )
    } else {
        format!(
            "Sending {} of {} layers · {span}",
            s.active_layers, s.effective_layers
        )
    }
}

/// The short-resolution span across a SEND snapshot's layers, lowest→highest,
/// e.g. `"540p–720p"`, or a single `"720p"` when all layers share a resolution,
/// or `""` when no resolutions are known yet (atomics not ticked). Pure.
pub fn send_layer_res_span(snap: &SimulcastSendSnapshot) -> String {
    let mut shorts: Vec<u32> = snap
        .layers
        .iter()
        .filter(|l| l.width > 0 && l.height > 0)
        .map(|l| l.width.min(l.height))
        .collect();
    if shorts.is_empty() {
        return String::new();
    }
    shorts.sort_unstable();
    let lo = *shorts.first().unwrap();
    let hi = *shorts.last().unwrap();
    if lo == hi {
        format!("{lo}p")
    } else {
        format!("{lo}p–{hi}p")
    }
}

/// One SEND-side rung pip for the §2 always-visible rung strip (issue #1131).
/// Lowest layer first. `active` distinguishes a filled (publishing) pip from a
/// shed (bitrate-0, ghosted/dashed) one. `res_label` sits under every pip;
/// `kbps_label` is `Some` ONLY on the top ACTIVE pip (so the strip shows the
/// uplink of the best layer currently flowing, without repeating it per rung).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendRung {
    /// This layer's simulcast id (0 = base / lowest).
    pub layer_id: u32,
    /// `true` when the layer is being encoded + sent; `false` for a shed layer.
    pub active: bool,
    /// Short resolution label under the pip, e.g. `"540p"`.
    pub res_label: String,
    /// Compact bitrate label, present only on the top active pip, e.g. `"600k"`.
    pub kbps_label: Option<String>,
}

/// The §2 SEND rung strip for AUDIO when it was a single-pip tier control.
///
/// Test-only now: audio moved to the layer-count control (`SendLayerCell`), which
/// builds its own multi-pip selection-driven strip via [`layer_send_rungs`] like
/// video/screen. Retained as the tested single-pip mapper. `best` is the
/// best-allowed tier index (`None`/Auto → tier 0 = best). Pure / host-tested.
#[allow(dead_code)]
pub fn audio_send_rung(best: Option<usize>) -> SendRung {
    let idx = match best {
        Some(i) if i < AUDIO_TIER_LABELS.len() => i,
        _ => 0,
    };
    SendRung {
        // A single conceptual rung; its id is the tier index for a stable testid.
        layer_id: idx as u32,
        active: true,
        res_label: AUDIO_TIER_LABELS[idx].to_string(),
        kbps_label: None,
    }
}

/// Build the SEND rung strip for a VIDEO/SCREEN layer-count slider directly from
/// the user's CEILING selection (not the live encoder snapshot), so dragging the
/// ceiling thumb updates the active/shed pips IMMEDIATELY rather than waiting for
/// the next AQ tick + snapshot.
///
/// `labels` is the kind's lowest-first rung labels (one per effective layer);
/// `ceiling_pos` is the ceiling thumb position (0-based layer index). A pip is
/// `active` iff `layer_id <= ceiling_pos` — i.e. layers `L0..=L{ceiling_pos}` are
/// published and the rest are shown shed. The base (L0) is always active (the
/// ceiling floors at 0). No kbps labels here (this strip reflects the user's
/// chosen COUNT, not live bitrate — the live summary line carries flowing rates).
/// Pure / host-tested.
pub fn layer_send_rungs(labels: &[&'static str], ceiling_pos: usize) -> Vec<SendRung> {
    labels
        .iter()
        .enumerate()
        .map(|(i, label)| SendRung {
            layer_id: i as u32,
            active: i <= ceiling_pos,
            res_label: (*label).to_string(),
            kbps_label: None,
        })
        .collect()
}

/// The strip's `role="img"` aria-label summarizing the rung state for SR users
/// (the individual pips are decorative). E.g. `"Sending 2 of 3 layers"`, or
/// `"Sending 1 layer"` for a single pip. Pure / host-tested.
pub fn send_rungs_aria(rungs: &[SendRung]) -> String {
    let total = rungs.len();
    let active = rungs.iter().filter(|r| r.active).count();
    if total <= 1 {
        "Sending 1 layer".to_string()
    } else {
        format!("Sending {active} of {total} layers")
    }
}

/// VIDEO RECEIVE summary, e.g. `"Pulling up to high quality · L–H across 4
/// peers"`. No peers → `"Not receiving video"`. `layers` is the per-peer
/// `layer_index` list; `count` is the video ladder size (the basis for the
/// quality letters via [`format_receive_spread`]). Pure / host-tested.
pub fn format_video_receive_summary(layers: &[u32], count: u32) -> String {
    let n = layers.len();
    if n == 0 {
        return "Not receiving video".to_string();
    }
    let spread = format_receive_spread(layers, count);
    let peers = if n == 1 {
        "1 peer".to_string()
    } else {
        format!("{n} peers")
    };
    format!("Pulling up to high quality · {spread} across {peers}")
}

/// AUDIO RECEIVE summary. No peers → `"Not receiving audio"`; otherwise the spec
/// phrase `"Pulling near-full quality"`. Pure / host-tested.
pub fn format_audio_receive_summary(n_peers: usize) -> String {
    if n_peers == 0 {
        "Not receiving audio".to_string()
    } else {
        "Pulling near-full quality".to_string()
    }
}

/// AUDIO SEND summary line for the LAYER-COUNT control — count-aware, so it tracks
/// the chosen audio layer ceiling (the bare-tier [`format_audio_send_summary`]
/// always read "Sending high quality" post-migration since audio is always Auto
/// on the tier axis now, contradicting the rung strip when the user lowers the
/// ceiling).
///
/// `layers` is the persisted audio layer COUNT (`None` = Auto / full ladder);
/// `layer_max` is the effective audio ladder depth. Derived from the SAME
/// per-kind ladder labels the slider + rung strip use
/// ([`send_layer_labels`] for `Audio`, lowest-first `["24k","32k","50k"]`), so the
/// summary's top-layer label can never drift from the rungs.
///
/// STATE-AWARE (mirrors [`format_send_layer_caption`]): when the mic is ACTIVE the
/// summary names the TOP currently-published layer (the best quality flowing) —
/// the full ladder reads "Sending up to {top}", a single base layer reads
/// "Sending {base} only". When the mic is OFF it switches to the future form
/// "Will send up to {top} when the mic is on" so it never claims to be sending
/// while nothing is captured. Pure / host-tested.
pub fn format_audio_send_layer_summary(
    layers: Option<u32>,
    layer_max: usize,
    source_active: bool,
) -> String {
    let labels = send_layer_labels(PrefMediaKind::Audio, layer_max);
    if labels.is_empty() {
        return if source_active {
            "Sending audio".to_string()
        } else {
            "Will send audio when the mic is on".to_string()
        };
    }
    // Active layers are L0..=ceiling_pos; the top published label is at ceiling_pos.
    let ceiling_pos = layer_ceiling_to_thumb_pos(layers, labels.len());
    let top = labels
        .get(ceiling_pos)
        .copied()
        .unwrap_or(labels[labels.len() - 1]);
    match (source_active, ceiling_pos == 0) {
        // Mic on, only the base layer published.
        (true, true) => format!("Sending {top} only"),
        // Mic on, multiple layers.
        (true, false) => format!("Sending up to {top}"),
        // Mic off, base only.
        (false, true) => format!("Will send {top} only when the mic is on"),
        // Mic off, multiple layers.
        (false, false) => format!("Will send up to {top} when the mic is on"),
    }
}

/// The per-kind phrase describing when an OFF source will start sending, e.g.
/// "when the camera is on". Pure (a `match`); shared by the caption + summary so
/// the wording can't drift between them.
fn source_on_phrase(kind: PrefMediaKind) -> &'static str {
    match kind {
        PrefMediaKind::Video => "when the camera is on",
        PrefMediaKind::Screen => "when sharing",
        PrefMediaKind::Audio => "when the mic is on",
    }
}

/// The SEND layer count caption ("range value" line) — STATE-AWARE about whether
/// the source is actually capturing.
///
/// - Source ACTIVE (camera on / sharing / mic on): the present-tense
///   "Sending {active} of {total} layers" (or "Sending 1 layer" for a 1-rung
///   ladder) — the live count.
/// - Source OFF: a future/conditional form using the CONFIGURED count so we never
///   claim to be "sending" when nothing is captured, e.g.
///   "Will send {active} layers when the camera is on" (the configured ceiling is
///   known from the persisted pref regardless of source state). A 1-layer ladder
///   reads "Will send 1 layer {phrase}".
///
/// `active`/`total` are layer COUNTS (`active <= total`, both ≥ 1). Pure /
/// host-tested.
pub fn format_send_layer_caption(
    kind: PrefMediaKind,
    active: usize,
    total: usize,
    source_active: bool,
) -> String {
    let layers_word = if active == 1 { "layer" } else { "layers" };
    if source_active {
        if total <= 1 {
            "Sending 1 layer".to_string()
        } else {
            format!("Sending {active} of {total} layers")
        }
    } else {
        let phrase = source_on_phrase(kind);
        format!("Will send {active} {layers_word} {phrase}")
    }
}

/// CONTENT (screen) SEND summary. Not sharing (`snap` is `None`) → `"Will send
/// up to 1080p when you share"`; sharing → `"Sending {res} · {mbps}"` (or just
/// `"Sending {res}"` before bitrates tick). Pure / host-tested.
pub fn format_content_send_summary(snap: Option<&SimulcastSendSnapshot>) -> String {
    let Some(s) = snap else {
        return "Will send up to 1080p when you share".to_string();
    };
    let res = send_layer_res_span(s);
    let total = format_send_total_kbps(s);
    match (res.is_empty(), total) {
        (false, t) if t > 0 => format!("Sending {res} · {}", format_mbps(t)),
        (false, _) => format!("Sending {res}"),
        (true, _) => "Sending screen".to_string(),
    }
}

/// CONTENT (screen) RECEIVE summary. No peer sharing → `"Nobody is sharing"`;
/// otherwise `"Pulling full quality · {letter} · {w}×{h}"` for the top-layer
/// peer, where the letter is the quality tier (L/M/H) of the received layer.
/// `top` is the highest-layer peer snapshot currently received. Pure.
pub fn format_content_receive_summary(top: Option<&ReceivedLayerSnapshot>) -> String {
    match top {
        None => "Nobody is sharing".to_string(),
        Some(s) => {
            let q = layer_quality_label(s.layer_index, s.layer_count, true);
            format!("Pulling full quality · {q} · {}×{}", s.width, s.height)
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// Issue #1131 — per-peer RECEIVE row rendering (pure / host-tested). The
// degradation REASON and the absolute quality come from the client
// (`snap.reason`, `quality_state(layer_index, full_ladder_len)`); these fns map
// them to the spec's markup data (CSS modifier, glyph, chip copy, metric text,
// and the full aria-label sentence). Kept pure so the §3/§4/§5 copy is testable
// without a DOM.
// ══════════════════════════════════════════════════════════════════════════

/// CSS modifier suffix for a quality state (`"optimal"|"medium"|"low"`), used in
/// both `.perf-q-dot--{m}` and the metric tinting. Pure.
pub fn quality_state_modifier(q: QualityState) -> &'static str {
    match q {
        QualityState::Optimal => "optimal",
        QualityState::Medium => "medium",
        QualityState::Low => "low",
    }
}

/// Non-color glyph for a quality state (§4): Optimal `●`, Medium `◐`, Low `○`.
/// The dot is `aria-hidden`; the state is also in the row's aria-label, so the
/// glyph is a redundant non-color cue, never the sole signal. Pure.
pub fn quality_state_glyph(q: QualityState) -> &'static str {
    match q {
        QualityState::Optimal => "●",
        QualityState::Medium => "◐",
        QualityState::Low => "○",
    }
}

/// Human phrase for a quality state, used inside the row aria-label sentence
/// (e.g. "optimal quality"). Pure.
pub fn quality_state_word(q: QualityState) -> &'static str {
    match q {
        QualityState::Optimal => "optimal",
        QualityState::Medium => "medium",
        QualityState::Low => "low",
    }
}

/// The reason chip's CSS modifier suffix (`"network"|"setting"|"sender"`). Pure.
pub fn reason_chip_modifier(r: DegradeReason) -> &'static str {
    match r {
        DegradeReason::Network => "network",
        DegradeReason::Setting => "setting",
        DegradeReason::Sender => "sender",
    }
}

/// The reason chip's short visible TEXT (§5). Pure.
pub fn reason_chip_text(r: DegradeReason) -> &'static str {
    match r {
        DegradeReason::Network => "Your network",
        DegradeReason::Setting => "Your setting",
        DegradeReason::Sender => "Sender",
    }
}

/// The reason chip's hover `title` (full explanation, §5). Pure.
pub fn reason_chip_title(r: DegradeReason) -> &'static str {
    match r {
        DegradeReason::Network => {
            "Your download can't sustain a higher layer right now (packet loss or congestion)."
        }
        DegradeReason::Setting => "You capped receive quality below the maximum for this stream.",
        DegradeReason::Sender => "The sender isn't publishing a higher layer right now.",
    }
}

/// The reason clause appended to the row aria-label (§5: "limited by …"). Pure.
pub fn reason_aria_clause(r: DegradeReason) -> &'static str {
    match r {
        DegradeReason::Network => "limited by your network",
        DegradeReason::Setting => "limited by your setting",
        DegradeReason::Sender => "limited by the sender",
    }
}

/// The per-peer row metric text (§3, Directive 4 SITE 6b). video/screen
/// `"{res} · ~{kbps} · {Q} · {i}/{n}"`; audio `"{kbps}k · {label} · {Q} · {i}/{n}"`,
/// where `{Q}` is the quality LETTER (L/M/H via [`layer_quality_label`]) and `n`
/// is the FULL-ladder length (so the `{i}/{n}` denominator matches the color
/// basis). `audio_label` is the receive audio rung label (e.g. "mid (32k)")
/// supplied by the caller (the receive submodule owns that mapping). Pure /
/// host-tested.
///
/// NOTE (#1222): this helper is shared with the signal-quality popup
/// (`signal_quality.rs`). The drawer layer-name rename (Directive 4) is global,
/// so the visible `L{i}/{n}` → `{Q} · {i}/{n}` swap propagates to that surface
/// too; its unit test + doc comment are updated in lockstep (Option A).
pub fn peer_row_metric(
    snap: &ReceivedLayerSnapshot,
    full_ladder_len: u32,
    audio_label: &str,
) -> String {
    let i = snap.layer_index + 1;
    // Quality LETTER (L/M/H) over the FULL ladder — the internal layer_index
    // stays 0-based; only the visible chip changes (e2e/protobuf stability).
    let q = layer_quality_label(snap.layer_index, full_ladder_len, true);
    if matches!(snap.kind, PrefMediaKind::Audio) {
        format!(
            "{}k · {} · {q} · {i}/{full_ladder_len}",
            snap.kbps, audio_label
        )
    } else {
        let res = format_send_layer_short(snap.width, snap.height);
        format!(
            "{res} · ~{} · {q} · {i}/{full_ladder_len}",
            format_kbps_compact(snap.kbps)
        )
    }
}

/// The full per-peer row aria-label sentence (§3). Color is never the sole
/// signal — this sentence carries label, kind, state, the res/bitrate, the layer
/// fraction, and (when present) the reason clause. `kind_noun` is the spoken kind
/// ("video"/"audio"/"shared content"); `res_or_bitrate` is the human detail
/// ("540p" or "32k"). Pure / host-tested.
pub fn peer_row_aria_label(
    label: &str,
    kind_noun: &str,
    q: QualityState,
    res_or_bitrate: &str,
    layer_1indexed: u32,
    full_ladder_len: u32,
    reason: Option<DegradeReason>,
) -> String {
    let base = format!(
        "{label}, receiving {kind_noun}, {} quality, {res_or_bitrate}, layer {layer_1indexed} of {full_ladder_len}",
        quality_state_word(q)
    );
    match reason {
        Some(r) => format!("{base}, {}", reason_aria_clause(r)),
        None => base,
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
    /// User SEND layer-ceiling for VIDEO — the maximum number of simulcast
    /// layers this publisher emits (the "layers published" control). `None` =
    /// Auto / no user cap (the full backpressure-governed ladder). A layer COUNT
    /// in `1..=effective_max_layers`. `#[serde(default)]` (= `None`) so prefs
    /// persisted before this control existed load as Auto — no behavior change
    /// until the user drags it.
    #[serde(default)]
    pub video_layers: Option<u32>,
    /// User SEND layer-ceiling for SCREEN. `None` = Auto. See `video_layers`.
    #[serde(default)]
    pub screen_layers: Option<u32>,
    /// User SEND layer-ceiling for AUDIO. `None` = Auto. See `video_layers`.
    /// Applied LIVE via the mic encoder's per-layer publish-gate (no restart): the
    /// base layer is always sent, and audio layers at/above this count are dropped
    /// at publish time. See `MicrophoneEncoder::set_user_layer_ceiling`.
    #[serde(default)]
    pub audio_layers: Option<u32>,
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
            video_layers: None,
            screen_layers: None,
            audio_layers: None,
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
///
/// Test-only since all SEND cells moved to the layer-count control (the tier
/// slider that consumed this is gone); retained as the tested inverse and the
/// documented half of the position↔index mapping.
#[allow(dead_code)]
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
///
/// Test-only since all SEND cells moved to the layer-count control; retained as
/// the tested SEND tier-bounds↔thumbs mapping (the receive side has its own
/// `receive::bounds_to_thumbs`).
#[allow(dead_code)]
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

/// Whether a dual-thumb selection sits at BOTH extremes (`min` at position 0 and
/// `max` at `last_position`) — i.e. the full default range, nothing constrained
/// (issue #1131 §D). Drives the Reset button's visibility: it is shown IFF this
/// is `false`, so dragging both thumbs back to the ends hides it live even when
/// the persisted `auto` flag is still false. Pure / host-tested; usize-based so
/// both the send (`usize`) and receive (`u32`, cast) sliders share it.
pub fn at_full_range(min_pos: usize, max_pos: usize, last_position: usize) -> bool {
    min_pos == 0 && max_pos == last_position
}

/// Percent offsets (0.0..=100.0) for the discrete tick marks on a range slider
/// track, one per step position `0..=max_pos` where `max_pos = step_count - 1`.
///
/// Tick `i` sits at `i / max_pos * 100%`, aligning with where the native range
/// thumb snaps (a `<input type=range min=0 max=N step=1>` thumb at value `v`
/// centres at `v/N` of the track). A single-step slider (`step_count <= 1`)
/// returns one tick at 0% (the lone stop). Returned left→right (ascending).
/// Pure so the positions are host-tested (and shared by the SEND + RECEIVE
/// sliders).
pub fn tick_offsets(step_count: usize) -> Vec<f32> {
    if step_count <= 1 {
        return vec![0.0];
    }
    let max_pos = (step_count - 1) as f32;
    (0..step_count)
        .map(|i| i as f32 / max_pos * 100.0)
        .collect()
}

// ── SEND layer-count model (video + screen) ────────────────────────
//
// The SEND control for video/screen is a LAYER-COUNT slider, NOT a tier slider.
// The track ticks are the kind's simulcast rungs, lowest-first (video: L0 360p /
// L1 540p / L2 720p; screen: its ladder). The FLOOR thumb is pinned at L0 (the
// base layer is always published — the base-present invariant), and the CEILING
// thumb is the published layer COUNT: position `p` (0-based layer index) means
// "publish layers L0..=Lp", i.e. a count of `p + 1`.
//
// The persisted value is `PerformancePreference::{video,screen}_layers`, an
// `Option<u32>` layer COUNT in `1..=layer_max`, where `None` == Auto == the full
// ladder (so the migration default and the "Reset" both land on `None`). These
// pure mappers convert between the stored count and the ceiling thumb position,
// and are host-tested so a wording/positioning regression breaks a test.

/// The rung labels to render for a SEND layer slider, lowest layer first
/// (index == `layer_id`), for an `layer_max`-rung ladder.
///
/// VIDEO mirrors the AQ camera ladder selection over `[low 360p, standard 540p,
/// hd 720p]`: `[low]`, `[low, hd]`, `[low, standard, hd]` for n = 1/2/3 — note
/// n=2 SKIPS the middle, matching the AQ `spaced_ladder_positions` rule — so we
/// special-case the count rather than taking a naive prefix.
///
/// SCREEN mirrors the AQ `simulcast_screen_layers` selection: 1→`[low]`,
/// 2→`[low, high]` (skips medium), 3→`[low, medium, high]`. The screen tiers use
/// QUALITATIVE labels (not resolution) because the AQ screen `low` and `medium`
/// rungs are BOTH 1280×720 (they differ in fps/bitrate, see `SCREEN_QUALITY_TIERS`),
/// so a resolution label would be ambiguous — the qualitative names match the AQ
/// tier labels and read consistently.
///
/// AUDIO mirrors the publisher's CONTIGUOUS audio ladder (`AUDIO_LAYER_KBPS =
/// [24, 32, 50]` kbps, lowest-first — the mic encoder publishes layers
/// `0..n` with NO skip, unlike the spaced video/screen ladders): 1→`[24k]`,
/// 2→`[24k, 32k]`, 3→`[24k, 32k, 50k]`.
///
/// Kept in lockstep with the AQ / publisher ladders here (the AQ tables are behind
/// a wasm-only crate); a reviewer must keep them in sync. Pure / host-tested.
pub fn send_layer_labels(kind: PrefMediaKind, layer_max: usize) -> Vec<&'static str> {
    let n = layer_max.clamp(1, 3);
    match kind {
        PrefMediaKind::Screen => {
            // SCREEN_QUALITY_TIERS = [high, medium, low]; simulcast_screen_layers
            // picks lowest-first: 1→[low], 2→[low, high] (skip medium), 3→full.
            match n {
                1 => vec!["low"],
                2 => vec!["low", "high"],
                _ => vec!["low", "medium", "high"],
            }
        }
        PrefMediaKind::Audio => {
            // AUDIO_LAYER_KBPS = [24, 32, 50] kbps, CONTIGUOUS lowest-first (no
            // skip): the publisher emits layers 0..n in order.
            match n {
                1 => vec!["24k"],
                2 => vec!["24k", "32k"],
                _ => vec!["24k", "32k", "50k"],
            }
        }
        PrefMediaKind::Video => {
            // Camera ladder via spaced_ladder_positions over [low, standard, hd]:
            // 1→[low], 2→[low, hd] (skip standard), 3→[low, standard, hd].
            match n {
                1 => vec!["360p"],
                2 => vec!["360p", "720p"],
                _ => vec!["360p", "540p", "720p"],
            }
        }
    }
}

/// Map a stored SEND layer-ceiling (`Option<u32>` COUNT; `None` = Auto/full) to
/// the ceiling thumb POSITION (0-based layer index) for a `layer_max`-rung track.
///
/// - `None` (Auto) → the top position (`layer_max - 1`) = full ladder.
/// - `Some(count)` → `count - 1`, clamped into `0..=layer_max-1` (a stored count
///   of 0 is meaningless — the base is always sent — so it floors at position 0;
///   an over-large count saturates at the top). Pure / host-tested.
pub fn layer_ceiling_to_thumb_pos(ceiling: Option<u32>, layer_max: usize) -> usize {
    let top = layer_max.saturating_sub(1);
    match ceiling {
        None => top,
        Some(count) => (count.max(1) as usize - 1).min(top),
    }
}

/// Inverse of [`layer_ceiling_to_thumb_pos`]: map a ceiling thumb POSITION to the
/// stored layer-ceiling value. The TOP position (full ladder) stores `None`
/// (Auto), so the migration default and Reset agree and a user at full never
/// pins a finite cap. Any lower position stores `Some(position + 1)` (the layer
/// COUNT). Pure / host-tested.
pub fn thumb_pos_to_layer_ceiling(pos: usize, layer_max: usize) -> Option<u32> {
    let top = layer_max.saturating_sub(1);
    if pos >= top {
        None
    } else {
        Some(pos as u32 + 1)
    }
}

impl PerformancePreference {
    /// Return a copy with any out-of-range index collapsed to `None` (Auto).
    ///
    /// ONE-SHOT MIGRATION (SEND tier slider → layer-count control): ALL THREE SEND
    /// tier sliders (video, screen, AND now audio) were removed — each replaced by
    /// the layer-count control that drives `*_layers`. A returning user with
    /// persisted manual tier bounds (`*_max`/`*_min`) would otherwise keep them
    /// applied to the encoder INVISIBLY (no UI surfaces them anymore), silently
    /// pinning their send quality. So we clear all three streams' `*_max`/`*_min`
    /// to `None` and set `*_auto = true` on load. This is safe and idempotent: a
    /// fresh/Auto pref is already in this state, so re-running it is a no-op; only
    /// legacy manual bounds are reset.
    ///
    /// (The `*_len` tier-count params are retained for API stability — the prior
    /// implementation clamped stale indices against them — but the migration now
    /// hard-clears all tier bounds, so they are unused.)
    pub fn sanitized(self, _video_len: usize, _audio_len: usize, _screen_len: usize) -> Self {
        PerformancePreference {
            // All three SEND tier sliders are gone (see fn doc): clear any
            // persisted manual bound and set Auto so no returning user is silently
            // pinned. The layer-count control owns each send axis via `*_layers`.
            video_max: None,
            video_min: None,
            video_auto: true,
            screen_max: None,
            screen_min: None,
            screen_auto: true,
            audio_max: None,
            audio_min: None,
            audio_auto: true,
            // Layer-ceiling counts carry no TIER index (they are layer counts in
            // `1..=effective_max_layers`, validated/clamped on the encoder side),
            // so they pass through unchanged here.
            video_layers: self.video_layers,
            screen_layers: self.screen_layers,
            audio_layers: self.audio_layers,
        }
    }

    /// Set the VIDEO SEND layer-ceiling COUNT (`None` = Auto / full ladder).
    ///
    /// The SEND control is purely layer-count now, so this ALSO forces the video
    /// tier bounds to Auto (`video_auto = true`, `video_max/min = None`): the
    /// AQ adapts each published layer's bitrate freely, and there is no second
    /// tier slider competing with the layer-count control. Pure.
    pub fn with_video_layers(mut self, layers: Option<u32>) -> Self {
        self.video_layers = layers;
        self.video_auto = true;
        self.video_max = None;
        self.video_min = None;
        self
    }

    /// Set the SCREEN SEND layer-ceiling COUNT (`None` = Auto). Same Auto-tier
    /// semantics as [`Self::with_video_layers`]. Pure.
    pub fn with_screen_layers(mut self, layers: Option<u32>) -> Self {
        self.screen_layers = layers;
        self.screen_auto = true;
        self.screen_max = None;
        self.screen_min = None;
        self
    }

    /// Set the AUDIO SEND layer-ceiling COUNT (`None` = Auto). Same Auto-tier
    /// semantics as [`Self::with_video_layers`]: the audio SEND control is now a
    /// layer-count slider too, so this frees the audio tier bounds to Auto
    /// (`audio_auto = true`, `audio_max/min = None`) — the AQ adapts the published
    /// audio layers' bitrate freely and there is no competing tier slider. Pure.
    pub fn with_audio_layers(mut self, layers: Option<u32>) -> Self {
        self.audio_layers = layers;
        self.audio_auto = true;
        self.audio_max = None;
        self.audio_min = None;
        self
    }

    /// `true` when adaptation is pinned to a single tier ("Fixed" badge): the
    /// stream is NOT on Auto and both bounds are set to the same tier. A stream
    /// on Auto is never "fixed" (it is fully automatic).
    ///
    /// NONE of the SEND streams render a tier slider anymore (all three — video,
    /// screen, AND audio — moved to the layer-count control), so this whole
    /// `*_is_fixed` family is now exercised only by unit tests: retained as the
    /// tested predicate over the persisted `*_max/*_min` fields (kept for the
    /// migration). `allow(dead_code)` for the bin target.
    #[allow(dead_code)]
    pub fn video_is_fixed(&self) -> bool {
        !self.video_auto && matches!((self.video_max, self.video_min), (Some(a), Some(b)) if a == b)
    }

    /// See [`Self::video_is_fixed`]. Test-only since audio also moved to the
    /// layer-count control.
    #[allow(dead_code)]
    pub fn audio_is_fixed(&self) -> bool {
        !self.audio_auto && matches!((self.audio_max, self.audio_min), (Some(a), Some(b)) if a == b)
    }

    /// See [`Self::video_is_fixed`]. Test-only since the SEND screen tier slider
    /// was replaced by the layer-count control.
    #[allow(dead_code)]
    pub fn screen_is_fixed(&self) -> bool {
        !self.screen_auto
            && matches!((self.screen_max, self.screen_min), (Some(a), Some(b)) if a == b)
    }

    /// Toggle the video stream's Auto flag. Turning Auto ON snaps both thumbs to
    /// the extremes (bounds → `None/None`). Turning Auto OFF leaves the stored
    /// thumb indices (which are extremes/`None` until the user drags). Pure.
    ///
    /// Test-only since the SEND video tier slider was replaced by the layer-count
    /// control (`with_video_layers` now owns the video send axis); retained as the
    /// tested mutator over the persisted `video_*` fields.
    #[allow(dead_code)]
    pub fn set_video_auto(mut self, on: bool) -> Self {
        self.video_auto = on;
        if on {
            self.video_max = None;
            self.video_min = None;
        }
        self
    }

    /// See [`Self::set_video_auto`]. Test-only since the SEND screen tier slider
    /// was replaced by the layer-count control.
    #[allow(dead_code)]
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
    ///
    /// Test-only since the SEND video tier slider was replaced by the layer-count
    /// control (`with_video_layers`). Retained as the tested tier-bound mutator.
    #[allow(dead_code)]
    pub fn with_video_thumbs(mut self, sel: RangeSel) -> Self {
        let (best, worst) = thumbs_to_bounds(sel, VIDEO_TIER_LABELS.len());
        self.video_max = best;
        self.video_min = worst;
        self.video_auto = false;
        self
    }

    /// Return a copy with the audio stream's bounds replaced by those derived
    /// from `sel`. Clears the Auto flag. Pure.
    ///
    /// Test-only since audio moved to the layer-count control (`with_audio_layers`
    /// now owns the audio send axis). Retained as the tested tier-bound mutator.
    #[allow(dead_code)]
    pub fn with_audio_thumbs(mut self, sel: RangeSel) -> Self {
        let (best, worst) = thumbs_to_bounds(sel, AUDIO_TIER_LABELS.len());
        self.audio_max = best;
        self.audio_min = worst;
        self.audio_auto = false;
        self
    }

    /// Return a copy with the screen stream's bounds replaced by those derived
    /// from `sel`. Clears the Auto flag. Pure.
    ///
    /// Test-only since the SEND screen tier slider was replaced by the layer-count
    /// control (`with_screen_layers`). Retained as the tested tier-bound mutator.
    #[allow(dead_code)]
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
///
/// Test-only since all SEND cells moved to the layer-count control (whose caption
/// is "N of M layers", not a tier span); retained as the tested SEND span
/// renderer (the receive side has its own `receive::span_text`).
#[allow(dead_code)]
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

// ── bar-meter math (SEND) ──────────────────────────────────────────
//
// The VU needle (an analog dial) was replaced by an inline bar-meter (4 vertical
// bars, ▂▄▆█) plus a one-line readout (#1095 redesign). The meter's lit-bar
// count is an integer "level" 0..=MAX_METER_LEVEL, derived from a 0..1 quality
// fraction (best = 1.0, worst = 0.0), so first paint and the live rAF loop agree.
// The old needle-angle functions are gone; the tests now pin the index→level
// mapping (the actually-rendered behaviour).

/// Number of bars in the inline bar-meter; the level ranges `0..=MAX_METER_LEVEL`.
/// Level 0 = all bars unlit (empty/idle state); level 4 = all four lit (best).
pub const MAX_METER_LEVEL: u8 = 4;

/// Map a 0..=1 quality fraction (0 = worst, 1 = best) to a lit-bar level
/// `0..=MAX_METER_LEVEL` via `round(fraction * MAX_METER_LEVEL)`. The fraction is
/// clamped to `[0, 1]` first, so out-of-range inputs (or NaN, which clamps to 0)
/// never overflow the bar count. Pure / host-tested.
pub fn level_from_fraction(fraction: f32) -> u8 {
    let clamped = if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    };
    (clamped * MAX_METER_LEVEL as f32).round() as u8
}

/// The 0..=1 quality fraction for a SEND tier index (0 = best tier). Best tier
/// → `1.0`, worst tier → `0.0`. Single-tier ladder → `1.0` (a lone tier is the
/// best available). Out-of-range clamps. Pure / host-tested.
pub fn tier_quality_fraction(index: usize, tier_count: usize) -> f32 {
    if tier_count <= 1 {
        return 1.0;
    }
    let max_idx = tier_count - 1;
    let clamped = index.min(max_idx);
    // Index 0 (best) → 1.0; worst index → 0.0.
    1.0 - (clamped as f32 / max_idx as f32)
}

/// Convert a SEND tier index into a meter level `0..=MAX_METER_LEVEL`. Best tier
/// → `MAX_METER_LEVEL`; worst → 1 (never 0 — a flowing stream lights at least one
/// bar; level 0 is reserved for the no-signal empty state). Pure / host-tested.
pub fn tier_to_meter_level(index: usize, tier_count: usize) -> u8 {
    level_from_fraction(tier_quality_fraction(index, tier_count)).max(1)
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

/// Empty-state meter level: 0 = all bars unlit (no signal), distinct from a
/// flowing stream's level (>=1).
pub const EMPTY_METER_LEVEL: u8 = 0;

/// Empty-state readout for the video meter (camera off / no snapshot).
pub const VIDEO_EMPTY_READOUT: &str = "Camera — off";
/// Empty-state readout for the audio meter (no snapshot).
pub const AUDIO_EMPTY_READOUT: &str = "Idle";
/// Empty-state readout for the screen meter (not sharing / no snapshot).
pub const SCREEN_EMPTY_READOUT: &str = "Screen — not sharing";

/// All three SEND meters' render state: lit-bar level + readout text. Pure so the
/// snapshot→meter mapping (including the empty-state reset) is host-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaugeState {
    pub video_level: u8,
    pub audio_level: u8,
    pub screen_level: u8,
    pub video_text: String,
    pub audio_text: String,
    pub screen_text: String,
}

/// Map the optional live SEND snapshots to all three meters' render state.
///
/// `Some` → live lit-bar levels + numeric readouts. `None` on an input (encoder
/// unavailable — camera turned off, or screen not sharing) → that meter resets to
/// level 0 (all bars unlit) with a status readout, so a stopped stream never
/// freezes on a stale reading. The video/audio meters share one
/// `LiveQualitySnapshot`; the screen meter has its own already-`Option`
/// `ScreenQualitySnapshot`. Single source of truth for both first paint and the
/// rAF loop.
pub fn gauge_state_from_snapshot(
    va: Option<&LiveQualitySnapshot>,
    screen: Option<&ScreenQualitySnapshot>,
) -> GaugeState {
    let (video_level, audio_level, video_text, audio_text) = match va {
        Some(s) => (
            tier_to_meter_level(s.video_tier_index, VIDEO_TIER_LABELS.len()),
            tier_to_meter_level(s.audio_tier_index, AUDIO_TIER_LABELS.len()),
            format_video_readout(s),
            format_audio_readout(s),
        ),
        None => (
            EMPTY_METER_LEVEL,
            EMPTY_METER_LEVEL,
            VIDEO_EMPTY_READOUT.to_string(),
            AUDIO_EMPTY_READOUT.to_string(),
        ),
    };
    let (screen_level, screen_text) = match screen {
        Some(s) => (
            tier_to_meter_level(s.tier_index, SCREEN_TIER_LABELS.len()),
            format_screen_readout(s),
        ),
        None => (EMPTY_METER_LEVEL, SCREEN_EMPTY_READOUT.to_string()),
    };
    GaugeState {
        video_level,
        audio_level,
        screen_level,
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

// ── DOM helpers for the throttled meter update (shared) ────────────

/// Write `data-level="<level>"` to the bar-meter element by id.
///
/// This is the per-tick DOM write that lights the meter bars without triggering a
/// Dioxus re-render (mirrors the pre-join mic meter's direct attribute write). CSS
/// reads `data-level` and lights the first N bars. One attribute write per tick.
/// No-ops if the element is missing. Replaces the retired needle-rotation write.
fn write_meter_level(meter_id: &str, level: u8) {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(meter_id))
    {
        let _ = el.set_attribute("data-level", &level.to_string());
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
/// Per-stream SEND "Reset" buttons (clear the thumbs back to the full range).
/// The `_AUTO` constant names are retained so existing #1095 e2e selectors keep
/// resolving even though the element is now a Reset button, not an Auto toggle,
/// and is only rendered when the stream is constrained off its extremes.
pub const TESTID_VIDEO_AUTO: &str = "perf-video-auto";
pub const TESTID_AUDIO_AUTO: &str = "perf-audio-auto";
pub const TESTID_SCREEN_AUTO: &str = "perf-screen-auto";
/// SEND ("Sending") bar-meters. The constant names are unchanged (#1095 e2e
/// selectors still resolve) even though the element is now a `.perf-meter`
/// bar-meter, not the retired VU needle gauge.
pub const TESTID_VU_VIDEO: &str = "perf-vu-video";
pub const TESTID_VU_AUDIO: &str = "perf-vu-audio";
pub const TESTID_VU_SCREEN: &str = "perf-vu-screen";

// The rAF driver writes `data-level` to the meter container by id and the
// readout text to the readout span by id. Send and receive use DISTINCT ids per
// kind so the two drivers never write the same node.
const VIDEO_METER_ID: &str = "perf-meter-video";
const AUDIO_METER_ID: &str = "perf-meter-audio";
const SCREEN_METER_ID: &str = "perf-meter-screen";
// Readout element ids keep their original `perf-vu-*-readout` names (NOT renamed
// to `perf-meter-*`) so the #961/#989 e2e that polls these ids by `#id` still
// resolves after the needle→bar-meter swap.
const VIDEO_READOUT_ID: &str = "perf-vu-video-readout";
const AUDIO_READOUT_ID: &str = "perf-vu-audio-readout";
const SCREEN_READOUT_ID: &str = "perf-vu-screen-readout";

// ── components ────────────────────────────────────────────────────

/// An inline bar-meter (four vertical bars, ▂▄▆█) followed by a one-line readout
/// on the same baseline (#1095 redesign — replaces the analog VU needle gauge).
///
/// The container carries `data-level="0..=MAX_METER_LEVEL"`; CSS lights the first
/// N bars. Only the container's `data-level` attribute and the readout text node
/// are mutated at runtime (by the rAF drivers) via direct DOM writes, so this
/// component never re-renders per tick. The bars are `aria-hidden`; the readout is
/// the sole accessible value (`aria-live="polite"`). Shared by both the "Sending"
/// and "Receiving" sides (distinct ids per instance so the drivers never collide).
#[component]
fn PerfMeter(
    /// Stable testid / aria target for the meter container (the `data-level` host).
    testid: &'static str,
    /// Id of the meter container (the rAF driver writes `data-level` here).
    meter_id: &'static str,
    /// Id of the readout text element (the rAF driver writes its text here).
    readout_id: &'static str,
    /// Accessible label, e.g. "Sending video" / "Receiving video".
    label: &'static str,
    /// Initial lit-bar level for first paint before the loop ticks.
    initial_level: u8,
    /// Initial readout text.
    initial_readout: String,
) -> Element {
    rsx! {
        div { class: "perf-meter-wrap",
            // Bars: decorative, lit via CSS from the container's data-level. The
            // four child bars are always present; CSS lights the first N.
            div {
                id: meter_id,
                class: "perf-meter",
                "data-testid": testid,
                "data-level": "{initial_level}",
                "aria-hidden": "true",
                role: "img",
                "aria-label": "{label}",
                span { class: "perf-meter__bar" }
                span { class: "perf-meter__bar" }
                span { class: "perf-meter__bar" }
                span { class: "perf-meter__bar" }
            }
            // The readout is the accessible value: announced via aria-live when
            // the quality changes (the bars above are decorative / aria-hidden).
            span {
                id: readout_id,
                class: "perf-meter__readout",
                role: "status",
                "aria-live": "polite",
                "aria-label": "{label}",
                "{initial_readout}"
            }
        }
    }
}

/// Headless driver for the three SEND bar-meters. Renders **nothing** — it only
/// owns the single ~4 Hz `requestAnimationFrame` polling loop that reads
/// `live_quality_snapshot()` / `live_screen_snapshot()` and writes each meter's
/// `data-level` + readout straight to the DOM nodes **by id** (so the meters can
/// live anywhere in the tree, e.g. each inside its own per-kind card).
///
/// Direct DOM writes mean no per-frame re-render. The loop self-cancels when the
/// driver unmounts (the `use_drop` clears the closure cell).
#[component]
fn QualityVuMeterDriver(
    /// Reads the current video/audio live snapshot. `None` → those meters reset
    /// to the empty state (level 0 + "Camera — off" / "Idle").
    read_snapshot: SnapshotReader,
    /// Reads the current screen-share live snapshot. `None` (not sharing) → the
    /// screen meter shows level 0 + "Screen — not sharing".
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
                    // reset the meter to level 0 and show the status readout.
                    // Without this branch a stream stopped *after* the panel opened
                    // would freeze the meter on a stale level.
                    let snap = reader.read();
                    let screen_snap = screen_reader.read();
                    let state = gauge_state_from_snapshot(snap.as_ref(), screen_snap.as_ref());
                    write_meter_level(VIDEO_METER_ID, state.video_level);
                    write_meter_level(AUDIO_METER_ID, state.audio_level);
                    write_meter_level(SCREEN_METER_ID, state.screen_level);
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
    /// When `true`, the slider operates in SEND LAYER-COUNT mode (video/screen):
    ///
    /// - the MIN (left) thumb is PINNED at position 0 and rendered non-interactive
    ///   (`disabled` + `aria-disabled`): the base layer (L0) is ALWAYS published,
    ///   so the floor is fixed and only the ceiling (max) thumb moves;
    /// - end-labels are SEMANTIC ("Base" left / "Max" right) because the `labels`
    ///   list is lowest-layer-first (NOT the worst→best tier order the default
    ///   end-labels assume), so the raw first/last would read inverted;
    /// - the moving (ceiling) thumb + fill use `--success` (the SEND treatment)
    ///   via the `perf-range--send-layer` container class, distinguishing it from
    ///   the full-range `--accent` Receive slider beside it.
    ///
    /// Defaults to `false` (the audio tier slider keeps both thumbs draggable,
    /// tier end-labels, and the default coloring).
    #[props(default)]
    layer_mode: bool,
    /// Optional override for the MAX-thumb `aria-valuetext` (SEND layer mode uses
    /// a count-aware string like "2 of 3 layers" instead of a resolution label).
    /// `None` → the default resolution/tier label. Ignored when not `layer_mode`.
    #[props(default)]
    max_valuetext_override: Option<String>,
) -> Element {
    // In layer mode the floor thumb is pinned (see prop doc); historically a
    // separate `pin_min` flag, now folded into `layer_mode` (they were always set
    // together).
    let pin_min = layer_mode;
    let max_pos = labels.len().saturating_sub(1);
    let min_value = sel.min_pos;
    let max_value = sel.max_pos;
    let min_id = format!("{id_prefix}-range-min");
    let max_id = format!("{id_prefix}-range-max");
    let min_valuetext = position_label(sel.min_pos, &labels).to_string();
    // Max-thumb aria-valuetext: in layer mode use the count-aware override when
    // provided; otherwise the tier/resolution label.
    let max_valuetext = match (layer_mode, max_valuetext_override.as_ref()) {
        (true, Some(s)) => s.clone(),
        _ => position_label(sel.max_pos, &labels).to_string(),
    };
    // Container modifier: SEND layer sliders get the `--success` thumb/fill
    // treatment + semantic end-labels.
    let range_class = if layer_mode {
        "perf-range perf-range--send-layer"
    } else {
        "perf-range"
    };
    // End-labels. Tier mode: worst (labels.last) LEFT, best (labels.first) RIGHT.
    // Layer mode: semantic "Base" (lowest, the pinned floor) LEFT, "Max" (highest)
    // RIGHT — the `labels` list is lowest-first so raw first/last would invert.
    let (left_label, right_label) = if layer_mode {
        ("Base", "Max")
    } else {
        (
            labels.last().copied().unwrap_or(""),
            labels.first().copied().unwrap_or(""),
        )
    };

    // Fill highlight between the thumbs (percent of track).
    let (fill_left, fill_right) = if max_pos == 0 {
        (0.0_f32, 100.0_f32)
    } else {
        (
            sel.min_pos as f32 / max_pos as f32 * 100.0,
            sel.max_pos as f32 / max_pos as f32 * 100.0,
        )
    };
    // Decorative tick marks — one per discrete step (aligned to the thumb stops).
    let ticks = tick_offsets(labels.len());

    rsx! {
        div { class: "{range_class}",
            // Left end-label: worst tier (tier mode) or "Base" (layer mode).
            span { class: "perf-range-end-label", "{left_label}" }
            div { class: "perf-range-track-wrap",
                div { class: "perf-range-track",
                    // Tick marks: notch per step position. Rendered INSIDE the
                    // track (so they inherit its low stacking + below the fill and
                    // the input thumbs) and `pointer-events: none` + aria-hidden —
                    // purely decorative, and MUST NOT intercept pointer drags (the
                    // WebKit pinned-floor fix in commit 603c7354 depends on nothing
                    // over the track swallowing pointer-down for the thumbs). The
                    // labels/caption convey the values to AT.
                    div {
                        class: "perf-range-ticks",
                        "aria-hidden": "true",
                        "data-testid": "{id_prefix}-range-ticks",
                        for off in ticks.iter() {
                            span {
                                class: "perf-range-tick",
                                style: "left: {off}%;",
                            }
                        }
                    }
                    div {
                        class: "perf-range-fill",
                        style: "left: {fill_left}%; right: {100.0 - fill_right}%;",
                    }
                }
                input {
                    id: "{min_id}",
                    class: if pin_min {
                        "perf-range-input perf-range-input-min is-pinned"
                    } else {
                        "perf-range-input perf-range-input-min"
                    },
                    "data-testid": min_testid,
                    r#type: "range",
                    min: "0",
                    max: "{max_pos}",
                    step: "1",
                    // When pinned (SEND layer sliders), the floor is fixed at the
                    // base layer (position 0) and the input is non-interactive.
                    value: if pin_min { "0".to_string() } else { format!("{min_value}") },
                    // WEBKIT BUG FIX: do NOT use the HTML `disabled` attribute to
                    // pin the floor. A `disabled`, full-width, on-top range input
                    // SWALLOWS pointer-down events meant for the max thumb beneath
                    // it in WebKit/Safari — WebKit does not reliably let
                    // `pointer-events: none` fall THROUGH a disabled form control,
                    // so the ceiling thumb below never receives the drag (the SEND
                    // max thumb became undraggable; the Receive slider was fine
                    // because its min is enabled + `pointer-events:none` on the
                    // track, letting events reach the max thumb). Instead make the
                    // floor immovable WITHOUT `disabled`:
                    //   - `.is-pinned { pointer-events: none }` (CSS) — no pointer,
                    //   - `tabindex=-1` — not keyboard-focusable, so arrows can't
                    //     move it,
                    //   - `aria-disabled=true` — conveyed to screen readers,
                    //   - the `oninput` early-return guard below — defensive no-op.
                    // The `.is-pinned` CSS also drops this input BELOW the max
                    // (z-index 0 < max's 1) so the ceiling thumb is the topmost
                    // interactive layer and always grabbable. See the ux memory
                    // `pattern_perf_send_pinned_floor_slider.md`.
                    tabindex: if pin_min { "-1" } else { "0" },
                    "aria-disabled": if pin_min { "true" } else { "false" },
                    "aria-label": if pin_min {
                        format!("Base {stream_noun} layer — always sent (fixed)")
                    } else {
                        format!("Worst {stream_noun} send quality")
                    },
                    "aria-valuetext": "{min_valuetext}",
                    oninput: move |evt| {
                        // Pinned floor never moves: ignore any input. (It is no
                        // longer `disabled`, so guard here AND rely on
                        // `pointer-events:none` + `tabindex=-1` to make a stray
                        // input impossible in practice.)
                        if pin_min {
                            return;
                        }
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
                    "aria-label": if layer_mode {
                        format!("Highest {stream_noun} layer to publish")
                    } else {
                        format!("Best {stream_noun} send quality")
                    },
                    "aria-valuetext": "{max_valuetext}",
                    oninput: move |evt| {
                        if let Ok(p) = evt.value().parse::<usize>() {
                            on_change.call(set_max_thumb(sel, p));
                        }
                    },
                }
            }
            // Right end-label: best tier (tier mode) or "Max" (layer mode).
            span { class: "perf-range-end-label", "{right_label}" }
        }
    }
}

/// A self-contained "?" help popover button (shared by send + receive rows, and
/// by the Diagnostics drawer's NetEq sections — #1131 cleanup).
///
/// `open_help` is the shared single-open signal keyed by `key_id`. Opening one
/// closes any other (since they all share the signal). Reuses the `.perf-help*`
/// styles (44×44 hit area, focus ring, aria-expanded, Escape/outside-click close,
/// focus return) so every caller gets the same a11y treatment for free.
///
/// Gap between the "?" button and its popover (matches the legacy `calc(100% +
/// 8px)` offset). Also used as the viewport edge margin so the popover never sits
/// flush to a screen edge.
const HELP_POPOVER_GAP_PX: f64 = 8.0;

/// Pure positioning math for the help popover, in viewport (CSS-pixel) coords.
///
/// The popover is rendered `position: fixed` so it escapes the Diagnostics
/// drawer's scroll-clip (`#diagnostics-sidebar { overflow-y: auto }` clips BOTH
/// axes per the CSS overflow-propagation rule). `fixed` anchors to the viewport
/// (no transformed ancestor on desktop), so to keep the popover visually tied to
/// its button we compute its top-left here from the button's rect.
///
/// Rules:
///   - Horizontal: align the popover's LEFT to the button's left, then clamp into
///     `[gap, viewport_w - w - gap]`. A button near the RIGHT drawer border is
///     pulled left so the popover never overhangs the border (fixes the
///     right-edge clip); a button near the left can't push it off-screen left.
///   - Vertical: prefer BELOW the button (`button_bottom + gap`). If that would
///     overflow the bottom margin AND there is more room ABOVE than below, FLIP to
///     above (`button_top - gap - h`). Finally clamp into the vertical viewport
///     so it can never spill past the top/bottom fold (fixes the bottom-fold
///     clip). The flip is what a pure-CSS `top: calc(100% + 8px)` could not do.
///
/// Pure data in → `(left, top)` out, so it is unit-tested without a browser.
fn compute_help_popover_position(
    btn_left: f64,
    btn_top: f64,
    btn_bottom: f64,
    popup_w: f64,
    popup_h: f64,
    viewport_w: f64,
    viewport_h: f64,
) -> (f64, f64) {
    let gap = HELP_POPOVER_GAP_PX;

    // Horizontal: left-align to the button, clamp into the viewport.
    let max_left = (viewport_w - popup_w - gap).max(gap);
    let left = btn_left.clamp(gap, max_left);

    // Vertical: below by default; flip above when below overflows and above has
    // more room.
    let space_below = viewport_h - btn_bottom - gap;
    let space_above = btn_top - gap;
    let below_top = btn_bottom + gap;
    let above_top = btn_top - gap - popup_h;
    let max_top = (viewport_h - popup_h - gap).max(gap);
    let top = if popup_h <= space_below {
        // Fits below.
        below_top
    } else if space_above > space_below {
        // Doesn't fit below and there's more room above → flip up, clamp so a
        // very tall popover can't run off the TOP edge.
        above_top.clamp(gap, max_top)
    } else {
        // More room below (or equal) but still doesn't fully fit → keep below,
        // clamped (the popover's own max-height + internal scroll keep it usable).
        below_top.clamp(gap, max_top)
    };

    (left, top)
}

/// Position the `position: fixed` help popover under (or above) its button, in
/// the viewport, and keep it there while open. Reuses the codebase's established
/// "fixed + clamp + reposition on scroll(capture)/resize + `use_drop` teardown"
/// pattern (see `signal_quality::install_popup_anchor`) so the popover genuinely
/// escapes the drawer's scroll-clip on every edge — without portaling the node out
/// of its `.perf-help` wrapper, so all ids/testids/aria/scrim wiring stay put.
///
/// `open` gates installation: listeners + the initial rAF layout are attached only
/// while the popover is open, and torn down when it closes or the component
/// unmounts. `btn_id`/`popup_id` are read live each reposition tick so a stale
/// rect is never used.
///
/// `open_help`/`key_id` (not a plain `bool`) are taken so the effect READS the
/// signal inside its body and therefore re-runs every time the popover opens or
/// closes WHILE the component stays mounted — the `HelpPopover` button is always
/// mounted, so a captured `bool` would be read once at mount (`false`) and never
/// re-fire on open. (Adversarial check 1: the `SimulcastLayersSection` bool-prop
/// effect only "works" because that whole subtree mounts/unmounts; this one does
/// not, so it must subscribe to the signal.)
fn use_help_popover_anchor(
    open_help: Signal<Option<&'static str>>,
    key_id: &'static str,
    btn_id: String,
    popup_id: String,
) {
    use std::cell::RefCell;
    use wasm_bindgen::closure::Closure;

    struct AnchorState {
        win: web_sys::Window,
        resize_cb: Closure<dyn FnMut()>,
        scroll_cb: Closure<dyn FnMut()>,
    }

    let state: Rc<RefCell<Option<AnchorState>>> = use_hook(|| Rc::new(RefCell::new(None)));

    {
        let state = state.clone();
        use_effect(move || {
            // Read the signal INSIDE the effect so it re-runs on every open/close.
            let open = open_help() == Some(key_id);

            // Tear down any previous installation first (open→close, or a
            // re-run) so listeners never stack.
            if let Some(prev) = state.borrow_mut().take() {
                let _ = prev.win.remove_event_listener_with_callback(
                    "resize",
                    prev.resize_cb.as_ref().unchecked_ref(),
                );
                let _ = prev.win.remove_event_listener_with_callback_and_bool(
                    "scroll",
                    prev.scroll_cb.as_ref().unchecked_ref(),
                    true,
                );
            }

            if !open {
                return;
            }
            let Some(win) = web_sys::window() else {
                return;
            };

            let reposition = {
                let btn_id = btn_id.clone();
                let popup_id = popup_id.clone();
                move || reposition_help_popover(&btn_id, &popup_id)
            };

            // First paint can race the popover attaching to the DOM, so lay it
            // out on the next animation frame (after the layout engine can
            // measure the popover's natural size).
            {
                let rep = reposition.clone();
                let cb = Closure::once_into_js(move |_ts: f64| rep());
                let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
            }

            let resize_cb: Closure<dyn FnMut()> = Closure::new({
                let rep = reposition.clone();
                move || rep()
            });
            let _ =
                win.add_event_listener_with_callback("resize", resize_cb.as_ref().unchecked_ref());

            // Capture-phase scroll so we observe scrolling on the drawer (any
            // ancestor scroll container), not just the window.
            let scroll_cb: Closure<dyn FnMut()> = Closure::new({
                let rep = reposition.clone();
                move || rep()
            });
            let _ = win.add_event_listener_with_callback_and_bool(
                "scroll",
                scroll_cb.as_ref().unchecked_ref(),
                true,
            );

            *state.borrow_mut() = Some(AnchorState {
                win,
                resize_cb,
                scroll_cb,
            });
        });
    }

    use_drop(move || {
        if let Some(prev) = state.borrow_mut().take() {
            let _ = prev.win.remove_event_listener_with_callback(
                "resize",
                prev.resize_cb.as_ref().unchecked_ref(),
            );
            let _ = prev.win.remove_event_listener_with_callback_and_bool(
                "scroll",
                prev.scroll_cb.as_ref().unchecked_ref(),
                true,
            );
        }
    });
}

/// Measure the button + popover and write the computed `left/top` (viewport
/// coords) onto the `position: fixed` popover. No-op if either element is absent
/// or the window is unavailable.
fn reposition_help_popover(btn_id: &str, popup_id: &str) {
    let Some(win) = web_sys::window() else {
        return;
    };
    let Some(doc) = win.document() else {
        return;
    };
    let (Some(btn), Some(popup)) = (
        doc.get_element_by_id(btn_id),
        doc.get_element_by_id(popup_id),
    ) else {
        return;
    };
    let btn_rect = btn.get_bounding_client_rect();
    let popup_rect = popup.get_bounding_client_rect();
    let viewport_w = win
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let viewport_h = win
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let (left, top) = compute_help_popover_position(
        btn_rect.left(),
        btn_rect.top(),
        btn_rect.bottom(),
        popup_rect.width(),
        popup_rect.height(),
        viewport_w,
        viewport_h,
    );

    if let Some(html) = popup.dyn_ref::<web_sys::HtmlElement>() {
        let _ = html.style().set_property("left", &format!("{left:.1}px"));
        let _ = html.style().set_property("top", &format!("{top:.1}px"));
    }
}

/// `pub(crate)` so the Diagnostics drawer can mount it directly rather than
/// duplicating the markup (single source of truth for the help affordance).
#[component]
pub(crate) fn HelpPopover(
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

    // Position the `position: fixed` popover so it escapes the drawer's
    // scroll-clip on every edge (right border + bottom fold). Installed only
    // while open; torn down on close/unmount. Takes the signal (not `help_open`)
    // so the effect re-fires on open/close while the button stays mounted.
    // (#1131 scroll-clip fix)
    use_help_popover_anchor(
        open_help,
        key_id,
        help_btn_id.clone(),
        help_popover_id.clone(),
    );

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

// (The former `SendCell` tier-slider component was removed once ALL THREE SEND
// kinds — video, screen, and audio — moved to the layer-count control
// `SendLayerCell`. The inverse-index tier model it used (`bounds_to_thumbs` /
// `span_text` / `tier_index_to_position` / the `*_audio_*` tier mutators) is
// retained below for its unit tests + as documentation of the convention, marked
// `#[allow(dead_code)]`.)

/// The VIDEO/SCREEN SEND column: a LAYER-COUNT control (not a tier slider).
///
/// Reuses [`DualRangeSlider`] in layer space (`layer_mode: true`): the track
/// ticks are the kind's simulcast rungs (lowest-first), the FLOOR thumb is PINNED
/// at the base layer (L0 is always published), the end-labels read "Base"→"Max"
/// left→right, and the CEILING thumb is the published layer COUNT. Dragging the
/// ceiling maps to
/// [`thumb_pos_to_layer_ceiling`] and calls `on_ceiling_change` with the stored
/// value (`None` at full = Auto), which the parent writes into
/// `PerformancePreference::{video,screen}_layers` → the encoder's
/// `set_user_layer_ceiling` path. The rung strip is selection-driven
/// ([`layer_send_rungs`]) so the active/shed pips update as the user drags.
///
/// The AQ per-layer bitrate adaptation is left fully automatic on this path (the
/// parent sends `*_min/*_max = None`); this control governs the COUNT only.
#[allow(clippy::too_many_arguments)]
#[component]
fn SendLayerCell(
    /// Accessible noun, e.g. "video" / "screen share".
    stream_noun: &'static str,
    /// Send id prefix, e.g. "perf-video" / "perf-screen".
    id_prefix: &'static str,
    min_testid: &'static str,
    max_testid: &'static str,
    /// Reset button testid (reuses the former Auto testid — surface unchanged).
    auto_testid: &'static str,
    help_testid: &'static str,
    help_body: &'static str,
    vu_testid: &'static str,
    vu_meter_id: &'static str,
    vu_readout_id: &'static str,
    vu_label: &'static str,
    vu_initial_level: u8,
    vu_initial_readout: String,
    consequence: String,
    /// Which media kind this cell drives — selects the per-kind ladder labels
    /// (video / screen / audio).
    kind: PrefMediaKind,
    /// The kind's EFFECTIVE max simulcast layers (the real ladder depth from
    /// `host.rs`). Drives the number of ticks AND the full/default ceiling.
    layer_max: usize,
    /// The persisted layer-ceiling COUNT (`None` = Auto / full ladder).
    layers: Option<u32>,
    /// Whether the SOURCE is actually capturing (camera on / sharing / mic on).
    /// Threaded from the panel's existing per-kind state (the same signal that
    /// drives the "not sharing" consequence). Switches the caption/summary from
    /// the present-tense "Sending N of M layers" to the future "Will send N layers
    /// {when …}" so we never claim to be sending while the source is off. The
    /// slider + ticks still render the configured ceiling (it is a setting).
    source_active: bool,
    /// Live summary line (flowing rates), shown under the strip caption.
    summary_line: String,
    open_help: Signal<Option<&'static str>>,
    /// Called with the new stored ceiling (`None` = Auto/full) on a ceiling drag.
    on_ceiling_change: EventHandler<Option<u32>>,
    /// Reset → clears to full (`None`). Same testid as the former Auto control.
    on_reset: EventHandler<()>,
) -> Element {
    let labels = send_layer_labels(kind, layer_max);
    let last_pos = labels.len().saturating_sub(1);
    // Ceiling thumb position from the stored count; floor is always 0 (base).
    let ceiling_pos = layer_ceiling_to_thumb_pos(layers, labels.len());
    let sel = RangeSel {
        min_pos: 0,
        max_pos: ceiling_pos,
    };
    // Selection-driven rung strip: active up to the ceiling, immediate feedback.
    let rungs = layer_send_rungs(&labels, ceiling_pos);
    let rungs_aria = send_rungs_aria(&rungs);
    let active_count = ceiling_pos + 1;
    // Human caption: present-tense "Sending N of M layers" when the source is
    // capturing, else the future "Will send N layers {when …}" using the
    // configured count (source-aware, pure / host-tested).
    let count_caption = format_send_layer_caption(kind, active_count, labels.len(), source_active);
    // Count-aware aria-valuetext for the ceiling thumb (screen-reader announces
    // "2 of 3 layers", not a bare resolution). Single-layer ladders announce the
    // lone layer.
    let ceiling_valuetext = if labels.len() <= 1 {
        "1 layer".to_string()
    } else {
        format!("{active_count} of {} layers", labels.len())
    };
    // Reset shown IFF not at the full ladder (ceiling below the top). At full the
    // slot is empty so the head reads clean (mirrors SendCell's rule).
    let show_reset = ceiling_pos < last_pos;
    // Ladder size as a Copy `usize` so the rung tooltips + the `on_change`
    // closure can use it WITHOUT borrowing `labels` after it is moved into the
    // closure / cloned into the slider below (Directive 4 SITE 2/3).
    let ladder_count = labels.len();

    rsx! {
        div { class: "perf-side perf-side--send",
            div { class: "perf-side__head",
                span { class: "perf-side__title",
                    svg {
                        class: "perf-dir-arrow",
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "14", height: "14", view_box: "0 0 24 24",
                        fill: "none", stroke: "currentColor", stroke_width: "2",
                        stroke_linecap: "round", stroke_linejoin: "round",
                        "aria-hidden": "true",
                        path { d: "M7 17 17 7" }
                        path { d: "M7 7h10v10" }
                    }
                    "Sending"
                }
                span { class: "perf-side__consequence", "{consequence}" }
                PerfMeter {
                    testid: vu_testid,
                    meter_id: vu_meter_id,
                    readout_id: vu_readout_id,
                    label: vu_label,
                    initial_level: vu_initial_level,
                    initial_readout: vu_initial_readout,
                }
                HelpPopover {
                    key_id: id_prefix,
                    help_testid,
                    help_label: vu_label,
                    help_body,
                    open_help,
                }
                if show_reset {
                    button {
                        r#type: "button",
                        class: "perf-reset-button",
                        "data-testid": auto_testid,
                        "aria-label": "Reset {stream_noun} layers — publish the full ladder",
                        title: "Publish all layers (automatic)",
                        onclick: move |_| on_reset.call(()),
                        "Reset"
                    }
                }
            }
            DualRangeSlider {
                id_prefix,
                min_testid,
                max_testid,
                stream_noun,
                labels: labels.clone(),
                sel,
                layer_mode: true,
                max_valuetext_override: ceiling_valuetext,
                on_change: move |s: RangeSel| {
                    // Only the ceiling (max) thumb is interactive; map its position
                    // to the stored layer-ceiling (None at full = Auto).
                    on_ceiling_change.call(thumb_pos_to_layer_ceiling(s.max_pos, ladder_count));
                },
            }
            div {
                class: "perf-rungs",
                "data-testid": "{id_prefix}-send-rungs",
                role: "img",
                "aria-label": "{rungs_aria}",
                for rung in rungs.iter() {
                    span {
                        key: "{rung.layer_id}",
                        class: if rung.active { "perf-rung is-active" } else { "perf-rung is-shed" },
                        "data-testid": "{id_prefix}-send-rung-{rung.layer_id}",
                        title: if rung.active {
                            format!(
                                "{} layer — publishing {}",
                                layer_quality_label(rung.layer_id, ladder_count as u32, false),
                                rung.res_label
                            )
                        } else {
                            format!(
                                "{} layer — not published (ceiling lowered)",
                                layer_quality_label(rung.layer_id, ladder_count as u32, false)
                            )
                        },
                        span { class: "perf-rung__bar", "aria-hidden": "true" }
                        span { class: "perf-rung__label", "{rung.res_label}" }
                    }
                }
            }
            div { class: "perf-side__caption",
                p {
                    class: "perf-range-value",
                    "data-testid": "{id_prefix}-range-value",
                    "{count_caption}"
                }
                p { class: "perf-summary-line", "{summary_line}" }
            }
        }
    }
}

/// One peer's RECEIVE snapshot for a SINGLE kind, flattened for the per-kind
/// receive summary (issue #1095 redesign). The panel splits the full per-peer
/// `PeerReceiveDiag` list into one `PeerKindSnap` Vec per kind so each
/// [`receive::ReceiveCell`] gets exactly the peers receiving ITS kind. The full
/// per-peer breakdown now lives in the Diagnostics panel's "Simulcast layers"
/// section; the card keeps only the aggregate summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerKindSnap {
    /// The peer's relay session id (used in the per-peer-row testid).
    pub session_id: u64,
    /// Human-friendly peer label (display name / user id / session id).
    pub label: String,
    /// The decoded layer snapshot for this kind.
    pub snap: ReceivedLayerSnapshot,
}

// ── help-popover bodies (§3 copy) ──────────────────────────────────

/// The panel intro, collapsed behind the header `(i)` info icon (it used to be an
/// always-visible paragraph that ate vertical space / the no-scroll budget). Plain
/// text (the popover body is a `&'static str`); the emphasis the old inline
/// `<span>`s carried is conveyed by the wording instead.
const HELP_PERF_INTRO: &str = "Each stream adapts to your connection automatically. Limit what you RECEIVE (saves your download) and what you SEND (saves your upload + CPU). For sending, the base layer is always sent so every viewer can see you; the right handle sets the highest layer you publish — how many layers you send. For receiving, the two handles bound the quality you'll accept. Reset returns to the full automatic range. The meter shows what's flowing right now.";

const HELP_VIDEO_SEND: &str = "Your camera sends several quality versions ('layers') so each viewer gets the best one their connection can handle. The base layer is ALWAYS sent (so every viewer can always see you); the right handle sets the HIGHEST layer you publish — i.e. how many layers you send. Lower it to send fewer top layers (saves your upload + CPU); raise it to send more, up to your device's limit. The encoder still adapts quality within what you allow. Reset returns to the full automatic ladder.";
const HELP_AUDIO_SEND: &str = "Your mic sends several audio quality versions ('layers') so each listener gets the best one their connection can handle. The base layer is ALWAYS sent (so everyone can always hear you); the right handle sets the HIGHEST audio layer you publish — i.e. how many layers you send. Lower it to send fewer top layers (saves your upload); raise it to send more, up to the audio ladder's limit. Reset returns to the full automatic ladder.";
const HELP_CONTENT_SEND: &str = "When you share your screen, the base layer is ALWAYS sent so every viewer can follow along. The right handle sets the HIGHEST layer you publish — how many sharpness layers you send. Lower it to send fewer top layers when your upload is tight; raise it for text-heavy screens, up to your device's limit. Reset returns to the full automatic ladder.";

/// The unified Performance settings panel body (#1095 redesign). Three stacked
/// per-kind cards (Video / Audio / Content), each split into a **Sending** column
/// and a **Receiving** column, so both directions are visible at once without a
/// direction tab. The panel now mounts INSIDE the Diagnostics drawer's "Quality
/// controls" group (#1131), which supplies the surrounding title + group label, so
/// the panel has no heading of its own: it leads with a slim simulcast strip whose
/// single `(i)` HelpPopover (testid `perf-intro-help`) holds the collapsed intro.
/// Two headless rAF drivers update the Sending and Receiving bar-meters
/// independently by id.
///
/// `pref` (send) + `receive_pref` are the current persisted preferences
/// (controlled by the parent). On any change the panel derives the new bounds
/// and calls the matching callback; the parent persists it and pushes it to the
/// encoder (send) or client (receive). The panel is otherwise stateless apart
/// from the open-popover signal and the throttled refresh tick.
#[component]
pub fn PerformanceSettingsPanel(
    // SEND side (#961). The snapshot readers default to inert `none()` readers so
    // call sites / tests that don't wire live encoders still compile and render
    // placeholder meters (the live drawer always wires them via the published
    // `PerfControlsHandle`).
    pref: PerformancePreference,
    on_change: EventHandler<PerformancePreference>,
    #[props(default = SnapshotReader::none())] read_snapshot: SnapshotReader,
    #[props(default = ScreenSnapshotReader::none())] read_screen_snapshot: ScreenSnapshotReader,
    // RECEIVE side (#989 simulcast).
    receive_pref: ReceivePreference,
    on_receive_change: EventHandler<(PrefMediaKind, KindReceivePref)>,
    #[props(default = ReceivedReader::none())] received_reader: ReceivedReader,
    // Live simulcast/AQ diagnostics (#1095 observability). Defaults to an inert
    // reader so existing call sites / tests that don't wire it still compile.
    #[props(default = DiagnosticsReader::none())] diagnostics_reader: DiagnosticsReader,
    // Effective simulcast layer ceilings for the SEND layer-count sliders
    // (sourced from `host.rs` where `effective_max_layers` is computed). These
    // set the slider's tick count AND its full/default ceiling so the control
    // reflects the REAL current ladder, not an abstract range. Default 1
    // (single-stream) so existing call sites / tests that don't wire them still
    // compile and render a 1-rung (no-op) layer control.
    #[props(default = 1)] video_layer_max: usize,
    #[props(default = 1)] screen_layer_max: usize,
    /// Audio's effective ladder depth (NOT CPU-clamped — audio encode is cheap),
    /// typically the full audio ladder even on weak runners.
    #[props(default = 1)]
    audio_layer_max: usize,
    /// Whether the MIC is currently capturing (threaded from `host.rs`'s
    /// `mic_enabled` prop). Audio has no per-layer SEND snapshot to infer this from
    /// (unlike video/screen, whose source-active state is the snapshot being
    /// `Some`), so the panel takes the mic state explicitly. Drives the audio
    /// caption's present-tense vs. "will send … when the mic is on" form. Default
    /// `false` so call sites / tests that don't wire it render the off-state copy.
    #[props(default)]
    audio_source_active: bool,
) -> Element {
    // First-paint SEND meter values (before the rAF driver ticks). The same pure
    // mapper drives the live loop, so first paint and live updates agree.
    let initial = read_snapshot.read();
    let initial_screen = read_screen_snapshot.read();
    let g = gauge_state_from_snapshot(initial.as_ref(), initial_screen.as_ref());

    // First-paint RECEIVE meter values.
    let rgv = receive::gauge_state(received_reader.read(PrefMediaKind::Video).as_ref());
    let rga = receive::gauge_state(received_reader.read(PrefMediaKind::Audio).as_ref());
    let rgs = receive::gauge_state(received_reader.read(PrefMediaKind::Screen).as_ref());

    // Which popover (if any) is currently open. `None` = all closed. Shared
    // across every cell so opening one closes the others.
    let open_help: Signal<Option<&'static str>> = use_signal(|| None);

    // Panel-level 4 Hz refresh tick. The per-card SUMMARY lines are always
    // visible AND live (layer count / total uplink / peer spread change at
    // runtime), so the panel subtree must re-render periodically. This loop is
    // gated to the panel mount (the panel only mounts inside the open Diagnostics
    // drawer's "Quality controls" group; #1131), and the summaries are cheap
    // (count / min-max). Scoping the tick to this child keeps it off the
    // top-level `Diagnostics` body, which would otherwise re-run its expensive
    // NetEq prelude 4×/s (#1128).
    //
    // Driven by a 250 ms `setInterval` (gloo `Interval`): the panel only needs a
    // ~4 Hz re-render, so the timer wakes 4×/s — a battery/CPU win on low-core
    // machines. The meter drivers stay on rAF. Cleanup: the `Interval` handle
    // lives in a `use_hook` cell and `use_drop` drops it on unmount.
    let mut diag_tick = use_signal(|| 0u64);
    {
        type IntervalCell = Rc<std::cell::RefCell<Option<gloo_timers::callback::Interval>>>;
        let cell: IntervalCell = use_hook(|| {
            let interval = gloo_timers::callback::Interval::new(250, move || {
                let next = diag_tick.peek().wrapping_add(1);
                diag_tick.set(next);
            });
            Rc::new(std::cell::RefCell::new(Some(interval)))
        });
        use_drop(move || {
            // Dropping the `Interval` cancels the underlying `setInterval`.
            *cell.borrow_mut() = None;
        });
    }
    // Subscribe this subtree to the throttled refresh.
    let _ = diag_tick();

    // Pull the live diagnostics once per (throttled) render. The SEND snapshots
    // and per-peer RECEIVE list feed the per-card summary lines below.
    let diag_summary = diagnostics_reader.summary;
    let strip_compact = format_simulcast_summary_compact(&diag_summary);
    let strip_full = format_simulcast_summary(&diag_summary);
    let send_video_snap = (diagnostics_reader.send_video)();
    let send_screen_snap = (diagnostics_reader.send_screen)();
    // Split the per-peer list into one Vec per kind for the receive summaries.
    let per_peer = (diagnostics_reader.per_peer_receive)();
    let recv_video_peers = peers_for_kind(&per_peer, PrefMediaKind::Video);
    let recv_audio_peers = peers_for_kind(&per_peer, PrefMediaKind::Audio);
    let recv_screen_peers = peers_for_kind(&per_peer, PrefMediaKind::Screen);

    // Per-card summary lines (filled live from the snapshots — §3 templates).
    let video_send_line = format_video_send_summary(send_video_snap.as_ref());
    let video_recv_line = format_video_receive_summary(
        &recv_video_peers
            .iter()
            .map(|p| p.snap.layer_index)
            .collect::<Vec<_>>(),
        videocall_client::max_layers_for_kind(PrefMediaKind::Video),
    );
    // Audio has no per-layer encoder snapshot; derive its send summary from the
    // chosen LAYER CEILING (count-aware) so it tracks the rung strip when the user
    // lowers the audio layer count — consistent with the "N of M layers" caption
    // the SendLayerCell renders. (The old bare-tier `format_audio_send_summary`
    // read "Sending high quality" regardless, contradicting the rungs.)
    let audio_send_line =
        format_audio_send_layer_summary(pref.audio_layers, audio_layer_max, audio_source_active);
    let audio_recv_line = format_audio_receive_summary(recv_audio_peers.len());
    let content_send_line = format_content_send_summary(send_screen_snap.as_ref());
    // Top-layer screen-share peer (highest layer_index) for the content receive line.
    let content_top = recv_screen_peers
        .iter()
        .max_by_key(|p| p.snap.layer_index)
        .map(|p| p.snap);
    let content_recv_line = format_content_receive_summary(content_top.as_ref());

    // §2 SEND rung strips: ALL THREE kinds (video, screen, AND audio) now use the
    // SEND layer-count control (`SendLayerCell`), which builds its OWN selection-
    // driven rung strip from the user's ceiling so the pips update as the ceiling
    // thumb drags (no dependence on the live snapshot for the active/shed boundary).

    // Receive-side consequence strings (peer counts; "not sharing" for content).
    let video_recv_consequence = consequence_from_peers(recv_video_peers.len());
    let audio_recv_consequence = consequence_from_peers(recv_audio_peers.len());
    let content_recv_consequence = if recv_screen_peers.is_empty() {
        "not sharing".to_string()
    } else {
        consequence_from_peers(recv_screen_peers.len())
    };

    rsx! {
        // Global effective-setting strip: compact copy + ONE help affordance.
        // The cross-nav button + "Performance" title row was removed when the panel
        // moved INTO the Diagnostics drawer (#1131): the drawer supplies the title +
        // the "Quality controls" group label, so a panel-local heading would double
        // up. The intro explanation stays, collapsed behind the single `(i)`
        // HelpPopover (single-open signal, aria-expanded, Escape/outside-click,
        // focus return). The earlier separate decorative `ⓘ` glyph was REMOVED
        // (#1131 review F2): two adjacent help affordances on one line duplicated;
        // the full per-layer simulcast detail it carried lives in Group B's
        // "Simulcast layers" section (its own source of truth, §3). The simulcast
        // framing is kept discoverable on the strip via the text span's title/aria.
        div {
            class: "perf-simulcast-strip",
            "data-testid": TESTID_SIMULCAST_STRIP,
            span {
                class: "perf-simulcast-strip__text",
                title: "Simulcast publishes multiple quality layers so viewers self-select. {strip_full}. Audio has its own ladder.",
                "aria-label": "Simulcast publishes multiple quality layers so viewers self-select. {strip_full}. Audio has its own ladder.",
                "{strip_compact}"
            }
            HelpPopover {
                key_id: "perf-intro",
                help_testid: "perf-intro-help",
                help_label: "About the Performance panel",
                help_body: HELP_PERF_INTRO,
                open_help,
            }
        }

        // Headless drivers: one ~4 Hz rAF loop each, updating the Sending and
        // Receiving bar-meters by id. They render nothing; both directions are
        // always mounted now, so every write lands.
        QualityVuMeterDriver { read_snapshot, read_screen_snapshot }
        receive::ReceivedQualityDriver { reader: received_reader }

        // ── Video card ──
        div { class: "perf-kind-card",
            div { class: "perf-kind-card__title", "Video" }
            div { class: "perf-card-cols",
                SendLayerCell {
                    stream_noun: "video",
                    id_prefix: "perf-video",
                    min_testid: TESTID_VIDEO_RANGE_MIN,
                    max_testid: TESTID_VIDEO_RANGE_MAX,
                    auto_testid: TESTID_VIDEO_AUTO,
                    help_testid: "perf-video-help",
                    help_body: HELP_VIDEO_SEND,
                    vu_testid: TESTID_VU_VIDEO,
                    vu_meter_id: VIDEO_METER_ID,
                    vu_readout_id: VIDEO_READOUT_ID,
                    vu_label: "Sending video",
                    vu_initial_level: g.video_level,
                    vu_initial_readout: g.video_text.clone(),
                    consequence: "your upload".to_string(),
                    kind: PrefMediaKind::Video,
                    layer_max: video_layer_max,
                    layers: pref.video_layers,
                    // Camera active iff the live SEND snapshot is Some (gated on the
                    // camera being enabled — the same signal as the "Camera — off"
                    // meter + the diagnostics reader).
                    source_active: send_video_snap.is_some(),
                    summary_line: video_send_line,
                    open_help,
                    on_ceiling_change: move |c: Option<u32>| on_change.call(pref.with_video_layers(c)),
                    on_reset: move |_| on_change.call(pref.with_video_layers(None)),
                }
                receive::ReceiveCell {
                    kind: PrefMediaKind::Video,
                    stream_noun: "video",
                    vu_initial_level: rgv.level,
                    vu_initial_readout: rgv.text.clone(),
                    consequence: video_recv_consequence,
                    summary_line: video_recv_line,
                    peers: recv_video_peers,
                    sub: receive_pref.video,
                    open_help,
                    on_change: move |sub: KindReceivePref| {
                        on_receive_change.call((PrefMediaKind::Video, sub));
                    },
                }
            }
        }

        // ── Audio card ──
        div { class: "perf-kind-card",
            div { class: "perf-kind-card__title", "Audio" }
            div { class: "perf-card-cols",
                SendLayerCell {
                    stream_noun: "audio",
                    id_prefix: "perf-audio",
                    min_testid: TESTID_AUDIO_RANGE_MIN,
                    max_testid: TESTID_AUDIO_RANGE_MAX,
                    auto_testid: TESTID_AUDIO_AUTO,
                    help_testid: "perf-audio-help",
                    help_body: HELP_AUDIO_SEND,
                    vu_testid: TESTID_VU_AUDIO,
                    vu_meter_id: AUDIO_METER_ID,
                    vu_readout_id: AUDIO_READOUT_ID,
                    vu_label: "Sending audio",
                    vu_initial_level: g.audio_level,
                    vu_initial_readout: g.audio_text.clone(),
                    consequence: "your upload".to_string(),
                    kind: PrefMediaKind::Audio,
                    layer_max: audio_layer_max,
                    layers: pref.audio_layers,
                    // Mic active iff the mic is enabled (threaded from host —
                    // audio has no per-layer SEND snapshot to infer it from).
                    source_active: audio_source_active,
                    summary_line: audio_send_line,
                    open_help,
                    on_ceiling_change: move |c: Option<u32>| on_change.call(pref.with_audio_layers(c)),
                    on_reset: move |_| on_change.call(pref.with_audio_layers(None)),
                }
                receive::ReceiveCell {
                    kind: PrefMediaKind::Audio,
                    stream_noun: "audio",
                    vu_initial_level: rga.level,
                    vu_initial_readout: rga.text.clone(),
                    consequence: audio_recv_consequence,
                    summary_line: audio_recv_line,
                    peers: recv_audio_peers,
                    sub: receive_pref.audio,
                    open_help,
                    on_change: move |sub: KindReceivePref| {
                        on_receive_change.call((PrefMediaKind::Audio, sub));
                    },
                }
            }
        }

        // ── Content (screen share) card ──
        div { class: "perf-kind-card",
            div { class: "perf-kind-card__title", "Content" }
            div { class: "perf-card-cols",
                SendLayerCell {
                    stream_noun: "screen share",
                    id_prefix: "perf-screen",
                    min_testid: TESTID_SCREEN_RANGE_MIN,
                    max_testid: TESTID_SCREEN_RANGE_MAX,
                    auto_testid: TESTID_SCREEN_AUTO,
                    help_testid: "perf-screen-help",
                    help_body: HELP_CONTENT_SEND,
                    vu_testid: TESTID_VU_SCREEN,
                    vu_meter_id: SCREEN_METER_ID,
                    vu_readout_id: SCREEN_READOUT_ID,
                    vu_label: "Sending screen",
                    vu_initial_level: g.screen_level,
                    vu_initial_readout: g.screen_text.clone(),
                    consequence: if send_screen_snap.is_some() { "your upload".to_string() } else { "not sharing".to_string() },
                    kind: PrefMediaKind::Screen,
                    layer_max: screen_layer_max,
                    layers: pref.screen_layers,
                    // Sharing iff the live screen SEND snapshot is Some (the same
                    // signal as the "not sharing" consequence above).
                    source_active: send_screen_snap.is_some(),
                    summary_line: content_send_line,
                    open_help,
                    on_ceiling_change: move |c: Option<u32>| on_change.call(pref.with_screen_layers(c)),
                    on_reset: move |_| on_change.call(pref.with_screen_layers(None)),
                }
                receive::ReceiveCell {
                    kind: PrefMediaKind::Screen,
                    stream_noun: "shared content",
                    vu_initial_level: rgs.level,
                    vu_initial_readout: rgs.text.clone(),
                    consequence: content_recv_consequence,
                    summary_line: content_recv_line,
                    peers: recv_screen_peers,
                    sub: receive_pref.screen,
                    open_help,
                    on_change: move |sub: KindReceivePref| {
                        on_receive_change.call((PrefMediaKind::Screen, sub));
                    },
                }
            }
        }
    }
}

/// The receive-side consequence string for a card head, e.g. `"from 4 peers"`
/// (singular `"from 1 peer"`); 0 peers → `"no senders"`. Pure / host-tested.
pub fn consequence_from_peers(n: usize) -> String {
    match n {
        0 => "no senders".to_string(),
        1 => "from 1 peer".to_string(),
        n => format!("from {n} peers"),
    }
}

/// Flatten the per-peer `PeerReceiveDiag` list down to the peers receiving ONE
/// kind, for that kind's receive summary line and for the Diagnostics panel's
/// "Simulcast layers" per-peer breakdown. Pure / host-tested.
pub fn peers_for_kind(peers: &[PeerReceiveDiag], kind: PrefMediaKind) -> Vec<PeerKindSnap> {
    peers
        .iter()
        .filter_map(|p| {
            let snap = match kind {
                PrefMediaKind::Video => p.video,
                PrefMediaKind::Audio => p.audio,
                PrefMediaKind::Screen => p.screen,
            }?;
            Some(PeerKindSnap {
                session_id: p.session_id,
                label: p.label.clone(),
                snap,
            })
        })
        .collect()
}

// ══════════════════════════════════════════════════════════════════════════
// RECEIVE side (simulcast P4/P5). Layer-index convention is DIRECT: index 0 =
// LOWEST quality, higher = HIGHER. Kept in its own module so its RangeSel /
// span_text / bounds_to_thumbs cannot be confused with the inverted send-side
// ones above.
// ══════════════════════════════════════════════════════════════════════════
pub mod receive {
    use super::{
        format_receive_spread, layer_quality_label, level_from_fraction, peer_row_aria_label,
        peer_row_metric, quality_state_glyph, quality_state_modifier, reason_chip_modifier,
        reason_chip_text, reason_chip_title, tick_offsets, write_meter_level, write_readout_text,
        PeerKindSnap,
    };
    use dioxus::prelude::*;
    use std::rc::Rc;
    use videocall_client::{
        max_layers_for_kind, quality_state, PrefMediaKind, ReceivedLayerSnapshot,
    };
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
    ///
    /// These mirror `videocall_aq::simulcast_layers(3)` = `[low, standard, hd]`,
    /// lowest-first: low = 640×360 (360p), standard = 960×540 (540p), hd =
    /// 1280×720 (720p). The middle "540p" is correct — `simulcast_layers(3)[1]`
    /// is the "standard" tier at 960×540 (#1079 reviewer confirmation).
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

    // ── bar-meter math + readout ───────────────────────────────────

    /// Empty-state meter level: 0 = all bars unlit (nothing received).
    pub const EMPTY_METER_LEVEL: u8 = super::EMPTY_METER_LEVEL;

    /// Empty-state readout shown when nothing of a kind is being received.
    pub const EMPTY_READOUT: &str = "Not receiving";

    /// The 0..=1 quality fraction for a RECEIVE layer index (0 = lowest). Lowest
    /// layer → `0.0`, top layer → `1.0`. Single-layer → `1.0`. Out-of-range
    /// clamps. Note the convention is OPPOSITE the send side (here index 0 is the
    /// worst, not the best). Pure / host-tested.
    pub fn layer_quality_fraction(layer_index: u32, layer_count: u32) -> f32 {
        if layer_count <= 1 {
            return 1.0;
        }
        let max_idx = layer_count - 1;
        let clamped = layer_index.min(max_idx);
        clamped as f32 / max_idx as f32
    }

    /// Convert a decoded `layer_index` into a meter level `0..=4`. Top layer →
    /// 4; lowest layer → 1 (never 0 — a flowing stream lights at least one bar;
    /// level 0 is the no-signal empty state). Pure / host-tested.
    pub fn meter_level(layer_index: u32, layer_count: u32) -> u8 {
        level_from_fraction(layer_quality_fraction(layer_index, layer_count)).max(1)
    }

    /// Format the readout line for a received snapshot. Video/screen show
    /// `"{Q} · {i+1}/{n} · {w}x{h}"`; audio shows `"{Q} · {i+1}/{n} · {kbps} kbps"`,
    /// where `{Q}` is the quality letter (L/M/H via [`super::layer_quality_label`]).
    /// Pure.
    pub fn format_readout(snap: &ReceivedLayerSnapshot) -> String {
        let layer = snap.layer_index + 1;
        let q = layer_quality_label(snap.layer_index, snap.layer_count, true);
        match snap.kind {
            PrefMediaKind::Audio => {
                format!("{q} · {layer}/{} · {} kbps", snap.layer_count, snap.kbps)
            }
            _ => format!(
                "{q} · {layer}/{} · {}x{}",
                snap.layer_count, snap.width, snap.height
            ),
        }
    }

    /// One meter's render state: lit-bar level + readout text. Pure.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct GaugeState {
        pub level: u8,
        pub text: String,
    }

    /// Map an optional received snapshot to a meter's render state. `Some` →
    /// live level + readout; `None` → level 0 (all bars unlit) + "Not receiving".
    pub fn gauge_state(snap: Option<&ReceivedLayerSnapshot>) -> GaugeState {
        match snap {
            Some(s) => GaugeState {
                level: meter_level(s.layer_index, s.layer_count),
                text: format_readout(s),
            },
            None => GaugeState {
                level: EMPTY_METER_LEVEL,
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
    // RECEIVE bar-meter testids. Names unchanged (#1095 e2e selectors) even
    // though the element is now a `.perf-meter`, not the retired VU needle.
    pub const TESTID_VU_VIDEO: &str = "perf-vu-recv-video";
    pub const TESTID_VU_AUDIO: &str = "perf-vu-recv-audio";
    pub const TESTID_VU_SCREEN: &str = "perf-vu-recv-screen";

    // Receive meter + readout ids — DISTINCT from the send ids so the two
    // headless drivers never write the same node.
    const VIDEO_METER_ID: &str = "perf-meter-recv-video";
    const AUDIO_METER_ID: &str = "perf-meter-recv-audio";
    const SCREEN_METER_ID: &str = "perf-meter-recv-screen";
    // Readout ids keep their original `perf-vu-recv-*-readout` names so the
    // #989 e2e that polls these by `#id` still resolves after the meter swap.
    const VIDEO_READOUT_ID: &str = "perf-vu-recv-video-readout";
    const AUDIO_READOUT_ID: &str = "perf-vu-recv-audio-readout";
    const SCREEN_READOUT_ID: &str = "perf-vu-recv-screen-readout";

    /// DOM ids for a kind's receive meter + readout.
    fn meter_ids(kind: PrefMediaKind) -> (&'static str, &'static str) {
        match kind {
            PrefMediaKind::Video => (VIDEO_METER_ID, VIDEO_READOUT_ID),
            PrefMediaKind::Audio => (AUDIO_METER_ID, AUDIO_READOUT_ID),
            PrefMediaKind::Screen => (SCREEN_METER_ID, SCREEN_READOUT_ID),
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
                help_body: "You pull the quality layer each sender offers that best fits your download. 'L2 of 3' means the highest of three versions. The left handle sets the lowest quality you'll accept (floor), the right handle the highest (ceiling); it adapts within that band. Lower the ceiling to save your download. Reset returns to the full automatic range.",
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
                help_body: "You pull the clearest audio each speaker offers that fits your download. The left handle sets the lowest quality you'll accept (floor), the right handle the highest (ceiling). Reset returns to the full automatic range.",
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
                help_body: "You pull the sharpest screen-share layer the presenter offers that fits your download. The left handle sets the lowest quality you'll accept (floor), the right handle the highest (ceiling); it adapts within that band. Reset returns to the full automatic range.",
                id_prefix: "perf-recv-screen",
            },
        }
    }

    // ── components ─────────────────────────────────────────────────

    /// Headless driver for the three received-quality bar-meters. Renders
    /// nothing; owns one ~4 Hz rAF loop reading `received_layer_snapshot(kind)`
    /// per kind and writing each meter's `data-level` + readout to the DOM by id.
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
                            let (meter_id, readout_id) = meter_ids(kind);
                            write_meter_level(meter_id, state.level);
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
        // Decorative tick marks — one per step position (`0..=top`, so `top + 1`
        // ticks), aligned to the receive thumb stops.
        let ticks = tick_offsets((top + 1) as usize);

        rsx! {
            div { class: "perf-range",
                span { class: "perf-range-end-label", "{index_label(kind, 0)}" }
                div { class: "perf-range-track-wrap",
                    div { class: "perf-range-track",
                        // Tick marks: see DualRangeSlider — inside the track, below
                        // fill/thumbs, `pointer-events: none` + aria-hidden so they
                        // never intercept a thumb drag (preserves the WebKit fix).
                        div {
                            class: "perf-range-ticks",
                            "aria-hidden": "true",
                            "data-testid": "{id_prefix}-range-ticks",
                            for off in ticks.iter() {
                                span {
                                    class: "perf-range-tick",
                                    style: "left: {off}%;",
                                }
                            }
                        }
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
                        "aria-label": "Minimum {stream_noun} receive quality",
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
                        "aria-label": "Maximum {stream_noun} receive quality",
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

    /// One kind's RECEIVE column inside a `KindCard`: a "Receiving" head
    /// (consequence + "?" help + Auto/Fixed), a bar-meter, a dual-thumb slider,
    /// and a live summary line (#1095 redesign — replaces the old `ReceiveRow` +
    /// diagnostics footer; the full per-peer breakdown moved to the Diagnostics
    /// panel's "Simulcast layers" section).
    #[allow(clippy::too_many_arguments)]
    #[component]
    pub fn ReceiveCell(
        kind: PrefMediaKind,
        stream_noun: &'static str,
        vu_initial_level: u8,
        vu_initial_readout: String,
        /// The "from N peers" / "not sharing" consequence text right of the title.
        consequence: String,
        /// The always-visible summary line under the slider (filled live by the
        /// parent from the per-peer snapshots).
        summary_line: String,
        /// The peers receiving THIS kind (issue #1131), for the §3 expandable
        /// per-peer breakdown. Empty → render the "No senders" empty state.
        #[props(default)]
        peers: Vec<PeerKindSnap>,
        sub: KindReceivePref,
        open_help: Signal<Option<&'static str>>,
        on_change: EventHandler<KindReceivePref>,
    ) -> Element {
        let meta = recv_meta(kind);
        let sel = bounds_to_thumbs(kind, sub.min, sub.max);
        let range_str = span_text(kind, sel);
        // Fixed = manual (not Auto) AND both thumbs collapsed to one layer.
        let is_fixed = !sub.auto && sel.min_pos == sel.max_pos;
        // Reset is shown IFF the thumbs are NOT at both extremes (#1131 §D), driven
        // by POSITIONS (not the `auto` flag), so dragging both back to the ends
        // hides it live. `top_index` is the receive ladder's last position.
        let show_reset = !super::at_full_range(
            sel.min_pos as usize,
            sel.max_pos as usize,
            top_index(kind) as usize,
        );
        let (vu_meter_id, vu_readout_id) = meter_ids(kind);

        // §3 per-peer disclosure data (issue #1131). `full_ladder_len` is the
        // FULL ladder size for this kind (the color basis + the `L i / n`
        // denominator), NOT the empirically-learned per-peer count. The aggregate
        // summary reuses `format_receive_spread` over the per-peer layer indices.
        let full_ladder_len = max_layers_for_kind(kind);
        let peer_count = peers.len();
        let spread = format_receive_spread(
            &peers.iter().map(|p| p.snap.layer_index).collect::<Vec<_>>(),
            full_ladder_len,
        );
        let agg = if peer_count == 1 {
            format!("1 peer · {spread}")
        } else {
            format!("{peer_count} peers · {spread}")
        };
        let id_prefix = meta.id_prefix;

        rsx! {
            div { class: "perf-side perf-side--recv",
                div { class: "perf-side__head",
                    span { class: "perf-side__title",
                        // §1 directional arrow (arrow-down-left, blue --accent),
                        // aria-hidden — "Receiving" remains the a11y label.
                        svg {
                            class: "perf-dir-arrow",
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "14", height: "14", view_box: "0 0 24 24",
                            fill: "none", stroke: "currentColor", stroke_width: "2",
                            stroke_linecap: "round", stroke_linejoin: "round",
                            "aria-hidden": "true",
                            path { d: "M17 7 7 17" }
                            path { d: "M17 17H7V7" }
                        }
                        "Receiving"
                    }
                    span { class: "perf-side__consequence", "{consequence}" }
                    super::PerfMeter {
                        testid: meta.vu_testid,
                        meter_id: vu_meter_id,
                        readout_id: vu_readout_id,
                        label: meta.vu_label,
                        initial_level: vu_initial_level,
                        initial_readout: vu_initial_readout,
                    }
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
                    // Reset clears both receive handles back to the full automatic
                    // range (which snaps the thumbs to the extremes and re-hides
                    // this button). Rendered ONLY when the thumbs are off the
                    // extremes (`show_reset`); at the full default range the slot is
                    // EMPTY (#1131 §D). Repurposes the former Auto testid so the
                    // testid surface is unchanged.
                    if show_reset {
                        button {
                            r#type: "button",
                            class: "perf-reset-button",
                            "data-testid": meta.auto_testid,
                            "aria-label": "Reset {stream_noun} quality limits",
                            title: "Clear both limits — back to the full automatic range",
                            onclick: move |_| {
                                on_change.call(KindReceivePref { min: None, max: None, auto: true });
                            },
                            "Reset"
                        }
                    }
                }
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
                // One flex line for the range readout + live summary (#2e). No
                // aria-live on the range-value (#4).
                div { class: "perf-side__caption",
                    p {
                        class: "perf-range-value",
                        "data-testid": "{meta.id_prefix}-range-value",
                        "Receiving: {range_str}"
                    }
                    p { class: "perf-summary-line", "{summary_line}" }
                }
                // §3 per-peer expandable breakdown. Native <details> gives free
                // aria-expanded + keyboard toggle; COLLAPSED by default to protect
                // the no-scroll budget (§7). EMPTY case is intentionally NOT given a
                // separate "No senders" line: the always-visible summary line above
                // already states it ("Not receiving …" / "Nobody is sharing"), so a
                // second empty line would be redundant copy AND net-new vertical
                // height against the tight no-scroll budget. The disclosure only
                // renders once at least one peer is receiving this kind.
                if !peers.is_empty() {
                    details {
                        class: "perf-peers",
                        "data-testid": "{id_prefix}-peers",
                        summary {
                            class: "perf-peers__summary",
                            "data-testid": "{id_prefix}-peers-summary",
                            svg {
                                class: "perf-peers__chev",
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "12", height: "12", view_box: "0 0 24 24",
                                fill: "none", stroke: "currentColor", stroke_width: "2",
                                stroke_linecap: "round", stroke_linejoin: "round",
                                "aria-hidden": "true",
                                path { d: "m9 18 6-6-6-6" }
                            }
                            span { class: "perf-peers__agg", "{agg}" }
                        }
                        ul { class: "perf-peers__list", role: "list",
                            for p in peers.iter() {
                                PeerRow {
                                    key: "{p.session_id}",
                                    id_prefix,
                                    kind,
                                    stream_noun,
                                    full_ladder_len,
                                    peer: p.clone(),
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// One peer row inside the §3 receive disclosure (issue #1131): a quality dot
    /// (color + non-color glyph), the ellipsized peer label, the metric text, and
    /// — only when the reception is below the full-ladder top — a tinted reason
    /// chip. The whole row carries a full-sentence `aria-label` so color is never
    /// the sole signal.
    #[component]
    fn PeerRow(
        id_prefix: &'static str,
        kind: PrefMediaKind,
        stream_noun: &'static str,
        full_ladder_len: u32,
        peer: PeerKindSnap,
    ) -> Element {
        let snap = peer.snap;
        let q = quality_state(snap.layer_index, full_ladder_len);
        let q_mod = quality_state_modifier(q);
        let q_glyph = quality_state_glyph(q);
        // Audio rung label for the metric ("low (24k)"/"mid (32k)"/"high (50k)").
        let audio_label = index_label(PrefMediaKind::Audio, snap.layer_index);
        let metric = peer_row_metric(&snap, full_ladder_len, audio_label);
        // The human res/bitrate detail used inside the aria sentence.
        let res_or_bitrate = if matches!(kind, PrefMediaKind::Audio) {
            format!("{}k", snap.kbps)
        } else {
            super::format_send_layer_short(snap.width, snap.height)
        };
        let aria = peer_row_aria_label(
            &peer.label,
            // `stream_noun` is the spoken kind ("video"/"audio"/"shared content").
            stream_noun,
            q,
            &res_or_bitrate,
            snap.layer_index + 1,
            full_ladder_len,
            snap.reason,
        );
        let session_id = peer.session_id;

        rsx! {
            li {
                class: "perf-peer-row",
                "data-testid": "{id_prefix}-peer-{session_id}",
                "aria-label": "{aria}",
                span {
                    class: "perf-q-dot perf-q-dot--{q_mod}",
                    "data-testid": "{id_prefix}-peer-{session_id}-q",
                    "aria-hidden": "true",
                    "{q_glyph}"
                }
                span {
                    class: "perf-peer-row__label",
                    title: "{peer.label}",
                    "{peer.label}"
                }
                span { class: "perf-peer-row__metric", "{metric}" }
                if let Some(r) = snap.reason {
                    span {
                        class: "perf-reason-chip perf-reason-chip--{reason_chip_modifier(r)}",
                        "data-testid": "{id_prefix}-peer-{session_id}-reason",
                        title: "{reason_chip_title(r)}",
                        "{reason_chip_text(r)}"
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
        fn meter_level_lowest_is_one_highest_is_max() {
            // Receive convention: layer 0 = LOWEST → level 1 (one bar, never 0);
            // top layer = HIGHEST → level 4 (all bars). The 3-layer middle is
            // index 1 → frac 0.5 → level 2.
            let max = crate::components::performance_settings::MAX_METER_LEVEL;
            assert_eq!(meter_level(0, 3), 1);
            assert_eq!(meter_level(2, 3), max);
            assert_eq!(meter_level(1, 3), 2); // frac 0.5 → round(2.0) = 2
            assert_eq!(meter_level(0, 2), 1);
            assert_eq!(meter_level(1, 2), max);
        }

        #[test]
        fn meter_level_clamps_and_single_layer_is_max() {
            // Out-of-range clamps to the top; a single layer is the best available.
            let max = crate::components::performance_settings::MAX_METER_LEVEL;
            assert_eq!(meter_level(99, 3), max);
            assert_eq!(meter_level(0, 1), max);
            assert_eq!(meter_level(0, 0), max);
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
                reason: None,
            };
            assert_eq!(format_readout(&v), "M · 2/3 · 960x540");
            let a = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Audio,
                layer_index: 0,
                layer_count: 3,
                width: 0,
                height: 0,
                kbps: 24,
                reason: None,
            };
            assert_eq!(format_readout(&a), "L · 1/3 · 24 kbps");
            let s = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Screen,
                layer_index: 2,
                layer_count: 3,
                width: 1920,
                height: 1080,
                kbps: 2500,
                reason: None,
            };
            assert_eq!(format_readout(&s), "H · 3/3 · 1920x1080");
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
                reason: None,
            };
            let st = gauge_state(Some(&snap));
            // top layer → all bars
            assert_eq!(
                st.level,
                crate::components::performance_settings::MAX_METER_LEVEL
            );
            assert_eq!(st.text, "H · 3/3 · 1280x720");
            let empty = gauge_state(None);
            assert_eq!(empty.level, EMPTY_METER_LEVEL);
            assert_eq!(empty.level, 0); // no signal → all bars unlit
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

    // ── Help popover positioning (drawer scroll-clip fix, #1131) ──────
    // The popover is `position: fixed`; this math keeps it on-screen on every
    // edge. All cases use gap = HELP_POPOVER_GAP_PX (8.0).

    /// Roomy case: button mid-viewport → popover sits directly below, left edge
    /// aligned to the button. (Breaks if the below-default or left-align changes.)
    #[test]
    fn help_popover_opens_below_when_room() {
        // button at (100, 200)-(122, 222), popover 260x120, viewport 720x1000.
        let (left, top) =
            compute_help_popover_position(100.0, 200.0, 222.0, 260.0, 120.0, 720.0, 1000.0);
        assert_eq!(left, 100.0, "left aligns to the button when it fits");
        assert_eq!(top, 222.0 + 8.0, "opens just below the button");
    }

    /// Right-edge clip: a button near the RIGHT drawer border must pull the
    /// popover LEFT so its right edge stays inside `viewport_w - gap`. (This is
    /// the horizontal half of the reported bug.)
    #[test]
    fn help_popover_clamps_off_right_edge() {
        // viewport 720 wide; button left at 700 would put a 260px popover at
        // 700..960, 240px off-screen. Expect left clamped to 720-260-8 = 452.
        let (left, _top) =
            compute_help_popover_position(700.0, 100.0, 122.0, 260.0, 120.0, 720.0, 1000.0);
        assert_eq!(
            left, 452.0,
            "popover pulled left to stay within the viewport"
        );
        assert!(left + 260.0 <= 720.0 - 8.0 + 0.001);
    }

    /// Bottom-fold clip: a tall popover whose below-placement would overflow the
    /// bottom, with MORE room above, must FLIP above the button. (The vertical
    /// half of the bug — a pure-CSS `top: calc(100% + 8px)` could never do this.)
    #[test]
    fn help_popover_flips_above_near_bottom() {
        // viewport 600 tall; button at bottom (560..582), popover 200 tall.
        // Below would be 590..790 (off-screen). Above-room (560-8=552) >
        // below-room (600-582-8=10) → flip: top = 560 - 8 - 200 = 352.
        let (_left, top) =
            compute_help_popover_position(100.0, 560.0, 582.0, 260.0, 200.0, 720.0, 600.0);
        assert_eq!(top, 352.0, "flips above the button when below overflows");
        assert!(top >= 8.0, "stays below the top margin");
    }

    /// Degenerate: popover taller than EITHER side → kept below but clamped so it
    /// never spills past the bottom (its max-height + internal scroll do the rest).
    #[test]
    fn help_popover_clamps_when_taller_than_viewport() {
        // popover 900 tall, viewport 600 → max_top = max(600-900-8, 8) = 8.
        let (_left, top) =
            compute_help_popover_position(100.0, 100.0, 122.0, 260.0, 900.0, 720.0, 600.0);
        assert_eq!(top, 8.0, "clamped to the top margin, not off-screen");
    }

    // ── Live diagnostics formatters (issue #1095) ──────────────────────

    #[test]
    fn simulcast_summary_formats_layers_and_off() {
        // Multi-layer: shows effective count + flag × cap breakdown per kind.
        let s = SimulcastSummary {
            flag: 3,
            video_capability: 3,
            audio_capability: 3,
            effective_video: 3,
            effective_audio: 3,
        };
        assert_eq!(
            format_simulcast_summary(&s),
            "Video/Screen: 3 layers (flag 3 × device cap 3) · Audio: 3 layers (flag 3 × device cap 3)"
        );
        // A weak device gated to 1 video layer reads "off"; audio independent.
        let weak = SimulcastSummary {
            flag: 3,
            video_capability: 1,
            audio_capability: 3,
            effective_video: 1,
            effective_audio: 3,
        };
        assert_eq!(
            format_simulcast_summary(&weak),
            "Video/Screen: off (1 layer) · Audio: 3 layers (flag 3 × device cap 3)"
        );
        // Flag off entirely → both off.
        let off = SimulcastSummary {
            flag: 1,
            video_capability: 3,
            audio_capability: 3,
            effective_video: 1,
            effective_audio: 1,
        };
        assert_eq!(
            format_simulcast_summary(&off),
            "Video/Screen: off (1 layer) · Audio: off (1 layer)"
        );
    }

    /// `layer_quality_label` (#1222): position→quality name (full + compact).
    /// Pins every branch — 3-layer Low/Med/High, 2-layer Low/High, degenerate
    /// 1-layer "Single"/"1", and the i>=top rule in a 4-layer ladder (index 1 and
    /// 2 are BOTH Medium; index 3 is High). Mutating `i >= top` to `i > top` would
    /// make (3,4) report Medium instead of High → fails; mutating the count==2
    /// branch flips (1,2); the (any,1) cases pin the degenerate guard.
    #[test]
    fn layer_quality_label_all_branches() {
        // 3-layer full + compact.
        assert_eq!(layer_quality_label(0, 3, false), "Low");
        assert_eq!(layer_quality_label(1, 3, false), "Medium");
        assert_eq!(layer_quality_label(2, 3, false), "High");
        assert_eq!(layer_quality_label(0, 3, true), "L");
        assert_eq!(layer_quality_label(1, 3, true), "M");
        assert_eq!(layer_quality_label(2, 3, true), "H");
        // 2-layer: Low/High only (no Medium).
        assert_eq!(layer_quality_label(0, 2, false), "Low");
        assert_eq!(layer_quality_label(1, 2, false), "High");
        assert_eq!(layer_quality_label(0, 2, true), "L");
        assert_eq!(layer_quality_label(1, 2, true), "H");
        // 1-layer degenerate.
        assert_eq!(layer_quality_label(0, 1, false), "Single");
        assert_eq!(layer_quality_label(0, 1, true), "1");
        assert_eq!(layer_quality_label(5, 1, false), "Single");
        // 4-layer: top = index 3 is High; everything 0<i<top is Medium.
        assert_eq!(layer_quality_label(0, 4, false), "Low");
        assert_eq!(layer_quality_label(1, 4, false), "Medium");
        assert_eq!(layer_quality_label(2, 4, false), "Medium");
        assert_eq!(layer_quality_label(3, 4, false), "High");
    }

    #[test]
    fn send_layer_and_header_formatting() {
        // DISPLAY by quality name: in a 3-layer ladder, internal layer_id 0 (the
        // base) renders "Low"; id 2 (the top) renders "High". The internal id /
        // data-testid suffix stays 0-based; only the visible label changes (#1222).
        assert_eq!(
            format_send_layer(0, 3, 640, 360, 400),
            "Low · 640×360 · 400 kbps"
        );
        assert_eq!(
            format_send_layer(2, 3, 1280, 720, 1500),
            "High · 1280×720 · 1500 kbps"
        );
        // Header: active/effective vs single-stream.
        let multi = SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: 3,
            active_layers: 2,
            layers: Vec::new(),
        };
        assert_eq!(format_send_header(&multi), "2 of 3 layers active");
        let single = SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: Vec::new(),
        };
        assert_eq!(format_send_header(&single), "Single layer");
    }

    #[test]
    fn peer_kind_line_formats_video_audio_and_none() {
        // Video → resolution; quality LETTER + 1-indexed position/total.
        let v = ReceivedLayerSnapshot {
            kind: PrefMediaKind::Video,
            layer_index: 2,
            layer_count: 3,
            width: 1280,
            height: 720,
            kbps: 1500,
            reason: None,
        };
        assert_eq!(
            format_peer_kind_line("video", Some(&v)),
            Some("video H · 3/3 · 1280×720".to_string())
        );
        // Audio → kbps detail.
        let a = ReceivedLayerSnapshot {
            kind: PrefMediaKind::Audio,
            layer_index: 0,
            layer_count: 3,
            width: 0,
            height: 0,
            kbps: 24,
            reason: None,
        };
        assert_eq!(
            format_peer_kind_line("audio", Some(&a)),
            Some("audio L · 1/3 · 24 kbps".to_string())
        );
        // None → no line.
        assert_eq!(format_peer_kind_line("screen", None), None);
    }

    #[test]
    fn diagnostics_reader_none_is_inert() {
        // The default reader yields no video snapshot (camera-off / unwired), no
        // screen, and no peers — so a panel without diagnostics wired renders the
        // static "off" lines, not a stale disclosure body.
        let r = DiagnosticsReader::none();
        assert!((r.send_video)().is_none());
        assert!((r.send_screen)().is_none());
        assert!((r.per_peer_receive)().is_empty());
        assert_eq!(r.summary, SimulcastSummary::default());
    }

    #[test]
    fn simulcast_strip_testid_is_stable() {
        // The global effective-setting strip testid is still driven by e2e; pin it
        // so a rename is a test failure. (The expandable per-row `{prefix}-diag*`
        // scheme moved to the Diagnostics panel's "Simulcast layers" section in
        // the #1095 redesign and no longer exists here.)
        assert_eq!(TESTID_SIMULCAST_STRIP, "perf-simulcast-strip");
    }

    // ── New compact formatters (issue #1095 redesign) ──────────────────

    #[test]
    fn simulcast_summary_compact_layers_and_off() {
        let on = SimulcastSummary {
            flag: 3,
            video_capability: 3,
            audio_capability: 3,
            effective_video: 3,
            effective_audio: 3,
        };
        assert_eq!(
            format_simulcast_summary_compact(&on),
            "Simulcast: 3 layers (device cap 3)"
        );
        // device cap can differ from effective (flag lower than cap).
        let capped = SimulcastSummary {
            flag: 2,
            video_capability: 3,
            audio_capability: 3,
            effective_video: 2,
            effective_audio: 2,
        };
        assert_eq!(
            format_simulcast_summary_compact(&capped),
            "Simulcast: 2 layers (device cap 3)"
        );
        // effective_video == 1 → "off".
        let off = SimulcastSummary {
            flag: 1,
            video_capability: 3,
            audio_capability: 3,
            effective_video: 1,
            effective_audio: 1,
        };
        assert_eq!(format_simulcast_summary_compact(&off), "Simulcast: off");
    }

    #[test]
    fn send_total_kbps_sums_only_active_layers() {
        let snap = SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: 3,
            active_layers: 2, // top (L2) is shed → not counted
            layers: vec![
                videocall_client::SimulcastLayerInfo {
                    layer_id: 0,
                    bitrate_kbps: 400,
                    width: 640,
                    height: 360,
                },
                videocall_client::SimulcastLayerInfo {
                    layer_id: 1,
                    bitrate_kbps: 900,
                    width: 960,
                    height: 540,
                },
                videocall_client::SimulcastLayerInfo {
                    layer_id: 2,
                    bitrate_kbps: 1500,
                    width: 1280,
                    height: 720,
                },
            ],
        };
        assert_eq!(format_send_total_kbps(&snap), 1300); // 400 + 900 (L2 shed)
                                                         // All 3 active → full sum.
        let all = SimulcastSendSnapshot {
            active_layers: 3,
            ..snap
        };
        assert_eq!(format_send_total_kbps(&all), 2800);
        // Single-stream (empty layers) → 0.
        let single = SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: Vec::new(),
        };
        assert_eq!(format_send_total_kbps(&single), 0);
    }

    #[test]
    fn send_layer_short_uses_shorter_dimension() {
        assert_eq!(format_send_layer_short(640, 360), "360p"); // landscape
        assert_eq!(format_send_layer_short(360, 640), "360p"); // portrait
        assert_eq!(format_send_layer_short(1280, 720), "720p");
        // Degenerate dim → raw fallback.
        assert_eq!(format_send_layer_short(0, 720), "0×720");
    }

    #[test]
    fn kbps_compact_k_and_m() {
        assert_eq!(format_kbps_compact(400), "400k");
        assert_eq!(format_kbps_compact(999), "999k");
        assert_eq!(format_kbps_compact(1400), "1.4M");
        assert_eq!(format_kbps_compact(2500), "2.5M");
        // Whole-Mbps trims the ".0".
        assert_eq!(format_kbps_compact(1000), "1M");
        assert_eq!(format_kbps_compact(2000), "2M");
    }

    #[test]
    fn receive_spread_range_collapse_and_empty() {
        // Mixed layers in a 3-ladder → lo–hi quality letters (L–H), en-dash.
        assert_eq!(format_receive_spread(&[0, 1, 2], 3), "L\u{2013}H");
        assert_eq!(format_receive_spread(&[2, 0], 3), "L\u{2013}H");
        // All same → single letter (no count; endpoints imply the ladder).
        assert_eq!(format_receive_spread(&[2, 2, 2], 3), "H");
        assert_eq!(format_receive_spread(&[0], 3), "L");
        // Empty → empty string.
        assert_eq!(format_receive_spread(&[], 3), "");
    }

    #[test]
    fn mbps_one_decimal() {
        assert_eq!(format_mbps(2600), "2.6 Mbps");
        assert_eq!(format_mbps(400), "0.4 Mbps");
        assert_eq!(format_mbps(2800), "2.8 Mbps");
    }

    // ── Per-card summary lines + consequence (#1095 redesign, §3 copy) ──

    fn send_snap_3layer() -> SimulcastSendSnapshot {
        SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: 3,
            active_layers: 3,
            layers: vec![
                videocall_client::SimulcastLayerInfo {
                    layer_id: 0,
                    bitrate_kbps: 400,
                    width: 640,
                    height: 360,
                },
                videocall_client::SimulcastLayerInfo {
                    layer_id: 1,
                    bitrate_kbps: 900,
                    width: 960,
                    height: 540,
                },
                videocall_client::SimulcastLayerInfo {
                    layer_id: 2,
                    bitrate_kbps: 1500,
                    width: 1280,
                    height: 720,
                },
            ],
        }
    }

    #[test]
    fn send_layer_res_span_spans_lowest_to_highest() {
        assert_eq!(send_layer_res_span(&send_snap_3layer()), "360p–720p");
        // All one resolution → single label.
        let mut s = send_snap_3layer();
        for l in &mut s.layers {
            l.width = 1280;
            l.height = 720;
        }
        assert_eq!(send_layer_res_span(&s), "720p");
        // No known resolutions (atomics not ticked) → empty.
        let single = SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: Vec::new(),
        };
        assert_eq!(send_layer_res_span(&single), "");
    }

    #[test]
    fn video_send_summary_camera_off_and_active() {
        // Camera off (None) → the spec's off line.
        assert_eq!(format_video_send_summary(None), "Camera — off");
        // Active simulcast → "Sending A of E layers · lo–hi".
        let s = send_snap_3layer();
        assert_eq!(
            format_video_send_summary(Some(&s)),
            "Sending 3 of 3 layers · 360p–720p"
        );
        // Single-layer → "Sending single layer · {res}".
        let single = SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: vec![videocall_client::SimulcastLayerInfo {
                layer_id: 0,
                bitrate_kbps: 800,
                width: 1280,
                height: 720,
            }],
        };
        assert_eq!(
            format_video_send_summary(Some(&single)),
            "Sending single layer · 720p"
        );
    }

    #[test]
    fn video_receive_summary_peers_and_spread() {
        assert_eq!(format_video_receive_summary(&[], 3), "Not receiving video");
        assert_eq!(
            format_video_receive_summary(&[0, 1, 2, 2], 3),
            "Pulling up to high quality · L\u{2013}H across 4 peers"
        );
        // Singular peer: index 1 in a 3-ladder → "M".
        assert_eq!(
            format_video_receive_summary(&[1], 3),
            "Pulling up to high quality · M across 1 peer"
        );
    }

    #[test]
    fn audio_receive_summary_peers_and_none() {
        assert_eq!(format_audio_receive_summary(0), "Not receiving audio");
        assert_eq!(format_audio_receive_summary(1), "Pulling near-full quality");
        assert_eq!(format_audio_receive_summary(5), "Pulling near-full quality");
    }

    #[test]
    fn audio_send_layer_summary_is_count_aware() {
        use PrefMediaKind::Audio;
        // MIC ON. Audio ladder lowest-first is ["24k","32k","50k"]. Full ladder
        // (Auto / None) → top published layer = "50k".
        assert_eq!(
            format_audio_send_layer_summary(None, 3, true),
            "Sending up to 50k"
        );
        // Ceiling 2 → top published layer = "32k" (the rung strip would show 2 of
        // 3 active); MUST differ from the full-ladder summary so a lowered ceiling
        // visibly changes the line.
        assert_eq!(
            format_audio_send_layer_summary(Some(2), 3, true),
            "Sending up to 32k"
        );
        // Ceiling 1 → only the base (24k) publishes → the "only" phrasing.
        assert_eq!(
            format_audio_send_layer_summary(Some(1), 3, true),
            "Sending 24k only"
        );
        // Sanity: the summary's top label is exactly the top ACTIVE rung label, so
        // the summary can never drift from the rung strip.
        let labels = send_layer_labels(Audio, 3);
        assert_eq!(labels.last().copied(), Some("50k"));
    }

    #[test]
    fn audio_send_layer_summary_is_source_aware_when_mic_off() {
        // MIC OFF: the future "will send … when the mic is on" form (never claims
        // to be sending while the mic is muted). Still count-aware (names the top
        // configured layer), and MUST differ from the mic-on copy at the same
        // ceiling so the state change is visible.
        assert_eq!(
            format_audio_send_layer_summary(None, 3, false),
            "Will send up to 50k when the mic is on"
        );
        assert_ne!(
            format_audio_send_layer_summary(None, 3, false),
            format_audio_send_layer_summary(None, 3, true),
            "mic-off copy must differ from mic-on"
        );
        // Base-only, mic off.
        assert_eq!(
            format_audio_send_layer_summary(Some(1), 3, false),
            "Will send 24k only when the mic is on"
        );
    }

    #[test]
    fn content_send_summary_sharing_and_not() {
        // Not sharing → the spec's "will send up to 1080p" line.
        assert_eq!(
            format_content_send_summary(None),
            "Will send up to 1080p when you share"
        );
        // Sharing with bitrates → "Sending {res} · {mbps}".
        let s = send_snap_3layer();
        assert_eq!(
            format_content_send_summary(Some(&s)),
            "Sending 360p–720p · 2.8 Mbps"
        );
    }

    #[test]
    fn content_receive_summary_top_peer_and_none() {
        assert_eq!(format_content_receive_summary(None), "Nobody is sharing");
        let top = ReceivedLayerSnapshot {
            kind: PrefMediaKind::Screen,
            layer_index: 2,
            layer_count: 3,
            width: 1920,
            height: 1080,
            kbps: 2500,
            reason: None,
        };
        assert_eq!(
            format_content_receive_summary(Some(&top)),
            "Pulling full quality · H · 1920×1080"
        );
        // 2-layer ladder: index 0 is Low → compact "L".
        let low = ReceivedLayerSnapshot {
            layer_index: 0,
            layer_count: 2,
            ..top
        };
        assert_eq!(
            format_content_receive_summary(Some(&low)),
            "Pulling full quality · L · 1920×1080"
        );
    }

    #[test]
    fn consequence_from_peers_singular_plural_zero() {
        assert_eq!(consequence_from_peers(0), "no senders");
        assert_eq!(consequence_from_peers(1), "from 1 peer");
        assert_eq!(consequence_from_peers(4), "from 4 peers");
    }

    #[test]
    fn peers_for_kind_filters_and_flattens() {
        let peers = vec![
            videocall_client::PeerReceiveDiag {
                session_id: 1,
                label: "alice".to_string(),
                video: Some(ReceivedLayerSnapshot {
                    kind: PrefMediaKind::Video,
                    layer_index: 2,
                    layer_count: 3,
                    width: 1280,
                    height: 720,
                    kbps: 1500,
                    reason: None,
                }),
                screen: None,
                audio: Some(ReceivedLayerSnapshot {
                    kind: PrefMediaKind::Audio,
                    layer_index: 0,
                    layer_count: 3,
                    width: 0,
                    height: 0,
                    kbps: 24,
                    reason: None,
                }),
            },
            videocall_client::PeerReceiveDiag {
                session_id: 2,
                label: "bob".to_string(),
                video: Some(ReceivedLayerSnapshot {
                    kind: PrefMediaKind::Video,
                    layer_index: 1,
                    layer_count: 3,
                    width: 960,
                    height: 540,
                    kbps: 900,
                    reason: None,
                }),
                screen: None,
                audio: None,
            },
        ];
        // Video → both peers.
        let v = peers_for_kind(&peers, PrefMediaKind::Video);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].session_id, 1);
        assert_eq!(v[0].label, "alice");
        assert_eq!(v[0].snap.layer_index, 2);
        // Audio → only alice.
        let a = peers_for_kind(&peers, PrefMediaKind::Audio);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].session_id, 1);
        // Screen → none.
        assert!(peers_for_kind(&peers, PrefMediaKind::Screen).is_empty());
    }

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
            ..Default::default()
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
            ..Default::default()
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
    fn at_full_range_drives_reset_visibility_from_positions() {
        // #1131 §D: the Reset button is shown IFF NOT at_full_range — driven by the
        // thumb POSITIONS, not the `auto` flag.
        // Both thumbs at the extremes (0 .. last) → full range → Reset hidden.
        assert!(
            at_full_range(0, 7, 7),
            "both extremes (8-tier send) is full range"
        );
        assert!(
            at_full_range(0, 2, 2),
            "both extremes (3-rung receive) is full range"
        );
        // Any thumb off its extreme → NOT full range → Reset shown. This is the
        // case the refinement requires: a user who DRAGGED to the ends hides it,
        // but a narrowed range (even one end) shows it.
        assert!(
            !at_full_range(0, 5, 7),
            "right thumb moved in → not full range"
        );
        assert!(
            !at_full_range(2, 7, 7),
            "left thumb moved in → not full range"
        );
        assert!(
            !at_full_range(3, 3, 7),
            "both collapsed interior → not full range"
        );
        // Dragging BOTH back to the extremes returns to full range (Reset hides),
        // even though the persisted bounds may still read as manual.
        assert!(
            at_full_range(0, 7, 7),
            "dragged back to both ends → full range again"
        );
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
    fn sanitized_migrates_all_send_tier_bounds_to_auto() {
        // A returning user with manual tier bounds on ALL THREE streams.
        let stale = PerformancePreference {
            video_max: Some(10), // out-of-range stale index
            video_min: Some(3),  // a once-valid manual video bound
            audio_max: Some(9),  // out-of-range stale audio index
            audio_min: Some(2),  // a valid manual audio bound
            screen_max: Some(2), // a once-valid manual screen bound
            screen_min: Some(1),
            video_auto: false,
            audio_auto: false,
            screen_auto: false,
            ..Default::default()
        };
        let clean = stale.sanitized(
            VIDEO_TIER_LABELS.len(),
            AUDIO_TIER_LABELS.len(),
            SCREEN_TIER_LABELS.len(),
        );
        // MIGRATION: ALL THREE streams' SEND tier bounds are CLEARED to None and
        // set to Auto — every SEND tier slider is gone (video, screen, AND audio
        // are now layer-count controls), so a returning user is never silently
        // pinned. Even once-VALID bounds (video_min=Some(3), audio_min=Some(2),
        // the screen bounds) are cleared, not just the out-of-range ones.
        assert_eq!(clean.video_max, None, "video tier bound migrated to None");
        assert_eq!(clean.video_min, None, "video tier bound migrated to None");
        assert!(clean.video_auto, "video migrated to Auto");
        assert_eq!(clean.screen_max, None, "screen tier bound migrated to None");
        assert_eq!(clean.screen_min, None, "screen tier bound migrated to None");
        assert!(clean.screen_auto, "screen migrated to Auto");
        assert_eq!(clean.audio_max, None, "audio tier bound migrated to None");
        assert_eq!(clean.audio_min, None, "audio tier bound migrated to None");
        assert!(clean.audio_auto, "audio migrated to Auto");
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
    fn meter_level_best_is_max_worst_is_one() {
        // SEND convention: tier 0 = BEST → all four bars; worst tier → one bar
        // (a flowing stream is never level 0). The 8-tier ladder's far ends.
        assert_eq!(tier_to_meter_level(0, 8), MAX_METER_LEVEL);
        assert_eq!(tier_to_meter_level(7, 8), 1);
        // A mid tier of a 5-tier ladder (index 2 → quality frac 0.5 → level 2).
        assert_eq!(tier_to_meter_level(2, 5), 2);
    }

    #[test]
    fn meter_level_clamps_out_of_range_and_single_tier() {
        // Out-of-range clamps to the worst tier → one bar; a single tier is the
        // best available → all four bars.
        assert_eq!(tier_to_meter_level(99, 8), 1);
        assert_eq!(tier_to_meter_level(0, 1), MAX_METER_LEVEL);
        assert_eq!(tier_to_meter_level(0, 0), MAX_METER_LEVEL);
    }

    #[test]
    fn level_from_fraction_rounds_and_clamps() {
        assert_eq!(level_from_fraction(0.0), 0);
        assert_eq!(level_from_fraction(1.0), MAX_METER_LEVEL);
        assert_eq!(level_from_fraction(0.5), 2); // round(2.0)
        assert_eq!(level_from_fraction(0.6), 2); // round(2.4)
        assert_eq!(level_from_fraction(0.7), 3); // round(2.8)
                                                 // Out-of-range / NaN clamp into [0, MAX] (never panics or overflows).
        assert_eq!(level_from_fraction(-5.0), 0);
        assert_eq!(level_from_fraction(5.0), MAX_METER_LEVEL);
        assert_eq!(level_from_fraction(f32::NAN), 0);
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
    fn gauge_state_live_snapshot_maps_to_levels_and_readouts() {
        let snap = LiveQualitySnapshot {
            video_tier_index: 0, // best of 8 → level 4
            video_width: 1920,
            video_height: 1080,
            video_fps: 30,
            video_ideal_kbps: 2500,
            audio_tier_index: 3, // worst of 4 → level 1 (floored, never 0)
            audio_kbps: 16,
            target_bitrate_kbps: 2000.0,
        };
        let screen = ScreenQualitySnapshot {
            tier_index: 1, // middle of 3 → quality frac 0.5 → level 2
            width: 1280,
            height: 720,
            fps: 15,
            ideal_kbps: 1200,
            target_bitrate_kbps: 1100,
        };
        let st = gauge_state_from_snapshot(Some(&snap), Some(&screen));
        assert_eq!(st.video_level, MAX_METER_LEVEL);
        assert_eq!(st.audio_level, 1);
        assert_eq!(st.video_text, "1920x1080·30fps·2500kbps");
        assert_eq!(st.audio_text, "16 kbps");
        assert_eq!(st.screen_level, 2);
        assert_eq!(st.screen_text, "1280x720·15fps·1200kbps");
    }

    #[test]
    fn gauge_state_none_resets_to_empty_state() {
        let st = gauge_state_from_snapshot(None, None);
        assert_eq!(st.video_level, EMPTY_METER_LEVEL);
        assert_eq!(st.audio_level, EMPTY_METER_LEVEL);
        assert_eq!(st.screen_level, EMPTY_METER_LEVEL);
        assert_eq!(st.video_level, 0); // no signal → all bars unlit
        assert_eq!(st.video_text, "Camera — off");
        assert_eq!(st.audio_text, "Idle");
        assert_eq!(st.screen_text, "Screen — not sharing");
    }

    #[test]
    fn screen_meter_none_shows_not_sharing_with_live_va() {
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
        assert_eq!(st.screen_level, EMPTY_METER_LEVEL);
        assert_eq!(st.screen_text, "Screen — not sharing");
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
            // Exercise the layer-ceiling fields' serde round-trip too (one set,
            // one Auto/None) so a missing #[serde] attr or field rename is caught.
            video_layers: Some(2),
            screen_layers: None,
            audio_layers: Some(1),
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
        // Migration safety: prefs persisted before the layer-ceiling control
        // existed have no `*_layers` keys and MUST default to None (Auto / full
        // ladder), so an upgrade never silently caps a user's published layers.
        assert_eq!(p.video_layers, None);
        assert_eq!(p.screen_layers, None);
        assert_eq!(p.audio_layers, None);
        assert_eq!(
            preference_to_encoder_bounds(&p),
            EncoderQualityBounds::default()
        );
    }

    // ── SEND layer-count control (video + screen) ──────────────────────

    #[test]
    fn default_ceiling_thumb_is_at_effective_max() {
        // Default (Auto / None) must place the ceiling thumb at the TOP position
        // = effective_max - 1, i.e. the full ladder. This is the "default ceiling
        // == effective max" guarantee. Checked across 1/2/3-layer ladders.
        assert_eq!(
            layer_ceiling_to_thumb_pos(None, 3),
            2,
            "3-layer full → top pos 2"
        );
        assert_eq!(
            layer_ceiling_to_thumb_pos(None, 2),
            1,
            "2-layer full → top pos 1"
        );
        assert_eq!(
            layer_ceiling_to_thumb_pos(None, 1),
            0,
            "1-layer → only pos 0"
        );
    }

    #[test]
    fn ceiling_thumb_maps_to_stored_layer_count() {
        // The ceiling thumb POSITION → stored layer COUNT (the value that flows to
        // pref.video_layers → encoder set_user_layer_ceiling). On a 3-rung ladder:
        //  - top pos (2) = full ladder → None (Auto), so a user at full never pins.
        //  - pos 1 → Some(2) (publish L0+L1 = 2 layers).
        //  - pos 0 → Some(1) (base only).
        assert_eq!(
            thumb_pos_to_layer_ceiling(2, 3),
            None,
            "full ladder stores Auto/None"
        );
        assert_eq!(thumb_pos_to_layer_ceiling(1, 3), Some(2), "mid → 2 layers");
        assert_eq!(
            thumb_pos_to_layer_ceiling(0, 3),
            Some(1),
            "base only → 1 layer"
        );
    }

    #[test]
    fn ceiling_round_trips_count_to_pos_to_count() {
        // Storing a count, deriving the thumb, and reading it back must be stable
        // for every count on a 3-layer ladder (the drag→store→re-render cycle).
        for layer_max in 1..=3usize {
            for count in 1..=layer_max as u32 {
                let stored = if count as usize == layer_max {
                    None // full normalizes to Auto
                } else {
                    Some(count)
                };
                let pos = layer_ceiling_to_thumb_pos(stored, layer_max);
                assert_eq!(
                    thumb_pos_to_layer_ceiling(pos, layer_max),
                    stored,
                    "count {count} of {layer_max} must round-trip"
                );
            }
        }
    }

    #[test]
    fn with_video_layers_sets_count_and_frees_tier_bounds() {
        // The SEND layer control owns the send axis: setting a layer count must
        // ALSO force the video tier bounds to Auto so the AQ adapts bitrate freely
        // (no competing tier slider). MUTATION CHECK: if with_video_layers stops
        // clearing the tier bounds (drop the video_max/min/auto resets), the
        // tier-bound assertions below fail.
        let manual = PerformancePreference {
            video_max: Some(2),
            video_min: Some(5),
            video_auto: false,
            ..Default::default()
        };
        let p = manual.with_video_layers(Some(2));
        assert_eq!(p.video_layers, Some(2), "layer count stored");
        assert!(p.video_auto, "tier axis freed to Auto");
        assert_eq!(p.video_max, None, "tier max cleared");
        assert_eq!(p.video_min, None, "tier min cleared");
    }

    #[test]
    fn with_audio_layers_sets_count_and_frees_tier_bounds() {
        // Audio's SEND layer control owns the audio send axis (mirror of video):
        // setting a layer count must ALSO force the audio tier bounds to Auto so
        // the AQ adapts the published layers' bitrate freely. MUTATION CHECK: if
        // with_audio_layers stops clearing the audio tier bounds, the tier-bound
        // assertions below fail.
        let manual = PerformancePreference {
            audio_max: Some(1),
            audio_min: Some(3),
            audio_auto: false,
            ..Default::default()
        };
        let p = manual.with_audio_layers(Some(2));
        assert_eq!(p.audio_layers, Some(2), "audio layer count stored");
        assert!(p.audio_auto, "audio tier axis freed to Auto");
        assert_eq!(p.audio_max, None, "audio tier max cleared");
        assert_eq!(p.audio_min, None, "audio tier min cleared");
    }

    #[test]
    fn with_screen_layers_reset_to_none_is_full_ladder() {
        // Reset → with_screen_layers(None) clears the cap to the full ladder.
        let capped = PerformancePreference::default().with_screen_layers(Some(1));
        assert_eq!(capped.screen_layers, Some(1));
        let reset = capped.with_screen_layers(None);
        assert_eq!(
            reset.screen_layers, None,
            "reset clears to full ladder (None)"
        );
    }

    #[test]
    fn tick_offsets_one_per_step_aligned_to_stops() {
        // A 3-step slider → 3 ticks at 0% / 50% / 100% (aligned to where a
        // <input type=range max=2> thumb snaps: 0/2, 1/2, 2/2).
        let three = tick_offsets(3);
        assert_eq!(three.len(), 3, "one tick per step position");
        assert_eq!(three[0], 0.0);
        assert_eq!(three[1], 50.0);
        assert_eq!(three[2], 100.0);
        // A 4-step receive slider → 4 ticks at 0/33.3/66.6/100.
        assert_eq!(tick_offsets(4).len(), 4);
        // Endpoints are always 0 and 100 for >1 step.
        let four = tick_offsets(4);
        assert_eq!(four[0], 0.0);
        assert_eq!(*four.last().unwrap(), 100.0);
        // Single-step (or zero) → one tick at the lone stop, never empty/NaN.
        assert_eq!(tick_offsets(1), vec![0.0]);
        assert_eq!(tick_offsets(0), vec![0.0]);
    }

    #[test]
    fn send_layer_caption_is_source_aware() {
        use PrefMediaKind::{Audio, Screen, Video};
        // SOURCE ON: present-tense "Sending N of M layers".
        assert_eq!(
            format_send_layer_caption(Video, 2, 3, true),
            "Sending 2 of 3 layers"
        );
        // Single-layer ladder, on → "Sending 1 layer".
        assert_eq!(
            format_send_layer_caption(Video, 1, 1, true),
            "Sending 1 layer"
        );
        // SOURCE OFF: future form using the CONFIGURED count + per-kind phrase;
        // never claims to be "sending". Each kind names its own trigger.
        assert_eq!(
            format_send_layer_caption(Video, 2, 3, false),
            "Will send 2 layers when the camera is on"
        );
        assert_eq!(
            format_send_layer_caption(Screen, 3, 3, false),
            "Will send 3 layers when sharing"
        );
        assert_eq!(
            format_send_layer_caption(Audio, 1, 3, false),
            "Will send 1 layer when the mic is on"
        );
        // The off-state copy MUST differ from the on-state copy AND name the
        // count (the core requirement: don't claim "sending" when the source is
        // off, but still convey N).
        let on = format_send_layer_caption(Video, 2, 3, true);
        let off = format_send_layer_caption(Video, 2, 3, false);
        assert_ne!(on, off, "off-state copy must differ from on-state");
        assert!(
            off.contains('2'),
            "off-state copy must name the configured count"
        );
        assert!(
            !off.contains("Sending"),
            "off-state must NOT claim to be sending"
        );
    }

    #[test]
    fn send_layer_labels_match_per_kind_ladders() {
        use PrefMediaKind::{Audio, Screen, Video};
        // VIDEO ladder (spaced): 1→[360p], 2→[360p,720p] (skip 540p), 3→full.
        assert_eq!(send_layer_labels(Video, 1), vec!["360p"]);
        assert_eq!(send_layer_labels(Video, 2), vec!["360p", "720p"]);
        assert_eq!(send_layer_labels(Video, 3), vec!["360p", "540p", "720p"]);
        // SCREEN ladder differs: qualitative labels (low/medium are both 720p so
        // resolution would be ambiguous). 1→[low], 2→[low, high] (skip medium),
        // 3→[low, medium, high]. The base differs from video (proves per-kind
        // routing, not a shared prefix).
        assert_eq!(send_layer_labels(Screen, 1), vec!["low"]);
        assert_eq!(send_layer_labels(Screen, 2), vec!["low", "high"]);
        assert_eq!(send_layer_labels(Screen, 3), vec!["low", "medium", "high"]);
        // AUDIO ladder is CONTIGUOUS (no skip — AUDIO_LAYER_KBPS = [24,32,50]):
        // 1→[24k], 2→[24k,32k], 3→[24k,32k,50k]. n=2 keeps the MIDDLE rung (32k),
        // unlike video/screen which skip it — proves the audio arm isn't a copy of
        // the spaced ladder.
        assert_eq!(send_layer_labels(Audio, 1), vec!["24k"]);
        assert_eq!(send_layer_labels(Audio, 2), vec!["24k", "32k"]);
        assert_eq!(send_layer_labels(Audio, 3), vec!["24k", "32k", "50k"]);
    }

    #[test]
    fn layer_send_rungs_active_up_to_ceiling_base_always_active() {
        // The selection-driven strip: pips L0..=ceiling are active, the rest shed,
        // and L0 (base) is ALWAYS active (the pinned-floor invariant). On a 3-rung
        // video ladder with ceiling at pos 1: [L0 active, L1 active, L2 shed].
        let labels = send_layer_labels(PrefMediaKind::Video, 3);
        let rungs = layer_send_rungs(&labels, 1);
        assert_eq!(rungs.len(), 3);
        assert!(rungs[0].active, "base layer always active");
        assert!(rungs[1].active, "up to the ceiling is active");
        assert!(!rungs[2].active, "above the ceiling is shed");
        // Ceiling at the base (pos 0): only L0 active.
        let base_only = layer_send_rungs(&labels, 0);
        assert!(base_only[0].active);
        assert!(!base_only[1].active);
        assert!(!base_only[2].active);
    }

    // ── issue #1131: per-peer receive row + send rung helpers ──────────

    fn snap(
        kind: PrefMediaKind,
        layer_index: u32,
        width: u32,
        height: u32,
        kbps: u32,
        reason: Option<DegradeReason>,
    ) -> ReceivedLayerSnapshot {
        ReceivedLayerSnapshot {
            kind,
            layer_index,
            layer_count: layer_index + 1,
            width,
            height,
            kbps,
            reason,
        }
    }

    #[test]
    fn quality_state_helpers_map_each_state() {
        // Distinct modifier/glyph/word per state — a swap would break a row's
        // class, its non-color cue, or its spoken quality word.
        assert_eq!(quality_state_modifier(QualityState::Optimal), "optimal");
        assert_eq!(quality_state_modifier(QualityState::Medium), "medium");
        assert_eq!(quality_state_modifier(QualityState::Low), "low");
        assert_eq!(quality_state_glyph(QualityState::Optimal), "●");
        assert_eq!(quality_state_glyph(QualityState::Medium), "◐");
        assert_eq!(quality_state_glyph(QualityState::Low), "○");
        assert_eq!(quality_state_word(QualityState::Low), "low");
    }

    #[test]
    fn reason_chip_copy_per_reason() {
        // Each reason has its own modifier/text/title/aria — these are the §5
        // user-facing strings and must not collapse together.
        assert_eq!(reason_chip_modifier(DegradeReason::Network), "network");
        assert_eq!(reason_chip_modifier(DegradeReason::Setting), "setting");
        assert_eq!(reason_chip_modifier(DegradeReason::Sender), "sender");
        assert_eq!(reason_chip_text(DegradeReason::Network), "Your network");
        assert_eq!(reason_chip_text(DegradeReason::Setting), "Your setting");
        assert_eq!(reason_chip_text(DegradeReason::Sender), "Sender");
        assert!(reason_chip_title(DegradeReason::Network).contains("download"));
        assert!(reason_chip_title(DegradeReason::Setting).contains("capped"));
        assert!(reason_chip_title(DegradeReason::Sender).contains("publishing"));
        assert_eq!(
            reason_aria_clause(DegradeReason::Network),
            "limited by your network"
        );
        assert_eq!(
            reason_aria_clause(DegradeReason::Sender),
            "limited by the sender"
        );
    }

    #[test]
    fn peer_row_metric_video_and_audio_shapes() {
        // Video/screen: "{res} · ~{kbps} · {Q} · {i}/{n}" (Directive 4 SITE 6b).
        // n is the FULL ladder length passed in (3); index 1 of 3 → letter "M",
        // 1-based position 2. (peer_row_metric is the real e2e-string site; the
        // rename propagates to signal_quality.rs too — #1222.) Mutating the
        // `layer_quality_label` letter or the i/n arithmetic breaks this literal.
        let v = snap(PrefMediaKind::Video, 1, 960, 540, 600, None);
        assert_eq!(peer_row_metric(&v, 3, "ignored"), "540p · ~600k · M · 2/3");
        // Audio: "{kbps}k · {label} · {Q} · {i}/{n}".
        let a = snap(PrefMediaKind::Audio, 1, 0, 0, 32, None);
        assert_eq!(
            peer_row_metric(&a, 3, "mid (32k)"),
            "32k · mid (32k) · M · 2/3"
        );
    }

    #[test]
    fn peer_row_aria_label_with_and_without_reason() {
        // No reason (optimal) → no trailing clause.
        let optimal = peer_row_aria_label(
            "Ana Ruiz",
            "video",
            QualityState::Optimal,
            "720p",
            3,
            3,
            None,
        );
        assert_eq!(
            optimal,
            "Ana Ruiz, receiving video, optimal quality, 720p, layer 3 of 3"
        );
        // With a reason → appends the §5 clause.
        let limited = peer_row_aria_label(
            "Ana Ruiz",
            "video",
            QualityState::Low,
            "360p",
            1,
            3,
            Some(DegradeReason::Network),
        );
        assert_eq!(
            limited,
            "Ana Ruiz, receiving video, low quality, 360p, layer 1 of 3, limited by your network"
        );
    }

    #[test]
    fn audio_send_rung_uses_best_cap_label() {
        // Auto / no cap → best tier (index 0) label, single active pip, no kbps.
        let auto = audio_send_rung(None);
        assert!(auto.active);
        assert_eq!(auto.kbps_label, None);
        assert_eq!(auto.res_label, AUDIO_TIER_LABELS[0]);
        // A manual cap at tier 2 → that tier's label.
        let capped = audio_send_rung(Some(2));
        assert_eq!(capped.res_label, AUDIO_TIER_LABELS[2]);
        // Out-of-range index falls back to best tier (panic-safe).
        let oob = audio_send_rung(Some(999));
        assert_eq!(oob.res_label, AUDIO_TIER_LABELS[0]);
    }
}
