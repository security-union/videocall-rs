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

use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use futures::channel::mpsc::{self, Receiver, Sender, UnboundedSender};
use futures::StreamExt;
use js_sys::Date;
use log::{debug, error};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::window;
use yew::Callback;

use videocall_types::protos::diagnostics_packet::{AudioMetrics, DiagnosticsPacket, VideoMetrics};

use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;

pub const VIDEO_CALL_DIAGNOSTICS_SUBSYSTEM: &str = "videocall-client-diag";
pub const VIDEO_CALL_PEER_STATUS_SUBSYSTEM: &str = "peer-status";

// Basic structure for diagnostics events
#[derive(Debug, Clone)]
pub enum DiagnosticEvent {
    FrameReceived {
        peer_id: String,
        media_type: MediaType,
        frame_size: u64, // Size of the frame in bytes
    },
    RequestStats,
    SetStatsCallback(Callback<String>),
    SetReportingInterval(u64),
    HeartbeatTick, // New event for heartbeat
    SetPacketHandler(Callback<DiagnosticsPacket>),
}

// Stats for a peer's decoder
#[derive(Debug, Clone)]
pub struct DecoderStats {
    pub peer_id: String,
    pub frames_decoded: u32,
    pub frames_dropped: u32,
    pub fps: f64,
    pub media_type: MediaType,
    pub last_frame_time: f64, // Add timestamp of last received frame
}

// Stats for a peer's connection
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub peer_id: String,
    pub bytes_received: u64,
    pub packets_received: u64,
    pub packets_lost: u64,
    pub jitter: f64,
}

// Structure to track FPS for a peer
#[derive(Debug)]
struct FpsTracker {
    frames_count: u32,
    fps: f64,
    last_fps_update: f64, // timestamp in ms
    total_frames: u32,
    #[allow(dead_code)]
    media_type: MediaType,
    last_frame_time: f64,     // Add timestamp of last received frame
    bytes_received: u64,      // Track total bytes received
    last_bitrate_update: f64, // Last time we calculated bitrate
    current_bitrate: f64,     // Current bitrate in kbits/sec
}

impl FpsTracker {
    fn new(media_type: MediaType) -> Self {
        let now = Date::now();
        Self {
            frames_count: 0,
            fps: 0.0,
            last_fps_update: now,
            total_frames: 0,
            media_type,
            last_frame_time: now,
            bytes_received: 0,
            last_bitrate_update: now,
            current_bitrate: 0.0,
        }
    }

    fn track_frame_with_size(&mut self, bytes: u64) -> (f64, f64) {
        self.frames_count += 1;
        self.total_frames += 1;
        let now = Date::now();
        self.last_frame_time = now; // Record when we received the frame

        // Update bytes and calculate bitrate
        self.bytes_received += bytes;
        let elapsed_ms = now - self.last_bitrate_update;

        // Update FPS calculation every second
        if elapsed_ms >= 1000.0 {
            self.fps = (self.frames_count as f64 * 1000.0) / elapsed_ms;
            self.frames_count = 0;

            // Calculate bitrate in kbits/sec
            let bits = (self.bytes_received * 8) as f64;
            self.current_bitrate = (bits / elapsed_ms) * 1000.0 / 1000.0; // Convert to kbits/sec

            // Reset counters
            self.bytes_received = 0;
            self.last_fps_update = now;
            self.last_bitrate_update = now;
        }

        (self.fps, self.current_bitrate)
    }

    // Check if no frames have been received for a while and reset FPS if needed
    fn _check_inactive(&mut self, now: f64) {
        let inactive_ms = now - self.last_frame_time;

        // If no frames for more than 1 second, consider the feed inactive
        if inactive_ms > 1000.0 {
            // Set FPS and bitrate to zero immediately when inactive
            if self.fps > 0.0 || self.current_bitrate > 0.0 {
                log::info!(
                    "Detected inactive stream, setting metrics to 0 (inactive for {inactive_ms:.0}ms)"
                );
                self.fps = 0.0;
                self.current_bitrate = 0.0;
                self.frames_count = 0;
                self.bytes_received = 0;
                self.last_fps_update = now;
                self.last_bitrate_update = now;
            }
        }
    }

