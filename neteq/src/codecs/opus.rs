// Compiled when the `ropus` dependency is enabled (features `native` and
// `web`, or `--features ropus`; PARITY_PLAN.md 8.2).
//
// Port target: WebRTC modules/audio_coding/codecs/opus/
// {audio_decoder_opus.cc, audio_coder_opus_common.h} and the helpers
// `WebRtcOpus_PacketHasFec`, `DurationEst`, `FecDurationEst`, `PlcDuration`
// from opus_interface.cc, at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: the Opus cases of neteq/audio_decoder_unittest.cc).
// Copyright (c) 2015 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! Opus decoder on `ropus` (pure Rust, bit-exact libopus port).
//!
//! The `WebRtcOpus_*` helpers from `opus_interface.cc` that the decoder uses
//! are ported here as private functions over [`OpusDecInst`], the Rust twin
//! of C `WebRtcOpusDecInst` (single-stream only).

use ropus::opus::decoder::{
    opus_packet_get_nb_channels, opus_packet_get_nb_frames, opus_packet_get_samples_per_frame,
    opus_packet_parse_impl, OpusDecoder, MAX_FRAMES,
};

use crate::api::audio_decoder::{
    AudioDecoder, DecodeResult, EncodedAudioFrame, ParseResult, SpeechType,
};

/// C `kWebRtcOpusMaxDecodeFrameSizeMs`: the format allows up to 120 ms frames.
const K_WEBRTC_OPUS_MAX_DECODE_FRAME_SIZE_MS: i32 = 120;

/// C `kWebRtcOpusPlcFrameSizeMs`: audio covered by each PLC call.
const K_WEBRTC_OPUS_PLC_FRAME_SIZE_MS: i32 = 10;

/// C `FrameSizePerChannel`.
fn frame_size_per_channel(frame_size_ms: i32, sample_rate_hz: i32) -> i32 {
    debug_assert!(frame_size_ms > 0);
    debug_assert_eq!(frame_size_ms % 10, 0);
    debug_assert!(sample_rate_hz > 0);
    debug_assert_eq!(sample_rate_hz % 1000, 0);
    frame_size_ms * (sample_rate_hz / 1000)
}

/// C `MaxFrameSizePerChannel`: maximum sample count per channel.
fn max_frame_size_per_channel(sample_rate_hz: i32) -> i32 {
    frame_size_per_channel(K_WEBRTC_OPUS_MAX_DECODE_FRAME_SIZE_MS, sample_rate_hz)
}

/// C `WebRtcOpus_NumSilkFrames`.
fn opus_num_silk_frames(payload: &[u8]) -> i32 {
    // For computing the payload length in ms, the sample rate is not important
    // since it cancels out. We use 48 kHz, but any valid sample rate would
    // work.
    let mut payload_length_ms = opus_packet_get_samples_per_frame(payload, 48000) / 48;
    if payload_length_ms < 10 {
        payload_length_ms = 10;
    }

    match payload_length_ms {
        10 | 20 => 1,
        40 => 2,
        60 => 3,
        _ => 0, // It is actually even an invalid packet.
    }
}

/// C `WebRtcOpus_PacketHasFec`.
///
/// Based on RFC 6716: parses the LP layer of the first frame for the LBRR
/// flag.
pub fn opus_packet_has_fec(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return false;
    }

    // In CELT_ONLY mode, packets should not have FEC.
    if payload[0] & 0x80 != 0 {
        return false;
    }

    let silk_frames = opus_num_silk_frames(payload);
    if silk_frames == 0 {
        return false; // Not valid.
    }

    let channels = opus_packet_get_nb_channels(payload);
    debug_assert!(channels == 1 || channels == 2);

    // Max number of frames in an Opus packet is 48.
    let mut frame_sizes = [0i16; MAX_FRAMES];
    let mut toc = 0u8;
    let mut payload_offset = 0i32;

    // Parse packet to get the frames. But we only care about the first frame,
    // since we can only decode the FEC from the first one.
    let Ok(len) = i32::try_from(payload.len()) else {
        return false;
    };
    if opus_packet_parse_impl(
        payload,
        len,
        false,
        &mut toc,
        &mut frame_sizes,
        &mut payload_offset,
        None,
    ) < 0
    {
        return false;
    }

    if frame_sizes[0] < 1 {
        return false;
    }
    let first_byte = payload[payload_offset as usize];

    // A frame starts with the LP layer. The LP layer begins with two to eight
    // header bits. These consist of one VAD bit per SILK frame (up to 3),
    // followed by a single flag indicating the presence of LBRR frames. For a
    // stereo packet, these first flags correspond to the mid channel, and a
    // second set of flags is included for the side channel. Because these are
    // the first symbols decoded by the range coder and because they are coded
    // as binary values with uniform probability, they can be extracted
    // directly from the most significant bits of the first byte of compressed
    // data.
    for n in 0..channels {
        // The LBRR bit for channel 1 is on the (`silk_frames` + 1)-th bit, and
        // that of channel 2 is on the |(`silk_frames` + 1) * 2 + 1|-th bit.
        if first_byte as i32 & (0x80 >> ((n + 1) * (silk_frames + 1) - 1)) != 0 {
            return true;
        }
    }

    false
}

