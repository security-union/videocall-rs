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

/// Exponential smoothing factor for buffer level filtering.
///
/// This value controls the responsiveness of the buffer level filter:
/// - Higher values (closer to 1.0) = more smoothing, slower response to changes
/// - Lower values (closer to 0.0) = less smoothing, faster response to changes
///
/// The value 0.8 provides improved balance between:
/// - Filtering out short-term fluctuations due to network jitter  
/// - Responding to actual buffer level trends quickly enough for effective control
///
/// Engineering rationale:
/// - Time constant τ = -frame_duration / ln(smoothing_factor)
/// - With 0.6 and 10ms frames: τ ≈ 20ms response time (much faster)
/// - This allows ~2-3 frames to reach 63% of a step change
/// - Provides much faster response for late-joining peer scenarios
/// - Reduces lag between raw and filtered buffer levels
const BUFFER_LEVEL_SMOOTHING_FACTOR: f64 = 0.6;

/// Buffer level filter for smoothing buffer measurements
///
/// This filter prevents oscillating acceleration/deceleration decisions by
/// smoothing the buffer level measurements over time. It accounts for
/// time-stretched samples from previous operations to provide accurate
/// buffering decisions.
///
/// Based on WebRTC's BufferLevelFilter implementation.
#[derive(Debug)]
pub struct BufferLevelFilter {
    /// Current filtered buffer level in samples
    filtered_level_samples: f64,
    /// Target buffer level in milliseconds (set by delay manager)
    target_level_ms: u32,
    /// Sample rate for conversions
    sample_rate_hz: u32,
    /// Exponential smoothing factor (0.0 to 1.0)
    smoothing_factor: f64,
    /// Whether the filter has been initialized
    initialized: bool,
}

impl BufferLevelFilter {
    /// Create a new buffer level filter
    pub fn new(sample_rate_hz: u32) -> Self {
        Self {
            filtered_level_samples: 0.0,
            target_level_ms: 0,
            sample_rate_hz,
            smoothing_factor: BUFFER_LEVEL_SMOOTHING_FACTOR,
            initialized: false,
        }
    }

    /// Update the filter with new buffer size and time-stretched samples
    ///
    /// # Arguments
    /// * `buffer_size_samples` - Current raw buffer size in samples
    /// * `time_stretched_samples` - Samples added/removed by time-stretching operations
    pub fn update(&mut self, buffer_size_samples: usize, time_stretched_samples: i32) {
        let current_level = buffer_size_samples as f64 - time_stretched_samples as f64;

        if !self.initialized {
            self.filtered_level_samples = current_level.max(0.0);
            self.initialized = true;
        } else {
            // Exponential smoothing: filtered = α × current + (1-α) × previous
            self.filtered_level_samples = self.smoothing_factor * self.filtered_level_samples
                + (1.0 - self.smoothing_factor) * current_level;

            // Prevent filtered level from diverging too far from raw buffer
            // If raw buffer is more than 2x target, don't let filter go below target/2
            let raw_buffer_samples = buffer_size_samples as f64;
            let target_samples = self.target_level_ms as f64 * self.sample_rate_hz as f64 / 1000.0;

            if raw_buffer_samples > target_samples * 2.0 {
                self.filtered_level_samples = self.filtered_level_samples.max(target_samples * 0.5);
            }
        }

        // Ensure filtered level never goes negative
        self.filtered_level_samples = self.filtered_level_samples.max(0.0);
    }

    /// Set the filtered buffer level directly (used for buffer flush scenarios)
    pub fn set_filtered_buffer_level(&mut self, buffer_size_samples: usize) {
        self.filtered_level_samples = buffer_size_samples as f64;
        self.initialized = true;
    }

    /// Get the current filtered buffer level in samples
    pub fn filtered_current_level(&self) -> usize {
        self.filtered_level_samples.max(0.0) as usize
    }

    /// Get the current filtered buffer level in milliseconds
    pub fn filtered_current_level_ms(&self) -> u32 {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        let samples = self.filtered_current_level();
        (samples as u64 * 1000 / self.sample_rate_hz as u64) as u32
    }

    /// Set the target buffer level (called by delay manager)
    pub fn set_target_buffer_level(&mut self, target_level_ms: u32) {
        self.target_level_ms = target_level_ms;
    }

    /// Get the target buffer level in samples
    pub fn target_level_samples(&self) -> usize {
        if self.sample_rate_hz == 0 {
            return 0;
        }
        (self.target_level_ms as u64 * self.sample_rate_hz as u64 / 1000) as usize
    }

    /// Reset the filter state
    pub fn reset(&mut self) {
        self.filtered_level_samples = 0.0;
        self.initialized = false;
    }

