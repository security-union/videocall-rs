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
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::clock::{default_clock, Clock};
use crate::constants::{
    cap_layers_to_budget, screen_share_camera_ceiling_index, simulcast_layers, uplink_budget_kbps,
    AudioQualityTier, VideoQualityTier, AQ_OUTLIER_GAP_FPS_RATIO, AQ_OUTLIER_HEALTH_FPS_RATIO,
    MAX_BITRATE_SLEW_KBPS_PER_SEC, PID_CORRECTION_THROTTLE_MS, PID_DEADBAND_FPS,
    PID_FPS_HISTORY_SIZE, PID_KD, PID_KI, PID_KP, PID_MAX_JITTER_PENALTY, PID_OUTPUT_MAX,
    PID_OUTPUT_MIN, VIDEO_QUALITY_TIERS,
};
use crate::manager::{AdaptiveQualityManager, TierTransitionRecord};
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;

const WINDOW_DURATION_SEC: u32 = 10;
const INACTIVE_TIMEOUT_SEC: u32 = 20;
const AQ_SUMMARY_INTERVAL_MS: f64 = 30_000.0;
const PID_STUCK_THRESHOLD_MS: f64 = 30_000.0;

/// EncoderControl is responsible for bridging the gap between the encoder and the
/// diagnostics system.
/// It closes the loop by allowing the encoder to adjust its settings based on
/// feedback from the diagnostics system.
#[derive(Debug, Clone)]
pub enum EncoderControl {
    UpdateBitrate { target_bitrate_kbps: u32 },
}

/// A window of diagnostic packet data for a single peer
pub struct DiagnosticPacketWindow {
    /// Diagnostic packets received within the window, with their timestamps
    packets: Vec<(f64, DiagnosticsPacket)>,
    /// Window duration in milliseconds
    window_duration_ms: f64,
    /// Last time the window was cleaned up
    last_cleanup: f64,
}

impl DiagnosticPacketWindow {
    pub fn new(window_duration_sec: u32) -> Self {
        Self {
            packets: Vec::new(),
            window_duration_ms: window_duration_sec as f64 * 1000.0,
            // Initialize to 0 so the first add_packet always triggers a cleanup pass.
            // Using Date::now() here would couple the struct to wall-clock time and
            // break any caller that passes simulated or monotonic timestamps.
            last_cleanup: 0.0,
        }
    }

    /// Add a packet to the window
    pub fn add_packet(&mut self, timestamp: f64, packet: DiagnosticsPacket) {
        self.packets.push((timestamp, packet));
        // Clean up old packets if we haven't done so in a while
        if timestamp - self.last_cleanup > 1000.0 {
            self.cleanup(timestamp);
        }
    }

    /// Remove packets older than the window duration
    fn cleanup(&mut self, current_time: f64) {
        let cutoff = current_time - self.window_duration_ms;
        self.packets.retain(|(ts, _)| *ts >= cutoff);
        self.last_cleanup = current_time;
    }

    /// Get the latest received FPS from this peer
    pub fn latest_fps(&self) -> Option<f64> {
        if let Some((_, packet)) = self.packets.last() {
            let fps = match packet.media_type.enum_value_or_default() {
                MediaType::VIDEO => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::AUDIO => packet.audio_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::SCREEN => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                _ => None,
            };
            return fps;
        }
        None
    }

    /// Get the average FPS received within the window.
    ///
    /// Uses average instead of minimum so that a single bad 1-second sample
    /// does not poison the entire 10-second window. Average smooths transient
    /// spikes while still reflecting sustained problems.
    ///
    /// **Reaction latency tradeoff:** averaging over a 10-second window means
    /// that a sudden FPS collapse only fully registers after ~5–6 seconds
    /// (half the window must be filled with bad samples before the average
    /// crosses the degradation threshold). Combined with `STEP_DOWN_REACTION_TIME_MS`
    /// (1.5s), worst-case time-to-step-down is ~7s. This is intentional: the
    /// extra latency prevents tier oscillation on networks with short packet-loss
    /// bursts, at the cost of slower response to genuine sustained degradation.
    pub fn avg_fps(&self) -> Option<f64> {
        if self.packets.is_empty() {
            return None;
        }

        let mut sum_fps = 0.0;
        let mut count = 0u32;

        for (_, packet) in &self.packets {
            let fps = match packet.media_type.enum_value_or_default() {
                MediaType::VIDEO => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::AUDIO => packet.audio_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::SCREEN => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                _ => None,
            };

            if let Some(fps) = fps {
                sum_fps += fps;
                count += 1;
            }
        }

        if count > 0 {
            Some(sum_fps / count as f64)
        } else {
            None
        }
    }

    /// Get the timestamp of the most recent packet
    pub fn latest_timestamp(&self) -> Option<f64> {
        self.packets.last().map(|(ts, _)| *ts)
    }

    /// Get the number of packets in the window
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    /// Check if there are any packets in the window
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

/// Manages diagnostic packets from multiple peers
pub struct DiagnosticPackets {
    /// Map of target_id to their diagnostic packet window
    peer_windows: HashMap<String, DiagnosticPacketWindow>,
    /// Window duration in seconds
    window_duration_sec: u32,
    /// Inactive peer timeout in seconds
    inactive_timeout_sec: u32,
}

impl DiagnosticPackets {
    pub fn new(window_duration_sec: u32, inactive_timeout_sec: u32) -> Self {
        Self {
            peer_windows: HashMap::new(),
            window_duration_sec,
            inactive_timeout_sec,
        }
    }

    /// Process a new diagnostic packet
    pub fn process_packet(&mut self, packet: DiagnosticsPacket, now: f64) {
        let target_id = packet.target_id.clone();

        // Add packet to the corresponding window
        let window = self
            .peer_windows
            .entry(target_id)
            .or_insert_with(|| DiagnosticPacketWindow::new(self.window_duration_sec));

        window.add_packet(now, packet);

        // Clean up inactive peers
        self.remove_inactive_peers(now);
    }

    /// Remove peers that haven't sent packets recently
    fn remove_inactive_peers(&mut self, now: f64) {
        let inactive_cutoff = now - (self.inactive_timeout_sec as f64 * 1000.0);
        self.peer_windows.retain(|_, window| {
            if let Some(latest) = window.latest_timestamp() {
                latest >= inactive_cutoff
            } else {
                false
            }
        });
    }

    /// Get the p75 FPS across all reporting peers.
    ///
    /// Collects each peer's average FPS (within their diagnostic window), sorts
    /// them, and returns the 75th percentile value together with the number of
    /// peers that actually contributed FPS data ("effective count"). With 5
    /// peers, the bottom ~25% (outliers with unusually low FPS) have minimal
    /// influence on the result, preventing a single bad receiver from tanking
    /// sender quality.
    ///
    /// The effective count may be less than `peer_windows.len()` when some peers
    /// have no usable FPS data (e.g., they joined but haven't produced video
    /// metrics yet). Callers should use this effective count — not the raw
    /// window count — for threshold selection so that the lenient/strict
    /// boundary stays consistent with the actual data feeding the p75.
    ///
    /// **Small-n behavior:** at n=3 the 0.75 quantile collapses to the median
    /// (index `floor(2 * 0.75)` = 1 out of \[0,1,2\]). For n=1 the method
    /// returns that peer's value. For **n=2** it applies an outlier guard
    /// (issue #1012): if exactly one of the two peers is genuinely healthy
    /// (`>= health_floor_fps`) and the other is a clear outlier below it
    /// (`lower < higher * AQ_OUTLIER_GAP_FPS_RATIO`), it returns the *healthy*
    /// peer's value — so a single constrained receiver cannot define the PID
    /// setpoint and drag the sender's bitrate down for everyone. When no peer
    /// clears the health floor (all struggling = real congestion) or the two
    /// are close (no clear outlier), it keeps the conservative minimum, exactly
    /// as before. The adaptive quality manager still uses
    /// `VIDEO_TIER_DEGRADE_FPS_RATIO_LENIENT` (0.30 vs 0.50) when
    /// `effective_peer_count < 3`, complementing this input-side guard.
    ///
    /// `health_floor_fps` is the absolute FPS at/above which a peer counts as
    /// healthy (the caller passes `target_fps * AQ_OUTLIER_HEALTH_FPS_RATIO`).
    /// A non-positive floor disables the rescue (falls back to minimum), so an
    /// unknown/zero target can never spuriously raise the setpoint.
    pub fn get_p75_fps(&self, health_floor_fps: f64) -> Option<(f64, usize)> {
        if self.peer_windows.is_empty() {
            return None;
        }

        let mut fps_values: Vec<f64> = self
            .peer_windows
            .values()
            .filter_map(|window| window.avg_fps())
            .filter(|v| v.is_finite())
            .collect();

        if fps_values.is_empty() {
            return None;
        }

        let n = fps_values.len();
        if n == 1 {
            // Single peer: its value is the only signal we have. Can't tell an
            // outlier from the truth, so return it verbatim.
            return Some((fps_values[0], n));
        }
        if n == 2 {
            let (lo, hi) = if fps_values[0] <= fps_values[1] {
                (fps_values[0], fps_values[1])
            } else {
                (fps_values[1], fps_values[0])
            };
            // Outlier guard (#1012): rescue the setpoint toward the healthy peer
            // ONLY when one peer is genuinely healthy AND the other is a clear
            // outlier well below it. Otherwise (both low = real congestion, or
            // both close = no outlier) keep the conservative minimum.
            let healthy_peer_exists = health_floor_fps > 0.0 && hi >= health_floor_fps;
            let clear_outlier = lo < hi * AQ_OUTLIER_GAP_FPS_RATIO;
            if healthy_peer_exists && clear_outlier {
                return Some((hi, n));
            }
            return Some((lo, n));
        }

        // Sort ascending and pick the 75th percentile.
        fps_values.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p75_index = ((n - 1) as f64 * 0.75).floor() as usize;
        Some((fps_values[p75_index], n))
    }

    /// Get all active peer IDs
    pub fn get_peer_ids(&self) -> Vec<String> {
        self.peer_windows.keys().cloned().collect()
    }

    /// Get the number of active peers
    pub fn peer_count(&self) -> usize {
        self.peer_windows.len()
    }
}

/// Upper bitrate clamp (kbps) for one simulcast layer (issue #989, PR B).
///
/// Mirrors the single-stream drain-hold pin in
/// [`EncoderBitrateController::process_diagnostics_packet_with_time`]: while a
/// self-targeted CONGESTION drain hold is active, the layer's upper bound is its
/// tier **ideal** (clamped up to at least `min_bitrate_kbps` for degenerate
/// tiers) so the layer cannot ramp bitrate back up and re-fill the draining
/// relay buffer; otherwise it is the tier **max**. Extracted as a pure function
/// so the pin decision is unit-testable independently of PID arithmetic.
fn layer_upper_clamp_kbps(tier: &VideoQualityTier, congestion_hold: bool) -> f64 {
    if congestion_hold {
        (tier.ideal_bitrate_kbps as f64).max(tier.min_bitrate_kbps as f64)
    } else {
        tier.max_bitrate_kbps as f64
    }
}

/// Per-simulcast-layer bitrate PID state (issue #989, PR B).
///
/// Each active simulcast layer encodes at a *fixed* resolution (its
/// `SIMULCAST_LAYER_TIERS` rung) and a per-layer adaptive bitrate. This struct
/// owns one layer's PID + slew state so a layer's bitrate can be fine-tuned
/// inside its own tier's `[min, max]` band, independently of the other layers.
///
/// The single fps setpoint (p75 across peers, shared by all layers) still
/// drives the *drop/add-top-layer* decision in the manager; the per-layer PIDs
/// only fine-tune bitrate *within* the active layers. This mirrors the
/// single-stream design where one PID fine-tunes bitrate and the manager
/// selects the tier — here the manager selects the active-layer count and each
/// active layer runs the same clamp/slew logic against its own fixed tier.
struct LayerPidState {
    /// This layer's tier (fixed resolution + bitrate bounds). Lives in the
    /// `&'static` simulcast ladder so a reference is cheap and stable.
    tier: &'static VideoQualityTier,
    /// Per-layer PID controller (same gains/limits as the single-stream PID).
    pid: pidgeon::PidController,
    /// Last bitrate (kbps) emitted for this layer, for the slew limiter.
    last_target_bitrate_kbps: f64,
}

impl LayerPidState {
    fn new(tier: &'static VideoQualityTier, initial_target_fps: f64) -> Self {
        let cfg = pidgeon::ControllerConfig::builder()
            .with_kp(PID_KP)
            .with_ki(PID_KI)
            .with_kd(PID_KD)
            .with_setpoint(initial_target_fps)
            .with_deadband(PID_DEADBAND_FPS)
            .with_output_limits(PID_OUTPUT_MIN, PID_OUTPUT_MAX)
            .with_anti_windup(true)
            .build()
            .expect("per-layer PID controller config is valid");
        Self {
            tier,
            pid: pidgeon::PidController::new(cfg),
            // Seed at the tier ideal so the first slew step starts from a sane
            // baseline rather than 0 (which would bypass the slew limiter).
            last_target_bitrate_kbps: tier.ideal_bitrate_kbps as f64,
        }
    }
}

pub struct EncoderBitrateController {
    pid: pidgeon::PidController,
    last_update: f64,
    ideal_bitrate_kbps: u32,
    target_fps: Arc<AtomicU32>,
    fps_history: std::collections::VecDeque<f64>,
    max_history_size: usize,
    initialization_complete: bool,
    diagnostic_packets: DiagnosticPackets,
    last_correction_time: f64,
    correction_throttle_ms: f64,
    /// One-shot flag that forces the *next* call to
    /// [`Self::process_diagnostics_packet_with_time`] to bypass the PID
    /// correction throttle so a freshly-computed (lower) bitrate target reaches
    /// the encoder in the same diagnostics-loop iteration.
    ///
    /// Set by [`Self::force_congestion_cut`] (issue #702): the aggressive cut
    /// drops resolution/keyframe/audio tiers immediately via `take_tier_changed`,
    /// but the encoder's actual bitrate target only updates from the value
    /// `process_diagnostics_packet` returns. Without this bypass, a CONGESTION
    /// signal arriving <1s after any prior AQ correction would be throttled
    /// (returns `None`), leaving the rate-controlled codec targeting the old,
    /// higher bitrate for up to ~1s — roughly the same bytes/sec on the wire,
    /// defeating the buffer-drain intent of the cut. The flag is consumed (reset
    /// to `false`) the next time the throttle guard is evaluated, so it only ever
    /// bypasses a single correction and never weakens the normal 1s PID throttle.
    force_next_correction: bool,
    /// Adaptive quality state machine for tier selection.
    quality_manager: AdaptiveQualityManager,
    /// Set to `true` after any tier transition, cleared by the caller via
    /// [`Self::take_tier_changed`].
    tier_changed: bool,
    /// Last computed fps_ratio for external observation.
    last_fps_ratio: f64,
    /// Last p75 peer FPS for external observation.
    last_p75_peer_fps: f64,
    /// Last computed bitrate_ratio for external observation.
    last_bitrate_ratio: f64,
    /// Last PID target bitrate for external observation.
    last_target_bitrate_kbps: f64,
    /// Timestamp (ms) of the last AQ_STATUS summary log emission.
    last_aq_summary_ms: f64,
    /// Timestamp (ms) when PID output first hit PID_OUTPUT_MAX.
    pid_saturated_since_ms: Option<f64>,
    /// Clock used for all internal wall-clock reads (e.g. `last_update`,
    /// congestion-throttle timestamps, force-step-down timings). The browser
    /// path uses [`JsDateClock`], native callers use [`SystemClock`], and
    /// tests can inject a [`TestClock`] for determinism.
    ///
    /// [`JsDateClock`]: crate::clock::JsDateClock
    /// [`SystemClock`]: crate::clock::SystemClock
    /// [`TestClock`]: crate::clock::TestClock
    clock: Arc<dyn Clock>,

