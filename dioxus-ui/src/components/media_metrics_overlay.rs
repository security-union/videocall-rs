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
//! `pointer-events: none` so it never intercepts tile interactions. But "on each
//! render" is NOT the ~1 Hz diagnostics cadence: a PeerTile re-renders whenever
//! ANY signal it reads changes, and it reads the audio level unconditionally, so
//! a SPEAKING tile re-renders at several Hz and rebuilds its payload (incl. an
//! O(peers) receive-snapshot scan) each time — see the cost note in
//! `peer_tile.rs`. All of this runs ONLY while the checkbox is on; off, the
//! payload is `None` and nothing is added to the DOM at all (conditional render,
//! not `visibility: hidden`).

use dioxus::prelude::*;

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
}
