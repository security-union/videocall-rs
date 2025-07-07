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

// WebAssembly (browser) wrapper around NetEq that exposes a small API for use inside
// a Dedicated Web Worker or AudioWorklet.

use crate::{codec::OpusDecoder, AudioPacket, NetEq, NetEqConfig, RtpHeader};
use serde_wasm_bindgen;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WebNetEq {
    neteq: std::cell::RefCell<NetEq>,
    leftovers: std::cell::RefCell<Vec<f32>>, // cached PCM between quanta
    sample_rate: u32,
    channels: u8,
}

#[wasm_bindgen]
impl WebNetEq {
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate: u32, channels: u8) -> Result<WebNetEq, JsValue> {
        let cfg = NetEqConfig {
            sample_rate,
            channels,
            ..Default::default()
        };
        let mut neteq = NetEq::new(cfg).map_err(Self::map_err)?;
        neteq.register_decoder(
            111,
            Box::new(OpusDecoder::new(sample_rate, channels).map_err(Self::map_err)?),
        );
        Ok(WebNetEq {
            neteq: std::cell::RefCell::new(neteq),
            leftovers: std::cell::RefCell::new(Vec::new()),
            sample_rate,
            channels,
        })
    }

    /// Insert an encoded Opus packet (RTP-like) into NetEq.
    /// `seq_no` – 16-bit RTP sequence
    /// `timestamp` – RTP timestamp (samples)
    /// `payload` – the compressed Opus data
    #[wasm_bindgen]
    pub fn insert_packet(
        &self,
        seq_no: u16,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), JsValue> {
        let hdr = RtpHeader::new(seq_no, timestamp, 0x1234_5678, 111, false);
        let packet = AudioPacket::new(hdr, payload.to_vec(), self.sample_rate, self.channels, 20);
        self.neteq
            .borrow_mut()
            .insert_packet(packet)
            .map_err(Self::map_err)
    }

    /// Get up to 10 ms of decoded PCM as a Float32Array.
    #[wasm_bindgen]
    pub fn get_audio(&self) -> Result<js_sys::Float32Array, JsValue> {
        if self.leftovers.borrow().is_empty() {
            let frame = self.neteq.borrow_mut().get_audio().map_err(Self::map_err)?;
            self.leftovers
                .borrow_mut()
                .extend_from_slice(&frame.samples);
        }
        // Consume everything we have (could be less/more than render quantum).
        let mut pcm = self.leftovers.borrow_mut();
        let out = js_sys::Float32Array::from(pcm.as_slice());
        pcm.clear();
        Ok(out)
    }

    /// Get current NetEq statistics as a JS object.
    #[wasm_bindgen(js_name = getStatistics)]
    pub fn get_statistics(&self) -> Result<JsValue, JsValue> {
        let stats = self.neteq.borrow().get_statistics();
        serde_wasm_bindgen::to_value(&stats).map_err(|e| JsValue::from_str(&format!("{:?}", e)))
    }

    fn map_err(e: crate::NetEqError) -> JsValue {
        JsValue::from_str(&e.to_string())
    }
}
