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

use js_sys::Float32Array;
use wasm_bindgen::prelude::*;
use web_sys::AudioWorkletNode;

/// Interface for sending PCM audio data to a specific peer's worklet
/// in the shared NetEq audio architecture
pub struct NetEqPeerSink {
    pub(crate) peer_id: String,
    pub(crate) pcm_worklet: Option<AudioWorkletNode>,
}

impl NetEqPeerSink {
    /// Send PCM data to this peer's dedicated worklet
    pub fn send_pcm(&self, pcm: Float32Array) -> Result<(), JsValue> {
        if let Some(worklet) = &self.pcm_worklet {
            let message = js_sys::Object::new();
            js_sys::Reflect::set(&message, &"command".into(), &"play".into())?;
            js_sys::Reflect::set(&message, &"pcm".into(), &pcm)?;

            worklet.port()?.post_message(&message)?;
        }
        Ok(())
    }

    /// Flush this peer's audio buffer
    pub fn flush(&self) -> Result<(), JsValue> {
        if let Some(worklet) = &self.pcm_worklet {
            let message = js_sys::Object::new();
            js_sys::Reflect::set(&message, &"command".into(), &"flush".into())?;
            worklet.port()?.post_message(&message)?;
        }
        Ok(())
    }

    /// Get the peer ID this sink is associated with
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }
}
