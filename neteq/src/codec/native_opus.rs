use crate::{NetEqError, Result};
use opus::{Channels, Decoder as OpusInner};

use super::AudioDecoder;

/// Wrapper around libopus via the `opus` crate` (native targets).
#[derive(Debug)]
pub struct OpusDecoder {
    inner: OpusInner,
    sample_rate: u32,
    channels: u8,
}

impl OpusDecoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self> {
        let ch_enum = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => return Err(NetEqError::InvalidChannelCount(channels)),
        };
        let inner = OpusInner::new(sample_rate, ch_enum)
            .map_err(|e| NetEqError::DecoderError(format!("Opus init: {e}")))?;
        Ok(Self {
            inner,
            sample_rate,
            channels,
        })
    }
}

impl AudioDecoder for OpusDecoder {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u8 {
        self.channels
    }

    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
        let max_samples = (self.sample_rate as usize * 120 / 1000) * self.channels as usize;
        let mut buf = vec![0.0f32; max_samples];
        let decoded_samples = self
            .inner
            .decode_float(encoded, &mut buf, false)
            .map_err(|e| NetEqError::DecoderError(format!("Opus decode: {e}")))?;
        buf.truncate(decoded_samples * self.channels as usize);
        Ok(buf)
    }
}
