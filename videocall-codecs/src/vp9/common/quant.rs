/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! VP9 quantizer parameters (8-bit / Profile 0).
//!
//! [`dc_quant`]/[`ac_quant`] port `vp9_dc_quant`/`vp9_ac_quant`
//! (`vp9/common/vp9_quant_common.c`) for the 8-bit path, using the generated
//! dequant lookups. [`quantizer_to_qindex`] ports the encoder's 0–63 quantizer
//! → qindex map (`vp9/encoder/vp9_quantize.c`).

use super::generated::{AC_QLOOKUP, DC_QLOOKUP};

/// Maximum qindex (`MAXQ`); qindex range is `0..=255`.
pub const MAXQ: i32 = 255;

/// Clamp `v` to `[lo, hi]` (libvpx `clamp`).
#[inline]
pub fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    v.max(lo).min(hi)
}

/// DC dequant step for `qindex + delta` (8-bit). Port of `vp9_dc_quant`.
pub fn dc_quant(qindex: i32, delta: i32) -> i16 {
    DC_QLOOKUP[clamp(qindex + delta, 0, MAXQ) as usize]
}

/// AC dequant step for `qindex + delta` (8-bit). Port of `vp9_ac_quant`.
pub fn ac_quant(qindex: i32, delta: i32) -> i16 {
    AC_QLOOKUP[clamp(qindex + delta, 0, MAXQ) as usize]
}

/// Maps a libvpx 0–63 quantizer to a base qindex. From `vp9/encoder/vp9_quantize.c`.
#[rustfmt::skip]
pub const QUANTIZER_TO_QINDEX: [i32; 64] = [
    0,   4,   8,   12,  16,  20,  24,  28,  32,  36,  40,  44,  48,
    52,  56,  60,  64,  68,  72,  76,  80,  84,  88,  92,  96,  100,
    104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, 148, 152,
    156, 160, 164, 168, 172, 176, 180, 184, 188, 192, 196, 200, 204,
    208, 212, 216, 220, 224, 228, 232, 236, 240, 244, 249, 255,
];

/// Map a 0–63 `quantizer` to its base qindex (`vp9_quantizer_to_qindex`).
/// Out-of-range inputs are clamped to the table bounds.
pub fn quantizer_to_qindex(quantizer: i32) -> i32 {
    let idx = clamp(quantizer, 0, 63) as usize;
    QUANTIZER_TO_QINDEX[idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_ac_quant_known_values() {
        // qindex 0 → smallest step (4); qindex 255 → largest.
        assert_eq!(dc_quant(0, 0), 4);
        assert_eq!(dc_quant(255, 0), 1336);
        assert_eq!(ac_quant(0, 0), 4);
        assert_eq!(ac_quant(255, 0), 1828);
    }

    #[test]
    fn quant_clamps_qindex_plus_delta() {
        // Below 0 and above MAXQ clamp to the endpoints.
        assert_eq!(dc_quant(0, -50), dc_quant(0, 0));
        assert_eq!(ac_quant(255, 100), ac_quant(255, 0));
    }

    #[test]
    fn quantizer_to_qindex_known() {
        assert_eq!(quantizer_to_qindex(0), 0);
        assert_eq!(quantizer_to_qindex(40), 160);
        assert_eq!(quantizer_to_qindex(62), 249);
        assert_eq!(quantizer_to_qindex(63), 255);
        // Clamped.
        assert_eq!(quantizer_to_qindex(100), 255);
        assert_eq!(quantizer_to_qindex(-5), 0);
    }
}