    fn get_metrics(&self) -> (f64, f64) {
        let now = Date::now();
        let inactive_ms = now - self.last_frame_time;

        // If inactive for more than 1 second, return zeros
        if inactive_ms > 1000.0 {
            (0.0, 0.0)
        } else {
            (self.fps, self.current_bitrate)
        }
    }
}

// Define a struct to hold the JavaScript timer resources
struct JsTimer {
    #[allow(dead_code)]
    closure: Closure<dyn FnMut()>,
    interval_id: i32,
}

impl Drop for JsTimer {
    fn drop(&mut self) {
        // This ensures the interval is cleared when the timer is dropped
        if let Some(window) = window() {
            log::info!("Cleaning up diagnostics heartbeat interval");
            window.clear_interval_with_handle(self.interval_id);
        }
    }
}

impl std::fmt::Debug for JsTimer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsTimer")
            .field("interval_id", &self.interval_id)
            .finish()
    }
}

// The DiagnosticManager manages the collection and reporting of diagnostic information
pub struct DiagnosticManager {
    sender: Sender<DiagnosticEvent>,
    frames_decoded: Arc<AtomicU32>,
    frames_dropped: Arc<AtomicU32>,
    report_interval_ms: u64,
    timer: Option<Rc<JsTimer>>,
}

unsafe impl Sync for DiagnosticManager {}
unsafe impl Send for DiagnosticManager {}

// Internal worker that processes diagnostic events
struct DiagnosticWorker {
    // Track FPS per peer and per media type (audio, video, screen)
    fps_trackers: HashMap<String, HashMap<MediaType, FpsTracker>>,
    on_stats_update: Option<Callback<String>>,
    last_report_time: f64, // timestamp in ms
    report_interval_ms: u64,
    packet_handler: Option<Callback<DiagnosticsPacket>>,
    receiver: Receiver<DiagnosticEvent>,
    userid: String,
}

impl std::fmt::Debug for DiagnosticManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiagnosticManager")
            .field("frames_decoded", &self.frames_decoded)
            .field("frames_dropped", &self.frames_dropped)
            .field("report_interval_ms", &self.report_interval_ms)
            .finish()
    }
}

impl DiagnosticManager {
    pub fn new(userid: String) -> Self {
        let (sender, receiver) = mpsc::channel(100);

        // Spawn the worker to process events
        let worker = DiagnosticWorker {
            fps_trackers: HashMap::new(),
            on_stats_update: None,
            packet_handler: None,
            last_report_time: Date::now(),
            report_interval_ms: 500,
            receiver,
            userid,
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        let mut manager = Self {
            sender: sender.clone(),
            frames_decoded: Arc::new(AtomicU32::new(0)),
            frames_dropped: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 500,
            timer: None,
        };

        manager.setup_heartbeat(sender);

        manager
    }

    // Start a JavaScript interval timer that sends heartbeat events
    fn setup_heartbeat(&mut self, sender: Sender<DiagnosticEvent>) {
        let sender_clone = sender.clone();

        // Create a closure that sends a heartbeat event through the channel
        let callback = Closure::wrap(Box::new(move || {
            if let Err(e) = sender_clone
                .clone()
                .try_send(DiagnosticEvent::HeartbeatTick)
            {
                log::info!("Failed to send heartbeat: {e:?}");
            }
        }) as Box<dyn FnMut()>);

        // Set up the interval to run every 500ms
        let interval_id = window()
            .expect("Failed to get window")
            .set_interval_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                500,
            )
            .expect("Failed to set interval");

        // Create and store the timer in an Rc
        self.timer = Some(Rc::new(JsTimer {
            closure: callback,
            interval_id,
        }));
    }

