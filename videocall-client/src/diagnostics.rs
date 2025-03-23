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

use videocall_types::protos::diagnostics_packet::{
    self as diag, quality_hints::QualityPreference, AudioMetrics, DiagnosticsPacket, VideoMetrics,
};

use videocall_types::protos::media_packet::media_packet::MediaType;

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
                    "Detected inactive stream, setting metrics to 0 (inactive for {:.0}ms)",
                    inactive_ms
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
                log::info!("Failed to send heartbeat: {:?}", e);
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
                if self.packet_handler.is_none() {
                    continue;
                }

                let mut packet = DiagnosticsPacket::new();
                packet.target_id = self.userid.clone();
                packet.sender_id = peer_id.clone();
                packet.timestamp_ms = timestamp_ms;

                let proto_media_type = match media_type {
                    MediaType::VIDEO => diag::diagnostics_packet::MediaType::VIDEO,
                    MediaType::SCREEN => diag::diagnostics_packet::MediaType::SCREEN,
                    MediaType::AUDIO => diag::diagnostics_packet::MediaType::AUDIO,
                    _ => continue,
                };
                packet.media_type = proto_media_type.into();

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

                if let Some(handler) = &self.packet_handler {
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
        result.push_str(&format!("Time: {:.0}ms\n", now));

        for (peer_id, media_stats) in stats.iter() {
            result.push_str(&format!("Peer {}: ", peer_id));

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
            result.push_str(&format!("{}=INACTIVE ", media_str));
        } else {
            result.push_str(&format!(
                "{}={:.2} FPS {:.1} kbit/s ",
                media_str, fps, bitrate
            ));
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
    packet_loss_percent: f32,
    median_latency_ms: u32,
    jitter_ms: u32,
    estimated_bandwidth_kbps: u32,
    round_trip_time_ms: u32,
    _peer_id: String,
}

impl StreamStats {
    fn new(peer_id: String, media_type: MediaType) -> Self {
        Self {
            _media_type: media_type,
            last_update: Date::now(),
            packet_loss_percent: 0.0,
            median_latency_ms: 0,
            jitter_ms: 0,
            estimated_bandwidth_kbps: 0,
            round_trip_time_ms: 0,
            _peer_id: peer_id,
        }
    }

    fn update_from_packet(&mut self, packet: &DiagnosticsPacket, media_type: MediaType) {
        self.last_update = Date::now();
        self.packet_loss_percent = packet.packet_loss_percent;
        self.median_latency_ms = packet.median_latency_ms;
        self.jitter_ms = packet.jitter_ms;

        self.estimated_bandwidth_kbps = match media_type {
            MediaType::VIDEO => packet.video_metrics.clone().unwrap().bitrate_kbps,
            MediaType::AUDIO => packet.audio_metrics.clone().unwrap().bitrate_kbps,
            _ => 0,
        };
        self.round_trip_time_ms = packet.round_trip_time_ms;
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
                log::info!("Failed to send sender heartbeat: {:?}", e);
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
                let media_type = match packet.media_type.enum_value_or_default() {
                    diag::diagnostics_packet::MediaType::VIDEO => MediaType::VIDEO,
                    diag::diagnostics_packet::MediaType::AUDIO => MediaType::AUDIO,
                    diag::diagnostics_packet::MediaType::SCREEN => MediaType::SCREEN,
                };

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
            result.push_str(&format!("Peer {}\n", peer_id));

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
            "  {}: Loss={:.1}% RTT={}ms BW={} kbit/s\n",
            media_str,
            stats.packet_loss_percent,
            stats.round_trip_time_ms,
            stats.estimated_bandwidth_kbps
        ));
    }
}

/// EncoderControl is responsible for bridging the gap between the encoder and the
/// diagnostics system.

/// It closes the loop by allowing the encoder to adjust its settings based on
/// feedback from the diagnostics system.
#[derive(Debug, Clone)]
pub enum EncoderControl {
    UpdateBitrate { target_bitrate_kbps: u32 },
    UpdateQualityPreference { preference: QualityPreference },
}

pub struct EncoderControlSender {
    pid: pidgeon::PidController,
    last_update: f64,
    _ideal_bitrate_kbps: u32,
    _current_fps: Rc<AtomicU32>,
    fps_history: std::collections::VecDeque<f64>, // Sliding window of recent FPS values
    max_history_size: usize,                      // Maximum size of history window
    last_error: f64,                              // Track the previous error for stability checks
    initialization_complete: bool,                // Flag to handle startup conditions
}

