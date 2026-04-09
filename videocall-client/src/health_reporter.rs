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

use crate::connection::ConnectionController;
use crate::decode::peer_decode_manager::keyframe_requests_sent_count;
use log::{debug, warn};
use protobuf::Message;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU32, Ordering};
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use videocall_types::protos::health_packet::{
    HealthPacket as PbHealthPacket, NetEqNetwork as PbNetEqNetwork,
    NetEqOperationCounters as PbNetEqOperationCounters, NetEqStats as PbNetEqStats,
    PeerStats as PbPeerStats, VideoStats as PbVideoStats,
};
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;
use wasm_bindgen_futures::spawn_local;
use web_time::{SystemTime, UNIX_EPOCH};

/// Health data cached for a specific peer
#[derive(Debug, Clone)]
pub struct PeerHealthData {
    pub peer_id: String,
    pub last_neteq_stats: Option<Value>,
    pub last_video_stats: Option<Value>,
    /// Sender's self-reported audio state (from peer heartbeat metadata).
    pub audio_enabled: bool,
    /// Sender's self-reported video state (from peer heartbeat metadata).
    pub video_enabled: bool,
    pub last_update_ms: u64,
    /// Timestamp of last audio stats update (ms since epoch). 0 = never received.
    pub last_audio_update_ms: u64,
    /// Timestamp of last video stats update (ms since epoch). 0 = never received.
    pub last_video_update_ms: u64,
}

impl PeerHealthData {
    pub fn new(peer_id: String) -> Self {
        Self {
            peer_id,
            last_neteq_stats: None,
            last_video_stats: None,
            audio_enabled: false,
            video_enabled: false,
            last_update_ms: 0,
            last_audio_update_ms: 0,
            last_video_update_ms: 0,
        }
    }

