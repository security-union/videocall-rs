// Port target (generic C variant only, never NEON/MIPS/ARMv7 asm): WebRTC
// common_audio/signal_processing/min_max_operations.c at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Max-abs, min/max and arg-max over vectors (the `...C` variants, which is
//! what `WebRtcSpl_MaxAbsValueW16` and friends dispatch to without NEON).
//!
//! C `RTC_DCHECK_GT(length, 0)` becomes a `debug_assert!`.

/// C `WebRtcSpl_MaxAbsValueW16C`. `abs(-32768)` is clamped to 32767.
pub fn max_abs_value_w16(vector: &[i16], length: usize) -> i16 {
    debug_assert!(length > 0);
    let mut maximum: i32 = 0;
    for &v in &vector[..length] {
        let absolute = (v as i32).abs();
        if absolute > maximum {
            maximum = absolute;
        }
    }
    if maximum > i16::MAX as i32 {
        maximum = i16::MAX as i32;
    }
    maximum as i16
}

/// C `WebRtcSpl_MaxAbsValueW32C`. `abs(i32::MIN)` is clamped to `i32::MAX`.
pub fn max_abs_value_w32(vector: &[i32], length: usize) -> i32 {
    debug_assert!(length > 0);
    let mut maximum: u32 = 0;
    for &v in &vector[..length] {
        let absolute = v.unsigned_abs();
        if absolute > maximum {
            maximum = absolute;
        }
    }
    if maximum > i32::MAX as u32 {
        maximum = i32::MAX as u32;
    }
    maximum as i32
}

/// C `WebRtcSpl_MaxValueW16C`.
pub fn max_value_w16(vector: &[i16], length: usize) -> i16 {
    debug_assert!(length > 0);
    let mut maximum = i16::MIN;
    for &v in &vector[..length] {
        if v > maximum {
            maximum = v;
        }
    }
    maximum
}

/// C `WebRtcSpl_MaxValueW32C`.
pub fn max_value_w32(vector: &[i32], length: usize) -> i32 {
    debug_assert!(length > 0);
    let mut maximum = i32::MIN;
    for &v in &vector[..length] {
        if v > maximum {
            maximum = v;
        }
    }
    maximum
}

/// C `WebRtcSpl_MinValueW16C`.
pub fn min_value_w16(vector: &[i16], length: usize) -> i16 {
    debug_assert!(length > 0);
    let mut minimum = i16::MAX;
    for &v in &vector[..length] {
        if v < minimum {
            minimum = v;
        }
    }
    minimum
}

/// C `WebRtcSpl_MinValueW32C`.
pub fn min_value_w32(vector: &[i32], length: usize) -> i32 {
    debug_assert!(length > 0);
    let mut minimum = i32::MAX;
    for &v in &vector[..length] {
        if v < minimum {
            minimum = v;
        }
    }
    minimum
}

/// C `WebRtcSpl_MaxAbsIndexW16`: first index of the largest `abs` value.
pub fn max_abs_index_w16(vector: &[i16], length: usize) -> usize {
    debug_assert!(length > 0);
    let mut index = 0;
    let mut maximum: i32 = 0;
    for (i, &v) in vector[..length].iter().enumerate() {
        let absolute = (v as i32).abs();
        if absolute > maximum {
            maximum = absolute;
            index = i;
        }
    }
    index
}

/// C `WebRtcSpl_MinMaxW16` (generic branch): `(min, max)`.
pub fn min_max_w16(vector: &[i16], length: usize) -> (i16, i16) {
    debug_assert!(length > 0);
    let mut minimum = i16::MAX;
    let mut maximum = i16::MIN;
    for &v in &vector[..length] {
        if v < minimum {
            minimum = v;
        }
        if v > maximum {
            maximum = v;
        }
    }
    (minimum, maximum)
}

/// C `WebRtcSpl_MaxAbsElementW16`: the element (with sign) of largest
/// magnitude. A tie between `-x` and `x` returns `x`.
pub fn max_abs_element_w16(vector: &[i16], length: usize) -> i16 {
    let (min_val, max_val) = min_max_w16(vector, length);
    // `-max_val` is computed in `int` after promotion.
    if min_val == max_val || (min_val as i32) < -(max_val as i32) {
        return min_val;
    }
    max_val
}

/// C `WebRtcSpl_MaxIndexW16`: first index of the maximum.
pub fn max_index_w16(vector: &[i16], length: usize) -> usize {
    debug_assert!(length > 0);
    let mut index = 0;
    let mut maximum = i16::MIN;
    for (i, &v) in vector[..length].iter().enumerate() {
        if v > maximum {
            maximum = v;
            index = i;
        }
    }
    index
}

/// C `WebRtcSpl_MaxIndexW32`: first index of the maximum.
pub fn max_index_w32(vector: &[i32], length: usize) -> usize {
    debug_assert!(length > 0);
    let mut index = 0;
    let mut maximum = i32::MIN;
    for (i, &v) in vector[..length].iter().enumerate() {
        if v > maximum {
            maximum = v;
            index = i;
        }
    }
    index
}

/// C `WebRtcSpl_MinIndexW16`: first index of the minimum.
pub fn min_index_w16(vector: &[i16], length: usize) -> usize {
    debug_assert!(length > 0);
    let mut index = 0;
    let mut minimum = i16::MAX;
    for (i, &v) in vector[..length].iter().enumerate() {
        if v < minimum {
            minimum = v;
            index = i;
        }
    }
    index
}

