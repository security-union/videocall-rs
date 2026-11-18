// Port target: WebRTC modules/audio_coding/codecs/pcm16b/
// {audio_decoder_pcm16b,pcm16b}.{h,cc} and
// codecs/legacy_encoded_audio_frame.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: the PCM16B cases of
// neteq/audio_decoder_unittest.cc, legacy_encoded_audio_frame_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! PCM16B (`L16`): big-endian 16-bit linear PCM. The parity gate codec.

use crate::api::audio_decoder::{
    AudioDecoder, DecodeResult, EncodedAudioFrame, ParseResult, SpeechType,
    K_MAX_NUMBER_OF_CHANNELS,
};
use crate::rtc_base::checked_div_exact;

/// C `WebRtcPcm16b_Encode`: writes `speech` as big-endian bytes into
/// `encoded` (`2 * speech.len()` bytes). Returns the number of bytes.
pub fn pcm16b_encode(speech: &[i16], encoded: &mut [u8]) -> usize {
    for (i, &sample) in speech.iter().enumerate() {
        let s = sample as u16;
        encoded[2 * i] = (s >> 8) as u8;
        encoded[2 * i + 1] = s as u8;
    }
    2 * speech.len()
}

/// C `WebRtcPcm16b_Decode`: reads big-endian samples from `encoded` into
/// `speech`. Returns the number of samples.
pub fn pcm16b_decode(encoded: &[u8], speech: &mut [i16]) -> usize {
    let len = encoded.len() / 2;
    for i in 0..len {
        speech[i] = ((encoded[2 * i] as u16) << 8 | encoded[2 * i + 1] as u16) as i16;
    }
    len
}

/// C++ `LegacyEncodedAudioFrame`: a frame that decodes through
/// `AudioDecoder::decode`.
#[derive(Debug, Clone)]
pub struct LegacyEncodedAudioFrame {
    payload: Vec<u8>,
    duration: usize,
}

impl LegacyEncodedAudioFrame {
    /// C++ constructor plus `Duration()`, evaluated once: the duration is a
    /// pure function of the payload for every legacy codec.
    fn new(decoder: &dyn AudioDecoder, payload: Vec<u8>) -> Self {
        let ret = decoder.packet_duration(&payload);
        let duration = if ret < 0 { 0 } else { ret as usize };
        Self { payload, duration }
    }

    /// C++ `SplitBySamples`: splits `payload` into frames of 10 to 20 ms
    /// (as C++ chooses), with timestamps advancing by `timestamps_per_ms`.
    /// `decoder` is used to compute each frame's duration.
    pub fn split_by_samples(
        decoder: &dyn AudioDecoder,
        payload: Vec<u8>,
        timestamp: u32,
        bytes_per_ms: usize,
        timestamps_per_ms: u32,
    ) -> Vec<ParseResult> {
        let mut results = Vec::new();
        let mut split_size_bytes = payload.len();

        // Find a "chunk size" >= 20 ms and < 40 ms.
        let min_chunk_size = bytes_per_ms * 20;
        if min_chunk_size >= payload.len() {
            let frame = LegacyEncodedAudioFrame::new(decoder, payload);
            results.push(ParseResult::new(timestamp, 0, Box::new(frame)));
        } else {
            // Reduce the split size by half as long as `split_size_bytes` is
            // at least twice the minimum chunk size (so that the resulting
            // size is at least as large as the minimum chunk size).
            while split_size_bytes >= 2 * min_chunk_size {
                split_size_bytes /= 2;
            }

            let timestamps_per_chunk =
                (split_size_bytes * timestamps_per_ms as usize / bytes_per_ms) as u32;
            let mut byte_offset = 0usize;
            let mut timestamp_offset = 0u32;
            while byte_offset < payload.len() {
                split_size_bytes = split_size_bytes.min(payload.len() - byte_offset);
                let new_payload = payload[byte_offset..byte_offset + split_size_bytes].to_vec();
                let frame = LegacyEncodedAudioFrame::new(decoder, new_payload);
                results.push(ParseResult::new(
                    timestamp.wrapping_add(timestamp_offset),
                    0,
                    Box::new(frame),
                ));
                byte_offset += split_size_bytes;
                timestamp_offset = timestamp_offset.wrapping_add(timestamps_per_chunk);
            }
        }

        results
    }

