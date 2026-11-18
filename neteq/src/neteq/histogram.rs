// Port target: WebRTC modules/audio_coding/neteq/histogram.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: histogram_unittest.cc).
// Copyright (c) 2019 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Forgetting histogram of relative delays (Q30 probabilities).

/// C++ `Histogram`.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: Vec<i32>,
    /// Q15.
    forget_factor: i32,
    base_forget_factor: i32,
    add_count: i32,
    start_forget_weight: Option<f64>,
}

impl Histogram {
    /// `num_buckets` buckets, `forget_factor` in Q15, optional start
    /// forget weight. Like C++, the buckets start at zero; call `reset` for
    /// the exponentially decaying start distribution.
    pub fn new(num_buckets: usize, forget_factor: i32, start_forget_weight: Option<f64>) -> Self {
        debug_assert!(forget_factor < 1 << 15);
        Self {
            buckets: vec![0; num_buckets],
            forget_factor: 0,
            base_forget_factor: forget_factor,
            add_count: 0,
            start_forget_weight,
        }
    }

    /// Resets to the initial (exponentially decaying) distribution
    /// `buckets[i] = 0.5^(i+1)` in Q30.
    pub fn reset(&mut self) {
        // Set temp_prob to (slightly more than) 1 in Q14. This ensures that
        // the sum of buckets is 1.
        let mut temp_prob: u16 = 0x4002; // 16384 + 2 = 100000000000010 binary.
        for bucket in self.buckets.iter_mut() {
            temp_prob >>= 1;
            *bucket = (temp_prob as i32) << 16;
        }
        self.forget_factor = 0; // Adapt the histogram faster for the first few packets.
        self.add_count = 0;
    }

    /// Adds an observation in bucket `index` and forgets old ones.
    ///
    /// Each bucket is first multiplied by the forgetting factor, then bucket
    /// `index` is increased by `1 - forget_factor`, keeping the sum at 1
    /// (Q30). Fixed-point rounding errors are corrected on the first
    /// buckets.
    pub fn add(&mut self, index: i32) {
        debug_assert!(index >= 0);
        debug_assert!(index < self.buckets.len() as i32);
        let mut vector_sum: i32 = 0; // Sum up the vector elements as they are processed.
                                     // Multiply each element in `buckets` with `forget_factor`.
        for bucket in self.buckets.iter_mut() {
            *bucket = ((*bucket as i64 * self.forget_factor as i64) >> 15) as i32;
            vector_sum = vector_sum.wrapping_add(*bucket);
        }

        // Increase the probability for the currently observed inter-arrival
        // time by 1 - `forget_factor`. The factor is in Q15, `buckets` in Q30.
        // Thus, left-shift 15 steps to obtain result in Q30.
        let increment = (32768 - self.forget_factor) << 15;
        self.buckets[index as usize] = self.buckets[index as usize].wrapping_add(increment);
        vector_sum = vector_sum.wrapping_add(increment); // Add to vector sum.

        // `buckets` should sum up to 1 (in Q30), but it may not due to
        // fixed-point rounding errors.
        vector_sum = vector_sum.wrapping_sub(1 << 30); // Should be zero. Compensate if not.
        if vector_sum != 0 {
            // Modify a few values early in `buckets`.
            let flip_sign = if vector_sum > 0 { -1 } else { 1 };
            for bucket in self.buckets.iter_mut() {
                // Add/subtract 1/16 of the element, but not more than `vector_sum`.
                let correction = flip_sign * std::cmp::min(vector_sum.abs(), *bucket >> 4);
                *bucket += correction;
                vector_sum += correction;
                if vector_sum.abs() == 0 {
                    break;
                }
            }
        }
        debug_assert!(vector_sum == 0); // Verify that the above is correct.

        self.add_count += 1;

        // Update `forget_factor` (changes only during the first seconds after
        // a reset). The factor converges to `base_forget_factor`.
        if let Some(start_forget_weight) = self.start_forget_weight {
            if self.forget_factor != self.base_forget_factor {
                let old_forget_factor = self.forget_factor;
                // C++: `int forget_factor = (1 << 15) * (1 - w / (add_count_ + 1));`
                let forget_factor = ((1 << 15) as f64
                    * (1.0 - start_forget_weight / (self.add_count + 1) as f64))
                    as i32;
                self.forget_factor =
                    std::cmp::max(0, std::cmp::min(self.base_forget_factor, forget_factor));
                // The effective weight on the new sample must be no smaller
                // than those on the old samples.
                debug_assert!(
                    (1 << 15) - self.forget_factor
                        >= (((1 << 15) - old_forget_factor) * self.forget_factor) >> 15
                );
            }
        } else {
            self.forget_factor += (self.base_forget_factor - self.forget_factor + 3) >> 2;
        }
    }

