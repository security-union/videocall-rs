// Ports, at WebRTC rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd, of:
// - modules/include/module_common_types_public.h (IsNewer*, Latest*)
// - api/numerics/sequence_number_unwrapper.h (SeqNumUnwrapper, M == 0 only)
// - rtc_base/numerics/mod_ops.h / sequence_number_util.h (ForwardDiff, AheadOrAt)
// - rtc_base/numerics/safe_conversions.h (saturated_cast, checked_cast,
//   dchecked_cast), safe_minmax.h (SafeClamp, SafeMin, SafeMax),
//   rtc_base/checks.h (CheckedDivExact)
// plus their unit tests. Copyright (c) The WebRTC project authors.
// BSD-3-Clause, see LICENSE-WEBRTC.

//! Small numeric helpers from `rtc_base` and `modules/include`.

// ---------------------------------------------------------------------------
// IsNewer / Latest (module_common_types_public.h)
// ---------------------------------------------------------------------------

/// Unsigned wrapping types usable with [`is_newer`] and [`SeqNumUnwrapper`].
pub trait WrappingUnsigned: Copy + Eq + Ord + std::fmt::Debug {
    /// `(max >> 1) + 1`, the half-way mark.
    const BREAKPOINT: Self;
    /// `max + 1` as i64.
    const MODULUS: i64;
    /// `self - other` with wraparound.
    fn wrapping_diff(self, other: Self) -> Self;
    /// Lossless widening.
    fn to_i64(self) -> i64;
}

macro_rules! impl_wrapping_unsigned {
    ($($t:ty),*) => {$(
        impl WrappingUnsigned for $t {
            const BREAKPOINT: Self = (<$t>::MAX >> 1) + 1;
            const MODULUS: i64 = <$t>::MAX as i64 + 1;
            fn wrapping_diff(self, other: Self) -> Self {
                self.wrapping_sub(other)
            }
            fn to_i64(self) -> i64 {
                self as i64
            }
        }
    )*};
}
impl_wrapping_unsigned!(u8, u16, u32);

/// C++ `IsNewer<U>`: true if `value` is ahead of `prev_value` modulo the type
/// width. Exactly half-way apart, the numerically larger value is newer.
/// Not a strict weak ordering; never use it as a map comparator.
pub fn is_newer<U: WrappingUnsigned>(value: U, prev_value: U) -> bool {
    if value.wrapping_diff(prev_value) == U::BREAKPOINT {
        return value > prev_value;
    }
    value != prev_value && value.wrapping_diff(prev_value) < U::BREAKPOINT
}

/// C++ `IsNewerSequenceNumber`.
pub fn is_newer_sequence_number(sequence_number: u16, prev_sequence_number: u16) -> bool {
    is_newer(sequence_number, prev_sequence_number)
}

/// C++ `IsNewerTimestamp`.
pub fn is_newer_timestamp(timestamp: u32, prev_timestamp: u32) -> bool {
    is_newer(timestamp, prev_timestamp)
}

/// C++ `LatestSequenceNumber`.
pub fn latest_sequence_number(sequence_number1: u16, sequence_number2: u16) -> u16 {
    if is_newer_sequence_number(sequence_number1, sequence_number2) {
        sequence_number1
    } else {
        sequence_number2
    }
}

/// C++ `LatestTimestamp`.
pub fn latest_timestamp(timestamp1: u32, timestamp2: u32) -> u32 {
    if is_newer_timestamp(timestamp1, timestamp2) {
        timestamp1
    } else {
        timestamp2
    }
}

// ---------------------------------------------------------------------------
// SeqNumUnwrapper (api/numerics/sequence_number_unwrapper.h), M == 0
// ---------------------------------------------------------------------------

/// C++ `ForwardDiff<T, 0>(a, b)`: distance from `a` forward to `b`.
pub fn forward_diff<T: WrappingUnsigned>(a: T, b: T) -> T {
    b.wrapping_diff(a)
}

/// C++ `AheadOrAt<T, 0>(a, b)`.
pub fn ahead_or_at<T: WrappingUnsigned>(a: T, b: T) -> bool {
    let max_dist = T::BREAKPOINT;
    if a.wrapping_diff(b) == max_dist {
        return b < a;
    }
    forward_diff(b, a) < max_dist
}