/// C `WebRtcSpl_MinIndexW32`: first index of the minimum.
pub fn min_index_w32(vector: &[i32], length: usize) -> usize {
    debug_assert!(length > 0);
    let mut index = 0;
    let mut minimum = i32::MAX;
    for (i, &v) in vector[..length].iter().enumerate() {
        if v < minimum {
            minimum = v;
            index = i;
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spl::{WORD16_MAX, WORD16_MIN, WORD32_MAX, WORD32_MIN};

    // signal_processing_unittest.cc: SplTest.MinMaxOperationsTest
    #[test]
    fn min_max_operations_test() {
        const N: usize = 17;
        let mut vector16: [i16; N] = [
            -1, 7485, 0, 3333, -18283, 0, 12334, -29871, 988, -3333, 345, -456, 222, 999, 888,
            8774, WORD16_MIN,
        ];
        let mut vector32: [i32; N] = [
            -1, 0, 283211, 3333, 8712345, 0, -3333, 89345, -374585456, 222, 999, 122345334,
            -12389756, -987329871, 888, -2, WORD32_MIN,
        ];

        assert_eq!(WORD16_MIN, min_value_w16(&vector16, N));
        assert_eq!(WORD32_MIN, min_value_w32(&vector32, N));
        assert_eq!(N - 1, min_index_w16(&vector16, N));
        assert_eq!(N - 1, min_index_w32(&vector32, N));
        assert_eq!(WORD16_MIN, max_abs_element_w16(&vector16, N));
        assert_eq!((WORD16_MIN, 12334), min_max_w16(&vector16, N));

        vector16[N - 1] = WORD16_MAX;
        vector32[N - 1] = WORD32_MAX;
        assert_eq!(WORD16_MAX, max_abs_value_w16(&vector16, N));
        assert_eq!(WORD16_MAX, max_value_w16(&vector16, N));
        assert_eq!(WORD32_MAX, max_abs_value_w32(&vector32, N));
        assert_eq!(WORD32_MAX, max_value_w32(&vector32, N));
        assert_eq!(N - 1, max_abs_index_w16(&vector16, N));
        assert_eq!(N - 1, max_index_w16(&vector16, N));
        assert_eq!(N - 1, max_index_w32(&vector32, N));
        assert_eq!(WORD16_MAX, max_abs_element_w16(&vector16, N));
        assert_eq!((-29871, WORD16_MAX), min_max_w16(&vector16, N));

        vector16[1] = WORD16_MAX;
        vector16[6] = WORD16_MIN;
        vector16[11] = WORD16_MIN;
        vector32[1] = WORD32_MAX;
        vector32[6] = WORD32_MIN;
        vector32[11] = WORD32_MIN;
        assert_eq!(WORD16_MAX, max_abs_value_w16(&vector16, N));
        assert_eq!(WORD16_MAX, max_value_w16(&vector16, N));
        assert_eq!(WORD16_MIN, min_value_w16(&vector16, N));
        assert_eq!(WORD32_MAX, max_abs_value_w32(&vector32, N));
        assert_eq!(WORD32_MAX, max_value_w32(&vector32, N));
        assert_eq!(WORD32_MIN, min_value_w32(&vector32, N));
        assert_eq!(6, max_abs_index_w16(&vector16, N));
        assert_eq!(1, max_index_w16(&vector16, N));
        assert_eq!(1, max_index_w32(&vector32, N));
        assert_eq!(6, min_index_w16(&vector16, N));
        assert_eq!(6, min_index_w32(&vector32, N));
        assert_eq!(WORD16_MIN, max_abs_element_w16(&vector16, N));
        assert_eq!((WORD16_MIN, WORD16_MAX), min_max_w16(&vector16, N));

        let single = [0i16];
        assert_eq!(0, max_abs_value_w16(&single, 1));
        assert_eq!(0, max_value_w16(&single, 1));
        assert_eq!(0, min_value_w16(&single, 1));
        assert_eq!(0, max_abs_index_w16(&single, 1));
        assert_eq!(0, max_index_w16(&single, 1));
        assert_eq!(0, min_index_w16(&single, 1));
        assert_eq!(0, max_abs_element_w16(&single, 1));
        assert_eq!((0, 0), min_max_w16(&single, 1));

        let two = [WORD16_MIN, WORD16_MAX];
        assert_eq!(WORD16_MAX, max_abs_value_w16(&two, 2));
        assert_eq!(WORD16_MAX, max_value_w16(&two, 2));
        assert_eq!(WORD16_MIN, min_value_w16(&two, 2));
        assert_eq!(0, max_abs_index_w16(&two, 2));
        assert_eq!(1, max_index_w16(&two, 2));
        assert_eq!(0, min_index_w16(&two, 2));
        assert_eq!(WORD16_MIN, max_abs_element_w16(&two, 2));
        assert_eq!((WORD16_MIN, WORD16_MAX), min_max_w16(&two, 2));
    }

    #[test]
    fn max_abs_element_ties_prefer_positive() {
        assert_eq!(5, max_abs_element_w16(&[5, -5], 2));
        assert_eq!(-6, max_abs_element_w16(&[-6, 5], 2));
        assert_eq!(6, max_abs_element_w16(&[6, -5], 2));
        assert_eq!(-7, max_abs_element_w16(&[-7, -7], 2));
    }
}
