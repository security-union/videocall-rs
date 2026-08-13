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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use futures::channel::mpsc::{self, Receiver, Sender};
use futures::StreamExt;
use js_sys::Date;
use log::{error, trace};
use videocall_types::Callback;
use wasm_bindgen::JsValue;

use videocall_types::protos::diagnostics_packet::{AudioMetrics, DiagnosticsPacket, VideoMetrics};

use videocall_diagnostics::{global_sender, metric, now_ms, DiagEvent};
use videocall_types::protos::media_packet::media_packet::MediaType;

use super::heartbeat::HeartbeatTimer;

/// Heartbeat cadence used by both [`DiagnosticManager`] and
/// [`SenderDiagnosticManager`]. 500ms drives the AQ feedback loop and
/// per-peer health reporting; do not change without updating the AQ-side
/// step-down thresholds that assume ~2 ticks/sec.
const HEARTBEAT_PERIOD_MS: u32 = 500;

// Basic structure for diagnostics events
#[derive(Debug, Clone)]
pub enum DiagnosticEvent {
    FrameReceived {
        peer_id: String,
        media_type: MediaType,
        frame_size: u64, // Size of the frame in bytes
        /// Whether this packet was actually DECODED, or merely arrived (issue #2190).
        ///
        /// `false` for every `DecodeStatus::SKIPPED` path in `Peer::decode` — most
        /// importantly the EXACT-MATCH simulcast guard, which drops any packet whose
        /// `simulcast_layer_id` is not this receiver's selected rung. The relay fails
        /// OPEN and forwards ALL of a publisher's rungs to a healthy receiver, so
        /// those wrong-rung packets DO arrive here; counting them as frames made
        /// `fps_received` read the LADDER SUM (an 8fps 3-rung camera published
        /// 7+15+30 = ~52 fps) instead of the rung actually decoded.
        ///
        /// Bytes are still accumulated regardless (see
        /// [`FpsTracker::track_frame_with_size`]) because they genuinely crossed this
        /// receiver's downlink — so `bitrate_kbps` keeps measuring real consumption
        /// while `fps_received` measures real decode. The two are gated on SEPARATE
        /// freshness clocks so that divergence is representable: bitrate live with
        /// fps at 0 is the receiving-but-not-decoding signature (wrong-rung, or a
        /// hidden/off-budget tile).
        decoded: bool,
    },
    DecodeError {
        peer_id: String,
        media_type: MediaType,
    },
    RemovePeer {
        peer_id: String,
    },
    RequestStats,
    SetStatsCallback(Callback<String>),
    SetReportingInterval(u64),
    HeartbeatTick, // New event for heartbeat
    SetPacketHandler(Callback<DiagnosticsPacket>),
    /// Clear the packet handler. Used at session teardown to break the
    /// `client -> diagnostics.packet_handler -> client` `Rc` cycle so the
    /// underlying `Inner` can drop after a meeting page unmount.
    ClearPacketHandler,
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
    /// Timestamp of the last DECODED frame (issue #2190). Drives the fps freshness gate:
    /// a stream that decodes nothing must report fps 0 rather than a stale cached value.
    last_frame_time: f64,
    /// Timestamp of the last ARRIVING packet, decoded or skipped (issue #2190).
    ///
    /// Deliberately SEPARATE from `last_frame_time`: bytes are billed on arrival, so the
    /// bitrate readout must stay live for a stream that is receiving but not decoding —
    /// a hidden/off-budget tile, or a receiver pinned to a rung the publisher stopped
    /// producing. Gating bitrate on decoded-frame freshness would report 0 kbps while the
    /// downlink is genuinely carrying traffic, hiding real bandwidth consumption at
    /// exactly the moment an operator is trying to account for it.
    last_packet_time: f64,
    bytes_received: u64,        // Track total bytes received
    last_bitrate_update: f64,   // Last time we calculated bitrate
    current_bitrate: f64,       // Current bitrate in kbits/sec
    decode_errors_count: u32,   // Windowed counter (resets every 1s)
    decode_errors_per_sec: f64, // Decode errors per second
    total_decode_errors: u64,   // Cumulative decode error counter (never resets)
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
            last_packet_time: now,
            bytes_received: 0,
            last_bitrate_update: now,
            current_bitrate: 0.0,
            decode_errors_count: 0,
            decode_errors_per_sec: 0.0,
            total_decode_errors: 0,
        }
    }

    /// Accumulate one arriving packet.
    ///
    /// `decoded` (issue #2190) separates the two things this tracker measures:
    ///   * FRAME counters (`frames_count`/`total_frames` → `fps`) advance only when
    ///     the packet was actually decoded, so `fps` is this receiver's real decode
    ///     cadence rather than the sum of every simulcast rung the relay forwarded.
    ///   * BYTE accumulation (→ `current_bitrate`) advances unconditionally, because
    ///     a skipped packet still consumed downlink bandwidth.
    ///
    /// The two freshness clocks are likewise separate, and each gates only its own
    /// readout (see [`FpsTracker::get_metrics`] and `send_diagnostic_packets`):
    ///   * `last_frame_time` follows the FRAME counter. Its contract is "is this stream
    ///     still producing video?" A stream whose every packet is skipped produces none,
    ///     so it must be allowed to go inactive — otherwise wrong-rung arrivals would
    ///     hold the gate open and keep republishing a stale fps.
    ///   * `last_packet_time` follows ARRIVALS. Bitrate must NOT be gated on decoded
    ///     frames: a receiving-but-not-decoding stream (hidden tile, or a rung the
    ///     publisher stopped producing) is still consuming downlink, and reporting
    ///     0 kbps for it would hide real bandwidth use.
    ///
    /// That separation is what makes "bitrate live while fps reads 0" a usable
    /// receiving-but-not-decoding signal instead of an unreachable state.
    fn track_frame_with_size(&mut self, bytes: u64, decoded: bool) -> (f64, f64) {
        let now = Date::now();
        if decoded {
            self.frames_count += 1;
            self.total_frames += 1;
            self.last_frame_time = now; // Record when we received the frame
        }
        // Every arrival refreshes the PACKET clock, decoded or not — it is what keeps the
        // bitrate readout live for a stream that receives without decoding.
        self.last_packet_time = now;

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

            // Calculate decode errors per second
            self.decode_errors_per_sec = (self.decode_errors_count as f64 * 1000.0) / elapsed_ms;
            self.decode_errors_count = 0;

            // Reset counters
            self.bytes_received = 0;
            self.last_fps_update = now;
            self.last_bitrate_update = now;
        }

        (self.fps, self.current_bitrate)
    }

    fn track_decode_error(&mut self) {
        self.decode_errors_count += 1;
        self.total_decode_errors += 1;
    }

    /// Current `(fps, bitrate_kbps, decode_errors_per_sec)`, each zeroed when ITS OWN source
    /// has been idle for over a second (issue #2190). The single definition of the
    /// staleness rule.
    ///
    /// The clocks are separate on purpose:
    ///   * fps keys off the last DECODED frame, so a stream that decodes nothing reports 0
    ///     rather than a stale rate.
    ///   * bitrate AND decode_errors key off the last ARRIVING packet, so a
    ///     receiving-but-not-decoding stream (an off-budget tile, or a rung the publisher
    ///     stopped producing) still reports its real downlink use instead of a misleading
    ///     0 kbps. Before the split, one decoded-frame clock zeroed BOTH and hid live
    ///     bandwidth.
    ///
    /// `decode_errors` sits on the ARRIVAL clock deliberately (review follow-up):
    /// `track_decode_error` fires from `Peer::decode`'s `Err` arm — a packet that ARRIVED and
    /// then failed to decode — so it is an arrival-keyed event. Pairing it with the decode
    /// clock would report 0 errors for exactly the stream that is arriving and erroring on
    /// every packet, which is the case an operator most needs to see.
    ///
    /// BOTH readout sites call this — `get_metrics` for the UI string and
    /// `send_diagnostic_packets` for the "video" DiagEvent that becomes
    /// `videocall_video_fps` / `videocall_video_bitrate_kbps`. That matters: the rule used to
    /// be DUPLICATED in `send_diagnostic_packets`, and since `set_stats_callback` has no
    /// caller anywhere in the repo, `get_metrics` reaches no production consumer at all — so
    /// tests asserting through it left the only shipped path revertible-green (measured, and
    /// caught in review). One method, two call sites, so the tests cover production.
    ///
    /// `now` is passed in rather than sampled here so the caller can reuse one `Date::now()`
    /// across a whole heartbeat sweep instead of crossing the JS boundary per tracker.
    fn gated_metrics(&self, now: f64) -> (f64, f64, f64) {
        let decode_idle = now - self.last_frame_time > 1000.0;
        let packet_idle = now - self.last_packet_time > 1000.0;
        let fps = if decode_idle { 0.0 } else { self.fps };
        let (bitrate, decode_errors) = if packet_idle {
            (0.0, 0.0)
        } else {
            (self.current_bitrate, self.decode_errors_per_sec)
        };
        (fps, bitrate, decode_errors)
    }

    /// `(fps, bitrate_kbps)` for the UI stats string. Thin wrapper over
    /// [`FpsTracker::gated_metrics`] — see it for the two-clock rationale.
    fn get_metrics(&self) -> (f64, f64) {
        let (fps, bitrate, _) = self.gated_metrics(Date::now());
        (fps, bitrate)
    }
}

