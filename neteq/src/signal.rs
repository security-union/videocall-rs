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

    let mut corr = 0.0;
    let mut energy = 0.0;
    let mut best_pos = 0;
    let mut best_corr = -1.0;
    for i in 0..a.len() {
        let x = a[i];
        let y = b[i];
        corr += x * y;
        energy += (x * x).max(y * y);
        if i + 1 >= len {
            let normalized_corr = if energy == 0.0 { 1.0 } else { corr / energy };

            let i_start = i + 1 - len;

            if normalized_corr >= 1.0 {
                return (i_start, 1.0);
            }

            if normalized_corr > best_corr {
                best_corr = normalized_corr;
                best_pos = i_start;
            }

            let old_x = a[i_start];
            let old_y = b[i_start];
            corr -= old_x * old_y;
            energy -= (old_x * old_x).max(old_y * old_y);
        }
    }

    (best_pos, best_corr)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 1: Known Values Test
    /// Catches the copy-paste bug with simple, verifiable cases
    #[test]
    fn test_normalized_correlation_known_values() {
        // Test 1: Identical signals should have correlation of 1.0
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let b = vec![1.0, 2.0, 3.0, 4.0];
        let corr = normalized_correlation(&a, &b);
        assert!(
            (corr - 1.0).abs() < 0.001,
            "Identical signals should correlate at 1.0, got {corr}"
        );

        // Test 2: Opposite signals should have correlation of -1.0
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let b = vec![-1.0, -2.0, -3.0, -4.0];
        let corr = normalized_correlation(&a, &b);
        assert!(
            (corr + 1.0).abs() < 0.001,
            "Opposite signals should correlate at -1.0, got {corr}"
        );

        // Test 3: Orthogonal signals should have correlation near 0
        let a = vec![1.0, 0.0, -1.0, 0.0];
        let b = vec![0.0, 1.0, 0.0, -1.0];
        let corr = normalized_correlation(&a, &b);
        assert!(
            corr.abs() < 0.001,
            "Orthogonal signals should have ~0 correlation, got {corr}"
        );
    }

    /// Test 2: Consistency Check
    /// The sliding window version should match manual calculation
    #[test]
    fn test_best_correlation_consistency() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let b = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0]; // b is a shifted
        let len = 3;

        let (best_pos, best_corr2) = best_normalized_correlation(&a, &b, len);

        // Manually verify the best position
        // At position where they align best, compute correlation manually
        let window_a = &a[best_pos - len + 1..=best_pos];
        let window_b = &b[best_pos - len + 1..=best_pos];
        let manual_corr = normalized_correlation(window_a, window_b);
        let manual_corr2 = manual_corr * manual_corr;

        assert!(
            (best_corr2 - manual_corr2).abs() < 0.001,
            "Sliding window correlation should match manual calculation: {best_corr2} vs {manual_corr2}"
        );
    }

    /// Test 3: Mathematical Property Test
    /// Correlation coefficient must be in [-1, 1]
    #[test]
    fn test_correlation_bounds() {
        // Random-ish signals
        let a = vec![1.5, -2.3, 4.1, -0.5, 3.2, -1.8, 2.7];
        let b = vec![0.8, 3.4, -1.2, 2.9, -0.7, 1.6, -2.1];

        let corr = normalized_correlation(&a, &b);
        assert!(
            corr >= -1.0 && corr <= 1.0,
            "Correlation must be in [-1, 1], got {corr}"
        );

        let len = 3;
        let (_pos, corr2) = best_normalized_correlation(&a, &b, len);
        assert!(
            corr2 >= 0.0 && corr2 <= 1.0,
            "Squared correlation must be in [0, 1], got {corr2}"
        );
    }

    /// Test 4: Energy Conservation Test - THE CRITICAL TEST
    /// This catches the copy-paste bug where sum_a2 and sum_b2 both use a[i]*b[i]
    #[test]
    fn test_sliding_window_energy_correctness() {
        // Use signals where a and b are clearly different
        // This ensures sum_a2 ≠ sum_ab ≠ sum_b2
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b = vec![5.0, 4.0, 3.0, 2.0, 1.0]; // Reversed pattern
        let len = 3;

        let (_pos, corr2) = best_normalized_correlation(&a, &b, len);

        // With the bug, when sum_a2 and sum_b2 both equal sum_ab:
        // corr2 = sum_ab^2 / (sum_ab * sum_ab) = 1.0 (always!)

        // Without the bug, for these different signals:
        // corr2 should be less than 1.0 (they're not perfectly correlated)

        // Let's verify with a specific window to be concrete:
        // For indices [1,2,3]: a=[2,3,4], b=[4,3,2]
        // sum_ab = 2*4 + 3*3 + 4*2 = 8 + 9 + 8 = 25
        // sum_a2 = 4 + 9 + 16 = 29
        // sum_b2 = 16 + 9 + 4 = 29
        // corr2 = 25^2 / (29 * 29) = 625/841 ≈ 0.743

        // With bug: corr2 = 25^2 / (25 * 25) = 1.0

        println!("Best correlation squared: {corr2}");
        assert!(
            corr2 < 0.99,
            "BUG DETECTED! Reversed signals showing near-perfect correlation. \
             This indicates sum_a2 and sum_b2 are computed incorrectly. Got corr2={corr2}"
        );
    }

    /// Test 5: Direct comparison test - most explicit bug detector
    #[test]
    fn test_different_signals_not_perfectly_correlated() {
        // Two completely different signals should NOT have correlation = 1.0
        let a = vec![1.0, 0.0, -1.0, 0.0, 1.0, 0.0];
        let b = vec![0.0, 1.0, 0.0, -1.0, 0.0, 1.0]; // 90° out of phase
        let len = 3;

        let (_pos, corr2) = best_normalized_correlation(&a, &b, len);

        // These orthogonal signals should have very low correlation
        // With the bug, corr2 would incorrectly be 1.0
        assert!(
            corr2 < 0.5,
            "BUG DETECTED! Orthogonal signals showing high correlation. \
             This proves sum_a2 and sum_b2 are wrong. Got corr2={corr2}"
        );
    }

    #[test]
    fn test_crossfade_basic() {
        let prev = vec![1.0, 1.0, 1.0, 1.0];
        let next = vec![0.0, 0.0, 0.0, 0.0];
        let mut out = vec![0.0; 4];

        crossfade(&prev, &next, 2, &mut out);

        // First part should be unchanged (prev without fade)
        assert_eq!(out[0], 1.0);
        assert_eq!(out[1], 1.0);

        // Faded region should transition from prev to next
        // out[2] should be mostly prev, out[3] should be mostly next
        assert!(
            out[2] >= 0.5 && out[2] <= 1.0,
            "out[2]={} should be in fade region",
            out[2]
        );
        assert!(
            out[3] >= 0.0 && out[3] <= 0.5,
            "out[3]={} should be in fade region",
            out[3]
        );
    }
}
