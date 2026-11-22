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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use futures::channel::mpsc::{self, Receiver, Sender};
use futures::StreamExt;
use js_sys::Date;
use log::{debug, error};
use videocall_types::Callback;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::window;

use videocall_types::protos::diagnostics_packet::{AudioMetrics, DiagnosticsPacket, VideoMetrics};

use super::frame_rate::{FrameEvent, FrameRate, FrameRateMeter};
use crate::adaptive_quality_constants::DIAGNOSTICS_REPORT_INTERVAL_MS;
use crate::rate_control::viewer::{ReceivedVideo, VideoArrival};
use crate::rate_control::wire::write_report;
use crate::utils::monotonic_now_ms;
use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_rate_control::Report;
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
    /// A backlog just started on `peer_id`'s camera stream: send its report
    /// now instead of waiting for the next tick.
    EarlyReport {
        peer_id: String,
        report: Report,
    },
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

/// Frames received from one peer for one media type, and what to report
/// about them.
#[derive(Debug)]
pub(crate) struct FpsTracker {
    meter: FrameRateMeter,
    total_frames: u32,
    #[allow(dead_code)]
    media_type: MediaType,
    bytes_received: u64,      // Bytes in the current measurement window
    last_bitrate_update: f64, // Start of the current measurement window
    current_bitrate: f64,     // Current bitrate in kbits/sec
}

impl FpsTracker {
    fn new(media_type: MediaType) -> Self {
        Self::new_at(media_type, Date::now())
    }

    pub(crate) fn new_at(media_type: MediaType, now: f64) -> Self {
        Self {
            meter: FrameRateMeter::default(),
            total_frames: 0,
            media_type,
            bytes_received: 0,
            last_bitrate_update: now,
            current_bitrate: 0.0,
        }
    }

    fn track_frame_with_size(&mut self, bytes: u64) {
        self.track_frame_at(bytes, Date::now())
    }

    /// Record one frame of `bytes` received at `now`. The bitrate is measured
    /// over the same windows as the frame rate, so a pause restarts both.
    pub(crate) fn track_frame_at(&mut self, bytes: u64, now: f64) {
        self.total_frames += 1;
        match self.meter.on_frame(now) {
            FrameEvent::Restarted => {
                self.bytes_received = 0;
                self.last_bitrate_update = now;
            }
            FrameEvent::Counted => self.bytes_received += bytes,
            FrameEvent::WindowClosed { elapsed_ms, .. } => {
                self.bytes_received += bytes;
                // bits per ms = kbit/s
                self.current_bitrate = (self.bytes_received * 8) as f64 / elapsed_ms;
                self.bytes_received = 0;
                self.last_bitrate_update = now;
            }
        }
    }

    /// The fps and bitrate (kbit/s) to report at `now`, or `None` while the
    /// first window after frames (re)started is still open: a viewer that
    /// just joined or resumed has not measured anything yet. No frame for
    /// more than a second reports 0: the viewer cannot tell an idle source
    /// from an outage, so it says what it sees and the sender, which knows
    /// whether it sent anything, decides.
    pub(crate) fn report_at(&self, now: f64) -> Option<(f64, f64)> {
        match self.meter.rate_at(now) {
            FrameRate::Silent => Some((0.0, 0.0)),
            FrameRate::Measuring => None,
            FrameRate::Rate(fps) => Some((fps, self.current_bitrate)),
        }
    }

    fn get_metrics(&self) -> (f64, f64) {
        self.report_at(Date::now()).unwrap_or((0.0, 0.0))
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
    /// Rate-control estimators for the camera streams we receive. Fed
    /// synchronously as packets arrive, read by the worker on every tick.
    received_video: Rc<RefCell<ReceivedVideo>>,
    /// The worker's per-peer trackers, shared so a departed peer is
    /// forgotten at once (an event on the bounded channel could be dropped,
    /// and the peer would be reported on forever).
    fps_trackers: FpsTrackers,
}

/// Frame-rate trackers per peer and media type.
type FpsTrackers = Rc<RefCell<HashMap<String, HashMap<MediaType, FpsTracker>>>>;

unsafe impl Sync for DiagnosticManager {}
unsafe impl Send for DiagnosticManager {}

// Internal worker that processes diagnostic events
struct DiagnosticWorker {
    // Track FPS per peer and per media type (audio, video, screen)
    fps_trackers: FpsTrackers,
    on_stats_update: Option<Callback<String>>,
    last_report_time: f64, // timestamp in ms
    report_interval_ms: u64,
    packet_handler: Option<Callback<DiagnosticsPacket>>,
    receiver: Receiver<DiagnosticEvent>,
    userid: String,
    received_video: Rc<RefCell<ReceivedVideo>>,
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
        let received_video = Rc::new(RefCell::new(ReceivedVideo::default()));
        let fps_trackers = FpsTrackers::default();