    // --- Simulcast per-layer state (issue #989, PR B) — ADDITIVE ---
    /// Per-layer PID/slew state, one entry per simulcast layer in the ladder
    /// (lowest layer first, index == `layer_id`). Empty in single-stream mode,
    /// in which case every simulcast code path is skipped and the controller
    /// behaves exactly as before. Populated by
    /// [`set_simulcast_layers`](Self::set_simulcast_layers).
    layer_pids: Vec<LayerPidState>,
    /// Last per-layer target bitrates (kbps), `layer_pids.len()` entries.
    /// Only the first `active_layer_count` are meaningful for the encoder; the
    /// rest are retained so a layer that is re-added resumes near its last
    /// target rather than jumping. Empty in single-stream mode.
    last_layer_target_bitrates_kbps: Vec<f64>,
}

impl EncoderBitrateController {
    /// Create a new bitrate controller using the default `VIDEO_QUALITY_TIERS`.
    pub fn new(ideal_bitrate_kbps: u32, target_fps: Arc<AtomicU32>) -> Self {
        Self::with_clock(ideal_bitrate_kbps, target_fps, default_clock())
    }

    /// Create a new bitrate controller with an injected [`Clock`].
    ///
    /// Native callers (e.g. the load-test bot) can pass a [`SystemClock`];
    /// tests can pass a [`TestClock`] for deterministic timestamps.
    ///
    /// [`SystemClock`]: crate::clock::SystemClock
    /// [`TestClock`]: crate::clock::TestClock
    pub fn with_clock(
        ideal_bitrate_kbps: u32,
        target_fps: Arc<AtomicU32>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let quality_manager =
            AdaptiveQualityManager::with_clock(VIDEO_QUALITY_TIERS, Arc::clone(&clock));
        Self::build(ideal_bitrate_kbps, target_fps, quality_manager, clock)
    }

    /// Create a new bitrate controller for screen share.
    ///
    /// Uses `SCREEN_QUALITY_TIERS` starting at `DEFAULT_SCREEN_TIER_INDEX`
    /// (medium/720p) via [`AdaptiveQualityManager::new_for_screen`]. The
    /// initial `ideal_bitrate_kbps` is synced from the starting tier so the
    /// PID controller does not make an unnecessary correction on the first
    /// update.
    pub fn new_for_screen(
        target_fps: Arc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
    ) -> Self {
        Self::new_for_screen_with_clock(target_fps, video_tiers, default_clock())
    }

    /// Create a new screen-share bitrate controller with an injected [`Clock`].
    pub fn new_for_screen_with_clock(
        target_fps: Arc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
        clock: Arc<dyn Clock>,
    ) -> Self {
        let quality_manager =
            AdaptiveQualityManager::new_for_screen_with_clock(video_tiers, Arc::clone(&clock));
        let tier_ideal = quality_manager.current_video_tier().ideal_bitrate_kbps;
        Self::build(tier_ideal, target_fps, quality_manager, clock)
    }

    /// Internal constructor shared by `new` and `new_for_screen`.
    fn build(
        ideal_bitrate_kbps: u32,
        target_fps: Arc<AtomicU32>,
        quality_manager: AdaptiveQualityManager,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let initial_target = target_fps.load(Ordering::Relaxed) as f64;

        // Configure PID: setpoint = target FPS, process_value = received FPS.
        // The controller computes error = setpoint − process_value internally.
        let controller_config = pidgeon::ControllerConfig::builder()
            .with_kp(PID_KP)
            .with_ki(PID_KI)
            .with_kd(PID_KD)
            .with_setpoint(initial_target)
            .with_deadband(PID_DEADBAND_FPS)
            .with_output_limits(PID_OUTPUT_MIN, PID_OUTPUT_MAX)
            .with_anti_windup(true)
            .build()
            .expect("PID controller config is valid");

        let pid = pidgeon::PidController::new(controller_config);
        let diagnostic_packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);

        let now = clock.now_ms();
        Self {
            pid,
            last_update: now,
            ideal_bitrate_kbps,
            target_fps,
            fps_history: std::collections::VecDeque::with_capacity(PID_FPS_HISTORY_SIZE),
            max_history_size: PID_FPS_HISTORY_SIZE,
            initialization_complete: false,
            diagnostic_packets,
            last_correction_time: 0.0,
            correction_throttle_ms: PID_CORRECTION_THROTTLE_MS,
            force_next_correction: false,
            quality_manager,
            tier_changed: false,
            last_fps_ratio: 0.0,
            last_p75_peer_fps: 0.0,
            last_bitrate_ratio: 0.0,
            last_target_bitrate_kbps: 0.0,
            last_aq_summary_ms: 0.0,
            pid_saturated_since_ms: None,
            clock,
            // Simulcast: single-stream by default (empty vecs). Enabled via
            // set_simulcast_layers().
            layer_pids: Vec::new(),
            last_layer_target_bitrates_kbps: Vec::new(),
        }
    }

    /// Enable simulcast for this controller with an `n`-layer ladder
    /// (issue #989, PR B).
    ///
    /// Configures the quality manager's active-layer state and builds one
    /// per-layer PID against the corresponding [`simulcast_layers`] rung.
    /// `n == 1` (or `0`) leaves the controller in single-stream mode — the
    /// per-layer vecs stay empty and every existing code path is unchanged,
    /// which is exactly what the bot and all current callers get.
    ///
    /// Call once after construction, before the first diagnostics packet.
    pub fn set_simulcast_layers(&mut self, n: usize) {
        // Clamp + configure the manager (single source of truth for the count).
        self.quality_manager.set_simulcast_layers(n);
        let effective = self.quality_manager.simulcast_layer_count();
        if effective <= 1 {
            // Single-stream mode: nothing to build, behave exactly as before.
            self.layer_pids.clear();
            self.last_layer_target_bitrates_kbps.clear();
            return;
        }
        let initial_target = self.target_fps.load(Ordering::Relaxed) as f64;
        let tiers = simulcast_layers(effective);
        self.layer_pids = tiers
            .iter()
            .map(|tier| LayerPidState::new(tier, initial_target))
            .collect();
        self.last_layer_target_bitrates_kbps =
            tiers.iter().map(|t| t.ideal_bitrate_kbps as f64).collect();
    }

    // Calculate the standard deviation of FPS values to measure jitter
    fn calculate_jitter(&self) -> f64 {
        if self.fps_history.len() < 2 {
            return 0.0; // Not enough samples to calculate jitter
        }

        // Calculate frame-to-frame differences
        let differences: Vec<f64> = self
            .fps_history
            .iter()
            .zip(self.fps_history.iter().skip(1))
            .map(|(&a, &b)| (b - a).abs())
            .collect();

        // Calculate mean of absolute differences
        let sum: f64 = differences.iter().sum();

        sum / differences.len() as f64
    }

    pub fn process_diagnostics_packet_with_time(
        &mut self,
        packet: DiagnosticsPacket,
        now: f64,
    ) -> Option<f64> {
        // Add the packet to our diagnostic packet manager
        self.diagnostic_packets.process_packet(packet.clone(), now);

        // Apply throttling - check if sufficient time has passed since last correction.
        // A one-shot bypass (armed by force_congestion_cut, issue #702) lets the
        // aggressive cut's lower bitrate target land immediately instead of waiting
        // out the throttle; it is consumed here so it never weakens the normal PID
        // throttle on subsequent corrections.
        let force_correction = std::mem::take(&mut self.force_next_correction);
        let time_since_last_correction = now - self.last_correction_time;
        if !force_correction && time_since_last_correction < self.correction_throttle_ms {
            log::debug!(
                "Throttling bitrate correction: {:.0}ms since last correction (throttle: {:.0}ms)",
                time_since_last_correction,
                self.correction_throttle_ms
            );
            return None; // Too soon since last correction, don't emit a new one
        }

        let target_fps = self.target_fps.load(Ordering::Relaxed) as f64;

        // Get the p75 FPS across all reporting peers. effective_peer_count is
        // the number of peers that contributed FPS data,
        // which may be less than peer_windows.len() (some peers may lack metrics).
        // The health floor lets the small-peer-count outlier guard (#1012)
        // distinguish "one constrained receiver" from "everyone is struggling".
        let health_floor_fps = target_fps * AQ_OUTLIER_HEALTH_FPS_RATIO;
        let (worst_fps, effective_peer_count) =
            self.diagnostic_packets.get_p75_fps(health_floor_fps)?;
        self.last_p75_peer_fps = worst_fps;

        let fps_received = worst_fps.min(target_fps);
        if target_fps <= 0.0 {
            self.last_correction_time = now;
            return Some(self.ideal_bitrate_kbps as f64);
        }

        // Keep setpoint in sync with the (potentially changing) target FPS
        if let Err(e) = self.pid.set_setpoint(target_fps) {
            log::warn!("Failed to update PID setpoint: {e}");
        }

        self.fps_history.push_back(fps_received);
        while self.fps_history.len() > self.max_history_size {
            self.fps_history.pop_front();
        }

        let jitter = self.calculate_jitter();
        let dt = (now - self.last_update) / 1000.0; // Convert ms to seconds
        self.last_update = now;

        // Wait for a few samples before reacting
        if !self.initialization_complete {
            if self.fps_history.len() >= 3 {
                self.initialization_complete = true;
            } else {
                self.last_correction_time = now;
                return Some(self.ideal_bitrate_kbps as f64);
            }
        }

        // PID internally computes error = setpoint (target_fps) − process_value (fps_received).
        // Output is clamped to [0, 50] by the controller config.
        let pid_output = match self.pid.compute(fps_received, dt) {
            Ok(output) => output,
            Err(e) => {
                log::debug!("PID compute error (dt={dt:.3}): {e}");
                0.0
            }
        };

        // The PID integral never decays on its own when error is within deadband.
        // Reset the controller when conditions are good so bitrate can recover.
        let current_error = target_fps - fps_received;
        if current_error.abs() <= PID_DEADBAND_FPS && pid_output > 0.0 {
            self.pid.reset();
            self.pid_saturated_since_ms = None;
        }

        // Stuck-PID watchdog: if output has been at maximum for too long,
        // force a reset. This breaks the feedback loop where reduced bitrate
        // causes low received FPS which keeps the PID saturated indefinitely.
        // 0.01 tolerance: PID integrator may asymptotically approach the max
        // without reaching it exactly due to f64 accumulation rounding.
        if pid_output >= PID_OUTPUT_MAX - 0.01 {
            let saturated_since = *self.pid_saturated_since_ms.get_or_insert(now);
            if now - saturated_since >= PID_STUCK_THRESHOLD_MS {
                log::warn!(
                    "AQ_PID_STUCK: output at maximum for {:.0}s, forcing reset",
                    (now - saturated_since) / 1000.0,
                );
                self.pid.reset();
                self.pid_saturated_since_ms = None;
            }
        } else {
            self.pid_saturated_since_ms = None;
        }

        let base_bitrate = self.ideal_bitrate_kbps as f64;
        let min_bitrate = base_bitrate * 0.1;
        let max_bitrate = base_bitrate * 1.5;

        // Map PID output [0, PID_OUTPUT_MAX] -> bitrate reduction [0%, 90%].
        let reduction_pct = pid_output / PID_OUTPUT_MAX * 0.9;
        let after_pid = base_bitrate * (1.0 - reduction_pct);

        // Additional jitter penalty: up to PID_MAX_JITTER_PENALTY reduction at maximum jitter.
        let normalized_jitter = jitter / target_fps;
        let jitter_factor = (normalized_jitter * 5.0).min(1.0);
        let jitter_reduction = after_pid * (jitter_factor * PID_MAX_JITTER_PENALTY);
        let corrected_bitrate = after_pid - jitter_reduction;

        let current_error = target_fps - fps_received;
        log::debug!(
            "FPS: target={:.1} received={:.1} error={:.1} | PID={:.2} reduction={:.1}% | \
             Jitter={:.2} factor={:.2} | Bitrate: base={:.0} corrected={:.0} | Peers: {}",
            target_fps,
            fps_received,
            current_error,
            pid_output,
            reduction_pct * 100.0,
            jitter,
            jitter_factor,
            base_bitrate,
            corrected_bitrate,
            self.diagnostic_packets.peer_count()
        );

        // Clamp to bounds; fall back to base on NaN.
        let final_bitrate = if corrected_bitrate.is_nan() {
            base_bitrate
        } else {
            corrected_bitrate.clamp(min_bitrate, max_bitrate)
        };

        // --- Adaptive quality manager: tier selection ---
        // Feed the same signals to the quality manager for tier transitions.
        // Use effective_peer_count (peers with FPS data) rather than
        // peer_windows.len() so the lenient/strict threshold stays consistent
        // with the actual peers feeding the p75 aggregation.
        let tier = self.quality_manager.current_video_tier();
        let ideal_for_tier = tier.ideal_bitrate_kbps as f64;
        // Capture the tier index BEFORE update() so that, in simulcast mode, we
        // can translate the manager's incident-hardened degrade/recover decision
        // (which moves video_tier_index) into a top-layer drop/add. This reuses
        // ALL the existing hysteresis / crash-ceiling / yo-yo / re-election /
        // congestion-hold gating for free, without changing tier semantics.
        let tier_index_before = self.quality_manager.video_tier_index();
        let tier_changed = self.quality_manager.update(
            fps_received,
            target_fps,
            final_bitrate,
            ideal_for_tier,
            now,
            effective_peer_count,
        );
        // Simulcast layer re-target (issue #989, PR B): a step DOWN sheds the
        // top active layer; a step UP restores it. The single fps setpoint
        // (p75) that drove the tier decision drives this. No-op in single-stream
        // mode (is_simulcast() == false).
        if self.quality_manager.is_simulcast() && tier_changed {
            let tier_index_after = self.quality_manager.video_tier_index();
            if tier_index_after > tier_index_before {
                if self.quality_manager.drop_top_layer() {
                    self.tier_changed = true;
                }
            } else if tier_index_after < tier_index_before && self.quality_manager.add_top_layer() {
                self.tier_changed = true;
            }
        }
        if tier_changed {
            self.tier_changed = true;
            // Update internal ideal bitrate to match the new tier so PID
            // operates within the new tier's range going forward.
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            self.pid.reset();
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: pid)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }

