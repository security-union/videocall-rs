// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/vector_scaling_operations.c (and
// `WebRtcSpl_AffineTransformVector` from vector_operations.c) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Vector shifts, scaling and affine transforms. `_in_place` variants cover C
//! calls where input and output alias.
//!
//! Integer semantics follow C exactly: `int16_t * int16_t` is an `int`
//! product, results stored into `int16_t` without saturation are truncated
//! (`as i16`, implementation-defined in C, modular on every supported
//! compiler), right shifts of negative values are arithmetic, and left
//! shifts that overflow (UB in C) wrap.

use super::spl_inl::sat_w32_to_w16;

/// One element of `WebRtcSpl_VectorBitShiftW16`.
#[inline]
fn bit_shift_w16(x: i16, right_shifts: i16) -> i16 {
    if right_shifts > 0 {
        // C promotes `x` to `int` before shifting, so any shift up to 31 is
        // defined (a right shift of 16 or more yields 0 or -1).
        ((x as i32) >> (right_shifts as u32).min(31)) as i16
    } else {
        // `x * (1 << -right_shifts)` in `int`, truncated to `int16_t`.
        (x as i32).wrapping_mul(1i32.wrapping_shl((-(right_shifts as i32)) as u32)) as i16
    }
}

/// C `WebRtcSpl_VectorBitShiftW16`.
pub fn vector_bit_shift_w16(
    out_vector: &mut [i16],
    vector_length: usize,
    in_vector: &[i16],
    right_shifts: i16,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        *out = bit_shift_w16(x, right_shifts);
    }
}

/// `vector_bit_shift_w16` with `out == in`.
pub fn vector_bit_shift_w16_in_place(vector: &mut [i16], vector_length: usize, right_shifts: i16) {
    for x in vector[..vector_length].iter_mut() {
        *x = bit_shift_w16(*x, right_shifts);
    }
}

/// C `WebRtcSpl_VectorBitShiftW32`.
pub fn vector_bit_shift_w32(
    out_vector: &mut [i32],
    vector_length: usize,
    in_vector: &[i32],
    right_shifts: i16,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        *out = if right_shifts > 0 {
            x >> right_shifts
        } else {
            x.wrapping_shl((-(right_shifts as i32)) as u32)
        };
    }
}

/// C `WebRtcSpl_VectorBitShiftW32ToW16`: shift, then saturate to `int16_t`.
pub fn vector_bit_shift_w32_to_w16(
    out_vector: &mut [i16],
    vector_length: usize,
    in_vector: &[i32],
    right_shifts: i32,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        let tmp_w32 = if right_shifts >= 0 {
            x >> right_shifts
        } else {
            x.wrapping_shl((-right_shifts) as u32)
        };
        *out = sat_w32_to_w16(tmp_w32);
    }
}

/// C `WebRtcSpl_ScaleVector`: `(int16_t)((in * gain) >> right_shifts)`.
pub fn scale_vector(
    in_vector: &[i16],
    out_vector: &mut [i16],
    gain: i16,
    vector_length: usize,
    right_shifts: i16,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        *out = ((x as i32 * gain as i32) >> right_shifts) as i16;
    }
}

/// `scale_vector` with `out == in`.
pub fn scale_vector_in_place(
    vector: &mut [i16],
    gain: i16,
    vector_length: usize,
    right_shifts: i16,
) {
    for x in vector[..vector_length].iter_mut() {
        *x = ((*x as i32 * gain as i32) >> right_shifts) as i16;
    }
}

/// C `WebRtcSpl_ScaleVectorWithSat`.
pub fn scale_vector_with_sat(
    in_vector: &[i16],
    out_vector: &mut [i16],
    gain: i16,
    vector_length: usize,
    right_shifts: i16,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        *out = sat_w32_to_w16((x as i32 * gain as i32) >> right_shifts);
    }
}

/// C `WebRtcSpl_ScaleAndAddVectors`:
/// `(int16_t)((gain1 * in1) >> shift1) + (int16_t)((gain2 * in2) >> shift2)`,
/// the sum computed in `int` and truncated to `int16_t`.
pub fn scale_and_add_vectors(
    in1: &[i16],
    gain1: i16,
    shift1: i32,
    in2: &[i16],
    gain2: i16,
    shift2: i32,
    out: &mut [i16],
    vector_length: usize,
) {
    for i in 0..vector_length {
        let a = ((gain1 as i32 * in1[i] as i32) >> shift1) as i16;
        let b = ((gain2 as i32 * in2[i] as i32) >> shift2) as i16;
        out[i] = (a as i32 + b as i32) as i16;
    }
}

