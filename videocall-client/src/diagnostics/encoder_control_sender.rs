use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use js_sys::Date;

use videocall_types::protos::diagnostics_packet::DiagnosticsPacket;

/// EncoderControl is responsible for bridging the gap between the encoder and the
/// diagnostics system.
/// It closes the loop by allowing the encoder to adjust its settings based on
/// feedback from the diagnostics system.
#[derive(Debug, Clone)]
pub enum EncoderControl {
    UpdateBitrate { target_bitrate_kbps: u32 },
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
        // Extract the received FPS from the packet
        let fps_received = match packet.video_metrics.as_ref() {
            Some(metrics) => metrics.fps_received as f64,
            None => return None, // No video metrics available
        };
        let target_fps = self._current_fps.load(Ordering::Relaxed) as f64;
        // Correct fps_received max to be the target fps
        let fps_received = fps_received.min(target_fps);
        if target_fps <= 0.0 {
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
        log::info!(
            "FPS: target={:.1} received={:.1} error={:.1} | PID output={:.2} | Jitter={:.2} factor={:.2} | Bitrate: base={:.0} bps pid_adj={:.0} jitter_adj={:.0} final={:.0} bps", 
            target_fps, fps_received, current_error,
            fps_error_output, jitter, jitter_factor,
            base_bitrate, fps_adjustment, jitter_reduction, corrected_bitrate
        );

        // Ensure we have a reasonable bitrate (between min_bitrate and max_bitrate)
        if !(min_bitrate..=max_bitrate).contains(&corrected_bitrate) || corrected_bitrate.is_nan() {
            log::warn!(
                "Bitrate out of bounds or NaN: {:.0} kbps (min: {:.0} kbps, max: {:.0} kbps)",
                corrected_bitrate,
                min_bitrate,
                max_bitrate
            );
            // Return a safe default value instead of None to maintain stability
            return Some(f64::max(min_bitrate, f64::min(base_bitrate, max_bitrate)));
        }

        Some(corrected_bitrate)
    }

