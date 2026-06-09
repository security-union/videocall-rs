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

/// Format one SEND simulcast layer line, e.g. `"L0 640×360 · 400 kbps"`. Pure.
pub fn format_send_layer(layer_id: u32, width: u32, height: u32, bitrate_kbps: u32) -> String {
    format!("L{layer_id} {width}×{height} · {bitrate_kbps} kbps")
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

/// Format one RECEIVE per-kind line for a peer, e.g. `"video L2/3 · 1280×720"`
/// or `"audio L1/3 · 24 kbps"`. Returns `None` when the kind is not flowing.
/// Pure / host-tested.
pub fn format_peer_kind_line(
    kind_label: &str,
    snap: Option<&ReceivedLayerSnapshot>,
) -> Option<String> {
    let s = snap?;
    let layer = s.layer_index + 1;
    let detail = if matches!(s.kind, PrefMediaKind::Audio) {
        format!("{} kbps", s.kbps)
    } else {
        format!("{}×{}", s.width, s.height)
    };
    Some(format!(
        "{kind_label} L{layer}/{} · {detail}",
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

/// Format the RECEIVE per-kind layer spread across peers, e.g. `"L0–L2"`, or
/// `"L3"` when every peer is on the same layer. `layers` is the list of
/// `layer_index` values (1-indexed for display via `+1`) for the peers receiving
/// this kind. Empty → empty string (caller renders the "not receiving" state).
/// Pure / host-tested.
pub fn format_receive_spread(layers: &[u32]) -> String {
    let Some(&first) = layers.first() else {
        return String::new();
    };
    let lo = layers.iter().copied().min().unwrap_or(first) + 1;
    let hi = layers.iter().copied().max().unwrap_or(first) + 1;
    if lo == hi {
        format!("L{lo}")
    } else {
        format!("L{lo}–L{hi}")
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

/// Build the §2 SEND rung strip from a video/screen SEND snapshot (issue #1131),
/// lowest layer first. Pure / host-tested.
///
///   * Simulcast active → one pip per EFFECTIVE layer; `active` is
///     `layer_id < active_layers` (the AQ controller sheds the TOP layers under
///     congestion, so shed pips are the high ones). The kbps label lands on the
///     highest active pip only.
///   * Single-stream (non-simulcast) video/screen → the producers
///     (`CameraEncoder`/`ScreenEncoder::live_simulcast_snapshot`) emit an EMPTY
///     `layers` Vec, so this returns an empty Vec and the caller renders NO strip
///     and keeps the plain summary line instead. (There is intentionally no
///     single-pip fallback for these kinds — single-stream carries no per-layer
///     ladder to draw.)
///
/// AUDIO has no per-layer encoder snapshot at all and never reaches this fn; the
/// panel builds its single send pip from the user's cap via [`audio_send_rung`].
///
/// Returns an EMPTY Vec when there are no layers (single-stream / atomics not yet
/// ticked), so the caller falls back to the summary line and renders no empty
/// strip.
pub fn send_rungs(snap: &SimulcastSendSnapshot) -> Vec<SendRung> {
    if snap.layers.is_empty() {
        return Vec::new();
    }
    // The highest ACTIVE layer id (for the single kbps label). active_layers is a
    // count of the lowest-N active layers, so the top active id is count-1.
    let top_active_id = snap.active_layers.saturating_sub(1);
    snap.layers
        .iter()
        .map(|l| {
            let active = l.layer_id < snap.active_layers;
            SendRung {
                layer_id: l.layer_id,
                active,
                res_label: format_send_layer_short(l.width, l.height),
                // kbps only on the top ACTIVE pip, and only once a bitrate exists.
                kbps_label: (active && l.layer_id == top_active_id && l.bitrate_kbps > 0)
                    .then(|| format_kbps_compact(l.bitrate_kbps)),
            }
        })
        .collect()
}

/// The §2 SEND rung strip for AUDIO, which has no per-layer encoder snapshot:
/// render a SINGLE filled pip at the user's best-allowed send tier, labeled from
/// [`AUDIO_TIER_LABELS`] (send-side inverted index: 0 = best). `best` is the
/// best-allowed tier index (`None`/Auto → tier 0 = best). Pure / host-tested.
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

/// VIDEO RECEIVE summary, e.g. `"Pulling up to high quality · L1–L3 across 4
/// peers"`. No peers → `"Not receiving video"`. `layers` is the per-peer
/// `layer_index` list (1-indexed for display via [`format_receive_spread`]).
/// Pure / host-tested.
pub fn format_video_receive_summary(layers: &[u32]) -> String {
    let n = layers.len();
    if n == 0 {
        return "Not receiving video".to_string();
    }
    let spread = format_receive_spread(layers);
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

/// AUDIO SEND summary, derived from the user's bounds (the audio row has no
/// per-layer encoder snapshot, so unlike video/content this reads the *intent*,
/// not the live wire). On Auto or full-range → `"Sending high quality"`; with a
/// manual cap → `"Sending up to {best-tier kbps}"` so a lowered cap is reflected
/// rather than claiming "high quality" falsely (#5). `best` is the best-allowed
/// SEND tier index (0 = best); `None` = no cap. Pure / host-tested.
pub fn format_audio_send_summary(is_auto: bool, best: Option<usize>) -> String {
    // Auto, or no upper cap, or the cap is already the best tier (index 0) →
    // we're sending the full-quality top tier.
    match best {
        _ if is_auto => "Sending high quality".to_string(),
        None | Some(0) => "Sending high quality".to_string(),
        Some(idx) => {
            let label = AUDIO_TIER_LABELS.get(idx).copied().unwrap_or("?");
            format!("Sending up to {label}")
        }
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
/// otherwise `"Pulling full quality · L{i} · {w}×{h}"` for the top-layer peer.
/// `top` is the highest-layer peer snapshot currently received. Pure.
pub fn format_content_receive_summary(top: Option<&ReceivedLayerSnapshot>) -> String {
    match top {
        None => "Nobody is sharing".to_string(),
        Some(s) => format!(
            "Pulling full quality · L{} · {}×{}",
            s.layer_index + 1,
            s.width,
            s.height
        ),
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

/// The per-peer row metric text (§3). video/screen `"{res} · ~{kbps} · L{i}/{n}"`;
/// audio `"{kbps}k · {label} · L{i}/{n}"`. `n` is the FULL-ladder length (so the
/// "L i / n" denominator matches the color basis). `audio_label` is the receive
/// audio rung label (e.g. "mid (32k)") supplied by the caller (the receive
/// submodule owns that mapping). Pure / host-tested.
pub fn peer_row_metric(
    snap: &ReceivedLayerSnapshot,
    full_ladder_len: u32,
    audio_label: &str,
) -> String {
    let i = snap.layer_index + 1;
    if matches!(snap.kind, PrefMediaKind::Audio) {
        format!("{}k · {} · L{i}/{full_ladder_len}", snap.kbps, audio_label)
    } else {
        let res = format_send_layer_short(snap.width, snap.height);
        format!(
            "{res} · ~{} · L{i}/{full_ladder_len}",
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

/// Whether a dual-thumb selection sits at BOTH extremes (`min` at position 0 and
/// `max` at `last_position`) — i.e. the full default range, nothing constrained
/// (issue #1131 §D). Drives the Reset button's visibility: it is shown IFF this
/// is `false`, so dragging both thumbs back to the ends hides it live even when
/// the persisted `auto` flag is still false. Pure / host-tested; usize-based so
/// both the send (`usize`) and receive (`u32`, cast) sliders share it.
pub fn at_full_range(min_pos: usize, max_pos: usize, last_position: usize) -> bool {
    min_pos == 0 && max_pos == last_position
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
                    "aria-label": "Worst {stream_noun} send quality",
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
                    "aria-label": "Best {stream_noun} send quality",
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

/// One kind's SEND column inside a [`KindCard`]: a "Sending" head (consequence +
/// "?" help + Auto/Fixed), a bar-meter, a dual-thumb slider, and a live summary
/// line (#1095 redesign — replaces the old `SendRow` + diagnostics footer).
///
/// Send uses the inverted tier convention (0 = best) and bounds what this peer
/// publishes; the counterpart [`receive::ReceiveCell`] uses the direct layer
/// convention (0 = lowest). Distinct testids / meter ids keep them from crossing.
#[allow(clippy::too_many_arguments)]
#[component]
fn SendCell(
    /// Accessible noun for the kind, e.g. "video" / "audio" / "screen share".
    stream_noun: &'static str,
    /// Send id prefix, e.g. "perf-video".
    id_prefix: &'static str,
    min_testid: &'static str,
    max_testid: &'static str,
    auto_testid: &'static str,
    fixed_testid: &'static str,
    help_testid: &'static str,
    help_body: &'static str,
    vu_testid: &'static str,
    vu_meter_id: &'static str,
    vu_readout_id: &'static str,
    vu_label: &'static str,
    vu_initial_level: u8,
    vu_initial_readout: String,
    /// The "your upload" / "not sharing" consequence text right of the side title.
    consequence: String,
    /// The always-visible summary line under the slider (filled live by the
    /// parent from the SEND snapshot). When a §2 rung strip is present it
    /// SUPERSEDES this line (the strip carries the live per-layer state); the
    /// summary is only shown when there is no strip (camera off / atomics not
    /// ticked / not sharing). Folding the strip over the summary protects the
    /// no-scroll budget (§7) — this applies to ALL kinds, including the single
    /// audio pip (whose label already states the tier the summary would repeat).
    summary_line: String,
    /// The §2 SEND rung strip (issue #1131), lowest layer first. Empty → render
    /// no strip and keep the summary line (e.g. camera off / atomics not ticked).
    #[props(default)]
    rungs: Vec<SendRung>,
    /// `role="img"` aria-label for the rung strip (e.g. "Sending 2 of 3 layers").
    #[props(default)]
    rungs_aria: String,
    labels: Vec<&'static str>,
    best: Option<usize>,
    worst: Option<usize>,
    is_fixed: bool,
    /// Shared single-open help signal (opening any popover closes the others).
    open_help: Signal<Option<&'static str>>,
    on_change: EventHandler<RangeSel>,
    /// Clears the stream back to the full automatic range (Reset). Named
    /// `on_auto_toggle` for continuity with the prior Auto control; always called
    /// with `true` now (full range).
    on_auto_toggle: EventHandler<bool>,
) -> Element {
    let sel = bounds_to_thumbs(best, worst, labels.len());
    let range_str = span_text(sel, &labels);
    // Reset is shown IFF the thumbs are NOT at both extremes (#1131 §D). Driven by
    // POSITIONS (not the `auto` flag) so dragging both thumbs back to the ends
    // hides it live, and it reacts on every drag.
    let show_reset = !at_full_range(sel.min_pos, sel.max_pos, labels.len().saturating_sub(1));

    rsx! {
        div { class: "perf-side perf-side--send",
            // Head row: the bar-meter sits INLINE here (not on its own line) to
            // keep the per-side height down for the no-scroll budget (#2d).
            div { class: "perf-side__head",
                span { class: "perf-side__title",
                    // §1 directional arrow (arrow-up-right, green --success),
                    // aria-hidden — the "Sending" text remains the a11y label.
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
                if is_fixed {
                    span {
                        class: "perf-fixed-badge",
                        "data-testid": fixed_testid,
                        title: "Send quality is pinned to a single tier — this stream won't adapt",
                        "aria-label": "{stream_noun} send quality pinned to a single tier — adaptation disabled",
                        "Fixed"
                    }
                }
                // Reset clears the two handles back to the full automatic range
                // (auto = true → bounds cleared, thumbs snap to the extremes, which
                // re-hides this button). Rendered ONLY when the thumbs are off the
                // extremes (`show_reset`); at the full default range the slot is
                // EMPTY so the head reads clean (#1131 §D). Repurposes the former
                // Auto testid so the testid surface is unchanged.
                if show_reset {
                    button {
                        r#type: "button",
                        class: "perf-reset-button",
                        "data-testid": auto_testid,
                        "aria-label": "Reset {stream_noun} quality limits",
                        title: "Clear both limits — back to the full automatic range",
                        onclick: move |_| on_auto_toggle.call(true),
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
                on_change: move |s: RangeSel| on_change.call(s),
            }
            // §2 SEND rung strip (always-visible). When present it carries the
            // live per-layer state; the summary line is then folded into the strip
            // caption to protect the no-scroll budget (kept separate only when
            // there is no strip — e.g. camera off / atomics not ticked).
            if !rungs.is_empty() {
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
                                format!("Layer {} — sending {}", rung.layer_id + 1, rung.res_label)
                            } else {
                                format!("Layer {} — shed (not sending under current conditions)", rung.layer_id + 1)
                            },
                            span { class: "perf-rung__bar", "aria-hidden": "true" }
                            span { class: "perf-rung__label",
                                "{rung.res_label}"
                                if let Some(k) = rung.kbps_label.as_ref() {
                                    " · {k}"
                                }
                            }
                        }
                    }
                }
            }
            // The slider range readout + (for the no-strip case) the live summary
            // share ONE flex line to save vertical space (#2e). The range-value
            // keeps its own testid and stable `span_text` content (no aria-live —
            // #4: a native range input already announces on change).
            div { class: "perf-side__caption",
                p {
                    class: "perf-range-value",
                    "data-testid": "{id_prefix}-range-value",
                    "Sending: {range_str}"
                }
                if rungs.is_empty() {
                    p { class: "perf-summary-line", "{summary_line}" }
                }
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

const HELP_VIDEO_SEND: &str = "Your camera sends several quality versions ('layers') so each viewer gets the best one their connection can handle. The left handle sets the lowest version you'll send (floor), the right handle the highest (ceiling); it adapts within that band. More layers = more upload. Reset returns to the full automatic range.";
const HELP_AUDIO_SEND: &str = "Your mic sends one or more audio quality versions. The left handle sets the lowest you'll send (floor), the right handle the highest (ceiling). Higher = clearer voice but more upload. Reset returns to the full automatic range.";
const HELP_CONTENT_SEND: &str = "When you share your screen, the left handle sets the lowest sharpness you publish (floor) and the right handle the highest (ceiling); it adapts within that band. Text-heavy screens benefit from a higher ceiling; video benefits from a lower one if your upload is tight. Reset returns to the full automatic range.";

/// The unified Performance settings panel body (#1095 redesign). Three stacked
/// per-kind cards (Video / Audio / Content), each split into a **Sending** column
/// and a **Receiving** column, so both directions are visible at once without a
/// direction tab. A header row carries the "Performance" title and a "Diagnostics"
/// cross-nav button; a slim simulcast strip (with an `(i)` tooltip) sits under the
/// intro. Two headless rAF drivers update the Sending and Receiving bar-meters
/// independently by id.
///
/// `pref` (send) + `receive_pref` are the current persisted preferences
/// (controlled by the parent). On any change the panel derives the new bounds
/// and calls the matching callback; the parent persists it and pushes it to the
/// encoder (send) or client (receive). The panel is otherwise stateless apart
/// from the open-popover signal and the throttled refresh tick.
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
    // Live simulcast/AQ diagnostics (#1095 observability). Defaults to an inert
    // reader so existing call sites / tests that don't wire it still compile.
    #[props(default = DiagnosticsReader::none())] diagnostics_reader: DiagnosticsReader,
    // Cross-nav: open the Call Diagnostics panel (closes settings). Defaults to a
    // no-op so call sites / tests that don't wire it still compile. (#1095 §4a)
    #[props(default)] on_open_diagnostics: EventHandler<()>,
) -> Element {
    let video_fixed = pref.video_is_fixed();
    let audio_fixed = pref.audio_is_fixed();
    let screen_fixed = pref.screen_is_fixed();

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
    // gated to the panel mount (the panel only mounts on the open Performance
    // tab), and the summaries are cheap (count / min-max).
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
    );
    // Audio has no per-layer snapshot; derive its send summary from the user's
    // bounds so a lowered cap is reflected rather than always claiming "high
    // quality" (#5).
    let audio_send_line = format_audio_send_summary(pref.audio_auto, pref.audio_max);
    let audio_recv_line = format_audio_receive_summary(recv_audio_peers.len());
    let content_send_line = format_content_send_summary(send_screen_snap.as_ref());
    // Top-layer screen-share peer (highest layer_index) for the content receive line.
    let content_top = recv_screen_peers
        .iter()
        .max_by_key(|p| p.snap.layer_index)
        .map(|p| p.snap);
    let content_recv_line = format_content_receive_summary(content_top.as_ref());

    // §2 SEND rung strips (issue #1131). Video/content build per-layer pips from
    // the live encoder snapshot (empty Vec → no strip, summary line shown);
    // audio has no per-layer snapshot, so a single pip at the user's best cap.
    let video_send_rungs = send_video_snap.as_ref().map(send_rungs).unwrap_or_default();
    let video_send_rungs_aria = send_rungs_aria(&video_send_rungs);
    let content_send_rungs = send_screen_snap
        .as_ref()
        .map(send_rungs)
        .unwrap_or_default();
    let content_send_rungs_aria = send_rungs_aria(&content_send_rungs);
    let audio_send_rungs = vec![audio_send_rung(pref.audio_max)];
    let audio_send_rungs_aria = send_rungs_aria(&audio_send_rungs);

    // Receive-side consequence strings (peer counts; "not sharing" for content).
    let video_recv_consequence = consequence_from_peers(recv_video_peers.len());
    let audio_recv_consequence = consequence_from_peers(recv_audio_peers.len());
    let content_recv_consequence = if recv_screen_peers.is_empty() {
        "not sharing".to_string()
    } else {
        consequence_from_peers(recv_screen_peers.len())
    };

    rsx! {
        // Header row: title left, Diagnostics cross-nav button right (§4a).
        div { class: "perf-header-row",
            h3 { class: "settings-section-title", "Performance" }
            button {
                r#type: "button",
                class: "perf-nav-button",
                "data-testid": "perf-open-diagnostics",
                title: "Open Call Diagnostics (live per-peer and per-layer detail)",
                "aria-label": "Open Call Diagnostics",
                onclick: move |_| on_open_diagnostics.call(()),
                // Lucide panel-right-open (NOT a gear) — conveys "open the side panel".
                svg {
                    class: "perf-nav-button__icon",
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "18", height: "18", view_box: "0 0 24 24",
                    fill: "none", stroke: "currentColor", stroke_width: "2",
                    stroke_linecap: "round", stroke_linejoin: "round",
                    "aria-hidden": "true",
                    rect { x: "3", y: "3", width: "18", height: "18", rx: "2" }
                    path { d: "M15 3v18" }
                    path { d: "m10 15-3-3 3-3" }
                }
                span { class: "perf-nav-button__label", "Diagnostics" }
            }
        }
        p { class: "settings-section-description",
            "Each stream adapts to your connection automatically. Limit what you "
            span { class: "perf-emph-recv", "receive" }
            " (saves your download) and what you "
            span { class: "perf-emph-send", "send" }
            " (saves your upload + CPU) by dragging the two handles: the "
            "left handle is the lowest quality the stream may use (floor), the "
            "right handle the highest (ceiling), and it adapts within that band. "
            "Reset clears both handles back to the full range. The meter shows "
            "what's flowing right now."
        }

        // Global effective-setting strip with an (i) tooltip. Compact copy; full
        // flag×cap text in the (i) title/aria.
        div {
            class: "perf-simulcast-strip",
            "data-testid": TESTID_SIMULCAST_STRIP,
            span { class: "perf-simulcast-strip__text", "{strip_compact}" }
            span {
                class: "perf-simulcast-strip__info",
                role: "img",
                tabindex: "0",
                title: "Simulcast publishes multiple quality layers so viewers self-select. {strip_full}. Audio has its own ladder.",
                "aria-label": "Simulcast publishes multiple quality layers so viewers self-select. {strip_full}. Audio has its own ladder.",
                "ⓘ"
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
                SendCell {
                    stream_noun: "video",
                    id_prefix: "perf-video",
                    min_testid: TESTID_VIDEO_RANGE_MIN,
                    max_testid: TESTID_VIDEO_RANGE_MAX,
                    auto_testid: TESTID_VIDEO_AUTO,
                    fixed_testid: "perf-video-fixed-badge",
                    help_testid: "perf-video-help",
                    help_body: HELP_VIDEO_SEND,
                    vu_testid: TESTID_VU_VIDEO,
                    vu_meter_id: VIDEO_METER_ID,
                    vu_readout_id: VIDEO_READOUT_ID,
                    vu_label: "Sending video",
                    vu_initial_level: g.video_level,
                    vu_initial_readout: g.video_text.clone(),
                    consequence: "your upload".to_string(),
                    summary_line: video_send_line,
                    rungs: video_send_rungs,
                    rungs_aria: video_send_rungs_aria,
                    labels: VIDEO_TIER_LABELS.to_vec(),
                    best: pref.video_max,
                    worst: pref.video_min,
                    is_fixed: video_fixed,
                    open_help,
                    on_change: move |sel: RangeSel| on_change.call(pref.with_video_thumbs(sel)),
                    on_auto_toggle: move |on: bool| on_change.call(pref.set_video_auto(on)),
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
                SendCell {
                    stream_noun: "audio",
                    id_prefix: "perf-audio",
                    min_testid: TESTID_AUDIO_RANGE_MIN,
                    max_testid: TESTID_AUDIO_RANGE_MAX,
                    auto_testid: TESTID_AUDIO_AUTO,
                    fixed_testid: "perf-audio-fixed-badge",
                    help_testid: "perf-audio-help",
                    help_body: HELP_AUDIO_SEND,
                    vu_testid: TESTID_VU_AUDIO,
                    vu_meter_id: AUDIO_METER_ID,
                    vu_readout_id: AUDIO_READOUT_ID,
                    vu_label: "Sending audio",
                    vu_initial_level: g.audio_level,
                    vu_initial_readout: g.audio_text.clone(),
                    consequence: "your upload".to_string(),
                    summary_line: audio_send_line,
                    rungs: audio_send_rungs,
                    rungs_aria: audio_send_rungs_aria,
                    labels: AUDIO_TIER_LABELS.to_vec(),
                    best: pref.audio_max,
                    worst: pref.audio_min,
                    is_fixed: audio_fixed,
                    open_help,
                    on_change: move |sel: RangeSel| on_change.call(pref.with_audio_thumbs(sel)),
                    on_auto_toggle: move |on: bool| on_change.call(pref.set_audio_auto(on)),
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
                SendCell {
                    stream_noun: "screen share",
                    id_prefix: "perf-screen",
                    min_testid: TESTID_SCREEN_RANGE_MIN,
                    max_testid: TESTID_SCREEN_RANGE_MAX,
                    auto_testid: TESTID_SCREEN_AUTO,
                    fixed_testid: "perf-screen-fixed-badge",
                    help_testid: "perf-screen-help",
                    help_body: HELP_CONTENT_SEND,
                    vu_testid: TESTID_VU_SCREEN,
                    vu_meter_id: SCREEN_METER_ID,
                    vu_readout_id: SCREEN_READOUT_ID,
                    vu_label: "Sending screen",
                    vu_initial_level: g.screen_level,
                    vu_initial_readout: g.screen_text.clone(),
                    consequence: if send_screen_snap.is_some() { "your upload".to_string() } else { "not sharing".to_string() },
                    summary_line: content_send_line,
                    rungs: content_send_rungs,
                    rungs_aria: content_send_rungs_aria,
                    labels: SCREEN_TIER_LABELS.to_vec(),
                    best: pref.screen_max,
                    worst: pref.screen_min,
                    is_fixed: screen_fixed,
                    open_help,
                    on_change: move |sel: RangeSel| on_change.call(pref.with_screen_thumbs(sel)),
                    on_auto_toggle: move |on: bool| on_change.call(pref.set_screen_auto(on)),
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
        format_receive_spread, level_from_fraction, peer_row_aria_label, peer_row_metric,
        quality_state_glyph, quality_state_modifier, reason_chip_modifier, reason_chip_text,
        reason_chip_title, write_meter_level, write_readout_text, PeerKindSnap,
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
        let spread =
            format_receive_spread(&peers.iter().map(|p| p.snap.layer_index).collect::<Vec<_>>());
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
            assert_eq!(format_readout(&v), "L2/3 · 960x540");
            let a = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Audio,
                layer_index: 0,
                layer_count: 3,
                width: 0,
                height: 0,
                kbps: 24,
                reason: None,
            };
            assert_eq!(format_readout(&a), "L1/3 · 24 kbps");
            let s = ReceivedLayerSnapshot {
                kind: PrefMediaKind::Screen,
                layer_index: 2,
                layer_count: 3,
                width: 1920,
                height: 1080,
                kbps: 2500,
                reason: None,
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
                reason: None,
            };
            let st = gauge_state(Some(&snap));
            // top layer → all bars
            assert_eq!(
                st.level,
                crate::components::performance_settings::MAX_METER_LEVEL
            );
            assert_eq!(st.text, "L3/3 · 1280x720");
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

    #[test]
    fn send_layer_and_header_formatting() {
        assert_eq!(format_send_layer(0, 640, 360, 400), "L0 640×360 · 400 kbps");
        assert_eq!(
            format_send_layer(2, 1280, 720, 1500),
            "L2 1280×720 · 1500 kbps"
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
        // Video → resolution; layer is 1-indexed for display.
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
            Some("video L3/3 · 1280×720".to_string())
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
            Some("audio L1/3 · 24 kbps".to_string())
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
        // Mixed layers → lo–hi (1-indexed).
        assert_eq!(format_receive_spread(&[0, 1, 2]), "L1–L3");
        assert_eq!(format_receive_spread(&[2, 0]), "L1–L3");
        // All same → single label.
        assert_eq!(format_receive_spread(&[2, 2, 2]), "L3");
        assert_eq!(format_receive_spread(&[0]), "L1");
        // Empty → empty string.
        assert_eq!(format_receive_spread(&[]), "");
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
        assert_eq!(format_video_receive_summary(&[]), "Not receiving video");
        assert_eq!(
            format_video_receive_summary(&[0, 1, 2, 2]),
            "Pulling up to high quality · L1–L3 across 4 peers"
        );
        // Singular peer.
        assert_eq!(
            format_video_receive_summary(&[1]),
            "Pulling up to high quality · L2 across 1 peer"
        );
    }

    #[test]
    fn audio_receive_summary_peers_and_none() {
        assert_eq!(format_audio_receive_summary(0), "Not receiving audio");
        assert_eq!(format_audio_receive_summary(1), "Pulling near-full quality");
        assert_eq!(format_audio_receive_summary(5), "Pulling near-full quality");
    }

    #[test]
    fn audio_send_summary_reflects_a_lowered_cap() {
        // Auto → the full-quality phrase regardless of stored index.
        assert_eq!(
            format_audio_send_summary(true, Some(2)),
            "Sending high quality"
        );
        // Manual, no cap / best-tier cap → full quality.
        assert_eq!(
            format_audio_send_summary(false, None),
            "Sending high quality"
        );
        assert_eq!(
            format_audio_send_summary(false, Some(0)),
            "Sending high quality"
        );
        // Manual with a lowered cap → reflects the capped tier (not "high
        // quality"). AUDIO_TIER_LABELS[2] == "24 kbps".
        assert_eq!(
            format_audio_send_summary(false, Some(2)),
            "Sending up to 24 kbps"
        );
        // Out-of-range index degrades gracefully rather than panicking.
        assert_eq!(
            format_audio_send_summary(false, Some(99)),
            "Sending up to ?"
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
            "Pulling full quality · L3 · 1920×1080"
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

    fn layer_info(
        layer_id: u32,
        bitrate_kbps: u32,
        width: u32,
        height: u32,
    ) -> videocall_client::SimulcastLayerInfo {
        videocall_client::SimulcastLayerInfo {
            layer_id,
            bitrate_kbps,
            width,
            height,
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
        // Video/screen: "{res} · ~{kbps} · L{i}/{n}". n is the FULL ladder length
        // passed in (3), not the snapshot's own layer_count.
        let v = snap(PrefMediaKind::Video, 1, 960, 540, 600, None);
        assert_eq!(peer_row_metric(&v, 3, "ignored"), "540p · ~600k · L2/3");
        // Audio: "{kbps}k · {label} · L{i}/{n}".
        let a = snap(PrefMediaKind::Audio, 1, 0, 0, 32, None);
        assert_eq!(
            peer_row_metric(&a, 3, "mid (32k)"),
            "32k · mid (32k) · L2/3"
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
    fn send_rungs_marks_shed_top_layers_and_labels_top_active() {
        // 3 effective layers, only the bottom 2 active (top shed under congestion).
        let s = SimulcastSendSnapshot {
            simulcast_active: true,
            effective_layers: 3,
            active_layers: 2,
            layers: vec![
                layer_info(0, 300, 640, 360),
                layer_info(1, 600, 960, 540),
                layer_info(2, 0, 1280, 720), // shed (bitrate 0)
            ],
        };
        let rungs = send_rungs(&s);
        assert_eq!(rungs.len(), 3);
        assert!(rungs[0].active && rungs[1].active, "bottom two active");
        assert!(!rungs[2].active, "top layer shed");
        // kbps label only on the TOP ACTIVE pip (layer 1), not on base or shed.
        assert_eq!(rungs[0].kbps_label, None);
        assert_eq!(rungs[1].kbps_label, Some("600k".to_string()));
        assert_eq!(rungs[2].kbps_label, None);
        // res label under every pip.
        assert_eq!(rungs[0].res_label, "360p");
        assert_eq!(rungs[2].res_label, "720p");
        assert_eq!(send_rungs_aria(&rungs), "Sending 2 of 3 layers");
    }

    #[test]
    fn send_rungs_empty_when_no_layers() {
        // Single-stream / atomics-not-ticked → no layers → empty Vec (caller then
        // renders no strip and keeps the summary line).
        let s = SimulcastSendSnapshot {
            simulcast_active: false,
            effective_layers: 1,
            active_layers: 1,
            layers: vec![],
        };
        assert!(send_rungs(&s).is_empty());
        assert_eq!(send_rungs_aria(&[]), "Sending 1 layer");
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