/// C++ `AheadOf<T, 0>(a, b)`.
pub fn ahead_of<T: WrappingUnsigned>(a: T, b: T) -> bool {
    a != b && ahead_or_at(a, b)
}

/// C++ `SeqNumUnwrapper<T>`: unwraps a wrapping counter into an `i64`. The
/// first unwrapped value equals the first value seen.
#[derive(Debug, Clone, Default)]
pub struct SeqNumUnwrapper<T: WrappingUnsigned> {
    last_unwrapped: i64,
    last_value: Option<T>,
}

impl<T: WrappingUnsigned> SeqNumUnwrapper<T> {
    /// A fresh unwrapper.
    pub fn new() -> Self {
        Self {
            last_unwrapped: 0,
            last_value: None,
        }
    }

    /// Unwraps `value` and updates the internal state.
    pub fn unwrap(&mut self, value: T) -> i64 {
        match self.last_value {
            None => self.last_unwrapped = value.to_i64(),
            Some(last) => self.last_unwrapped += Self::delta(last, value),
        }
        self.last_value = Some(value);
        self.last_unwrapped
    }

    /// Unwraps `value` without updating the internal state.
    pub fn peek_unwrap(&self, value: T) -> i64 {
        match self.last_value {
            None => value.to_i64(),
            Some(last) => self.last_unwrapped + Self::delta(last, value),
        }
    }

    /// Back to the initial state.
    pub fn reset(&mut self) {
        self.last_unwrapped = 0;
        self.last_value = None;
    }

    fn delta(last_value: T, new_value: T) -> i64 {
        let mut result = forward_diff(last_value, new_value).to_i64();
        if !ahead_or_at(new_value, last_value) {
            result -= T::MODULUS;
        }
        result
    }
}

/// C++ `RtpTimestampUnwrapper`.
pub type RtpTimestampUnwrapper = SeqNumUnwrapper<u32>;
/// C++ `RtpSequenceNumberUnwrapper`.
pub type RtpSequenceNumberUnwrapper = SeqNumUnwrapper<u16>;

// ---------------------------------------------------------------------------
// safe_conversions.h / safe_minmax.h / checks.h
// ---------------------------------------------------------------------------

/// Integer conversions with the semantics of `rtc_base/numerics/safe_conversions.h`.
pub trait IntCast<Dst>: Copy {
    /// C++ `saturated_cast<Dst>`: clamps to `Dst`'s range.
    fn saturated(self) -> Dst;
    /// C++ `static_cast<Dst>` (truncating / wrapping, like Rust `as`).
    fn truncated(self) -> Dst;
    /// True if the value fits in `Dst` unchanged.
    fn fits(self) -> bool;
}

macro_rules! impl_int_cast {
    ($src:ty => $($dst:ty),*) => {$(
        impl IntCast<$dst> for $src {
            #[inline]
            fn saturated(self) -> $dst {
                let wide = self as i128;
                if wide < <$dst>::MIN as i128 {
                    <$dst>::MIN
                } else if wide > <$dst>::MAX as i128 {
                    <$dst>::MAX
                } else {
                    self as $dst
                }
            }
            #[inline]
            fn truncated(self) -> $dst {
                self as $dst
            }
            #[inline]
            fn fits(self) -> bool {
                let wide = self as i128;
                wide >= <$dst>::MIN as i128 && wide <= <$dst>::MAX as i128
            }
        }
    )*};
}

