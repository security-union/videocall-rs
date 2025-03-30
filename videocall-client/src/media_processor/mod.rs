use js_sys::{Array, JsString, Object, Promise, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioData, MediaStream, MediaStreamTrack, MediaStreamTrackProcessor,
    MediaStreamTrackProcessorInit, ReadableStream, ReadableStreamDefaultReader, VideoFrame,
};

mod native;
mod polyfill;

/// Represents a frame of media, either audio or video
#[derive(Debug)]
pub enum MediaFrame {
    Video(VideoFrame),
    Audio(AudioData),
}

/// Trait for reading frames from a media track
pub trait MediaFrameReader {
    /// Read the next frame from the media track
    /// Returns a JsValue that is actually a Promise that will resolve to a result with frame data
    fn read_frame(&self) -> JsValue;

    /// Close the reader and free any resources
    fn close(&self) -> Result<(), JsValue>;

    /// Get the kind of media track this reader is reading from
    fn track_kind(&self) -> &str;
}

/// Helper function to convert a Promise from read_frame to a MediaFrame Result
pub async fn promise_to_media_frame(promise: JsValue) -> Result<MediaFrame, JsValue> {
    let js_result = JsFuture::from(Promise::from(promise)).await?;
    let value = Reflect::get(&js_result, &JsString::from("value"))?;

    if value.is_undefined() {
        return Err(JsValue::from_str("End of stream"));
    }

    // Try to determine if it's a video or audio frame based on properties
    let type_check = value.clone().unchecked_into::<Object>();

    if js_sys::Reflect::has(&type_check, &JsString::from("displayWidth"))? {
        Ok(MediaFrame::Video(value.unchecked_into::<VideoFrame>()))
    } else if js_sys::Reflect::has(&type_check, &JsString::from("sampleRate"))? {
        Ok(MediaFrame::Audio(value.unchecked_into::<AudioData>()))
    } else {
        Err(JsValue::from_str("Unknown frame type"))
    }
}

/// Processor for media frames from a MediaStreamTrack
/// This will use the native browser API if available, otherwise fall back to a polyfill
#[wasm_bindgen]
pub struct MediaFrameProcessor {
    reader: ReadableStreamDefaultReader,
    track_kind: String,
}

impl MediaFrameProcessor {
    /// Checks if the browser natively supports MediaStreamTrackProcessor
    pub fn is_supported() -> bool {
        let window = web_sys::window().expect("no global window exists");
        match js_sys::Reflect::get(&window, &JsValue::from_str("MediaStreamTrackProcessor")) {
            Ok(value) => !value.is_undefined(),
            Err(_) => false,
        }
    }

    /// Creates a new MediaFrameProcessor for the given track
    pub fn new(track: &MediaStreamTrack) -> Result<Self, JsValue> {
        let track_kind = track.kind();

        // Check if MediaStreamTrackProcessor is supported natively
        let is_supported = Self::is_supported();

        let reader = if is_supported {
            // Use native implementation
            let processor =
                MediaStreamTrackProcessor::new(&MediaStreamTrackProcessorInit::new(track))?;
            processor
                .readable()
                .get_reader()
                .unchecked_into::<ReadableStreamDefaultReader>()
        } else {
            // Use polyfill
            polyfill::create_processor(track)?
        };

        Ok(Self { reader, track_kind })
    }

    /// Reads the next frame from the track
    ///
    /// Returns a JsValue that is actually a Promise that will resolve to an object containing a VideoFrame or AudioData
    pub fn read_frame(&self) -> JsValue {
        self.reader.read().into()
    }

    /// Process frames using an async callback
    ///
    /// Example:
    /// ```no_run
    /// let processor = MediaFrameProcessor::new(&video_track)?;
    /// processor.process_frames(|frame| {
    ///     // Handle the frame
    ///     Ok(())
    /// }).await?;
    /// ```
    pub async fn process_video_frames<F>(&self, mut callback: F) -> Result<(), JsValue>
    where
        F: FnMut(VideoFrame) -> Result<(), JsValue>,
    {
        if self.track_kind != "video" {
            return Err(JsValue::from_str("Not a video track"));
        }

        loop {
            let promise = self.read_frame();
            let js_result = JsFuture::from(js_sys::Promise::from(promise)).await?;
            let value = Reflect::get(&js_result, &JsString::from("value"))?;

            if value.is_undefined() {
                return Ok(()); // End of stream
            }

            let video_frame = value.unchecked_into::<VideoFrame>();
            callback(video_frame)?;
        }
    }

    /// Process audio frames using an async callback
    pub async fn process_audio_frames<F>(&self, mut callback: F) -> Result<(), JsValue>
    where
        F: FnMut(AudioData) -> Result<(), JsValue>,
    {
        if self.track_kind != "audio" {
            return Err(JsValue::from_str("Not an audio track"));
        }

        loop {
            let promise = self.read_frame();
            let js_result = JsFuture::from(js_sys::Promise::from(promise)).await?;
            let value = Reflect::get(&js_result, &JsString::from("value"))?;

            if value.is_undefined() {
                return Ok(()); // End of stream
            }

            let audio_data = value.unchecked_into::<AudioData>();
            callback(audio_data)?;
        }
    }

    /// Closes the processor and frees resources
    pub fn close(&self) -> Result<(), JsValue> {
        // Note: cancel() returns a Promise, not a Result
        // We'll just ignore the promise for now as this is a synchronous function
        let _ = self.reader.cancel();
        Ok(())
    }

    /// Returns the kind of track this processor is reading from ("audio" or "video")
    pub fn track_kind(&self) -> &str {
        &self.track_kind
    }
}

// impl Clone for MediaFrameProcessor {
//     fn clone(&self) -> Self {
//         Self {
//             reader: self.reader.clone(),
//             track_kind: self.track_kind.clone(),
//         }
//     }
// }

/// A simple demo that captures video from a camera and displays it
/// using the MediaFrameProcessor
#[wasm_bindgen]
pub struct MediaProcessorDemo {
    container_id: String,
    processor: Option<MediaFrameProcessor>,
    video: Option<web_sys::HtmlVideoElement>,
    canvas: Option<web_sys::HtmlCanvasElement>,
    running: bool,
}

impl Clone for MediaProcessorDemo {
    fn clone(&self) -> Self {
        Self {
            container_id: self.container_id.clone(),
            processor: None, // Don't try to clone the processor
            video: self.video.clone(),
            canvas: self.canvas.clone(),
            running: self.running,
        }
    }
}

#[wasm_bindgen]
impl MediaProcessorDemo {
    /// Create a new demo that will render in the specified container
    #[wasm_bindgen(constructor)]
    pub fn new(container_id: String) -> Self {
        Self {
            container_id,
            processor: None,
            video: None,
            canvas: None,
            running: false,
        }
    }

    /// Start the demo by requesting camera access and processing frames
    pub async fn start(&mut self) -> Result<(), JsValue> {
        // For simplicity, we'll just return Ok. In a real implementation,
        // this would set up camera access and start processing frames.
        web_sys::console::log_1(&JsValue::from_str("MediaProcessorDemo.start() called"));
        Ok(())
    }

    /// Stop the demo and release resources
    pub fn stop(&mut self) -> Result<(), JsValue> {
        web_sys::console::log_1(&JsValue::from_str("MediaProcessorDemo.stop() called"));
        self.running = false;

        // Close the processor if it exists
        if let Some(processor) = &self.processor {
            processor.close()?;
        }

        // Reset elements
        self.processor = None;
        self.video = None;
        self.canvas = None;

        Ok(())
    }
}
