/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 */

use crate::{NetEqError, Result};
use opus::{Channels, Decoder};

/// Native Xiph libopus decode backend.
pub(super) struct OpusBackend(Decoder);

impl OpusBackend {
    pub(super) fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        Decoder::new(sample_rate, channels_enum(channels)?)
            .map(Self)
            .map_err(|e| NetEqError::DecoderError(format!("libopus init: {e}")))
    }

    /// Decode one packet into `out`; returns samples decoded per channel.
    pub(super) fn decode_float(&mut self, encoded: &[u8], out: &mut [f32]) -> Result<usize> {
        self.0
            .decode_float(encoded, out, false)
            .map_err(|e| NetEqError::DecoderError(format!("libopus decode: {e}")))
    }
}

impl std::fmt::Debug for OpusBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusBackend")
            .field("implementation", &"libopus")
            .finish_non_exhaustive()
    }
}

fn channels_enum(channels: u8) -> Result<Channels> {
    match channels {
        1 => Ok(Channels::Mono),
        2 => Ok(Channels::Stereo),
        n => Err(NetEqError::InvalidChannelCount(n)),
    }
}
