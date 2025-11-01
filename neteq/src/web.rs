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

use crate::{codec::UnifiedOpusDecoder, AudioPacket, NetEq, NetEqConfig, RtpHeader};
#[cfg(feature = "matomo-logger")]
use matomo_logger::worker as matomo_worker;
use serde_wasm_bindgen;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub struct WebNetEq {
    neteq: std::cell::RefCell<Option<NetEq>>,
    sample_rate: u32,
    channels: u8,
    additional_delay_ms: u32,
}

#[wasm_bindgen]
impl WebNetEq {
    #[wasm_bindgen(constructor)]
    pub fn new(
        sample_rate: u32,
        channels: u8,
        additional_delay_ms: u32,
    ) -> Result<WebNetEq, JsValue> {
        Ok(WebNetEq {
            neteq: std::cell::RefCell::new(None), // Will be initialized in init()
            sample_rate,
            channels,
            additional_delay_ms,
        })
    }

    /// Initialize the NetEq with the appropriate Opus decoder.
    /// This must be called after construction and is async.
    #[wasm_bindgen]
    pub async fn init(&self) -> Result<(), JsValue> {
        let cfg = NetEqConfig {
            sample_rate: self.sample_rate,
            channels: self.channels,
            additional_delay_ms: self.additional_delay_ms,
            ..Default::default()
        };
        let mut neteq = NetEq::new(cfg).map_err(Self::map_err)?;

        // Create the unified decoder that automatically detects browser capabilities
        let decoder = UnifiedOpusDecoder::new(self.sample_rate, self.channels)
            .await
            .map_err(Self::map_err)?;

        log::info!("NetEq initialized with decoder: {}", decoder.decoder_type());

        neteq.register_decoder(111, Box::new(decoder));

        *self.neteq.borrow_mut() = Some(neteq);
        Ok(())
    }

    /// Check if NetEq is initialized
    #[wasm_bindgen(js_name = isInitialized)]
    pub fn is_initialized(&self) -> bool {
        self.neteq.borrow().is_some()
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
        let mut neteq_ref = self.neteq.borrow_mut();
        let neteq = neteq_ref
            .as_mut()
            .ok_or_else(|| JsValue::from_str("NetEq not initialized. Call init() first."))?;

        let hdr = RtpHeader::new(seq_no, timestamp, 0x1234_5678, 111, false);
        let packet = AudioPacket::new(hdr, payload.to_vec(), self.sample_rate, self.channels, 20);
        neteq.insert_packet(packet).map_err(Self::map_err)
    }

    /// Get 10ms of decoded PCM directly from NetEq as a Float32Array.
    #[wasm_bindgen]
    pub fn get_audio(&self) -> Result<js_sys::Float32Array, JsValue> {
        let mut neteq_ref = self.neteq.borrow_mut();
        let neteq = neteq_ref
            .as_mut()
            .ok_or_else(|| JsValue::from_str("NetEq not initialized. Call init() first."))?;

        let frame = neteq.get_audio().map_err(Self::map_err)?;
        let out = js_sys::Float32Array::from(frame.samples.as_slice());
        Ok(out)
    }

    /// Get current NetEq statistics as a JS object.
    #[wasm_bindgen(js_name = getStatistics)]
    pub fn get_statistics(&self) -> Result<JsValue, JsValue> {
        let neteq_ref = self.neteq.borrow();
        let neteq = neteq_ref
            .as_ref()
            .ok_or_else(|| JsValue::from_str("NetEq not initialized. Call init() first."))?;

        let stats = neteq.get_statistics();
        serde_wasm_bindgen::to_value(&stats).map_err(|e| JsValue::from_str(&format!("{:?}", e)))
    }

    fn map_err(e: crate::NetEqError) -> JsValue {
        JsValue::from_str(&e.to_string())
    }
}

#[wasm_bindgen(js_name = initNetEq)]
pub fn init_net_eq() {
    // Initialize worker-side logger bridge: forward WARN+ to main thread (Matomo)
    // Keep console at INFO for local visibility inside the workers
    #[cfg(all(feature = "matomo-logger", target_arch = "wasm32"))]
    {
        use log::LevelFilter;

        if let Err(_e) = matomo_worker::init_with_bridge(LevelFilter::Info, LevelFilter::Debug, {
            // The bridge expects the object as 'arguments[0]'
            js_sys::Function::new_no_args("self.postMessage(arguments[0]);")
        }) {
            use web_sys::console;

            console::error_1(&"[neteq-worker] Failed to initialize matomo worker bridge".into());
        }
    }
}
