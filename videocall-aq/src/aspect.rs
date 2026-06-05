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
 */

//! Aspect-ratio-preserving dimension fitting for the video encoders.
//!
//! ## Why this exists (issue #1037)
//!
//! The adaptive-quality (AQ) tiers express their resolution ceilings as a
//! `max_width` / `max_height` pair (see [`crate::constants`]). Those ceilings
//! are authored at 16:9 (e.g. `1280x720`, `1920x1080`). Real capture sources
//! are frequently *not* 16:9:
//!
//! - Webcams are very often 4:3 (e.g. `640x480`, `1280x960`).
//! - Desktop / window captures can be 16:10 (`1920x1200`), 21:9, portrait, etc.
//!
//! The old encoder code clamped width and height *independently* against the
//! tier ceiling (`w.min(max_w)`, `h.min(max_h)`). When the source aspect did
//! not match the 16:9 ceiling, the two axes were scaled by different factors,
//! so WebCodecs baked a non-uniform stretch/squash into the encoded stream
//! (peer cameras looked vertically squashed; screen-share looked horizontally
//! stretched). The consumer canvas/CSS is already correct (it honours the
//! source buffer aspect via `object-fit`), so the distortion was purely an
//! encode-side defect.
//!
//! [`fit_within_preserving_aspect`] replaces the per-axis clamp with a single
//! uniform downscale factor, so the encoded frame always carries the *source*
//! aspect ratio. Quality (resolution) may still drop as the tier ceiling
//! tightens, but the displayed aspect ratio never changes — which is the hard
//! requirement from the issue.

/// Round a dimension down to the nearest even value, with a floor of 2.
///
/// Most video codecs we target (VP9, H.264 in 4:2:0 chroma subsampling)
/// require even frame dimensions; some encoder configurations reject odd
/// widths/heights outright. We round **down** (rather than to nearest) so the
/// result can never exceed the tier ceiling it was fitted into — overshooting
/// the ceiling by one pixel would defeat the bandwidth intent of the tier and,
/// on the per-frame path, could ping-pong the "dimensions changed" reconfigure
/// branch. A floor of 2 prevents a degenerate 0-pixel dimension from a very
/// aggressive downscale of a tiny source.
#[inline]
fn round_down_even(v: u32) -> u32 {
    // Clear the low bit (round down to even), then ensure a minimum of 2.
    let even = v & !1;
    even.max(2)
}

