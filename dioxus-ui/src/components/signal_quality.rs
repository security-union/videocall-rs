// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-peer signal quality tracking and popup chart.
//!
//! [`PeerSignalHistory`] collects periodic quality samples and derives a
//! [`SignalLevel`] that drives the [`SignalBarsIcon`] overlay on each tile.
//! [`SignalQualityPopup`] renders a scrollable SVG line chart of the history
//! with separate lines for audio, video, screen share, and latency.

use std::collections::VecDeque;

use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;

use crate::theme::color as theme_color;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Discrete signal quality level shown as 0-5 filled bars.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SignalLevel {
    /// 5 bars -- excellent quality
    #[default]
    Excellent,
    /// 4 bars -- good quality
    Good,
    /// 3 bars -- fair quality
    Fair,
    /// 2 bars -- poor quality
    Poor,
    /// 1 bar -- bad quality
    Bad,
    /// 0 bars with red slash -- connection lost
    Lost,
}

impl SignalLevel {
    /// Number of filled bars for this level (0..=5).
    pub fn bars(self) -> u8 {
        match self {
            Self::Excellent => 5,
            Self::Good => 4,
            Self::Fair => 3,
            Self::Poor => 2,
            Self::Bad => 1,
            Self::Lost => 0,
        }
    }

    /// Whether the signal is completely lost.
    pub fn is_lost(self) -> bool {
        self == Self::Lost
    }

    /// Derive a signal level from a combined quality score (0.0 -- 1.0).
    pub fn from_quality(q: f64) -> Self {
        if q >= 0.9 {
            Self::Excellent
        } else if q >= 0.75 {
            Self::Good
        } else if q >= 0.5 {
            Self::Fair
        } else if q >= 0.25 {
            Self::Poor
        } else if q > 0.0 {
            Self::Bad
        } else {
            Self::Lost
        }
    }
}

/// A single quality measurement at a point in time.
#[derive(Clone, Debug)]
pub struct SignalSample {
    /// Milliseconds since epoch (`js_sys::Date::now()`).
    pub timestamp_ms: f64,
    // Normalized quality scores (0.0-1.0)
    pub audio_quality: f64,
    pub video_quality: f64,
    pub screen_quality: f64,
    // Raw video metrics
    pub video_fps: f64,
    pub video_bitrate_kbps: f64,
    /// Video resolution as "WxH" (e.g. "1280x720"), empty when unknown.
    pub video_resolution: String,
    // Raw audio metrics
    pub audio_bitrate_kbps: f64,
    pub audio_expand_rate: f64, // per-mille
    pub audio_buffer_ms: f64,
    // Raw screen share metrics
    pub screen_enabled: bool,
    pub screen_fps: f64,
    pub screen_bitrate_kbps: f64,
    /// Screen-share **received** resolution as "WxH" (e.g. "1920x1080"), i.e.
    /// the dimensions of the decoded canvas. Empty when unknown.
    pub screen_resolution: String,
    /// Screen-share **source** resolution as "WxH" — the publisher's native
    /// `MediaStreamTrack.getSettings()` capture dimensions. Empty when the
    /// publisher doesn't report it (older clients) or hasn't started sharing
    /// yet. When this differs from `screen_resolution` the publisher's
    /// encoder downscaled in transit.
    pub screen_source_resolution: String,
    /// Issue #903: publisher's encoder *target* bitrate for the screen-share // @token-exempt: issue ref, not a color
    /// track (kbps). What the encoder is currently trying to produce, not
    /// the realised on-the-wire bitrate (which is `screen_bitrate_kbps`).
    /// `0` means the publisher hasn't stamped the field — older client or
    /// AQ tier 0 (unconstrained). The Cause tooltip line is omitted in
    /// either case.
    pub screen_encoder_target_bitrate_kbps: u32,
    /// Issue #903: name of the adaptive-quality tier currently constraining // @token-exempt: issue ref, not a color
    /// the publisher's screen-share encoder (e.g. `"high"`, `"medium"`,
    /// `"low"`). Empty when AQ isn't engaged or the publisher is older.
    pub screen_adaptive_tier: String,
    /// Issue #903: short publisher-classified cause of the downscale, // @token-exempt: issue ref, not a color
    /// one of `"bitrate-limited"`, `"cpu-pressure"`, `"network-rtt"`,
    /// `"network-loss"`, `"manual-cap"`, or empty. Empty means the encoder
    /// is unconstrained or the publisher doesn't supply the field.
    pub screen_cause_hint: String,
    // Latency
    pub latency_ms: f64,
}

// Manual PartialEq for SignalSample so the Props derive works.
impl PartialEq for SignalSample {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp_ms == other.timestamp_ms
            && self.audio_quality == other.audio_quality
            && self.video_quality == other.video_quality
            && self.screen_quality == other.screen_quality
            && self.video_fps == other.video_fps
            && self.video_bitrate_kbps == other.video_bitrate_kbps
            && self.video_resolution == other.video_resolution
            && self.audio_bitrate_kbps == other.audio_bitrate_kbps
            && self.audio_expand_rate == other.audio_expand_rate
            && self.audio_buffer_ms == other.audio_buffer_ms
            && self.screen_enabled == other.screen_enabled
            && self.screen_fps == other.screen_fps
            && self.screen_bitrate_kbps == other.screen_bitrate_kbps
            && self.screen_resolution == other.screen_resolution
            && self.screen_source_resolution == other.screen_source_resolution
            && self.screen_encoder_target_bitrate_kbps == other.screen_encoder_target_bitrate_kbps
            && self.screen_adaptive_tier == other.screen_adaptive_tier
            && self.screen_cause_hint == other.screen_cause_hint
            && self.latency_ms == other.latency_ms
    }
}

/// Builder struct for passing raw metrics to [`PeerSignalHistory::push_sample`].
/// Keeps the argument list manageable when many metrics are tracked.
#[derive(Clone, Debug, Default)]
pub struct SampleData {
    pub video_fps: f64,
    pub video_bitrate_kbps: f64,
    /// Video resolution as "WxH", empty when unknown.
    pub video_resolution: String,
    pub audio_bitrate_kbps: f64,
    pub audio_expand_rate: f64,
    pub audio_buffer_ms: f64,
    pub screen_enabled: bool,
    pub screen_fps: f64,
    pub screen_bitrate_kbps: f64,
    /// Screen-share **received** resolution as "WxH", empty when unknown.
    pub screen_resolution: String,
    /// Screen-share **source** resolution as "WxH" — publisher's native
    /// capture size as reported on the wire. Empty when the publisher
    /// doesn't report it or hasn't been seen yet.
    pub screen_source_resolution: String,
    /// Issue #903: publisher's encoder target bitrate for the screen-share // @token-exempt: issue ref, not a color
    /// track (kbps); `0` when the publisher doesn't supply the field.
    pub screen_encoder_target_bitrate_kbps: u32,
    /// Issue #903: name of the AQ tier currently constraining the // @token-exempt: issue ref, not a color
    /// publisher's screen-share encoder. Empty when AQ isn't engaged.
    pub screen_adaptive_tier: String,
    /// Issue #903: short publisher-classified cause of the downscale. // @token-exempt: issue ref, not a color
    /// Empty when the encoder is unconstrained or the publisher doesn't
    /// supply the field.
    pub screen_cause_hint: String,
    pub latency_ms: f64,
    pub audio_enabled: bool,
    pub video_enabled: bool,
}

/// Maximum number of signal samples retained per peer.
/// At 1 sample/second this covers 30 minutes of history.
const MAX_SIGNAL_SAMPLES: usize = 1800;

/// Accumulates [`SignalSample`]s for a single peer.  Uses a bounded
/// [`VecDeque`] so memory stays capped even for very long meetings or
/// tabs left open indefinitely.
#[derive(Clone, Debug, Default)]
pub struct PeerSignalHistory {
    samples: VecDeque<SignalSample>,
}

impl PeerSignalHistory {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    /// Append a new sample, computing quality scores internally from raw
    /// metrics. Evicts the oldest sample when at capacity.
    pub fn push_sample(&mut self, data: &SampleData) {
        self.push_sample_at(data, js_sys::Date::now());
    }

