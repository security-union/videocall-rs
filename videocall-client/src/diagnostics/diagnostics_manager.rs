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

/// Shared by the fps freshness gate and the freeze-episode machine.
const DECODE_IDLE_GAP_MS: f64 = 1000.0;

#[cfg(test)]
type TrackFrameCall = (MediaType, u64, bool, bool, Option<f64>);

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
        /// `Peer::visible` at arrival time (issue #2249) — the flag both the VIDEO and
        /// SCREEN arms gate decode on. Distinct from `decoded` (a per-packet outcome):
        /// this is the receiver's STANDING intent, and it is what separates a freeze
        /// from a tile we declined to decode when both read fps 0 with bitrate live.
        decode_eligible: bool,
        /// The peer decoder's output clock at arrival (#2511). See [`FpsTracker::last_output_ms`].
        last_output_ms: Option<f64>,
    },
    DecodeError {
        peer_id: String,
        media_type: MediaType,
    },
    RemovePeer {
        peer_id: String,
    },
    /// The peer's self-reported media-enabled state (#2511), on the HEARTBEAT because
    /// `FrameReceived` stops arriving exactly when the source stops publishing.
    PeerMediaState {
        peer_id: String,
        video_enabled: bool,
        screen_enabled: bool,
    },
    PeerDecodeEligibility {
        peer_id: String,
        decode_eligible: bool,
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
    /// Last-arrival value of `Peer::visible` (issue #2249). Re-stamped on EVERY arrival
    /// alongside `last_packet_time`, so it cannot latch stale for a still-delivering
    /// stream. Initialised `true`: a missing signal must fail OPEN into the freeze branch.
    decode_eligible: bool,
    /// #2511 freeze clock: decoder OUTPUT, where `last_frame_time` is INPUT. A rebuilt decoder's
    /// `0.0` is ignored; every other reported instant is adopted, including a lower one.
    last_output_ms: f64,
    /// #2511: `freeze_episodes_total` advances on the false->true edge only.
    in_freeze: bool,
    last_freeze_observe_ms: f64,
    freeze_episodes_total: u64,
    freeze_ms_total: f64,
    max_decode_gap_ms: f64,
    /// Whether the SOURCE is publishing this media (#2511); heartbeat-carried, unlike
    /// `decode_eligible`. Initialised `true`: a missing signal must fail OPEN.
    publishing: bool,
    /// Earliest instant freeze time may be billed from (#2511); restamped on every
    /// un-billable heartbeat.
    billable_since_ms: f64,
}

impl FpsTracker {
    fn new(media_type: MediaType) -> Self {
        Self::new_at(media_type, Date::now())
    }