/// C `WebRtcSpl_ScaleAndAddVectorsWithRoundC`. Returns 0 or -1.
///
/// `(1 << right_shifts) >> 1` is evaluated before the argument check in C;
/// a negative shift there is UB but its value is unused because the
/// function returns -1. The `int` sum may overflow (UB in C) and wraps here.
pub fn scale_and_add_vectors_with_round(
    in_vector1: &[i16],
    in_vector1_scale: i16,
    in_vector2: &[i16],
    in_vector2_scale: i16,
    right_shifts: i32,
    out_vector: &mut [i16],
    length: usize,
) -> i32 {
    if length == 0 || right_shifts < 0 {
        return -1;
    }
    let round_value: i32 = 1i32.wrapping_shl(right_shifts as u32) >> 1;
    for i in 0..length {
        let sum = (in_vector1[i] as i32 * in_vector1_scale as i32)
            .wrapping_add(in_vector2[i] as i32 * in_vector2_scale as i32)
            .wrapping_add(round_value);
        out_vector[i] = (sum >> right_shifts) as i16;
    }
    0
}

/// One element of `WebRtcSpl_AffineTransformVector`.
#[inline]
fn affine(x: i16, gain: i16, add_constant: i32, right_shifts: i16) -> i16 {
    sat_w32_to_w16((x as i32 * gain as i32).wrapping_add(add_constant) >> right_shifts)
}

/// C `WebRtcSpl_AffineTransformVector`:
/// `out = sat16((in * gain + add_constant) >> right_shifts)`.
pub fn affine_transform_vector(
    out_vector: &mut [i16],
    in_vector: &[i16],
    gain: i16,
    add_constant: i32,
    right_shifts: i16,
    vector_length: usize,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        *out = affine(x, gain, add_constant, right_shifts);
    }
}

/// `affine_transform_vector` with `out == in`.
pub fn affine_transform_vector_in_place(
    vector: &mut [i16],
    gain: i16,
    add_constant: i32,
    right_shifts: i16,
    vector_length: usize,
) {
    for x in vector[..vector_length].iter_mut() {
        *x = affine(*x, gain, add_constant, right_shifts);
    }
}

