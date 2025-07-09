/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::components::neteq_chart::{
    AdvancedChartType, ChartType, NetEqAdvancedChart, NetEqChart, NetEqStats, NetEqStatusDisplay,
};
use std::collections::HashMap;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct DiagnosticsProps {
    /// Whether the diagnostics sidebar is open
    pub is_open: bool,
    /// Callback to close the diagnostics sidebar
    pub on_close: Callback<()>,
    /// Reception diagnostics data
    pub diagnostics_data: Option<String>,
    /// Sending statistics data
    pub sender_stats: Option<String>,
    /// Encoder settings data
    pub encoder_settings: Option<String>,
    /// NetEQ statistics data (JSON string) - aggregated from all peers
    pub neteq_stats: Option<String>,
    /// NetEQ stats per peer
    pub neteq_stats_per_peer: HashMap<String, Vec<String>>,
    /// NetEQ buffer history for charting (legacy, aggregated)
    pub neteq_buffer_history: Vec<u64>,
    /// NetEQ jitter history for charting (legacy, aggregated)
    pub neteq_jitter_history: Vec<u64>,
    /// NetEQ buffer history per peer
    pub neteq_buffer_per_peer: HashMap<String, Vec<u64>>,
    /// NetEQ jitter history per peer
    pub neteq_jitter_per_peer: HashMap<String, Vec<u64>>,
    /// Current video enabled state
    pub video_enabled: bool,
    /// Current microphone enabled state
    pub mic_enabled: bool,
    /// Current screen share state
    pub share_screen: bool,
}

fn parse_neteq_stats_history(neteq_stats_str: &str) -> Vec<NetEqStats> {
    log::info!(
        "[parse_neteq_stats_history] Parsing {} characters of stats data",
        neteq_stats_str.len()
    );
    log::debug!("[parse_neteq_stats_history] Raw data: {}", neteq_stats_str);

    let mut stats = Vec::new();

    // Try to parse as newline-delimited JSON (JSONL format)
    let lines: Vec<&str> = neteq_stats_str.lines().collect();
    log::info!(
        "[parse_neteq_stats_history] Found {} lines to parse",
        lines.len()
    );

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            log::debug!("[parse_neteq_stats_history] Skipping empty line {}", i);
            continue;
        }
        log::debug!(
            "[parse_neteq_stats_history] Parsing line {}: {}",
            i,
            trimmed
        );

        match serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(trimmed) {
            Ok(raw_stat) => {
                let stat: NetEqStats = raw_stat.into();
                log::info!("[parse_neteq_stats_history] Successfully parsed line {}: buffer_ms={}, target_ms={}", i, stat.buffer_ms, stat.target_ms);
                stats.push(stat);
            }
            Err(e) => {
                log::warn!(
                    "[parse_neteq_stats_history] Failed to parse line {}: {}",
                    i,
                    e
                );
                log::debug!(
                    "[parse_neteq_stats_history] Failed line content: '{}'",
                    trimmed
                );
            }
        }
    }

    // If that didn't work, try to parse as a single JSON object
    if stats.is_empty() {
        log::info!("[parse_neteq_stats_history] No lines parsed successfully, trying as single JSON object");
        match serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(neteq_stats_str)
        {
            Ok(raw_stat) => {
                let stat: NetEqStats = raw_stat.into();
                log::info!("[parse_neteq_stats_history] Successfully parsed as single JSON: buffer_ms={}, target_ms={}", stat.buffer_ms, stat.target_ms);
                stats.push(stat);
            }
            Err(e) => {
                log::warn!(
                    "[parse_neteq_stats_history] Failed to parse as single JSON: {}",
                    e
                );
            }
        }
    }

    // Keep only the last 60 entries (60 seconds of data)
    if stats.len() > 60 {
        stats.drain(0..stats.len() - 60);
    }

    log::info!(
        "[parse_neteq_stats_history] Successfully parsed {} stats entries",
        stats.len()
    );
    stats
}

