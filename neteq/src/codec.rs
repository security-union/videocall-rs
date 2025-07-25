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
mod native_opus;
#[cfg(not(feature = "web"))]
mod native_opus;
#[cfg(feature = "web")]
mod safari_decoder;
#[cfg(not(feature = "web"))]
mod wasm_stub;

#[cfg(feature = "web")]
pub use safari_decoder::SafariOpusDecoder;

#[cfg(not(feature = "web"))]
pub use native_opus::NativeOpusDecoder;
#[cfg(not(feature = "web"))]
pub use wasm_stub::*;

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

    /// Creates a new unified decoder (audio context parameter kept for compatibility)
    pub async fn new_with_playback(
        sample_rate: u32,
        channels: u8,
        _audio_context: Option<&web_sys::AudioContext>,
    ) -> Result<Self> {
        Self::new(sample_rate, channels).await
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
#[cfg(feature = "web")]
pub type OpusDecoder = UnifiedOpusDecoder;

#[cfg(not(feature = "web"))]
pub type OpusDecoder = native_opus::NativeOpusDecoder;
