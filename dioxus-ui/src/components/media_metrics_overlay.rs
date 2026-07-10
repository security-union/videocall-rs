// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-tile media-metrics overlay (issue 1768).
//!
//! When the diagnostics "Show media metrics on tiles" checkbox is on, each tile
//! renders a small, passive readout anchored at its bottom edge:
//!   * a REMOTE peer tile shows what THIS client is RECEIVING from that peer —
//!     decoded video resolution, received video fps, and the received audio
//!     layer's bitrate;
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
//! decoded resolution, smoothed received fps (#1772), and the received-audio kbps
//! signal (#1769) — so building it is an O(1) signal read with NO per-render
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
//! those fns' docs. This replaces #1772's smoothed ARRIVAL rate: once #1783
//! coalesces late-frame bursts to a single draw, painted-fps caps at the source
//! rate, so the number matches what the viewer sees rather than the (burstier)
//! network arrival rate. The RAW `fps_received` arrival signal stays untouched for
//! every other consumer (drawer chart, signal popup, health reporter), where the
//! network-burstiness view is the useful one.

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
    /// Audio bitrate (kbps) — the received layer's rung (remote) or the send
    /// bitrate (self). `None` → em-dash.
    pub audio_kbps: Option<u32>,
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
/// canvas), not the arrival rate. Painted-fps caps at the source frame rate
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
/// This is what makes the overlay PAINTED-sourced rather than ARRIVAL-sourced: it
/// keys on the painted metric name ([`METRIC_FPS_PAINTED`]), so an arrival-rate
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

/// Map a peer's audio-on state to the received-audio kbps the overlay shows
/// (issue #1769): `true` → the base received-audio layer nominal
/// ([`videocall_client::decode::layer_chooser::base_audio_layer_kbps`]); `false`
/// → `0.0`, which the overlay's `> 0.0` gate renders as the em-dash "—k".
///
/// Extracted as a pure, host-testable fn so the audio-on → kbps mapping is
/// guarded by a unit test rather than re-implemented inline: `peer_tile.rs` routes
/// BOTH the `peer_status` heartbeat write and the mount seed through this fn, so
/// the test exercises the exact production mapping. Audio is single-layer by
/// default, so the base rung is the exact received nominal the drawer shows.
pub fn overlay_audio_kbps(audio_enabled: bool) -> f64 {
    if audio_enabled {
        videocall_client::decode::layer_chooser::base_audio_layer_kbps() as f64
    } else {
        0.0
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn overlay_audio_kbps_on_shows_a_number_off_shows_em_dash() {
        // issue #1769: the exact production mapping `peer_tile.rs` routes both the
        // peer_status write AND the mount seed through. Audio-ON must yield a real
        // received-audio kbps that formats as "{n}k"; audio-OFF must yield 0.0,
        // which the overlay's `> 0.0` gate renders as the em-dash "—k". This is
        // the discriminating guard for #1769 (not a `== AUDIO_LAYER_KBPS[0]`
        // tautology): it asserts on→real-number and off→em-dash behaviourally.
        let on = overlay_audio_kbps(true);
        assert!(
            on > 0.0,
            "audio-on must map to a real received-audio kbps, got {on}"
        );
        // ON routes through the overlay's gate + formatter to a number + "k".
        let on_kbps = (on > 0.0).then_some(on.round() as u32);
        let on_line = format_media_metrics_line(false, Some((1280, 720)), Some(30.0), on_kbps);
        assert!(
            !on_line.contains("\u{2014}k") && on_line.ends_with(&format!("{}k", on.round() as i64)),
            "audio-on must render a numeric kbps, not the em-dash: {on_line}"
        );

        // OFF → 0.0 → gate drops it → the formatter renders the em-dash "—k".
        let off = overlay_audio_kbps(false);
        assert_eq!(off, 0.0, "audio-off must map to 0.0");
        let off_kbps = (off > 0.0).then_some(off.round() as u32);
        let off_line = format_media_metrics_line(false, Some((1280, 720)), Some(30.0), off_kbps);
        assert!(
            off_line.ends_with("\u{2014}k"),
            "audio-off must render the em-dash: {off_line}"
        );
    }

    #[test]
    fn overlay_fps_is_sourced_from_painted_not_arrival() {
        // issue #1784 (fails-if-source-reverted guard): the overlay's "↓ fps" is fed
        // from the PAINTED metric (`fps_painted` on the `video_painted` event), never
        // from the arrival-rate `fps_received`. A painted event for this peer yields
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
            "an arrival-rate fps_received event must NOT feed the painted overlay"
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