/// C `WebRtcOpus_FecDurationEst`: duration of the FEC data in samples at
/// `sample_rate_hz`, or 0.
pub fn opus_fec_duration_est(payload: &[u8], sample_rate_hz: i32) -> i32 {
    if !opus_packet_has_fec(payload) {
        return 0;
    }
    let samples = opus_packet_get_samples_per_frame(payload, sample_rate_hz);
    let samples_per_ms = sample_rate_hz / 1000;
    if samples < 10 * samples_per_ms || samples > 120 * samples_per_ms {
        // Invalid payload duration.
        return 0;
    }
    samples
}

/// C `WebRtcOpusDecInst` (single-stream decoder only).
struct OpusDecInst {
    decoder: OpusDecoder,
    channels: usize,
    in_dtx_mode: bool,
    sample_rate_hz: i32,
    /// C `last_packet_num_channels` (webrtc:376493209 workaround).
    last_packet_num_channels: i32,
}

impl OpusDecInst {
    /// C `WebRtcOpus_DecoderCreate`. `None` if libopus rejects the
    /// parameters.
    fn create(channels: usize, sample_rate_hz: i32) -> Option<Self> {
        let decoder = OpusDecoder::new(sample_rate_hz, channels as i32).ok()?;
        Some(Self {
            decoder,
            channels,
            in_dtx_mode: false,
            sample_rate_hz,
            last_packet_num_channels: channels as i32,
        })
    }

    /// C `WebRtcOpus_DecoderInit`.
    fn init(&mut self) {
        self.decoder.reset();
        self.in_dtx_mode = false;
    }

    /// C `DetermineAudioType`: 2 (comfort noise) or 0 (speech).
    fn determine_audio_type(&mut self, encoded_bytes: usize) -> i16 {
        // Audio type becomes comfort noise if `encoded_byte` is 1 and keeps
        // to be so if the following `encoded_byte` are 0 or 1.
        if encoded_bytes == 0 && self.in_dtx_mode {
            2 // Comfort noise.
        } else if encoded_bytes == 1 || encoded_bytes == 2 {
            // TODO(henrik.lundin): There is a slight risk that a 2-byte
            // payload is in fact a 1-byte TOC with a 1-byte payload. That will
            // be erroneously interpreted as comfort noise output, but such a
            // payload is probably faulty anyway.
            self.in_dtx_mode = true;
            2 // Comfort noise.
        } else {
            self.in_dtx_mode = false;
            0 // Speech.
        }
    }

    /// C `DecodeNative`. `frame_size` is the maximum Opus frame size in the
    /// normal case and the PLC size on losses. Returns samples per channel
    /// or -1.
    ///
    /// C trusts the caller's buffer to hold `frame_size` samples per channel;
    /// here `frame_size` is clamped to what `decoded` holds, which changes
    /// nothing when the buffer is large enough for the packet (libopus
    /// returns `OPUS_BUFFER_TOO_SMALL` otherwise, which also maps to -1).
    fn decode_native(
        &mut self,
        encoded: &[u8],
        frame_size: i32,
        decoded: &mut [i16],
        audio_type: &mut i16,
        decode_fec: bool,
    ) -> i32 {
        if !encoded.is_empty() {
            // TODO: https://issues.webrtc.org/376493209 - When fixed, remove
            // block below.
            self.last_packet_num_channels = opus_packet_get_nb_channels(encoded);
            debug_assert!(self.last_packet_num_channels == 1 || self.last_packet_num_channels == 2);
        }
        let available = (decoded.len() / self.channels).min(i32::MAX as usize) as i32;
        let frame_size = frame_size.min(available);
        let data = if encoded.is_empty() {
            None
        } else {
            Some(encoded)
        };
        let decoded_samples_per_channel = if frame_size <= 0 {
            -1
        } else {
            self.decoder
                .decode(data, decoded, frame_size, decode_fec)
                .unwrap_or(-1)
        };

        if decoded_samples_per_channel <= 0 {
            return -1;
        }

        *audio_type = self.determine_audio_type(encoded.len());

        // TODO: https://issues.webrtc.org/376493209 - When fixed, remove block
        // below. When stereo decoding is enabled and the last observed
        // non-empty packet encoded mono audio, the Opus decoder may generate
        // non-trivial stereo audio. As that is undesired, in that case make
        // sure that `decoded` contains trivial stereo audio by copying the
        // left channel into the right one.
        if self.channels == 2 && self.last_packet_num_channels == 1 {
            let num_channels = self.channels;
            let total = decoded_samples_per_channel as usize * num_channels;
            for i in (0..total).step_by(num_channels) {
                decoded[i + 1] = decoded[i];
            }
        }

        decoded_samples_per_channel
    }

    /// C `DecodePlc`.
    fn decode_plc(&mut self, decoded: &mut [i16]) -> i32 {
        let mut audio_type = 0i16;
        let plc_samples =
            frame_size_per_channel(K_WEBRTC_OPUS_PLC_FRAME_SIZE_MS, self.sample_rate_hz);

        let decoded_samples = self.decode_native(&[], plc_samples, decoded, &mut audio_type, false);
        if decoded_samples < 0 {
            return -1;
        }
        decoded_samples
    }