#[function_component(Diagnostics)]
pub fn diagnostics(props: &DiagnosticsProps) -> Html {
    let selected_peer = use_state(|| "All Peers".to_string());

    // Log data received for debugging
    log::info!(
        "[Diagnostics] Rendering with {} per-peer stats entries",
        props.neteq_stats_per_peer.len()
    );
    for (peer_id, stats) in &props.neteq_stats_per_peer {
        log::info!(
            "[Diagnostics] Peer {}: {} stats entries",
            peer_id,
            stats.len()
        );
    }

    let close_handler = {
        let on_close = props.on_close.clone();
        Callback::from(move |_| {
            on_close.emit(());
        })
    };

    // Get list of available peers
    let available_peers: Vec<String> = {
        let mut peers = vec!["All Peers".to_string()];
        let mut peer_keys: Vec<String> = props.neteq_stats_per_peer.keys().cloned().collect();
        peer_keys.sort();
        peers.extend(peer_keys);
        peers
    };

    // Parse NetEQ stats based on selected peer
    let neteq_stats_history = if *selected_peer == "All Peers" {
        log::info!("[Diagnostics] Parsing stats for 'All Peers'");
        let result = props
            .neteq_stats
            .as_ref()
            .map(|stats_str| parse_neteq_stats_history(stats_str))
            .unwrap_or_default();
        log::info!(
            "[Diagnostics] All Peers parsing result: {} stats entries",
            result.len()
        );
        result
    } else {
        log::info!("[Diagnostics] Parsing stats for peer '{}'", *selected_peer);
        let result = props
            .neteq_stats_per_peer
            .get(&*selected_peer)
            .map(|peer_stats| {
                log::info!(
                    "[Diagnostics] Found {} raw stats entries for peer '{}'",
                    peer_stats.len(),
                    *selected_peer
                );
                let joined = peer_stats.join("\n");
                log::info!("[Diagnostics] Joined stats: {} characters", joined.len());
                parse_neteq_stats_history(&joined)
            })
            .unwrap_or_default();
        log::info!(
            "[Diagnostics] Peer '{}' parsing result: {} stats entries",
            *selected_peer,
            result.len()
        );
        result
    };

    let latest_neteq_stats = neteq_stats_history.last().cloned();
    log::info!("[Diagnostics] Latest stats: {:?}", latest_neteq_stats);

    // Get buffer and jitter history for selected peer
    let (buffer_history, jitter_history) = if *selected_peer == "All Peers" {
        (
            props.neteq_buffer_history.clone(),
            props.neteq_jitter_history.clone(),
        )
    } else {
        (
            props
                .neteq_buffer_per_peer
                .get(&*selected_peer)
                .cloned()
                .unwrap_or_default(),
            props
                .neteq_jitter_per_peer
                .get(&*selected_peer)
                .cloned()
                .unwrap_or_default(),
        )
    };

    // Peer selection callback
    let on_peer_change = {
        let selected_peer = selected_peer.clone();
        Callback::from(move |event: Event| {
            let target = event.target_unchecked_into::<web_sys::HtmlSelectElement>();
            selected_peer.set(target.value());
        })
    };

    html! {
        <div id="diagnostics-sidebar" class={if props.is_open {"visible"} else {""}}>
            <div class="sidebar-header">
                <h2>{"NetEq Performance Dashboard"}</h2>
                <button class="close-button" onclick={close_handler}>{"×"}</button>
            </div>
            <div class="sidebar-content">

                // Peer Selection
                if available_peers.len() > 1 {
                    <div class="diagnostics-section">
                        <h3>{"Peer Selection"}</h3>
                        <select
                            class="peer-selector"
                            onchange={on_peer_change}
                            value={(*selected_peer).clone()}
                        >
                            {for available_peers.iter().map(|peer| {
                                html! {
                                    <option value={peer.clone()} selected={peer == &*selected_peer}>
                                        {peer.clone()}
                                    </option>
                                }
                            })}
                        </select>
                        <p class="peer-info">
                            {format!("Showing statistics for: {}", *selected_peer)}
                        </p>
                    </div>
                }

                // NetEQ Status Display
                <div class="diagnostics-section">
                    <h3>{"Current Status"}</h3>
                    <NetEqStatusDisplay latest_stats={latest_neteq_stats} />
                </div>

                // NetEQ Advanced Charts
                if !neteq_stats_history.is_empty() {
                    <div class="diagnostics-charts">
                        <div class="charts-grid">
                            <div class="chart-container">
                                <NetEqAdvancedChart
                                    stats_history={neteq_stats_history.clone()}
                                    chart_type={AdvancedChartType::BufferVsTarget}
                                    width={290}
                                    height={200}
                                />
                            </div>
                            <div class="chart-container">
                                <NetEqAdvancedChart
                                    stats_history={neteq_stats_history.clone()}
                                    chart_type={AdvancedChartType::NetworkAdaptation}
                                    width={290}
                                    height={200}
                                />
                            </div>
                        </div>

                        <div class="charts-grid">
                            <div class="chart-container">
                                <NetEqAdvancedChart
                                    stats_history={neteq_stats_history.clone()}
                                    chart_type={AdvancedChartType::QualityMetrics}
                                    width={290}
                                    height={200}
                                />
                            </div>
                            <div class="chart-container">
                                <NetEqAdvancedChart
                                    stats_history={neteq_stats_history.clone()}
                                    chart_type={AdvancedChartType::ReorderingAnalysis}
                                    width={290}
                                    height={200}
                                />
                            </div>
                        </div>

                        <div class="charts-grid">
                            <div class="chart-container">
                                <NetEqAdvancedChart
                                    stats_history={neteq_stats_history.clone()}
                                    chart_type={AdvancedChartType::SystemPerformance}
                                    width={290}
                                    height={200}
                                />
                            </div>
                            <div class="chart-container">
                                // Legacy buffer/jitter charts
                                <div style="display:flex; gap:12px; align-items:center;">
                                    <NetEqChart
                                        data={buffer_history.clone()}
                                        chart_type={ChartType::Buffer}
                                        width={140}
                                        height={80}
                                    />
                                    <NetEqChart
                                        data={jitter_history.clone()}
                                        chart_type={ChartType::Jitter}
                                        width={140}
                                        height={80}
                                    />
                                </div>
                            </div>
                        </div>
                    </div>
                } else {
                    // Fallback to legacy charts if no parsed NetEQ stats
                    <div class="diagnostics-section">
                        <h3>{"NetEQ Buffer / Jitter History"}</h3>
                        <div style="display:flex; gap:12px; align-items:center;">
                            <NetEqChart
                                data={buffer_history.clone()}
                                chart_type={ChartType::Buffer}
                                width={140}
                                height={80}
                            />
                            <NetEqChart
                                data={jitter_history.clone()}
                                chart_type={ChartType::Jitter}
                                width={140}
                                height={80}
                            />
                        </div>
                    </div>
                }

                // Per-Peer Statistics Summary
                if available_peers.len() > 2 { // More than just "All Peers" and one actual peer
                    <div class="diagnostics-section">
                        <h3>{"Per-Peer Summary"}</h3>
                        <div class="peer-summary">
                            {for props.neteq_stats_per_peer.keys().map(|peer_id| {
                                let peer_buffer = props.neteq_buffer_per_peer.get(peer_id);
                                let latest_buffer = peer_buffer.and_then(|b| b.last()).unwrap_or(&0);
                                let peer_jitter = props.neteq_jitter_per_peer.get(peer_id);
                                let latest_jitter = peer_jitter.and_then(|j| j.last()).unwrap_or(&0);

                                html! {
                                    <div class="peer-summary-item">
                                        <strong>{peer_id.clone()}</strong>
                                        <span>{format!("Buffer: {}ms, Jitter: {}ms", latest_buffer, latest_jitter)}</span>
                                    </div>
                                }
                            })}
                        </div>
                    </div>
                }

                // Traditional Diagnostics Sections
                <div class="diagnostics-data">
                    <div class="diagnostics-section">
                        <h3>{"Reception Stats"}</h3>
                        {
                            if let Some(data) = &props.diagnostics_data {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No reception data available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Sending Stats"}</h3>
                        {
                            if let Some(data) = &props.sender_stats {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No sending data available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Encoder Settings"}</h3>
                        {
                            if let Some(data) = &props.encoder_settings {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No encoder settings available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"NetEQ Raw Stats"}</h3>
                        {
                            if let Some(data) = &props.neteq_stats {
                                html! { <pre>{ data }</pre> }
                            } else {
                                html! { <p>{"No NetEQ stats available."}</p> }
                            }
                        }
                    </div>
                    <div class="diagnostics-section">
                        <h3>{"Media Status"}</h3>
                        <pre>{format!("Video: {}\nAudio: {}\nScreen Share: {}",
                            if props.video_enabled { "Enabled" } else { "Disabled" },
                            if props.mic_enabled { "Enabled" } else { "Disabled" },
                            if props.share_screen { "Enabled" } else { "Disabled" }
                        )}</pre>
                    </div>
                </div>
            </div>
        </div>
    }
}
