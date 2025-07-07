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

use super::AudioDecoder;
use crate::{NetEqError, Result};
use js_sys::{Float32Array, Function, Reflect, Uint8Array};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::window;
use web_sys::{
    AudioData, AudioDecoder as WcAudioDecoder, AudioDecoderConfig, AudioDecoderInit,
    EncodedAudioChunk, EncodedAudioChunkInit, EncodedAudioChunkType as ChunkType,
};

/// Opus decoder backed by the browser's WebCodecs `AudioDecoder`.
///
/// Because WebCodecs is async, we maintain an internal ring‐buffer of decoded
/// PCM frames.  The synchronous `decode()` call enqueues the given encoded
/// packet for decoding *and returns the next available PCM frame* (from a
/// previous packet).  This introduces a 1-packet delay but keeps the NetEq API
/// synchronous.
pub struct OpusDecoder {
    wc_decoder: WcAudioDecoder,
    pcm_queue: Rc<RefCell<VecDeque<Vec<f32>>>>,
    sample_rate: u32,
    channels: u8,
    next_timestamp: u64,
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        // Shared queue for output callback.
        let queue: Rc<RefCell<VecDeque<Vec<f32>>>> = Rc::new(RefCell::new(VecDeque::new()));

        // Create JS closure that copies PCM from AudioData → Vec<f32> and pushes
        // it onto the queue.
        let queue_clone = queue.clone();
        let output_cb =
            Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |audio_data_js: JsValue| {
                if let Ok(audio_data) = audio_data_js.dyn_into::<AudioData>() {
                    // Extract PCM for all channels interleaved.
                    let frames = audio_data.number_of_frames() as usize;
                    let chans = audio_data.number_of_channels() as usize;
                    let mut interleaved = Vec::with_capacity(frames * chans);

                    for ch in 0..chans {
                        // Copy plane `ch` to a Float32Array then to Vec<f32>.
                        let opts = web_sys::AudioDataCopyToOptions::new(ch as u32);
                        let len = frames;
                        let js_f32 = Float32Array::new_with_length(len as u32);

                        // Call audio_data.copyTo(js_f32, opts) via JS reflection to avoid binding mismatch.
                        if let Ok(copy_fn) =
                            js_sys::Reflect::get(&audio_data, &JsValue::from_str("copyTo"))
                        {
                            let _ = copy_fn.unchecked_into::<Function>().call2(
                                &audio_data,
                                &js_f32.clone().into(),
                                &opts,
                            );
                        }
                        let mut tmp = vec![0.0f32; len];
                        js_f32.copy_to(&mut tmp[..]);
                        // Push samples interleaved.
                        if interleaved.is_empty() {
                            interleaved.resize(len * chans, 0.0);
                        }
                        for (i, sample) in tmp.into_iter().enumerate() {
                            interleaved[i * chans + ch] = sample;
                        }
                    }

                    queue_clone.borrow_mut().push_back(interleaved);
                    let _ = audio_data.close();
                }
            }));

        let error_cb = Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |e: JsValue| {
            // Try to extract a readable message from the exception/value.
            let msg = if let Some(err) = e.dyn_ref::<js_sys::Error>() {
                err.message().into()
            } else if let Ok(msg_val) = js_sys::Reflect::get(&e, &JsValue::from_str("message")) {
                msg_val
                    .as_string()
                    .unwrap_or_else(|| "<unknown>".to_string())
            } else {
                format!("{:?}", e)
            };
            web_sys::console::error_1(&format!("[WebCodecs OpusDecoder] error: {}", msg).into());
        }));

        // Build AudioDecoderInit with output & error callbacks.
        let output_fn: &Function = output_cb.as_ref().unchecked_ref();
        let error_fn: &Function = error_cb.as_ref().unchecked_ref();

        let init = AudioDecoderInit::new(error_fn, output_fn);

        let wc_decoder = WcAudioDecoder::new(&init)
            .map_err(|e| NetEqError::DecoderError(format!("AudioDecoder init: {:?}", e)))?;

        // Configure for Opus.
        let cfg = AudioDecoderConfig::new("opus", channels as u32, sample_rate);
        wc_decoder
            .configure(&cfg)
            .map_err(|e| NetEqError::DecoderError(format!("configure: {:?}", e)))?;

        // Leak the closures so they stay alive for the lifetime of the decoder.
        output_cb.forget();
        error_cb.forget();

        Ok(Self {
            wc_decoder,
            pcm_queue: queue,
            sample_rate,
            channels,
            next_timestamp: 0,
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        // Wrap payload in EncodedAudioChunk.
        let len = encoded.len() as u32;
        let data_js = Uint8Array::new_with_length(len);
        data_js.copy_from(encoded);

        let init =
            EncodedAudioChunkInit::new(&data_js.into(), self.next_timestamp as f64, ChunkType::Key);
        init.set_duration(20_000f64.into()); // 20 ms assumption

        let chunk = EncodedAudioChunk::new(&init)
            .map_err(|e| NetEqError::DecoderError(format!("chunk: {:?}", e)))?;

        // Decode – errors are logged, but we continue.
        if let Err(e) = self.wc_decoder.decode(&chunk) {
            let msg = if let Some(err) = e.dyn_ref::<js_sys::Error>() {
                err.message().into()
            } else if let Ok(m) = js_sys::Reflect::get(&e, &JsValue::from_str("message")) {
                m.as_string().unwrap_or_else(|| "<unknown>".to_string())
            } else {
                format!("{:?}", e)
            };
            web_sys::console::error_1(
                &format!("[WebCodecs OpusDecoder] decode error: {}", msg).into(),
            );
        }

        self.next_timestamp += 20_000; // 20 ms in microseconds for monotonic ordering

        // Return the oldest decoded frame if available; otherwise, an empty vec (silence).
        if let Some(frame) = self.pcm_queue.borrow_mut().pop_front() {
            Ok(frame)
        } else {
            web_sys::console::warn_1(
                &"[WebCodecs OpusDecoder] returning silence (queue empty)".into(),
            );
            // No frame ready yet—return silence of 20 ms.
            let samples_per_channel = (self.sample_rate as f32 * 0.02) as usize;
            let total = samples_per_channel * self.channels as usize;
            Ok(vec![0.0; total])
        }
    }
}

