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
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use videocall_diagnostics::{DiagEvent, MetricValue};
use yew::prelude::*;

// Serializable versions of DiagEvent structures (with owned strings instead of &'static str)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializableDiagEvent {
    pub subsystem: String,
    pub stream_id: Option<String>,
    pub ts_ms: u64,
    pub metrics: Vec<SerializableMetric>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SerializableMetric {
    pub name: String,
    pub value: MetricValue,
}

impl From<DiagEvent> for SerializableDiagEvent {
    fn from(event: DiagEvent) -> Self {
        Self {
            subsystem: event.subsystem.to_string(),
            stream_id: event.stream_id,
            ts_ms: event.ts_ms,
            metrics: event
                .metrics
                .into_iter()
                .map(|m| SerializableMetric {
                    name: m.name.to_string(),
                    value: m.value,
                })
                .collect(),
        }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ConnectionManagerState {
    pub election_state: String,
    pub election_progress: Option<f64>,
    pub servers_total: Option<u64>,
    pub active_connection_id: Option<String>,
    pub active_server_url: Option<String>,
    pub active_server_type: Option<String>,
    pub active_server_rtt: Option<f64>,
    pub failure_reason: Option<String>,
    pub servers: Vec<ServerInfo>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct ServerInfo {
    pub connection_id: String,
    pub url: String,
    pub server_type: String,
    pub status: String,
    pub rtt: Option<f64>,
    pub active: bool,
    pub connected: bool,
    pub measurement_count: Option<u64>,
}

impl Default for ConnectionManagerState {
    fn default() -> Self {
        Self {
            election_state: "unknown".to_string(),
            election_progress: None,
            servers_total: None,
            active_connection_id: None,
            active_server_url: None,
            active_server_type: None,
            active_server_rtt: None,
            failure_reason: None,
            servers: Vec::new(),
        }
    }
}

impl ConnectionManagerState {
    pub fn from_serializable_events(events: &[SerializableDiagEvent]) -> Self {
        let mut state = Self::default();
        for event in events {
            if event.subsystem != "connection_manager" {
                continue;
            }

            if event.stream_id.is_none() {
                // Main connection manager event
                Self::process_main_event(event, &mut state);
            } else if let Some(connection_id) = &event.stream_id {
                // Individual server event
                if let Some(server) = Self::process_server_event(event, connection_id) {
                    // Update existing server or add new one
                    if let Some(existing) = state
                        .servers
                        .iter_mut()
                        .find(|s| s.connection_id == server.connection_id)
                    {
                        *existing = server;
                    } else {
                        state.servers.push(server);
                    }
                }
            }
        }

        // Sort servers for consistent display
        state
            .servers
            .sort_by(|a, b| a.connection_id.cmp(&b.connection_id));
        state
    }

    fn process_main_event(event: &SerializableDiagEvent, state: &mut ConnectionManagerState) {
        for metric in &event.metrics {
            match metric.name.as_str() {
                "election_state" => {
                    if let MetricValue::Text(text) = &metric.value {
                        state.election_state = text.clone();
                    }
                }
                "election_progress" => {
                    if let MetricValue::F64(progress) = &metric.value {
                        state.election_progress = Some(*progress);
                    }
                }
                "servers_total" => {
                    if let MetricValue::U64(total) = &metric.value {
                        state.servers_total = Some(*total);
                    }
                }
                "active_connection_id" => {
                    if let MetricValue::Text(id) = &metric.value {
                        state.active_connection_id = Some(id.clone());
                    }
                }
                "active_server_url" => {
                    if let MetricValue::Text(url) = &metric.value {
                        state.active_server_url = Some(url.clone());
                    }
                }
                "active_server_type" => {
                    if let MetricValue::Text(server_type) = &metric.value {
                        state.active_server_type = Some(server_type.clone());
                    }
                }
                "active_server_rtt" => {
                    if let MetricValue::F64(rtt) = &metric.value {
                        state.active_server_rtt = Some(*rtt);
                    }
                }
                "failure_reason" => {
                    if let MetricValue::Text(reason) = &metric.value {
                        state.failure_reason = Some(reason.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn process_server_event(
        event: &SerializableDiagEvent,
        connection_id: &str,
    ) -> Option<ServerInfo> {
        let mut server = ServerInfo {
            connection_id: connection_id.to_string(),
            url: "unknown".to_string(),
            server_type: "unknown".to_string(),
            status: "unknown".to_string(),
            rtt: None,
            active: false,
            connected: false,
            measurement_count: None,
        };

        for metric in &event.metrics {
            match metric.name.as_str() {
                "server_url" => {
                    if let MetricValue::Text(url) = &metric.value {
                        server.url = url.clone();
                    }
                }
                "server_type" => {
                    if let MetricValue::Text(server_type) = &metric.value {
                        server.server_type = server_type.clone();
                    }
                }
                "server_status" => {
                    if let MetricValue::Text(status) = &metric.value {
                        server.status = status.clone();
                    }
                }
                "server_rtt" => {
                    if let MetricValue::F64(rtt) = &metric.value {
                        server.rtt = Some(*rtt);
                    }
                }
                "server_active" => {
                    if let MetricValue::U64(active) = &metric.value {
                        server.active = *active > 0;
                    }
                }
                "server_connected" => {
                    if let MetricValue::U64(connected) = &metric.value {
                        server.connected = *connected > 0;
                    }
                }
                "measurement_count" => {
                    if let MetricValue::U64(count) = &metric.value {
                        server.measurement_count = Some(*count);
                    }
                }
                _ => {}
            }
        }

        Some(server)
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct AudioWorkletState {
    pub peer_id: String,
    pub worklet_state: String,
    pub sample_rate: Option<u64>,
    pub is_terminated: bool,
    pub uptime_ms: Option<u64>,
    pub pending_tasks: u64,
    pub last_health_check_ms: Option<u64>,
    pub health_check_overdue: bool,
    pub pcm_data_received: u64,
    pub pcm_sent_to_worklet: u64,
    pub pcm_send_failed: u64,
    pub last_pcm_success_timestamp: Option<u64>,
    pub last_error: Option<String>,
    pub timestamp: u64,
}

impl Default for AudioWorkletState {
    fn default() -> Self {
        Self {
            peer_id: "unknown".to_string(),
            worklet_state: "unknown".to_string(),
            sample_rate: None,
            is_terminated: false,
            uptime_ms: None,
            pending_tasks: 0,
            last_health_check_ms: None,
            health_check_overdue: false,
            pcm_data_received: 0,
            pcm_sent_to_worklet: 0,
            pcm_send_failed: 0,
            last_pcm_success_timestamp: None,
            last_error: None,
            timestamp: 0,
        }
    }
}

impl AudioWorkletState {
    pub fn from_diagnostic_events(peer_id: &str, events: &[SerializableDiagEvent]) -> Self {
        let mut state = Self {
            peer_id: peer_id.to_string(),
            ..Self::default()
        };

        // Process events in chronological order
        let mut sorted_events = events.to_vec();
        sorted_events.sort_by_key(|e| e.ts_ms);

        for event in sorted_events {
            if event.subsystem != "audio_worklet" {
                continue;
            }

            state.timestamp = event.ts_ms;

            for metric in &event.metrics {
                match metric.name.as_str() {
                    "worklet_state" => {
                        if let MetricValue::Text(ws) = &metric.value {
                            state.worklet_state = ws.clone();
                        }
                    }
                    "sample_rate" => {
                        if let MetricValue::U64(sr) = &metric.value {
                            state.sample_rate = Some(*sr);
                        }
                    }
                    "is_terminated" => {
                        if let MetricValue::U64(term) = &metric.value {
                            state.is_terminated = *term > 0;
                        }
                    }
                    "uptime_ms" => {
                        if let MetricValue::U64(ut) = &metric.value {
                            state.uptime_ms = Some(*ut);
                        }
                    }
                    "pending_tasks" => {
                        if let MetricValue::U64(pt) = &metric.value {
                            state.pending_tasks = *pt;
                        }
                    }
                    "last_health_check_ms" => {
                        if let MetricValue::U64(hc) = &metric.value {
                            state.last_health_check_ms = Some(*hc);
                        }
                    }
                    "health_check_overdue" => {
                        if let MetricValue::U64(overdue) = &metric.value {
                            state.health_check_overdue = *overdue > 0;
                        }
                    }
                    "pcm_data_received" => {
                        if let MetricValue::U64(_) = &metric.value {
                            state.pcm_data_received += 1;
                        }
                    }
                    "pcm_sent_to_worklet" => {
                        if let MetricValue::U64(_) = &metric.value {
                            state.pcm_sent_to_worklet += 1;
                        }
                    }
                    "pcm_send_failed" => {
                        if let MetricValue::U64(_) = &metric.value {
                            state.pcm_send_failed += 1;
                        }
                    }
                    "last_pcm_success_timestamp" => {
                        if let MetricValue::U64(ts) = &metric.value {
                            state.last_pcm_success_timestamp = Some(*ts);
                        }
                    }
                    "pcm_error" => {
                        if let MetricValue::Text(error) = &metric.value {
                            state.last_error = Some(error.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        state
    }
}

#[derive(Properties, PartialEq)]
pub struct AudioWorkletDisplayProps {
    pub audio_worklet_states: HashMap<String, AudioWorkletState>,
}

#[function_component(AudioWorkletDisplay)]
pub fn audio_worklet_display(props: &AudioWorkletDisplayProps) -> Html {
    if props.audio_worklet_states.is_empty() {
        return html! {
            <div class="audio-worklet-display">
                <p class="no-data">{"No audio worklet data available"}</p>
            </div>
        };
    }

    html! {
        <div class="audio-worklet-display">
            {for props.audio_worklet_states.iter().map(|(peer_id, state)| {
                let worklet_status_class = match state.worklet_state.as_str() {
                    "ready" => "status-healthy",
                    "initializing" => "status-initializing",
                    "uninitialized" => "status-warning",
                    "terminated" | "terminating" => "status-terminated",
                    _ => "status-unknown"
                };

                let pcm_flow_healthy = state.pcm_sent_to_worklet > 0 &&
                    state.last_pcm_success_timestamp.is_some() &&
                    !state.health_check_overdue;

                html! {
                    <div class="worklet-peer-card">
                        <div class="peer-header">
                            <span class="peer-id">{peer_id}</span>
                            <span class={classes!("worklet-status", worklet_status_class)}>
                                {state.worklet_state.to_uppercase()}
                            </span>
                        </div>

                        <div class="worklet-metrics">
                            {
                                if let Some(sample_rate) = state.sample_rate {
                                    html! {
                                        <div class="metric-row">
                                            <span class="metric-label">{"Sample Rate:"}</span>
                                            <span class="metric-value">{format!("{}Hz", sample_rate)}</span>
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }

                            <div class="metric-row">
                                <span class="metric-label">{"PCM Flow:"}</span>
                                <span class={classes!("metric-value", if pcm_flow_healthy { "flow-healthy" } else { "flow-unhealthy" })}>
                                    {format!("↓{} ↑{} ✗{}", state.pcm_data_received, state.pcm_sent_to_worklet, state.pcm_send_failed)}
                                </span>
                            </div>

                            {
                                if state.pending_tasks > 0 {
                                    html! {
                                        <div class="metric-row">
                                            <span class="metric-label">{"Pending Tasks:"}</span>
                                            <span class="metric-value">{state.pending_tasks}</span>
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }

                            {
                                if let Some(uptime) = state.uptime_ms {
                                    html! {
                                        <div class="metric-row">
                                            <span class="metric-label">{"Uptime:"}</span>
                                            <span class="metric-value">{format!("{:.1}s", uptime as f64 / 1000.0)}</span>
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }

                            {
                                if let Some(error) = &state.last_error {
                                    html! {
                                        <div class="metric-row error-row">
                                            <span class="metric-label">{"Last Error:"}</span>
                                            <span class="metric-value error-text">{error}</span>
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }
                        </div>
                    </div>
                }
            })}
        </div>
    }
}

#[derive(Properties, PartialEq)]
pub struct ConnectionManagerDisplayProps {
    pub connection_manager_state: Option<String>,
}

#[function_component(ConnectionManagerDisplay)]
pub fn connection_manager_display(props: &ConnectionManagerDisplayProps) -> Html {
    let parsed_state = props.connection_manager_state.as_ref().map(|json| {
        let events: Vec<SerializableDiagEvent> = serde_json::from_str(json).unwrap_or_default();
        ConnectionManagerState::from_serializable_events(&events)
    });

    // Common CSS styles for both branches
    let common_styles = r#"
        .connection-manager-display {
            font-family: -apple-system, BlinkMacSystemFont, 'SF Pro Text', Inter, system-ui, sans-serif;
            font-size: 13px;
            line-height: 1.4;
            color: #FFFFFF;
            background: #1C1C1E;
            border-radius: 12px;
            padding: 16px;
            margin-bottom: 16px;
            border: 1px solid #38383A;
        }

        .connection-manager-display h4 {
            margin: 0 0 12px 0;
            font-size: 14px;
            font-weight: 600;
            color: #FFFFFF;
            border-bottom: 1px solid #38383A;
            padding-bottom: 6px;
        }

        .no-data {
            color: #AEAEB2;
            font-style: italic;
            text-align: center;
            padding: 20px;
            background: #1C1C1E;
            border-radius: 8px;
            border: 1px dashed #38383A;
            margin: 0;
        }

        .connection-status { margin-bottom: 16px; }
        .status-grid { display: grid; gap: 8px; }
        .status-item { display: flex; justify-content: space-between; align-items: center; padding: 6px 0; }
        .status-label { font-weight: 500; color: #AEAEB2; }
        .status-value { font-weight: 600; padding: 2px 8px; border-radius: 4px; font-size: 12px; }

        .status-testing { background: #2C2C2E; color: #FF9F0A; border: 1px solid #48484A; }
        .status-elected { background: #2C2C2E; color: #0A84FF; border: 1px solid #48484A; }
        .status-failed { background: #2C2C2E; color: #FF453A; border: 1px solid #48484A; }
        .status-reconnecting { background: #2C2C2E; color: #AEAEB2; border: 1px solid #48484A; }

        .progress-container { display: flex; align-items: center; gap: 8px; min-width: 120px; }
        .progress-bar { flex: 1; height: 6px; background: #2C2C2E; border-radius: 3px; overflow: hidden; }
        .progress-fill { height: 100%; background: linear-gradient(90deg, #0A84FF, #30D158); border-radius: 3px; transition: width 0.3s ease; }
        .progress-text { font-size: 11px; font-weight: 600; color: #FFFFFF; min-width: 35px; }

        .active-connection { margin-bottom: 16px; padding: 12px; background: #2C2C2E; border-radius: 8px; border-left: 4px solid #0A84FF; }
        .connection-details { display: grid; gap: 6px; }
        .detail-item { display: flex; justify-content: space-between; align-items: center; }
        .detail-label { font-weight: 500; color: #FFFFFF; }
        .detail-value { font-weight: 600; }
        .server-url { font-family: Menlo, Monaco, 'SF Mono', 'Courier New', monospace; font-size: 11px; background: #1C1C1E; padding: 2px 6px; border-radius: 3px; max-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

        .connection-type { padding: 2px 8px; border-radius: 4px; font-size: 11px; }
        .type-websocket { background: #1C1C1E; color: #0A84FF; border: 1px solid #38383A; }
        .type-webtransport { background: #1C1C1E; color: #0A84FF; border: 1px solid #38383A; }

        .rtt-value { padding: 2px 6px; border-radius: 3px; font-size: 11px; font-weight: 600; }
        .rtt-good { background: #1C1C1E; color: #30D158; border: 1px solid #38383A; }
        .rtt-ok { background: #1C1C1E; color: #FF9F0A; border: 1px solid #38383A; }
        .rtt-poor { background: #1C1C1E; color: #FF453A; border: 1px solid #38383A; }

        .servers-list { margin-bottom: 16px; }
        .servers-grid { display: grid; gap: 8px; }
        .server-card { background: #1C1C1E; border: 1px solid #38383A; border-radius: 8px; padding: 10px; transition: all 0.2s ease; }
        .server-card:hover { border-color: #48484A; box-shadow: 0 2px 6px rgba(0,0,0,0.35); }
        .server-active { border-color: #0A84FF; box-shadow: 0 2px 6px rgba(10,132,255,0.25); }
        .server-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 6px; }
        .server-id { font-weight: 600; font-size: 11px; color: #FFFFFF; font-family: Menlo, Monaco, 'SF Mono', 'Courier New', monospace; background: #2C2C2E; padding: 2px 6px; border-radius: 3px; }
        .server-indicators { display: flex; gap: 4px; align-items: center; }
        .indicator { font-size: 12px; font-weight: bold; }
        .active-indicator { color: #30D158; }
        .status-indicator { font-size: 14px; }
        .server-details { font-size: 11px; }
        .server-url { color: #AEAEB2; margin-bottom: 4px; font-family: Menlo, Monaco, 'SF Mono', 'Courier New', monospace; word-break: break-all; }
        .server-info { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; }
        .server-type { padding: 2px 6px; border-radius: 3px; font-size: 10px; font-weight: 600; background: #2C2C2E; border: 1px solid #38383A; color: #0A84FF; }
        .server-rtt { padding: 2px 6px; border-radius: 3px; font-size: 10px; font-weight: 600; }
        .no-rtt { color: #8E8E93; background: #1C1C1E; border: 1px solid #38383A; }
        .measurement-count { font-size: 10px; color: #AEAEB2; background: #1C1C1E; padding: 2px 4px; border-radius: 3px; border: 1px solid #38383A; }

        .connection-error { background: #2C2C2E; color: #FF453A; padding: 12px; border-radius: 8px; border-left: 4px solid #FF453A; }
        .error-reason { margin: 6px 0 0 0; font-size: 12px; font-style: italic; }

        /* Audio Worklet Display Styles */
        .audio-worklet-display { margin: 8px 0; }
        .worklet-peer-card { background: #1C1C1E; border: 1px solid #38383A; border-radius: 8px; padding: 10px; margin-bottom: 8px; }
        .peer-header { display: flex; justify-content: space-between; align-items: center; margin-bottom: 8px; border-bottom: 1px solid #38383A; padding-bottom: 6px; }
        .peer-id { font-weight: 600; font-size: 12px; color: #FFFFFF; font-family: Menlo, Monaco, 'SF Mono', 'Courier New', monospace; background: #2C2C2E; padding: 2px 6px; border-radius: 3px; }
        .worklet-status { padding: 2px 8px; border-radius: 4px; font-size: 11px; font-weight: 600; }
        .status-healthy { background: #1C1C1E; color: #30D158; border: 1px solid #38383A; }
        .status-initializing { background: #1C1C1E; color: #FF9F0A; border: 1px solid #38383A; }
        .status-warning { background: #1C1C1E; color: #FF9F0A; border: 1px solid #38383A; }
        .status-terminated { background: #1C1C1E; color: #FF453A; border: 1px solid #38383A; }
        .status-unknown { background: #1C1C1E; color: #8E8E93; border: 1px solid #38383A; }
        .worklet-metrics { display: grid; gap: 6px; }
        .metric-row { display: flex; justify-content: space-between; align-items: center; font-size: 11px; }
        .metric-label { color: #AEAEB2; font-weight: 500; }
        .metric-value { color: #FFFFFF; font-weight: 600; font-family: Menlo, Monaco, 'SF Mono', 'Courier New', monospace; }
        .flow-healthy { color: #30D158; }
        .flow-unhealthy { color: #FF453A; }
        .error-row { border: 1px solid #FF453A; border-radius: 4px; padding: 4px; background: rgba(255, 69, 58, 0.1); }
        .error-text { color: #FF453A; font-size: 10px; word-break: break-all; }
    "#;

    if let Some(state) = parsed_state {
        html! {
            <>
                <style>{common_styles}</style>
                <div class="connection-manager-display">
                    // Overall Status
                    <div class="connection-status">
                        <h4>{"Connection Status"}</h4>
                        <div class="status-grid">
                            <div class="status-item">
                                <span class="status-label">{"State:"}</span>
                                <span class={classes!("status-value", format!("status-{}", state.election_state))}>
                                    {state.election_state.to_uppercase()}
                                </span>
                            </div>
                            {
                                if let Some(progress) = state.election_progress {
                                    if state.election_state == "testing" {
                                        html! {
                                            <div class="status-item">
                                                <span class="status-label">{"Progress:"}</span>
                                                <div class="progress-container">
                                                    <div class="progress-bar">
                                                        <div class="progress-fill" style={format!("width: {}%", progress * 100.0)}></div>
                                                    </div>
                                                    <span class="progress-text">{format!("{:.0}%", progress * 100.0)}</span>
                                                </div>
                                            </div>
                                        }
                                    } else {
                                        html! {}
                                    }
                                } else {
                                    html! {}
                                }
                            }
                            {
                                if let Some(total) = state.servers_total {
                                    html! {
                                        <div class="status-item">
                                            <span class="status-label">{"Total Servers:"}</span>
                                            <span class="status-value">{total}</span>
                                        </div>
                                    }
                                } else {
                                    html! {}
                                }
                            }
                        </div>
                    </div>

                    // Active Connection Info (when elected)
                    {
                        if state.election_state == "elected" {
                            html! {
                                <div class="active-connection">
                                    <h4>{"Active Connection"}</h4>
                                    <div class="connection-details">
                                        {
                                            if let Some(url) = &state.active_server_url {
                                                html! {
                                                    <div class="detail-item">
                                                        <span class="detail-label">{"Server:"}</span>
                                                        <span class="detail-value server-url">{url}</span>
                                                    </div>
                                                }
                                            } else {
                                                html! {}
                                            }
                                        }
                                        {
                                            if let Some(server_type) = &state.active_server_type {
                                                html! {
                                                    <div class="detail-item">
                                                        <span class="detail-label">{"Type:"}</span>
                                                        <span class={classes!("detail-value", "connection-type", format!("type-{server_type}"))}>
                                                            {server_type.to_uppercase()}
                                                        </span>
                                                    </div>
                                                }
                                            } else {
                                                html! {}
                                            }
                                        }
                                        {
                                            if let Some(rtt) = state.active_server_rtt {
                                                html! {
                                                    <div class="detail-item">
                                                        <span class="detail-label">{"RTT:"}</span>
                                                        <span class={classes!("detail-value", "rtt-value",
                                                            if rtt < 50.0 { "rtt-good" }
                                                            else if rtt < 150.0 { "rtt-ok" }
                                                            else { "rtt-poor" }
                                                        )}>
                                                            {format!("{:.1}ms", rtt)}
                                                        </span>
                                                    </div>
                                                }
                                            } else {
                                                html! {}
                                            }
                                        }
                                    </div>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    // Server List
                    {
                        if !state.servers.is_empty() {
                            html! {
                                <div class="servers-list">
                                    <h4>{"Servers"}</h4>
                                    <div class="servers-grid">
                                        {for state.servers.iter().map(|server| {
                                            html! {
                                                <div class={classes!("server-card", if server.active { "server-active" } else { "" })}>
                                                    <div class="server-header">
                                                        <span class="server-id">{&server.connection_id}</span>
                                                        <div class="server-indicators">
                                                            {
                                                                if server.active {
                                                                    html! { <span class="indicator active-indicator" title="Active">{"●"}</span> }
                                                                } else {
                                                                    html! {}
                                                                }
                                                            }
                                                            <span class={classes!("indicator", "status-indicator", format!("status-{}", server.status))}
                                                                  title={server.status.clone()}>
                                                                {
                                                                    match server.status.as_str() {
                                                                        "connecting" => "⏳",
                                                                        "connected" => "🔗",
                                                                        "testing" => "🔍",
                                                                        "active" => "✅",
                                                                        _ => "❓"
                                                                    }
                                                                }
                                                            </span>
                                                        </div>
                                                    </div>
                                                    <div class="server-details">
                                                        <div class="server-url">{&server.url}</div>
                                                        <div class="server-info">
                                                            <span class={classes!("server-type", format!("type-{}", server.server_type))}>
                                                                {server.server_type.to_uppercase()}
                                                            </span>
                                                            {
                                                                if let Some(rtt) = server.rtt {
                                                                    html! {
                                                                        <span class={classes!("server-rtt",
                                                                            if rtt < 50.0 { "rtt-good" }
                                                                            else if rtt < 150.0 { "rtt-ok" }
                                                                            else { "rtt-poor" }
                                                                        )}>
                                                                            {format!("{:.1}ms", rtt)}
                                                                        </span>
                                                                    }
                                                                } else {
                                                                    html! { <span class="server-rtt no-rtt">{"—"}</span> }
                                                                }
                                                            }
                                                            {
                                                                if let Some(count) = server.measurement_count {
                                                                    if count > 0 {
                                                                        html! { <span class="measurement-count" title="RTT measurements">{format!("{}📊", count)}</span> }
                                                                    } else {
                                                                        html! {}
                                                                    }
                                                                } else {
                                                                    html! {}
                                                                }
                                                            }
                                                        </div>
                                                    </div>
                                                </div>
                                            }
                                        })}
                                    </div>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    // Error State
                    {
                        if state.election_state == "failed" {
                            html! {
                                <div class="connection-error">
                                    <h4>{"Connection Failed"}</h4>
                                    {
                                        if let Some(reason) = &state.failure_reason {
                                            html! { <p class="error-reason">{reason}</p> }
                                        } else {
                                            html! { <p class="error-reason">{"Unknown error occurred"}</p> }
                                        }
                                    }
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }
                </div>
            </>
        }
    } else {
        html! {
            <>
                <style>{common_styles}</style>
                <div class="connection-manager-display">
                    <p class="no-data">{"No connection manager data available"}</p>
                </div>
            </>
        }
    }
}

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
    /// Connection manager diagnostics state
    pub connection_manager_state: Option<String>,
    /// Audio worklet diagnostics data (JSON string) - per peer
    pub audio_worklet_diagnostics: HashMap<String, Vec<String>>,
}

fn parse_neteq_stats_history(neteq_stats_str: &str) -> Vec<NetEqStats> {
    let mut stats = Vec::new();

    // Try to parse as newline-delimited JSON (JSONL format)
    let lines: Vec<&str> = neteq_stats_str.lines().collect();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            log::debug!("[parse_neteq_stats_history] Skipping empty line {i}");
            continue;
        }

        match serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(trimmed) {
            Ok(raw_stat) => {
                let stat: NetEqStats = raw_stat.into();
                stats.push(stat);
            }
            Err(e) => {
                log::warn!("[parse_neteq_stats_history] Failed to parse line {i}: {e}");
                log::debug!("[parse_neteq_stats_history] Failed line content: '{trimmed}'");
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
                stats.push(stat);
            }
            Err(e) => {
                log::warn!("[parse_neteq_stats_history] Failed to parse as single JSON: {e}");
            }
        }
    }

    // Keep only the last 60 entries (60 seconds of data)
    if stats.len() > 60 {
        stats.drain(0..stats.len() - 60);
    }
    stats
}

#[function_component(Diagnostics)]
pub fn diagnostics(props: &DiagnosticsProps) -> Html {
    let selected_peer = use_state(|| "All Peers".to_string());

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

    // Process audio worklet diagnostics
    let audio_worklet_states: HashMap<String, AudioWorkletState> = props
        .audio_worklet_diagnostics
        .iter()
        .map(|(peer_id, events)| {
            let events_vec: Vec<SerializableDiagEvent> = events
                .iter()
                .filter_map(|event_str| serde_json::from_str(event_str).ok())
                .collect();
            let state = AudioWorkletState::from_diagnostic_events(peer_id, &events_vec);
            (peer_id.clone(), state)
        })
        .collect();

    // Parse NetEQ stats based on selected peer
    let neteq_stats_history = if *selected_peer == "All Peers" {
        let result = props
            .neteq_stats
            .as_ref()
            .map(|stats_str| parse_neteq_stats_history(stats_str))
            .unwrap_or_default();
        result
    } else {
        let result = props
            .neteq_stats_per_peer
            .get(&*selected_peer)
            .map(|peer_stats| {
                let joined = peer_stats.join("\n");
                parse_neteq_stats_history(&joined)
            })
            .unwrap_or_default();
        result
    };

    let latest_neteq_stats = neteq_stats_history.last().cloned();

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
                <h2>{"Call Diagnostics"}</h2>
                <button class="close-button" onclick={close_handler}>{"×"}</button>
            </div>
            <div class="sidebar-content">

                // Application Version
                <div class="diagnostics-section">
                    <h3>{"Application Version"}</h3>
                    <pre>{format!("VideoCall UI: {}", env!("CARGO_PKG_VERSION"))}</pre>
                </div>

                // Connection Manager Status - Now at the top for visibility
                <div class="diagnostics-section">
                    <h3>{"Connection Manager"}</h3>
                    <ConnectionManagerDisplay connection_manager_state={props.connection_manager_state.clone()} />
                </div>

                // Audio Worklet Status - Critical for debugging audio issues
                {
                    if !audio_worklet_states.is_empty() {
                        html! {
                            <div class="diagnostics-section">
                                <h3>{"Worklet Status"}</h3>
                                <AudioWorkletDisplay audio_worklet_states={audio_worklet_states.clone()} />
                            </div>
                        }
                    } else {
                        html! {}
                    }
                }

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
                    <h3>{"Audio Status"}</h3>
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
                                    chart_type={AdvancedChartType::DecodeOperations}
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
