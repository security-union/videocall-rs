// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/include/spl_inl.h (+ spl_inl.c) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Inline helpers: normalization shifts, saturation, leading zeros.
//!
//! On arm64 and x86 neither `WEBRTC_ARCH_ARM_V7` nor `MIPS32_LE` is defined,
//! so the generic branch of `spl_inl.h` is the one compiled; that is what is
//! ported here.

/// C `kWebRtcSpl_CountLeadingZeros32_Table` (spl_inl.c).
const COUNT_LEADING_ZEROS_32_TABLE: [i8; 64] = [
    32, 8, 17, -1, -1, 14, -1, -1, -1, 20, -1, -1, -1, 28, -1, 18, //
    -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 26, 25, 24, //
    4, 11, 23, 31, 3, 7, 10, 16, 22, 30, -1, -1, 2, 6, 13, 9, //
    -1, 15, -1, 21, -1, 29, 19, -1, -1, -1, -1, -1, 1, 27, 5, 12,
];

/// C `WebRtcSpl_CountLeadingZeros32_NotBuiltin`. Only for tests, like C.
pub fn count_leading_zeros_32_not_builtin(mut n: u32) -> i32 {
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    // `n * 0x8c0b2891` is unsigned and wraps in C.
    COUNT_LEADING_ZEROS_32_TABLE[(n.wrapping_mul(0x8c0b_2891) >> 26) as usize] as i32
}

/// C `WebRtcSpl_CountLeadingZeros64_NotBuiltin`. Only for tests, like C.
pub fn count_leading_zeros_64_not_builtin(n: u64) -> i32 {
    let leading_zeros: i32 = if n >> 32 == 0 { 32 } else { 0 };
    leading_zeros + count_leading_zeros_32_not_builtin((n >> (32 - leading_zeros)) as u32)
}

/// C `WebRtcSpl_CountLeadingZeros32` (`__builtin_clz` path, 32 for 0).
pub fn count_leading_zeros_32(n: u32) -> i32 {
    n.leading_zeros() as i32
}

/// C `WebRtcSpl_CountLeadingZeros64` (`__builtin_clzll` path, 64 for 0).
pub fn count_leading_zeros_64(n: u64) -> i32 {
    n.leading_zeros() as i32
}

/// C `WebRtcSpl_SatW32ToW16`.
pub fn sat_w32_to_w16(value32: i32) -> i16 {
    if value32 > 32767 {
        32767
    } else if value32 < -32768 {
        -32768
    } else {
        value32 as i16
    }
}

/// C `WebRtcSpl_AddSatW32`.
pub fn add_sat_w32(a: i32, b: i32) -> i32 {
    // C does the addition in unsigned arithmetic to avoid signed overflow UB.
    let sum = (a as u32).wrapping_add(b as u32) as i32;
    if (a < 0) == (b < 0) && (a < 0) != (sum < 0) {
        return if sum < 0 { i32::MAX } else { i32::MIN };
    }
    sum
}

/// C `WebRtcSpl_SubSatW32`.
pub fn sub_sat_w32(a: i32, b: i32) -> i32 {
    let diff = (a as u32).wrapping_sub(b as u32) as i32;
    if (a < 0) != (b < 0) && (a < 0) != (diff < 0) {
        return if diff < 0 { i32::MAX } else { i32::MIN };
    }
    diff
}

/// C `WebRtcSpl_AddSatW16`.
pub fn add_sat_w16(a: i16, b: i16) -> i16 {
    sat_w32_to_w16(a as i32 + b as i32)
}

/// C `WebRtcSpl_SubSatW16`.
pub fn sub_sat_w16(var1: i16, var2: i16) -> i16 {
    sat_w32_to_w16(var1 as i32 - var2 as i32)
}

/// C `WebRtcSpl_GetSizeInBits`.
pub fn get_size_in_bits(n: u32) -> i16 {
    (32 - count_leading_zeros_32(n)) as i16
}

/// Number of left shifts to normalize `a` (C `WebRtcSpl_NormW32`), 0 for 0.
pub fn norm_w32(a: i32) -> i16 {
    if a == 0 {
        0
    } else {
        (count_leading_zeros_32((if a < 0 { !a } else { a }) as u32) - 1) as i16
    }
}

/// C `WebRtcSpl_NormW16`, 0 for 0.
pub fn norm_w16(a: i16) -> i16 {
    let a32 = a as i32;
    if a == 0 {
        0
    } else {
        (count_leading_zeros_32((if a < 0 { !a32 } else { a32 }) as u32) - 17) as i16
    }
}

