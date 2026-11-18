//! `AudioDecoder` over the system C libopus (the one the C++ reference links).
//!
//! The Rust NetEq normally decodes Opus with `ropus`, which drifts from C
//! libopus in codec-internal CNG and in hybrid-fullband PLC (see README).
//! To make the parity gate test NetEq rather than the codec, this decoder
//! calls the same `opus_decode` the C++ side calls. Everything around the
//! codec call mirrors `neteq::codecs::opus` (itself a port of
//! `audio_decoder_opus.cc` and the `WebRtcOpus_*` helpers): ParsePayload with
//! the FEC frame first, DTX frames of at most two bytes, 10 ms PLC, and the
//! mono-packet-on-stereo-decoder workaround.

use std::os::raw::{c_int, c_uchar};

use neteq::api::audio_decoder::{
    AudioDecoder, DecodeResult, EncodedAudioFrame, ParseResult, SpeechType,
};

/// Opaque `OpusDecoder`.
#[repr(C)]
struct RawOpusDecoder {
    _private: [u8; 0],
}

const OPUS_RESET_STATE: c_int = 4028;
const MAX_FRAMES: usize = 48;

extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut RawOpusDecoder;
    fn opus_decoder_destroy(st: *mut RawOpusDecoder);
    fn opus_decoder_ctl(st: *mut RawOpusDecoder, request: c_int, ...) -> c_int;
    fn opus_decode(
        st: *mut RawOpusDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut i16,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_packet_get_nb_channels(data: *const c_uchar) -> c_int;
    fn opus_packet_get_nb_frames(data: *const c_uchar, len: i32) -> c_int;
    fn opus_packet_get_samples_per_frame(data: *const c_uchar, fs: i32) -> c_int;
    fn opus_packet_parse(
        data: *const c_uchar,
        len: i32,
        out_toc: *mut c_uchar,
        frames: *mut *const c_uchar,
        size: *mut i16,
        payload_offset: *mut c_int,
    ) -> c_int;
}

/// C `kWebRtcOpusMaxDecodeFrameSizeMs`.
const MAX_DECODE_FRAME_SIZE_MS: i32 = 120;
/// C `kWebRtcOpusPlcFrameSizeMs`.
const PLC_FRAME_SIZE_MS: i32 = 10;

fn frame_size_per_channel(frame_size_ms: i32, sample_rate_hz: i32) -> i32 {
    frame_size_ms * (sample_rate_hz / 1000)
}

fn samples_per_frame(payload: &[u8], sample_rate_hz: i32) -> i32 {
    // SAFETY: callers pass a non-empty slice, so `data[0]` (the TOC) exists.
    unsafe { opus_packet_get_samples_per_frame(payload.as_ptr(), sample_rate_hz) }
}

fn num_channels_of(payload: &[u8]) -> i32 {
    // SAFETY: callers pass a non-empty slice.
    unsafe { opus_packet_get_nb_channels(payload.as_ptr()) }
}

/// C `WebRtcOpus_NumSilkFrames`.
fn num_silk_frames(payload: &[u8]) -> i32 {
    let mut payload_length_ms = samples_per_frame(payload, 48000) / 48;
    if payload_length_ms < 10 {
        payload_length_ms = 10;
    }
    match payload_length_ms {
        10 | 20 => 1,
        40 => 2,
        60 => 3,
        _ => 0,
    }
}

/// C `WebRtcOpus_PacketHasFec`.
fn packet_has_fec(payload: &[u8]) -> bool {
    if payload.is_empty() || payload[0] & 0x80 != 0 {
        return false;
    }
    let silk_frames = num_silk_frames(payload);
    if silk_frames == 0 {
        return false;
    }
    let channels = num_channels_of(payload);
    let Ok(len) = i32::try_from(payload.len()) else {
        return false;
    };
    let mut frame_sizes = [0i16; MAX_FRAMES];
    let mut frames = [std::ptr::null::<c_uchar>(); MAX_FRAMES];
    let mut toc = 0u8;
    let mut payload_offset: c_int = 0;
    // SAFETY: `payload` is valid for `len` bytes; the out arrays hold
    // MAX_FRAMES entries as libopus requires.
    let n = unsafe {
        opus_packet_parse(
            payload.as_ptr(),
            len,
            &mut toc,
            frames.as_mut_ptr(),
            frame_sizes.as_mut_ptr(),
            &mut payload_offset,
        )
    };
    if n < 0 || frame_sizes[0] < 1 {
        return false;
    }
    let first_byte = payload[payload_offset as usize];
    (0..channels).any(|n| first_byte as i32 & (0x80 >> ((n + 1) * (silk_frames + 1) - 1)) != 0)
}

