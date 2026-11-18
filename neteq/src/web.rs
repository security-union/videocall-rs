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

//! Browser wrapper around [`NetEq`] for a Dedicated Web Worker.
//!
//! One `WebNetEq` decodes one remote Opus stream. Opus runs on `ropus`
//! inside the worker (PARITY_PLAN.md 8.2): NetEq decodes synchronously
//! inside `GetAudio`, and the asynchronous WebCodecs `AudioDecoder` cannot
//! give it that, so the web path is the same code as the native path.
//!
//! Configuration (PARITY_PLAN.md 8.4) follows what Chrome's audio receiver
//! uses: WebRTC defaults, no base minimum delay, smart flushing off, and
//! `enable_muted_state` on so a silent stream costs almost nothing per tick.
//! The 0.x crate added a fixed 80 ms on top of the target delay; that knob
//! compensated for millisecond timestamps NetEq could not measure jitter
//! from, and WebRTC has no equivalent, so it is gone.

use crate::api::{AudioFrame, Config, NetEq, RtpHeader, RtpPacketInfo, SdpAudioFormat};
use crate::api::{Clock, SystemClock};
use crate::telemetry::{NetEqStats, TelemetryCollector};
#[cfg(feature = "matomo-logger")]
use matomo_logger::worker as matomo_worker;
use wasm_bindgen::prelude::*;

/// RTP payload type the worker registers Opus under.
pub const OPUS_PAYLOAD_TYPE: u8 = 111;

/// Opus RTP clock (RFC 7587). NetEq output runs at this rate too.
pub const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;

/// One `GetAudio` call: 10 ms.
pub const FRAME_MS: u32 = 10;

/// Samples per channel in one 10 ms output frame at 48 kHz.
pub const FRAME_SAMPLES_PER_CHANNEL: usize = (OPUS_SAMPLE_RATE_HZ / 1000 * FRAME_MS) as usize;

/// SSRC used for every packet. The stream is identified by the worker, one
/// worker per remote peer.
const SSRC: u32 = 0x1234_5678;

/// NetEq for one remote Opus stream, with dashboard telemetry.
#[wasm_bindgen]
pub struct WebNetEq {
    neteq: NetEq,
    collector: TelemetryCollector,
    /// Reused for every `GetAudio`, so a tick allocates nothing here.
    frame: AudioFrame,
    channels: usize,
}

impl std::fmt::Debug for WebNetEq {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebNetEq")
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

fn now_us() -> i64 {
    SystemClock.current_time_us()
}

fn now_ms() -> u64 {
    u64::try_from(now_us() / 1000).unwrap_or(0)
}

/// Opus SDP for `channels` output channels (RFC 7587: always `opus/48000/2`;
/// `stereo=1` asks for a stereo decoder).
fn opus_format(channels: usize) -> SdpAudioFormat {
    let mut format = SdpAudioFormat::new("opus", OPUS_SAMPLE_RATE_HZ as i32, 2);
    if channels == 2 {
        format
            .parameters
            .insert("stereo".to_string(), "1".to_string());
    }
    format
}

impl WebNetEq {
    /// NetEq for a 48 kHz Opus stream with `channels` (1 or 2) channels.
    pub fn try_new(sample_rate: u32, channels: u8) -> crate::Result<WebNetEq> {
        if sample_rate != OPUS_SAMPLE_RATE_HZ {
            return Err(crate::NetEqError::InvalidSampleRate(sample_rate));
        }
        if !(1..=2).contains(&channels) {
            return Err(crate::NetEqError::InvalidChannelCount(channels));
        }
        let channels = usize::from(channels);
        let config = Config {
            sample_rate_hz: OPUS_SAMPLE_RATE_HZ as i32,
            enable_muted_state: true,
            ..Config::default()
        };
        let mut neteq = NetEq::builder(config).build();
        if !neteq.register_payload_type(i32::from(OPUS_PAYLOAD_TYPE), &opus_format(channels)) {
            return Err(crate::NetEqError::InvalidConfig(
                "Opus payload type rejected".to_string(),
            ));
        }
        // Build the decoder now rather than on the first packet.
        neteq.create_decoder(i32::from(OPUS_PAYLOAD_TYPE));
        Ok(WebNetEq {
            neteq,
            collector: TelemetryCollector::new(),
            frame: AudioFrame::new(),
            channels,
        })
    }

    /// Insert one Opus packet. `timestamp` is in 48 kHz samples.
    pub fn insert(&mut self, seq_no: u16, timestamp: u32, payload: &[u8]) -> crate::Result<()> {
        let header = RtpHeader {
            marker_bit: false,
            payload_type: OPUS_PAYLOAD_TYPE,
            sequence_number: seq_no,
            timestamp,
            ssrc: SSRC,
        };
        let receive_time_us = now_us();
        self.collector
            .on_packet_inserted(seq_no, u64::try_from(receive_time_us / 1000).unwrap_or(0));
        self.neteq.insert_packet(
            &header,
            payload,
            RtpPacketInfo::new(&header, Some(receive_time_us)),
        )
    }

