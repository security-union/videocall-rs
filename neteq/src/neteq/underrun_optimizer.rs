// Port target: WebRTC modules/audio_coding/neteq/underrun_optimizer.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: underrun_optimizer_unittest.cc).
// Copyright (c) 2022 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Picks the delay that keeps underruns below the configured quantile.

use crate::api::tick_timer::TickTimer;

use crate::api::tick_timer::Stopwatch;
use crate::neteq::histogram::Histogram;

const DELAY_BUCKETS: usize = 100;
const BUCKET_SIZE_MS: i32 = 20;

/// C++ `UnderrunOptimizer`: estimates the probability of buffer underrun due
/// to late packet arrival. The optimal delay keeps the probability of
/// underrun below `1 - histogram_quantile`.
#[derive(Debug)]
pub struct UnderrunOptimizer {
    tick_timer: TickTimer,
    histogram: Histogram,
    /// In Q30.
    histogram_quantile: i32,
    resample_interval_ms: Option<i32>,
    resample_stopwatch: Option<Stopwatch>,
    max_delay_in_interval_ms: i32,
    optimal_delay_ms: Option<i32>,
}

impl UnderrunOptimizer {
    /// `histogram_quantile` is Q30, `forget_factor` Q15.
    pub fn new(
        tick_timer: TickTimer,
        histogram_quantile: i32,
        forget_factor: i32,
        start_forget_weight: Option<i32>,
        resample_interval_ms: Option<i32>,
    ) -> Self {
        Self {
            tick_timer,
            histogram: Histogram::new(
                DELAY_BUCKETS,
                forget_factor,
                start_forget_weight.map(f64::from),
            ),
            histogram_quantile,
            resample_interval_ms,
            resample_stopwatch: None,
            max_delay_in_interval_ms: 0,
            optimal_delay_ms: None,
        }
    }

    /// Feeds one relative packet delay.
    pub fn update(&mut self, relative_delay_ms: i32) {
        let mut histogram_update: Option<i32> = None;
        if let Some(resample_interval_ms) = self.resample_interval_ms {
            let stopwatch = self
                .resample_stopwatch
                .get_or_insert_with(|| self.tick_timer.get_new_stopwatch());
            // C++ `static_cast<int>(ElapsedMs())`: truncating u64 -> int.
            if stopwatch.elapsed_ms() as i32 > resample_interval_ms {
                histogram_update = Some(self.max_delay_in_interval_ms);
                self.resample_stopwatch = Some(self.tick_timer.get_new_stopwatch());
                self.max_delay_in_interval_ms = 0;
            }
            self.max_delay_in_interval_ms =
                std::cmp::max(self.max_delay_in_interval_ms, relative_delay_ms);
        } else {
            histogram_update = Some(relative_delay_ms);
        }
        let Some(histogram_update) = histogram_update else {
            return;
        };

        let index = histogram_update / BUCKET_SIZE_MS;
        if index < self.histogram.num_buckets() {
            // Maximum delay to register is 2000 ms.
            self.histogram.add(index);
        }
        let bucket_index = self.histogram.quantile(self.histogram_quantile);
        self.optimal_delay_ms = Some((1 + bucket_index) * BUCKET_SIZE_MS);
    }

    /// The current optimal delay, if any.
    pub fn get_optimal_delay_ms(&self) -> Option<i32> {
        self.optimal_delay_ms
    }

    /// Resets all state.
    pub fn reset(&mut self) {
        self.histogram.reset();
        self.resample_stopwatch = None;
        self.max_delay_in_interval_ms = 0;
        self.optimal_delay_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_HISTOGRAM_QUANTILE: i32 = 1020054733; // 0.95 in Q30.
    const FORGET_FACTOR: i32 = 32745; // 0.9993 in Q15.

    // C++ UnderrunOptimizerTest.ResamplePacketDelays
    #[test]
    fn resample_packet_delays() {
        let tick_timer = TickTimer::new();
        const RESAMPLE_INTERVAL_MS: i32 = 500;
        let mut underrun_optimizer = UnderrunOptimizer::new(
            tick_timer.clone(),
            DEFAULT_HISTOGRAM_QUANTILE,
            FORGET_FACTOR,
            None,
            Some(RESAMPLE_INTERVAL_MS),
        );

        // The histogram should be updated once with the maximum delay
        // observed for the following sequence of updates.
        for i in (0..500).step_by(20) {
            underrun_optimizer.update(i);
            assert!(underrun_optimizer.get_optimal_delay_ms().is_none());
        }
        tick_timer.increment_by((RESAMPLE_INTERVAL_MS / tick_timer.ms_per_tick() + 1) as u64);
        underrun_optimizer.update(0);
        assert_eq!(underrun_optimizer.get_optimal_delay_ms(), Some(500));
    }

    // Not in C++: without a resample interval every delay goes straight into
    // the histogram, and delays of 2000 ms or more are not registered.
    #[test]
    fn no_resampling_updates_every_packet() {
        let tick_timer = TickTimer::new();
        let mut underrun_optimizer = UnderrunOptimizer::new(
            tick_timer,
            DEFAULT_HISTOGRAM_QUANTILE,
            FORGET_FACTOR,
            None,
            None,
        );
        underrun_optimizer.update(45);
        assert_eq!(underrun_optimizer.get_optimal_delay_ms(), Some(60));
        underrun_optimizer.reset();
        assert!(underrun_optimizer.get_optimal_delay_ms().is_none());
    }
}
