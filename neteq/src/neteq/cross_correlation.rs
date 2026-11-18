// Port target: WebRTC modules/audio_coding/neteq/cross_correlation.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2015 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Cross-correlation with an automatic right shift that avoids overflow.

use crate::spl;

/// C++ `CrossCorrelationWithAutoShift`.
///
/// C reads `sequence_2` at `sequence_2_ptr + i * cross_correlation_step + j`,
/// which goes backwards for a negative step (TimeStretch passes -1). So
/// `sequence_2` is the whole underlying buffer and `sequence_2_offset` is the
/// index the C pointer points at. `sequence_1` starts at the C pointer.
/// Writes `cross_correlation_length` values into `cross_correlation` and
/// returns the right shift applied.
pub fn cross_correlation_with_auto_shift(
    sequence_1: &[i16],
    sequence_2: &[i16],
    sequence_2_offset: usize,
    sequence_1_length: usize,
    cross_correlation_length: usize,
    cross_correlation_step: i32,
    cross_correlation: &mut [i32],
) -> i32 {
    // Find the element that has the maximum absolute value of sequence_1 and 2.
    // Note that these values may be negative.
    let max_1 = spl::max_abs_element_w16(sequence_1, sequence_1_length);
    let sequence_2_shift: i32 = cross_correlation_step * (cross_correlation_length as i32 - 1);
    let sequence_2_start = if sequence_2_shift >= 0 {
        sequence_2_offset
    } else {
        (sequence_2_offset as isize + sequence_2_shift as isize) as usize
    };
    let sequence_2_length = sequence_1_length + sequence_2_shift.unsigned_abs() as usize;
    let max_2 = spl::max_abs_element_w16(&sequence_2[sequence_2_start..], sequence_2_length);

    // In order to avoid overflow when computing the sum we should scale the
    // samples so that (in_vector_length * max_1 * max_2) will not overflow.
    // `abs(max_1 * max_2)` is an `int` (at most 2^30), widened to `int64_t`.
    let max_value: i64 = (max_1 as i32 * max_2 as i32).abs() as i64 * sequence_1_length as i64;
    let factor = (max_value >> 31) as i32;
    let scaling = if factor == 0 {
        0
    } else {
        31 - spl::norm_w32(factor) as i32
    };

    spl::cross_correlation(
        cross_correlation,
        sequence_1,
        sequence_2,
        sequence_2_offset,
        sequence_1_length,
        cross_correlation_length,
        scaling,
        cross_correlation_step,
    );

    scaling
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_inputs_need_no_shift() {
        let seq1 = [1i16, 2, 3];
        let seq2 = [1i16, 1, 1, 1, 1];
        let mut out = [0i32; 3];
        assert_eq!(
            0,
            cross_correlation_with_auto_shift(&seq1, &seq2, 0, 3, 3, 1, &mut out)
        );
        assert_eq!([6, 6, 6], out);
    }

    #[test]
    fn large_inputs_shift_and_negative_step() {
        // 32767 * -32768 * 64 = -2^36 + ... -> factor = 2^36 >> 31 ~ 31,
        // norm_w32(31) = 26, so the shift is 5.
        let seq1 = [32767i16; 64];
        let mut seq2 = vec![0i16; 70];
        seq2[0] = -32768;
        let mut out = [0i32; 4];
        // C pointer at seq2[3] with step -1: windows start at 3, 2, 1, 0.
        let scaling = cross_correlation_with_auto_shift(&seq1, &seq2, 3, 64, 4, -1, &mut out);
        assert_eq!(5, scaling);
        assert_eq!([0, 0, 0, (32767 * -32768) >> 5], out);
    }

    // Golden hash against the real C++ (see spl/golden/golden_gen.cc).
    #[test]
    fn golden_cross_correlation_with_auto_shift() {
        let hash = crate::spl::golden_tests::golden_cross_correlation_hash(true);
        assert_eq!(0xac6a3b431dae17e2, hash);
    }
}