/// C `WebRtcOpus_FecDurationEst`.
fn fec_duration_est(payload: &[u8], sample_rate_hz: i32) -> i32 {
    if !packet_has_fec(payload) {
        return 0;
    }
    let samples = samples_per_frame(payload, sample_rate_hz);
    let per_ms = sample_rate_hz / 1000;
    if samples < 10 * per_ms || samples > 120 * per_ms {
        return 0;
    }
    samples
}

/// C `WebRtcOpusDecInst`.
struct LibopusDecInst {
    decoder: *mut RawOpusDecoder,
    channels: usize,
    in_dtx_mode: bool,
    sample_rate_hz: i32,
    last_packet_num_channels: i32,
}

// SAFETY: the libopus decoder is a private heap object owned by this struct
// and only touched through `&mut self`.
unsafe impl Send for LibopusDecInst {}

impl Drop for LibopusDecInst {
    fn drop(&mut self) {
        // SAFETY: `decoder` came from `opus_decoder_create` and is freed once.
        unsafe { opus_decoder_destroy(self.decoder) };
    }
}

impl LibopusDecInst {
    fn create(channels: usize, sample_rate_hz: i32) -> Option<Self> {
        let mut error: c_int = 0;
        // SAFETY: plain constructor call; `error` is a valid out pointer.
        let decoder = unsafe { opus_decoder_create(sample_rate_hz, channels as c_int, &mut error) };
        if decoder.is_null() || error != 0 {
            return None;
        }
        Some(Self {
            decoder,
            channels,
            in_dtx_mode: false,
            sample_rate_hz,
            last_packet_num_channels: channels as i32,
        })
    }

    fn init(&mut self) {
        // SAFETY: valid decoder; OPUS_RESET_STATE takes no argument.
        unsafe { opus_decoder_ctl(self.decoder, OPUS_RESET_STATE) };
        self.in_dtx_mode = false;
    }

    fn determine_audio_type(&mut self, encoded_bytes: usize) -> i16 {
        if encoded_bytes == 0 && self.in_dtx_mode {
            2
        } else if encoded_bytes == 1 || encoded_bytes == 2 {
            self.in_dtx_mode = true;
            2
        } else {
            self.in_dtx_mode = false;
            0
        }
    }

    fn decode_native(
        &mut self,
        encoded: &[u8],
        frame_size: i32,
        decoded: &mut [i16],
        audio_type: &mut i16,
        decode_fec: bool,
    ) -> i32 {
        if !encoded.is_empty() {
            self.last_packet_num_channels = num_channels_of(encoded);
        }
        let available = (decoded.len() / self.channels).min(i32::MAX as usize) as i32;
        let frame_size = frame_size.min(available);
        let (data, len) = if encoded.is_empty() {
            (std::ptr::null(), 0)
        } else {
            (encoded.as_ptr(), encoded.len() as i32)
        };
        let n = if frame_size <= 0 {
            -1
        } else {
            // SAFETY: `decoded` holds at least `frame_size * channels`
            // samples (clamped above); `data` is null with len 0 (PLC) or
            // valid for `len` bytes.
            unsafe {
                opus_decode(
                    self.decoder,
                    data,
                    len,
                    decoded.as_mut_ptr(),
                    frame_size,
                    decode_fec as c_int,
                )
            }
        };
        if n <= 0 {
            return -1;
        }
        *audio_type = self.determine_audio_type(encoded.len());
        if self.channels == 2 && self.last_packet_num_channels == 1 {
            let total = n as usize * self.channels;
            for i in (0..total).step_by(self.channels) {
                decoded[i + 1] = decoded[i];
            }
        }
        n
    }

    fn decode_plc(&mut self, decoded: &mut [i16]) -> i32 {
        let mut audio_type = 0i16;
        let plc_samples = frame_size_per_channel(PLC_FRAME_SIZE_MS, self.sample_rate_hz);
        self.decode_native(&[], plc_samples, decoded, &mut audio_type, false)
            .max(-1)
    }

    fn decode(&mut self, encoded: &[u8], decoded: &mut [i16], audio_type: &mut i16) -> i32 {
        if encoded.is_empty() {
            *audio_type = self.determine_audio_type(0);
            self.decode_plc(decoded)
        } else {
            let max = frame_size_per_channel(MAX_DECODE_FRAME_SIZE_MS, self.sample_rate_hz);
            self.decode_native(encoded, max, decoded, audio_type, false)
        }
    }