    /// Run one `GetAudio` and append its 10 ms to `out` as `f32` in
    /// `[-1, 1)`. Returns false when NetEq reported a muted (all-zero) frame.
    /// On error, appends 10 ms of silence and returns the error.
    pub fn get_audio_into(&mut self, out: &mut Vec<f32>) -> crate::Result<bool> {
        let result = self.neteq.get_audio(&mut self.frame, None);
        self.collector
            .on_get_audio(self.neteq.last_operation(), now_ms());
        // Always append exactly one frame so the worklet's sample accounting
        // holds even if NetEq ever returned another shape.
        let frame_len = self.frame_len();
        let start = out.len();
        match result {
            Ok(status) => {
                let data = self.frame.data();
                let audible = !status.muted && !data.is_empty();
                if audible {
                    let take = data.len().min(frame_len);
                    out.extend(
                        data[..take]
                            .iter()
                            .map(|&sample| f32::from(sample) / 32768.0),
                    );
                }
                out.resize(start + frame_len, 0.0);
                Ok(audible)
            }
            Err(e) => {
                out.resize(start + frame_len, 0.0);
                Err(e)
            }
        }
    }

    /// Interleaved samples in one 10 ms output frame.
    pub fn frame_len(&self) -> usize {
        FRAME_SAMPLES_PER_CHANNEL * self.channels
    }

    /// Output channel count.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Dashboard statistics. Does not reset NetEq's counters.
    pub fn stats(&self) -> NetEqStats {
        NetEqStats::from_neteq(&self.neteq, &self.collector)
    }

    /// The wrapped NetEq.
    pub fn neteq(&self) -> &NetEq {
        &self.neteq
    }
}

#[wasm_bindgen]
impl WebNetEq {
    /// NetEq for a 48 kHz Opus stream with `channels` (1 or 2) channels.
    #[wasm_bindgen(constructor)]
    pub fn new(sample_rate: u32, channels: u8) -> Result<WebNetEq, JsValue> {
        Self::try_new(sample_rate, channels).map_err(Self::map_err)
    }

    /// Insert an encoded Opus packet.
    /// `seq_no` – 16-bit RTP sequence number
    /// `timestamp` – RTP timestamp in 48 kHz samples
    /// `payload` – the Opus packet
    #[wasm_bindgen]
    pub fn insert_packet(
        &mut self,
        seq_no: u16,
        timestamp: u32,
        payload: &[u8],
    ) -> Result<(), JsValue> {
        self.insert(seq_no, timestamp, payload)
            .map_err(Self::map_err)
    }

    /// 10 ms of decoded audio as a Float32Array (interleaved).
    #[wasm_bindgen]
    pub fn get_audio(&mut self) -> Result<js_sys::Float32Array, JsValue> {
        let mut pcm = Vec::with_capacity(self.frame_len());
        self.get_audio_into(&mut pcm).map_err(Self::map_err)?;
        Ok(js_sys::Float32Array::from(pcm.as_slice()))
    }

    /// Drop every buffered packet and the decoded audio not yet played.
    /// The next packet starts a new stream.
    #[wasm_bindgen]
    pub fn flush(&mut self) {
        self.neteq.flush_buffers();
        self.collector.on_flush();
    }

    /// Statistics as a JS object in the `telemetry::NetEqStats` schema.
    #[wasm_bindgen(js_name = getStatistics)]
    pub fn get_statistics(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.stats())
            .map_err(|e| JsValue::from_str(&format!("{e:?}")))
    }

    fn map_err(e: crate::NetEqError) -> JsValue {
        JsValue::from_str(&e.to_string())
    }
}

#[wasm_bindgen(js_name = initNetEq)]
pub fn init_net_eq() {
    // Initialize worker-side logger bridge: forward WARN+ to main thread
    // (Matomo). Lower levels stay in the worker's console, so per-packet
    // debug logs never cost a postMessage. Console stays at INFO for local
    // visibility inside the workers.
    #[cfg(feature = "matomo-logger")]
    {
        use log::LevelFilter;

        if let Err(_e) = matomo_worker::init_with_bridge(LevelFilter::Info, LevelFilter::Warn, {
            // The bridge expects the object as 'arguments[0]'
            js_sys::Function::new_no_args("self.postMessage(arguments[0]);")
        }) {
            web_sys::console::error_1(
                &"[neteq-worker] Failed to initialize matomo worker bridge".into(),
            );
        }
    }
}
