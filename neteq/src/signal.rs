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

/// Compute the best normalized cross-correlation of given length
/// between two equal-length slices.
pub fn best_normalized_correlation(a: &[f32], b: &[f32], len: usize) -> (usize, f32) {
    debug_assert_eq!(a.len(), b.len());
    debug_assert!(a.len() >= len);

    let mut sum_ab = 0.0;
    let mut sum_a2 = 0.0;
    let mut sum_b2 = 0.0;
    let mut best_pos = 0;
    let mut best_corr2 = 0.0;
    for i in 0..a.len() {
        sum_ab += a[i] * b[i];
        sum_a2 += a[i] * a[i];
        sum_b2 += b[i] * b[i];
        if i >= len {
            sum_ab -= a[i - len] * b[i - len];
            sum_a2 -= a[i - len] * b[i - len];
            sum_b2 -= a[i - len] * b[i - len];
            let corr2 = if sum_a2 == 0.0 || sum_b2 == 0.0 {
                0.0
            } else if sum_ab < 0.0 {
                -(sum_ab * sum_ab / (sum_a2 * sum_b2))
            } else {
                sum_ab * sum_ab / (sum_a2 * sum_b2)
            };

            if corr2 > best_corr2 {
                best_corr2 = corr2;
                best_pos = i;
            }
        }
    }

    (best_pos, best_corr2)
}

/// Cross-fade `fade_len` samples between the tail of `prev` and the head of `next`,
/// writing into `out`. `prev` and `next` must both have at least `fade_len` samples.
pub fn crossfade(prev: &[f32], next: &[f32], fade_len: usize, out: &mut [f32]) {
    let fade_start = prev.len() - fade_len;
    // copy first part (prev without last fade_len)
    out[..fade_start].copy_from_slice(&prev[..fade_start]);

    for i in 0..fade_len {
        let fade_out = 1.0 - (i as f32 / fade_len as f32);
        let fade_in = 1.0 - fade_out;
        let sample = prev[fade_start + i] * fade_out + next[i] * fade_in;
        out[fade_start + i] = sample;
    }
}
