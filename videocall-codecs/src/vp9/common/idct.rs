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

//! Inverse DCT transforms, bit-exact with the VP9 decoder.
//!
//! Ports `vpx_idct4x4_16_add_c` and `vpx_idct8x8_64_add_c` (plus the `idct4_c`
//! / `idct8_c` butterflies) from libvpx `vpx_dsp/inv_txfm.c`, using the 8-bit,
//! non-high-bitdepth configuration (`tran_low_t = int16_t`,
//! `tran_high_t = int32_t`, `CONFIG_EMULATE_HARDWARE = 0`).
//!
//! In that configuration `WRAPLOW(x)` is the identity; the 16-bit truncation
//! that the decoder relies on comes instead from storing intermediate results
//! into the `int16_t` `step`/`output` arrays. We reproduce that exactly by
//! narrowing with `as i16` at the same points. The multiply-accumulate temps
//! use `i64` (a superset of the C `int32_t` `tran_high_t`); no wrap occurs for
//! legal coefficient magnitudes, so results are bit-identical.
//!
//! Only the full (non-eob-shortcut) variants are ported. libvpx's `*_1_add` /
//! partial variants are value-identical fast paths and are not needed.

// cospi_i_64 constants (`vpx_dsp/txfm_common.h`), Q14 fixed point.
const COSPI_4_64: i64 = 16069;
const COSPI_8_64: i64 = 15137;
const COSPI_12_64: i64 = 13623;
const COSPI_16_64: i64 = 11585;
const COSPI_20_64: i64 = 9102;
const COSPI_24_64: i64 = 6270;
const COSPI_28_64: i64 = 3196;

/// `DCT_CONST_BITS` — fixed-point fractional width of the cospi constants.
const DCT_CONST_BITS: u32 = 14;

/// `dct_const_round_shift` = `ROUND_POWER_OF_TWO(input, DCT_CONST_BITS)`.
#[inline]
fn dct_const_round_shift(input: i64) -> i64 {
    (input + (1 << (DCT_CONST_BITS - 1))) >> DCT_CONST_BITS
}

/// `ROUND_POWER_OF_TWO(value, n)` (`vpx_ports/mem.h`).
#[inline]
fn round_power_of_two(value: i64, n: u32) -> i64 {
    (value + (1 << (n - 1))) >> n
}

/// `clip_pixel_add`: add a (WRAPLOW'd) residual to a pixel and clamp to `0..=255`.
#[inline]
fn clip_pixel_add(dest: u8, trans: i64) -> u8 {
    let v = dest as i64 + trans;
    v.clamp(0, 255) as u8
}

/// `idct4_c`: 4-point inverse DCT butterfly. `output` is the `int16_t` domain.
#[inline]
fn idct4(input: &[i32; 4], output: &mut [i16; 4]) {
    let mut step = [0i16; 4];

    // stage 1
    let temp1 = (input[0] as i64 + input[2] as i64) * COSPI_16_64;
    let temp2 = (input[0] as i64 - input[2] as i64) * COSPI_16_64;
    step[0] = dct_const_round_shift(temp1) as i16;
    step[1] = dct_const_round_shift(temp2) as i16;
    let temp1 = input[1] as i64 * COSPI_24_64 - input[3] as i64 * COSPI_8_64;
    let temp2 = input[1] as i64 * COSPI_8_64 + input[3] as i64 * COSPI_24_64;
    step[2] = dct_const_round_shift(temp1) as i16;
    step[3] = dct_const_round_shift(temp2) as i16;

    // stage 2
    output[0] = (step[0] as i32 + step[3] as i32) as i16;
    output[1] = (step[1] as i32 + step[2] as i32) as i16;
    output[2] = (step[1] as i32 - step[2] as i32) as i16;
    output[3] = (step[0] as i32 - step[3] as i32) as i16;
}

/// `idct8_c`: 8-point inverse DCT butterfly. `output` is the `int16_t` domain.
#[inline]
fn idct8(input: &[i32; 8], output: &mut [i16; 8]) {
    let mut step1 = [0i16; 8];
    let mut step2 = [0i16; 8];

    // stage 1
    step1[0] = input[0] as i16;
    step1[2] = input[4] as i16;
    step1[1] = input[2] as i16;
    step1[3] = input[6] as i16;
    let temp1 = input[1] as i64 * COSPI_28_64 - input[7] as i64 * COSPI_4_64;
    let temp2 = input[1] as i64 * COSPI_4_64 + input[7] as i64 * COSPI_28_64;
    step1[4] = dct_const_round_shift(temp1) as i16;
    step1[7] = dct_const_round_shift(temp2) as i16;
    let temp1 = input[5] as i64 * COSPI_12_64 - input[3] as i64 * COSPI_20_64;
    let temp2 = input[5] as i64 * COSPI_20_64 + input[3] as i64 * COSPI_12_64;
    step1[5] = dct_const_round_shift(temp1) as i16;
    step1[6] = dct_const_round_shift(temp2) as i16;

    // stage 2
    let temp1 = (step1[0] as i64 + step1[2] as i64) * COSPI_16_64;
    let temp2 = (step1[0] as i64 - step1[2] as i64) * COSPI_16_64;
    step2[0] = dct_const_round_shift(temp1) as i16;
    step2[1] = dct_const_round_shift(temp2) as i16;
    let temp1 = step1[1] as i64 * COSPI_24_64 - step1[3] as i64 * COSPI_8_64;
    let temp2 = step1[1] as i64 * COSPI_8_64 + step1[3] as i64 * COSPI_24_64;
    step2[2] = dct_const_round_shift(temp1) as i16;
    step2[3] = dct_const_round_shift(temp2) as i16;
    step2[4] = (step1[4] as i32 + step1[5] as i32) as i16;
    step2[5] = (step1[4] as i32 - step1[5] as i32) as i16;
    step2[6] = (-(step1[6] as i32) + step1[7] as i32) as i16;
    step2[7] = (step1[6] as i32 + step1[7] as i32) as i16;

    // stage 3
    step1[0] = (step2[0] as i32 + step2[3] as i32) as i16;
    step1[1] = (step2[1] as i32 + step2[2] as i32) as i16;
    step1[2] = (step2[1] as i32 - step2[2] as i32) as i16;
    step1[3] = (step2[0] as i32 - step2[3] as i32) as i16;
    step1[4] = step2[4];
    let temp1 = (step2[6] as i64 - step2[5] as i64) * COSPI_16_64;
    let temp2 = (step2[5] as i64 + step2[6] as i64) * COSPI_16_64;
    step1[5] = dct_const_round_shift(temp1) as i16;
    step1[6] = dct_const_round_shift(temp2) as i16;
    step1[7] = step2[7];

    // stage 4
    output[0] = (step1[0] as i32 + step1[7] as i32) as i16;
    output[1] = (step1[1] as i32 + step1[6] as i32) as i16;
    output[2] = (step1[2] as i32 + step1[5] as i32) as i16;
    output[3] = (step1[3] as i32 + step1[4] as i32) as i16;
    output[4] = (step1[3] as i32 - step1[4] as i32) as i16;
    output[5] = (step1[2] as i32 - step1[5] as i32) as i16;
    output[6] = (step1[1] as i32 - step1[6] as i32) as i16;
    output[7] = (step1[0] as i32 - step1[7] as i32) as i16;
}

