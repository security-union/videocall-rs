pub use neteq::NetEqStats as RawNetEqStats;
use serde::{Deserialize, Serialize};
use yew::prelude::*;

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
    pub calls_per_sec: u64,
    pub avg_frames: u64,
    pub underruns: u64,
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
    // Network jitter metrics (RFC 3550)
    pub jitter_ms: i32,
    pub jitter_peaks_found: u16,
}

// Convert from the raw NetEQ structure to the UI structure
impl From<RawNetEqStats> for NetEqStats {
    fn from(raw: RawNetEqStats) -> Self {
        // Calculate calls_per_sec as sum of all decode operations per second
        let calls_per_sec = (raw.network.operation_counters.normal_per_sec
            + raw.network.operation_counters.expand_per_sec
            + raw.network.operation_counters.accelerate_per_sec
            + raw.network.operation_counters.fast_accelerate_per_sec
            + raw.network.operation_counters.preemptive_expand_per_sec
            + raw.network.operation_counters.merge_per_sec
            + raw.network.operation_counters.comfort_noise_per_sec
            + raw.network.operation_counters.dtmf_per_sec
            + raw.network.operation_counters.undefined_per_sec) as u64;

        Self {
            timestamp: raw.timestamp_ms,
            buffer_ms: raw.current_buffer_size_ms,
            target_ms: raw.target_delay_ms,
            packets_awaiting_decode: raw.packets_awaiting_decode as u32,
            packets_per_sec: raw.packets_per_sec,
            expand_rate: neteq::q14::to_per_mille(raw.network.expand_rate), // Convert Q14 to per-mille (‰)
            accel_rate: neteq::q14::to_per_mille(raw.network.accelerate_rate), // Convert Q14 to per-mille (‰)
            calls_per_sec,
            // avg_frames represents the total number of audio frames emitted from the jitter buffer
            avg_frames: raw.lifetime.jitter_buffer_emitted_count,
            // underruns maps to concealment_events (times the buffer ran empty and we had to expand)
            underruns: raw.lifetime.concealment_events,
            reorder_rate: raw.network.reorder_rate_permyriad as u32,
            reordered_packets: raw.network.reordered_packets,
            max_reorder_distance: raw.network.max_reorder_distance as u32,
            sequence_number: 0, // Not available in raw data (RTP-specific)
            rtp_timestamp: 0,   // Not available in raw data (RTP-specific)
            normal_per_sec: raw.network.operation_counters.normal_per_sec,
            expand_per_sec: raw.network.operation_counters.expand_per_sec,
            accelerate_per_sec: raw.network.operation_counters.accelerate_per_sec,
            fast_accelerate_per_sec: raw.network.operation_counters.fast_accelerate_per_sec,
            preemptive_expand_per_sec: raw.network.operation_counters.preemptive_expand_per_sec,
            merge_per_sec: raw.network.operation_counters.merge_per_sec,
            comfort_noise_per_sec: raw.network.operation_counters.comfort_noise_per_sec,
            dtmf_per_sec: raw.network.operation_counters.dtmf_per_sec,
            undefined_per_sec: raw.network.operation_counters.undefined_per_sec,
            jitter_ms: raw.network.jitter_ms,
            jitter_peaks_found: raw.network.jitter_peaks_found,
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

#[derive(Properties, PartialEq)]
pub struct BaseChartProps {
    pub config: ChartConfig,
    pub data_len: usize,
    pub width: u32,
    pub height: u32,
}

#[function_component(BaseChart)]
pub fn base_chart(props: &BaseChartProps) -> Html {
    let BaseChartProps {
        config,
        data_len,
        width,
        height,
    } = props;

    let chart_width = *width as f64;
    let chart_height = *height as f64;
    let margin_left = 60.0;
    let margin_bottom = 40.0;
    let margin_top = 30.0;
    let margin_right = 20.0;
    let plot_width = chart_width - margin_left - margin_right;
    let plot_height = chart_height - margin_bottom - margin_top;

    if *data_len == 0 {
        return html! {
            <div class="neteq-advanced-chart">
                <div class="chart-title">{ config.title }</div>
                <div class="no-data">{"No data available"}</div>
            </div>
        };
    }

    // Generate legend
    let legend_elements: Vec<Html> = config.series.iter().enumerate().map(|(i, series)| {
        let y_pos = 15 + (i * 15) as i32;
        html! {
            <text x="5" y={y_pos.to_string()} fill={series.color} font-size="10">{series.label}</text>
        }
    }).collect();

    // Generate polylines for each series with REVERSED x-axis (newest data on right)
    let series_elements: Vec<Html> = config
        .series
        .iter()
        .map(|series| {
            let points: String = series
                .data_points
                .iter()
                .enumerate()
                .map(|(i, &value)| {
                    let x = margin_left + (i as f64 / (*data_len - 1).max(1) as f64 * plot_width);
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
            html! {
                <polyline points={points} fill="none" stroke={series.color} stroke-width="2" />
            }
        })
        .collect();
    html! {
        <div class="neteq-advanced-chart">
            <div class="chart-title">{ config.title }</div>
            <svg width="100%" height="100%" viewBox={format!("0 0 {width} {height}")}>
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
                <line x1={(margin_left + plot_width).to_string()} y1={(plot_height + margin_top).to_string()} x2={(margin_left + plot_width).to_string()} y2={(plot_height + margin_top + 5.0).to_string()} stroke="#666" stroke-width="1" />

                // Data series
                { for series_elements }

                // Legend
                { for legend_elements }

                // Y-axis labels
                <text x={(margin_left - 10.0).to_string()} y={(plot_height + margin_top + 4.0).to_string()} fill="#aaa" font-size="12" text-anchor="end">{"0"}</text>
                <text x={(margin_left - 10.0).to_string()} y={(margin_top + plot_height / 2.0 + 4.0).to_string()} fill="#aaa" font-size="12" text-anchor="end">{format!("{:.1}", config.max_value / 2.0)}</text>
                <text x={(margin_left - 10.0).to_string()} y={(margin_top + 4.0).to_string()} fill="#aaa" font-size="12" text-anchor="end">{format!("{:.1}", config.max_value)}</text>

                // X-axis time labels - REVERSED (0s on right, older time on left)
                <text x={(margin_left + plot_width).to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="13" text-anchor="middle">{"0s"}</text>
                <text x={(margin_left + plot_width / 2.0).to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="13" text-anchor="middle">{ format!("{}s", data_len / 2) }</text>
                <text x={margin_left.to_string()} y={(chart_height - 10.0).to_string()} fill="#aaa" font-size="13" text-anchor="middle">{ format!("{}s", data_len) }</text>

                // Y-axis unit label
                <text x="5" y={(margin_top + plot_height / 2.0).to_string()} fill="#aaa" font-size="11" transform={format!("rotate(-90, 5, {})", margin_top + plot_height / 2.0)}>{config.y_axis_label}</text>

                // Chart title
                <text x={(chart_width / 2.0).to_string()} y="15" fill="#fff" font-size="14" text-anchor="middle" font-weight="bold">{ config.title }</text>
            </svg>
        </div>
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

#[derive(PartialEq, Eq, Clone)]
pub enum AdvancedChartType {
    BufferVsTarget,
    DecodeOperations,
    QualityMetrics,
    ReorderingAnalysis,
    SystemPerformance,
    NetworkJitter,
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
    pub fn title(&self) -> &'static str {
        match self {
            AdvancedChartType::BufferVsTarget => "Audio Buffer Size vs Target",
            AdvancedChartType::DecodeOperations => "Audio Decode Operations Per Second",
            AdvancedChartType::QualityMetrics => "Audio Packet Count & Quality",
            AdvancedChartType::ReorderingAnalysis => "Audio Packet Reordering",
            AdvancedChartType::SystemPerformance => "Audio Processing Performance",
            AdvancedChartType::NetworkJitter => "Audio Packet Jitter & Timing",
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
            <svg width="100%" height="100%" viewBox={format!("0 0 {width} {height}")} preserveAspectRatio="none">
                // Y-axis
                <line x1={margin_left.to_string()} y1="5" x2={margin_left.to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />
                // X-axis
                <line x1={margin_left.to_string()} y1={(plot_height + 5.0).to_string()} x2={(chart_width - 5.0).to_string()} y2={(plot_height + 5.0).to_string()} stroke="#666" stroke-width="1" />

                // Data line
                if !points.is_empty() {
                    <polyline points={points} fill="none" stroke={chart_type.stroke_color()} stroke-width="2" />
                }

                // Y-axis labels
                <text x="0" y="10" fill="#888" font-size="11">{ max_val }</text>
                <text x="0" y={(plot_height + 5.0).to_string()} fill="#888" font-size="11">{"0"}</text>

                // X-axis labels
                <text x={margin_left.to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="11">{"0s"}</text>
                <text x={(chart_width - 20.0).to_string()} y={(chart_height - 1.0).to_string()} fill="#888" font-size="11">{ format!("{}s", time_span) }</text>
            </svg>
        </div>
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
            title: "Audio Buffer Size vs Target",
            y_axis_label: "Buffer (ms)",
            max_value: max_buffer,
            series: vec![
                ChartSeries {
                    data_points: buffer_data,
                    color: "#007bff",
                    label: "Current Buffer",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: target_data,
                    color: "#28a745",
                    label: "Target Buffer",
                    scale_factor: 1.0,
                },
            ],
        }
    }

    pub fn decode_operations(stats_history: &[NetEqStats]) -> Self {
        // Find max operations per second across all operation types
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

        // Extract data for the most important operation types
        let normal_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.normal_per_sec as f64)
            .collect();
        let expand_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.expand_per_sec as f64)
            .collect();
        let accelerate_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.accelerate_per_sec as f64)
            .collect();
        let preemptive_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.preemptive_expand_per_sec as f64)
            .collect();
        let merge_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.merge_per_sec as f64)
            .collect();

        Self {
            title: "Audio Decode Operations Per Second",
            y_axis_label: "Operations/sec",
            max_value: max_ops,
            series: vec![
                ChartSeries {
                    data_points: normal_data,
                    color: "#28a745", // Green for normal operation
                    label: "Normal",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: expand_data,
                    color: "#dc3545", // Red for packet loss concealment
                    label: "Expand",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: accelerate_data,
                    color: "#fd7e14", // Orange for time compression
                    label: "Accelerate",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: preemptive_data,
                    color: "#6f42c1", // Purple for preemptive expansion
                    label: "Preemptive Expand",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: merge_data,
                    color: "#17a2b8", // Cyan for merge operations
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
        let _max_underruns = stats_history
            .iter()
            .map(|s| s.underruns)
            .max()
            .unwrap_or(1)
            .max(1) as f64;

        let packet_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.packets_awaiting_decode as f64)
            .collect();

        let underrun_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.underruns as f64 * 0.3) // Scale underruns to fit on chart
            .collect();

        Self {
            title: "Audio Packet Count & Quality",
            y_axis_label: "Count",
            max_value: max_packets,
            series: vec![
                ChartSeries {
                    data_points: packet_data,
                    color: "#6f42c1",
                    label: "Packets",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: underrun_data,
                    color: "#dc3545",
                    label: "Underruns",
                    scale_factor: 0.3,
                },
            ],
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

        let rate_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.reorder_rate as f64)
            .collect();

        let distance_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.max_reorder_distance as f64)
            .collect();

        Self {
            title: "Audio Packet Reordering",
            y_axis_label: "Rate/Distance",
            max_value: max_rate.max(max_distance),
            series: vec![
                ChartSeries {
                    data_points: rate_data,
                    color: "#dc3545",
                    label: "Reorder Rate",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: distance_data,
                    color: "#17a2b8",
                    label: "Max Distance",
                    scale_factor: 1.0,
                },
            ],
        }
    }

    pub fn system_performance(stats_history: &[NetEqStats]) -> Self {
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

        let calls_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.calls_per_sec as f64)
            .collect();

        let frames_data: Vec<f64> = stats_history.iter().map(|s| s.avg_frames as f64).collect();

        Self {
            title: "Audio Processing Performance",
            y_axis_label: "Performance",
            max_value: max_calls.max(max_frames),
            series: vec![
                ChartSeries {
                    data_points: calls_data,
                    color: "#28a745",
                    label: "Calls/sec",
                    scale_factor: 1.0,
                },
                ChartSeries {
                    data_points: frames_data,
                    color: "#ffc107",
                    label: "Avg Frames",
                    scale_factor: 1.0,
                },
            ],
        }
    }

    pub fn network_jitter(stats_history: &[NetEqStats]) -> Self {
        // RFC 3550 interarrival jitter (EWMA of transit time deviation)
        let jitter_data: Vec<f64> = stats_history
            .iter()
            .map(|s| s.jitter_ms.max(0) as f64)
            .collect();

        let max_value = jitter_data.iter().cloned().fold(1.0f64, f64::max);

        Self {
            title: "Audio Packet Jitter & Timing",
            y_axis_label: "Jitter (ms)",
            max_value,
            series: vec![ChartSeries {
                data_points: jitter_data,
                color: "#dc3545",
                label: "RFC 3550 Jitter",
                scale_factor: 1.0,
            }],
        }
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

    if stats_history.is_empty() {
        return html! {
            <div class="neteq-advanced-chart">
                <div class="chart-title">{ chart_type.title() }</div>
                <div class="no-data">{"No data available"}</div>
            </div>
        };
    }

    let config = match chart_type {
        AdvancedChartType::BufferVsTarget => ChartConfig::buffer_vs_target(stats_history),
        AdvancedChartType::DecodeOperations => ChartConfig::decode_operations(stats_history),
        AdvancedChartType::QualityMetrics => ChartConfig::quality_metrics(stats_history),
        AdvancedChartType::ReorderingAnalysis => ChartConfig::reordering_analysis(stats_history),
        AdvancedChartType::SystemPerformance => ChartConfig::system_performance(stats_history),
        AdvancedChartType::NetworkJitter => ChartConfig::network_jitter(stats_history),
    };

    html! {
        <BaseChart config={config} data_len={stats_history.len()} width={*width} height={*height} />
    }
}

#[function_component(NetEqStatusDisplay)]
pub fn neteq_status_display(props: &NetEqStatusDisplayProps) -> Html {
    let NetEqStatusDisplayProps { latest_stats } = props;

    // Stats logging moved to neteq_worker.rs (every 5s; UI stats emission remains 1Hz)

    // Common CSS styles for both branches
    let common_styles = r#"
        .neteq-status {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
        }
        
        .status-grid {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
            gap: 12px;
            padding: 8px;
        }
        
        .status-item {
            background: rgba(255, 255, 255, 0.05);
            border-radius: 8px;
            padding: 20px 16px;
            text-align: center;
            border: 1px solid rgba(255, 255, 255, 0.1);
            transition: all 0.2s ease;
            min-height: 120px;
            display: flex;
            flex-direction: column;
            justify-content: flex-start;
            align-items: center;
            gap: 8px;
        }
        
        .status-item:hover {
            background: rgba(255, 255, 255, 0.08);
            border-color: rgba(255, 255, 255, 0.2);
        }
        
        .status-value {
            font-size: 36px;
            font-weight: 700;
            line-height: 1;
            color: #ffffff;
            text-shadow: 0 1px 2px rgba(0, 0, 0, 0.3);
            white-space: nowrap;
            margin: 0;
            padding: 0;
        }
        
        .status-value.good {
            color: #10b981;
        }
        
        .status-value.warning {
            color: #f59e0b;
        }
        
        .status-label {
            font-size: 11px;
            font-weight: 600;
            text-transform: uppercase;
            letter-spacing: 0.5px;
            color: #d1d5db;
            line-height: 1.2;
            word-wrap: break-word;
            hyphens: auto;
            text-align: center;
            max-width: 100%;
            margin: 0;
            padding: 0;
            width: 100%;
        }
        
        .status-subtitle {
            font-size: 9px;
            color: #9ca3af;
            line-height: 1.3;
            font-weight: 400;
            word-wrap: break-word;
            hyphens: auto;
            text-align: center;
            max-width: 100%;
            margin: 0;
            padding: 0;
            width: 100%;
        }
    "#;

    if let Some(stats) = latest_stats {
        // Detect stale data purely from the current stats snapshot (no state tracking needed).
        // Don't use buffer_ms - it can retain residual data (63ms + 3 packets) after mute
        // if the flush didn't fully complete before the stats snapshot.
        // The reliable signals are: no packets arriving AND no decode operations happening.
        let is_stale = stats.packets_per_sec == 0 && stats.normal_per_sec == 0.0;

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
            <>
                <style>{common_styles}</style>
                <div class="neteq-status">
                    <div class="status-grid">
                        <div class="status-item">
                            <div class={buffer_class}>
                                {if is_stale { "--".to_string() } else { stats.buffer_ms.to_string() }}
                            </div>
                            <div class="status-label">{"BUFFER (MS)"}</div>
                            <div class="status-subtitle">{"Audio data buffered for playback"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{stats.target_ms}</div>
                            <div class="status-label">{"TARGET (MS)"}</div>
                            <div class="status-subtitle">{"Optimal buffer size for network"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { stats.packets_awaiting_decode.to_string() }}
                            </div>
                            <div class="status-label">{"PACKETS"}</div>
                            <div class="status-subtitle">{"Encoded packets awaiting decode"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { stats.packets_per_sec.to_string() }}
                            </div>
                            <div class="status-label">{"PACKETS/S"}</div>
                            <div class="status-subtitle">{"Audio packets received in the last second"}</div>
                        </div>
                        <div class="status-item">
                            <div class={underrun_class}>
                                {if is_stale { "--".to_string() } else { stats.underruns.to_string() }}
                            </div>
                            <div class="status-label">{"UNDERRUNS (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Total times audio buffer ran empty"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { format!("{:.1}", stats.expand_rate) }}
                            </div>
                            <div class="status-label">{"EXPAND RATE (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Audio stretching when buffer low (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { format!("{:.1}", stats.accel_rate) }}
                            </div>
                            <div class="status-label">{"ACCEL RATE (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Audio compression when buffer full (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{stats.reorder_rate}</div>
                            <div class="status-label">{"REORDER RATE"}</div>
                            <div class="status-subtitle">{"Out-of-order packet frequency (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{stats.reordered_packets}</div>
                            <div class="status-label">{"REORDERED PACKETS (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Total packets received out-of-order"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{stats.max_reorder_distance}</div>
                            <div class="status-label">{"MAX REORDER DISTANCE"}</div>
                            <div class="status-subtitle">{"Largest gap in packet sequence"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { format!("{}", stats.jitter_ms) }}
                            </div>
                            <div class="status-label">{"JITTER (MS)"}</div>
                            <div class="status-subtitle">{"Mean deviation of packet arrival timing"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">
                                {if is_stale { "--".to_string() } else { stats.jitter_peaks_found.to_string() }}
                            </div>
                            <div class="status-label">{"JITTER SPIKES"}</div>
                            <div class="status-subtitle">{"Packets with abnormal transit delay"}</div>
                        </div>
                    </div>
                </div>
            </>
        }
    } else {
        html! {
            <>
                <style>{common_styles}</style>
                <div class="neteq-status">
                    <div class="status-grid">
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"BUFFER (MS)"}</div>
                            <div class="status-subtitle">{"Audio data buffered for playback"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"TARGET (MS)"}</div>
                            <div class="status-subtitle">{"Optimal buffer size for network"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"PACKETS"}</div>
                            <div class="status-subtitle">{"Encoded packets awaiting decode"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"PACKETS/S"}</div>
                            <div class="status-subtitle">{"Audio packets received in the last second"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"UNDERRUNS (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Total times audio buffer ran empty"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"EXPAND RATE (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Audio stretching when buffer low (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"ACCEL RATE (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Audio compression when buffer full (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"REORDER RATE"}</div>
                            <div class="status-subtitle">{"Out-of-order packet frequency (‰)"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"REORDERED PACKETS (LIFETIME)"}</div>
                            <div class="status-subtitle">{"Total packets received out-of-order"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"MAX REORDER DISTANCE"}</div>
                            <div class="status-subtitle">{"Largest gap in packet sequence"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"JITTER (MS)"}</div>
                            <div class="status-subtitle">{"Mean deviation of packet arrival timing"}</div>
                        </div>
                        <div class="status-item">
                            <div class="status-value">{"--"}</div>
                            <div class="status-label">{"JITTER SPIKES"}</div>
                            <div class="status-subtitle">{"Packets with abnormal transit delay"}</div>
                        </div>
                    </div>
                </div>
            </>
        }
    }
}
