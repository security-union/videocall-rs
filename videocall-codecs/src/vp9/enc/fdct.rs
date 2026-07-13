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

//! Forward DCT transforms (4x4 and 8x8).
//!
//! Ports `vpx_fdct4x4_c` / `vpx_fdct8x8_c` from libvpx `vpx_dsp/fwd_txfm.c`.
//! Although the forward transform is an encoder-free choice, porting it
//! faithfully keeps reconstruction error predictable and lets us round-trip
//! against the bit-exact [`super::super::common::idct`]. The 8-bit non-HBD
//! configuration stores the inter-pass buffer as `int16_t` (reproduced with
//! `as i16`); multiply-accumulate temps use `i64` (a superset of the C
//! `int32_t`), which is bit-identical because no overflow occurs for 8-bit
//! residual inputs.

const COSPI_4_64: i64 = 16069;
const COSPI_8_64: i64 = 15137;
const COSPI_12_64: i64 = 13623;
const COSPI_16_64: i64 = 11585;
const COSPI_20_64: i64 = 9102;
const COSPI_24_64: i64 = 6270;
const COSPI_28_64: i64 = 3196;

const DCT_CONST_BITS: u32 = 14;

/// `fdct_round_shift` = `ROUND_POWER_OF_TWO(input, DCT_CONST_BITS)`.
#[inline]
fn fdct_round_shift(input: i64) -> i16 {
    ((input + (1 << (DCT_CONST_BITS - 1))) >> DCT_CONST_BITS) as i16
}

/// `vpx_fdct4x4_c`: forward 4x4 DCT of a residual block.
///
/// `input` is a residual plane (`stride` samples per row); `output` receives 16
/// coefficients in raster order.
pub fn fdct4x4(input: &[i16], stride: usize, output: &mut [i16; 16]) {
    let mut intermediate = [0i16; 16];

    // Pass 0: transform columns of `input`, writing transposed into `intermediate`.
    for i in 0..4 {
        let mut in_high = [0i64; 4];
        for (k, v) in in_high.iter_mut().enumerate() {
            *v = input[i + k * stride] as i64 * 16;
        }
        if i == 0 && in_high[0] != 0 {
            in_high[0] += 1;
        }
        fdct4_pass(&in_high, &mut intermediate[i * 4..i * 4 + 4]);
    }

    // Pass 1: transform columns of `intermediate` (transposed rows), writing `output`.
    for i in 0..4 {
        let in_high = [
            intermediate[i] as i64,
            intermediate[i + 4] as i64,
            intermediate[i + 8] as i64,
            intermediate[i + 12] as i64,
        ];
        fdct4_pass(&in_high, &mut output[i * 4..i * 4 + 4]);
    }

    // Normalize.
    for v in output.iter_mut() {
        *v = ((*v as i32 + 1) >> 2) as i16;
    }
}

/// One 4-point forward DCT pass writing `out[0..4]`.
#[inline]
fn fdct4_pass(in_high: &[i64; 4], out: &mut [i16]) {
    let step0 = in_high[0] + in_high[3];
    let step1 = in_high[1] + in_high[2];
    let step2 = in_high[1] - in_high[2];
    let step3 = in_high[0] - in_high[3];
    out[0] = fdct_round_shift((step0 + step1) * COSPI_16_64);
    out[2] = fdct_round_shift((step0 - step1) * COSPI_16_64);
    out[1] = fdct_round_shift(step2 * COSPI_24_64 + step3 * COSPI_8_64);
    out[3] = fdct_round_shift(-step2 * COSPI_8_64 + step3 * COSPI_24_64);
}

/// `vpx_fdct8x8_c`: forward 8x8 DCT of a residual block.
///
/// `input` is a residual plane (`stride` samples per row); `output` receives 64
/// coefficients in raster order.
pub fn fdct8x8(input: &[i16], stride: usize, output: &mut [i16; 64]) {
    let mut intermediate = [0i16; 64];

    // Pass 0: columns of `input`, scaled by 4.
    for i in 0..8 {
        let mut s = [0i64; 8];
        s[0] = (input[i] as i64 + input[i + 7 * stride] as i64) * 4;
        s[1] = (input[i + stride] as i64 + input[i + 6 * stride] as i64) * 4;
        s[2] = (input[i + 2 * stride] as i64 + input[i + 5 * stride] as i64) * 4;
        s[3] = (input[i + 3 * stride] as i64 + input[i + 4 * stride] as i64) * 4;
        s[4] = (input[i + 3 * stride] as i64 - input[i + 4 * stride] as i64) * 4;
        s[5] = (input[i + 2 * stride] as i64 - input[i + 5 * stride] as i64) * 4;
        s[6] = (input[i + stride] as i64 - input[i + 6 * stride] as i64) * 4;
        s[7] = (input[i] as i64 - input[i + 7 * stride] as i64) * 4;
        fdct8_pass(&s, &mut intermediate[i * 8..i * 8 + 8]);
    }

    // Pass 1: columns of `intermediate`.
    for i in 0..8 {
        let mut s = [0i64; 8];
        s[0] = intermediate[i] as i64 + intermediate[i + 7 * 8] as i64;
        s[1] = intermediate[i + 8] as i64 + intermediate[i + 6 * 8] as i64;
        s[2] = intermediate[i + 2 * 8] as i64 + intermediate[i + 5 * 8] as i64;
        s[3] = intermediate[i + 3 * 8] as i64 + intermediate[i + 4 * 8] as i64;
        s[4] = intermediate[i + 3 * 8] as i64 - intermediate[i + 4 * 8] as i64;
        s[5] = intermediate[i + 2 * 8] as i64 - intermediate[i + 5 * 8] as i64;
        s[6] = intermediate[i + 8] as i64 - intermediate[i + 6 * 8] as i64;
        s[7] = intermediate[i] as i64 - intermediate[i + 7 * 8] as i64;
        fdct8_pass(&s, &mut output[i * 8..i * 8 + 8]);
    }

    // Normalize (integer division truncates toward zero, matching C `/= 2`).
    for v in output.iter_mut() {
        *v /= 2;
    }
}

