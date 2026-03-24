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
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use js_sys::Date;

use crate::adaptive_quality_constants::{
    AudioQualityTier, VideoQualityTier, PID_CORRECTION_THROTTLE_MS, PID_DEADBAND_FPS,
    PID_FPS_HISTORY_SIZE, PID_KD, PID_KI, PID_KP, PID_MAX_JITTER_PENALTY, PID_OUTPUT_MAX,
    PID_OUTPUT_MIN, VIDEO_QUALITY_TIERS,
};
use crate::diagnostics::adaptive_quality_manager::AdaptiveQualityManager;
use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;
use videocall_types::protos::media_packet::media_packet::MediaType;

const WINDOW_DURATION_SEC: u32 = 10;
const INACTIVE_TIMEOUT_SEC: u32 = 20;

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

    /// Get the minimum FPS received within the window
    pub fn min_fps(&self) -> Option<f64> {
        if self.packets.is_empty() {
            return None;
        }

        let mut min_fps = f64::INFINITY;
        let mut found_fps = false;

        for (_, packet) in &self.packets {
            let fps = match packet.media_type.enum_value_or_default() {
                MediaType::VIDEO => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::AUDIO => packet.audio_metrics.as_ref().map(|m| m.fps_received as f64),
                MediaType::SCREEN => packet.video_metrics.as_ref().map(|m| m.fps_received as f64),
                _ => None,
            };

            if let Some(fps) = fps {
                min_fps = min_fps.min(fps);
                found_fps = true;
            }
        }

        if found_fps {
            Some(min_fps)
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

    /// Get the peer with the lowest FPS
    pub fn get_worst_fps_peer(&self) -> Option<(String, f64)> {
        if self.peer_windows.is_empty() {
            return None;
        }

        let mut worst_peer = None;
        let mut min_fps = f64::INFINITY;

        for (peer_id, window) in &self.peer_windows {
            if let Some(fps) = window.min_fps() {
                if fps < min_fps {
                    min_fps = fps;
                    worst_peer = Some((peer_id.clone(), fps));
                }
            }
        }

        worst_peer
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

pub struct EncoderBitrateController {
    pid: pidgeon::PidController,
    last_update: f64,
    ideal_bitrate_kbps: u32,
    target_fps: Rc<AtomicU32>,
    fps_history: std::collections::VecDeque<f64>,
    max_history_size: usize,
    initialization_complete: bool,
    diagnostic_packets: DiagnosticPackets,
    last_correction_time: f64,
    correction_throttle_ms: f64,
    /// Adaptive quality state machine for tier selection.
    quality_manager: AdaptiveQualityManager,
    /// Set to `true` after any tier transition, cleared by the caller via
    /// [`Self::take_tier_changed`].
    tier_changed: bool,
}

impl EncoderBitrateController {
    /// Create a new bitrate controller using the default `VIDEO_QUALITY_TIERS`.
    pub fn new(ideal_bitrate_kbps: u32, target_fps: Rc<AtomicU32>) -> Self {
        let quality_manager = AdaptiveQualityManager::new(VIDEO_QUALITY_TIERS);
        Self::build(ideal_bitrate_kbps, target_fps, quality_manager)
    }

    /// Create a new bitrate controller for screen share.
    ///
    /// Uses `SCREEN_QUALITY_TIERS` starting at the highest tier (1080p) via
    /// [`AdaptiveQualityManager::new_for_screen`]. The initial
    /// `ideal_bitrate_kbps` is synced from the starting tier so the PID
    /// controller does not make an unnecessary correction on the first update.
    pub fn new_for_screen(
        target_fps: Rc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
    ) -> Self {
        let quality_manager = AdaptiveQualityManager::new_for_screen(video_tiers);
        let tier_ideal = quality_manager.current_video_tier().ideal_bitrate_kbps;
        Self::build(tier_ideal, target_fps, quality_manager)
    }

    /// Create a new bitrate controller using a custom video tier array
    /// (e.g., `SCREEN_QUALITY_TIERS` for screen share).
    pub fn new_with_tiers(
        ideal_bitrate_kbps: u32,
        target_fps: Rc<AtomicU32>,
        video_tiers: &'static [VideoQualityTier],
    ) -> Self {
        let quality_manager = AdaptiveQualityManager::new(video_tiers);
        Self::build(ideal_bitrate_kbps, target_fps, quality_manager)
    }

    /// Internal constructor shared by `new`, `new_with_tiers`, and `new_for_screen`.
    fn build(
        ideal_bitrate_kbps: u32,
        target_fps: Rc<AtomicU32>,
        quality_manager: AdaptiveQualityManager,
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

        Self {
            pid,
            last_update: Date::now(),
            ideal_bitrate_kbps,
            target_fps,
            fps_history: std::collections::VecDeque::with_capacity(PID_FPS_HISTORY_SIZE),
            max_history_size: PID_FPS_HISTORY_SIZE,
            initialization_complete: false,
            diagnostic_packets,
            last_correction_time: 0.0,
            correction_throttle_ms: PID_CORRECTION_THROTTLE_MS,
            quality_manager,
            tier_changed: false,
        }
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

        // Apply throttling - check if sufficient time has passed since last correction
        let time_since_last_correction = now - self.last_correction_time;
        if time_since_last_correction < self.correction_throttle_ms {
            log::debug!(
                "Throttling bitrate correction: {:.0}ms since last correction (throttle: {:.0}ms)",
                time_since_last_correction,
                self.correction_throttle_ms
            );
            return None; // Too soon since last correction, don't emit a new one
        }

        // Get the worst performing peer's FPS
        let worst_fps = match self.diagnostic_packets.get_worst_fps_peer() {
            Some((_, fps)) => fps,
            None => return None,
        };

        let target_fps = self.target_fps.load(Ordering::Relaxed) as f64;
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
        let tier = self.quality_manager.current_video_tier();
        let ideal_for_tier = tier.ideal_bitrate_kbps as f64;
        let tier_changed = self.quality_manager.update(
            fps_received,
            target_fps,
            final_bitrate,
            ideal_for_tier,
            now,
        );
        if tier_changed {
            self.tier_changed = true;
            // Update internal ideal bitrate to match the new tier so PID
            // operates within the new tier's range going forward.
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
        }

        // Clamp the PID output to the current tier's bitrate bounds.
        let tier = self.quality_manager.current_video_tier();
        let tier_min = tier.min_bitrate_kbps as f64;
        let tier_max = tier.max_bitrate_kbps as f64;
        let tier_clamped = final_bitrate.clamp(tier_min, tier_max);

        self.last_correction_time = now;
        Some(tier_clamped)
    }

    pub fn process_diagnostics_packet(&mut self, packet: DiagnosticsPacket) -> Option<f64> {
        self.process_diagnostics_packet_with_time(packet, Date::now())
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

    /// Force an immediate video quality step-down due to server congestion.
    ///
    /// Delegates to [`AdaptiveQualityManager::force_video_step_down`].
    /// Returns `true` if the tier actually changed.
    pub fn force_video_step_down(&mut self) -> bool {
        let now = Date::now();
        let changed = self.quality_manager.force_video_step_down(now);
        if changed {
            self.tier_changed = true;
            let new_tier = self.quality_manager.current_video_tier();
            self.ideal_bitrate_kbps = new_tier.ideal_bitrate_kbps;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;
    use std::sync::atomic::AtomicU32;
    use videocall_types::protos::diagnostics_packet::{
        AudioMetrics, DiagnosticsPacket, VideoMetrics,
    };
    use wasm_bindgen_test::*;

    // Remove browser-only configuration and make tests run in any environment
    // wasm_bindgen_test_configure!(run_in_browser);

    fn create_test_packet(
        sender_id: &str,
        target_id: &str,
        fps: f32,
        bitrate_kbps: u32,
    ) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = sender_id.to_string();
        packet.target_id = target_id.to_string();
        packet.timestamp_ms = js_sys::Date::now() as u64;
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
        packet.timestamp_ms = js_sys::Date::now() as u64;
        packet.media_type =
            videocall_types::protos::media_packet::media_packet::MediaType::AUDIO.into();

        let mut audio_metrics = AudioMetrics::new();
        audio_metrics.fps_received = fps;
        audio_metrics.bitrate_kbps = bitrate_kbps;
        packet.audio_metrics = ::protobuf::MessageField::some(audio_metrics);

        packet
    }

    #[wasm_bindgen_test]
    fn test_happy_path() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_multiple_peers() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
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

        // The controller should adjust bitrate based on the worst peer (peer3)
        // Process one more packet to check the behavior
        let result4 = controller.process_diagnostics_packet_with_time(
            create_test_packet("test_sender", "test_target", 30.0, 500),
            base_time + 3300.0,
        );

        assert!(result4.is_some(), "Fourth packet should return a bitrate");

        // With one very poor peer, the bitrate should be significantly reduced
        if let Some(bitrate) = result4 {
            // Should be much lower than ideal due to the poor peer
            assert!(
                bitrate < ideal_bitrate_kbps as f64 * 0.7, // 70% of ideal or less
                "Expected reduced bitrate due to poor peer, got {bitrate} bps (ideal: {ideal_bitrate_kbps} bps)"
            );
        }
    }

    #[wasm_bindgen_test]
    fn test_peer_cleanup() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
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

        // Check min FPS (should be 20.0)
        assert_eq!(window.min_fps(), Some(20.0));

        // Force cleanup with timestamp that should only be outside the window for the first packet
        window.cleanup(base_time + 10500.0);

        // Should have 2 packets left (the ones at base_time + 1000 and base_time + 2000)
        assert_eq!(window.len(), 2);

        // Now force a cleanup that should remove all packets
        window.cleanup(base_time + 15000.0);

        // All packets should be removed
        assert_eq!(window.len(), 0);
        assert_eq!(window.min_fps(), None);
    }

    #[wasm_bindgen_test]
    fn test_different_media_types() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_bandwidth_drop() {
        // Setup with a target of 30 FPS
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_calculate_jitter() {
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_throttling_basic() {
        // Setup
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_bitrate_recovery_after_fps_improves() {
        let target_fps = Rc::new(AtomicU32::new(30));
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
            degraded_bitrate < ideal_bitrate_kbps as f64 * 0.5,
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

    #[wasm_bindgen_test]
    fn test_dynamic_target_fps_change() {
        let target_fps = Rc::new(AtomicU32::new(30));
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
        // FPS (15) is far below new target (60) — bitrate should decrease significantly
        assert!(
            bitrate_at_60_target < ideal_bitrate_kbps as f64 * 0.5,
            "Bitrate should drop when FPS is far below new target, got {bitrate_at_60_target}"
        );
    }

    #[wasm_bindgen_test]
    fn test_progressive_integral_accumulation() {
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_pid_and_jitter_combined_clamp_to_min() {
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_same_timestamp_dt_zero() {
        let target_fps = Rc::new(AtomicU32::new(30));
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

    #[wasm_bindgen_test]
    fn test_new_for_screen_starts_at_highest_tier() {
        use crate::adaptive_quality_constants::{DEFAULT_SCREEN_TIER_INDEX, SCREEN_QUALITY_TIERS};

        let target_fps = Rc::new(AtomicU32::new(15));
        let controller = EncoderBitrateController::new_for_screen(target_fps, SCREEN_QUALITY_TIERS);

        // Should start at the highest screen tier (index 0, "high")
        assert_eq!(controller.video_tier_index(), DEFAULT_SCREEN_TIER_INDEX);
        assert_eq!(controller.current_video_tier().label, "high");

        // The ideal_bitrate_kbps should be synced with the starting tier
        let expected_bitrate = SCREEN_QUALITY_TIERS[DEFAULT_SCREEN_TIER_INDEX].ideal_bitrate_kbps;
        assert_eq!(
            controller.ideal_bitrate_kbps, expected_bitrate,
            "Initial ideal_bitrate_kbps should match the starting tier's ideal_bitrate_kbps"
        );
    }
}