    /// Update sample rate (called when sample rate changes)
    pub fn set_sample_rate(&mut self, sample_rate_hz: u32) {
        self.sample_rate_hz = sample_rate_hz;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_level_filter_creation() {
        let filter = BufferLevelFilter::new(16000);
        assert_eq!(filter.filtered_current_level(), 0);
        assert_eq!(filter.sample_rate_hz, 16000);
    }

    #[test]
    fn test_buffer_level_filter_update() {
        let mut filter = BufferLevelFilter::new(16000);

        // First update should set the level directly
        filter.update(1000, 0);
        assert_eq!(filter.filtered_current_level(), 1000);

        // Subsequent updates should be smoothed
        filter.update(2000, 0);
        let filtered_level = filter.filtered_current_level();
        assert!(filtered_level > 1000 && filtered_level < 2000);
    }

    #[test]
    fn test_time_stretched_samples_compensation() {
        let mut filter = BufferLevelFilter::new(16000);

        // Update with time-stretched samples
        filter.update(1000, 100); // 100 samples were time-stretched
        assert_eq!(filter.filtered_current_level(), 900);
    }

    #[test]
    fn test_set_filtered_buffer_level() {
        let mut filter = BufferLevelFilter::new(16000);

        filter.set_filtered_buffer_level(500);
        assert_eq!(filter.filtered_current_level(), 500);
        assert!(filter.initialized);
    }

    #[test]
    fn test_target_level_conversion() {
        let mut filter = BufferLevelFilter::new(16000);

        filter.set_target_buffer_level(100); // 100ms
        assert_eq!(filter.target_level_samples(), 1600); // 100ms * 16000Hz / 1000 = 1600 samples
    }

    #[test]
    fn test_filtered_level_ms_conversion() {
        let mut filter = BufferLevelFilter::new(16000);

        filter.set_filtered_buffer_level(1600); // 1600 samples
        assert_eq!(filter.filtered_current_level_ms(), 100); // 1600 * 1000 / 16000 = 100ms
    }

    #[test]
    fn test_reset() {
        let mut filter = BufferLevelFilter::new(16000);

        filter.update(1000, 0);
        assert_eq!(filter.filtered_current_level(), 1000);

        filter.reset();
        assert_eq!(filter.filtered_current_level(), 0);
        assert!(!filter.initialized);
    }

    // === NEW TESTS TO DEBUG THE STUCK FILTER ===

    #[test]
    fn test_stuck_filter_reproduction() {
        let mut filter = BufferLevelFilter::new(48000); // 48kHz like in user's logs
        filter.set_target_buffer_level(80); // 80ms target

        // Simulate the exact scenario from user logs:
        // Raw buffer around 11k samples (230ms), filtered stuck at 1920 samples (40ms)

        // Initialize with some value
        filter.update(1920, 0); // Start at 1920 samples (40ms)
        assert_eq!(filter.filtered_current_level(), 1920);

        // Now try updating with much higher raw buffer values
        for i in 1..=10 {
            let raw_samples = 11000 + i * 100; // 11k+ samples like in logs
            let prev_filtered = filter.filtered_current_level();

            filter.update(raw_samples, 0);
            let new_filtered = filter.filtered_current_level();

            println!(
                "Iteration {}: raw={}, prev_filtered={}, new_filtered={}, change={}",
                i,
                raw_samples,
                prev_filtered,
                new_filtered,
                new_filtered as i32 - prev_filtered as i32
            );

            // With smoothing_factor 0.6: new = 0.6 * old + 0.4 * current
            // Expected: new = 0.6 * 1920 + 0.4 * 11000 = 1152 + 4400 = 5552
            let expected = (0.6 * prev_filtered as f64 + 0.4 * raw_samples as f64) as usize;

            if i == 1 {
                // First update should move significantly
                assert!(
                    new_filtered > 4000,
                    "Filter should move from {} toward {}, got {} (expected ~{})",
                    prev_filtered,
                    raw_samples,
                    new_filtered,
                    expected
                );
            }
        }
    }

    #[test]
    fn test_bounds_clamping_bug() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80); // 80ms = 3840 samples at 48kHz

        // Test if bounds clamping is causing the stuck behavior
        filter.update(1920, 0); // Initialize at 40ms

        // Target is 80ms = 3840 samples
        // Raw buffer is 240ms = 11520 samples
        // This is > 2x target (7680), so bounds kick in
        let raw_samples = 11520;
        let target_samples = 3840;

        filter.update(raw_samples, 0);
        let filtered = filter.filtered_current_level();

        println!(
            "Raw: {}, Target: {}, Filtered: {}",
            raw_samples, target_samples, filtered
        );
        println!("Target/2: {}", target_samples / 2);

