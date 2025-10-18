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

#[derive(Debug, Clone)]
pub struct Histogram {
    /// Buckets hold probabilities in Q30 fixed point (sum = 1 << 30).
    buckets: Vec<i32>,
    /// Current forget factor in Q15 (0..=32767). Matches C++ implementation.
    forget_factor: i32,
    /// Steady-state forget factor (base) in Q15.
    base_forget_factor: i32,
    /// Number of times Add() has been called since Reset or construction.
    add_count: u32,
    /// Optional start weight used in the original algorithm to ramp the forget factor.
    start_forget_weight: Option<f64>,
}

impl Histogram {
    /// Create a new Histogram with `num_buckets`, integer `forget_factor` (base) in Q15,
    /// and optional `start_forget_weight` (same semantic as original C++).
    pub fn new(
        num_buckets: usize,
        base_forget_factor: f64,
        start_forget_weight: Option<f64>,
    ) -> Self {
        debug_assert!(
            (0.0..=1.0).contains(&base_forget_factor),
            "base_forget_factor must be in [0.0, 1.0]"
        );

        // convert f64 to i32 Q15
        let base_forget_factor_q15 = f64_to_q(base_forget_factor, 15) as i32;

        debug_assert!(base_forget_factor_q15 < (1 << 15));
        debug_assert!(base_forget_factor_q15 >= 0);
        Histogram {
            buckets: vec![0i32; num_buckets],
            forget_factor: 0,
            base_forget_factor: base_forget_factor_q15,
            add_count: 0,
            start_forget_weight,
        }
    }

    /// Add an observation with value `value` (index into buckets).
    pub fn add(&mut self, value: usize) {
        debug_assert!(value < self.buckets.len());
        // Sum as we process. Use i64 temporaries for safe intermediate arithmetic.
        let mut vector_sum: i64 = 0;

        // Multiply each bucket by forget_factor (Q15).
        for b in self.buckets.iter_mut() {
            let tmp = ((*b as i64) * (self.forget_factor as i64)) >> 15;
            *b = tmp as i32;
            vector_sum += tmp;
        }

        // Add new sample: (32768 - forget_factor) << 15 (result in Q30)
        let add_amount: i64 = ((1 << 15) as i64 - self.forget_factor as i64) << 15;
        self.buckets[value] = (self.buckets[value] as i64 + add_amount) as i32;
        vector_sum += add_amount;

        // Desired sum is 1 << 30 (Q30).
        vector_sum -= 1i64 << 30;

        if vector_sum != 0 {
            let flip_sign: i64 = if vector_sum > 0 { -1 } else { 1 };
            // Modify a few values early in buckets to compensate for rounding error.
            for b in self.buckets.iter_mut() {
                // Add/subtract 1/16 of the element, but not more than |vector_sum|.
                let correction = flip_sign * vector_sum.abs().min((*b as i64) >> 4).max(0);
                *b = (*b as i64 + correction) as i32;
                vector_sum += correction;
                if vector_sum == 0 {
                    break;
                }
            }
        }
        debug_assert_eq!(vector_sum, 0);

        self.add_count = self.add_count.saturating_add(1);

        // Update forget_factor_ (ramp towards base_forget_factor_)
        if let Some(start_weight) = self.start_forget_weight {
            if self.forget_factor != self.base_forget_factor {
                let old_forget = self.forget_factor;

                // Compute: (1<<15) * (1 - start_weight / (add_count + 1))
                // Use f64 then clamp to [0, base_forget_factor]
                let forget_f = ((1u32 << 15) as f64
                    * (1.0 - start_weight / (self.add_count as f64 + 1.0)))
                    .round();
                self.forget_factor = 0.max(self.base_forget_factor.min(forget_f as i32));

                // The histogram is updated recursively by forgetting the old histogram
                // with |forget_factor_| and adding a new sample multiplied by |1 -
                // forget_factor_|. We need to make sure that the effective weight on the
                // new sample is no smaller than those on the old samples.
                debug_assert!(
                    (1 << 15) - self.forget_factor
                        >= (((1 << 15) - old_forget) * self.forget_factor) >> 15
                );
            }
        } else {
            // forget_factor_ += (base_forget_factor_ - forget_factor_ + 3) >> 2;
            let diff = (self.base_forget_factor - self.forget_factor + 3) >> 2;
            self.forget_factor = self.forget_factor + diff;
        }
    }

    /// Return the bucket index corresponding to the given quantile probability (0.0..=1.0).
    /// Finds the smallest index such that the reverse cumulative probability >= probability.
    pub fn quantile(&self, probability: f64) -> usize {
        debug_assert!(
            (0.0..=1.0).contains(&probability),
            "probability must be in [0.0, 1.0]"
        );

        let total_q30: i64 = 1 << 30;

        // Convert probability to Q30
        let probability_q30 = f64_to_q(probability, 30);

        let inverse_probability = total_q30 - probability_q30;
        let mut index = 0;
        let mut sum: i64 = total_q30;

        sum -= self.buckets[0] as i64;

        let n = self.buckets.len();
        while sum > inverse_probability && index < n - 1 {
            index += 1;
            sum -= self.buckets[index] as i64;
        }

        index
    }

    /// Reset to an exponentially decaying distribution:
    /// buckets[i] = 0.5^(i+1) in Q30, as in the original Reset().
    pub fn reset(&mut self) {
        // Set temp_prob to (slightly more than) 1 in Q14. This ensures that the sum is
        // as close to 1 as possible.
        let mut temp_prob: u32 = 0x4002;
        for b in self.buckets.iter_mut() {
            temp_prob >>= 1;
            // shift left 16 to get Q30 (Q14 << 16 = Q30)
            *b = (temp_prob << 16) as i32;
        }
        self.forget_factor = 0;
        self.add_count = 0;
    }

    /// Number of buckets.
    pub fn num_buckets(&self) -> usize {
        self.buckets.len()
    }
}

fn f64_to_q(v: f64, q: u8) -> i64 {
    let total_q: i64 = 1 << q;
    (v * (total_q as f64)).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reset_sum_is_one_q30() {
        let mut h = Histogram::new(8, 0.5, None);
        h.reset();
        let sum: i64 = h.buckets.iter().map(|&b| b as i64).sum();
        let expected = 1i64 << 30; // 1 in Q30

        // Allow small rounding difference (less then 1%).
        assert!(
            (expected - sum).abs() <= expected / 100,
            "sum = {}, expected ≈ {}, diff = {:.3}%",
            sum,
            expected,
            (expected - sum).abs() as f64 / expected as f64 * 100.0
        );
    }

    #[test]
    fn test_add_and_quantile_basic() {
        // small histogram
        let mut h = Histogram::new(4, 0.5, None);
        h.reset();

        // call add on bucket 0 several times and ensure quantile moves toward 0.
        for _ in 0..10 {
            h.add(0);
        }

        let q = h.quantile(0.5);
        // quantile should be fairly small (prefer low indices after many adds to 0).
        assert!(q <= 1);
    }

    #[test]
    fn test_num_buckets() {
        let h = Histogram::new(7, 0.1, None);
        assert_eq!(h.num_buckets(), 7);
    }
}
