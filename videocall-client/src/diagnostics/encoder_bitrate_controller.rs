use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use js_sys::Date;

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
            last_cleanup: Date::now(),
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
    _ideal_bitrate_kbps: u32,
    _current_fps: Rc<AtomicU32>,
    fps_history: std::collections::VecDeque<f64>, // Sliding window of recent FPS values
    max_history_size: usize,                      // Maximum size of history window
    last_error: f64,                              // Track the previous error for stability checks
    initialization_complete: bool,                // Flag to handle startup conditions
    diagnostic_packets: DiagnosticPackets,        // Manager for multiple peers' diagnostic data
    last_correction_time: f64,                    // Timestamp of last bitrate correction
    correction_throttle_ms: f64,                  // Minimum time between corrections in ms
}

impl EncoderBitrateController {
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

        // Create diagnostic packets manager with 10-second window and 30-second timeout
        let diagnostic_packets = DiagnosticPackets::new(WINDOW_DURATION_SEC, INACTIVE_TIMEOUT_SEC);

        Self {
            pid,
            last_update: Date::now(),
            _ideal_bitrate_kbps: ideal_bitrate_kbps,
            _current_fps: current_fps,
            fps_history: std::collections::VecDeque::with_capacity(10),
            max_history_size: 10,
            last_error: 0.0,
            initialization_complete: false,
            diagnostic_packets,
            last_correction_time: 0.0, // Initialize to 0 to ensure first correction is sent
            correction_throttle_ms: 1000.0, // Default to 1Hz (1000ms)
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
            None => {
                return None;
            }
        };

        let target_fps = self._current_fps.load(Ordering::Relaxed) as f64;
        // Correct fps_received max to be the target fps
        let fps_received = worst_fps.min(target_fps);
        if target_fps <= 0.0 {
            // Update last correction time
            self.last_correction_time = now;
            return Some(self._ideal_bitrate_kbps as f64); // Default bitrate in bps if target FPS is invalid
        }

        // Add current FPS to history
        self.fps_history.push_back(fps_received);

        // Maintain history size limit
        while self.fps_history.len() > self.max_history_size {
            self.fps_history.pop_front();
        }

        // Calculate jitter (FPS standard deviation)
        let jitter = self.calculate_jitter();

        // Calculate time delta since last update using the provided timestamp
        let dt = now - self.last_update;
        self.last_update = now;

        // Compute the error: difference between target and actual FPS
        let current_error = target_fps - fps_received;

        // Special handling for initialization
        if !self.initialization_complete {
            if self.fps_history.len() >= 3 {
                self.initialization_complete = true;
            } else {
                // During initialization, just track the error but don't react strongly
                self.last_error = current_error;
                // Update last correction time
                self.last_correction_time = now;
                return Some(self._ideal_bitrate_kbps as f64); // Return default bitrate in bps during initialization
            }
        }

        // Calculate rate of change of error for smoother response
        self.last_error = current_error;

        // Use PID controller to compute adjustment based on FPS error
        let fps_error_output = self.pid.compute(current_error, dt);

        // Get the jitter factor (normalized by target FPS)
        let normalized_jitter = jitter / target_fps;
        let jitter_factor = (normalized_jitter * 5.0).min(1.0);

        // Base bitrate calculation (convert from kbps to bps)
        let base_bitrate = self._ideal_bitrate_kbps as f64;

        // Adjust bitrate based on PID output
        // Scale factor is lower (3,000) for more gradual adjustments
        let fps_adjustment = fps_error_output * 3_000.0;

        // Apply the PID-based adjustment
        let after_pid = base_bitrate - fps_adjustment;

        // Apply jitter penalty (up to 50% reduction for maximum jitter)
        let jitter_reduction = after_pid * (jitter_factor * 0.9);

        // Calculate final bitrate
        let corrected_bitrate = after_pid - jitter_reduction;

        // Calculate min and max bitrate limits based on ideal bitrate (in bps)
        let min_bitrate = (self._ideal_bitrate_kbps as f64) * 0.1; // 10% of ideal
        let max_bitrate = (self._ideal_bitrate_kbps as f64) * 1.5; // 150% of ideal

        // Log detailed diagnostic information
        log::debug!(
            "FPS: target={:.1} received={:.1} error={:.1} | PID output={:.2} | Jitter={:.2} factor={:.2} | Bitrate: base={:.0} bps pid_adj={:.0} jitter_adj={:.0} final={:.0} bps | Peers: {}",
            target_fps, fps_received, current_error,
            fps_error_output, jitter, jitter_factor,
            base_bitrate, fps_adjustment, jitter_reduction, corrected_bitrate,
            self.diagnostic_packets.peer_count()
        );

        // Ensure we have a reasonable bitrate (between min_bitrate and max_bitrate)
        let final_bitrate = if !(min_bitrate..=max_bitrate).contains(&corrected_bitrate)
            || corrected_bitrate.is_nan()
        {
            log::warn!(
                "Bitrate out of bounds or NaN: {:.0} kbps (min: {:.0} kbps, max: {:.0} kbps)",
                corrected_bitrate,
                min_bitrate,
                max_bitrate
            );
            // Use a safe default value instead
            f64::max(min_bitrate, f64::min(base_bitrate, max_bitrate))
        } else {
            corrected_bitrate
        };

        // Update last correction time
        self.last_correction_time = now;

        Some(final_bitrate)
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
                    "Expected bitrate close to base ({} kbps), got {} kbps",
                    ideal_bitrate_kbps,
                    bitrate
                );
            }
        }

        // Check history shows stable FPS
        assert_eq!(controller.fps_history.len(), 10);
        let jitter = controller.calculate_jitter();
        assert!(
            jitter < 0.1,
            "Expected near-zero jitter in happy path, got {}",
            jitter
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
                "Expected reduced bitrate due to poor peer, got {} bps (ideal: {} bps)",
                bitrate,
                ideal_bitrate_kbps
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

        // Fast forward 20 seconds (less than the 30-second timeout)
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

        // Fast forward another 15 seconds (total 35 seconds, > 30-second timeout)
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
            "Expected bitrate to decrease when FPS drops. Good: {}, Poor: {}",
            good_bitrate,
            poor_bitrate
        );

        // Verify that the bitrate is within the expected bounds (min and max)
        let min_bitrate = (ideal_bitrate_kbps) as f64 * 0.1; // 10% of ideal
        let max_bitrate = (ideal_bitrate_kbps) as f64 * 1.5; // 150% of ideal
        assert!(
            poor_bitrate >= min_bitrate,
            "Poor bitrate {} bps should be greater than or equal to minimum bitrate {} bps",
            poor_bitrate,
            min_bitrate
        );
        assert!(
            poor_bitrate <= max_bitrate,
            "Poor bitrate {} bps should be less than or equal to maximum bitrate {} bps",
            poor_bitrate,
            max_bitrate
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
            "Expected jitter {}, got {}",
            expected_jitter,
            actual_jitter
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
            "Expected jitter {}, got {}",
            expected_jitter,
            actual_jitter
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
}
