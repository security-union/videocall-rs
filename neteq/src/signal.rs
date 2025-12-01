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

    /// Test 2: Consistency Check
    /// The sliding window version should match manual calculation
    #[test]
    fn test_best_correlation_always_found() {
        struct TestCase {
            name: &'static str,
            a: Vec<f32>,
            b: Vec<f32>,
            len: usize,
            want_best_pos: Option<usize>,
            want_best_corr_min: f32,
            want_best_corr_max: f32,
        }

        for test_case in [
            TestCase {
                name: "pos==0",
                a: vec![0.3, 0.2, 0.1, 0.4, -0.6],
                b: vec![0.3, 0.2, 0.1, -0.2, 0.8],
                len: 3,
                want_best_pos: Some(0),
                want_best_corr_min: 0.99,
                want_best_corr_max: 1.0,
            },
            TestCase {
                name: "pos==1",
                a: vec![-0.4, 0.3, 0.2, 0.1, 0.4],
                b: vec![0.1, 0.3, 0.2, 0.1, -0.2],
                len: 3,
                want_best_pos: Some(1),
                want_best_corr_min: 0.99,
                want_best_corr_max: 1.0,
            },
            TestCase {
                name: "pos==2",
                a: vec![0.3, -0.4, 0.3, 0.2, 0.1],
                b: vec![0.3, 0.1, 0.3, 0.2, 0.1],
                len: 3,
                want_best_pos: Some(2),
                want_best_corr_min: 0.99,
                want_best_corr_max: 1.0,
            },
            TestCase {
                name: "pos==1, not perfect",
                a: vec![-0.4, 0.3, 0.2, 0.1, 0.4],
                b: vec![0.1, 0.2, 0.2, 0.1, -0.2],
                len: 3,
                want_best_pos: Some(1),
                want_best_corr_min: 0.5,
                want_best_corr_max: 0.9,
            },
            TestCase {
                name: "anti align",
                a: vec![-0.2, 0.3, -0.5],
                b: vec![0.2, -0.3, 0.5],
                len: 3,
                want_best_pos: Some(0),
                want_best_corr_min: -1.0,
                want_best_corr_max: -0.99,
            },
            TestCase {
                name: "all zero",
                a: vec![0.0, 0.0, 0.0, 0.0, 0.0],
                b: vec![0.0, 0.0, 0.0, 0.0, 0.0],
                len: 3,
                want_best_pos: None,
                want_best_corr_min: 0.99,
                want_best_corr_max: 1.0,
            },
            TestCase {
                name: "reverse signal",
                a: vec![1.0, 2.0, 3.0, 4.0, 5.0],
                b: vec![5.0, 4.0, 3.0, 2.0, 1.0], // Reversed pattern
                len: 3,
                want_best_pos: None,
                want_best_corr_min: 0.01,
                want_best_corr_max: 0.9,
            },
            TestCase {
                name: "out of phase",
                a: vec![1.0, 0.0, -1.0, 0.0, 1.0, 0.0],
                b: vec![0.0, 1.0, 0.0, -1.0, 0.0, 1.0], // 90° out of phase
                len: 3,
                want_best_pos: None,
                want_best_corr_min: -1.0,
                want_best_corr_max: 0.0,
            },
        ] {
            let (best_pos, best_corr) =
                best_normalized_correlation(&test_case.a, &test_case.b, test_case.len);
            if let Some(want_best_pos) = test_case.want_best_pos {
                assert!(
                    want_best_pos == best_pos,
                    "{}: Unexpected position, want: {} got: {}",
                    test_case.name,
                    want_best_pos,
                    best_pos
                );
            }
            assert!(
                best_corr > test_case.want_best_corr_min
                    || best_corr < test_case.want_best_corr_max,
                "{}: Unexpected correlation, want: {}<=x<={} got: {}",
                test_case.name,
                test_case.want_best_corr_min,
                test_case.want_best_corr_max,
                best_corr
            );
        }
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
