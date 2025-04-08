use js_sys::{Function, Object, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{MediaStreamTrack, WritableStream};

/// Result structure for the polyfill generator creation
#[derive(Clone)]
pub struct PolyfillGeneratorResult {
    pub track: JsValue,
    pub writable: WritableStream,
}

/// A JavaScript-based polyfill for MediaStreamTrackGenerator
/// This uses TransformStream internally to create a WritableStream
pub struct PolyfillMediaFrameGenerator {
    track: JsValue,
    writable: WritableStream,
}

impl PolyfillMediaFrameGenerator {
    pub fn new(kind: &str) -> Result<Self, JsValue> {
        log::info!("new PolyfillMediaFrameGenerator {}", kind);
        ensure_polyfill_initialized()?;

        // Create a MediaStreamTrackGenerator polyfill instance
        let window = web_sys::window().expect("no global window exists");
        let polyfill =
            js_sys::Reflect::get(&window, &JsValue::from_str("__mediaFrameGeneratorPolyfill"))?;
        let create_fn = Reflect::get(&polyfill, &JsValue::from_str("createGenerator"))?
            .dyn_into::<Function>()?;

        let js_generator = create_fn.call1(&JsValue::NULL, &JsValue::from_str(kind))?;

        // Extract the track and writable properties
        let js_generator_obj = js_generator.dyn_into::<Object>()?;
        let track = Reflect::get(&js_generator_obj, &JsValue::from_str("track"))?;
        let writable = Reflect::get(&js_generator_obj, &JsValue::from_str("writable"))?
            .dyn_into::<WritableStream>()?;

        Ok(Self { track, writable })
    }

    pub fn track(&self) -> JsValue {
        // Add explicit debugging to help diagnose issues
        log::debug!(
            "Returning polyfill track of type: {:?}",
            js_typeof(&self.track)
        );
        self.track.clone()
    }

    pub fn writable(&self) -> WritableStream {
        self.writable.clone()
    }

    pub fn track_kind(&self) -> Result<String, JsValue> {
        let track = self
            .track
            .dyn_ref::<MediaStreamTrack>()
            .ok_or_else(|| JsValue::from_str("Failed to cast to MediaStreamTrack"))?;
        Ok(track.kind())
    }
}

/// Create a polyfill generator with the specified kind
pub fn create_generator(kind: &str) -> Result<PolyfillGeneratorResult, JsValue> {
    let generator = PolyfillMediaFrameGenerator::new(kind)?;

    Ok(PolyfillGeneratorResult {
        track: generator.track(),
        writable: generator.writable(),
    })
}

/// Initialize the polyfill code if it hasn't been initialized yet
fn ensure_polyfill_initialized() -> Result<(), JsValue> {
    let window = web_sys::window().expect("no global window exists");

    if js_sys::Reflect::get(&window, &JsValue::from_str("__mediaFrameGeneratorPolyfill"))?
        .is_undefined()
    {
        // Initialize the polyfill
        js_sys::eval(
            r#"
            window.__mediaFrameGeneratorPolyfill = {
                createGenerator: function(kind) {
                    console.log("Creating polyfill generator for", kind);
                    if (kind !== 'audio' && kind !== 'video') {
                        throw new Error('Kind must be "audio" or "video"');
                    }
                    
                    // Create a TransformStream to handle frame writing
                    const { readable, writable } = new TransformStream({
                        transform(frame, controller) {
                            // Pass the frame through to the readable side
                            controller.enqueue(frame);
                            
                            // Close VideoFrame or AudioData resources as needed
                            if (typeof frame.close === 'function') {
                                frame.close();
                            }
                        }
                    });
                    
                    // Create a real MediaStreamTrack from a dummy MediaStream
                    const dummyStream = new MediaStream();
                    
                    // Get a real track from the correct track collection
                    let track;
                    if (kind === 'audio') {
                        const audioCtx = new (window.AudioContext || window.webkitAudioContext)();
                        const oscillator = audioCtx.createOscillator();
                        oscillator.frequency.value = 0; // Silent
                        const dest = audioCtx.createMediaStreamDestination();
                        oscillator.connect(dest);
                        track = dest.stream.getAudioTracks()[0];
                    } else {
                        // For video, create a canvas and get its track
                        const canvas = document.createElement('canvas');
                        canvas.width = 2;
                        canvas.height = 2;
                        track = canvas.captureStream().getVideoTracks()[0];
                    }
                    
                    // Store the readable stream on the track for our internal use
                    track._readable = readable;
                    
                    return {
                        track: track,
                        writable: writable
                    };
                }
            };
            "#,
        )?;
    }

    Ok(())
}

// Helper function to get JavaScript typeof
fn js_typeof(val: &JsValue) -> String {
    let window = web_sys::window().expect("no global window exists");
    let typeof_fn = r#"
    function(obj) {
        return typeof obj;
    }
    "#;

    match js_sys::Function::new_with_args("obj", typeof_fn).call1(&JsValue::NULL, val) {
        Ok(js_type) => js_type.as_string().unwrap_or_else(|| "unknown".to_string()),
        Err(_) => "error".to_string(),
    }
}
