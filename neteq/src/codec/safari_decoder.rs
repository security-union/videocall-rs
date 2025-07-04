//! Safari-specific Opus decoder implemented via an `AudioWorklet`.
//!
//! Safari (WebKit) does not yet expose the WebCodecs `AudioDecoder` API that
//! we use on Chromium / Firefox.  Instead we spin up a custom worklet
//! (`decoder-worklet`) – the exact same script that is already used by the
//! Yew front-end – and communicate with it over `postMessage`.
//!
//! The worklet sends back raw PCM frames (`Float32Array`) which we collect in
//! a ring-buffer on the Rust side and expose synchronously via `AudioDecoder`.
//!
//! Rough data-flow:
//!   NetEq -> SafariOpusDecoder::decode()  ——postMessage—>  decoder-worklet
//!                                            |                  |
//!                                            |  (PCM plane[0])  |
//!                                            |  (Float32Array)  |
//!                                            v                  |
//!                             (MessagePort.onmessage)   <——postMessage——
//!                                            |                  |
//!                              push samples into queue           |
//!                                            |                  |
//!   NetEq <- SafariOpusDecoder::decode() pops frame <────────────┘
//!
//! NOTE: This file purposefully re-implements a *tiny* subset of the helper
//! utilities that live in `videocall-client/src/decode/safari/…` so the `neteq`
//! crate can stay self-contained.
#![cfg(feature = "web")]

use super::AudioDecoder;
use crate::{NetEqError, Result};
use gloo_utils::format::JsValueSerdeExt;
use js_sys::{Float32Array, Function, Uint8Array};
use serde::Serialize;
use std::{cell::RefCell, collections::VecDeque, rc::Rc};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioContextOptions, AudioWorkletNode, AudioWorkletNodeOptions, MessagePort,
};

// -----------------------------------------------------------------------------
// Worklet message definitions – MUST match decoderWorker.min.js
// -----------------------------------------------------------------------------
#[derive(Serialize, Debug)]
#[serde(tag = "command", rename_all = "camelCase")]
enum DecoderMessages {
    Init { decoder_sample_rate: u32, number_of_channels: u32 },
    Decode { pages: Vec<u8> },
    Flush,
    Close,
}

// -----------------------------------------------------------------------------
// Tiny helper wrapper around AudioWorkletNode
// -----------------------------------------------------------------------------
#[derive(Clone, Default)]
struct WorkletHandle {
    inner: Rc<RefCell<Option<AudioWorkletNode>>>,
}

impl WorkletHandle {
    async fn instantiate(
        &self,
        ctx: &AudioContext,
        script_path: &str,
        name: &str,
        channels: u32,
    ) -> Result<()> {
        // Load module (one-time; OK to call repeatedly – WebKit dedups).
        JsFuture::from(ctx.audio_worklet()?.add_module(script_path)?).await.map_err(|e| {
            NetEqError::DecoderError(format!("addModule: {:?}", e))
        })?;

        let opts = AudioWorkletNodeOptions::new();
        opts.set_number_of_inputs(0); // decoder has no audio inputs
        opts.set_number_of_outputs(0); // we get raw pcm via postMessage instead
        opts.set_channel_count(channels);

        let node = AudioWorkletNode::new_with_options(ctx, name, &opts)?;
        self.inner.borrow_mut().replace(node);
        Ok(())
    }

    fn port(&self) -> Option<MessagePort> {
        self.inner
            .borrow()
            .as_ref()
            .and_then(|n| n.port().ok())
    }

    fn send<T: Serialize>(&self, msg: &T) -> Result<()> {
        let port = self.port().ok_or_else(|| {
            NetEqError::DecoderError("AudioWorklet not instantiated".into())
        })?;
        let js_val = JsValue::from_serde(msg)
            .map_err(|e| NetEqError::DecoderError(format!("serde to JsValue: {e}")))?;
        port.post_message(&js_val)
            .map_err(|e| NetEqError::DecoderError(format!("postMessage: {e:?}")))
    }
}

// -----------------------------------------------------------------------------
// SafariOpusDecoder
// -----------------------------------------------------------------------------

pub struct SafariOpusDecoder {
    worklet: WorkletHandle,
    ctx: AudioContext,
    pcm_queue: Rc<RefCell<VecDeque<Vec<f32>>>>,
    sample_rate: u32,
    channels: u8,
}

impl SafariOpusDecoder {
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        // Shared queue between JS → Rust.
        let queue: Rc<RefCell<VecDeque<Vec<f32>>>> = Rc::new(RefCell::new(VecDeque::new()));

        // Create AudioContext with desired sample-rate.
        let opts = AudioContextOptions::new();
        opts.sample_rate(sample_rate as f32);
        let ctx = AudioContext::new_with_context_options(&opts)
            .map_err(|e| NetEqError::DecoderError(format!("AudioContext: {e:?}")))?;

        // Instantiate worklet.
        let worklet = WorkletHandle::default();
        worklet
            .instantiate(&ctx, "/decoderWorker.min.js", "decoder-worklet", channels as u32)
            .await?;

        // Hook up onmessage to capture PCM.
        {
            let queue_clone = queue.clone();
            let on_msg = Closure::<dyn FnMut(JsValue)>::wrap(Box::new(move |evt: JsValue| {
                if let Some(obj) = js_sys::Object::try_from(&evt).ok() {
                    // Expect { pcm: Float32Array, frames: u32 }
                    if let Ok(pcm_val) = js_sys::Reflect::get(&obj, &JsValue::from_str("pcm")) {
                        if pcm_val.is_instance_of::<Float32Array>() {
                            let pcm_js: Float32Array = pcm_val.unchecked_into();
                            let mut buf = vec![0.0f32; pcm_js.length() as usize];
                            pcm_js.copy_to(&mut buf[..]);
                            queue_clone.borrow_mut().push_back(buf);
                        }
                    }
                }
            }));
            if let Some(port) = worklet.port() {
                port.set_onmessage(Some(on_msg.as_ref().unchecked_ref()));
            }
            on_msg.forget(); // keep closure alive
        }

        // Send init message.
        worklet.send(&DecoderMessages::Init {
            decoder_sample_rate: sample_rate,
            number_of_channels: channels as u32,
        })?;

        Ok(Self {
            worklet,
            ctx,
            pcm_queue: queue,
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

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        // Forward packet to worklet.
        let data_js = Uint8Array::new_with_length(encoded.len() as u32);
        data_js.copy_from(encoded);
        self.worklet.send(&DecoderMessages::Decode {
            pages: encoded.to_vec(),
        })?;

        // Pop oldest decoded frame (20 ms) if available.
        if let Some(frame) = self.pcm_queue.borrow_mut().pop_front() {
            Ok(frame)
        } else {
            // Return silence if nothing decoded yet (startup / loss).
            let samples = (self.sample_rate as f32 * 0.02) as usize * self.channels as usize;
            Ok(vec![0.0; samples])
        }
    }
}

// SAFETY: wasm32 is single-threaded in browsers.
#[cfg(target_arch = "wasm32")]
unsafe impl Send for SafariOpusDecoder {}
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for SafariOpusDecoder {} 