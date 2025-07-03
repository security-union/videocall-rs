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

/// Simple signal-processing helpers used by time-stretch routines.

/// Compute normalized cross-correlation of two equal-length  slices.
/// Returns value in [-1,1].
pub fn normalized_correlation(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut sum_ab = 0.0;
    let mut sum_a2 = 0.0;
    let mut sum_b2 = 0.0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        sum_ab += x * y;
        sum_a2 += x * x;
        sum_b2 += y * y;
    }
    if sum_a2 == 0.0 || sum_b2 == 0.0 {
        0.0
    } else {
        sum_ab / (sum_a2.sqrt() * sum_b2.sqrt())
    }
}

/// Cross-fade `fade_len` samples between the tail of `prev` and the head of `next`,
/// writing into `out`. `prev` and `next` must both have at least `fade_len` samples.
pub fn crossfade(prev: &[f32], next: &[f32], fade_len: usize, out: &mut Vec<f32>) {
    let total_prev = prev.len();
    // copy first part (prev without last fade_len)
    out.extend_from_slice(&prev[..total_prev - fade_len]);

    for i in 0..fade_len {
        let fade_out = 1.0 - (i as f32 / fade_len as f32);
        let fade_in = 1.0 - fade_out;
        let sample = prev[total_prev - fade_len + i] * fade_out + next[i] * fade_in;
        out.push(sample);
    }
}
