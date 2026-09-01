// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-tile media-metrics overlay (issue 1768).
//!
//! When the diagnostics "Show media metrics on tiles" checkbox is on, each tile
//! renders a small, passive readout anchored at its bottom edge:
//!   * a REMOTE peer tile shows what THIS client is RECEIVING from that peer —
//!     decoded video resolution and received video fps. Its audio field is always
//!     the em-dash: a single-layer publisher's bitrate is set by its own AQ tier,
//!     which no receiver observes;
//!   * the LOCAL user's OWN tile shows what it is SENDING — the live encode
//!     resolution / target fps and the audio send bitrate.
//!
//! Cost: the numbers are pre-resolved into a [`MediaMetricsOverlay`] on each
//! render — in `peer_tile.rs` for a REMOTE peer, and in `host.rs` for the local
//! self-view (which is NOT a grid tile) — and this module only formats + renders
//! them. There is NO per-frame (rAF) work and no pixel reads; the overlay is
//! `pointer-events: none` so it never intercepts tile interactions. The REMOTE
//! peer payload (`peer_tile.rs`) is now built entirely from per-tile Dioxus
//! signals the component already maintains at the ~1 Hz diagnostics cadence —
//! decoded resolution and smoothed received fps (#1772) — so building it is an
//! O(1) signal read with NO per-render
//! O(peers) `per_peer_received_snapshots()` scan (that snapshot path still backs
//! the drawer / signal popup, and `peer_tile` still walks it when a signal popup
//! is OPEN, but it is no longer walked to build this overlay). The SELF payload
//! (`host.rs`) is likewise a cheap read of the live send-quality snapshot. A
//! PeerTile still re-renders whenever ANY signal it reads changes (it reads the
//! audio level unconditionally, so a SPEAKING tile re-renders at several Hz), but
//! each rebuild is now just those cheap reads. All of this runs ONLY while the
//! checkbox is on; off, the payload is `None`, nothing subscribes to the metric
//! signals, and nothing is added to the DOM at all (conditional render, not
//! `visibility: hidden`).
//!
//! The overlay's fps is the PAINTED rate (issue #1784): frames the decoder
//! actually drew to the canvas, measured at the paint site and delivered per-peer
//! on the `video_painted` diagnostics event (see
//! [`videocall_client::decode::peer_decoder`]). The overlay selects it via
//! [`overlay_painted_fps_sample`] and feeds it through [`next_overlay_fps`], which
//! snaps DOWN to 0 the instant painting stops (a stopped peer paints nothing → the
//! meter samples 0 → the readout reverts to the em-dash at once, not a phantom-fps
//! tail) and EMA-smooths ([`smooth_fps`]) the residual bucket-boundary jitter — see
//! those fns' docs. This replaces #1772's smoothed DECODE-CALL rate: once #1783
//! coalesces late-frame bursts to a single draw, painted-fps caps at the source
//! rate, so the number matches what the viewer sees rather than the (burstier)
//! decode-call rate. The RAW `fps_received` signal stays untouched for every other
//! consumer (drawer chart, signal popup, health reporter), where the burstiness view
//! is the useful one. Note `fps_received` counts DECODE CALLS, not arrivals: since
//! #2190 packets the simulcast rung guard skipped are excluded, so it no longer sums
//! the publisher's ladder. It still reads healthy during a visual freeze (the decode
//! loop keeps running), which is why the overlay is painted-sourced.
//!
//! Receiver-inclusive (issue #1787): because painted-fps is measured at THIS
//! client's paint site, it reflects LOCAL render health as well as the sender's
//! rate. A CPU-bound or throttled receiver (e.g. a backgrounded tab, or a slow
//! device that can't keep up) paints fewer frames and so reads a REDUCED overlay
//! fps even when the sender is perfectly healthy. This is intended — the overlay
//! exists to surface local jank — but is stated here to preempt "the overlay says
//! 18fps but the sender is fine" confusion: that number is the viewer's own paint
//! rate, not necessarily a sender fault. The drawer/popup DECODE-CALL fps (labelled
//! `FPS(decoded):` / `fps decoded` since #2190) is the one to compare against the
//! sender's output; a gap between it and painted while it reads healthy points at the
//! local receiver.

use dioxus::prelude::*;
use videocall_client::decode::peer_decoder::METRIC_FPS_PAINTED;
use videocall_diagnostics::{Metric, MetricValue};

