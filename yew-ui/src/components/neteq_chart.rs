use serde::{Deserialize, Serialize};
use yew::prelude::*;

// NetEQ structures from the actual NetEQ crate (for parsing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkStatistics {
    pub current_buffer_size_ms: u16,
    pub preferred_buffer_size_ms: u16,
    pub jitter_peaks_found: u16,
    pub expand_rate: u16,
    pub speech_expand_rate: u16,
    pub preemptive_rate: u16,
    pub accelerate_rate: u16,
    pub mean_waiting_time_ms: i32,
    pub median_waiting_time_ms: i32,
    pub min_waiting_time_ms: i32,
    pub max_waiting_time_ms: i32,
    pub reordered_packets: u32,
    pub total_packets_received: u32,
    pub reorder_rate_permyriad: u16,
    pub max_reorder_distance: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LifetimeStatistics {
    pub total_samples_received: u64,
    pub concealed_samples: u64,
    pub concealment_events: u64,
    pub jitter_buffer_delay_ms: u64,
    pub jitter_buffer_emitted_count: u64,
    pub jitter_buffer_target_delay_ms: u64,
    pub inserted_samples_for_deceleration: u64,
    pub removed_samples_for_acceleration: u64,
    pub silent_concealed_samples: u64,
    pub relative_packet_arrival_delay_ms: u64,
    pub jitter_buffer_packets_received: u64,
    pub buffer_flushes: u64,
    pub late_packets_discarded: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawNetEqStats {
    pub network: NetworkStatistics,
    pub lifetime: LifetimeStatistics,
    pub current_buffer_size_ms: u32,
    pub target_delay_ms: u32,
    pub packet_count: usize,
}

// UI-friendly structure for charts (keeping the old one)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NetEqStats {
    pub timestamp: u64,
    pub buffer_ms: u32,
    pub target_ms: u32,
    pub packets: u32,
    pub expand_rate: f32,
    pub accel_rate: f32,
    pub calls_per_sec: u64,
    pub avg_frames: u64,
    pub underruns: u64,
    pub reorder_rate: u32,
    pub reordered_packets: u32,
    pub max_reorder_distance: u32,
    pub sequence_number: u32,
    pub rtp_timestamp: u32,
}

// Convert from the raw NetEQ structure to the UI structure
impl From<RawNetEqStats> for NetEqStats {
    fn from(raw: RawNetEqStats) -> Self {
        Self {
            timestamp: 0, // We don't have a timestamp in the raw data, use 0 or current time
            buffer_ms: raw.current_buffer_size_ms,
            target_ms: raw.target_delay_ms,
            packets: raw.packet_count as u32,
            expand_rate: raw.network.expand_rate as f32,
            accel_rate: raw.network.accelerate_rate as f32,
            calls_per_sec: 0, // Not available in raw data
            avg_frames: 0,    // Not available in raw data
            underruns: 0,     // Not available in raw data (could map from concealment events)
            reorder_rate: raw.network.reorder_rate_permyriad as u32,
            reordered_packets: raw.network.reordered_packets,
            max_reorder_distance: raw.network.max_reorder_distance as u32,
            sequence_number: 0, // Not available in raw data
            rtp_timestamp: 0,   // Not available in raw data
        }
    }
}

#[derive(Properties, PartialEq)]
pub struct NetEqChartProps {
    pub data: Vec<u64>,
    pub chart_type: ChartType,
    pub width: u32,
    pub height: u32,
}

#[derive(Properties, PartialEq)]
pub struct NetEqAdvancedChartProps {
    pub stats_history: Vec<NetEqStats>,
    pub chart_type: AdvancedChartType,
    pub width: u32,
    pub height: u32,
}

#[derive(Properties, PartialEq)]
pub struct NetEqStatusDisplayProps {
    pub latest_stats: Option<NetEqStats>,
}

#[derive(PartialEq, Clone)]
pub enum ChartType {
    Buffer,
    Jitter,
}

#[derive(PartialEq, Clone)]
pub enum AdvancedChartType {
    BufferVsTarget,
    NetworkAdaptation,
    QualityMetrics,
    ReorderingAnalysis,
    SystemPerformance,
}

impl ChartType {
    fn stroke_color(&self) -> &'static str {
        match self {
            ChartType::Buffer => "#8ef",
            ChartType::Jitter => "#ff8",
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
            AdvancedChartType::NetworkAdaptation => "Network Adaptation Rates",
            AdvancedChartType::QualityMetrics => "Packet Count & Audio Quality",
            AdvancedChartType::ReorderingAnalysis => "Packet Reordering Analysis",
            AdvancedChartType::SystemPerformance => "System Performance",
        }
    }
}