    /// The encoded payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl EncodedAudioFrame for LegacyEncodedAudioFrame {
    fn duration(&self) -> usize {
        self.duration
    }

    fn decode(&self, decoder: &mut dyn AudioDecoder, decoded: &mut [i16]) -> Option<DecodeResult> {
        let mut speech_type = SpeechType::Speech;
        let sample_rate_hz = decoder.sample_rate_hz();
        let ret = decoder.decode(&self.payload, sample_rate_hz, decoded, &mut speech_type);
        if ret < 0 {
            return None;
        }
        Some(DecodeResult {
            num_decoded_samples: ret as usize,
            speech_type,
        })
    }
}

/// C++ `AudioDecoderPcm16B`.
#[derive(Debug, Clone)]
pub struct AudioDecoderPcm16B {
    sample_rate_hz: i32,
    num_channels: usize,
}

impl AudioDecoderPcm16B {
    /// Decoder at 8, 16, 32 or 48 kHz with `num_channels >= 1`.
    pub fn new(sample_rate_hz: i32, num_channels: usize) -> Self {
        debug_assert!(
            matches!(sample_rate_hz, 8000 | 16000 | 32000 | 48000),
            "Unsupported sample rate {sample_rate_hz}"
        );
        debug_assert!(num_channels >= 1);
        debug_assert!(num_channels <= K_MAX_NUMBER_OF_CHANNELS);
        Self {
            sample_rate_hz,
            num_channels,
        }
    }
}

impl AudioDecoder for AudioDecoderPcm16B {
    fn parse_payload(&mut self, payload: Vec<u8>, timestamp: u32) -> Vec<ParseResult> {
        let samples_per_ms = checked_div_exact(self.sample_rate_hz, 1000) as usize;
        LegacyEncodedAudioFrame::split_by_samples(
            self,
            payload,
            timestamp,
            samples_per_ms * 2 * self.num_channels,
            samples_per_ms as u32,
        )
    }

    fn decode_internal(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        debug_assert_eq!(self.sample_rate_hz, sample_rate_hz);
        // Adjust the encoded length down to ensure the same number of samples
        // in each channel.
        let encoded_len_adjusted = self.packet_duration(encoded) as usize * 2 * self.channels(); // 2 bytes per sample per channel
        let ret = pcm16b_decode(&encoded[..encoded_len_adjusted], decoded);
        *speech_type = SpeechType::from_codec_type(1);
        ret as i32
    }

    fn reset(&mut self) {}

    fn packet_duration(&self, encoded: &[u8]) -> i32 {
        // Two encoded byte per sample per channel.
        (encoded.len() / (2 * self.channels())) as i32
    }

    fn packet_duration_redundant(&self, encoded: &[u8]) -> i32 {
        self.packet_duration(encoded)
    }

    fn sample_rate_hz(&self) -> i32 {
        self.sample_rate_hz
    }

    fn channels(&self) -> usize {
        self.num_channels
    }
}

#[cfg(test)]
mod tests {
    //! Ports of the PCM16B cases of neteq/audio_decoder_unittest.cc and of
    //! codecs/legacy_encoded_audio_frame_unittest.cc. WebRTC's
    //! `testfile32kHz.pcm` is not in the sparse clone, so the encode/decode
    //! tests use a fixed synthetic signal; PCM16B is lossless either way.

    use super::*;

