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

//! Calculates network jitter based on frame arrival times.

const VIDEO_FRAME_RATE: u64 = 30; // Assuming a 30fps source
const FRAME_PERIOD_MS: f64 = 1000.0 / VIDEO_FRAME_RATE as f64;

#[derive(Debug)]
pub struct JitterEstimator {
    // The running average jitter calculation.
    jitter: f64,
    // The arrival time of the previously processed frame.
    last_arrival_time_ms: u128,
    // The sequence number of the previously processed frame.
    last_sequence_number: u64,
    // A flag to indicate if this is the first frame being processed.
    is_first_frame: bool,
}

impl Default for JitterEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterEstimator {
    pub fn new() -> Self {
        Self {
            jitter: 0.0,
            last_arrival_time_ms: 0,
            last_sequence_number: 0,
            is_first_frame: true,
        }
    }

    /// Updates the jitter estimate with a new frame's arrival information.
    pub fn update_estimate(&mut self, sequence_number: u64, arrival_time_ms: u128) {
        if self.is_first_frame {
            self.last_arrival_time_ms = arrival_time_ms;
            self.last_sequence_number = sequence_number;
            self.is_first_frame = false;
            return;
        }

        // Per RFC 3550, only update for in-order packets.
        if sequence_number > self.last_sequence_number {
            // D(i, j) = (R_j - R_i) - (S_j - S_i)
            // D(i, j) is the difference in transit times.

            // Difference in arrival times (R_j - R_i)
            let arrival_diff = arrival_time_ms - self.last_arrival_time_ms;

            // Difference in "sending" times, using sequence number as a proxy for the RTP timestamp.
            let seq_diff = sequence_number - self.last_sequence_number;
            let timestamp_diff = seq_diff as f64 * FRAME_PERIOD_MS;

            let transit_diff = arrival_diff as f64 - timestamp_diff;

            // J(i) = J(i-1) + (|D(i-1, i)| - J(i-1))/16
            self.jitter += (transit_diff.abs() - self.jitter) / 16.0;

            self.last_arrival_time_ms = arrival_time_ms;
            self.last_sequence_number = sequence_number;
        }
    }

    /// Returns the current jitter estimate in milliseconds.
    pub fn get_jitter_estimate_ms(&self) -> f64 {
        self.jitter
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_estimator_has_zero_jitter() {
        let estimator = JitterEstimator::new();
        assert_eq!(estimator.get_jitter_estimate_ms(), 0.0);
    }

    #[test]
    fn first_frame_sets_initial_state_but_does_not_change_jitter() {
        let mut estimator = JitterEstimator::new();
        estimator.update_estimate(1, 1000);
        assert_eq!(estimator.get_jitter_estimate_ms(), 0.0);
        assert_eq!(estimator.last_arrival_time_ms, 1000);
        assert_eq!(estimator.last_sequence_number, 1);
    }

    #[test]
    fn steady_arrival_produces_no_jitter() {
        let mut estimator = JitterEstimator::new();
        let mut time_ms = 1000;
        let mut seq = 1;

        // First frame
        estimator.update_estimate(seq, time_ms);

        // Subsequent frames with perfect timing
        for i in 2..=100 {
            seq = i;
            time_ms += FRAME_PERIOD_MS.round() as u128;
            estimator.update_estimate(seq, time_ms);
        }

        assert!(
            estimator.get_jitter_estimate_ms() < 1.0,
            "Jitter should be close to zero for steady arrival"
        );
    }

    #[test]
    fn late_arrival_increases_jitter() {
        let mut estimator = JitterEstimator::new();
        let mut time_ms: u128 = 1000;

        // Initial frames with perfect timing
        estimator.update_estimate(1, time_ms);
        time_ms += FRAME_PERIOD_MS.round() as u128;
        estimator.update_estimate(2, time_ms);
        assert!(estimator.get_jitter_estimate_ms() < 1.0);

        // A frame arrives 50ms late
        time_ms += FRAME_PERIOD_MS.round() as u128 + 50;
        estimator.update_estimate(3, time_ms);

        // Expected jitter is influenced by the delta of 50ms.
        // J_1 = J_0 + (|D_1| - J_0) / 16 = 0 + (50 - 0) / 16 = 3.125
        assert!((estimator.get_jitter_estimate_ms() - 3.125).abs() < 0.1);

        // Another perfect frame arrives
        time_ms += FRAME_PERIOD_MS.round() as u128;
        estimator.update_estimate(4, time_ms);

        // J_2 = J_1 + (|D_2| - J_1) / 16. D_2 should be near -50 because the *interval* was correct,
        // but the arrival was late compared to the timestamp. But we take abs(), so it's 50.
        // The logic is complex, but the jitter should go down as it smooths.
        // J_2 = 3.125 + (|-50| - 3.125) / 16 -> This isn't quite right.
        // The D for the *second* interval is actually close to 0.
        // Arrival diff = 33ms. Timestamp diff = 33.33ms. D is small.
        // J_2 = 3.125 + (|D_2| - 3.125) / 16 = 3.125 + (0 - 3.125) / 16 = 2.929
        assert!((estimator.get_jitter_estimate_ms() - 2.93).abs() < 0.1);
    }
}
