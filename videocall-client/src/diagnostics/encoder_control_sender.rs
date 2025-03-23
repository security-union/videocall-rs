use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

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

        // Calculate time delta since last update
        let now = Date::now();
        let dt = now - self.last_update;
        self.last_update = now;

        // Skip processing if time delta is too small or too large
        // This avoids instability from rapid updates or stale data
        if !(50.0..=1000.0).contains(&dt) {
            log::info!("Skipping update - time delta ({} ms) out of range", dt);
            return Some(self._ideal_bitrate_kbps as f64); // Return default bitrate in bps
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

        // Apply jitter penalty (up to 20% reduction for maximum jitter)
        let jitter_reduction = after_pid * (jitter_factor * 0.2);

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
}