    fn decode_fec(&mut self, encoded: &[u8], decoded: &mut [i16], audio_type: &mut i16) -> i32 {
        if !packet_has_fec(encoded) {
            return 0;
        }
        let fec_samples = samples_per_frame(encoded, self.sample_rate_hz);
        self.decode_native(encoded, fec_samples, decoded, audio_type, true)
    }

    fn plc_duration(&self) -> i32 {
        frame_size_per_channel(PLC_FRAME_SIZE_MS, self.sample_rate_hz)
    }

    fn duration_est(&self, payload: &[u8]) -> i32 {
        if payload.is_empty() {
            return self.plc_duration();
        }
        let Ok(len) = i32::try_from(payload.len()) else {
            return 0;
        };
        // SAFETY: `payload` is non-empty and valid for `len` bytes.
        let frames = unsafe { opus_packet_get_nb_frames(payload.as_ptr(), len) };
        if frames < 0 {
            return 0;
        }
        let samples = frames * samples_per_frame(payload, self.sample_rate_hz);
        if samples > 120 * self.sample_rate_hz / 1000 {
            return 0;
        }
        samples
    }
}

struct OpusFrame {
    payload: Vec<u8>,
    is_primary_payload: bool,
    duration: usize,
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

/// Opus `AudioDecoder` backed by C libopus (C++ `AudioDecoderOpusImpl`).
pub struct LibopusAudioDecoder {
    dec_state: LibopusDecInst,
    channels: usize,
    sample_rate_hz: i32,
}

impl LibopusAudioDecoder {
    /// Decoder for `num_channels` (1 or 2) at `sample_rate_hz`. `None` if
    /// libopus rejects the parameters.
    pub fn new(num_channels: usize, sample_rate_hz: i32) -> Option<Self> {
        let mut dec_state = LibopusDecInst::create(num_channels, sample_rate_hz)?;
        dec_state.init();
        Some(Self {
            dec_state,
            channels: num_channels,
            sample_rate_hz,
        })
    }

    fn frame(&self, payload: Vec<u8>, is_primary_payload: bool) -> OpusFrame {
        let ret = if is_primary_payload {
            self.packet_duration(&payload)
        } else {
            self.packet_duration_redundant(&payload)
        };
        OpusFrame {
            payload,
            is_primary_payload,
            duration: ret.max(0) as usize,
        }
    }
}

impl AudioDecoder for LibopusAudioDecoder {
    fn parse_payload(&mut self, payload: Vec<u8>, timestamp: u32) -> Vec<ParseResult> {
        let mut results = Vec::with_capacity(2);
        if self.packet_has_fec(&payload) {
            let duration = self.packet_duration_redundant(&payload);
            let fec_frame = self.frame(payload.clone(), false);
            results.push(ParseResult::new(
                timestamp.wrapping_sub(duration as u32),
                1,
                Box::new(fec_frame),
            ));
        }
        let frame = self.frame(payload, true);
        results.push(ParseResult::new(timestamp, 0, Box::new(frame)));
        results
    }

    fn decode_internal(
        &mut self,
        encoded: &[u8],
        _sample_rate_hz: i32,
        decoded: &mut [i16],
        speech_type: &mut SpeechType,
    ) -> i32 {
        let mut temp_type = 1i16;
        let mut ret = self.dec_state.decode(encoded, decoded, &mut temp_type);
        if ret > 0 {
            ret *= self.channels as i32;
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
            return self.decode_internal(encoded, sample_rate_hz, decoded, speech_type);
        }
        let mut temp_type = 1i16;
        let mut ret = self.dec_state.decode_fec(encoded, decoded, &mut temp_type);
        if ret > 0 {
            ret *= self.channels as i32;
        }
        *speech_type = SpeechType::from_codec_type(temp_type);
        ret
    }

    fn generate_plc(&mut self, _requested: usize, concealment_audio: &mut Vec<i16>) {
        // `WebRTC-Audio-OpusGeneratePlc` is off by default: a no-op, like
        // the ropus decoder with `generate_plc == false`.
        let _ = concealment_audio;
    }

    fn reset(&mut self) {
        self.dec_state.init();
    }

    fn packet_duration(&self, encoded: &[u8]) -> i32 {
        self.dec_state.duration_est(encoded)
    }

    fn packet_duration_redundant(&self, encoded: &[u8]) -> i32 {
        if !self.packet_has_fec(encoded) {
            return self.packet_duration(encoded);
        }
        fec_duration_est(encoded, self.sample_rate_hz)
    }

    fn packet_has_fec(&self, encoded: &[u8]) -> bool {
        packet_has_fec(encoded)
    }

    fn sample_rate_hz(&self) -> i32 {
        self.sample_rate_hz
    }

    fn channels(&self) -> usize {
        self.channels
    }
}