    /// Append a sample with an explicit timestamp. Lets host unit tests
    /// exercise the quality-derivation logic without depending on `js_sys`.
    pub fn push_sample_at(&mut self, data: &SampleData, timestamp_ms: f64) {
        if self.samples.len() >= MAX_SIGNAL_SAMPLES {
            self.samples.pop_front();
        }

        // Video quality: fps as ratio of a 30fps target, clamped to 0-1.
        let video_quality = if data.video_enabled {
            (data.video_fps / 30.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Audio quality: composite of expand_rate and jitter buffer health.
        //
        // - expand_rate: 0 = perfect, 1000 = fully concealed -- weight 60%
        // - buffer_health: how close the buffer is to the optimal range --
        //   weight 40%. Buffer too large (>150ms) signals congestion/jitter;
        //   too small (<20ms) risks underrun.
        //
        // This composite ensures the audio line shows meaningful variation
        // based on buffer depth changes even when there is no packet loss.
        let audio_quality = if data.audio_enabled {
            let expand_score = (1.0 - (data.audio_expand_rate / 1000.0)).clamp(0.0, 1.0);
            let buffer_score = if data.audio_buffer_ms <= 0.0 {
                // No buffer data yet -- assume moderate health.
                0.5
            } else if data.audio_buffer_ms < 20.0 {
                // Buffer dangerously low -- risk of underrun.
                (data.audio_buffer_ms / 20.0).clamp(0.0, 1.0) * 0.8
            } else if data.audio_buffer_ms <= 150.0 {
                // Healthy range.
                1.0
            } else {
                // Buffer too large -- indicates network problems.
                (1.0 - ((data.audio_buffer_ms - 150.0) / 350.0)).clamp(0.0, 1.0)
            };
            (expand_score * 0.6 + buffer_score * 0.4).clamp(0.0, 1.0)
        } else {
            0.0
        };

        // Screen quality: fps as ratio of a 30fps target when screen share is active.
        let screen_quality = if data.screen_enabled {
            (data.screen_fps / 30.0).clamp(0.0, 1.0)
        } else {
            0.0
        };

        self.samples.push_back(SignalSample {
            timestamp_ms,
            audio_quality,
            video_quality,
            screen_quality,
            video_fps: data.video_fps,
            video_bitrate_kbps: data.video_bitrate_kbps,
            video_resolution: data.video_resolution.clone(),
            audio_bitrate_kbps: data.audio_bitrate_kbps,
            audio_expand_rate: data.audio_expand_rate,
            audio_buffer_ms: data.audio_buffer_ms,
            screen_enabled: data.screen_enabled,
            screen_fps: data.screen_fps,
            screen_bitrate_kbps: data.screen_bitrate_kbps,
            screen_resolution: data.screen_resolution.clone(),
            screen_source_resolution: data.screen_source_resolution.clone(),
            screen_encoder_target_bitrate_kbps: data.screen_encoder_target_bitrate_kbps,
            screen_adaptive_tier: data.screen_adaptive_tier.clone(),
            screen_cause_hint: data.screen_cause_hint.clone(),
            latency_ms: data.latency_ms,
        });
    }

    /// Return a `Vec` copy of the samples (for passing to the popup component).
    /// Callers should avoid calling this on every render -- only when the popup
    /// is actually visible.
    pub fn samples_vec(&self) -> Vec<SignalSample> {
        self.samples.iter().cloned().collect()
    }

    /// Derive the current signal level from the most recent sample.
    ///
    /// If audio or video is disabled for the peer the caller should set the
    /// corresponding quality to `None` when computing the combined score.
    pub fn current_level(
        &self,
        audio_enabled: bool,
        video_enabled: bool,
        screen_enabled: bool,
    ) -> SignalLevel {
        match self.samples.back() {
            Some(s) => {
                let combined = combined_quality(
                    s.audio_quality,
                    s.video_quality,
                    s.screen_quality,
                    audio_enabled,
                    video_enabled,
                    screen_enabled,
                );
                SignalLevel::from_quality(combined)
            }
            None => SignalLevel::Excellent, // no data yet -- assume good
        }
    }
}

/// Compute a single combined quality score.
///
/// When multiple streams are enabled the score is the mean of the active ones.
/// When none are active we return 1.0 (peer has everything intentionally off --
/// not a quality problem).
fn combined_quality(
    audio: f64,
    video: f64,
    screen: f64,
    audio_en: bool,
    video_en: bool,
    screen_en: bool,
) -> f64 {
    let mut sum = 0.0;
    let mut count = 0;
    if audio_en {
        sum += audio;
        count += 1;
    }
    if video_en {
        sum += video;
        count += 1;
    }
    if screen_en {
        sum += screen;
        count += 1;
    }
    if count == 0 {
        1.0
    } else {
        sum / count as f64
    }
}

/// Convenience bundle passed through the rendering pipeline so we don't keep
/// adding individual arguments.
pub struct SignalInfo {
    pub level: SignalLevel,
    pub history: Vec<SignalSample>,
    /// Meeting start time (Unix ms) for the chart X-axis reference.
    pub meeting_start_ms: f64,
    /// Current transport string for this peer (`"webtransport"` |
    /// `"websocket"` | `"unknown"`), as reported via the `peer_status`
    /// diagnostics metric. `None` when no `peer_status` event has been
    /// observed yet. Renders as a header badge in [`SignalQualityPopup`];
    /// not part of the time-series chart.
    pub transport: Option<String>,
}

// ---------------------------------------------------------------------------
// Popup component
// ---------------------------------------------------------------------------

/// Props for [`SignalQualityPopup`].
#[derive(Props, Clone, PartialEq)]
pub struct SignalQualityPopupProps {
    /// The peer's session ID -- used to generate unique DOM element IDs so
    /// multiple popups can coexist without duplicate-ID collisions.
    peer_id: String,
    /// Human-readable peer name shown in the popup header.
    peer_name: String,
    /// Full history of samples to chart.
    history: Vec<SignalSample>,
    /// Meeting start time (Unix ms). The X-axis is relative to this so all
    /// peers share the same time reference for easy comparison.
    meeting_start_ms: f64,
    /// Current transport string (`"webtransport"` | `"websocket"` |
    /// `"unknown"`) sourced from the `peer_status` diagnostics metric.
    /// Rendered as a small WT/WS/em-dash badge in the popup header.
    /// `None` is treated like `"unknown"` (em-dash).
    #[props(default)]
    transport: Option<String>,
    /// Called when the user dismisses the popup.
    on_close: EventHandler<()>,
}

/// Get or create the global tooltip element on `<body>`.
/// Rendering the tooltip outside all grid-items guarantees it is never
/// clipped or hidden by adjacent peer tile stacking contexts.
fn get_or_create_tooltip_el() -> web_sys::HtmlElement {
    let doc = gloo_utils::document();
    if let Some(el) = doc.get_element_by_id("signal-chart-tooltip-global") {
        el.unchecked_into()
    } else {
        let el = doc.create_element("div").unwrap();
        el.set_id("signal-chart-tooltip-global");
        el.set_class_name("signal-chart-tooltip");
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        html_el.style().set_property("display", "none").unwrap();
        doc.body().unwrap().append_child(&html_el).unwrap();
        html_el
    }
}

/// Show the tooltip at viewport coordinates with metric content.
#[allow(clippy::too_many_arguments)]
fn show_body_tooltip(
    x: f64,
    y: f64,
    time_str: &str,
    sample: &SignalSample,
    show_video: bool,
    show_audio: bool,
    show_screen: bool,
    show_latency: bool,
) {
    let el = get_or_create_tooltip_el();
    let style = el.style();
    style.set_property("left", &format!("{x:.0}px")).unwrap();
    style.set_property("top", &format!("{y:.0}px")).unwrap();
    style.set_property("display", "block").unwrap();

    let video_tier = infer_video_tier(&sample.video_resolution);
    let video_line = if show_video {
        if sample.video_resolution.is_empty() {
            format!(
                "<span style='color:{}'>Video: {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        } else if video_tier.is_empty() {
            format!(
                "<span style='color:{}'>Video: {} | {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_resolution,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        } else {
            format!(
                "<span style='color:{}'>Video: {} ({}) | {:.1} fps | {:.0} kbps</span>",
                theme_color::SIGNAL_VIDEO,
                sample.video_resolution,
                video_tier,
                sample.video_fps,
                sample.video_bitrate_kbps
            )
        }
    } else {
        String::new()
    };
    let audio_line = if show_audio {
        format!(
            "<span style='color:{}'>Audio: buf {:.0}ms | expand {:.0}\u{2030}</span>",
            theme_color::SIGNAL_AUDIO,
            sample.audio_buffer_ms,
            sample.audio_expand_rate
        )
    } else {
        String::new()
    };
    let screen_line = build_screen_tooltip_line(sample, show_screen);
    let screen_cause_line = build_screen_cause_line(sample, show_screen);
    let latency_line = if show_latency {
        format!(
            "<span style='color:{}'>Server RTT: {:.0} ms</span>",
            theme_color::SIGNAL_LATENCY,
            sample.latency_ms
        )
    } else {
        String::new()
    };

    let lines: Vec<String> = [
        Some(format!(
            "<div>Time: {time_str}</div><div style='border-bottom:1px solid {};margin:2px 0'></div>",
            theme_color::TOOLTIP_DIVIDER
        )),
        if video_line.is_empty() {
            None
        } else {
            Some(format!("<div>{video_line}</div>"))
        },
        if audio_line.is_empty() {
            None
        } else {
            Some(format!("<div>{audio_line}</div>"))
        },
        if screen_line.is_empty() {
            None
        } else {
            Some(format!("<div>{screen_line}</div>"))
        },
        if screen_cause_line.is_empty() {
            None
        } else {
            // Slight left-indent (font-size:11px) so the cause line reads as
            // a continuation of the Screen row above it.
            Some(format!(
                "<div style='font-size:11px;padding-left:8px'>{screen_cause_line}</div>"
            ))
        },
        if latency_line.is_empty() {
            None
        } else {
            Some(format!("<div>{latency_line}</div>"))
        },
    ]
    .into_iter()
    .flatten()
    .collect();
    el.set_inner_html(&lines.join(""));
}

fn infer_video_tier(resolution: &str) -> &'static str {
    let mut parts = resolution.split('x');
    let _width = parts.next().and_then(|w| w.parse::<u32>().ok());
    let height = parts.next().and_then(|h| h.parse::<u32>().ok());

    match height.unwrap_or_default() {
        h if h >= 1080 => "Full HD",
        h if h >= 900 => "HD+",
        h if h >= 720 => "HD",
        h if h >= 540 => "Standard",
        h if h >= 480 => "Medium",
        h if h >= 360 => "Low",
        h if h >= 270 => "Very Low",
        h if h > 0 => "Minimal",
        _ => "",
    }
}

/// Compact tier label used by the Screen tooltip line. The Screen tooltip
/// has been tightened to fit on one row alongside the resolution numbers,
/// so we substitute the most common labels for their standard
/// abbreviations (`Full HD` → `FHD`, `Quad HD` → `QHD`, `4K UHD` → `UHD`).
/// `HD` and the lower-density labels (`Medium`, `Low`, `Very Low`, etc.)
/// stay as-is because they're already short or have no widely-understood
/// abbreviation.
///
/// The camera-video tooltip line continues to use [`infer_video_tier`]
/// because that row has more horizontal real estate and the full label is
/// easier to scan when only a single value is shown.
fn infer_video_tier_short(resolution: &str) -> &'static str {
    match infer_video_tier(resolution) {
        "Full HD" => "FHD",
        "Quad HD" => "QHD",
        "4K UHD" => "UHD",
        other => other,
    }
}

/// Parse a `"WxH"` resolution string into `(width, height)`. Returns `None`
/// when either side is missing, non-numeric, or zero. Used by the
/// degradation-ratio helper below.
fn parse_resolution(resolution: &str) -> Option<(u32, u32)> {
    let mut parts = resolution.split('x');
    let w = parts.next()?.parse::<u32>().ok()?;
    let h = parts.next()?.parse::<u32>().ok()?;
    if w == 0 || h == 0 {
        None
    } else {
        Some((w, h))
    }
}

/// Compute the screen-share downscale ratio as a rounded percentage of the
/// publisher's source pixel area lost in transit:
///
/// `1 - (received_w × received_h) / (source_w × source_h)`
///
/// Returns `None` when either resolution is missing / unparseable OR when the
/// received area is not strictly smaller than the source (no downscale to
/// report). The percentage is rounded to the nearest integer in the
/// `1..=100` range — values that round to zero return `None` rather than
/// surfacing a misleading `↓0%` badge.
///
/// Using pixel area (not linear dimensions) is intentional: a 2× linear
/// downscale loses 75% of pixels, which is the user-visible quantity.
fn screen_downscale_percent(source: &str, received: &str) -> Option<u32> {
    let (sw, sh) = parse_resolution(source)?;
    let (rw, rh) = parse_resolution(received)?;
    let src_area = sw as u64 * sh as u64;
    let rcv_area = rw as u64 * rh as u64;
    if rcv_area >= src_area {
        return None;
    }
    // 0.0 < ratio < 1.0; multiply, round-half-away-from-zero.
    let pct = 100.0 * (1.0 - (rcv_area as f64 / src_area as f64));
    let pct_rounded = pct.round() as u32;
    if pct_rounded == 0 {
        None
    } else {
        Some(pct_rounded.min(100))
    }
}

/// Pick the tooltip color for a given downscale percentage.
/// Severity buckets follow the user-facing copy in the legend help text:
///   - ≥50% pixel-area loss is severe (danger).
///   - 25-49% is moderate (warning).
///   - <25% is mild (muted).
fn screen_downscale_color(pct: u32) -> &'static str {
    if pct >= 50 {
        theme_color::ERROR_TEXT
    } else if pct >= 25 {
        theme_color::WARNING_TEXT
    } else {
        theme_color::TEXT_MUTED
    }
}

/// Render the Screen-share tooltip line. Pulled out so the per-arm formatting
/// stays readable and so unit tests can exercise it without going through the
/// DOM. Returns an empty string when the screen share isn't active or the
/// caller has disabled the screen series.
///
/// Shape rules (post-#903 tightening — drop the colon after `Screen`, use // @token-exempt: issue ref, not a color
/// middle-dot separators, join units to numbers, abbreviate tier names):
///   - Received unknown → `Screen · Nfps · Mkbps` (legacy fallback).
///   - Source unknown or Source == Received → `Screen WxH (tier) · Nfps · Mkbps`.
///   - Source != Received → `Screen AxB → CxD ↓P%` plus `· Nfps · Mkbps`.
///     The `Source` / `Received` labels are dropped (the arrow already conveys
///     direction); tier names are dropped from the expanded form because the
///     resolution numbers are what matters when comparing the two; the
///     `pixel area` suffix is dropped because the `%` already implies it.
fn build_screen_tooltip_line(sample: &SignalSample, show_screen: bool) -> String {
    if !show_screen || !sample.screen_enabled {
        return String::new();
    }

    // Compact metrics tail used by every branch. `·` (U+00B7 MIDDLE DOT)
    // replaces the previous `|` pipe so the row reads less like a CSV.
    // No space between number and unit (`850kbps`, `12.5fps`) — the user's
    // tightening spec called this out explicitly.
    let metrics_suffix = format!(
        " \u{00B7} {:.1}fps \u{00B7} {:.0}kbps",
        sample.screen_fps, sample.screen_bitrate_kbps
    );

    let received_known = !sample.screen_resolution.is_empty();
    let source_known = !sample.screen_source_resolution.is_empty();
    let source_equals_received = sample.screen_source_resolution == sample.screen_resolution;

    if !received_known {
        // Nothing to attribute. Same single-line shape used by older clients
        // before #883 introduced received-resolution tracking. // @token-exempt: issue ref, not a color
        return format!(
            "<span style='color:{}'>Screen{}</span>",
            theme_color::SIGNAL_SCREEN,
            metrics_suffix
        );
    }

    if !source_known || source_equals_received {
        // Either the publisher is older / doesn't report a source dimension,
        // or the encoder hit no tier constraint and shipped the native size.
        // In both cases collapse to a single value — there is nothing to
        // compare against.
        let tier = infer_video_tier_short(&sample.screen_resolution);
        return if tier.is_empty() {
            format!(
                "<span style='color:{}'>Screen {}{}</span>",
                theme_color::SIGNAL_SCREEN,
                sample.screen_resolution,
                metrics_suffix
            )
        } else {
            format!(
                "<span style='color:{}'>Screen {} ({}){}</span>",
                theme_color::SIGNAL_SCREEN,
                sample.screen_resolution,
                tier,
                metrics_suffix
            )
        };
    }

    // Source != Received. Show both with the arrow separator so it's
    // immediately legible that downscaling happened. Tier names are dropped
    // from the expanded form per the #903 tightening — the resolution // @token-exempt: issue ref, not a color
    // numbers are the comparison data, the tier names add noise.

    // Optional degradation badge when the encoder downscaled in transit.
    // U+2193 DOWNWARDS ARROW is the icon, the pct is bucketed for severity.
    // We drop the previous " pixel area" suffix — the % already implies
    // it and the row is tight enough as-is.
    let badge = if let Some(pct) =
        screen_downscale_percent(&sample.screen_source_resolution, &sample.screen_resolution)
    {
        format!(
            " <span style='color:{}'>\u{2193}{}%</span>",
            screen_downscale_color(pct),
            pct
        )
    } else {
        String::new()
    };

    format!(
        "<span style='color:{}'>Screen {} \u{2192} {}</span>{}{}",
        theme_color::SIGNAL_SCREEN,
        sample.screen_source_resolution,
        sample.screen_resolution,
        badge,
        format_args!(
            "<span style='color:{}'>{}</span>",
            theme_color::SIGNAL_SCREEN,
            metrics_suffix
        )
    )
}

/// Render the optional **Cause** sub-line that explains *why* the publisher's
/// encoder downscaled in transit (issue #903). The line is sourced from the // @token-exempt: issue ref, not a color
/// publisher-stamped `VideoMetadata` fields
/// `encoder_target_bitrate_kbps` / `adaptive_tier` / `cause_hint` and renders
/// in one of these compact shapes (post-#903 tightening — drop "encoder // @token-exempt: issue ref, not a color
/// target", "limited by", "adaptive-quality"; join units to numbers; use
/// middle-dot separators):
///
///   1. **Combined** — all three present:
///      `Cause: <cause_hint> · <N>kbps · tier '<tier>'`
///   2. **Primary** — bitrate + tier present:
///      `Cause: <N>kbps · tier '<tier>'`
///   3. **Hint-only fallback** — only `cause_hint`:
///      `Cause: <cause_hint>`
///
/// `tier` is preserved as a literal word because users may not recognise a
/// bare `'low'` / `'medium'` / `'high'` label without that cue.
///
/// Returns an empty string when:
/// * the screen series is hidden or `screen_enabled` is false, OR
/// * all three publisher-stamped fields are zero / empty (older publishers
///   that don't supply cause data, or the unconstrained-tier path).
///
/// The empty-string return is load-bearing: the tooltip render loop omits
/// the line entirely when this helper returns empty, so older publishers
/// never see a placeholder shipped in their UI. See the unit tests below.
fn build_screen_cause_line(sample: &SignalSample, show_screen: bool) -> String {
    if !show_screen || !sample.screen_enabled {
        return String::new();
    }

    let bitrate = sample.screen_encoder_target_bitrate_kbps;
    let tier = sample.screen_adaptive_tier.trim();
    let hint = sample.screen_cause_hint.trim();
    let has_bitrate = bitrate > 0;
    let has_tier = !tier.is_empty();
    let has_hint = !hint.is_empty();

    // No data → no line. Older publishers, AQ at top tier, and zero-initialised
    // newer publishers all land here.
    if !has_bitrate && !has_tier && !has_hint {
        return String::new();
    }

    // Use U+00B7 MIDDLE DOT as the inline separator, matching the Screen
    // tooltip line's tightened style. Each branch builds the trailing
    // evidence list once with the same dot-joining rule.
    let body = match (has_bitrate, has_tier, has_hint) {
        // Combined: hint + bitrate + tier. Most informative — lead with
        // the hint summary, then dot-join the concrete signals.
        (true, true, true) => {
            format!("Cause: {hint} \u{00B7} {bitrate}kbps \u{00B7} tier '{tier}'")
        }
        // Primary: bitrate + tier without a hint.
        (true, true, false) => {
            format!("Cause: {bitrate}kbps \u{00B7} tier '{tier}'")
        }
        // Bitrate + hint (no tier).
        (true, false, true) => format!("Cause: {hint} \u{00B7} {bitrate}kbps"),
        // Tier + hint (no bitrate).
        (false, true, true) => format!("Cause: {hint} \u{00B7} tier '{tier}'"),
        // Single signal fallbacks.
        (true, false, false) => format!("Cause: {bitrate}kbps"),
        (false, true, false) => format!("Cause: tier '{tier}'"),
        (false, false, true) => format!("Cause: {hint}"),
        (false, false, false) => return String::new(),
    };

    format!(
        "<span style='color:{}'>{}</span>",
        theme_color::TEXT_MUTED,
        body
    )
}

/// Hide the global tooltip.
fn hide_body_tooltip() {
    if let Some(el) = gloo_utils::document().get_element_by_id("signal-chart-tooltip-global") {
        let html_el: web_sys::HtmlElement = el.unchecked_into();
        html_el.style().set_property("display", "none").unwrap();
    }
}

use wasm_bindgen::JsCast;

/// Popup overlay showing a scrollable SVG line chart of audio, video,
/// screen share quality, and latency.
#[component]
pub fn SignalQualityPopup(props: SignalQualityPopupProps) -> Element {
    let history = &props.history;
    let on_close = props.on_close;
    let popup_title = format!("Signal Quality - {}", props.peer_name);

    // Derive the transport badge tuple once, before rsx, so we don't pay
    // for repeated `String::as_str` / match work inside the rsx! macro
    // during re-renders. Mirrors the diagnostics popup pattern.
    let (transport_label, transport_class, transport_title) = match props.transport.as_deref() {
        Some("webtransport") => ("WT", "connection-type type-webtransport", "WebTransport"),
        Some("websocket") => ("WS", "connection-type type-websocket", "WebSocket"),
        _ => ("\u{2014}", "connection-type", "Transport unknown"),
    };

    // No Dioxus signal for tooltip — we manipulate a <body>-level DOM element
    // directly to escape all stacking contexts from grid-items.
    // Hide tooltip when this popup component unmounts.
    use_drop(hide_body_tooltip);

    // Which legend help text is currently expanded (if any).
    let mut help_visible = use_signal(|| None::<&'static str>);

    // Per-metric visibility toggles (all on by default).
    let mut show_audio = use_signal(|| true);
    let mut show_video = use_signal(|| true);
    let mut show_screen = use_signal(|| true);
    let mut show_latency = use_signal(|| true);

    // Unique scroll container ID so multiple popups don't collide.
    let scroll_id = format!("signal-chart-scroll-{}", props.peer_id);

    // Chart dimensions
    let chart_height: f64 = 180.0;
    let px_per_sec: f64 = 10.0;
    let visible_seconds: f64 = 30.0;
    let visible_width: f64 = visible_seconds * px_per_sec;
    let padding_top: f64 = 10.0;
    let padding_bottom: f64 = 20.0;
    let draw_height = chart_height - padding_top - padding_bottom;

    if history.is_empty() {
        return rsx! {
            div {
                class: "signal-quality-popup",
                div { class: "popup-header",
                    span { class: "popup-title", "{popup_title}" }
                    div { class: "popup-header-actions",
                        span {
                            class: "{transport_class}",
                            title: "{transport_title}",
                            "{transport_label}"
                        }
                        button {
                            class: "popup-close",
                            onclick: move |_| on_close.call(()),
                            "X"
                        }
                    }
                }
                p { style: "color: {theme_color::TEXT_SUBTLE}; font-size: 12px;", "No data yet." }
            }
        };
    }

    // X-axis origin is meeting start, not when the first sample was recorded.
    // This means all peers share the same time reference so charts are directly
    // comparable side-by-side.
    let first_ts = props.meeting_start_ms;
    let last_ts = history.last().map(|s| s.timestamp_ms).unwrap_or(first_ts);
    let total_seconds = ((last_ts - first_ts) / 1000.0).max(1.0);
    let chart_width = (total_seconds * px_per_sec).max(visible_width) + 10.0;

    // Determine if any sample has screen share enabled
    let has_screen_data = history.iter().any(|s| s.screen_enabled);

    // Determine max latency for the right y-axis scale
    let max_latency = history
        .iter()
        .map(|s| s.latency_ms)
        .fold(0.0_f64, f64::max)
        .max(10.0); // At least 10ms scale so labels are meaningful
                    // Round up to a nice number for the axis
    let max_latency_axis = nice_ceil(max_latency);

    // Build polyline points for quality lines (left y-axis, 0-100%)
    let audio_points: String = build_quality_polyline(
        history,
        first_ts,
        px_per_sec,
        padding_top,
        draw_height,
        |s| s.audio_quality,
    );
    let video_points: String = build_quality_polyline(
        history,
        first_ts,
        px_per_sec,
        padding_top,
        draw_height,
        |s| s.video_quality,
    );
    let screen_points: String = if has_screen_data {
        build_quality_polyline(
            history,
            first_ts,
            px_per_sec,
            padding_top,
            draw_height,
            |s| s.screen_quality,
        )
    } else {
        String::new()
    };

    // Build polyline points for latency (right y-axis, 0-max_latency ms)
    let latency_points: String = history
        .iter()
        .map(|s| {
            let x = ((s.timestamp_ms - first_ts) / 1000.0) * px_per_sec;
            let normalized = (s.latency_ms / max_latency_axis).clamp(0.0, 1.0);
            let y = padding_top + draw_height * (1.0 - normalized);
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Y-axis labels (left side -- quality)
    let y_labels: Vec<(&str, f64)> = vec![
        ("100%", padding_top),
        ("50%", padding_top + draw_height * 0.5),
        ("0%", padding_top + draw_height),
    ];

    // Y-axis labels (right side -- latency)
    let max_latency_str = format!("{}ms", max_latency_axis as u32);
    let mid_latency_str = format!("{}ms", (max_latency_axis / 2.0) as u32);
    let mid_latency_y = padding_top + draw_height * 0.5;
    let bottom_latency_y = padding_top + draw_height;

    // X-axis tick labels (every 10 seconds)
    let tick_interval = 10.0_f64;
    let num_ticks = (total_seconds / tick_interval).ceil() as usize + 1;

    let chart_width_px = format!("{chart_width:.0}px");
    let chart_height_str = format!("{chart_height:.0}");
    let chart_width_str = format!("{chart_width:.0}");
    let visible_width_style = format!("max-width: {visible_width:.0}px;");

    // Grid lines span the full chart width
    let grid_lines: Vec<f64> = y_labels.iter().map(|(_, y)| *y).collect();

    // Auto-scroll only when the user is already viewing the latest data.
    // If the scrollbar is pulled back to inspect older data, don't jump
    // to the end — that would be disruptive. Only auto-scroll when the
    // scroll position is at (or within 20px of) the right edge.
    let scroll_id_for_scroll = scroll_id.clone();
    spawn(async move {
        TimeoutFuture::new(0).await;
        if let Some(el) = gloo_utils::document().get_element_by_id(&scroll_id_for_scroll) {
            let at_end = el.scroll_left() + el.client_width() >= el.scroll_width() - 20;
            if at_end {
                el.set_scroll_left(el.scroll_width());
            }
        }
    });

    rsx! {
        div {
            class: "signal-quality-popup",
            // Stop clicks inside the popup from bubbling to tile handlers.
            onclick: move |e| e.stop_propagation(),
            div { class: "popup-header",
                span { class: "popup-title", "{popup_title}" }
                div { class: "popup-header-actions",
                    span {
                        class: "{transport_class}",
                        title: "{transport_title}",
                        "{transport_label}"
                    }
                    button {
                        class: "popup-close",
                        onclick: move |_| on_close.call(()),
                        "X"
                    }
                }
            }
            div { class: "signal-chart-wrapper",
                // Fixed Y-axis overlay (left side -- quality %)
                svg {
                    class: "signal-chart-y-axis",
                    width: "30",
                    height: "{chart_height_str}",
                    view_box: "0 0 30 {chart_height_str}",
                    for (label, y) in y_labels.iter() {
                        text {
                            x: "28",
                            y: "{y}",
                            fill: "{theme_color::TEXT_SUBTLE}",
                            font_size: "9",
                            text_anchor: "end",
                            dominant_baseline: "middle",
                            "{label}"
                        }
                    }
                }
                // Scrollable chart area
                div {
                    class: "signal-chart-scroll",
                    id: "{scroll_id}",
                    style: "{visible_width_style}",
                    onscroll: {
                        let scroll_id = scroll_id.clone();
                        move |_| {
                            let doc = gloo_utils::document();
                            if let Some(src) = doc.get_element_by_id(&scroll_id) {
                                let scroll_left = src.scroll_left();
                                let els = doc.get_elements_by_class_name("signal-chart-scroll");
                                for i in 0..els.length() {
                                    if let Some(el) = els.item(i) {
                                        if el.id() != scroll_id {
                                            el.set_scroll_left(scroll_left);
                                        }
                                    }
                                }
                            }
                        }
                    },
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "{chart_width_px}",
                        height: "{chart_height_str}",
                        view_box: "0 0 {chart_width_str} {chart_height_str}",
                        // Grid lines
                        for grid_y in grid_lines.iter() {
                            line {
                                x1: "0",
                                y1: "{grid_y}",
                                x2: "{chart_width_str}",
                                y2: "{grid_y}",
                                stroke: "{theme_color::SIGNAL_GRID_MAJOR}",
                                stroke_width: "0.5",
                            }
                        }
                        // X-axis ticks
                        for tick_i in 0..num_ticks {
                            {
                                let t = tick_i as f64 * tick_interval;
                                let x = t * px_per_sec;
                                let mins = (t / 60.0).floor() as u32;
                                let secs = (t % 60.0).floor() as u32;
                                let label = if mins > 0 {
                                    format!("{mins}m{secs:02}s")
                                } else {
                                    format!("{secs}s")
                                };
                                let y_bottom = padding_top + draw_height;
                                rsx! {
                                    line {
                                        x1: "{x}",
                                        y1: "{padding_top}",
                                        x2: "{x}",
                                        y2: "{y_bottom}",
                                        stroke: "{theme_color::SIGNAL_GRID_MINOR}",
                                        stroke_width: "0.5",
                                    }
                                    text {
                                        x: "{x}",
                                        y: "{chart_height_str}",
                                        fill: "{theme_color::TEXT_SUBTLE}",
                                        font_size: "8",
                                        text_anchor: "middle",
                                        "{label}"
                                    }
                                }
                            }
                        }
                        // Audio polyline
                        if show_audio() {
                            polyline {
                                points: "{audio_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_AUDIO}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Video polyline
                        if show_video() {
                            polyline {
                                points: "{video_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_VIDEO}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Screen share polyline (only when data exists and enabled)
                        if has_screen_data && show_screen() {
                            polyline {
                                points: "{screen_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_SCREEN}",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Latency polyline
                        if show_latency() {
                            polyline {
                                points: "{latency_points}",
                                fill: "none",
                                stroke: "{theme_color::SIGNAL_LATENCY_DIM}",
                                stroke_width: "1",
                                stroke_linejoin: "round",
                                stroke_dasharray: "3 6",
                            }
                        }
                    }
                    // HTML overlay for tooltip interaction (more reliable than SVG rect in WASM).
                    // Positioned over the SVG chart area, same dimensions.
                    {
                        let first_ts_for_move = first_ts;
                        let history_for_move = history.clone();
                        let overlay_style = format!(
                            "position: absolute; top: {padding_top}px; left: 0; \
                             width: {chart_width:.0}px; height: {draw_height:.0}px; \
                             cursor: crosshair;"
                        );
                        rsx! {
                            div {
                                style: "{overlay_style}",
                                onmousemove: move |evt: MouseEvent| {
                                    let v_audio = show_audio();
                                    let v_video = show_video();
                                    let v_screen = show_screen();
                                    let v_latency = show_latency();
                                    let client = evt.client_coordinates();
                                    let elem = evt.element_coordinates();
                                    let time_offset_sec = elem.x / px_per_sec;
                                    let target_ts = first_ts_for_move + time_offset_sec * 1000.0;
                                    let idx = history_for_move
                                        .binary_search_by(|s| {
                                            s.timestamp_ms.partial_cmp(&target_ts)
                                                .unwrap_or(std::cmp::Ordering::Equal)
                                        })
                                        .unwrap_or_else(|i| i);
                                    let sample = [idx.saturating_sub(1), idx.min(history_for_move.len().saturating_sub(1))]
                                        .iter()
                                        .filter_map(|&i| history_for_move.get(i))
                                        .min_by(|a, b| {
                                            let da = (a.timestamp_ms - target_ts).abs();
                                            let db = (b.timestamp_ms - target_ts).abs();
                                            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                                        });
                                    if let Some(sample) = sample {
                                        let elapsed_secs = (sample.timestamp_ms - first_ts_for_move) / 1000.0;
                                        let mins = (elapsed_secs / 60.0).floor() as u32;
                                        let secs = (elapsed_secs % 60.0).floor() as u32;
                                        let time_str = format!("{mins}m {secs:02}s");
                                        show_body_tooltip(
                                            client.x + 12.0,
                                            client.y - 10.0,
                                            &time_str,
                                            sample,
                                            v_video,
                                            v_audio,
                                            v_screen,
                                            v_latency,
                                        );
                                    }
                                },
                                onmouseleave: move |_| {
                                    hide_body_tooltip();
                                },
                            }
                        }
                    }
                }
                // Fixed Y-axis overlay (right side -- latency)
                svg {
                    class: "signal-chart-y-axis-right",
                    width: "35",
                    height: "{chart_height_str}",
                    view_box: "0 0 35 {chart_height_str}",
                    text {
                        x: "2",
                        y: "{padding_top}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{max_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{mid_latency_y}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{mid_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{bottom_latency_y}",
                        fill: "{theme_color::TEXT_SUBTLE}",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "0ms"
                    }
                }
            }
            // Tooltip is rendered directly on <body> via show_body_tooltip/hide_body_tooltip
            // to escape all grid-item stacking contexts.
            // Legend with visibility checkboxes
            div { class: "signal-chart-legend",
                label { class: "legend-item",
                    input {
                        r#type: "checkbox",
                        checked: show_audio(),
                        onchange: move |_| show_audio.set(!show_audio()),
                    }
                    span { class: "dot", style: "background: {theme_color::SIGNAL_AUDIO};" }
                    "Audio"
                    button {
                        class: "legend-help-btn",
                        onclick: move |_| {
                            let current = help_visible();
                            if current == Some("audio") {
                                help_visible.set(None);
                            } else {
                                help_visible.set(Some("audio"));
                            }
                        },
                        "?"
                    }
                }
                label { class: "legend-item",
                    input {
                        r#type: "checkbox",
                        checked: show_video(),
                        onchange: move |_| show_video.set(!show_video()),
                    }
                    span { class: "dot", style: "background: {theme_color::SIGNAL_VIDEO};" }
                    "Video"
                    button {
                        class: "legend-help-btn",
                        onclick: move |_| {
                            let current = help_visible();
                            if current == Some("video") {
                                help_visible.set(None);
                            } else {
                                help_visible.set(Some("video"));
                            }
                        },
                        "?"
                    }
                }
                if has_screen_data {
                    label { class: "legend-item",
                        input {
                            r#type: "checkbox",
                            checked: show_screen(),
                            onchange: move |_| show_screen.set(!show_screen()),
                        }
                        span { class: "dot", style: "background: {theme_color::SIGNAL_SCREEN};" }
                        "Screen"
                        button {
                            class: "legend-help-btn",
                            onclick: move |_| {
                                let current = help_visible();
                                if current == Some("screen") {
                                    help_visible.set(None);
                                } else {
                                    help_visible.set(Some("screen"));
                                }
                            },
                            "?"
                        }
                    }
                }
                label { class: "legend-item",
                    input {
                        r#type: "checkbox",
                        checked: show_latency(),
                        onchange: move |_| show_latency.set(!show_latency()),
                    }
                    span { class: "dot", style: "background: {theme_color::SIGNAL_LATENCY};" }
                    "Server RTT"
                    button {
                        class: "legend-help-btn",
                        onclick: move |_| {
                            let current = help_visible();
                            if current == Some("latency") {
                                help_visible.set(None);
                            } else {
                                help_visible.set(Some("latency"));
                            }
                        },
                        "?"
                    }
                }
            }
            // Legend help text (shown when a "?" button is clicked)
            if let Some(topic) = help_visible() {
                div { class: "legend-help-text",
                    match topic {
                        "audio" => rsx! {
                            strong { "Audio Quality" }
                            p { "Composite score from two metrics shown in the tooltip:" }
                            p {
                                strong { "Buffer (buf ms): " }
                                "How much audio is queued for playback. "
                                "20\u{2013}80ms is ideal. Below 20ms risks glitches; above 150ms means network congestion."
                            }
                            p {
                                strong { "Expand Rate (expand \u{2030}): " }
                                "How much audio the system had to synthesize due to missing packets. "
                                "0\u{2030} is perfect. Above 50\u{2030} you may hear artifacts; above 200\u{2030} is noticeable dropout."
                            }
                        },
                        "video" => rsx! {
                            strong { "Video Quality" }
                            p { "Based on received frames per second (FPS) relative to a 30fps target." }
                            p {
                                strong { "Resolution: " }
                                "The dimensions of the video being received (e.g., 1280x720)."
                            }
                            p {
                                strong { "FPS: " }
                                "Frames per second being received. Higher is smoother. "
                                "Below 15fps the video looks choppy."
                            }
                            p {
                                strong { "Bitrate (kbps): " }
                                "Data rate of the video stream. Higher bitrate generally means better picture quality."
                            }
                        },
                        "screen" => rsx! {
                            strong { "Screen Share Quality" }
                            p { "Based on received FPS for the shared screen content." }
                            p {
                                strong { "Source vs Received resolution: " }
                                "Source is the publisher's native capture resolution (their monitor / window). "
                                "Received is what your client decoded. A gap between the two means the publisher's "
                                "encoder downscaled the content in transit — usually because the network or CPU "
                                "couldn't sustain a full-resolution stream."
                            }
                            p {
                                strong { "↓ Pixel-area badge: " }
                                "Quantifies how much detail was lost in transit. A 2× linear downscale (e.g., 1080p \u{2192} 540p) "
                                "drops 75% of the pixels, which is what the badge reports. \u{2265}50% is shown in red, 25\u{2013}49% in amber, "
                                "<25% in muted text."
                            }
                            p {
                                strong { "Cause: " }
                                "Sub-line shown below the Screen row when the publisher's encoder "
                                "reports it is constrained. Sourced from the publisher's adaptive-"
                                "quality system: the encoder's current target bitrate, the tier "
                                "actively limiting it (e.g. 'low' / 'medium'), and a short cause "
                                "classifier (bitrate-limited, cpu-pressure, network-rtt, "
                                "network-loss, manual-cap). The line is omitted when the "
                                "publisher is unconstrained or is an older client that doesn't "
                                "report this data."
                            }
                            p {
                                strong { "FPS: " }
                                "Frames per second of the shared screen. Screen shares typically run at 5\u{2013}15fps."
                            }
                            p {
                                strong { "Bitrate (kbps): " }
                                "Data rate of the screen share stream. Combined with resolution this is the main driver of how sharp the shared content looks."
                            }
                        },
                        "latency" => rsx! {
                            strong { "Server RTT" }
                            p { "Round-trip time from your device to the relay server and back." }
                            p { "This is the same value for all peers in your session — it measures your connection to the relay, not end-to-end latency to each peer." }
                            p {
                                "Below 50ms is excellent. "
                                "50\u{2013}150ms is acceptable. "
                                "Above 200ms causes noticeable delay."
                            }
                        },
                        _ => rsx! {},
                    }
                }
            }
        }
    }
}

/// Build a polyline `points` string from history, mapping a quality accessor
/// to y-coordinates on the chart (0.0 at top = 100%, 1.0 at bottom = 0%).
fn build_quality_polyline(
    history: &[SignalSample],
    first_ts: f64,
    px_per_sec: f64,
    padding_top: f64,
    draw_height: f64,
    quality_fn: impl Fn(&SignalSample) -> f64,
) -> String {
    history
        .iter()
        .map(|s| {
            let x = ((s.timestamp_ms - first_ts) / 1000.0) * px_per_sec;
            let y = padding_top + draw_height * (1.0 - quality_fn(s));
            format!("{x:.1},{y:.1}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Round a value up to a "nice" number for axis labels.
fn nice_ceil(val: f64) -> f64 {
    if val <= 0.0 {
        return 10.0;
    }
    let magnitude = 10.0_f64.powf(val.log10().floor());
    let normalized = val / magnitude;
    let nice = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_level_from_quality_boundaries() {
        assert_eq!(SignalLevel::from_quality(1.0), SignalLevel::Excellent);
        assert_eq!(SignalLevel::from_quality(0.9), SignalLevel::Excellent);
        assert_eq!(SignalLevel::from_quality(0.89), SignalLevel::Good);
        assert_eq!(SignalLevel::from_quality(0.75), SignalLevel::Good);
        assert_eq!(SignalLevel::from_quality(0.74), SignalLevel::Fair);
        assert_eq!(SignalLevel::from_quality(0.5), SignalLevel::Fair);
        assert_eq!(SignalLevel::from_quality(0.49), SignalLevel::Poor);
        assert_eq!(SignalLevel::from_quality(0.25), SignalLevel::Poor);
        assert_eq!(SignalLevel::from_quality(0.24), SignalLevel::Bad);
        assert_eq!(SignalLevel::from_quality(0.01), SignalLevel::Bad);
        assert_eq!(SignalLevel::from_quality(0.0), SignalLevel::Lost);
    }

    #[test]
    fn signal_level_bars() {
        assert_eq!(SignalLevel::Excellent.bars(), 5);
        assert_eq!(SignalLevel::Lost.bars(), 0);
    }

    #[test]
    fn combined_quality_both() {
        let q = combined_quality(0.8, 0.6, 0.0, true, true, false);
        assert!((q - 0.7).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_audio_only() {
        let q = combined_quality(0.8, 0.0, 0.0, true, false, false);
        assert!((q - 0.8).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_neither() {
        let q = combined_quality(0.0, 0.0, 0.0, false, false, false);
        assert!((q - 1.0).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_all_three() {
        let q = combined_quality(0.9, 0.6, 0.3, true, true, true);
        assert!((q - 0.6).abs() < 1e-9);
    }

    #[test]
    fn combined_quality_screen_only() {
        let q = combined_quality(0.0, 0.0, 0.7, false, false, true);
        assert!((q - 0.7).abs() < 1e-9);
    }

    #[test]
    fn push_sample_records_screen_resolution() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            video_fps: 30.0,
            video_bitrate_kbps: 800.0,
            video_resolution: "1280x720".to_string(),
            audio_bitrate_kbps: 64.0,
            audio_expand_rate: 0.0,
            audio_buffer_ms: 60.0,
            screen_enabled: true,
            screen_fps: 15.0,
            screen_bitrate_kbps: 1200.0,
            screen_resolution: "1920x1080".to_string(),
            screen_source_resolution: "1920x1080".to_string(),
            screen_encoder_target_bitrate_kbps: 0,
            screen_adaptive_tier: String::new(),
            screen_cause_hint: String::new(),
            latency_ms: 40.0,
            audio_enabled: true,
            video_enabled: true,
        };
        history.push_sample_at(&data, 1_000.0);

        let samples = history.samples_vec();
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!(s.screen_resolution, "1920x1080");
        assert_eq!(s.screen_source_resolution, "1920x1080");
        assert!(s.screen_enabled);
        assert!((s.screen_fps - 15.0).abs() < 1e-9);
        // Screen quality is fps / 30 when enabled.
        assert!((s.screen_quality - 0.5).abs() < 1e-9);
    }

    #[test]
    fn push_sample_screen_quality_zero_when_disabled() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: false,
            screen_fps: 15.0,
            screen_bitrate_kbps: 1200.0,
            screen_resolution: String::new(),
            screen_source_resolution: String::new(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 2_000.0);
        let samples = history.samples_vec();
        assert_eq!(samples[0].screen_quality, 0.0);
        assert_eq!(samples[0].screen_resolution, "");
        assert_eq!(samples[0].screen_source_resolution, "");
    }

    #[test]
    fn infer_video_tier_classifies_screen_resolutions() {
        // Long-form tier name is used by the camera-video line and as the
        // input to the short-form lookup below.
        assert_eq!(infer_video_tier("1920x1080"), "Full HD");
        assert_eq!(infer_video_tier("1280x720"), "HD");
        assert_eq!(infer_video_tier("640x480"), "Medium");
        assert_eq!(infer_video_tier(""), "");
        assert_eq!(infer_video_tier("garbage"), "");
    }

    #[test]
    fn infer_video_tier_short_abbreviates_common_labels() {
        // Screen tooltip uses the abbreviated form so the row stays narrow.
        // FHD / QHD / UHD are the only abbreviations applied; HD and the
        // lower tiers are already compact and keep their long form.
        assert_eq!(infer_video_tier_short("1920x1080"), "FHD");
        assert_eq!(infer_video_tier_short("1280x720"), "HD");
        assert_eq!(infer_video_tier_short("960x540"), "Standard");
        assert_eq!(infer_video_tier_short("640x480"), "Medium");
        // Unknown / empty inputs pass through.
        assert_eq!(infer_video_tier_short(""), "");
        assert_eq!(infer_video_tier_short("garbage"), "");
    }

    // -----------------------------------------------------------------
    // Source vs received tooltip behavior. These tests exercise the pure
    // formatter, so we can drive them through host `cargo test` without
    // any browser / DOM dependency.
    // -----------------------------------------------------------------

    fn screen_sample(received: &str, source: &str) -> SignalSample {
        SignalSample {
            timestamp_ms: 0.0,
            audio_quality: 0.0,
            video_quality: 0.0,
            screen_quality: 0.5,
            video_fps: 0.0,
            video_bitrate_kbps: 0.0,
            video_resolution: String::new(),
            audio_bitrate_kbps: 0.0,
            audio_expand_rate: 0.0,
            audio_buffer_ms: 0.0,
            screen_enabled: true,
            screen_fps: 8.0,
            screen_bitrate_kbps: 720.0,
            screen_resolution: received.to_string(),
            screen_source_resolution: source.to_string(),
            screen_encoder_target_bitrate_kbps: 0,
            screen_adaptive_tier: String::new(),
            screen_cause_hint: String::new(),
            latency_ms: 0.0,
        }
    }

    #[test]
    fn screen_downscale_percent_pixel_area_math() {
        // 2× linear downscale -> 75% pixel-area loss.
        assert_eq!(screen_downscale_percent("2560x1440", "1280x720"), Some(75));
        // 1.5× linear-ish (1920x1080 -> 1280x720) -> ~55.6% rounded to 56.
        assert_eq!(screen_downscale_percent("1920x1080", "1280x720"), Some(56));
        // Source == Received -> no badge.
        assert_eq!(screen_downscale_percent("1920x1080", "1920x1080"), None);
        // Received >= Source area (no downscale) -> no badge.
        assert_eq!(screen_downscale_percent("1280x720", "1920x1080"), None);
        // Source unknown -> no badge.
        assert_eq!(screen_downscale_percent("", "1920x1080"), None);
        // Received unknown -> no badge.
        assert_eq!(screen_downscale_percent("1920x1080", ""), None);
        // Sub-1% loss rounds to 0 and returns None to avoid "↓0%" noise.
        // 1920x1080 -> 1920x1079 = 0.09% area loss -> None.
        assert_eq!(screen_downscale_percent("1920x1080", "1920x1079"), None);
        // Unparseable input -> no badge.
        assert_eq!(screen_downscale_percent("garbage", "1920x1080"), None);
    }

    #[test]
    fn screen_downscale_color_severity_buckets() {
        // <25% -> muted text.
        assert_eq!(screen_downscale_color(0), theme_color::TEXT_MUTED);
        assert_eq!(screen_downscale_color(24), theme_color::TEXT_MUTED);
        // 25-49% -> amber warning.
        assert_eq!(screen_downscale_color(25), theme_color::WARNING_TEXT);
        assert_eq!(screen_downscale_color(49), theme_color::WARNING_TEXT);
        // >=50% -> danger.
        assert_eq!(screen_downscale_color(50), theme_color::ERROR_TEXT);
        assert_eq!(screen_downscale_color(75), theme_color::ERROR_TEXT);
        assert_eq!(screen_downscale_color(100), theme_color::ERROR_TEXT);
    }

    #[test]
    fn tooltip_collapses_when_source_equals_received() {
        // Post-#903 tightening: `Screen ` (no colon), abbreviated tier name, // @token-exempt: issue ref, not a color
        // middle-dot separators, joined units. // @token-exempt: issue ref, not a color
        let s = screen_sample("1920x1080", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains("Screen 1920x1080"));
        assert!(line.contains("(FHD)"));
        // No legacy noise.
        assert!(!line.contains("Screen:"));
        assert!(!line.contains("(Full HD)"));
        assert!(!line.contains(" | "));
        // Single value — no arrow, no badge.
        assert!(!line.contains("Source"));
        assert!(!line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
    }

    #[test]
    fn tooltip_shows_arrow_when_source_differs_from_received() {
        let s = screen_sample("1280x720", "2560x1440");
        let line = build_screen_tooltip_line(&s, true);
        // Post-tightening: bare numbers + arrow, no "Source" / "Received"
        // labels, no tier names in the expanded form.
        assert!(line.contains("Screen 2560x1440 \u{2192} 1280x720"));
        assert!(!line.contains("Source 2560x1440"));
        assert!(!line.contains("Received 1280x720"));
        assert!(!line.contains("(HD)"));
        assert!(!line.contains("(Full HD)"));
        // 2560x1440 -> 1280x720 = 75% pixel-area loss, danger color.
        // Compact badge: no "pixel area" suffix.
        assert!(line.contains("\u{2193}75%"));
        assert!(!line.contains("pixel area"));
        assert!(line.contains(theme_color::ERROR_TEXT));
    }

    #[test]
    fn tooltip_uses_warning_color_for_moderate_downscale() {
        // 1920x1080 -> 1600x900 = 30.6% loss -> warning. Compact badge.
        let s = screen_sample("1600x900", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains(theme_color::WARNING_TEXT));
        assert!(line.contains("\u{2193}31%"));
        assert!(!line.contains("pixel area"));
    }

    #[test]
    fn tooltip_uses_muted_color_for_minor_downscale() {
        // 1920x1080 -> 1820x1024 = ~10.1% loss -> muted.
        let s = screen_sample("1820x1024", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.contains(theme_color::TEXT_MUTED));
        assert!(line.contains("\u{2193}"));
        assert!(!line.contains("pixel area"));
    }

    #[test]
    fn tooltip_received_only_when_source_unknown() {
        let s = screen_sample("1280x720", "");
        let line = build_screen_tooltip_line(&s, true);
        // Source unknown collapses to the single-value shape; HD stays
        // long-form because it has no widely-known shorter abbreviation.
        assert!(line.contains("Screen 1280x720"));
        assert!(line.contains("(HD)"));
        assert!(!line.contains("Screen:"));
        // Older publisher -> no arrow, no badge.
        assert!(!line.contains("Source"));
        assert!(!line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
    }

    #[test]
    fn tooltip_legacy_shape_when_received_unknown() {
        // Both unknown -> fall back to no-resolution shape (pre-#891 baseline). // @token-exempt: issue ref, not a color
        // Post-tightening: `Screen` with no colon, dot-joined metrics tail.
        let s = screen_sample("", "");
        let line = build_screen_tooltip_line(&s, true);
        assert!(line.starts_with("<span"));
        assert!(line.contains("Screen"));
        assert!(!line.contains("Screen:"));
        // Compact units: `8.0fps`, `720kbps` (no space).
        assert!(line.contains("8.0fps"));
        assert!(line.contains("720kbps"));
        assert!(!line.contains("Source"));
        assert!(!line.contains("Received"));
    }

    #[test]
    fn tooltip_empty_when_screen_disabled() {
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_enabled = false;
        assert_eq!(build_screen_tooltip_line(&s, true), "");
    }

    #[test]
    fn tooltip_metrics_suffix_always_present() {
        // All three branches use the dot-joined compact metrics tail.
        for (recv, src) in [
            ("1280x720", "1280x720"),
            ("1280x720", "1920x1080"),
            ("1280x720", ""),
        ] {
            let s = screen_sample(recv, src);
            let line = build_screen_tooltip_line(&s, true);
            assert!(line.contains("8.0fps"), "missing 8.0fps in {line}");
            assert!(line.contains("720kbps"), "missing 720kbps in {line}");
            assert!(
                line.contains("\u{00B7}"),
                "missing middle-dot separator in {line}"
            );
        }
    }

    #[test]
    fn tooltip_drops_badge_at_or_below_zero_rounded() {
        // <0.5% downscale rounds to 0 — we must NOT render "↓0%".
        let s = screen_sample("1919x1080", "1920x1080");
        let line = build_screen_tooltip_line(&s, true);
        // The two resolutions differ as strings so the expanded shape
        // still fires (we get the arrow), but no badge because the area
        // delta rounds to 0.
        assert!(line.contains("\u{2192}"));
        assert!(!line.contains("\u{2193}"));
        assert!(!line.contains("pixel area"));
    }

    // -----------------------------------------------------------------
    // Issue #903: Cause line rendering. Sourced from publisher-stamped // @token-exempt: issue ref, not a color
    // `VideoMetadata` fields. Post-tightening copy is compact:
    //   * No data → empty (older publisher or unconstrained tier).
    //   * Bitrate + tier → `Cause: <N>kbps · tier '<tier>'`.
    //   * Cause hint only → `Cause: <hint>`.
    //   * All three → `Cause: <hint> · <N>kbps · tier '<tier>'`.
    // The Screen line wrapper drops the row entirely when the helper
    // returns an empty string, so it is load-bearing that `""` and
    // not a placeholder is returned for the no-data cases.
    // -----------------------------------------------------------------

    #[test]
    fn cause_line_empty_when_no_publisher_data() {
        // Older publisher / unconstrained AQ tier: all three fields
        // arrive as proto3 defaults. We MUST omit the line — shipping
        // "not yet instrumented" or any placeholder regressed in #891. // @token-exempt: issue ref, not a color
        let s = screen_sample("1280x720", "2560x1440");
        assert_eq!(s.screen_encoder_target_bitrate_kbps, 0);
        assert!(s.screen_adaptive_tier.is_empty());
        assert!(s.screen_cause_hint.is_empty());
        assert_eq!(build_screen_cause_line(&s, true), "");
    }

    #[test]
    fn cause_line_primary_shape_with_bitrate_and_tier() {
        // Bitrate + tier with no hint → compact "Cause: 800kbps · tier 'low'".
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(line.contains("Cause: 800kbps \u{00B7} tier 'low'"));
        // Wordy phrasing must be gone.
        assert!(!line.contains("encoder target"));
        assert!(!line.contains("limited by"));
        assert!(!line.contains("adaptive-quality"));
        assert!(line.contains(theme_color::TEXT_MUTED));
    }

    #[test]
    fn cause_line_hint_only_fallback() {
        // Older / partial publisher only stamps cause_hint. Compact
        // fallback: bare "Cause: <hint>" — no bitrate, no tier word.
        let mut s = screen_sample("1280x720", "1920x1080");
        s.screen_cause_hint = "cpu-pressure".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(line.contains("Cause: cpu-pressure"));
        assert!(!line.contains("kbps"));
        assert!(!line.contains("tier"));
        assert!(line.contains(theme_color::TEXT_MUTED));
    }

    #[test]
    fn cause_line_combined_shape_with_all_three() {
        // All three present → `Cause: <hint> · <N>kbps · tier '<tier>'`.
        // Hint leads as the human summary; the dot-joined evidence
        // follows for users who want the concrete numbers.
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_encoder_target_bitrate_kbps = 500;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "network-rtt".to_string();
        let line = build_screen_cause_line(&s, true);
        assert!(
            line.contains("Cause: network-rtt \u{00B7} 500kbps \u{00B7} tier 'low'"),
            "unexpected combined cause line: {line}"
        );
        // No legacy wordy phrasing.
        assert!(!line.contains("encoder target"));
        assert!(!line.contains("\u{2014}")); // em-dash gone, dot is the joiner
    }

    #[test]
    fn cause_line_empty_when_screen_disabled() {
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_enabled = false;
        // Even with publisher data, hide when screen series is off.
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "bitrate-limited".to_string();
        assert_eq!(build_screen_cause_line(&s, true), "");
    }

    #[test]
    fn cause_line_empty_when_screen_series_hidden() {
        let mut s = screen_sample("1280x720", "2560x1440");
        s.screen_encoder_target_bitrate_kbps = 800;
        s.screen_adaptive_tier = "low".to_string();
        s.screen_cause_hint = "bitrate-limited".to_string();
        assert_eq!(build_screen_cause_line(&s, false), "");
    }

    #[test]
    fn cause_line_no_placeholder_text() {
        // Regression test for the #891 lesson — earlier code shipped a // @token-exempt: issue ref, not a color
        // "not yet instrumented" placeholder when no publisher data was
        // available. The omit-line behaviour is the contract; verify the
        // helper never emits that placeholder for any of the no-data
        // configurations (resolution match, resolution mismatch, source
        // unknown).
        let cases = [
            ("1920x1080", "1920x1080"),
            ("1280x720", "2560x1440"),
            ("1280x720", ""),
            ("", ""),
        ];
        for (recv, src) in cases {
            let s = screen_sample(recv, src);
            let line = build_screen_cause_line(&s, true);
            assert!(!line.contains("not yet instrumented"));
            assert!(!line.contains("#903")); // @token-exempt: issue ref, not a color
        }
    }

    #[test]
    fn push_sample_carries_source_resolution_through_to_signal_sample() {
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: true,
            screen_fps: 10.0,
            screen_bitrate_kbps: 800.0,
            screen_resolution: "1280x720".to_string(),
            screen_source_resolution: "2560x1440".to_string(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 5_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_resolution, "1280x720");
        assert_eq!(s.screen_source_resolution, "2560x1440");
    }

    #[test]
    fn push_sample_carries_encoder_state_through_to_signal_sample() {
        // Issue #903: publisher-stamped encoder state must round-trip // @token-exempt: issue ref, not a color
        // through `SampleData` to `SignalSample` so the Cause line in
        // the tooltip has data to render. Regression guard against
        // forgetting to wire one of the three fields.
        let mut history = PeerSignalHistory::new();
        let data = SampleData {
            screen_enabled: true,
            screen_fps: 10.0,
            screen_bitrate_kbps: 800.0,
            screen_resolution: "1280x720".to_string(),
            screen_source_resolution: "2560x1440".to_string(),
            screen_encoder_target_bitrate_kbps: 500,
            screen_adaptive_tier: "low".to_string(),
            screen_cause_hint: "bitrate-limited".to_string(),
            video_enabled: true,
            audio_enabled: true,
            ..Default::default()
        };
        history.push_sample_at(&data, 6_000.0);
        let s = &history.samples_vec()[0];
        assert_eq!(s.screen_encoder_target_bitrate_kbps, 500);
        assert_eq!(s.screen_adaptive_tier, "low");
        assert_eq!(s.screen_cause_hint, "bitrate-limited");
    }

    #[test]
    fn nice_ceil_values() {
        assert!((nice_ceil(45.0) - 50.0).abs() < 1e-9);
        assert!((nice_ceil(150.0) - 200.0).abs() < 1e-9);
        assert!((nice_ceil(8.0) - 10.0).abs() < 1e-9);
        assert!((nice_ceil(0.0) - 10.0).abs() < 1e-9);
    }
}
