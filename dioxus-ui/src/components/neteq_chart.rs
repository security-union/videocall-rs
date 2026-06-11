use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
pub use neteq::NetEqStats as RawNetEqStats;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::rc::Rc;

use crate::theme::color as theme_color;

/// Hard per-peer retention cap for the scrollable NetEq time-series: 2 hours at
/// ≤1 sample/sec (owner decision 2). At ~64 B/sample this is ~460 KB/peer.
pub const NETEQ_SAMPLE_CAP: usize = 7200;

/// X-axis density for the scrollable charts: pixels per elapsed second. Mirrors
/// the signal-quality popup idiom (`px_per_sec`).
pub const NETEQ_PX_PER_SEC: f64 = 8.0;

/// Minimum chart viewport width (px) so a short meeting's chart fills the drawer
/// before it needs to scroll. The growing chart width never drops below this
/// (`neteq_chart_width` clamps with `.max(NETEQ_MIN_CHART_WIDTH)`). Named so the
/// width math and its tests share one source of truth (no bare `600.0`). (#1223)
pub const NETEQ_MIN_CHART_WIDTH: f64 = 600.0;

/// Compact, parse-once NetEq sample stored in the per-peer ring buffer that
/// backs the scrollable charts AND the Current Status tiles (one storage, no
/// second path). Decoded from each incoming `stats_json` ONCE in the subscribe
/// loop (`NetEqSample::from_json`) so the render path never re-parses retained
/// JSON — the old O(n)-per-event `parse_neteq_stats_history` is gone (#1223).
///
/// ~64 B/sample → ~460 KB at the 2-hour cap, ~4.6 MB across 10 peers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetEqSample {
    /// Wall-clock-ish event timestamp (`DiagEvent::ts_ms`) — the REAL sample
    /// time, replacing the old hard-coded `timestamp: 0`. Drives the time-based
    /// X axis (`neteq_x`) and the growing chart width (`neteq_chart_width`).
    pub timestamp_ms: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub packets_awaiting_decode: u32,
    pub packets_per_sec: u32,
    // The five DecodeOperations series (the surviving decode-ops chart plots
    // exactly these five — see `ChartConfig::decode_operations`).
    pub normal_per_sec: f32,
    pub expand_per_sec: f32,
    pub accelerate_per_sec: f32,
    pub preemptive_expand_per_sec: f32,
    pub merge_per_sec: f32,
    /// Per-mille (‰) — the `From<RawNetEqStats>` impl converts the Q14 raw via
    /// `q14::to_per_mille`, and the status tiles render the ‰ unit. Kept in
    /// per-mille here so the tile read needs no further conversion.
    pub expand_rate: f32,
    /// Per-mille (‰), same convention as `expand_rate`.
    pub accel_rate: f32,
    /// Per-myriad (‱), straight from `reorder_rate_permyriad` (a `u16`).
    pub reorder_rate: u32,
    pub reordered_packets: u32,
    pub max_reorder_distance: u32,
}

impl NetEqSample {
    /// Parse one incoming `stats_json` line into a compact sample, ONCE, at
    /// arrival. `ts_ms` is the diag event's real `ts_ms`. Malformed JSON →
    /// `None` (no panic; a `log::warn!` records the parse error) so a single bad
    /// frame can't poison the ring buffer.
    pub fn from_json(json: &str, ts_ms: u64) -> Option<Self> {
        match serde_json::from_str::<RawNetEqStats>(json) {
            Ok(raw) => Some(Self::from_raw(raw, ts_ms)),
            Err(e) => {
                log::warn!("[NetEqSample::from_json] failed to parse stats_json: {e}");
                None
            }
        }
    }

    /// Map a decoded `RawNetEqStats` into the compact sample, applying the
    /// `q14::to_per_mille` conversion to the expand/accel rates so the tiles
    /// (which render the ‰ unit) read the stored value directly.
    fn from_raw(raw: RawNetEqStats, ts_ms: u64) -> Self {
        Self {
            timestamp_ms: ts_ms,
            buffer_ms: raw.current_buffer_size_ms,
            target_ms: raw.target_delay_ms,
            packets_awaiting_decode: raw.packets_awaiting_decode as u32,
            packets_per_sec: raw.packets_per_sec,
            normal_per_sec: raw.network.operation_counters.normal_per_sec,
            expand_per_sec: raw.network.operation_counters.expand_per_sec,
            accelerate_per_sec: raw.network.operation_counters.accelerate_per_sec,
            preemptive_expand_per_sec: raw.network.operation_counters.preemptive_expand_per_sec,
            merge_per_sec: raw.network.operation_counters.merge_per_sec,
            expand_rate: neteq::q14::to_per_mille(raw.network.expand_rate),
            accel_rate: neteq::q14::to_per_mille(raw.network.accelerate_rate),
            reorder_rate: raw.network.reorder_rate_permyriad as u32,
            reordered_packets: raw.network.reordered_packets,
            max_reorder_distance: raw.network.max_reorder_distance as u32,
        }
    }
}

/// Shared, render-prop wrapper around the (up-to-7200-element) NetEq history.
///
/// The history vec is built ONCE per kept-sample tick in the parent and handed
/// to several chart components. A plain `Vec<NetEqSample>` prop makes Dioxus
/// derive a CONTENT-based `PartialEq`, so every render-diff would walk all 7200
/// samples (and again per chart) just to decide "did the prop change?". Wrapping
/// it in an `Rc` and comparing by POINTER identity (`Rc::ptr_eq`) makes that
/// memo check O(1): if the parent handed down the same `Rc`, the prop is equal
/// and the chart subtree is skipped — no O(n) element walk. Clone is a refcount
/// bump, not a deep copy, so passing the same history to four charts is cheap.
/// (#1223)
#[derive(Clone)]
pub struct NetEqHistory(pub Rc<Vec<NetEqSample>>);