/// `localStorage` key for the "Show media metrics on tiles" preference
/// (issue 1768). Boolean, defaults to `false` (off) via [`crate::local_storage`].
pub const MEDIA_METRICS_OVERLAY_KEY: &str = "diagnostics.media_metrics_overlay";

/// Context flag: is the per-tile media-metrics overlay enabled? (issue 1768)
///
/// Provided once at the meeting root (attendants). The diagnostics-drawer
/// checkbox writes it (and persists to [`MEDIA_METRICS_OVERLAY_KEY`]); every
/// `PeerTile` reads it to decide whether to build an overlay payload. A single
/// shared signal so toggling the checkbox shows/hides every tile's overlay
/// reactively.
#[derive(Clone, Copy)]
pub struct MediaMetricsOverlayCtx(pub Signal<bool>);

/// Pre-resolved per-tile media metrics for the overlay (issue 1768).
///
/// Built in `peer_tile.rs` at the diagnostics cadence; every field is already a
/// display-ready primitive so the render path does no computation. A `None`
/// field renders as an em-dash so the layout stays stable when a metric is
/// momentarily unavailable (e.g. audio-only peer → no resolution/fps).
#[derive(Clone, PartialEq, Debug)]
pub struct MediaMetricsOverlay {
    /// `true` for the local user's own tile (SENDING metrics); `false` for a
    /// remote peer (RECEIVED metrics). Drives the self/peer style + testid.
    pub is_self: bool,
    /// Video resolution `(width, height)` in px — decoded (remote) or encoded
    /// (self). `None` → em-dash (camera off / not yet known).
    pub resolution: Option<(u32, u32)>,
    /// Video frames per second — received (remote) or target (self). `None` →
    /// em-dash.
    pub fps: Option<f64>,
    /// Audio bitrate (kbps): the live send bitrate on the self tile, always `None`
    /// (em-dash) on a remote peer's. `None` → em-dash.
    pub audio_kbps: Option<u32>,
}

/// Issue 1821: pre-resolved stats for the shared-content tile's overlay. Built in
/// `peer_tile.rs` (only for the ScreenOnly sharer tile, and only when the
/// diagnostics checkbox is on) from the `screen_resolution` (decoded WxH) and
/// `screen_fps` (decode-call rate) signals the tile already maintains. Screen shares
/// carry no audio in this app, so there is no audio field.
#[derive(Clone, PartialEq, Debug)]
pub struct ScreenMetricsOverlay {
    /// Decoded screen-share resolution `(width, height)` in px. `None` → em-dash
    /// (not yet decoded).
    pub resolution: Option<(u32, u32)>,
    /// Screen-share frames per second (decode-call rate). `None` → em-dash.
    pub fps: Option<f64>,
}

/// Parse a `"{w}x{h}"` resolution string (the format `peer_tile.rs` stores in its
/// `video_resolution` signal) into `(w, h)`. Returns `None` for an empty or
/// malformed string, or if either dimension is 0. Pure / host-tested.
pub fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let mut parts = s.split('x');
    let w = parts.next()?.trim().parse::<u32>().ok()?;
    let h = parts.next()?.trim().parse::<u32>().ok()?;
    if parts.next().is_some() || w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

/// EMA smoothing factor for the overlay's displayed fps (issue #1772, retained by
/// #1784 as cosmetic smoothing).
///
/// Applied once per painted-fps sample, which the decoder's `video_painted` timer
/// emits at ~1 Hz. At a 1 s sample interval an EMA with `alpha = 0.25` has a ~3.5 s
/// time constant (the time to reach ~63% of a step: `-1 / ln(1 - alpha)` ≈ 3.48
/// samples) and a ~3 s center-of-mass (`(1 - alpha) / alpha`), i.e. the "3–5 s
/// effective window" a smoothed field-debug readout wants. Post-#1783 the painted
/// rate already caps at the source frame rate, so the residual jitter this damps is
/// only the ±1 fps a paint landing just before vs after a 1 s bucket boundary (the
/// paint cadence and the sample-timer cadence are not phase-locked); keeping the EMA
/// is cosmetic, but it also carries the mandatory snap-down (see
/// [`next_overlay_fps`]) so removing it would risk that behaviour for no gain.
pub const OVERLAY_FPS_EMA_ALPHA: f64 = 0.25;

