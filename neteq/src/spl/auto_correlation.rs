// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/auto_correlation.cc at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_AutoCorrelation` (CNG encoder, test support only).

use super::min_max_operations::max_abs_value_w16;
use super::spl_inl::{get_size_in_bits, norm_w32};

/// C `WebRtcSpl_AutoCorrelation`. Returns `order + 1`.
///
/// The `int32_t` sum may overflow (UB in C, wraps in practice); wrapping
/// addition is associative, so the C++ 4x unroll gives the same result as
/// this plain loop. Product shifts are arithmetic.
pub fn auto_correlation(
    in_vector: &[i16],
    in_vector_length: usize,
    order: usize,
    result: &mut [i32],
    scale: &mut i32,
) -> usize {
    debug_assert!(order <= in_vector_length);

    let smax = max_abs_value_w16(in_vector, in_vector_length);
    let scaling: i32 = if smax == 0 {
        0
    } else {
        let nbits = get_size_in_bits(in_vector_length as u32) as i32;
        let t = norm_w32(smax as i32 * smax as i32) as i32;
        if t > nbits {
            0
        } else {
            nbits - t
        }
    };

    for i in 0..=order {
        let mut sum: i32 = 0;
        for j in 0..in_vector_length - i {
            sum = sum.wrapping_add((in_vector[j] as i32 * in_vector[i + j] as i32) >> scaling);
        }
        result[i] = sum;
    }

    *scale = scaling;
    order + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spl::{WORD16_MAX, WORD16_MIN};

    // signal_processing_unittest.cc: SplTest.AutoCorrelationTest
    #[test]
    fn auto_correlation_test() {
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
        let expected: [i32; 9] = [
            302681398, 14223410, -121705063, -85221647, -17104971, 61806945, 6644603, -669329, 43,
        ];
        let mut vector32 = [0i32; 9];
        let mut scale = 0;
        assert_eq!(
            9,
            auto_correlation(&vector16, 9, 8, &mut vector32, &mut scale)
        );
        assert_eq!(3, scale);
        assert_eq!(expected, vector32);
    }
}
