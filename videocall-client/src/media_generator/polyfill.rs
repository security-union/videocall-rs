use js_sys::{Function, Object, Reflect};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{WritableStream, MediaStreamTrack};

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
        ensure_polyfill_initialized()?;
        
        // Create a MediaStreamTrackGenerator polyfill instance
        let polyfill = js_sys::eval(
            r#"
            window.__mediaFrameGeneratorPolyfill.createGenerator(arguments[0])
            "#,
        )?;
        
        // Call the polyfill with the track kind
        let js_generator = js_sys::Reflect::apply(
            &polyfill.dyn_into::<Function>()?,
            &JsValue::NULL,
            &js_sys::Array::of1(&JsValue::from_str(kind)),
        )?;
        
        // Extract the track and writable properties
        let js_generator_obj = js_generator.dyn_into::<Object>()?;
        let track = Reflect::get(&js_generator_obj, &JsValue::from_str("track"))?;
        let writable = Reflect::get(&js_generator_obj, &JsValue::from_str("writable"))?
            .dyn_into::<WritableStream>()?;
        
        Ok(Self { track, writable })
    }

    pub fn track(&self) -> JsValue {
        self.track.clone()
    }
    
    pub fn writable(&self) -> WritableStream {
        self.writable.clone()
    }
    
    pub fn track_kind(&self) -> Result<String, JsValue> {
        let track = self.track.dyn_ref::<MediaStreamTrack>()
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
    
    if js_sys::Reflect::get(&window, &JsValue::from_str("__mediaFrameGeneratorPolyfill"))?.is_undefined() {
        // Initialize the polyfill
        js_sys::eval(
            r#"
            window.__mediaFrameGeneratorPolyfill = {
                createGenerator: function(kind) {
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
                    
                    // Create dummy MediaStreamTrack
                    const dummyTrack = {
                        kind: kind,
                        addEventListener: function() {},
                        removeEventListener: function() {},
                        // Add other MediaStreamTrack methods as needed
                    };
                    
                    // Store the readable stream on the track for internal use
                    dummyTrack._readable = readable;
                    
                    return {
                        track: dummyTrack,
                        writable: writable
                    };
                }
            };
            "#,
        )?;
    }
    
    Ok(())
} 