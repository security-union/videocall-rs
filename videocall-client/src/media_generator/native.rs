use wasm_bindgen::prelude::*;
use web_sys::{MediaStreamTrackGenerator, MediaStreamTrackGeneratorInit, WritableStream};

/// Native implementation of a media frame generator
pub struct NativeMediaFrameGenerator {
    generator: MediaStreamTrackGenerator,
}

impl NativeMediaFrameGenerator {
    pub fn new(kind: &str) -> Result<Self, JsValue> {
        // Create a new generator with the specified kind
        let generator = MediaStreamTrackGenerator::new(&MediaStreamTrackGeneratorInit::new(kind))?;

        Ok(Self { generator })
    }

    pub fn track(&self) -> JsValue {
        self.generator.clone().dyn_into().unwrap()
    }

    pub fn writable(&self) -> WritableStream {
        self.generator.writable()
    }
}
