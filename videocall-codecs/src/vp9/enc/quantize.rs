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

//! Forward quantization (fast-path / `fp`).
//!
//! Ports the realtime quantizer libvpx uses under the `--rt` deadline:
//! `vp9_quantize_fp_c` (`vp9/encoder/vp9_quantize.c`) together with the
//! per-qindex `quant_fp`/`round_fp` derivation from `vp9_init_quantizer`. The
//! dequant step sizes come from [`super::super::common::quant`].
//!
//! Coefficients are visited in scan order so the returned end-of-block index
//! matches the decoder's. Quantized magnitudes are clamped to the range the
//! coefficient token machinery can encode (CAT6), and the dequantized output is
//! computed from the clamped value so encoder and decoder reconstruct
//! identically.

use crate::vp9::common::block::TxSize;
use crate::vp9::common::generated::{SCAN_DEFAULT_4X4, SCAN_DEFAULT_8X8};
use crate::vp9::common::quant::{ac_quant, dc_quant};

/// Largest coefficient magnitude the token coder can represent (8-bit): the
/// CAT6 base value (67) plus its 14 extra bits (`2^14 - 1`). Larger quantized
/// values are clamped here so every emitted coefficient is codable.
pub const CAT6_MAX_ABS: i32 = 67 + (1 << 14) - 1;

/// Per-qindex quantizer factors for one plane, index `[0] = DC`, `[1] = AC`.
#[derive(Clone, Copy, Debug)]
pub struct QuantParams {
    /// Dequant step sizes (`vp9_dc_quant`/`vp9_ac_quant`).
    pub dequant: [i16; 2],
    /// `quant_fp = (1 << 16) / dequant`.
    pub quant_fp: [i16; 2],
    /// `round_fp = (rounding_factor * dequant) >> 7`.
    pub round_fp: [i16; 2],
}

impl QuantParams {
    /// Build the fast-path factors for `qindex` (0..=255), with per-plane DC/AC
    /// deltas (0 for the keyframe Y and, in our configuration, chroma).
    pub fn new(qindex: i32, dc_delta: i32, ac_delta: i32) -> Self {
        let dequant = [dc_quant(qindex, dc_delta), ac_quant(qindex, ac_delta)];
        let mut quant_fp = [0i16; 2];
        let mut round_fp = [0i16; 2];
        for i in 0..2 {
            let d = dequant[i] as i32;
            quant_fp[i] = ((1 << 16) / d) as i16;
            let rounding_factor = if qindex == 0 {
                64
            } else if i == 0 {
                48
            } else {
                42
            };
            round_fp[i] = ((rounding_factor * d) >> 7) as i16;
        }
        Self {
            dequant,
            quant_fp,
            round_fp,
        }
    }
}

/// Number of coefficients and the default scan for a transform size.
fn scan_for(tx_size: TxSize) -> &'static [i16] {
    match tx_size {
        TxSize::Tx4X4 => &SCAN_DEFAULT_4X4,
        TxSize::Tx8X8 => &SCAN_DEFAULT_8X8,
        // Larger transforms are not produced by the current keyframe pipeline.
        _ => unreachable!("quantize_fp only supports 4x4 and 8x8 transforms"),
    }
}

/// Quantize a single coefficient with one `(round, quant_fp, dequant)` triple.
/// The exact scalar arithmetic the SIMD kernels below must reproduce. Returns
/// `(qcoeff, dqcoeff)`.
#[inline]
fn quantize_one(coeff: i16, round: i16, quant_fp: i16, dequant: i16) -> (i16, i16) {
    let coeff_i = coeff as i32;
    let sign = coeff_i >> 31; // 0 for non-negative, -1 for negative
    let abs_coeff = (coeff_i ^ sign) - sign;
    let mut tmp = (abs_coeff + round as i32).clamp(i16::MIN as i32, i16::MAX as i32);
    tmp = (tmp * quant_fp as i32) >> 16;
    tmp = tmp.min(CAT6_MAX_ABS);
    let q = (tmp ^ sign) - sign;
    // Decoder computes dqcoeff = qcoeff * dequant truncated to int16.
    (q as i16, (q * dequant as i32) as i16)
}

