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
#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
mod native_opus;
#[cfg(feature = "web")]
mod safari_decoder;
#[cfg(not(feature = "web"))]
mod wasm_stub;

#[cfg(feature = "web")]
pub use safari_decoder::SafariOpusDecoder;

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
/// Unified Opus decoder using opus-decoder library for Safari compatibility
pub struct UnifiedOpusDecoder {
    /// SafariOpusDecoder using opus-decoder library
    decoder: safari_decoder::SafariOpusDecoder,
    /// Cached sample rate (48kHz for Opus)
    sample_rate: u32,
    /// Cached channel count (mono for now)
    channels: u8,
}

#[cfg(feature = "web")]
impl UnifiedOpusDecoder {
    /// Creates a new unified decoder using opus-decoder library
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let mut decoder = safari_decoder::SafariOpusDecoder::new(sample_rate, channels);
        // Initialize the decoder during construction
        decoder.init_decoder().await?;
        Ok(Self {
            decoder,
            sample_rate,
            channels,
        })
    }

    /// Get the decoder type for debugging
    pub fn decoder_type(&self) -> &'static str {
        self.decoder.get_decoder_type()
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
        self.decoder.decode_sync(encoded)
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
        // Use the synchronous decode method (decoder is already initialized)
        let samples = self.decoder.decode_sync(encoded);
        Ok(samples)
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