// In wasm32 we are single-threaded, so it is safe to mark the decoder as Send/Sync to satisfy
// NetEq's trait bounds even though it contains `JsValue`s which are not inherently thread-safe.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for OpusDecoder {}

#[cfg(target_arch = "wasm32")]
unsafe impl Sync for OpusDecoder {}

// --- Safari fallback ---------------------------------------------------------
// On Safari (WebKit) the WebCodecs `AudioDecoder` API is not available.
// Instead of failing outright, we provide a very small fallback that simply
// returns silence. This keeps the `NetEq` state-machine alive and allows the
// rest of the pipeline (jitter-buffer, playout logic, etc.) to work while we
// develop a full worklet-based decoder.
//
// The fallback is only selected at runtime when the browser is detected to be
// Safari. All other browsers will continue to use the WebCodecs implementation
// above.

// Cached result to avoid repeated checks
use std::sync::OnceLock;
static IS_IOS: OnceLock<bool> = OnceLock::new();

/// Detects if the current environment is likely iOS Safari.
/// Checks user agent and the absence of AudioEncoder API which causes crashes on iOS.
pub fn is_ios() -> bool {
    *IS_IOS.get_or_init(|| {
        if let Some(window) = window() {
            // Check if AudioEncoder exists in window
            let audio_encoder_exists = is_audio_encoder_available();
            if let Ok(ua) = window.navigator().user_agent() {
                let ua_lower = ua.to_lowercase();
                let likely_ios = ua_lower.contains("iphone") || ua_lower.contains("ipad") || ua_lower.contains("ipod");
                // Consider it iOS if the user agent suggests iOS OR if AudioEncoder is missing
                // Audio Encoder may be missing on older browsers too, so we check both conditions
                let result = likely_ios || !audio_encoder_exists;
                log::info!(
                    "Platform detection: User Agent='{}', LikelyiOS={}, AudioEncoderAvailable={}, FinalResult={}",
                    ua, likely_ios, audio_encoder_exists, result
                );
                return result;
            }
        }
        log::warn!("Could not determine platform, assuming not iOS.");
        false // Default to false if detection fails
    })
}

/// Safely check if AudioEncoder is available without crashing
fn is_audio_encoder_available() -> bool {
    // Use reflection to safely check if AudioEncoder exists on the window object
    if let Some(window) = window() {
        let global = JsValue::from(window);

        // First check if AudioEncoder exists on the window object
        match Reflect::has(&global, &JsValue::from_str("AudioEncoder")) {
            Ok(exists) => {
                if !exists {
                    return false;
                }

                // Try to access it to make sure it's properly supported
                match Reflect::get(&global, &JsValue::from_str("AudioEncoder")) {
                    Ok(constructor) => {
                        // Check if it's a function/constructor by verifying it's not undefined/null
                        !constructor.is_undefined() && !constructor.is_null()
                    }
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    } else {
        false
    }
}

/// Minimal placeholder decoder for Safari. At the moment it produces silence
/// but satisfies the `AudioDecoder` trait so the rest of the code builds.
///
/// In the future this can be replaced by a worklet-powered implementation that
/// mirrors the WebCodecs path.  For now, having a working stub is already a
/// big step forward because it unblocks compilation/running on Safari.
#[derive(Debug)]
pub struct SafariOpusDecoder {
    sample_rate: u32,
    channels: u8,
}

impl SafariOpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

impl AudioDecoder for SafariOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, _encoded: &[u8]) -> Result<Vec<f32>> {
        // Produce a frame of silence (20 ms) so timing matches the normal
        // Opus decoder path.
        let samples_per_channel = (self.sample_rate as f32 * 0.02) as usize;
        let total = samples_per_channel * self.channels as usize;
        Ok(vec![0.0; total])
    }
}

// Helper that hides the runtime choice between WebCodecs and Safari fallback.
// Callers should use this instead of invoking `OpusDecoder::new` directly.
#[allow(dead_code)]
pub fn create_opus_decoder(sample_rate: u32, channels: u8) -> Result<Box<dyn AudioDecoder>> {
    if is_ios() {
        Ok(Box::new(SafariOpusDecoder::new(sample_rate, channels)?))
    } else {
        Ok(Box::new(OpusDecoder::new(sample_rate, channels)?))
    }
}