    /// C `WebRtcOpus_Decode`. Returns samples per channel or -1.
    fn decode(&mut self, encoded: &[u8], decoded: &mut [i16], audio_type: &mut i16) -> i32 {
        let decoded_samples_per_channel = if encoded.is_empty() {
            *audio_type = self.determine_audio_type(0);
            self.decode_plc(decoded)
        } else {
            let max_frame_size = max_frame_size_per_channel(self.sample_rate_hz);
            self.decode_native(encoded, max_frame_size, decoded, audio_type, false)
        };
        if decoded_samples_per_channel < 0 {
            return -1;
        }
        decoded_samples_per_channel
    }

    /// C `WebRtcOpus_DecodeFec`. Returns samples per channel, 0 if there is
    /// no FEC, or -1.
    fn decode_fec(&mut self, encoded: &[u8], decoded: &mut [i16], audio_type: &mut i16) -> i32 {
        if !opus_packet_has_fec(encoded) {
            return 0;
        }

        let fec_samples = opus_packet_get_samples_per_frame(encoded, self.sample_rate_hz);

        let decoded_samples = self.decode_native(encoded, fec_samples, decoded, audio_type, true);
        if decoded_samples < 0 {
            return -1;
        }
        decoded_samples
    }

    /// C `WebRtcOpus_DurationEst`.
    fn duration_est(&self, payload: &[u8]) -> i32 {
        if payload.is_empty() {
            // WebRtcOpus_Decode calls PLC when payload length is zero. So we
            // return PLC duration correspondingly.
            return self.plc_duration();
        }

        let Ok(frames) = opus_packet_get_nb_frames(payload) else {
            // Invalid payload data.
            return 0;
        };
        let samples = frames * opus_packet_get_samples_per_frame(payload, self.sample_rate_hz);
        if samples > 120 * self.sample_rate_hz / 1000 {
            // More than 120 ms' worth of samples.
            return 0;
        }
        samples
    }

    /// C `WebRtcOpus_PlcDuration`.
    fn plc_duration(&self) -> i32 {
        frame_size_per_channel(K_WEBRTC_OPUS_PLC_FRAME_SIZE_MS, self.sample_rate_hz)
    }
}

/// C++ `OpusFrame` (audio_coder_opus_common.h). The duration is cached at
/// parse time; for a non-empty payload it does not depend on decoder state.
struct OpusFrame {
    payload: Vec<u8>,
    is_primary_payload: bool,
    duration: usize,
}

impl OpusFrame {
    fn new(decoder: &AudioDecoderOpus, payload: Vec<u8>, is_primary_payload: bool) -> Self {
        let ret = if is_primary_payload {
            decoder.packet_duration(&payload)
        } else {
            decoder.packet_duration_redundant(&payload)
        };
        let duration = if ret < 0 { 0 } else { ret as usize };
        Self {
            payload,
            is_primary_payload,
            duration,
        }
    }
}

impl EncodedAudioFrame for OpusFrame {
    fn duration(&self) -> usize {
        self.duration
    }

    fn is_dtx_packet(&self) -> bool {
        self.payload.len() <= 2
    }

    fn decode(&self, decoder: &mut dyn AudioDecoder, decoded: &mut [i16]) -> Option<DecodeResult> {
        let mut speech_type = SpeechType::Speech;
        let sample_rate_hz = decoder.sample_rate_hz();
        let ret = if self.is_primary_payload {
            decoder.decode(&self.payload, sample_rate_hz, decoded, &mut speech_type)
        } else {
            decoder.decode_redundant(&self.payload, sample_rate_hz, decoded, &mut speech_type)
        };

        if ret < 0 {
            return None;
        }
        Some(DecodeResult {
            num_decoded_samples: ret as usize,
            speech_type,
        })
    }
}

/// C++ `AudioDecoderOpusImpl`.
pub struct AudioDecoderOpus {
    dec_state: OpusDecInst,
    channels: usize,
    sample_rate_hz: i32,
    generate_plc: bool,
}

impl std::fmt::Debug for AudioDecoderOpus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioDecoderOpus")
            .field("channels", &self.channels)
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("generate_plc", &self.generate_plc)
            .field("in_dtx_mode", &self.dec_state.in_dtx_mode)
            .finish_non_exhaustive()
    }
}

impl AudioDecoderOpus {
    /// Decoder for `num_channels` (1 or 2) at `sample_rate_hz` (8, 16, 24 or
    /// 48 kHz). `generate_plc` is `WebRTC-Audio-OpusGeneratePlc` (default
    /// false: `generate_plc` is a no-op).
    ///
    /// Panics if libopus rejects the parameters (C++ `RTC_CHECK_EQ`).
    pub fn new(num_channels: usize, sample_rate_hz: i32, generate_plc: bool) -> Self {
        debug_assert!(num_channels == 1 || num_channels == 2);
        let mut dec_state =
            OpusDecInst::create(num_channels, sample_rate_hz).unwrap_or_else(|| {
                panic!("WebRtcOpus_DecoderCreate failed ({num_channels} ch, {sample_rate_hz} Hz)")
            });
        dec_state.init();
        Self {
            dec_state,
            channels: num_channels,
            sample_rate_hz,
            generate_plc,
        }
    }
}

