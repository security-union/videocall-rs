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

const VIDEO_FRAME_RATE: u64 = 30;
const FRAME_PERIOD_MS: f64 = 1000.0 / VIDEO_FRAME_RATE as f64;
const MICROS_PER_MS: f64 = 1000.0;
const MIN_CAPTURE_FRAME_PERIOD_MS: f64 = 1000.0 / 240.0;
const MAX_CAPTURE_ARRIVAL_SKEW_MS: f64 = 1000.0;

#[derive(Debug)]
pub struct JitterEstimator {
    // The running average jitter calculation.
    jitter: f64,
    // The arrival time of the previously processed frame.
    last_arrival_time_ms: u128,
    // The sequence number of the previously processed frame.
    last_sequence_number: u64,
    last_capture_timestamp_us: f64,
    last_capture_arrival_ms: u128,
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
            last_capture_timestamp_us: 0.0,
            last_capture_arrival_ms: 0,
            is_first_frame: true,
        }
    }

    /// Updates the jitter estimate with a new frame's arrival information.
    pub fn update_estimate(
        &mut self,
        sequence_number: u64,
        capture_timestamp_us: f64,
        arrival_time_ms: u128,
    ) {
        if self.is_first_frame {
            self.last_arrival_time_ms = arrival_time_ms;
            self.last_sequence_number = sequence_number;
            self.last_capture_timestamp_us = capture_timestamp_us;
            self.last_capture_arrival_ms = arrival_time_ms;
            self.is_first_frame = false;
            return;
        }

        // Per RFC 3550, only update for in-order packets.
        if sequence_number > self.last_sequence_number {
            let repeated_capture = capture_timestamp_us == self.last_capture_timestamp_us;
            // D(i, j) = (R_j - R_i) - (S_j - S_i)
            if let Some(arrival_diff) = arrival_time_ms.checked_sub(self.last_arrival_time_ms) {
                let seq_diff = sequence_number - self.last_sequence_number;
                let transit_diff = self
                    .capture_transit_diff_ms(capture_timestamp_us, arrival_time_ms)
                    .unwrap_or(arrival_diff as f64 - seq_diff as f64 * FRAME_PERIOD_MS);

                // J(i) = J(i-1) + (|D(i-1, i)| - J(i-1))/16
                self.jitter += (transit_diff.abs() - self.jitter) / 16.0;
            }

            self.last_arrival_time_ms = arrival_time_ms;
            self.last_sequence_number = sequence_number;
            if !repeated_capture {
                self.last_capture_timestamp_us = capture_timestamp_us;
                self.last_capture_arrival_ms = arrival_time_ms;
            }
        }
    }

    fn capture_transit_diff_ms(
        &self,
        capture_timestamp_us: f64,
        arrival_time_ms: u128,
    ) -> Option<f64> {
        let arrival_diff_ms = arrival_time_ms.checked_sub(self.last_capture_arrival_ms)? as f64;
        let send_diff_ms = (capture_timestamp_us - self.last_capture_timestamp_us) / MICROS_PER_MS;
        let transit_diff_ms = arrival_diff_ms - send_diff_ms;
        (send_diff_ms >= MIN_CAPTURE_FRAME_PERIOD_MS
            && transit_diff_ms.abs() <= MAX_CAPTURE_ARRIVAL_SKEW_MS)
            .then_some(transit_diff_ms)
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
        estimator.update_estimate(1, 0.0, 1000);
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
        estimator.update_estimate(seq, 0.0, time_ms);

        // Subsequent frames with perfect timing
        for i in 2..=100 {
            seq = i;
            time_ms += FRAME_PERIOD_MS.round() as u128;
            estimator.update_estimate(seq, 0.0, time_ms);
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
        estimator.update_estimate(1, 0.0, time_ms);
        time_ms += FRAME_PERIOD_MS.round() as u128;
        estimator.update_estimate(2, 0.0, time_ms);
        assert!(estimator.get_jitter_estimate_ms() < 1.0);

        // A frame arrives 50ms late
        time_ms += FRAME_PERIOD_MS.round() as u128 + 50;
        estimator.update_estimate(3, 0.0, time_ms);

        // Expected jitter is influenced by the delta of 50ms.
        // J_1 = J_0 + (|D_1| - J_0) / 16 = 0 + (50 - 0) / 16 = 3.125
        assert!((estimator.get_jitter_estimate_ms() - 3.125).abs() < 0.1);

        // Another perfect frame arrives
        time_ms += FRAME_PERIOD_MS.round() as u128;
        estimator.update_estimate(4, 0.0, time_ms);

        // J_2 = J_1 + (|D_2| - J_1) / 16. D_2 should be near -50 because the *interval* was correct,
        // but the arrival was late compared to the timestamp. But we take abs(), so it's 50.
        // The logic is complex, but the jitter should go down as it smooths.
        // J_2 = 3.125 + (|-50| - 3.125) / 16 -> This isn't quite right.
        // The D for the *second* interval is actually close to 0.
        // Arrival diff = 33ms. Timestamp diff = 33.33ms. D is small.
        // J_2 = 3.125 + (|D_2| - 3.125) / 16 = 3.125 + (0 - 3.125) / 16 = 2.929
        assert!((estimator.get_jitter_estimate_ms() - 2.93).abs() < 0.1);
    }

    #[test]
    fn backward_arrival_step_rebaselines_without_a_jitter_sample() {
        let mut estimator = JitterEstimator::new();
        estimator.update_estimate(1, 0.0, 1000);
        estimator.update_estimate(2, 0.0, 990);
        assert!(
            estimator.get_jitter_estimate_ms() < FRAME_PERIOD_MS,
            "jitter {} after a 10ms backward clock step",
            estimator.get_jitter_estimate_ms()
        );

        estimator.update_estimate(3, 0.0, 990 + FRAME_PERIOD_MS.round() as u128);
        assert!(
            estimator.get_jitter_estimate_ms() < 1.0,
            "the backward step must not feed a transit sample, jitter {}",
            estimator.get_jitter_estimate_ms()
        );
    }

    fn feed_steady(
        estimator: &mut JitterEstimator,
        fps: f64,
        first_seq: u64,
        frames: u64,
        capture_base_us: f64,
        arrival_base_ms: f64,
    ) {
        for i in 0..frames {
            let t = i as f64 / fps;
            estimator.update_estimate(
                first_seq + i,
                capture_base_us + t * 1_000_000.0,
                (arrival_base_ms + t * 1000.0).round() as u128,
            );
        }
    }

    #[test]
    fn steady_capture_timestamps_produce_no_jitter_at_any_video_fps() {
        for fps in [15.0, 7.0, 10.0, 25.0, 100.0] {
            let mut estimator = JitterEstimator::new();
            feed_steady(&mut estimator, fps, 1, 200, 1_000_000.0, 1000.0);
            assert!(
                estimator.get_jitter_estimate_ms() < 1.0,
                "{fps} fps: jitter {}",
                estimator.get_jitter_estimate_ms()
            );
        }
    }

    #[test]
    fn late_frame_at_15_fps_raises_jitter_by_rfc3550_step() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 0.0, 1000.0);
        let before = estimator.get_jitter_estimate_ms();
        assert!(before < 1.0, "baseline jitter {before}");

        let capture_us = 100.0 * 1_000_000.0 / 15.0;
        let arrival_ms = (1000.0_f64 + 100.0 * 1000.0 / 15.0).round() as u128 + 50;
        estimator.update_estimate(101, capture_us, arrival_ms);

        let after = estimator.get_jitter_estimate_ms();
        assert!(
            (after - (before + (50.0 - before) / 16.0)).abs() < 0.1,
            "jitter {before} -> {after}"
        );
    }

    #[test]
    fn unusable_capture_timestamps_fall_back_to_sequence_spacing() {
        // 66ms arrivals: seq spacing yields |D| = 66 - 33.333 = 32.667 per sample.
        let after_first_sample = 32.667 / 16.0;
        for timestamps in [[0.0, 0.0], [5_000_000.0, 4_000_000.0]] {
            let mut estimator = JitterEstimator::new();
            estimator.update_estimate(1, timestamps[0], 1000);
            estimator.update_estimate(2, timestamps[1], 1066);
            assert!(
                (estimator.get_jitter_estimate_ms() - after_first_sample).abs() < 0.01,
                "{timestamps:?}: jitter {}",
                estimator.get_jitter_estimate_ms()
            );
        }
        let mut nan = JitterEstimator::new();
        nan.update_estimate(1, f64::NAN, 1000);
        nan.update_estimate(2, 66_000.0, 1066);
        assert!((nan.get_jitter_estimate_ms() - after_first_sample).abs() < 0.01);
    }

    #[test]
    fn millisecond_timestamps_fall_back_to_sequence_spacing() {
        let mut estimator = JitterEstimator::new();
        let mut arrival_ms = 1_700_000_000_000u128;
        for seq in 1..=100 {
            estimator.update_estimate(seq, arrival_ms as f64, arrival_ms);
            arrival_ms += FRAME_PERIOD_MS.round() as u128;
        }
        assert!(
            estimator.get_jitter_estimate_ms() < 1.0,
            "jitter {}",
            estimator.get_jitter_estimate_ms()
        );
    }

    #[test]
    fn capture_clock_restart_rebaselines_after_one_fallback_sample() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 50_000_000.0, 1000.0);
        let arrival_base = 1000.0 + 100.0 * 1000.0 / 15.0;
        feed_steady(&mut estimator, 15.0, 101, 100, 0.0, arrival_base);
        assert!(
            estimator.get_jitter_estimate_ms() < 1.0,
            "jitter {}",
            estimator.get_jitter_estimate_ms()
        );
    }

    #[test]
    fn capture_clock_jump_without_arrival_gap_takes_one_fallback_sample() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 0.0, 1000.0);
        let before = estimator.get_jitter_estimate_ms();
        let arrival_base = 1000.0 + 100.0 * 1000.0 / 15.0;
        feed_steady(&mut estimator, 15.0, 101, 1, 3_600_000_000.0, arrival_base);
        let after = estimator.get_jitter_estimate_ms();
        assert!(before < 1.0, "baseline jitter {before}");
        assert!(after < 3.0, "jitter {before} -> {after}");
    }

    #[test]
    fn capture_lead_beyond_bound_falls_back_to_sequence_spacing() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 0.0, 1000.0);
        let before = estimator.get_jitter_estimate_ms();
        let on_time_us = 100.0 * 1_000_000.0 / 15.0;
        let arrival_base = 1000.0 + 100.0 * 1000.0 / 15.0;
        feed_steady(
            &mut estimator,
            15.0,
            101,
            1,
            on_time_us + 1_500_000.0,
            arrival_base,
        );
        let after = estimator.get_jitter_estimate_ms();
        assert!(before < 1.0, "baseline jitter {before}");
        assert!(after - before < 3.0, "jitter {before} -> {after}");
    }

    #[test]
    fn screen_reencode_with_repeated_capture_timestamp_keeps_capture_reference() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 10.0, 1, 50, 0.0, 1000.0);
        let last_us = 49.0 * 100_000.0;
        let last_arrival = 1000 + 4900;
        estimator.update_estimate(51, last_us, last_arrival + 80);
        let after_reencode = estimator.get_jitter_estimate_ms();
        estimator.update_estimate(52, last_us + 100_000.0, last_arrival + 100);
        let after_next = estimator.get_jitter_estimate_ms();
        // The next frame is on time against the first carrier: |D| = 0.
        assert!(
            (after_next - after_reencode * 15.0 / 16.0).abs() < 0.01,
            "jitter {after_reencode} -> {after_next}"
        );
    }

    #[test]
    fn frozen_capture_timestamp_tracks_sequence_spacing() {
        let mut frozen = JitterEstimator::new();
        let mut zero = JitterEstimator::new();
        for i in 0..100u64 {
            let arrival_ms = (1000.0 + i as f64 * 1000.0 / 15.0).round() as u128;
            frozen.update_estimate(i + 1, 5_000_000.0, arrival_ms);
            zero.update_estimate(i + 1, 0.0, arrival_ms);
        }
        let jitter = frozen.get_jitter_estimate_ms();
        assert!(jitter > 30.0, "jitter {jitter}");
        assert_eq!(jitter, zero.get_jitter_estimate_ms());
    }

    #[test]
    fn frozen_capture_timestamp_resuming_after_long_gap_falls_back() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 0.0, 1000.0);
        let frozen_us = 99.0 * 1_000_000.0 / 15.0;
        let mut arrival_ms = (1000.0_f64 + 99.0 * 1000.0 / 15.0).round();
        for seq in 101..=130u64 {
            arrival_ms += 1000.0 / 15.0;
            estimator.update_estimate(seq, frozen_us, arrival_ms.round() as u128);
        }
        let before = estimator.get_jitter_estimate_ms();
        arrival_ms += 1000.0 / 15.0;
        estimator.update_estimate(131, frozen_us + 66_667.0, arrival_ms.round() as u128);
        let after = estimator.get_jitter_estimate_ms();
        assert!(after < before + 3.0, "jitter {before} -> {after}");
    }

    #[test]
    fn sender_pause_with_matching_capture_gap_adds_no_jitter() {
        let mut estimator = JitterEstimator::new();
        feed_steady(&mut estimator, 15.0, 1, 100, 0.0, 1000.0);
        let pause_ms = 5000.0;
        let resume = 100.0 * 1000.0 / 15.0 + pause_ms;
        feed_steady(
            &mut estimator,
            15.0,
            101,
            10,
            resume * 1000.0,
            1000.0 + resume,
        );
        assert!(
            estimator.get_jitter_estimate_ms() < 1.0,
            "jitter {}",
            estimator.get_jitter_estimate_ms()
        );
    }
}
