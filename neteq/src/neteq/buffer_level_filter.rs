// Port target: WebRTC modules/audio_coding/neteq/buffer_level_filter.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: buffer_level_filter_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Exponential filter of the buffer level (Q8), factors 251..254.

use crate::rtc_base::{dchecked_cast, saturated_cast};

/// C++ `BufferLevelFilter`.
#[derive(Debug, Clone)]
pub struct BufferLevelFilter {
    /// Filter factor for the buffer level filter in Q8.
    level_factor: i32,
    /// Filtered current buffer level in Q8.
    filtered_current_level: i32,
}

impl Default for BufferLevelFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferLevelFilter {
    /// A filter in its reset state.
    pub fn new() -> Self {
        let mut filter = Self {
            level_factor: 0,
            filtered_current_level: 0,
        };
        filter.reset();
        filter
    }

    /// Resets the filter.
    pub fn reset(&mut self) {
        self.filtered_current_level = 0;
        self.level_factor = 253;
    }

    /// Updates with the current `buffer_size_samples`, compensating for
    /// `time_stretched_samples` added (+) or removed (-) since last update.
    pub fn update(&mut self, buffer_size_samples: usize, time_stretched_samples: i32) {
        // Filter:
        // `filtered_current_level` = `level_factor` * `filtered_current_level` +
        //                            (1 - `level_factor`) * `buffer_size_samples`
        // `level_factor` and `filtered_current_level` are in Q8.
        // `buffer_size_samples` is in Q0.
        let filtered_current_level: i64 =
            ((self.level_factor as i64 * self.filtered_current_level as i64) >> 8)
                + (256 - self.level_factor) as i64 * dchecked_cast::<i64, _>(buffer_size_samples);

        // Account for time-scale operations (accelerate and pre-emptive
        // expand) and make sure that the filtered value remains non-negative.
        self.filtered_current_level = saturated_cast::<i32, _>(std::cmp::max(
            0,
            filtered_current_level - time_stretched_samples as i64 * (1 << 8),
        ));
    }

    /// Sets the filtered level directly (in samples). Only for large changes
    /// in buffer size, such as buffer flushes.
    pub fn set_filtered_buffer_level(&mut self, buffer_size_samples: i32) {
        self.filtered_current_level = saturated_cast::<i32, _>(buffer_size_samples as i64 * 256);
    }

    /// Picks the filter factor from the target level.
    pub fn set_target_buffer_level(&mut self, target_buffer_level_ms: i32) {
        self.level_factor = if target_buffer_level_ms <= 20 {
            251
        } else if target_buffer_level_ms <= 60 {
            252
        } else if target_buffer_level_ms <= 140 {
            253
        } else {
            254
        };
    }

    /// Filtered level in samples, rounded from Q8.
    pub fn filtered_current_level(&self) -> i32 {
        // Round to nearest whole sample.
        ((self.filtered_current_level as i64 + (1 << 7)) >> 8) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_destroy() {
        let filter = BufferLevelFilter::new();
        assert_eq!(0, filter.filtered_current_level());
    }

    #[test]
    fn convergence_test() {
        let mut filter = BufferLevelFilter::new();
        for times in (10..=50).step_by(10) {
            for value in (100..=200).step_by(10) {
                filter.reset();
                filter.set_target_buffer_level(20); // Makes filter coefficient 251/256.
                for _ in 0..times {
                    filter.update(value as usize, 0);
                }
                // Expect the filtered value to be (theoretically)
                // (1 - (251/256) ^ `times`) * `value`.
                let expected_value_double = (1.0 - (251.0f64 / 256.0).powi(times)) * value as f64;
                let expected_value = expected_value_double as i32;

                // The actual value may differ slightly from the expected value
                // due to intermediate-stage rounding errors in the filter
                // implementation. This is why we use a tolerance of +/-1.
                assert!(
                    (expected_value - filter.filtered_current_level()).abs() <= 1,
                    "times = {times}, value = {value}"
                );
            }
        }
    }

    // Verify that target buffer level impacts on the filter convergence.
    #[test]
    fn filter_factor() {
        let mut filter = BufferLevelFilter::new();
        // Update 10 times with value 100.
        const TIMES: i32 = 10;
        const VALUE: usize = 100;

        filter.set_target_buffer_level(60); // Makes filter coefficient 252/256.
        for _ in 0..TIMES {
            filter.update(VALUE, 0);
        }
        // Expect the filtered value to be
        // (1 - (252/256) ^ `TIMES`) * `VALUE`.
        assert_eq!(15, filter.filtered_current_level());

        filter.reset();
        filter.set_target_buffer_level(140); // Makes filter coefficient 253/256.
        for _ in 0..TIMES {
            filter.update(VALUE, 0);
        }
        assert_eq!(11, filter.filtered_current_level());

        filter.reset();
        filter.set_target_buffer_level(160); // Makes filter coefficient 254/256.
        for _ in 0..TIMES {
            filter.update(VALUE, 0);
        }
        assert_eq!(8, filter.filtered_current_level());
    }

    #[test]
    fn time_stretched_samples() {
        let mut filter = BufferLevelFilter::new();
        filter.set_target_buffer_level(20); // Makes filter coefficient 251/256.
                                            // Update 10 times with value 100.
        const TIMES: i32 = 10;
        const VALUE: usize = 100;
        const TIME_STRETCHED_SAMPLES: i32 = 3;
        for _ in 0..TIMES {
            filter.update(VALUE, 0);
        }
        // Expect the filtered value to be
        // (1 - (251/256) ^ `TIMES`) * `VALUE`.
        const EXPECTED_VALUE: i32 = 18;
        assert_eq!(EXPECTED_VALUE, filter.filtered_current_level());

        // Update filter again, now with non-zero value for packet length.
        // Set the current filtered value to be the input, in order to isolate
        // the impact of `TIME_STRETCHED_SAMPLES`.
        filter.update(
            filter.filtered_current_level() as usize,
            TIME_STRETCHED_SAMPLES,
        );
        assert_eq!(
            EXPECTED_VALUE - TIME_STRETCHED_SAMPLES,
            filter.filtered_current_level()
        );
        // Try negative value and verify that we come back to the previous
        // result.
        filter.update(
            filter.filtered_current_level() as usize,
            -TIME_STRETCHED_SAMPLES,
        );
        assert_eq!(EXPECTED_VALUE, filter.filtered_current_level());
    }

    // Not in C++: the filtered level never goes negative and saturates.
    #[test]
    fn clamps_at_zero_and_saturates() {
        let mut filter = BufferLevelFilter::new();
        filter.update(0, 1000);
        assert_eq!(0, filter.filtered_current_level());
        filter.set_filtered_buffer_level(i32::MAX);
        assert_eq!(i32::MAX, filter.filtered_current_level);
        assert_eq!(1 << 23, filter.filtered_current_level());
    }
}