impl AudioDecoder for AudioDecoderOpus {
    fn parse_payload(&mut self, payload: Vec<u8>, timestamp: u32) -> Vec<ParseResult> {
        let mut results = Vec::with_capacity(2);

        if self.packet_has_fec(&payload) {
            let duration = self.packet_duration_redundant(&payload);
            debug_assert!(duration >= 0);
            let fec_frame = OpusFrame::new(self, payload.clone(), false);
            results.push(ParseResult::new(
                timestamp.wrapping_sub(duration as u32),
                1,
                Box::new(fec_frame),
            ));
        }
        let frame = OpusFrame::new(self, payload, true);
        results.push(ParseResult::new(timestamp, 0, Box::new(frame)));
        results
    }

    fn decode_internal(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        debug_assert_eq!(sample_rate_hz, self.sample_rate_hz);
        let mut temp_type = 1i16; // Default is speech.
        let mut ret = self.dec_state.decode(encoded, decoded, &mut temp_type);
        if ret > 0 {
            ret *= self.channels as i32; // Return total number of samples.
        }
        *speech_type = SpeechType::from_codec_type(temp_type);
        ret
    }

    fn decode_redundant_internal(
        &mut self,
        encoded: &[u8],
        sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        if !self.packet_has_fec(encoded) {
            // This packet is a RED packet.
            return self.decode_internal(encoded, sample_rate_hz, decoded, speech_type);
        }

        debug_assert_eq!(sample_rate_hz, self.sample_rate_hz);
        let mut temp_type = 1i16; // Default is speech.
        let mut ret = self.dec_state.decode_fec(encoded, decoded, &mut temp_type);
        if ret > 0 {
            ret *= self.channels as i32; // Return total number of samples.
        }
        *speech_type = SpeechType::from_codec_type(temp_type);
        ret
    }

    fn generate_plc(
        &mut self,
        _requested_samples_per_channel: usize,
        concealment_audio: &mut Vec<i16>,
    ) {
        if !self.generate_plc {
            return;
        }
        // C++ `AppendData(plc_size, setter)`: grow by `plc_size`, then keep
        // only what the setter reports as written.
        let plc_size = self.dec_state.plc_duration() as usize * self.channels;
        let old_len = concealment_audio.len();
        concealment_audio.resize(old_len + plc_size, 0);
        let mut temp_type = 1i16;
        let ret = self
            .dec_state
            .decode(&[], &mut concealment_audio[old_len..], &mut temp_type);
        let written = if ret < 0 {
            0
        } else {
            ret as usize * self.channels
        };
        concealment_audio.truncate(old_len + written);
    }

    fn reset(&mut self) {
        self.dec_state.init();
    }

    fn packet_duration(&self, encoded: &[u8]) -> i32 {
        self.dec_state.duration_est(encoded)
    }

    fn packet_duration_redundant(&self, encoded: &[u8]) -> i32 {
        if !self.packet_has_fec(encoded) {
            // This packet is a RED packet.
            return self.packet_duration(encoded);
        }
        opus_fec_duration_est(encoded, self.sample_rate_hz)
    }

    fn packet_has_fec(&self, encoded: &[u8]) -> bool {
        opus_packet_has_fec(encoded)
    }

    fn sample_rate_hz(&self) -> i32 {
        self.sample_rate_hz
    }

    fn channels(&self) -> usize {
        self.channels
    }
}

#[cfg(test)]
mod tests {
    //! Ports of codecs/opus/audio_decoder_opus_unittest.cc and the Opus
    //! cases of neteq/audio_decoder_unittest.cc. WebRTC's speech resources
    //! (`near48_*.pcm`, `testfile32kHz.pcm`) are not in the sparse clone, so
    //! a synthetic voiced signal stands in for speech; the encoder is
    //! `ropus`'s, configured like `GetEncoderConfig`.

    use ropus::{Application, Bitrate, Channels, Encoder, InbandFec, Signal};

    use super::*;

    const K_SAMPLE_RATE_HZ: i32 = 48000;
    const K_ENCODER_FRAME_LENGTH: usize = 20 * K_SAMPLE_RATE_HZ as usize / 1000;

    fn channels_enum(num_channels: usize) -> Channels {
        if num_channels == 1 {
            Channels::Mono
        } else {
            Channels::Stereo
        }
    }

    /// `GetEncoderConfig`: VoIP, 32 kbps, complexity 10, no CBR. The
    /// signal hint is `Voice` so libopus stays in SILK / hybrid mode on the
    /// synthetic signal, as it does on WebRTC's speech recordings (in CELT
    /// mode the decoder's DTX concealment fades to silence).
    fn make_encoder(
        sample_rate_hz: i32,
        num_channels: usize,
        dtx_enabled: bool,
        fec_enabled: bool,
    ) -> Encoder {
        let mut encoder = make_encoder_with_application(
            sample_rate_hz,
            num_channels,
            dtx_enabled,
            fec_enabled,
            Application::Voip,
        );
        encoder.set_signal(Signal::Voice).expect("signal");
        encoder
    }

