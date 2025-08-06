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

use log::{debug, warn};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use wasm_bindgen_futures::spawn_local;
use web_time::{SystemTime, UNIX_EPOCH};
use yew::prelude::Callback;

/// Health data cached for a specific peer
#[derive(Debug, Clone)]
pub struct PeerHealthData {
    pub peer_id: String,
    pub last_neteq_stats: Option<Value>,
    pub last_video_stats: Option<Value>,
    pub can_listen: bool,
    pub can_see: bool,
    pub last_update_ms: u64,
}

impl PeerHealthData {
    pub fn new(peer_id: String) -> Self {
        Self {
            peer_id,
            last_neteq_stats: None,
            last_video_stats: None,
            can_listen: false,
            can_see: false,
            last_update_ms: 0,
        }
    }

    pub fn update_audio_stats(&mut self, neteq_stats: Value) {
        self.last_neteq_stats = Some(neteq_stats);
        self.can_listen = true; // If we're receiving audio stats, we can listen
        self.last_update_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    }

    pub fn update_video_stats(&mut self, video_stats: Value) {
        self.last_video_stats = Some(video_stats);
        self.can_see = true; // If we're receiving video stats, we can see
        self.last_update_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
    }

    /// Mark peer as no longer sending audio (timeout)
    pub fn mark_audio_timeout(&mut self) {
        self.can_listen = false;
    }

    /// Mark peer as no longer sending video (timeout)
    pub fn mark_video_timeout(&mut self) {
        self.can_see = false;
    }
}

/// Health reporter that collects diagnostics and sends health packets
#[derive(Debug)]
pub struct HealthReporter {
    session_id: String,
    meeting_id: String, // Add meeting_id field
    reporting_peer: String,
    peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>>,
    send_packet_callback: Option<Callback<PacketWrapper>>,
    health_interval_ms: u64,
}

impl HealthReporter {
    /// Create a new health reporter
    pub fn new(session_id: String, reporting_peer: String) -> Self {
        Self {
            session_id,
            meeting_id: "".to_string(), // Will be set later
            reporting_peer,
            peer_health_data: Rc::new(RefCell::new(HashMap::new())),
            send_packet_callback: None,
            health_interval_ms: 5000, // Send health every 5 seconds
        }
    }

    /// Set the meeting ID
    pub fn set_meeting_id(&mut self, meeting_id: String) {
        self.meeting_id = meeting_id;
    }