impl PartialEq for NetEqHistory {
    fn eq(&self, other: &Self) -> bool {
        // Identity, not content: the parent rebuilds the Rc only when the
        // history actually changed, so pointer equality is the correct (and
        // O(1)) "unchanged" signal. A derived content compare would be O(7200).
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// Whether a specific peer (not the "All Peers" aggregate) is selected. The
/// NetEq Current-Status tiles and the time-series charts are only meaningful for
/// ONE peer's deque — concatenating every peer's samples into one timeline mixes
/// unrelated clocks. Pure + host-testable so the gating decision has one source
/// of truth. (#1223)
pub fn single_peer_selected(selected: &str) -> bool {
    selected != "All Peers"
}

/// Push a sample into the per-peer ring buffer, enforcing the 2-hour cap by
/// dropping the OLDEST sample (`pop_front`) before appending. Extracted as a
/// free fn so the retention behaviour is unit-testable without the subscribe
/// loop. (#1223)
pub fn push_capped(deque: &mut VecDeque<NetEqSample>, sample: NetEqSample) {
    if deque.len() >= NETEQ_SAMPLE_CAP {
        deque.pop_front();
    }
    deque.push_back(sample);
}

/// Throttle decision: keep at most one sample per second per peer. Returns
/// `true` when there is no prior push (`None`) OR at least 1000 ms have elapsed
/// since the last kept push. Extracted so the loop's per-peer throttle is
/// unit-testable. (#1223)
pub fn should_push(last_push_ms: Option<u64>, now_ms: u64) -> bool {
    match last_push_ms {
        None => true,
        Some(last) => now_ms.saturating_sub(last) >= 1000,
    }
}

/// Total chart width in px for the scrollable, growing SVG. Mirrors the
/// signal-quality formula `(total_seconds * px_per_sec).max(min_width) + 10`.
/// `total_seconds` is derived from the OLDEST RETAINED sample (`first_ts`) — NOT
/// meeting start — so once the deque is capped the visible span honestly tracks
/// what is actually retained (this differs from `signal_quality`, which anchors
/// `first_ts` to `meeting_start_ms` for cross-peer comparability). (#1223)
pub fn neteq_chart_width(first_ts: u64, last_ts: u64, px_per_sec: f64, min_width: f64) -> f64 {
    let total_seconds = ((last_ts.saturating_sub(first_ts)) as f64 / 1000.0).max(1.0);
    (total_seconds * px_per_sec).max(min_width) + 10.0
}

/// X coordinate (px) for a sample at `ts_ms`, relative to the oldest retained
/// sample at `first_ts`, at `px_per_sec`. Mirrors signal_quality.rs:2338. (#1223)
pub fn neteq_x(ts_ms: u64, first_ts: u64, px_per_sec: f64) -> f64 {
    (ts_ms.saturating_sub(first_ts) as f64 / 1000.0) * px_per_sec
}

// Chart data series configuration
#[derive(Clone, PartialEq)]
pub struct ChartSeries {
    pub data_points: Vec<f64>,
    pub color: &'static str,
    pub label: &'static str,
    pub scale_factor: f64,
}

#[derive(Clone, PartialEq)]
pub struct ChartConfig {
    pub title: &'static str,
    pub y_axis_label: &'static str,
    pub series: Vec<ChartSeries>,
    pub max_value: f64,
}

/// Scrollable, time-based NetEq chart. Mirrors the signal-quality popup idiom
/// (signal_quality.rs:2287-2522): a fixed external Y-axis `<svg>` OUTSIDE the
/// scroll box, then a `.neteq-chart-scroll` div holding a growing inner SVG
/// whose width tracks elapsed seconds. The four NetEq charts share one timeline
/// (each has a unique scroll id; `onscroll` copies `scroll_left` to the sibling
/// `.neteq-chart-scroll` elements). X-axis time labels live INSIDE the scrolling
/// SVG (seconds from the OLDEST RETAINED sample); Y labels live in the fixed SVG.
#[component]
fn BaseChart(
    config: ChartConfig,
    /// The samples backing this chart, index-aligned with every series'
    /// `data_points`. Drives the time-based X (`first_ts` = oldest retained).
    /// Wrapped in [`NetEqHistory`] so the prop memo compares by `Rc::ptr_eq`
    /// (O(1)) instead of walking up to 7200 elements per render-diff.
    samples: NetEqHistory,
    /// Unique scroll-container id (one per chart) so scroll-sync can target the
    /// other three siblings without self-targeting.
    scroll_id: String,
    /// `true` only when the peer's deque is at the 2-hour cap — gates the
    /// "Showing last 2 hours" caption (owner decision 2).
    capped: bool,
    /// When `false`, suppress the in-SVG `.chart-title` (the diagnostics drawer
    /// renders its own `.diag-chart-head__title` above each chart, so the internal
    /// title would duplicate the heading — #1222). Defaults to `true`.
    #[props(default = true)]
    show_title: bool,
) -> Element {
    // Deref the Rc once; everything below reads the slice/vec behind it.
    let samples = &samples.0;
    // Empty → the "No data available" placeholder; never a mega-wide empty SVG.
    if samples.is_empty() {
        return rsx! {
            div { class: "neteq-advanced-chart",
                if show_title {
                    div { class: "chart-title", "{config.title}" }
                }
                div { class: "no-data", "No data available" }
            }
        };
    }

    // Drawing geometry: fixed height; the draw band is height − top − bottom.
    let chart_height: f64 = 160.0;
    let padding_top: f64 = 24.0;
    let padding_bottom: f64 = 22.0;
    let draw_height = chart_height - padding_top - padding_bottom; // 114

    // Time axis: first_ts is the OLDEST RETAINED sample (honest axis after cap),
    // NOT meeting start. last_ts is the newest. The width grows with elapsed
    // seconds and honours a min viewport so short meetings don't look squashed.
    let first_ts = samples.first().map(|s| s.timestamp_ms).unwrap_or(0);
    let last_ts = samples.last().map(|s| s.timestamp_ms).unwrap_or(first_ts);
    // Min viewport so the chart fills the drawer before it needs to scroll.
    let chart_width = neteq_chart_width(first_ts, last_ts, NETEQ_PX_PER_SEC, NETEQ_MIN_CHART_WIDTH);
    let total_seconds = ((last_ts.saturating_sub(first_ts)) as f64 / 1000.0).max(1.0);

    let max_value = config.max_value.max(1.0);

    // One polyline per series, x from real sample time, y normalized to max_value.
    // N2: this builds the FULL polyline over every retained sample (the spec
    // default). At multi-hour retention the timeline grows to ~57k px, so the
    // SVG raster for the off-screen span could pressure low-power devices.
    // Window-clipping the polyline to the visible scroll range is a possible
    // future optimization — DEFERRED pending real-device profiling; no clipping
    // is added now so the default behaviour (full history) is unchanged. (#1223)
    let series_elements: Vec<Element> = config
        .series
        .iter()
        .map(|series| {
            let points: String = series
                .data_points
                .iter()
                .enumerate()
                .map(|(i, &value)| {
                    let ts = samples.get(i).map(|s| s.timestamp_ms).unwrap_or(first_ts);
                    let x = neteq_x(ts, first_ts, NETEQ_PX_PER_SEC);
                    let normalized = (value.max(0.0) / max_value).clamp(0.0, 1.0);
                    let y = padding_top + draw_height * (1.0 - normalized);
                    format!("{x:.1},{y:.1}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            let color = series.color;
            rsx! {
                polyline {
                    points: "{points}",
                    fill: "none",
                    stroke: "{color}",
                    stroke_width: "1.5",
                    stroke_linejoin: "round",
                }
            }
        })
        .collect();

    // Legend: small colored labels pinned to the visible viewport via the fixed
    // Y-axis svg is awkward; instead render them in the scrolling svg near the
    // left edge so they ride with the start of the timeline.
    let legend_elements: Vec<Element> = config
        .series
        .iter()
        .enumerate()
        .map(|(i, series)| {
            let y_pos = 12 + (i as i32 * 13);
            let color = series.color;
            let label = series.label;
            rsx! {
                text { x: "4", y: "{y_pos}", fill: "{color}", font_size: "9", "{label}" }
            }
        })
        .collect();

    // X-axis tick labels every 10s, inside the scrolling SVG.
    let tick_interval = 10.0_f64;
    let num_ticks = (total_seconds / tick_interval).ceil() as usize + 1;

    // Y-axis labels (fixed external svg): 0 / half / max.
    let y_zero = padding_top + draw_height;
    let y_mid = padding_top + draw_height * 0.5;
    let half_max = format!("{:.1}", max_value / 2.0);
    let max_str = format!("{max_value:.1}");

    let chart_width_str = format!("{chart_width:.0}");
    let chart_width_px = format!("{chart_width:.0}px");
    let chart_height_str = format!("{chart_height:.0}");
    let x_axis_y = chart_height - 6.0; // baseline for the time labels

    // Auto-follow: after each render, stick to the right edge ONLY if the user
    // is already within 20px of it (never interrupt a deliberate scroll-back).
    // INSTANT `set_scroll_left` (no smooth behavior) — reduced-motion safe.
    //
    // This is a bare `spawn` in the component body (NOT a `use_effect`), exactly
    // mirroring the working signal_quality.rs:2371-2384 idiom. The body re-runs
    // every render — and BaseChart re-renders whenever a new sample extends the
    // history (the `NetEqHistory` Rc prop changes) — so the follow re-fires on
    // each timeline growth. A `use_effect` here would re-run ONLY when a Signal
    // read inside it changed; `last_ts` is a plain Copy `u64`, not a Signal, so an
    // effect keyed on it would run once and never re-follow on growth — a silent
    // regression. The bare-spawn form keeps the proven per-render behaviour.
    let scroll_id_for_follow = scroll_id.clone();
    spawn(async move {
        TimeoutFuture::new(0).await;
        if let Some(el) = gloo_utils::document().get_element_by_id(&scroll_id_for_follow) {
            let at_end = el.scroll_left() + el.client_width() >= el.scroll_width() - 20;
            if at_end {
                // Instant jump to the right edge (no smooth behavior).
                el.set_scroll_left(el.scroll_width());
            }
        }
    });

    rsx! {
        div { class: "neteq-advanced-chart",
            if show_title {
                div { class: "chart-title", "{config.title}" }
            }
            div { class: "neteq-chart-wrapper",
                // Fixed Y-axis overlay (outside the scroll box).
                svg {
                    class: "neteq-chart-y-axis",
                    width: "48",
                    height: "{chart_height_str}",
                    view_box: "0 0 48 {chart_height_str}",
                    // Y-axis labels
                    text { x: "44", y: "{y_zero}", fill: "{theme_color::TEXT_MUTED}", font_size: "10", text_anchor: "end", dominant_baseline: "middle", "0" }
                    text { x: "44", y: "{y_mid}", fill: "{theme_color::TEXT_MUTED}", font_size: "10", text_anchor: "end", dominant_baseline: "middle", "{half_max}" }
                    text { x: "44", y: "{padding_top}", fill: "{theme_color::TEXT_MUTED}", font_size: "10", text_anchor: "end", dominant_baseline: "middle", "{max_str}" }
                    // Y-axis unit label (rotated)
                    text { x: "10", y: "{y_mid}", fill: "{theme_color::TEXT_MUTED}", font_size: "9", text_anchor: "middle", transform: "rotate(-90, 10, {y_mid})", "{config.y_axis_label}" }
                }
                // Scrollable chart area (growing inner SVG).
                div {
                    class: "neteq-chart-scroll",
                    id: "{scroll_id}",
                    onscroll: {
                        let scroll_id = scroll_id.clone();
                        move |_| {
                            // Scroll-sync: copy this box's scroll_left onto the
                            // other `.neteq-chart-scroll` siblings so the four
                            // charts share one timeline. (signal_quality.rs:2501)
                            let doc = gloo_utils::document();
                            if let Some(src) = doc.get_element_by_id(&scroll_id) {
                                let scroll_left = src.scroll_left();
                                let els = doc.get_elements_by_class_name("neteq-chart-scroll");
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
                        // X/Y plot frame
                        line { x1: "0", y1: "{y_zero}", x2: "{chart_width_str}", y2: "{y_zero}", stroke: "{theme_color::AXIS}", stroke_width: "0.5" }
                        // X-axis ticks + time labels (every 10s)
                        for tick_i in 0..num_ticks {
                            {
                                let t = tick_i as f64 * tick_interval;
                                let x = t * NETEQ_PX_PER_SEC;
                                let mins = (t / 60.0).floor() as u32;
                                let secs = (t % 60.0).floor() as u32;
                                let label = if mins > 0 { format!("{mins}m{secs:02}s") } else { format!("{secs}s") };
                                rsx! {
                                    line { x1: "{x}", y1: "{padding_top}", x2: "{x}", y2: "{y_zero}", stroke: "{theme_color::SIGNAL_GRID_MINOR}", stroke_width: "0.5" }
                                    text { x: "{x}", y: "{x_axis_y}", fill: "{theme_color::TEXT_MUTED}", font_size: "9", text_anchor: "middle", "{label}" }
                                }
                            }
                        }
                        // Data series
                        for elem in series_elements { {elem} }
                        // Legend (rides the left of the timeline)
                        for elem in legend_elements { {elem} }
                    }
                }
            }
            // 2-hour retention caption — ONLY at the cap (owner decision 2).
            if capped {
                div { class: "neteq-chart-cap-note", "Showing last 2 hours" }
            }
        }
    }
}

#[derive(PartialEq, Clone)]
pub enum ChartType {
    Buffer,
    Jitter,
}

#[derive(PartialEq, Clone)]
pub enum AdvancedChartType {
    BufferVsTarget,
    DecodeOperations,
    QualityMetrics,
    ReorderingAnalysis,
    // `SystemPerformance` was removed (#1131 cleanup): its only two series
    // (`calls_per_sec`, `avg_frames`) were never populated — they are not part of
    // `RawNetEqStats` at all — so the chart was a permanently flat line at zero.
}

impl ChartType {
    fn stroke_color(&self) -> &'static str {
        match self {
            ChartType::Buffer => theme_color::NETEQ_BUFFER,
            ChartType::Jitter => theme_color::NETEQ_JITTER,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            ChartType::Buffer => "Buffer (ms)",
            ChartType::Jitter => "Jitter (ms)",
        }
    }
}

impl AdvancedChartType {
    fn title(&self) -> &'static str {
        match self {
            AdvancedChartType::BufferVsTarget => "Buffer Size vs Target",
            AdvancedChartType::DecodeOperations => "Decode Operations Per Second",
            AdvancedChartType::QualityMetrics => "Packets Awaiting Decode",
            AdvancedChartType::ReorderingAnalysis => "Packet Reordering",
        }
    }
}

#[component]
pub fn NetEqChart(data: Vec<u64>, chart_type: ChartType, width: u32, height: u32) -> Element {
    let chart_width = width as f64;
    let chart_height = height as f64;
    let margin_left = 25.0;
    let margin_bottom = 15.0;
    let plot_width = chart_width - margin_left - 10.0;
    let plot_height = chart_height - margin_bottom - 5.0;

    let max_val = *data.iter().max().unwrap_or(&1);
    let max_val_f64 = max_val as f64;
    let data_len = data.len();

    let points: String = if data.is_empty() {
        String::new()
    } else {
        data.iter()
            .enumerate()
            .map(|(i, v)| {
                let x = margin_left
                    + (i as f64 / (data_len.saturating_sub(1).max(1) as f64) * plot_width);
                let y = plot_height
                    - (*v as f64 / if max_val_f64 == 0.0 { 1.0 } else { max_val_f64 }
                        * plot_height)
                    + 5.0;
                format!("{x:.1},{y:.1}")
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let time_span = data_len.saturating_sub(1);
    let stroke_color = chart_type.stroke_color();
    let label = chart_type.label();
    let ml = margin_left.to_string();
    let ph5 = (plot_height + 5.0).to_string();
    let cw5 = (chart_width - 5.0).to_string();
    let ch1 = (chart_height - 1.0).to_string();
    let cw20 = (chart_width - 20.0).to_string();
    let view_box = format!("0 0 {width} {height}");
    let time_label = format!("{}s", time_span);

    rsx! {
        div { class: "neteq-chart",
            div { class: "chart-title", "{label}" }
            svg {
                width: "{width}",
                height: "{height}",
                view_box: "{view_box}",
                preserve_aspect_ratio: "none",
                // Y-axis
                line { x1: "{ml}", y1: "5", x2: "{ml}", y2: "{ph5}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // X-axis
                line { x1: "{ml}", y1: "{ph5}", x2: "{cw5}", y2: "{ph5}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // Data line
                if !points.is_empty() {
                    polyline { points: "{points}", fill: "none", stroke: "{stroke_color}", stroke_width: "2" }
                }
                // Y-axis labels
                text { x: "0", y: "10", fill: "{theme_color::TEXT_SUBTLE}", font_size: "11", "{max_val}" }
                text { x: "0", y: "{ph5}", fill: "{theme_color::TEXT_SUBTLE}", font_size: "11", "0" }
                // X-axis labels
                text { x: "{ml}", y: "{ch1}", fill: "{theme_color::TEXT_SUBTLE}", font_size: "11", "0s" }
                text { x: "{cw20}", y: "{ch1}", fill: "{theme_color::TEXT_SUBTLE}", font_size: "11", "{time_label}" }
            }
        }
    }
}

// Helper functions to create chart configurations
impl ChartConfig {
    pub fn buffer_vs_target(stats_history: &[NetEqSample]) -> Self {
        let max_buffer = stats_history
            .iter()
            .map(|s| s.buffer_ms.max(s.target_ms))
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        let buffer_data: Vec<f64> = stats_history.iter().map(|s| s.buffer_ms as f64).collect();
        let target_data: Vec<f64> = stats_history.iter().map(|s| s.target_ms as f64).collect();
        Self {
            title: "Buffer Size vs Target",
            y_axis_label: "Buffer (ms)",
            max_value: max_buffer,
            series: vec![
                ChartSeries {
                    data_points: buffer_data,
                    color: theme_color::NETEQ_BLUE,
                    label: "Current Buffer",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: target_data,
                    color: theme_color::NETEQ_GREEN,
                    label: "Target Buffer",
                    scale_factor: 1.0,
                },
            ],
        }
    }

    pub fn decode_operations(stats_history: &[NetEqSample]) -> Self {
        // Y ceiling = the max across exactly the FIVE plotted series. The compact
        // `NetEqSample` intentionally omits fast_accelerate / comfort_noise / dtmf
        // (they were never plotted — only padded the old MAX), so the axis ceiling
        // now matches the data on screen (#1223).
        let max_ops = stats_history
            .iter()
            .map(|s| {
                s.normal_per_sec
                    .max(s.expand_per_sec)
                    .max(s.accelerate_per_sec)
                    .max(s.preemptive_expand_per_sec)
                    .max(s.merge_per_sec)
            })
            .fold(1.0f32, f32::max)
            .max(1.0) as f64;
        Self {
            title: "Decode Operations Per Second",
            y_axis_label: "Operations/sec",
            max_value: max_ops,
            series: vec![
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.normal_per_sec as f64)
                        .collect(),
                    color: theme_color::NETEQ_GREEN,
                    label: "Normal",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.expand_per_sec as f64)
                        .collect(),
                    color: theme_color::NETEQ_RED,
                    label: "Expand",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.accelerate_per_sec as f64)
                        .collect(),
                    color: theme_color::NETEQ_ORANGE,
                    label: "Accelerate",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.preemptive_expand_per_sec as f64)
                        .collect(),
                    color: theme_color::NETEQ_PURPLE,
                    label: "Preemptive Expand",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.merge_per_sec as f64)
                        .collect(),
                    color: theme_color::NETEQ_TEAL,
                    label: "Merge",
                    scale_factor: 1.0,
                },
            ],
        }
    }

    pub fn quality_metrics(stats_history: &[NetEqSample]) -> Self {
        let max_packets = stats_history
            .iter()
            .map(|s| s.packets_awaiting_decode)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        // Single real series: packets buffered but not yet decoded (queue depth).
        // The former "Underruns" series was dropped (#1131 cleanup) — `underruns`
        // was never populated (it isn't a `RawNetEqStats`/`NetEqSample` field), so
        // it plotted a flat line at zero and the unexplained ×0.3 scale only
        // confused the axis.
        Self {
            title: "Packets Awaiting Decode",
            y_axis_label: "Packets",
            max_value: max_packets,
            series: vec![ChartSeries {
                data_points: stats_history
                    .iter()
                    .map(|s| s.packets_awaiting_decode as f64)
                    .collect(),
                color: theme_color::NETEQ_PURPLE,
                label: "Packets",
                scale_factor: 1.0,
            }],
        }
    }

    pub fn reordering_analysis(stats_history: &[NetEqSample]) -> Self {
        let max_rate = stats_history
            .iter()
            .map(|s| s.reorder_rate)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        let max_distance = stats_history
            .iter()
            .map(|s| s.max_reorder_distance)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        // Two series share one Y axis but DIFFERENT units: reorder rate is
        // per-myriad (‱) and max distance is a packet count. The axis label and
        // series labels carry the units so the shared scale isn't read as one unit
        // (#1131 cleanup). Both are real telemetry, so the chart is kept.
        Self {
            title: "Packet Reordering",
            y_axis_label: "Rate (‱) · Distance (pkts)",
            max_value: max_rate.max(max_distance),
            series: vec![
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.reorder_rate as f64)
                        .collect(),
                    color: theme_color::NETEQ_RED,
                    label: "Reorder rate (‱)",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: stats_history
                        .iter()
                        .map(|s| s.max_reorder_distance as f64)
                        .collect(),
                    color: theme_color::NETEQ_TEAL,
                    label: "Max distance (pkts)",
                    scale_factor: 1.0,
                },
            ],
        }
    }
}

#[component]
pub fn NetEqAdvancedChart(
    /// Shared history wrapper — see [`NetEqHistory`]. Cloning to hand it to
    /// `BaseChart` is a refcount bump, and the prop memo compares by pointer.
    stats_history: NetEqHistory,
    chart_type: AdvancedChartType,
    /// Unique scroll-container id so the four stacked charts can scroll-sync
    /// without self-targeting (see `BaseChart`).
    scroll_id: String,
    /// `true` only at the 2-hour cap → gates the "Showing last 2 hours" caption.
    capped: bool,
    /// Forwarded to [`BaseChart`]: when `false`, suppress the in-SVG
    /// `.chart-title` (the diagnostics drawer renders its own per-chart heading —
    /// #1222). Defaults to `true`.
    #[props(default = true)]
    show_title: bool,
) -> Element {
    if stats_history.0.is_empty() {
        return rsx! {
            div { class: "neteq-advanced-chart",
                if show_title {
                    div { class: "chart-title", "{chart_type.title()}" }
                }
                div { class: "no-data", "No data available" }
            }
        };
    }

    // ChartConfig::* take `&[NetEqSample]`; `&stats_history.0` derefs the Rc'd
    // Vec to a slice with no copy.
    let config = match chart_type {
        AdvancedChartType::BufferVsTarget => ChartConfig::buffer_vs_target(&stats_history.0),
        AdvancedChartType::DecodeOperations => ChartConfig::decode_operations(&stats_history.0),
        AdvancedChartType::QualityMetrics => ChartConfig::quality_metrics(&stats_history.0),
        AdvancedChartType::ReorderingAnalysis => ChartConfig::reordering_analysis(&stats_history.0),
    };

    rsx! {
        BaseChart { config, samples: stats_history, scroll_id, capped, show_title }
    }
}

// ── Current Status threshold classifiers (Directive 5, #1222) ─────────────────
// Each returns `(class, reason)` where `class` ∈ `is-good | is-warn | is-poor`
// and `reason` is the WCAG text shown alongside the color (never color alone).
// Compared on the SAME units the value strings use (‰ for expand/accel, ‱ for
// reorder; buffer/target/packets are raw). Pure / host-testable.

/// Buffer health vs the adaptive Target. `0` is poor (queue ran dry); within
/// ±20% of target is good; otherwise the buffer has drifted from target.
fn classify_buffer(buffer_ms: u32, target_ms: u32) -> (&'static str, Option<&'static str>) {
    if buffer_ms == 0 {
        ("is-poor", Some("buffer empty — audio starving"))
    } else if buffer_ms >= (target_ms as f32 * 0.8) as u32
        && buffer_ms <= (target_ms as f32 * 1.2) as u32
    {
        ("is-good", None)
    } else {
        ("is-warn", Some("buffer drifting from target"))
    }
}

/// Packets-awaiting-decode queue depth. ≤8 steady-low is healthy; 9–20 building;
/// >20 decode can't keep up.
fn classify_packets(packets: u32) -> (&'static str, Option<&'static str>) {
    if packets <= 8 {
        ("is-good", None)
    } else if packets <= 20 {
        ("is-warn", Some("queue building"))
    } else {
        ("is-poor", Some("decode falling behind"))
    }
}

/// Expand (loss-concealment) rate in ‰. `0` is healthy; a few ‰ is occasional
/// concealment; sustained high (>50‰) means the network is dropping/delaying audio.
fn classify_expand(expand_permille: f32) -> (&'static str, Option<&'static str>) {
    if expand_permille == 0.0 {
        ("is-good", None)
    } else if expand_permille <= 50.0 {
        ("is-warn", Some("occasional concealment"))
    } else {
        ("is-poor", Some("sustained packet loss — network degraded"))
    }
}

/// Accelerate (catch-up) rate in ‰. ≤30‰ normal; 30–80‰ draining a full buffer;
/// >80‰ chronically overfull (high-latency catch-up).
fn classify_accel(accel_permille: f32) -> (&'static str, Option<&'static str>) {
    if accel_permille <= 30.0 {
        ("is-good", None)
    } else if accel_permille <= 80.0 {
        ("is-warn", Some("draining a full buffer"))
    } else {
        ("is-poor", Some("buffer overfull — high latency catch-up"))
    }
}

/// Reorder rate in ‱ (lifetime cumulative). ≤50‱ healthy; 50–200‱ some
/// reordering; >200‱ heavy reordering / path instability.
fn classify_reorder(reorder_permyriad: u32) -> (&'static str, Option<&'static str>) {
    if reorder_permyriad <= 50 {
        ("is-good", None)
    } else if reorder_permyriad <= 200 {
        ("is-warn", Some("some reordering"))
    } else {
        ("is-poor", Some("heavy reordering — path instability"))
    }
}

/// Current Status — two-tier "stat + details" layout (iteration 4, #1222).
/// Tier 1 = primary Buffer + Target stats; Tier 2 = the flow group (Packets
/// awaiting / Packets-per-sec / Expand / Accel) as compact rows; Tier 3 = the
/// demoted reordering trio. Value/class/reason are factored ONCE so the populated
/// and `None` branches share the render (the `None` branch passes `"--"` values,
/// `None` reasons, and neutral `""` classes). Quality classes tint the VALUE only
/// (`.is-good/.is-warn/.is-poor` in style.css → var(--diag-q-*)). Styling lives
/// ONCE in `style.css` under `.neteq-status .status-*`.
#[component]
pub fn NetEqStatusDisplay(latest_stats: Option<NetEqSample>) -> Element {
    // Factor every value/class/reason once from the optional sample. Expand /
    // accelerate rates arrive as Q14 fractions converted to per-mille (‰); the
    // reorder rate is per-myriad (‱). The value strings carry the unit so the
    // numbers are interpretable without guessing the scale.
    let buffer_val = latest_stats
        .as_ref()
        .map(|s| s.buffer_ms.to_string())
        .unwrap_or_else(|| "--".to_string());
    let target_val = latest_stats
        .as_ref()
        .map(|s| s.target_ms.to_string())
        .unwrap_or_else(|| "--".to_string());
    let packets_val = latest_stats
        .as_ref()
        .map(|s| s.packets_awaiting_decode.to_string())
        .unwrap_or_else(|| "--".to_string());
    let pps_val = latest_stats
        .as_ref()
        .map(|s| s.packets_per_sec.to_string())
        .unwrap_or_else(|| "--".to_string());
    let expand_str = latest_stats
        .as_ref()
        .map(|s| format!("{:.1}\u{2030}", s.expand_rate))
        .unwrap_or_else(|| "--".to_string());
    let accel_str = latest_stats
        .as_ref()
        .map(|s| format!("{:.1}\u{2030}", s.accel_rate))
        .unwrap_or_else(|| "--".to_string());
    let reorder_str = latest_stats
        .as_ref()
        .map(|s| format!("{}\u{2031}", s.reorder_rate))
        .unwrap_or_else(|| "--".to_string());
    let reordered_val = latest_stats
        .as_ref()
        .map(|s| s.reordered_packets.to_string())
        .unwrap_or_else(|| "--".to_string());
    let maxdist_val = latest_stats
        .as_ref()
        .map(|s| s.max_reorder_distance.to_string())
        .unwrap_or_else(|| "--".to_string());

    // Quality classes + reasons (neutral for the None branch).
    let (buffer_q, buffer_reason) = latest_stats
        .as_ref()
        .map(|s| classify_buffer(s.buffer_ms, s.target_ms))
        .unwrap_or(("", None));
    let (packets_q, packets_title) = latest_stats
        .as_ref()
        .map(|s| classify_packets(s.packets_awaiting_decode))
        .map(|(c, r)| (c, r.unwrap_or("")))
        .unwrap_or(("", ""));
    let (expand_q, expand_title) = latest_stats
        .as_ref()
        .map(|s| classify_expand(s.expand_rate))
        .map(|(c, r)| (c, r.unwrap_or("")))
        .unwrap_or(("", ""));
    let (accel_q, accel_title) = latest_stats
        .as_ref()
        .map(|s| classify_accel(s.accel_rate))
        .map(|(c, r)| (c, r.unwrap_or("")))
        .unwrap_or(("", ""));
    let (reorder_q, reorder_title) = latest_stats
        .as_ref()
        .map(|s| classify_reorder(s.reorder_rate))
        .map(|(c, r)| (c, r.unwrap_or("")))
        .unwrap_or(("", ""));
    // Target is neutral (no color); packets/s, reordered, max-dist are neutral too.
    let target_q = "";

    rsx! {
        div { class: "neteq-status",
            // Tier 1 — primary: Buffer + Target.
            div { class: "status-primary",
                div { class: "status-stat status-stat--primary {buffer_q}",
                    div { class: "status-stat__value", "{buffer_val}" }
                    div { class: "status-stat__label", "Buffer" }
                    div { class: "status-stat__unit", "ms" }
                    if let Some(r) = buffer_reason {
                        div { class: "status-stat__reason", "{r}" }
                    }
                }
                div { class: "status-stat status-stat--primary {target_q}",
                    div { class: "status-stat__value", "{target_val}" }
                    div { class: "status-stat__label", "Target" }
                    div { class: "status-stat__unit", "ms" }
                }
            }
            // Tier 2 — flow group, 4-up compact rows. `title` carries the reason.
            div { class: "status-secondary",
                div { class: "status-row {packets_q}", title: "{packets_title}",
                    span { class: "status-row__label", "Packets awaiting" }
                    span { class: "status-row__value", "{packets_val}" }
                }
                div { class: "status-row",
                    span { class: "status-row__label", "Packets / s" }
                    span { class: "status-row__value", "{pps_val}" }
                }
                div { class: "status-row {expand_q}", title: "{expand_title}",
                    span { class: "status-row__label", "Expand rate" }
                    span { class: "status-row__value", "{expand_str}" }
                }
                div { class: "status-row {accel_q}", title: "{accel_title}",
                    span { class: "status-row__label", "Accelerate rate" }
                    span { class: "status-row__value", "{accel_str}" }
                }
            }
            // Tier 3 — reordering trio, demoted muted micro-row.
            div { class: "status-reorder",
                span { class: "status-reorder__head", "Reordering" }
                span { class: "status-reorder__item {reorder_q}", title: "{reorder_title}", "Rate {reorder_str}" }
                span { class: "status-reorder__item", "Reordered {reordered_val}" }
                span { class: "status-reorder__item", "Max dist {maxdist_val}" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a compact `NetEqSample` from raw NetEq stats via the real
    /// `from_raw` mapping, so the chart-config tests exercise the actual data
    /// path (not a hand-rolled fixture that could drift from production mapping).
    fn sample_with(
        buffer_ms: u16,
        target_ms: u32,
        packets_awaiting: usize,
        expand_q14: u16,
        reorder_permyriad: u16,
        max_reorder_distance: u16,
    ) -> NetEqSample {
        let mut raw = RawNetEqStats {
            network: neteq::statistics::NetworkStatistics::default(),
            lifetime: neteq::statistics::LifetimeStatistics::default(),
            current_buffer_size_ms: buffer_ms as u32,
            target_delay_ms: target_ms,
            packets_awaiting_decode: packets_awaiting,
            packets_per_sec: 0,
        };
        raw.network.expand_rate = expand_q14;
        raw.network.reorder_rate_permyriad = reorder_permyriad;
        raw.network.max_reorder_distance = max_reorder_distance;
        NetEqSample::from_raw(raw, 0)
    }

    /// A minimal sample carrying only a timestamp — for the cap / time-axis
    /// tests where only `timestamp_ms` matters.
    fn sample_at(ts_ms: u64) -> NetEqSample {
        let mut s = sample_with(0, 0, 0, 0, 0, 0);
        s.timestamp_ms = ts_ms;
        s
    }

    /// `AdvancedChartType` must hold exactly the four surviving charts —
    /// `SystemPerformance` was removed (#1131) because its only two series were
    /// never populated. Re-adding a fifth (or restoring SystemPerformance) flips
    /// the title set this asserts on, catching an accidental revert.
    #[test]
    fn advanced_chart_titles_are_the_four_kept_charts() {
        let titles: Vec<&str> = [
            AdvancedChartType::BufferVsTarget,
            AdvancedChartType::DecodeOperations,
            AdvancedChartType::QualityMetrics,
            AdvancedChartType::ReorderingAnalysis,
        ]
        .iter()
        .map(|c| c.title())
        .collect();
        assert_eq!(
            titles,
            [
                "Buffer Size vs Target",
                "Decode Operations Per Second",
                "Packets Awaiting Decode",
                "Packet Reordering",
            ]
        );
        // The dead "System Performance" chart must not come back.
        assert!(!titles.contains(&"System Performance"));
    }

    /// `quality_metrics` must plot exactly ONE series (packets awaiting decode).
    /// The former second series ("Underruns") was dropped because `underruns` is
    /// never populated; if it is re-added this length flips to 2 and fails.
    #[test]
    fn quality_metrics_has_single_packets_series() {
        let stats = vec![
            sample_with(80, 100, 5, 0, 0, 0),
            sample_with(80, 100, 9, 0, 0, 0),
        ];
        let cfg = ChartConfig::quality_metrics(&stats);
        assert_eq!(cfg.series.len(), 1, "only the packets series should remain");
        assert_eq!(cfg.series[0].label, "Packets");
        // Y axis is a packet count, not a generic "Count".
        assert_eq!(cfg.y_axis_label, "Packets");
        // The single series carries the real queue-depth values.
        assert_eq!(cfg.series[0].data_points, vec![5.0, 9.0]);
    }

    /// `reordering_analysis` keeps both real series but the axis + series labels
    /// must spell out the DIFFERENT units (‱ rate vs. packet distance) that share
    /// the one Y axis — the old label was the ambiguous "Rate/Distance".
    #[test]
    fn reordering_analysis_labels_carry_units() {
        let stats = vec![sample_with(80, 100, 5, 0, 30, 4)];
        let cfg = ChartConfig::reordering_analysis(&stats);
        assert_eq!(cfg.series.len(), 2);
        assert!(
            cfg.y_axis_label.contains('‱') && cfg.y_axis_label.contains("pkts"),
            "axis label must disambiguate the two units, got {:?}",
            cfg.y_axis_label
        );
        assert_eq!(cfg.series[0].label, "Reorder rate (‱)");
        assert_eq!(cfg.series[1].label, "Max distance (pkts)");
    }

    /// Parse-once: `NetEqSample::from_json` extracts the exact mapped fields and
    /// renders the Q14 expand fraction as per-mille (4096 Q14 → 250‰), which is
    /// why the status tiles append the ‰ unit. Catches a wrong field map or a
    /// dropped q14 conversion. We serialize a REAL `RawNetEqStats` so the test
    /// rides the production serde path (no hand-built JSON that could drift).
    #[test]
    fn from_json_maps_fields_and_converts_q14_to_per_mille() {
        // 4096 Q14 = 250‰; buffer 80, target 100, packets 5, reorder 30‱.
        let mut raw = RawNetEqStats {
            network: neteq::statistics::NetworkStatistics::default(),
            lifetime: neteq::statistics::LifetimeStatistics::default(),
            current_buffer_size_ms: 80,
            target_delay_ms: 100,
            packets_awaiting_decode: 5,
            packets_per_sec: 42,
        };
        raw.network.expand_rate = 4096;
        raw.network.reorder_rate_permyriad = 30;
        raw.network.reordered_packets = 7;
        raw.network.max_reorder_distance = 4;
        raw.network.operation_counters.normal_per_sec = 50.0;
        let json = serde_json::to_string(&raw).expect("raw serializes");

        let s = NetEqSample::from_json(&json, 12345).expect("valid json parses");
        assert_eq!(s.timestamp_ms, 12345);
        assert_eq!(s.buffer_ms, 80);
        assert_eq!(s.target_ms, 100);
        assert_eq!(s.packets_awaiting_decode, 5);
        assert_eq!(s.packets_per_sec, 42);
        assert_eq!(s.reorder_rate, 30);
        assert_eq!(s.reordered_packets, 7);
        assert_eq!(s.max_reorder_distance, 4);
        assert!((s.normal_per_sec - 50.0).abs() < 0.001);
        assert!(
            (s.expand_rate - 250.0).abs() < 0.01,
            "4096 Q14 must map to 250‰, got {}",
            s.expand_rate
        );

        // Malformed JSON → None, no panic.
        assert!(NetEqSample::from_json("{ not json", 1).is_none());
    }

    /// Retention cap: pushing 7201 samples leaves exactly 7200 and drops the
    /// OLDEST. The first retained element must be the SECOND-pushed sample, not
    /// the first. Catches `pop_back` instead of `pop_front`, or a wrong cap.
    #[test]
    fn push_capped_drops_oldest_at_cap() {
        let mut dq: VecDeque<NetEqSample> = VecDeque::new();
        // Push 7201 samples whose timestamp == push index, so identity is clear.
        for i in 0..(NETEQ_SAMPLE_CAP as u64 + 1) {
            push_capped(&mut dq, sample_at(i));
        }
        assert_eq!(dq.len(), NETEQ_SAMPLE_CAP, "deque must be capped at 7200");
        // Sample 0 was dropped; the oldest retained is sample 1.
        assert_eq!(
            dq.front().unwrap().timestamp_ms,
            1,
            "oldest retained must be the 2nd-pushed sample (pop_front), not the 1st"
        );
        assert_eq!(
            dq.back().unwrap().timestamp_ms,
            NETEQ_SAMPLE_CAP as u64,
            "newest retained must be the last-pushed sample"
        );
    }

    /// Throttle decision (single peer): no prior push keeps; <1000ms skips;
    /// exactly 1000ms keeps. Catches flipping `>=1000` to `>1000` (the 1000ms
    /// case would then wrongly skip).
    #[test]
    fn should_push_respects_one_hz_throttle() {
        assert!(should_push(None, 0), "first sample always kept");
        assert!(
            !should_push(Some(1000), 1500),
            "500ms later must be skipped"
        );
        assert!(
            !should_push(Some(1000), 1999),
            "999ms later must be skipped"
        );
        assert!(
            should_push(Some(1000), 2000),
            "exactly 1000ms later must be kept"
        );
        assert!(should_push(Some(1000), 5000), "well past 1s is kept");
    }

    /// Throttle is per-peer independent: a fresh push for peer B is kept even
    /// when peer A pushed <1s ago. Mimics the loop's per-peer last_push_ms map.
    #[test]
    fn should_push_is_per_peer_independent() {
        let mut last_push: HashMap<&str, u64> = HashMap::new();
        // Peer A pushes at t=1000 (kept — no prior).
        assert!(should_push(last_push.get("A").copied(), 1000));
        last_push.insert("A", 1000);
        // Peer B's FIRST push at t=1200 is kept even though A pushed 200ms ago.
        assert!(
            should_push(last_push.get("B").copied(), 1200),
            "peer B must not be throttled by peer A's recent push"
        );
        // Peer A again at t=1200 is throttled (only 200ms since its own push).
        assert!(!should_push(last_push.get("A").copied(), 1200));
    }

    /// Time-axis math: `neteq_x` maps elapsed ms → px at `NETEQ_PX_PER_SEC`, and
    /// `neteq_chart_width` grows with elapsed seconds while honouring the
    /// min-viewport `(total_seconds*px_per_sec).max(min)+10`. Expecteds are
    /// recomputed FROM the consts (so a const change doesn't break the test), but
    /// the assertion exercises the SOURCE `neteq_chart_width`/`neteq_x` — so a
    /// mutation in their bodies (dropping `+10.0` or the `.max(min)` clamp, or a
    /// wrong px_per_sec scale) makes actual != expected and fails.
    #[test]
    fn time_axis_math_x_and_width() {
        // x: 5s after first_ts = 5 * NETEQ_PX_PER_SEC.
        let expected_x = 5.0 * NETEQ_PX_PER_SEC;
        assert!((neteq_x(6000, 1000, NETEQ_PX_PER_SEC) - expected_x).abs() < 0.001);
        // x at first_ts is 0.
        assert!((neteq_x(1000, 1000, NETEQ_PX_PER_SEC)).abs() < 0.001);

        // Short span (10s) clamps to the min viewport: max(10s*px, min) + 10.
        let total_short = (10_000f64 / 1000.0).max(1.0);
        let expected_short = (total_short * NETEQ_PX_PER_SEC).max(NETEQ_MIN_CHART_WIDTH) + 10.0;
        let w_short = neteq_chart_width(0, 10_000, NETEQ_PX_PER_SEC, NETEQ_MIN_CHART_WIDTH);
        assert!(
            (w_short - expected_short).abs() < 0.001,
            "got {w_short}, expected {expected_short}"
        );

        // Long span (200s) grows past the min: 200s*px + 10.
        let total_long = (200_000f64 / 1000.0).max(1.0);
        let expected_long = (total_long * NETEQ_PX_PER_SEC).max(NETEQ_MIN_CHART_WIDTH) + 10.0;
        let w_long = neteq_chart_width(0, 200_000, NETEQ_PX_PER_SEC, NETEQ_MIN_CHART_WIDTH);
        assert!(
            (w_long - expected_long).abs() < 0.001,
            "got {w_long}, expected {expected_long}"
        );
    }

    /// Honest axis after cap: when the deque is capped, the chart origin
    /// (`first_ts`) is the OLDEST RETAINED sample's timestamp — NOT 0 / meeting
    /// start. Build a capped history whose first retained sample has a NONZERO
    /// timestamp and assert the width reflects (last - first), not (last - 0).
    /// Catches anyone re-anchoring first_ts to 0.
    #[test]
    fn honest_axis_uses_oldest_retained_sample() {
        // The capped deque retains samples 1..=7200 (sample 0 dropped). Their
        // timestamps are seconds: ts = i*1000. So first retained = 1000ms,
        // last = 7_200_000ms. Honest span = 7,199,000ms ≈ 7199s.
        let mut dq: VecDeque<NetEqSample> = VecDeque::new();
        for i in 0..(NETEQ_SAMPLE_CAP as u64 + 1) {
            push_capped(&mut dq, sample_at(i * 1000));
        }
        let first_ts = dq.front().unwrap().timestamp_ms;
        let last_ts = dq.back().unwrap().timestamp_ms;
        assert_eq!(first_ts, 1000, "origin must be the oldest RETAINED sample");

        // Expected from the honest origin, recomputed from the consts (so a const
        // change doesn't break the test) but exercising the SOURCE
        // `neteq_chart_width` — dropping `+10.0` or `.max(min)` in its body would
        // make actual != expected and fail.
        let total_honest = (last_ts.saturating_sub(first_ts) as f64 / 1000.0).max(1.0);
        let expected_honest = (total_honest * NETEQ_PX_PER_SEC).max(NETEQ_MIN_CHART_WIDTH) + 10.0;
        let honest = neteq_chart_width(first_ts, last_ts, NETEQ_PX_PER_SEC, NETEQ_MIN_CHART_WIDTH);
        // Width if someone wrongly anchored to 0 (spans last - 0, one extra second).
        let wrong = neteq_chart_width(0, last_ts, NETEQ_PX_PER_SEC, NETEQ_MIN_CHART_WIDTH);
        assert!(
            (honest - expected_honest).abs() < 0.001,
            "got {honest}, expected {expected_honest}"
        );
        assert_ne!(
            honest, wrong,
            "honest origin must differ from a 0-anchored origin"
        );
    }

    /// Decode-operations Y ceiling is computed over exactly the FIVE plotted
    /// series (normal/expand/accelerate/preemptive/merge) — the compact sample
    /// omits the never-plotted fast_accelerate/comfort_noise/dtmf. Five series
    /// are plotted; catches an accidental sixth.
    #[test]
    fn decode_operations_plots_five_series() {
        let cfg = ChartConfig::decode_operations(&[sample_with(0, 0, 0, 0, 0, 0)]);
        assert_eq!(cfg.series.len(), 5);
        let labels: Vec<&str> = cfg.series.iter().map(|s| s.label).collect();
        assert_eq!(
            labels,
            [
                "Normal",
                "Expand",
                "Accelerate",
                "Preemptive Expand",
                "Merge"
            ]
        );
    }

    /// `single_peer_selected` gates the Current-Status tiles + time-series charts
    /// to a SINGLE peer; the "All Peers" aggregate gets the placeholder instead.
    /// Catches flipping the `!=` to `==` in `single_peer_selected` (which would
    /// invert the gate: charts for "All Peers", placeholder for a real peer).
    #[test]
    fn single_peer_selected_gates_only_all_peers() {
        assert!(
            !single_peer_selected("All Peers"),
            "the aggregate must NOT count as a single peer"
        );
        assert!(
            single_peer_selected("peer-123"),
            "a specific peer id is a single peer"
        );
        // Empty string is anything-but-"All Peers", so it is treated as a single
        // peer (it is `!= "All Peers"`). Pins the exact comparison semantics.
        assert!(single_peer_selected(""), "empty != \"All Peers\" → single");
    }

    // ── Directive 5 threshold classifiers (#1222) ────────────────────────────
    // Each pins BOTH sides of every boundary so mutating any threshold fails.

    /// Buffer vs target 100ms: 80 good, 79 warn (drift low), 120 good, 121 warn
    /// (drift high), 0 poor (empty). ±20% window = [80,120].
    #[test]
    fn classify_buffer_boundaries() {
        assert_eq!(classify_buffer(80, 100), ("is-good", None));
        assert_eq!(
            classify_buffer(79, 100),
            ("is-warn", Some("buffer drifting from target"))
        );
        assert_eq!(classify_buffer(120, 100), ("is-good", None));
        assert_eq!(
            classify_buffer(121, 100),
            ("is-warn", Some("buffer drifting from target"))
        );
        assert_eq!(
            classify_buffer(0, 100),
            ("is-poor", Some("buffer empty — audio starving"))
        );
    }

    /// Packets awaiting: 8 good, 9 warn, 20 warn, 21 poor.
    #[test]
    fn classify_packets_boundaries() {
        assert_eq!(classify_packets(8), ("is-good", None));
        assert_eq!(classify_packets(9), ("is-warn", Some("queue building")));
        assert_eq!(classify_packets(20), ("is-warn", Some("queue building")));
        assert_eq!(
            classify_packets(21),
            ("is-poor", Some("decode falling behind"))
        );
    }

    /// Expand ‰: 0 good, 1 warn, 50 warn, 51 poor.
    #[test]
    fn classify_expand_boundaries() {
        assert_eq!(classify_expand(0.0), ("is-good", None));
        assert_eq!(
            classify_expand(1.0),
            ("is-warn", Some("occasional concealment"))
        );
        assert_eq!(
            classify_expand(50.0),
            ("is-warn", Some("occasional concealment"))
        );
        assert_eq!(
            classify_expand(51.0),
            ("is-poor", Some("sustained packet loss — network degraded"))
        );
    }

    /// Accel ‰: 30 good, 31 warn, 80 warn, 81 poor.
    #[test]
    fn classify_accel_boundaries() {
        assert_eq!(classify_accel(30.0), ("is-good", None));
        assert_eq!(
            classify_accel(31.0),
            ("is-warn", Some("draining a full buffer"))
        );
        assert_eq!(
            classify_accel(80.0),
            ("is-warn", Some("draining a full buffer"))
        );
        assert_eq!(
            classify_accel(81.0),
            ("is-poor", Some("buffer overfull — high latency catch-up"))
        );
    }

    /// Reorder ‱: 50 good, 51 warn, 200 warn, 201 poor.
    #[test]
    fn classify_reorder_boundaries() {
        assert_eq!(classify_reorder(50), ("is-good", None));
        assert_eq!(classify_reorder(51), ("is-warn", Some("some reordering")));
        assert_eq!(classify_reorder(200), ("is-warn", Some("some reordering")));
        assert_eq!(
            classify_reorder(201),
            ("is-poor", Some("heavy reordering — path instability"))
        );
    }
}
