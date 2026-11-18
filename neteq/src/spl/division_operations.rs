// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/division_operations.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_DivU32U16`, `DivW32W16`, `DivW32W16ResW16`, `DivW32HiLow`.
//!
//! C integer division truncates toward zero, as Rust's does.

/// C `WebRtcSpl_DivU32U16`: `num / den`, `0xFFFFFFFF` for `den == 0`.
pub fn div_u32_u16(num: u32, den: u16) -> u32 {
    if den != 0 {
        num / den as u32
    } else {
        0xFFFF_FFFF
    }
}

/// C `WebRtcSpl_DivW32W16`: `num / den`, returning `i32::MAX` for `den == 0`.
/// `i32::MIN / -1` overflows (UB in C); it wraps here.
pub fn div_w32_w16(num: i32, den: i16) -> i32 {
    if den != 0 {
        num.wrapping_div(den as i32)
    } else {
        0x7FFF_FFFF
    }
}

/// C `WebRtcSpl_DivW32W16ResW16`: `(int16_t)(num / den)`, `0x7FFF` for 0.
pub fn div_w32_w16_res_w16(num: i32, den: i16) -> i16 {
    if den != 0 {
        num.wrapping_div(den as i32) as i16
    } else {
        0x7FFF
    }
}

/// C `WebRtcSpl_DivW32HiLow`: `num / (den_hi << 16 + den_low << 1)` in Q31,
/// used by `levinson_durbin`. All `int16_t * int16_t` products are `int`;
/// the left shifts may overflow in C (UB, wraps in practice) and wrap here;
/// right shifts of negatives are arithmetic; `(int16_t)` casts truncate.
pub fn div_w32_hi_low(num: i32, den_hi: i16, den_low: i16) -> i32 {
    let approx = div_w32_w16(0x1FFF_FFFF, den_hi) as i16;
    // tmpW32 = den * approx in Q30.
    let mut tmp_w32: i32 = ((den_hi as i32 * approx as i32) << 1)
        .wrapping_add(((den_low as i32 * approx as i32) >> 15) << 1);
    // 2.0 - den * approx, computed in int64 then truncated.
    tmp_w32 = (0x7fff_ffffi64 - tmp_w32 as i64) as i32;

    let mut tmp_hi = (tmp_w32 >> 16) as i16;
    let mut tmp_low = (tmp_w32.wrapping_sub((tmp_hi as i32) << 16) >> 1) as i16;

    // 1/den in Q29.
    tmp_w32 = ((tmp_hi as i32 * approx as i32)
        .wrapping_add((tmp_low as i32 * approx as i32) >> 15))
    .wrapping_shl(1);

    tmp_hi = (tmp_w32 >> 16) as i16;
    tmp_low = (tmp_w32.wrapping_sub((tmp_hi as i32) << 16) >> 1) as i16;

    let num_hi = (num >> 16) as i16;
    let num_low = (num.wrapping_sub((num_hi as i32) << 16) >> 1) as i16;

    // num * (1/den) in Q28.
    tmp_w32 = (num_hi as i32 * tmp_hi as i32)
        .wrapping_add((num_hi as i32 * tmp_low as i32) >> 15)
        .wrapping_add((num_low as i32 * tmp_hi as i32) >> 15);

    // Q28 -> Q31 (WEBRTC_SPL_LSHIFT_W32).
    tmp_w32.wrapping_shl(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.MathOperationsTest (division part).
    #[test]
    fn math_operations_test_division() {
        let num: i32 = 117;
        let den: i32 = -5;
        let den_u: u16 = 5;
        assert_eq!(-23, div_w32_w16_res_w16(num, den as i16));
        assert_eq!(-23, div_w32_w16(num, den as i16));
        assert_eq!(23, div_u32_u16(num as u32, den_u));
        assert_eq!(0, div_w32_hi_low(128, 0, 256));
    }

    #[test]
    fn divide_by_zero_guards() {
        assert_eq!(i32::MAX, div_w32_w16(5, 0));
        assert_eq!(0x7FFF, div_w32_w16_res_w16(5, 0));
        assert_eq!(u32::MAX, div_u32_u16(5, 0));
        assert_eq!(-2, div_w32_w16(-5, 2));
    }
}
