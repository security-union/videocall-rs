use js_sys::{JsString, Object, Promise, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioData, MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit, 
    VideoFrame, WritableStream, WritableStreamDefaultWriter,
};

mod native;
mod polyfill;

/// A wrapper for MediaStreamTrackGenerator that provides a fallback for browsers
/// that don't support it natively
#[wasm_bindgen]
pub struct MediaFrameGenerator {
    track: JsValue,
    writable: WritableStream,
    track_kind: String,
}

#[wasm_bindgen]
impl MediaFrameGenerator {
    /// Checks if the browser natively supports MediaStreamTrackGenerator
    pub fn is_supported() -> bool {
        let window = web_sys::window().expect("no global window exists");
        match js_sys::Reflect::get(&window, &JsValue::from_str("MediaStreamTrackGenerator")) {
            Ok(value) => !value.is_undefined(),
            Err(_) => false,
        }
    }

    /// Creates a new MediaFrameGenerator for the given kind ("audio" or "video")
    #[wasm_bindgen(constructor)]
    pub fn new(kind: &str) -> Result<MediaFrameGenerator, JsValue> {
        if !["audio", "video"].contains(&kind) {
            return Err(JsValue::from_str("Kind must be 'audio' or 'video'"));
        }

        let is_supported = Self::is_supported();
        
        if is_supported {
            // Use native implementation
            let generator = MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new(kind))?;
            let track = generator.clone().dyn_into::<JsValue>()?;
            let writable = generator.writable();
            
            Ok(Self {
                track,
                writable,
                track_kind: kind.to_string(),
            })
        } else {
            // Use polyfill
            let result = polyfill::create_generator(kind)?;
            Ok(Self {
                track: result.track,
                writable: result.writable,
                track_kind: kind.to_string(),
            })
        }
    }

    /// Returns the MediaStreamTrack
    pub fn track(&self) -> JsValue {
        self.track.clone()
    }

    /// Returns the writable stream
    pub fn writable(&self) -> WritableStream {
        self.writable.clone()
    }

    /// Returns the kind of track this generator creates ("audio" or "video")
    pub fn track_kind(&self) -> String {
        self.track_kind.clone()
    }

    /// Writes a frame to the generator
    pub async fn write_frame(&self, frame: &JsValue) -> Result<(), JsValue> {
        let writer = self.writable.get_writer()?;
        
        // Wait for the writer to be ready
        JsFuture::from(writer.ready()).await?;
        
        // Write the frame
        if self.track_kind == "audio" {
            let audio_data = frame.dyn_ref::<AudioData>()
                .ok_or_else(|| JsValue::from_str("Expected AudioData"))?;
            JsFuture::from(writer.write_with_chunk(audio_data)).await?;
        } else {
            let video_frame = frame.dyn_ref::<VideoFrame>()
                .ok_or_else(|| JsValue::from_str("Expected VideoFrame"))?;
            JsFuture::from(writer.write_with_chunk(video_frame)).await?;
        }
        
        // Release the lock
        writer.release_lock();
        
        Ok(())
    }

    /// Creates a writer for the writable stream
    pub fn create_writer(&self) -> Result<WritableStreamDefaultWriter, JsValue> {
        self.writable.get_writer()
    }
} 