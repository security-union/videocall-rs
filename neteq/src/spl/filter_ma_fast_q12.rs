// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/filter_ma_fast_q12.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_FilterMAFastQ12`.

use super::signal_processing_library::spl_sat;

/// C `WebRtcSpl_FilterMAFastQ12`. Reads `in_ptr[i - j]` before the pointer, so
/// `in_vector` is the whole buffer and `in_offset` the index the C pointer
/// points at (at least `ma_coef_length - 1`).
///
/// Products are `int`, accumulated in `int64_t` (so no wrap), saturated to
/// the Q12 range of `int16_t` and rounded with an arithmetic `>> 12`.
pub fn filter_ma_fast_q12(
    in_vector: &[i16],
    in_offset: usize,
    out_vector: &mut [i16],
    ma_coef: &[i16],
    ma_coef_length: usize,
    vector_length: usize,
) {
    for (i, out) in out_vector[..vector_length].iter_mut().enumerate() {
        let mut o: i64 = 0;
        for (j, &b) in ma_coef[..ma_coef_length].iter().enumerate() {
            o += (b as i32 * in_vector[in_offset + i - j] as i32) as i64;
        }
        o = spl_sat(134215679, o, -134217728);
        *out = ((o + 2048) >> 12) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.FilterTest (MA part).
    #[test]
    fn filter_test_ma() {
        let data_in: [i16; 4] = [1, 2, 33, 100];
        let b: [i16; 4] = [4, 12, 133, 110];
        let mut data_out = [0i16; 4];
        filter_ma_fast_q12(&data_in, 3, &mut data_out, &b, 4, 1);
        assert_eq!(0, data_out[0]);
    }

    // SplTest.FilterMAFastQ12SaturatesAfterOverflow
    #[test]
    fn filter_ma_fast_q12_saturates_after_overflow() {
        let coefficients = [32767i16; 3];
        let positive = [32767i16; 3];
        let negative = [-32768i16; 3];
        let mut output = [0i16; 1];
        filter_ma_fast_q12(&positive, 2, &mut output, &coefficients, 3, 1);
        assert_eq!(32767, output[0]);
        filter_ma_fast_q12(&negative, 2, &mut output, &coefficients, 3, 1);
        assert_eq!(-32768, output[0]);
    }
}
