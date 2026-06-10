use dioxus::prelude::*;
pub use neteq::NetEqStats as RawNetEqStats;
use serde::{Deserialize, Serialize};

use crate::theme::color as theme_color;

// UI-friendly structure for charts (keeping the old one)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetEqStats {
    pub timestamp: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub packets_awaiting_decode: u32,
    pub packets_per_sec: u32,
    pub expand_rate: f32,
    pub accel_rate: f32,
    pub reorder_rate: u32,
    pub reordered_packets: u32,
    pub max_reorder_distance: u32,
    pub sequence_number: u32,
    pub rtp_timestamp: u32,
    // Operation counters per second
    pub normal_per_sec: f32,
    pub expand_per_sec: f32,
    pub accelerate_per_sec: f32,
    pub fast_accelerate_per_sec: f32,
    pub preemptive_expand_per_sec: f32,
    pub merge_per_sec: f32,
    pub comfort_noise_per_sec: f32,
    pub dtmf_per_sec: f32,
    pub undefined_per_sec: f32,
}

impl From<RawNetEqStats> for NetEqStats {
    fn from(raw: RawNetEqStats) -> Self {
        Self {
            timestamp: 0,
            buffer_ms: raw.current_buffer_size_ms,
            target_ms: raw.target_delay_ms,
            packets_awaiting_decode: raw.packets_awaiting_decode as u32,
            packets_per_sec: raw.packets_per_sec,
            expand_rate: neteq::q14::to_per_mille(raw.network.expand_rate),
            accel_rate: neteq::q14::to_per_mille(raw.network.accelerate_rate),
            reorder_rate: raw.network.reorder_rate_permyriad as u32,
            reordered_packets: raw.network.reordered_packets,
            max_reorder_distance: raw.network.max_reorder_distance as u32,
            sequence_number: 0,
            rtp_timestamp: 0,
            normal_per_sec: raw.network.operation_counters.normal_per_sec,
            expand_per_sec: raw.network.operation_counters.expand_per_sec,
            accelerate_per_sec: raw.network.operation_counters.accelerate_per_sec,
            fast_accelerate_per_sec: raw.network.operation_counters.fast_accelerate_per_sec,
            preemptive_expand_per_sec: raw.network.operation_counters.preemptive_expand_per_sec,
            merge_per_sec: raw.network.operation_counters.merge_per_sec,
            comfort_noise_per_sec: raw.network.operation_counters.comfort_noise_per_sec,
            dtmf_per_sec: raw.network.operation_counters.dtmf_per_sec,
            undefined_per_sec: raw.network.operation_counters.undefined_per_sec,
        }
    }
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

#[component]
fn BaseChart(config: ChartConfig, data_len: usize, width: u32, height: u32) -> Element {
    let chart_width = width as f64;
    let chart_height = height as f64;
    let margin_left = 60.0;
    let margin_bottom = 40.0;
    let margin_top = 30.0;
    let margin_right = 20.0;
    let plot_width = chart_width - margin_left - margin_right;
    let plot_height = chart_height - margin_bottom - margin_top;

    if data_len == 0 {
        return rsx! {
            div { class: "neteq-advanced-chart",
                div { class: "chart-title", "{config.title}" }
                div { class: "no-data", "No data available" }
            }
        };
    }

    // Generate polylines for each series
    let series_elements: Vec<Element> = config
        .series
        .iter()
        .map(|series| {
            let points: String = series
                .data_points
                .iter()
                .enumerate()
                .map(|(i, &value)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - (value.max(0.0) / config.max_value * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            let color = series.color;
            rsx! {
                polyline { points: "{points}", fill: "none", stroke: "{color}", stroke_width: "2" }
            }
        })
        .collect();

    // Generate legend
    let legend_elements: Vec<Element> = config
        .series
        .iter()
        .enumerate()
        .map(|(i, series)| {
            let y_pos = 15 + (i * 15) as i32;
            let color = series.color;
            let label = series.label;
            rsx! {
                text { x: "5", y: "{y_pos}", fill: "{color}", font_size: "10", "{label}" }
            }
        })
        .collect();

    let ml = margin_left.to_string();
    let mt = margin_top.to_string();
    let ph_mt = (plot_height + margin_top).to_string();
    let cw_mr = (chart_width - margin_right).to_string();
    let ml5 = (margin_left - 5.0).to_string();
    let mid_y = (margin_top + plot_height / 2.0).to_string();
    let ml_pw = (margin_left + plot_width).to_string();
    let ph_mt5 = (plot_height + margin_top + 5.0).to_string();
    let y_zero_label = (plot_height + margin_top + 4.0).to_string();
    let y_mid_label = (margin_top + plot_height / 2.0 + 4.0).to_string();
    let y_max_label = (margin_top + 4.0).to_string();
    let ml10 = (margin_left - 10.0).to_string();
    let ch10 = (chart_height - 10.0).to_string();
    let mid_x = (margin_left + plot_width / 2.0).to_string();
    let half_max = format!("{:.1}", config.max_value / 2.0);
    let max_str = format!("{:.1}", config.max_value);
    let mid_time = format!("{}s", data_len / 2);
    let max_time = format!("{}s", data_len);
    let rotate_transform = format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0);
    let view_box = format!("0 0 {width} {height}");
    let cx2 = (chart_width / 2.0).to_string();

    rsx! {
        div { class: "neteq-advanced-chart",
            div { class: "chart-title", "{config.title}" }
            svg {
                width: "{width}",
                height: "{height}",
                view_box: "{view_box}",
                // Y-axis
                line { x1: "{ml}", y1: "{mt}", x2: "{ml}", y2: "{ph_mt}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // X-axis
                line { x1: "{ml}", y1: "{ph_mt}", x2: "{cw_mr}", y2: "{ph_mt}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // Y-axis tick marks
                line { x1: "{ml5}", y1: "{mt}", x2: "{ml}", y2: "{mt}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                line { x1: "{ml5}", y1: "{mid_y}", x2: "{ml}", y2: "{mid_y}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                line { x1: "{ml5}", y1: "{ph_mt}", x2: "{ml}", y2: "{ph_mt}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // X-axis tick marks
                line { x1: "{ml}", y1: "{ph_mt}", x2: "{ml}", y2: "{ph_mt5}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                line { x1: "{ml_pw}", y1: "{ph_mt}", x2: "{ml_pw}", y2: "{ph_mt5}", stroke: "{theme_color::AXIS}", stroke_width: "1" }
                // Data series
                for elem in series_elements { {elem} }
                // Legend
                for elem in legend_elements { {elem} }
                // Y-axis labels
                text { x: "{ml10}", y: "{y_zero_label}", fill: "{theme_color::TEXT_MUTED}", font_size: "12", text_anchor: "end", "0" }
                text { x: "{ml10}", y: "{y_mid_label}", fill: "{theme_color::TEXT_MUTED}", font_size: "12", text_anchor: "end", "{half_max}" }
                text { x: "{ml10}", y: "{y_max_label}", fill: "{theme_color::TEXT_MUTED}", font_size: "12", text_anchor: "end", "{max_str}" }
                // X-axis time labels
                text { x: "{ml_pw}", y: "{ch10}", fill: "{theme_color::TEXT_MUTED}", font_size: "13", text_anchor: "middle", "0s" }
                text { x: "{mid_x}", y: "{ch10}", fill: "{theme_color::TEXT_MUTED}", font_size: "13", text_anchor: "middle", "{mid_time}" }
                text { x: "{ml}", y: "{ch10}", fill: "{theme_color::TEXT_MUTED}", font_size: "13", text_anchor: "middle", "{max_time}" }
                // Y-axis unit label
                text { x: "5", y: "{mid_y}", fill: "{theme_color::TEXT_MUTED}", font_size: "11", transform: "{rotate_transform}", "{config.y_axis_label}" }
                // Chart title
                text { x: "{cx2}", y: "15", fill: "{theme_color::TEXT_PRIMARY}", font_size: "14", text_anchor: "middle", font_weight: "bold", "{config.title}" }
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
    // (`calls_per_sec`, `avg_frames`) were never populated — the `From<RawNetEqStats>`
    // impl hard-coded both to 0 — so the chart was a permanently flat line at zero.
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
    pub fn buffer_vs_target(stats_history: &[NetEqStats]) -> Self {
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

    pub fn decode_operations(stats_history: &[NetEqStats]) -> Self {
        let max_ops = stats_history
            .iter()
            .map(|s| {
                s.normal_per_sec
                    .max(s.expand_per_sec)
                    .max(s.accelerate_per_sec)
                    .max(s.fast_accelerate_per_sec)
                    .max(s.preemptive_expand_per_sec)
                    .max(s.merge_per_sec)
                    .max(s.comfort_noise_per_sec)
                    .max(s.dtmf_per_sec)
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

    pub fn quality_metrics(stats_history: &[NetEqStats]) -> Self {
        let max_packets = stats_history
            .iter()
            .map(|s| s.packets_awaiting_decode)
            .max()
            .unwrap_or(1)
            .max(1) as f64;
        // Single real series: packets buffered but not yet decoded (queue depth).
        // The former "Underruns" series was dropped (#1131 cleanup) — `underruns`
        // is never populated (hard-coded 0 in `From<RawNetEqStats>`), so it plotted
        // a flat line at zero and the unexplained ×0.3 scale only confused the axis.
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

    pub fn reordering_analysis(stats_history: &[NetEqStats]) -> Self {
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
    stats_history: Vec<NetEqStats>,
    chart_type: AdvancedChartType,
    width: u32,
    height: u32,
) -> Element {
    if stats_history.is_empty() {
        return rsx! {
            div { class: "neteq-advanced-chart",
                div { class: "chart-title", "{chart_type.title()}" }
                div { class: "no-data", "No data available" }
            }
        };
    }

    let config = match chart_type {
        AdvancedChartType::BufferVsTarget => ChartConfig::buffer_vs_target(&stats_history),
        AdvancedChartType::DecodeOperations => ChartConfig::decode_operations(&stats_history),
        AdvancedChartType::QualityMetrics => ChartConfig::quality_metrics(&stats_history),
        AdvancedChartType::ReorderingAnalysis => ChartConfig::reordering_analysis(&stats_history),
    };

    rsx! {
        BaseChart { config: config, data_len: stats_history.len(), width: width, height: height }
    }
}

#[component]
pub fn NetEqStatusDisplay(latest_stats: Option<NetEqStats>) -> Element {
    let common_styles = r#"
        .neteq-status { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; }
        .status-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 12px; padding: 8px; }
        .status-item { background: rgba(255, 255, 255, 0.05); border-radius: 8px; padding: 20px 16px; text-align: center; border: 1px solid rgba(255, 255, 255, 0.1); transition: all 0.2s ease; min-height: 120px; display: flex; flex-direction: column; justify-content: flex-start; align-items: center; gap: 8px; }
        .status-item:hover { background: rgba(255, 255, 255, 0.08); border-color: rgba(255, 255, 255, 0.2); }
        .status-value { font-size: 36px; font-weight: 700; line-height: 1; color: #ffffff; text-shadow: 0 1px 2px rgba(0, 0, 0, 0.3); white-space: nowrap; margin: 0; padding: 0; }
        .status-value.good { color: #10b981; }
        .status-value.warning { color: #f59e0b; }
        .status-label { font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; color: #d1d5db; line-height: 1.2; text-align: center; max-width: 100%; margin: 0; padding: 0; width: 100%; }
        .status-subtitle { font-size: 9px; color: #9ca3af; line-height: 1.3; font-weight: 400; text-align: center; max-width: 100%; margin: 0; padding: 0; width: 100%; }
    "#;

    if let Some(stats) = latest_stats {
        let buffer_class = if stats.buffer_ms == 0 {
            "status-value warning"
        } else if stats.buffer_ms >= (stats.target_ms as f32 * 0.8) as u32
            && stats.buffer_ms <= (stats.target_ms as f32 * 1.2) as u32
        {
            "status-value good"
        } else {
            "status-value"
        };
        // Expand / accelerate rates arrive as Q14 fractions converted to per-mille
        // (‰) by `q14::to_per_mille` (1000‰ = 100%); the reorder rate is per-myriad
        // (‱) from `reorder_rate_permyriad`. The value strings carry the unit so the
        // numbers are interpretable without guessing the scale (cleanup #1131).
        let expand_str = format!("{:.1}\u{2030}", stats.expand_rate);
        let accel_str = format!("{:.1}\u{2030}", stats.accel_rate);

        rsx! {
            style { "{common_styles}" }
            div { class: "neteq-status",
                div { class: "status-grid",
                    div { class: "status-item", div { class: "{buffer_class}", "{stats.buffer_ms}" } div { class: "status-label", "BUFFER (MS)" } div { class: "status-subtitle", "Audio data buffered for playback" } }
                    div { class: "status-item", div { class: "status-value", "{stats.target_ms}" } div { class: "status-label", "TARGET (MS)" } div { class: "status-subtitle", "Optimal buffer size for network" } }
                    div { class: "status-item", div { class: "status-value", "{stats.packets_awaiting_decode}" } div { class: "status-label", "PACKETS" } div { class: "status-subtitle", "Encoded packets awaiting decode" } }
                    div { class: "status-item", div { class: "status-value", "{stats.packets_per_sec}" } div { class: "status-label", "PACKETS/S" } div { class: "status-subtitle", "Audio packets received in the last second" } }
                    div { class: "status-item", div { class: "status-value", "{expand_str}" } div { class: "status-label", "EXPAND RATE (\u{2030})" } div { class: "status-subtitle", "Audio stretched to fill gaps (loss/late) \u{2014} per-mille of output" } }
                    div { class: "status-item", div { class: "status-value", "{accel_str}" } div { class: "status-label", "ACCEL RATE (\u{2030})" } div { class: "status-subtitle", "Audio compressed to drain a full buffer \u{2014} per-mille of output" } }
                    div { class: "status-item", div { class: "status-value", "{stats.reorder_rate}\u{2031}" } div { class: "status-label", "REORDER RATE (\u{2031})" } div { class: "status-subtitle", "Out-of-order packets \u{2014} per-myriad of received" } }
                    div { class: "status-item", div { class: "status-value", "{stats.reordered_packets}" } div { class: "status-label", "REORDERED PACKETS" } div { class: "status-subtitle", "Total packets received out-of-order" } }
                    div { class: "status-item", div { class: "status-value", "{stats.max_reorder_distance}" } div { class: "status-label", "MAX REORDER DISTANCE" } div { class: "status-subtitle", "Largest gap in packet sequence" } }
                }
            }
        }
    } else {
        rsx! {
            style { "{common_styles}" }
            div { class: "neteq-status",
                div { class: "status-grid",
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "BUFFER (MS)" } div { class: "status-subtitle", "Audio data buffered for playback" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "TARGET (MS)" } div { class: "status-subtitle", "Optimal buffer size for network" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "PACKETS" } div { class: "status-subtitle", "Encoded packets awaiting decode" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "PACKETS/S" } div { class: "status-subtitle", "Audio packets received in the last second" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "EXPAND RATE (\u{2030})" } div { class: "status-subtitle", "Audio stretched to fill gaps (loss/late) \u{2014} per-mille of output" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "ACCEL RATE (\u{2030})" } div { class: "status-subtitle", "Audio compressed to drain a full buffer \u{2014} per-mille of output" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "REORDER RATE (\u{2031})" } div { class: "status-subtitle", "Out-of-order packets \u{2014} per-myriad of received" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "REORDERED PACKETS" } div { class: "status-subtitle", "Total packets received out-of-order" } }
                    div { class: "status-item", div { class: "status-value", "--" } div { class: "status-label", "MAX REORDER DISTANCE" } div { class: "status-subtitle", "Largest gap in packet sequence" } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a chart `NetEqStats` from the raw NetEq stats via the real `From`
    /// conversion, so the tests exercise the actual data path (not a hand-rolled
    /// fixture that could drift from production mapping).
    fn raw_with(
        buffer_ms: u16,
        target_ms: u32,
        packets_awaiting: usize,
        expand_q14: u16,
        reorder_permyriad: u16,
        max_reorder_distance: u16,
    ) -> NetEqStats {
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
        raw.into()
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
        let stats = vec![raw_with(80, 100, 5, 0, 0, 0), raw_with(80, 100, 9, 0, 0, 0)];
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
        let stats = vec![raw_with(80, 100, 5, 0, 30, 4)];
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

    /// The `From` conversion renders Q14 expand/accel fractions as per-mille (‰),
    /// which is why the status tiles append the ‰ unit. 4096 Q14 = 250‰ (= 25%).
    #[test]
    fn expand_rate_converts_q14_to_per_mille() {
        let stats = raw_with(80, 100, 0, 4096, 0, 0);
        assert!((stats.expand_rate - 250.0).abs() < 0.01);
    }
}
