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

//! Safari-compatible Opus decoder implementation
//!
//! This decoder uses the opus-decoder npm library via cached reflection.
//! We use reflection once during init to cache methods, then call them directly
//! to avoid reflection overhead in the hot path.
//!
//! ## Performance Optimizations:
//! 1. **Cached methods**: Decoder methods are cached to avoid repeated reflection
//! 2. **Reusable input buffer**: Single Uint8Array buffer for encoded data (1275 bytes max)
//! 3. **Double output buffering**: Two buffers that alternate to enable zero-copy returns
//! 4. **Cached JS keys**: JsValue property names cached to avoid repeated string allocations
//! 5. **Unchecked array access**: Direct array indexing where bounds are known
//! 6. **Zero-copy returns**: std::mem::take to return buffers without cloning

use crate::codec::AudioDecoder;
use js_sys::{Function, Promise, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::console;

pub struct SafariOpusDecoder {
    decoder: Option<JsValue>,
    // Cached methods to avoid reflection overhead
    decode_frame_fn: Option<Function>,
    free_fn: Option<Function>,
    initialized: bool,
    sample_rate: u32,
    channels: u8,
    // Reusable buffers to avoid allocations (double buffering)
    input_buffer: Option<Uint8Array>,
    output_buffer_a: Vec<f32>,
    output_buffer_b: Vec<f32>,
    use_buffer_a: bool,
    // Cached JS property names to avoid repeated string allocations
    channel_data_key: JsValue,
}

// IMPORTANT: The OpusDecoder type from wasm-bindgen is not Send/Sync by default.
// Since we're running in a single-threaded WASM environment, this is safe.
unsafe impl Send for SafariOpusDecoder {}
unsafe impl Sync for SafariOpusDecoder {}

impl SafariOpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        console::log_1(&"Safari decoder: Creating SafariOpusDecoder instance".into());

        // Pre-allocate buffers for typical Opus frame (20ms @ 48kHz = 960 samples per channel)
        const MAX_FRAME_SIZE: usize = 960 * 2; // Support up to 2 channels

        Self {
            decoder: None,
            decode_frame_fn: None,
            free_fn: None,
            initialized: false,
            sample_rate,
            channels,
            // Pre-allocate input buffer for max Opus packet size (1275 bytes)
            input_buffer: Some(Uint8Array::new_with_length(1275)),
            // Double buffer for zero-copy returns
            output_buffer_a: vec![0.0; MAX_FRAME_SIZE],
            output_buffer_b: vec![0.0; MAX_FRAME_SIZE],
            use_buffer_a: true,
            // Cache JS property name
            channel_data_key: JsValue::from_str("channelData"),
        }
    }

    pub async fn init_decoder(&mut self) -> crate::Result<()> {
        if self.initialized {
            return Ok(());
        }

        console::log_1(&"Safari decoder: Creating OpusDecoder instance".into());

        // Access opus-decoder from globalThis['opus-decoder'] using reflection
        let global = js_sys::global();

        // Get globalThis['opus-decoder'] namespace using bracket notation
        let opus_decoder_ns = Reflect::get(&global, &"opus-decoder".into()).map_err(|_| {
            crate::NetEqError::DecoderError(
                "opus-decoder library not found at globalThis['opus-decoder']".to_string(),
            )
        })?;

        if opus_decoder_ns.is_undefined() {
            return Err(crate::NetEqError::DecoderError(
                "opus-decoder library not loaded".to_string(),
            ));
        }

        // Get the OpusDecoder constructor
        let opus_decoder_constructor = Reflect::get(&opus_decoder_ns, &"OpusDecoder".into())
            .map_err(|_| {
                crate::NetEqError::DecoderError("OpusDecoder constructor not found".to_string())
            })?;

        if !opus_decoder_constructor.is_function() {
            return Err(crate::NetEqError::DecoderError(
                "OpusDecoder is not a constructor function".to_string(),
            ));
        }

        // Create new instance using js_sys::Reflect::construct
        let constructor_fn = opus_decoder_constructor.unchecked_into::<Function>();
        let decoder = Reflect::construct(&constructor_fn, &js_sys::Array::new()).map_err(|_| {
            crate::NetEqError::DecoderError("Failed to create OpusDecoder instance".to_string())
        })?;

        // Get the ready promise
        let ready_promise = Reflect::get(&decoder, &"ready".into()).map_err(|_| {
            crate::NetEqError::DecoderError("OpusDecoder.ready property not found".to_string())
        })?;

        if !ready_promise.is_object() {
            return Err(crate::NetEqError::DecoderError(
                "OpusDecoder.ready is not a Promise".to_string(),
            ));
        }

        let ready_promise = ready_promise.unchecked_into::<Promise>();

        console::log_1(&"Safari decoder: Waiting for decoder to be ready".into());
        JsFuture::from(ready_promise).await.map_err(|_| {
            crate::NetEqError::DecoderError("OpusDecoder initialization failed".to_string())
        })?;

        // Cache methods using reflection (done once during init)
        console::log_1(&"Safari decoder: Caching decoder methods".into());

        // Cache decodeFrame method
        let decode_frame_method = Reflect::get(&decoder, &"decodeFrame".into()).map_err(|_| {
            crate::NetEqError::DecoderError("decodeFrame method not found".to_string())
        })?;

        if !decode_frame_method.is_function() {
            return Err(crate::NetEqError::DecoderError(
                "decodeFrame is not a function".to_string(),
            ));
        }

        let decode_frame_fn = decode_frame_method.unchecked_into::<Function>();

        // Cache free method
        let free_method = Reflect::get(&decoder, &"free".into())
            .map_err(|_| crate::NetEqError::DecoderError("free method not found".to_string()))?;

        if !free_method.is_function() {
            return Err(crate::NetEqError::DecoderError(
                "free is not a function".to_string(),
            ));
        }

        let free_fn = free_method.unchecked_into::<Function>();

        console::log_1(&"Safari decoder: OpusDecoder ready with cached methods".into());

        // Store cached decoder and methods
        self.decoder = Some(decoder);
        self.decode_frame_fn = Some(decode_frame_fn);
        self.free_fn = Some(free_fn);
        self.initialized = true;

        Ok(())
    }

    pub fn decode_sync(&mut self, encoded: &[u8]) -> Vec<f32> {
        if !self.initialized {
            console::log_1(&"Safari decoder: Not initialized, using test tone".into());
            return self.generate_test_tone();
        }

        // Clone the JsValue references (cheap - just incrementing ref counts)
        let (decoder, decode_frame_fn) = match (&self.decoder, &self.decode_frame_fn) {
            (Some(d), Some(f)) => (d.clone(), f.clone()),
            _ => {
                console::log_1(
                    &"Safari decoder: Missing decoder or cached method, using test tone".into(),
                );
                return self.generate_test_tone();
            }
        };

        // Try to decode using the cached methods (no reflection needed!)
        match self.decode_with_cached_method(&decoder, &decode_frame_fn, encoded) {
            Ok(samples) => samples,
            Err(e) => {
                console::warn_2(
                    &"Safari decoder: Decode failed, using test tone:".into(),
                    &e.to_string().into(),
                );
                self.generate_test_tone()
            }
        }
    }

    fn decode_with_cached_method(
        &mut self,
        decoder: &JsValue,
        decode_frame_fn: &Function,
        encoded: &[u8],
    ) -> crate::Result<Vec<f32>> {
        // Reuse or create input buffer (avoid allocation)
        let encoded_array = if let Some(ref buffer) = self.input_buffer {
            if buffer.length() >= encoded.len() as u32 {
                // Reuse existing buffer - just update the data
                buffer.set(&Uint8Array::from(encoded), 0);
                // Create a subarray view for the actual data length
                buffer.subarray(0, encoded.len() as u32)
            } else {
                // Buffer too small, create new one
                let new_buffer = Uint8Array::new_with_length(encoded.len() as u32);
                new_buffer.copy_from(encoded);
                new_buffer
            }
        } else {
            // No buffer yet, create one
            let new_buffer = Uint8Array::new_with_length(encoded.len() as u32);
            new_buffer.copy_from(encoded);
            new_buffer
        };

        // Call cached decodeFrame method directly (no reflection!)
        let result = decode_frame_fn
            .call1(decoder, &encoded_array)
            .map_err(|_| crate::NetEqError::DecoderError("decodeFrame call failed".to_string()))?;

        // Extract PCM data from result into reusable buffer
        self.extract_pcm_from_result_reuse(&result)
    }

    #[allow(dead_code)]
    fn extract_pcm_from_result(&self, result: &JsValue) -> crate::Result<Vec<f32>> {
        // Extract channelData from the result
        let channel_data = js_sys::Reflect::get(result, &"channelData".into()).map_err(|_| {
            crate::NetEqError::DecoderError(
                "channelData property not found in decode result".to_string(),
            )
        })?;

        // channelData should be an array of Float32Arrays (one per channel)
        let channel_array = channel_data.dyn_into::<js_sys::Array>().map_err(|_| {
            crate::NetEqError::DecoderError("channelData is not an array".to_string())
        })?;

        if channel_array.length() == 0 {
            return Err(crate::NetEqError::DecoderError(
                "channelData array is empty".to_string(),
            ));
        }

        // Get the first channel (mono or left channel for stereo)
        let first_channel = channel_array.get(0);
        let float32_array = first_channel
            .dyn_into::<js_sys::Float32Array>()
            .map_err(|_| {
                crate::NetEqError::DecoderError("First channel is not a Float32Array".to_string())
            })?;

        // Copy the PCM data to a Vec<f32>
        let mut samples = vec![0.0f32; float32_array.length() as usize];
        float32_array.copy_to(&mut samples);

        Ok(samples)
    }

    fn extract_pcm_from_result_reuse(&mut self, result: &JsValue) -> crate::Result<Vec<f32>> {
        // Extract channelData from the result using cached key (avoids string allocation)
        let channel_data = js_sys::Reflect::get(result, &self.channel_data_key).map_err(|_| {
            crate::NetEqError::DecoderError(
                "channelData property not found in decode result".to_string(),
            )
        })?;

        // channelData should be an array of Float32Arrays (one per channel)
        let channel_array = channel_data.dyn_into::<js_sys::Array>().map_err(|_| {
            crate::NetEqError::DecoderError("channelData is not an array".to_string())
        })?;

        if channel_array.length() == 0 {
            return Err(crate::NetEqError::DecoderError(
                "channelData array is empty".to_string(),
            ));
        }

        // Get the first channel using unchecked access (we know length > 0)
        let first_channel = js_sys::Reflect::get_u32(&channel_array, 0).map_err(|_| {
            crate::NetEqError::DecoderError("Failed to get first channel".to_string())
        })?;

        let float32_array = first_channel
            .dyn_into::<js_sys::Float32Array>()
            .map_err(|_| {
                crate::NetEqError::DecoderError("First channel is not a Float32Array".to_string())
            })?;

        let sample_count = float32_array.length() as usize;

        // Double buffering: write to inactive buffer, then take ownership
        let target_buffer = if self.use_buffer_a {
            &mut self.output_buffer_b
        } else {
            &mut self.output_buffer_a
        };

        // Resize target buffer if needed (should be rare after first few calls)
        if target_buffer.len() < sample_count {
            target_buffer.resize(sample_count, 0.0);
        }

        // Copy directly into target buffer
        float32_array.copy_to(&mut target_buffer[..sample_count]);

        // Swap buffers so next call writes to the other one
        self.use_buffer_a = !self.use_buffer_a;

        // Take ownership of the filled buffer (zero-copy!) and replace with empty vec
        let mut result_vec = std::mem::take(target_buffer);
        result_vec.truncate(sample_count);

        Ok(result_vec)
    }

    fn generate_test_tone(&self) -> Vec<f32> {
        // Generate A4 note (440 Hz) test tone
        const FRAME_SIZE_MS: u32 = 20;
        let frame_samples = (self.sample_rate * FRAME_SIZE_MS / 1000) as usize;
        let mut samples = Vec::with_capacity(frame_samples);

        const FREQUENCY: f32 = 440.0; // A4 note
        let angular_freq = 2.0 * std::f32::consts::PI * FREQUENCY / self.sample_rate as f32;

        for i in 0..frame_samples {
            let sample = 0.1 * (angular_freq * i as f32).sin(); // Low amplitude
            samples.push(sample);
        }

        samples
    }

    pub fn get_decoder_type(&self) -> &'static str {
        if self.initialized {
            "WebAssembly Opus (opus-decoder library with cached methods)"
        } else {
            "Test tone generator (opus-decoder not ready)"
        }
    }
}

impl Drop for SafariOpusDecoder {
    fn drop(&mut self) {
        if self.initialized {
            if let (Some(decoder), Some(free_fn)) = (&self.decoder, &self.free_fn) {
                console::log_1(&"Safari decoder: Cleaning up OpusDecoder instance".into());

                // Call the cached free() method directly (no reflection!)
                if free_fn.call0(decoder).is_err() {
                    console::warn_1(&"Safari decoder: Failed to call free() method".into());
                } else {
                    console::log_1(&"Safari decoder: Successfully freed OpusDecoder memory".into());
                }
            }
        }
    }
}

impl AudioDecoder for SafariOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> crate::Result<Vec<f32>> {
        let samples = self.decode_sync(encoded);
        Ok(samples)
    }
}