impl EncoderControlSender {
    pub fn new(ideal_bitrate_kbps: u32, current_fps: Rc<AtomicU32>) -> Self {
        // Configure the PID controller for stable bitrate control
        // Lower gains make the controller more gentle and less prone to overreaction
        let controller_config = pidgeon::ControllerConfig::default()
            .with_kp(0.2) // Proportional gain - how quickly to respond to current error
            .with_ki(0.05) // Integral gain - how strongly to respond to accumulated error
            .with_kd(0.02) // Derivative gain - dampen oscillations
            .with_setpoint(0.0) // Target error is zero (received FPS = target FPS)
            .with_deadband(0.5) // Ignore tiny fluctuations (±0.5 FPS)
            .with_output_limits(0.0, 50.0) // Limit maximum adjustment
            .with_anti_windup(true); // Prevent integral term from accumulating too much

        let pid = pidgeon::PidController::new(controller_config);
        Self {
            pid,
            last_update: Date::now(),
            _ideal_bitrate_kbps: ideal_bitrate_kbps,
            _current_fps: current_fps,
            fps_history: std::collections::VecDeque::with_capacity(10),
            max_history_size: 10,
            last_error: 0.0,
            initialization_complete: false,
        }
    }

    // Calculate the standard deviation of FPS values to measure jitter
    fn calculate_jitter(&self) -> f64 {
        if self.fps_history.len() < 2 {
            return 0.0; // Not enough samples to calculate jitter
        }

        // Calculate mean
        let sum: f64 = self.fps_history.iter().sum();
        let mean = sum / self.fps_history.len() as f64;

        // Calculate variance
        let variance: f64 = self
            .fps_history
            .iter()
            .map(|&fps| {
                let diff = fps - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.fps_history.len() as f64;

        // Return standard deviation
        variance.sqrt()
    }

    pub fn process_diagnostics_packet(&mut self, packet: DiagnosticsPacket) -> Option<f64> {
        // Extract the received FPS from the packet
        let fps_received = match packet.video_metrics.as_ref() {
            Some(metrics) => metrics.fps_received as f64,
            None => return None, // No video metrics available
        };

        let target_fps = self._current_fps.load(Ordering::Relaxed) as f64;
        if target_fps <= 0.0 {
            return Some(500_000.0); // Default bitrate if target FPS is invalid
        }

        // Add current FPS to history
        self.fps_history.push_back(fps_received);

        // Maintain history size limit
        while self.fps_history.len() > self.max_history_size {
            self.fps_history.pop_front();
        }

        // Calculate jitter (FPS standard deviation)
        let jitter = self.calculate_jitter();

        // Calculate time delta since last update
        let now = Date::now();
        let dt = now - self.last_update;
        self.last_update = now;

        // Skip processing if time delta is too small or too large
        // This avoids instability from rapid updates or stale data
        if !(50.0..=5000.0).contains(&dt) {
            log::info!("Skipping update - time delta ({} ms) out of range", dt);
            return Some(500_000.0); // Return default bitrate
        }

        // Compute the error: difference between target and actual FPS
        let current_error = target_fps - fps_received;

        // Special handling for initialization
        if !self.initialization_complete {
            if self.fps_history.len() >= 3 {
                self.initialization_complete = true;
            } else {
                // During initialization, just track the error but don't react strongly
                self.last_error = current_error;
                return Some(500_000.0); // Return default bitrate during initialization
            }
        }

        // Calculate rate of change of error for smoother response
        let error_derivative = (current_error - self.last_error) / dt * 1000.0;
        self.last_error = current_error;

        // Use PID controller to compute adjustment based on FPS error
        let fps_error_output = self.pid.compute(current_error, dt);

        // Get the jitter factor (normalized by target FPS)
        let normalized_jitter = jitter / target_fps;
        let jitter_factor = (normalized_jitter * 5.0).min(1.0);

        // Base bitrate calculation
        let base_bitrate = 500_000.0;

        // Adjust bitrate based on PID output
        // Scale factor is lower (3,000) for more gradual adjustments
        let fps_adjustment = fps_error_output * 3_000.0;

        // Apply the PID-based adjustment
        let after_pid = base_bitrate - fps_adjustment;

        // Apply jitter penalty (up to 20% reduction for maximum jitter)
        let jitter_reduction = after_pid * (jitter_factor * 0.2);

        // Calculate final bitrate
        let corrected_bitrate = after_pid - jitter_reduction;

        // Log detailed diagnostic information
        log::info!(
            "FPS: target={:.1} received={:.1} error={:.1} | PID output={:.2} | Jitter={:.2} factor={:.2} | Bitrate: base={} pid_adj={:.0} jitter_adj={:.0} final={:.0}", 
            target_fps, fps_received, current_error,
            fps_error_output, jitter, jitter_factor,
            base_bitrate, fps_adjustment, jitter_reduction, corrected_bitrate
        );

        // Ensure we have a reasonable bitrate (between 100kbps and 2Mbps)
        if !(100_000.0..=2_000_000.0).contains(&corrected_bitrate) || corrected_bitrate.is_nan() {
            log::warn!("Bitrate out of bounds or NaN: {}", corrected_bitrate);
            // Return a safe default value instead of None to maintain stability
            return Some(f64::max(100_000.0, f64::min(base_bitrate, 2_000_000.0)));
        }

        Some(corrected_bitrate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::atomic::AtomicU32;
    use videocall_types::protos::diagnostics_packet::{DiagnosticsPacket, VideoMetrics};
    use wasm_bindgen_test::*;

    // Remove browser-only configuration and make tests run in any environment
    // wasm_bindgen_test_configure!(run_in_browser);

    // Helper to simulate time passing more reliably
    fn simulate_time_passing(controller: &mut EncoderControlSender, ms: f64) {
        let now = js_sys::Date::now();
        controller.last_update = now - ms;
    }

    fn create_test_packet(fps: f32, bitrate_kbps: u32) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = "test_sender".to_string();
        packet.target_id = "test_target".to_string();
        packet.timestamp_ms = js_sys::Date::now() as u64;
        packet.media_type =
            videocall_types::protos::diagnostics_packet::diagnostics_packet::MediaType::VIDEO
                .into();

        let mut video_metrics = VideoMetrics::new();
        video_metrics.fps_received = fps;
        video_metrics.bitrate_kbps = bitrate_kbps;
        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);

        packet
    }

    #[wasm_bindgen_test]
    fn test_happy_path() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
        let mut controller = EncoderControlSender::new(500_000, target_fps.clone());

        // Generate a series of packets with perfect conditions
        // FPS matches the target exactly, no jitter
        for _ in 0..10 {
            let packet = create_test_packet(30.0, 500);
            let result = controller.process_diagnostics_packet(packet);

            // With perfect conditions (no error, no jitter),
            // the bitrate should stay close to the ideal
            if let Some(bitrate) = result {
                // Should be close to base bitrate (500,000)
                assert!(
                    (bitrate - 500_000.0).abs() < 10_000.0,
                    "Expected bitrate close to base (500,000), got {}",
                    bitrate
                );
            }

            // Simulate time passing for the next packet
            simulate_time_passing(&mut controller, 100.0); // 100ms ago
        }

        // Check history shows stable FPS
        assert_eq!(controller.fps_history.len(), 10);
        let jitter = controller.calculate_jitter();
        assert!(
            jitter < 0.1,
            "Expected near-zero jitter in happy path, got {}",
            jitter
        );
    }

    #[wasm_bindgen_test]
    fn test_bandwidth_drop() {
        // Setup with a target of 30 FPS
        let target_fps = Rc::new(AtomicU32::new(30));
        let mut controller = EncoderControlSender::new(500_000, target_fps.clone());

        // First get a baseline with perfect conditions
        let good_packet = create_test_packet(30.0, 500); // Perfect FPS
        simulate_time_passing(&mut controller, 100.0);
        let good_bitrate = match controller.process_diagnostics_packet(good_packet) {
            Some(bitrate) => bitrate,
            None => panic!("Failed to get initial bitrate"),
        };

        // Now simulate a significant drop in FPS
        for _ in 0..5 {
            // Feed multiple poor FPS packets to build up effect
            let bad_packet = create_test_packet(5.0, 500); // Very low FPS
            simulate_time_passing(&mut controller, 100.0);
            controller.process_diagnostics_packet(bad_packet);
        }

        // One more poor FPS packet and get the resulting bitrate
        let final_packet = create_test_packet(5.0, 500);
        simulate_time_passing(&mut controller, 100.0);
        let poor_bitrate = match controller.process_diagnostics_packet(final_packet) {
            Some(bitrate) => bitrate,
            None => panic!("Failed to get final bitrate"),
        };

        // Verify that bitrate decreased when FPS decreased
        assert!(
            poor_bitrate < good_bitrate,
            "Expected bitrate to decrease when FPS drops. Good: {}, Poor: {}",
            good_bitrate,
            poor_bitrate
        );
    }
}
