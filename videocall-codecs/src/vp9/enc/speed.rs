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

//! `cpu_used` speed presets.
//!
//! Maps the app's `cpu_used` (0 = slowest/best, 8 = fastest/worst) onto the
//! encoder knobs that exist today: the integer-pel motion search range, a
//! ZEROMV-first early-exit threshold, and the 16x16-vs-8x8 partition decision.
//! Presets 7-8 are the realtime tier the app defaults to; 5-6 widen the search
//! for a little more quality at more cost.
//!
//! ## 16x16 partitions
//!
//! Every preset uses `BLOCK_16X16` leaves as the base partition (a 16x16 owns a
//! 2x2 mode-info region, luma coded as four 8x8 transforms, chroma as one 8x8),
//! which quarters the per-block partition/mode/skip signalling floor relative to
//! the old fixed full-split-to-8x8 structure — the change that makes 500 kbps at
//! 640x480 reachable. Keyframes always use 16x16 when the block fits the frame:
//! the luma reconstruction is bit-identical to the 8x8-split path (same four 8x8
//! DC-predicted transforms in the same raster order), so splitting an intra 16x16
//! would only spend more bits for the same pixels. Inter blocks split a 16x16
//! down to four 8x8 leaves only when the block's best motion-compensated SAD
//! exceeds [`SpeedFeatures::split_16x16_sad`] — a quality valve for regions that
//! a single motion vector predicts poorly (e.g. uncovered background). The
//! realtime presets 7-8 disable the valve (`u32::MAX`) so the floor stays minimal;
//! presets 5-6 lower it to trade a little rate for quality on busy motion.
//!
//! Presets 0-4 (quarter-pel, recursive partition search, extra modes) are a
//! quality follow-up and currently clamp to 5. Out-of-range values clamp to the
//! `[0, 8]` contract like libvpx.

/// Resolved per-frame speed knobs for one `cpu_used` level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedFeatures {
    /// Integer-pel motion search half-range in pixels (`±range`).
    pub search_range_px: i32,
    /// If the ZEROMV (no-motion) SAD for an 8x8 luma block is at or below this,
    /// the motion search returns ZEROMV without refining. `0` disables the
    /// early exit. Larger = more aggressive (faster, slightly lower quality).
    pub zeromv_early_exit_sad: u32,
    /// Whether `BLOCK_16X16` leaves are used as the base partition. True for
    /// every implemented preset (the 8x8-only structure is legacy); kept as a
    /// field so the intent is explicit at each call site.
    pub max_partition_16x16: bool,
    /// Inter split valve: a 16x16 whose best motion-compensated luma SAD exceeds
    /// this is split into four 8x8 leaves for quality. `u32::MAX` disables the
    /// split (always keep 16x16 when it fits), minimising the signalling floor.
    /// Keyframes ignore this — intra 16x16 is always kept when it fits.
    pub split_16x16_sad: u32,
}

impl SpeedFeatures {
    /// Resolve the knobs for a `cpu_used` value, clamping out-of-range and
    /// not-yet-implemented (0-4) levels up to 5.
    pub fn for_cpu_used(cpu_used: u8) -> Self {
        // Quality tiers 0-4 are not implemented yet; treat them as 5.
        let level = cpu_used.clamp(0, 8).max(5);
        match level {
            8 => SpeedFeatures {
                search_range_px: 8,
                zeromv_early_exit_sad: 256,
                max_partition_16x16: true,
                split_16x16_sad: u32::MAX,
            },
            7 => SpeedFeatures {
                search_range_px: 16,
                zeromv_early_exit_sad: 128,
                max_partition_16x16: true,
                split_16x16_sad: u32::MAX,
            },
            // 5-6: wider search, less aggressive early exit, and an active split
            // valve (a 16x16 luma SAD above ~8/pixel splits to 8x8 for quality).
            6 => SpeedFeatures {
                search_range_px: 24,
                zeromv_early_exit_sad: 64,
                max_partition_16x16: true,
                split_16x16_sad: 16 * 16 * 8,
            },
            _ => SpeedFeatures {
                search_range_px: 24,
                zeromv_early_exit_sad: 0,
                max_partition_16x16: true,
                split_16x16_sad: 16 * 16 * 6,
            },
        }
    }
}

impl Default for SpeedFeatures {
    /// The app default (`cpu_used = 7`).
    fn default() -> Self {
        Self::for_cpu_used(7)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_preset_7() {
        assert_eq!(SpeedFeatures::default(), SpeedFeatures::for_cpu_used(7));
        assert_eq!(SpeedFeatures::default().search_range_px, 16);
    }

    #[test]
    fn faster_preset_narrows_search() {
        let s8 = SpeedFeatures::for_cpu_used(8);
        let s7 = SpeedFeatures::for_cpu_used(7);
        let s5 = SpeedFeatures::for_cpu_used(5);
        assert!(s8.search_range_px <= s7.search_range_px);
        assert!(s7.search_range_px <= s5.search_range_px);
    }

    #[test]
    fn faster_preset_more_aggressive_early_exit() {
        let s8 = SpeedFeatures::for_cpu_used(8);
        let s5 = SpeedFeatures::for_cpu_used(5);
        assert!(s8.zeromv_early_exit_sad >= s5.zeromv_early_exit_sad);
    }

    #[test]
    fn all_presets_use_16x16_partitions() {
        for cpu in 0..=8u8 {
            assert!(
                SpeedFeatures::for_cpu_used(cpu).max_partition_16x16,
                "cpu_used {cpu} should use 16x16 leaves"
            );
        }
    }

    #[test]
    fn realtime_presets_never_split_16x16() {
        // Presets 7-8 keep every fitting 16x16 as a leaf (minimal signalling
        // floor); presets 5-6 have a finite split valve for quality.
        assert_eq!(SpeedFeatures::for_cpu_used(8).split_16x16_sad, u32::MAX);
        assert_eq!(SpeedFeatures::for_cpu_used(7).split_16x16_sad, u32::MAX);
        assert!(SpeedFeatures::for_cpu_used(6).split_16x16_sad < u32::MAX);
        assert!(SpeedFeatures::for_cpu_used(5).split_16x16_sad < u32::MAX);
    }

    #[test]
    fn low_presets_clamp_to_5() {
        for cpu in 0..=5u8 {
            assert_eq!(
                SpeedFeatures::for_cpu_used(cpu),
                SpeedFeatures::for_cpu_used(5),
                "cpu_used {cpu} should clamp to preset 5"
            );
        }
    }

    #[test]
    fn out_of_range_clamps_to_8() {
        assert_eq!(
            SpeedFeatures::for_cpu_used(200),
            SpeedFeatures::for_cpu_used(8)
        );
    }
}
