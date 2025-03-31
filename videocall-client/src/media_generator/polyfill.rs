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
                    
                    // Create a more efficient TransformStream with proper backpressure handling
                    const { readable, writable } = new TransformStream({
                        // Use a small high-water mark to reduce memory usage but allow some buffering
                        highWaterMark: 2,
                        transform(frame, controller) {
                            // Pass the frame through efficiently without extra processing
                            controller.enqueue(frame);
                            
                            // Close VideoFrame or AudioData resources as needed
                            if (typeof frame.close === 'function') {
                                frame.close();
                            }
                        }
                    });
                    
                    // Cache track object
                    let track;
                    
                    // Create optimized silent tracks based on kind
                    if (kind === 'audio') {
                        // Create audio track with minimal resources
                        const ctx = new (window.AudioContext || window.webkitAudioContext)({
                            latencyHint: 'playback',
                            sampleRate: 8000  // Use lower sample rate for efficiency
                        });
                        const dest = ctx.createMediaStreamDestination();
                        // Don't create oscillator unless needed (silent track)
                        track = dest.stream.getAudioTracks()[0];
                        
                        // Store context for later cleanup
                        track._audioCtx = ctx;
                    } else {
                        // For video tracks, create minimal 2x2 canvas
                        // This is much more efficient than larger canvases
                        const canvas = document.createElement('canvas');
                        canvas.width = 2;
                        canvas.height = 2;
                        // Capture with minimal framerate
                        track = canvas.captureStream(0).getVideoTracks()[0];
                        
                        // Store canvas for later cleanup
                        track._canvas = canvas;
                    }
                    
                    // Store the readable stream on the track
                    track._readable = readable;
                    
                    return {
                        track: track,
                        writable: writable
                    };
                },
                
                // Add explicit cleanup method
                cleanup: function(track) {
                    if (!track) return;
                    
                    // Stop the track to free resources
                    if (typeof track.stop === 'function') {
                        track.stop();
                    }
                    
                    // Clean up audio context if it exists
                    if (track._audioCtx) {
                        track._audioCtx.close();
                        delete track._audioCtx;
                    }
                    
                    // Remove references to DOM objects
                    if (track._canvas) {
                        delete track._canvas;
                    }
                    
                    // Clear the readable stream reference
                    if (track._readable) {
                        delete track._readable;
                    }
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
