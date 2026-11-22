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

//! Encoder output size for a captured frame under a quality tier cap.

use web_sys::{MediaTrackSettings, VideoFrame};

/// A frame or encoder size in pixels. A zero side means "no size yet".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameSize {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

impl FrameSize {
    pub(crate) const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub(crate) fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The size `VideoEncoder` scales `frame` from: its visible rect, or its
    /// display size when the browser gives no visible rect.
    pub(crate) fn of(frame: &VideoFrame) -> Self {
        match frame.visible_rect() {
            Some(rect) if rect.width() > 0.0 && rect.height() > 0.0 => {
                Self::new(rect.width() as u32, rect.height() as u32)
            }
            _ => Self::new(frame.display_width(), frame.display_height()),
        }
    }

    /// The size a track reports in its settings; empty when it reports none.
    pub(crate) fn of_settings(settings: &MediaTrackSettings) -> Self {
        Self::from_reported(settings.get_width(), settings.get_height())
    }

    /// A size from reported sides; a missing or negative side becomes 0.
    fn from_reported(width: Option<i32>, height: Option<i32>) -> Self {
        let side = |value: Option<i32>| value.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        Self::new(side(width), side(height))
    }

    /// The encoder size for a frame of this size under a quality tier `cap`.
    ///
    /// The tier caps (the camera's rate-control tiers, `SCREEN_QUALITY_TIERS`) are
    /// written landscape but describe a resolution class, so the cap is
    /// turned to match the frame: a portrait 720x1280 frame fits a 1280x720
    /// tier as-is instead of being squashed to 720x720.
    ///
    /// Both sides scale by one factor (the aspect ratio is kept), then round
    /// to the nearest even number, since VP8/VP9 encode 4:2:0 and some
    /// hardware encoders reject odd sizes. The result never exceeds the cap
    /// or the frame; a 1 px side stays 1 px. A cap with a zero side means no
    /// cap. An empty size is returned unchanged.
    pub(crate) fn fit_within(self, cap: FrameSize) -> FrameSize {
        if self.is_empty() {
            return self;
        }
        let FrameSize {
            width: frame_width,
            height: frame_height,
        } = self;
        // Turn the cap to match the frame's orientation.
        let (limit_width, limit_height) = if cap.is_empty() {
            (frame_width, frame_height)
        } else {
            let (cap_long, cap_short) = (cap.width.max(cap.height), cap.width.min(cap.height));
            if frame_width >= frame_height {
                (cap_long, cap_short)
            } else {
                (cap_short, cap_long)
            }
        };

        // One scale factor for both sides, as numerator / denominator, never
        // above 1: the smaller of limit_width / frame_width and
        // limit_height / frame_height.
        let (numerator, denominator) = if frame_width <= limit_width && frame_height <= limit_height
        {
            (1, 1)
        } else if limit_width as u64 * frame_height as u64
            <= limit_height as u64 * frame_width as u64
        {
            (limit_width as u64, frame_width as u64)
        } else {
            (limit_height as u64, frame_height as u64)
        };

        let scale_side = |side: u32, limit: u32| -> u32 {
            // round(side * numerator / denominator / 2) * 2, in integers.
            let nearest_even = ((side as u64 * numerator + denominator) / (2 * denominator)) * 2;
            // Never past the cap, and never upscale past the frame itself.
            let ceiling = limit.min(side);
            let limit_even = (ceiling & !1) as u64;
            // A side that scales to 0 becomes 2, unless the frame or cap side
            // is 1 px: then it stays 1 (odd, but never larger than the source).
            let floor = 2.min(ceiling) as u64;
            nearest_even.min(limit_even).max(floor) as u32
        };

        FrameSize::new(
            scale_side(frame_width, limit_width),
            scale_side(frame_height, limit_height),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::FrameSize;

    fn fit(width: u32, height: u32, cap_width: u32, cap_height: u32) -> FrameSize {
        FrameSize::new(width, height).fit_within(FrameSize::new(cap_width, cap_height))
    }

    fn aspect(size: FrameSize) -> f64 {
        size.width as f64 / size.height as f64
    }

    #[test]
    fn portrait_frame_under_landscape_cap_keeps_aspect() {
        // iPhone portrait camera, top tier: fits once the cap is turned to match.
        assert_eq!(fit(720, 1280, 1280, 720), FrameSize::new(720, 1280));
        // Same frame under a lower tier: must shrink, not squash.
        assert_eq!(fit(720, 1280, 640, 360), FrameSize::new(360, 640));
    }

    #[test]
    fn landscape_frame_under_portrait_cap_keeps_aspect() {
        assert_eq!(fit(1920, 1080, 720, 1280), FrameSize::new(1280, 720));
    }

    #[test]
    fn landscape_frame_over_landscape_cap_scales_both_sides() {
        assert_eq!(fit(1920, 1080, 1280, 720), FrameSize::new(1280, 720));
        // 4:3 camera under a 16:9 cap is limited by the short side.
        let size = fit(1280, 960, 1280, 720);
        assert_eq!(size, FrameSize::new(960, 720));
        assert!((aspect(size) - 4.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn frame_smaller_than_cap_is_unchanged() {
        assert_eq!(fit(640, 480, 1280, 720), FrameSize::new(640, 480));
        assert_eq!(fit(480, 640, 1280, 720), FrameSize::new(480, 640));
    }

    #[test]
    fn results_are_even() {
        // 720x1280 into 426x240: exact scale gives 239.6x426.
        assert_eq!(fit(720, 1280, 426, 240), FrameSize::new(240, 426));
        // 1280x720 into 854x480: exact scale gives 853.3x480.
        assert_eq!(fit(1280, 720, 854, 480), FrameSize::new(854, 480));
        // An odd native frame below the cap is rounded down to even.
        assert_eq!(fit(641, 481, 1280, 720), FrameSize::new(640, 480));
        // An odd frame that needs scaling: 1281x721 into 854x480. The short
        // side limits (480/721), exact scale gives 852.8x480.
        assert_eq!(fit(1281, 721, 854, 480), FrameSize::new(852, 480));
        assert_eq!(fit(721, 1281, 854, 480), FrameSize::new(480, 852));
    }

    #[test]
    fn never_upscales_past_the_frame() {
        assert_eq!(fit(1, 1, 1280, 720), FrameSize::new(1, 1));
        assert_eq!(fit(1, 720, 1280, 720), FrameSize::new(1, 720));
        // A side that would scale to 0 is held at 2, which the frame has.
        assert_eq!(fit(4000, 2, 1280, 720), FrameSize::new(1280, 2));
        let frames = [
            (1, 1),
            (1, 3),
            (3, 1),
            (2, 2),
            (3, 3),
            (5, 1281),
            (1281, 721),
        ];
        for &(width, height) in &frames {
            let size = fit(width, height, 854, 480);
            assert!(
                size.width <= width && size.height <= height,
                "{width}x{height} upscaled to {size:?}"
            );
        }
    }

    #[test]
    fn aspect_ratio_is_preserved_within_rounding() {
        let frames = [
            FrameSize::new(720, 1280),
            FrameSize::new(1280, 720),
            FrameSize::new(1080, 1920),
            FrameSize::new(1280, 960),
            FrameSize::new(480, 640),
        ];
        let caps = [
            FrameSize::new(1280, 720),
            FrameSize::new(854, 480),
            FrameSize::new(640, 360),
            FrameSize::new(426, 240),
        ];
        for &frame in &frames {
            for &cap in &caps {
                let size = frame.fit_within(cap);
                assert!(
                    size.width.max(size.height) <= cap.width.max(cap.height)
                        && size.width.min(size.height) <= cap.width.min(cap.height),
                    "{frame:?} under {cap:?} gave {size:?}, outside the cap"
                );
                assert!(
                    size.width % 2 == 0 && size.height % 2 == 0,
                    "{size:?} is not even"
                );
                // Rounding to even moves each side by at most 1 px, so the
                // aspect ratio stays within 1% at these sizes.
                let drift = (aspect(size) - aspect(frame)).abs() / aspect(frame);
                assert!(
                    drift < 0.01,
                    "{frame:?} under {cap:?} gave {size:?}, aspect drift {drift}"
                );
            }
        }
    }

    #[test]
    fn zero_sized_frame_stays_empty() {
        assert_eq!(fit(0, 720, 1280, 720), FrameSize::new(0, 720));
        assert_eq!(fit(1280, 0, 1280, 720), FrameSize::new(1280, 0));
        assert!(FrameSize::new(0, 720).is_empty());
        assert!(FrameSize::new(1280, 0).is_empty());
        assert!(!FrameSize::new(1, 1).is_empty());
    }

    #[test]
    fn zero_cap_means_native_resolution() {
        assert_eq!(fit(1920, 1080, 0, 0), FrameSize::new(1920, 1080));
    }

    #[test]
    fn reported_size_with_a_missing_or_negative_side_is_empty() {
        assert_eq!(
            FrameSize::from_reported(Some(1280), Some(720)),
            FrameSize::new(1280, 720)
        );
        assert_eq!(
            FrameSize::from_reported(Some(1280), None),
            FrameSize::new(1280, 0)
        );
        assert!(FrameSize::from_reported(None, None).is_empty());
        assert!(FrameSize::from_reported(Some(-1), Some(720)).is_empty());
    }
}
