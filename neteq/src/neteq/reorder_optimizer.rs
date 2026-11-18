// Port target: WebRTC modules/audio_coding/neteq/reorder_optimizer.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: reorder_optimizer_unittest.cc).
// Copyright (c) 2022 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Trades delay against reordered-packet loss with a cost function.

use crate::neteq::histogram::Histogram;

const DELAY_BUCKETS: usize = 100;
const BUCKET_SIZE_MS: i32 = 20;

/// C++ `ReorderOptimizer`: an optimal delay that reduces the chance of
/// missing reordered packets. `ms_per_loss_percent` tunes the delay/loss
/// trade-off.
#[derive(Debug)]
pub struct ReorderOptimizer {
    histogram: Histogram,
    ms_per_loss_percent: i32,
    optimal_delay_ms: Option<i32>,
}

impl ReorderOptimizer {
    /// `forget_factor` in Q15.
    pub fn new(
        forget_factor: i32,
        ms_per_loss_percent: i32,
        start_forget_weight: Option<i32>,
    ) -> Self {
        Self {
            histogram: Histogram::new(
                DELAY_BUCKETS,
                forget_factor,
                start_forget_weight.map(f64::from),
            ),
            ms_per_loss_percent,
            optimal_delay_ms: None,
        }
    }

    /// Feeds one packet's relative delay and whether it was reordered.
    pub fn update(&mut self, relative_delay_ms: i32, reordered: bool, base_delay_ms: i32) {
        let index = if reordered {
            relative_delay_ms / BUCKET_SIZE_MS
        } else {
            0
        };
        if index < self.histogram.num_buckets() {
            // Maximum delay to register is 2000 ms.
            self.histogram.add(index);
        }
        let bucket_index = self.minimize_cost_function(base_delay_ms);
        self.optimal_delay_ms = Some((1 + bucket_index) * BUCKET_SIZE_MS);
    }

    /// The current optimal delay, if any.
    pub fn get_optimal_delay_ms(&self) -> Option<i32> {
        self.optimal_delay_ms
    }

    /// Resets all state.
    pub fn reset(&mut self) {
        self.histogram.reset();
        self.optimal_delay_ms = None;
    }

    fn minimize_cost_function(&self, base_delay_ms: i32) -> i32 {
        let buckets = self.histogram.buckets();

        // Values are calculated in Q30.
        let mut loss_probability: i64 = 1 << 30;
        let mut min_cost: i64 = i64::MAX;
        let mut min_bucket: i32 = 0;
        for (i, &bucket) in buckets.iter().enumerate() {
            let i = i as i32;
            loss_probability -= bucket as i64;
            let delay_ms: i64 = (std::cmp::max(0, i * BUCKET_SIZE_MS - base_delay_ms) as i64) << 30;
            // C++: `100 * ms_per_loss_percent_` is int, promoted to int64 by
            // the multiplication with `loss_probability`.
            let cost = delay_ms + (100 * self.ms_per_loss_percent) as i64 * loss_probability;

            if cost < min_cost {
                min_cost = cost;
                min_bucket = i;
            }
            if loss_probability == 0 {
                break;
            }
        }

        min_bucket
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORGET_FACTOR: i32 = 32745; // 0.9993 in Q15.
    const MS_PER_LOSS_PERCENT: i32 = 20;
    const START_FORGET_WEIGHT: i32 = 1;

    // C++ ReorderOptimizerTest.OnlyIncreaseDelayForReorderedPackets
    #[test]
    fn only_increase_delay_for_reordered_packets() {
        let mut reorder_optimizer = ReorderOptimizer::new(
            FORGET_FACTOR,
            MS_PER_LOSS_PERCENT,
            Some(START_FORGET_WEIGHT),
        );
        assert!(reorder_optimizer.get_optimal_delay_ms().is_none());

        // Delay should not increase for in-order packets.
        reorder_optimizer.update(60, false, 0);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(20));

        reorder_optimizer.update(100, false, 0);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(20));

        reorder_optimizer.update(80, true, 0);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(100));
    }

    // C++ ReorderOptimizerTest.AvoidIncreasingDelayWhenProbabilityIsLow
    #[test]
    fn avoid_increasing_delay_when_probability_is_low() {
        let mut reorder_optimizer = ReorderOptimizer::new(
            FORGET_FACTOR,
            MS_PER_LOSS_PERCENT,
            Some(START_FORGET_WEIGHT),
        );

        reorder_optimizer.update(40, true, 0);
        reorder_optimizer.update(40, true, 0);
        reorder_optimizer.update(40, true, 0);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(60));

        // The cost of the delay is too high relative the probability.
        reorder_optimizer.update(600, true, 0);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(60));
    }

    // C++ ReorderOptimizerTest.BaseDelayIsSubtractedFromCost
    #[test]
    fn base_delay_is_subtracted_from_cost() {
        const BASE_DELAY_MS: i32 = 200;
        let mut reorder_optimizer = ReorderOptimizer::new(
            FORGET_FACTOR,
            MS_PER_LOSS_PERCENT,
            Some(START_FORGET_WEIGHT),
        );

        reorder_optimizer.update(40, true, BASE_DELAY_MS);
        reorder_optimizer.update(40, true, BASE_DELAY_MS);
        reorder_optimizer.update(40, true, BASE_DELAY_MS);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(60));

        // The cost of the delay is too high relative the probability.
        reorder_optimizer.update(600, true, BASE_DELAY_MS);
        assert_eq!(reorder_optimizer.get_optimal_delay_ms(), Some(620));
    }
}
