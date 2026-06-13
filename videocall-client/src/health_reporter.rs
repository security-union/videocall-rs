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
use crate::connection::{
    connection_handshake_failures, connection_session_drops, reelection_aborted_total,
    reelection_failed_total, reelection_preserved_total, reelection_proceeded_total,
};
use crate::decode::peer_decode_manager::keyframe_requests_sent_count;
use crate::diagnostics::adaptive_quality_manager::TierTransitionRecord;
use crate::encode::{
    camera_encoder_errors_closed_codec, camera_encoder_errors_configure_fatal,
    camera_encoder_errors_generic, camera_encoder_errors_vpx_mem_alloc,
    camera_encoder_frames_submitted_ok, screen_encoder_errors_closed_codec,
    screen_encoder_errors_configure_fatal, screen_encoder_errors_generic,
    screen_encoder_errors_vpx_mem_alloc, screen_encoder_frames_submitted_ok,
};
use log::{debug, trace, warn};
use protobuf::Message;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use videocall_diagnostics::{subscribe, DiagEvent, MetricValue};
use videocall_types::protos::health_packet::{
    decode_budget::OverrideMode as PbOverrideMode, DecodeBudget as PbDecodeBudget,
    HealthPacket as PbHealthPacket, NetEqNetwork as PbNetEqNetwork,
    NetEqOperationCounters as PbNetEqOperationCounters, NetEqStats as PbNetEqStats,
    PeerStats as PbPeerStats, TierDwell as PbTierDwell, TierTransition as PbTierTransition,
    VideoStats as PbVideoStats,
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
    /// Camera video stats (media_type=VIDEO).
    pub last_camera_stats: Option<Value>,
    /// Screen share video stats (media_type=SCREEN).
    pub last_screen_stats: Option<Value>,
    /// Sender's self-reported audio state (from peer heartbeat metadata).
    pub audio_enabled: bool,
    /// Sender's self-reported video state (from peer heartbeat metadata).
    pub video_enabled: bool,
    pub last_update_ms: u64,
    /// Timestamp of last audio stats update (ms since epoch). 0 = never received.
    pub last_audio_update_ms: u64,
    /// Timestamp of last camera video stats update (ms since epoch). 0 = never received.
    pub last_camera_update_ms: u64,
    /// Timestamp of last screen share stats update (ms since epoch). 0 = never received.
    pub last_screen_update_ms: u64,
    /// Cumulative decode error count across the session lifetime.
    pub decode_errors_total: u64,
}

impl PeerHealthData {
    pub fn new(peer_id: String) -> Self {
        Self {
            peer_id,
            last_neteq_stats: None,
            last_camera_stats: None,
            last_screen_stats: None,
            audio_enabled: false,
            video_enabled: false,
            last_update_ms: 0,
            last_audio_update_ms: 0,
            last_camera_update_ms: 0,
            last_screen_update_ms: 0,
            decode_errors_total: 0,
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

    pub fn update_camera_stats(&mut self, video_stats: Value) {
        self.last_camera_stats = Some(video_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_camera_update_ms = now_ms;
    }

    pub fn update_screen_stats(&mut self, video_stats: Value) {
        self.last_screen_stats = Some(video_stats);
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.last_update_ms = now_ms;
        self.last_screen_update_ms = now_ms;
    }
}

/// Snapshot of climb-rate limiter state, updated by the encoder each tick.
#[derive(Debug, Clone, Default)]
pub struct ClimbLimiterSnapshot {
    pub crash_ceiling_active: bool,
    pub crash_ceiling_tier_index: Option<u32>,
    pub crash_ceiling_decay_ms: Option<f64>,
    pub step_up_blocked_ceiling: u64,
    pub step_up_blocked_slowdown: u64,
    pub step_up_blocked_screen_share: u64,
}

/// Snapshot of the adaptive decode-budget controller's current decision (#987).
///
/// Captured from the `decode_budget` diagnostics subsystem (published by the
/// Dioxus control loop) and folded into each HEALTH packet so population-scale
/// dashboards can observe the receiver-side tile-cap decision that today only
/// exists in client console logs. Mirrors how the AdaptiveQuality tier atomics
/// ride the health packet.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecodeBudgetSnapshot {
    /// Current effective cap on simultaneously decoded video tiles.
    pub effective_cap: u32,
    /// Natural/unconstrained tile count the layout would show (∩ CANVAS_LIMIT).
    pub natural: u32,
    /// Whether the pressured latch is engaged (the loop owns the cap).
    pub pressured: bool,
    /// Override mode, as the proto `OverrideMode` enum integer value
    /// (1 = Auto, 2 = Fixed; 0 = unset/Auto).
    pub override_mode: u32,
    /// User's hard tile cap; meaningful only when `override_mode` is Fixed.
    pub override_fixed_n: u32,
}

/// Shared buffer of tier transition records from camera and screen encoders.
type TierTransitionBuffers = Rc<RefCell<Vec<Rc<RefCell<Vec<TierTransitionRecord>>>>>>;

/// Shared climb-rate limiter snapshot (double-wrapped for late binding).
type SharedClimbLimiterSnapshot = Rc<RefCell<Rc<RefCell<ClimbLimiterSnapshot>>>>;

/// Shared dwell-time sample buffer (double-wrapped for late binding).
type SharedDwellSamples = Rc<RefCell<Rc<RefCell<Vec<(String, f64)>>>>>;

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
    /// Encoder p75 peer FPS (f32 bits in AtomicU32).
    encoder_p75_peer_fps: Rc<RefCell<Rc<AtomicU32>>>,
    /// Encoder PID target bitrate kbps (f32 bits in AtomicU32).
    encoder_target_bitrate_kbps: Rc<RefCell<Rc<AtomicU32>>>,
    /// Screen share quality tier index.
    adaptive_screen_tier: Rc<RefCell<Rc<AtomicU32>>>,
    /// Screen sharing active flag.
    screen_sharing_active: Rc<RefCell<Rc<AtomicBool>>>,
    /// Encoder output FPS (camera).
    encoder_output_fps: Rc<RefCell<Arc<AtomicU32>>>,
    /// #1143: camera encoder EFFECTIVE simulcast layer count (ladder depth the
    /// publisher is configured to encode/send). Wrapped for late binding like the
    /// other encoder sources; swapped in by `set_encoder_metric_sources`. Reads as
    /// 0 (field omitted) until the encoder atom is wired.
    effective_video_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// #1143: camera encoder ACTIVE simulcast layer count (layers presently
    /// encoded + sent; `<=` effective, the gap being AQ-shed layers).
    active_video_layers: Rc<RefCell<Rc<AtomicU32>>>,
    /// Shared tier transition buffers (camera + screen, drained each health packet).
    tier_transitions: TierTransitionBuffers,
    /// Climb-rate limiter snapshot, updated by the encoder each tick.
    /// Double-wrapped so `set_encoder_metric_sources` (called after
    /// `start_health_reporting`) can swap the inner Rc and the spawned loop
    /// picks up the encoder's buffer on its next tick.
    climb_limiter_snapshot: SharedClimbLimiterSnapshot,
    /// Dwell time samples buffer, drained each health packet.
    /// Double-wrapped for the same late-binding reason as `climb_limiter_snapshot`.
    dwell_samples: SharedDwellSamples,
    /// Shutdown flag set by [`shutdown()`](Self::shutdown). The
    /// `start_health_reporting` future captures a `Weak<AtomicBool>` clone of
    /// this and exits as soon as the flag is observed `true`. Required because
    /// that future also clones the send-packet callback (an `Rc` strong
    /// reference back into the `VideoCallClient`), creating a cycle that
    /// otherwise prevents `Inner` from dropping after a meeting page unmount.
    /// Without this flag the leaked `VideoCallClient` would keep running until
    /// the server eventually tore down its WebTransport session — the bug
    /// reproduced in the cc7tp meeting incident on 2026-05-01.
    shutdown: Rc<AtomicBool>,
    /// TELEM-8: Accumulated long-task durations (ms) since last health packet.
    longtask_buffer: Rc<RefCell<Vec<f64>>>,
    /// TELEM-9: Latest render FPS reading from the rAF cadence observer.
    render_fps: Rc<RefCell<Option<f64>>>,
    /// #987: Latest adaptive decode-budget snapshot from the `decode_budget`
    /// diagnostics subsystem. `None` until the controller publishes its first
    /// decision (no peers / pre-warmup), in which case the field is omitted.
    decode_budget: Rc<RefCell<Option<DecodeBudgetSnapshot>>>,
    /// #1032: Latest total-process memory reading from
    /// `performance.measureUserAgentSpecificMemory()`. That API is async
    /// (returns a Promise) and Chrome-only/`crossOriginIsolated`-gated, so it
    /// is sampled in a background task and the last resolved value is cached
    /// here. The report loop reads this cell synchronously and never awaits.
    /// `None` until the first sample resolves, or permanently when the API is
    /// unavailable, in which case the proto field is omitted.
    agent_memory_bytes: Rc<RefCell<Option<u64>>>,
}

/// Static client metadata read from JS globals (TELEM-7).
#[derive(Debug, Clone, Default)]
pub struct ClientMetadata {
    pub cores: u32,
    pub architecture: String,
    pub gpu_family: String,
    pub network_effective_type: String,
    pub network_downlink: f64,
    pub network_rtt: u32,
    pub battery_charging: Option<bool>,
    pub battery_level: Option<f64>,
    pub capability_score: u32,
}

/// Normalize a raw GPU renderer string to a short family name.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn normalize_gpu_family(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if raw.contains("Apple") {
        return "Apple GPU".to_string();
    }
    if raw.contains("NVIDIA") || raw.contains("GeForce") {
        if let Some(pos) = raw.find("GeForce") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
        if let Some(pos) = raw.find("NVIDIA") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
    }
    if raw.contains("AMD") || raw.contains("Radeon") {
        if let Some(pos) = raw.find("Radeon") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(24).collect();
            return family.trim().to_string();
        }
        return "AMD GPU".to_string();
    }
    if raw.contains("Intel") {
        if let Some(pos) = raw.find("Intel") {
            let sub = &raw[pos..];
            let family: String = sub.chars().take(32).collect();
            return family.trim().to_string();
        }
    }
    raw.chars().take(32).collect::<String>().trim().to_string()
}