// The DiagnosticManager manages the collection and reporting of diagnostic information
pub struct DiagnosticManager {
    sender: Sender<DiagnosticEvent>,
    frames_decoded: Arc<AtomicU32>,
    report_interval_ms: u64,
    /// Issue #2190 test seam: every `(media_type, frame_size, decoded)` triple passed
    /// to [`DiagnosticManager::track_frame`], in call order.
    ///
    /// Recorded SYNCHRONOUSLY inside `track_frame`, before the `try_send`, on purpose:
    /// the `DiagnosticWorker` that owns `fps_trackers` runs on a `spawn_local` task, so
    /// asserting on tracker state would require yielding to the executor and would make
    /// the test a race. This seam observes what the CALL SITE decided, which is exactly
    /// the thing the fix changes.
    #[cfg(test)]
    track_frame_calls: std::cell::RefCell<Vec<(MediaType, u64, bool)>>,
    /// Drives the periodic `HeartbeatTick` event.
    ///
    /// Backed by a Worker (when available) so that background-tab throttling
    /// on the main thread does not starve adaptive-quality feedback or
    /// diagnostics reporting. Dropping the manager terminates the worker: the
    /// inner `HeartbeatTimer`'s `Drop` impl handles teardown.
    timer: Option<HeartbeatTimer>,
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
            report_interval_ms: 500,
            timer: None,
            #[cfg(test)]
            track_frame_calls: std::cell::RefCell::new(Vec::new()),
        };

        manager.setup_heartbeat(sender);

        manager
    }

    // Start a worker-driven heartbeat timer that dispatches `HeartbeatTick`
    // events on the main thread. The Worker is immune to background-tab
    // throttling so the AQ feedback loop and diagnostics reporting keep
    // running at the configured cadence even when the tab is hidden.
    fn setup_heartbeat(&mut self, sender: Sender<DiagnosticEvent>) {
        let timer = HeartbeatTimer::start(HEARTBEAT_PERIOD_MS, move || {
            if let Err(e) = sender.clone().try_send(DiagnosticEvent::HeartbeatTick) {
                log::debug!("Failed to enqueue heartbeat event: {e:?}");
            }
        });
        self.timer = Some(timer);
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

    /// Clear the packet handler so the diagnostics manager no longer holds a
    /// reference to the [`VideoCallClient`](crate::VideoCallClient) clone
    /// passed to it. Called from `VideoCallClient::disconnect()` to break the
    /// `Rc` cycle that keeps `Inner` alive after the UI scope has dropped its
    /// own client clones.
    pub fn clear_packet_handler(&self) {
        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::ClearPacketHandler)
        {
            error!("Failed to clear packet handler: {e}");
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

    // Track a frame received from a peer for a specific media type.
    //
    // `decoded` (issue #2190) must be `false` whenever the caller's decode returned
    // `DecodeStatus::SKIPPED` — the packet arrived but nothing was decoded. See the
    // `DiagnosticEvent::FrameReceived::decoded` docs for why counting skips inflated
    // `fps_received` to the simulcast ladder sum.
    pub fn track_frame(
        &self,
        peer_id: &str,
        media_type: MediaType,
        frame_size: u64,
        decoded: bool,
    ) -> f64 {
        // Issue #2190 test seam — record before any early return so the assertion sees
        // exactly what the call site passed. See `track_frame_calls`.
        #[cfg(test)]
        self.track_frame_calls
            .borrow_mut()
            .push((media_type, frame_size, decoded));

        // Gated on `decoded` for the same reason as the per-peer tracker: this counter's
        // NAME is its contract — `frames_decoded` must not be advanced by a packet that
        // decoded nothing. (It has no emitter today: `health_reporter` has a
        // `"frames_decoded"` arm and a proto field, but nothing broadcasts that metric, so
        // `VideoStats.frames_decoded` currently rides at its proto-0 default. Gate it
        // correctly anyway, so whoever wires the emitter inherits truthful semantics
        // rather than the ladder-sum bug this issue fixes.)
        if decoded {
            self.frames_decoded.fetch_add(1, Ordering::Relaxed);
        }

        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::FrameReceived {
                peer_id: peer_id.to_string(),
                media_type,
                frame_size,
                decoded,
            })
        {
            error!("Failed to send frame event: {e}");
        }

        if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::RequestStats) {
            error!("Failed to request stats: {e}");
        }

        0.0
    }

    // Track a decode error for a specific peer (Phase 1 metric - windowed counter)
    // Note: this counts codec/decode errors (keyframe miss, parse error, decoder reset),
    // NOT CPU-pressure-driven throughput drops (which WebCodecs does not expose).
    pub fn track_decode_error(&self, peer_id: &str, media_type: MediaType) {
        // Send event to worker to increment the windowed counter for this peer
        if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::DecodeError {
            peer_id: peer_id.to_string(),
            media_type,
        }) {
            error!("Failed to track decode error: {e}");
        }
    }

    // Remove all tracking state for a departed peer.
    // Must be called when a peer leaves to prevent stale FpsTracker entries from
    // broadcasting stale DiagEvents indefinitely, which would defeat the freshness gate.
    pub fn remove_peer(&self, peer_id: &str) {
        if let Err(e) = self.sender.clone().try_send(DiagnosticEvent::RemovePeer {
            peer_id: peer_id.to_string(),
        }) {
            error!("Failed to remove peer from diagnostics: {e}");
        }
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

    /// Issue #2190 test seam: drain the recorded `track_frame` calls. See
    /// [`DiagnosticManager::track_frame_calls`].
    #[cfg(test)]
    pub(crate) fn take_track_frame_calls_for_test(&self) -> Vec<(MediaType, u64, bool)> {
        std::mem::take(&mut *self.track_frame_calls.borrow_mut())
    }

    /// Issue #2190 test seam: the cumulative `frames_decoded` counter, which must
    /// advance only for genuinely decoded packets.
    #[cfg(test)]
    pub(crate) fn frames_decoded_for_test(&self) -> u32 {
        self.frames_decoded.load(Ordering::Relaxed)
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
                decoded,
            } => {
                let peer_trackers = self.fps_trackers.entry(peer_id.clone()).or_default();

                let tracker = peer_trackers
                    .entry(media_type)
                    .or_insert_with(|| FpsTracker::new(media_type));

                tracker.track_frame_with_size(frame_size, decoded);
            }
            DiagnosticEvent::DecodeError {
                peer_id,
                media_type,
            } => {
                let peer_trackers = self.fps_trackers.entry(peer_id.clone()).or_default();

                let tracker = peer_trackers
                    .entry(media_type)
                    .or_insert_with(|| FpsTracker::new(media_type));

                tracker.track_decode_error();
            }
            DiagnosticEvent::SetStatsCallback(callback) => {
                self.on_stats_update = Some(callback);
            }
            DiagnosticEvent::SetReportingInterval(interval) => {
                self.report_interval_ms = interval;
            }
            DiagnosticEvent::RemovePeer { peer_id } => {
                self.fps_trackers.remove(&peer_id);
            }
            DiagnosticEvent::RequestStats => {
                self.maybe_report_stats_to_ui();
            }
            DiagnosticEvent::HeartbeatTick => {
                // PER-TICK hot path: fires on every diagnostics heartbeat (~1 Hz
                // per stream). Demoted debug!->trace! so it stays off even when
                // console-log collection bumps the ceiling to Debug (#1100
                // follow-up). Not on the analyzer keep-list.
                trace!("Diagnostics heartbeat tick");

                // Always report stats on heartbeat
                self.maybe_report_stats_to_ui();
                // Create and send diagnostic packets for each peer

                self.send_diagnostic_packets();
            }
            DiagnosticEvent::SetPacketHandler(callback) => {
                self.packet_handler = Some(callback);
            }
            DiagnosticEvent::ClearPacketHandler => {
                self.packet_handler = None;
            }
        }
    }

    fn send_diagnostic_packets(&self) {
        let now = Date::now();
        let timestamp_ms = now as u64;

        for (peer_id, peer_trackers) in &self.fps_trackers {
            for (media_type, tracker) in peer_trackers {
                // Inactivity-aware metrics via the SHARED gate (issue #2190): report zeros
                // rather than stale cached values from the last active window, with fps on the
                // DECODE clock and bitrate + decode_errors on the ARRIVAL clock (decode errors
                // are arrival-keyed — `track_decode_error` fires from `Peer::decode`'s `Err`
                // arm, a packet that arrived and then failed).
                //
                // This is THE production path — these values become the "video" DiagEvent →
                // health packet → `videocall_video_fps` / `videocall_video_bitrate_kbps`, so
                // the distinction is what an operator actually sees. It previously duplicated
                // the rule inline, which meant the tracker tests (which assert through
                // `get_metrics`, whose only consumer `set_stats_callback` has NO caller in
                // the repo) could not see a revert here at all. Calling the one method makes
                // those tests cover this path.
                let (fps, bitrate, decode_errors) = tracker.gated_metrics(now);

                // Only broadcast "video" subsystem events for VIDEO and SCREEN media types.
                // AUDIO packet rate from fps_trackers must NOT go into the video-quality channel
                // because:
                //   1. Audio packet rate (~50/s) would overwrite the real video fps in the
                //      health reporter's video_stats, showing "50 fps" instead of actual fps.
                //   2. An inactive AUDIO tracker (fps=0) would zero out video fps even when
                //      video is flowing fine, causing quality scores to disappear (the N-1
                //      alternating stats bug observed in vcprobe).
                // Audio quality is measured by NetEQ via the "neteq" subsystem — not here.
                if *media_type != MediaType::AUDIO {
                    let video_event = DiagEvent {
                        subsystem: "video",
                        stream_id: None,
                        ts_ms: now_ms(),
                        metrics: vec![
                            metric!("fps_received", fps),
                            metric!("bitrate_kbps", bitrate),
                            metric!("decode_errors_per_sec", decode_errors),
                            metric!("decode_errors_total", tracker.total_decode_errors),
                            metric!("media_type", format!("{:?}", media_type)),
                            metric!("from_peer", self.userid.clone()),
                            metric!("to_peer", peer_id.clone()),
                        ],
                    };
                    // Fires per (peer x stream) on every diagnostics heartbeat —
                    // O(peers) churn. Demoted debug!->trace!; not on the analyzer
                    // keep-list.
                    trace!(
                        "Broadcasting video event for peer {} ({:?}): FPS={:.2}, Bitrate={:.1}kbps, DecodeErrors={:.1}/s",
                        peer_id, media_type, fps, bitrate, decode_errors
                    );
                    let _ = global_sender().try_broadcast(video_event);
                }

                // Only create and send protobuf packets if packet handler is set (legacy system)
                if let Some(handler) = &self.packet_handler {
                    let mut packet = DiagnosticsPacket::new();
                    packet.target_id = self.userid.clone();
                    packet.sender_id = peer_id.clone();
                    packet.timestamp_ms = timestamp_ms;

                    packet.media_type = (*media_type).into();

                    if *media_type == MediaType::AUDIO {
                        let mut audio_metrics = AudioMetrics::new();
                        audio_metrics.bitrate_kbps = bitrate as u32;
                        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);
                    } else {
                        let mut video_metrics = VideoMetrics::new();
                        video_metrics.bitrate_kbps = bitrate as u32;
                        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
                    }

                    // Fires per (peer x stream) on every diagnostics heartbeat —
                    // O(peers) churn. Demoted debug!->trace!; not on the analyzer
                    // keep-list.
                    trace!(
                        "Sending diagnostic packet to {}: {:?} FPS: {:.2} Bitrate: {:.1} kbit/s",
                        peer_id,
                        media_type,
                        fps,
                        bitrate
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
    // NOTE(#1108): `AddSenderChannel` (the encoder-AQ fan-in) was removed in
    // Stage 2 — receiver FPS no longer feeds the sender AQ. Ingested peer
    // diagnostics still drive the global broadcast + UI stats (sinks 1 & 2).
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

#[derive(Debug)]
pub struct SenderDiagnosticManager {
    sender: Sender<SenderDiagnosticEvent>,
    /// See [`DiagnosticManager::timer`] for the rationale behind the
    /// worker-backed heartbeat.
    timer: Option<HeartbeatTimer>,
    _report_interval_ms: u64,
}

struct SenderDiagnosticWorker {
    stream_stats: HashMap<String, HashMap<MediaType, StreamStats>>, // peer_id -> media_type -> stats
    on_stats_update: Option<Callback<String>>,
    encoder_callbacks: Vec<Callback<DiagnosticsPacket>>,
    // NOTE(#1108): `sender_channels` (the encoder-AQ fan-in) removed in Stage 2.
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
        let timer = HeartbeatTimer::start(HEARTBEAT_PERIOD_MS, move || {
            if let Err(e) = sender
                .clone()
                .try_send(SenderDiagnosticEvent::HeartbeatTick)
            {
                log::debug!("Failed to enqueue sender heartbeat event: {e:?}");
            }
        });
        self.timer = Some(timer);
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

    // NOTE(#1108): `add_sender_channel` (subscribed the encoder AQ to receiver
    // diagnostics) was removed in Stage 2 along with the encoder-AQ fan-in.

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
                // Fires for every received diagnostics packet (per-packet).
                // Demoted debug!->trace!; not on the analyzer keep-list.
                trace!(
                    "Broadcasting sender event for target {target_id}: sender={sender_id}, media_type={media_type:?}"
                );
                let _ = global_sender().try_broadcast(event);

                // Sink 2: per-(peer,media) UI stats accounting (only our own
                // outbound streams, sender_id == userid). Drives the UI stats
                // string. KEPT (issue #1108).
                if sender_id == self.userid {
                    let peer_stats = self.stream_stats.entry(target_id.clone()).or_default();
                    let stats = peer_stats
                        .entry(media_type)
                        .or_insert_with(|| StreamStats::new(target_id, media_type));
                    stats.update_from_packet(&packet, media_type);
                }
                // NOTE(#1108): the encoder-AQ fan-in (sink 3) that forwarded
                // these packets to the encoder control loops was removed here —
                // the sender no longer adapts to receiver-reported FPS.
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

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    wasm_bindgen_test_configure!(run_in_browser);

    /// Issue #2190: `FpsTracker` must count FRAMES only when a packet was actually
    /// decoded, while still accumulating its BYTES either way.
    ///
    /// This pins the callee that the `peer_decode_manager` call site feeds. Both
    /// halves need their own coverage: the call-site regression
    /// (`skipped_wrong_rung_packet_is_not_counted_as_a_received_frame`) proves the
    /// right `decoded` value is PASSED, and this proves the tracker ACTS on it.
    /// Neither implies the other — dropping the `if decoded` guard here leaves the
    /// call-site test green, because that test observes the argument, not the effect.
    ///
    /// `fps` is asserted via the 1-second rollover rather than by reading
    /// `frames_count`, so the assertion goes through the same arithmetic that
    /// produces `fps_received` → `videocall_video_fps`.
    ///
    /// MUTATION: removing the `if decoded` guard around the frame counters makes the
    /// `fps` assertion read ~2x the true cadence — the exact ladder-sum inflation
    /// #2190 reports.
    ///
    /// `#[wasm_bindgen_test]`: `FpsTracker` timestamps with `js_sys::Date::now()`.
    #[wasm_bindgen_test]
    fn fps_tracker_counts_decoded_frames_only_but_bills_all_bytes() {
        let mut tracker = FpsTracker::new(MediaType::VIDEO);

        // Three arrivals inside one window: ONE decoded (the selected rung) and TWO
        // skipped (wrong-rung packets the relay forwarded anyway). This is the 3-rung
        // publisher shape from the field report.
        tracker.track_frame_with_size(100, false);
        tracker.track_frame_with_size(100, true);
        tracker.track_frame_with_size(100, false);

        // Back-date the window anchor so the NEXT call rolls over and publishes the
        // fps for the window above, rather than waiting a real second. Rollover is
        // independent of `decoded`, so a skipped arrival can drive it.
        tracker.last_bitrate_update = Date::now() - 1000.0;
        let (fps, bitrate) = tracker.track_frame_with_size(100, false);

        assert_eq!(
            tracker.total_frames, 1,
            "only the DECODED arrival is a frame; counting the two skipped rungs is \
             what inflated fps to the ladder sum (#2190)"
        );
        assert!(
            fps > 0.0 && fps <= 1.5,
            "the rolled-over fps must reflect ONE decoded frame in ~1s, not three \
             arrivals (got {fps})"
        );
        assert!(
            bitrate > 0.0,
            "bytes from SKIPPED packets still crossed the downlink and must be \
             billed to bitrate_kbps (got {bitrate})"
        );
    }

    /// Issue #2190: a RECEIVING-but-not-DECODING stream must read fps 0 while its
    /// bitrate stays LIVE.
    ///
    /// The two freshness clocks are independent, and each half of this matters:
    ///   * fps must zero out — otherwise a receiver pinned to a rung the publisher
    ///     stopped sending keeps republishing a stale nonzero fps, reporting health for
    ///     a frozen tile (the #2190 defect, in its other form).
    ///   * bitrate must NOT zero out — those packets are genuinely crossing the
    ///     downlink, and reporting 0 kbps would hide real bandwidth consumption for a
    ///     hidden/off-budget tile at exactly the moment an operator is accounting for
    ///     it. This is the state a single decoded-frame clock made unrepresentable.
    ///
    /// MUTATION: moving `self.last_packet_time = now` inside the `if decoded` block, or
    /// re-gating bitrate on the decode clock in `FpsTracker::gated_metrics`, fails the bitrate
    /// assertion. Hoisting `self.last_frame_time = now` out of the `if decoded` block fails
    /// the fps one.
    ///
    /// This test asserts through `get_metrics`, which is a thin wrapper over `gated_metrics` —
    /// the SAME method `send_diagnostic_packets` (the only path that reaches Prometheus) calls.
    /// That single-definition property is what makes this test cover production. An earlier
    /// version claimed it covered `send_diagnostic_packets` while that function DUPLICATED the
    /// rule inline, so reverting the split there left this green — measured in review, not
    /// predicted. Do not re-inline the gate.
    #[wasm_bindgen_test]
    fn receiving_but_not_decoding_zeroes_fps_but_keeps_bitrate_live() {
        let mut tracker = FpsTracker::new(MediaType::VIDEO);

        // Establish a real published fps and bitrate.
        tracker.last_bitrate_update = Date::now() - 1000.0;
        let (fps, bitrate) = tracker.track_frame_with_size(100, true);
        assert!(fps > 0.0, "precondition: a real fps was published");
        assert!(bitrate > 0.0, "precondition: a real bitrate was published");

        // Age BOTH clocks past the 1s window, so the only thing that can bring either
        // back is the arrival below. Back-dating only `last_frame_time` would leave the
        // packet clock trivially fresh from the decoded call above, and the assertion
        // would pass even if skipped arrivals did NOT refresh it — i.e. it would not
        // catch re-gating the packet clock on `decoded`.
        let stale = Date::now() - 2000.0;
        tracker.last_frame_time = stale;
        tracker.last_packet_time = stale;

        // A wrong-rung packet arrives. It must NOT revive fps, but it MUST refresh the
        // packet clock and so keep the bitrate readout live.
        tracker.track_frame_with_size(100, false);

        let (gated_fps, gated_bitrate) = tracker.get_metrics();
        assert_eq!(
            gated_fps, 0.0,
            "a stream decoding nothing must read fps 0; skipped arrivals must not hold \
             the DECODE freshness gate open"
        );
        assert!(
            gated_bitrate > 0.0,
            "packets are still arriving and consuming downlink, so bitrate must stay \
             LIVE — zeroing it hides real bandwidth use (got {gated_bitrate})"
        );
    }

    /// Issue #2190 (review follow-up): `decode_errors_per_sec` must ride the ARRIVAL clock,
    /// not the decode clock.
    ///
    /// `track_decode_error` fires from `Peer::decode`'s `Err` arm — a packet that ARRIVED and
    /// then failed to decode. That arm also calls `track_frame(..., false)` so the failed
    /// packet refreshes the arrival clock and bills its bytes without advancing decoded FPS.
    /// Pairing errors with the decode clock reported 0 for exactly the stream that is arriving
    /// and erroring on every packet: the case an operator most needs to see, silenced.
    ///
    /// MUTATION: moving `decode_errors` back under `decode_idle` in
    /// `FpsTracker::gated_metrics` makes this read 0.0 and fails.
    #[wasm_bindgen_test]
    fn decode_errors_ride_the_arrival_clock_not_the_decode_clock() {
        let mut tracker = FpsTracker::new(MediaType::VIDEO);

        // Mirror the production error arm: increment the error counter, then record the failed
        // packet as a non-decoded arrival. The arrival rolls the window over.
        tracker.track_decode_error();
        tracker.last_bitrate_update = Date::now() - 1000.0;
        tracker.track_frame_with_size(100, false);
        assert!(
            tracker.decode_errors_per_sec > 0.0,
            "precondition: a nonzero error rate was published"
        );

        // Nothing has DECODED for 2s (the arriving-and-erroring stream), but packets are
        // still arriving — `last_packet_time` was just refreshed by the call above.
        tracker.last_frame_time = Date::now() - 2000.0;

        let (fps, bitrate, decode_errors) = tracker.gated_metrics(Date::now());
        assert_eq!(fps, 0.0, "nothing decoded, so fps is 0");
        assert!(
            bitrate > 0.0,
            "packets still arriving, so bitrate stays live"
        );
        assert!(
            decode_errors > 0.0,
            "decode errors are ARRIVAL-keyed events — a stream arriving and erroring on every \
             packet must still report its error rate, not 0 (got {decode_errors})"
        );

        // When arrivals stop too, the error rate goes quiet with them.
        tracker.last_packet_time = Date::now() - 2000.0;
        let (_, _, quiet_errors) = tracker.gated_metrics(Date::now());
        assert_eq!(
            quiet_errors, 0.0,
            "with no arrivals there are no new errors to report"
        );
    }

    /// Issue #2190: when arrivals STOP entirely, bitrate must also go to zero.
    ///
    /// Guards the other direction of the split — without this, `last_packet_time` could
    /// be hardcoded "always fresh" and the test above would still pass, latching a stale
    /// bitrate forever on a stream that has genuinely stopped.
    #[wasm_bindgen_test]
    fn fully_idle_stream_zeroes_both_fps_and_bitrate() {
        let mut tracker = FpsTracker::new(MediaType::VIDEO);

        tracker.last_bitrate_update = Date::now() - 1000.0;
        let (fps, bitrate) = tracker.track_frame_with_size(100, true);
        assert!(
            fps > 0.0 && bitrate > 0.0,
            "precondition: both were published"
        );

        // Nothing decoded AND nothing arrived for 2s.
        let stale = Date::now() - 2000.0;
        tracker.last_frame_time = stale;
        tracker.last_packet_time = stale;

        assert_eq!(
            tracker.get_metrics(),
            (0.0, 0.0),
            "a stream receiving nothing at all must read INACTIVE on both axes"
        );
    }
}