/// `vpx_idct4x4_16_add_c`: inverse 4x4 DCT of `input` (16 coefficients, raster
/// order), adding the result into `dest` (row-major, `stride` bytes per row).
pub fn idct4x4_add(input: &[i16; 16], dest: &mut [u8], stride: usize) {
    let mut out = [0i16; 16];

    // Rows
    for i in 0..4 {
        let row = [
            input[i * 4] as i32,
            input[i * 4 + 1] as i32,
            input[i * 4 + 2] as i32,
            input[i * 4 + 3] as i32,
        ];
        let mut tmp = [0i16; 4];
        idct4(&row, &mut tmp);
        out[i * 4..i * 4 + 4].copy_from_slice(&tmp);
    }

    // Columns
    for i in 0..4 {
        let col = [
            out[i] as i32,
            out[4 + i] as i32,
            out[8 + i] as i32,
            out[12 + i] as i32,
        ];
        let mut temp_out = [0i16; 4];
        idct4(&col, &mut temp_out);
        for (j, &v) in temp_out.iter().enumerate() {
            let idx = j * stride + i;
            dest[idx] = clip_pixel_add(dest[idx], round_power_of_two(v as i64, 4));
        }
    }
}

/// `vpx_idct8x8_64_add_c`: inverse 8x8 DCT of `input` (64 coefficients, raster
/// order), adding the result into `dest` (row-major, `stride` bytes per row).
pub fn idct8x8_add(input: &[i16; 64], dest: &mut [u8], stride: usize) {
    let mut out = [0i16; 64];

    // Rows
    for i in 0..8 {
        let mut row = [0i32; 8];
        for (j, r) in row.iter_mut().enumerate() {
            *r = input[i * 8 + j] as i32;
        }
        let mut tmp = [0i16; 8];
        idct8(&row, &mut tmp);
        out[i * 8..i * 8 + 8].copy_from_slice(&tmp);
    }

    // Columns
    for i in 0..8 {
        let mut col = [0i32; 8];
        for (j, c) in col.iter_mut().enumerate() {
            *c = out[j * 8 + i] as i32;
        }
        let mut temp_out = [0i16; 8];
        idct8(&col, &mut temp_out);
        for (j, &v) in temp_out.iter().enumerate() {
            let idx = j * stride + i;
            dest[idx] = clip_pixel_add(dest[idx], round_power_of_two(v as i64, 5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::idct_fixtures::{IDCT4X4_FIXTURES, IDCT8X8_FIXTURES};

    #[test]
    fn idct4x4_matches_libvpx_fixtures() {
        for (n, f) in IDCT4X4_FIXTURES.iter().enumerate() {
            let mut dest = f.dest_in;
            idct4x4_add(&f.input, &mut dest, 4);
            assert_eq!(dest, f.dest_out, "idct4x4 fixture {n} mismatch");
        }
    }

    #[test]
    fn idct8x8_matches_libvpx_fixtures() {
        for (n, f) in IDCT8X8_FIXTURES.iter().enumerate() {
            let mut dest = f.dest_in;
            idct8x8_add(&f.input, &mut dest, 8);
            assert_eq!(dest, f.dest_out, "idct8x8 fixture {n} mismatch");
        }
    }

    #[test]
    fn idct4x4_all_zero_is_identity() {
        let input = [0i16; 16];
        let mut dest = [100u8; 16];
        idct4x4_add(&input, &mut dest, 4);
        assert_eq!(dest, [100u8; 16]);
    }

    #[test]
    fn idct8x8_dc_only_is_flat_offset() {
        // A single DC coefficient produces a constant offset over the block.
        let mut input = [0i16; 64];
        input[0] = 64;
        let mut dest = [10u8; 64];
        idct8x8_add(&input, &mut dest, 8);
        let first = dest[0];
        assert!(dest.iter().all(|&p| p == first));
        assert!(first > 10, "positive DC should raise the pixel value");
    }
}
