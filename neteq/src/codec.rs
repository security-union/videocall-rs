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

use crate::{NetEqError, Result};

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
#[cfg(feature = "web")]
mod safari_decoder;
#[cfg(not(feature = "web"))]
mod wasm_stub;

#[cfg(feature = "web")]
pub use safari_decoder::SafariOpusDecoder;

#[cfg(not(feature = "web"))]
pub use wasm_stub::*;

// -----------------------------------------------------------------------------
// Unified Opus Decoder - Browser Detection & Selection
// -----------------------------------------------------------------------------

#[cfg(feature = "web")]
/// Unified Opus decoder - currently only uses Safari implementation
pub struct UnifiedOpusDecoder {
    /// SafariOpusDecoder for all web targets
    decoder: safari_decoder::SafariOpusDecoder,
}

#[cfg(feature = "web")]
impl UnifiedOpusDecoder {
    /// Creates a new unified decoder (currently always uses Safari implementation)
    pub async fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let decoder = safari_decoder::SafariOpusDecoder::new(sample_rate, channels).await?;
        Ok(Self { decoder })
    }

    /// Creates a new unified decoder with optional audio playback for Safari
    pub async fn new_with_playback(
        sample_rate: u32,
        channels: u8,
        audio_context: Option<&web_sys::AudioContext>,
    ) -> Result<Self> {
        let decoder = safari_decoder::SafariOpusDecoder::new(sample_rate, channels).await?;

        // Initialize audio playback if context provided
        if let Some(ctx) = audio_context {
            decoder.init_audio_playback(ctx).await?;
        }

        Ok(Self { decoder })
    }

    /// Check if the browser supports WebCodecs AudioDecoder
    fn has_webcodecs_support() -> bool {
        // For now, always return false to use Safari decoder
        // This avoids thread safety issues with WebCodecs implementation
        false
    }

    /// Get the decoder type for debugging
    pub fn decoder_type(&self) -> &'static str {
        self.decoder.decoder_type()
    }

    /// Enable audio playback for Safari decoder
    pub async fn enable_audio_playback(&self, audio_context: &web_sys::AudioContext) -> Result<()> {
        self.decoder.init_audio_playback(audio_context).await
    }

    /// Flush audio buffers
    pub fn flush_audio(&self) -> Result<()> {
        self.decoder.flush_audio()
    }
}

#[cfg(feature = "web")]
impl AudioDecoder for UnifiedOpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.decoder.sample_rate()
    }

    fn channels(&self) -> u8 {
        self.decoder.channels()
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        self.decoder.decode(encoded)
    }
}

// Convenience type alias for the recommended decoder
#[cfg(feature = "web")]
pub type OpusDecoder = UnifiedOpusDecoder;

#[cfg(not(feature = "web"))]
pub type OpusDecoder = wasm_stub::StubOpusDecoder;