/// Painted-fps threshold at/below which the overlay SNAPS its smoothed value to 0
/// instead of EMA-decaying toward it (issue #1772; the source is painted-fps as of
/// #1784). The decoder's painted-fps meter samples exactly 0 within ~1 s of a peer's
/// video stopping (a stopped/hidden tile paints nothing that window); below this
/// threshold there is effectively no video (the lowest real simulcast rung is 7 fps,
/// well above it), so the reading is treated as "stopped" and the overlay reverts to
/// the em-dash at once rather than trailing a phantom-fps tail.
pub const OVERLAY_FPS_SNAP_DOWN_EPSILON: f64 = 0.5;

/// One exponential-moving-average step for the overlay's fps readout (issue
/// #1772): `next = prev + alpha * (sample - prev)`.
///
/// SEMANTICS: as of #1784 this smooths the PAINTED rate (frames drawn to the
/// canvas), not the decode-call rate. Painted-fps caps at the source frame rate
/// post-#1783, so this only damps the ±1 fps bucket-boundary quantization between
/// consecutive 1 s samples (e.g. a true-30 source reading 29/31/30) — cosmetic, not
/// load-bearing for correctness. Pure / host-tested.
///
/// This is the INNER steady-state EMA step only. The asymmetric snap-down /
/// seed-up policy the overlay actually feeds lives in [`next_overlay_fps`], which
/// calls this for the smoothing case; call that, not this, from the feed path.
pub fn smooth_fps(prev: f64, sample: f64) -> f64 {
    prev + OVERLAY_FPS_EMA_ALPHA * (sample - prev)
}

/// One overlay-fps update step (issue #1772): given the previous smoothed value
/// and the latest PAINTED-fps sample (issue #1784), return the next smoothed value.
/// ASYMMETRIC by design — snap DOWN, smooth UP:
///   * SNAP-DOWN — a sample at/below [`OVERLAY_FPS_SNAP_DOWN_EPSILON`] (the `0` the
///     decoder's painted-fps meter samples within ~1 s of a peer's video stopping,
///     since a stopped/hidden tile paints nothing that window) collapses the output
///     to exactly `0.0` immediately, so the overlay's `fps > 0.0` gate reverts to
///     the em-dash — instead of an EMA decay that trails a ~10–12 s phantom-fps tail
///     and then hovers near-but-never-zero, showing "0fps" over a stale resolution
///     exactly when the drop matters.
///   * SEED-UP — on (re)appearance (`prev <= 0`) the first real sample is shown
///     verbatim so the overlay reflects the true rate at once, not a ramp from 0.
///   * SMOOTH — otherwise [`smooth_fps`] damps the residual bucket-boundary jitter.
///
/// Pure / host-tested; the production feed in `peer_tile.rs`'s `video_painted` arm
/// calls exactly this on the painted sample, and the raw `fps_received` arrival
/// signal every other consumer reads (drawer chart, signal popup, health reporter)
/// is left untouched.
pub fn next_overlay_fps(prev: f64, sample: f64) -> f64 {
    if sample <= OVERLAY_FPS_SNAP_DOWN_EPSILON {
        return 0.0;
    }
    if prev <= 0.0 {
        return sample;
    }
    smooth_fps(prev, sample)
}

/// The overlay's "↓ fps" SOURCE (issue #1784). Given the metrics of a
/// `video_painted` diagnostics event (emitted per-peer by the decoder at the paint
/// site) and the tile's `peer_id`, return `Some(painted_fps)` when the event is THIS
/// peer's CAMERA painted-fps update, else `None`.
///
/// This is what makes the overlay PAINTED-sourced rather than DECODE-CALL-sourced: it
/// keys on the painted metric name ([`METRIC_FPS_PAINTED`]), so a decode-call-rate
/// `video` / `fps_received` event — which carries no `fps_painted` metric — yields
/// `None` and never moves the overlay's fps. Screen-share painted events
/// (`media_type == "SCREEN"`) are filtered out so they don't feed the camera
/// overlay's fps field (screen fps has its own row/consumer). `to_peer` is the
/// SENDING peer's session id, matched against this tile's `peer_id` exactly as the
/// existing `video` / `video_resolution` arms do.
///
/// Pure / host-tested. `peer_tile.rs`'s `video_painted` arm calls exactly this, then
/// feeds the result through [`next_overlay_fps`] to preserve the snap-down-on-stop.
pub fn overlay_painted_fps_sample(metrics: &[Metric], peer_id: &str) -> Option<f64> {
    let mut to_peer: Option<&str> = None;
    let mut fps: Option<f64> = None;
    let mut is_screen = false;
    for m in metrics {
        match (m.name, &m.value) {
            ("to_peer", MetricValue::Text(p)) => to_peer = Some(p.as_ref()),
            (name, MetricValue::F64(v)) if name == METRIC_FPS_PAINTED => fps = Some(*v),
            ("media_type", MetricValue::Text(t)) => is_screen = t.as_ref() == "SCREEN",
            _ => {}
        }
    }
    if to_peer != Some(peer_id) || is_screen {
        return None;
    }
    fps
}

