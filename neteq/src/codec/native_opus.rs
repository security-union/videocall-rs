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
// Native decode backend (non-wasm), selected at compile time:
//   * default           -> libopus via the C `opus` crate
//   * `ropus-codec`      -> pure-Rust `ropus` (no C toolchain; wasm-clean core)
//
// Both expose the same minimal `OpusBackend` surface (`new` + `decode_float`),
// so the `NativeOpusDecoder` wrapper and its `AudioDecoder` impl below are
// backend-agnostic and written exactly once.
// -----------------------------------------------------------------------------

#[cfg(all(not(target_arch = "wasm32"), not(feature = "ropus-codec")))]
mod backend {
    use crate::{NetEqError, Result};
    use opus::{Channels, Decoder};

    /// libopus decode backend (C, via the `opus` crate).
    pub struct OpusBackend(Decoder);

    impl OpusBackend {
        pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
            Decoder::new(sample_rate, channels_enum(channels)?)
                .map(Self)
                .map_err(|e| NetEqError::DecoderError(format!("Opus init: {e}")))
        }

        /// Decode one packet into `out`; returns samples decoded per channel.
        pub fn decode_float(&mut self, encoded: &[u8], out: &mut [f32]) -> Result<usize> {
            self.0
                .decode_float(encoded, out, false)
                .map_err(|e| NetEqError::DecoderError(format!("Opus decode: {e}")))
        }
    }

    fn channels_enum(channels: u8) -> Result<Channels> {
        match channels {
            1 => Ok(Channels::Mono),
            2 => Ok(Channels::Stereo),
            n => Err(NetEqError::InvalidChannelCount(n)),
        }
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "ropus-codec"))]
mod backend {
    use crate::{NetEqError, Result};
    use ropus::{Channels, DecodeMode, Decoder};

    /// Pure-Rust decode backend (`ropus`).
    pub struct OpusBackend(Decoder);

    impl OpusBackend {
        pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
            Decoder::new(sample_rate, channels_enum(channels)?)
                .map(Self)
                .map_err(|e| NetEqError::DecoderError(format!("ropus init: {e}")))
        }

        /// Decode one packet into `out`; returns samples decoded per channel.
        pub fn decode_float(&mut self, encoded: &[u8], out: &mut [f32]) -> Result<usize> {
            self.0
                .decode_float(encoded, out, DecodeMode::Normal)
                .map_err(|e| NetEqError::DecoderError(format!("ropus decode: {e}")))
        }
    }

    fn channels_enum(channels: u8) -> Result<Channels> {
        match channels {
            1 => Ok(Channels::Mono),
            2 => Ok(Channels::Stereo),
            n => Err(NetEqError::InvalidChannelCount(n)),
        }
    }
}

// `ropus::Decoder` does not implement `Debug`, so give the backend an opaque
// manual impl. This keeps `NativeOpusDecoder`'s derived `Debug` working
// identically for both backends.
#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for backend::OpusBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusBackend").finish_non_exhaustive()
    }
}

#[cfg(not(target_arch = "wasm32"))]
use backend::OpusBackend;

#[cfg(not(target_arch = "wasm32"))]
/// Synchronous native Opus decoder.
///
/// Wraps a compile-time-selected [`OpusBackend`]: libopus (the C `opus` crate)
/// by default, or the pure-Rust `ropus` codec under the `ropus-codec` feature.
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