macro_rules! impl_int_cast_all {
    ($($src:ty),*) => {$(
        impl_int_cast!($src => i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
    )*};
}
impl_int_cast_all!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// C++ `saturated_cast<Dst>(value)`.
#[inline]
pub fn saturated_cast<Dst, Src: IntCast<Dst>>(value: Src) -> Dst {
    value.saturated()
}

/// C++ `dchecked_cast<Dst>(value)`: debug-asserts the value fits, then
/// truncates like `static_cast`.
#[inline]
pub fn dchecked_cast<Dst, Src: IntCast<Dst>>(value: Src) -> Dst {
    debug_assert!(value.fits(), "dchecked_cast: value out of range");
    value.truncated()
}

/// C++ `checked_cast<Dst>(value)`: panics (even in release) if the value
/// does not fit.
#[inline]
pub fn checked_cast<Dst, Src: IntCast<Dst>>(value: Src) -> Dst {
    assert!(value.fits(), "checked_cast: value out of range");
    value.truncated()
}

/// C++ `SafeClamp(x, min, max)`. Debug-asserts `min <= max`.
#[inline]
pub fn safe_clamp<T: PartialOrd>(x: T, min: T, max: T) -> T {
    debug_assert!(min <= max);
    if x <= min {
        min
    } else if x >= max {
        max
    } else {
        x
    }
}

/// C++ `SafeMin(a, b)` for same-typed arguments.
#[inline]
pub fn safe_min<T: PartialOrd>(a: T, b: T) -> T {
    if b < a {
        b
    } else {
        a
    }
}

/// C++ `SafeMax(a, b)` for same-typed arguments.
#[inline]
pub fn safe_max<T: PartialOrd>(a: T, b: T) -> T {
    if b > a {
        b
    } else {
        a
    }
}

/// C++ `CheckedDivExact(a, b)`: `a / b`, asserting there is no remainder.
#[inline]
pub fn checked_div_exact<T>(a: T, b: T) -> T
where
    T: Copy + std::ops::Div<Output = T> + std::ops::Rem<Output = T> + PartialEq + Default,
{
    assert!(a % b == T::default(), "CheckedDivExact: inexact division");
    a / b
}

#[cfg(test)]
mod tests {
    use super::*;

    // module_common_types_public_unittest.cc

    #[test]
    fn is_newer_sequence_number_equal() {
        assert!(!is_newer_sequence_number(0x0001, 0x0001));
    }

    #[test]
    fn is_newer_sequence_number_no_wrap() {
        assert!(is_newer_sequence_number(0xFFFF, 0xFFFE));
        assert!(is_newer_sequence_number(0x0001, 0x0000));
        assert!(is_newer_sequence_number(0x0100, 0x00FF));
    }

    #[test]
    fn is_newer_sequence_number_forward_wrap() {
        assert!(is_newer_sequence_number(0x0000, 0xFFFF));
        assert!(is_newer_sequence_number(0x0000, 0xFF00));
        assert!(is_newer_sequence_number(0x00FF, 0xFFFF));
        assert!(is_newer_sequence_number(0x00FF, 0xFF00));
    }

    #[test]
    fn is_newer_sequence_number_backward_wrap() {
        assert!(!is_newer_sequence_number(0xFFFF, 0x0000));
        assert!(!is_newer_sequence_number(0xFF00, 0x0000));
        assert!(!is_newer_sequence_number(0xFFFF, 0x00FF));
        assert!(!is_newer_sequence_number(0xFF00, 0x00FF));
    }

    #[test]
    fn is_newer_sequence_number_half_way_apart() {
        assert!(is_newer_sequence_number(0x8000, 0x0000));
        assert!(!is_newer_sequence_number(0x0000, 0x8000));
    }

    #[test]
    fn is_newer_timestamp_equal() {
        assert!(!is_newer_timestamp(0x00000001, 0x000000001));
    }

    #[test]
    fn is_newer_timestamp_no_wrap() {
        assert!(is_newer_timestamp(0xFFFFFFFF, 0xFFFFFFFE));
        assert!(is_newer_timestamp(0x00000001, 0x00000000));
        assert!(is_newer_timestamp(0x00010000, 0x0000FFFF));
    }

    #[test]
    fn is_newer_timestamp_forward_wrap() {
        assert!(is_newer_timestamp(0x00000000, 0xFFFFFFFF));
        assert!(is_newer_timestamp(0x00000000, 0xFFFF0000));
        assert!(is_newer_timestamp(0x0000FFFF, 0xFFFFFFFF));
        assert!(is_newer_timestamp(0x0000FFFF, 0xFFFF0000));
    }

    #[test]
    fn is_newer_timestamp_backward_wrap() {
        assert!(!is_newer_timestamp(0xFFFFFFFF, 0x00000000));
        assert!(!is_newer_timestamp(0xFFFF0000, 0x00000000));
        assert!(!is_newer_timestamp(0xFFFFFFFF, 0x0000FFFF));
        assert!(!is_newer_timestamp(0xFFFF0000, 0x0000FFFF));
    }

    #[test]
    fn is_newer_timestamp_half_way_apart() {
        assert!(is_newer_timestamp(0x80000000, 0x00000000));
        assert!(!is_newer_timestamp(0x00000000, 0x80000000));
    }

    #[test]
    fn latest_sequence_number_no_wrap() {
        assert_eq!(0xFFFF, latest_sequence_number(0xFFFF, 0xFFFE));
        assert_eq!(0x0001, latest_sequence_number(0x0001, 0x0000));
        assert_eq!(0x0100, latest_sequence_number(0x0100, 0x00FF));
        assert_eq!(0xFFFF, latest_sequence_number(0xFFFE, 0xFFFF));
        assert_eq!(0x0001, latest_sequence_number(0x0000, 0x0001));
        assert_eq!(0x0100, latest_sequence_number(0x00FF, 0x0100));
    }

    #[test]
    fn latest_sequence_number_wrap() {
        assert_eq!(0x0000, latest_sequence_number(0x0000, 0xFFFF));
        assert_eq!(0x0000, latest_sequence_number(0x0000, 0xFF00));
        assert_eq!(0x00FF, latest_sequence_number(0x00FF, 0xFFFF));
        assert_eq!(0x00FF, latest_sequence_number(0x00FF, 0xFF00));
        assert_eq!(0x0000, latest_sequence_number(0xFFFF, 0x0000));
        assert_eq!(0x0000, latest_sequence_number(0xFF00, 0x0000));
        assert_eq!(0x00FF, latest_sequence_number(0xFFFF, 0x00FF));
        assert_eq!(0x00FF, latest_sequence_number(0xFF00, 0x00FF));
    }

    #[test]
    fn latest_timestamp_no_wrap() {
        assert_eq!(0xFFFFFFFF, latest_timestamp(0xFFFFFFFF, 0xFFFFFFFE));
        assert_eq!(0x00000001, latest_timestamp(0x00000001, 0x00000000));
        assert_eq!(0x00010000, latest_timestamp(0x00010000, 0x0000FFFF));
    }

    #[test]
    fn latest_timestamp_wrap() {
        assert_eq!(0x00000000, latest_timestamp(0x00000000, 0xFFFFFFFF));
        assert_eq!(0x00000000, latest_timestamp(0x00000000, 0xFFFF0000));
        assert_eq!(0x0000FFFF, latest_timestamp(0x0000FFFF, 0xFFFFFFFF));
        assert_eq!(0x0000FFFF, latest_timestamp(0x0000FFFF, 0xFFFF0000));
        assert_eq!(0x00000000, latest_timestamp(0xFFFFFFFF, 0x00000000));
        assert_eq!(0x00000000, latest_timestamp(0xFFFF0000, 0x00000000));
        assert_eq!(0x0000FFFF, latest_timestamp(0xFFFFFFFF, 0x0000FFFF));
        assert_eq!(0x0000FFFF, latest_timestamp(0xFFFF0000, 0x0000FFFF));
    }

    // sequence_number_unwrapper_unittest.cc (the M == 0 cases)

    #[test]
    fn seq_num_unwrapper_preserve_start_value() {
        let mut unwrapper = SeqNumUnwrapper::<u8>::new();
        assert_eq!(123, unwrapper.unwrap(123));
    }

    #[test]
    fn seq_num_unwrapper_forward_wrap() {
        let mut unwrapper = SeqNumUnwrapper::<u8>::new();
        assert_eq!(255, unwrapper.unwrap(255));
        assert_eq!(256, unwrapper.unwrap(0));
    }

    #[test]
    fn seq_num_unwrapper_backward_wrap() {
        let mut unwrapper = SeqNumUnwrapper::<u8>::new();
        assert_eq!(0, unwrapper.unwrap(0));
        assert_eq!(-2, unwrapper.unwrap(254));
    }

    #[test]
    fn seq_num_unwrapper_unwrap() {
        let mut unwrapper = SeqNumUnwrapper::<u16>::new();
        let k_max = u16::MAX as i64;
        let k_max_dist = (u16::MAX / 2 + 1) as i64;
        let max_dist = u16::MAX / 2 + 1;

        assert_eq!(0, unwrapper.unwrap(0));
        assert_eq!(k_max_dist, unwrapper.unwrap(max_dist));
        assert_eq!(0, unwrapper.unwrap(0));

        assert_eq!(k_max_dist, unwrapper.unwrap(max_dist));
        assert_eq!(k_max, unwrapper.unwrap(u16::MAX));
        assert_eq!(k_max + 1, unwrapper.unwrap(0));
        assert_eq!(k_max, unwrapper.unwrap(u16::MAX));
        assert_eq!(k_max_dist, unwrapper.unwrap(max_dist));
        assert_eq!(0, unwrapper.unwrap(0));
    }

    #[test]
    fn seq_num_unwrapper_reset() {
        let k_max = u16::MAX;
        let k_max_step = k_max / 2;
        let mut unwrapper = SeqNumUnwrapper::<u16>::new();
        assert_eq!(10, unwrapper.unwrap(10));
        assert_eq!((k_max_step + 10) as i64, unwrapper.unwrap(k_max_step + 10));

        assert_eq!(k_max as i64 + 3, unwrapper.peek_unwrap(2));
        unwrapper.reset();
        // After reset() the range is reset back to the start.
        assert_eq!(2, unwrapper.peek_unwrap(2));
    }

    #[test]
    fn seq_num_unwrapper_peek_unwrap() {
        let k_max = u16::MAX;
        let k_max_step = k_max / 2;
        let k_max_dist = k_max_step + 1;
        let mut unwrapper = SeqNumUnwrapper::<u16>::new();
        // No previous unwraps, so peek_unwrap(x) == x.
        assert_eq!(10, unwrapper.peek_unwrap(10));
        assert_eq!(
            (k_max_dist + 10) as i64,
            unwrapper.peek_unwrap(k_max_dist + 10)
        );

        assert_eq!(10, unwrapper.unwrap(10));
        assert_eq!(12, unwrapper.peek_unwrap(12));
        // State should not have updated, so k_max_dist + 12 should be negative.
        assert_eq!(-(k_max_dist as i64) + 12, unwrapper.unwrap(k_max_dist + 12));

        // Test peek_unwrap after around.
        unwrapper.reset();
        assert_eq!(k_max_step as i64, unwrapper.unwrap(k_max_step));
        assert_eq!(2 * k_max_step as i64, unwrapper.unwrap(2 * k_max_step));
        assert_eq!(k_max as i64 + 1, unwrapper.peek_unwrap(0));
        // Wrap back to last range.
        assert_eq!((k_max - 3) as i64, unwrapper.peek_unwrap(k_max - 3));
    }

    #[test]
    fn rtp_timestamp_unwrapper_wraps_forward() {
        let mut unwrapper = RtpTimestampUnwrapper::new();
        assert_eq!(u32::MAX as i64, unwrapper.unwrap(u32::MAX));
        assert_eq!(u32::MAX as i64 + 961, unwrapper.unwrap(960));
    }

    // safe_conversions / safe_minmax / checks

    #[test]
    fn saturated_cast_clamps() {
        assert_eq!(i16::MAX, saturated_cast::<i16, i32>(40_000));
        assert_eq!(i16::MIN, saturated_cast::<i16, i64>(-40_000));
        assert_eq!(123i16, saturated_cast::<i16, i32>(123));
        assert_eq!(0usize, saturated_cast::<usize, i64>(-5));
        assert_eq!(i32::MAX, saturated_cast::<i32, usize>(usize::MAX));
    }

    #[test]
    fn dchecked_and_checked_cast_pass_values_that_fit() {
        assert_eq!(7i32, dchecked_cast::<i32, usize>(7));
        assert_eq!(-7i16, checked_cast::<i16, i32>(-7));
    }

    #[test]
    #[should_panic]
    fn checked_cast_panics_out_of_range() {
        let _ = checked_cast::<i16, i32>(1 << 20);
    }

    #[test]
    fn safe_clamp_min_max() {
        assert_eq!(0, safe_clamp(-3, 0, 10));
        assert_eq!(10, safe_clamp(30, 0, 10));
        assert_eq!(4, safe_clamp(4, 0, 10));
        assert_eq!(2usize, safe_min(2usize, 9));
        assert_eq!(9usize, safe_max(2usize, 9));
    }

    #[test]
    fn checked_div_exact_divides() {
        assert_eq!(48, checked_div_exact(48000, 1000));
    }

    #[test]
    #[should_panic]
    fn checked_div_exact_panics_on_remainder() {
        let _ = checked_div_exact(48001, 1000);
    }
}
