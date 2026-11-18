// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/vector_operations.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_ElementwiseVectorMult` (CNG encoder, test support only).
//! `WebRtcSpl_AffineTransformVector` and `AddAffineVectorToVector` from the
//! same C file live in `vector_scaling_operations.rs`.

/// C `WebRtcSpl_ElementwiseVectorMult`:
/// `out = (int16_t)((in * window) >> right_shifts)` (truncating store,
/// arithmetic shift).
pub fn elementwise_vector_mult(
    out_vector: &mut [i16],
    in_vector: &[i16],
    window: &[i16],
    vector_length: usize,
    right_shifts: i16,
) {
    for i in 0..vector_length {
        out_vector[i] = ((in_vector[i] as i32 * window[i] as i32) >> right_shifts) as i16;
    }
}

/// `elementwise_vector_mult` with `out == in`.
pub fn elementwise_vector_mult_in_place(
    vector: &mut [i16],
    window: &[i16],
    vector_length: usize,
    right_shifts: i16,
) {
    for i in 0..vector_length {
        vector[i] = ((vector[i] as i32 * window[i] as i32) >> right_shifts) as i16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.VectorOperationsTest
    // (ElementwiseVectorMult).
    #[test]
    fn elementwise_vector_mult_test() {
        let a16: [i16; 4] = [4, 12, 133, 1100];
        let b16 = a16;
        let mut tmp16 = [0i16; 4];
        elementwise_vector_mult(&mut tmp16, &a16, &b16, 4, 6);
        assert_eq!(a16.map(|a| ((a as i32 * a as i32) >> 6) as i16), tmp16);
        let mut in_place = a16;
        elementwise_vector_mult_in_place(&mut in_place, &b16, 4, 6);
        assert_eq!(tmp16, in_place);
    }
}
