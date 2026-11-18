// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/energy.c and get_scaling_square.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_Energy` and `WebRtcSpl_GetScalingSquare` (CNG encoder, test
//! support only).

use super::spl_inl::{get_size_in_bits, norm_w32};

/// C `WebRtcSpl_GetScalingSquare`: right shift needed so that summing
/// `times` squares of the largest sample cannot overflow.
///
/// C computes `sabs = (x > 0 ? x : -x)` into an `int16_t`, so `-(-32768)`
/// truncates back to -32768 (implementation-defined conversion, modular in
/// practice). The port keeps that quirk.
pub fn get_scaling_square(in_vector: &[i16], in_vector_length: usize, times: usize) -> i16 {
    let nbits = get_size_in_bits(times as u32);
    let mut smax: i16 = -1;
    for &x in &in_vector[..in_vector_length] {
        let sabs: i16 = if x > 0 { x } else { (-(x as i32)) as i16 };
        smax = if sabs > smax { sabs } else { smax };
    }
    let t = norm_w32(smax as i32 * smax as i32);
    // C: `smax == 0 ? 0 : (t > nbits) ? 0 : nbits - t`.
    if smax == 0 || t > nbits {
        0
    } else {
        nbits - t
    }
}

/// C `WebRtcSpl_Energy`: sum of `(x * x) >> scale` with the scale from
/// `get_scaling_square`; the `int32_t` sum wraps like C in practice.
pub fn energy(vector: &[i16], vector_length: usize, scale_factor: &mut i32) -> i32 {
    let scaling = get_scaling_square(vector, vector_length, vector_length) as i32;
    let mut en: i32 = 0;
    for &x in &vector[..vector_length] {
        en = en.wrapping_add((x as i32 * x as i32) >> scaling);
    }
    *scale_factor = scaling;
    en
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.SignalProcessingTest (Energy) and
    // SplTest.VectorOperationsTest (GetScalingSquare).
    #[test]
    fn energy_test() {
        let b16: [i16; 4] = [1, 2, 33, 100];
        let mut scale = 0;
        assert_eq!(11094, energy(&b16, 4, &mut scale));
        assert_eq!(0, scale);
        assert_eq!(0, get_scaling_square(&[4, 12, 133, 1100], 4, 1));
    }
}
