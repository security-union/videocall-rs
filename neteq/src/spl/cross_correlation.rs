// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/cross_correlation.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_CrossCorrelationC`.

/// C `WebRtcSpl_CrossCorrelationC`. Shifts every product (not the NEON
/// 64-bit accumulate). `seq2` is the whole buffer and `seq2_offset` the index
/// the C pointer points at, because a negative `step_seq2` reads backwards.
///
/// C accumulates `corr += (seq1[j] * seq2[j]) >> right_shifts` in `int32_t`;
/// signed overflow there is UB and wraps in practice, so this wraps. The
/// right shift of a negative product is arithmetic (implementation-defined in
/// C, arithmetic on every supported compiler).
pub fn cross_correlation(
    cross_correlation: &mut [i32],
    seq1: &[i16],
    seq2: &[i16],
    seq2_offset: usize,
    dim_seq: usize,
    dim_cross_correlation: usize,
    right_shifts: i32,
    step_seq2: i32,
) {
    let mut seq2_index = seq2_offset as isize;
    for out in cross_correlation[..dim_cross_correlation].iter_mut() {
        let base = seq2_index as usize;
        let seq2_window = &seq2[base..base + dim_seq];
        let mut corr: i32 = 0;
        for (&a, &b) in seq1[..dim_seq].iter().zip(seq2_window) {
            corr = corr.wrapping_add((a as i32 * b as i32) >> right_shifts);
        }
        seq2_index += step_seq2 as isize;
        *out = corr;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spl::{WORD16_MAX, WORD16_MIN};

    const VECTOR16: [i16; 9] = [
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

    // signal_processing_unittest.cc: SplTest.CrossCorrelationTest (the
    // non-NEON expectation).
    #[test]
    fn cross_correlation_test() {
        let k_vector16: [i16; 9] = [
            1,
            4323,
            1963,
            WORD16_MAX,
            WORD16_MIN + 5,
            -3333,
            -876,
            8483,
            142,
        ];
        let mut vector32 = [0i32; 3];
        cross_correlation(&mut vector32, &VECTOR16, &k_vector16, 0, 6, 3, 2, 1);
        assert_eq!([-266947903, -15579555, -171282001], vector32);
    }

    #[test]
    fn negative_step_reads_backwards() {
        let seq1 = [1i16, 2];
        let seq2 = [10i16, 20, 30, 40];
        let mut out = [0i32; 3];
        // C pointer at seq2[2], step -1: windows start at 2, 1, 0.
        cross_correlation(&mut out, &seq1, &seq2, 2, 2, 3, 0, -1);
        assert_eq!([30 + 80, 20 + 60, 10 + 40], out);
    }
}
