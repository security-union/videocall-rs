// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/spl_sqrt.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_Sqrt` (Taylor-series fixed-point square root).

use super::spl_inl::norm_w32;

/// C `WebRtcSpl_SqrtLocal`. Every `int` product and sum is ported with
/// wrapping arithmetic, `(int16_t)` casts truncate and `>>` is arithmetic.
fn sqrt_local(input: i32) -> i32 {
    let mut b: i32 = input / 2;
    b = b.wrapping_sub(0x4000_0000); // B = in/2 - 1/2
    let x_half = (b >> 16) as i16; // x_half = x/2 = (in-1)/2
    b = b.wrapping_add(0x4000_0000); // B = 1 + x/2
    b = b.wrapping_add(0x4000_0000); // Add 0.5 twice (1.0 does not exist in Q31)

    let x2: i32 = (x_half as i32).wrapping_mul(x_half as i32).wrapping_mul(2); // (x/2)^2
    let mut a: i32 = x2.wrapping_neg(); // -(x/2)^2
    b = b.wrapping_add(a >> 1); // B = 1 + x/2 - 0.5*(x/2)^2

    a >>= 16;
    a = a.wrapping_mul(a).wrapping_mul(2); // (x/2)^4
    let mut t16 = (a >> 16) as i16;
    b = b.wrapping_add((-20480i32).wrapping_mul(t16 as i32).wrapping_mul(2)); // B - 0.625*A

    a = (x_half as i32).wrapping_mul(t16 as i32).wrapping_mul(2); // (x/2)^5
    t16 = (a >> 16) as i16;
    b = b.wrapping_add(28672i32.wrapping_mul(t16 as i32).wrapping_mul(2)); // B + 0.875*A

    t16 = (x2 >> 16) as i16;
    a = (x_half as i32).wrapping_mul(t16 as i32).wrapping_mul(2); // (x/2)^3
    b = b.wrapping_add(a >> 1); // B + 0.5*A

    b.wrapping_add(32768) // Round off bit
}

/// C `WebRtcSpl_Sqrt`: rounded `sqrt(abs(value))`; `i32::MIN` maps to
/// `i32::MAX` before the root.
pub fn sqrt(value: i32) -> i32 {
    const K_SQRT_2: i16 = 23170; // 1/sqrt2 (==5a82)

    let mut a = value;
    if a < 0 {
        if a == i32::MIN {
            a = i32::MAX;
        } else {
            a = -a;
        }
    } else if a == 0 {
        return 0;
    }

    let sh = norm_w32(a); // # shifts to normalize A
    a = a.wrapping_shl(sh as u32); // Normalize A
    if a < (i32::MAX - 32767) {
        a += 32768; // Round off bit
    } else {
        a = i32::MAX;
    }

    let x_norm = (a >> 16) as i16; // x_norm = AH

    let nshift: i16 = sh / 2;
    debug_assert!(nshift >= 0);

    a = (x_norm as i32).wrapping_shl(16);
    a = a.wrapping_abs(); // WEBRTC_SPL_ABS_W32: abs(x_norm<<16)
    a = sqrt_local(a); // A = sqrt(A)

    if 2 * nshift == sh {
        // Even shift value case.
        let t16 = (a >> 16) as i16; // t16 = AH
        a = (K_SQRT_2 as i32).wrapping_mul(t16 as i32).wrapping_mul(2); // 1/sqrt(2)*t16
        a = a.wrapping_add(32768); // Round off
        a &= 0x7fff_0000; // Round off
        a >>= 15; // A = A>>16
    } else {
        a >>= 16; // A = A>>16
    }

    a &= 0x0000_ffff;
    a >> nshift // De-normalize the result.
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.MathOperationsTest (Sqrt).
    #[test]
    fn math_operations_test_sqrt() {
        assert_eq!(33700, sqrt(1134567892));
    }

    #[test]
    fn edges() {
        assert_eq!(0, sqrt(0));
        assert_eq!(sqrt(5), sqrt(-5));
        assert_eq!(sqrt(i32::MAX), sqrt(i32::MIN));
    }
}
