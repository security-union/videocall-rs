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
///
/// SIMD-accelerated on aarch64 (NEON), bit-identical to the scalar port for the
/// encoder's 8-bit residual inputs (`|input| <= 255`), which is the same premise
/// the scalar i64 arithmetic relies on to avoid overflow. Other targets use the
/// scalar path.
pub fn fdct4x4(input: &[i16], stride: usize, output: &mut [i16; 16]) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64; the kernel reads 4 rows of 4
        // samples at `stride` pitch and writes all 16 outputs.
        unsafe { arch_neon::fdct4x4(input, stride, output) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        fdct4x4_scalar(input, stride, output)
    }
}

/// Scalar reference for [`fdct4x4`] (`vpx_fdct4x4_c`).
fn fdct4x4_scalar(input: &[i16], stride: usize, output: &mut [i16; 16]) {
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
///
/// SIMD-accelerated on aarch64 (NEON), bit-identical to the scalar port for the
/// encoder's 8-bit residual inputs (see [`fdct4x4`]). Other targets use scalar.
pub fn fdct8x8(input: &[i16], stride: usize, output: &mut [i16; 64]) {
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64; the kernel reads 8 rows of 8
        // samples at `stride` pitch and writes all 64 outputs.
        unsafe { arch_neon::fdct8x8(input, stride, output) }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        fdct8x8_scalar(input, stride, output)
    }
}