    /// Deterministic full-range test signal (LCG).
    fn test_signal(len: usize) -> Vec<i16> {
        let mut state: u32 = 0x1234_5678;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as i16
            })
            .collect()
    }

    fn encode(speech: &[i16]) -> Vec<u8> {
        let mut encoded = vec![0u8; 2 * speech.len()];
        assert_eq!(encoded.len(), pcm16b_encode(speech, &mut encoded));
        encoded
    }

    #[test]
    fn encode_is_big_endian() {
        let mut encoded = [0u8; 6];
        assert_eq!(6, pcm16b_encode(&[0x0102, -1, i16::MIN], &mut encoded));
        assert_eq!([0x01, 0x02, 0xFF, 0xFF, 0x80, 0x00], encoded);
        let mut decoded = [0i16; 3];
        assert_eq!(3, pcm16b_decode(&encoded, &mut decoded));
        assert_eq!([0x0102, -1, i16::MIN], decoded);
        // An odd trailing byte is ignored.
        assert_eq!(1, pcm16b_decode(&encoded[..3], &mut decoded));
    }

    /// C++ `AudioDecoderPcm16BTest.EncodeDecode`: 16 kHz mono, 20 ms
    /// frames, 10 frames; lossless, 2 bytes per sample.
    #[test]
    fn audio_decoder_pcm16b_encode_decode() {
        const CODEC_INPUT_RATE_HZ: i32 = 16000;
        let frame_size = 20 * CODEC_INPUT_RATE_HZ as usize / 1000;
        let data_length = 10 * frame_size;
        let mut decoder = AudioDecoderPcm16B::new(CODEC_INPUT_RATE_HZ, 1);
        let input = test_signal(data_length);
        let mut decoded = vec![0i16; data_length];
        let mut encoded_bytes = 0;
        for (input_frame, output_frame) in
            input.chunks(frame_size).zip(decoded.chunks_mut(frame_size))
        {
            let encoded = encode(input_frame);
            encoded_bytes += encoded.len();
            let parse_result = decoder.parse_payload(encoded, 0);
            assert_eq!(1, parse_result.len());
            assert_eq!(frame_size, parse_result[0].frame.duration());
            let decode_result = parse_result[0]
                .frame
                .decode(&mut decoder, output_frame)
                .expect("decode");
            assert_eq!(frame_size, decode_result.num_decoded_samples);
            assert_eq!(SpeechType::Speech, decode_result.speech_type);
        }
        assert_eq!(2 * data_length, encoded_bytes);
        assert_eq!(input, decoded);
        assert!(!decoder.has_decode_plc());
    }

    /// C++ `ReInitTest`: decoding the same payload after `reset` gives the
    /// same output.
    #[test]
    fn audio_decoder_pcm16b_reinit() {
        let mut decoder = AudioDecoderPcm16B::new(16000, 1);
        let encoded = encode(&test_signal(320));
        let mut outputs = [vec![0i16; 320], vec![0i16; 320]];
        for output in outputs.iter_mut() {
            decoder.reset();
            let parse_result = decoder.parse_payload(encoded.clone(), 0);
            assert_eq!(1, parse_result.len());
            let result = parse_result[0].frame.decode(&mut decoder, output).unwrap();
            assert_eq!(320, result.num_decoded_samples);
        }
        assert_eq!(outputs[0], outputs[1]);
    }

    /// Stereo: an odd sample count is rounded down to whole sample pairs,
    /// and the output check uses all channels.
    #[test]
    fn audio_decoder_pcm16b_stereo_rounds_to_whole_frames() {
        let mut decoder = AudioDecoderPcm16B::new(8000, 2);
        let encoded = encode(&test_signal(161)); // 80.5 stereo frames.
        assert_eq!(80, decoder.packet_duration(&encoded));
        assert_eq!(80, decoder.packet_duration_redundant(&encoded));
        let mut speech_type = SpeechType::ComfortNoise;
        let mut out = vec![0i16; 160];
        assert_eq!(
            160,
            decoder.decode(&encoded, 8000, &mut out, &mut speech_type)
        );
        assert_eq!(SpeechType::Speech, speech_type);
        // Too small an output buffer is rejected by `AudioDecoder::decode`.
        let mut small = vec![0i16; 159];
        assert_eq!(
            -1,
            decoder.decode(&encoded, 8000, &mut small, &mut speech_type)
        );
    }

    /// Mono 8-bit stand-in so `split_by_samples` results can be checked
    /// byte by byte: decodes each payload byte to one sample.
    struct ByteDecoder {
        bytes_per_sample: usize,
    }

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
            (encoded.len() / self.bytes_per_sample) as i32
        }
        fn sample_rate_hz(&self) -> i32 {
            8000
        }
        fn channels(&self) -> usize {
            1
        }
    }

    /// C++ `SplitBySamplesTest.PayloadSizes`, for every parameter of the
    /// C++ suite: (bytes_per_ms, samples_per_ms).
    #[test]
    fn split_by_samples_payload_sizes() {
        const PARAMS: [(usize, u32); 14] = [
            (8, 8),       // PCMu
            (8, 8),       // PCMa
            (2 * 8, 8),   // PCMu_2ch
            (2 * 8, 8),   // PCMa_2ch
            (8, 16),      // G722
            (16, 8),      // PCM16B
            (32, 16),     // PCM16Bwb
            (64, 32),     // PCM16Bswb32kHz
            (96, 48),     // PCM16Bswb48kHz
            (2 * 16, 8),  // PCM16B_2ch
            (2 * 32, 16), // PCM16Bwb_2ch
            (2 * 64, 32), // PCM16Bswb32kHz_2ch
            (2 * 96, 48), // PCM16Bswb48kHz_2ch
            (5 * 16, 8),  // PCM16B_5ch
        ];
        const K_BASE_TIMESTAMP: u32 = 0x12345678;
        // payload_size_ms -> frame sizes in ms:
        // 10 -> 10, 20 -> 20, 30 -> 30, 40 -> 20 + 20, 50 -> 25 + 25,
        // 60 -> 30 + 30.
        let expected_splits: [(usize, &[usize]); 6] = [
            (10, &[10]),
            (20, &[20]),
            (30, &[30]),
            (40, &[20, 20]),
            (50, &[25, 25]),
            (60, &[30, 30]),
        ];

        for (bytes_per_ms, samples_per_ms) in PARAMS {
            let mut decoder = ByteDecoder {
                bytes_per_sample: 1,
            };
            for &(payload_size_ms, frame_sizes) in &expected_splits {
                // The payload values steadily increase (modulo 256), so no
                // byte can be missed or repeated unnoticed.
                let num_bytes = payload_size_ms * bytes_per_ms;
                let payload: Vec<u8> = (0..num_bytes).map(|i| i as u8).collect();

                let results = LegacyEncodedAudioFrame::split_by_samples(
                    &decoder,
                    payload,
                    K_BASE_TIMESTAMP,
                    bytes_per_ms,
                    samples_per_ms,
                );

                assert_eq!(frame_sizes.len(), results.len());
                let mut expected_timestamp = K_BASE_TIMESTAMP;
                let mut value: u8 = 0;
                for (result, &frame_size_ms) in results.iter().zip(frame_sizes) {
                    let length_bytes = frame_size_ms * bytes_per_ms;
                    assert_eq!(length_bytes, result.frame.duration());
                    assert_eq!(expected_timestamp, result.timestamp);
                    assert_eq!(0, result.priority);
                    let mut out = vec![0i16; length_bytes];
                    let decoded = result.frame.decode(&mut decoder, &mut out).unwrap();
                    assert_eq!(length_bytes, decoded.num_decoded_samples);
                    for sample in out {
                        assert_eq!(value as i16, sample);
                        value = value.wrapping_add(1);
                    }
                    expected_timestamp += frame_size_ms as u32 * samples_per_ms;
                }
            }
        }
    }

    /// `parse_payload` splits a 60 ms PCM16B packet into two 30 ms frames.
    #[test]
    fn parse_payload_splits_long_packets() {
        let mut decoder = AudioDecoderPcm16B::new(48000, 1);
        let results = decoder.parse_payload(encode(&test_signal(48 * 60)), 1000);
        assert_eq!(2, results.len());
        assert_eq!(1000, results[0].timestamp);
        assert_eq!(1000 + 48 * 30, results[1].timestamp);
        assert_eq!(48 * 30, results[0].frame.duration());
        assert_eq!(48 * 30, results[1].frame.duration());
    }
}
