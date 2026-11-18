// Replaces WebRTC's builtin audio decoder factory for the codecs NetEq
// handles here (PARITY_PLAN.md 3.4): `L16` -> PCM16B, `opus` -> Opus (when
// `ropus` is compiled in). `CN`, `telephone-event` and `red` never reach the
// factory; `DecoderDatabase` handles them by SDP name.

//! The default [`AudioDecoderFactory`].
//!
//! SDP to config mapping follows WebRTC's `AudioDecoderL16::SdpToConfig` and
//! `AudioDecoderOpus::SdpToConfig` (api/audio_codecs/{L16,opus}) with the
//! default field trials (`WebRTC-Audio-OpusDecodeStereoByDefault` and
//! `WebRTC-Audio-OpusGeneratePlc` off).

use crate::api::audio_decoder::{AudioDecoder, AudioDecoderFactory, K_MAX_NUMBER_OF_CHANNELS};
use crate::api::audio_format::SdpAudioFormat;
use crate::codecs::pcm16b::AudioDecoderPcm16B;

/// Makes PCM16B and Opus decoders.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinAudioDecoderFactory;

impl BuiltinAudioDecoderFactory {
    /// The factory (stateless).
    pub fn new() -> Self {
        Self
    }
}

/// C++ `AudioDecoderL16::SdpToConfig` + `Config::IsOk`. Returns
/// `(sample_rate_hz, num_channels)`.
fn l16_sdp_to_config(format: &SdpAudioFormat) -> Option<(i32, usize)> {
    let sample_rate_hz = format.clockrate_hz;
    let num_channels = format.num_channels;
    let is_ok = matches!(sample_rate_hz, 8000 | 16000 | 32000 | 48000)
        && (1..=K_MAX_NUMBER_OF_CHANNELS).contains(&num_channels);
    if format.name.eq_ignore_ascii_case("L16") && is_ok {
        Some((sample_rate_hz, num_channels))
    } else {
        None
    }
}

/// C++ `AudioDecoderOpus::SdpToConfig`. Returns the channel count; the
/// decoder always runs at 48 kHz. Without a `stereo` parameter the decoder is
/// mono (`WebRTC-Audio-OpusDecodeStereoByDefault` off).
#[cfg_attr(not(feature = "ropus"), allow(dead_code))]
fn opus_sdp_to_config(format: &SdpAudioFormat) -> Option<usize> {
    if !format.name.eq_ignore_ascii_case("opus")
        || format.clockrate_hz != 48000
        || format.num_channels != 2
    {
        return None;
    }
    // Parse the "stereo" codec parameter. If set, it overrides the default
    // number of channels.
    match format.parameters.get("stereo").map(String::as_str) {
        None => Some(1),
        Some("0") => Some(1),
        Some("1") => Some(2),
        // Malformed stereo parameter.
        Some(_) => None,
    }
}

impl AudioDecoderFactory for BuiltinAudioDecoderFactory {
    fn make_decoder(&self, format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
        // C++ tries the factory template's decoders in order: Opus, then
        // L16. The SDP names are disjoint, so the order does not matter.
        #[cfg(feature = "ropus")]
        if let Some(num_channels) = opus_sdp_to_config(format) {
            return Some(Box::new(crate::codecs::opus::AudioDecoderOpus::new(
                num_channels,
                48000,
                false,
            )));
        }
        if let Some((sample_rate_hz, num_channels)) = l16_sdp_to_config(format) {
            return Some(Box::new(AudioDecoderPcm16B::new(
                sample_rate_hz,
                num_channels,
            )));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::audio_format::CodecParameterMap;

    fn make(format: SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
        BuiltinAudioDecoderFactory::new().make_decoder(&format)
    }

    #[test]
    fn l16_formats() {
        for rate in [8000, 16000, 32000, 48000] {
            for channels in [1, 2, 24] {
                let decoder = make(SdpAudioFormat::new("L16", rate, channels)).expect("L16");
                assert_eq!(rate, decoder.sample_rate_hz());
                assert_eq!(channels, decoder.channels());
            }
        }
        // Case-insensitive name.
        assert!(make(SdpAudioFormat::new("l16", 8000, 1)).is_some());
        assert!(make(SdpAudioFormat::new("L16", 44100, 1)).is_none());
        assert!(make(SdpAudioFormat::new("L16", 8000, 0)).is_none());
        assert!(make(SdpAudioFormat::new("L16", 8000, 25)).is_none());
    }

    #[test]
    fn unsupported_formats() {
        for name in ["pcmu", "pcma", "g722", "CN", "telephone-event", "red"] {
            assert!(make(SdpAudioFormat::new(name, 8000, 1)).is_none(), "{name}");
        }
    }

    #[cfg(feature = "ropus")]
    #[test]
    fn opus_formats() {
        // Mono unless "stereo=1" (OpusDecodeStereoByDefault off).
        let decoder = make(SdpAudioFormat::new("opus", 48000, 2)).expect("opus");
        assert_eq!(48000, decoder.sample_rate_hz());
        assert_eq!(1, decoder.channels());

        let with_stereo = |value: &str| {
            let mut parameters = CodecParameterMap::new();
            parameters.insert("stereo".to_string(), value.to_string());
            SdpAudioFormat::with_parameters("OPUS", 48000, 2, parameters)
        };
        assert_eq!(2, make(with_stereo("1")).expect("opus").channels());
        assert_eq!(1, make(with_stereo("0")).expect("opus").channels());
        assert!(make(with_stereo("2")).is_none());

        // RFC 7587: always opus/48000/2 in SDP.
        assert!(make(SdpAudioFormat::new("opus", 48000, 1)).is_none());
        assert!(make(SdpAudioFormat::new("opus", 16000, 2)).is_none());
    }

    #[cfg(not(feature = "ropus"))]
    #[test]
    fn opus_needs_ropus() {
        assert!(make(SdpAudioFormat::new("opus", 48000, 2)).is_none());
        let _ = CodecParameterMap::new();
    }
}
