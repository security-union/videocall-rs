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

//! SIMD-accelerated sum of absolute differences (SAD) for integer-pel motion
//! search.
//!
//! [`sad`] returns `sum(|src[y][x] - ref[y][x]|)` over a `w`x`h` block — the
//! hottest inner loop of [`crate::vp9::enc::mcomp`]. The result is an **exact**
//! integer sum, so the SIMD kernels are bit-identical to the scalar reference:
//! the same motion vector is selected and the encoded bitstream is byte-for-byte
//! unchanged.
//!
//! Dispatch is per-target:
//! - **aarch64:** NEON (baseline on aarch64, no runtime detection needed).
//! - **x86_64:** SSE2 baseline (always available on x86_64) with an AVX2 path
//!   chosen once at runtime and cached.
//! - **everything else** (incl. `wasm32`): the scalar reference loop.
//!
//! All kernels accept an arbitrary `w`/`h`; widths that are not a multiple of
//! the SIMD lane width fall back to narrower loads and finally a scalar tail, so
//! any block size is handled correctly. In practice the encoder only calls this
//! with 16x16 and 8x8 luma blocks.

/// Scalar reference: `sum(|src - ref|)` over a `w`x`h` block. Kept as the exact
/// oracle the SIMD kernels must match, and as the fallback on architectures
/// without a specialized path.
#[inline]
#[allow(clippy::too_many_arguments)]
fn sad_scalar(
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    reference: &[u8],
    ref_off: usize,
    ref_stride: usize,
    w: usize,
    h: usize,
) -> u32 {
    let mut src_p = src_off;
    let mut ref_p = ref_off;
    let mut sad = 0u32;
    for _ in 0..h {
        for c in 0..w {
            let s = src[src_p + c] as i32;
            let r = reference[ref_p + c] as i32;
            sad += (s - r).unsigned_abs();
        }
        src_p += src_stride;
        ref_p += ref_stride;
    }
    sad
}

/// Sum of absolute differences between two `w`x`h` 8-bit blocks.
///
/// `src_off`/`ref_off` are the byte offset of each block's top-left sample
/// within its backing slice; `src_stride`/`ref_stride` are the row pitch. The
/// caller must guarantee every sampled byte is in bounds, i.e.
/// `off + (h - 1) * stride + w <= slice.len()` for both planes (the encoder's
/// reference plane is border-extended so motion-search reads always are).
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn sad(
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    reference: &[u8],
    ref_off: usize,
    ref_stride: usize,
    w: usize,
    h: usize,
) -> u32 {
    // Bounds the unsafe SIMD kernels rely on for soundness. Cheap to check in
    // debug; the release motion-search hot path skips it.
    debug_assert!(w == 0 || h == 0 || src_off + (h - 1) * src_stride + w <= src.len());
    debug_assert!(w == 0 || h == 0 || ref_off + (h - 1) * ref_stride + w <= reference.len());
    dispatch(
        src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
    )
}