    /// Set the callback for sending packets
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>) {
        self.send_packet_callback = Some(callback);
    }

    /// Set health reporting interval
    pub fn set_health_interval(&mut self, interval_ms: u64) {
        self.health_interval_ms = interval_ms;
    }

    /// Start subscribing to real diagnostics events via videocall_diagnostics
    pub fn start_diagnostics_subscription(&self) {
        let peer_health_data = Rc::downgrade(&self.peer_health_data);

        spawn_local(async move {
            debug!("Started health diagnostics subscription");

            let mut receiver = subscribe();
            while let Ok(event) = receiver.recv().await {
                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    Self::process_diagnostics_event(event, &peer_health_data);
                } else {
                    debug!("HealthReporter dropped, stopping diagnostics subscription");
                    break;
                }
            }
        });
    }

    /// Process a diagnostics event and update peer health data
    fn process_diagnostics_event(
        event: DiagEvent,
        peer_health_data: &Rc<RefCell<HashMap<String, PeerHealthData>>>,
    ) {
        let stream_id = event
            .stream_id
            .clone()
            .unwrap_or_else(|| "unknown->unknown".to_string());

        // Parse the new format: "reporting_peer->target_peer"
        let parts: Vec<&str> = stream_id.split("->").collect();
        let (reporting_peer, target_peer) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            ("unknown", "unknown")
        };

        // Handle NetEQ events (audio)
        if event.subsystem == "neteq" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                for metric in &event.metrics {
                    match metric.name {
                        "stats_json" => {
                            if let MetricValue::Text(json_str) = &metric.value {
                                if let Ok(neteq_json) = serde_json::from_str::<Value>(json_str) {
                                    peer_data.update_audio_stats(neteq_json);
                                    peer_data.can_listen = true;
                                    debug!(
                                        "Updated NetEQ stats for peer: {} (from {})",
                                        target_peer, reporting_peer
                                    );
                                }
                            }
                        }
                        "audio_buffer_ms" => {
                            if let MetricValue::U64(buffer_ms) = &metric.value {
                                // Update can_listen based on buffer health
                                peer_data.can_listen = *buffer_ms > 0;
                                debug!(
                                    "Updated audio health (buffer: {}ms) for peer: {} (from {})",
                                    buffer_ms, target_peer, reporting_peer
                                );
                            }
                        }
                        "packets_awaiting_decode" => {
                            if let MetricValue::U64(packets) = &metric.value {
                                debug!(
                                    "Updated packets awaiting decode: {packets} for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Handle decoder events (from local DiagnosticManager)
        else if event.subsystem == "decoder" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                for metric in &event.metrics {
                    match metric.name {
                        "fps" => {
                            if let MetricValue::F64(fps) = &metric.value {
                                // Update health based on FPS (consider >0 as active)
                                peer_data.can_see = *fps > 0.0;
                                debug!(
                                    "Updated video health (FPS: {fps:.2}) for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        "media_type" => {
                            if let MetricValue::Text(media_type) = &metric.value {
                                // Handle audio vs video media type
                                if media_type.contains("AUDIO") {
                                    peer_data.can_listen = true;
                                } else if media_type.contains("VIDEO")
                                    || media_type.contains("SCREEN")
                                {
                                    peer_data.can_see = true;
                                }
                                debug!(
                                    "Updated media health ({media_type}) for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Handle sender events (from local SenderDiagnosticManager)
        else if event.subsystem == "sender" {
            debug!(
                "Received sender event for peer: {} at {}",
                target_peer, event.ts_ms
            );
            // Sender events are mainly for server reporting, less impact on health status
        }
        // Handle video events
        else if event.subsystem == "video_decoder" || event.subsystem == "video" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                // Extract video stats from metrics
                let mut video_stats = json!({
                    "timestamp_ms": event.ts_ms
                });

                for metric in &event.metrics {
                    match metric.name {
                        "fps_received" => {
                            if let MetricValue::F64(fps) = &metric.value {
                                video_stats["fps_received"] = json!(fps);
                                peer_data.can_see = *fps > 0.0;
                            }
                        }
                        "frames_decoded" => {
                            if let MetricValue::U64(frames) = &metric.value {
                                video_stats["frames_decoded"] = json!(frames);
                            }
                        }
                        "bitrate_kbps" => {
                            if let MetricValue::U64(bitrate) = &metric.value {
                                video_stats["bitrate_kbps"] = json!(bitrate);
                            }
                        }
                        _ => {}
                    }
                }

                peer_data.update_video_stats(video_stats);
                debug!("Updated video health for peer: {target_peer}");
            }
        }
    }

    /// Start periodic health reporting
    pub fn start_health_reporting(&self) {
        if self.send_packet_callback.is_none() {
            warn!("Cannot start health reporting: no send packet callback set");
            return;
        }

        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let session_id = self.session_id.clone();
        let meeting_id = self.meeting_id.clone();
        let reporting_peer = self.reporting_peer.clone();
        let send_callback = self.send_packet_callback.clone().unwrap();
        let interval_ms = self.health_interval_ms;

        spawn_local(async move {
            debug!("Started health reporting with interval: {interval_ms}ms");

            loop {
                // Wait for the interval
                gloo_timers::future::TimeoutFuture::new(interval_ms as u32).await;

                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    if let Ok(health_map) = peer_health_data.try_borrow() {
                        let health_packet = Self::create_health_packet(
                            &session_id,
                            &meeting_id,
                            &reporting_peer,
                            &health_map,
                        );

                        if let Some(packet) = health_packet {
                            send_callback.emit(packet);
                            debug!("Sent health packet for session: {session_id}");
                        }
                    }
                } else {
                    debug!("HealthReporter dropped, stopping health reporting");
                    break;
                }
            }
        });
    }

    /// Create a health packet from current peer health data
    fn create_health_packet(
        session_id: &str,
        meeting_id: &str,
        reporting_peer: &str,
        health_map: &HashMap<String, PeerHealthData>,
    ) -> Option<PacketWrapper> {
        if health_map.is_empty() {
            return None;
        }

        // Build the peer stats map for the connectivity matrix
        let mut peer_stats = serde_json::Map::new();

        for (peer_id, health_data) in health_map.iter() {
            let peer_stat = json!({
                "can_listen": health_data.can_listen,
                "can_see": health_data.can_see,
                "neteq_stats": health_data.last_neteq_stats,
                "video_stats": health_data.last_video_stats
            });
            peer_stats.insert(peer_id.clone(), peer_stat);
        }

        let health_data = json!({
            "session_id": session_id,
            "meeting_id": meeting_id,
            "reporting_peer": reporting_peer,
            "timestamp_ms": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            "peer_stats": peer_stats
        });

        let health_json = health_data.to_string();

        Some(PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            email: reporting_peer.to_string(),
            data: health_json.into_bytes(),
            ..Default::default()
        })
    }

    /// Remove a peer from health tracking
    pub fn remove_peer(&self, peer_id: &str) {
        if let Ok(mut health_map) = self.peer_health_data.try_borrow_mut() {
            health_map.remove(peer_id);
            debug!("Removed peer from health tracking: {peer_id}");
        }
    }

    /// Get current health summary for debugging
    pub fn get_health_summary(&self) -> Option<Value> {
        if let Ok(health_map) = self.peer_health_data.try_borrow() {
            let summary = health_map
                .iter()
                .map(|(peer_id, health_data)| {
                    (
                        peer_id.clone(),
                        json!({
                            "can_listen": health_data.can_listen,
                            "can_see": health_data.can_see,
                            "last_update_ms": health_data.last_update_ms
                        }),
                    )
                })
                .collect::<serde_json::Map<_, _>>();

            Some(Value::Object(summary))
        } else {
            None
        }
    }
}