    /// Injected clock: `js_sys::Date::now` panics on a native target.
    fn new_at(media_type: MediaType, now: f64) -> Self {
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
            decode_eligible: true,
            last_output_ms: now,
            in_freeze: false,
            last_freeze_observe_ms: now,
            freeze_episodes_total: 0,
            freeze_ms_total: 0.0,
            max_decode_gap_ms: 0.0,
            publishing: true,
            billable_since_ms: now,
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
    fn track_frame_with_size(
        &mut self,
        bytes: u64,
        decoded: bool,
        decode_eligible: bool,
        last_output_ms: Option<f64>,
    ) -> (f64, f64) {
        self.track_frame_with_size_at(bytes, decoded, decode_eligible, last_output_ms, Date::now())
    }

    /// Injected clock: `js_sys::Date::now` panics on a native target.
    fn track_frame_with_size_at(
        &mut self,
        bytes: u64,
        decoded: bool,
        decode_eligible: bool,
        last_output_ms: Option<f64>,
        now: f64,
    ) -> (f64, f64) {
        self.decode_eligible = decode_eligible;
        if let Some(reported) = last_output_ms {
            // `0.0` is the decoder's "no output yet" sentinel, not an instant. Every other value
            // is adopted, even a LOWER one (wall-clock can step back), but only a FRESH one ends
            // an episode: closing on a stale-but-advanced report re-bills time already billed.
            if reported > 0.0 {
                let advanced = reported > self.last_output_ms;
                self.last_output_ms = reported;
                if advanced && now - reported <= DECODE_IDLE_GAP_MS {
                    self.close_freeze_window_at(now);
                }
            }
        }
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
        let decode_idle = now - self.last_frame_time > DECODE_IDLE_GAP_MS;
        // Deliberately NOT DECODE_IDLE_GAP_MS: this is the ARRIVAL clock.
        let packet_idle = now - self.last_packet_time > 1000.0;
        let fps = if decode_idle { 0.0 } else { self.fps };
        let (bitrate, decode_errors) = if packet_idle {
            (0.0, 0.0)
        } else {
            (self.current_bitrate, self.decode_errors_per_sec)
        };
        (fps, bitrate, decode_errors)
    }

    /// Advance the freeze-episode machine one heartbeat (#2511), keyed on decoder OUTPUT. A
    /// tile that is INELIGIBLE (#2249) or whose source is not publishing is not frozen.
    fn observe_freeze(&mut self, now: f64) {
        if now < self.last_freeze_observe_ms {
            self.in_freeze = false;
            self.last_freeze_observe_ms = now;
            self.billable_since_ms = now;
            return;
        }

        if !self.decode_eligible || !self.publishing {
            self.in_freeze = false;
            self.last_freeze_observe_ms = now;
            self.billable_since_ms = now;
            return;
        }

        // Clamped to the billable window; `last_output_ms` alone spans un-billable time.
        let gap = now - self.last_output_ms.max(self.billable_since_ms);
        if gap > self.max_decode_gap_ms {
            self.max_decode_gap_ms = gap;
        }

        if gap > DECODE_IDLE_GAP_MS {
            if self.in_freeze {
                self.freeze_ms_total += (now - self.last_freeze_observe_ms).max(0.0);
            } else {
                self.in_freeze = true;
                self.freeze_episodes_total += 1;
                self.freeze_ms_total += gap;
            }
        } else {
            self.in_freeze = false;
        }

        self.last_freeze_observe_ms = now;
    }

    fn reset_freeze_window(&mut self, now: f64) {
        self.in_freeze = false;
        self.last_freeze_observe_ms = now;
        self.billable_since_ms = now;
    }

    /// Ends an open episode. Deliberately NOT `reset_freeze_window`: rebasing `billable_since_ms`
    /// here makes `observe_freeze`'s clamp measure from the reporting ARRIVAL, not the output.
    fn close_freeze_window_at(&mut self, now: f64) {
        if self.in_freeze {
            self.freeze_ms_total += (now - self.last_freeze_observe_ms).max(0.0);
        }
        self.in_freeze = false;
        self.last_freeze_observe_ms = now;
    }

    fn set_decode_eligible_at(&mut self, decode_eligible: bool, now: f64) {
        if self.decode_eligible == decode_eligible {
            return;
        }
        if !decode_eligible {
            self.observe_freeze(now);
        }
        self.decode_eligible = decode_eligible;
        self.reset_freeze_window(now);
    }

    fn set_publishing_at(&mut self, publishing: bool, now: f64) {
        if self.publishing == publishing {
            return;
        }
        if !publishing {
            self.observe_freeze(now);
        }
        self.publishing = publishing;
        self.reset_freeze_window(now);
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
    userid: String,
    frames_decoded: Arc<AtomicU32>,
    report_interval_ms: u64,
    /// Issue #2190 test seam: every [`TrackFrameCall`] passed to `track_frame`, in call order.
    ///
    /// Recorded SYNCHRONOUSLY inside `track_frame`, before the `try_send`, on purpose:
    /// the `DiagnosticWorker` that owns `fps_trackers` runs on a `spawn_local` task, so
    /// asserting on tracker state would require yielding to the executor and would make
    /// the test a race. This seam observes what the CALL SITE decided, which is exactly
    /// the thing the fix changes.
    #[cfg(test)]
    track_frame_calls: std::cell::RefCell<Vec<TrackFrameCall>>,
    /// #2511 test seam, same rationale as `track_frame_calls`: `(peer_id, video, screen)`.
    #[cfg(test)]
    peer_media_state_calls: std::cell::RefCell<Vec<(String, bool, bool)>>,
    #[cfg(test)]
    peer_decode_eligibility_calls: std::cell::RefCell<Vec<(String, bool)>>,
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
    latest_peer_media_state: HashMap<String, PeerMediaStateSnapshot>,
    latest_peer_decode_eligibility: HashMap<String, bool>,
    on_stats_update: Option<Callback<String>>,
    last_report_time: f64, // timestamp in ms
    report_interval_ms: u64,
    packet_handler: Option<Callback<DiagnosticsPacket>>,
    receiver: Receiver<DiagnosticEvent>,
    userid: String,
}

#[derive(Debug, Clone, Copy)]
struct PeerMediaStateSnapshot {
    video_enabled: bool,
    screen_enabled: bool,
}

impl Default for PeerMediaStateSnapshot {
    fn default() -> Self {
        Self {
            video_enabled: true,
            screen_enabled: true,
        }
    }
}

impl PeerMediaStateSnapshot {
    fn publishing_for(self, media_type: MediaType) -> bool {
        match media_type {
            MediaType::VIDEO => self.video_enabled,
            MediaType::SCREEN => self.screen_enabled,
            _ => true,
        }
    }
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
        let manager_userid = userid.clone();

        // Spawn the worker to process events
        let worker = DiagnosticWorker {
            fps_trackers: HashMap::new(),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
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
            userid: manager_userid,
            frames_decoded: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 500,
            timer: None,
            #[cfg(test)]
            track_frame_calls: std::cell::RefCell::new(Vec::new()),
            #[cfg(test)]
            peer_media_state_calls: std::cell::RefCell::new(Vec::new()),
            #[cfg(test)]
            peer_decode_eligibility_calls: std::cell::RefCell::new(Vec::new()),
        };

        manager.setup_heartbeat(sender);

        manager
    }

    #[cfg(test)]
    pub(crate) fn new_for_test_without_timer(userid: String) -> Self {
        let (sender, _receiver) = mpsc::channel(100);
        Self {
            sender,
            userid,
            frames_decoded: Arc::new(AtomicU32::new(0)),
            report_interval_ms: 500,
            track_frame_calls: std::cell::RefCell::new(Vec::new()),
            peer_media_state_calls: std::cell::RefCell::new(Vec::new()),
            peer_decode_eligibility_calls: std::cell::RefCell::new(Vec::new()),
            timer: None,
        }
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
        decode_eligible: bool,
        last_output_ms: Option<f64>,
    ) -> f64 {
        // Issue #2190 test seam — record before any early return so the assertion sees
        // exactly what the call site passed. See `track_frame_calls`.
        #[cfg(test)]
        self.track_frame_calls.borrow_mut().push((
            media_type,
            frame_size,
            decoded,
            decode_eligible,
            last_output_ms,
        ));

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
                decode_eligible,
                last_output_ms,
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

    /// Drive `FpsTracker::publishing` for this peer's VIDEO and SCREEN trackers (#2511).
    pub fn set_peer_media_state(&self, peer_id: &str, video_enabled: bool, screen_enabled: bool) {
        #[cfg(test)]
        self.peer_media_state_calls.borrow_mut().push((
            peer_id.to_string(),
            video_enabled,
            screen_enabled,
        ));

        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::PeerMediaState {
                peer_id: peer_id.to_string(),
                video_enabled,
                screen_enabled,
            })
        {
            error!("Failed to send peer media state: {e}");
        }
    }

    pub fn set_peer_decode_eligibility(&self, peer_id: &str, decode_eligible: bool) {
        #[cfg(test)]
        self.peer_decode_eligibility_calls
            .borrow_mut()
            .push((peer_id.to_string(), decode_eligible));

        if let Err(e) = self
            .sender
            .clone()
            .try_send(DiagnosticEvent::PeerDecodeEligibility {
                peer_id: peer_id.to_string(),
                decode_eligible,
            })
        {
            error!("Failed to send peer decode eligibility: {e}");
        }

        for media_type in [MediaType::VIDEO, MediaType::SCREEN] {
            let _ = global_sender().try_broadcast(DiagEvent {
                subsystem: "decode_eligibility",
                stream_id: None,
                ts_ms: now_ms(),
                metrics: vec![
                    metric!("media_type", format!("{:?}", media_type)),
                    metric!("from_peer", self.userid.clone()),
                    metric!("to_peer", peer_id.to_string()),
                    metric!("decode_eligible", u64::from(decode_eligible)),
                ],
            });
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
    pub(crate) fn take_track_frame_calls_for_test(&self) -> Vec<TrackFrameCall> {
        std::mem::take(&mut *self.track_frame_calls.borrow_mut())
    }

    /// #2511 test seam: drain the recorded `set_peer_media_state` calls.
    #[cfg(test)]
    pub(crate) fn take_peer_media_state_calls_for_test(&self) -> Vec<(String, bool, bool)> {
        std::mem::take(&mut *self.peer_media_state_calls.borrow_mut())
    }

    #[cfg(test)]
    pub(crate) fn take_peer_decode_eligibility_calls_for_test(&self) -> Vec<(String, bool)> {
        std::mem::take(&mut *self.peer_decode_eligibility_calls.borrow_mut())
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

    fn event_now(&self) -> f64 {
        #[cfg(test)]
        {
            self.last_report_time
        }
        #[cfg(not(test))]
        {
            Date::now()
        }
    }

    fn tracker_for_event(&mut self, peer_id: &str, media_type: MediaType) -> &mut FpsTracker {
        let media_state = self
            .latest_peer_media_state
            .get(peer_id)
            .copied()
            .unwrap_or_default();
        let decode_eligible = self
            .latest_peer_decode_eligibility
            .get(peer_id)
            .copied()
            .unwrap_or(true);
        #[cfg(test)]
        let tracker_now = self.last_report_time;
        let peer_trackers = self.fps_trackers.entry(peer_id.to_string()).or_default();

        peer_trackers.entry(media_type).or_insert_with(|| {
            #[cfg(test)]
            let mut tracker = FpsTracker::new_at(media_type, tracker_now);
            #[cfg(not(test))]
            let mut tracker = FpsTracker::new(media_type);

            tracker.publishing = media_state.publishing_for(media_type);
            tracker.decode_eligible = decode_eligible;
            tracker
        })
    }

    fn handle_event(&mut self, event: DiagnosticEvent) {
        match event {
            DiagnosticEvent::FrameReceived {
                peer_id,
                media_type,
                frame_size,
                decoded,
                decode_eligible,
                last_output_ms,
            } => {
                let tracker = self.tracker_for_event(&peer_id, media_type);

                tracker.track_frame_with_size(frame_size, decoded, decode_eligible, last_output_ms);
            }
            DiagnosticEvent::DecodeError {
                peer_id,
                media_type,
            } => {
                let tracker = self.tracker_for_event(&peer_id, media_type);

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
                self.latest_peer_media_state.remove(&peer_id);
                self.latest_peer_decode_eligibility.remove(&peer_id);
            }
            DiagnosticEvent::PeerMediaState {
                peer_id,
                video_enabled,
                screen_enabled,
            } => {
                let media_state = PeerMediaStateSnapshot {
                    video_enabled,
                    screen_enabled,
                };
                self.latest_peer_media_state
                    .insert(peer_id.clone(), media_state);
                let now = self.event_now();
                if let Some(peer_trackers) = self.fps_trackers.get_mut(&peer_id) {
                    if let Some(t) = peer_trackers.get_mut(&MediaType::VIDEO) {
                        t.set_publishing_at(video_enabled, now);
                    }
                    if let Some(t) = peer_trackers.get_mut(&MediaType::SCREEN) {
                        t.set_publishing_at(screen_enabled, now);
                    }
                }
            }
            DiagnosticEvent::PeerDecodeEligibility {
                peer_id,
                decode_eligible,
            } => {
                self.latest_peer_decode_eligibility
                    .insert(peer_id.clone(), decode_eligible);
                let now = self.event_now();
                if let Some(peer_trackers) = self.fps_trackers.get_mut(&peer_id) {
                    if let Some(t) = peer_trackers.get_mut(&MediaType::VIDEO) {
                        t.set_decode_eligible_at(decode_eligible, now);
                    }
                    if let Some(t) = peer_trackers.get_mut(&MediaType::SCREEN) {
                        t.set_decode_eligible_at(decode_eligible, now);
                    }
                }
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

    fn send_diagnostic_packets(&mut self) {
        self.send_diagnostic_packets_at(Date::now());
    }

    /// Injected clock: `js_sys::Date::now` panics on a native target.
    fn send_diagnostic_packets_at(&mut self, now: f64) {
        let timestamp_ms = now as u64;
        let userid = &self.userid;
        let packet_handler = &self.packet_handler;

        for (peer_id, peer_trackers) in &mut self.fps_trackers {
            for (media_type, tracker) in peer_trackers.iter_mut() {
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
                    tracker.observe_freeze(now);

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
                            metric!("from_peer", userid.clone()),
                            metric!("to_peer", peer_id.clone()),
                            // #2249: rides the same event as the bitrate it qualifies.
                            metric!(
                                "decode_eligible",
                                if tracker.decode_eligible { 1u64 } else { 0u64 }
                            ),
                            // #2511: rides fps_received's event => same media bucket.
                            metric!("freeze_episodes_total", tracker.freeze_episodes_total),
                            metric!("freeze_ms_total", tracker.freeze_ms_total as u64),
                            metric!("max_decode_gap_ms", tracker.max_decode_gap_ms as u64),
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
                if let Some(handler) = packet_handler {
                    let mut packet = DiagnosticsPacket::new();
                    packet.target_id = userid.clone();
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

    /// #2511: one sustained freeze is ONE episode.
    #[test]
    fn a_sustained_freeze_opens_exactly_one_episode() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 0,
            "a gap under DECODE_IDLE_GAP_MS is a healthy inter-frame interval"
        );

        tracker.observe_freeze(t0 + 1500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "the gap crossed => one episode"
        );

        tracker.observe_freeze(t0 + 2000.0);
        tracker.observe_freeze(t0 + 2500.0);
        tracker.observe_freeze(t0 + 3000.0);
        tracker.observe_freeze(t0 + 3500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "still ONE freeze, not five samples of it"
        );
    }

    #[test]
    fn the_episode_re_arms_after_an_output_frame() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 1500.0);
        assert_eq!(tracker.freeze_episodes_total, 1);

        tracker.track_frame_with_size_at(1_200, true, true, Some(t0 + 2000.0), t0 + 2000.0);
        tracker.observe_freeze(t0 + 2100.0);
        assert!(!tracker.in_freeze, "output again must close the episode");
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "closing an episode must not itself count one"
        );

        tracker.observe_freeze(t0 + 3500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 2,
            "a second freeze after recovery is a second episode"
        );
    }

    #[test]
    fn an_output_frame_closes_an_open_freeze_before_the_next_heartbeat() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 1500.0);
        assert_eq!(tracker.freeze_episodes_total, 1);
        assert!(tracker.in_freeze);

        tracker.track_frame_with_size_at(1_200, true, true, Some(t0 + 2000.0), t0 + 2000.0);
        assert!(
            !tracker.in_freeze,
            "output recovery must close the open episode"
        );
        assert_eq!(
            tracker.last_freeze_observe_ms,
            t0 + 2000.0,
            "the close point must be the arrival that reported the new output"
        );

        tracker.observe_freeze(t0 + 3500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 2,
            "a second freeze after output recovery must not be merged into the first episode"
        );
    }

    /// #2511: the head of the interval is included.
    #[test]
    fn integrated_freeze_ms_equals_the_wall_gap() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 1500.0);
        tracker.observe_freeze(t0 + 2000.0);
        tracker.observe_freeze(t0 + 2500.0);
        tracker.observe_freeze(t0 + 3000.0);

        assert_eq!(
            tracker.freeze_ms_total, 3000.0,
            "nothing decoded since t0, so the whole 3000ms is freeze time"
        );
        assert_eq!(
            tracker.max_decode_gap_ms, 3000.0,
            "the widest observed decode gap is the same 3000ms"
        );
    }

    #[test]
    fn an_ineligible_tile_produces_no_episode_and_no_freeze_time() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);
        tracker.decode_eligible = false;

        tracker.observe_freeze(t0 + 5_000.0);
        tracker.observe_freeze(t0 + 10_000.0);
        tracker.observe_freeze(t0 + 30_000.0);

        assert_eq!(tracker.freeze_episodes_total, 0);
        assert_eq!(tracker.freeze_ms_total, 0.0);
        assert_eq!(
            tracker.max_decode_gap_ms, 0.0,
            "a tile we declined to decode contributes no decode gap either"
        );

        tracker.decode_eligible = true;
        tracker.observe_freeze(t0 + 32_000.0);
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "an eligible tile decoding nothing IS frozen"
        );
        assert!(tracker.in_freeze);
        assert_eq!(
            tracker.freeze_ms_total, 2_000.0,
            "billed from re-eligibility, not from the last decode 32s ago"
        );
    }

    /// #2511: field 13 is a MAX over the connection, not the latest gap. Two production
    /// paths shrink `gap` after a large one and nothing ever resets the field.
    #[test]
    fn max_decode_gap_ms_holds_the_worst_gap_after_recovery() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 45_000.0);
        assert_eq!(tracker.max_decode_gap_ms, 45_000.0);

        tracker.track_frame_with_size_at(1_200, true, true, Some(t0 + 45_100.0), t0 + 45_100.0);
        tracker.observe_freeze(t0 + 45_600.0);
        assert_eq!(
            tracker.max_decode_gap_ms, 45_000.0,
            "whole-connection MAX must survive recovery"
        );
    }

    /// #2511: bunched arrivals (screen-share, lossy link) report output that is already seconds
    /// old; clamping the gap to the reporting arrival hides exactly that outage.
    #[test]
    fn a_sparse_arrival_does_not_shrink_the_output_gap() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.track_frame_with_size_at(1_200, true, true, Some(t0 + 60.0), t0 + 3_000.0);
        tracker.observe_freeze(t0 + 3_500.0);

        assert_eq!(
            tracker.max_decode_gap_ms, 3_440.0,
            "the gap must span t0+60 -> t0+3500, not the 500ms since the reporting arrival"
        );
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "3.44s without decoder output is an episode"
        );
    }

    /// #2511: a bunched arrival burst (WS head-of-line) reports the PREVIOUS burst's instant —
    /// advanced, but stale. Closing the window on it re-bills a span the open episode already
    /// billed, so `freeze_ms_total` can exceed wall time. Every other test reports a FRESH instant.
    #[test]
    fn bursty_stale_output_reports_do_not_bill_past_wall_time() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        let mut now = t0;
        while now < t0 + 30_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            if (now - t0) % 3_000.0 == 0.0 {
                tracker.track_frame_with_size_at(1_200, true, true, Some(now - 2_000.0), now);
            }
            tracker.observe_freeze(now);
        }

        let elapsed = now - t0;
        assert!(
            tracker.freeze_ms_total <= elapsed,
            "billed {} ms over {elapsed} ms of wall time",
            tracker.freeze_ms_total
        );
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "output never became fresh, so the outage is ONE continuous episode"
        );
    }

    /// #2511: a pinned future OUTPUT clock yields a negative gap that reads healthy for the whole
    /// step. Sweep-clock half of the same rollback: `a_backward_clock_step_rebaselines_the_freeze_window`.
    #[test]
    fn a_backward_clock_step_rebaselines_the_output_clock() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.track_frame_with_size_at(1_200, true, true, Some(t0 + 100.0), t0 + 100.0);

        let jumped = t0 + 100.0 - 30_000.0;
        tracker.track_frame_with_size_at(1_200, true, true, Some(jumped), jumped);

        let mut now = jumped;
        while now < jumped + 10_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            tracker.track_frame_with_size_at(1_200, true, true, Some(jumped), now);
            tracker.observe_freeze(now);
        }

        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "the post-jump outage must still be detected"
        );
        assert!(
            tracker.max_decode_gap_ms >= 9_000.0,
            "the gap must run from the rebaselined clock, got {}",
            tracker.max_decode_gap_ms
        );
    }

    /// #2511: an episode interrupted by an un-billable window is a NEW episode when
    /// billing resumes — a stale `in_freeze` would bill the time but never count it.
    #[test]
    fn an_episode_interrupted_by_an_unbillable_window_counts_as_a_new_episode() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 1_500.0);
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "precondition: an episode is open"
        );

        tracker.decode_eligible = false;
        tracker.observe_freeze(t0 + 2_000.0);

        tracker.decode_eligible = true;
        tracker.observe_freeze(t0 + 62_000.0);
        assert_eq!(
            tracker.freeze_episodes_total, 2,
            "a stale in_freeze takes the continuation branch, so the new episode accrues \
             time but is never counted"
        );
    }

    /// #2511: a camera-off peer delivers no frames and is not frozen.
    #[test]
    fn a_source_that_stopped_publishing_accrues_no_freeze_time() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);
        tracker.publishing = false;

        let mut now = t0;
        while now < t0 + 600_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            tracker.observe_freeze(now);
        }

        assert_eq!(
            tracker.freeze_episodes_total, 0,
            "ten minutes of camera-off is not a freeze episode"
        );
        assert_eq!(tracker.freeze_ms_total, 0.0);
        assert_eq!(
            tracker.max_decode_gap_ms, 0.0,
            "a lifetime max cannot be corrected downstream, so it must never record the \
             camera-off span in the first place"
        );

        tracker.publishing = true;
        tracker.observe_freeze(t0 + 600_500.0);
        assert_eq!(tracker.freeze_episodes_total, 0);
        assert_eq!(tracker.freeze_ms_total, 0.0);
        assert_eq!(
            tracker.max_decode_gap_ms, 500.0,
            "billing re-arms at camera-on; the ten idle minutes are not charged to it"
        );
    }

    /// #2511: under a monotone clock, `freeze_ms_total` is an integral over wall time.
    #[test]
    fn freeze_ms_total_never_exceeds_elapsed_wall_time_across_an_eligibility_gap() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        let mut now = t0;
        while now < t0 + 10_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            tracker.observe_freeze(now);
        }
        assert_eq!(
            tracker.freeze_ms_total, 10_000.0,
            "precondition: a real 10s freeze accrued while the tile was on screen"
        );

        tracker.decode_eligible = false;
        while now < t0 + 20_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            tracker.observe_freeze(now);
        }

        tracker.decode_eligible = true;
        now += HEARTBEAT_PERIOD_MS as f64;
        tracker.observe_freeze(now);

        let elapsed = now - t0;
        assert!(
            tracker.freeze_ms_total <= elapsed,
            "freeze_ms_total {} exceeds the {elapsed}ms of wall clock it was measured \
             over, which breaks every fraction-of-time-frozen query",
            tracker.freeze_ms_total
        );
        assert!(
            tracker.max_decode_gap_ms <= elapsed,
            "max_decode_gap_ms {} exceeds elapsed wall time {elapsed}",
            tracker.max_decode_gap_ms
        );
        assert_eq!(
            tracker.freeze_ms_total, 10_000.0,
            "the offscreen window bills nothing and the earlier episode is not re-billed"
        );
    }

    #[test]
    fn a_backward_clock_step_rebaselines_the_freeze_window() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        tracker.observe_freeze(t0 + 1_500.0);
        tracker.observe_freeze(t0 + 2_000.0);
        assert_eq!(tracker.freeze_episodes_total, 1);
        assert_eq!(tracker.freeze_ms_total, 2_000.0);

        tracker.observe_freeze(t0 + 800.0);
        assert_eq!(
            tracker.freeze_ms_total, 2_000.0,
            "a backward step must not add negative or overlapping time"
        );
        assert!(!tracker.in_freeze, "the old window must close on rollback");

        tracker.observe_freeze(t0 + 2_100.0);
        assert_eq!(
            tracker.freeze_ms_total, 3_300.0,
            "billing resumes from the rollback baseline, not from the pre-step frame clock"
        );
        assert_eq!(
            tracker.freeze_episodes_total, 2,
            "the post-rollback window is a new billable interval"
        );
    }

    /// #2511, through the PRODUCTION arrival path: `decode_eligible` flips true on the
    /// first eligible ARRIVAL, decoded or not.
    #[test]
    fn a_returning_tile_bills_only_the_time_it_has_been_back_on_screen() {
        let t0 = 1_000_000.0;
        let mut tracker = FpsTracker::new_at(MediaType::VIDEO, t0);

        // Five minutes paginated out; the relay keeps forwarding, so arrivals continue.
        let mut now = t0;
        while now < t0 + 300_000.0 {
            now += HEARTBEAT_PERIOD_MS as f64;
            tracker.track_frame_with_size_at(1_200, false, false, Some(t0), now);
            tracker.observe_freeze(now);
        }
        assert_eq!(
            tracker.freeze_ms_total, 0.0,
            "precondition: nothing billed offscreen"
        );

        now += HEARTBEAT_PERIOD_MS as f64;
        tracker.track_frame_with_size_at(1_200, false, true, Some(t0), now);
        tracker.observe_freeze(now);

        assert_eq!(
            tracker.freeze_episodes_total, 0,
            "one 500ms heartbeat back on screen is not an episode"
        );
        assert_eq!(tracker.freeze_ms_total, 0.0);
        assert_eq!(
            tracker.max_decode_gap_ms, 500.0,
            "the billed gap is the 500ms since re-eligibility, not the 300500ms since \
             the last decode"
        );
    }

    #[test]
    fn the_heartbeat_sweep_drives_the_freeze_machine() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0))]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.send_diagnostic_packets_at(t0 + 3000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "the heartbeat sweep must open the episode for a stream that decoded nothing"
        );
        assert_eq!(tracker.freeze_ms_total, 3000.0);
        assert_eq!(tracker.max_decode_gap_ms, 3000.0);
    }

    fn freeze_worker(peer_id: &str, t0: f64) -> DiagnosticWorker {
        let (_tx, rx) = mpsc::channel(1);
        DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0))]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        }
    }

    /// 30/s DECODED, ELIGIBLE arrivals reporting `output_ms`, swept every `HEARTBEAT_PERIOD_MS`.
    fn drive_arrivals_without_output(
        worker: &mut DiagnosticWorker,
        peer_id: &str,
        t0: f64,
        seconds: f64,
        output_ms: f64,
    ) -> f64 {
        let mut next_sweep = t0 + HEARTBEAT_PERIOD_MS as f64;
        let mut last_sweep = t0;
        let arrivals = (seconds * 30.0) as u32;
        for i in 1..=arrivals {
            let now = t0 + f64::from(i) * (1000.0 / 30.0);
            worker
                .tracker_for_event(peer_id, MediaType::VIDEO)
                .track_frame_with_size_at(1_200, true, true, Some(output_ms), now);
            while next_sweep <= now {
                worker.send_diagnostic_packets_at(next_sweep);
                last_sweep = next_sweep;
                next_sweep += HEARTBEAT_PERIOD_MS as f64;
            }
        }
        last_sweep
    }

    #[test]
    fn arrivals_without_decoder_output_are_a_freeze() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-no-output";
        let mut worker = freeze_worker(peer_id, t0);

        let last_sweep = drive_arrivals_without_output(&mut worker, peer_id, t0, 5.0, 0.0);
        assert_eq!(
            last_sweep,
            t0 + 5_000.0,
            "ten heartbeats over the 5s window"
        );

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "arrivals are decoder INPUT; nothing was output for 5s, so this is one episode"
        );
        assert!(
            tracker.max_decode_gap_ms >= 5_000.0,
            "the gap must span the whole output-less window, got {}",
            tracker.max_decode_gap_ms
        );
        assert!(
            tracker.freeze_ms_total >= 4_000.0,
            "the episode's billed time must approach the window, got {}",
            tracker.freeze_ms_total
        );
    }

    #[test]
    fn a_rebuilt_decoders_zero_clock_does_not_rewind_the_freeze_clock() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-rebuild";
        let mut worker = freeze_worker(peer_id, t0);

        worker
            .tracker_for_event(peer_id, MediaType::VIDEO)
            .track_frame_with_size_at(1_200, true, true, Some(t0 + 100.0), t0 + 100.0);
        let last_sweep = drive_arrivals_without_output(&mut worker, peer_id, t0 + 100.0, 5.0, 0.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "the rebuild produced no frames for 5s — that is an episode"
        );
        assert!(
            tracker.max_decode_gap_ms >= 4_900.0,
            "the gap must keep growing across the rebuild, got {} (last sweep {})",
            tracker.max_decode_gap_ms,
            last_sweep
        );
    }

    /// #2511: the camera off→ON edge rebases the billable window, and the resumed tile is judged
    /// on OUTPUT — the arrivals below DECODE throughout, so the input clock reads healthy.
    #[test]
    fn a_resumed_camera_bills_only_the_time_since_it_came_back() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-resume";
        let mut worker = freeze_worker(peer_id, t0);

        worker
            .tracker_for_event(peer_id, MediaType::VIDEO)
            .track_frame_with_size_at(1_200, true, true, Some(t0 + 100.0), t0 + 100.0);
        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: false,
            screen_enabled: false,
        });
        worker.send_diagnostic_packets_at(t0 + 10_000.0);
        assert_eq!(
            worker.fps_trackers[peer_id][&MediaType::VIDEO].freeze_ms_total,
            0.0,
            "precondition: an off camera bills nothing"
        );

        worker
            .tracker_for_event(peer_id, MediaType::VIDEO)
            .set_publishing_at(true, t0 + 10_000.0);

        // 3s of 30/s DECODED arrivals that report no new output.
        let mut now = t0 + 10_000.0;
        while now < t0 + 13_000.0 {
            now += 1000.0 / 30.0;
            worker
                .tracker_for_event(peer_id, MediaType::VIDEO)
                .track_frame_with_size_at(1_200, true, true, Some(t0 + 100.0), now);
        }
        worker.send_diagnostic_packets_at(t0 + 13_000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_episodes_total, 1,
            "3s back on, decoding every arrival and producing nothing, is a freeze"
        );
        assert_eq!(
            tracker.max_decode_gap_ms, 3_000.0,
            "billed from the resume, not from the output 12.9s ago"
        );
    }

    /// #2511: the publish gate is per media type and must not blank the other bucket.
    #[test]
    fn the_peer_media_state_event_gates_video_and_screen_independently() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-state";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([
                    (MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0)),
                    (MediaType::SCREEN, FpsTracker::new_at(MediaType::SCREEN, t0)),
                ]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: false,
            screen_enabled: true,
        });
        worker.send_diagnostic_packets_at(t0 + 3_000.0);

        let trackers = &worker.fps_trackers[peer_id];
        assert_eq!(
            trackers[&MediaType::VIDEO].freeze_ms_total,
            0.0,
            "the camera is off — no frames is the expected state, not a freeze"
        );
        assert_eq!(
            trackers[&MediaType::SCREEN].freeze_ms_total,
            3_000.0,
            "the share is still on and delivered nothing for 3s — that IS a freeze"
        );

        // Swap both flags, so each bucket is pinned in BOTH directions.
        worker.last_report_time = t0 + 3_000.0;
        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: true,
            screen_enabled: false,
        });
        worker.send_diagnostic_packets_at(t0 + 6_000.0);

        let trackers = &worker.fps_trackers[peer_id];
        assert_eq!(
            trackers[&MediaType::SCREEN].freeze_ms_total,
            3_000.0,
            "the share STOPPED — it accrues nothing further, and the earlier episode stands"
        );
        assert_eq!(
            trackers[&MediaType::VIDEO].freeze_ms_total,
            3_000.0,
            "the camera came back and still decoded nothing for 3s — billed from camera-on"
        );
    }

    #[test]
    fn decode_eligibility_reopen_does_not_bill_the_hidden_gap() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2249-toggle";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0))]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.send_diagnostic_packets_at(t0 + 1_500.0);
        worker.last_report_time = t0 + 1_600.0;
        worker.handle_event(DiagnosticEvent::PeerDecodeEligibility {
            peer_id: peer_id.to_string(),
            decode_eligible: false,
        });
        worker.last_report_time = t0 + 2_600.0;
        worker.handle_event(DiagnosticEvent::PeerDecodeEligibility {
            peer_id: peer_id.to_string(),
            decode_eligible: true,
        });
        worker.send_diagnostic_packets_at(t0 + 3_000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_ms_total, 1_600.0,
            "only the visible freeze through the hide transition is billable"
        );
        assert_eq!(
            tracker.max_decode_gap_ms, 1_600.0,
            "the hidden gap must not inflate the lifetime max"
        );
    }

    #[test]
    fn publishing_reopen_does_not_bill_the_source_off_gap() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-publish-toggle";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0))]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.send_diagnostic_packets_at(t0 + 1_500.0);
        worker.last_report_time = t0 + 1_600.0;
        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: false,
            screen_enabled: true,
        });
        worker.last_report_time = t0 + 2_600.0;
        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: true,
            screen_enabled: true,
        });
        worker.send_diagnostic_packets_at(t0 + 3_000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_ms_total, 1_600.0,
            "only the camera-on freeze through the off transition is billable"
        );
        assert_eq!(tracker.max_decode_gap_ms, 1_600.0);
    }

    #[test]
    fn peer_media_state_before_first_packet_gates_later_trackers() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2511-pre-state";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::new(),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.handle_event(DiagnosticEvent::PeerMediaState {
            peer_id: peer_id.to_string(),
            video_enabled: false,
            screen_enabled: true,
        });
        assert!(
            !worker.fps_trackers.contains_key(peer_id),
            "media-state heartbeats must not create freeze trackers by themselves"
        );

        let tracker = worker.tracker_for_event(peer_id, MediaType::VIDEO);
        assert!(
            !tracker.publishing,
            "the later tracker must inherit the earlier camera-off heartbeat"
        );
        tracker.track_frame_with_size_at(1_200, false, true, Some(t0), t0 + 500.0);

        worker.send_diagnostic_packets_at(t0 + 3_000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_ms_total, 0.0,
            "an intentionally off camera must not accrue freeze time if its off heartbeat \
             arrived before the tracker existed"
        );
        assert_eq!(tracker.max_decode_gap_ms, 0.0);
    }

    #[test]
    fn decode_eligibility_before_first_packet_gates_later_trackers() {
        let t0 = 1_000_000.0;
        let peer_id = "peer-2249-pre-eligibility";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::new(),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        worker.handle_event(DiagnosticEvent::PeerDecodeEligibility {
            peer_id: peer_id.to_string(),
            decode_eligible: false,
        });
        assert!(
            !worker.fps_trackers.contains_key(peer_id),
            "decode-eligibility events must not create freeze trackers by themselves"
        );

        let tracker = worker.tracker_for_event(peer_id, MediaType::VIDEO);
        assert!(
            !tracker.decode_eligible,
            "the later tracker must inherit the earlier offscreen state"
        );
        tracker.track_decode_error();

        worker.send_diagnostic_packets_at(t0 + 3_000.0);

        let tracker = &worker.fps_trackers[peer_id][&MediaType::VIDEO];
        assert_eq!(
            tracker.freeze_ms_total, 0.0,
            "an offscreen tile must not accrue freeze time if its eligibility event \
             arrived before the tracker existed"
        );
        assert_eq!(tracker.max_decode_gap_ms, 0.0);
    }

    /// Wasm-only: the global bus is Closed natively.
    #[wasm_bindgen_test]
    fn the_video_event_carries_the_freeze_counters() {
        use videocall_diagnostics::{subscribe, MetricValue};

        let t0 = Date::now();
        let peer_id = "peer-2511-emit";
        let (_tx, rx) = mpsc::channel(1);
        let mut worker = DiagnosticWorker {
            fps_trackers: HashMap::from([(
                peer_id.to_string(),
                HashMap::from([(MediaType::VIDEO, FpsTracker::new_at(MediaType::VIDEO, t0))]),
            )]),
            latest_peer_media_state: HashMap::new(),
            latest_peer_decode_eligibility: HashMap::new(),
            on_stats_update: None,
            last_report_time: t0,
            report_interval_ms: 500,
            packet_handler: None,
            receiver: rx,
            userid: "me".to_string(),
        };

        let mut bus = subscribe();
        worker.send_diagnostic_packets_at(t0 + 3000.0);

        let mut seen: Option<Vec<(&'static str, u64)>> = None;
        while let Ok(event) = bus.try_recv() {
            let is_ours = event.metrics.iter().any(|m| {
                m.name == "to_peer" && matches!(&m.value, MetricValue::Text(s) if s == peer_id)
            });
            if is_ours {
                seen = Some(
                    event
                        .metrics
                        .iter()
                        .filter_map(|m| match &m.value {
                            MetricValue::U64(v) => Some((m.name, *v)),
                            _ => None,
                        })
                        .collect(),
                );
            }
        }
        let seen = seen.expect("the video DiagEvent for this peer must be broadcast");
        assert!(
            seen.contains(&("freeze_episodes_total", 1)),
            "the emitted event must carry the opened episode, got {seen:?}"
        );
        assert!(
            seen.contains(&("freeze_ms_total", 3000)),
            "the emitted event must carry the integrated freeze ms, got {seen:?}"
        );
        assert!(
            seen.contains(&("max_decode_gap_ms", 3000)),
            "the emitted event must carry the max decode gap, got {seen:?}"
        );
    }

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
        tracker.track_frame_with_size(100, false, true, None);
        tracker.track_frame_with_size(100, true, true, None);
        tracker.track_frame_with_size(100, false, true, None);

        // Back-date the window anchor so the NEXT call rolls over and publishes the
        // fps for the window above, rather than waiting a real second. Rollover is
        // independent of `decoded`, so a skipped arrival can drive it.
        tracker.last_bitrate_update = Date::now() - 1000.0;
        let (fps, bitrate) = tracker.track_frame_with_size(100, false, true, None);

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
        let (fps, bitrate) = tracker.track_frame_with_size(100, true, true, None);
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
        tracker.track_frame_with_size(100, false, true, None);

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
        tracker.track_frame_with_size(100, false, true, None);
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
        let (fps, bitrate) = tracker.track_frame_with_size(100, true, true, None);
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