    pub fn update_audio_stats(&mut self, neteq_stats: Value) {
        self.last_neteq_stats = Some(neteq_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_audio_update_ms = now_ms;
    }

    pub fn update_video_stats(&mut self, video_stats: Value) {
        self.last_video_stats = Some(video_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_video_update_ms = now_ms;
    }
}

/// Health reporter that collects diagnostics and sends health packets
#[derive(Debug)]
pub struct HealthReporter {
    session_id: Rc<RefCell<String>>,
    meeting_id: String,
    display_name: String,
    reporting_peer: String,
    peer_health_data: Rc<RefCell<HashMap<String, PeerHealthData>>>,
    send_packet_callback: Option<Callback<PacketWrapper>>,
    health_interval_ms: u64,
    reporting_audio_enabled: Rc<RefCell<bool>>,
    reporting_video_enabled: Rc<RefCell<bool>>,
    active_server_url: Rc<RefCell<Option<String>>>,
    active_server_type: Rc<RefCell<Option<String>>>,
    active_server_rtt_ms: Rc<RefCell<Option<f64>>>,
    connection_controller: Rc<RefCell<Option<Rc<ConnectionController>>>>,
    /// Adaptive video tier index from CameraEncoder (0=best, 7=minimal).
    /// Wrapped in RefCell so `set_adaptive_tier_sources` (called after
    /// `start_health_reporting`) can swap the inner Rc and the spawned loop
    /// picks up the new atomic on its next tick.
    adaptive_video_tier: Rc<RefCell<Rc<AtomicU32>>>,
    /// Adaptive audio tier index from CameraEncoder (0=high, 3=emergency).
    adaptive_audio_tier: Rc<RefCell<Rc<AtomicU32>>>,
}

impl HealthReporter {
    /// Create a new health reporter
    pub fn new(session_id: String, reporting_peer: String, health_interval_ms: u64) -> Self {
        Self {
            session_id: Rc::new(RefCell::new(session_id)),
            meeting_id: String::new(),
            display_name: String::new(),
            reporting_peer,
            peer_health_data: Rc::new(RefCell::new(HashMap::new())),
            send_packet_callback: None,
            health_interval_ms,
            reporting_audio_enabled: Rc::new(RefCell::new(false)),
            reporting_video_enabled: Rc::new(RefCell::new(false)),
            active_server_url: Rc::new(RefCell::new(None)),
            active_server_type: Rc::new(RefCell::new(None)),
            active_server_rtt_ms: Rc::new(RefCell::new(None)),
            connection_controller: Rc::new(RefCell::new(None)),
            adaptive_video_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            adaptive_audio_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
        }
    }

    /// Set the meeting ID
    pub fn set_meeting_id(&mut self, meeting_id: String) {
        self.meeting_id = meeting_id;
    }

    /// Update the session_id to the server-assigned value received via SESSION_ASSIGNED.
    /// Must be called when SESSION_ASSIGNED arrives so health packets carry the correct
    /// session_id that matches the PacketWrapper.session_id used for room traffic.
    pub fn set_session_id(&mut self, session_id: String) {
        *self.session_id.borrow_mut() = session_id;
    }

    /// Set the display name for health packet reporting
    pub fn set_display_name(&mut self, display_name: String) {
        self.display_name = display_name;
    }

    /// Update sender self-state: audio enabled (authoritative)
    pub fn set_reporting_audio_enabled(&self, enabled: bool) {
        if let Ok(mut ae) = self.reporting_audio_enabled.try_borrow_mut() {
            *ae = enabled;
        }
    }

    /// Update sender self-state: video enabled (authoritative)
    pub fn set_reporting_video_enabled(&self, enabled: bool) {
        if let Ok(mut ve) = self.reporting_video_enabled.try_borrow_mut() {
            *ve = enabled;
        }
    }

    /// Set the callback for sending packets
    pub fn set_send_packet_callback(&mut self, callback: Callback<PacketWrapper>) {
        self.send_packet_callback = Some(callback);
    }

    /// Set health reporting interval
    pub fn set_health_interval(&mut self, interval_ms: u64) {
        self.health_interval_ms = interval_ms;
    }

    /// Set the connection controller reference for communication metrics
    pub fn set_connection_controller(&self, connection_controller: Rc<ConnectionController>) {
        *self.connection_controller.borrow_mut() = Some(connection_controller);
    }

    /// Bind the adaptive quality tier atomics from a CameraEncoder so the
    /// health reporter can include the current encoding tiers in each packet.
    pub fn set_adaptive_tier_sources(
        &mut self,
        video_tier: Rc<AtomicU32>,
        audio_tier: Rc<AtomicU32>,
    ) {
        *self.adaptive_video_tier.borrow_mut() = video_tier;
        *self.adaptive_audio_tier.borrow_mut() = audio_tier;
    }

    /// Start subscribing to real diagnostics events via videocall_diagnostics
    pub fn start_diagnostics_subscription(&self) {
        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);

        spawn_local(async move {
            debug!("Started health diagnostics subscription");

            let mut receiver = subscribe();
            while let Ok(event) = receiver.recv().await {
                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    // Capture self-state from sender diagnostics events
                    if event.subsystem == "sender" {
                        if let (Some(ae), Some(ve)) =
                            (Weak::upgrade(&audio_enabled), Weak::upgrade(&video_enabled))
                        {
                            for m in &event.metrics {
                                match m.name {
                                    "sender_audio_enabled" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            *ae.borrow_mut() = *v > 0;
                                        }
                                    }
                                    "sender_video_enabled" => {
                                        if let MetricValue::U64(v) = &m.value {
                                            *ve.borrow_mut() = *v > 0;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // Capture connection manager elected server and RTT
                    if event.subsystem == "connection_manager" {
                        if let (Some(url_rc), Some(typ_rc), Some(rtt_rc)) = (
                            Weak::upgrade(&active_server_url),
                            Weak::upgrade(&active_server_type),
                            Weak::upgrade(&active_server_rtt_ms),
                        ) {
                            for m in &event.metrics {
                                match m.name {
                                    "active_server_url" => {
                                        if let MetricValue::Text(v) = &m.value {
                                            *url_rc.borrow_mut() = Some(v.clone());
                                        }
                                    }
                                    "active_server_type" => {
                                        if let MetricValue::Text(v) = &m.value {
                                            *typ_rc.borrow_mut() = Some(v.clone());
                                        }
                                    }
                                    "active_server_rtt" => {
                                        if let MetricValue::F64(v) = &m.value {
                                            *rtt_rc.borrow_mut() = Some(*v);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
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
        // Prefer structured from/to fields if present; fall back to stream_id if set
        let mut reporting_peer: Option<String> = None;
        let mut target_peer: Option<String> = None;
        for metric in &event.metrics {
            match metric.name {
                "from_peer" => {
                    if let MetricValue::Text(s) = &metric.value {
                        reporting_peer = Some(s.clone());
                    }
                }
                "to_peer" => {
                    if let MetricValue::Text(s) = &metric.value {
                        target_peer = Some(s.clone());
                    }
                }
                _ => {}
            }
        }

        // Fallback to stream_id parsing if structured fields are absent
        if reporting_peer.is_none() || target_peer.is_none() {
            if let Some(sid) = event.stream_id.clone() {
                let parts: Vec<&str> = sid.split("->").collect();
                if parts.len() == 2 {
                    reporting_peer.get_or_insert(parts[0].to_string());
                    target_peer.get_or_insert(parts[1].to_string());
                }
            }
        }
        let reporting_peer = reporting_peer.unwrap_or_else(|| "unknown".to_string());
        let target_peer = target_peer.unwrap_or_else(|| "unknown".to_string());

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
                                    debug!(
                                     "Updated NetEQ stats for peer: {target_peer} (from {reporting_peer})"
                                    );
                                }
                            }
                        }
                        "audio_buffer_ms" => {
                            if let MetricValue::U64(buffer_ms) = &metric.value {
                                debug!(
                                    "Updated audio health (buffer: {buffer_ms}ms) for peer: {target_peer} (from {reporting_peer})"
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
        // Handle sender events (from local SenderDiagnosticManager)
        else if event.subsystem == "sender" {
            debug!(
                "Received sender event for peer: {} at {}",
                target_peer, event.ts_ms
            );
            // Sender events are mainly for server reporting, less impact on health status
        }
        // Handle peer status events (mute/camera on/off)
        else if event.subsystem == "peer_status" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                for metric in &event.metrics {
                    match metric.name {
                        "audio_enabled" => {
                            if let MetricValue::U64(v) = &metric.value {
                                peer_data.audio_enabled = *v > 0;
                            }
                        }
                        "video_enabled" => {
                            if let MetricValue::U64(v) = &metric.value {
                                peer_data.video_enabled = *v > 0;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Handle video events
        else if event.subsystem == "video_decoder" || event.subsystem == "video" {
            if let Ok(mut health_map) = peer_health_data.try_borrow_mut() {
                let peer_data = health_map
                    .entry(target_peer.to_string())
                    .or_insert_with(|| PeerHealthData::new(target_peer.to_string()));

                // Extract video stats from metrics
                let mut video_stats = match &peer_data.last_video_stats {
                    Some(Value::Object(map)) => Value::Object(map.clone()),
                    _ => json!({}),
                };
                // Always update timestamp
                video_stats["timestamp_ms"] = json!(event.ts_ms);

                for metric in &event.metrics {
                    match metric.name {
                        "fps_received" => {
                            if let MetricValue::F64(fps) = &metric.value {
                                video_stats["fps_received"] = json!(fps);
                            }
                        }
                        "frames_buffered" | "packets_buffered" => match &metric.value {
                            MetricValue::U64(v) => {
                                video_stats["frames_buffered"] = json!(v);
                            }
                            MetricValue::F64(v) => {
                                video_stats["frames_buffered"] = json!(v);
                            }
                            _ => {}
                        },
                        "frames_decoded" => {
                            if let MetricValue::U64(frames) = &metric.value {
                                video_stats["frames_decoded"] = json!(frames);
                            }
                        }
                        "decode_errors_per_sec" => {
                            if let MetricValue::F64(error_rate) = &metric.value {
                                video_stats["decode_errors_per_sec"] = json!(error_rate);
                            }
                        }
                        "bitrate_kbps" => match &metric.value {
                            MetricValue::U64(bitrate) => {
                                video_stats["bitrate_kbps"] = json!(bitrate);
                            }
                            MetricValue::F64(bitrate) => {
                                video_stats["bitrate_kbps"] = json!(*bitrate as u64);
                            }
                            _ => {}
                        },
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
        let session_id = Rc::downgrade(&self.session_id);
        let meeting_id = self.meeting_id.clone();
        let reporting_peer = self.reporting_peer.clone();
        let display_name = self.display_name.clone();
        let send_callback = self.send_packet_callback.clone().unwrap();
        let interval_ms = self.health_interval_ms;
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);
        let connection_controller = Rc::downgrade(&self.connection_controller);
        let adaptive_video_tier = self.adaptive_video_tier.clone();
        let adaptive_audio_tier = self.adaptive_audio_tier.clone();

        spawn_local(async move {
            debug!("Started health reporting with interval: {interval_ms}ms");

            loop {
                // Wait for the interval
                gloo_timers::future::TimeoutFuture::new(interval_ms as u32).await;

                // Upgrade session_id Weak ref; if the HealthReporter was dropped, stop.
                let session_id_val = match Weak::upgrade(&session_id) {
                    Some(s) => s.borrow().clone(),
                    None => break,
                };

                if let Some(peer_health_data) = Weak::upgrade(&peer_health_data) {
                    if let Ok(health_map) = peer_health_data.try_borrow() {
                        let self_audio_enabled = Weak::upgrade(&audio_enabled)
                            .and_then(|ae| ae.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        let self_video_enabled = Weak::upgrade(&video_enabled)
                            .and_then(|ve| ve.try_borrow().ok().map(|v| *v))
                            .unwrap_or(false);
                        // Snapshot active connection info for this tick
                        let active_url = Weak::upgrade(&active_server_url)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_type = Weak::upgrade(&active_server_type)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| v.clone()));
                        let active_rtt = Weak::upgrade(&active_server_rtt_ms)
                            .and_then(|rc| rc.try_borrow().ok().and_then(|v| *v));

                        // Get communication metrics from connection controller
                        let (send_queue_bytes, packets_received_per_sec, packets_sent_per_sec) =
                            if let Some(cc_rc) = Weak::upgrade(&connection_controller) {
                                if let Ok(cc_opt) = cc_rc.try_borrow() {
                                    if let Some(cc) = cc_opt.as_ref() {
                                        // Calculate latest packet rates
                                        cc.calculate_packet_rates();
                                        (
                                            cc.get_send_queue_depth(),
                                            Some(cc.get_packets_received_per_sec()),
                                            Some(cc.get_packets_sent_per_sec()),
                                        )
                                    } else {
                                        (None, None, None)
                                    }
                                } else {
                                    (None, None, None)
                                }
                            } else {
                                (None, None, None)
                            };

                        let health_packet = Self::create_health_packet(
                            &session_id_val,
                            &meeting_id,
                            &reporting_peer,
                            &display_name,
                            &health_map,
                            self_audio_enabled,
                            self_video_enabled,
                            active_url,
                            active_type,
                            active_rtt,
                            send_queue_bytes,
                            packets_received_per_sec,
                            packets_sent_per_sec,
                            adaptive_video_tier.borrow().load(Ordering::Relaxed),
                            adaptive_audio_tier.borrow().load(Ordering::Relaxed),
                            videocall_transport::webtransport::datagram_drop_count(),
                            keyframe_requests_sent_count(),
                        );

                        if let Some(packet) = health_packet {
                            send_callback.emit(packet);
                            debug!("Sent health packet for session: {session_id_val}");
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
    #[allow(clippy::too_many_arguments)]
    fn create_health_packet(
        session_id: &str,
        meeting_id: &str,
        reporting_peer: &str,
        display_name: &str,
        health_map: &HashMap<String, PeerHealthData>,
        self_audio_enabled: bool,
        self_video_enabled: bool,
        active_server_url: Option<String>,
        active_server_type: Option<String>,
        active_server_rtt_ms: Option<f64>,
        send_queue_bytes: Option<u64>,
        packets_received_per_sec: Option<f64>,
        packets_sent_per_sec: Option<f64>,
        adaptive_video_tier: u32,
        adaptive_audio_tier: u32,
        datagram_drops_total: u64,
        keyframe_requests_sent_total: u64,
    ) -> Option<PacketWrapper> {
        if health_map.is_empty() {
            return None;
        }

        // Build protobuf HealthPacket with structured stats
        let mut pb = PbHealthPacket::new();
        pb.session_id = session_id.to_string();
        pb.meeting_id = meeting_id.to_string();
        pb.reporting_user_id = reporting_peer.as_bytes().to_vec();
        pb.timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        pb.reporting_audio_enabled = self_audio_enabled;
        pb.reporting_video_enabled = self_video_enabled;
        if !display_name.is_empty() {
            pb.display_name = Some(display_name.to_string());
        }

        // Include active connection info if available
        if let Some(url) = active_server_url {
            pb.active_server_url = url;
        }
        if let Some(typ) = active_server_type {
            pb.active_server_type = typ;
        }
        if let Some(rtt) = active_server_rtt_ms {
            pb.active_server_rtt_ms = rtt;
        }

        // Communication load metrics
        pb.send_queue_bytes = send_queue_bytes;
        pb.packets_received_per_sec = packets_received_per_sec;
        pb.packets_sent_per_sec = packets_sent_per_sec;

        // Receiver-side metrics: adaptive quality and transport health
        pb.adaptive_video_tier = Some(adaptive_video_tier);
        pb.adaptive_audio_tier = Some(adaptive_audio_tier);
        pb.datagram_drops_total = Some(datagram_drops_total);
        pb.keyframe_requests_sent_total = Some(keyframe_requests_sent_total);

        // Tab visibility and throttling
        #[cfg(target_arch = "wasm32")]
        {
            let tab_hidden = web_sys::window()
                .and_then(|w| w.document())
                .map(|d| d.hidden())
                .unwrap_or(false);
            pb.is_tab_visible = !tab_hidden;
            pb.is_tab_throttled = tab_hidden;

            // Memory usage (Chrome only)
            if let Some(window) = web_sys::window() {
                if let Some(perf) = window.performance() {
                    // Try to access performance.memory (Chrome extension)
                    if let Ok(memory) = js_sys::Reflect::get(&perf, &"memory".into()) {
                        if !memory.is_undefined() {
                            if let Ok(used) =
                                js_sys::Reflect::get(&memory, &"usedJSHeapSize".into())
                            {
                                if let Some(used_f64) = used.as_f64() {
                                    pb.memory_used_bytes = Some(used_f64 as u64);
                                }
                            }
                            if let Ok(total) =
                                js_sys::Reflect::get(&memory, &"jsHeapSizeLimit".into())
                            {
                                if let Some(total_f64) = total.as_f64() {
                                    pb.memory_total_bytes = Some(total_f64 as u64);
                                }
                            }
                        }
                    }
                }
            }
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        const STATS_STALE_MS: u64 = 5_000;

        for (peer_id, health_data) in health_map.iter() {
            // Freshness gate: stats older than 5s are stale (FPS/NetEQ trackers stop
            // emitting DiagEvents when no frames arrive, so timestamps stop advancing).
            let audio_fresh = health_data.last_audio_update_ms > 0
                && now_ms.saturating_sub(health_data.last_audio_update_ms) < STATS_STALE_MS;
            let video_fresh = health_data.last_video_update_ms > 0
                && now_ms.saturating_sub(health_data.last_video_update_ms) < STATS_STALE_MS;

            let mut ps = PbPeerStats::new();
            // can_listen/can_see: receiver-observed. True only while stream is fresh.
            ps.can_listen = audio_fresh;
            ps.can_see = video_fresh;
            // audio_enabled/video_enabled: sender's self-reported state from heartbeat.
            ps.audio_enabled = health_data.audio_enabled;
            ps.video_enabled = health_data.video_enabled;

            // NetEQ mapping
            if let Some(neteq) = &health_data.last_neteq_stats {
                let mut ns = PbNetEqStats::new();
                if let Some(v) = neteq.get("current_buffer_size_ms").and_then(|v| v.as_f64()) {
                    ns.current_buffer_size_ms = v;
                }
                if let Some(v) = neteq
                    .get("packets_awaiting_decode")
                    .and_then(|v| v.as_f64())
                {
                    ns.packets_awaiting_decode = v;
                }
                if let Some(v) = neteq.get("packets_per_sec").and_then(|v| v.as_f64()) {
                    ns.packets_per_sec = v;
                }
                if let Some(v) = neteq.get("target_delay_ms").and_then(|v| v.as_f64()) {
                    // Delay manager target: the algorithm's estimate of buffering needed
                    // to absorb observed network jitter. This is the real VoIP jitter metric.
                    ns.target_delay_ms = v;
                }

                // Calculate audio packet loss percentage from WINDOWED rates (not lifetime)
                // Use expand_per_sec (concealment events/sec) and packets_per_sec (packets/sec)
                let expand_per_sec = neteq
                    .get("network")
                    .and_then(|n| n.get("operation_counters"))
                    .and_then(|oc| oc.get("expand_per_sec"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                let packets_per_sec = neteq
                    .get("packets_per_sec")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                // Calculate loss % from windowed rates (resets every ~1 second).
                // Gate on >= 2.0 pps (matches quality-score gate): below that the
                // speaker is likely in DTX silence and the ratio is unreliable.
                // Clamp to 0–100: packet loss cannot exceed 100% by definition,
                // and unsynchronised window rollovers can momentarily inflate it.
                if packets_per_sec >= 2.0 {
                    ps.audio_packet_loss_pct =
                        ((expand_per_sec / packets_per_sec) * 100.0).clamp(0.0, 100.0);
                }

                if let Some(network) = neteq.get("network") {
                    let mut nn = PbNetEqNetwork::new();
                    if let Some(counters) = network.get("operation_counters") {
                        let mut oc = PbNetEqOperationCounters::new();
                        if let Some(v) = counters.get("normal_per_sec").and_then(|v| v.as_f64()) {
                            oc.normal_per_sec = v;
                        }
                        if let Some(v) = counters.get("expand_per_sec").and_then(|v| v.as_f64()) {
                            oc.expand_per_sec = v;
                        }
                        if let Some(v) = counters.get("accelerate_per_sec").and_then(|v| v.as_f64())
                        {
                            oc.accelerate_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("fast_accelerate_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.fast_accelerate_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("preemptive_expand_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.preemptive_expand_per_sec = v;
                        }
                        if let Some(v) = counters.get("merge_per_sec").and_then(|v| v.as_f64()) {
                            oc.merge_per_sec = v;
                        }
                        if let Some(v) = counters
                            .get("comfort_noise_per_sec")
                            .and_then(|v| v.as_f64())
                        {
                            oc.comfort_noise_per_sec = v;
                        }
                        if let Some(v) = counters.get("dtmf_per_sec").and_then(|v| v.as_f64()) {
                            oc.dtmf_per_sec = v;
                        }
                        if let Some(v) = counters.get("undefined_per_sec").and_then(|v| v.as_f64())
                        {
                            oc.undefined_per_sec = v;
                        }
                        nn.operation_counters = ::protobuf::MessageField::some(oc);
                    }
                    ns.network = ::protobuf::MessageField::some(nn);
                }
                ps.neteq_stats = ::protobuf::MessageField::some(ns);
            }

            // Video mapping
            if let Some(video) = &health_data.last_video_stats {
                let mut vs = PbVideoStats::new();
                if let Some(v) = video.get("fps_received").and_then(|v| v.as_f64()) {
                    vs.fps_received = v;
                }
                if let Some(v) = video.get("frames_buffered").and_then(|v| v.as_f64()) {
                    vs.frames_buffered = v;
                }
                if let Some(v) = video.get("frames_decoded").and_then(|v| v.as_u64()) {
                    vs.frames_decoded = v;
                }
                if let Some(v) = video.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                    vs.bitrate_kbps = v;
                }
                ps.video_stats = ::protobuf::MessageField::some(vs);

                // Extract decode_errors_per_sec (windowed rate) from video stats
                if let Some(error_rate) =
                    video.get("decode_errors_per_sec").and_then(|v| v.as_f64())
                {
                    ps.frames_dropped_per_sec = error_rate;
                }
            }

            // ── Quality scores ─────────────────────────────────────────────
            // Only set when the stream is active; absent = Grafana shows a gap,
            // not a misleading zero. audio_fresh/video_fresh computed above.

            // Audio quality (0-100): only meaningful when packets are flowing
            let audio_packets_per_sec = ps
                .neteq_stats
                .as_ref()
                .map(|n| n.packets_per_sec)
                .unwrap_or(0.0);

            if audio_fresh && audio_packets_per_sec >= 2.0 && health_data.audio_enabled {
                let conceal = ps
                    .neteq_stats
                    .as_ref()
                    .and_then(|n| n.network.as_ref())
                    .and_then(|net| net.operation_counters.as_ref())
                    .map(|oc| oc.expand_per_sec)
                    .unwrap_or(0.0);
                let loss = ps.audio_packet_loss_pct;

                // Penalties sum to 100 max.
                // Jitter (target_delay_ms) is intentionally excluded: in this stack it
                // settles at a fixed NetEQ default (~120ms) and carries no diagnostic
                // signal. Concealment already captures the downstream effect of real
                // jitter (late/lost packets → expand events → audible degradation).
                let conceal_penalty = (conceal / 10.0).min(1.0) * 70.0;
                let loss_penalty = (loss / 5.0).min(1.0) * 30.0;
                let score = (100.0 - conceal_penalty - loss_penalty).max(0.0);
                ps.audio_quality_score = Some(score);
            }

            // Video quality (0-100): only meaningful when frames are actively arriving.
            // fps > 0.0 already proves video is flowing; video_enabled (sender self-report
            // from peer_status events) is not required here and would suppress scores
            // if peer_status hasn't arrived yet.
            let fps = ps
                .video_stats
                .as_ref()
                .map(|v| v.fps_received)
                .unwrap_or(0.0);
            if video_fresh && fps > 0.0 {
                let dropped = ps.frames_dropped_per_sec;

                // Video health: measures whether video is present and stable, not
                // hardware FPS capability. A 15fps camera in low light is not a
                // "problem" — it is the camera doing auto-exposure correctly.
                //
                // fps >= 5  → 100  (video is working; FPS is hardware context, not quality)
                // fps 1–4   → 0–50 (near-frozen; something is likely wrong)
                // fps == 0  → handled by outer guard; score is absent (None)
                let video_health = if fps >= 5.0 { 100.0 } else { fps / 5.0 * 50.0 };
                // Decode error penalty: 0/s→0, 10+/s→−50
                let drop_penalty = (dropped / 10.0).min(1.0) * 50.0;
                let score = (video_health - drop_penalty).clamp(0.0, 100.0);
                ps.video_quality_score = Some(score);
            }

            // Call quality: worst of whichever streams are active
            let call_score = match (ps.audio_quality_score, ps.video_quality_score) {
                (Some(a), Some(v)) => Some(a.min(v)),
                (Some(a), None) => Some(a),
                (None, Some(v)) => Some(v),
                (None, None) => None,
            };
            ps.call_quality_score = call_score;

            pb.peer_stats.insert(peer_id.clone(), ps);
        }

        let bytes = pb.write_to_bytes().unwrap_or_default();
        Some(PacketWrapper {
            packet_type: PacketType::HEALTH.into(),
            user_id: reporting_peer.as_bytes().to_vec(),
            data: bytes,
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
                            "audio_enabled": health_data.audio_enabled,
                            "video_enabled": health_data.video_enabled,
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
