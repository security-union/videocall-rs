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
    AdvancedChartType, ChartType, NetEqAdvancedChart, NetEqChart, NetEqStats,
    NetEqStatusDisplay,
};
use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use gloo_timers::callback::Timeout;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
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
                "server_url" => { if let MetricValue::Text(url) = &metric.value { server.url = url.clone(); } }
                "server_type" => { if let MetricValue::Text(st) = &metric.value { server.server_type = st.clone(); } }
                "server_status" => { if let MetricValue::Text(status) = &metric.value { server.status = status.clone(); } }
                "server_rtt" => { if let MetricValue::F64(rtt) = &metric.value { server.rtt = Some(*rtt); } }
                "server_active" => { if let MetricValue::U64(active) = &metric.value { server.active = *active > 0; } }
                "server_connected" => { if let MetricValue::U64(connected) = &metric.value { server.connected = *connected > 0; } }
                "measurement_count" => { if let MetricValue::U64(count) = &metric.value { server.measurement_count = Some(*count); } }
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
        match serde_json::from_str::<crate::components::neteq_chart::RawNetEqStats>(neteq_stats_str)
        {
            Ok(raw_stat) => {
                let stat: NetEqStats = raw_stat.into();
                stats.push(stat);
            }
            Err(_) => {}
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
) -> Element {
    let mut selected_peer = use_signal(|| "All Peers".to_string());
    let mut diagnostics_data = use_signal(|| None::<String>);
    let mut sender_stats = use_signal(|| None::<String>);
    let mut connection_manager_state = use_signal(|| None::<String>);
    let mut neteq_stats_per_peer = use_signal(HashMap::<String, Vec<String>>::new);
    let mut neteq_buffer_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut neteq_jitter_per_peer = use_signal(HashMap::<String, Vec<u64>>::new);
    let mut encoder_settings = use_signal(|| None::<String>);
    let prev_abort_handle = use_hook(|| Rc::new(RefCell::new(None::<AbortHandle>)));

    // Subscribe/unsubscribe on open/close
    use_effect(move || {
        // Abort any previously spawned subscription
        if let Some(h) = prev_abort_handle.borrow_mut().take() {
            h.abort();
        }

        if !is_open {
            diagnostics_data.set(None);
            sender_stats.set(None);
            encoder_settings.set(None);
            connection_manager_state.set(None);
            neteq_stats_per_peer.set(HashMap::new());
            neteq_buffer_per_peer.set(HashMap::new());
            neteq_jitter_per_peer.set(HashMap::new());
            return;
        }

        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        *prev_abort_handle.borrow_mut() = Some(abort_handle);

        let connection_events_async = Rc::new(RefCell::new(Vec::<SerializableDiagEvent>::new()));
        let neteq_stats_async = Rc::new(RefCell::new(HashMap::<String, Vec<String>>::new()));
        let neteq_buffer_async = Rc::new(RefCell::new(HashMap::<String, Vec<u64>>::new()));
        let neteq_jitter_async = Rc::new(RefCell::new(HashMap::<String, Vec<u64>>::new()));
        let stats_flush_timeout: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));
        let buffer_flush_timeout: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));
        let jitter_flush_timeout: Rc<RefCell<Option<Timeout>>> = Rc::new(RefCell::new(None));

        let fut = {
            let connection_events_async = connection_events_async.clone();
            let neteq_stats_async = neteq_stats_async.clone();
            let neteq_buffer_async = neteq_buffer_async.clone();
            let neteq_jitter_async = neteq_jitter_async.clone();
            let stats_flush_timeout = stats_flush_timeout.clone();
            let buffer_flush_timeout = buffer_flush_timeout.clone();
            let jitter_flush_timeout = jitter_flush_timeout.clone();

            async move {
                let mut rx = subscribe();
                while let Ok(evt) = rx.recv().await {
                    match evt.subsystem {
                        "decoder" => {
                            let mut text = String::new();
                            for m in &evt.metrics {
                                match m.name {
                                    "fps" => { if let MetricValue::F64(v) = &m.value { text.push_str(&format!("FPS: {v:.2}\n")); } }
                                    "bitrate_kbps" => { if let MetricValue::F64(v) = &m.value { text.push_str(&format!("Bitrate: {v:.1} kbps\n")); } }
                                    "media_type" => { if let MetricValue::Text(t) = &m.value { text.push_str(&format!("Media Type: {t}\n")); } }
                                    _ => {}
                                }
                            }
                            if !text.is_empty() {
                                let peer_id = evt.stream_id.clone().unwrap_or_else(|| "unknown".to_string());
                                text.push_str(&format!("Peer: {}\nTimestamp: {}\n", peer_id, evt.ts_ms));
                                diagnostics_data.set(Some(text));
                            }
                        }
                        "sender" => {
                            let mut text = String::new();
                            for m in &evt.metrics {
                                match m.name {
                                    "sender_id" => { if let MetricValue::Text(v) = &m.value { text.push_str(&format!("Sender: {v}\n")); } }
                                    "target_id" => { if let MetricValue::Text(v) = &m.value { text.push_str(&format!("Target: {v}\n")); } }
                                    "media_type" => { if let MetricValue::Text(v) = &m.value { text.push_str(&format!("Media Type: {v}\n")); } }
                                    _ => {}
                                }
                            }
                            if !text.is_empty() {
                                text.push_str(&format!("Timestamp: {}\n", evt.ts_ms));
                                sender_stats.set(Some(text));
                            }
                        }
                        "neteq" => {
                            for m in &evt.metrics {
                                match m.name {
                                    "stats_json" => {
                                        if let MetricValue::Text(json) = &m.value {
                                            let stream_id = evt.stream_id.clone().unwrap_or_else(|| "unknown->unknown".to_string());
                                            let parts: Vec<&str> = stream_id.split("->").collect();
                                            let target_peer = if parts.len() == 2 { parts[1] } else { "unknown" };
                                            {
                                                let mut stats = neteq_stats_async.borrow_mut();
                                                let entry = stats.entry(target_peer.to_string()).or_default();
                                                entry.push(json.clone());
                                                if entry.len() > 60 { entry.remove(0); }
                                            }
                                            if let Some(t) = stats_flush_timeout.borrow_mut().take() { t.cancel(); }
                                            {
                                                let nsa = neteq_stats_async.clone();
                                                let handle = Timeout::new(180, move || {
                                                    neteq_stats_per_peer.set(nsa.borrow().clone());
                                                });
                                                *stats_flush_timeout.borrow_mut() = Some(handle);
                                            }
                                        }
                                    }
                                    "audio_buffer_ms" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            let stream_id = evt.stream_id.clone().unwrap_or_else(|| "unknown->unknown".to_string());
                                            let parts: Vec<&str> = stream_id.split("->").collect();
                                            let target_peer = if parts.len() == 2 { parts[1] } else { "unknown" };
                                            {
                                                let mut buffer = neteq_buffer_async.borrow_mut();
                                                let entry = buffer.entry(target_peer.to_string()).or_default();
                                                entry.push(*v);
                                                if entry.len() > 50 { entry.remove(0); }
                                            }
                                            if let Some(t) = buffer_flush_timeout.borrow_mut().take() { t.cancel(); }
                                            {
                                                let nba = neteq_buffer_async.clone();
                                                let handle = Timeout::new(180, move || {
                                                    neteq_buffer_per_peer.set(nba.borrow().clone());
                                                });
                                                *buffer_flush_timeout.borrow_mut() = Some(handle);
                                            }
                                        }
                                    }
                                    "jitter_buffer_delay_ms" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            let stream_id = evt.stream_id.clone().unwrap_or_else(|| "unknown->unknown".to_string());
                                            let parts: Vec<&str> = stream_id.split("->").collect();
                                            let target_peer = if parts.len() == 2 { parts[1] } else { "unknown" };
                                            {
                                                let mut jitter = neteq_jitter_async.borrow_mut();
                                                let entry = jitter.entry(target_peer.to_string()).or_default();
                                                entry.push(*v);
                                                if entry.len() > 50 { entry.remove(0); }
                                            }
                                            if let Some(t) = jitter_flush_timeout.borrow_mut().take() { t.cancel(); }
                                            {
                                                let nja = neteq_jitter_async.clone();
                                                let handle = Timeout::new(180, move || {
                                                    neteq_jitter_per_peer.set(nja.borrow().clone());
                                                });
                                                *jitter_flush_timeout.borrow_mut() = Some(handle);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "connection_manager" => {
                            {
                                let mut events = connection_events_async.borrow_mut();
                                events.push(SerializableDiagEvent::from(evt));
                                if events.len() > 20 { events.remove(0); }
                            }
                            let events = connection_events_async.borrow().clone();
                            let serialized = serde_json::to_string(&events).unwrap_or_default();
                            connection_manager_state.set(Some(serialized));
                        }
                        _ => {}
                    }
                }
            }
        };
        let abortable = Abortable::new(fut, abort_reg);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = abortable.await;
        });

        // abort on cleanup is handled by drop of the spawned future
    });

    // Get list of available peers
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
        if all.is_empty() { Vec::new() } else { parse_neteq_stats_history(&all.join("\n")) }
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
        for buf in buffer_map.values() { ab.extend(buf.iter().cloned()); }
        let mut aj = Vec::new();
        for jit in jitter_map.values() { aj.extend(jit.iter().cloned()); }
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
    let enc_settings = encoder_settings();
    let video_str = if video_enabled { "Enabled" } else { "Disabled" };
    let audio_str = if mic_enabled { "Enabled" } else { "Disabled" };
    let screen_str = if share_screen { "Enabled" } else { "Disabled" };
    let media_status = format!("Video: {video_str}\nAudio: {audio_str}\nScreen Share: {screen_str}");
    let version = env!("CARGO_PKG_VERSION");
    let version_str = format!("VideoCall UI: {version}");
    let peer_info = format!("Showing statistics for: {current_peer}");

    rsx! {
        div {
            id: "diagnostics-sidebar",
            class: if is_open { "visible" } else { "" },
            div { class: "sidebar-header",
                h2 { "Call Diagnostics" }
                button { class: "close-button", onclick: move |_| on_close.call(()), "\u{00d7}" }
            }
            div { class: "sidebar-content",
                div { class: "diagnostics-section",
                    h3 { "Application Version" }
                    pre { "{version_str}" }
                }
                div { class: "diagnostics-section",
                    h3 { "Connection Manager" }
                    ConnectionManagerDisplay { connection_manager_state: conn_state }
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
                                option {
                                    value: "{peer}",
                                    selected: peer == &current_peer,
                                    "{peer}"
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
                            for (peer_id, _) in stats_map.iter() {
                                {
                                    let latest_buffer = buffer_map.get(peer_id).and_then(|b| b.last()).unwrap_or(&0);
                                    let latest_jitter = jitter_map.get(peer_id).and_then(|j| j.last()).unwrap_or(&0);
                                    let summary = format!("Buffer: {latest_buffer}ms, Jitter: {latest_jitter}ms");
                                    rsx! {
                                        div { class: "peer-summary-item",
                                            strong { "{peer_id}" }
                                            span { "{summary}" }
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
