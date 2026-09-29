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

use js_sys::Reflect;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{OffscreenCanvas, OffscreenCanvasRenderingContext2d, VideoFrame, VideoFrameInit};

/// Canvas size that renders a frame upright, or `None` when its pixels already are.
///
/// `display_w`/`display_h` are the frame's `displayWidth`/`displayHeight`, which
/// WebCodecs reports with the rotation already applied.
pub(super) fn upright_canvas_dims(
    rotation: f64,
    flip: bool,
    display_w: u32,
    display_h: u32,
) -> Option<(u32, u32)> {
    let quarter_turns = (rotation / 90.0).round() as i64;
    if (quarter_turns.rem_euclid(4) == 0 && !flip) || display_w == 0 || display_h == 0 {
        None
    } else {
        Some((display_w, display_h))
    }
}

/// Bakes a camera frame's `rotation`/`flip` metadata into its pixels, because
/// `VideoEncoder` encodes the unrotated pixels and this pipeline does not carry
/// the orientation to receivers.
pub(super) struct FrameUprighter {
    rotation_key: JsValue,
    flip_key: JsValue,
    surface: Option<(OffscreenCanvas, OffscreenCanvasRenderingContext2d)>,
    failure_logged: bool,
}

impl FrameUprighter {
    pub(super) fn new() -> Self {
        Self {
            rotation_key: JsValue::from_str("rotation"),
            flip_key: JsValue::from_str("flip"),
            surface: None,
            failure_logged: false,
        }
    }

    /// Returns `frame` untouched when it is already upright; otherwise closes it
    /// and returns an upright copy. On failure the original frame is returned.
    pub(super) fn upright(&mut self, frame: VideoFrame) -> VideoFrame {
        let rotation = Reflect::get(&frame, &self.rotation_key)
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let flip = Reflect::get(&frame, &self.flip_key)
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let Some((width, height)) = upright_canvas_dims(
            rotation,
            flip,
            frame.display_width(),
            frame.display_height(),
        ) else {
            return frame;
        };
        match self.draw_upright(&frame, width, height) {
            Ok(upright) => {
                frame.close();
                upright
            }
            Err(e) => {
                if !self.failure_logged {
                    self.failure_logged = true;
                    log::warn!(
                        "CameraEncoder: could not upright a rotation={rotation} flip={flip} frame: {e:?}"
                    );
                }
                frame
            }
        }
    }

    fn draw_upright(
        &mut self,
        frame: &VideoFrame,
        width: u32,
        height: u32,
    ) -> Result<VideoFrame, JsValue> {
        let (canvas, context) = match self.surface.take() {
            Some(surface) => surface,
            None => {
                let canvas = OffscreenCanvas::new(width, height)?;
                let context = canvas
                    .get_context("2d")?
                    .ok_or_else(|| JsValue::from_str("OffscreenCanvas has no 2d context"))?
                    .dyn_into::<OffscreenCanvasRenderingContext2d>()?;
                (canvas, context)
            }
        };
        let upright = paint_upright(&canvas, &context, frame, width, height);
        self.surface = Some((canvas, context));
        upright
    }
}

fn paint_upright(
    canvas: &OffscreenCanvas,
    context: &OffscreenCanvasRenderingContext2d,
    frame: &VideoFrame,
    width: u32,
    height: u32,
) -> Result<VideoFrame, JsValue> {
    if canvas.width() != width {
        canvas.set_width(width);
    }
    if canvas.height() != height {
        canvas.set_height(height);
    }
    context.draw_image_with_video_frame_and_dw_and_dh(
        frame,
        0.0,
        0.0,
        width as f64,
        height as f64,
    )?;
    let init = VideoFrameInit::new();
    init.set_timestamp(frame.timestamp().unwrap_or(0.0));
    if let Some(duration) = frame.duration() {
        init.set_duration(duration);
    }
    VideoFrame::new_with_offscreen_canvas_and_video_frame_init(canvas, &init)
}

#[cfg(test)]
mod tests {
    use super::upright_canvas_dims;

    #[test]
    fn unrotated_unflipped_frame_is_left_alone() {
        assert_eq!(upright_canvas_dims(0.0, false, 1280, 720), None);
        assert_eq!(upright_canvas_dims(360.0, false, 1280, 720), None);
    }

    #[test]
    fn portrait_phone_frame_is_uprighted_at_its_display_dims() {
        assert_eq!(upright_canvas_dims(90.0, false, 480, 640), Some((480, 640)));
        assert_eq!(
            upright_canvas_dims(270.0, false, 480, 640),
            Some((480, 640))
        );
        assert_eq!(
            upright_canvas_dims(-90.0, false, 480, 640),
            Some((480, 640))
        );
    }

    #[test]
    fn upside_down_and_mirrored_frames_are_uprighted() {
        assert_eq!(
            upright_canvas_dims(180.0, false, 640, 480),
            Some((640, 480))
        );
        assert_eq!(upright_canvas_dims(0.0, true, 640, 480), Some((640, 480)));
    }

    #[test]
    fn dimensionless_frame_is_never_drawn() {
        assert_eq!(upright_canvas_dims(90.0, false, 0, 640), None);
        assert_eq!(upright_canvas_dims(90.0, true, 480, 0), None);
    }
}

#[cfg(test)]
mod wasm_tests {
    use super::FrameUprighter;
    use js_sys::{Object, Reflect, Uint8Array};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::*;
    use web_sys::{OffscreenCanvas, OffscreenCanvasRenderingContext2d, VideoFrame};

