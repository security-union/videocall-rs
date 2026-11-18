// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/third_party/spl_sqrt_floor/spl_sqrt_floor.c (C variant, not _arm.S) at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! `WebRtcSpl_SqrtFloor`.

/// C `WebRtcSpl_SqrtFloor`: `floor(sqrt(value))` by successive
/// approximation, 0 for negative input.
///
/// The `WEBRTC_SPL_SQRT_ITER(N)` macro is unrolled for N = 15..0; the loop
/// below runs the same iterations in the same order.
pub fn sqrt_floor(value: i32) -> i32 {
    let mut value = value;
    let mut root: i32 = 0;
    for n in (0..16).rev() {
        let try1 = root + (1 << n);
        if value >= try1 << n {
            value -= try1 << n;
            root |= 2 << n;
        }
    }
    root >> 1
}

#[cfg(test)]
mod tests {
    use super::*;

    // signal_processing_unittest.cc: SplTest.MathOperationsTest (SqrtFloor).
    #[test]
    fn math_operations_test_sqrt_floor() {
        assert_eq!(33683, sqrt_floor(1134567892));
    }

    #[test]
    fn exact_floor_for_many_values() {
        assert_eq!(0, sqrt_floor(-5));
        assert_eq!(0, sqrt_floor(0));
        assert_eq!(46340, sqrt_floor(i32::MAX));
        let mut x: i64 = 1;
        while x <= i32::MAX as i64 {
            let v = x as i32;
            let expected = (x as f64).sqrt().floor() as i32;
            assert_eq!(expected, sqrt_floor(v), "{v}");
            x = x * 3 / 2 + 1;
        }
    }
}