/// Scalar reference for [`fdct8x8`] (`vpx_fdct8x8_c`).
fn fdct8x8_scalar(input: &[i16], stride: usize, output: &mut [i16; 64]) {
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

#[cfg(target_arch = "aarch64")]
mod arch_neon {
    use super::{
        COSPI_12_64, COSPI_16_64, COSPI_20_64, COSPI_24_64, COSPI_28_64, COSPI_4_64, COSPI_8_64,
    };
    use std::arch::aarch64::*;

    // The DCT constants as i32 (they all fit); the products stay within i32 for
    // 8-bit residual inputs, so this NEON path is bit-identical to the scalar i64
    // arithmetic over the encoder's actual input range.
    const C16: i32 = COSPI_16_64 as i32;
    const C8: i32 = COSPI_8_64 as i32;
    const C24: i32 = COSPI_24_64 as i32;
    const C4: i32 = COSPI_4_64 as i32;
    const C12: i32 = COSPI_12_64 as i32;
    const C20: i32 = COSPI_20_64 as i32;
    const C28: i32 = COSPI_28_64 as i32;

    /// `fdct_round_shift` across four lanes: `(x + (1 << 13)) >> 14`.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn rs4(x: int32x4_t) -> int32x4_t {
        vshrq_n_s32(vaddq_s32(x, vdupq_n_s32(1 << 13)), 14)
    }

    /// Truncate to i16 and sign-extend back — emulates the scalar `intermediate`
    /// being stored as `i16` between the two passes.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn i16_roundtrip(x: int32x4_t) -> int32x4_t {
        vmovl_s16(vmovn_s32(x))
    }

    /// Transpose the 4x4 i32 matrix whose rows are `a,b,c,d`; result row `k` is
    /// `[a[k], b[k], c[k], d[k]]`.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn transpose4(a: int32x4_t, b: int32x4_t, c: int32x4_t, d: int32x4_t) -> [int32x4_t; 4] {
        let ab = vtrnq_s32(a, b);
        let cd = vtrnq_s32(c, d);
        [
            vcombine_s32(vget_low_s32(ab.0), vget_low_s32(cd.0)),
            vcombine_s32(vget_low_s32(ab.1), vget_low_s32(cd.1)),
            vcombine_s32(vget_high_s32(ab.0), vget_high_s32(cd.0)),
            vcombine_s32(vget_high_s32(ab.1), vget_high_s32(cd.1)),
        ]
    }

    /// One 4-point forward DCT pass across four columns (lanes), returning the
    /// four output positions. Mirrors `super::fdct4_pass` lane-wise.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn fdct4_pass(
        in0: int32x4_t,
        in1: int32x4_t,
        in2: int32x4_t,
        in3: int32x4_t,
    ) -> [int32x4_t; 4] {
        let step0 = vaddq_s32(in0, in3);
        let step1 = vaddq_s32(in1, in2);
        let step2 = vsubq_s32(in1, in2);
        let step3 = vsubq_s32(in0, in3);
        let o0 = rs4(vmulq_n_s32(vaddq_s32(step0, step1), C16));
        let o2 = rs4(vmulq_n_s32(vsubq_s32(step0, step1), C16));
        // step2 * C24 + step3 * C8
        let o1 = rs4(vaddq_s32(vmulq_n_s32(step2, C24), vmulq_n_s32(step3, C8)));
        // step3 * C24 - step2 * C8
        let o3 = rs4(vsubq_s32(vmulq_n_s32(step3, C24), vmulq_n_s32(step2, C8)));
        [o0, o1, o2, o3]
    }

    /// NEON `vpx_fdct4x4_c`, vectorized across the four columns.
    ///
    /// # Safety
    /// `input` must expose 4 rows of 4 samples at `stride` pitch; NEON required.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn fdct4x4(input: &[i16], stride: usize, output: &mut [i16; 16]) {
        let ip = input.as_ptr();
        let row =
            |r: usize| -> int32x4_t { vshlq_n_s32(vmovl_s16(vld1_s16(ip.add(r * stride))), 4) };
        let mut r0 = row(0);
        let r1 = row(1);
        let r2 = row(2);
        let r3 = row(3);
        // Pass-0 DC bias: column 0's in_high[0] += 1 when non-zero.
        let l0 = vgetq_lane_s32(r0, 0);
        if l0 != 0 {
            r0 = vsetq_lane_s32(l0 + 1, r0, 0);
        }

        let o = fdct4_pass(r0, r1, r2, r3);
        // Store/reload as i16 (the scalar `intermediate` type), then transpose so
        // pass 1 transforms the intermediate's columns.
        let t = transpose4(
            i16_roundtrip(o[0]),
            i16_roundtrip(o[1]),
            i16_roundtrip(o[2]),
            i16_roundtrip(o[3]),
        );
        let p = fdct4_pass(t[0], t[1], t[2], t[3]);
        let po = transpose4(p[0], p[1], p[2], p[3]);

        let op = output.as_mut_ptr();
        for (i, &prow) in po.iter().enumerate() {
            // Scalar writes the pass-1 result to `output` as i16, then normalizes
            // it as `((v as i32 + 1) >> 2) as i16`.
            let stored = vmovl_s16(vmovn_s32(prow));
            let norm = vshrq_n_s32(vaddq_s32(stored, vdupq_n_s32(1)), 2);
            vst1_s16(op.add(i * 4), vmovn_s32(norm));
        }
    }

    /// An 8-wide row of i32 values, held as two NEON quads: columns 0..4 and 4..8.
    type V = (int32x4_t, int32x4_t);

    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn add2(a: V, b: V) -> V {
        (vaddq_s32(a.0, b.0), vaddq_s32(a.1, b.1))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn sub2(a: V, b: V) -> V {
        (vsubq_s32(a.0, b.0), vsubq_s32(a.1, b.1))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn muln2(a: V, k: i32) -> V {
        (vmulq_n_s32(a.0, k), vmulq_n_s32(a.1, k))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn rs2(a: V) -> V {
        (rs4(a.0), rs4(a.1))
    }
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn i16rt2(a: V) -> V {
        (i16_roundtrip(a.0), i16_roundtrip(a.1))
    }

    /// One 8-point forward DCT pass across eight columns. Mirrors
    /// `super::fdct8_pass` lane-wise, including the intermediate i16 rounding of
    /// the odd-part `t2`/`t3` terms.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn fdct8_pass(s: &[V; 8]) -> [V; 8] {
        // Even part (fdct4 on s0..s3).
        let x0 = add2(s[0], s[3]);
        let x1 = add2(s[1], s[2]);
        let x2 = sub2(s[1], s[2]);
        let x3 = sub2(s[0], s[3]);
        let o0 = rs2(muln2(add2(x0, x1), C16));
        let o2 = rs2(add2(muln2(x2, C24), muln2(x3, C8)));
        let o4 = rs2(muln2(sub2(x0, x1), C16));
        let o6 = rs2(sub2(muln2(x3, C24), muln2(x2, C8)));

        // Odd part. t2/t3 are round-shifted then stored as i16 in the scalar code.
        let t2 = i16rt2(rs2(muln2(sub2(s[6], s[5]), C16)));
        let t3 = i16rt2(rs2(muln2(add2(s[6], s[5]), C16)));
        let y0 = add2(s[4], t2);
        let y1 = sub2(s[4], t2);
        let y2 = sub2(s[7], t3);
        let y3 = add2(s[7], t3);
        let o1 = rs2(add2(muln2(y0, C28), muln2(y3, C4)));
        let o3 = rs2(add2(muln2(y2, C12), muln2(y1, -C20)));
        let o5 = rs2(add2(muln2(y1, C12), muln2(y2, C20)));
        let o7 = rs2(add2(muln2(y3, C28), muln2(y0, -C4)));

        [o0, o1, o2, o3, o4, o5, o6, o7]
    }

    /// Transpose an 8x8 i32 matrix stored as eight row-pairs, via four 4x4 block
    /// transposes with the off-diagonal blocks swapped.
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn transpose8(r: &[V; 8]) -> [V; 8] {
        let at = transpose4(r[0].0, r[1].0, r[2].0, r[3].0); // top-left block^T
        let ct = transpose4(r[4].0, r[5].0, r[6].0, r[7].0); // bottom-left block^T
        let bt = transpose4(r[0].1, r[1].1, r[2].1, r[3].1); // top-right block^T
        let dt = transpose4(r[4].1, r[5].1, r[6].1, r[7].1); // bottom-right block^T
        [
            (at[0], ct[0]),
            (at[1], ct[1]),
            (at[2], ct[2]),
            (at[3], ct[3]),
            (bt[0], dt[0]),
            (bt[1], dt[1]),
            (bt[2], dt[2]),
            (bt[3], dt[3]),
        ]
    }

    /// Signed divide-by-2 truncating toward zero (matches the scalar `v /= 2`,
    /// which differs from an arithmetic `>> 1` for negative odd values).
    #[inline]
    #[target_feature(enable = "neon")]
    unsafe fn div2_trunc(v: int32x4_t) -> int32x4_t {
        // bias = 1 for negatives, 0 otherwise; (v + bias) >> 1.
        let bias = vnegq_s32(vshrq_n_s32(v, 31));
        vshrq_n_s32(vaddq_s32(v, bias), 1)
    }

    /// NEON `vpx_fdct8x8_c`, vectorized across the eight columns.
    ///
    /// # Safety
    /// `input` must expose 8 rows of 8 samples at `stride` pitch; NEON required.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn fdct8x8(input: &[i16], stride: usize, output: &mut [i16; 64]) {
        let ip = input.as_ptr();
        let load = |r: usize| -> V {
            let base = ip.add(r * stride);
            (vmovl_s16(vld1_s16(base)), vmovl_s16(vld1_s16(base.add(4))))
        };
        let r = [
            load(0),
            load(1),
            load(2),
            load(3),
            load(4),
            load(5),
            load(6),
            load(7),
        ];

        // Pass 0: butterfly-pair the rows (scaled by 4), then the 8-point pass.
        let s0 = [
            muln2(add2(r[0], r[7]), 4),
            muln2(add2(r[1], r[6]), 4),
            muln2(add2(r[2], r[5]), 4),
            muln2(add2(r[3], r[4]), 4),
            muln2(sub2(r[3], r[4]), 4),
            muln2(sub2(r[2], r[5]), 4),
            muln2(sub2(r[1], r[6]), 4),
            muln2(sub2(r[0], r[7]), 4),
        ];
        let o = fdct8_pass(&s0);
        // Store the intermediate as i16, then transpose so pass 1 sees columns.
        let inter = [
            i16rt2(o[0]),
            i16rt2(o[1]),
            i16rt2(o[2]),
            i16rt2(o[3]),
            i16rt2(o[4]),
            i16rt2(o[5]),
            i16rt2(o[6]),
            i16rt2(o[7]),
        ];
        let t = transpose8(&inter);

        // Pass 1: butterfly-pair (no scaling), then the 8-point pass.
        let s1 = [
            add2(t[0], t[7]),
            add2(t[1], t[6]),
            add2(t[2], t[5]),
            add2(t[3], t[4]),
            sub2(t[3], t[4]),
            sub2(t[2], t[5]),
            sub2(t[1], t[6]),
            sub2(t[0], t[7]),
        ];
        let p = fdct8_pass(&s1);
        let po = transpose8(&p);

        let op = output.as_mut_ptr();
        for (i, &prow) in po.iter().enumerate() {
            // Scalar writes pass 1 to `output` as i16 then normalizes with `/= 2`.
            let lo = div2_trunc(vmovl_s16(vmovn_s32(prow.0)));
            let hi = div2_trunc(vmovl_s16(vmovn_s32(prow.1)));
            vst1_s16(op.add(i * 8), vmovn_s32(lo));
            vst1_s16(op.add(i * 8 + 4), vmovn_s32(hi));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};

    /// Deterministic xorshift32 (matches the crate's other test RNGs).
    struct XorShift32(u32);
    impl XorShift32 {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        /// Residual-range sample in `[-255, 255]` (the encoder's fdct input domain).
        fn residual(&mut self) -> i16 {
            (self.next() % 511) as i16 - 255
        }
    }

    #[test]
    fn fdct4x4_matches_scalar() {
        let mut rng = XorShift32(0x1357_9bdf);
        for trial in 0..4096 {
            let mut input = [0i16; 16];
            match trial {
                0 => {}
                1 => input.iter_mut().for_each(|v| *v = 255),
                2 => input.iter_mut().for_each(|v| *v = -255),
                _ => input.iter_mut().for_each(|v| *v = rng.residual()),
            }
            let mut a = [0i16; 16];
            let mut b = [0i16; 16];
            fdct4x4(&input, 4, &mut a);
            fdct4x4_scalar(&input, 4, &mut b);
            assert_eq!(a, b, "fdct4x4 mismatch trial {trial}");
        }
    }

    #[test]
    fn fdct8x8_matches_scalar() {
        let mut rng = XorShift32(0x2468_ace0);
        for trial in 0..4096 {
            let mut input = [0i16; 64];
            match trial {
                0 => {}
                1 => input.iter_mut().for_each(|v| *v = 255),
                2 => input.iter_mut().for_each(|v| *v = -255),
                _ => input.iter_mut().for_each(|v| *v = rng.residual()),
            }
            let mut a = [0i16; 64];
            let mut b = [0i16; 64];
            fdct8x8(&input, 8, &mut a);
            fdct8x8_scalar(&input, 8, &mut b);
            assert_eq!(a, b, "fdct8x8 mismatch trial {trial}");
        }
    }

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
