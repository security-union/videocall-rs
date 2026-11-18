// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/include/signal_processing_library.h (macros) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The `WEBRTC_SPL_*` macros NetEq uses, as functions and constants.
//!
//! The C macros cast their operands (`(int16_t)a`), so callers that pass a
//! wider C value must truncate with `as i16` at the call site, exactly where
//! the macro would.

/// `WEBRTC_SPL_WORD16_MAX`.
pub const WORD16_MAX: i16 = 32767;
/// `WEBRTC_SPL_WORD16_MIN`.
pub const WORD16_MIN: i16 = -32768;
/// `WEBRTC_SPL_WORD32_MAX`.
pub const WORD32_MAX: i32 = 0x7fff_ffff;
/// `WEBRTC_SPL_WORD32_MIN`.
pub const WORD32_MIN: i32 = i32::MIN;
/// `WEBRTC_SPL_MAX_LPC_ORDER`.
pub const MAX_LPC_ORDER: usize = 14;

/// `WEBRTC_SPL_SHIFT_W32(x, c)`: `x * (1 << c)` if `c >= 0`, else `x >> -c`.
///
/// C overflow on the left shift is UB; it wraps on every compiler WebRTC
/// supports, so this wraps. The right shift of a negative `x` is
/// implementation-defined in C (arithmetic on clang/gcc/msvc), and Rust's
/// `>>` on `i32` is arithmetic.
pub fn spl_shift_w32(x: i32, c: i32) -> i32 {
    if c >= 0 {
        x.wrapping_shl(c as u32)
    } else {
        x >> -c
    }
}

/// `WEBRTC_SPL_LSHIFT_W32(x, c)`: `x << c` (wrapping; UB on overflow in C).
pub fn spl_lshift_w32(x: i32, c: i32) -> i32 {
    x.wrapping_shl(c as u32)
}

/// `WEBRTC_SPL_RSHIFT_U32(x, c)`.
pub fn spl_rshift_u32(x: u32, c: i32) -> u32 {
    x >> c
}

/// `WEBRTC_SPL_ABS_W16(a)`.
///
/// The macro negates after integer promotion, so its C type is `int`, not
/// `int16_t`: `WEBRTC_SPL_ABS_W16(-32768)` is `32768`. Callers that store it
/// in an `int16_t` truncate with `as i16`.
pub fn spl_abs_w16(a: i16) -> i32 {
    let a = a as i32;
    if a >= 0 {
        a
    } else {
        -a
    }
}

/// `WEBRTC_SPL_ABS_W32(a)`. Negating `i32::MIN` is UB in C; it wraps to
/// `i32::MIN` in practice, so this wraps.
pub fn spl_abs_w32(a: i32) -> i32 {
    if a >= 0 {
        a
    } else {
        a.wrapping_neg()
    }
}

/// `WEBRTC_SPL_MUL(a, b)` on `int32_t` operands (wrapping).
pub fn spl_mul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

/// `WEBRTC_SPL_MUL_16_16(a, b)`.
pub fn spl_mul_16_16(a: i16, b: i16) -> i32 {
    a as i32 * b as i32
}

/// `WEBRTC_SPL_MUL_16_16_RSFT(a, b, c)`. Arithmetic shift, like clang.
pub fn spl_mul_16_16_rsft(a: i16, b: i16, c: i32) -> i32 {
    spl_mul_16_16(a, b) >> c
}

/// `WEBRTC_SPL_MUL_16_16_RSFT_WITH_ROUND(a, b, c)`.
pub fn spl_mul_16_16_rsft_with_round(a: i16, b: i16, c: i32) -> i32 {
    (spl_mul_16_16(a, b).wrapping_add(1i32 << (c - 1))) >> c
}

/// `WEBRTC_SPL_SAT(a, b, c)`: `b` clamped to `[c, a]` (upper bound first).
pub fn spl_sat(a: i64, b: i64, c: i64) -> i64 {
    if b > a {
        a
    } else if b < c {
        c
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.MacroTest (the macros ported here).
    #[test]
    fn macro_test() {
        let big_a: i32 = 10;
        let big_b: i32 = 21;
        let mut a: i32 = -3;

        assert_eq!(3, spl_abs_w16(a as i16));
        assert_eq!(3, spl_abs_w32(a));
        assert_eq!(-63, spl_mul(a, big_b));

        a = (WORD16_MAX >> 1) as i32;
        let b: i32 = -3;
        assert_eq!(-12288, spl_mul_16_16_rsft(a as i16, b as i16, 2));
        assert_eq!(-12287, spl_mul_16_16_rsft_with_round(a as i16, b as i16, 2));
        assert_eq!(21, spl_sat(a as i64, big_a as i64, big_b as i64));
        assert_eq!(21, spl_sat(a as i64, big_b as i64, big_a as i64));

        let shift_amount = 1;
        assert_eq!(32766, spl_shift_w32(a, shift_amount));
        assert_eq!(32766, spl_lshift_w32(a, 1));
        assert_eq!(8191u32, spl_rshift_u32(a as u32, 1));

        assert_eq!(-49149, spl_mul_16_16(a as i16, b as i16));
        assert_eq!(1073676289, spl_mul_16_16(WORD16_MAX, WORD16_MAX));
    }

    #[test]
    fn abs_edges_match_c() {
        // The C macro yields an `int`, so -32768 maps to 32768.
        assert_eq!(32768, spl_abs_w16(i16::MIN));
        assert_eq!(i32::MIN, spl_abs_w32(i32::MIN));
        assert_eq!(5, spl_shift_w32(10, -1));
        assert_eq!(-5, spl_shift_w32(-10, -1));
        assert_eq!(-1, spl_shift_w32(-1, -31));
        assert_eq!(i32::MIN, spl_shift_w32(1, 31));
    }
}
