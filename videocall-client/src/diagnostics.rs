use futures::channel::mpsc::{self, Receiver, Sender};
use futures::StreamExt;
use js_sys::Date;
use log::{debug, error};
use std::{
    collections::HashMap,
    error::Error,
    rc::Rc,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use videocall_types::protos::diagnostics_packet::{
    self as diag, AudioMetrics, DiagnosticsPacket, VideoMetrics,
};
use videocall_types::protos::media_packet::media_packet::MediaType;
use wasm_bindgen::prelude::*;
use web_sys::{console, window};
use yew::Callback;

// Basic structure for diagnostics events
#[derive(Debug, Clone)]
pub enum DiagnosticEvent {
    FrameReceived {
        peer_id: String,
        media_type: MediaType,
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
    last_frame_time: f64, // Add timestamp of last received frame
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
        }
    }

    fn track_frame(&mut self) -> f64 {
        self.frames_count += 1;
        self.total_frames += 1;
        let now = Date::now();
        self.last_frame_time = now; // Record when we received the frame
        let elapsed_ms = now - self.last_fps_update;

        // Update FPS calculation every second
        if elapsed_ms >= 1000.0 {
            self.fps = (self.frames_count as f64 * 1000.0) / elapsed_ms;
            self.frames_count = 0;
            self.last_fps_update = now;
        }

        self.fps
    }

    // Check if no frames have been received for a while and reset FPS if needed
    fn check_inactive(&mut self, now: f64) {
        let inactive_ms = now - self.last_frame_time;

        // If no frames for more than 1 second, consider the feed inactive
        if inactive_ms > 1000.0 {
            // Set FPS to zero immediately when inactive
            if self.fps > 0.0 {
                console::log_1(
                    &format!(
                        "Detected inactive stream, setting FPS to 0 (inactive for {:.0}ms)",
                        inactive_ms
                    )
                    .into(),
                );
                self.fps = 0.0;
                self.frames_count = 0;
                self.last_fps_update = now;
            }
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
            console::log_1(&"Cleaning up diagnostics heartbeat interval".into());
            window.clear_interval_with_handle(self.interval_id);
        }
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

impl Default for DiagnosticManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagnosticManager {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::channel(100); // Buffer size of 100 messages

        // Spawn the worker to process events
        let worker = DiagnosticWorker {
            fps_trackers: HashMap::new(),
            on_stats_update: None,
            packet_handler: None,
            last_report_time: Date::now(),
            report_interval_ms: 500,
            receiver,
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        let mut manager = Self {
            sender: sender.clone(),
            frames_decoded: Arc::new(AtomicU32::new(0)),
            frames_dropped: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 500,
            timer: None,
        };

        // Set up heartbeat timer to ensure diagnostics run even when no frames are coming in
        manager.setup_heartbeat(sender);

        manager
    }

    // Start a JavaScript interval timer that sends heartbeat events
    fn setup_heartbeat(&mut self, sender: Sender<DiagnosticEvent>) {
        let window = window().expect("Failed to get window");
        let sender_clone = sender.clone();

        // Create a closure that sends a heartbeat event through the channel
        let callback = Closure::wrap(Box::new(move || {
            if let Err(e) = sender_clone
                .clone()
                .try_send(DiagnosticEvent::HeartbeatTick)
            {
                console::log_1(&format!("Failed to send heartbeat: {:?}", e).into());
            }
        }) as Box<dyn FnMut()>);

        // Set up the interval to run every 500ms
        let interval_id = window
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
    pub fn track_frame(&self, peer_id: &str, media_type: MediaType) -> f64 {
        self.frames_decoded.fetch_add(1, Ordering::SeqCst);

        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::FrameReceived {
                peer_id: peer_id.to_string(),
                media_type,
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
            } => {
                let peer_trackers = self.fps_trackers.entry(peer_id.clone()).or_default();

                let tracker = peer_trackers
                    .entry(media_type)
                    .or_insert_with(|| FpsTracker::new(media_type));

                tracker.track_frame();
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
            // Create separate packets for audio and video
            for (media_type, tracker) in peer_trackers {
                // Skip if we don't have a packet handler
                if self.packet_handler.is_none() {
                    continue;
                }

                // Create a new diagnostics packet
                let mut packet = DiagnosticsPacket::new();
                packet.sender_id = window().unwrap().location().hostname().unwrap_or_default();
                packet.target_id = peer_id.clone();
                packet.timestamp_ms = timestamp_ms;

                // Convert MediaType from our enum to the proto enum
                let proto_media_type = match media_type {
                    MediaType::VIDEO => diag::diagnostics_packet::MediaType::VIDEO,
                    MediaType::SCREEN => diag::diagnostics_packet::MediaType::SCREEN,
                    MediaType::AUDIO => diag::diagnostics_packet::MediaType::AUDIO,
                    _ => continue, // Skip unknown media types
                };
                packet.media_type = proto_media_type.into();

                // Set metrics based on media type
                if *media_type == MediaType::AUDIO {
                    let mut audio_metrics = AudioMetrics::new();
                    audio_metrics.fps_received = tracker.fps as f32;

                    // audio_metrics.set_sample_rate(48000); // Default sample rate
                    // packet.set_audio_metrics(audio_metrics);
                    packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);
                } else {
                    // For video and screen
                    let mut video_metrics = VideoMetrics::new();
                    video_metrics.fps_received = tracker.fps as f32;
                    // video_metrics.set_frames_decoded(tracker.total_frames);
                    packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
                }

                // Send the packet
                if let Some(handler) = &self.packet_handler {
                    debug!(
                        "Sending diagnostic packet to {}: {:?} FPS: {:.2}",
                        peer_id, media_type, tracker.fps
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
    fn get_all_fps_stats(&self) -> HashMap<String, HashMap<MediaType, f64>> {
        let mut result = HashMap::new();

        for (peer_id, peer_trackers) in &self.fps_trackers {
            let mut media_fps = HashMap::new();
            for (media_type, tracker) in peer_trackers {
                media_fps.insert(*media_type, tracker.fps);
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
            // Create and send diagnostic packets for each media type
            for (media_type, fps) in media_stats.iter() {
                let mut packet = DiagnosticsPacket::new();
                packet.sender_id = peer_id.clone();
                packet.timestamp_ms = now as u64;

                // Convert MediaType to diagnostics packet MediaType
                packet.media_type = match media_type {
                    MediaType::VIDEO => diag::diagnostics_packet::MediaType::VIDEO,
                    MediaType::AUDIO => diag::diagnostics_packet::MediaType::AUDIO,
                    MediaType::SCREEN => diag::diagnostics_packet::MediaType::SCREEN,
                    _ => continue, // Skip other types
                }
                .into();

                match media_type {
                    MediaType::VIDEO | MediaType::SCREEN => {
                        let mut video_metrics = VideoMetrics::new();
                        video_metrics.fps_received = *fps as f32;
                        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
                    }
                    MediaType::AUDIO => {
                        let mut audio_metrics = AudioMetrics::new();
                        audio_metrics.fps_received = *fps as f32;
                        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);
                    }
                    _ => {}
                }

                // TODO: Send packet through WebRTC data channel
            }

            // Continue with existing string formatting
            result.push_str(&format!("Peer {}: ", peer_id));
            for (media_type, fps) in media_stats.iter() {
                let media_str = match media_type {
                    MediaType::VIDEO => "Video",
                    MediaType::AUDIO => "Audio",
                    MediaType::SCREEN => "Screen",
                    MediaType::HEARTBEAT => "Heartbeat",
                };

                if *fps <= 0.01 {
                    result.push_str(&format!("{}=INACTIVE ", media_str));
                } else {
                    result.push_str(&format!("{}={:.2} FPS ", media_str, fps));
                }
            }
            result.push('\n');
        }

        if stats.is_empty() {
            result.push_str("No active peers.\n");
        }

        result
    }
}