    fn make_encoder_with_application(
        sample_rate_hz: i32,
        num_channels: usize,
        dtx_enabled: bool,
        fec_enabled: bool,
        application: Application,
    ) -> Encoder {
        let mut builder = Encoder::builder(
            sample_rate_hz as u32,
            channels_enum(num_channels),
            application,
        )
        .bitrate(Bitrate::Bits(32000))
        .complexity(10)
        .vbr(true)
        .dtx(dtx_enabled);
        if fec_enabled {
            // FEC will only be encoded if there is packet loss.
            builder = builder.inband_fec(InbandFec::Enabled).packet_loss_perc(20);
        }
        builder.build().expect("encoder")
    }

    /// C++ `WhiteNoiseGenerator` (deterministic LCG instead of
    /// `webrtc::Random`).
    struct WhiteNoiseGenerator {
        amplitude: i32,
        state: u32,
    }

    impl WhiteNoiseGenerator {
        fn new(amplitude_dbfs: f64) -> Self {
            let amplitude = (10f64.powf(amplitude_dbfs / 20.0) * i16::MAX as f64) as i32;
            Self {
                amplitude,
                state: 42,
            }
        }

        fn generate_next_frame(&mut self, frame: &mut [i16]) {
            for sample in frame.iter_mut() {
                self.state = self
                    .state
                    .wrapping_mul(1_664_525)
                    .wrapping_add(1_013_904_223);
                let span = (2 * self.amplitude + 1) as u32;
                *sample = ((self.state >> 8) % span) as i32 as i16 - self.amplitude as i16;
            }
        }
    }

    /// Voiced, speech-like test signal: a 150 Hz (left) / 190 Hz (right)
    /// harmonic series with a 3 Hz syllable envelope.
    struct SpeechGenerator {
        num_channels: usize,
        sample_rate_hz: i32,
        n: usize,
    }

    impl SpeechGenerator {
        fn new(num_channels: usize, sample_rate_hz: i32) -> Self {
            Self {
                num_channels,
                sample_rate_hz,
                n: 0,
            }
        }

        fn next_frame(&mut self, samples_per_channel: usize) -> Vec<i16> {
            let fs = self.sample_rate_hz as f64;
            let mut frame = Vec::with_capacity(samples_per_channel * self.num_channels);
            for _ in 0..samples_per_channel {
                let t = self.n as f64 / fs;
                let envelope = 0.55 + 0.45 * (2.0 * std::f64::consts::PI * 3.0 * t).sin();
                for channel in 0..self.num_channels {
                    let f0 = if channel == 0 { 150.0 } else { 190.0 };
                    let mut value = 0.0;
                    for harmonic in 1..=12 {
                        let phase = 2.0 * std::f64::consts::PI * f0 * harmonic as f64 * t;
                        value += phase.sin() / harmonic as f64;
                    }
                    frame.push((value * envelope * 6000.0) as i16);
                }
                self.n += 1;
            }
            frame
        }
    }

    fn encode(encoder: &mut Encoder, pcm: &[i16]) -> Vec<u8> {
        let mut out = vec![0u8; 1500];
        let len = encoder.encode(pcm, &mut out).expect("encode");
        out.truncate(len);
        out
    }

    fn is_zeroed_frame(audio: &[i16]) -> bool {
        audio.iter().all(|&v| v == 0)
    }

    fn is_trivial_stereo(audio: &[i16]) -> bool {
        assert_eq!(0, audio.len() % 2);
        audio.chunks(2).all(|pair| pair[0] == pair[1])
    }

