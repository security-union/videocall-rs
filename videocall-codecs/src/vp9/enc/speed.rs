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
//! encoder knobs that exist today: the integer-pel motion search range and a
//! ZEROMV-first early-exit threshold. Presets 7-8 are the realtime tier the app
//! defaults to; 5-6 widen the search for a little more quality at more cost.
//!
//! Presets 0-4 (quarter-pel, recursive partition search, extra modes) are a
//! quality follow-up and currently clamp to 5. Out-of-range values clamp to the
//! `[0, 8]` contract like libvpx. A `_max_partition_16x16` field is carried but
//! unused until variance-based 16x16 partitioning lands.

/// Resolved per-frame speed knobs for one `cpu_used` level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedFeatures {
    /// Integer-pel motion search half-range in pixels (`±range`).
    pub search_range_px: i32,
    /// If the ZEROMV (no-motion) SAD for an 8x8 luma block is at or below this,
    /// the motion search returns ZEROMV without refining. `0` disables the
    /// early exit. Larger = more aggressive (faster, slightly lower quality).
    pub zeromv_early_exit_sad: u32,
    /// Placeholder for the future variance-based 16x16 partition decision. Always
    /// `false` today (the encoder is fixed full-split to 8x8); reserved so the
    /// preset table already distinguishes the tiers that will enable it.
    pub max_partition_16x16: bool,
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
                max_partition_16x16: false,
            },
            7 => SpeedFeatures {
                search_range_px: 16,
                zeromv_early_exit_sad: 128,
                max_partition_16x16: false,
            },
            // 5-6: wider search, less aggressive early exit.
            6 => SpeedFeatures {
                search_range_px: 24,
                zeromv_early_exit_sad: 64,
                max_partition_16x16: false,
            },
            _ => SpeedFeatures {
                search_range_px: 24,
                zeromv_early_exit_sad: 0,
                max_partition_16x16: false,
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
