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

use crate::Result;

// Trait common to all decoders -------------------------------------------------
pub trait AudioDecoder {
    /// Return sample-rate in Hz of produced PCM.
    fn sample_rate(&self) -> u32;
    /// Return number of interleaved output channels.
    fn channels(&self) -> u8;
    /// Decode an encoded packet into PCM samples (interleaved f32).
    fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>>;
}

// Select implementation depending on target -----------------------------------
#[cfg(not(target_arch = "wasm32"))]
mod native_opus;
#[cfg(not(target_arch = "wasm32"))]
pub use native_opus::OpusDecoder;

#[cfg(target_arch = "wasm32")]
mod wasm_stub;
#[cfg(target_arch = "wasm32")]
pub use wasm_stub::OpusDecoder;