/// One 8-point forward DCT pass writing `out[0..8]`.
#[inline]
fn fdct8_pass(s: &[i64; 8], out: &mut [i16]) {
    // Even part (fdct4 on s0..s3).
    let x0 = s[0] + s[3];
    let x1 = s[1] + s[2];
    let x2 = s[1] - s[2];
    let x3 = s[0] - s[3];
    out[0] = fdct_round_shift((x0 + x1) * COSPI_16_64);
    out[2] = fdct_round_shift(x2 * COSPI_24_64 + x3 * COSPI_8_64);
    out[4] = fdct_round_shift((x0 - x1) * COSPI_16_64);
    out[6] = fdct_round_shift(-x2 * COSPI_8_64 + x3 * COSPI_24_64);

    // Odd part.
    let t2 = fdct_round_shift((s[6] - s[5]) * COSPI_16_64) as i64;
    let t3 = fdct_round_shift((s[6] + s[5]) * COSPI_16_64) as i64;
    let x0 = s[4] + t2;
    let x1 = s[4] - t2;
    let x2 = s[7] - t3;
    let x3 = s[7] + t3;
    out[1] = fdct_round_shift(x0 * COSPI_28_64 + x3 * COSPI_4_64);
    out[3] = fdct_round_shift(x2 * COSPI_12_64 + x1 * -COSPI_20_64);
    out[5] = fdct_round_shift(x1 * COSPI_12_64 + x2 * COSPI_20_64);
    out[7] = fdct_round_shift(x3 * COSPI_28_64 + x0 * -COSPI_4_64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};

    #[test]
    fn fdct4x4_dc_only_for_flat_residual() {
        // A constant residual has energy only in the DC coefficient.
        let input = [20i16; 16];
        let mut out = [0i16; 16];
        fdct4x4(&input, 4, &mut out);
        assert!(out[0] != 0);
        assert!(out[1..].iter().all(|&c| c == 0), "AC coeffs should be zero");
    }

    #[test]
    fn fdct8x8_dc_scales_with_mean() {
        let input = [15i16; 64];
        let mut out = [0i16; 64];
        fdct8x8(&input, 8, &mut out);
        assert!(out[0] != 0);
        assert!(out[1..].iter().all(|&c| c == 0));
    }

    /// fdct -> (no quant) -> idct should recover the residual up to the
    /// transform's fixed-point rounding, which is small.
    #[test]
    fn fdct4x4_idct_roundtrip_near_lossless() {
        let residual: [i16; 16] = [12, -5, 30, 7, -18, 4, 9, -22, 3, 15, -8, 1, 25, -13, 6, -2];
        // Reconstruct against a mid-gray predictor.
        let pred = [128u8; 16];
        let mut coeffs = [0i16; 16];
        fdct4x4(&residual, 4, &mut coeffs);
        let mut recon = pred;
        idct4x4_add(&coeffs, &mut recon, 4);
        for i in 0..16 {
            let want = 128 + residual[i] as i32;
            let got = recon[i] as i32;
            assert!((want - got).abs() <= 2, "idx {i}: want {want}, got {got}");
        }
    }

    #[test]
    fn fdct8x8_idct_roundtrip_near_lossless() {
        let mut residual = [0i16; 64];
        // A deterministic mix of low- and high-frequency content.
        for (i, r) in residual.iter_mut().enumerate() {
            *r = (((i * 37 + 11) % 41) as i16) - 20;
        }
        let pred = [100u8; 64];
        let mut coeffs = [0i16; 64];
        fdct8x8(&residual, 8, &mut coeffs);
        let mut recon = pred;
        idct8x8_add(&coeffs, &mut recon, 8);
        for i in 0..64 {
            let want = 100 + residual[i] as i32;
            let got = recon[i] as i32;
            assert!((want - got).abs() <= 2, "idx {i}: want {want}, got {got}");
        }
    }
}
