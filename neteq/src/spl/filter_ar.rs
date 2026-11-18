// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/filter_ar.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_FilterAR` (used by the CNG decoder).

use super::copy_set_operations::copy_from_end_w16;

/// C `WebRtcSpl_FilterAR`: AR filter in Q12 with a high/low split state.
/// Returns `in_vector_length`.
///
/// Integer semantics as in C: the high accumulator `o` is `int64_t`, the low
/// accumulator `oLOW` is `int32_t` (overflow is UB in C, wraps here), shifts
/// of negative values are arithmetic, and the stores into `int16_t` truncate.
/// The state save copies `ar_coef_length - 1` samples when the input is at
/// least `filter_state_length` long, exactly like C.
pub fn filter_ar(
    ar_coef: &[i16],
    ar_coef_length: usize,
    in_vector: &[i16],
    in_vector_length: usize,
    filter_state: &mut [i16],
    filter_state_length: usize,
    filter_state_low: &mut [i16],
    out_vector: &mut [i16],
    out_vector_low: &mut [i16],
) -> usize {
    let a = ar_coef;
    let a_length = ar_coef_length;
    let state_length = filter_state_length;

    for i in 0..in_vector_length {
        let mut a_index = 1usize;
        // C keeps this as `int` because it may go negative; it is never used
        // to index while negative.
        let mut filtered_ix: isize = i as isize - 1;
        let mut state_index = state_length as isize - 1;

        let mut o: i64 = (in_vector[i] as i32 * (1 << 12)) as i64;
        let mut o_low: i32 = 0;

        let stop = if i < a_length { i + 1 } else { a_length };
        for _ in 1..stop {
            debug_assert!(filtered_ix >= 0);
            let coef = a[a_index] as i32;
            o -= (coef * out_vector[filtered_ix as usize] as i32) as i64;
            o_low = o_low.wrapping_sub(coef * out_vector_low[filtered_ix as usize] as i32);
            a_index += 1;
            filtered_ix -= 1;
        }
        for _ in (i + 1)..a_length {
            let coef = a[a_index] as i32;
            o -= (coef * filter_state[state_index as usize] as i32) as i64;
            o_low = o_low.wrapping_sub(coef * filter_state_low[state_index as usize] as i32);
            a_index += 1;
            state_index -= 1;
        }

        o += (o_low >> 12) as i64;
        let filtered = ((o + 2048) >> 12) as i16;
        out_vector[i] = filtered;
        out_vector_low[i] = (o - (filtered as i32 * (1 << 12)) as i64) as i16;
    }

    // Save the filter state.
    if in_vector_length >= state_length {
        copy_from_end_w16(out_vector, in_vector_length, a_length - 1, filter_state);
        copy_from_end_w16(
            out_vector_low,
            in_vector_length,
            a_length - 1,
            filter_state_low,
        );
    } else {
        for i in 0..state_length - in_vector_length {
            filter_state[i] = filter_state[i + in_vector_length];
            filter_state_low[i] = filter_state_low[i + in_vector_length];
        }
        for i in 0..in_vector_length {
            filter_state[state_length - in_vector_length + i] = out_vector[i];
            filter_state_low[state_length - in_vector_length + i] = out_vector_low[i];
        }
    }

    in_vector_length
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.FilterTest (FilterAR part).
    #[test]
    fn filter_test_filter_ar() {
        let a5: [i16; 5] = [1, 2, 33, 100, -5];
        let data_in: [i16; 4] = [1, 2, 33, 100];
        let mut state = [0i16; 4];
        let mut state_low = [0i16; 4];
        let mut data_out = [0i16; 4];
        let mut out_low = [0i16; 4];
        assert_eq!(
            4,
            filter_ar(
                &a5,
                5,
                &data_in,
                4,
                &mut state,
                4,
                &mut state_low,
                &mut data_out,
                &mut out_low
            )
        );
    }

    #[test]
    fn unity_filter_is_identity_and_keeps_state() {
        let a: [i16; 3] = [4096, 0, 0];
        let data_in: [i16; 5] = [1, -2, 300, -32768, 32767];
        let mut state = [0i16; 2];
        let mut state_low = [0i16; 2];
        let mut out = [0i16; 5];
        let mut out_low = [0i16; 5];
        filter_ar(
            &a,
            3,
            &data_in,
            5,
            &mut state,
            2,
            &mut state_low,
            &mut out,
            &mut out_low,
        );
        assert_eq!(data_in, out);
        assert_eq!([0; 5], out_low);
        assert_eq!([-32768, 32767], state);
    }
}