/// Selects the per-target kernel. Each `cfg` variant is a single tail
/// expression so exactly one is compiled.
#[cfg(target_arch = "aarch64")]
#[inline]
#[allow(clippy::too_many_arguments)]
fn dispatch(
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    reference: &[u8],
    ref_off: usize,
    ref_stride: usize,
    w: usize,
    h: usize,
) -> u32 {
    // SAFETY: NEON is guaranteed on aarch64, and the debug_asserts in `sad`
    // document the in-bounds precondition the kernel dereferences under.
    unsafe {
        aarch64::sad_neon(
            src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
        )
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[allow(clippy::too_many_arguments)]
fn dispatch(
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    reference: &[u8],
    ref_off: usize,
    ref_stride: usize,
    w: usize,
    h: usize,
) -> u32 {
    if x86::avx2_available() {
        // SAFETY: guarded by the cached runtime AVX2 feature check; in-bounds
        // per the debug_asserts in `sad`.
        unsafe {
            x86::sad_avx2(
                src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
            )
        }
    } else {
        // SAFETY: SSE2 is part of the x86_64 baseline; in-bounds as above.
        unsafe {
            x86::sad_sse2(
                src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
            )
        }
    }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
#[inline]
#[allow(clippy::too_many_arguments)]
fn dispatch(
    src: &[u8],
    src_off: usize,
    src_stride: usize,
    reference: &[u8],
    ref_off: usize,
    ref_stride: usize,
    w: usize,
    h: usize,
) -> u32 {
    sad_scalar(
        src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
    )
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::*;

    /// NEON SAD. Processes columns in 16- then 8-wide chunks with a scalar tail,
    /// accumulating absolute differences pairwise into u16 lanes. With the
    /// encoder's block heights (<= 16) each u16 lane stays well below 65535
    /// (`h * 2 * 255`), so no intermediate widening is required.
    ///
    /// # Safety
    /// Every sampled byte must be in bounds: for both planes,
    /// `off + (h - 1) * stride + w <= slice.len()`.
    #[target_feature(enable = "neon")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn sad_neon(
        src: &[u8],
        src_off: usize,
        src_stride: usize,
        reference: &[u8],
        ref_off: usize,
        ref_stride: usize,
        w: usize,
        h: usize,
    ) -> u32 {
        let src_ptr = src.as_ptr();
        let ref_ptr = reference.as_ptr();
        let mut acc16 = vdupq_n_u16(0);
        let mut tail: u32 = 0;

        let mut src_p = src_off;
        let mut ref_p = ref_off;
        for _ in 0..h {
            let mut c = 0;
            // 16 bytes per step.
            while c + 16 <= w {
                let s = vld1q_u8(src_ptr.add(src_p + c));
                let r = vld1q_u8(ref_ptr.add(ref_p + c));
                acc16 = vpadalq_u8(acc16, vabdq_u8(s, r));
                c += 16;
            }
            // 8 bytes.
            if c + 8 <= w {
                let s = vld1_u8(src_ptr.add(src_p + c));
                let r = vld1_u8(ref_ptr.add(ref_p + c));
                // Widen the 8-lane pairwise sum into the low half of acc16.
                let d = vabd_u8(s, r);
                acc16 = vaddq_u16(acc16, vmovl_u8(d));
                c += 8;
            }
            // Scalar remainder (only for widths that are not a multiple of 8).
            while c < w {
                let s = *src_ptr.add(src_p + c) as i32;
                let r = *ref_ptr.add(ref_p + c) as i32;
                tail += (s - r).unsigned_abs();
                c += 1;
            }
            src_p += src_stride;
            ref_p += ref_stride;
        }
        // Horizontal sum of all u16 lanes into a u32.
        vaddlvq_u16(acc16) + tail
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Cached AVX2 detection: 0 = unknown, 1 = present, 2 = absent.
    static AVX2: AtomicU8 = AtomicU8::new(0);

    /// Whether AVX2 is available, detected once and cached.
    #[inline]
    pub fn avx2_available() -> bool {
        match AVX2.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => {
                let has = is_x86_feature_detected!("avx2");
                AVX2.store(if has { 1 } else { 2 }, Ordering::Relaxed);
                has
            }
        }
    }

    /// Horizontal sum of the four u64 lanes of a 256-bit accumulator.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn hsum_epi64x4(v: __m256i) -> u32 {
        let lo = _mm256_castsi256_si128(v);
        let hi = _mm256_extracti128_si256(v, 1);
        let s = _mm_add_epi64(lo, hi);
        let s = _mm_add_epi64(s, _mm_unpackhi_epi64(s, s));
        _mm_cvtsi128_si32(s) as u32
    }

    /// Horizontal sum of the two u64 lanes of a 128-bit accumulator.
    #[inline]
    #[target_feature(enable = "sse2")]
    unsafe fn hsum_epi64x2(v: __m128i) -> u32 {
        let s = _mm_add_epi64(v, _mm_unpackhi_epi64(v, v));
        _mm_cvtsi128_si32(s) as u32
    }

    /// SSE2 SAD. 16- then 8-wide column chunks via `_mm_sad_epu8`, scalar tail.
    ///
    /// # Safety
    /// Every sampled byte must be in bounds (see [`super::sad`]).
    #[target_feature(enable = "sse2")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn sad_sse2(
        src: &[u8],
        src_off: usize,
        src_stride: usize,
        reference: &[u8],
        ref_off: usize,
        ref_stride: usize,
        w: usize,
        h: usize,
    ) -> u32 {
        let src_ptr = src.as_ptr();
        let ref_ptr = reference.as_ptr();
        let mut acc = _mm_setzero_si128();
        let mut tail: u32 = 0;

        let mut src_p = src_off;
        let mut ref_p = ref_off;
        for _ in 0..h {
            let mut c = 0;
            while c + 16 <= w {
                let s = _mm_loadu_si128(src_ptr.add(src_p + c) as *const __m128i);
                let r = _mm_loadu_si128(ref_ptr.add(ref_p + c) as *const __m128i);
                acc = _mm_add_epi64(acc, _mm_sad_epu8(s, r));
                c += 16;
            }
            if c + 8 <= w {
                let s = _mm_loadl_epi64(src_ptr.add(src_p + c) as *const __m128i);
                let r = _mm_loadl_epi64(ref_ptr.add(ref_p + c) as *const __m128i);
                acc = _mm_add_epi64(acc, _mm_sad_epu8(s, r));
                c += 8;
            }
            while c < w {
                let s = *src_ptr.add(src_p + c) as i32;
                let r = *ref_ptr.add(ref_p + c) as i32;
                tail += (s - r).unsigned_abs();
                c += 1;
            }
            src_p += src_stride;
            ref_p += ref_stride;
        }
        hsum_epi64x2(acc) + tail
    }

    /// AVX2 SAD. For 16-wide blocks it folds two rows into one 256-bit
    /// `_mm256_sad_epu8`; other widths and the odd final row use the SSE2 kernel.
    ///
    /// # Safety
    /// Every sampled byte must be in bounds (see [`super::sad`]). AVX2 must be
    /// available (guaranteed by the [`avx2_available`] guard at the call site).
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn sad_avx2(
        src: &[u8],
        src_off: usize,
        src_stride: usize,
        reference: &[u8],
        ref_off: usize,
        ref_stride: usize,
        w: usize,
        h: usize,
    ) -> u32 {
        // Two-rows-at-a-time only pays off when a row is exactly one 128-bit
        // load; for anything else the SSE2 kernel is already optimal.
        if w != 16 || h < 2 {
            return sad_sse2(
                src, src_off, src_stride, reference, ref_off, ref_stride, w, h,
            );
        }

        let src_ptr = src.as_ptr();
        let ref_ptr = reference.as_ptr();
        let mut acc = _mm256_setzero_si256();

        let pairs = h / 2;
        let mut src_p = src_off;
        let mut ref_p = ref_off;
        for _ in 0..pairs {
            let s0 = _mm_loadu_si128(src_ptr.add(src_p) as *const __m128i);
            let s1 = _mm_loadu_si128(src_ptr.add(src_p + src_stride) as *const __m128i);
            let r0 = _mm_loadu_si128(ref_ptr.add(ref_p) as *const __m128i);
            let r1 = _mm_loadu_si128(ref_ptr.add(ref_p + ref_stride) as *const __m128i);
            let s = _mm256_set_m128i(s1, s0);
            let r = _mm256_set_m128i(r1, r0);
            acc = _mm256_add_epi64(acc, _mm256_sad_epu8(s, r));
            src_p += 2 * src_stride;
            ref_p += 2 * ref_stride;
        }

        let mut total = hsum_epi64x4(acc);
        // Odd trailing row, if any.
        if h & 1 == 1 {
            total += sad_sse2(src, src_p, src_stride, reference, ref_p, ref_stride, w, 1);
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn byte(&mut self) -> u8 {
            (self.next() & 0xff) as u8
        }
    }

    /// SIMD `sad` must equal the scalar oracle for every supported block size
    /// over many pseudo-random block pairs, including all-0 / all-255 edges.
    #[test]
    fn simd_matches_scalar_all_sizes() {
        let mut rng = XorShift32(0x1234_5678);
        // 16x16 and 8x8 are the encoder's real sizes; the others exercise the
        // narrower-load and scalar-tail paths of the kernels.
        let sizes = [
            (16usize, 16usize),
            (8, 8),
            (16, 8),
            (8, 16),
            (13, 7),
            (16, 1),
        ];

        for &(w, h) in &sizes {
            // Use padded strides so a wrong stride read would be caught, and so
            // the two planes have independent geometry.
            let src_stride = w + 5;
            let ref_stride = w + 11;
            let src_off = 3;
            let ref_off = 7;
            let src_len = src_off + h * src_stride + 16;
            let ref_len = ref_off + h * ref_stride + 16;

            for trial in 0..64 {
                let mut src = vec![0u8; src_len];
                let mut reference = vec![0u8; ref_len];
                match trial {
                    // Edge cases: min/max saturation in both directions.
                    0 => { /* all zeros */ }
                    1 => reference.iter_mut().for_each(|b| *b = 255),
                    2 => src.iter_mut().for_each(|b| *b = 255),
                    3 => {
                        src.iter_mut().for_each(|b| *b = 255);
                        reference.iter_mut().for_each(|b| *b = 255);
                    }
                    _ => {
                        src.iter_mut().for_each(|b| *b = rng.byte());
                        reference.iter_mut().for_each(|b| *b = rng.byte());
                    }
                }

                let expected = sad_scalar(
                    &src, src_off, src_stride, &reference, ref_off, ref_stride, w, h,
                );
                let got = sad(
                    &src, src_off, src_stride, &reference, ref_off, ref_stride, w, h,
                );
                assert_eq!(
                    got, expected,
                    "mismatch for {w}x{h} trial {trial}: simd={got} scalar={expected}"
                );
            }
        }
    }

    /// A known-value spot check independent of the scalar oracle.
    #[test]
    fn sad_known_value() {
        // 8x8: src all 10, ref all 3 -> |10-3| * 64 = 448.
        let w = 8;
        let h = 8;
        let src = vec![10u8; w * h];
        let reference = vec![3u8; w * h];
        assert_eq!(sad(&src, 0, w, &reference, 0, w, w, h), 448);
    }
}
