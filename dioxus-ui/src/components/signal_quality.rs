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
        let timestamp_ms = js_sys::Date::now();
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
fn show_body_tooltip(x: f64, y: f64, time_str: &str, sample: &SignalSample) {
    let el = get_or_create_tooltip_el();
    let style = el.style();
    style.set_property("left", &format!("{x:.0}px")).unwrap();
    style.set_property("top", &format!("{y:.0}px")).unwrap();
    style.set_property("display", "block").unwrap();

    let video_tier = infer_video_tier(&sample.video_resolution);
    let video_line = if sample.video_resolution.is_empty() {
        format!(
            "<span style='color:#81C784'>Video: {:.1} fps | {:.0} kbps</span>",
            sample.video_fps, sample.video_bitrate_kbps
        )
    } else if video_tier.is_empty() {
        format!(
            "<span style='color:#81C784'>Video: {} | {:.1} fps | {:.0} kbps</span>",
            sample.video_resolution, sample.video_fps, sample.video_bitrate_kbps
        )
    } else {
        format!(
            "<span style='color:#81C784'>Video: {} ({}) | {:.1} fps | {:.0} kbps</span>",
            sample.video_resolution, video_tier, sample.video_fps, sample.video_bitrate_kbps
        )
    };
    let audio_line = format!(
        "<span style='color:#4FC3F7'>Audio: buf {:.0}ms | expand {:.0}\u{2030}</span>",
        sample.audio_buffer_ms, sample.audio_expand_rate
    );
    let screen_line = if sample.screen_enabled {
        format!(
            "<br><span style='color:#CE93D8'>Screen: {:.1} fps | {:.0} kbps</span>",
            sample.screen_fps, sample.screen_bitrate_kbps
        )
    } else {
        String::new()
    };
    let latency_line = format!(
        "<span style='color:#FF8A65'>Latency: {:.0} ms</span>",
        sample.latency_ms
    );

    el.set_inner_html(&format!(
        "<div>Time: {time_str}</div>\
         <div style='border-bottom:1px solid rgba(255,255,255,0.15);margin:2px 0'></div>\
         <div>{video_line}</div>\
         <div>{audio_line}</div>\
         {screen_line}\
         <div>{latency_line}</div>"
    ));
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
                    button {
                        class: "popup-close",
                        onclick: move |_| on_close.call(()),
                        "X"
                    }
                }
                p { style: "color: #888; font-size: 12px;", "No data yet." }
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
                button {
                    class: "popup-close",
                    onclick: move |_| on_close.call(()),
                    "X"
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
                            fill: "#888",
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
                                stroke: "rgba(255,255,255,0.1)",
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
                                        y1: "{y_bottom}",
                                        x2: "{x}",
                                        y2: "{y_bottom:.0}",
                                        stroke: "rgba(255,255,255,0.15)",
                                        stroke_width: "0.5",
                                    }
                                    text {
                                        x: "{x}",
                                        y: "{chart_height_str}",
                                        fill: "#888",
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
                                stroke: "#4FC3F7",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Video polyline
                        if show_video() {
                            polyline {
                                points: "{video_points}",
                                fill: "none",
                                stroke: "#81C784",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Screen share polyline (only when data exists and enabled)
                        if has_screen_data && show_screen() {
                            polyline {
                                points: "{screen_points}",
                                fill: "none",
                                stroke: "#CE93D8",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                            }
                        }
                        // Latency polyline
                        if show_latency() {
                            polyline {
                                points: "{latency_points}",
                                fill: "none",
                                stroke: "#FF8A65",
                                stroke_width: "1.5",
                                stroke_linejoin: "round",
                                stroke_dasharray: "4 2",
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
                        fill: "#888",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{max_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{mid_latency_y}",
                        fill: "#888",
                        font_size: "9",
                        dominant_baseline: "middle",
                        "{mid_latency_str}"
                    }
                    text {
                        x: "2",
                        y: "{bottom_latency_y}",
                        fill: "#888",
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
                    span { class: "dot", style: "background: #4FC3F7;" }
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
                    span { class: "dot", style: "background: #81C784;" }
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
                        span { class: "dot", style: "background: #CE93D8;" }
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
                    span { class: "dot", style: "background: #FF8A65;" }
                    "Latency"
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
                                strong { "FPS: " }
                                "Frames per second of the shared screen. Screen shares typically run at 5\u{2013}15fps."
                            }
                            p {
                                strong { "Bitrate (kbps): " }
                                "Data rate of the screen share stream."
                            }
                        },
                        "latency" => rsx! {
                            strong { "Latency (RTT)" }
                            p { "Round-trip time from your device to the relay server and back." }
                            p {
                                "Below 50ms is excellent. "
                                "50\u{2013}150ms is acceptable. "
                                "Above 200ms causes noticeable delay in the conversation."
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
    fn nice_ceil_values() {
        assert!((nice_ceil(45.0) - 50.0).abs() < 1e-9);
        assert!((nice_ceil(150.0) - 200.0).abs() < 1e-9);
        assert!((nice_ceil(8.0) - 10.0).abs() < 1e-9);
        assert!((nice_ceil(0.0) - 10.0).abs() < 1e-9);
    }
}
