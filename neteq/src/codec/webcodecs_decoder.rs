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

//! WebCodecs-based Opus decoder using browser's native hardware acceleration

use crate::codec::AudioDecoder;
use crate::Result;
use js_sys::{Function, Reflect, Uint8Array};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::console;

// Helper to convert JsValue errors to NetEqError
fn js_err(msg: &str) -> impl Fn(JsValue) -> crate::NetEqError + '_ {
    move |e| crate::NetEqError::DecoderError(format!("{msg}: {e:?}"))
}

/// WebCodecs AudioDecoder wrapper for hardware-accelerated Opus decoding
pub struct WebCodecsAudioDecoder {
    decoder: Option<JsValue>,
    sample_rate: u32,
    channels: u8,
    // Buffered output samples (WebCodecs is async, we buffer synchronously)
    output_buffer: Rc<RefCell<Vec<f32>>>,
    // Reusable buffers
    input_buffer: Uint8Array,
}

unsafe impl Send for WebCodecsAudioDecoder {}
unsafe impl Sync for WebCodecsAudioDecoder {}

impl WebCodecsAudioDecoder {
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let output_buffer = Rc::new(RefCell::new(Vec::new()));
        let output_buffer_clone = output_buffer.clone();

        // Create AudioDecoder config
        let config = js_sys::Object::new();
        Reflect::set(&config, &"codec".into(), &"opus".into())
            .map_err(js_err("Failed to set codec"))?;
        Reflect::set(&config, &"sampleRate".into(), &(sample_rate as f64).into())
            .map_err(js_err("Failed to set sampleRate"))?;
        Reflect::set(
            &config,
            &"numberOfChannels".into(),
            &(channels as f64).into(),
        )
        .map_err(js_err("Failed to set numberOfChannels"))?;

        // Output callback - receives decoded AudioData
        let output_cb = Closure::wrap(Box::new(move |audio_data: JsValue| {
            if let Err(e) = Self::handle_output(&audio_data, &output_buffer_clone) {
                console::error_1(&format!("WebCodecs output error: {e:?}").into());
            }
        }) as Box<dyn FnMut(JsValue)>);

        // Error callback
        let error_cb = Closure::wrap(Box::new(move |e: JsValue| {
            console::error_1(&"WebCodecs decoder error:".into());
            console::error_1(&e);
        }) as Box<dyn FnMut(JsValue)>);

        // Create AudioDecoderInit object
        let init = js_sys::Object::new();
        Reflect::set(&init, &"output".into(), output_cb.as_ref())
            .map_err(js_err("Failed to set output callback"))?;
        Reflect::set(&init, &"error".into(), error_cb.as_ref())
            .map_err(js_err("Failed to set error callback"))?;

        // Get AudioDecoder constructor from global scope (works in both window and worker)
        let global = js_sys::global();
        let audio_decoder_ctor = Reflect::get(&global, &"AudioDecoder".into())
            .map_err(js_err("AudioDecoder not found"))?;

        // Create decoder instance
        let decoder = Reflect::construct(
            &audio_decoder_ctor.unchecked_into::<Function>(),
            &js_sys::Array::of1(&init),
        )
        .map_err(js_err("Failed to construct AudioDecoder"))?;

        // Configure decoder
        let configure_fn = Reflect::get(&decoder, &"configure".into())
            .map_err(js_err("Failed to get configure method"))?
            .dyn_into::<Function>()
            .map_err(|_| crate::NetEqError::DecoderError("configure not a function".to_string()))?;

        configure_fn
            .call1(&decoder, &config)
            .map_err(|e| crate::NetEqError::DecoderError(format!("Configure failed: {e:?}")))?;

        // Wait for decoder to be ready
        let state_prop =
            Reflect::get(&decoder, &"state".into()).map_err(js_err("Failed to get state"))?;
        let state = state_prop.as_string().unwrap_or_default();
        console::log_1(&format!("WebCodecs decoder state: {state}").into());

        output_cb.forget();
        error_cb.forget();