    wasm_bindgen_test_configure!(run_in_browser);

    /// A 4x2 source, left half green and right half black, tagged with `rotation` and `flip`.
    fn source_frame(rotation: f64, flip: bool) -> VideoFrame {
        let canvas = OffscreenCanvas::new(4, 2).unwrap();
        let context = canvas
            .get_context("2d")
            .unwrap()
            .unwrap()
            .dyn_into::<OffscreenCanvasRenderingContext2d>()
            .unwrap();
        context.set_fill_style_str("rgb(0, 0, 0)");
        context.fill_rect(0.0, 0.0, 4.0, 2.0);
        context.set_fill_style_str("rgb(0, 255, 0)");
        context.fill_rect(0.0, 0.0, 2.0, 2.0);
        let init = Object::new();
        Reflect::set(&init, &"timestamp".into(), &JsValue::from_f64(1234.0)).unwrap();
        Reflect::set(&init, &"duration".into(), &JsValue::from_f64(33_333.0)).unwrap();
        Reflect::set(&init, &"rotation".into(), &JsValue::from_f64(rotation)).unwrap();
        Reflect::set(&init, &"flip".into(), &JsValue::from_bool(flip)).unwrap();
        VideoFrame::new_with_offscreen_canvas_and_video_frame_init(&canvas, init.unchecked_ref())
            .unwrap()
    }

    fn rotation_of(frame: &VideoFrame) -> f64 {
        Reflect::get(frame, &"rotation".into())
            .unwrap()
            .as_f64()
            .unwrap_or(0.0)
    }

    fn flip_of(frame: &VideoFrame) -> bool {
        Reflect::get(frame, &"flip".into())
            .unwrap()
            .as_bool()
            .unwrap_or(false)
    }

    /// Green level of the first and last pixel of the frame's raw (encoder-input)
    /// buffer. Byte 1 is green in every RGB-family layout (RGBA/RGBX/BGRA/BGRX).
    async fn first_and_last_green(frame: &VideoFrame) -> (u8, u8) {
        let format = Reflect::get(frame, &"format".into())
            .unwrap()
            .as_string()
            .unwrap_or_default();
        assert!(
            format.starts_with("RGB") || format.starts_with("BGR"),
            "precondition: expected an RGB-family canvas frame, got {format:?}"
        );
        let buffer = Uint8Array::new_with_length(frame.allocation_size().unwrap());
        JsFuture::from(frame.copy_to_with_u8_array(&buffer))
            .await
            .unwrap();
        let bytes = buffer.to_vec();
        (bytes[1], bytes[bytes.len() - 3])
    }

    #[wasm_bindgen_test]
    async fn a_quarter_turned_camera_frame_is_encoded_upright() {
        let source = source_frame(90.0, false);
        assert_eq!(
            rotation_of(&source),
            90.0,
            "precondition: this browser must expose VideoFrame rotation"
        );
        assert_eq!((source.display_width(), source.display_height()), (2, 4));

        let upright = FrameUprighter::new().upright(Clone::clone(&source));

        assert_eq!(rotation_of(&upright), 0.0);
        assert_eq!((upright.coded_width(), upright.coded_height()), (2, 4));
        assert_eq!(
            (upright.timestamp(), upright.duration()),
            (Some(1234.0), Some(33_333.0)),
            "the chunk timestamp is sent on the wire, so it must survive"
        );
        let (top, bottom) = first_and_last_green(&upright).await;
        assert!(
            top > 200 && bottom < 50,
            "a 90-degree clockwise turn must put the source's green left half on top, \
             got top={top} bottom={bottom}"
        );
        assert_eq!(
            source.coded_width(),
            0,
            "the replaced source frame must be closed"
        );
        upright.close();
    }

    #[wasm_bindgen_test]
    async fn a_mirrored_camera_frame_is_encoded_mirrored() {
        let source = source_frame(0.0, true);
        assert!(
            flip_of(&source),
            "precondition: this browser must expose VideoFrame flip"
        );

        let upright = FrameUprighter::new().upright(Clone::clone(&source));

        assert!(!flip_of(&upright));
        assert_eq!((upright.coded_width(), upright.coded_height()), (4, 2));
        let (left, right) = first_and_last_green(&upright).await;
        assert!(
            left < 50 && right > 200,
            "a horizontal flip must move the source's green left half to the right, \
             got left={left} right={right}"
        );
        upright.close();
    }

    #[wasm_bindgen_test]
    fn a_failed_draw_keeps_the_cached_canvas() {
        let mut uprighter = FrameUprighter::new();
        uprighter.upright(source_frame(90.0, false)).close();
        let cached: JsValue = uprighter
            .surface
            .as_ref()
            .expect("first draw caches")
            .0
            .clone()
            .into();

        let closed = source_frame(90.0, false);
        closed.close();
        assert!(uprighter.draw_upright(&closed, 2, 4).is_err());

        let kept: JsValue = uprighter
            .surface
            .as_ref()
            .expect("still cached")
            .0
            .clone()
            .into();
        assert!(Object::is(&kept, &cached));
    }

    #[wasm_bindgen_test]
    fn an_unrotated_frame_passes_through_as_the_same_object() {
        let source = source_frame(0.0, false);
        let passed = FrameUprighter::new().upright(Clone::clone(&source));
        let (passed_js, source_js): (&JsValue, &JsValue) = (passed.as_ref(), source.as_ref());
        assert!(Object::is(passed_js, source_js));
        assert_eq!(passed.coded_width(), 4);
        passed.close();
    }
}