    /// Smallest bucket index whose cumulative probability reaches
    /// `probability` (Q30).
    pub fn quantile(&self, probability: i32) -> i32 {
        // Find the bucket for which the probability of observing an
        // inter-arrival time larger than or equal to `index` is larger than
        // or equal to `probability`. Start with `sum` = 1 and subtract
        // elements from the start of the histogram.
        let inverse_probability = (1 << 30) - probability;
        let mut index: usize = 0; // Start from the beginning of `buckets`.
        let mut sum: i32 = 1 << 30; // Assign to 1 in Q30.
        sum = sum.wrapping_sub(self.buckets[index]);

        while sum > inverse_probability && index < self.buckets.len() - 1 {
            // Subtract the probabilities one by one until the sum is no
            // longer greater than `inverse_probability`.
            index += 1;
            sum = sum.wrapping_sub(self.buckets[index]);
        }
        index as i32
    }

    /// Number of buckets.
    pub fn num_buckets(&self) -> i32 {
        self.buckets.len() as i32
    }

    /// Bucket probabilities (Q30).
    pub fn buckets(&self) -> &[i32] {
        &self.buckets
    }

    pub fn base_forget_factor_for_testing(&self) -> i32 {
        self.base_forget_factor
    }

    pub fn forget_factor_for_testing(&self) -> i32 {
        self.forget_factor
    }

    pub fn start_forget_weight_for_testing(&self) -> Option<f64> {
        self.start_forget_weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C++ HistogramTest.Initialization
    #[test]
    fn initialization() {
        let mut histogram = Histogram::new(65, 32440, None);
        histogram.reset();
        let buckets = histogram.buckets();
        let mut sum = 0.0f64;
        for (i, &bucket) in buckets.iter().enumerate() {
            let expected = 0.5f64.powi((i + 1) as i32) * (1u64 << 30) as f64;
            // Tolerance 65537 in Q30 corresponds to a delta of approximately
            // 0.00006.
            assert!(
                (expected - bucket as f64).abs() <= 65537.0,
                "bucket {i}: {bucket} vs {expected}"
            );
            sum += bucket as f64;
        }
        assert_eq!(1 << 30, sum as i32); // Should be 1 in Q30.
    }

    // C++ HistogramTest.Add
    #[test]
    fn add() {
        let mut histogram = Histogram::new(10, 32440, None);
        histogram.reset();
        let before = histogram.buckets().to_vec();
        let index = 5;
        histogram.add(index);
        let after = histogram.buckets().to_vec();
        assert!(after[index as usize] > before[index as usize]);
        let sum: i32 = after.iter().sum();
        assert_eq!(1 << 30, sum);
    }

    // C++ HistogramTest.ForgetFactor
    #[test]
    fn forget_factor() {
        let mut histogram = Histogram::new(10, 32440, None);
        histogram.reset();
        let before = histogram.buckets().to_vec();
        let index = 4;
        histogram.add(index);
        let after = histogram.buckets().to_vec();
        for i in 0..histogram.num_buckets() {
            if i != index {
                assert!(after[i as usize] < before[i as usize]);
            }
        }
    }

    // C++ HistogramTest.ReachSteadyStateForgetFactor
    #[test]
    fn reach_steady_state_forget_factor() {
        const STEADY_STATE_FORGET_FACTOR: i32 = ((1 << 15) as f64 * 0.9993) as i32;
        let mut histogram = Histogram::new(100, STEADY_STATE_FORGET_FACTOR, Some(1.0));
        histogram.reset();
        let n = (1 << 15) / ((1 << 15) - STEADY_STATE_FORGET_FACTOR);
        for _ in 0..n {
            histogram.add(0);
        }
        assert_eq!(
            histogram.forget_factor_for_testing(),
            STEADY_STATE_FORGET_FACTOR
        );
    }

    // Not in C++: the constructor leaves the buckets at zero (no `Reset`),
    // and the first `add` puts all the mass in one bucket.
    #[test]
    fn new_histogram_starts_empty() {
        let mut histogram = Histogram::new(4, 32440, None);
        assert_eq!(histogram.buckets(), &[0, 0, 0, 0]);
        assert_eq!(histogram.forget_factor_for_testing(), 0);
        histogram.add(2);
        assert_eq!(histogram.buckets(), &[0, 0, 1 << 30, 0]);
        assert_eq!(histogram.quantile(1020054733), 2);
        // forget_factor += (32440 - 0 + 3) >> 2.
        assert_eq!(histogram.forget_factor_for_testing(), 8110);
    }
}