        // Spawn the worker to process events
        let worker = DiagnosticWorker {
            fps_trackers: fps_trackers.clone(),
            on_stats_update: None,
            packet_handler: None,
            last_report_time: Date::now(),
            report_interval_ms: 500,
            receiver,
            userid,
            received_video: received_video.clone(),
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        let mut manager = Self {
            sender: sender.clone(),
            frames_decoded: Arc::new(AtomicU32::new(0)),
            frames_dropped: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 500,
            timer: None,
            received_video,
            fps_trackers,
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

        // One report per stream every DIAGNOSTICS_REPORT_INTERVAL_MS.
        let interval_id = window()
            .expect("Failed to get window")
            .set_interval_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                DIAGNOSTICS_REPORT_INTERVAL_MS as i32,
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

    /// A camera frame from `peer_id` reached the client at `arrival_ms`
    /// (monotonic), before any decode. Feeds that stream's rate-control
    /// estimator; a backlog that just started is reported at once.
    pub(crate) fn on_video_arrival(&self, peer_id: &str, arrival: VideoArrival, arrival_ms: f64) {
        let early = self
            .received_video
            .borrow_mut()
            .on_frame(peer_id, arrival, arrival_ms);
        if let Some(report) = early {
            if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::EarlyReport {
                peer_id: peer_id.to_string(),
                report,
            }) {
                error!("Failed to queue early rate-control report: {e}");
            }
        }
    }

    /// The peer left: forget its estimator and its trackers at once, so it
    /// is not reported on again.
    pub fn remove_peer(&self, peer_id: &str) {
        forget_peer(&self.fps_trackers, &self.received_video, peer_id);
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
                let mut fps_trackers = self.fps_trackers.borrow_mut();
                let peer_trackers = fps_trackers.entry(peer_id).or_default();

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
            DiagnosticEvent::EarlyReport { peer_id, report } => {
                self.send_early_report(&peer_id, report, Date::now());
            }
        }
    }

    fn send_diagnostic_packets(&self) {
        let now = Date::now();
        let monotonic_now = monotonic_now_ms();

        // The handler runs after the trackers are released: it sends on the
        // transport and must not find them borrowed.
        let mut packets = Vec::new();
        let fps_trackers = self.fps_trackers.borrow();
        for (peer_id, peer_trackers) in fps_trackers.iter() {
            for (media_type, tracker) in peer_trackers {
                let Some((fps, bitrate_kbps)) = tracker.report_at(now) else {
                    continue;
                };
                // Only the camera stream is rate controlled.
                let rate_report = if *media_type == MediaType::VIDEO {
                    self.received_video
                        .borrow_mut()
                        .report(peer_id, monotonic_now)
                } else {
                    None
                };
                debug!(
                    "Broadcasting decoder event for peer {} ({:?}): FPS={:.2}, Bitrate={:.1}kbps",
                    peer_id, media_type, fps, bitrate_kbps
                );
                for event in reception_events(
                    &self.userid,
                    peer_id,
                    *media_type,
                    fps,
                    bitrate_kbps,
                    rate_report,
                ) {
                    let _ = global_sender().try_broadcast(event);
                }

                // Only create and send protobuf packets if packet handler is set (legacy system)
                if self.packet_handler.is_some() {
                    debug!(
                        "Sending diagnostic packet to {}: {:?} FPS: {:.2} Bitrate: {:.1} kbit/s",
                        peer_id, media_type, fps, bitrate_kbps
                    );
                    packets.push(diagnostics_packet(
                        &self.userid,
                        peer_id,
                        *media_type,
                        fps,
                        bitrate_kbps,
                        rate_report,
                        now as u64,
                    ));
                }
            }
        }
        drop(fps_trackers);
        if let Some(handler) = &self.packet_handler {
            for packet in packets {
                handler.emit(packet);
            }
        }
    }