/// Scalar reference: quantize `n` raster-order coefficients with one
/// `(round, quant_fp, dequant)` triple. The oracle the SIMD kernels match, and
/// the fallback on architectures without a specialized path.
#[inline]
fn quantize_core_scalar(
    coeff: &[i16],
    round: i16,
    quant_fp: i16,
    dequant: i16,
    n: usize,
    qcoeff: &mut [i16],
    dqcoeff: &mut [i16],
) {
    for i in 0..n {
        let (q, dq) = quantize_one(coeff[i], round, quant_fp, dequant);
        qcoeff[i] = q;
        dqcoeff[i] = dq;
    }
}

/// Quantize `n` raster-order coefficients with one `(round, quant_fp, dequant)`
/// triple. SIMD-accelerated but bit-identical to [`quantize_core_scalar`].
///
/// Dispatch mirrors [`crate::vp9::enc::sad`]: NEON on aarch64, runtime-detected
/// AVX2 on x86_64, scalar elsewhere.
#[inline]
fn quantize_core(
    coeff: &[i16],
    round: i16,
    quant_fp: i16,
    dequant: i16,
    n: usize,
    qcoeff: &mut [i16],
    dqcoeff: &mut [i16],
) {
    debug_assert!(coeff.len() >= n && qcoeff.len() >= n && dqcoeff.len() >= n);

    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is guaranteed on aarch64; the debug_assert above bounds
        // the pointer accesses.
        unsafe { arch_neon::quantize_core(coeff, round, quant_fp, dequant, n, qcoeff, dqcoeff) }
    }

    #[cfg(target_arch = "x86_64")]
    {
        if arch_x86::avx2_available() {
            // SAFETY: guarded by the cached runtime AVX2 check; bounded as above.
            unsafe {
                arch_x86::quantize_core_avx2(coeff, round, quant_fp, dequant, n, qcoeff, dqcoeff)
            }
        } else if arch_x86::sse41_available() {
            // SAFETY: guarded by the cached runtime SSE4.1 check; bounded as above.
            unsafe {
                arch_x86::quantize_core_sse41(coeff, round, quant_fp, dequant, n, qcoeff, dqcoeff)
            }
        } else {
            quantize_core_scalar(coeff, round, quant_fp, dequant, n, qcoeff, dqcoeff)
        }
    }

    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        quantize_core_scalar(coeff, round, quant_fp, dequant, n, qcoeff, dqcoeff)
    }
}

/// Quantize `coeff` (raster order) into `qcoeff` and `dqcoeff`, returning the
/// end-of-block count (number of coefficients up to and including the last
/// non-zero one in scan order). All three slices must be at least `n²` long for
/// the given transform.
///
/// Port of `vp9_quantize_fp_c` with an added CAT6 magnitude clamp. Only the DC
/// coefficient (raster index 0) uses the DC quantizer factors; every other
/// position uses AC. We therefore quantize the whole block with AC factors
/// (vectorizable — position-independent), then overwrite the DC coefficient,
/// then derive the EOB from the quantized magnitudes in scan order. This is
/// bit-identical to the per-coefficient scan-order loop it replaces because each
/// output position depends only on its own input.
pub fn quantize_fp(
    coeff: &[i16],
    tx_size: TxSize,
    qp: &QuantParams,
    qcoeff: &mut [i16],
    dqcoeff: &mut [i16],
) -> u16 {
    let scan = scan_for(tx_size);
    let n = scan.len();

    // AC pass over every position (the scan is a permutation of `0..n`, so all
    // are written); the DC position is corrected next.
    quantize_core(
        coeff,
        qp.round_fp[1],
        qp.quant_fp[1],
        qp.dequant[1],
        n,
        qcoeff,
        dqcoeff,
    );
    let (q0, dq0) = quantize_one(coeff[0], qp.round_fp[0], qp.quant_fp[0], qp.dequant[0]);
    qcoeff[0] = q0;
    dqcoeff[0] = dq0;

    // End-of-block: 1 + the largest scan index whose coefficient is non-zero
    // (`tmp != 0` in the scalar loop is exactly `qcoeff[rc] != 0`). Scan from the
    // end and stop at the first non-zero — typical blocks are sparse in the
    // high-frequency tail, so this exits early.
    for i in (0..n).rev() {
        if qcoeff[scan[i] as usize] != 0 {
            return (i + 1) as u16;
        }
    }
    0
}