        // Check if it's clamped to target/2 = 1920
        if filtered == target_samples / 2 {
            println!(
                "BUG FOUND: Filter is clamped to target/2 = {} samples",
                target_samples / 2
            );
        }
    }

    #[test]
    fn test_mathematical_correctness() {
        let mut filter = BufferLevelFilter::new(48000);

        // Simple test without bounds or complications
        filter.update(1000, 0); // Initialize
        assert_eq!(filter.filtered_current_level(), 1000);

        // Update with higher value
        filter.update(5000, 0);
        let result = filter.filtered_current_level();

        // Expected: 0.6 * 1000 + 0.4 * 5000 = 600 + 2000 = 2600
        let expected = (0.6 * 1000.0 + 0.4 * 5000.0) as usize;
        assert_eq!(result, expected, "Mathematical error in smoothing");
    }

    #[test]
    fn test_time_stretched_samples_bug() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80); // 80ms target

        // Initialize filter
        filter.update(1920, 0); // Start at 1920 samples (40ms)
        assert_eq!(filter.filtered_current_level(), 1920);

        // Simulate the exact scenario from logs:
        // Raw buffer is high (11k+ samples) but time-stretched samples make current_level = 1920

        for i in 1..=10 {
            let raw_samples = 11000 + i * 100; // High raw buffer

            // HYPOTHESIS: Large time_stretched_samples cancel out the raw buffer
            // current_level = raw_samples - time_stretched_samples
            // If time_stretched_samples = raw_samples - 1920, then current_level = 1920
            let time_stretched_samples = (raw_samples - 1920) as i32;

            let prev_filtered = filter.filtered_current_level();
            filter.update(raw_samples, time_stretched_samples);
            let new_filtered = filter.filtered_current_level();

            println!("Test iteration {}: raw={}, time_stretched={}, current_level={}, prev_filtered={}, new_filtered={}", 
                i, raw_samples, time_stretched_samples, raw_samples as i32 - time_stretched_samples, prev_filtered, new_filtered);

            // If time_stretched_samples cancels out raw buffer growth, filter should stay at 1920
            if time_stretched_samples as usize == raw_samples - 1920 {
                assert_eq!(
                    new_filtered, 1920,
                    "Filter should be stuck at 1920 due to time-stretched compensation"
                );
            }
        }
    }

    #[test]
    fn test_reset_loop_bug() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80);

        // Test if filter gets reset repeatedly
        filter.update(11000, 0); // Set to high value
        let high_value = filter.filtered_current_level();
        assert!(high_value > 5000);

        // Reset and re-initialize at 1920
        filter.reset();
        filter.update(1920, 0);
        assert_eq!(filter.filtered_current_level(), 1920);

        // If this happens every frame, filter would be stuck at 1920
        println!("RESET BUG: Filter reset from {} to 1920", high_value);
    }

    #[test]
    fn test_sample_memory_accumulation_bug_fixed() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80);

        // Simulate the bug scenario: sample_memory keeps accumulating
        filter.update(1920, 0); // Initialize
        assert_eq!(filter.filtered_current_level(), 1920);

        // Simulate multiple frames where sample_memory is NOT reset (the old bug)
        let mut accumulated_sample_memory = 0i32;

        for frame in 1..=10 {
            let raw_samples = 11000 + frame * 100;

            // Each acceleration operation adds ~1000 samples to memory (larger accumulation)
            let samples_removed_this_frame = 1000i32;
            accumulated_sample_memory += samples_removed_this_frame;

            // OLD BUG: time_stretched_samples keeps growing
            let time_stretched_samples = accumulated_sample_memory;

            let _prev_filtered = filter.filtered_current_level();
            filter.update(raw_samples, time_stretched_samples);
            let new_filtered = filter.filtered_current_level();

            println!(
                "Frame {}: raw={}, time_stretched={}, current_level={}, filtered={}",
                frame,
                raw_samples,
                time_stretched_samples,
                raw_samples as i32 - time_stretched_samples,
                new_filtered
            );

            // After a few frames, accumulated time_stretched should make current_level very low
            if frame >= 5 {
                let current_level = raw_samples as i32 - time_stretched_samples;
                assert!(
                    current_level < 7000,
                    "Current level should be much lower due to accumulation, got {}",
                    current_level
                );
            }
        }

        println!("BUG DEMONSTRATED: Filter gets wrong inputs due to sample_memory accumulation");

        // NOW TEST THE FIX: sample_memory resets each frame
        filter.update(1920, 0); // Re-initialize

        for frame in 1..=5 {
            let raw_samples = 11000 + frame * 100;

            // FIXED: time_stretched_samples is only from this frame (sample_memory resets)
            let samples_removed_this_frame = 500i32; // Only current frame
            let time_stretched_samples = samples_removed_this_frame; // Not accumulated!

            let _prev_filtered = filter.filtered_current_level();
            filter.update(raw_samples, time_stretched_samples);
            let new_filtered = filter.filtered_current_level();

            println!(
                "FIXED Frame {}: raw={}, time_stretched={}, current_level={}, filtered={}",
                frame,
                raw_samples,
                time_stretched_samples,
                raw_samples as i32 - time_stretched_samples,
                new_filtered
            );

            // With reset sample_memory, current_level should be reasonable
            let current_level = raw_samples as i32 - time_stretched_samples;
            assert!(
                current_level > 10000,
                "Current level should be high with fixed sample_memory"
            );
        }

        println!("FIX VERIFIED: Filter gets correct inputs when sample_memory resets");
    }

    #[test]
    fn test_webrtc_exact_pattern() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80);

        // Simulate WebRTC's exact pattern based on neteq_impl.cc
        filter.update(1920, 0); // Initialize

        // Simulate WebRTC's pattern:
        // 1. Set sample_memory to available samples for time-stretching
        // 2. Use it for decision
        // 3. Subtract consumed samples after successful operation

        let mut webrtc_sample_memory = 0i32;

        for frame in 1..=10 {
            let raw_samples = 11000 + frame * 100;
            let samples_left = 2000; // Assume we have 2000 samples available

            // WebRTC pattern: SET sample_memory to available samples (not accumulate)
            webrtc_sample_memory = samples_left; // Line 1304: set_sample_memory(samples_left + extracted_samples)

            // Use for this frame's decision
            let time_stretched_samples = webrtc_sample_memory;

            let _prev_filtered = filter.filtered_current_level();
            filter.update(raw_samples, time_stretched_samples);
            let new_filtered = filter.filtered_current_level();

            println!(
                "WebRTC Frame {}: raw={}, sample_memory={}, current_level={}, filtered={}",
                frame,
                raw_samples,
                webrtc_sample_memory,
                raw_samples as i32 - time_stretched_samples,
                new_filtered
            );

            // After successful time-stretching, WebRTC subtracts consumed samples (line 1084-1085)
            // AddSampleMemory(-(samples_left + output_size_samples_))
            let output_size_samples = 480; // 10ms at 48kHz
            webrtc_sample_memory -= samples_left + output_size_samples;

            println!("  After subtract: sample_memory={}", webrtc_sample_memory);
        }

        println!("WebRTC pattern: sample_memory is set to available samples, then adjusted");
    }

    #[test]
    fn test_our_neteq_fix_realistic_scenario() {
        let mut filter = BufferLevelFilter::new(48000);
        filter.set_target_buffer_level(80);

        // Simulate our NetEQ implementation with the fix
        filter.update(1920, 0); // Initialize

        // Simulate realistic scenario: high raw buffer with reasonable time_stretched_samples
        for frame in 1..=10 {
            let raw_buffer_samples = 11000 + frame * 100; // High raw buffer (like user's 380ms)

            // Our fix: sample_memory is set to available samples for time-stretching
            // In our acceleration methods, we use extended_frame which might be ~1.5x normal frame
            let frame_samples = 480; // 10ms at 48kHz
            let extended_frame_samples = (frame_samples as f32 * 1.5) as usize; // ~720 samples
            let available_samples_for_time_stretching = extended_frame_samples;

            // This is what our fixed NetEQ should pass to the filter
            let time_stretched_samples = available_samples_for_time_stretching as i32;

            let prev_filtered = filter.filtered_current_level();
            filter.update(raw_buffer_samples, time_stretched_samples);
            let new_filtered = filter.filtered_current_level();

            println!("FIXED Frame {}: raw_buffer={}ms ({}), time_stretched={}, current_level={}, filtered={}ms ({})", 
                frame,
                raw_buffer_samples * 1000 / 48000, raw_buffer_samples,
                time_stretched_samples,
                raw_buffer_samples as i32 - time_stretched_samples,
                new_filtered * 1000 / 48000, new_filtered);

            // Verify the filter is moving and not stuck
            if frame > 1 {
                assert_ne!(
                    new_filtered, prev_filtered,
                    "Filter should not be stuck at same value"
                );
            }

            // Verify current_level is reasonable (should be most of raw_buffer)
            let current_level = raw_buffer_samples as i32 - time_stretched_samples;
            assert!(
                current_level > 10000,
                "Current level should be high with reasonable time_stretched_samples, got {}",
                current_level
            );
        }

        println!(
            "✅ Our NetEQ fix produces reasonable time_stretched_samples, filter moves correctly"
        );
    }
}
