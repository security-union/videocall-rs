// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/downsample_fast.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_DownsampleFastC`.

use super::spl_inl::sat_w32_to_w16;

/// C `WebRtcSpl_DownsampleFastC`. The filter reads `data_in_ptr[i - j]`, before
/// the pointer, so `data_in` is the whole buffer and `data_in_offset` the
/// index the C pointer points at. Returns 0 or -1.
///
/// `endpos` is `size_t` arithmetic in C (`factor` converts to `size_t`), so
/// it wraps here too. The Q12 accumulator is `int32_t`; overflow is UB in C
/// and wraps in practice, so it wraps here. `out_s32 >>= 12` is an arithmetic
/// shift (implementation-defined in C).
pub fn downsample_fast(
    data_in: &[i16],
    data_in_offset: usize,
    data_in_length: usize,
    data_out: &mut [i16],
    data_out_length: usize,
    coefficients: &[i16],
    coefficients_length: usize,
    factor: i32,
    delay: usize,
) -> i32 {
    let step = factor as isize as usize;
    let endpos = delay
        .wrapping_add(step.wrapping_mul(data_out_length.wrapping_sub(1)))
        .wrapping_add(1);

    if data_out_length == 0 || coefficients_length == 0 || data_in_length < endpos {
        return -1;
    }

    let mut out_index = 0usize;
    let mut i = delay;
    while i < endpos {
        let mut out_s32: i32 = 2048; // Round value, 0.5 in Q12.
        for (j, &coefficient) in coefficients[..coefficients_length].iter().enumerate() {
            // `i - j` may go below the C pointer ("negative" positions hold
            // the filter state), hence the offset.
            let sample = data_in[data_in_offset + i - j];
            out_s32 = out_s32.wrapping_add(coefficient as i32 * sample as i32);
        }
        out_s32 >>= 12; // Q0.
        data_out[out_index] = sat_w32_to_w16(out_s32);
        out_index += 1;
        i = i.wrapping_add(step);
    }
    debug_assert_eq!(out_index, data_out_length);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_arguments() {
        let input = [0i16; 8];
        let mut out = [0i16; 4];
        assert_eq!(
            -1,
            downsample_fast(&input, 0, 8, &mut out, 0, &[1], 1, 2, 0)
        );
        assert_eq!(
            -1,
            downsample_fast(&input, 0, 8, &mut out, 4, &[1], 0, 2, 0)
        );
        // endpos = 1 + 2 * 3 + 1 = 8 <= 8 is fine; delay 2 needs 9.
        assert_eq!(0, downsample_fast(&input, 0, 8, &mut out, 4, &[1], 1, 2, 1));
        assert_eq!(
            -1,
            downsample_fast(&input, 0, 8, &mut out, 4, &[1], 1, 2, 2)
        );
    }

    #[test]
    fn filters_rounds_and_saturates() {
        // Two taps of 0.5 (2048 in Q12) over state before the pointer.
        let input = [100i16, 300, 32767, 32767, -7, -8];
        let mut out = [0i16; 3];
        let coefficients = [2048i16, 2048];
        assert_eq!(
            0,
            downsample_fast(&input, 1, 5, &mut out, 3, &coefficients, 2, 2, 0)
        );
        // i = 0: (2048*300 + 2048*100 + 2048) >> 12 = 200
        // i = 2: (2048*32767*2 + 2048) >> 12 = 32767
        // i = 4: (2048*-8 + 2048*-7 + 2048) >> 12 = floor(-7) = -7
        assert_eq!([200, 32767, -7], out);
        let input = [32767i16, 32767, 32767];
        let mut out = [0i16; 1];
        assert_eq!(
            0,
            downsample_fast(&input, 2, 1, &mut out, 1, &[4096, 4096, 4096], 3, 1, 0)
        );
        assert_eq!([32767], out);
    }
}
