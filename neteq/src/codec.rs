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

//! Audio codec support for NetEq.

use crate::Result;

/// Trait for audio decoders.
pub trait AudioDecoder {
    /// Returns the sample rate of the decoder.
    fn sample_rate(&self) -> u32;

    /// Returns the number of channels.
    fn channels(&self) -> u8;

    /// Decodes a single audio packet.
    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>>;
}

// Platform-specific codec implementations
#[cfg(feature = "web")]
mod browser_detect;
#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
mod native_opus;
#[cfg(feature = "web")]
mod safari_decoder;
#[cfg(not(feature = "web"))]
mod wasm_stub;
#[cfg(feature = "web")]
mod webcodecs_decoder;

#[cfg(feature = "web")]
pub use safari_decoder::SafariOpusDecoder;
#[cfg(feature = "web")]
pub use webcodecs_decoder::WebCodecsAudioDecoder;

// Always export NativeOpusDecoder for native targets
#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
pub use native_opus::NativeOpusDecoder;

#[cfg(not(feature = "web"))]
pub use wasm_stub::*;

// Stub implementation for when native feature is not available
#[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
pub struct StubOpusDecoder {
    sample_rate: u32,
    channels: u8,
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
impl StubOpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        log::warn!("Using stub Opus decoder - no actual decoding will occur");
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

#[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
impl AudioDecoder for StubOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, _encoded: &[u8]) -> Result<Vec<f32>> {
        // Return silence for 20ms frame
        let samples_per_channel = (self.sample_rate as f32 * 0.02) as usize;
        let total_samples = samples_per_channel * self.channels as usize;
        Ok(vec![0.0; total_samples])
    }
}

// -----------------------------------------------------------------------------
// Unified Opus Decoder - Browser Detection & Selection
// -----------------------------------------------------------------------------

#[cfg(feature = "web")]
enum DecoderBackend {
    WebCodecs(webcodecs_decoder::WebCodecsAudioDecoder),
    JsLibrary(safari_decoder::SafariOpusDecoder),
}

#[cfg(feature = "web")]
/// Unified Opus decoder with automatic backend selection
/// - iOS/Safari: Uses opus-decoder JS library
/// - Chrome/Android: Uses WebCodecs hardware acceleration
pub struct UnifiedOpusDecoder {
    decoder: DecoderBackend,
    sample_rate: u32,
    channels: u8,
}

#[cfg(feature = "web")]
impl UnifiedOpusDecoder {
    /// Creates a new unified decoder with automatic backend selection
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let backend = browser_detect::detect_audio_backend().await;

        let decoder = match backend {
            browser_detect::AudioBackend::WebCodecs => {
                log::info!("Initializing WebCodecs AudioDecoder (hardware-accelerated)");
                let dec =
                    webcodecs_decoder::WebCodecsAudioDecoder::new(sample_rate, channels).await?;
                DecoderBackend::WebCodecs(dec)
            }
            browser_detect::AudioBackend::JsLibrary => {
                log::info!("Initializing opus-decoder JS library (Safari/iOS)");
                let mut dec = safari_decoder::SafariOpusDecoder::new(sample_rate, channels);
                dec.init_decoder().await?;
                DecoderBackend::JsLibrary(dec)
            }
        };

        Ok(Self {
            decoder,
            sample_rate,
            channels,
        })
    }

    /// Get the decoder type for debugging
    pub fn decoder_type(&self) -> &'static str {
        match &self.decoder {
            DecoderBackend::WebCodecs(d) => d.get_decoder_type(),
            DecoderBackend::JsLibrary(d) => d.get_decoder_type(),
        }
    }

    /// Enable audio playback (kept for compatibility, now a no-op)
    pub async fn enable_audio_playback(
        &self,
        _audio_context: &web_sys::AudioContext,
    ) -> Result<()> {
        Ok(())
    }

    /// Flush audio buffers (kept for compatibility, now a no-op)
    pub fn flush_audio(&self) -> Result<()> {
        Ok(())
    }

    /// Async decode method for internal use
    pub async fn decode_async(&mut self, encoded: &[u8]) -> Vec<f32> {
        match &mut self.decoder {
            DecoderBackend::WebCodecs(d) => d.decode(encoded).unwrap_or_default(),
            DecoderBackend::JsLibrary(d) => d.decode_sync(encoded),
        }
    }
}

#[cfg(feature = "web")]
impl AudioDecoder for UnifiedOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        match &mut self.decoder {
            DecoderBackend::WebCodecs(d) => d.decode(encoded),
            DecoderBackend::JsLibrary(d) => Ok(d.decode_sync(encoded)),
        }
    }
}

// Convenience type alias for the recommended decoder
#[cfg(target_arch = "wasm32")]
pub type OpusDecoder = UnifiedOpusDecoder;

#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
pub type OpusDecoder = native_opus::NativeOpusDecoder;

// For non-wasm32 targets without native feature, use stub
#[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
pub type OpusDecoder = StubOpusDecoder;