#[cfg(target_arch = "aarch64")]
mod arch_neon {
    use super::CAT6_MAX_ABS;
    use std::arch::aarch64::*;

    /// NEON quantize core: 4 i32 lanes per step, scalar tail. Bit-identical to
    /// `quantize_core_scalar` — the same clamps, `>> 16`, CAT6 min, sign restore
    /// and wrapping `as i16` narrows, just SIMD-parallel across coefficients.
    ///
    /// # Safety
    /// `coeff`, `qcoeff`, `dqcoeff` must each be at least `n` long.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn quantize_core(
        coeff: &[i16],
        round: i16,
        quant_fp: i16,
        dequant: i16,
        n: usize,
        qcoeff: &mut [i16],
        dqcoeff: &mut [i16],
    ) {
        let vround = vdupq_n_s32(round as i32);
        let vquant = vdupq_n_s32(quant_fp as i32);
        let vdequant = vdupq_n_s32(dequant as i32);
        let vcat6 = vdupq_n_s32(CAT6_MAX_ABS);
        let vlo = vdupq_n_s32(i16::MIN as i32);
        let vhi = vdupq_n_s32(i16::MAX as i32);

        let cp = coeff.as_ptr();
        let qp = qcoeff.as_mut_ptr();
        let dp = dqcoeff.as_mut_ptr();

        let mut i = 0;
        while i + 4 <= n {
            // Widen 4 i16 coefficients to i32 lanes.
            let c = vmovl_s16(vld1_s16(cp.add(i)));
            let sign = vshrq_n_s32(c, 31); // 0 or -1 per lane
            let abs = vabsq_s32(c); // == (c ^ sign) - sign for i16-range inputs

            // clamp(abs + round, i16::MIN, i16::MAX)
            let mut tmp = vaddq_s32(abs, vround);
            tmp = vminq_s32(vmaxq_s32(tmp, vlo), vhi);
            // (tmp * quant_fp) >> 16   (tmp >= 0, so the arithmetic shift matches)
            tmp = vshrq_n_s32(vmulq_s32(tmp, vquant), 16);
            // min(tmp, CAT6_MAX_ABS)
            tmp = vminq_s32(tmp, vcat6);

            // q = (tmp ^ sign) - sign; fits i16, narrow is exact.
            let q = vsubq_s32(veorq_s32(tmp, sign), sign);
            vst1_s16(qp.add(i), vmovn_s32(q));
            // dq = (q * dequant) as i16 — low 16 bits (vmovn truncates, matching
            // the wrapping cast; the i32 product itself never overflows here).
            vst1_s16(dp.add(i), vmovn_s32(vmulq_s32(q, vdequant)));

            i += 4;
        }
        // Scalar tail for any n not a multiple of 4 (none in practice: n is 16 or 64).
        while i < n {
            let (q, dq) = super::quantize_one(*cp.add(i), round, quant_fp, dequant);
            *qp.add(i) = q;
            *dp.add(i) = dq;
            i += 1;
        }
    }
}

#[cfg(target_arch = "x86_64")]
mod arch_x86 {
    use super::CAT6_MAX_ABS;
    use std::arch::x86_64::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Cached feature detection: 0 = unknown, 1 = present, 2 = absent.
    static AVX2: AtomicU8 = AtomicU8::new(0);
    static SSE41: AtomicU8 = AtomicU8::new(0);

