// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/filter_ar_fast_q12.c (C variant, not _armv7.S) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_FilterARFastQ12`.

use super::signal_processing_library::spl_sat;

/// C `WebRtcSpl_FilterARFastQ12`. The filter reads past outputs at
/// `data_out_ptr[i - j]`, i.e. state stored before the C pointer, so
/// `data_out` is the whole buffer and `data_out_offset` the index the C
/// pointer points at (at least `coefficients_length - 1`).
///
/// Products are `int`, accumulated in `int64_t`, saturated to the Q12 range
/// of `int16_t` and rounded with an arithmetic `>> 12`.
pub fn filter_ar_fast_q12(
    data_in: &[i16],
    data_out: &mut [i16],
    data_out_offset: usize,
    coefficients: &[i16],
    coefficients_length: usize,
    data_length: usize,
) {
    debug_assert!(data_length > 0);
    debug_assert!(coefficients_length > 1);

    for i in 0..data_length {
        let mut sum: i64 = 0;
        for j in (1..coefficients_length).rev() {
            let past = data_out[data_out_offset + i - j];
            sum += (coefficients[j] as i32 * past as i32) as i64;
        }
        let mut output: i64 = (coefficients[0] as i32 * data_in[i] as i32) as i64;
        output -= sum;
        output = spl_sat(134215679, output, -134217728);
        data_out[data_out_offset + i] = ((output + 2048) >> 12) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.FilterTest (AR part).
    #[test]
    fn filter_test_ar() {
        let a: [i16; 4] = [1, 2, 33, 100];
        let data_in = a;
        let mut data_out = [0i16; 4];
        filter_ar_fast_q12(&data_in, &mut data_out, 3, &a, 4, 1);
        assert_eq!(0, data_out[3]);
    }

    #[test]
    fn recursion_and_saturation() {
        // y[n] = x[n] - 0.5 * y[n-1] with x in Q0 and coefficients in Q12.
        let coefficients = [4096i16, 2048];
        let data_in = [1000i16, 0, 0, 32767];
        let mut data_out = [0i16; 5];
        filter_ar_fast_q12(&data_in, &mut data_out, 1, &coefficients, 2, 4);
        // 1000; -500; 250; 32767 - 125 = 32642 < 32767.5 -> 32642
        assert_eq!([0, 1000, -500, 250, 32642], data_out);
        let mut data_out = [-32768i16, 0];
        filter_ar_fast_q12(&[32767], &mut data_out, 1, &coefficients, 2, 1);
        // 32767 + 16384 saturates to 134215679 (Q12) -> 32767.
        assert_eq!(32767, data_out[1]);
    }
}
