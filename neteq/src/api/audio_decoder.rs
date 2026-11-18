// Port of WebRTC api/audio_codecs/audio_decoder.{h,cc} and the slice of
// api/audio_codecs/audio_decoder_factory.h NetEq needs, at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2014 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// Deviation (PARITY_PLAN.md 3.4): C++ `EncodedAudioFrame` keeps a raw
// `AudioDecoder*`. Here frames are decoder-free and NetEq passes the decoder
// in at decode time. `duration()` is cached at parse time.

//! The decoder interface NetEq drives.

use crate::api::audio_format::SdpAudioFormat;

/// C++ `AudioDecoder::kNotImplemented`.
pub const K_NOT_IMPLEMENTED: i32 = -2;

/// C++ `AudioDecoder::kMaxNumberOfChannels` (`kMaxNumberOfAudioChannels`).
pub const K_MAX_NUMBER_OF_CHANNELS: usize = 24;

/// What a decoder produced. Values match C++.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum SpeechType {
    #[default]
    Speech = 1,
    ComfortNoise = 2,
}

impl SpeechType {
    /// C++ `AudioDecoder::ConvertSpeechType`: 0 and 1 are speech, 2 is
    /// comfort noise.
    pub fn from_codec_type(value: i16) -> Self {
        match value {
            0 | 1 => SpeechType::Speech,
            2 => SpeechType::ComfortNoise,
            _ => {
                debug_assert!(false, "unexpected speech type {value}");
                SpeechType::Speech
            }
        }
    }
}

/// Result of [`EncodedAudioFrame::decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeResult {
    /// Samples written, summed over all channels (interleaved).
    pub num_decoded_samples: usize,
    pub speech_type: SpeechType,
}

/// One parsed, still-encoded audio frame, as stored in a NetEq `Packet`.
pub trait EncodedAudioFrame: Send {
    /// Duration in samples per channel. Cached at parse time.
    fn duration(&self) -> usize;

    /// True if this is a DTX packet (Opus: payload of at most 2 bytes).
    fn is_dtx_packet(&self) -> bool {
        false
    }

    /// Decodes into `decoded` (interleaved), using `decoder`. Returns `None`
    /// on decoder error.
    fn decode(&self, decoder: &mut dyn AudioDecoder, decoded: &mut [i16]) -> Option<DecodeResult>;
}

/// One frame found by [`AudioDecoder::parse_payload`].
pub struct ParseResult {
    pub timestamp: u32,
    /// 0 is primary; higher is lower priority (e.g. Opus in-band FEC is 1).
    pub priority: i32,
    pub frame: Box<dyn EncodedAudioFrame>,
}

impl ParseResult {
    /// C++ `ParseResult(timestamp, priority, frame)`.
    pub fn new(timestamp: u32, priority: i32, frame: Box<dyn EncodedAudioFrame>) -> Self {
        debug_assert!(priority >= 0);
        Self {
            timestamp,
            priority,
            frame,
        }
    }
}

impl std::fmt::Debug for ParseResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParseResult")
            .field("timestamp", &self.timestamp)
            .field("priority", &self.priority)
            .field("duration", &self.frame.duration())
            .finish()
    }
}

/// C++ `AudioDecoder`. Implementors provide `decode_internal`, `reset`,
/// `sample_rate_hz` and `channels`; everything else has the C++ default.
pub trait AudioDecoder: Send {
    /// Splits `payload` into encoded frames. The default returns one frame
    /// with priority 0 whose duration is `packet_duration(payload)` (0 if
    /// unknown), decoded with [`AudioDecoder::decode`].
    fn parse_payload(&mut self, payload: Vec<u8>, timestamp: u32) -> Vec<ParseResult> {
        let duration = self.packet_duration(&payload);
        let frame = LegacyEncodedFrame {
            payload,
            duration: if duration < 0 { 0 } else { duration as usize },
        };
        vec![ParseResult::new(timestamp, 0, Box::new(frame))]
    }

    /// C++ `Decode`: rejects the call (-1) if the known packet duration does
    /// not fit in `decoded`, else forwards to `decode_internal`. Returns the
    /// number of samples written (all channels), or -1 on error.
    fn decode(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        let duration = self.packet_duration(encoded);
        if duration >= 0 && duration as usize * self.channels() > decoded.len() {
            return -1;
        }
        self.decode_internal(encoded, sample_rate_hz, decoded, speech_type)
    }

    /// C++ `DecodeRedundant`: same as `decode` but for redundant (FEC) data.
    fn decode_redundant(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        let duration = self.packet_duration_redundant(encoded);
        if duration >= 0 && duration as usize * self.channels() > decoded.len() {
            return -1;
        }
        self.decode_redundant_internal(encoded, sample_rate_hz, decoded, speech_type)
    }