/// Received-audio kbps for a REMOTE peer's overlay: always `0.0`, which the
/// overlay's `> 0.0` gate renders as the em-dash "—k".
///
/// Nothing a receiver observes determines a publisher's audio bitrate — the rate
/// comes from the publisher's own AQ audio tier, not from the layer id being
/// decoded — so this takes no arguments. The SELF tile's `audio_kbps` is the only
/// honest audio figure.
pub fn overlay_audio_kbps() -> f64 {
    0.0
}

/// The overlay's `audio_kbps` field from the per-tile signal: a real number, or
/// `None` for the em-dash. `0.0` means "nothing to report" and is the only value
/// that renders as "—".
pub fn overlay_audio_kbps_display(kbps: f64) -> Option<u32> {
    (kbps > 0.0).then_some(kbps.round() as u32)
}

/// Format the compact overlay line with a leading direction glyph — `"↑ …"` when
/// `is_self` (SENDING) or `"↓ …"` for a remote peer (RECEIVING) — e.g.
/// `"↓ 1280×720 · 30fps · 48k"`. The glyph is a SHAPE cue so self/peer stay
/// distinguishable without relying on color (issue 1768: self text is white like
/// peers, so color alone can't be the cue — CVD + contrast). Each of the three
/// metric segments independently renders an em-dash (`\u{2014}`) when its metric
/// is absent, so an audio-only peer reads `"↓ — · —fps · 24k"` and the
/// three-column shape never shifts. Pure / host-tested — a wording change breaks a
/// test.
pub fn format_media_metrics_line(
    is_self: bool,
    resolution: Option<(u32, u32)>,
    fps: Option<f64>,
    audio_kbps: Option<u32>,
) -> String {
    let dash = "\u{2014}";
    // Direction glyph is a SHAPE cue (not color): "↑" sending (self), "↓"
    // receiving (peer). Keeps self/peer distinguishable for color-blind users and
    // on a bright self-video where a tinted color would fail AA contrast.
    let arrow = if is_self { "\u{2191}" } else { "\u{2193}" };
    let res = resolution
        .map(|(w, h)| format!("{w}\u{00d7}{h}"))
        .unwrap_or_else(|| dash.to_string());
    // fps rounded to a whole number — the tile has no room for decimals and the
    // received rate jitters sub-integer between ticks.
    let fps = fps
        .map(|f| format!("{}fps", f.round() as i64))
        .unwrap_or_else(|| format!("{dash}fps"));
    let audio = audio_kbps
        .map(|k| format!("{k}k"))
        .unwrap_or_else(|| format!("{dash}k"));
    format!("{arrow} {res} \u{b7} {fps} \u{b7} {audio}")
}

/// Render the per-tile overlay element for `data`, or an empty node when `None`
/// (checkbox off / no payload) so nothing is added to the DOM (issue 1768).
///
/// The container is `aria-hidden`: it is a passive, per-tile visual duplicate of
/// data the diagnostics drawer already exposes in a structured, on-demand form
/// (its per-peer Reception dump + the Simulcast layers section). Announcing ~1 Hz
/// numeric churn across every tile would flood screen-reader users with no added
/// task value; the CHECKBOX that toggles the feature is itself fully labeled and
/// keyboard-operable (see `diagnostics.rs`), which is where AT users control it.
/// `pointer-events: none` (in CSS) keeps tile clicks/hover unaffected.
pub fn media_metrics_overlay(data: Option<&MediaMetricsOverlay>) -> Element {
    let Some(d) = data else {
        return rsx! {};
    };
    let line = format_media_metrics_line(d.is_self, d.resolution, d.fps, d.audio_kbps);
    let modifier = if d.is_self {
        "media-metrics-overlay--self"
    } else {
        "media-metrics-overlay--peer"
    };
    let testid = if d.is_self {
        "media-metrics-overlay-self"
    } else {
        "media-metrics-overlay-peer"
    };
    rsx! {
        div {
            class: "media-metrics-overlay {modifier}",
            "aria-hidden": "true",
            "data-testid": testid,
            "{line}"
        }
    }
}