    /// Read/populate a cached feature flag.
    #[inline]
    fn cached(slot: &AtomicU8, detected: impl FnOnce() -> bool) -> bool {
        match slot.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let has = detected();
                slot.store(if has { 1 } else { 2 }, Ordering::Relaxed);
                has
            }
        }
    }

    /// Whether AVX2 is available, detected once and cached.
    #[inline]
    pub(super) fn avx2_available() -> bool {
        cached(&AVX2, || is_x86_feature_detected!("avx2"))
    }

    /// Whether SSE4.1 is available, detected once and cached.
    #[inline]
    pub(super) fn sse41_available() -> bool {
        cached(&SSE41, || is_x86_feature_detected!("sse4.1"))
    }

    /// Truncating narrow of four i32 lanes to four i16 (low 16 bits of each),
    /// matching Rust's wrapping `as i16`. Same mask-then-`packus` idiom as the
    /// AVX2 path, at 128-bit width; the four results land in the low 8 bytes.
    #[inline]
    #[target_feature(enable = "sse4.1")]
    unsafe fn narrow_i32x4_to_i16x4(v: __m128i) -> __m128i {
        let masked = _mm_and_si128(v, _mm_set1_epi32(0xffff));
        _mm_packus_epi32(masked, masked)
    }

    /// SSE4.1 quantize core: 4 i32 lanes per step, scalar tail. Bit-identical to
    /// `quantize_core_scalar`. This 128-bit path shares the AVX2 path's exact
    /// arithmetic and narrowing idiom, so verifying it (runnable under Rosetta,
    /// which lacks AVX2) also exercises that shared logic.
    ///
    /// # Safety
    /// `coeff`, `qcoeff`, `dqcoeff` must each be at least `n` long, and SSE4.1
    /// must be available (guaranteed by the [`sse41_available`] guard).
    #[target_feature(enable = "sse4.1")]
    pub(super) unsafe fn quantize_core_sse41(
        coeff: &[i16],
        round: i16,
        quant_fp: i16,
        dequant: i16,
        n: usize,
        qcoeff: &mut [i16],
        dqcoeff: &mut [i16],
    ) {
        let vround = _mm_set1_epi32(round as i32);
        let vquant = _mm_set1_epi32(quant_fp as i32);
        let vdequant = _mm_set1_epi32(dequant as i32);
        let vcat6 = _mm_set1_epi32(CAT6_MAX_ABS);
        let vlo = _mm_set1_epi32(i16::MIN as i32);
        let vhi = _mm_set1_epi32(i16::MAX as i32);

        let cp = coeff.as_ptr();
        let qp = qcoeff.as_mut_ptr();
        let dp = dqcoeff.as_mut_ptr();

        let mut i = 0;
        while i + 4 <= n {
            let c = _mm_cvtepi16_epi32(_mm_loadl_epi64(cp.add(i) as *const __m128i));
            let sign = _mm_srai_epi32(c, 31);
            let abs = _mm_abs_epi32(c);

            let mut tmp = _mm_add_epi32(abs, vround);
            tmp = _mm_min_epi32(_mm_max_epi32(tmp, vlo), vhi);
            tmp = _mm_srai_epi32(_mm_mullo_epi32(tmp, vquant), 16);
            tmp = _mm_min_epi32(tmp, vcat6);

            let q = _mm_sub_epi32(_mm_xor_si128(tmp, sign), sign);
            _mm_storel_epi64(qp.add(i) as *mut __m128i, narrow_i32x4_to_i16x4(q));
            let dq = _mm_mullo_epi32(q, vdequant);
            _mm_storel_epi64(dp.add(i) as *mut __m128i, narrow_i32x4_to_i16x4(dq));

            i += 4;
        }
        while i < n {
            let (q, dq) = super::quantize_one(*cp.add(i), round, quant_fp, dequant);
            *qp.add(i) = q;
            *dp.add(i) = dq;
            i += 1;
        }
    }

    /// Truncating narrow of eight i32 lanes to eight i16 (low 16 bits of each),
    /// matching Rust's wrapping `as i16`. Masks to 16 bits then `packus` (which
    /// won't saturate on in-range values) and de-interleaves the 128-bit halves.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn narrow_i32x8_to_i16x8(v: __m256i) -> __m128i {
        let masked = _mm256_and_si256(v, _mm256_set1_epi32(0xffff));
        let packed = _mm256_packus_epi32(masked, masked); // per-128-lane interleave
        let perm = _mm256_permute4x64_epi64(packed, 0b1000);
        _mm256_castsi256_si128(perm)
    }

    /// AVX2 quantize core: 8 i32 lanes per step, scalar tail. Bit-identical to
    /// `quantize_core_scalar`.
    ///
    /// # Safety
    /// `coeff`, `qcoeff`, `dqcoeff` must each be at least `n` long, and AVX2 must
    /// be available (guaranteed by the [`avx2_available`] guard at the call site).
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn quantize_core_avx2(
        coeff: &[i16],
        round: i16,
        quant_fp: i16,
        dequant: i16,
        n: usize,
        qcoeff: &mut [i16],
        dqcoeff: &mut [i16],
    ) {
        let vround = _mm256_set1_epi32(round as i32);
        let vquant = _mm256_set1_epi32(quant_fp as i32);
        let vdequant = _mm256_set1_epi32(dequant as i32);
        let vcat6 = _mm256_set1_epi32(CAT6_MAX_ABS);
        let vlo = _mm256_set1_epi32(i16::MIN as i32);
        let vhi = _mm256_set1_epi32(i16::MAX as i32);

        let cp = coeff.as_ptr();
        let qp = qcoeff.as_mut_ptr();
        let dp = dqcoeff.as_mut_ptr();

        let mut i = 0;
        while i + 8 <= n {
            // Widen 8 i16 coefficients to i32 lanes.
            let c = _mm256_cvtepi16_epi32(_mm_loadu_si128(cp.add(i) as *const __m128i));
            let sign = _mm256_srai_epi32(c, 31);
            let abs = _mm256_abs_epi32(c);

            let mut tmp = _mm256_add_epi32(abs, vround);
            tmp = _mm256_min_epi32(_mm256_max_epi32(tmp, vlo), vhi);
            tmp = _mm256_srai_epi32(_mm256_mullo_epi32(tmp, vquant), 16);
            tmp = _mm256_min_epi32(tmp, vcat6);

            let q = _mm256_sub_epi32(_mm256_xor_si256(tmp, sign), sign);
            _mm_storeu_si128(qp.add(i) as *mut __m128i, narrow_i32x8_to_i16x8(q));
            let dq = _mm256_mullo_epi32(q, vdequant);
            _mm_storeu_si128(dp.add(i) as *mut __m128i, narrow_i32x8_to_i16x8(dq));

            i += 8;
        }
        // Scalar tail (n not a multiple of 8: e.g. would apply to a hypothetical
        // smaller block; n is 16 or 64 today so this rarely runs).
        while i < n {
            let (q, dq) = super::quantize_one(*cp.add(i), round, quant_fp, dequant);
            *qp.add(i) = q;
            *dp.add(i) = dq;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vp9::common::idct::{idct4x4_add, idct8x8_add};
    use crate::vp9::enc::fdct::{fdct4x4, fdct8x8};

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
    }

    /// The SIMD quantize core must be byte-for-byte identical to the scalar core
    /// for both block sizes over many random coefficient blocks and quantizer
    /// factors, including saturation edges (i16::MIN/MAX coefficients and the
    /// smallest/largest real quantizers).
    #[test]
    fn simd_quantize_core_matches_scalar() {
        let mut rng = XorShift32(0x9e37_79b9);
        for &n in &[16usize, 64usize] {
            for trial in 0..256 {
                // Real quantizer factors span qindex 0..=255 for DC and AC.
                let qp = QuantParams::new((rng.next() % 256) as i32, 0, 0);
                let ac = (rng.next() & 1) as usize;
                let (round, quant_fp, dequant) = (qp.round_fp[ac], qp.quant_fp[ac], qp.dequant[ac]);

                let mut coeff = vec![0i16; n];
                match trial {
                    0 => {} // all zero
                    1 => coeff.iter_mut().for_each(|c| *c = i16::MAX),
                    2 => coeff.iter_mut().for_each(|c| *c = i16::MIN),
                    3 => coeff
                        .iter_mut()
                        .enumerate()
                        .for_each(|(i, c)| *c = if i & 1 == 0 { i16::MAX } else { i16::MIN }),
                    _ => coeff.iter_mut().for_each(|c| *c = rng.next() as i16),
                }

                let mut q_simd = vec![0i16; n];
                let mut dq_simd = vec![0i16; n];
                let mut q_scalar = vec![0i16; n];
                let mut dq_scalar = vec![0i16; n];

                quantize_core(
                    &coeff,
                    round,
                    quant_fp,
                    dequant,
                    n,
                    &mut q_simd,
                    &mut dq_simd,
                );
                quantize_core_scalar(
                    &coeff,
                    round,
                    quant_fp,
                    dequant,
                    n,
                    &mut q_scalar,
                    &mut dq_scalar,
                );

                assert_eq!(q_simd, q_scalar, "qcoeff mismatch n={n} trial={trial}");
                assert_eq!(dq_simd, dq_scalar, "dqcoeff mismatch n={n} trial={trial}");
            }
        }
    }

    #[test]
    fn quant_params_sane() {
        // qindex 0: smallest step, DC round factor 64.
        let qp = QuantParams::new(0, 0, 0);
        assert_eq!(qp.dequant, [4, 4]);
        assert_eq!(qp.quant_fp, [16384, 16384]);
        assert_eq!(qp.round_fp[0], (64 * 4) >> 7);
        // Higher qindex: larger step, AC round factor 42.
        let qp = QuantParams::new(128, 0, 0);
        assert_eq!(qp.round_fp[1], ((42 * qp.dequant[1] as i32) >> 7) as i16);
    }

    #[test]
    fn all_zero_coeffs_give_eob_zero() {
        let coeff = [0i16; 16];
        let qp = QuantParams::new(100, 0, 0);
        let mut q = [0i16; 16];
        let mut dq = [0i16; 16];
        let eob = quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
        assert_eq!(eob, 0);
        assert!(q.iter().all(|&c| c == 0));
    }

    #[test]
    fn dc_only_gives_eob_one() {
        let mut coeff = [0i16; 16];
        coeff[0] = 2000;
        let qp = QuantParams::new(100, 0, 0);
        let mut q = [0i16; 16];
        let mut dq = [0i16; 16];
        let eob = quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
        assert_eq!(eob, 1);
        assert!(q[0] != 0);
        assert!(q[1..].iter().all(|&c| c == 0));
        // dqcoeff is a multiple of the DC step.
        assert_eq!(dq[0], (q[0] as i32 * qp.dequant[0] as i32) as i16);
    }

    #[test]
    fn dequant_is_multiple_of_step() {
        let mut coeff = [0i16; 64];
        for (i, c) in coeff.iter_mut().enumerate() {
            *c = ((i as i16 * 13) % 97) - 48;
        }
        let qp = QuantParams::new(80, 0, 0);
        let mut q = [0i16; 64];
        let mut dq = [0i16; 64];
        quantize_fp(&coeff, TxSize::Tx8X8, &qp, &mut q, &mut dq);
        for i in 0..64 {
            let ac = (i != 0) as usize;
            assert_eq!(dq[i], (q[i] as i32 * qp.dequant[ac] as i32) as i16);
        }
    }

    /// fdct -> quantize -> idct reconstruction error stays bounded; near-lossless
    /// at qindex 0 and merely bounded at a high qindex.
    #[test]
    fn roundtrip_error_bounded_4x4() {
        let residual: [i16; 16] = [
            40, -12, 33, -7, 18, -25, 9, 14, -30, 6, 21, -3, 11, -19, 5, -8,
        ];
        for (qindex, bound) in [(0i32, 3i32), (100, 40)] {
            let qp = QuantParams::new(qindex, 0, 0);
            let mut coeff = [0i16; 16];
            fdct4x4(&residual, 4, &mut coeff);
            let mut q = [0i16; 16];
            let mut dq = [0i16; 16];
            quantize_fp(&coeff, TxSize::Tx4X4, &qp, &mut q, &mut dq);
            let mut recon = [128u8; 16];
            idct4x4_add(&dq, &mut recon, 4);
            for i in 0..16 {
                let want = 128 + residual[i] as i32;
                let got = recon[i] as i32;
                assert!(
                    (want - got).abs() <= bound,
                    "q{qindex} idx{i}: want {want} got {got}"
                );
            }
        }
    }

    #[test]
    fn roundtrip_error_bounded_8x8() {
        let mut residual = [0i16; 64];
        for (i, r) in residual.iter_mut().enumerate() {
            *r = (((i * 29 + 5) % 61) as i16) - 30;
        }
        let qp = QuantParams::new(0, 0, 0);
        let mut coeff = [0i16; 64];
        fdct8x8(&residual, 8, &mut coeff);
        let mut q = [0i16; 64];
        let mut dq = [0i16; 64];
        quantize_fp(&coeff, TxSize::Tx8X8, &qp, &mut q, &mut dq);
        let mut recon = [100u8; 64];
        idct8x8_add(&dq, &mut recon, 8);
        for i in 0..64 {
            let want = 100 + residual[i] as i32;
            let got = recon[i] as i32;
            assert!((want - got).abs() <= 3, "idx{i}: want {want} got {got}");
        }
    }
}
