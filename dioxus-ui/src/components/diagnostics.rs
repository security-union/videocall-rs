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
use crate::components::performance_settings::{
    format_kbps_compact, format_mbps, format_peer_kind_line, format_send_header, format_send_layer,
    format_send_layer_short, format_send_total_kbps, format_simulcast_summary, peers_for_kind,
    DiagnosticsReader,
};
use crate::context::{
    confirm_transport_change, load_transport_sticky, TransportPreference, TransportPreferenceCtx,
};
use dioxus::prelude::*;
use dioxus_core::Task;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::rc::Rc;
use videocall_client::{PrefMediaKind, VideoCallClient};
use videocall_diagnostics::{subscribe, MetricValue};

// Serializable versions of DiagEvent structures
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

impl From<videocall_diagnostics::DiagEvent> for SerializableDiagEvent {
    fn from(event: videocall_diagnostics::DiagEvent) -> Self {
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
    /// Total configured servers (WS + WT URLs) the manager was set up with,
    /// independent of the `ElectionState`. Emitted as `configured_servers_total`
    /// from the connection-manager bus. Phase 7 (discussion 562).
    pub configured_servers_total: Option<u64>,
    /// `true` when the manager has at most one configured server. The UI
    /// renders a "Limited connectivity" badge while this is set, since
    /// re-elections are gated on having multiple candidates. Phase 7
    /// (discussion 562).
    pub single_server_only: Option<bool>,
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
            configured_servers_total: None,
            single_server_only: None,
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
                Self::process_main_event(event, &mut state);
            } else if let Some(connection_id) = &event.stream_id {
                if let Some(server) = Self::process_server_event(event, connection_id) {
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
                "configured_servers_total" => {
                    if let MetricValue::U64(total) = &metric.value {
                        state.configured_servers_total = Some(*total);
                    }
                }
                "single_server_only" => {
                    if let MetricValue::U64(flag) = &metric.value {
                        // Encoded as u64-bool to match the
                        // `server_active`/`server_connected` convention.
                        state.single_server_only = Some(*flag != 0);
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
                    if let MetricValue::Text(st) = &metric.value {
                        server.server_type = st.clone();
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

#[component]
pub fn ConnectionManagerDisplay(connection_manager_state: Option<String>) -> Element {
    let parsed_state = connection_manager_state.as_ref().map(|json| {
        let events: Vec<SerializableDiagEvent> = serde_json::from_str(json).unwrap_or_default();
        ConnectionManagerState::from_serializable_events(&events)
    });

    let common_styles = include_str!("diagnostics_cm_styles.css");

    if let Some(state) = parsed_state {
        let election_upper = state.election_state.to_uppercase();
        let status_class = format!("status-value status-{}", state.election_state);
        rsx! {
            style { "{common_styles}" }
            div { class: "connection-manager-display",
                div { class: "connection-status",
                    h4 { "Connection Status" }
                    div { class: "status-grid",
                        div { class: "status-item",
                            span { class: "status-label", "State:" }
                            span { class: "{status_class}", "{election_upper}" }
                        }
                        if let Some(progress) = state.election_progress {
                            if state.election_state == "testing" {
                                {
                                    let progress_pct = (progress * 100.0).min(100.0);
                                    let progress_pct_str = format!("{progress_pct:.0}%");
                                    rsx! {
                                        div { class: "status-item",
                                            span { class: "status-label", "Progress:" }
                                            div { class: "progress-container",
                                                div { class: "progress-bar",
                                                    div { class: "progress-fill", style: "width: {progress_pct}%",
                                                    }
                                                }
                                                span { class: "progress-text", "{progress_pct_str}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(total) = state.servers_total {
                            div { class: "status-item",
                                span { class: "status-label", "Total Servers:" }
                                span { class: "status-value", "{total}" }
                            }
                        }
                        if let Some(total) = state.configured_servers_total {
                            div { class: "status-item",
                                span { class: "status-label", "Configured Servers:" }
                                span { class: "status-value", "{total}" }
                            }
                        }
                    }
                    if state.single_server_only == Some(true) {
                        // Phase 7 (discussion 562): when only one (or zero)
                        // candidates are configured, the watchdog suppresses
                        // re-election (it would re-elect onto the same host).
                        // Surface a badge so the user knows recovery is gated
                        // and isn't just a silent stall.
                        div { class: "limited-connectivity-badge",
                            role: "alert",
                            aria_live: "polite",
                            span { class: "badge-icon", "!" }
                            span { class: "badge-text",
                                "Limited connectivity \u{2014} only 1 server reachable. \
                                 Re-elections disabled."
                            }
                        }
                    }
                }
                if state.election_state == "elected" {
                    div { class: "active-connection",
                        h4 { "Active Connection" }
                        div { class: "connection-details",
                            if let Some(url) = &state.active_server_url {
                                div { class: "detail-item",
                                    span { class: "detail-label", "Server:" }
                                    span { class: "detail-value server-url", "{url}" }
                                }
                            }
                            if let Some(server_type) = &state.active_server_type {
                                {
                                    let st_upper = server_type.to_uppercase();
                                    let type_class = format!("detail-value connection-type type-{server_type}");
                                    rsx! {
                                        div { class: "detail-item",
                                            span { class: "detail-label", "Type:" }
                                            span { class: "{type_class}", "{st_upper}" }
                                        }
                                    }
                                }
                            }
                            if let Some(rtt) = state.active_server_rtt {
                                {
                                    let rtt_class = if rtt < 50.0 { "rtt-good" } else if rtt < 150.0 { "rtt-ok" } else { "rtt-poor" };
                                    let rtt_str = format!("{rtt:.1}ms");
                                    let full_class = format!("detail-value rtt-value {rtt_class}");
                                    rsx! {
                                        div { class: "detail-item",
                                            span { class: "detail-label", "RTT:" }
                                            span { class: "{full_class}", "{rtt_str}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if !state.servers.is_empty() {
                    div { class: "servers-list",
                        h4 { "Servers" }
                        div { class: "servers-grid",
                            for server in state.servers.iter() {
                                {
                                    let card_class = if server.active { "server-card server-active" } else { "server-card" };
                                    let status_emoji = match server.status.as_str() {
                                        "connecting" => "\u{231b}",
                                        "connected" => "\u{1f517}",
                                        "testing" => "\u{1f50d}",
                                        "active" => "\u{2705}",
                                        _ => "\u{2753}",
                                    };
                                    let st_upper = server.server_type.to_uppercase();
                                    let type_class = format!("server-type type-{}", server.server_type);
                                    rsx! {
                                        div { class: "{card_class}",
                                            div { class: "server-header",
                                                span { class: "server-id", "{server.connection_id}" }
                                                div { class: "server-indicators",
                                                    if server.active {
                                                        span { class: "indicator active-indicator", title: "Active", "\u{25cf}" }
                                                    }
                                                    span { class: "indicator status-indicator", title: "{server.status}", "{status_emoji}" }
                                                }
                                            }
                                            div { class: "server-details",
                                                div { class: "server-url", "{server.url}" }
                                                div { class: "server-info",
                                                    span { class: "{type_class}", "{st_upper}" }
                                                    if let Some(rtt) = server.rtt {
                                                        {
                                                            let rtt_class = if rtt < 50.0 { "rtt-good" } else if rtt < 150.0 { "rtt-ok" } else { "rtt-poor" };
                                                            let rtt_str = format!("{rtt:.1}ms");
                                                            rsx! {
                                                                span { class: "server-rtt {rtt_class}", "{rtt_str}" }
                                                            }
                                                        }
                                                    } else {
                                                        span { class: "server-rtt no-rtt", "\u{2014}" }
                                                    }
                                                    if let Some(count) = server.measurement_count {
                                                        if count > 0 {
                                                            span { class: "measurement-count", title: "RTT measurements", "{count}\u{1f4ca}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if state.election_state == "failed" {
                    div { class: "connection-error",
                        h4 { "Connection Failed" }
                        if let Some(reason) = &state.failure_reason {
                            p { class: "error-reason", "{reason}" }
                        } else {
                            p { class: "error-reason", "Unknown error occurred" }
                        }
                    }
                }
            }
        }
    } else {
        rsx! {
            style { "{common_styles}" }
            div { class: "connection-manager-display",
                p { class: "no-data", "No connection manager data available" }
            }
        }
    }
}

fn parse_neteq_stats_history(neteq_stats_str: &str) -> Vec<NetEqStats> {
    let mut stats = Vec::new();
    let lines: Vec<&str> = neteq_stats_str.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(trimmed) {
            Ok(raw_stat) => {
                let stat: NetEqStats = raw_stat.into();
                stats.push(stat);
            }
            Err(e) => {
                log::warn!("[parse_neteq_stats_history] Failed to parse line {i}: {e}");
            }
        }
    }
    if stats.is_empty() {
        if let Ok(raw_stat) =
            serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(neteq_stats_str)
        {
            let stat: NetEqStats = raw_stat.into();
            stats.push(stat);
        }
    }
    if stats.len() > 60 {
        stats.drain(0..stats.len() - 60);
    }
    stats
}

#[component]
pub fn Diagnostics(
    is_open: bool,
    on_close: EventHandler<()>,
    video_enabled: bool,
    mic_enabled: bool,
    share_screen: bool,
    encoder_settings: Option<String>,
    /// Live SEND/RECEIVE simulcast reader for the "Simulcast layers" section,
    /// published by `Host` (which owns the encoders). `None` until Host mounts /
    /// when diagnostics aren't wired. (#1095 §6 MOVE)
    #[props(default)]
    diagnostics_reader: Option<DiagnosticsReader>,
    /// Cross-nav: open Performance settings from the diagnostics header. Defaults
    /// to a no-op so call sites that don't wire it still compile. (#1095 §4b)
    #[props(default)]
    on_open_performance: EventHandler<()>,
) -> Element {
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let mut selected_peer = use_signal(|| "All Peers".to_string());
    let mut diagnostics_data = use_signal(|| None::<String>);
    let mut sender_stats = use_signal(|| None::<String>);
    let mut connection_manager_state = use_signal(|| None::<String>);
    let mut neteq_stats_per_peer = use_signal(HashMap::<String, Vec<String>>::new);
    let mut neteq_buffer_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut neteq_jitter_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut peer_transport_per_peer = use_signal(HashMap::<String, String>::new);
    let mut diag_task = use_signal(|| None::<Task>);
    let mut backend_versions = use_signal(Vec::<serde_json::Value>::new);

    // Subscribe to diagnostics events using Dioxus `spawn`.
    // `spawn` runs within the Dioxus runtime so signal mutations properly
    // trigger re-renders.  We explicitly cancel the previous Task on each
    // re-run to prevent double-subscriptions (open → close → open).
    use_effect(move || {
        // Cancel any previous subscription task.
        if let Some(task) = *diag_task.peek() {
            task.cancel();
        }

        if !is_open {
            diagnostics_data.set(None);
            sender_stats.set(None);
            connection_manager_state.set(None);
            neteq_stats_per_peer.set(HashMap::new());
            neteq_buffer_per_peer.set(HashMap::new());
            neteq_jitter_per_peer.set(HashMap::new());
            peer_transport_per_peer.set(HashMap::new());
            diag_task.set(None);
            return;
        }

        let task = spawn(async move {
            let mut rx = subscribe();
            let mut connection_events = Vec::<SerializableDiagEvent>::new();
            let mut neteq_stats = HashMap::<String, Vec<String>>::new();
            let mut neteq_buffer = HashMap::<String, Vec<u64>>::new();
            let mut neteq_jitter = HashMap::<String, Vec<u64>>::new();
            // Per-peer transport label, locally cached. peer_status events
            // arrive on every heartbeat (~periodic), so we only push to the
            // signal when the value actually changes — heartbeat ticks must
            // not cause UI re-renders.
            let mut peer_transport = HashMap::<String, String>::new();

            while let Ok(evt) = rx.recv().await {
                match evt.subsystem {
                    "decoder" => {
                        let mut text = String::new();
                        for m in &evt.metrics {
                            match m.name {
                                "fps" => {
                                    if let MetricValue::F64(v) = &m.value {
                                        text.push_str(&format!("FPS: {v:.2}\n"));
                                    }
                                }
                                "bitrate_kbps" => {
                                    if let MetricValue::F64(v) = &m.value {
                                        text.push_str(&format!("Bitrate: {v:.1} kbps\n"));
                                    }
                                }
                                "media_type" => {
                                    if let MetricValue::Text(t) = &m.value {
                                        text.push_str(&format!("Media Type: {t}\n"));
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !text.is_empty() {
                            let peer_id = evt
                                .stream_id
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string());
                            text.push_str(&format!(
                                "Peer: {}\nTimestamp: {}\n",
                                peer_id, evt.ts_ms
                            ));
                            diagnostics_data.set(Some(text));
                        }
                    }
                    "sender" => {
                        let mut text = String::new();
                        for m in &evt.metrics {
                            match m.name {
                                "sender_id" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Sender: {v}\n"));
                                    }
                                }
                                "target_id" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Target: {v}\n"));
                                    }
                                }
                                "media_type" => {
                                    if let MetricValue::Text(v) = &m.value {
                                        text.push_str(&format!("Media Type: {v}\n"));
                                    }
                                }
                                _ => {}
                            }
                        }
                        if !text.is_empty() {
                            text.push_str(&format!("Timestamp: {}\n", evt.ts_ms));
                            sender_stats.set(Some(text));
                        }
                    }
                    "neteq" => {
                        let stream_id = evt
                            .stream_id
                            .clone()
                            .unwrap_or_else(|| "unknown->unknown".to_string());
                        let parts: Vec<&str> = stream_id.split("->").collect();
                        let target_peer = if parts.len() == 2 {
                            parts[1]
                        } else {
                            "unknown"
                        };
                        let mut stats_dirty = false;
                        let mut buffer_dirty = false;
                        let mut jitter_dirty = false;
                        for m in &evt.metrics {
                            match m.name {
                                "stats_json" => {
                                    if let MetricValue::Text(json) = &m.value {
                                        let entry =
                                            neteq_stats.entry(target_peer.to_string()).or_default();
                                        entry.push(json.clone());
                                        if entry.len() > 60 {
                                            entry.remove(0);
                                        }
                                        stats_dirty = true;
                                    }
                                }
                                "audio_buffer_ms" => {
                                    if let MetricValue::U64(v) = &m.value {
                                        let entry = neteq_buffer
                                            .entry(target_peer.to_string())
                                            .or_default();
                                        entry.push(*v);
                                        if entry.len() > 50 {
                                            entry.remove(0);
                                        }
                                        buffer_dirty = true;
                                    }
                                }
                                "jitter_buffer_delay_ms" => {
                                    if let MetricValue::U64(v) = &m.value {
                                        let entry = neteq_jitter
                                            .entry(target_peer.to_string())
                                            .or_default();
                                        entry.push(*v);
                                        if entry.len() > 50 {
                                            entry.remove(0);
                                        }
                                        jitter_dirty = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                        // Batch: update signals once per event, not per-metric.
                        if stats_dirty {
                            neteq_stats_per_peer.set(neteq_stats.clone());
                        }
                        if buffer_dirty {
                            neteq_buffer_per_peer.set(neteq_buffer.clone());
                        }
                        if jitter_dirty {
                            neteq_jitter_per_peer.set(neteq_jitter.clone());
                        }
                    }
                    "peer_status" => {
                        let mut peer_id: Option<String> = None;
                        let mut transport: Option<String> = None;
                        for m in &evt.metrics {
                            match m.name {
                                "to_peer" => {
                                    if let MetricValue::Text(t) = &m.value {
                                        peer_id = Some(t.clone());
                                    }
                                }
                                "peer_transport" => {
                                    if let MetricValue::Text(t) = &m.value {
                                        transport = Some(t.clone());
                                    }
                                }
                                _ => {}
                            }
                        }
                        if let (Some(p), Some(t)) = (peer_id, transport) {
                            // Only push to the signal when the value
                            // actually changes; otherwise we'd re-render
                            // on every heartbeat tick.
                            let changed = match peer_transport.get(&p) {
                                Some(prev) => prev != &t,
                                None => true,
                            };
                            if changed {
                                peer_transport.insert(p, t);
                                peer_transport_per_peer.set(peer_transport.clone());
                            }
                        }
                    }
                    "connection_manager" => {
                        connection_events.push(SerializableDiagEvent::from(evt));
                        if connection_events.len() > 20 {
                            connection_events.remove(0);
                        }
                        let serialized =
                            serde_json::to_string(&connection_events).unwrap_or_default();
                        connection_manager_state.set(Some(serialized));
                    }
                    _ => {}
                }
            }
        });
        diag_task.set(Some(task));
    });

    // Fetch aggregated version info from meeting-api when the panel opens.
    use_effect(move || {
        if !is_open {
            backend_versions.set(Vec::new());
            return;
        }
        spawn(async move {
            if let Ok(base_url) = crate::constants::meeting_api_base_url() {
                let url = format!("{base_url}/api/v1/versions");
                if let Ok(resp) = reqwest::get(&url).await {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(components) = body["components"].as_array() {
                            backend_versions.set(components.clone());
                        }
                    }
                }
            }
        });
    });

    // The live "Simulcast layers" section runs its OWN 250 ms (≈4 Hz) refresh
    // tick, scoped to its child component `SimulcastLayersSection` (below), so the
    // tick re-renders ONLY that small subtree — NOT this top-level `Diagnostics`
    // body, which re-executes the expensive NetEq prelude (clone + parse of up to
    // 60×N JSON lines). Keeping the tick out of here avoids ~thousands of JSON
    // parses/sec at scale on the main thread (perf review #1). The section's
    // `is_open` gating + `use_drop` interval cleanup live in that child.

    // Resolve numeric session IDs to display names via VideoCallClient context.
    let client = use_context::<VideoCallClient>();
    let peer_display_name = move |session_id: &str| -> String {
        client
            .get_peer_user_id(session_id)
            .unwrap_or_else(|| session_id.to_string())
    };

    // Get list of available peers (keys are raw session IDs).
    let available_peers: Vec<String> = {
        let mut peers = vec!["All Peers".to_string()];
        let stats = neteq_stats_per_peer();
        let mut peer_keys: Vec<String> = stats.keys().cloned().collect();
        peer_keys.sort();
        peers.extend(peer_keys);
        peers
    };

    // Parse NetEQ stats based on selected peer
    let current_peer = selected_peer();
    let stats_map = neteq_stats_per_peer();
    let neteq_stats_history = if current_peer == "All Peers" {
        let mut all = Vec::new();
        for stats in stats_map.values() {
            all.extend(stats.clone());
        }
        if all.is_empty() {
            Vec::new()
        } else {
            parse_neteq_stats_history(&all.join("\n"))
        }
    } else {
        stats_map
            .get(&current_peer)
            .map(|peer_stats| parse_neteq_stats_history(&peer_stats.join("\n")))
            .unwrap_or_default()
    };

    let latest_neteq_stats = neteq_stats_history.last().cloned();

    let buffer_map = neteq_buffer_per_peer();
    let jitter_map = neteq_jitter_per_peer();
    let (buffer_history, jitter_history) = if current_peer == "All Peers" {
        let mut ab = Vec::new();
        for buf in buffer_map.values() {
            ab.extend(buf.iter().cloned());
        }
        let mut aj = Vec::new();
        for jit in jitter_map.values() {
            aj.extend(jit.iter().cloned());
        }
        (ab, aj)
    } else {
        (
            buffer_map.get(&current_peer).cloned().unwrap_or_default(),
            jitter_map.get(&current_peer).cloned().unwrap_or_default(),
        )
    };

    let conn_state = connection_manager_state();
    let diag_data = diagnostics_data();
    let send_stats = sender_stats();
    let enc_settings = encoder_settings;
    let video_str = if video_enabled { "Enabled" } else { "Disabled" };
    let audio_str = if mic_enabled { "Enabled" } else { "Disabled" };
    let screen_str = if share_screen { "Enabled" } else { "Disabled" };
    let media_status =
        format!("Video: {video_str}\nAudio: {audio_str}\nScreen Share: {screen_str}");
    let current_peer_display = if current_peer == "All Peers" {
        "All Peers".to_string()
    } else {
        peer_display_name(&current_peer)
    };
    let peer_info = format!("Showing statistics for: {current_peer_display}");

    rsx! {
        div {
            id: "diagnostics-sidebar",
            class: if is_open { "visible" } else { "" },
            div { class: "sidebar-header",
                h2 { "Call Diagnostics" }
                // Spacer pushes the actions to the right; × stays rightmost.
                div { style: "flex: 1 1 auto;" }
                // Cross-nav: Diagnostics → Performance settings (#1095 §4b).
                button {
                    r#type: "button",
                    class: "sidebar-header-action",
                    "data-testid": "diag-open-performance",
                    title: "Open Performance settings (set send/receive quality limits)",
                    "aria-label": "Open Performance settings",
                    onclick: move |_| on_open_performance.call(()),
                    // Lucide sliders-horizontal.
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "18", height: "18", view_box: "0 0 24 24",
                        fill: "none", stroke: "currentColor", stroke_width: "2",
                        stroke_linecap: "round", stroke_linejoin: "round",
                        "aria-hidden": "true",
                        line { x1: "21", y1: "4", x2: "14", y2: "4" }
                        line { x1: "10", y1: "4", x2: "3", y2: "4" }
                        line { x1: "21", y1: "12", x2: "12", y2: "12" }
                        line { x1: "8", y1: "12", x2: "3", y2: "12" }
                        line { x1: "21", y1: "20", x2: "16", y2: "20" }
                        line { x1: "12", y1: "20", x2: "3", y2: "20" }
                        line { x1: "14", y1: "2", x2: "14", y2: "6" }
                        line { x1: "8", y1: "10", x2: "8", y2: "14" }
                        line { x1: "16", y1: "18", x2: "16", y2: "22" }
                    }
                    span { class: "sidebar-header-action__label", "Performance" }
                }
                button { class: "close-button", onclick: move |_| on_close.call(()), "\u{00d7}" }
            }
            div { class: "sidebar-content",
                div { class: "diagnostics-section",
                    h3 { "Build Info" }
                    div { class: "build-info-table",
                        div { class: "build-info-header",
                            span { class: "build-info-cell", "Component" }
                            span { class: "build-info-cell", "Commit" }
                            span { class: "build-info-cell", "Branch" }
                        }
                        div { class: "build-info-row",
                            span { class: "build-info-cell build-info-service", "dioxus-ui (v{env!(\"CARGO_PKG_VERSION\")})" }
                            span { class: "build-info-cell monospace", "" }
                            span { class: "build-info-cell", "" }
                        }
                        for comp in backend_versions() {
                            {
                                let svc = comp["service"].as_str().unwrap_or("?").to_string();
                                let ver = comp["version"].as_str().unwrap_or("").to_string();
                                let sha = comp["git_sha"].as_str().unwrap_or("?").to_string();
                                let br = comp["git_branch"].as_str().unwrap_or("?").to_string();
                                let label = if ver.is_empty() { svc } else { format!("{svc} ({ver})") };
                                rsx! {
                                    div { class: "build-info-row",
                                        span { class: "build-info-cell build-info-service", "{label}" }
                                        span { class: "build-info-cell monospace", "{sha}" }
                                        span { class: "build-info-cell", "{br}" }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "diagnostics-section",
                    h3 { "Connection Manager" }
                    ConnectionManagerDisplay { connection_manager_state: conn_state }
                }
                // Simulcast layers (#1095 §6 MOVE): the per-layer SEND ladder + the
                // per-peer RECEIVE breakdown that used to live in the Performance
                // panel's expandable footers. Extracted into its own child so its
                // 4 Hz refresh tick re-renders ONLY this section, not the NetEq
                // prelude / charts in this parent (perf review #1).
                SimulcastLayersSection { is_open, reader: diagnostics_reader.clone() }
                div { class: "diagnostics-section",
                    h3 { "Transport Preference" }
                    div { class: "device-setting-group",
                        select {
                            id: "diagnostics-transport-select",
                            class: "peer-selector",
                            onchange: move |evt: Event<FormData>| {
                                confirm_transport_change(
                                    &evt.value(),
                                    (transport_pref_ctx.0)(),
                                    "diagnostics-transport-select",
                                    load_transport_sticky(),
                                );
                            },
                            option {
                                value: "webtransport",
                                selected: (transport_pref_ctx.0)() == TransportPreference::WebTransport,
                                "WebTransport (default)"
                            }
                            option {
                                value: "websocket",
                                selected: (transport_pref_ctx.0)() == TransportPreference::WebSocket,
                                "WebSocket"
                            }
                        }
                    }
                    p { class: "transport-preference-note",
                        "Changing protocol will reload the page."
                    }
                }
                if available_peers.len() > 1 {
                    div { class: "diagnostics-section",
                        h3 { "Peer Selection" }
                        select {
                            class: "peer-selector",
                            onchange: move |e: Event<FormData>| {
                                selected_peer.set(e.value());
                            },
                            value: "{current_peer}",
                            for peer in available_peers.iter() {
                                {
                                    let label = if peer == "All Peers" {
                                        "All Peers".to_string()
                                    } else {
                                        peer_display_name(peer)
                                    };
                                    rsx! {
                                        option {
                                            value: "{peer}",
                                            selected: peer == &current_peer,
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                        p { class: "peer-info", "{peer_info}" }
                    }
                }
                div { class: "diagnostics-section",
                    h3 { "Current Status" }
                    NetEqStatusDisplay { latest_stats: latest_neteq_stats }
                }
                if !neteq_stats_history.is_empty() {
                    div { class: "diagnostics-charts",
                        div { class: "charts-grid",
                            div { class: "chart-container",
                                NetEqAdvancedChart { stats_history: neteq_stats_history.clone(), chart_type: AdvancedChartType::BufferVsTarget, width: 290, height: 200 }
                            }
                            div { class: "chart-container",
                                NetEqAdvancedChart { stats_history: neteq_stats_history.clone(), chart_type: AdvancedChartType::DecodeOperations, width: 290, height: 200 }
                            }
                        }
                        div { class: "charts-grid",
                            div { class: "chart-container",
                                NetEqAdvancedChart { stats_history: neteq_stats_history.clone(), chart_type: AdvancedChartType::QualityMetrics, width: 290, height: 200 }
                            }
                            div { class: "chart-container",
                                NetEqAdvancedChart { stats_history: neteq_stats_history.clone(), chart_type: AdvancedChartType::ReorderingAnalysis, width: 290, height: 200 }
                            }
                        }
                        div { class: "charts-grid",
                            div { class: "chart-container",
                                NetEqAdvancedChart { stats_history: neteq_stats_history.clone(), chart_type: AdvancedChartType::SystemPerformance, width: 290, height: 200 }
                            }
                        }
                    }
                } else {
                    div { class: "diagnostics-section",
                        h3 { "NetEQ Buffer / Jitter History" }
                        div { style: "display:flex; gap:12px; align-items:center;",
                            NetEqChart { data: buffer_history.clone(), chart_type: ChartType::Buffer, width: 140, height: 80 }
                            NetEqChart { data: jitter_history.clone(), chart_type: ChartType::Jitter, width: 140, height: 80 }
                        }
                    }
                }
                if available_peers.len() > 2 {
                    div { class: "diagnostics-section",
                        h3 { "Per-Peer Summary" }
                        div { class: "peer-summary",
                            {
                                let transport_map = peer_transport_per_peer();
                                rsx! {
                                    for (peer_id, _) in stats_map.iter() {
                                        {
                                            let display = peer_display_name(peer_id);
                                            let latest_buffer = buffer_map.get(peer_id).and_then(|b| b.last()).unwrap_or(&0);
                                            let latest_jitter = jitter_map.get(peer_id).and_then(|j| j.last()).unwrap_or(&0);
                                            let summary = format!("Buffer: {latest_buffer}ms, Jitter: {latest_jitter}ms");
                                            let (badge_label, badge_class, badge_title) = match transport_map.get(peer_id).map(String::as_str) {
                                                Some("webtransport") => ("WT", "connection-type type-webtransport", "WebTransport"),
                                                Some("websocket") => ("WS", "connection-type type-websocket", "WebSocket"),
                                                _ => ("\u{2014}", "connection-type", "Transport unknown"),
                                            };
                                            rsx! {
                                                div { class: "peer-summary-item",
                                                    strong { "{display}" }
                                                    div {
                                                        style: "display:flex; gap:8px; align-items:center;",
                                                        span { class: "{badge_class}", title: "{badge_title}", "{badge_label}" }
                                                        span { "{summary}" }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                div { class: "diagnostics-data",
                    div { class: "diagnostics-section",
                        h3 { "Reception Stats" }
                        if let Some(data) = &diag_data {
                            pre { "{data}" }
                        } else {
                            p { "No reception data available." }
                        }
                    }
                    div { class: "diagnostics-section",
                        h3 { "Sending Stats" }
                        if let Some(data) = &send_stats {
                            pre { "{data}" }
                        } else {
                            p { "No sending data available." }
                        }
                    }
                    div { class: "diagnostics-section",
                        h3 { "Encoder Settings" }
                        if let Some(data) = &enc_settings {
                            pre { "{data}" }
                        } else {
                            p { "No encoder settings available." }
                        }
                    }
                    div { class: "diagnostics-section",
                        h3 { "Media Status" }
                        pre { "{media_status}" }
                    }
                }
            }
        }
    }
}

/// The live "Simulcast layers" section, extracted into its OWN component so its
/// 4 Hz refresh tick re-renders only this small subtree — NOT the parent
/// [`Diagnostics`] body (which re-executes the expensive NetEq prelude). This is
/// the scoped-subscription pattern the perf meters already use (perf review #1).
///
/// Owns the 250 ms `gloo` `Interval` (gated to `is_open`, dropped on unmount via
/// `use_drop`) and the three live reads off the `DiagnosticsReader`. The reader's
/// closures touch encoder atomics / client per-peer state, so this subtree must
/// re-render periodically; the reads are cheap (counts / min-max / a small
/// per-peer Vec). `reader` is `None` until `Host` publishes it (or when
/// diagnostics aren't wired) → the section renders nothing.
#[component]
fn SimulcastLayersSection(is_open: bool, reader: Option<DiagnosticsReader>) -> Element {
    // 4 Hz refresh tick scoped to THIS component. Gated to `is_open`; the handle
    // lives in a `use_hook` cell and `use_drop` cancels it on unmount.
    let mut tick = use_signal(|| 0u64);
    {
        type IntervalCell = Rc<std::cell::RefCell<Option<gloo_timers::callback::Interval>>>;
        let cell: IntervalCell = use_hook(|| Rc::new(std::cell::RefCell::new(None)));
        let cell_effect = cell.clone();
        use_effect(move || {
            if is_open {
                let interval = gloo_timers::callback::Interval::new(250, move || {
                    let next = tick.peek().wrapping_add(1);
                    tick.set(next);
                });
                *cell_effect.borrow_mut() = Some(interval);
            } else {
                *cell_effect.borrow_mut() = None;
            }
        });
        use_drop(move || {
            *cell.borrow_mut() = None;
        });
    }
    // Subscribe this subtree (only) to the throttled refresh.
    let _ = tick();

    // No reader wired → render nothing (no empty "Simulcast layers" heading).
    let Some(reader) = reader.as_ref() else {
        return rsx! {};
    };

    let summary_line = format_simulcast_summary(&reader.summary);
    let send_video_snap = (reader.send_video)();
    let send_screen_snap = (reader.send_screen)();
    let per_peer_receive = (reader.per_peer_receive)();

    rsx! {
        div { class: "diagnostics-section",
            h3 { "Simulcast layers" }
            p { class: "simulcast-effective", "{summary_line}" }
            // The SEND ladder labels rungs 0-based (L0 = base) while the RECEIVE
            // breakdown labels layers 1-based ("L1/3"); a one-line hint reconciles
            // the two conventions under this single heading (#10).
            p { class: "simulcast-note", "Send rungs are 0-based (L0 = base); receive layers are 1-based." }
            SimulcastSendLadder {
                title: "Video (sending)",
                not_sharing_text: "Camera — off",
                snap: send_video_snap,
            }
            SimulcastSendLadder {
                title: "Screen (sending)",
                not_sharing_text: "Screen — not sharing",
                snap: send_screen_snap,
            }
            SimulcastReceiveBreakdown { peers: per_peer_receive }
        }
    }
}

/// One SEND stream's per-layer simulcast ladder for the Diagnostics "Simulcast
/// layers" section (#1095 §6 MOVE — relocated from the Performance panel). One
/// chip per EFFECTIVE layer (res + bitrate), styled active vs shed. `snap` is
/// `None` when the source is off (camera off / not sharing) → a static line.
#[component]
fn SimulcastSendLadder(
    title: &'static str,
    /// Static line shown when `snap` is `None` (source off / not sharing).
    not_sharing_text: &'static str,
    snap: Option<videocall_client::SimulcastSendSnapshot>,
) -> Element {
    let Some(snap) = snap else {
        return rsx! {
            div { class: "simulcast-send",
                span { class: "simulcast-send-title", "{title}" }
                span { class: "simulcast-send-static", "{not_sharing_text}" }
            }
        };
    };

    // Single-layer → static line (no per-layer ladder to show).
    if !snap.simulcast_active {
        let detail = snap
            .layers
            .first()
            .map(|l| {
                format!(
                    "Single layer · {} · {}",
                    format_send_layer_short(l.width, l.height),
                    format_kbps_compact(l.bitrate_kbps)
                )
            })
            .unwrap_or_else(|| "Single layer".to_string());
        return rsx! {
            div { class: "simulcast-send",
                span { class: "simulcast-send-title", "{title}" }
                span { class: "simulcast-send-static", "{detail}" }
            }
        };
    }

    let header = format_send_header(&snap);
    let total = format_send_total_kbps(&snap);
    let header_line = if total == 0 {
        header
    } else {
        format!("{header} · {} total", format_mbps(total))
    };
    let max_kbps = snap
        .layers
        .iter()
        .map(|l| l.bitrate_kbps)
        .max()
        .unwrap_or(1)
        .max(1);

    rsx! {
        div { class: "simulcast-send",
            span { class: "simulcast-send-title", "{title}" }
            span { class: "simulcast-send-header", "{header_line}" }
            div { class: "simulcast-send-ladder", "data-testid": "diag-simulcast-ladder",
                for layer in snap.layers.iter().cloned() {
                    {
                        let active = layer.layer_id < snap.active_layers;
                        let grow = (layer.bitrate_kbps as f32 / max_kbps as f32).max(0.4);
                        let chip_class = if active {
                            "simulcast-rung is-active"
                        } else {
                            "simulcast-rung is-shed"
                        };
                        let full = format_send_layer(
                            layer.layer_id, layer.width, layer.height, layer.bitrate_kbps,
                        );
                        let res_short = format_send_layer_short(layer.width, layer.height);
                        let kbps_short = format_kbps_compact(layer.bitrate_kbps);
                        rsx! {
                            div {
                                class: chip_class,
                                "data-testid": "diag-simulcast-rung-{layer.layer_id}",
                                title: "{full}",
                                style: "flex-grow: {grow};",
                                span { class: "simulcast-rung-id", "L{layer.layer_id}" }
                                span { class: "simulcast-rung-res", "{res_short}" }
                                span { class: "simulcast-rung-kbps", "{kbps_short}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The per-peer RECEIVE simulcast breakdown for the Diagnostics "Simulcast
/// layers" section (#1095 §6 MOVE). One block per kind (video / audio / screen):
/// the top-3 peers (highest layer first) + a "+N more" tail. Fed by the live
/// per-peer snapshot list.
#[component]
fn SimulcastReceiveBreakdown(peers: Vec<videocall_client::PeerReceiveDiag>) -> Element {
    rsx! {
        div { class: "simulcast-recv",
            span { class: "simulcast-recv-title", "Receiving (per peer)" }
            for (kind, kind_label) in [
                (PrefMediaKind::Video, "video"),
                (PrefMediaKind::Audio, "audio"),
                (PrefMediaKind::Screen, "screen"),
            ] {
                {
                    // `peers_for_kind` returns a fresh owned Vec; sort it IN PLACE
                    // (no extra `.clone()` per tick) and read the spread off the
                    // sorted order rather than building a second `layers` Vec.
                    let mut kind_peers = peers_for_kind(&peers, kind);
                    if kind_peers.is_empty() {
                        rsx! {}
                    } else {
                        let n = kind_peers.len();
                        // Sort by layer DESC (highest quality first) — top-3 shown.
                        kind_peers.sort_by_key(|p| std::cmp::Reverse(p.snap.layer_index));
                        // Spread = lowest..highest layer; the Vec is now DESC, so
                        // last = lowest, first = highest.
                        let lowest_layer = kind_peers.last().map(|p| p.snap.layer_index + 1).unwrap_or(1);
                        let highest_layer = kind_peers.first().map(|p| p.snap.layer_index + 1).unwrap_or(1);
                        let spread = if lowest_layer == highest_layer {
                            format!("L{lowest_layer}")
                        } else {
                            format!("L{lowest_layer}–L{highest_layer}")
                        };
                        let extra = n.saturating_sub(3);
                        kind_peers.truncate(3);
                        rsx! {
                            div { class: "simulcast-recv-kind",
                                "data-testid": "diag-simulcast-recv-{kind_label}",
                                span { class: "simulcast-recv-kind-head", "{kind_label} · {n} peer(s) · {spread}" }
                                for p in kind_peers.into_iter() {
                                    {
                                        // Borrow the label for the testid; the line owns its String.
                                        let session_id = p.session_id;
                                        let line = format_peer_kind_line(kind_label, Some(&p.snap))
                                            .map(|l| format!("{}: {l}", p.label))
                                            .unwrap_or(p.label);
                                        rsx! {
                                            div {
                                                class: "simulcast-recv-peer",
                                                "data-testid": "diag-simulcast-recv-peer-{session_id}",
                                                "{line}"
                                            }
                                        }
                                    }
                                }
                                if extra > 0 {
                                    span { class: "simulcast-recv-more",
                                        "data-testid": "diag-simulcast-recv-more-{kind_label}",
                                        "+{extra} more peer(s) at L{lowest_layer}"
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
