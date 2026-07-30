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
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

const DEFAULT_CAPTURE_WIDTH: u32 = 640;
const DEFAULT_CAPTURE_HEIGHT: u32 = 480;

/// Resolve capture dimensions as a pair so the selected aspect ratio stays coherent.
///
/// Capture tracks may omit dimensions before their first frame or after ending.
/// Prefer a complete settings pair, then a complete preview-video pair, then a
/// safe default pair.
pub(crate) fn resolve_capture_dimensions(
    settings_w: Option<f64>,
    settings_h: Option<f64>,
    video_w: u32,
    video_h: u32,
) -> (u32, u32) {
    match (settings_w, settings_h) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => (width as u32, height as u32),
        _ if video_w > 0 && video_h > 0 => (video_w, video_h),
        _ => (DEFAULT_CAPTURE_WIDTH, DEFAULT_CAPTURE_HEIGHT),
    }
}

/// Decide whether the per-frame source-dimension stamp should be updated.
///
/// The camera encode loop seeds the #1196 source-aspect atomics from a
/// possibly-fallback value at acquisition, then corrects them from decoded
/// frames. Returns `Some((frame_w, frame_h))` only when the frame reports a
/// complete, non-zero pair that DIFFERS from the current stamp — so a
/// steady-state stream (unchanged dims) stores nothing and a blank/dimensionless
/// frame never clobbers a known stamp.
pub(crate) fn corrected_source_dims(
    frame_w: u32,
    frame_h: u32,
    current_w: u32,
    current_h: u32,
) -> Option<(u32, u32)> {
    if frame_w > 0 && frame_h > 0 && (frame_w != current_w || frame_h != current_h) {
        Some((frame_w, frame_h))
    } else {
        None
    }
}

/// Resolve the value to STAMP as the source aspect (issue #1196) from track
/// settings alone.
///
/// Unlike [`resolve_capture_dimensions`] (which falls back to a safe non-zero
/// pair so the encoder ladder can always be constructed), the stamp must never
/// fabricate an aspect: when `getSettings()` omits a complete pair this returns
/// `(0, 0)` = "unknown", which the proto3 default-skip drops so receivers never
/// see a wrong source aspect. Used by the screen encoder, which seeds the stamp
/// only at (re)acquisition and does not per-frame-correct it.
pub(crate) fn settings_source_stamp(
    settings_w: Option<f64>,
    settings_h: Option<f64>,
) -> (u32, u32) {
    match (settings_w, settings_h) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => (width as u32, height as u32),
        _ => (0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrected_source_dims_updates_on_first_real_frame() {
        // Seeded from the 640x480 fallback; the first real decoded frame
        // corrects the stamp to the true source aspect.
        assert_eq!(
            corrected_source_dims(1280, 720, 640, 480),
            Some((1280, 720))
        );
    }

    #[test]
    fn corrected_source_dims_none_when_unchanged() {
        // Steady state: no store.
        assert_eq!(corrected_source_dims(1280, 720, 1280, 720), None);
    }

    #[test]
    fn corrected_source_dims_none_for_blank_frame() {
        // A dimensionless/blank frame must not clobber a known stamp — guards
        // the `> 0` check on BOTH axes independently.
        assert_eq!(corrected_source_dims(0, 0, 1280, 720), None);
        assert_eq!(corrected_source_dims(1280, 0, 640, 480), None);
        assert_eq!(corrected_source_dims(0, 720, 640, 480), None);
    }

    #[test]
    fn corrected_source_dims_some_on_single_axis_change() {
        // Only one axis changed: the OR change-gate must still fire (guards
        // `||` -> `&&` mutation).
        assert_eq!(
            corrected_source_dims(1280, 480, 640, 480),
            Some((1280, 480))
        );
        assert_eq!(corrected_source_dims(640, 720, 640, 480), Some((640, 720)));
    }

    #[test]
    fn settings_source_stamp_uses_complete_pair() {
        assert_eq!(
            settings_source_stamp(Some(1280.0), Some(720.0)),
            (1280, 720)
        );
    }

    #[test]
    fn settings_source_stamp_unknown_when_incomplete() {
        // Absent, partial, or zero settings -> (0,0) = proto3 skip, never a
        // fabricated aspect.
        assert_eq!(settings_source_stamp(None, None), (0, 0));
        assert_eq!(settings_source_stamp(Some(1280.0), None), (0, 0));
        assert_eq!(settings_source_stamp(Some(0.0), Some(0.0)), (0, 0));
        // Single zero axis must still be "unknown" (guards `> 0.0` -> `>= 0.0`).
        assert_eq!(settings_source_stamp(Some(0.0), Some(720.0)), (0, 0));
    }
}