    // Set the callback for UI updates
    pub fn set_stats_callback(&self, callback: Callback<String>) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::SetStatsCallback(callback))
        {
            error!("Failed to set stats callback: {e}");
        }
    }

    // Set the callback for when a diagnostic packet is received
    pub fn set_packet_handler(&self, callback: Callback<DiagnosticsPacket>) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::SetPacketHandler(callback))
        {
            error!("Failed to set packet handler: {e}");
        }
    }

    // Set how often stats should be reported to the UI (in milliseconds)
    pub fn set_reporting_interval(&mut self, interval_ms: u64) {
        self.report_interval_ms = interval_ms;
        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::SetReportingInterval(interval_ms))
        {
            error!("Failed to set reporting interval: {e}");
        }
    }

    // Track a frame received from a peer for a specific media type
    pub fn track_frame(&self, peer_id: &str, media_type: MediaType, frame_size: u64) -> f64 {
        self.frames_decoded.fetch_add(1, Ordering::SeqCst);

        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::FrameReceived {
                peer_id: peer_id.to_string(),
                media_type,
                frame_size,
            })
        {
            error!("Failed to send frame event: {e}");
        }

        if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::RequestStats) {
            error!("Failed to request stats: {e}");
        }

        0.0
    }

    // Increment the frames dropped counter
    pub fn increment_frames_dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::SeqCst);
    }

    // Get the current frames decoded count
    pub fn get_frames_decoded(&self) -> u32 {
        self.frames_decoded.load(Ordering::SeqCst)
    }

    // Get the current frames dropped count
    pub fn get_frames_dropped(&self) -> u32 {
        self.frames_dropped.load(Ordering::SeqCst)
    }

    // Method to be implemented fully later
    pub fn report_event(&self, _event: DiagnosticEvent) -> Result<(), Box<dyn Error>> {
        // Will be implemented when we need it
        Ok(())
    }

    // Method to be implemented fully later
    pub fn get_stats(&self) -> Result<JsValue, Box<dyn Error>> {
        // Will be implemented when we need it
        Ok(JsValue::null())
    }
}

impl Drop for DiagnosticManager {
    fn drop(&mut self) {
        // Simply drop the timer, its own Drop impl will handle cleanup
        self.timer = None;
    }
}

