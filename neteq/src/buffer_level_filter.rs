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
/// The value 0.9 provides good balance between:
/// - Filtering out short-term fluctuations due to network jitter
/// - Responding to actual buffer level trends quickly enough for effective control
///
/// Engineering rationale:
/// - Time constant τ = -frame_duration / ln(smoothing_factor)
/// - With 0.9 and 10ms frames: τ ≈ 95ms response time
/// - This allows ~10 frames to reach 63% of a step change
const BUFFER_LEVEL_SMOOTHING_FACTOR: f64 = 0.9;

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
            self.filtered_level_samples = current_level;
            self.initialized = true;
        } else {
            // Exponential smoothing: filtered = α × current + (1-α) × previous
            self.filtered_level_samples = self.smoothing_factor * self.filtered_level_samples
                + (1.0 - self.smoothing_factor) * current_level;
        }
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
}