    pub fn process_diagnostics_packet(&mut self, packet: DiagnosticsPacket) -> Option<f64> {
        self.process_diagnostics_packet_with_time(packet, Date::now())
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
            videocall_types::protos::media_packet::media_packet::MediaType::VIDEO.into();

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
        // Use 500 kbps as the ideal bitrate
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderControlSender::new(ideal_bitrate_kbps, target_fps.clone());

        // Generate a series of packets with perfect conditions
        // FPS matches the target exactly, no jitter
        for _ in 0..10 {
            let packet = create_test_packet(30.0, 500);
            let result = controller.process_diagnostics_packet(packet);

            // With perfect conditions (no error, no jitter),
            // the bitrate should stay close to the ideal
            if let Some(bitrate) = result {
                // Should be close to base bitrate (in kbps)
                assert!(
                    (bitrate - (ideal_bitrate_kbps) as f64).abs() < 10.0,
                    "Expected bitrate close to base ({} kbps), got {} kbps",
                    ideal_bitrate_kbps,
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
        // Use 500 kbps as the ideal bitrate
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderControlSender::new(ideal_bitrate_kbps, target_fps.clone());

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
    fn test_degraded_system_gradual_recovery() {
        // Setup with standard parameters
        let target_fps = Rc::new(AtomicU32::new(30));
        let ideal_bitrate_kbps = 500;
        let mut controller = EncoderControlSender::new(ideal_bitrate_kbps, target_fps.clone());

        // Simulation time control
        let start_time = 1000.0;
        let mut current_time = start_time;
        let time_step_ms = 100.0;

        // Initialize controller
        controller.last_update = start_time;

        // Data collection for trend analysis
        let mut all_bitrates: Vec<f64> = Vec::new();
        let mut all_fps_values: Vec<f32> = Vec::new();

        log::info!("=== BASELINE PHASE: Initial stable conditions ===");

        // Phase 0: Establish baseline (30 seconds of stable conditions)
        let stable_fps = 30.0;
        let baseline_duration_ms = 30_000.0;
        let baseline_end_time = current_time + baseline_duration_ms;

        let mut baseline_bitrates: Vec<f64> = Vec::new();

        // Run with stable conditions
        while current_time < baseline_end_time {
            current_time += time_step_ms;

            let packet = create_test_packet(stable_fps, 500);
            let bitrate = controller
                .process_diagnostics_packet_with_time(packet, current_time)
                .unwrap();

            baseline_bitrates.push(bitrate);
            all_bitrates.push(bitrate);
            all_fps_values.push(stable_fps);

            // Log periodically
            if (current_time - start_time) % 5000.0 < time_step_ms {
                log::info!(
                    "Baseline at {:.1}s: FPS={:.1}, Bitrate={:.1}",
                    (current_time - start_time) / 1000.0,
                    stable_fps,
                    bitrate
                );
            }
        }

        // Verify baseline is stable
        let baseline_mean = baseline_bitrates.iter().sum::<f64>() / baseline_bitrates.len() as f64;
        let baseline_variance = baseline_bitrates
            .iter()
            .map(|&x| (x - baseline_mean).powi(2))
            .sum::<f64>()
            / baseline_bitrates.len() as f64;
        let baseline_std_dev = baseline_variance.sqrt();

        log::info!(
            "Baseline stats: Mean={:.1}, StdDev={:.1} ({:.1}% of mean)",
            baseline_mean,
            baseline_std_dev,
            (baseline_std_dev / baseline_mean) * 100.0
        );

        // Verify baseline is relatively stable (std dev should be small percentage of mean)
        assert!(
            baseline_std_dev < baseline_mean * 0.05,
            "Expected stable baseline, but std dev ({:.1}) is too high relative to mean ({:.1})",
            baseline_std_dev,
            baseline_mean
        );

        log::info!("=== DEGRADATION PHASE: Network deterioration ===");

        // Phase 1: Network deterioration (60 seconds)
        let degraded_fps = 5.0;
        let degradation_duration_ms = 60_000.0;
        let degradation_end_time = current_time + degradation_duration_ms;

        let mut degradation_bitrates: Vec<f64> = Vec::new();
        let mut degradation_bitrate_trend: Vec<f64> = Vec::new();

        // Capture initial conditions for comparison
        let initial_degradation_time = current_time;

        while current_time < degradation_end_time {
            current_time += time_step_ms;

            let packet = create_test_packet(degraded_fps, 500);
            let bitrate = controller
                .process_diagnostics_packet_with_time(packet, current_time)
                .unwrap();

            degradation_bitrates.push(bitrate);
            all_bitrates.push(bitrate);
            all_fps_values.push(degraded_fps);

            // Sample trend at regular intervals
            if (current_time - initial_degradation_time) % 5000.0 < time_step_ms {
                degradation_bitrate_trend.push(bitrate);

                log::info!(
                    "Degradation at {:.1}s: FPS={:.1}, Bitrate={:.1}",
                    (current_time - start_time) / 1000.0,
                    degraded_fps,
                    bitrate
                );
            }
        }

        // Verify degradation response
        // 1. Direction: Bitrate should trend downward
        // 2. Consistency: Changes should be gradual

        let degradation_trend_len = degradation_bitrate_trend.len();
        if degradation_trend_len >= 3 {
            // Analyze the general trend (should be downward)
            let initial_samples_avg = degradation_bitrate_trend
                .iter()
                .take(degradation_trend_len / 3)
                .sum::<f64>()
                / (degradation_trend_len / 3) as f64;

            let final_samples_avg = degradation_bitrate_trend
                .iter()
                .skip(2 * degradation_trend_len / 3)
                .sum::<f64>()
                / (degradation_trend_len - 2 * degradation_trend_len / 3) as f64;

            log::info!(
                "Degradation trend: Initial avg={:.1}, Final avg={:.1}, Change={:.1}%",
                initial_samples_avg,
                final_samples_avg,
                (final_samples_avg - initial_samples_avg) / initial_samples_avg * 100.0
            );

            // We just want to verify the direction, not a specific magnitude
            // The controller should trend downward over time when FPS is poor
            assert!(
                final_samples_avg <= initial_samples_avg,
                "Expected bitrate to trend downward during degradation"
            );
        }

        // Verify smooth transitions (no wild jumps)
        for window in degradation_bitrates.windows(2) {
            let delta = (window[1] - window[0]).abs();
            // Relative change should be small per step
            let max_delta_percent = 0.75; // 75% max change per step
            let max_allowed_delta = window[0] * max_delta_percent;

            assert!(
                delta <= max_allowed_delta,
                "Detected excessive bitrate change: {:.1} to {:.1} (change of {:.1}% exceeds {:.1}%)",
                window[0], window[1],
                (delta / window[0]) * 100.0,
                max_delta_percent * 100.0
            );
        }

        log::info!("=== RECOVERY PHASE: Network improvement ===");

        // Phase 2: Network recovery (90 seconds)
        let recovery_duration_ms = 90_000.0;
        let recovery_end_time = current_time + recovery_duration_ms;

        let mut recovery_bitrates: Vec<f64> = Vec::new();
        let mut recovery_bitrate_trend: Vec<f64> = Vec::new();

        // Capture initial recovery state
        let initial_recovery_time = current_time;

        while current_time < recovery_end_time {
            current_time += time_step_ms;

            // Linear improvement from degraded_fps back to stable_fps
            let progress = (current_time - initial_recovery_time) / recovery_duration_ms;
            let current_fps = degraded_fps + (stable_fps - degraded_fps) * progress as f32;

            let packet = create_test_packet(current_fps, 500);
            let bitrate = controller
                .process_diagnostics_packet_with_time(packet, current_time)
                .unwrap();

            recovery_bitrates.push(bitrate);
            all_bitrates.push(bitrate);
            all_fps_values.push(current_fps);

            // Sample trend at regular intervals
            if (current_time - initial_recovery_time) % 5000.0 < time_step_ms {
                recovery_bitrate_trend.push(bitrate);

                log::info!(
                    "Recovery at {:.1}s: FPS={:.1}, Bitrate={:.1}, Progress={:.1}%",
                    (current_time - start_time) / 1000.0,
                    current_fps,
                    bitrate,
                    progress * 100.0
                );
            }
        }

        // Verify recovery response
        // 1. Direction: Bitrate should trend upward
        // 2. Consistency: Changes should be gradual

        let recovery_trend_len = recovery_bitrate_trend.len();
        if recovery_trend_len >= 3 {
            // Analyze the general trend (should be upward)
            let initial_samples_avg = recovery_bitrate_trend
                .iter()
                .take(recovery_trend_len / 3)
                .sum::<f64>()
                / (recovery_trend_len / 3) as f64;

            let final_samples_avg = recovery_bitrate_trend
                .iter()
                .skip(2 * recovery_trend_len / 3)
                .sum::<f64>()
                / (recovery_trend_len - 2 * recovery_trend_len / 3) as f64;

            log::info!(
                "Recovery trend: Initial avg={:.1}, Final avg={:.1}, Change={:.1}%",
                initial_samples_avg,
                final_samples_avg,
                (final_samples_avg - initial_samples_avg) / initial_samples_avg * 100.0
            );

            // We expect the trend to be upward during recovery
            assert!(
                final_samples_avg >= initial_samples_avg,
                "Expected bitrate to trend upward during recovery"
            );
        }

        // Verify smooth transitions (no wild jumps)
        for window in recovery_bitrates.windows(2) {
            let delta = (window[1] - window[0]).abs();
            // Relative change should be small per step
            let max_delta_percent = 0.25; // 25% max change per step
            let max_allowed_delta = window[0] * max_delta_percent;

            assert!(
                delta <= max_allowed_delta,
                "Detected excessive bitrate change: {:.1} to {:.1} (change of {:.1}% exceeds {:.1}%)",
                window[0], window[1],
                (delta / window[0]) * 100.0,
                max_delta_percent * 100.0
            );
        }

        // Overall simulation statistics
        let overall_min = all_bitrates.iter().copied().fold(f64::INFINITY, f64::min);
        let overall_max = all_bitrates
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let degradation_min = degradation_bitrates
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min);
        let recovery_max = recovery_bitrates
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);

        log::info!("=== TEST SUMMARY ===");
        log::info!(
            "Simulation duration: {:.1} seconds",
            (current_time - start_time) / 1000.0
        );
        log::info!("Baseline bitrate: {:.1} kbps", baseline_mean);
        log::info!(
            "Overall range: {:.1} - {:.1} kbps",
            overall_min,
            overall_max
        );
        log::info!("Lowest during degradation: {:.1} kbps", degradation_min);
        log::info!("Highest during recovery: {:.1} kbps", recovery_max);
        log::info!(
            "FPS range: {:.1} - {:.1}",
            all_fps_values.iter().copied().fold(f32::INFINITY, f32::min),
            all_fps_values
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max)
        );

        // Verify overall behavior is adaptive
        assert!(
            degradation_min < baseline_mean && recovery_max > degradation_min,
            "Controller should adapt bitrate down during degradation and up during recovery"
        );
    }

    #[wasm_bindgen_test]
    fn test_calculate_jitter() {
        let target_fps = Rc::new(AtomicU32::new(30));
        let mut controller = EncoderControlSender::new(500, target_fps);

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
}