/// Read client metadata from `window.__videocall_client_metadata` and
/// `navigator.hardwareConcurrency`.
#[cfg(target_arch = "wasm32")]
fn read_client_metadata() -> ClientMetadata {
    use js_sys::Reflect;
    use wasm_bindgen::JsValue;

    let mut meta = ClientMetadata::default();

    let Some(window) = web_sys::window() else {
        return meta;
    };

    // Cores from navigator
    meta.cores = {
        let cores_f64 = window.navigator().hardware_concurrency();
        if cores_f64.is_finite() && cores_f64 >= 1.0 {
            cores_f64.min(u32::MAX as f64) as u32
        } else {
            0
        }
    };

    // Capability score from window.__videocall_capability_score
    if let Ok(score_val) = Reflect::get(&window, &JsValue::from_str("__videocall_capability_score"))
    {
        if let Some(score) = score_val.as_f64() {
            if score.is_finite() && score > 0.0 {
                meta.capability_score = score.min(u32::MAX as f64) as u32;
            }
        }
    }

    // Read __videocall_client_metadata object
    let Ok(obj) = Reflect::get(&window, &JsValue::from_str("__videocall_client_metadata")) else {
        return meta;
    };
    if obj.is_undefined() || obj.is_null() {
        return meta;
    }

    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("architecture")) {
        if let Some(s) = v.as_string() {
            meta.architecture = s;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("gpu")) {
        if let Some(s) = v.as_string() {
            meta.gpu_family = normalize_gpu_family(&s);
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_effective_type")) {
        if let Some(s) = v.as_string() {
            meta.network_effective_type = s;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_downlink")) {
        if let Some(f) = v.as_f64() {
            meta.network_downlink = f;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("network_rtt")) {
        if let Some(f) = v.as_f64() {
            meta.network_rtt = f as u32;
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("battery_charging")) {
        if let Some(b) = v.as_bool() {
            meta.battery_charging = Some(b);
        }
    }
    if let Ok(v) = Reflect::get(&obj, &JsValue::from_str("battery_level")) {
        if let Some(f) = v.as_f64() {
            meta.battery_level = Some(f);
        }
    }

    meta
}

#[cfg(not(target_arch = "wasm32"))]
fn read_client_metadata() -> ClientMetadata {
    ClientMetadata::default()
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
            encoder_p75_peer_fps: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            encoder_target_bitrate_kbps: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            adaptive_screen_tier: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            screen_sharing_active: Rc::new(RefCell::new(Rc::new(AtomicBool::new(false)))),
            encoder_output_fps: Rc::new(RefCell::new(Arc::new(AtomicU32::new(0)))),
            // #1143: 0 until the encoder atoms are wired by
            // `set_encoder_metric_sources`; a 0 effective count omits the field.
            effective_video_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            active_video_layers: Rc::new(RefCell::new(Rc::new(AtomicU32::new(0)))),
            tier_transitions: Rc::new(RefCell::new(Vec::new())),
            climb_limiter_snapshot: Rc::new(RefCell::new(Rc::new(RefCell::new(
                ClimbLimiterSnapshot::default(),
            )))),
            dwell_samples: Rc::new(RefCell::new(Rc::new(RefCell::new(Vec::new())))),
            shutdown: Rc::new(AtomicBool::new(false)),
            longtask_buffer: Rc::new(RefCell::new(Vec::new())),
            render_fps: Rc::new(RefCell::new(None)),
            decode_budget: Rc::new(RefCell::new(None)),
            agent_memory_bytes: Rc::new(RefCell::new(None)),
        }
    }

    /// Signal the health-reporting future to exit on its next tick. Sets the
    /// shutdown flag and clears the send-packet callback so that future ticks
    /// after this call cannot publish further packets even if a tick races
    /// the flag. Called from [`VideoCallClient::disconnect()`](
    /// crate::VideoCallClient::disconnect).
    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.send_packet_callback = None;
        // Drop the strong reference to the connection controller so we don't
        // keep it alive past the explicit disconnect.
        if let Ok(mut cc) = self.connection_controller.try_borrow_mut() {
            *cc = None;
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

    /// Returns a clone of the video tier index atomic for external reads.
    ///
    /// Used by `VideoCallClient::camera_tier_index()` to expose the current
    /// camera quality tier for adaptive screen-share tier selection.
    pub fn video_tier_index(&self) -> Option<Rc<AtomicU32>> {
        if let Ok(tier) = self.adaptive_video_tier.try_borrow() {
            Some(tier.clone())
        } else {
            None
        }
    }

    /// Bind the encoder metric atomics from CameraEncoder and ScreenEncoder so the
    /// health reporter can include encoder decision inputs in each health packet.
    #[allow(clippy::too_many_arguments)]
    pub fn set_encoder_metric_sources(
        &mut self,
        p75_peer_fps: Rc<AtomicU32>,
        target_bitrate_kbps: Rc<AtomicU32>,
        screen_tier: Rc<AtomicU32>,
        screen_active: Rc<AtomicBool>,
        output_fps: Arc<AtomicU32>,
        camera_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        screen_transitions: Rc<RefCell<Vec<TierTransitionRecord>>>,
        climb_limiter_snapshot: Rc<RefCell<ClimbLimiterSnapshot>>,
        dwell_samples: Rc<RefCell<Vec<(String, f64)>>>,
        effective_video_layers: Rc<AtomicU32>,
        active_video_layers: Rc<AtomicU32>,
    ) {
        *self.encoder_p75_peer_fps.borrow_mut() = p75_peer_fps;
        *self.encoder_target_bitrate_kbps.borrow_mut() = target_bitrate_kbps;
        *self.adaptive_screen_tier.borrow_mut() = screen_tier;
        *self.screen_sharing_active.borrow_mut() = screen_active;
        *self.encoder_output_fps.borrow_mut() = output_fps;
        *self.tier_transitions.borrow_mut() = vec![camera_transitions, screen_transitions];
        *self.climb_limiter_snapshot.borrow_mut() = climb_limiter_snapshot;
        *self.dwell_samples.borrow_mut() = dwell_samples;
        *self.effective_video_layers.borrow_mut() = effective_video_layers;
        *self.active_video_layers.borrow_mut() = active_video_layers;
    }

    /// Start subscribing to real diagnostics events via videocall_diagnostics
    pub fn start_diagnostics_subscription(&self) {
        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);
        let longtask_buffer = Rc::downgrade(&self.longtask_buffer);
        let render_fps_state = Rc::downgrade(&self.render_fps);
        let decode_budget_state = Rc::downgrade(&self.decode_budget);

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
                    // TELEM-8/9: capture client_perf subsystem events
                    if event.subsystem == "client_perf" {
                        for m in &event.metrics {
                            match m.name {
                                "client_longtask_duration_ms" => {
                                    if let MetricValue::F64(duration) = &m.value {
                                        if let Some(buf) = Weak::upgrade(&longtask_buffer) {
                                            if let Ok(mut v) = buf.try_borrow_mut() {
                                                v.push(*duration);
                                            }
                                        }
                                    }
                                }
                                "client_render_fps" => {
                                    if let MetricValue::F64(fps) = &m.value {
                                        if let Some(fps_rc) = Weak::upgrade(&render_fps_state) {
                                            if let Ok(mut f) = fps_rc.try_borrow_mut() {
                                                *f = Some(*fps);
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    // #987: capture the adaptive decode-budget snapshot. The
                    // Dioxus control loop publishes one event per decision change
                    // (state-change driven, not per render), so we simply overwrite
                    // the latest snapshot here and read it at packet-assembly time.
                    if event.subsystem == "decode_budget" {
                        if let Some(db_rc) = Weak::upgrade(&decode_budget_state) {
                            let mut snap = DecodeBudgetSnapshot::default();
                            for m in &event.metrics {
                                if let MetricValue::U64(v) = &m.value {
                                    match m.name {
                                        "decode_budget_effective_cap" => {
                                            snap.effective_cap = *v as u32
                                        }
                                        "decode_budget_natural" => snap.natural = *v as u32,
                                        "decode_budget_pressured" => snap.pressured = *v != 0,
                                        "decode_budget_override_mode" => {
                                            snap.override_mode = *v as u32
                                        }
                                        "decode_budget_override_fixed_n" => {
                                            snap.override_fixed_n = *v as u32
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if let Ok(mut cell) = db_rc.try_borrow_mut() {
                                *cell = Some(snap);
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
                                    // Per-NetEQ-event (continuous audio-stats stream).
                                    // Demoted debug!->trace!; not on the analyzer keep-list
                                    // (the analyzer greps "audio health (buffer: Nms)" below,
                                    // NOT this line).
                                    trace!(
                                     "Updated NetEQ stats for peer: {target_peer} (from {reporting_peer})"
                                    );
                                }
                            }
                        }
                        "audio_buffer_ms" => {
                            if let MetricValue::U64(buffer_ms) = &metric.value {
                                // NOTE: kept as a PERIODIC sample (logged every ~1 Hz
                                // NetEQ tick per peer), NOT edge-triggered. The meeting
                                // analyzer (`scripts/parse_meeting_console_logs.sh`)
                                // computes n_samples / n_nonzero / median / median_nonzero
                                // from this line as a uniform sample stream — change-point
                                // logging would bias all four (a stable 150ms buffer would
                                // report n=1, median=150 instead of the true distribution).
                                // The large per-tick offenders demoted in this PR are
                                // elsewhere (MEDIA receive, heartbeat, ConnectionManager,
                                // Rendering-meeting-view, Host-render); this analyzer-
                                // critical sample is left intact at debug!.
                                debug!(
                                    "Updated audio health (buffer: {buffer_ms}ms) for peer: {target_peer} (from {reporting_peer})"
                                );
                            }
                        }
                        "packets_awaiting_decode" => {
                            if let MetricValue::U64(packets) = &metric.value {
                                // Per-NetEQ-event. Demoted debug!->trace!; not on the
                                // analyzer keep-list.
                                trace!(
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
            // Per-sender-event (fires for every received diagnostics packet).
            // Demoted debug!->trace!; not on the analyzer keep-list.
            trace!(
                "Received sender event for peer: {} at {}",
                target_peer,
                event.ts_ms
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

                // Determine if this is camera or screen based on media_type metric.
                let is_screen = event.metrics.iter().any(|m| {
                    m.name == "media_type"
                        && matches!(&m.value, MetricValue::Text(s) if s == "SCREEN")
                });

                // Pick the right stats bucket (camera or screen).
                let existing = if is_screen {
                    &peer_data.last_screen_stats
                } else {
                    &peer_data.last_camera_stats
                };
                let mut video_stats = match existing {
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
                        "decode_errors_total" => {
                            if let MetricValue::U64(total) = &metric.value {
                                peer_data.decode_errors_total = *total;
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
                        // Freeze observability (#1013): windowed per-stream
                        // packet-loss rate and keyframe-request rate, emitted by
                        // the decoder. Stored in the camera/screen video_stats
                        // bucket (split by is_screen) so they fold into the
                        // per-peer health packet and the video quality score.
                        "video_seq_loss_per_sec" => {
                            if let MetricValue::F64(loss) = &metric.value {
                                video_stats["video_seq_loss_per_sec"] = json!(loss);
                            }
                        }
                        "keyframe_requests_per_sec" => {
                            if let MetricValue::F64(kf) = &metric.value {
                                video_stats["keyframe_requests_per_sec"] = json!(kf);
                            }
                        }
                        // Buffered video playout latency (#1252): total across both receive stages
                        // and its stage-1 attribution. Stored in the camera/screen video_stats
                        // bucket; folded into the health packet only when fps_received > 0.
                        "playout_latency_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_latency_ms"] = json!(v);
                            }
                        }
                        "playout_stage1_span_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_stage1_span_ms"] = json!(v);
                            }
                        }
                        // Stage-3 paint lag (#1252): decoded-but-unpainted backlog in the
                        // worker->main postMessage + paint queues. Same bucket/guard as the two
                        // latency fields above.
                        "playout_paint_lag_ms" => {
                            if let MetricValue::F64(v) = &metric.value {
                                video_stats["playout_paint_lag_ms"] = json!(v);
                            }
                        }
                        _ => {}
                    }
                }

                if is_screen {
                    peer_data.update_screen_stats(video_stats);
                    // Per-video-event (continuous per-stream stats). Demoted
                    // debug!->trace!; not on the analyzer keep-list.
                    trace!("Updated screen health for peer: {target_peer}");
                } else {
                    peer_data.update_camera_stats(video_stats);
                    // Per-video-event. Demoted debug!->trace!; not on the analyzer
                    // keep-list.
                    trace!("Updated camera health for peer: {target_peer}");
                }
            }
        }
    }

    /// #1032: Start the background total-process memory sampler.
    ///
    /// `performance.measureUserAgentSpecificMemory()` returns total agent
    /// memory including GPU-backed and worker allocations — exactly the
    /// non-heap memory that `performance.memory` (JS heap) misses. It is:
    ///   - **async** (returns a Promise), so we must `await` it OFF the
    ///     health-report hot path and cache the resolved value, and
    ///   - **Chrome-only and gated on `crossOriginIsolated`**, so it may be
    ///     entirely absent. We feature-detect once and, if missing, never spawn
    ///     the loop (the cached value stays `None`, the proto field is omitted,
    ///     and `agent_memory_bytes` simply never appears for that client).
    ///
    /// Graceful degradation: any missing global, non-isolated context, thrown
    /// exception, or malformed result clears the cache — we never panic and
    /// never block the report cadence.
    #[cfg(target_arch = "wasm32")]
    fn start_agent_memory_sampler(&self) {
        use wasm_bindgen::JsCast;

        // Feature-detect: window + crossOriginIsolated + the API function.
        // `measureUserAgentSpecificMemory` only exists in cross-origin-isolated
        // contexts on Chromium; bail out cleanly everywhere else.
        let Some(window) = web_sys::window() else {
            return;
        };
        let cross_origin_isolated = js_sys::Reflect::get(&window, &"crossOriginIsolated".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !cross_origin_isolated {
            debug!("agent-memory sampler: not crossOriginIsolated, skipping");
            return;
        }
        let Some(perf) = window.performance() else {
            return;
        };
        let measure_fn = match js_sys::Reflect::get(&perf, &"measureUserAgentSpecificMemory".into())
        {
            Ok(f) if f.is_function() => f.unchecked_into::<js_sys::Function>(),
            _ => {
                debug!(
                    "agent-memory sampler: measureUserAgentSpecificMemory unavailable, skipping"
                );
                return;
            }
        };

        // Sample on a slow cadence — this is a coarse pressure signal, not a
        // per-frame metric, and the API itself can take tens of ms to resolve.
        const AGENT_MEMORY_SAMPLE_INTERVAL_MS: u32 = 30_000;

        let cache = Rc::downgrade(&self.agent_memory_bytes);
        let shutdown = Rc::downgrade(&self.shutdown);

        spawn_local(async move {
            use wasm_bindgen_futures::JsFuture;

            loop {
                // Honour shutdown the same way the report loop does.
                match Weak::upgrade(&shutdown) {
                    Some(flag) if flag.load(Ordering::Acquire) => break,
                    None => break,
                    _ => {}
                }

                // Invoke the API. It returns a Promise resolving to an object
                // whose `bytes` field is the total agent memory in bytes.
                match measure_fn.call0(&perf) {
                    Ok(promise_val) => {
                        let promise: js_sys::Promise = promise_val.into();
                        match JsFuture::from(promise).await {
                            Ok(result) => {
                                let sample = js_sys::Reflect::get(&result, &"bytes".into())
                                    .ok()
                                    .and_then(|bytes| bytes.as_f64())
                                    .map(|bytes_f64| bytes_f64 as u64);
                                if let Some(cell) = Weak::upgrade(&cache) {
                                    if let Ok(mut c) = cell.try_borrow_mut() {
                                        *c = sample;
                                    }
                                } else {
                                    // HealthReporter dropped; stop sampling.
                                    break;
                                }
                            }
                            Err(e) => {
                                // Rejected (e.g. permissions/throttling). Clear the
                                // cached value so stale data cannot linger forever.
                                if let Some(cell) = Weak::upgrade(&cache) {
                                    if let Ok(mut c) = cell.try_borrow_mut() {
                                        *c = None;
                                    }
                                }
                                debug!("agent-memory sampler: measure rejected: {e:?}");
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(cell) = Weak::upgrade(&cache) {
                            if let Ok(mut c) = cell.try_borrow_mut() {
                                *c = None;
                            }
                        }
                        debug!("agent-memory sampler: call threw: {e:?}");
                    }
                }

                gloo_timers::future::TimeoutFuture::new(AGENT_MEMORY_SAMPLE_INTERVAL_MS).await;
            }
            debug!("agent-memory sampler stopped");
        });
    }

    /// Non-wasm builds have no browser memory API; the sampler is a no-op and
    /// `agent_memory_bytes` stays `None`.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_agent_memory_sampler(&self) {}

    /// Start periodic health reporting
    pub fn start_health_reporting(&self) {
        if self.send_packet_callback.is_none() {
            warn!("Cannot start health reporting: no send packet callback set");
            return;
        }

        // #1032: kick off the background total-process memory sampler. It runs
        // on its own cadence and caches the last resolved value so the report
        // loop below can read it synchronously (never awaiting in the hot path).
        self.start_agent_memory_sampler();

        let peer_health_data = Rc::downgrade(&self.peer_health_data);
        let session_id = Rc::downgrade(&self.session_id);
        let meeting_id = self.meeting_id.clone();
        let reporting_peer = self.reporting_peer.clone();
        let display_name = self.display_name.clone();
        let send_callback = self.send_packet_callback.clone().unwrap();
        let interval_ms = self.health_interval_ms;
        // Weak ref to the shutdown flag. We never need the strong reference
        // here — `Rc::downgrade` keeps the future from holding the
        // `Rc<AtomicBool>` past the HealthReporter's own lifetime, but the
        // flag itself can also be observed `true` directly via `shutdown()`
        // for prompt teardown without waiting for a tick.
        let shutdown = Rc::downgrade(&self.shutdown);
        let audio_enabled = Rc::downgrade(&self.reporting_audio_enabled);
        let video_enabled = Rc::downgrade(&self.reporting_video_enabled);
        let active_server_url = Rc::downgrade(&self.active_server_url);
        let active_server_type = Rc::downgrade(&self.active_server_type);
        let active_server_rtt_ms = Rc::downgrade(&self.active_server_rtt_ms);
        let connection_controller = Rc::downgrade(&self.connection_controller);
        let adaptive_video_tier = self.adaptive_video_tier.clone();
        let adaptive_audio_tier = self.adaptive_audio_tier.clone();
        let encoder_p75_peer_fps = self.encoder_p75_peer_fps.clone();
        let encoder_target_bitrate_kbps = self.encoder_target_bitrate_kbps.clone();
        let adaptive_screen_tier = self.adaptive_screen_tier.clone();
        let screen_sharing_active = self.screen_sharing_active.clone();
        let encoder_output_fps = self.encoder_output_fps.clone();
        // #1143: send-side simulcast layer counts (camera encoder).
        let effective_video_layers = self.effective_video_layers.clone();
        let active_video_layers = self.active_video_layers.clone();
        let tier_transitions = self.tier_transitions.clone();
        let climb_limiter_snapshot = self.climb_limiter_snapshot.clone();
        let dwell_samples = self.dwell_samples.clone();
        let longtask_buffer = self.longtask_buffer.clone();
        let render_fps_cell = self.render_fps.clone();
        let decode_budget_cell = self.decode_budget.clone();
        // #1032: cached total-process memory reading sampled in the background.
        let agent_memory_cell = self.agent_memory_bytes.clone();

        spawn_local(async move {
            debug!("Started health reporting with interval: {interval_ms}ms");

            loop {
                // Wait for the interval
                gloo_timers::future::TimeoutFuture::new(interval_ms as u32).await;

                // Honour an explicit shutdown signal (e.g. UI unmount) without
                // waiting for the HealthReporter's `Rc` count to fall to zero.
                // `send_callback` is an `Rc` strong reference back into
                // `VideoCallClient`, so without this exit the reporter loop
                // would keep the entire client alive until the server tore the
                // session down on its own — the leak observed in cc7tp.
                if let Some(flag) = Weak::upgrade(&shutdown) {
                    if flag.load(Ordering::Acquire) {
                        debug!("HealthReporter shutdown signalled, stopping health reporting");
                        break;
                    }
                } else {
                    // The HealthReporter (and its shutdown flag) have been
                    // dropped already — nothing to report against.
                    break;
                }

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

                        // Read encoder decision inputs from shared atomics (f32 bits → f64).
                        let p75_peer_fps_val =
                            f32::from_bits(encoder_p75_peer_fps.borrow().load(Ordering::Relaxed))
                                as f64;
                        let target_bitrate_kbps_val = f32::from_bits(
                            encoder_target_bitrate_kbps.borrow().load(Ordering::Relaxed),
                        ) as f64;
                        let screen_tier_val = adaptive_screen_tier.borrow().load(Ordering::Relaxed);
                        let screen_active_val =
                            screen_sharing_active.borrow().load(Ordering::Relaxed);
                        let output_fps_val = encoder_output_fps.borrow().load(Ordering::Relaxed);
                        // #1143: live send-side simulcast layer counts.
                        let effective_layers_val =
                            effective_video_layers.borrow().load(Ordering::Relaxed);
                        let active_layers_val =
                            active_video_layers.borrow().load(Ordering::Relaxed);

                        // Drain tier transitions from all encoder buffers.
                        let mut drained_transitions = Vec::new();
                        if let Ok(buffers) = tier_transitions.try_borrow() {
                            for buf in buffers.iter() {
                                if let Ok(mut t) = buf.try_borrow_mut() {
                                    drained_transitions.append(&mut *t);
                                }
                            }
                        }

                        // Snapshot climb-rate limiter state (double-wrap: outer then inner).
                        let limiter_snap = climb_limiter_snapshot
                            .try_borrow()
                            .ok()
                            .and_then(|outer| outer.try_borrow().ok().map(|s| s.clone()))
                            .unwrap_or_default();

                        // Drain dwell samples (double-wrap: outer then inner).
                        let drained_dwells: Vec<(String, f64)> = dwell_samples
                            .try_borrow()
                            .ok()
                            .and_then(|outer| {
                                outer
                                    .try_borrow_mut()
                                    .ok()
                                    .map(|mut d| std::mem::take(&mut *d))
                            })
                            .unwrap_or_default();

                        // TELEM-8: drain accumulated long-task durations
                        let drained_longtasks: Vec<f64> = longtask_buffer
                            .try_borrow_mut()
                            .ok()
                            .map(|mut v| std::mem::take(&mut *v))
                            .unwrap_or_default();

                        // TELEM-9: read latest render FPS
                        let current_render_fps = render_fps_cell.try_borrow().ok().and_then(|v| *v);

                        // #987: read latest decode-budget snapshot (None until the
                        // controller has published its first decision).
                        let decode_budget_snapshot =
                            decode_budget_cell.try_borrow().ok().and_then(|v| *v);

                        // #1032: read cached total-process memory (None until the
                        // background sampler resolves, or permanently when the API
                        // is unavailable). Synchronous read — never awaits here.
                        let agent_memory_bytes =
                            agent_memory_cell.try_borrow().ok().and_then(|v| *v);

                        // TELEM-7: read client metadata from JS globals
                        let client_meta = read_client_metadata();

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
                            videocall_transport::websocket::websocket_drop_count(),
                            keyframe_requests_sent_count(),
                            p75_peer_fps_val,
                            target_bitrate_kbps_val,
                            screen_tier_val,
                            screen_active_val,
                            output_fps_val,
                            effective_layers_val,
                            active_layers_val,
                            drained_transitions,
                            limiter_snap,
                            drained_dwells,
                            connection_handshake_failures(),
                            connection_session_drops(),
                            [
                                reelection_proceeded_total(),
                                reelection_aborted_total(),
                                reelection_preserved_total(),
                                reelection_failed_total(),
                            ],
                            drained_longtasks,
                            current_render_fps,
                            client_meta,
                            decode_budget_snapshot,
                            agent_memory_bytes,
                        );

                        if let Some(packet) = health_packet {
                            send_callback.emit(packet);
                            // PER-TICK hot path: fires on every health-report
                            // interval (~1 Hz per session). Demoted debug!->trace!
                            // so it stays off even when console-log collection
                            // bumps to Debug (#1100 follow-up). Not on the analyzer
                            // keep-list.
                            trace!("Sent health packet for session: {session_id_val}");
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
        websocket_drops_total: u64,
        keyframe_requests_sent_total: u64,
        encoder_p75_peer_fps: f64,
        encoder_target_bitrate_kbps: f64,
        adaptive_screen_tier: u32,
        screen_sharing_active: bool,
        encoder_output_fps: u32,
        // #1143: send-side simulcast layer counts (camera). 0 = unwired/omitted.
        effective_video_layers: u32,
        active_video_layers: u32,
        tier_transitions: Vec<TierTransitionRecord>,
        climb_limiter: ClimbLimiterSnapshot,
        dwell_samples: Vec<(String, f64)>,
        handshake_failures_total: u64,
        session_drops_total: u64,
        // Cumulative re-election outcome totals (Tier B #3), in the fixed order
        // [proceeded, aborted, preserved, failed]. Cumulative since process
        // start — the relay maps these onto a GaugeVec it .set()s, so the
        // monotonic client value charts correctly with increase()/rate().
        reelection_totals: [u64; 4],
        longtask_durations: Vec<f64>,
        render_fps: Option<f64>,
        client_metadata: ClientMetadata,
        decode_budget: Option<DecodeBudgetSnapshot>,
        agent_memory_bytes: Option<u64>,
    ) -> Option<PacketWrapper> {
        // Keep client-wide telemetry flowing even before any peer stats have
        // been observed (solo sessions / warm-up).

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

        // Include active connection info if available.
        //
        // SECURITY: do NOT copy `active_server_url` into the protobuf. The lobby
        // URL carries the user's room JWT (`?token=<JWT>&instance_id=<UUID>`),
        // and HealthPacket is republished by the relay onto the NATS telemetry
        // topic `health.diagnostics.{region}.{service_type}.{server_id}` — any
        // health-pipeline consumer would receive the credential in cleartext.
        // The `active_server_type` and `active_server_rtt_ms` fields below are
        // sufficient for downstream observability; transport identity is
        // additionally available via `active_connection_id` on the diagnostic
        // bus (UI side). The proto field is left at its default empty string
        // and is slated for deprecation in a follow-up PR.
        // The `active_server_url` argument is intentionally swallowed here.
        let _ = active_server_url;
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
        pb.websocket_drops_total = Some(websocket_drops_total);
        pb.keyframe_requests_sent_total = Some(keyframe_requests_sent_total);

        // Encoder decision inputs (P0)
        if encoder_p75_peer_fps.is_finite() {
            pb.encoder_p75_peer_fps = Some(encoder_p75_peer_fps);
        }
        pb.adaptive_screen_tier = Some(adaptive_screen_tier);
        pb.screen_sharing_active = Some(screen_sharing_active);

        // Encoder outputs (P1)
        // encoder_output_fps uses > 0 (not is_finite) because 0 means the encoder
        // hasn't started yet, which isn't diagnostic. The other encoder metrics
        // allow 0.0 through because a zero ratio/bitrate IS the diagnostic signal.
        if encoder_output_fps > 0 {
            pb.encoder_output_fps = Some(encoder_output_fps);
        }

        // #1143: send-side simulcast layer counts (camera). Gated on > 0 (same
        // convention as encoder_output_fps): 0 means the encoder atoms have not
        // been wired yet, which is not diagnostic — omit rather than emit a
        // misleading 0. A wired single-stream publisher reports 1 (the
        // inert-simulcast signal the dashboard alerts on). active is clamped to
        // effective defensively so the gap can never read negative.
        if effective_video_layers > 0 {
            pb.effective_video_layers = Some(effective_video_layers);
            pb.active_video_layers = Some(active_video_layers.min(effective_video_layers));
        }

        if encoder_target_bitrate_kbps.is_finite() {
            pb.encoder_target_bitrate_kbps = Some(encoder_target_bitrate_kbps);
        }

        // Tier transition events (P2)
        for t in &tier_transitions {
            let mut pb_t = PbTierTransition::new();
            pb_t.direction = t.direction.to_string();
            pb_t.stream = t.stream.to_string();
            pb_t.from_tier = t.from_tier.clone();
            pb_t.to_tier = t.to_tier.clone();
            pb_t.trigger = t.trigger.to_string();
            pb.tier_transitions.push(pb_t);
        }

        // Climb-rate limiter telemetry (PR-H)
        pb.crash_ceiling_active = Some(climb_limiter.crash_ceiling_active);
        if climb_limiter.crash_ceiling_active {
            pb.crash_ceiling_tier_index = climb_limiter.crash_ceiling_tier_index;
            pb.crash_ceiling_decay_ms = climb_limiter.crash_ceiling_decay_ms;
        }
        // Only emit blocked counters when non-zero to reduce packet size.
        if climb_limiter.step_up_blocked_ceiling > 0 {
            pb.step_up_blocked_ceiling = Some(climb_limiter.step_up_blocked_ceiling);
        }
        if climb_limiter.step_up_blocked_slowdown > 0 {
            pb.step_up_blocked_slowdown = Some(climb_limiter.step_up_blocked_slowdown);
        }
        if climb_limiter.step_up_blocked_screen_share > 0 {
            pb.step_up_blocked_screen_share = Some(climb_limiter.step_up_blocked_screen_share);
        }
        for (tier_label, dwell_ms) in &dwell_samples {
            let mut pb_d = PbTierDwell::new();
            pb_d.tier = tier_label.clone();
            pb_d.dwell_ms = *dwell_ms;
            pb.tier_dwells.push(pb_d);
        }

        // Encoder error counters (cumulative, global statics — zero-cost to read).
        // Only emit when non-zero to keep packet size small in the common (healthy) case.
        let cam_closed = camera_encoder_errors_closed_codec();
        let cam_vpx = camera_encoder_errors_vpx_mem_alloc();
        let cam_configure = camera_encoder_errors_configure_fatal();
        let cam_generic = camera_encoder_errors_generic();
        let cam_frames = camera_encoder_frames_submitted_ok();
        let scr_closed = screen_encoder_errors_closed_codec();
        let scr_vpx = screen_encoder_errors_vpx_mem_alloc();
        let scr_configure = screen_encoder_errors_configure_fatal();
        let scr_generic = screen_encoder_errors_generic();
        let scr_frames = screen_encoder_frames_submitted_ok();

        if cam_closed > 0 {
            pb.camera_encoder_errors_closed_codec = Some(cam_closed);
        }
        if cam_vpx > 0 {
            pb.camera_encoder_errors_vpx_mem_alloc = Some(cam_vpx);
        }
        if cam_configure > 0 {
            pb.camera_encoder_errors_configure_fatal = Some(cam_configure);
        }
        if cam_generic > 0 {
            pb.camera_encoder_errors_generic = Some(cam_generic);
        }
        if cam_frames > 0 {
            pb.camera_encoder_frames_submitted_ok = Some(cam_frames);
        }
        if scr_closed > 0 {
            pb.screen_encoder_errors_closed_codec = Some(scr_closed);
        }
        if scr_vpx > 0 {
            pb.screen_encoder_errors_vpx_mem_alloc = Some(scr_vpx);
        }
        if scr_configure > 0 {
            pb.screen_encoder_errors_configure_fatal = Some(scr_configure);
        }
        if scr_generic > 0 {
            pb.screen_encoder_errors_generic = Some(scr_generic);
        }
        if scr_frames > 0 {
            pb.screen_encoder_frames_submitted_ok = Some(scr_frames);
        }

        // Connection-loss reason counters
        if handshake_failures_total > 0 {
            pb.connection_handshake_failures_total = Some(handshake_failures_total);
        }
        if session_drops_total > 0 {
            pb.connection_session_drops_total = Some(session_drops_total);
        }

        // Re-election outcome counters (Tier B #3). Only attach a field when its
        // cumulative value is non-zero — keeps the packet small for the common
        // case (most sessions never re-elect) and mirrors the connection-loss
        // counters directly above. Order: [proceeded, aborted, preserved, failed].
        if reelection_totals[0] > 0 {
            pb.reelection_proceeded_total = Some(reelection_totals[0]);
        }
        if reelection_totals[1] > 0 {
            pb.reelection_aborted_total = Some(reelection_totals[1]);
        }
        if reelection_totals[2] > 0 {
            pb.reelection_preserved_total = Some(reelection_totals[2]);
        }
        if reelection_totals[3] > 0 {
            pb.reelection_failed_total = Some(reelection_totals[3]);
        }

        // TELEM-7: Static client metadata
        if client_metadata.cores > 0 {
            pb.client_cores = Some(client_metadata.cores);
        }
        if !client_metadata.architecture.is_empty() {
            pb.client_architecture = Some(client_metadata.architecture.clone());
        }
        if !client_metadata.gpu_family.is_empty() {
            pb.client_gpu_family = Some(client_metadata.gpu_family.clone());
        }
        if !client_metadata.network_effective_type.is_empty() {
            pb.client_network_effective_type = Some(client_metadata.network_effective_type.clone());
        }
        if client_metadata.network_downlink > 0.0 {
            pb.client_network_downlink = Some(client_metadata.network_downlink);
        }
        if client_metadata.network_rtt > 0 {
            pb.client_network_rtt = Some(client_metadata.network_rtt);
        }
        pb.client_battery_charging = client_metadata.battery_charging;
        pb.client_battery_level = client_metadata.battery_level;
        if client_metadata.capability_score > 0 {
            pb.client_capability_score = Some(client_metadata.capability_score);
        }

        // TELEM-8: Long task durations since last packet
        pb.longtask_durations_ms = longtask_durations;

        // TELEM-9: Render FPS
        pb.render_fps = render_fps;

        // #987: Adaptive decode-budget controller snapshot. Only present once the
        // controller has published a decision (so a no-peer / pre-warmup packet
        // omits it). Mirrors how the AdaptiveQuality tier fields ride the packet.
        if let Some(db) = decode_budget {
            let mut pb_db = PbDecodeBudget::new();
            pb_db.effective_cap = db.effective_cap;
            pb_db.natural = db.natural;
            pb_db.pressured = db.pressured;
            // Map the integer override mode (1 = Auto, 2 = Fixed; 0/other = Auto)
            // to the proto enum. `override_fixed_n` is only meaningful for Fixed.
            pb_db.override_mode = ::protobuf::EnumOrUnknown::new(match db.override_mode {
                2 => PbOverrideMode::OVERRIDE_MODE_FIXED,
                _ => PbOverrideMode::OVERRIDE_MODE_AUTO,
            });
            if db.override_mode == 2 {
                pb_db.override_fixed_n = db.override_fixed_n;
            }
            // #1143: tiles ACTUALLY being decoded right now. `effective_cap` is
            // the budget ceiling and `natural` is the unconstrained layout count;
            // the realized decode set is the smaller of the two (a 10-tile cap
            // with only 3 peers decodes 3, not 10). This is the per-client
            // "videos showing" signal the observability issue asks for.
            pb_db.active_set = db.effective_cap.min(db.natural);
            pb.decode_budget = ::protobuf::MessageField::some(pb_db);
        }

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

            // #1032: WASM linear-memory size — WebAssembly.Memory.buffer.byteLength.
            // This is the WASM heap, distinct from the JS heap read above. Always
            // available, synchronous, O(1); the highest-value cheapest non-heap
            // signal. `wasm_bindgen::memory()` returns the `WebAssembly.Memory`
            // JsValue whose `.buffer.byteLength` is the current linear-memory size.
            let mem = wasm_bindgen::memory();
            if let Ok(buffer) = js_sys::Reflect::get(&mem, &"buffer".into()) {
                if let Ok(byte_len) = js_sys::Reflect::get(&buffer, &"byteLength".into()) {
                    if let Some(len_f64) = byte_len.as_f64() {
                        pb.wasm_memory_bytes = Some(len_f64 as u64);
                    }
                }
            }
        }

        // #1032: total-process memory from the background sampler (cached value;
        // see `start_agent_memory_sampler`). Absent when the API is unavailable
        // or has not yet resolved its first reading. Platform-agnostic so the
        // value flows through on the wire identically on every target.
        if let Some(agent_mem) = agent_memory_bytes {
            pb.agent_memory_bytes = Some(agent_mem);
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
            let camera_fresh = health_data.last_camera_update_ms > 0
                && now_ms.saturating_sub(health_data.last_camera_update_ms) < STATS_STALE_MS;
            let video_fresh = camera_fresh
                || (health_data.last_screen_update_ms > 0
                    && now_ms.saturating_sub(health_data.last_screen_update_ms) < STATS_STALE_MS);

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
                    ps.audio_concealment_pct =
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

            // Camera video mapping (backward compat: goes into existing video_stats field)
            if let Some(video) = &health_data.last_camera_stats {
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

                // Buffered video playout latency (#1252). Guard #1 (load-bearing): only fold the
                // span when fps_received > 0. A DecodeBudget-paused or hidden tile keeps a stale
                // frame buffered but decodes nothing, so its arrival-time span would read as
                // latency even though the user isn't waiting on it. fps_received > 0 means frames
                // are actually being received/decoded, so the lag is real. When fps == 0 the proto
                // field stays at its 0.0 default, which the server publishes as "at live".
                if vs.fps_received > 0.0 {
                    if let Some(v) = video.get("playout_latency_ms").and_then(|v| v.as_f64()) {
                        vs.playout_latency_ms = v;
                    }
                    if let Some(v) = video.get("playout_stage1_span_ms").and_then(|v| v.as_f64()) {
                        vs.playout_stage1_span_ms = v;
                    }
                    // Stage-3 paint lag (#1252): same fps_received > 0 guard — a paused/hidden tile
                    // decodes nothing and paints nothing, so any residual emitted-vs-painted skew
                    // is not user-perceived latency. When fps == 0 the field stays at its 0.0
                    // default => "at live".
                    if let Some(v) = video.get("playout_paint_lag_ms").and_then(|v| v.as_f64()) {
                        vs.playout_paint_lag_ms = v;
                    }
                }
                ps.video_stats = ::protobuf::MessageField::some(vs);

                // Extract decode_errors_per_sec (windowed rate) from camera video stats
                if let Some(error_rate) =
                    video.get("decode_errors_per_sec").and_then(|v| v.as_f64())
                {
                    ps.frames_dropped_per_sec = error_rate;
                }

                // Freeze observability (#1013): windowed per-stream loss /
                // keyframe-request rates (camera only this pass). These feed the
                // video quality score so a frozen-but-still-decoding stream
                // (fps reads ~30, video visually stuck) no longer scores 100.
                if let Some(loss) = video.get("video_seq_loss_per_sec").and_then(|v| v.as_f64()) {
                    ps.video_seq_loss_per_sec = Some(loss);
                }
                if let Some(kf) = video
                    .get("keyframe_requests_per_sec")
                    .and_then(|v| v.as_f64())
                {
                    ps.keyframe_requests_per_sec = Some(kf);
                }
            }

            // Screen share video mapping (new field, separate from camera)
            if let Some(screen) = &health_data.last_screen_stats {
                let mut svs = PbVideoStats::new();
                if let Some(v) = screen.get("fps_received").and_then(|v| v.as_f64()) {
                    svs.fps_received = v;
                }
                if let Some(v) = screen.get("frames_buffered").and_then(|v| v.as_f64()) {
                    svs.frames_buffered = v;
                }
                if let Some(v) = screen.get("frames_decoded").and_then(|v| v.as_u64()) {
                    svs.frames_decoded = v;
                }
                if let Some(v) = screen.get("bitrate_kbps").and_then(|v| v.as_u64()) {
                    svs.bitrate_kbps = v;
                }
                ps.screen_video_stats = ::protobuf::MessageField::some(svs);
            }

            // Cumulative decode error count (only set if > 0 to avoid noise)
            if health_data.decode_errors_total > 0 {
                ps.decoder_errors_total = Some(health_data.decode_errors_total);
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
                let loss = ps.audio_concealment_pct;

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
            // fps > 0.0 already proves decode CALLS are flowing; video_enabled (sender
            // self-report from peer_status events) is not required here and would suppress
            // scores if peer_status hasn't arrived yet.
            //
            // Freeze observability (#1013): during a freeze, fps_received still reads ~30
            // because decode calls keep firing fire-and-forget, yet the picture is visually
            // frozen because packets are lost and the stream is stuck requesting keyframes.
            // We fold the windowed loss rate and keyframe-request rate into the score so it
            // drops well below 100 in that state.
            let fps = ps
                .video_stats
                .as_ref()
                .map(|v| v.fps_received)
                .unwrap_or(0.0);
            if video_fresh && fps > 0.0 {
                ps.video_quality_score = video_quality_score(
                    fps,
                    ps.frames_dropped_per_sec,
                    ps.video_seq_loss_per_sec.unwrap_or(0.0),
                    ps.keyframe_requests_per_sec.unwrap_or(0.0),
                );
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

/// Compute the per-peer video quality score (0–100), or `None` when no frames
/// are flowing.
///
/// Freeze observability (#1013): a broadcast-relay stream can read fps ≈ 30
/// while being visually frozen — decode CALLS keep firing fire-and-forget even
/// as packets are lost and the stream is stuck requesting keyframes. fps alone
/// therefore cannot detect a freeze. We add two penalties on top of the
/// fps/decode-error health so a sustained loss or keyframe storm forces the
/// score well below 100:
///
/// * `loss_per_sec` — windowed packet-loss rate from `SequenceTracker`.
///   `5 lost/s → −30`. Mirrors the audio `loss_penalty` shape.
/// * `kf_per_sec`   — windowed keyframe-request (PLI) rate. A stream that is
///   continuously asking for keyframes is, by definition, not decoding cleanly,
///   so even a *sustained* ≥1 PLI/s is a strong freeze signal: `1 PLI/s → −40`.
///
/// The caller applies the outer `video_fresh && fps > 0.0` guard, so a true
/// freeze with zero fps yields `None` (Grafana renders a gap) rather than a
/// misleading 0 — `None` is the correct "no signal" state.
///
/// Returns `None` only when `fps <= 0.0` (defensive; the caller already
/// guards this), otherwise `Some(score)` clamped to `0..=100`.
fn video_quality_score(
    fps: f64,
    dropped_per_sec: f64,
    loss_per_sec: f64,
    kf_per_sec: f64,
) -> Option<f64> {
    if fps <= 0.0 {
        return None;
    }

    // Video health: measures whether video is present and stable, not hardware
    // FPS capability. A 15fps camera in low light is not a "problem" — it is the
    // camera doing auto-exposure correctly.
    //   fps >= 5  → 100  (video is working; FPS is hardware context, not quality)
    //   fps 1–4   → 0–50 (near-frozen; something is likely wrong)
    let video_health = if fps >= 5.0 { 100.0 } else { fps / 5.0 * 50.0 };

    // Decode error penalty: 0/s→0, 10+/s→−50.
    let drop_penalty = (dropped_per_sec / 10.0).min(1.0) * 50.0;
    // Packet-loss penalty: 0/s→0, 5+/s→−30 (mirrors the audio loss penalty).
    let loss_penalty = (loss_per_sec / 5.0).min(1.0) * 30.0;
    // Keyframe-storm penalty: a sustained ≥1 PLI/s means the decoder cannot make
    // forward progress → −40.
    let kf_penalty = (kf_per_sec / 1.0).min(1.0) * 40.0;

    let score = (video_health - drop_penalty - loss_penalty - kf_penalty).clamp(0.0, 100.0);
    Some(score)
}

// ===================================================================
// Security: HealthPacket credential-leak guard
// ===================================================================
//
// These tests guard the JWT-leak fix on branch
// `fix/security-redact-jwt-active-server-url`. A regression here means the
// user's room JWT escapes the client over the NATS health pipeline.

#[cfg(test)]
mod tests {
    use super::*;
    use protobuf::Message;
    use videocall_types::protos::health_packet::HealthPacket as PbHealthPacket;

    // ── Freeze observability (#1013): video_quality_score ────────────────

    /// Healthy stream: fps≥5, no loss, no keyframe storm → score 100.
    #[test]
    fn video_quality_score_healthy_is_100() {
        assert_eq!(video_quality_score(30.0, 0.0, 0.0, 0.0), Some(100.0));
    }

    /// fps == 0 yields None (absent), not 0 — a freeze with zero fps must show
    /// a Grafana gap, not a misleading zero.
    #[test]
    fn video_quality_score_zero_fps_is_none() {
        assert_eq!(video_quality_score(0.0, 0.0, 0.0, 0.0), None);
    }

    /// The core #1013 case: fps reads a healthy ~30 (decode calls still firing)
    /// but the stream is under sustained packet loss AND a keyframe storm. The
    /// score MUST drop well below 100 (it used to read 100 here — the bug).
    #[test]
    fn video_quality_score_drops_during_freeze_with_loss_and_keyframe_storm() {
        // 30 fps, no decode errors, 5 lost/s (-30), 1 PLI/s (-40) => 100-30-40=30.
        let score = video_quality_score(30.0, 0.0, 5.0, 1.0).expect("fps>0 => Some");
        assert!(
            score < 80.0,
            "freeze (loss + keyframe storm) should score well below 80, got {score}"
        );
        assert!((score - 30.0).abs() < 1e-9, "expected 30.0, got {score}");
    }

    /// Loss alone (no keyframe storm) still pulls the score down.
    #[test]
    fn video_quality_score_loss_only_penalty() {
        // 30 fps, 5 lost/s (-30) => 70.
        let score = video_quality_score(30.0, 0.0, 5.0, 0.0).expect("fps>0 => Some");
        assert!((score - 70.0).abs() < 1e-9, "expected 70.0, got {score}");
    }

    /// A sustained keyframe-request rate alone is a strong freeze signal.
    #[test]
    fn video_quality_score_keyframe_storm_only_penalty() {
        // 30 fps, 1 PLI/s (-40) => 60.
        let score = video_quality_score(30.0, 0.0, 0.0, 1.0).expect("fps>0 => Some");
        assert!((score - 60.0).abs() < 1e-9, "expected 60.0, got {score}");
    }

    /// Penalties saturate and the score clamps at 0, never negative.
    #[test]
    fn video_quality_score_clamps_at_zero() {
        let score = video_quality_score(30.0, 100.0, 100.0, 100.0).expect("fps>0 => Some");
        assert_eq!(score, 0.0);
    }

    /// Construct a `HealthPacket` via the production `create_health_packet`
    /// path, passing a `Some(...)` URL containing a JWT, and assert that the
    /// resulting protobuf has an empty `active_server_url` field.
    ///
    /// This test fails if anyone reintroduces `pb.active_server_url = url;` —
    /// preventing accidental regression of the credential leak.
    #[test]
    fn health_packet_does_not_carry_active_server_url() {
        // Seed `health_map` with at least one entry so `create_health_packet`
        // does not early-return `None`.
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let dirty_url = "https://webtransport.example.com:4433/lobby?token=eyJhbGciOiJIUzI1NiJ9.payload.sig&instance_id=11111111-2222-3333-4444-555555555555".to_string();

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            Some(dirty_url.clone()), // active_server_url — must be ignored
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0.0, // encoder_p75_peer_fps
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            0, // effective_video_layers (#1143)
            0, // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
        )
        .expect("create_health_packet must return Some when health_map is non-empty");

        // Round-trip the wrapper through the protobuf so we are asserting on
        // exactly what goes on the wire, not an in-memory builder field.
        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");

        assert!(
            pb.active_server_url.is_empty(),
            "HealthPacket.active_server_url must be empty (no JWT leak); got {:?}",
            pb.active_server_url
        );
        assert!(
            !pb.active_server_url.contains("eyJ"),
            "HealthPacket.active_server_url must not contain JWT-prefix `eyJ`"
        );
        assert!(
            !pb.active_server_url.contains("token="),
            "HealthPacket.active_server_url must not contain `token=`"
        );

        // Sanity: `active_server_type` and `active_server_rtt_ms` are still
        // populated — the security fix must not break observability of
        // transport identity and RTT.
        assert_eq!(pb.active_server_type, "webtransport");
        assert_eq!(pb.active_server_rtt_ms, 42.0);
    }

    /// Build a HealthPacket through the production `create_health_packet` path
    /// with the given decode-budget snapshot, then round-trip it through the
    /// protobuf so the assertions are on exactly what goes on the wire (#987).
    fn health_packet_with_decode_budget(
        decode_budget: Option<DecodeBudgetSnapshot>,
    ) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0.0, // encoder_p75_peer_fps
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            0, // effective_video_layers (#1143)
            0, // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            decode_budget,
            None,
        )
        .expect("create_health_packet must return Some when health_map is non-empty");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    /// #1032: build a HealthPacket through the production path with the given
    /// cached agent-memory value, then round-trip it through protobuf so the
    /// assertions are on exactly what goes on the wire.
    fn health_packet_with_agent_memory(agent_memory_bytes: Option<u64>) -> PbHealthPacket {
        let mut health_map = HashMap::new();
        health_map.insert(
            "peer-1".to_string(),
            PeerHealthData::new("peer-1".to_string()),
        );

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0.0, // encoder_p75_peer_fps
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            0, // effective_video_layers (#1143)
            0, // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            agent_memory_bytes,
        )
        .expect("create_health_packet must return Some when health_map is non-empty");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    fn health_packet_with_camera_playout_stats(fps_received: f64) -> PbHealthPacket {
        let mut peer = PeerHealthData::new("peer-1".to_string());
        peer.last_camera_stats = Some(json!({
            "fps_received": fps_received,
            "playout_latency_ms": 1500.0,
            "playout_stage1_span_ms": 1200.0,
            "playout_paint_lag_ms": 1800.0,
        }));

        let mut health_map = HashMap::new();
        health_map.insert("peer-1".to_string(), peer);

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0.0, // encoder_p75_peer_fps
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            0, // effective_video_layers (#1143)
            0, // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            None,
        )
        .expect("create_health_packet must return Some when health_map is non-empty");

        PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf")
    }

    #[test]
    fn playout_latency_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_latency_ms, 1500.0);
        assert_eq!(stats.playout_stage1_span_ms, 1200.0);
    }

    #[test]
    fn playout_latency_omitted_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(stats.playout_latency_ms, 0.0);
        assert_eq!(stats.playout_stage1_span_ms, 0.0);
    }

    #[test]
    fn playout_paint_lag_folds_when_fps_received_positive() {
        let pb = health_packet_with_camera_playout_stats(30.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 30.0);
        assert_eq!(stats.playout_paint_lag_ms, 1800.0);
    }

    #[test]
    fn playout_paint_lag_omitted_when_fps_received_zero() {
        let pb = health_packet_with_camera_playout_stats(0.0);
        let stats = pb
            .peer_stats
            .get("peer-1")
            .expect("peer stats must be present")
            .video_stats
            .as_ref()
            .expect("camera video stats must be present");

        assert_eq!(stats.fps_received, 0.0);
        assert_eq!(stats.playout_paint_lag_ms, 0.0);
    }

    /// #1032: a cached agent-memory reading rides the HealthPacket on the wire.
    #[test]
    fn agent_memory_rides_health_packet_when_present() {
        let pb = health_packet_with_agent_memory(Some(2_147_483_648));
        assert_eq!(pb.agent_memory_bytes, Some(2_147_483_648));
    }

    /// #1032: when the background sampler has produced no value (API absent or
    /// not yet resolved), the field is omitted — Grafana shows a gap, not a
    /// misleading zero.
    #[test]
    fn agent_memory_absent_when_none() {
        let pb = health_packet_with_agent_memory(None);
        assert!(pb.agent_memory_bytes.is_none());
    }

    /// #1032: packet construction must not disappear just because the peer
    /// health map is still empty; client-wide telemetry like non-heap memory
    /// still needs to flow during solo sessions and warm-up.
    #[test]
    fn health_packet_still_emitted_with_empty_peer_map() {
        let health_map = HashMap::new();

        let wrapper = HealthReporter::create_health_packet(
            "session-id-test",
            "meeting-id-test",
            "reporting-peer",
            "Display Name",
            &health_map,
            true,
            true,
            None,
            Some("webtransport".to_string()),
            Some(42.0),
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            0.0, // encoder_p75_peer_fps
            0.0, // encoder_target_bitrate_kbps
            0,
            false,
            0,
            0, // effective_video_layers (#1143)
            0, // active_video_layers (#1143)
            Vec::new(),
            ClimbLimiterSnapshot::default(),
            Vec::new(),
            0,
            0,
            [0, 0, 0, 0], // reelection_totals [proceeded, aborted, preserved, failed]
            Vec::new(),
            None,
            ClientMetadata::default(),
            None,
            Some(512),
        )
        .expect("empty peer map must still produce a packet");

        let pb = PbHealthPacket::parse_from_bytes(&wrapper.data)
            .expect("HealthPacket payload must be valid protobuf");

        assert!(pb.peer_stats.is_empty());
        assert_eq!(pb.agent_memory_bytes, Some(512));
    }

    /// #1032: a failed sample must clear the cache instead of leaving the last
    /// successful measurement visible forever.
    #[test]
    fn agent_memory_cache_clears_on_failure() {
        let cache = Rc::new(RefCell::new(Some(123)));
        *cache.borrow_mut() = Some(456);
        assert_eq!(*cache.borrow(), Some(456));

        *cache.borrow_mut() = None;
        assert_eq!(*cache.borrow(), None);
    }

    /// #1032: WASM linear memory is read inline in a `wasm32`-gated block, so on
    /// the (non-wasm) test target the field must be absent. This guards against
    /// anyone moving the read out of the cfg block and emitting a host-side
    /// value that would be meaningless for browser memory observability.
    #[test]
    fn wasm_memory_absent_on_non_wasm_target() {
        let pb = health_packet_with_agent_memory(None);
        assert!(
            pb.wasm_memory_bytes.is_none(),
            "wasm_memory_bytes must only be populated on the wasm32 target"
        );
    }

    #[test]
    fn decode_budget_snapshot_rides_health_packet_pressured_auto() {
        let pb = health_packet_with_decode_budget(Some(DecodeBudgetSnapshot {
            effective_cap: 5,
            natural: 12,
            pressured: true,
            override_mode: 1, // Auto
            override_fixed_n: 0,
        }));

        let db = pb
            .decode_budget
            .as_ref()
            .expect("decode_budget must be set");
        assert_eq!(db.effective_cap, 5);
        assert_eq!(db.natural, 12);
        assert!(db.pressured);
        assert_eq!(
            db.override_mode.enum_value_or_default(),
            PbOverrideMode::OVERRIDE_MODE_AUTO
        );
        // override_fixed_n is meaningless in Auto and left at its default.
        assert_eq!(db.override_fixed_n, 0);
    }

    #[test]
    fn decode_budget_snapshot_rides_health_packet_fixed_override() {
        let pb = health_packet_with_decode_budget(Some(DecodeBudgetSnapshot {
            effective_cap: 3,
            natural: 12,
            pressured: false,
            override_mode: 2, // Fixed
            override_fixed_n: 3,
        }));

        let db = pb
            .decode_budget
            .as_ref()
            .expect("decode_budget must be set");
        assert_eq!(db.effective_cap, 3);
        assert_eq!(
            db.override_mode.enum_value_or_default(),
            PbOverrideMode::OVERRIDE_MODE_FIXED
        );
        assert_eq!(db.override_fixed_n, 3);
    }

    #[test]
    fn decode_budget_absent_when_no_snapshot() {
        // No snapshot (controller pre-warmup / no peers) → field omitted so a
        // healthy no-peer packet stays minimal and backward-compatible.
        let pb = health_packet_with_decode_budget(None);
        assert!(pb.decode_budget.is_none());
    }

    #[test]
    fn normalize_gpu_family_known_vendors() {
        assert_eq!(normalize_gpu_family("Apple M1 Pro"), "Apple GPU");
        assert_eq!(normalize_gpu_family("Apple GPU"), "Apple GPU");
        assert_eq!(
            normalize_gpu_family(
                "ANGLE (Intel(R) Iris(R) Plus Graphics 645 Direct3D11 vs_5_0 ps_5_0, D3D11)"
            ),
            "Intel(R) Iris(R) Plus Graphics 6"
        );
        assert_eq!(
            normalize_gpu_family("ANGLE (NVIDIA GeForce RTX 3060 Direct3D11)"),
            "GeForce RTX 3060 Direct3"
        );
        assert_eq!(
            normalize_gpu_family("AMD Radeon Pro 5500M"),
            "Radeon Pro 5500M"
        );
        assert_eq!(normalize_gpu_family(""), "");
    }

    #[test]
    fn normalize_gpu_family_unknown_truncates() {
        let long = "SomeUnknownVendor With A Very Long Renderer String That Exceeds 32 Chars";
        let result = normalize_gpu_family(long);
        assert!(result.len() <= 32);
    }
}
