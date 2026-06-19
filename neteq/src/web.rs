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
use serde_wasm_bindgen;
use wasm_bindgen::prelude::*;

/// Upper bound on NetEQ's adaptive jitter-buffer target for the browser/wasm path (issue #1299).
///
/// Without this, the wasm config leaves `max_delay_ms = 0`, so the adaptive target is bounded only
/// by the derived 3000ms cap (`base_maximum_delay_ms = max_packets_in_buffer*20*3/4`). A jitter or
/// stall episode then ratchets the 97th-percentile target toward 3s and gates Accelerate's catch-up
/// off — the target re-labels multi-second lag as the "correct" buffer depth (the #1299 mechanism).
///
/// Setting `max_delay_ms` here engages `DelayManager::set_maximum_delay` (neteq.rs), capping the
/// effective maximum target so Accelerate's setpoint stays low. 300ms sits in the issue's
/// recommended 200–400ms range: high enough to absorb normal mobile/high-latency jitter, low enough
/// that the steady-state target can never approach the seconds-deep regime. This is necessary but
/// not sufficient on its own — Accelerate cannot claw back seconds already buffered — which is why
/// the resync-to-live governor (also #1299, in `NetEq::maybe_resync_to_live`) is the real fix.
const RESYNC_MAX_DELAY_MS: u32 = 300;

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
            // Bound the adaptive jitter-buffer target so it can't ratchet to the 3s cap and gate
            // Accelerate off (issue #1299, part 2). The resync-to-live governor (part 1) is enabled
            // by the NetEqConfig defaults (resync_ceiling_ms / resync_cooldown_ms).
            max_delay_ms: RESYNC_MAX_DELAY_MS,
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

    /// Flush the jitter/packet buffer and reset internal state (issue #1402).
    ///
    /// Called when a peer's audio stream ends (mic-off / host force-mute) so the
    /// NetEq buffer stops emitting expand/comfort-noise concealment ("hiss")
    /// packets for a stream that is no longer producing data. Delegates to
    /// [`NetEq::flush`], which drains the packet buffer, clears leftover samples,
    /// and resets the delay/governor state. A no-op (not an error) before
    /// `init()` — there is nothing buffered to flush.
    #[wasm_bindgen]
    pub fn flush(&self) {
        if let Some(neteq) = self.neteq.borrow_mut().as_mut() {
            neteq.flush();
        }
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

/// Initialization hook for WebAssembly workers. Currently a no-op; retained
/// for API compatibility with existing worker bootstrap scripts.
#[wasm_bindgen(js_name = initNetEq)]
pub fn init_net_eq() {}
