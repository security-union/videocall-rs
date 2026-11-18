// Port target: WebRTC modules/audio_coding/neteq/comfort_noise.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: comfort_noise_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ keeps `DecoderDatabase*` and `SyncBuffer*`; here they are passed per
// call.

//! RFC 3389 comfort noise generation from SID packets.

use crate::codecs::cng::ComfortNoiseDecoder;
use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::dsp_helper::DspHelper;
use crate::neteq::packet::Packet;
use crate::neteq::sync_buffer::SyncBuffer;

/// C++ `ComfortNoise`.
#[derive(Debug)]
pub struct ComfortNoise {
    fs_hz: i32,
    first_call: bool,
    overlap_length: usize,
    internal_error_code: i32,
}

impl ComfortNoise {
    /// C++ `ComfortNoise::kOK`.
    pub const K_OK: i32 = 0;
    /// C++ `ComfortNoise::kUnknownPayloadType`.
    pub const K_UNKNOWN_PAYLOAD_TYPE: i32 = 1;
    /// C++ `ComfortNoise::kInternalError`.
    pub const K_INTERNAL_ERROR: i32 = 2;
    /// C++ `ComfortNoise::kMultiChannelNotSupported`.
    pub const K_MULTI_CHANNEL_NOT_SUPPORTED: i32 = 3;

    /// Comfort noise at `fs_hz`; overlap is `5 * fs_hz / 8000` samples.
    pub fn new(fs_hz: i32) -> Self {
        Self {
            fs_hz,
            first_call: true,
            overlap_length: (5 * fs_hz / 8000) as usize,
            // C++ leaves this uninitialized and never writes it.
            internal_error_code: 0,
        }
    }

    /// Resets the state. Should be called before each new comfort noise
    /// period.
    pub fn reset(&mut self) {
        self.first_call = true;
    }

    /// Updates the CNG decoder parameters from the SID `packet`. Returns one
    /// of the `K_*` codes.
    pub fn update_parameters(
        &mut self,
        packet: &Packet,
        decoder_database: &mut DecoderDatabase,
    ) -> i32 {
        // Get comfort noise decoder.
        if decoder_database.set_active_cng_decoder(packet.payload_type) != DecoderDatabase::K_OK {
            return Self::K_UNKNOWN_PAYLOAD_TYPE;
        }
        let cng_decoder = decoder_database.get_active_cng_decoder();
        debug_assert!(cng_decoder.is_some());
        if let Some(cng_decoder) = cng_decoder {
            cng_decoder.update_sid(&packet.payload);
        }
        Self::K_OK
    }

    /// Generates `requested_length` samples of comfort noise into `output`,
    /// cross-fading with the end of `sync_buffer` on the first call. Returns
    /// one of the `K_*` codes.
    pub fn generate(
        &mut self,
        requested_length: usize,
        output: &mut AudioMultiVector,
        decoder_database: &mut DecoderDatabase,
        sync_buffer: &mut SyncBuffer,
    ) -> i32 {
        self.generate_with_decoder(
            requested_length,
            output,
            || decoder_database.get_active_cng_decoder(),
            sync_buffer,
        )
    }