impl DiagnosticWorker {
    async fn run(mut self) {
        while let Some(event) = self.receiver.next().await {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: DiagnosticEvent) {
        match event {
            DiagnosticEvent::FrameReceived {
                peer_id,
                media_type,
                frame_size,
            } => {
                let peer_trackers = self.fps_trackers.entry(peer_id.clone()).or_default();

                let tracker = peer_trackers
                    .entry(media_type)
                    .or_insert_with(|| FpsTracker::new(media_type));

                tracker.track_frame_with_size(frame_size);
            }
            DiagnosticEvent::SetStatsCallback(callback) => {
                self.on_stats_update = Some(callback);
            }
            DiagnosticEvent::SetReportingInterval(interval) => {
                self.report_interval_ms = interval;
            }
            DiagnosticEvent::RequestStats => {
                self.maybe_report_stats_to_ui();
            }
            DiagnosticEvent::HeartbeatTick => {
                // Log heartbeat for debugging
                debug!("Diagnostics heartbeat tick");

                // Always report stats on heartbeat
                self.maybe_report_stats_to_ui();
                // Create and send diagnostic packets for each peer

                self.send_diagnostic_packets();
            }
            DiagnosticEvent::SetPacketHandler(callback) => {
                self.packet_handler = Some(callback);
            }
        }
    }

    fn send_diagnostic_packets(&self) {
        let now = Date::now();
        let timestamp_ms = now as u64;

        for (peer_id, peer_trackers) in &self.fps_trackers {
            for (media_type, tracker) in peer_trackers {
                // Always publish to global diagnostics broadcast system (independent of packet handler)
                let event = DiagEvent {
                    subsystem: "decoder",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("fps", tracker.fps),
                        metric!("bitrate_kbps", tracker.current_bitrate),
                        metric!("media_type", format!("{:?}", media_type)),
                        metric!("from_peer", self.userid.clone()),
                        metric!("to_peer", peer_id.clone()),
                    ],
                };
                debug!(
                    "Broadcasting decoder event for peer {} ({:?}): FPS={:.2}, Bitrate={:.1}kbps",
                    peer_id, media_type, tracker.fps, tracker.current_bitrate
                );
                let _ = global_sender().try_broadcast(event);

                // Also publish a normalized video event that the health reporter uses for UI + server
                let video_event = DiagEvent {
                    subsystem: "video",
                    stream_id: None,
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("fps_received", tracker.fps),
                        metric!("bitrate_kbps", tracker.current_bitrate),
                        metric!("from_peer", self.userid.clone()),
                        metric!("to_peer", peer_id.clone()),
                    ],
                };
                let _ = global_sender().try_broadcast(video_event);

                // Only create and send protobuf packets if packet handler is set (legacy system)
                if let Some(handler) = &self.packet_handler {
                    let mut packet = DiagnosticsPacket::new();
                    packet.target_id = self.userid.clone();
                    packet.sender_id = peer_id.clone();
                    packet.timestamp_ms = timestamp_ms;

                    packet.media_type = (*media_type).into();

                    if *media_type == MediaType::AUDIO {
                        let mut audio_metrics = AudioMetrics::new();
                        audio_metrics.fps_received = tracker.fps as f32;
                        audio_metrics.bitrate_kbps = tracker.current_bitrate as u32;
                        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);
                    } else {
                        let mut video_metrics = VideoMetrics::new();
                        video_metrics.fps_received = tracker.fps as f32;
                        video_metrics.bitrate_kbps = tracker.current_bitrate as u32;
                        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
                    }

                    debug!(
                        "Sending diagnostic packet to {}: {:?} FPS: {:.2} Bitrate: {:.1} kbit/s",
                        peer_id, media_type, tracker.fps, tracker.current_bitrate
                    );
                    handler.emit(packet);
                }
            }
        }
    }

    // Check if it's time to report stats and do so if needed
    fn maybe_report_stats_to_ui(&mut self) {
        let now = Date::now();
        let elapsed_ms = now - self.last_report_time;

        if elapsed_ms >= self.report_interval_ms as f64 {
            // Time to report
            let stats_string = self.get_fps_stats_string();

            // Report stats to UI if callback is set
            if let Some(callback) = &self.on_stats_update {
                callback.emit(stats_string);
            }

            // Update last report time
            self.last_report_time = now;
        }
    }

    // Get all FPS stats for all peers
    fn get_all_fps_stats(&self) -> HashMap<String, HashMap<MediaType, (f64, f64)>> {
        let mut result = HashMap::new();
        for (peer_id, peer_trackers) in &self.fps_trackers {
            let mut media_fps = HashMap::new();
            for (media_type, tracker) in peer_trackers {
                let metrics = tracker.get_metrics();
                media_fps.insert(*media_type, metrics);
            }
            result.insert(peer_id.clone(), media_fps);
        }

        result
    }

    // Get a formatted string with FPS stats for all peers
    fn get_fps_stats_string(&self) -> String {
        let stats = self.get_all_fps_stats();
        let mut result = String::new();

        // Add timestamp
        let now = Date::now();
        result.push_str(&format!("Time: {now:.0}ms\n"));

        for (peer_id, media_stats) in stats.iter() {
            result.push_str(&format!("Peer {peer_id}: "));

            // First show Video if it exists
            if let Some((fps, bitrate)) = media_stats.get(&MediaType::VIDEO) {
                self.append_media_stats(&mut result, "Video", *fps, *bitrate);
            }

            // Then show Audio if it exists
            if let Some((fps, bitrate)) = media_stats.get(&MediaType::AUDIO) {
                self.append_media_stats(&mut result, "Audio", *fps, *bitrate);
            }

            // Finally show Screen if it exists
            if let Some((fps, bitrate)) = media_stats.get(&MediaType::SCREEN) {
                self.append_media_stats(&mut result, "Screen", *fps, *bitrate);
            }

            result.push('\n');
        }

        if stats.is_empty() {
            result.push_str("No active peers.\n");
        }

        result
    }

    fn append_media_stats(&self, result: &mut String, media_str: &str, fps: f64, bitrate: f64) {
        if fps <= 0.01 || bitrate <= 0.01 {
            result.push_str(&format!("{media_str}=INACTIVE "));
        } else {
            result.push_str(&format!("{media_str}={fps:.2} FPS {bitrate:.1} kbit/s "));
        }
    }
}

// Event types for sender diagnostics
#[derive(Debug, Clone)]
pub enum SenderDiagnosticEvent {
    DiagnosticPacketReceived(DiagnosticsPacket),
    SetStatsCallback(Callback<String>),
    SetReportingInterval(u64),
    HeartbeatTick,
    AddEncoderCallback(Callback<DiagnosticsPacket>),
    AddSenderChannel(UnboundedSender<DiagnosticsPacket>, MediaType),
}

// Structure to track stats for a media stream we're sending
#[derive(Debug)]
struct StreamStats {
    _media_type: MediaType,
    last_update: f64,
    median_latency_ms: u32,
    jitter_ms: u32,
    estimated_bandwidth_kbps: u32,
    round_trip_time_ms: u32,
    _peer_id: String,
}