        // Clamp the PID output to the current tier's bitrate bounds.
        let tier = self.quality_manager.current_video_tier();
        let tier_min = tier.min_bitrate_kbps as f64;
        // While a self-targeted CONGESTION drain hold is active, pin the PID's
        // max output to the tier's *ideal* (lower bound of the usable range)
        // rather than its max, so the PID cannot ramp bitrate back up within
        // the new tier and re-fill the still-draining relay buffer. The hold
        // expires by timestamp, after which the normal tier max applies again.
        let tier_max = if self.quality_manager.congestion_hold_active(now) {
            (tier.ideal_bitrate_kbps as f64).max(tier_min)
        } else {
            tier.max_bitrate_kbps as f64
        };
        let tier_clamped = final_bitrate.clamp(tier_min, tier_max);

        let outside_previous_tier =
            self.last_target_bitrate_kbps > tier_max || self.last_target_bitrate_kbps < tier_min;
        let bypass_slew = tier_changed
            || outside_previous_tier
            || self.last_target_bitrate_kbps <= 0.0
            || dt <= 0.0;
        let slewed = if bypass_slew {
            tier_clamped
        } else {
            let max_delta = MAX_BITRATE_SLEW_KBPS_PER_SEC as f64 * dt;
            tier_clamped.clamp(
                self.last_target_bitrate_kbps - max_delta,
                self.last_target_bitrate_kbps + max_delta,
            )
        };

        // Store encoder decision inputs for external observation (health reporting).
        self.last_fps_ratio = fps_received / target_fps;
        self.last_bitrate_ratio = slewed / ideal_for_tier;
        self.last_target_bitrate_kbps = slewed;

        let should_log_summary =
            tier_changed || (now - self.last_aq_summary_ms >= AQ_SUMMARY_INTERVAL_MS);
        if should_log_summary {
            self.last_aq_summary_ms = now;
            let video_tier = self.quality_manager.current_video_tier();
            let audio_tier = self.quality_manager.current_audio_tier();
            log::info!(
                "AQ_STATUS: video_tier={}({}) audio_tier={}({}) base_bitrate={} corrected_bitrate={:.0} \
                 fps_ratio={:.2} bitrate_ratio={:.2} peers={}",
                video_tier.label, self.quality_manager.video_tier_index(),
                audio_tier.label, self.quality_manager.audio_tier_index(),
                self.ideal_bitrate_kbps, slewed,
                self.last_fps_ratio, self.last_bitrate_ratio,
                self.diagnostic_packets.peer_count(),
            );
        }

        // --- Per-layer bitrate PID (issue #989, PR B) ---
        // In simulcast mode, additionally compute a per-layer target bitrate for
        // every layer in the ladder, each clamped to its OWN fixed tier's
        // [min, max] band and slew-limited like the single-stream path. The
        // single fps setpoint (`fps_received`) drives all layers; the manager
        // (above) decides how many are active. The encoder only encodes+sends
        // the first `active_layer_count` of these. No-op in single-stream mode.
        if self.quality_manager.is_simulcast() && !self.layer_pids.is_empty() {
            // Mirror the single-stream drain-hold pin on the per-layer axis: while
            // a self-targeted CONGESTION drain hold is active, each layer's max is
            // pinned to its tier *ideal* (not its tier max) so surviving layers
            // cannot ramp back up and re-fill the still-draining relay buffer.
            // Computed here (not inside the mutable layer loop) to avoid borrowing
            // `quality_manager` while `layer_pids` is mutably borrowed.
            let congestion_hold = self.quality_manager.congestion_hold_active(now);
            let active = self.quality_manager.active_layer_count();
            self.compute_layer_bitrates(
                fps_received,
                target_fps,
                jitter,
                dt,
                congestion_hold,
                active,
            );
        }