/// Fit a source `(src_w, src_h)` inside the box `(max_w, max_h)` using a single
/// uniform downscale factor, preserving the source aspect ratio.
///
/// The scale factor is `s = min(max_w / src_w, max_h / src_h, 1.0)`:
/// - `min` of the two axis ratios guarantees the result fits within *both*
///   `max_w` and `max_h` (the more-constrained axis wins).
/// - Clamping at `1.0` means we **never upscale**: if the source already fits
///   inside the box, it is returned unchanged (modulo even rounding), which
///   avoids spending bandwidth to interpolate a source up to a tier ceiling.
///
/// Both output dimensions are rounded down to even values with a floor of 2
/// (see [`round_down_even`]). Because rounding is *downward*, the postconditions
/// `out_w <= max_w` and `out_h <= max_h` always hold (assuming `max_w, max_h >= 2`,
/// which every real tier satisfies).
///
/// ## Degenerate inputs
///
/// - If `src_w == 0` or `src_h == 0` the source aspect is undefined and we
///   cannot divide by it. We fall back to the box dimensions
///   (`round_down_even(max_w)`, `round_down_even(max_h)`) so the caller still
///   gets a usable, in-bounds config rather than a panic or a zero dimension.
/// - If `max_w == 0` or `max_h == 0` (should never happen for a real tier) we
///   fall back to the even-clamped source so we never emit a zero dimension.
///
/// The function performs no floating-point division by zero: every divisor is
/// guarded above before the ratio computation.
pub fn fit_within_preserving_aspect(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    // Degenerate source: aspect is undefined, fall back to the box.
    if src_w == 0 || src_h == 0 {
        return (round_down_even(max_w), round_down_even(max_h));
    }
    // Degenerate box: should never happen for a real tier, but never divide by
    // zero or emit a zero dimension — fall back to the (even-clamped) source.
    if max_w == 0 || max_h == 0 {
        return (round_down_even(src_w), round_down_even(src_h));
    }

    let src_w_f = src_w as f64;
    let src_h_f = src_h as f64;
    let scale_w = max_w as f64 / src_w_f;
    let scale_h = max_h as f64 / src_h_f;

    // Uniform factor: the most-constrained axis wins, and we never upscale.
    let scale = scale_w.min(scale_h).min(1.0);

    // Round each scaled axis to even (>= 2). round() before round_down_even so
    // sub-pixel scaling lands on the nearest integer first, then we snap down
    // to even to stay within the ceiling.
    let out_w = round_down_even((src_w_f * scale).round() as u32);
    let out_h = round_down_even((src_h_f * scale).round() as u32);

    (out_w, out_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: aspect ratio as f64.
    fn ar(w: u32, h: u32) -> f64 {
        w as f64 / h as f64
    }

    /// Assert that `out` preserves `src`'s aspect ratio within a tolerance that
    /// accounts for even-rounding of small dimensions.
    fn assert_aspect_preserved(src_w: u32, src_h: u32, out_w: u32, out_h: u32) {
        let src_ar = ar(src_w, src_h);
        let out_ar = ar(out_w, out_h);
        let rel_err = (src_ar - out_ar).abs() / src_ar;
        assert!(
            rel_err < 0.02,
            "aspect drift too large: src {src_w}x{src_h} (ar={src_ar:.4}) -> \
             out {out_w}x{out_h} (ar={out_ar:.4}), rel_err={rel_err:.4}"
        );
    }

    fn assert_within_box(out_w: u32, out_h: u32, max_w: u32, max_h: u32) {
        assert!(
            out_w <= max_w && out_h <= max_h,
            "result {out_w}x{out_h} exceeds box {max_w}x{max_h}"
        );
    }

    #[test]
    fn four_three_into_sixteen_nine_preserves_aspect() {
        // 640x480 (4:3) into a 1280x720 (16:9) tier.
        // Height is the binding axis: scale = min(1280/640, 720/480, 1) = min(2, 1.5, 1) = 1.0
        // -> source already fits, returned unchanged (still 4:3), NOT 1280x720.
        let (w, h) = fit_within_preserving_aspect(640, 480, 1280, 720);
        assert_within_box(w, h, 1280, 720);
        assert_aspect_preserved(640, 480, w, h);
        // Must NOT have been stretched to the 16:9 ceiling.
        assert_ne!((w, h), (1280, 720));
        assert_eq!((w, h), (640, 480));
    }

    #[test]
    fn large_four_three_into_sixteen_nine_downscales_uniformly() {
        // 1600x1200 (4:3) into a 1280x720 (16:9) tier. Height binds:
        // scale = min(1280/1600=0.8, 720/1200=0.6, 1) = 0.6
        // -> 960x720, which is 4:3 and fits, NOT 1280x720.
        let (w, h) = fit_within_preserving_aspect(1600, 1200, 1280, 720);
        assert_within_box(w, h, 1280, 720);
        assert_aspect_preserved(1600, 1200, w, h);
        assert_ne!((w, h), (1280, 720));
        assert_eq!((w, h), (960, 720));
    }

    #[test]
    fn sixteen_nine_into_sixteen_nine_passthrough() {
        // Exact-fit / passthrough: 1280x720 into 1280x720.
        let (w, h) = fit_within_preserving_aspect(1280, 720, 1280, 720);
        assert_eq!((w, h), (1280, 720));
    }

    #[test]
    fn larger_sixteen_nine_into_sixteen_nine_downscales_to_ceiling() {
        // 1920x1080 into a 1280x720 tier: scale = 1280/1920 = 0.6667 on both axes.
        // -> 1280x720 (still 16:9).
        let (w, h) = fit_within_preserving_aspect(1920, 1080, 1280, 720);
        assert_within_box(w, h, 1280, 720);
        assert_aspect_preserved(1920, 1080, w, h);
        assert_eq!((w, h), (1280, 720));
    }

    #[test]
    fn sixteen_ten_into_sixteen_nine_preserves_sixteen_ten() {
        // 1920x1200 (16:10) into a 1920x1080 (16:9) tier. Height binds:
        // scale = min(1920/1920=1.0, 1080/1200=0.9, 1) = 0.9
        // -> 1728x1080, which is 16:10 and fits, NOT 1920x1080.
        let (w, h) = fit_within_preserving_aspect(1920, 1200, 1920, 1080);
        assert_within_box(w, h, 1920, 1080);
        assert_aspect_preserved(1920, 1200, w, h);
        assert_ne!((w, h), (1920, 1080));
        assert_eq!((w, h), (1728, 1080));
    }

    #[test]
    fn source_smaller_than_tier_returned_unchanged() {
        // 320x240 into a 1280x720 tier: source already fits, no upscale.
        let (w, h) = fit_within_preserving_aspect(320, 240, 1280, 720);
        assert_eq!((w, h), (320, 240));
    }

    #[test]
    fn portrait_source_into_landscape_tier() {
        // 720x1280 (9:16 portrait) into a 1280x720 (16:9) tier. Width is far
        // less constrained; height binds: scale = min(1280/720=1.78, 720/1280=0.5625, 1)
        // = 0.5625 -> 405->404 (even), 720. Aspect preserved.
        let (w, h) = fit_within_preserving_aspect(720, 1280, 1280, 720);
        assert_within_box(w, h, 1280, 720);
        assert_aspect_preserved(720, 1280, w, h);
    }

    #[test]
    fn output_dimensions_are_even() {
        // Pick a source whose uniform downscale lands on odd raw values.
        // 999x563 into 640x360: scale = min(640/999=0.6406, 360/563=0.6394, 1)
        // = 0.6394 -> 638.8->639 then even->638, 360. Both must be even.
        let (w, h) = fit_within_preserving_aspect(999, 563, 640, 360);
        assert_eq!(w % 2, 0, "width {w} not even");
        assert_eq!(h % 2, 0, "height {h} not even");
        assert_within_box(w, h, 640, 360);
        assert_aspect_preserved(999, 563, w, h);
    }

    #[test]
    fn even_rounding_holds_across_many_sizes() {
        let tiers = [(1920u32, 1080u32), (1280, 720), (640, 360), (320, 180)];
        let sources = [
            (640u32, 480u32),
            (1280, 960),
            (1920, 1200),
            (1366, 768),
            (1440, 900),
            (3840, 2160),
            (1080, 1920),
            (1234, 567),
        ];
        for &(mw, mh) in &tiers {
            for &(sw, sh) in &sources {
                let (w, h) = fit_within_preserving_aspect(sw, sh, mw, mh);
                assert_eq!(w % 2, 0, "width {w} not even for {sw}x{sh}->{mw}x{mh}");
                assert_eq!(h % 2, 0, "height {h} not even for {sw}x{sh}->{mw}x{mh}");
                assert_within_box(w, h, mw, mh);
                assert_aspect_preserved(sw, sh, w, h);
            }
        }
    }

    #[test]
    fn never_upscales() {
        // Source strictly smaller than the box on both axes is never enlarged.
        for &(sw, sh) in &[(2u32, 2u32), (100, 100), (640, 480), (16, 9)] {
            let (w, h) = fit_within_preserving_aspect(sw, sh, 1920, 1080);
            assert!(
                w <= sw.max(2) && h <= sh.max(2),
                "upscaled {sw}x{sh} -> {w}x{h}"
            );
        }
    }

    #[test]
    fn zero_source_does_not_panic_and_falls_back_to_box() {
        // Zero width or height: aspect undefined, fall back to the (even) box.
        assert_eq!(fit_within_preserving_aspect(0, 480, 1280, 720), (1280, 720));
        assert_eq!(fit_within_preserving_aspect(640, 0, 1280, 720), (1280, 720));
        assert_eq!(fit_within_preserving_aspect(0, 0, 1280, 720), (1280, 720));
        // Odd box dimensions round down to even.
        assert_eq!(fit_within_preserving_aspect(0, 0, 1281, 721), (1280, 720));
    }

    #[test]
    fn zero_box_does_not_panic_and_falls_back_to_source() {
        // Degenerate box: never divide by zero, fall back to even-clamped source.
        assert_eq!(fit_within_preserving_aspect(640, 480, 0, 720), (640, 480));
        assert_eq!(fit_within_preserving_aspect(640, 480, 1280, 0), (640, 480));
        assert_eq!(fit_within_preserving_aspect(641, 481, 0, 0), (640, 480));
    }

    #[test]
    fn tiny_source_floors_at_two() {
        // A 1x1 source should never produce a 0 dimension.
        let (w, h) = fit_within_preserving_aspect(1, 1, 1280, 720);
        assert!(w >= 2 && h >= 2, "floor-of-2 violated: {w}x{h}");
    }
}
