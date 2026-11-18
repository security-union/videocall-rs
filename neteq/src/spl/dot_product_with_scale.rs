// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/dot_product_with_scale.cc at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_DotProductWithScale`.

/// C `WebRtcSpl_DotProductWithScale`: sum of `(v1[i] * v2[i]) >> scaling`.
///
/// Each product is an `int` shifted on its own (arithmetic shift of a
/// negative value is implementation-defined in C, arithmetic in practice),
/// accumulated in `int64_t` and saturated to `int32_t` at the end
/// (`saturated_cast`). The C++ 4x unroll does not change the result since
/// each term is shifted before the exact 64-bit sum.
pub fn dot_product_with_scale(
    vector1: &[i16],
    vector2: &[i16],
    length: usize,
    scaling: i32,
) -> i32 {
    let sum: i64 = vector1[..length]
        .iter()
        .zip(&vector2[..length])
        .map(|(&a, &b)| ((a as i32 * b as i32) >> scaling) as i64)
        .sum();
    sum.clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spl::{WORD16_MAX, WORD16_MIN};

    // signal_processing_unittest.cc: SplTest.DotProductWithScaleTest
    #[test]
    fn dot_product_with_scale_test() {
        let vector16: [i16; 9] = [
            1,
            -15511,
            4323,
            1963,
            WORD16_MAX,
            0,
            WORD16_MIN + 5,
            -3333,
            345,
        ];
        assert_eq!(
            605362796,
            dot_product_with_scale(&vector16, &vector16, 9, 2)
        );
    }

    #[test]
    fn saturates_instead_of_wrapping() {
        let v = [i16::MIN; 4];
        // 4 * 2^30 = 2^32 overflows int32; saturated_cast clamps it.
        assert_eq!(i32::MAX, dot_product_with_scale(&v, &v, 4, 0));
        let w = [i16::MAX; 4];
        assert_eq!(i32::MIN, dot_product_with_scale(&v, &w, 4, 0));
    }
}
