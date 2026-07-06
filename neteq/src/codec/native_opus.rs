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
use crate::Result;

// -----------------------------------------------------------------------------
// Native decode backend (non-wasm): Xiph libopus or pure-Rust `ropus`.
//
// `OpusBackend` is a tiny surface (`new` + `decode_float`) so the
// `NativeOpusDecoder` wrapper and its `AudioDecoder` impl below stay agnostic
// of the codec crate.
// -----------------------------------------------------------------------------

#[cfg(feature = "native-libopus")]
mod libopus;
#[cfg(feature = "native-ropus")]
#[path = "native_opus/ropus.rs"]
mod ropus_backend;

#[cfg(feature = "native-libopus")]
use libopus::OpusBackend;
#[cfg(all(not(feature = "native-libopus"), feature = "native-ropus"))]
use ropus_backend::OpusBackend;

#[cfg(not(target_arch = "wasm32"))]
/// Synchronous native Opus decoder using the selected native backend.
#[derive(Debug)]
pub struct NativeOpusDecoder {
    inner: OpusBackend,
    sample_rate: u32,
    channels: u8,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeOpusDecoder {
    /// Create a new synchronous native Opus decoder
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        Ok(Self {
            inner: OpusBackend::new(sample_rate, channels)?,
            sample_rate,
            channels,
        })
    }

    #[allow(dead_code)]
    pub async fn new_async(sample_rate: u32, channels: u8) -> Result<Self> {
        Self::new(sample_rate, channels)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl AudioDecoder for NativeOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        let max_samples = (self.sample_rate as usize * 120 / 1000) * self.channels as usize;
        let mut buf = vec![0.0f32; max_samples];
        let decoded_samples = self.inner.decode_float(encoded, &mut buf)?;
        buf.truncate(decoded_samples * self.channels as usize);
        Ok(buf)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_mono_silence_packet() {
        let sample_rate = 48_000;
        let samples = vec![0.0; 960];
        let encoded = encode_float(sample_rate, &samples);
        let mut decoder = NativeOpusDecoder::new(sample_rate, 1).unwrap();

        let decoded = decoder.decode(&encoded).unwrap();

        assert_eq!(decoded.len(), samples.len());
        assert!(decoded.iter().all(|sample| sample.is_finite()));
    }

    #[cfg(feature = "native-libopus")]
    fn encode_float(sample_rate: u32, samples: &[f32]) -> Vec<u8> {
        let mut encoder =
            opus::Encoder::new(sample_rate, opus::Channels::Mono, opus::Application::Audio)
                .unwrap();
        let mut encoded = vec![0; 4_000];
        let len = encoder.encode_float(samples, &mut encoded).unwrap();
        encoded.truncate(len);
        encoded
    }

    #[cfg(all(not(feature = "native-libopus"), feature = "native-ropus"))]
    fn encode_float(sample_rate: u32, samples: &[f32]) -> Vec<u8> {
        let mut encoder = ropus::Encoder::builder(
            sample_rate,
            ropus::Channels::Mono,
            ropus::Application::Audio,
        )
        .build()
        .unwrap();
        let mut encoded = vec![0; 4_000];
        let len = encoder.encode_float(samples, &mut encoded).unwrap();
        encoded.truncate(len);
        encoded
    }
}

// -----------------------------------------------------------------------------
// WebCodecs AudioDecoder (web)
// -----------------------------------------------------------------------------

// WebCodecs decoder temporarily disabled - using Safari decoder for all web targets
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)]
/// WebCodecs AudioDecoder wrapper for browsers that support it (Chrome, Firefox)
pub struct NativeOpusDecoder {
    sample_rate: u32,
    channels: u8,
}

#[cfg(target_arch = "wasm32")]
impl NativeOpusDecoder {
    #[allow(dead_code)]
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        // WebCodecs implementation disabled for now
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

#[cfg(target_arch = "wasm32")]
impl AudioDecoder for NativeOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, _encoded: &[u8]) -> Result<Vec<f32>> {
        // WebCodecs implementation disabled - return silence
        let samples = (self.sample_rate as f32 * 0.02) as usize * self.channels as usize;
        Ok(vec![0.0; samples])
    }
}
