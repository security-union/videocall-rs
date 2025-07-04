use super::AudioDecoder;
use crate::{NetEqError, Result};

#[derive(Debug, Default)]
pub struct OpusDecoder;

impl OpusDecoder {
    pub fn new(_sample_rate: u32, _channels: u8) -> Result<Self> {
        Err(NetEqError::DecoderError(
            "Opus decoder unavailable in wasm build".into(),
        ))
    }
}

impl AudioDecoder for OpusDecoder {
    fn sample_rate(&self) -> u32 {
        48000
    }
    fn channels(&self) -> u8 {
        1
    }
    fn decode(&mut self, _encoded: &[u8]) -> Result<Vec<f32>> {
        Err(NetEqError::DecoderError(
            "decode() called on wasm stub".into(),
        ))
    }
}