/// Issue 1821: format the shared-content stats line — `"↓ 1920×1080 · 30fps"`.
/// Received-only (screen always arrives FROM a peer), so it always leads with the
/// `↓` glyph and has NO audio segment. Each field independently renders an
/// em-dash (`\u{2014}`) when absent so the shape is stable pre-decode. Pure /
/// host-tested — a wording change breaks a test.
pub fn format_screen_metrics_line(resolution: Option<(u32, u32)>, fps: Option<f64>) -> String {
    let dash = "\u{2014}";
    let res = resolution
        .map(|(w, h)| format!("{w}\u{00d7}{h}"))
        .unwrap_or_else(|| dash.to_string());
    let fps = fps
        .map(|f| format!("{}fps", f.round() as i64))
        .unwrap_or_else(|| format!("{dash}fps"));
    format!("\u{2193} {res} \u{b7} {fps}")
}

/// Issue 1821: render the shared-content tile's stats overlay for `data`, or an
/// empty node when `None` (checkbox off / not the sharer tile). `aria-hidden`
/// and `pointer-events: none` (CSS) exactly like the camera
/// [`media_metrics_overlay`] — a passive, decorative duplicate of data the
/// diagnostics drawer already exposes; the checkbox is the AT control.
pub fn screen_metrics_overlay(data: Option<&ScreenMetricsOverlay>) -> Element {
    let Some(d) = data else {
        return rsx! {};
    };
    let line = format_screen_metrics_line(d.resolution, d.fps);
    rsx! {
        div {
            class: "media-metrics-overlay media-metrics-overlay--screen",
            "aria-hidden": "true",
            "data-testid": "media-metrics-overlay-screen",
            "{line}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_screen_line_full() {
        // issue 1821: received-only (↓), no audio segment.
        assert_eq!(
            format_screen_metrics_line(Some((1920, 1080)), Some(30.0)),
            "\u{2193} 1920\u{00d7}1080 \u{b7} 30fps"
        );
    }

    #[test]
    fn screen_line_rounds_fps_and_em_dashes_missing() {
        // fps rounds to a whole number.
        assert_eq!(
            format_screen_metrics_line(Some((1280, 720)), Some(14.6)),
            "\u{2193} 1280\u{00d7}720 \u{b7} 15fps"
        );
        // Pre-decode: both fields em-dash, shape preserved, still no audio segment.
        assert_eq!(
            format_screen_metrics_line(None, None),
            "\u{2193} \u{2014} \u{b7} \u{2014}fps"
        );
        // Resolution known, fps not yet.
        assert_eq!(
            format_screen_metrics_line(Some((640, 360)), None),
            "\u{2193} 640\u{00d7}360 \u{b7} \u{2014}fps"
        );
    }

    #[test]
    fn screen_line_has_no_audio_segment() {
        // issue 1821 (guard): the screen line must never carry the "· Nk" audio
        // segment the camera line has — screen shares carry no audio here. If
        // someone copies the camera formatter, the "k" tail would appear.
        let line = format_screen_metrics_line(Some((1920, 1080)), Some(30.0));
        assert!(
            !line.contains('k'),
            "screen line must have no audio segment: {line}"
        );
        // Exactly two dot-separated segments (res · fps), not three.
        assert_eq!(
            line.matches('\u{b7}').count(),
            1,
            "screen line has one middot: {line}"
        );
    }

    #[test]
    fn parses_valid_resolution() {
        assert_eq!(parse_resolution("1280x720"), Some((1280, 720)));
        assert_eq!(parse_resolution("320x180"), Some((320, 180)));
        assert_eq!(parse_resolution(" 640x360 "), Some((640, 360)));
    }

    #[test]
    fn rejects_malformed_or_zero_resolution() {
        assert_eq!(parse_resolution(""), None);
        assert_eq!(parse_resolution("1280"), None);
        assert_eq!(parse_resolution("1280x"), None);
        assert_eq!(parse_resolution("axb"), None);
        assert_eq!(parse_resolution("0x720"), None);
        assert_eq!(parse_resolution("1280x0"), None);
        // A third component is malformed for a WxH string.
        assert_eq!(parse_resolution("1x2x3"), None);
    }

    #[test]
    fn formats_full_line() {
        // Peer (receiving) → leading "↓".
        assert_eq!(
            format_media_metrics_line(false, Some((1280, 720)), Some(30.0), Some(48)),
            "\u{2193} 1280\u{00d7}720 \u{b7} 30fps \u{b7} 48k"
        );
    }

    #[test]
    fn formats_new_ladder_base_rung() {
        // issue 1768: the base simulcast rung is 320×180 @ 7 fps / ~120 kbps.
        // Self (sending) → leading "↑".
        assert_eq!(
            format_media_metrics_line(true, Some((320, 180)), Some(7.0), Some(12)),
            "\u{2191} 320\u{00d7}180 \u{b7} 7fps \u{b7} 12k"
        );
    }

    #[test]
    fn self_and_peer_glyphs_differ_as_shape_cue() {
        // issue 1768: self vs peer MUST be distinguishable by SHAPE (↑/↓), not by
        // color alone — self text is white like peers, so a CVD user or a bright
        // self-video (where a tint fails contrast) still tells them apart.
        let peer = format_media_metrics_line(false, Some((1280, 720)), Some(30.0), Some(48));
        let me = format_media_metrics_line(true, Some((1280, 720)), Some(30.0), Some(48));
        assert!(
            peer.starts_with("\u{2193} "),
            "peer overlay must lead with ↓ (receiving): {peer}"
        );
        assert!(
            me.starts_with("\u{2191} "),
            "self overlay must lead with ↑ (sending): {me}"
        );
        assert_ne!(
            peer, me,
            "self vs peer must be distinguishable by shape, not color alone"
        );
    }

    #[test]
    fn missing_fields_render_em_dash_but_keep_three_columns() {
        // Audio-only peer: no video res/fps, but audio still flowing.
        assert_eq!(
            format_media_metrics_line(false, None, None, Some(24)),
            "\u{2193} \u{2014} \u{b7} \u{2014}fps \u{b7} 24k"
        );
        // Everything unknown.
        assert_eq!(
            format_media_metrics_line(false, None, None, None),
            "\u{2193} \u{2014} \u{b7} \u{2014}fps \u{b7} \u{2014}k"
        );
    }

    /// **Issue #2170 made this state reachable ON THE SELF TILE for the first
    /// time.** The self overlay's resolution must render an em-dash while the
    /// SENDING fps/audio still read as live.
    ///
    /// Before #2170, `host.rs::self_metrics_overlay` built its resolution from the
    /// AQ tier's bounding box, and its `(video_width > 0 && video_height > 0)` gate
    /// could NEVER fire — a tier box is never zero, so `resolution: None` was
    /// unreachable for `is_self = true`. Now the camera encoder publishes `(0, 0)`
    /// as its NOT-YET-PUBLISHED sentinel (pre-first-frame, mid-codec-rebuild, and
    /// after `stop()`), so that gate DOES fire and this line renders.
    ///
    /// Deliberately NOT the "everything unknown" shape already covered above: the
    /// state #2170 creates is a partial one — geometry absent because it is a
    /// MEASUREMENT with no reading yet, while fps and audio kbps are still live
    /// values. A regression that blanked the whole line, or that printed `0×0`,
    /// would both be wrong in different ways, and only a `Some`-fps/`Some`-audio
    /// fixture distinguishes them.
    ///
    /// The `↑` prefix is asserted here too: this is the SELF arm, and it is the one
    /// the overlay's own shape-cue contract (CVD-safe self/peer distinction) applies
    /// to. `e2e/tests/media-metrics-overlay.spec.ts` polls PAST this state by
    /// design — its regex requires digits. This test guards the FORMATTER arm only:
    /// its `None` is a hand-passed literal, so the #2170 publish path cannot reach it.
    #[test]
    fn self_line_renders_an_em_dash_for_unpublished_encode_geometry() {
        assert_eq!(
            format_media_metrics_line(true, None, Some(30.0), Some(24)),
            "\u{2191} \u{2014} \u{b7} 30fps \u{b7} 24k"
        );
        // The three-column shape survives, exactly as for the peer arm.
        let line = format_media_metrics_line(true, None, Some(30.0), Some(24));
        assert_eq!(
            line.matches('\u{b7}').count(),
            2,
            "the self line keeps its three columns when geometry is unknown: {line}"
        );
        // It must NOT print a measured zero. `0×0` is the defect #2170 removes.
        // Asserted on the `×` SEPARATOR, not on the digit `0` — `30fps` and `24k`
        // both legitimately contain a zero, so a bare digit check would be either
        // vacuous or wrong. The separator appears only in a rendered dimension pair,
        // so its absence is exactly "no geometry was printed".
        assert!(
            !line.contains('\u{00d7}'),
            "an unpublished self geometry must print no dimension pair at all, \
             least of all 0×0: {line}"
        );
    }

    #[test]
    fn fps_is_rounded_to_whole() {
        assert_eq!(
            format_media_metrics_line(false, Some((640, 360)), Some(14.6), Some(24)),
            "\u{2193} 640\u{00d7}360 \u{b7} 15fps \u{b7} 24k"
        );
    }

    #[test]
    fn smooth_fps_converges_to_a_steady_input() {
        // issue #1772: feeding a constant rate must converge to that rate. Start
        // seeded at the value (as the production caller does on the first sample)
        // and confirm it stays put; also confirm convergence from a cold 0.
        let steady = 30.0;
        let mut y = steady;
        for _ in 0..50 {
            y = smooth_fps(y, steady);
        }
        assert!(
            (y - steady).abs() < 1e-9,
            "seeded steady input must stay: {y}"
        );

        let mut cold = 0.0;
        for _ in 0..100 {
            cold = smooth_fps(cold, steady);
        }
        assert!(
            (cold - steady).abs() < 0.5,
            "must converge toward the steady input from cold: {cold}"
        );
    }

    #[test]
    fn smooth_fps_damps_a_25_40_25_burst_near_25() {
        // issue #1772: a single-bucket 25→40→25 arrival spike (the exact field
        // failure — a 25 fps sender momentarily reading 40) must be damped to
        // within a few fps of the 25 baseline. With alpha = 0.25 the damped peak
        // is 25 + 0.25*(40-25) = 28.75, and the next 25 sample pulls it back.
        let baseline = 25.0;
        let peak = smooth_fps(baseline, 40.0);
        assert!(
            peak < 30.0,
            "damped peak must stay within a few fps of 25, got {peak}"
        );
        assert!(
            (peak - baseline).abs() < 4.0,
            "damped peak {peak} must be within a few fps of the {baseline} baseline"
        );
        // The spike decays back toward baseline on the following steady sample.
        let after = smooth_fps(peak, baseline);
        assert!(
            (after - baseline).abs() < peak - baseline,
            "output must decay back toward baseline after the spike: {after}"
        );
    }

    #[test]
    fn smooth_fps_output_is_below_the_raw_spike() {
        // issue #1772 (fails-if-smoothing-removed guard): a spike above the
        // running value must NOT pass through at full amplitude. If someone
        // deletes the EMA and returns `sample`, this assertion fails.
        let prev = 25.0;
        let raw_spike = 60.0;
        let smoothed = smooth_fps(prev, raw_spike);
        assert!(
            smoothed < raw_spike,
            "smoothed output {smoothed} must be strictly below the raw spike {raw_spike}"
        );
        // And it must actually move toward the spike (not ignore the input).
        assert!(
            smoothed > prev,
            "smoothed output {smoothed} must track upward toward the spike from {prev}"
        );
    }

    #[test]
    fn next_overlay_fps_snaps_down_to_zero_when_video_stops() {
        // issue #1772 (snap-down guard): drive the EXACT production step
        // (`peer_tile.rs` calls `next_overlay_fps`) for a 25,25,0 sequence — a
        // peer decoding at 25 fps whose video then stops (raw fps → 0). The final
        // smoothed value MUST be exactly 0.0 so the overlay's `fps > 0.0` gate
        // reverts to the em-dash, not "0fps" over a stale resolution.
        //
        // Mutation-sensitive: if the snap-down is removed and this EMA-decays,
        // `next_overlay_fps(25, 0)` = 18.75 and the final value is > 0 → this fails.
        let mut y = 0.0;
        for &raw in &[25.0, 25.0, 0.0] {
            y = next_overlay_fps(y, raw);
        }
        assert_eq!(
            y, 0.0,
            "a raw 0 fps sample must snap the smoothed value to 0"
        );
    }

    #[test]
    fn next_overlay_fps_seeds_up_and_smooths_nonzero_samples() {
        // issue #1772: on (re)appearance the first sample shows verbatim (seed),
        // and a subsequent nonzero sample is EMA-smoothed (NOT snapped) — proving
        // the snap-down is asymmetric and doesn't clobber a live upward reading.
        assert_eq!(
            next_overlay_fps(0.0, 30.0),
            30.0,
            "first sample on (re)appearance must seed verbatim"
        );
        let after = next_overlay_fps(30.0, 60.0);
        assert!(
            after > 30.0 && after < 60.0,
            "a nonzero sample must be EMA-smoothed, not snapped: {after}"
        );
    }

    #[test]
    fn overlay_audio_kbps_em_dashes_every_remote_peer() {
        // The nominals come from the production ladder, so this fails the moment a
        // layer->bitrate mapping is reintroduced here.
        let kbps = overlay_audio_kbps();
        assert_eq!(kbps, 0.0);
        for rung in 0..3u32 {
            let nominal = videocall_client::decode::layer_chooser::audio_layer_kbps(rung).unwrap();
            assert_ne!(kbps, f64::from(nominal), "reads as rung {rung}'s nominal");
        }
        let line = format_media_metrics_line(
            false,
            Some((1280, 720)),
            Some(30.0),
            overlay_audio_kbps_display(kbps),
        );
        assert!(
            line.ends_with("\u{2014}k"),
            "a remote peer's audio field must render the em-dash: {line}"
        );
    }

    #[test]
    fn overlay_audio_kbps_display_gates_zero_to_the_em_dash() {
        // The last link to the rendered field: overlay_audio_kbps -> this gate ->
        // format_media_metrics_line, each called by a test rather than re-implemented.
        assert_eq!(overlay_audio_kbps_display(0.0), None);
        assert_eq!(overlay_audio_kbps_display(48.0), Some(48));
        assert_eq!(overlay_audio_kbps_display(24.4), Some(24));
        assert_eq!(overlay_audio_kbps_display(23.6), Some(24));
    }

    #[test]
    fn overlay_fps_is_sourced_from_painted_not_arrival() {
        // issue #1784 (fails-if-source-reverted guard): the overlay's "↓ fps" is fed
        // from the PAINTED metric (`fps_painted` on the `video_painted` event), never
        // from the decode-call-rate `fps_received`. A painted event for this peer yields
        // the painted value; an arrival-shaped event (carrying `fps_received`, not
        // `fps_painted`) yields None, so it can never move the overlay. If someone
        // repoints the overlay source back to `fps_received`, the arrival case below
        // returns Some and this test fails.
        let painted = [
            Metric {
                name: "to_peer",
                value: MetricValue::text_static("peer-1"),
            },
            Metric {
                name: METRIC_FPS_PAINTED,
                value: MetricValue::F64(24.0),
            },
            Metric {
                name: "media_type",
                value: MetricValue::text_static("VIDEO"),
            },
        ];
        assert_eq!(
            overlay_painted_fps_sample(&painted, "peer-1"),
            Some(24.0),
            "a painted event for this peer must yield the painted fps"
        );

        let arrival = [
            Metric {
                name: "to_peer",
                value: MetricValue::text_static("peer-1"),
            },
            Metric {
                name: "fps_received",
                value: MetricValue::F64(55.0),
            },
            Metric {
                name: "media_type",
                value: MetricValue::text_static("VIDEO"),
            },
        ];
        assert_eq!(
            overlay_painted_fps_sample(&arrival, "peer-1"),
            None,
            "a decode-call-rate fps_received event must NOT feed the painted overlay"
        );
    }

    #[test]
    fn overlay_painted_fps_filters_wrong_peer_and_screen() {
        let make = |to: &'static str, mt: &'static str| {
            [
                Metric {
                    name: "to_peer",
                    value: MetricValue::text_static(to),
                },
                Metric {
                    name: METRIC_FPS_PAINTED,
                    value: MetricValue::F64(30.0),
                },
                Metric {
                    name: "media_type",
                    value: MetricValue::text_static(mt),
                },
            ]
        };
        // Camera painted-fps for THIS peer → the value.
        assert_eq!(
            overlay_painted_fps_sample(&make("peer-1", "VIDEO"), "peer-1"),
            Some(30.0)
        );
        // A different peer's event must not leak into this tile.
        assert_eq!(
            overlay_painted_fps_sample(&make("peer-2", "VIDEO"), "peer-1"),
            None
        );
        // Screen-share painted-fps must not feed the camera overlay's fps field.
        assert_eq!(
            overlay_painted_fps_sample(&make("peer-1", "SCREEN"), "peer-1"),
            None
        );
    }
}