    /// One camera-stream report about `peer_id`, sent outside the tick, with
    /// the stream's current fps and bitrate. Skipped while those are still
    /// being measured: an fps of 0 would read as an outage to an older
    /// publisher, and the next tick carries the report anyway.
    fn send_early_report(&self, peer_id: &str, report: Report, now: f64) {
        let Some(handler) = &self.packet_handler else {
            return;
        };
        let Some((fps, bitrate_kbps)) = self
            .fps_trackers
            .borrow()
            .get(peer_id)
            .and_then(|trackers| trackers.get(&MediaType::VIDEO))
            .and_then(|tracker| tracker.report_at(now))
        else {
            return;
        };
        debug!(
            "Sending early rate-control report to {peer_id}: {} kbps, {} ms backlog",
            report.capacity_kbps(),
            report.backlog_ms_rounded()
        );
        handler.emit(diagnostics_packet(
            &self.userid,
            peer_id,
            MediaType::VIDEO,
            fps,
            bitrate_kbps,
            Some(report),
            now as u64,
        ));
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
        for (peer_id, peer_trackers) in self.fps_trackers.borrow().iter() {
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
    fn new(peer_id: String, media_type: MediaType, now: f64) -> Self {
        StreamStats {
            _media_type: media_type,
            last_update: now,
            median_latency_ms: 0,
            jitter_ms: 0,
            estimated_bandwidth_kbps: 0,
            round_trip_time_ms: 0,
            _peer_id: peer_id,
        }
    }

    fn update_from_packet(&mut self, packet: &DiagnosticsPacket, media_type: MediaType, now: f64) {
        self.last_update = now;

        // A report without the metrics its media type implies counts as 0.
        let metrics = match media_type {
            MediaType::VIDEO | MediaType::SCREEN => {
                packet.video_metrics.as_ref().map(|m| m.bitrate_kbps)
            }
            MediaType::AUDIO => packet.audio_metrics.as_ref().map(|m| m.bitrate_kbps),
            _ => None,
        };
        self.estimated_bandwidth_kbps = metrics.unwrap_or(0);
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
    /// Our session id, shared with the worker; see [`Self::set_own_session_id`].
    own_session_id: Rc<Cell<Option<u64>>>,
}

struct SenderDiagnosticWorker {
    /// Our session id, once the server assigned it. Viewers name the stream
    /// a report is about by its owner's session id (`sender_id`).
    own_session_id: Rc<Cell<Option<u64>>>,
    stream_stats: HashMap<String, HashMap<MediaType, StreamStats>>, // peer_id -> media_type -> stats
    on_stats_update: Option<Callback<String>>,
    encoder_callbacks: Vec<Callback<DiagnosticsPacket>>,
    last_report_time: f64,
    report_interval_ms: u64,
    receiver: Receiver<SenderDiagnosticEvent>,
}

impl SenderDiagnosticManager {
    /// `_userid` is unused: reports name the stream they describe by its
    /// owner's session id, set with [`Self::set_own_session_id`]. The
    /// parameter stays so the public signature does not change.
    pub fn new(_userid: String) -> Self {
        let (sender, receiver) = mpsc::channel(100);

        let own_session_id = Rc::new(Cell::new(None));
        let worker = SenderDiagnosticWorker {
            own_session_id: own_session_id.clone(),
            stream_stats: HashMap::new(),
            on_stats_update: None,
            encoder_callbacks: Vec::new(),
            last_report_time: Date::now(),
            report_interval_ms: 500,
            receiver,
        };

        wasm_bindgen_futures::spawn_local(worker.run());

        let mut manager = Self {
            sender: sender.clone(),
            timer: None,
            _report_interval_ms: 500,
            own_session_id,
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
                DIAGNOSTICS_REPORT_INTERVAL_MS as i32,
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

    pub fn set_reporting_interval(&self, interval_ms: u64) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(SenderDiagnosticEvent::SetReportingInterval(interval_ms))
        {
            error!("Failed to set sender reporting interval: {e}");
        }
    }

    /// Record the session id the server assigned us. Diagnostics reports
    /// reach the whole room; only the ones about our own streams count in
    /// the sender stats.
    pub(crate) fn set_own_session_id(&self, session_id: u64) {
        self.own_session_id.set(Some(session_id));
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
                self.on_report(packet, Date::now());
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
        }
    }

    /// A viewer's report, received at `now`.
    fn on_report(&mut self, packet: DiagnosticsPacket, now: f64) {
        let sender_id = packet.sender_id.clone();
        let target_id = packet.target_id.clone();
        let media_type: MediaType = packet.media_type.enum_value_or_default();

        // Publish to global diagnostics broadcast system
        let event = DiagEvent {
            subsystem: "sender",
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

        // Viewers publish to the whole room, so this may be a report
        // about another participant's stream. `sender_id` is the
        // session id of the stream's owner (see
        // `DiagnosticManager::send_diagnostic_packets`).
        let about_our_stream = self
            .own_session_id
            .get()
            .is_some_and(|own| sender_id == own.to_string());
        if !about_our_stream {
            return;
        }

        let peer_stats = self.stream_stats.entry(target_id.clone()).or_default();
        let stats = peer_stats
            .entry(media_type)
            .or_insert_with(|| StreamStats::new(target_id, media_type, now));
        stats.update_from_packet(&packet, media_type, now);
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

        // One block per viewer reporting on our own streams.
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

/// The diagnostics packet `userid` (the viewer) sends about `peer_id`'s
/// `media_type` stream. `rate_report` goes on the camera stream's video
/// metrics; `None` writes "no report".
/// Forgets everything diagnostics kept about `peer_id`.
fn forget_peer(fps_trackers: &FpsTrackers, received_video: &RefCell<ReceivedVideo>, peer_id: &str) {
    fps_trackers.borrow_mut().remove(peer_id);
    received_video.borrow_mut().remove(peer_id);
}

fn diagnostics_packet(
    userid: &str,
    peer_id: &str,
    media_type: MediaType,
    fps: f64,
    bitrate_kbps: f64,
    rate_report: Option<Report>,
    timestamp_ms: u64,
) -> DiagnosticsPacket {
    let mut packet = DiagnosticsPacket::new();
    packet.target_id = userid.to_string();
    packet.sender_id = peer_id.to_string();
    packet.timestamp_ms = timestamp_ms;
    packet.media_type = media_type.into();
    if media_type == MediaType::AUDIO {
        let mut audio_metrics = AudioMetrics::new();
        audio_metrics.fps_received = fps as f32;
        audio_metrics.bitrate_kbps = bitrate_kbps as u32;
        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);
    } else {
        let mut video_metrics = VideoMetrics::new();
        video_metrics.fps_received = fps as f32;
        video_metrics.bitrate_kbps = bitrate_kbps as u32;
        write_report(&mut video_metrics, rate_report);
        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
    }
    packet
}

/// Events describing what we receive from one peer's stream: a "decoder"
/// event for every media type, and, for the camera stream only, the
/// normalized "video" event the health reporter turns into `can_see` and the
/// video stats. A silent screen share or audio stream says nothing about
/// whether the peer's video is visible.
fn reception_events(
    userid: &str,
    peer_id: &str,
    media_type: MediaType,
    fps: f64,
    bitrate_kbps: f64,
    rate_report: Option<Report>,
) -> Vec<DiagEvent> {
    let mut decoder_metrics = vec![
        metric!("fps", fps),
        metric!("bitrate_kbps", bitrate_kbps),
        metric!("media_type", format!("{:?}", media_type)),
        metric!("from_peer", userid.to_string()),
        metric!("to_peer", peer_id.to_string()),
    ];
    // What we report to this peer about its camera stream, for the
    // Reception Stats panel.
    if let Some(report) = rate_report {
        decoder_metrics.push(metric!("capacity_kbps", report.capacity_kbps() as u64));
        decoder_metrics.push(metric!("backlog_ms", report.backlog_ms_rounded() as u64));
    }
    let decoder_event = DiagEvent {
        subsystem: "decoder",
        stream_id: None,
        ts_ms: now_ms(),
        metrics: decoder_metrics,
    };
    if media_type != MediaType::VIDEO {
        return vec![decoder_event];
    }
    let video_event = DiagEvent {
        subsystem: "video",
        stream_id: None,
        ts_ms: now_ms(),
        metrics: vec![
            metric!("fps_received", fps),
            metric!("bitrate_kbps", bitrate_kbps),
            metric!("from_peer", userid.to_string()),
            metric!("to_peer", peer_id.to_string()),
        ],
    };
    vec![decoder_event, video_event]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report_about(stream_owner_session: u64, from_viewer: &str) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = stream_owner_session.to_string();
        packet.target_id = from_viewer.to_string();
        packet.media_type = MediaType::VIDEO.into();
        let mut metrics = VideoMetrics::new();
        metrics.fps_received = 2.0;
        packet.video_metrics = ::protobuf::MessageField::some(metrics);
        packet
    }

    fn sender_worker(own_session: Option<u64>) -> SenderDiagnosticWorker {
        let (_events_tx, events_rx) = mpsc::channel(1);
        SenderDiagnosticWorker {
            own_session_id: Rc::new(Cell::new(own_session)),
            stream_stats: HashMap::new(),
            on_stats_update: None,
            encoder_callbacks: Vec::new(),
            last_report_time: 0.0,
            report_interval_ms: 500,
            receiver: events_rx,
        }
    }

    fn receiver_worker() -> DiagnosticWorker {
        let (_events_tx, events_rx) = mpsc::channel(1);
        DiagnosticWorker {
            fps_trackers: FpsTrackers::default(),
            on_stats_update: None,
            last_report_time: 0.0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: events_rx,
            userid: "me".to_string(),
            received_video: Rc::new(RefCell::new(ReceivedVideo::default())),
        }
    }

    fn rate_report(capacity_kbps: u32, backlog_ms: u32) -> Report {
        Report::from_wire(capacity_kbps, backlog_ms)
    }

    /// Reports about our stream feed our sender stats; reports about another
    /// participant's stream do not.
    #[test]
    fn sender_stats_count_only_reports_about_our_stream() {
        let mut worker = sender_worker(Some(1));
        worker.on_report(report_about(1, "viewer-b"), 0.0);
        worker.on_report(report_about(3, "viewer-c"), 0.0);
        let mut viewers: Vec<_> = worker.stream_stats.keys().cloned().collect();
        viewers.sort();
        assert_eq!(viewers, vec!["viewer-b".to_string()]);
    }

    /// Only the camera stream speaks for "can I see this peer": a silent
    /// screen share or audio stream must not produce a "video" event.
    #[test]
    fn only_the_video_stream_produces_the_video_event() {
        let subsystems = |media_type| -> Vec<&'static str> {
            reception_events("me", "42", media_type, 0.0, 0.0, None)
                .iter()
                .map(|event| event.subsystem)
                .collect()
        };
        assert_eq!(subsystems(MediaType::VIDEO), vec!["decoder", "video"]);
        assert_eq!(subsystems(MediaType::SCREEN), vec!["decoder"]);
        assert_eq!(subsystems(MediaType::AUDIO), vec!["decoder"]);
    }

    /// A report about our stream that lacks its metrics must not take down
    /// the sender worker (it used to `unwrap()` them).
    #[test]
    fn report_without_metrics_counts_as_zero_bitrate() {
        let mut worker = sender_worker(Some(1));
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = "1".to_string();
        packet.target_id = "viewer-b".to_string();
        packet.media_type = MediaType::VIDEO.into();
        worker.on_report(packet, 0.0);
        let stats = &worker.stream_stats["viewer-b"][&MediaType::VIDEO];
        assert_eq!(stats.estimated_bandwidth_kbps, 0);
    }

    /// The camera stream's report rides on its video metrics next to fps
    /// and bitrate; no report is the proto3 zero.
    #[test]
    fn the_camera_report_rides_on_the_video_metrics() {
        let packet = diagnostics_packet(
            "me",
            "42",
            MediaType::VIDEO,
            29.5,
            812.0,
            Some(rate_report(700, 35)),
            1_000,
        );
        assert_eq!(
            (packet.target_id.as_str(), packet.sender_id.as_str()),
            ("me", "42")
        );
        let metrics = packet.video_metrics.as_ref().expect("video metrics");
        assert_eq!((metrics.fps_received, metrics.bitrate_kbps), (29.5, 812));
        assert_eq!((metrics.capacity_kbps, metrics.backlog_ms), (700, 35));

        let none = diagnostics_packet("me", "42", MediaType::VIDEO, 29.5, 812.0, None, 1_000);
        let metrics = none.video_metrics.as_ref().expect("video metrics");
        assert_eq!((metrics.capacity_kbps, metrics.backlog_ms), (0, 0));

        let audio = diagnostics_packet("me", "42", MediaType::AUDIO, 50.0, 40.0, None, 1_000);
        assert!(audio.video_metrics.is_none());
        assert!(audio.audio_metrics.is_some());
    }

    /// The Reception Stats panel shows what we report about a camera stream.
    #[test]
    fn the_decoder_event_carries_our_report_when_there_is_one() {
        let names = |report| -> Vec<&'static str> {
            reception_events("me", "42", MediaType::VIDEO, 30.0, 800.0, report)[0]
                .metrics
                .iter()
                .map(|metric| metric.name)
                .collect()
        };
        let with = names(Some(rate_report(700, 35)));
        assert!(with.contains(&"capacity_kbps") && with.contains(&"backlog_ms"));
        let without = names(None);
        assert!(!without.contains(&"capacity_kbps") && !without.contains(&"backlog_ms"));
    }

    /// A tracker that measured a 30 fps stream between 0 and 2 s.
    fn measured_video_tracker() -> FpsTracker {
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, 0.0);
        for frame in 0..60 {
            tracker.track_frame_at(3_000, f64::from(frame) * 1000.0 / 30.0);
        }
        tracker
    }

    /// A departed peer is forgotten at once, by the caller, with no event
    /// that could be dropped on the way.
    #[test]
    fn a_departed_peer_is_no_longer_reported() {
        let worker = receiver_worker();
        for peer in ["42", "43"] {
            worker
                .fps_trackers
                .borrow_mut()
                .entry(peer.to_string())
                .or_default()
                .insert(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, 0.0));
        }
        forget_peer(&worker.fps_trackers, &worker.received_video, "42");
        let peers: Vec<_> = worker.fps_trackers.borrow().keys().cloned().collect();
        assert_eq!(peers, vec!["43".to_string()]);
    }