impl StreamStats {
    fn new(peer_id: String, media_type: MediaType) -> Self {
        StreamStats {
            _media_type: media_type,
            last_update: Date::now(),
            median_latency_ms: 0,
            jitter_ms: 0,
            estimated_bandwidth_kbps: 0,
            round_trip_time_ms: 0,
            _peer_id: peer_id,
        }
    }

    fn update_from_packet(&mut self, packet: &DiagnosticsPacket, media_type: MediaType) {
        self.last_update = Date::now();

        self.estimated_bandwidth_kbps = match media_type {
            MediaType::VIDEO => packet.video_metrics.clone().unwrap().bitrate_kbps,
            MediaType::AUDIO => packet.audio_metrics.clone().unwrap().bitrate_kbps,
            MediaType::SCREEN => packet.video_metrics.clone().unwrap().bitrate_kbps,
            _ => 0,
        };
    }

    fn is_stale(&self) -> bool {
        Date::now() - self.last_update > 2000.0 // Consider stale after 2 seconds
    }
}

#[derive(Debug, Clone)]
pub struct SenderDiagnosticManager {
    sender: Sender<SenderDiagnosticEvent>,
    timer: Option<Rc<JsTimer>>,
    _report_interval_ms: u64,
}

struct SenderDiagnosticWorker {
    stream_stats: HashMap<String, HashMap<MediaType, StreamStats>>, // peer_id -> media_type -> stats
    on_stats_update: Option<Callback<String>>,
    encoder_callbacks: Vec<Callback<DiagnosticsPacket>>,
    sender_channels: Vec<(UnboundedSender<DiagnosticsPacket>, MediaType)>,
    last_report_time: f64,
    report_interval_ms: u64,
    receiver: Receiver<SenderDiagnosticEvent>,
    userid: String,
}

impl SenderDiagnosticManager {
    pub fn new(userid: String) -> Self {
        let (sender, receiver) = mpsc::channel(100);

        let worker = SenderDiagnosticWorker {
            stream_stats: HashMap::new(),
            on_stats_update: None,
            encoder_callbacks: Vec::new(),
            sender_channels: Vec::new(),
            last_report_time: Date::now(),
            report_interval_ms: 500,
            receiver,
            userid,
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        let mut manager = Self {
            sender: sender.clone(),
            timer: None,
            _report_interval_ms: 500,
        };

        // Set up heartbeat timer
        manager.setup_heartbeat(sender);

        manager
    }

    fn setup_heartbeat(&mut self, sender: Sender<SenderDiagnosticEvent>) {
        let sender_clone = sender.clone();

        let callback = Closure::wrap(Box::new(move || {
            if let Err(e) = sender_clone
                .clone()
                .try_send(SenderDiagnosticEvent::HeartbeatTick)
            {
                log::info!("Failed to send sender heartbeat: {e:?}");
            }
        }) as Box<dyn FnMut()>);

        let interval_id = window()
            .expect("Failed to get window")
            .set_interval_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                500,
            )
            .expect("Failed to set interval");

        self.timer = Some(Rc::new(JsTimer {
            closure: callback,
            interval_id,
        }));
    }

    pub fn set_stats_callback(&self, callback: Callback<String>) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::SetStatsCallback(callback))
        {
            error!("Failed to set sender stats callback: {e}");
        }
    }

    pub fn add_encoder_callback(&self, callback: Callback<DiagnosticsPacket>) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::AddEncoderCallback(callback))
        {
            error!("Failed to set encoder callback: {e}");
        }
    }

    pub fn add_sender_channel(
        &self,
        sender: UnboundedSender<DiagnosticsPacket>,
        media_type: MediaType,
    ) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::AddSenderChannel(sender, media_type))
        {
            error!("Failed to set sender channel: {e}");
        }
    }

    pub fn set_reporting_interval(&self, interval_ms: u64) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::SetReportingInterval(interval_ms))
        {
            error!("Failed to set sender reporting interval: {e}");
        }
    }

    pub fn handle_diagnostic_packet(&self, packet: DiagnosticsPacket) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::DiagnosticPacketReceived(packet))
        {
            error!("Failed to handle diagnostic packet: {e}");
        }
    }
}

impl Drop for SenderDiagnosticManager {
    fn drop(&mut self) {
        self.timer = None;
    }
}

