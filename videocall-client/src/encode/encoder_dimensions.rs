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

/// Returns the `(width, height)` the encoder should be configured with for a
/// `frame_width` x `frame_height` frame under a `cap_width` x `cap_height`
/// tier cap, or `None` when the frame has no size yet.
///
/// The tier caps (`VIDEO_QUALITY_TIERS`, `SCREEN_QUALITY_TIERS`) are all
/// written landscape (1280x720, 854x480, ...). They describe a resolution
/// class, not an orientation, so the cap is applied orientation-agnostic: the
/// frame's long side must fit the cap's long side and its short side the
/// cap's short side. A portrait 720x1280 phone frame fits a 1280x720 tier
/// as-is instead of being squashed to 720x720.
///
/// Both sides are scaled by one factor, so the aspect ratio is preserved,
/// then rounded to the nearest even number. VP8/VP9 encode 4:2:0, and some
/// hardware encoders reject odd sizes. Rounding never goes past the cap or
/// the frame itself. A cap of 0 on either side means "no cap" (camera native
/// resolution), matching `CameraEncoder::tier_max_width`.
///
/// The result never exceeds the frame on either side. A 1 px side stays 1 px
/// (the only case where the result can be odd).
pub(crate) fn fit_to_tier(
    frame_width: u32,
    frame_height: u32,
    cap_width: u32,
    cap_height: u32,
) -> Option<(u32, u32)> {
    if frame_width == 0 || frame_height == 0 {
        return None;
    }
    // Turn the cap to match the frame's orientation.
    let (limit_width, limit_height) = if cap_width == 0 || cap_height == 0 {
        (frame_width, frame_height)
    } else {
        let (cap_long, cap_short) = (cap_width.max(cap_height), cap_width.min(cap_height));
        if frame_width >= frame_height {
            (cap_long, cap_short)
        } else {
            (cap_short, cap_long)
        }
    };

    // One scale factor for both sides, as numerator / denominator, never
    // above 1: the smaller of limit_width / frame_width and
    // limit_height / frame_height.
    let (numerator, denominator) = if frame_width <= limit_width && frame_height <= limit_height {
        (1, 1)
    } else if limit_width as u64 * frame_height as u64 <= limit_height as u64 * frame_width as u64 {
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
        // A side that scales to 0 becomes 2, unless the frame or cap side is
        // 1 px: then it stays 1 (odd, but never larger than the source).
        let floor = 2.min(ceiling) as u64;
        nearest_even.min(limit_even).max(floor) as u32
    };

    Some((
        scale_side(frame_width, limit_width),
        scale_side(frame_height, limit_height),
    ))
}

/// The size `VideoEncoder` scales to its configured size: the frame's visible
/// rect, or its display size when the browser gives no visible rect.
pub(crate) fn visible_size(frame: &web_sys::VideoFrame) -> (u32, u32) {
    match frame.visible_rect() {
        Some(rect) if rect.width() > 0.0 && rect.height() > 0.0 => {
            (rect.width() as u32, rect.height() as u32)
        }
        _ => (frame.display_width(), frame.display_height()),
    }
}

#[cfg(test)]
mod tests {
    use super::fit_to_tier;

    fn aspect(dims: (u32, u32)) -> f64 {
        dims.0 as f64 / dims.1 as f64
    }

    #[test]
    fn portrait_frame_under_landscape_cap_keeps_aspect() {
        // iPhone portrait camera, top tier.
        let dims = fit_to_tier(720, 1280, 1280, 720).unwrap();
        assert_eq!(dims, (720, 1280), "fits once the cap is turned to match");

        // Same frame under a lower tier: must shrink, not squash.
        let dims = fit_to_tier(720, 1280, 640, 360).unwrap();
        assert_eq!(dims, (360, 640));
    }

    #[test]
    fn landscape_frame_under_portrait_cap_keeps_aspect() {
        let dims = fit_to_tier(1920, 1080, 720, 1280).unwrap();
        assert_eq!(dims, (1280, 720));
    }