/// C `WebRtcSpl_NormU32`, 0 for 0.
pub fn norm_u32(a: u32) -> i16 {
    if a == 0 {
        0
    } else {
        count_leading_zeros_32(a) as i16
    }
}

/// C `WebRtc_MulAccumW16`: `a * b + c` (wrapping, UB on overflow in C).
pub fn mul_accum_w16(a: i16, b: i16, c: i32) -> i32 {
    (a as i32 * b as i32).wrapping_add(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.InlineTest
    #[test]
    fn inline_test() {
        let a16: i16 = 121;
        let b16: i16 = -17;
        let a32: i32 = 111121;
        let b32: i32 = -1711;

        assert_eq!(17, get_size_in_bits(a32 as u32));

        assert_eq!(0, norm_w32(0));
        assert_eq!(31, norm_w32(-1));
        assert_eq!(0, norm_w32(i32::MIN));
        assert_eq!(14, norm_w32(a32));

        assert_eq!(0, norm_w16(0));
        assert_eq!(15, norm_w16(-1));
        assert_eq!(0, norm_w16(i16::MIN));
        // C passes the int32 -1711 to an int16 parameter (fits).
        assert_eq!(4, norm_w16(b32 as i16));
        for ii in 0..15 {
            let value: i16 = 1 << ii;
            assert_eq!(14 - ii, norm_w16(value));
            assert_eq!(15 - ii, norm_w16(-value));
        }

        assert_eq!(0, norm_u32(0));
        assert_eq!(0, norm_u32(0xffff_ffff));
        assert_eq!(15, norm_u32(a32 as u32));

        assert_eq!(104, add_sat_w16(a16, b16));
        assert_eq!(138, sub_sat_w16(a16, b16));
    }

    // SplTest.AddSubSatW32
    #[test]
    fn add_sub_sat_w32() {
        const ARGS: [i32; 12] = [
            i32::MIN,
            i32::MIN + 1,
            -3,
            -2,
            -1,
            0,
            1,
            -1,
            2,
            3,
            i32::MAX - 1,
            i32::MAX,
        ];
        for &a in &ARGS {
            for &b in &ARGS {
                let sum = (a as i64 + b as i64).clamp(i32::MIN as i64, i32::MAX as i64);
                let diff = (a as i64 - b as i64).clamp(i32::MIN as i64, i32::MAX as i64);
                assert_eq!(sum, add_sat_w32(a, b) as i64, "{a} + {b}");
                assert_eq!(diff, sub_sat_w32(a, b) as i64, "{a} - {b}");
            }
        }
    }

    // SplTest.CountLeadingZeros32
    #[test]
    fn count_leading_zeros32() {
        assert_eq!(32, count_leading_zeros_32(0));
        assert_eq!(32, count_leading_zeros_32_not_builtin(0));
        for i in 0..32 {
            let single_one: u32 = 1 << i;
            let all_ones: u32 = single_one.wrapping_mul(2).wrapping_sub(1);
            assert_eq!(31 - i, count_leading_zeros_32(single_one));
            assert_eq!(31 - i, count_leading_zeros_32_not_builtin(single_one));
            assert_eq!(31 - i, count_leading_zeros_32(all_ones));
            assert_eq!(31 - i, count_leading_zeros_32_not_builtin(all_ones));
        }
    }

    // SplTest.CountLeadingZeros64
    #[test]
    fn count_leading_zeros64() {
        assert_eq!(64, count_leading_zeros_64(0));
        assert_eq!(64, count_leading_zeros_64_not_builtin(0));
        for i in 0..64 {
            let single_one: u64 = 1 << i;
            let all_ones: u64 = single_one.wrapping_mul(2).wrapping_sub(1);
            assert_eq!(63 - i, count_leading_zeros_64(single_one));
            assert_eq!(63 - i, count_leading_zeros_64_not_builtin(single_one));
            assert_eq!(63 - i, count_leading_zeros_64(all_ones));
            assert_eq!(63 - i, count_leading_zeros_64_not_builtin(all_ones));
        }
    }

    #[test]
    fn sat_w32_to_w16_edges() {
        assert_eq!(32767, sat_w32_to_w16(32768));
        assert_eq!(32767, sat_w32_to_w16(i32::MAX));
        assert_eq!(-32768, sat_w32_to_w16(-32769));
        assert_eq!(-32768, sat_w32_to_w16(i32::MIN));
        assert_eq!(-5, sat_w32_to_w16(-5));
    }
}