impl SenderDiagnosticWorker {
    async fn run(mut self) {
        while let Some(event) = self.receiver.next().await {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: SenderDiagnosticEvent) {
        match event {
            SenderDiagnosticEvent::DiagnosticPacketReceived(packet) => {
                let sender_id = packet.sender_id.clone();
                let target_id = packet.target_id.clone();
                let media_type: MediaType = packet.media_type.enum_value_or_default();

                // Publish to global diagnostics broadcast system
                let event = DiagEvent {
                    subsystem: VIDEO_CALL_DIAGNOSTICS_SUBSYSTEM,
                    stream_id: Some(target_id.clone()),
                    ts_ms: now_ms(),
                    metrics: vec![
                        metric!("sender_id", sender_id.clone()),
                        metric!("target_id", target_id.clone()),
                        metric!("media_type", format!("{:?}", media_type)),
                        metric!("packet_timestamp", packet.timestamp_ms),
                    ],
                };
                debug!(
                    "Broadcasting sender event for target {target_id}: sender={sender_id}, media_type={media_type:?}"
                );
                let _ = global_sender().try_broadcast(event);

                if sender_id == self.userid {
                    let peer_stats = self.stream_stats.entry(target_id.clone()).or_default();
                    let stats = peer_stats
                        .entry(media_type)
                        .or_insert_with(|| StreamStats::new(target_id, media_type));
                    stats.update_from_packet(&packet, media_type);
                }

                for (sender, sender_media_type) in &self.sender_channels {
                    if sender_media_type == &media_type {
                        if let Err(e) = sender.unbounded_send(packet.clone()) {
                            error!("Failed to send diagnostic packet to sender: {e}");
                        }
                    }
                }
            }
            SenderDiagnosticEvent::SetStatsCallback(callback) => {
                self.on_stats_update = Some(callback);
            }
            SenderDiagnosticEvent::SetReportingInterval(interval) => {
                self.report_interval_ms = interval;
            }
            SenderDiagnosticEvent::HeartbeatTick => {
                self.maybe_report_stats_to_ui();
            }
            SenderDiagnosticEvent::AddEncoderCallback(callback) => {
                // Add the callback to the list of callbacks
                self.encoder_callbacks.push(callback);
            }
            SenderDiagnosticEvent::AddSenderChannel(sender, media_type) => {
                self.sender_channels.push((sender, media_type));
            }
        }
    }

    fn maybe_report_stats_to_ui(&mut self) {
        let now = Date::now();
        let elapsed_ms = now - self.last_report_time;

        if elapsed_ms >= self.report_interval_ms as f64 {
            let stats_string = self.get_stats_string();

            if let Some(callback) = &self.on_stats_update {
                callback.emit(stats_string);
            }

            self.last_report_time = now;
        }
    }

    fn get_stats_string(&mut self) -> String {
        let mut result = String::new();

        // Remove stale entries
        self.stream_stats.retain(|_, media_stats| {
            media_stats.retain(|_, stats| !stats.is_stale());
            !media_stats.is_empty()
        });

        // Only show stats for the current peer (where peer_id matches our userid)
        for (peer_id, media_stats) in &self.stream_stats {
            result.push_str(&format!("Peer {peer_id}\n"));

            // First show Video if it exists
            if let Some(stats) = media_stats.get(&MediaType::VIDEO) {
                self.append_media_stats(&mut result, "Video", stats);
            }

            // Then show Audio if it exists
            if let Some(stats) = media_stats.get(&MediaType::AUDIO) {
                self.append_media_stats(&mut result, "Audio", stats);
            }

            // Finally show Screen if it exists
            if let Some(stats) = media_stats.get(&MediaType::SCREEN) {
                self.append_media_stats(&mut result, "Screen", stats);
            }
        }
        if self.stream_stats.is_empty() {
            result.push_str("No feedback received about your streams yet.\n");
        }

        result
    }

    fn append_media_stats(&self, result: &mut String, media_str: &str, stats: &StreamStats) {
        result.push_str(&format!(
            "{}: {} kbps, {} ms latency, {} ms jitter, {} ms RTT\n",
            media_str,
            stats.estimated_bandwidth_kbps,
            stats.median_latency_ms,
            stats.jitter_ms,
            stats.round_trip_time_ms,
        ));
    }
}