    #[test]
    fn landscape_frame_over_landscape_cap_scales_both_sides() {
        let dims = fit_to_tier(1920, 1080, 1280, 720).unwrap();
        assert_eq!(dims, (1280, 720));
        // 4:3 camera under a 16:9 cap is limited by the short side.
        let dims = fit_to_tier(1280, 960, 1280, 720).unwrap();
        assert_eq!(dims, (960, 720));
        assert!((aspect(dims) - 4.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn frame_smaller_than_cap_is_unchanged() {
        assert_eq!(fit_to_tier(640, 480, 1280, 720), Some((640, 480)));
        assert_eq!(fit_to_tier(480, 640, 1280, 720), Some((480, 640)));
    }

    #[test]
    fn results_are_even() {
        // 720x1280 into 426x240: exact scale gives 239.6x426.
        let dims = fit_to_tier(720, 1280, 426, 240).unwrap();
        assert_eq!(dims, (240, 426));

        // 1280x720 into 854x480: exact scale gives 853.3x480.
        let dims = fit_to_tier(1280, 720, 854, 480).unwrap();
        assert_eq!(dims, (854, 480));

        // An odd native frame below the cap is rounded down to even.
        assert_eq!(fit_to_tier(641, 481, 1280, 720), Some((640, 480)));

        // An odd frame that needs scaling: 1281x721 into 854x480. The short
        // side limits (480/721), exact scale gives 852.8x480.
        assert_eq!(fit_to_tier(1281, 721, 854, 480), Some((852, 480)));
        assert_eq!(fit_to_tier(721, 1281, 854, 480), Some((480, 852)));
    }

    #[test]
    fn never_upscales_past_the_frame() {
        assert_eq!(fit_to_tier(1, 1, 1280, 720), Some((1, 1)));
        assert_eq!(fit_to_tier(1, 720, 1280, 720), Some((1, 720)));
        // A side that would scale to 0 is held at 2, which the frame has.
        assert_eq!(fit_to_tier(4000, 2, 1280, 720), Some((1280, 2)));
        let frames = [
            (1, 1),
            (1, 3),
            (3, 1),
            (2, 2),
            (3, 3),
            (5, 1281),
            (1281, 721),
        ];
        for &(fw, fh) in &frames {
            let (w, h) = fit_to_tier(fw, fh, 854, 480).unwrap();
            assert!(w <= fw && h <= fh, "{fw}x{fh} upscaled to {w}x{h}");
        }
    }

    #[test]
    fn aspect_ratio_is_preserved_within_rounding() {
        let frames = [
            (720, 1280),
            (1280, 720),
            (1080, 1920),
            (1280, 960),
            (480, 640),
        ];
        let caps = [(1280, 720), (854, 480), (640, 360), (426, 240)];
        for &(fw, fh) in &frames {
            for &(cw, ch) in &caps {
                let (w, h) = fit_to_tier(fw, fh, cw, ch).unwrap();
                let long_cap = cw.max(ch);
                let short_cap = cw.min(ch);
                assert!(
                    w.max(h) <= long_cap && w.min(h) <= short_cap,
                    "{fw}x{fh} under {cw}x{ch} gave {w}x{h}, outside the cap"
                );
                assert!(w % 2 == 0 && h % 2 == 0, "{w}x{h} is not even");
                // Rounding to even moves each side by at most 1 px, so the
                // aspect ratio stays within 1% at these sizes.
                let drift = (aspect((w, h)) - aspect((fw, fh))).abs() / aspect((fw, fh));
                assert!(
                    drift < 0.01,
                    "{fw}x{fh} under {cw}x{ch} gave {w}x{h}, aspect drift {drift}"
                );
            }
        }
    }

    #[test]
    fn zero_sized_frame_has_no_size() {
        assert_eq!(fit_to_tier(0, 720, 1280, 720), None);
        assert_eq!(fit_to_tier(1280, 0, 1280, 720), None);
    }

    #[test]
    fn zero_cap_means_native_resolution() {
        assert_eq!(fit_to_tier(1920, 1080, 0, 0), Some((1920, 1080)));
    }
}