    /// `generate` with the decoder lookup abstracted, so tests can drive it
    /// with a bare `ComfortNoiseDecoder`. `get_decoder` is called at the
    /// point where C++ calls `GetActiveCngDecoder()`.
    pub(crate) fn generate_with_decoder<'a>(
        &mut self,
        requested_length: usize,
        output: &mut AudioMultiVector,
        get_decoder: impl FnOnce() -> Option<&'a mut ComfortNoiseDecoder>,
        sync_buffer: &mut SyncBuffer,
    ) -> i32 {
        debug_assert!(
            self.fs_hz == 8000 || self.fs_hz == 16000 || self.fs_hz == 32000 || self.fs_hz == 48000
        );
        // Not adapted for multi-channel yet.
        if output.channels() != 1 {
            log::error!("No multi-channel support");
            return Self::K_MULTI_CHANNEL_NOT_SUPPORTED;
        }

        let mut number_of_samples = requested_length;
        let mut new_period = false;
        if self.first_call {
            // Generate noise and overlap slightly with old data.
            number_of_samples = requested_length + self.overlap_length;
            new_period = true;
        }
        output.assert_size(number_of_samples);
        // Get the decoder from the database.
        let Some(cng_decoder) = get_decoder() else {
            log::error!("Unknwown payload type");
            return Self::K_UNKNOWN_PAYLOAD_TYPE;
        };

        let mut temp = vec![0i16; number_of_samples];
        if !cng_decoder.generate(&mut temp, new_period) {
            // Error returned.
            output.zeros(requested_length);
            log::error!("ComfortNoiseDecoder::Genererate failed to generate comfort noise");
            return Self::K_INTERNAL_ERROR;
        }
        output[0].overwrite_at_slice(&temp, 0);

        if self.first_call {
            // Set tapering window parameters. Values are in Q15.
            // `muting_window`: mixing factor for overlap data.
            // `muting_window_increment`: mixing factor increment (negative).
            // `unmuting_window`: mixing factor for comfort noise.
            // `unmuting_window_increment`: mixing factor increment.
            let (
                muting_window,
                muting_window_increment,
                unmuting_window,
                unmuting_window_increment,
            ) = match self.fs_hz {
                8000 => (
                    DspHelper::K_MUTE_FACTOR_START_8KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_8KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_8KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_8KHZ,
                ),
                16000 => (
                    DspHelper::K_MUTE_FACTOR_START_16KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_16KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_16KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_16KHZ,
                ),
                32000 => (
                    DspHelper::K_MUTE_FACTOR_START_32KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_32KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_32KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_32KHZ,
                ),
                // fs_hz == 48000
                _ => (
                    DspHelper::K_MUTE_FACTOR_START_48KHZ,
                    DspHelper::K_MUTE_FACTOR_INCREMENT_48KHZ,
                    DspHelper::K_UNMUTE_FACTOR_START_48KHZ,
                    DspHelper::K_UNMUTE_FACTOR_INCREMENT_48KHZ,
                ),
            };
            // The C++ windows are `int16_t`.
            let muting_window_increment = muting_window_increment as i16;
            let unmuting_window_increment = unmuting_window_increment as i16;
            let mut muting = muting_window as i16;
            let mut unmuting = unmuting_window as i16;

            // Do overlap-add between new vector and overlap.
            let start_ix = sync_buffer.size() - self.overlap_length;
            for i in 0..self.overlap_length {
                // overlapVec[i] = WinMute * overlapVec[i] + WinUnMute * outData[i]
                let old = sync_buffer[0][start_ix + i] as i32;
                let noise = output[0][i] as i32;
                sync_buffer[0][start_ix + i] = ((old * muting as i32)
                    .wrapping_add(noise * unmuting as i32)
                    .wrapping_add(16384)
                    >> 15) as i16;
                muting = muting.wrapping_add(muting_window_increment);
                unmuting = unmuting.wrapping_add(unmuting_window_increment);
            }
            // Remove `overlap_length` samples from the front of `output` since
            // they were mixed into `sync_buffer` above.
            output.pop_front(self.overlap_length);
        }
        self.first_call = false;
        Self::K_OK
    }

    /// Last internal error code.
    pub fn internal_error_code(&self) -> i32 {
        self.internal_error_code
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::api::audio_format::SdpAudioFormat;
    use crate::codecs::builtin_factory::BuiltinAudioDecoderFactory;
    use crate::neteq::time_stretch::test_signal::{feed_interleaved, make_signal};
    use crate::spl::test_util::{Hash, Rng};

    const K_CNG_PAYLOAD_TYPE: u8 = 13;

    fn cng_database(fs: i32) -> DecoderDatabase {
        let mut db = DecoderDatabase::new(Arc::new(BuiltinAudioDecoderFactory::new()));
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(K_CNG_PAYLOAD_TYPE as i32, &SdpAudioFormat::new("CN", fs, 1))
        );
        db
    }

    fn sid_packet(payload_type: u8, payload: Vec<u8>) -> Packet {
        let mut packet = Packet::new();
        packet.payload_type = payload_type;
        packet.payload = payload;
        packet
    }

    // comfort_noise_unittest.cc: CreateAndDestroy.
    #[test]
    fn create_and_destroy() {
        let fs = 8000;
        let _db = cng_database(fs);
        let _sync_buffer = SyncBuffer::new(1, 1000);
        let cn = ComfortNoise::new(fs);
        assert_eq!(0, cn.internal_error_code());
    }

    #[test]
    fn unknown_payload_type_and_missing_decoder() {
        let fs = 16000;
        let mut db = cng_database(fs);
        let mut sync_buffer = SyncBuffer::new(1, 1000);
        let mut cn = ComfortNoise::new(fs);
        assert_eq!(
            ComfortNoise::K_UNKNOWN_PAYLOAD_TYPE,
            cn.update_parameters(&sid_packet(99, vec![40, 1, 2]), &mut db)
        );
        // No active CNG decoder yet: C++ grows `output` before failing.
        let mut output = AudioMultiVector::new(1);
        assert_eq!(
            ComfortNoise::K_UNKNOWN_PAYLOAD_TYPE,
            cn.generate(160, &mut output, &mut db, &mut sync_buffer)
        );
        assert_eq!(160 + 10, output.size());

        let mut stereo = AudioMultiVector::new(2);
        assert_eq!(
            ComfortNoise::K_MULTI_CHANNEL_NOT_SUPPORTED,
            cn.generate(160, &mut stereo, &mut db, &mut sync_buffer)
        );
    }

    // Golden hash against the real C++ ComfortNoise
    // (spl/golden/golden_gen_wp2b.cc, `comfort_noise_golden`, whose stub
    // DecoderDatabase holds one CN decoder at payload type 13).
    #[test]
    fn golden_comfort_noise() {
        let mut r = Rng::new();
        let mut h = Hash::new();
        for fs in [8000, 16000, 32000, 48000] {
            let mut db = cng_database(fs);
            let mut sync_buffer = SyncBuffer::new(1, 1000);
            let sig = make_signal(&mut r, 1000, fs);
            for (i, &s) in sig.iter().enumerate() {
                sync_buffer[0][i] = s;
            }
            let mut cn = ComfortNoise::new(fs);
            let max_request = 600.min(fs / 50);
            for _ in 0..60 {
                if r.below(3) == 0 {
                    let payload_type = if r.below(4) == 0 {
                        99
                    } else {
                        K_CNG_PAYLOAD_TYPE
                    };
                    let len = r.range(1, 14) as usize;
                    let payload: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
                    let packet = sid_packet(payload_type, payload);
                    h.feed(cn.update_parameters(&packet, &mut db) as i64);
                }
                if r.below(8) == 0 {
                    cn.reset();
                }
                let requested = if r.below(15) == 0 {
                    640
                } else {
                    r.range(1, max_request) as usize
                };
                let channels = if r.below(20) == 0 { 2 } else { 1 };
                let mut out = AudioMultiVector::new(channels);
                let ret = cn.generate(requested, &mut out, &mut db, &mut sync_buffer);
                h.feed(ret as i64);
                h.feed(out.size() as i64);
                feed_interleaved(&mut h, &out);
                for i in sync_buffer.size() - 64..sync_buffer.size() {
                    h.feed(sync_buffer[0][i] as i64);
                }
            }
        }
        assert_eq!(0x9e72cee3ca0465a0, h.value());
    }
}