    /// Codec-specific decode. Returns samples written (all channels) or -1.
    fn decode_internal(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32;

    /// Codec-specific redundant decode. Defaults to `decode_internal`.
    fn decode_redundant_internal(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        self.decode_internal(encoded, sample_rate_hz, decoded, speech_type)
    }

    /// True if the decoder has its own packet loss concealment
    /// ([`AudioDecoder::decode_plc`]).
    fn has_decode_plc(&self) -> bool {
        false
    }

    /// Generates `num_frames` frames of codec PLC into `decoded`. Returns the
    /// number of samples written.
    fn decode_plc(&mut self, _num_frames: usize, _decoded: &mut [i16]) -> usize {
        0
    }

    /// Appends at least `requested_samples_per_channel` samples of
    /// concealment audio to `concealment_audio`. Appending nothing means
    /// NetEq falls back to its own Expand.
    fn generate_plc(
        &mut self,
        _requested_samples_per_channel: usize,
        _concealment_audio: &mut Vec<i16>,
    ) {
    }

    /// Resets the decoder state.
    fn reset(&mut self);

    /// Last error code from the codec, 0 if none.
    fn error_code(&mut self) -> i32 {
        0
    }

    /// Duration of `encoded` in samples per channel, or `K_NOT_IMPLEMENTED`.
    fn packet_duration(&self, _encoded: &[u8]) -> i32 {
        K_NOT_IMPLEMENTED
    }

    /// Duration of the redundant data in `encoded`, or `K_NOT_IMPLEMENTED`.
    fn packet_duration_redundant(&self, _encoded: &[u8]) -> i32 {
        K_NOT_IMPLEMENTED
    }

    /// True if `encoded` carries in-band FEC.
    fn packet_has_fec(&self, _encoded: &[u8]) -> bool {
        false
    }

    /// Output sample rate in Hz.
    fn sample_rate_hz(&self) -> i32;

    /// Number of output channels.
    fn channels(&self) -> usize;
}

/// C++ `OldStyleEncodedFrame`: the frame produced by the default
/// `parse_payload`. Decodes with `AudioDecoder::decode` at the decoder's own
/// sample rate.
struct LegacyEncodedFrame {
    payload: Vec<u8>,
    duration: usize,
}

impl EncodedAudioFrame for LegacyEncodedFrame {
    fn duration(&self) -> usize {
        self.duration
    }

    fn decode(&self, decoder: &mut dyn AudioDecoder, decoded: &mut [i16]) -> Option<DecodeResult> {
        let mut speech_type = SpeechType::Speech;
        let sample_rate_hz = decoder.sample_rate_hz();
        let ret = decoder.decode(&self.payload, sample_rate_hz, decoded, &mut speech_type);
        if ret < 0 {
            None
        } else {
            Some(DecodeResult {
                num_decoded_samples: ret as usize,
                speech_type,
            })
        }
    }
}

/// Creates decoders for SDP formats (the slice of C++
/// `AudioDecoderFactory` that `DecoderDatabase` uses).
pub trait AudioDecoderFactory: Send + Sync {
    /// Returns a new decoder for `format`, or `None` if unsupported.
    fn make_decoder(&self, format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono 8-bit "codec": one sample per byte, duration = payload length.
    struct ByteDecoder;

    impl AudioDecoder for ByteDecoder {
        fn decode_internal(
            &mut self,
            encoded: &[u8],
            _sample_rate_hz: i32,
            decoded: &mut [i16],
            speech_type: &mut SpeechType,
        ) -> i32 {
            for (out, &byte) in decoded.iter_mut().zip(encoded) {
                *out = byte as i16;
            }
            *speech_type = SpeechType::Speech;
            encoded.len() as i32
        }
        fn reset(&mut self) {}
        fn packet_duration(&self, encoded: &[u8]) -> i32 {
            encoded.len() as i32
        }
        fn sample_rate_hz(&self) -> i32 {
            8000
        }
        fn channels(&self) -> usize {
            1
        }
    }

    #[test]
    fn default_parse_payload_yields_one_primary_frame() {
        let mut decoder = ByteDecoder;
        let results = decoder.parse_payload(vec![1, 2, 3], 4711);
        assert_eq!(1, results.len());
        assert_eq!(4711, results[0].timestamp);
        assert_eq!(0, results[0].priority);
        assert_eq!(3, results[0].frame.duration());
        assert!(!results[0].frame.is_dtx_packet());

        let mut out = [0i16; 3];
        let result = results[0].frame.decode(&mut decoder, &mut out).unwrap();
        assert_eq!(3, result.num_decoded_samples);
        assert_eq!([1, 2, 3], out);
    }

    #[test]
    fn decode_rejects_too_small_output() {
        let mut decoder = ByteDecoder;
        let mut speech_type = SpeechType::Speech;
        let mut out = [0i16; 2];
        assert_eq!(
            -1,
            decoder.decode(&[1, 2, 3], 8000, &mut out, &mut speech_type)
        );
        let frames = decoder.parse_payload(vec![1, 2, 3], 0);
        assert!(frames[0].frame.decode(&mut decoder, &mut out).is_none());
    }

    #[test]
    fn convert_speech_type() {
        assert_eq!(SpeechType::Speech, SpeechType::from_codec_type(0));
        assert_eq!(SpeechType::Speech, SpeechType::from_codec_type(1));
        assert_eq!(SpeechType::ComfortNoise, SpeechType::from_codec_type(2));
    }
}