/// C `WebRtcSpl_AddAffineVectorToVector`:
/// `out = sat16(out + ((in * gain + add_constant) >> right_shifts))`.
pub fn add_affine_vector_to_vector(
    out_vector: &mut [i16],
    in_vector: &[i16],
    gain: i16,
    add_constant: i32,
    right_shifts: i16,
    vector_length: usize,
) {
    for (out, &x) in out_vector[..vector_length]
        .iter_mut()
        .zip(&in_vector[..vector_length])
    {
        let scaled = (x as i32 * gain as i32).wrapping_add(add_constant) >> right_shifts;
        *out = sat_w32_to_w16((*out as i32).wrapping_add(scaled));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: [i32; 4] = [4, 12, 133, 1100];

    // signal_processing_unittest.cc: SplTest.BasicArrayOperationsTest (shifts).
    #[test]
    fn basic_array_operations_test_shifts() {
        let b32: [i32; 4] = B;
        let b16: [i16; 4] = B.map(|b| b as i16);
        let mut tmp16 = [0i16; 4];
        let mut tmp32 = [0i32; 4];
        vector_bit_shift_w32_to_w16(&mut tmp16, 4, &b32, 1);
        assert_eq!(B.map(|b| (b >> 1) as i16), tmp16);
        vector_bit_shift_w16(&mut tmp16, 4, &b16, 1);
        assert_eq!(B.map(|b| (b >> 1) as i16), tmp16);
        vector_bit_shift_w32(&mut tmp32, 4, &b32, 1);
        assert_eq!(B.map(|b| b >> 1), tmp32);
    }

    /// C shifts the int-promoted sample, so right shifts of 16 or more are
    /// legal and give 0 for non-negative input and -1 for negative input
    /// (expand.cc's `Correlation` shifts by 16 when its input is silent).
    #[test]
    fn vector_bit_shift_w16_right_shift_of_16_or_more() {
        let input: [i16; 4] = [0, 1234, -1, i16::MIN];
        let mut out = [7i16; 4];
        vector_bit_shift_w16(&mut out, 4, &input, 16);
        assert_eq!([0, 0, -1, -1], out);
        let mut in_place = input;
        vector_bit_shift_w16_in_place(&mut in_place, 4, 20);
        assert_eq!([0, 0, -1, -1], in_place);
    }

    // SplTest.AddAffineVectorToVectorSaturates
    #[test]
    fn add_affine_vector_to_vector_saturates() {
        let input: [i16; 6] = [20000, -20000, 20000, -20000, 1, -1];
        let mut output: [i16; 6] = [20000, -20000, -10000, 10000, 1, -1];
        add_affine_vector_to_vector(&mut output, &input, 2, 0, 0, 6);
        assert_eq!([32767, -32768, 30000, -30000, 3, -3], output);
    }

    // SplTest.AffineTransformVectorSaturates
    #[test]
    fn affine_transform_vector_saturates() {
        let mut samples: [i16; 7] = [-32768, -16384, -1, 0, 1, 16384, 32767];
        affine_transform_vector_in_place(&mut samples, -2, 0, 0, 7);
        assert_eq!([32767, 32767, 2, 0, -2, -32768, -32768], samples);

        let mut value = [32767i16];
        affine_transform_vector_in_place(&mut value, 1, 1, 0, 1);
        assert_eq!(32767, value[0]);
        value[0] = -32768;
        affine_transform_vector_in_place(&mut value, 1, -1, 0, 1);
        assert_eq!(-32768, value[0]);
    }

    // SplTest.AffineTransformVectorRoundsAndShifts
    #[test]
    fn affine_transform_vector_rounds_and_shifts() {
        let input: [i16; 9] = [-32768, -3, -2, -1, 0, 1, 2, 3, 32767];
        let mut output = [0i16; 9];
        affine_transform_vector(&mut output, &input, 3, 2, 2, 9);
        assert_eq!([-24576, -2, -1, -1, 0, 1, 2, 2, 24575], output);
    }

    // SplTest.VectorOperationsTest (the scaling functions).
    #[test]
    fn vector_operations_test() {
        let a16: [i16; 4] = B.map(|b| b as i16);
        let b16: [i16; 4] = a16;
        let mut tmp16 = [0i16; 4];

        affine_transform_vector(&mut tmp16, &b16, 3, 7, 2, 4);
        assert_eq!(B.map(|b| ((b * 3 + 7) >> 2) as i16), tmp16);

        assert_eq!(
            0,
            scale_and_add_vectors_with_round(&b16, 3, &b16, 2, 2, &mut tmp16, 4)
        );
        assert_eq!(B.map(|b| ((b * 3 + b * 2 + 2) >> 2) as i16), tmp16);

        add_affine_vector_to_vector(&mut tmp16, &b16, 3, 7, 2, 4);
        assert_eq!(
            B.map(|b| (((b * 3 + b * 2 + 2) >> 2) + ((b * 3 + 7) >> 2)) as i16),
            tmp16
        );

        scale_vector(&b16, &mut tmp16, 13, 4, 2);
        assert_eq!(B.map(|b| ((b * 13) >> 2) as i16), tmp16);

        scale_vector_with_sat(&b16, &mut tmp16, 13, 4, 2);
        assert_eq!(B.map(|b| ((b * 13) >> 2) as i16), tmp16);

        scale_and_add_vectors(&a16, 13, 2, &b16, 7, 2, &mut tmp16, 4);
        assert_eq!(B.map(|b| (((b * 13) >> 2) + ((b * 7) >> 2)) as i16), tmp16);
    }

    #[test]
    fn truncation_and_negative_shift_semantics() {
        // ScaleVector truncates, it does not saturate.
        let mut out = [0i16; 1];
        scale_vector(&[32767], &mut out, 4, 1, 0);
        assert_eq!((32767i32 * 4) as i16, out[0]);
        // VectorBitShiftW16 with a negative shift multiplies and truncates.
        vector_bit_shift_w16(&mut out, 1, &[20000], -1);
        assert_eq!(40000i32 as i16, out[0]);
        // Arithmetic right shift of negatives.
        vector_bit_shift_w16(&mut out, 1, &[-3], 1);
        assert_eq!(-2, out[0]);
        vector_bit_shift_w32_to_w16(&mut out, 1, &[70000], -1);
        assert_eq!(32767, out[0]);
        assert_eq!(
            -1,
            scale_and_add_vectors_with_round(&[1], 1, &[1], 1, -1, &mut out, 1)
        );
        assert_eq!(
            -1,
            scale_and_add_vectors_with_round(&[1], 1, &[1], 1, 1, &mut out, 0)
        );
    }
}