#[function_component(NetEqChart)]
pub fn neteq_chart(props: &NetEqChartProps) -> Html {
    let NetEqChartProps {
        data,
        chart_type,
        width,
        height,
    } = props;

    let chart_width = *width as f64;
    let chart_height = *height as f64;
    let margin_left = 25.0;
    let margin_bottom = 15.0;
    let plot_width = chart_width - margin_left - 10.0;
    let plot_height = chart_height - margin_bottom - 5.0;

    let max_val = *data.iter().max().unwrap_or(&1);
    let max_val_f64 = max_val as f64;
    let data_len = data.len();

    // Generate polyline points
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

    html! {
        <div class="neteq-chart">
            <div class="chart-title">{ chart_type.label() }</div>
            <svg width={width.to_string()} height={height.to_string()} viewBox={format!("0 0 {width} {height}")} preserveAspectRatio="none">
                // Y-axis
                <line x1={margin_left.to_string()} y1="5" x2={margin_left.to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />
                // X-axis
                <line x1={margin_left.to_string()} y1={(plot_height + 5.0).to_string()} x2={(chart_width - 5.0).to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />

                // Data line
                if !points.is_empty() {
                    <polyline points={points} fill="none" stroke={chart_type.stroke_color()} stroke-width="2" />
                }

                // Y-axis labels
                <text x="0" y="10" fill="#888" font-size="8">{ max_val }</text>
                <text x="0" y={(plot_height + 5.0).to_string()} fill="#888" font-size="8">{"0"}</text>

                // X-axis labels
                <text x={margin_left.to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="8">{"0s"}</text>
                <text x={(chart_width - 20.0).to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="8">{ format!("{}s", time_span) }</text>
            </svg>
        </div>
    }
}

#[function_component(NetEqAdvancedChart)]
pub fn neteq_advanced_chart(props: &NetEqAdvancedChartProps) -> Html {
    let NetEqAdvancedChartProps {
        stats_history,
        chart_type,
        width,
        height,
    } = props;

    let chart_width = *width as f64;
    let chart_height = *height as f64;
    let margin_left = 60.0; // Increased for Y-axis labels and values
    let margin_bottom = 40.0; // Increased for X-axis labels
    let margin_top = 30.0; // Increased for title space
    let margin_right = 20.0;
    let plot_width = chart_width - margin_left - margin_right;
    let plot_height = chart_height - margin_bottom - margin_top;

    if stats_history.is_empty() {
        return html! {
            <div class="neteq-advanced-chart">
                <div class="chart-title">{ chart_type.title() }</div>
                <div class="no-data">{"No data available"}</div>
            </div>
        };
    }

    let data_len = stats_history.len();

    let chart_content = match chart_type {
        AdvancedChartType::BufferVsTarget => {
            let max_buffer = stats_history
                .iter()
                .map(|s| s.buffer_ms.max(s.target_ms))
                .max()
                .unwrap_or(1)
                .max(1) as f64; // Ensure min value of 1

            let buffer_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.buffer_ms as f64).max(0.0) / max_buffer * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}") // Default to bottom if invalid
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            let target_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.target_ms as f64).max(0.0) / max_buffer * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}") // Default to bottom if invalid
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            html! {
                <>
                    <polyline points={buffer_points} fill="none" stroke="#007bff" stroke-width="2" />
                    <polyline points={target_points} fill="none" stroke="#28a745" stroke-width="2" />
                    <text x="5" y="15" fill="#007bff" font-size="10">{"Current Buffer"}</text>
                    <text x="5" y="30" fill="#28a745" font-size="10">{"Target Buffer"}</text>

                    // Y-axis labels (positioned to the left of the axis)
                    <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{"0"}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", (max_buffer / 2.0) as u32)}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", max_buffer as u32)}</text>

                    // Y-axis unit label
                    <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="8" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{"Buffer (ms)"}</text>
                </>
            }
        }
        AdvancedChartType::NetworkAdaptation => {
            let max_rate = stats_history
                .iter()
                .map(|s| s.expand_rate.max(s.accel_rate))
                .fold(1.0f32, f32::max)
                .max(1.0) as f64; // Ensure min value of 1

            let expand_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.expand_rate as f64).max(0.0) / max_rate * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            let accel_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.accel_rate as f64).max(0.0) / max_rate * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            html! {
                <>
                    <polyline points={expand_points} fill="none" stroke="#dc3545" stroke-width="2" />
                    <polyline points={accel_points} fill="none" stroke="#fd7e14" stroke-width="2" />
                    <text x="5" y="15" fill="#dc3545" font-size="10">{"Expand Rate"}</text>
                    <text x="5" y="30" fill="#fd7e14" font-size="10">{"Accel Rate"}</text>

                    // Y-axis labels
                    <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{"0"}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{:.1}", max_rate / 2.0)}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{:.1}", max_rate)}</text>

                    // Y-axis unit label
                    <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="8" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{"Rate (‰)"}</text>
                </>
            }
        }
        AdvancedChartType::QualityMetrics => {
            let max_packets = stats_history
                .iter()
                .map(|s| s.packets)
                .max()
                .unwrap_or(1)
                .max(1) as f64;
            let max_underruns = stats_history
                .iter()
                .map(|s| s.underruns)
                .max()
                .unwrap_or(1)
                .max(1) as f64;

            let packet_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.packets as f64).max(0.0) / max_packets * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            // Scale underruns to fit on chart
            let underrun_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.underruns as f64).max(0.0) / max_underruns * plot_height * 0.3);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            html! {
                <>
                    <polyline points={packet_points} fill="none" stroke="#6f42c1" stroke-width="2" />
                    <polyline points={underrun_points} fill="none" stroke="#e83e8c" stroke-width="2" />
                    <text x="5" y="15" fill="#6f42c1" font-size="10">{"Packets"}</text>
                    <text x="5" y="30" fill="#e83e8c" font-size="10">{"Underruns"}</text>

                    // Y-axis labels
                    <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{"0"}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", (max_packets / 2.0) as u32)}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", max_packets as u32)}</text>

                    // Y-axis unit label
                    <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="8" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{"Count"}</text>
                </>
            }
        }
        AdvancedChartType::ReorderingAnalysis => {
            let max_reorder_rate = stats_history
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

            let reorder_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.reorder_rate as f64).max(0.0) / max_reorder_rate * plot_height);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            let distance_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.max_reorder_distance as f64).max(0.0) / max_distance
                            * plot_height
                            * 0.5);
                    if y.is_finite() {
                        format!("{x:.1},{y:.1}")
                    } else {
                        let height = margin_top + plot_height;
                        format!("{x:.1},{height:.1}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");

            html! {
                <>
                    <polyline points={reorder_points} fill="none" stroke="#ff6b6b" stroke-width="2" />
                    <polyline points={distance_points} fill="none" stroke="#4ecdc4" stroke-width="2" />
                    <text x="5" y="15" fill="#ff6b6b" font-size="10">{"Reorder Rate"}</text>
                    <text x="5" y="30" fill="#4ecdc4" font-size="10">{"Max Distance"}</text>

                    // Y-axis labels
                    <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{"0"}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", (max_reorder_rate / 2.0) as u32)}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", max_reorder_rate as u32)}</text>

                    // Y-axis unit label
                    <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="8" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{"Rate/Distance"}</text>
                </>
            }
        }
        AdvancedChartType::SystemPerformance => {
            let max_calls = stats_history
                .iter()
                .map(|s| s.calls_per_sec)
                .max()
                .unwrap_or(1)
                .max(1) as f64;
            let max_frames = stats_history
                .iter()
                .map(|s| s.avg_frames)
                .max()
                .unwrap_or(1)
                .max(1) as f64;

            let calls_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.calls_per_sec as f64).max(0.0) / max_calls * plot_height);
                    format!("{x:.1},{y:.1}")
                })
                .collect::<Vec<_>>()
                .join(" ");

            let frames_points: String = stats_history
                .iter()
                .enumerate()
                .map(|(i, stats)| {
                    let x = margin_left + (i as f64 / (data_len - 1).max(1) as f64 * plot_width);
                    let y = margin_top + plot_height
                        - ((stats.avg_frames as f64).max(0.0) / max_frames * plot_height * 0.5);
                    format!("{x:.1},{y:.1}")
                })
                .collect::<Vec<_>>()
                .join(" ");

            html! {
                <>
                    <polyline points={calls_points} fill="none" stroke="#20c997" stroke-width="2" />
                    <polyline points={frames_points} fill="none" stroke="#ffc107" stroke-width="2" />
                    <text x="5" y="15" fill="#20c997" font-size="10">{"Calls/sec"}</text>
                    <text x="5" y="30" fill="#ffc107" font-size="10">{"Avg Frames"}</text>

                    // Y-axis labels
                    <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{"0"}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", (max_calls / 2.0) as u64)}</text>
                    <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="9" text-anchor="end">{format!("{}", max_calls as u64)}</text>

                    // Y-axis unit label
                    <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="8" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{"Performance"}</text>
                </>
            }
        }
    };

    html! {
        <div class="neteq-advanced-chart">
            <div class="chart-title">{ chart_type.title() }</div>
            <svg width={width.to_string()} height={height.to_string()} viewBox={format!("0 0 {width} {height}")}>
                // Y-axis
                <line x1={margin_left.to_string()} y1={margin_top.to_string()} x2={margin_left.to_string()} y2={(plot_height + margin_top).to_string()} stroke="#666" stroke-width="1" />
                // X-axis
                <line x1={margin_left.to_string()} y1={(plot_height + margin_top).to_string()} x2={(chart_width - margin_right).to_string()} y2={(plot_height + margin_top).to_string()} stroke="#666" stroke-width="1" />

                // Y-axis tick marks and labels
                <line x1={(margin_left - 5.0).to_string()} y1={margin_top.to_string()} x2={margin_left.to_string()} y2={margin_top.to_string()} stroke="#666" stroke-width="1" />
                <line x1={(margin_left - 5.0).to_string()} y1={(margin_top + plot_height / 2.0).to_string()} x2={margin_left.to_string()} y2={(margin_top + plot_height / 2.0).to_string()} stroke="#666" stroke-width="1" />
                <line x1={(margin_left - 5.0).to_string()} y1={(plot_height + margin_top).to_string()} x2={margin_left.to_string()} y2={(plot_height + margin_top).to_string()} stroke="#666" stroke-width="1" />

                // X-axis tick marks
                <line x1={margin_left.to_string()} y1={(plot_height + margin_top).to_string()} x2={margin_left.to_string()} y2={(plot_height + margin_top + 5.0).to_string()} stroke="#666" stroke-width="1" />
                <line x1={(margin_left + plot_width / 2.0).to_string()} y1={(plot_height + margin_top).to_string()} x2={(margin_left + plot_width / 2.0).to_string()} y2={(plot_height + margin_top + 5.0).to_string()} stroke="#666" stroke-width="1" />
                <line x1={(margin_left + plot_width).to_string()} y1={(plot_height + margin_top).to_string()} x2={(margin_left + plot_width).to_string()} y2={(plot_height + margin_top + 5.0).to_string()} stroke="#666" stroke-width="1" />

                { chart_content }

                // X-axis time labels with better positioning
                <text x={margin_left.to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="10" text-anchor="middle">{"0s"}</text>
                <text x={(margin_left + plot_width / 2.0).to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="10" text-anchor="middle">{ format!("{}s", data_len / 2) }</text>
                <text x={(margin_left + plot_width).to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="10" text-anchor="middle">{ format!("{}s", data_len) }</text>

                // Chart title
                <text x={(chart_width / 2.0).to_string()} y="15" fill="#fff" font-size="12" text-anchor="middle" font-weight="bold">{ chart_type.title() }</text>
            </svg>
        </div>
    }
}

#[function_component(NetEqStatusDisplay)]
pub fn neteq_status_display(props: &NetEqStatusDisplayProps) -> Html {
    let NetEqStatusDisplayProps { latest_stats } = props;

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

        let underrun_class = if stats.underruns > 0 {
            "status-value warning"
        } else {
            "status-value good"
        };

        html! {
            <div class="neteq-status">
                <div class="status-grid">
                    <div class="status-item">
                        <div class={buffer_class}>{stats.buffer_ms}</div>
                        <div class="status-label">{"Buffer (ms)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{stats.target_ms}</div>
                        <div class="status-label">{"Target (ms)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{stats.packets}</div>
                        <div class="status-label">{"Packets"}</div>
                    </div>
                    <div class="status-item">
                        <div class={underrun_class}>{stats.underruns}</div>
                        <div class="status-label">{"Underruns"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{format!("{:.1}", stats.expand_rate)}</div>
                        <div class="status-label">{"Expand Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{format!("{:.1}", stats.accel_rate)}</div>
                        <div class="status-label">{"Accel Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{stats.reorder_rate}</div>
                        <div class="status-label">{"Reorder Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{stats.reordered_packets}</div>
                        <div class="status-label">{"Reordered Packets"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{stats.max_reorder_distance}</div>
                        <div class="status-label">{"Max Reorder Distance"}</div>
                    </div>
                </div>
            </div>
        }
    } else {
        html! {
            <div class="neteq-status">
                <div class="status-grid">
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Buffer (ms)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Target (ms)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Packets"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Underruns"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Expand Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Accel Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Reorder Rate (‰)"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Reordered Packets"}</div>
                    </div>
                    <div class="status-item">
                        <div class="status-value">{"--"}</div>
                        <div class="status-label">{"Max Reorder Distance"}</div>
                    </div>
                </div>
            </div>
        }
    }
}
