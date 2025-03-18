use futures::channel::mpsc::{self, Receiver, Sender};
use futures::StreamExt;
use js_sys::Date;
use log::{debug, error};
use std::error::Error;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};
use videocall_types::protos::media_packet::media_packet::MediaType;
use wasm_bindgen::JsValue;
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
}

// Stats for a peer's decoder
#[derive(Debug, Clone)]
pub struct DecoderStats {
    pub peer_id: String,
    pub frames_decoded: u32,
    pub frames_dropped: u32,
    pub fps: f64,
    pub media_type: MediaType,
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
    _media_type: MediaType,
}

impl FpsTracker {
    fn new(media_type: MediaType) -> Self {
        Self {
            frames_count: 0,
            fps: 0.0,
            last_fps_update: Date::now(),
            total_frames: 0,
            _media_type: media_type,
        }
    }

    fn track_frame(&mut self) -> f64 {
        self.frames_count += 1;
        self.total_frames += 1;
        let now = Date::now();
        let elapsed_ms = now - self.last_fps_update;

        // Update FPS calculation every second
        if elapsed_ms >= 1000.0 {
            self.fps = (self.frames_count as f64 * 1000.0) / elapsed_ms;
            self.frames_count = 0;
            self.last_fps_update = now;
        }

        self.fps
    }
}

// The DiagnosticManager manages the collection and reporting of diagnostic information
pub struct DiagnosticManager {
    sender: Sender<DiagnosticEvent>,
    frames_decoded: Arc<AtomicU32>,
    frames_dropped: Arc<AtomicU32>,
    report_interval_ms: u64,
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
            last_report_time: Date::now(),
            report_interval_ms: 1000,
            receiver,
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        Self {
            sender,
            frames_decoded: Arc::new(AtomicU32::new(0)),
            frames_dropped: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 1000, // Default to 1 second updates
        }
    }

    // Set the callback for UI updates
    pub fn set_stats_callback(&self, callback: Callback<String>) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::SetStatsCallback(callback))
        {
            error!("Failed to set stats callback - {}", e);
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
            // Successfully set the interval
            error!("Failed to set reporting interval - {}", e);
        }
    }

    // Track a frame received from a peer for a specific media type
    pub fn track_frame(&self, peer_id: &str, media_type: MediaType) -> f64 {
        // Increment total frames decoded
        self.frames_decoded.fetch_add(1, Ordering::SeqCst);

        // Send the frame event to the worker
        if self
            .sender
            .clone()
            .try_send(DiagnosticEvent::FrameReceived {
                peer_id: peer_id.to_string(),
                media_type,
            })
            .is_ok()
        {
            // Frame event sent successfully
        } else {
            debug!("Failed to send frame event - channel full or closed");
        }

        // We don't know the actual FPS here, but we'll request stats
        if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::RequestStats) {
            debug!(
                "Failed to send request stats - channel full or closed: {}",
                e
            );
        }

        0.0 // Return a default value since we can't get real-time FPS anymore
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

impl DiagnosticWorker {
    async fn run(mut self) {
        while let Some(event) = self.receiver.next().await {
            self.handle_event(event);
            self.maybe_report_stats_to_ui();
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

        for (peer_id, media_stats) in stats.iter() {
            result.push_str(&format!("Peer {}: ", peer_id));
            for (media_type, fps) in media_stats.iter() {
                let media_str = match media_type {
                    MediaType::VIDEO => "Video",
                    MediaType::AUDIO => "Audio",
                    MediaType::SCREEN => "Screen",
                    MediaType::HEARTBEAT => "Heartbeat",
                };
                result.push_str(&format!("{}={:.2} FPS ", media_str, fps));
            }
            result.push('\n');
        }

        result
    }
}