        Ok(Self {
            decoder: Some(decoder),
            sample_rate,
            channels,
            output_buffer,
            input_buffer: Uint8Array::new_with_length(1275),
        })
    }

    fn handle_output(audio_data: &JsValue, output_buffer: &Rc<RefCell<Vec<f32>>>) -> Result<()> {
        // Get number of frames
        let num_frames = Reflect::get(audio_data, &"numberOfFrames".into())
            .map_err(js_err("Failed to get numberOfFrames"))?
            .as_f64()
            .unwrap_or(0.0) as usize;

        if num_frames == 0 {
            return Ok(());
        }

        // Get number of channels
        let num_channels = Reflect::get(audio_data, &"numberOfChannels".into())
            .map_err(js_err("Failed to get numberOfChannels"))?
            .as_f64()
            .unwrap_or(1.0) as usize;

        // Allocate output buffer
        let total_samples = num_frames * num_channels;
        let mut samples = vec![0.0f32; total_samples];

        // Copy audio data - create copyTo options
        let copy_options = js_sys::Object::new();
        Reflect::set(&copy_options, &"planeIndex".into(), &0.into())
            .map_err(js_err("Failed to set planeIndex"))?;
        Reflect::set(&copy_options, &"format".into(), &"f32-planar".into())
            .map_err(js_err("Failed to set format"))?;

        let samples_array = js_sys::Float32Array::from(samples.as_slice());

        // Call copyTo method
        let copy_to_fn = Reflect::get(audio_data, &"copyTo".into())
            .map_err(js_err("Failed to get copyTo"))?
            .dyn_into::<Function>()
            .map_err(|_| crate::NetEqError::DecoderError("copyTo not a function".to_string()))?;

        copy_to_fn
            .call2(audio_data, &samples_array, &copy_options)
            .map_err(|e| crate::NetEqError::DecoderError(format!("copyTo failed: {e:?}")))?;

        // Copy from Float32Array to Vec
        samples_array.copy_to(&mut samples);

        // Store in output buffer
        output_buffer.borrow_mut().extend_from_slice(&samples);

        // Close AudioData to free resources
        let close_fn = Reflect::get(audio_data, &"close".into())
            .map_err(js_err("Failed to get close"))?
            .dyn_into::<Function>()
            .map_err(|_| crate::NetEqError::DecoderError("close not a function".to_string()))?;
        let _ = close_fn.call0(audio_data);

        Ok(())
    }

    pub fn get_decoder_type(&self) -> &'static str {
        "WebCodecs"
    }

    /// Queue a decode operation (async - results will arrive in output callback)
    fn queue_decode_operation(&mut self, decoder: &JsValue, encoded: &[u8]) -> Result<()> {
        // Reuse input buffer
        if self.input_buffer.length() < encoded.len() as u32 {
            self.input_buffer = Uint8Array::new_with_length(encoded.len() as u32);
        }
        self.input_buffer.set(&Uint8Array::from(encoded), 0);

        // Create EncodedAudioChunk
        let chunk_init = js_sys::Object::new();
        Reflect::set(&chunk_init, &"type".into(), &"key".into())
            .map_err(js_err("Failed to set chunk type"))?;
        Reflect::set(&chunk_init, &"timestamp".into(), &0.into())
            .map_err(js_err("Failed to set timestamp"))?;
        Reflect::set(
            &chunk_init,
            &"data".into(),
            &self.input_buffer.subarray(0, encoded.len() as u32).buffer(),
        )
        .map_err(js_err("Failed to set data"))?;

        let global = js_sys::global();
        let encoded_chunk_ctor = Reflect::get(&global, &"EncodedAudioChunk".into())
            .map_err(js_err("EncodedAudioChunk not found"))?;
        let chunk = Reflect::construct(
            &encoded_chunk_ctor.unchecked_into::<Function>(),
            &js_sys::Array::of1(&chunk_init),
        )
        .map_err(js_err("Failed to construct EncodedAudioChunk"))?;

        // Queue decode (async - output will come via callback)
        let decode_fn = Reflect::get(decoder, &"decode".into())
            .map_err(js_err("Failed to get decode method"))?
            .dyn_into::<Function>()
            .map_err(|_| crate::NetEqError::DecoderError("decode not a function".to_string()))?;

        decode_fn
            .call1(decoder, &chunk)
            .map_err(|e| crate::NetEqError::DecoderError(format!("Decode failed: {e:?}")))?;

        Ok(())
    }
}

impl AudioDecoder for WebCodecsAudioDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        // IMPORTANT: WebCodecs decode() is asynchronous - the output callback will be
        // called later. To work with NetEq's synchronous interface, we return samples
        // from the PREVIOUS decode call while queuing the current one.
        // This adds ~10-20ms of latency but is necessary for the API design.

        // First, grab any samples from previous decode operations
        let samples_to_return = self.output_buffer.borrow_mut().drain(..).collect();

        // Clone the decoder JsValue to avoid borrow conflicts
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| crate::NetEqError::DecoderError("Decoder not initialized".to_string()))?
            .clone();

        // Now queue the current packet for decoding (async - results will come later)
        if let Err(e) = self.queue_decode_operation(&decoder, encoded) {
            console::warn_1(&format!("WebCodecs decode queue failed: {e:?}").into());
            // Return what we have buffered, even if queuing failed
        }

        Ok(samples_to_return)
    }
}