        self.last_correction_time = now;
        Some(slewed)
    }

    /// Compute and store per-layer target bitrates for the active simulcast
    /// ladder (issue #989, PR B). Reuses the same PID→reduction→jitter→clamp→slew
    /// pipeline as the single-stream path, but per layer against the layer's own
    /// fixed tier bounds. Called only in simulcast mode.
    ///
    /// `congestion_hold` mirrors the single-stream drain-hold pin: when `true`,
    /// each layer's upper clamp is its tier *ideal* rather than its tier max, so
    /// the surviving layers cannot ramp bitrate back up during a self-targeted
    /// CONGESTION drain hold and re-fill the relay buffer the cut is draining.
    fn compute_layer_bitrates(
        &mut self,
        fps_received: f64,
        target_fps: f64,
        jitter: f64,
        dt: f64,
        congestion_hold: bool,
        active: usize,
    ) {
        for i in 0..self.layer_pids.len() {
            // Keep each layer's setpoint synced to the (possibly changing) target.
            if let Err(e) = self.layer_pids[i].pid.set_setpoint(target_fps) {
                log::warn!("Failed to update layer {i} PID setpoint: {e}");
            }

            let layer = &mut self.layer_pids[i];
            let base = layer.tier.ideal_bitrate_kbps as f64;

            let pid_output = match layer.pid.compute(fps_received, dt) {
                Ok(o) => o,
                Err(e) => {
                    log::debug!("layer {i} PID compute error (dt={dt:.3}): {e}");
                    0.0
                }
            };
            // Reset the layer PID when error is within deadband so its bitrate
            // can recover (mirrors the single-stream reset).
            let current_error = target_fps - fps_received;
            if current_error.abs() <= PID_DEADBAND_FPS && pid_output > 0.0 {
                layer.pid.reset();
            }

            // PID output [0, MAX] -> reduction [0%, 90%], plus jitter penalty.
            let reduction_pct = pid_output / PID_OUTPUT_MAX * 0.9;
            let after_pid = base * (1.0 - reduction_pct);
            let normalized_jitter = if target_fps > 0.0 {
                jitter / target_fps
            } else {
                0.0
            };
            let jitter_factor = (normalized_jitter * 5.0).min(1.0);
            let jitter_reduction = after_pid * (jitter_factor * PID_MAX_JITTER_PENALTY);
            let corrected = after_pid - jitter_reduction;

            // Clamp to THIS layer's fixed tier bounds. While a CONGESTION drain
            // hold is active, pin the upper bound to the tier ideal (not max) so
            // the layer cannot ramp back up and re-fill the draining buffer —
            // exactly mirroring the single-stream pin in the main process loop.
            let tier_min = layer.tier.min_bitrate_kbps as f64;
            let tier_max = layer_upper_clamp_kbps(layer.tier, congestion_hold);
            let clamped = if corrected.is_nan() {
                base
            } else {
                corrected.clamp(tier_min, tier_max)
            };

            // Slew-limit per layer (skip on first sample / dt<=0).
            let prev = layer.last_target_bitrate_kbps;
            let slewed = if prev <= 0.0 || dt <= 0.0 {
                clamped
            } else {
                let max_delta = MAX_BITRATE_SLEW_KBPS_PER_SEC as f64 * dt;
                clamped.clamp(prev - max_delta, prev + max_delta)
            };
            layer.last_target_bitrate_kbps = slewed;
            self.last_layer_target_bitrates_kbps[i] = slewed;
        }

        // --- Sender uplink budget cap (issue #989, Phase 1) ---
        // The per-layer PIDs above each clamp to their OWN tier band, so their
        // SUM can exceed what the sender's uplink can afford (e.g. 3 layers at
        // their tier ideals = 2800 kbps). Publishing N layers costs the sum, so
        // cap the ACTIVE layers' targets to the uplink budget (sum of active
        // tier ideals), proportionally shedding bitrate above each layer's tier
        // floor. This shrinks automatically as the AQ sheds the top layer under
        // congestion (`active` drops → budget drops). Shed layers are untouched
        // (not encoded/sent). The base layer keeps its floor so it stays
        // viewable. No-op when the layers already fit (the common case at low
        // tiers), so it never disturbs an already-affordable configuration.
        let tiers = simulcast_layers(self.layer_pids.len());
        let budget = uplink_budget_kbps(tiers, active);
        cap_layers_to_budget(
            &mut self.last_layer_target_bitrates_kbps,
            tiers,
            active,
            budget,
        );
        // Keep each layer's slew baseline in sync with the capped value so the
        // next tick's slew limiter measures delta from what we actually emitted,
        // not from the pre-cap PID target (which would let the cap "snap back").
        for i in 0..active.min(self.layer_pids.len()) {
            self.layer_pids[i].last_target_bitrate_kbps = self.last_layer_target_bitrates_kbps[i];
        }
    }

    pub fn process_diagnostics_packet(&mut self, packet: DiagnosticsPacket) -> Option<f64> {
        self.process_diagnostics_packet_with_time(packet, self.clock.now_ms())
    }

    // Get the count of active peers
    pub fn peer_count(&self) -> usize {
        self.diagnostic_packets.peer_count()
    }

    // Get all active peer IDs
    pub fn peer_ids(&self) -> Vec<String> {
        self.diagnostic_packets.get_peer_ids()
    }

    /// Returns `true` if a tier transition occurred since the last call to this
    /// method, then resets the flag. Callers should use this to detect when
    /// encoder settings (resolution, fps, keyframe interval) need updating.
    pub fn take_tier_changed(&mut self) -> bool {
        std::mem::take(&mut self.tier_changed)
    }

    /// Get the current video quality tier recommendation.
    pub fn current_video_tier(&self) -> &'static VideoQualityTier {
        self.quality_manager.current_video_tier()
    }

    /// Get the current audio quality tier recommendation.
    pub fn current_audio_tier(&self) -> &'static AudioQualityTier {
        self.quality_manager.current_audio_tier()
    }

    /// Get the current video tier index.
    pub fn video_tier_index(&self) -> usize {
        self.quality_manager.video_tier_index()
    }

    /// Get the current audio tier index.
    pub fn audio_tier_index(&self) -> usize {
        self.quality_manager.audio_tier_index()
    }

    // -----------------------------------------------------------------
    // Simulcast accessors (issue #989, PR B) — ADDITIVE
    // -----------------------------------------------------------------

    /// Whether this controller is in simulcast mode (`> 1` layers).
    pub fn is_simulcast(&self) -> bool {
        self.quality_manager.is_simulcast()
    }

    /// Number of simulcast layers in the configured ladder (`1` in
    /// single-stream mode).
    pub fn simulcast_layer_count(&self) -> usize {
        self.quality_manager.simulcast_layer_count()
    }

    /// Number of simulcast layers currently active (encoded + sent). `1` in
    /// single-stream mode. The encoder shuts down layers with `layer_id >=`
    /// this value.
    pub fn active_layer_count(&self) -> usize {
        self.quality_manager.active_layer_count()
    }

    /// Per-layer target bitrates in kbps, lowest layer first (index ==
    /// `layer_id`). One entry per ladder layer; only the first
    /// [`active_layer_count`](Self::active_layer_count) are encoded by the
    /// caller, but all are returned so a re-added layer resumes near its last
    /// target. Empty in single-stream mode.
    pub fn layer_target_bitrates_kbps(&self) -> &[f64] {
        &self.last_layer_target_bitrates_kbps
    }

    /// Fixed resolution `(width, height)` for a given simulcast `layer_id`, or
    /// `None` if out of range / single-stream mode. Resolution is fixed by
    /// [`simulcast_layers`]; only the bitrate adapts.
    pub fn layer_resolution(&self, layer_id: usize) -> Option<(u32, u32)> {
        self.layer_pids
            .get(layer_id)
            .map(|l| (l.tier.max_width, l.tier.max_height))
    }

    /// Last computed fps_ratio (received / target) for health reporting.
    pub fn last_fps_ratio(&self) -> f64 {
        self.last_fps_ratio
    }

    /// Last p75 peer FPS for health reporting.
    pub fn last_p75_peer_fps(&self) -> f64 {
        self.last_p75_peer_fps
    }

    /// Last computed bitrate_ratio (tier_clamped / ideal_for_tier) for health reporting.
    pub fn last_bitrate_ratio(&self) -> f64 {
        self.last_bitrate_ratio
    }

    /// Last PID target bitrate (kbps) for health reporting.
    pub fn last_target_bitrate_kbps(&self) -> f64 {
        self.last_target_bitrate_kbps
    }

    /// Force an immediate video quality step-down due to server congestion.
    ///
    /// Delegates to [`AdaptiveQualityManager::force_video_step_down`].
    /// Returns `true` if the tier actually changed.
    pub fn force_video_step_down(&mut self) -> bool {
        let now = self.clock.now_ms();
        // Capture the forced-transition guard state BEFORE the manager call,
        // since a successful step-down updates last_transition_time_ms (which
        // would then make forced_transition_guards_clear read false on the same
        // tick). The layer shed below uses this so it fires only when the
        // request was NOT blocked by warmup / min-interval — but, unlike the
        // tier step-down, independent of the tier floor.
        let guards_clear = self.quality_manager.forced_transition_guards_clear(now);
        let changed = self.quality_manager.force_video_step_down(now);
        // Simulcast (issue #989): WS backpressure / a gentle congestion
        // step-down sheds the top active layer too. Drive this off the LAYER
        // floor, not off the manager's tier `changed` return: the manager
        // returns `false` once `video_tier_index` is at its lowest tier, but the
        // layer axis is independent of that tier floor (simulcast layers have
        // FIXED resolutions). Coupling to `changed` would stop shedding layers
        // the moment the tier index bottoms out. We still respect the warmup /
        // min-interval guards via `guards_clear` so a blocked request sheds
        // nothing. No-op in single-stream mode (drop_top_layer floors at 1).
        if guards_clear
            && self.quality_manager.is_simulcast()
            && self.quality_manager.drop_top_layer()
        {
            self.tier_changed = true;
        }
        if changed {
            self.tier_changed = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            self.pid.reset();
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: force_step_down)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }
        changed
    }

    /// Aggressively cut video quality in response to a self-targeted server
    /// CONGESTION signal (the relay is dropping our outbound packets).
    ///
    /// Delegates to [`AdaptiveQualityManager::force_congestion_cut`], which
    /// drops multiple tiers at once and arms a short drain hold that pins the
    /// PID's bitrate ceiling so the overflowing relay buffer can recover.
    /// Returns `true` if the tier actually changed.
    pub fn force_congestion_cut(&mut self) -> bool {
        let now = self.clock.now_ms();
        let changed = self.quality_manager.force_congestion_cut(now);
        // Simulcast (issue #989): a self-targeted CONGESTION cut is the
        // strongest signal — the relay is actively dropping our packets — so
        // shed the top active layer to cut egress AND sender encode CPU
        // immediately, in addition to the per-layer bitrate cut. The manager's
        // `force_congestion_cut` already drops two *tiers* in single-stream mode;
        // here we mirror that aggression on the layer axis by dropping the top
        // layer once. (A two-layer drop is not warranted: dropping one HD/SD
        // layer already sheds the bulk of the egress, and the per-layer bitrate
        // PIDs continue to throttle the remaining layers under the same drain
        // hold.)
        //
        // Gate the layer shed on the drain HOLD being active post-cut, NOT on
        // the manager's `changed` return. `force_congestion_cut` returns `false`
        // in two distinct cases: (a) blocked by the warmup / min-transition-
        // interval guard — nothing happened, no hold armed; and (b) the tier is
        // already at its floor but the hold IS armed (see manager.rs). We must
        // shed a layer in case (b) but not (a). `congestion_hold_active(now)` is
        // true iff the cut took effect cleanly — and it stays true at the tier
        // floor, so this decouples the layer axis from the tier floor while
        // still respecting the warmup / min-interval guards. No-op in
        // single-stream mode (drop_top_layer floors at 1).
        let cut_took_effect = self.quality_manager.congestion_hold_active(now);
        let layer_shed = cut_took_effect
            && self.quality_manager.is_simulcast()
            && self.quality_manager.drop_top_layer();
        if layer_shed {
            self.tier_changed = true;
            // A layer was shed but the tier index may NOT have moved (we were at
            // the tier floor). Still arm the one-shot throttle bypass so the
            // recomputed per-layer bitrates — now clamped by the just-armed drain
            // hold — reach the encoder this iteration rather than ~1s later.
            self.force_next_correction = true;
        }
        if changed {
            self.tier_changed = true;
            // Arm a one-shot throttle bypass so the next process_diagnostics_packet
            // recomputes and emits the new (lower) clamped bitrate immediately,
            // even if a prior PID correction occurred <1s ago. The held tier's
            // congestion-hold ceiling pin (controller.rs ~line 627) keeps that
            // recomputed bitrate <= the tier ideal, so the cut lands once at the
            // held tier's clamped bitrate rather than being deferred ~1s. See
            // `force_next_correction` field docs (issue #702).
            self.force_next_correction = true;
            let old_bitrate = self.ideal_bitrate_kbps;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
            self.pid.reset();
            log::info!(
                "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: congestion_cut)",
                old_bitrate,
                self.ideal_bitrate_kbps,
                new_tier.label,
                self.quality_manager.video_tier_index(),
            );
        }
        changed
    }

    /// Notify this controller that screen sharing state changed.
    ///
    /// When screen share becomes active, the camera is forced to a conservative
    /// tier and capped there to prevent bandwidth contention. When screen share
    /// stops, the cap is removed and the camera recovers naturally.
    pub fn notify_screen_sharing(&mut self, active: bool) {
        if active {
            let ceiling = screen_share_camera_ceiling_index();
            let now = self.clock.now_ms();
            let changed = self.quality_manager.force_video_step_to(ceiling, now);
            self.quality_manager.set_quality_ceiling(Some(ceiling));
            if changed {
                let old_bitrate = self.ideal_bitrate_kbps;
                let new_tier = self.quality_manager.current_video_tier();
                self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
                self.tier_changed = true;
                self.pid.reset();
                log::info!(
                    "AQ_BITRATE_CHANGE: base_bitrate {} -> {} kbps (tier: {}, index: {}, reason: screen_share_coordination)",
                    old_bitrate, self.ideal_bitrate_kbps, new_tier.label, self.quality_manager.video_tier_index(),
                );
            }
        } else {
            self.quality_manager.set_quality_ceiling(None);
        }
    }

    /// Drain tier transition records from the quality manager.
    pub fn drain_tier_transitions(&mut self) -> Vec<TierTransitionRecord> {
        self.quality_manager.drain_transitions()
    }

    /// Notify the quality manager that a server re-election completed.
    /// Suppresses crash ceiling arming for `REELECTION_CEILING_SUPPRESSION_MS`.
    ///
    /// Wired through: ConnectionManager → shared AtomicBool signal → CameraEncoder
    /// control loop (checks/clears each tick) → this method.
    pub fn notify_reelection_completed(&mut self) {
        let now = self.clock.now_ms();
        self.quality_manager.notify_reelection_completed(now);
    }

    /// Return the current crash ceiling state, if active.
    /// `(ceiling_index, tier_label, current_decay_ms)`
    pub fn crash_ceiling_info(&self) -> Option<(usize, &'static str, f64)> {
        self.quality_manager.crash_ceiling_info()
    }

    /// Return step-up blocked counters: `(ceiling, slowdown, screen_share)`.
    pub fn step_up_blocked_counts(&self) -> (u64, u64, u64) {
        self.quality_manager.step_up_blocked_counts()
    }

    /// Drain accumulated dwell time samples: `Vec<(tier_label, dwell_ms)>`.
    pub fn drain_dwell_samples(&mut self) -> Vec<(&'static str, f64)> {
        self.quality_manager.drain_dwell_samples()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::default_clock;
    use std::sync::atomic::AtomicU32;
    use std::sync::Arc;
    use videocall_types::protos::diagnostics_packet::{
        AudioMetrics, DiagnosticsPacket, VideoMetrics,
    };

    // Dual-target: on Wasm this delegates to `wasm-bindgen-test`; on native
    // the `#[test]` attribute is already correct.
    #[cfg(target_arch = "wasm32")]
    use wasm_bindgen_test::wasm_bindgen_test as test;

    // Remove browser-only configuration and make tests run in any environment
    // wasm_bindgen_test_configure!(run_in_browser);

    /// Portable "current time" for test packet timestamps. The exact value
    /// does not matter — production code only compares packet timestamps to
    /// each other — so we read through the default clock, which picks the
    /// correct backend (`Date::now` on Wasm, `SystemTime` on native).
    fn test_now_ms() -> f64 {
        default_clock().now_ms()
    }

    fn create_test_packet(
        sender_id: &str,
        target_id: &str,
        fps: f32,
        bitrate_kbps: u32,
    ) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = sender_id.to_string();
        packet.target_id = target_id.to_string();
        packet.timestamp_ms = test_now_ms() as u64;
        packet.media_type =
            videocall_types::protos::media_packet::media_packet::MediaType::VIDEO.into();

        let mut video_metrics = VideoMetrics::new();
        video_metrics.fps_received = fps;
        video_metrics.bitrate_kbps = bitrate_kbps;
        packet.video_metrics = ::protobuf::MessageField::some(video_metrics);

        packet
    }

    fn create_test_audio_packet(
        sender_id: &str,
        target_id: &str,
        fps: f32,
        bitrate_kbps: u32,
    ) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = sender_id.to_string();
        packet.target_id = target_id.to_string();
        packet.timestamp_ms = test_now_ms() as u64;
        packet.media_type =
            videocall_types::protos::media_packet::media_packet::MediaType::AUDIO.into();

        let mut audio_metrics = AudioMetrics::new();
        audio_metrics.fps_received = fps;
        audio_metrics.bitrate_kbps = bitrate_kbps;
        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);

        packet
    }

    #[test]
    fn test_happy_path() {
        // Setup
        let target_fps = Arc::new(AtomicU32::new(30));
        // Use 500 kbps as the ideal bitrate
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Use fixed timesteps to ensure we're over the throttle time
        let base_time = 1000.0;

        // Generate a series of packets with perfect conditions
        // FPS matches the target exactly, no jitter
        for i in 0..10 {
            let packet = create_test_packet("peer1", "self", 30.0, 500);
            // Each packet is sent 1100ms apart to avoid throttling
            let result = controller
                .process_diagnostics_packet_with_time(packet, base_time + (i as f64 * 1100.0));

            // With perfect conditions (no error, no jitter),
            // the bitrate should stay close to the ideal
            assert!(result.is_some(), "Packet should return a bitrate");
            if let Some(bitrate) = result {
                // Should be close to base bitrate (in kbps)
                assert!(
                    (bitrate - (ideal_bitrate_kbps) as f64).abs() < 10.0,
                    "Expected bitrate close to base ({ideal_bitrate_kbps} kbps), got {bitrate} kbps"
                );
            }
        }

        // Check history shows stable FPS
        assert_eq!(controller.fps_history.len(), 10);
        let jitter = controller.calculate_jitter();
        assert!(
            jitter < 0.1,
            "Expected near-zero jitter in happy path, got {jitter}"
        );

        // Verify we have exactly one peer
        assert_eq!(controller.peer_count(), 1);
        assert_eq!(controller.peer_ids(), vec!["self"]);
    }

    #[test]
    fn test_multiple_peers() {
        // Setup
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Base time for simulation
        let base_time = 1000.0;

        // First peer with good FPS (30 fps) - should get a result
        let good_peer_packet = create_test_packet("good_sender", "peer1", 30.0, 500);
        let result1 = controller.process_diagnostics_packet_with_time(good_peer_packet, base_time);
        assert!(result1.is_some(), "First packet should return a bitrate");

        // Send packets from other peers spaced out beyond throttle period

        // Second peer with average FPS (20 fps)
        let avg_peer_packet = create_test_packet("avg_sender", "peer2", 20.0, 500);
        let result2 =
            controller.process_diagnostics_packet_with_time(avg_peer_packet, base_time + 1100.0);
        assert!(result2.is_some(), "Second packet should return a bitrate");

        // Third peer with poor FPS (5 fps)
        let poor_peer_packet = create_test_packet("poor_sender", "peer3", 5.0, 500);
        let result3 =
            controller.process_diagnostics_packet_with_time(poor_peer_packet, base_time + 2200.0);
        assert!(result3.is_some(), "Third packet should return a bitrate");

        // Verify we have three peers
        assert_eq!(controller.peer_count(), 3);
        assert!(controller.peer_ids().contains(&"peer1".to_string()));
        assert!(controller.peer_ids().contains(&"peer2".to_string()));
        assert!(controller.peer_ids().contains(&"peer3".to_string()));

        // With p75 aggregation, a single poor peer (peer3 at 5fps) should NOT
        // tank the bitrate when 3 out of 4 peers are doing fine.
        // Peers: [5, 20, 30, 30] -> p75_index = floor(3*0.75) = 2 -> p75 = 30.
        let result4 = controller.process_diagnostics_packet_with_time(
            create_test_packet("test_sender", "test_target", 30.0, 500),
            base_time + 3300.0,
        );

        assert!(result4.is_some(), "Fourth packet should return a bitrate");

        // With p75 filtering, the single poor peer is treated as an outlier.
        // The bitrate should remain reasonable (not aggressively reduced).
        if let Some(bitrate) = result4 {
            assert!(
                bitrate > ideal_bitrate_kbps as f64 * 0.5,
                "Expected bitrate to remain reasonable with p75 filtering, got {bitrate} kbps (ideal: {ideal_bitrate_kbps} kbps)"
            );
        }
    }

    #[test]
    fn test_peer_cleanup() {
        // Setup
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Base time for simulation
        let base_time = 1000.0;

        // Add three peers, spacing packets to avoid throttling
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender1", "peer1", 30.0, 500),
            base_time,
        );
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender2", "peer2", 28.0, 500),
            base_time + 1100.0,
        );
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender3", "peer3", 25.0, 500),
            base_time + 2200.0,
        );

        // Verify we have three peers
        assert_eq!(controller.peer_count(), 3);

        // Fast forward 20 seconds (less than the 20-second timeout)
        // Update only peer1 and peer2
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender1", "peer1", 29.0, 500),
            base_time + 20_000.0,
        );
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender2", "peer2", 27.0, 500),
            base_time + 21_100.0, // Add 1100ms to avoid throttling
        );

        // Peer3 is still in the window (hasn't timed out yet)
        assert_eq!(controller.peer_count(), 3);

        // Fast forward another 15 seconds (total 35 seconds, > 20-second timeout)
        // Update only peer1
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender1", "peer1", 29.0, 500),
            base_time + 35_000.0,
        );

        // Peer3 should be removed (no updates for 35 seconds)
        // Peer2 should still be there (last update was 15 seconds ago)
        assert_eq!(controller.peer_count(), 2);
        assert!(controller.peer_ids().contains(&"peer1".to_string()));
        assert!(controller.peer_ids().contains(&"peer2".to_string()));
        assert!(!controller.peer_ids().contains(&"peer3".to_string()));

        // Fast forward another 20 seconds (total 55 seconds)
        // At this point peer2 should time out as well
        controller.process_diagnostics_packet_with_time(
            create_test_packet("sender1", "peer1", 29.0, 500),
            base_time + 55_000.0,
        );

        // Only peer1 should remain
        assert_eq!(controller.peer_count(), 1);
        assert!(controller.peer_ids().contains(&"peer1".to_string()));
    }

    #[test]
    fn test_diagnostic_packet_window() {
        // Create a window with 10-second duration
        let mut window = DiagnosticPacketWindow::new(10);

        // Base time for testing
        let base_time = 1000.0;

        // Add packets at different times
        window.add_packet(base_time, create_test_packet("sender1", "peer1", 30.0, 500));
        window.add_packet(
            base_time + 1000.0,
            create_test_packet("sender1", "peer1", 25.0, 500),
        );
        window.add_packet(
            base_time + 2000.0,
            create_test_packet("sender1", "peer1", 20.0, 500),
        );

        // Check length and latest timestamp
        assert_eq!(window.len(), 3);
        assert_eq!(window.latest_timestamp(), Some(base_time + 2000.0));

        // Check avg FPS: (30 + 25 + 20) / 3 = 25.0
        assert_eq!(window.avg_fps(), Some(25.0));

        // Force cleanup with timestamp that should only be outside the window for the first packet
        window.cleanup(base_time + 10500.0);

        // Should have 2 packets left (the ones at base_time + 1000 and base_time + 2000)
        assert_eq!(window.len(), 2);

        // Now force a cleanup that should remove all packets
        window.cleanup(base_time + 15000.0);

        // All packets should be removed
        assert_eq!(window.len(), 0);
        assert_eq!(window.avg_fps(), None);
    }

    #[test]
    fn test_different_media_types() {
        // Setup
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Base time for simulation
        let base_time = 1000.0;

        // Mix of video and audio packets, spaced out to avoid throttling
        // First packet (video)
        let result1 = controller.process_diagnostics_packet_with_time(
            create_test_packet("sender1", "peer1", 30.0, 500), // Video
            base_time,
        );
        assert!(result1.is_some(), "First packet should return a bitrate");

        // Second packet (audio), beyond throttle period
        let result2 = controller.process_diagnostics_packet_with_time(
            create_test_audio_packet("sender2", "peer2", 40.0, 100), // Audio
            base_time + 1100.0,
        );
        assert!(result2.is_some(), "Second packet should return a bitrate");

        // Verify both peers are tracked
        assert_eq!(controller.peer_count(), 2);

        // Process a new packet and verify the result
        let result3 = controller.process_diagnostics_packet_with_time(
            create_test_packet("sender3", "peer3", 25.0, 400),
            base_time + 2200.0,
        );

        // We should get a sensible bitrate
        assert!(result3.is_some(), "Third packet should return a bitrate");
    }

    #[test]
    fn test_bandwidth_drop() {
        // Setup with a target of 30 FPS
        let target_fps = Arc::new(AtomicU32::new(30));
        // Use 500 kbps as the ideal bitrate
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Use fixed base time for testing
        let base_time = 1000.0;

        // First get a baseline with perfect conditions
        let good_packet = create_test_packet("good_sender", "peer1", 30.0, 500); // Perfect FPS
        let good_result = controller.process_diagnostics_packet_with_time(good_packet, base_time);
        assert!(
            good_result.is_some(),
            "First packet should return a bitrate"
        );
        let good_bitrate = good_result.unwrap();

        // Now simulate a significant drop in FPS, spacing packets to avoid throttling
        for i in 0..5 {
            // Feed multiple poor FPS packets to build up effect
            let bad_packet = create_test_packet("poor_sender", "peer2", 5.0, 500); // Very low FPS
            let time = base_time + 1100.0 * (i as f64 + 1.0);
            let result = controller.process_diagnostics_packet_with_time(bad_packet, time);
            assert!(result.is_some(), "Packet should return a bitrate");
        }

        // One more poor FPS packet and get the resulting bitrate
        let final_packet = create_test_packet("test_sender", "test_peer", 15.0, 500);
        let final_result =
            controller.process_diagnostics_packet_with_time(final_packet, base_time + 6600.0);
        assert!(
            final_result.is_some(),
            "Final packet should return a bitrate"
        );
        let poor_bitrate = final_result.unwrap();

        // Verify that bitrate decreased when FPS decreased
        assert!(
            poor_bitrate < good_bitrate,
            "Expected bitrate to decrease when FPS drops. Good: {good_bitrate}, Poor: {poor_bitrate}"
        );

        // Verify that the bitrate is within the expected bounds (min and max)
        let min_bitrate = (ideal_bitrate_kbps) as f64 * 0.1; // 10% of ideal
        let max_bitrate = (ideal_bitrate_kbps) as f64 * 1.5; // 150% of ideal
        assert!(
            poor_bitrate >= min_bitrate,
            "Poor bitrate {poor_bitrate} bps should be greater than or equal to minimum bitrate {min_bitrate} bps"
        );
        assert!(
            poor_bitrate <= max_bitrate,
            "Poor bitrate {poor_bitrate} bps should be less than or equal to maximum bitrate {max_bitrate} bps"
        );
    }

    #[test]
    fn test_calculate_jitter() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);

        // Test empty history
        assert_eq!(
            controller.calculate_jitter(),
            0.0,
            "Empty history should return 0 jitter"
        );

        // Test single value (should still be 0)
        controller.fps_history.push_back(30.0);
        assert_eq!(
            controller.calculate_jitter(),
            0.0,
            "Single value should return 0 jitter"
        );

        // Test constant values (no jitter)
        controller.fps_history.push_back(30.0);
        controller.fps_history.push_back(30.0);
        assert_eq!(
            controller.calculate_jitter(),
            0.0,
            "Constant values should have 0 jitter"
        );

        // Test oscillating values
        controller.fps_history.clear();
        controller.fps_history.push_back(10.0);
        controller.fps_history.push_back(20.0);
        controller.fps_history.push_back(10.0);
        controller.fps_history.push_back(20.0);

        // For oscillating [10,20,10,20], we expect:
        // Differences: |20-10| = 10, |10-20| = 10, |20-10| = 10
        // Mean difference = (10 + 10 + 10) / 3 = 10
        let expected_jitter = 10.0;
        let actual_jitter = controller.calculate_jitter();
        assert!(
            (actual_jitter - expected_jitter).abs() < 0.001,
            "Expected jitter {expected_jitter}, got {actual_jitter}"
        );

        // Test gradual change
        controller.fps_history.clear();
        controller.fps_history.push_back(10.0);
        controller.fps_history.push_back(12.0);
        controller.fps_history.push_back(14.0);
        controller.fps_history.push_back(16.0);

        // For gradual change [10,12,14,16], we expect:
        // Differences: |12-10| = 2, |14-12| = 2, |16-14| = 2
        // Mean difference = (2 + 2 + 2) / 3 = 2
        let expected_jitter = 2.0;
        let actual_jitter = controller.calculate_jitter();
        assert!(
            (actual_jitter - expected_jitter).abs() < 0.001,
            "Expected jitter {expected_jitter}, got {actual_jitter}"
        );
    }

    #[test]
    fn test_throttling_basic() {
        // Setup
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Use fixed timestamps for deterministic testing
        let base_time = 1000.0;

        // First packet should produce a result (no throttling yet)
        let packet1 = create_test_packet("sender1", "peer1", 25.0, 500);
        let result1 = controller.process_diagnostics_packet_with_time(packet1, base_time);
        assert!(result1.is_some(), "First packet should return a bitrate");

        // Add a second peer to make sure peer tracking still works during throttling
        let packet2 = create_test_packet("sender2", "peer2", 20.0, 500);

        // Second packet within throttle period should not produce a result
        let result2 = controller.process_diagnostics_packet_with_time(packet2, base_time + 500.0);
        assert!(
            result2.is_none(),
            "Packet within throttle period should not return a bitrate"
        );

        // Verify both peers are tracked despite throttling
        assert_eq!(controller.peer_count(), 2);

        // Third packet after throttle period should produce a result
        let packet3 = create_test_packet("sender3", "peer3", 15.0, 500);
        let result3 = controller.process_diagnostics_packet_with_time(packet3, base_time + 1100.0);
        assert!(
            result3.is_some(),
            "Packet after throttle period should return a bitrate"
        );

        // Verify all peers are tracked
        assert_eq!(controller.peer_count(), 3);
    }

    #[test]
    fn test_bitrate_recovery_after_fps_improves() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        let base_time = 1000.0;

        // Phase 1: initialization with good FPS
        for i in 0..3 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base_time + (i as f64 * 1100.0),
            );
        }

        // Phase 2: sustained poor FPS to drive bitrate down.
        // Need enough iterations for the integral to accumulate significantly.
        let mut degraded_bitrate = ideal_bitrate_kbps as f64;
        for i in 3..25 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 5.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                degraded_bitrate = b;
            }
        }
        assert!(
            degraded_bitrate < ideal_bitrate_kbps as f64 * 0.7,
            "Sustained poor FPS should significantly reduce bitrate, got {degraded_bitrate}"
        );

        // Phase 3: FPS recovers to target — bitrate should climb back toward ideal.
        // Need enough iterations for the diagnostic window (10s) to purge old
        // 5-FPS packets, then the PID reset kicks in.
        let mut recovered_bitrate = degraded_bitrate;
        for i in 25..50 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                recovered_bitrate = b;
            }
        }
        assert!(
            recovered_bitrate > degraded_bitrate,
            "Bitrate should recover when FPS improves. Degraded: {degraded_bitrate}, Recovered: {recovered_bitrate}"
        );
        assert!(
            (recovered_bitrate - ideal_bitrate_kbps as f64).abs() < 50.0,
            "Recovered bitrate should be close to ideal ({ideal_bitrate_kbps}), got {recovered_bitrate}"
        );
    }

    #[test]
    fn test_dynamic_target_fps_change() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        let base_time = 1000.0;

        // Initialize and stabilize at 30 FPS
        for i in 0..5 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base_time + (i as f64 * 1100.0),
            );
        }

        // Change target to 15 FPS, send 15 FPS packets.
        // Run enough iterations for fps_history to flush old 30-FPS entries
        // (max_history_size=10) so jitter from the transition settles.
        target_fps.store(15, Ordering::Relaxed);

        let mut bitrate_at_15 = 0.0;
        for i in 5..20 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 15.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                bitrate_at_15 = b;
            }
        }
        // With FPS matching new target and jitter settled, bitrate should be near ideal
        assert!(
            (bitrate_at_15 - ideal_bitrate_kbps as f64).abs() < 50.0,
            "Bitrate should stay near ideal when FPS matches new target, got {bitrate_at_15}"
        );

        // Now change target to 60 FPS — received 15 FPS is way below
        target_fps.store(60, Ordering::Relaxed);

        let mut bitrate_at_60_target = 0.0;
        for i in 20..35 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 15.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                bitrate_at_60_target = b;
            }
        }
        // FPS (15) is far below new target (60) — bitrate should decrease significantly.
        // The PID drives bitrate toward zero, but tier-clamped output floors at the
        // current tier's min_bitrate_kbps. Verify we hit that floor.
        let tier_min = controller.current_video_tier().min_bitrate_kbps as f64;
        assert!(
            bitrate_at_60_target <= tier_min + 50.0,
            "Bitrate should drop to near tier minimum when FPS is far below target, \
             got {bitrate_at_60_target} (tier min: {tier_min})"
        );
    }

    #[test]
    fn test_progressive_integral_accumulation() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        let base_time = 1000.0;

        // Initialize at the SAME FPS we'll test with (20) to avoid jitter
        // from a 30→20 transition masking the integral effect.
        for i in 0..3 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 20.0, 500),
                base_time + (i as f64 * 1100.0),
            );
        }

        // Feed moderate error (20 FPS, target 30) and track progressive reduction
        let mut bitrates = Vec::new();
        for i in 3..15 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 20.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                bitrates.push(b);
            }
        }

        assert!(bitrates.len() >= 5, "Should have multiple bitrate samples");

        // Bitrate should decrease over time as the integral accumulates.
        let first = bitrates[0];
        let last = *bitrates.last().unwrap();
        assert!(
            last < first,
            "Integral accumulation should progressively reduce bitrate. First: {first}, Last: {last}"
        );
        // Every sample should be below ideal since error is always positive
        for (i, &b) in bitrates.iter().enumerate() {
            assert!(
                b < ideal_bitrate_kbps as f64,
                "Bitrate at step {i} should be below ideal ({ideal_bitrate_kbps}), got {b}"
            );
        }
    }

    #[test]
    fn test_pid_and_jitter_combined_clamp_to_min() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        let base_time = 1000.0;
        let min_bitrate = ideal_bitrate_kbps as f64 * 0.1; // 50

        // Initialize with oscillating FPS to build up jitter
        controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", 30.0, 500),
            base_time,
        );
        controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", 5.0, 500),
            base_time + 1100.0,
        );
        controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", 30.0, 500),
            base_time + 2200.0,
        );

        // Now send worst case: very low FPS with high jitter
        let mut final_bitrate = 0.0;
        for i in 3..20 {
            // Alternate between 2 and 28 to maximize jitter
            let fps = if i % 2 == 0 { 2.0 } else { 28.0 };
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", fps, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                final_bitrate = b;
            }
        }

        // Even with max PID output + max jitter, bitrate should be clamped to min, not below
        assert!(
            final_bitrate >= min_bitrate,
            "Bitrate should never go below min ({min_bitrate}), got {final_bitrate}"
        );
    }

    #[test]
    fn test_same_timestamp_dt_zero() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        let base_time = 1000.0;

        // Initialize normally
        for i in 0..3 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base_time + (i as f64 * 1100.0),
            );
        }

        // Send two packets at the exact same timestamp (dt=0).
        // First one is processed normally.
        let result1 = controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", 20.0, 500),
            base_time + 5000.0,
        );
        assert!(result1.is_some());

        // Second has dt=0 but is throttled (within 1000ms). Send one after throttle
        // with same timestamp as last_update to produce dt=0 for PID.
        // We need to advance past the throttle window but keep the same last_update.
        // The simplest way: advance time by exactly the throttle (1000ms) so
        // last_update was set at 5000 and now=6000. dt = (6000-5000)/1000 = 1.0s, not zero.
        //
        // To truly get dt=0, we'd need two non-throttled calls at the same time,
        // which is impossible since the second would be throttled.
        // Instead, verify that the PID gracefully handles very small dt.
        let result2 = controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", 20.0, 500),
            base_time + 6000.01, // 1000.01ms later, dt ≈ 0.001s
        );
        assert!(
            result2.is_some(),
            "Very small dt should still produce a result"
        );
        let bitrate = result2.unwrap();
        let min_bitrate = ideal_bitrate_kbps as f64 * 0.1;
        let max_bitrate = ideal_bitrate_kbps as f64 * 1.5;
        assert!(
            (min_bitrate..=max_bitrate).contains(&bitrate),
            "Bitrate should be within bounds even with tiny dt, got {bitrate}"
        );
    }

    #[test]
    fn test_new_for_screen_starts_at_midpoint_tier() {
        use crate::constants::{DEFAULT_SCREEN_TIER_INDEX, SCREEN_QUALITY_TIERS};

        let target_fps = Arc::new(AtomicU32::new(15));
        let controller = EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);

        // Should start at DEFAULT_SCREEN_TIER_INDEX (index 1, "medium" — midpoint of 3-tier ladder)
        assert_eq!(controller.video_tier_index(), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(controller.current_video_tier().label, "medium");

        // The ideal_bitrate_kbps should be synced with the starting tier
        let expected_bitrate = SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX].ideal_bitrate_kbps;
        assert_eq!(
            controller.ideal_bitrate_kbps, expected_bitrate,
            "Initial ideal_bitrate_kbps should match the starting tier's ideal_bitrate_kbps"
        );
    }

    // =====================================================================
    // Screen sharing coordination (notify_screen_sharing)
    // =====================================================================

    #[test]
    fn test_notify_screen_sharing_active_forces_ceiling() {
        use crate::constants::screen_share_camera_ceiling_index;

        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(1500, target_fps);

        // Camera starts at DEFAULT_VIDEO_TIER_INDEX (the "medium" tier, 480p).
        let initial_index = controller.video_tier_index();
        let ceiling = screen_share_camera_ceiling_index();

        // Screen share activates — camera should jump to ceiling tier
        controller.notify_screen_sharing(true);

        assert_eq!(
            controller.video_tier_index(),
            ceiling,
            "Camera should be forced to ceiling tier '{}' (index {}), was at index {}",
            controller.current_video_tier().label,
            controller.video_tier_index(),
            initial_index,
        );
        assert_eq!(
            controller.current_video_tier().label,
            "low",
            "Ceiling tier should be 'low'"
        );
        // ideal_bitrate should be synced with the new tier
        assert_eq!(
            controller.ideal_bitrate_kbps,
            controller.current_video_tier().ideal_bitrate_kbps,
            "ideal_bitrate_kbps should match the ceiling tier"
        );
        // tier_changed should be set so the encoding loop picks it up
        assert!(
            controller.take_tier_changed(),
            "tier_changed should be true after screen share activation"
        );
    }

    #[test]
    fn test_notify_screen_sharing_deactivate_removes_ceiling() {
        use crate::constants::screen_share_camera_ceiling_index;

        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(1500, target_fps);

        let ceiling = screen_share_camera_ceiling_index();

        // Activate then deactivate
        controller.notify_screen_sharing(true);
        assert_eq!(controller.video_tier_index(), ceiling);
        controller.take_tier_changed(); // consume

        controller.notify_screen_sharing(false);

        // Tier doesn't change on deactivation — only the ceiling is removed.
        // The camera stays at its current tier and recovers naturally via the
        // PID step-up mechanism.
        assert_eq!(
            controller.video_tier_index(),
            ceiling,
            "Tier should not jump on deactivation, stays at current position"
        );
        assert!(
            !controller.take_tier_changed(),
            "tier_changed should NOT be set on deactivation (tier didn't move)"
        );

        // Feed good conditions — the camera should eventually step up past the
        // old ceiling, proving the ceiling was actually removed.
        //
        // Use the default clock as the base so timestamps are consistent with
        // the quality manager's clock-based `created_at_ms` and
        // `last_transition_time_ms`. The +10_000 offset ensures warmup (5s)
        // and min-transition-interval (3s) are both satisfied from the start.
        let base = test_now_ms() + 10_000.0;
        for i in 0..15 {
            let t = base + (i as f64 * 1100.0);
            let packet = create_test_packet("s", "peer1", 29.0, 1400);
            controller.process_diagnostics_packet_with_time(packet, t);
        }
        assert!(
            controller.video_tier_index() < ceiling,
            "Camera should step up past old ceiling after removal, \
             got index {} (ceiling was {})",
            controller.video_tier_index(),
            ceiling,
        );
    }

    #[test]
    fn test_notify_screen_sharing_double_activation_is_idempotent() {
        use crate::constants::screen_share_camera_ceiling_index;

        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(1500, target_fps);

        let ceiling = screen_share_camera_ceiling_index();

        // First activation
        controller.notify_screen_sharing(true);
        assert_eq!(controller.video_tier_index(), ceiling);
        let tier_after_first = controller.current_video_tier().label;
        let bitrate_after_first = controller.ideal_bitrate_kbps;
        controller.take_tier_changed(); // consume

        // Second activation — should be a no-op (already at ceiling)
        controller.notify_screen_sharing(true);
        assert_eq!(
            controller.video_tier_index(),
            ceiling,
            "Double activation should not change tier"
        );
        assert_eq!(
            controller.current_video_tier().label,
            tier_after_first,
            "Tier label should be unchanged"
        );
        assert_eq!(
            controller.ideal_bitrate_kbps, bitrate_after_first,
            "Bitrate should be unchanged"
        );
        assert!(
            !controller.take_tier_changed(),
            "tier_changed should NOT be set on redundant activation"
        );
    }

    // =====================================================================
    // P75 aggregation and avg_fps tests
    // =====================================================================

    #[test]
    fn test_avg_fps_window() {
        let mut window = DiagnosticPacketWindow::new(10);
        let base_time = 1000.0;

        // Add packets at 30, 5, 25 fps
        window.add_packet(base_time, create_test_packet("s", "p", 30.0, 500));
        window.add_packet(base_time + 1000.0, create_test_packet("s", "p", 5.0, 500));
        window.add_packet(base_time + 2000.0, create_test_packet("s", "p", 25.0, 500));

        // avg should be (30 + 5 + 25) / 3 = 20.0, not 5.0 (the minimum)
        let avg = window.avg_fps().unwrap();
        assert!(
            (avg - 20.0).abs() < 0.001,
            "avg_fps should be 20.0, got {avg}"
        );
    }

    #[test]
    fn test_p75_aggregation_filters_outlier() {
        // 5 peers: 4 at 28fps, 1 at 0fps. The p75 should be ~28, not 0.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        // Add good peers
        for i in 0..4 {
            let target_id = format!("good_peer_{i}");
            let packet = create_test_packet("sender", &target_id, 28.0, 500);
            packets.process_packet(packet, base + (i as f64 * 100.0));
        }
        // Add bad peer
        let bad_packet = create_test_packet("sender", "bad_peer", 0.0, 500);
        packets.process_packet(bad_packet, base + 400.0);

        assert_eq!(packets.peer_count(), 5);

        // n>=3 uses the true percentile; the health floor is irrelevant here.
        let (p75, effective_n) = packets.get_p75_fps(21.0).unwrap();
        assert_eq!(effective_n, 5);
        // With 5 values [0, 28, 28, 28, 28] sorted ascending,
        // p75_index = floor(4 * 0.75) = 3, so p75 = 28.0
        assert!(
            (p75 - 28.0).abs() < 0.001,
            "p75 should be ~28.0 (filtering outlier), got {p75}"
        );
    }

    #[test]
    fn test_p75_aggregation_with_widespread_degradation() {
        // 5 peers: 4 at 5fps, 1 at 28fps. The p75 should be ~5.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        for i in 0..4 {
            let target_id = format!("slow_peer_{i}");
            let packet = create_test_packet("sender", &target_id, 5.0, 500);
            packets.process_packet(packet, base + (i as f64 * 100.0));
        }
        let good_packet = create_test_packet("sender", "good_peer", 28.0, 500);
        packets.process_packet(good_packet, base + 400.0);

        assert_eq!(packets.peer_count(), 5);

        let (p75, effective_n) = packets.get_p75_fps(21.0).unwrap();
        assert_eq!(effective_n, 5);
        // With 5 values [5, 5, 5, 5, 28] sorted ascending,
        // p75_index = floor(4 * 0.75) = 3, so p75 = 5.0
        assert!(
            (p75 - 5.0).abs() < 0.001,
            "p75 should be ~5.0 (widespread degradation), got {p75}"
        );
    }

    #[test]
    fn test_single_peer_p75_returns_that_peer() {
        // With 1 peer, get_p75_fps should return that peer's avg_fps directly.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        let packet = create_test_packet("sender", "peer1", 12.0, 500);
        packets.process_packet(packet, base);

        assert_eq!(packets.peer_count(), 1);
        // n=1 returns the peer's value verbatim; health floor is irrelevant.
        let (p75, effective_n) = packets.get_p75_fps(21.0).unwrap();
        assert_eq!(effective_n, 1);
        assert!(
            (p75 - 12.0).abs() < 0.001,
            "Single peer p75 should be that peer's fps, got {p75}"
        );
    }

    #[test]
    fn test_two_peers_outlier_rescues_to_healthy_peer() {
        // #1012: with 2 peers where one is healthy and the other is a clear
        // outlier below it, get_p75_fps returns the HEALTHY peer's value so a
        // single constrained receiver cannot define the setpoint. Target 30fps
        // → health floor 21fps; peer at 28 is healthy, peer at 12 is a clear
        // outlier (12 < 28 * 0.60 = 16.8), so the result is 28, not 12.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;
        let health_floor = 30.0 * AQ_OUTLIER_HEALTH_FPS_RATIO; // 21.0

        packets.process_packet(create_test_packet("sender", "peer1", 12.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 28.0, 500),
            base + 100.0,
        );

        assert_eq!(packets.peer_count(), 2);
        let (p75, effective_n) = packets.get_p75_fps(health_floor).unwrap();
        assert_eq!(effective_n, 2);
        assert!(
            (p75 - 28.0).abs() < 0.001,
            "outlier guard should rescue to the healthy peer (28.0), got {p75}"
        );
    }

    #[test]
    fn test_two_peers_both_low_keeps_minimum() {
        // #1012 masking guard: when NEITHER peer is healthy (real congestion),
        // the outlier guard must NOT fire — keep the conservative minimum so
        // the sender still steps down. Target 30fps → floor 21; both peers
        // (14, 16) are below the floor, so the result is the minimum (14).
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;
        let health_floor = 30.0 * AQ_OUTLIER_HEALTH_FPS_RATIO; // 21.0

        packets.process_packet(create_test_packet("sender", "peer1", 14.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 16.0, 500),
            base + 100.0,
        );

        assert_eq!(packets.peer_count(), 2);
        let (p75, effective_n) = packets.get_p75_fps(health_floor).unwrap();
        assert_eq!(effective_n, 2);
        assert!(
            (p75 - 14.0).abs() < 0.001,
            "both-low (real congestion) must keep the minimum (14.0), got {p75}"
        );
    }

    #[test]
    fn test_two_peers_both_healthy_keeps_minimum() {
        // #1012: two healthy peers a few fps apart are NOT an outlier split
        // (28 is not < 30 * 0.60 = 18), so the guard does not fire and the
        // minimum is kept — which is fine because both are healthy anyway.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;
        let health_floor = 30.0 * AQ_OUTLIER_HEALTH_FPS_RATIO; // 21.0

        packets.process_packet(create_test_packet("sender", "peer1", 28.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 30.0, 500),
            base + 100.0,
        );

        assert_eq!(packets.peer_count(), 2);
        let (p75, effective_n) = packets.get_p75_fps(health_floor).unwrap();
        assert_eq!(effective_n, 2);
        assert!(
            (p75 - 28.0).abs() < 0.001,
            "two close healthy peers should keep the minimum (28.0), got {p75}"
        );
    }

    #[test]
    fn test_two_peers_outlier_guard_disabled_without_target() {
        // #1012: a non-positive health floor (unknown/zero target_fps) disables
        // the rescue entirely, so the setpoint can never be spuriously raised.
        // Falls back to the conservative minimum (12.0).
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        packets.process_packet(create_test_packet("sender", "peer1", 12.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 28.0, 500),
            base + 100.0,
        );

        let (p75, _) = packets.get_p75_fps(0.0).unwrap();
        assert!(
            (p75 - 12.0).abs() < 0.001,
            "zero health floor must disable rescue and keep the minimum (12.0), got {p75}"
        );
    }

    #[test]
    fn test_three_peers_p75_filters_outlier() {
        // With 3 peers, p75 kicks in (no longer minimum fallback).
        // Sorted: [5, 25, 30] -> p75_index = floor(2 * 0.75) = 1 -> value = 25.
        // The outlier at 5 fps does NOT dominate.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        packets.process_packet(create_test_packet("sender", "peer1", 30.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 5.0, 500),
            base + 100.0,
        );
        packets.process_packet(
            create_test_packet("sender", "peer3", 25.0, 500),
            base + 200.0,
        );

        assert_eq!(packets.peer_count(), 3);
        let (p75, effective_n) = packets.get_p75_fps(21.0).unwrap();
        assert_eq!(effective_n, 3);
        assert!(
            (p75 - 25.0).abs() < 0.001,
            "Three-peer p75 should be 25.0 (index 1 of [5,25,30]), got {p75}"
        );
    }

    #[test]
    fn test_p75_effective_count_excludes_non_fps_peers() {
        // 3 registered peers, but one has no usable FPS (media_type=VIDEO,
        // video_metrics=None — simulates a peer that just joined and hasn't
        // produced video yet). effective_n should be 2, not 3, so the lenient
        // threshold applies.
        let mut packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);
        let base = 1000.0;

        // Two peers with valid FPS
        packets.process_packet(create_test_packet("sender", "peer1", 28.0, 500), base);
        packets.process_packet(
            create_test_packet("sender", "peer2", 25.0, 500),
            base + 100.0,
        );

        // Third peer: media_type=VIDEO but video_metrics=None -> avg_fps()=None
        let mut no_fps_packet = DiagnosticsPacket::new();
        no_fps_packet.sender_id = "sender".to_string();
        no_fps_packet.target_id = "peer_no_fps".to_string();
        no_fps_packet.media_type = MediaType::VIDEO.into();
        // Intentionally do NOT set video_metrics — it stays None.
        packets.process_packet(no_fps_packet, base + 200.0);

        // All 3 peers are registered in the window map
        assert_eq!(packets.peer_count(), 3);

        // But only 2 contributed FPS data
        let (p75, effective_n) = packets.get_p75_fps(21.0).unwrap();
        assert_eq!(
            effective_n, 2,
            "effective_n should be 2 (peer_no_fps has no FPS data), got {effective_n}"
        );
        // With 2 effective peers [25, 28] and health floor 21: 28 is healthy
        // but 25 is NOT a clear outlier (25 >= 28 * 0.60 = 16.8), so the
        // outlier guard does not fire and the minimum (25) is kept.
        assert!(
            (p75 - 25.0).abs() < 0.001,
            "p75 should be 25.0 (min of 2 effective, no outlier), got {p75}"
        );
    }

    #[test]
    fn test_pid_converges_near_target_fps() {
        // Feed p75 = 28 fps (very close to target 30) for ~30 iterations.
        // The PID should converge near ideal_bitrate (500 kbps) since there
        // is very little error. This proves the PID is stable with the p75 signal.
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500u32;
        let mut controller = EncoderBitrateController::new(ideal_bitrate_kbps, target_fps.clone());

        // Use base_time far in the past so the warmup guard and tier transition
        // timers in the adaptive quality manager are safely exceeded.
        let base_time = 1000.0;

        let mut final_bitrate = ideal_bitrate_kbps as f64;
        for i in 0..30 {
            let result = controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 28.0, 500),
                base_time + (i as f64 * 1100.0),
            );
            if let Some(b) = result {
                final_bitrate = b;
            }
        }

        // Final bitrate should be within 10% of ideal (450-550 range).
        assert!(
            (450.0..=550.0).contains(&final_bitrate),
            "PID should converge near ideal_bitrate ({ideal_bitrate_kbps}) with p75=28, \
             got {final_bitrate}"
        );
    }

    // =====================================================================
    // Slew-rate limiter and reconfigure-threshold tests
    // =====================================================================

    /// Warm a freshly-built controller through initialization without leaving
    /// behind a `last_target_bitrate_kbps` baseline that would interfere with
    /// slew-limit assertions. Returns the time of the last call so the caller
    /// can keep advancing the clock monotonically.
    fn warm_up_for_slew_test(
        controller: &mut EncoderBitrateController,
        base_time: f64,
        fps: f32,
        bitrate_kbps: u32,
    ) -> f64 {
        for i in 0..3 {
            let t = base_time + (i as f64 * 1100.0);
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", fps, bitrate_kbps),
                t,
            );
        }
        base_time + 2.0 * 1100.0
    }

    #[test]
    fn test_slew_limit_caps_per_tick_swing() {
        use crate::constants::SCREEN_QUALITY_TIERS;

        let target_fps = Arc::new(AtomicU32::new(15));
        let mut controller =
            EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);

        let base_time = 100_000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 15.0, 2500);

        controller.last_target_bitrate_kbps = 1500.0;
        controller.last_update = last_warm;

        let max_delta = MAX_BITRATE_SLEW_KBPS_PER_SEC as f64;
        let mut prev = 1500.0;
        let mut now = last_warm;
        for tick in 1..=7 {
            now += 1100.0;
            let result = controller
                .process_diagnostics_packet_with_time(
                    create_test_packet("s", "peer1", 15.0, 2500),
                    now,
                )
                .expect("slew tick should emit a bitrate");
            let dt_seconds = 1.1;
            let allowed = max_delta * dt_seconds;
            assert!(
                (result - prev).abs() <= allowed + 0.5,
                "tick {tick}: bitrate {result:.1} jumped more than {allowed:.1} kbps \
                 from previous {prev:.1}"
            );
            prev = result;
        }
    }

    #[test]
    fn test_slew_limit_handles_dt_zero() {
        use crate::constants::SCREEN_QUALITY_TIERS;

        let target_fps = Arc::new(AtomicU32::new(15));
        let mut controller =
            EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);

        let base_time = 100_000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 15.0, 2500);

        let now = last_warm + 1100.0;
        let first = controller
            .process_diagnostics_packet_with_time(create_test_packet("s", "peer1", 15.0, 2500), now)
            .expect("first tick should emit a bitrate");

        controller.last_correction_time = 0.0;
        controller.last_target_bitrate_kbps = 200.0;

        let second = controller
            .process_diagnostics_packet_with_time(create_test_packet("s", "peer1", 15.0, 2500), now)
            .expect("duplicate-timestamp tick should still emit a bitrate");

        let max_step = MAX_BITRATE_SLEW_KBPS_PER_SEC as f64;
        assert!(
            (second - 200.0).abs() > max_step,
            "with dt=0 the slew limit must be bypassed: prev=200, got {second} \
             (first={first})"
        );
    }

    #[test]
    fn test_slew_limit_bypassed_on_tier_downshift() {
        use crate::constants::SCREEN_QUALITY_TIERS;

        let target_fps = Arc::new(AtomicU32::new(15));
        let mut controller =
            EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);

        let base_time = 100_000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 15.0, 2500);

        let high_max = SCREEN_QUALITY_TIERS[0].max_bitrate_kbps as f64;
        controller.last_target_bitrate_kbps = high_max;
        controller.last_update = last_warm;
        controller.last_correction_time = 0.0;

        let low_index = SCREEN_QUALITY_TIERS.len() - 1;
        controller
            .quality_manager
            .force_video_step_to(low_index, last_warm);
        controller.ideal_bitrate_kbps = SCREEN_QUALITY_TIERS[low_index].ideal_bitrate_kbps;

        let low_tier_max = SCREEN_QUALITY_TIERS[low_index].max_bitrate_kbps as f64;
        let now = last_warm + 1100.0;
        let result = controller
            .process_diagnostics_packet_with_time(create_test_packet("s", "peer1", 15.0, 200), now)
            .expect("downshift tick should emit a bitrate");

        assert!(
            result <= low_tier_max,
            "after downshift, output {result} must respect new tier max {low_tier_max}"
        );
        let max_step = MAX_BITRATE_SLEW_KBPS_PER_SEC as f64 * 1.1;
        assert!(
            high_max - result > max_step,
            "downshift drop ({} -> {}) must exceed slew step ({}); slew was not bypassed",
            high_max,
            result,
            max_step,
        );
    }

    #[test]
    fn test_congestion_cut_bypasses_throttle_immediately() {
        use crate::clock::TestClock;
        use crate::constants::{DEFAULT_VIDEO_TIER_INDEX, VIDEO_QUALITY_TIERS};

        // The aggressive congestion cut (#702) must drop the *bitrate* target
        // immediately, not just resolution/keyframe tiers. The encoder's bitrate
        // only updates from the value process_diagnostics_packet returns, so if a
        // CONGESTION arrives <1s after a prior PID correction, the normal throttle
        // would return None and the codec would keep targeting the old (higher)
        // bitrate for ~1s — defeating the buffer-drain intent of the cut.
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].ideal_bitrate_kbps;
        let mut controller = EncoderBitrateController::with_clock(
            ideal_bitrate_kbps,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );

        // Warm up past the initialization gate so corrections actually emit.
        // Start well past the manager's 5s warmup window (created_at = base_ms),
        // and hold a steady ~0.60 fps ratio (18/30) — between the 0.50 degrade and
        // 0.70 recover thresholds — so no tier transition fires during warm-up and
        // the min-transition-interval guard cannot block the congestion cut.
        let stable_fps = 18.0_f32;
        let base_time = base_ms as f64 + 6000.0;
        let last_warm =
            warm_up_for_slew_test(&mut controller, base_time, stable_fps, ideal_bitrate_kbps);

        // Perform one normal correction so last_correction_time is recent.
        let correction_time = last_warm + 1100.0;
        let prior = controller
            .process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", stable_fps, ideal_bitrate_kbps),
                correction_time,
            )
            .expect("warm-up correction should emit a bitrate");
        assert!(prior > 0.0, "prior correction should be positive");

        // A self-targeted CONGESTION arrives only 200ms later — well inside the
        // 1s PID correction throttle. Align the clock so force_congestion_cut's
        // drain-hold window is anchored to the same timeline as the packet below.
        let congestion_time = correction_time + 200.0;
        clock.set_ms(congestion_time as u64);
        let cut_changed = controller.force_congestion_cut();
        assert!(
            cut_changed,
            "congestion cut should change tier from the default starting tier"
        );

        // The very next diagnostics packet — still <1s since the prior correction —
        // must NOT be throttled: the one-shot bypass forces a fresh computation so
        // the lower bitrate reaches the encoder in this same iteration.
        let held_tier = controller.current_video_tier();
        let held_ideal = held_tier.ideal_bitrate_kbps as f64;
        let result = controller
            .process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", stable_fps, ideal_bitrate_kbps),
                congestion_time,
            )
            .expect("congestion cut must bypass the throttle and emit a bitrate immediately");

        // The emitted bitrate must respect the congestion-hold ceiling pin: it is
        // clamped at or below the held (lower) tier's ideal, proving the cut landed
        // immediately rather than being deferred to the old higher target.
        assert!(
            result <= held_ideal + 0.5,
            "post-cut bitrate {result:.1} must be <= held tier ideal {held_ideal:.1} \
             (congestion-hold ceiling pin); cut was not applied immediately"
        );
        assert!(
            result < prior,
            "post-cut bitrate {result:.1} must be lower than the pre-cut target {prior:.1}"
        );

        // The bypass is one-shot: a follow-up packet still inside the throttle
        // window must be throttled again (the normal 1s PID throttle is intact).
        let throttled = controller.process_diagnostics_packet_with_time(
            create_test_packet("s", "peer1", stable_fps, ideal_bitrate_kbps),
            congestion_time + 100.0,
        );
        assert!(
            throttled.is_none(),
            "the throttle bypass must be one-shot; the normal 1s throttle must \
             still apply to the following correction"
        );
    }

    // =====================================================================
    // Simulcast controller tests (#989, PR B) — ADDITIVE
    // =====================================================================

    #[test]
    fn test_controller_single_stream_by_default() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let controller = EncoderBitrateController::new(500, target_fps);
        assert!(!controller.is_simulcast());
        assert_eq!(controller.simulcast_layer_count(), 1);
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
        assert_eq!(controller.layer_resolution(0), None);
    }

    #[test]
    fn test_set_simulcast_layers_builds_per_layer_state() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(3);

        assert!(controller.is_simulcast());
        assert_eq!(controller.simulcast_layer_count(), 3);
        assert_eq!(controller.active_layer_count(), 3);
        // Per-layer fixed resolutions match the ladder [low, standard, hd].
        assert_eq!(controller.layer_resolution(0), Some((640, 360)));
        assert_eq!(controller.layer_resolution(1), Some((960, 540)));
        assert_eq!(controller.layer_resolution(2), Some((1280, 720)));
        assert_eq!(controller.layer_resolution(3), None);
        assert_eq!(controller.layer_target_bitrates_kbps().len(), 3);
    }

    #[test]
    fn test_set_simulcast_layers_one_stays_single_stream() {
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(1);
        assert!(!controller.is_simulcast());
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
    }

    #[test]
    fn test_per_layer_bitrates_clamped_to_layer_tier_bounds() {
        use crate::constants::simulcast_layers;

        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(3);

        // Drive perfect conditions for many ticks; each layer's bitrate must
        // stay within its own fixed tier [min, max] band.
        let base = 1000.0;
        for i in 0..20 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base + (i as f64 * 1100.0),
            );
        }

        let tiers = simulcast_layers(3);
        let bitrates = controller.layer_target_bitrates_kbps();
        assert_eq!(bitrates.len(), 3);
        for (i, &b) in bitrates.iter().enumerate() {
            let tmin = tiers[i].min_bitrate_kbps as f64;
            let tmax = tiers[i].max_bitrate_kbps as f64;
            assert!(
                b >= tmin - 0.5 && b <= tmax + 0.5,
                "layer {i} bitrate {b} must be within tier bounds [{tmin}, {tmax}]"
            );
        }
    }

    #[test]
    fn test_congestion_cut_sheds_top_layer_in_simulcast() {
        use crate::clock::TestClock;
        use crate::constants::{DEFAULT_VIDEO_TIER_INDEX, VIDEO_QUALITY_TIERS};

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal = VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].ideal_bitrate_kbps;
        let mut controller = EncoderBitrateController::with_clock(
            ideal,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        controller.set_simulcast_layers(3);
        assert_eq!(controller.active_layer_count(), 3);

        // Warm past the manager's 5s warmup so the forced cut is allowed.
        let stable_fps = 18.0_f32;
        let base_time = base_ms as f64 + 6000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, stable_fps, ideal);

        clock.set_ms((last_warm + 1100.0) as u64);
        let changed = controller.force_congestion_cut();
        assert!(
            changed,
            "congestion cut should change tier from default start"
        );
        assert_eq!(
            controller.active_layer_count(),
            2,
            "a congestion cut in simulcast mode must shed the top active layer"
        );
    }

    #[test]
    fn test_force_step_down_sheds_top_layer_in_simulcast() {
        use crate::clock::TestClock;

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::with_clock(
            500,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        controller.set_simulcast_layers(3);

        // Warm past warmup so the forced step-down is allowed.
        let base_time = base_ms as f64 + 6000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 18.0, 500);
        clock.set_ms((last_warm + 1100.0) as u64);

        assert!(controller.force_video_step_down());
        assert_eq!(
            controller.active_layer_count(),
            2,
            "force_video_step_down in simulcast mode must shed the top active layer"
        );
    }

    #[test]
    fn test_single_stream_force_paths_do_not_touch_layers() {
        use crate::clock::TestClock;

        // In single-stream mode the force_* paths must NOT change active layers
        // (it stays pinned at 1) — preserving today's behavior for the bot.
        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::with_clock(
            500,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        // No set_simulcast_layers → single-stream.
        let base_time = base_ms as f64 + 6000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 18.0, 500);
        clock.set_ms((last_warm + 1100.0) as u64);

        controller.force_video_step_down();
        assert_eq!(controller.active_layer_count(), 1);
        assert!(controller.layer_target_bitrates_kbps().is_empty());
    }

    #[test]
    fn test_active_layer_sum_stays_within_uplink_budget() {
        // After processing many diagnostics ticks in 3-layer simulcast, the SUM
        // of the ACTIVE layers' target bitrates must never exceed the uplink
        // budget (sum of active tier ideals), and each layer must stay at/above
        // its tier floor. This is the core Phase-1 budget guarantee.
        use crate::constants::{simulcast_layers, uplink_budget_kbps};

        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::new(500, target_fps);
        controller.set_simulcast_layers(3);

        // Perfect conditions push every per-layer PID toward its tier ideal/max,
        // which (un-capped) would sum to 2800+ kbps — exercising the cap.
        let base = 1000.0;
        for i in 0..30 {
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", 30.0, 500),
                base + (i as f64 * 1100.0),
            );
        }

        let active = controller.active_layer_count();
        let tiers = simulcast_layers(3);
        let budget = uplink_budget_kbps(tiers, active);
        let bitrates = controller.layer_target_bitrates_kbps();
        let active_sum: f64 = bitrates[..active].iter().sum();
        assert!(
            active_sum <= budget + 1e-6,
            "active sum {active_sum} (active={active}) must fit within budget {budget}"
        );
        for i in 0..active {
            assert!(
                bitrates[i] >= tiers[i].min_bitrate_kbps as f64 - 1e-6,
                "layer {i} ({}) below its floor {}",
                bitrates[i],
                tiers[i].min_bitrate_kbps,
            );
        }
    }

    #[test]
    fn test_budget_shrinks_as_top_layer_is_shed() {
        // The budget is the sum of ACTIVE tier ideals, so shedding the top layer
        // must lower the budget the active set is held to. 3 active = 2800,
        // 2 active = 1300, 1 active = 400.
        use crate::constants::{simulcast_layers, uplink_budget_kbps};
        let tiers = simulcast_layers(3);
        assert!(uplink_budget_kbps(tiers, 3) > uplink_budget_kbps(tiers, 2));
        assert!(uplink_budget_kbps(tiers, 2) > uplink_budget_kbps(tiers, 1));
        assert_eq!(uplink_budget_kbps(tiers, 1), 400.0);
    }

    #[test]
    fn test_congestion_cut_sheds_layer_even_at_tier_floor() {
        // Finding 2 regression: force_congestion_cut returns `false` once the
        // tier index is at the floor (manager can't drop further), but it STILL
        // arms the drain hold — so in simulcast mode it must still shed a layer
        // while active_layer_count > 1. The layer axis is independent of the
        // tier floor.
        use crate::clock::TestClock;
        use crate::constants::VIDEO_QUALITY_TIERS;

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let mut controller = EncoderBitrateController::with_clock(
            400,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        controller.set_simulcast_layers(3);
        assert_eq!(controller.active_layer_count(), 3);

        // Warm past warmup so the forced cut is allowed.
        let base_time = base_ms as f64 + 6000.0;
        let last_warm = warm_up_for_slew_test(&mut controller, base_time, 18.0, 400);

        // Force the tier index to the floor WITHOUT shedding layers, so the cut
        // below exercises the "manager returns false but hold is armed" path.
        let floor = VIDEO_QUALITY_TIERS.len() - 1;
        controller
            .quality_manager
            .force_video_step_to(floor, last_warm);
        assert_eq!(controller.video_tier_index(), floor);
        assert_eq!(
            controller.active_layer_count(),
            3,
            "forcing the tier to the floor must not touch the layer axis"
        );

        // Cut > MIN_TIER_TRANSITION_INTERVAL_MS after the forced step-to so the
        // guard is clear and the cut takes effect (arms the hold) at the floor.
        let cut_at = last_warm + 1600.0;
        clock.set_ms(cut_at as u64);
        let changed = controller.force_congestion_cut();
        assert!(
            !changed,
            "at the tier floor the manager reports no tier change"
        );
        assert_eq!(
            controller.active_layer_count(),
            2,
            "a congestion cut must still shed a layer at the tier floor (drain hold armed)"
        );
        // The drain hold is armed, and a layer was shed, so the one-shot throttle
        // bypass must be set so the held per-layer bitrates land immediately.
        assert!(
            controller
                .quality_manager
                .congestion_hold_active(cut_at + 1.0),
            "drain hold must be armed even at the tier floor"
        );
    }

    #[test]
    fn test_per_layer_bitrates_pinned_to_ideal_during_congestion_hold() {
        // Finding 1 regression: while a CONGESTION drain hold is active, each
        // surviving layer's bitrate must be clamped to its tier IDEAL (not max),
        // mirroring the single-stream pin, so the layers can't ramp back up and
        // re-fill the draining relay buffer.
        use crate::clock::TestClock;
        use crate::constants::{simulcast_layers, DEFAULT_VIDEO_TIER_INDEX, VIDEO_QUALITY_TIERS};

        let base_ms: u64 = 100_000;
        let clock = Arc::new(TestClock::new(base_ms));
        let target_fps = Arc::new(AtomicU32::new(30));
        let ideal = VIDEO_QUALITY_TIERS[DEFAULT_VIDEO_TIER_INDEX].ideal_bitrate_kbps;
        let mut controller = EncoderBitrateController::with_clock(
            ideal,
            target_fps,
            Arc::clone(&clock) as Arc<dyn crate::clock::Clock>,
        );
        controller.set_simulcast_layers(3);

        // Warm past warmup with PERFECT conditions so the PIDs want max bitrate.
        let perfect_fps = 30.0_f32;
        let base_time = base_ms as f64 + 6000.0;
        let mut now = warm_up_for_slew_test(&mut controller, base_time, perfect_fps, ideal);

        // Arm a congestion cut (also sheds a layer + arms the drain hold).
        now += 1100.0;
        clock.set_ms(now as u64);
        let cut_at = now;
        controller.force_congestion_cut();
        let active = controller.active_layer_count();
        assert!(active >= 1);

        // Drive perfect-condition ticks DURING the hold window only. Each
        // surviving layer's bitrate must stay at/below its tier ideal (the hold
        // pin), never climbing toward its tier max. Stay strictly inside
        // CONGESTION_HOLD_MS so the hold is guaranteed active each tick.
        use crate::constants::CONGESTION_HOLD_MS;
        let tiers = simulcast_layers(controller.simulcast_layer_count());
        let mut ticks_in_hold = 0;
        while now + 600.0 < cut_at + CONGESTION_HOLD_MS {
            now += 600.0;
            clock.set_ms(now as u64);
            assert!(
                controller.quality_manager.congestion_hold_active(now),
                "hold should still be active at t={now}"
            );
            ticks_in_hold += 1;
            controller.process_diagnostics_packet_with_time(
                create_test_packet("s", "peer1", perfect_fps, ideal),
                now,
            );
            let bitrates = controller.layer_target_bitrates_kbps();
            for layer_id in 0..active {
                let tier = &tiers[layer_id];
                let tier_ideal = tier.ideal_bitrate_kbps as f64;
                let tier_max = tier.max_bitrate_kbps as f64;
                assert!(
                    tier_max > tier_ideal,
                    "test precondition: layer {layer_id} ideal {tier_ideal} < max {tier_max}"
                );
                assert!(
                    bitrates[layer_id] <= tier_ideal + 0.5,
                    "layer {layer_id} bitrate {} must be pinned <= tier ideal {tier_ideal} during \
                     the congestion hold (not ramping toward max {tier_max})",
                    bitrates[layer_id],
                );
            }
        }
        assert!(
            ticks_in_hold > 0,
            "test must exercise at least one tick inside the congestion hold"
        );

        // Sanity: after the hold expires the per-layer clamp returns to using
        // the tier max (verified deterministically by the pure-function test
        // below — the PID itself is reduction-only and won't drive above ideal
        // under perfect conditions, so we don't assert a numeric climb here).
        assert!(!controller
            .quality_manager
            .congestion_hold_active(cut_at + CONGESTION_HOLD_MS + 1.0));
    }

    #[test]
    fn test_layer_upper_clamp_pins_to_ideal_only_during_hold() {
        // Finding 1, focused regression: the per-layer upper clamp is the tier
        // IDEAL while a congestion drain hold is active, and the tier MAX
        // otherwise — mirroring the single-stream pin. Deterministic and
        // independent of PID arithmetic.
        use crate::constants::simulcast_layers;

        for tier in simulcast_layers(3) {
            let ideal = tier.ideal_bitrate_kbps as f64;
            let max = tier.max_bitrate_kbps as f64;
            assert!(
                max > ideal,
                "precondition: tier '{}' max {max} must exceed ideal {ideal}",
                tier.label
            );
            assert_eq!(
                layer_upper_clamp_kbps(tier, true),
                ideal,
                "tier '{}': during the hold the upper clamp must be the IDEAL",
                tier.label
            );
            assert_eq!(
                layer_upper_clamp_kbps(tier, false),
                max,
                "tier '{}': without the hold the upper clamp must be the MAX",
                tier.label
            );
        }
    }

    #[test]
    fn test_bitrate_change_threshold_is_010() {
        assert!(
            (crate::constants::BITRATE_CHANGE_THRESHOLD - 0.10).abs() < f64::EPSILON,
            "BITRATE_CHANGE_THRESHOLD must be 0.10 to keep encoder reconfigures rare; \
             changing it forces a review of keyframe-cost trade-offs"
        );
    }
}