    /// An early report goes out at once with the stream's fps, bitrate and
    /// the report; not while the stream is still being measured, and not
    /// about a peer we have no tracker for.
    #[test]
    fn an_early_report_goes_out_at_once() {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let mut worker = receiver_worker();
        let log = sent.clone();
        worker.packet_handler = Some(Callback::from(move |packet: DiagnosticsPacket| {
            log.borrow_mut().push(packet)
        }));
        worker
            .fps_trackers
            .borrow_mut()
            .entry("42".to_string())
            .or_default()
            .insert(MediaType::VIDEO, measured_video_tracker());
        worker
            .fps_trackers
            .borrow_mut()
            .entry("43".to_string())
            .or_default()
            .insert(MediaType::VIDEO, {
                // One frame in: still measuring.
                let mut tracker = FpsTracker::new_at(MediaType::VIDEO, 1_900.0);
                tracker.track_frame_at(3_000, 1_990.0);
                tracker
            });

        worker.send_early_report("42", rate_report(450, 120), 2_000.0);
        worker.send_early_report("43", rate_report(450, 120), 2_000.0);
        worker.send_early_report("44", rate_report(450, 120), 2_000.0);

        let sent = sent.borrow();
        assert_eq!(sent.len(), 1, "only the measured stream");
        let packet = &sent[0];
        assert_eq!(packet.sender_id, "42");
        let metrics = packet.video_metrics.as_ref().expect("video metrics");
        assert_eq!((metrics.capacity_kbps, metrics.backlog_ms), (450, 120));
        assert!(metrics.fps_received > 25.0);
    }
}
