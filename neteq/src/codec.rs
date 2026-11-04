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

    /// Produce a packet-loss-concealment (PLC) frame from the decoder's internal
    /// state, for use when a packet is missing (issue 620).
    ///
    /// # Contract
    /// - `num_samples_per_channel` is the number of samples **per channel** the
    ///   caller needs (e.g. 480 for a 10 ms frame at 48 kHz).
    /// - On success the returned vector is **interleaved** `f32` PCM containing
    ///   exactly `num_samples_per_channel * channels()` samples.
    /// - Returning `None` means "this decoder has no codec-level PLC primitive,
    ///   or concealment failed": the caller then falls back to the quiet-noise
    ///   `Expand` path. The default implementation returns `None`, so a decoder
    ///   opts in explicitly by overriding this method.
    ///
    /// Codec PLC is stateful: it extrapolates the missing frame from the audio
    /// of previously decoded packets, so it is only meaningful after at least
    /// one real packet has been decoded on this decoder instance.
    fn decode_plc(&mut self, num_samples_per_channel: usize) -> Option<Vec<f32>> {
        let _ = num_samples_per_channel;
        None
    }
}

/// The Opus module embedded in `scripts/opus-decoder.min.js`, inflated (#3210).
/// `opus_decoder_wasm_tests` keeps the two files in step.
#[cfg(any(feature = "web", test))]
pub const OPUS_DECODER_WASM: &[u8] = include_bytes!("scripts/opus-decoder.wasm");

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
        let backend = browser_detect::detect_audio_backend();

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

    // NOTE (issue 620): neither web backend (WebCodecs nor the opus-decoder JS
    // library) exposes a libopus-style PLC primitive, so `decode_plc` is left as
    // the trait default (`None`). On the browser production path, packet-loss
    // concealment therefore falls back to NetEQ's quiet-noise `Expand` path.
    // Native targets override `decode_plc` (see `NativeOpusDecoder`).
}

// Convenience type alias for the recommended decoder
#[cfg(target_arch = "wasm32")]
pub type OpusDecoder = UnifiedOpusDecoder;

#[cfg(all(not(target_arch = "wasm32"), feature = "native"))]
pub type OpusDecoder = native_opus::NativeOpusDecoder;

// For non-wasm32 targets without native feature, use stub
#[cfg(all(not(target_arch = "wasm32"), not(feature = "native")))]
pub type OpusDecoder = StubOpusDecoder;

#[cfg(test)]
mod opus_decoder_wasm_tests {
    use super::OPUS_DECODER_WASM;

    const SCRIPT: &str = include_str!("scripts/opus-decoder.min.js");

    /// The library's `simple-yenc` decoder (with its `dynEncode` header and crc32 check), as the
    /// script runs it before inflating.
    fn yenc_decode(text: &str) -> Vec<u8> {
        const CP1252: [u32; 33] = [
            0, 8364, 0, 8218, 402, 8222, 8230, 8224, 8225, 710, 8240, 352, 8249, 338, 0, 381, 0, 0,
            8216, 8217, 8220, 8221, 8226, 8211, 8212, 732, 8482, 353, 8250, 339, 0, 382, 376,
        ];
        let units: Vec<u32> = text.encode_utf16().map(u32::from).collect();
        assert!(text.starts_with("dynEncode"));
        let hex = |from: usize, to: usize| {
            u32::from_str_radix(&text[from..to], 16).expect("hex in the dynEncode header")
        };
        let version = hex(9, 11);
        assert_eq!(version, 1, "the dynEncode version this test decodes");
        let offset = hex(11, 13);
        let crc = i32::from_le_bytes(
            (0..4)
                .map(|k| hex(13 + 2 * k, 15 + 2 * k) as u8)
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );
        let mut out = Vec::with_capacity(units.len());
        let mut escaped = false;
        let mut s = 21;
        while s < units.len() {
            let mut n = units[s];
            if n == 61 && !escaped {
                escaped = true;
                s += 1;
                continue;
            }
            if n == 92 && s + 5 < units.len() && (units[s + 1] == 117 || units[s + 1] == 85) {
                let digits: String = units[s + 2..s + 6]
                    .iter()
                    .map(|&u| char::from_u32(u).unwrap())
                    .collect();
                n = u32::from_str_radix(&digits, 16).unwrap();
                s += 5;
            }
            if n > 255 {
                if let Some(index) = CP1252.iter().position(|&c| c != 0 && c == n) {
                    n = index as u32 + 127;
                }
            }
            if escaped {
                escaped = false;
                n = n.wrapping_sub(64);
            }
            let byte = if n < offset && n > 0 {
                n.wrapping_add(256 - offset)
            } else {
                n.wrapping_sub(offset)
            };
            out.push(byte as u8);
            s += 1;
        }
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut h = (i as u32) << 24;
            for _ in 0..8 {
                h = if h & 0x8000_0000 != 0 {
                    (h << 1) ^ 0x04C1_1DB7
                } else {
                    h << 1
                };
            }
            *entry = h;
        }
        let sum = out.iter().fold(u32::MAX, |n, &b| {
            (n << 8) ^ table[(((n >> 24) as u8) ^ b) as usize]
        });
        assert_eq!(sum as i32, crc, "simple-yenc crc32");
        out
    }

    #[test]
    fn yenc_decode_maps_the_last_cp1252_entry() {
        assert_eq!(yenc_decode("dynEncode0158c69fc964\u{178}"), [71]);
    }

    #[test]
    fn the_inflated_module_is_the_one_the_script_embeds() {
        let getter = "Object.defineProperty(n,\"I\",{get:()=>String.raw`";
        let start = SCRIPT.find(getter).expect("the Opus module getter") + getter.len();
        let len = SCRIPT[start..].find('`').expect("end of the module string");
        let deflated = yenc_decode(&SCRIPT[start..start + len]);
        let inflated = miniz_oxide::inflate::decompress_to_vec(&deflated).expect("raw deflate");
        assert_eq!(inflated.len(), OPUS_DECODER_WASM.len());
        assert!(inflated == OPUS_DECODER_WASM);
    }
}
