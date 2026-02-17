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

//! Diagnostics panel component for displaying call statistics and connection information

use dioxus::prelude::*;
use futures::future::{AbortHandle, Abortable};
use std::collections::HashMap;
use videocall_diagnostics::{subscribe, MetricValue};

#[derive(Props, Clone, PartialEq)]
pub struct DiagnosticsProps {
    /// Whether the diagnostics sidebar is open
    pub is_open: bool,
    /// Callback to close the diagnostics sidebar
    pub on_close: EventHandler<()>,
    /// Current video enabled state
    pub video_enabled: bool,
    /// Current microphone enabled state
    pub mic_enabled: bool,
    /// Current screen share state
    pub share_screen: bool,
}

#[derive(Clone, Default)]
struct DiagnosticsState {
    decoder_stats: Option<String>,
    sender_stats: Option<String>,
    connection_state: String,
    active_server: Option<String>,
    active_server_rtt: Option<f64>,
    neteq_buffer_per_peer: HashMap<String, Vec<u64>>,
}

#[component]
pub fn Diagnostics(props: DiagnosticsProps) -> Element {
    let mut state = use_signal(DiagnosticsState::default);
    let mut selected_peer = use_signal(|| "All Peers".to_string());

    // Subscribe to diagnostics when panel is open
    let is_open = props.is_open;
    use_effect(move || {
        if !is_open {
            // Clear state when closed
            state.set(DiagnosticsState::default());
            return;
        }

        let (abort_handle, abort_reg) = AbortHandle::new_pair();

        let fut = async move {
            let mut rx = subscribe();
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
                            state.write().decoder_stats = Some(text);
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
                            state.write().sender_stats = Some(text);
                        }
                    }
                    "connection_manager" => {
                        if evt.stream_id.is_none() {
                            for m in &evt.metrics {
                                match m.name {
                                    "election_state" => {
                                        if let MetricValue::Text(text) = &m.value {
                                            state.write().connection_state = text.clone();
                                        }
                                    }
                                    "active_server_url" => {
                                        if let MetricValue::Text(url) = &m.value {
                                            state.write().active_server = Some(url.clone());
                                        }
                                    }
                                    "active_server_rtt" => {
                                        if let MetricValue::F64(rtt) = &m.value {
                                            state.write().active_server_rtt = Some(*rtt);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "neteq" => {
                        for m in &evt.metrics {
                            if m.name == "audio_buffer_ms" {
                                if let MetricValue::U64(v) = &m.value {
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
                                    let mut s = state.write();
                                    let entry = s
                                        .neteq_buffer_per_peer
                                        .entry(target_peer.to_string())
                                        .or_default();
                                    entry.push(*v);
                                    if entry.len() > 50 {
                                        entry.remove(0);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        };

        let abortable = Abortable::new(fut, abort_reg);
        wasm_bindgen_futures::spawn_local(async move {
            let _ = abortable.await;
        });

        // Cleanup
        move || {
            abort_handle.abort();
        }
    });

    // Get available peers for selection
    let available_peers: Vec<String> = {
        let mut peers = vec!["All Peers".to_string()];
        let mut peer_keys: Vec<String> = state.read().neteq_buffer_per_peer.keys().cloned().collect();
        peer_keys.sort();
        peers.extend(peer_keys);
        peers
    };

    // Get buffer history for display
    let buffer_history = if *selected_peer.read() == "All Peers" {
        let mut aggregated = Vec::new();
        for buf in state.read().neteq_buffer_per_peer.values() {
            aggregated.extend(buf.iter().cloned());
        }
        aggregated
    } else {
        state
            .read()
            .neteq_buffer_per_peer
            .get(&*selected_peer.read())
            .cloned()
            .unwrap_or_default()
    };

    let avg_buffer = if buffer_history.is_empty() {
        0
    } else {
        buffer_history.iter().sum::<u64>() / buffer_history.len() as u64
    };

    rsx! {
        div {
            id: "diagnostics-sidebar",
            class: if props.is_open { "visible" } else { "" },
            div { class: "sidebar-header",
                h2 { "Call Diagnostics" }
                button {
                    class: "close-button",
                    onclick: move |_| props.on_close.call(()),
                    "×"
                }
            }
            div { class: "sidebar-content",
                // Application Version
                div { class: "diagnostics-section",
                    h3 { "Application Version" }
                    pre { "VideoCall UI: {}", env!("CARGO_PKG_VERSION") }
                }

                // Connection Status
                div { class: "diagnostics-section",
                    h3 { "Connection Status" }
                    div { class: "status-grid",
                        div { class: "status-item",
                            span { class: "status-label", "State:" }
                            span {
                                class: "status-value",
                                "{state.read().connection_state.to_uppercase()}"
                            }
                        }
                        if let Some(server) = state.read().active_server.as_ref() {
                            div { class: "status-item",
                                span { class: "status-label", "Server:" }
                                span { class: "status-value server-url", "{server}" }
                            }
                        }
                        if let Some(rtt) = state.read().active_server_rtt {
                            div { class: "status-item",
                                span { class: "status-label", "RTT:" }
                                span { class: "status-value", "{rtt:.1}ms" }
                            }
                        }
                    }
                }

                // Peer Selection
                if available_peers.len() > 1 {
                    div { class: "diagnostics-section",
                        h3 { "Peer Selection" }
                        select {
                            class: "peer-selector",
                            onchange: move |evt| selected_peer.set(evt.value()),
                            value: "{selected_peer}",
                            for peer in available_peers.iter() {
                                option { value: "{peer}", "{peer}" }
                            }
                        }
                    }
                }

                // NetEQ Buffer Stats
                div { class: "diagnostics-section",
                    h3 { "Audio Buffer" }
                    p { "Average buffer: {avg_buffer}ms" }
                    p { "Samples: {}", buffer_history.len() }
                }

                // Decoder Stats
                div { class: "diagnostics-section",
                    h3 { "Reception Stats" }
                    if let Some(data) = state.read().decoder_stats.as_ref() {
                        pre { "{data}" }
                    } else {
                        p { "No reception data available." }
                    }
                }

                // Sender Stats
                div { class: "diagnostics-section",
                    h3 { "Sending Stats" }
                    if let Some(data) = state.read().sender_stats.as_ref() {
                        pre { "{data}" }
                    } else {
                        p { "No sending data available." }
                    }
                }

                // Media Status
                div { class: "diagnostics-section",
                    h3 { "Media Status" }
                    pre {
                        "Video: {}\nAudio: {}\nScreen Share: {}",
                        if props.video_enabled { "Enabled" } else { "Disabled" },
                        if props.mic_enabled { "Enabled" } else { "Disabled" },
                        if props.share_screen { "Enabled" } else { "Disabled" }
                    }
                }
            }
        }
    }
}