    /// C++ `EncodeDecodeSpeech`.
    fn encode_decode_speech(
        encoder: &mut Encoder,
        decoder: &mut AudioDecoderOpus,
        generator: &mut SpeechGenerator,
        timestamp: &mut u32,
        max_frames: usize,
    ) {
        let mut decoded_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * decoder.channels()];
        for _ in 0..max_frames {
            let payload = encode(encoder, &generator.next_frame(K_ENCODER_FRAME_LENGTH));
            let parse_results = decoder.parse_payload(payload, *timestamp);
            *timestamp = timestamp.wrapping_add(1);
            assert_eq!(1, parse_results.len());
            let decode_results = parse_results[0]
                .frame
                .decode(decoder, &mut decoded_frame)
                .expect("decode");
            assert_eq!(decoded_frame.len(), decode_results.num_decoded_samples);
        }
    }

    #[test]
    fn packet_has_fec_parses_lbrr_flag() {
        // TOC 0x08: SILK NB, 20 ms, mono, one frame. LBRR is the second bit.
        assert!(opus_packet_has_fec(&[0x08, 0x40, 0x11, 0x22]));
        assert!(!opus_packet_has_fec(&[0x08, 0x80, 0x11, 0x22]));
        // CELT-only packets never carry FEC.
        assert!(!opus_packet_has_fec(&[0x80 | 0x08, 0xFF, 0xFF]));
        assert!(!opus_packet_has_fec(&[]));
        // TOC only: the first frame is empty.
        assert!(!opus_packet_has_fec(&[0x08]));
        // Stereo (s bit): the side channel's LBRR flag is bit (1+1)*2-1.
        assert!(opus_packet_has_fec(&[0x0C, 0x10, 0x00]));
        // 40 ms SILK (config 2): two VAD bits, LBRR is the third bit.
        assert!(opus_packet_has_fec(&[0x10, 0x20, 0x00]));
        assert!(!opus_packet_has_fec(&[0x10, 0x40, 0x00]));
    }

    #[test]
    fn fec_duration_est() {
        let fec = [0x08u8, 0x40, 0x11, 0x22];
        assert_eq!(960, opus_fec_duration_est(&fec, 48000));
        assert_eq!(320, opus_fec_duration_est(&fec, 16000));
        assert_eq!(0, opus_fec_duration_est(&[0x08, 0x00, 0x11], 48000));
    }

    #[test]
    fn packet_duration_and_dtx() {
        let decoder = AudioDecoderOpus::new(1, 48000, false);
        // Empty payload: the PLC duration (10 ms).
        assert_eq!(480, decoder.packet_duration(&[]));
        // 20 ms SILK, one frame.
        assert_eq!(960, decoder.packet_duration(&[0x08, 0x01]));
        // Code 1: two equal frames, 40 ms.
        assert_eq!(1920, decoder.packet_duration(&[0x09, 0x01, 0x02]));
        // Code 3 with 6 x 20 ms = 120 ms is still valid; 7 frames is not.
        assert_eq!(5760, decoder.packet_duration(&[0x0B, 0x06]));
        assert_eq!(0, decoder.packet_duration(&[0x0B, 0x07]));
        // Code 3 without a frame count byte is invalid.
        assert_eq!(0, decoder.packet_duration(&[0x0B]));
        // Not FEC, so the redundant duration is the primary one.
        assert_eq!(960, decoder.packet_duration_redundant(&[0x08, 0x01]));
        assert_eq!(960, decoder.packet_duration_redundant(&[0x08, 0x40, 0x01]));
    }

    #[test]
    fn parse_payload_emits_fec_frame_first() {
        let mut decoder = AudioDecoderOpus::new(1, 48000, false);
        let fec_payload = vec![0x08u8, 0x40, 0x11, 0x22];
        let results = decoder.parse_payload(fec_payload, 10_000);
        assert_eq!(2, results.len());
        assert_eq!(1, results[0].priority);
        assert_eq!(10_000 - 960, results[0].timestamp);
        assert_eq!(960, results[0].frame.duration());
        assert_eq!(0, results[1].priority);
        assert_eq!(10_000, results[1].timestamp);
        assert_eq!(960, results[1].frame.duration());
        assert!(!results[1].frame.is_dtx_packet());

        // FEC timestamps wrap like C++ `uint32_t` arithmetic.
        let results = decoder.parse_payload(vec![0x08, 0x40, 0x11, 0x22], 100);
        assert_eq!(100u32.wrapping_sub(960), results[0].timestamp);

        let results = decoder.parse_payload(vec![0x08, 0x00, 0x11], 7);
        assert_eq!(1, results.len());
        assert_eq!(0, results[0].priority);

        let results = decoder.parse_payload(vec![0x08, 0x00], 7);
        assert!(results[0].frame.is_dtx_packet());
    }

    /// The `ReInitTest` and decode-length checks of C++
    /// `AudioDecoderOpusTest` (16 / 48 kHz, mono / stereo, 10 ms frames).
    #[test]
    fn audio_decoder_opus_reinit_and_frame_sizes() {
        for sample_rate_hz in [16000, 48000] {
            for num_channels in [1usize, 2] {
                let frame_size = sample_rate_hz as usize / 100;
                // C++ uses VoIP for mono and Audio for stereo here.
                let application = if num_channels == 1 {
                    Application::Voip
                } else {
                    Application::Audio
                };
                let mut encoder = make_encoder_with_application(
                    sample_rate_hz,
                    num_channels,
                    false,
                    false,
                    application,
                );
                let mut decoder = AudioDecoderOpus::new(num_channels, sample_rate_hz, false);
                assert!(!decoder.has_decode_plc());
                let mut generator = SpeechGenerator::new(num_channels, sample_rate_hz);

                // Decode ten frames; each yields exactly one frame of audio.
                let mut decoded = vec![0i16; frame_size * num_channels];
                for _ in 0..10 {
                    let payload = encode(&mut encoder, &generator.next_frame(frame_size));
                    let parse_result = decoder.parse_payload(payload, 0);
                    assert_eq!(1, parse_result.len());
                    assert_eq!(frame_size, parse_result[0].frame.duration());
                    let result = parse_result[0]
                        .frame
                        .decode(&mut decoder, &mut decoded)
                        .expect("decode");
                    assert_eq!(frame_size * num_channels, result.num_decoded_samples);
                }

                // ReInitTest: same payload, decoder reset before each decode.
                let encoded = encode(&mut encoder, &generator.next_frame(frame_size));
                let mut outputs = [
                    vec![0i16; frame_size * num_channels],
                    vec![0i16; frame_size * num_channels],
                ];
                for output in outputs.iter_mut() {
                    decoder.reset();
                    let parse_result = decoder.parse_payload(encoded.clone(), 0);
                    assert_eq!(1, parse_result.len());
                    let result = parse_result[0]
                        .frame
                        .decode(&mut decoder, output)
                        .expect("decode");
                    assert_eq!(frame_size * num_channels, result.num_decoded_samples);
                }
                assert_eq!(outputs[0], outputs[1]);
            }
        }
    }

    #[test]
    fn mono_encoder_stereo_decoder_outputs_trivial_stereo() {
        let mut generator = WhiteNoiseGenerator::new(-70.0);
        // Create a mono encoder.
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 1, false, false);
        // Create a stereo decoder.
        const K_DECODER_NUM_CHANNELS: usize = 2;
        let mut decoder = AudioDecoderOpus::new(K_DECODER_NUM_CHANNELS, K_SAMPLE_RATE_HZ, false);
        let mut input_frame = vec![0i16; K_ENCODER_FRAME_LENGTH];
        let mut decoded_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * K_DECODER_NUM_CHANNELS];

        for timestamp in 0..30u32 {
            generator.generate_next_frame(&mut input_frame);
            let payload = encode(&mut encoder, &input_frame);

            // Decode.
            let parse_results = decoder.parse_payload(payload, timestamp);
            assert_eq!(1, parse_results.len());
            let decode_results = parse_results[0]
                .frame
                .decode(&mut decoder, &mut decoded_frame)
                .expect("decode");
            assert_eq!(decoded_frame.len(), decode_results.num_decoded_samples);

            assert!(is_trivial_stereo(&decoded_frame));
        }
    }

    #[test]
    fn mono_encoder_stereo_decoder_outputs_trivial_stereo_plc() {
        // Create a mono encoder.
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 1, false, false);
        // Create a stereo decoder with `WebRTC-Audio-OpusGeneratePlc`.
        const K_DECODER_NUM_CHANNELS: usize = 2;
        let mut decoder = AudioDecoderOpus::new(K_DECODER_NUM_CHANNELS, K_SAMPLE_RATE_HZ, true);
        let mut generator = SpeechGenerator::new(1, K_SAMPLE_RATE_HZ);

        let mut timestamp = 0u32;
        // Feed the encoder with speech.
        encode_decode_speech(
            &mut encoder,
            &mut decoder,
            &mut generator,
            &mut timestamp,
            50,
        );

        // Generate packet loss concealment.
        let mut concealment_audio = Vec::new();
        const K_IGNORED: usize = 123;
        decoder.generate_plc(K_IGNORED, &mut concealment_audio);
        // Opus generates one 10 ms block per call, regardless of the request.
        assert_eq!(
            K_SAMPLE_RATE_HZ as usize / 100 * K_DECODER_NUM_CHANNELS,
            concealment_audio.len()
        );
        // Make sure that packet loss concealment is not a muted frame.
        assert!(!is_zeroed_frame(&concealment_audio));
        assert!(is_trivial_stereo(&concealment_audio));

        // Also check the first decoded audio frame after packet loss
        // concealment.
        let payload = encode(&mut encoder, &generator.next_frame(K_ENCODER_FRAME_LENGTH));
        let parse_results = decoder.parse_payload(payload, timestamp);
        let mut decoded_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * K_DECODER_NUM_CHANNELS];
        let result = parse_results[0]
            .frame
            .decode(&mut decoder, &mut decoded_frame)
            .expect("decode");
        assert_eq!(decoded_frame.len(), result.num_decoded_samples);
        assert!(!is_zeroed_frame(&decoded_frame));
        assert!(is_trivial_stereo(&decoded_frame));
    }

    #[test]
    fn generate_plc_is_a_no_op_by_default() {
        let mut decoder = AudioDecoderOpus::new(1, K_SAMPLE_RATE_HZ, false);
        let mut concealment_audio = vec![7i16];
        decoder.generate_plc(480, &mut concealment_audio);
        assert_eq!(vec![7i16], concealment_audio);
    }

    #[test]
    fn mono_encoder_stereo_decoder_outputs_trivial_stereo_fec() {
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 1, false, true);
        const K_DECODER_NUM_CHANNELS: usize = 2;
        let mut decoder = AudioDecoderOpus::new(K_DECODER_NUM_CHANNELS, K_SAMPLE_RATE_HZ, false);
        let mut decoded_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * K_DECODER_NUM_CHANNELS];
        let mut generator = SpeechGenerator::new(1, K_SAMPLE_RATE_HZ);

        // Encode and decode until FEC is found in a packet.
        let mut fec_found = false;
        for timestamp in 0..500u32 {
            let payload = encode(&mut encoder, &generator.next_frame(K_ENCODER_FRAME_LENGTH));
            let parse_results = decoder.parse_payload(payload, timestamp);
            if parse_results.len() == 1 {
                // No FEC frame encoded. Decode to update the decoder state.
                parse_results[0]
                    .frame
                    .decode(&mut decoder, &mut decoded_frame);
                continue;
            }
            assert_eq!(2, parse_results.len());
            assert_eq!(1, parse_results[0].priority); // FEC frame.
            fec_found = true;

            let decode_results = parse_results[0]
                .frame
                .decode(&mut decoder, &mut decoded_frame)
                .expect("decode");
            assert_eq!(decoded_frame.len(), decode_results.num_decoded_samples);
            assert!(is_trivial_stereo(&decoded_frame));
            break;
        }
        assert!(fec_found);
    }

    #[test]
    fn stereo_encoder_stereo_decoder_outputs_non_trivial_stereo_comfort_noise() {
        // Create a stereo encoder with DTX.
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 2, true, false);
        // Create a stereo decoder.
        const K_DECODER_NUM_CHANNELS: usize = 2;
        let mut decoder = AudioDecoderOpus::new(K_DECODER_NUM_CHANNELS, K_SAMPLE_RATE_HZ, false);
        let mut generator = SpeechGenerator::new(2, K_SAMPLE_RATE_HZ);

        let mut timestamp = 0u32;
        // Feed the encoder with speech, otherwise DTX will never kick in.
        encode_decode_speech(
            &mut encoder,
            &mut decoder,
            &mut generator,
            &mut timestamp,
            100,
        );

        // C++ `EncodeDecodeNoiseUntilDecoderInDtxMode`.
        let mut noise = WhiteNoiseGenerator::new(-70.0);
        let mut input_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * 2];
        let mut decoded_frame = vec![0i16; K_ENCODER_FRAME_LENGTH * K_DECODER_NUM_CHANNELS];
        let mut in_dtx = false;
        for _ in 0..50 {
            noise.generate_next_frame(&mut input_frame);
            let payload = encode(&mut encoder, &input_frame);
            let parse_results = decoder.parse_payload(payload, timestamp);
            timestamp += 1;
            assert_eq!(1, parse_results.len());
            let decode_results = parse_results[0]
                .frame
                .decode(&mut decoder, &mut decoded_frame)
                .expect("decode");
            assert_eq!(decoded_frame.len(), decode_results.num_decoded_samples);
            if parse_results[0].frame.is_dtx_packet() {
                assert_eq!(SpeechType::ComfortNoise, decode_results.speech_type);
                in_dtx = true;
                break;
            }
            assert_eq!(SpeechType::Speech, decode_results.speech_type);
        }
        assert!(in_dtx, "decoder never entered DTX mode");

        // Decode an empty packet so that Opus generates comfort noise.
        let mut speech_type = SpeechType::Speech;
        let num_decoded_samples =
            decoder.decode(&[], K_SAMPLE_RATE_HZ, &mut decoded_frame, &mut speech_type);
        assert_eq!(SpeechType::ComfortNoise, speech_type);
        assert!(num_decoded_samples > 0);
        assert!(num_decoded_samples as usize <= decoded_frame.len());
        let decoded_view = &decoded_frame[..num_decoded_samples as usize];
        // Make sure that comfort noise is not a muted frame.
        assert!(!is_zeroed_frame(decoded_view));
        assert!(!is_trivial_stereo(decoded_view));

        // A reset leaves DTX mode: an empty payload is plain PLC again.
        decoder.reset();
        decoder.decode(&[], K_SAMPLE_RATE_HZ, &mut decoded_frame, &mut speech_type);
        assert_eq!(SpeechType::Speech, speech_type);
    }

    #[test]
    fn stereo_encoder_stereo_decoder_outputs_non_trivial_stereo_plc() {
        // Create a stereo encoder.
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 2, false, false);
        // Create a stereo decoder with `WebRTC-Audio-OpusGeneratePlc`.
        const K_DECODER_NUM_CHANNELS: usize = 2;
        let mut decoder = AudioDecoderOpus::new(K_DECODER_NUM_CHANNELS, K_SAMPLE_RATE_HZ, true);
        let mut generator = SpeechGenerator::new(2, K_SAMPLE_RATE_HZ);

        let mut timestamp = 0u32;
        // Feed the encoder with speech.
        encode_decode_speech(
            &mut encoder,
            &mut decoder,
            &mut generator,
            &mut timestamp,
            100,
        );

        // Generate packet loss concealment.
        let mut concealment_audio = Vec::new();
        const K_IGNORED: usize = 123;
        decoder.generate_plc(K_IGNORED, &mut concealment_audio);
        assert!(!concealment_audio.is_empty());
        // Make sure that packet loss concealment is not a muted frame.
        assert!(!is_zeroed_frame(&concealment_audio));
        assert!(!is_trivial_stereo(&concealment_audio));
    }

    #[test]
    fn decode_rejects_too_small_output_buffer() {
        let mut encoder = make_encoder(K_SAMPLE_RATE_HZ, 1, false, false);
        let mut decoder = AudioDecoderOpus::new(1, K_SAMPLE_RATE_HZ, false);
        let mut generator = SpeechGenerator::new(1, K_SAMPLE_RATE_HZ);
        let payload = encode(&mut encoder, &generator.next_frame(K_ENCODER_FRAME_LENGTH));
        let mut small = vec![0i16; K_ENCODER_FRAME_LENGTH - 1];
        let mut speech_type = SpeechType::Speech;
        assert_eq!(
            -1,
            decoder.decode(&payload, K_SAMPLE_RATE_HZ, &mut small, &mut speech_type)
        );
    }
}
