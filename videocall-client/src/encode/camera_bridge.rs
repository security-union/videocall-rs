use gloo_utils::window;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlVideoElement, MediaStreamTrack, HtmlCanvasElement, CanvasRenderingContext2d, OffscreenCanvas, WebGl2RenderingContext};

/// VideoProcessorCompat provides video frame processing capabilities without requiring MediaStreamTrackProcessor
pub struct VideoProcessorCompat {
    video: HtmlVideoElement,
    canvas: HtmlCanvasElement,
    context: CanvasRenderingContext2d,
}

impl VideoProcessorCompat {
    /// Creates a new VideoProcessorCompat instance
    pub fn new(track: &MediaStreamTrack) -> Result<Self, JsValue> {
        let document = window().document().ok_or("No document found")?;
        let video = document
            .create_element("video")?
            .dyn_into::<HtmlVideoElement>()?;
            
        let canvas = document
            .create_element("canvas")?
            .dyn_into::<HtmlCanvasElement>()?;
        
        let context = canvas
            .get_context("2d")?
            .ok_or("Failed to get 2D context")?
            .dyn_into::<CanvasRenderingContext2d>()?;

        // Create a MediaStream with just this track
        let stream = web_sys::MediaStream::new()?;
        stream.add_track(track);
        video.set_src_object(Some(&stream));
        
        Ok(Self {
            video,
            canvas,
            context,
        })
    }

    /// Starts processing frames and calls the provided callback with each frame
    pub fn start(&mut self, callback: Closure<dyn FnMut(JsValue)>) -> Result<(), JsValue> {
        let video = self.video.clone();
        let canvas = self.canvas.clone();
        let context = self.context.clone();
        let callback = callback.as_ref().unchecked_ref::<js_sys::Function>();

        // Create frame processing function
        let process_frame = Closure::wrap(Box::new(move || {
            // Draw the current frame to the canvas
            if let Err(e) = context.draw_image_with_html_video_element(&video, 0.0, 0.0) {
                log::error!("Error drawing video frame: {:?}", e);
                return;
            }

            // Get the image data
            let image_data = match context.get_image_data(0.0, 0.0, video.video_width() as f64, video.video_height() as f64) {
                Ok(data) => data,
                Err(e) => {
                    log::error!("Error getting image data: {:?}", e);
                    return;
                }
            };

            // Create a frame object with the data
            let frame = js_sys::Object::new();
            js_sys::Reflect::set(&frame, &"width".into(), &video.video_width().into()).unwrap();
            js_sys::Reflect::set(&frame, &"height".into(), &video.video_height().into()).unwrap();
            
            // Convert the Uint8ClampedArray to an ArrayBuffer
            let array = js_sys::Uint8Array::new(&image_data.data());
            js_sys::Reflect::set(&frame, &"data".into(), &array.buffer()).unwrap();

            // Call the user's callback with the frame data
            let _ = js_sys::Reflect::apply(
                callback,
                &JsValue::NULL,
                &js_sys::Array::of1(&frame),
            );

            // Request the next frame using reflection
            let request_video_frame_callback = js_sys::Reflect::get(
                &video,
                &"requestVideoFrameCallback".into(),
            )?;
            let _ = js_sys::Reflect::apply(
                &request_video_frame_callback,
                &video,
                &js_sys::Array::of1(&process_frame.as_ref().unchecked_ref()),
            );
        }) as Box<dyn FnMut()>);

        // Set up metadata loaded handler
        let metadata_loaded = Closure::wrap(Box::new(move || {
            let width = video.video_width();
            let height = video.video_height();
            canvas.set_width(width);
            canvas.set_height(height);

            // Start frame processing using reflection
            let request_video_frame_callback = js_sys::Reflect::get(
                &video,
                &"requestVideoFrameCallback".into(),
            )?;
            let _ = js_sys::Reflect::apply(
                &request_video_frame_callback,
                &video,
                &js_sys::Array::of1(&process_frame.as_ref().unchecked_ref()),
            );
            process_frame.forget();
        }) as Box<dyn FnMut()>);

        // Set up the metadata loaded handler
        self.video.set_onloadedmetadata(Some(metadata_loaded.as_ref().unchecked_ref()));
        metadata_loaded.forget();

        Ok(())
    }

    /// Stop processing frames
    pub fn stop(&mut self) {
        // Clean up resources
        self.video.set_src_object(None);
        let _ = self.video.pause();
    }
}

impl Drop for VideoProcessorCompat {
    fn drop(&mut self) {
        self.stop();
    }
} 