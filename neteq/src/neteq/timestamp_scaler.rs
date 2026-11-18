// Port target: WebRTC modules/audio_coding/neteq/timestamp_scaler.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: timestamp_scaler_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ keeps `const DecoderDatabase&`; here it is passed per call, `&mut`
// because `DecoderInfo::sample_rate_hz` may create the decoder.

//! Converts between external (RTP) and internal timestamps for codecs whose
//! RTP clock differs from their sample rate (e.g. G.722).

use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::packet::{Packet, PacketList};

/// C++ `TimestampScaler`.
#[derive(Debug, Clone)]
pub struct TimestampScaler {
    first_packet_received: bool,
    numerator: i32,
    denominator: i32,
    external_ref: u32,
    internal_ref: u32,
}

impl Default for TimestampScaler {
    fn default() -> Self {
        Self::new()
    }
}

impl TimestampScaler {
    /// A scaler with ratio 1/1.
    pub fn new() -> Self {
        Self {
            first_packet_received: false,
            numerator: 1,
            denominator: 1,
            external_ref: 0,
            internal_ref: 0,
        }
    }

    /// Forgets the reference timestamps.
    pub fn reset(&mut self) {
        self.first_packet_received = false;
    }

    /// Scales `packet.timestamp` to internal.
    pub fn to_internal_packet(
        &mut self,
        packet: &mut Packet,
        decoder_database: &mut DecoderDatabase,
    ) {
        packet.timestamp =
            self.to_internal(packet.timestamp, packet.payload_type, decoder_database);
    }

    /// Scales every packet in `packet_list` to internal.
    pub fn to_internal_list(
        &mut self,
        packet_list: &mut PacketList,
        decoder_database: &mut DecoderDatabase,
    ) {
        for packet in packet_list.iter_mut() {
            self.to_internal_packet(packet, decoder_database);
        }
    }

    /// Scales `external_timestamp` for `rtp_payload_type` to internal.
    pub fn to_internal(
        &mut self,
        external_timestamp: u32,
        rtp_payload_type: u8,
        decoder_database: &mut DecoderDatabase,
    ) -> u32 {
        let Some(info) = decoder_database.get_decoder_info_mut(rtp_payload_type) else {
            // Payload type is unknown. Do not scale.
            return external_timestamp;
        };
        if !(info.is_comfort_noise() || info.is_dtmf()) {
            // Do not change the timestamp scaling settings for DTMF or CNG.
            self.numerator = info.sample_rate_hz();
            if info.get_format().clockrate_hz == 0 {
                // If the clockrate is invalid (i.e. with an old-style external
                // codec) we cannot do any timestamp scaling.
                self.denominator = self.numerator;
            } else {
                self.denominator = info.get_format().clockrate_hz;
            }
        }
        if self.numerator != self.denominator {
            // We have a scale factor != 1.
            if !self.first_packet_received {
                self.external_ref = external_timestamp;
                self.internal_ref = external_timestamp;
                self.first_packet_received = true;
            }
            let external_diff = external_timestamp as i64 - self.external_ref as i64;
            debug_assert!(self.denominator > 0);
            self.external_ref = external_timestamp;
            // C++ `uint32_t += int64_t`: computed in 64 bits, truncated.
            self.internal_ref = (self.internal_ref as i64)
                .wrapping_add((external_diff * self.numerator as i64) / self.denominator as i64)
                as u32;
            self.internal_ref
        } else {
            // No scaling.
            external_timestamp
        }
    }

    /// Scales `internal_timestamp` back to external.
    pub fn to_external(&self, internal_timestamp: u32) -> u32 {
        if !self.first_packet_received || (self.numerator == self.denominator) {
            // Not initialized, or scale factor is 1.
            internal_timestamp
        } else {
            let internal_diff = internal_timestamp as i64 - self.internal_ref as i64;
            debug_assert!(self.numerator > 0);
            // Do not update references in this method.
            // Switch `denominator` and `numerator` to convert the other way.
            (self.external_ref as i64)
                .wrapping_add((internal_diff * self.denominator as i64) / self.numerator as i64)
                as u32
        }
    }
}

#[cfg(test)]
mod tests {
    //! Port of timestamp_scaler_unittest.cc. C++ uses a mock database
    //! returning `DecoderInfo`s made by the builtin factory; here a real
    //! `DecoderDatabase` with a test factory plays that role. G.722 (16 kHz
    //! audio on an 8 kHz RTP clock) is not a builtin codec, so the factory
    //! fakes its sample rate.

    use std::sync::Arc;

    use super::*;
    use crate::api::audio_decoder::{AudioDecoder, AudioDecoderFactory, SpeechType};
    use crate::api::audio_format::SdpAudioFormat;

    struct RateOnlyDecoder {
        sample_rate_hz: i32,
    }

    impl AudioDecoder for RateOnlyDecoder {
        fn decode_internal(
            &mut self,
            _encoded: &[u8],
            _sample_rate_hz: i32,
            _decoded: &mut [i16],
            _speech_type: &mut SpeechType,
        ) -> i32 {
            0
        }
        fn reset(&mut self) {}
        fn sample_rate_hz(&self) -> i32 {
            self.sample_rate_hz
        }
        fn channels(&self) -> usize {
            1
        }
    }

    /// Sample rates as WebRTC's builtin decoders report them: G.722 decodes
    /// at 16 kHz, everything else at its clock rate.
    struct TestFactory;

    impl AudioDecoderFactory for TestFactory {
        fn make_decoder(&self, format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
            let sample_rate_hz = if format.name.eq_ignore_ascii_case("g722") {
                16000
            } else {
                format.clockrate_hz
            };
            Some(Box::new(RateOnlyDecoder { sample_rate_hz }))
        }
    }

    fn db_with(entries: &[(u8, SdpAudioFormat)]) -> DecoderDatabase {
        let mut db = DecoderDatabase::new(Arc::new(TestFactory));
        for (payload_type, format) in entries {
            assert_eq!(
                DecoderDatabase::K_OK,
                db.register_payload(*payload_type as i32, format)
            );
        }
        db
    }

    #[test]
    fn test_no_scaling() {
        // Use PCMu, because it doesn't use scaled timestamps.
        const K_RTP_PAYLOAD_TYPE: u8 = 0;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("pcmu", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        // Test both sides of the timestamp wrap-around.
        let mut timestamp: u32 = 0xFFFF_FFFF - 5;
        while timestamp != 5 {
            // Scale to internal timestamp.
            assert_eq!(
                timestamp,
                scaler.to_internal(timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            // Scale back.
            assert_eq!(timestamp, scaler.to_external(timestamp));
            timestamp = timestamp.wrapping_add(1);
        }
    }

    #[test]
    fn test_no_scaling_large_step() {
        const K_RTP_PAYLOAD_TYPE: u8 = 0;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("pcmu", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        // Test both sides of the timestamp wrap-around.
        const K_STEP: u32 = 160;
        // `timestamp` will be a large positive value.
        let mut timestamp = 0u32.wrapping_sub(5 * K_STEP);
        while timestamp != 5 * K_STEP {
            assert_eq!(
                timestamp,
                scaler.to_internal(timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(timestamp, scaler.to_external(timestamp));
            timestamp = timestamp.wrapping_add(K_STEP);
        }
    }

    #[test]
    fn test_g722() {
        // Use G722, which has a factor 2 scaling.
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("g722", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        let mut external_timestamp: u32 = 0xFFFF_FFFF - 5;
        let mut internal_timestamp = external_timestamp;
        while external_timestamp != 5 {
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            internal_timestamp = internal_timestamp.wrapping_add(2);
            external_timestamp = external_timestamp.wrapping_add(1);
        }
    }

    #[test]
    fn test_g722_large_step() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("g722", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        const K_STEP: u32 = 320;
        // `external_timestamp` will be a large positive value.
        let mut external_timestamp = 0u32.wrapping_sub(5 * K_STEP);
        let mut internal_timestamp = external_timestamp;
        while external_timestamp != 5 * K_STEP {
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            // Internal timestamp should be incremented with twice the step.
            internal_timestamp = internal_timestamp.wrapping_add(2 * K_STEP);
            external_timestamp = external_timestamp.wrapping_add(K_STEP);
        }
    }

    #[test]
    fn test_g722_with_cng() {
        const K_RTP_PAYLOAD_TYPE_G722: u8 = 17;
        const K_RTP_PAYLOAD_TYPE_CNG: u8 = 13;
        let mut db = db_with(&[
            (
                K_RTP_PAYLOAD_TYPE_G722,
                SdpAudioFormat::new("g722", 8000, 1),
            ),
            (K_RTP_PAYLOAD_TYPE_CNG, SdpAudioFormat::new("cn", 16000, 1)),
        ]);
        let mut scaler = TimestampScaler::new();
        let mut external_timestamp: u32 = 0xFFFF_FFFF - 5;
        let mut internal_timestamp = external_timestamp;
        let mut next_is_cng = false;
        while external_timestamp != 5 {
            // Alternate between G.722 and CNG every other packet.
            let payload_type = if next_is_cng {
                K_RTP_PAYLOAD_TYPE_CNG
            } else {
                K_RTP_PAYLOAD_TYPE_G722
            };
            next_is_cng = !next_is_cng;
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, payload_type, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            internal_timestamp = internal_timestamp.wrapping_add(2);
            external_timestamp = external_timestamp.wrapping_add(1);
        }
    }

    /// Make sure that `to_internal_packet` is wired up correctly.
    #[test]
    fn test_g722_packet() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("g722", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        let mut external_timestamp: u32 = 0xFFFF_FFFF - 5;
        let mut internal_timestamp = external_timestamp;
        let mut packet = Packet::new();
        packet.payload_type = K_RTP_PAYLOAD_TYPE;
        while external_timestamp != 5 {
            packet.timestamp = external_timestamp;
            scaler.to_internal_packet(&mut packet, &mut db);
            assert_eq!(internal_timestamp, packet.timestamp);
            internal_timestamp = internal_timestamp.wrapping_add(2);
            external_timestamp = external_timestamp.wrapping_add(1);
        }
    }

    /// Make sure that `to_internal_list` is wired up correctly.
    #[test]
    fn test_g722_packet_list() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("g722", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        let external_timestamp: u32 = 0xFFFF_FFFF - 5;
        let internal_timestamp = external_timestamp;
        let mut packet_list = PacketList::new();
        let mut packet1 = Packet::new();
        packet1.payload_type = K_RTP_PAYLOAD_TYPE;
        packet1.timestamp = external_timestamp;
        let mut packet2 = Packet::new();
        packet2.payload_type = K_RTP_PAYLOAD_TYPE;
        packet2.timestamp = external_timestamp.wrapping_add(10);
        packet_list.push_back(packet1);
        packet_list.push_back(packet2);

        scaler.to_internal_list(&mut packet_list, &mut db);
        assert_eq!(internal_timestamp, packet_list[0].timestamp);
        assert_eq!(
            internal_timestamp.wrapping_add(20),
            packet_list[1].timestamp
        );
    }

    #[test]
    fn test_g722_reset() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("g722", 8000, 1))]);
        let mut scaler = TimestampScaler::new();
        let mut external_timestamp: u32 = 0xFFFF_FFFF - 5;
        let mut internal_timestamp = external_timestamp;
        while external_timestamp != 5 {
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            internal_timestamp = internal_timestamp.wrapping_add(2);
            external_timestamp = external_timestamp.wrapping_add(1);
        }
        // Reset the scaler. After this, we expect the internal and external to
        // start over at the same value again.
        scaler.reset();
        internal_timestamp = external_timestamp;
        while external_timestamp != 15 {
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            internal_timestamp = internal_timestamp.wrapping_add(2);
            external_timestamp = external_timestamp.wrapping_add(1);
        }
    }

    /// Opus does not need a timestamp scaler; a trivial-scaler case.
    #[test]
    fn test_opus_large_step() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        let mut db = db_with(&[(K_RTP_PAYLOAD_TYPE, SdpAudioFormat::new("opus", 48000, 2))]);
        let mut scaler = TimestampScaler::new();
        const K_STEP: u32 = 960;
        let mut external_timestamp = 0u32.wrapping_sub(5 * K_STEP);
        let mut internal_timestamp = external_timestamp;
        while external_timestamp != 5 * K_STEP {
            assert_eq!(
                internal_timestamp,
                scaler.to_internal(external_timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
            );
            assert_eq!(external_timestamp, scaler.to_external(internal_timestamp));
            internal_timestamp = internal_timestamp.wrapping_add(K_STEP);
            external_timestamp = external_timestamp.wrapping_add(K_STEP);
        }
    }

    #[test]
    fn failures() {
        const K_RTP_PAYLOAD_TYPE: u8 = 17;
        // Unknown payload type: no scaling.
        let mut db = db_with(&[]);
        let mut scaler = TimestampScaler::new();
        let timestamp = 4711u32; // Some number.
        assert_eq!(
            timestamp,
            scaler.to_internal(timestamp, K_RTP_PAYLOAD_TYPE, &mut db)
        );
    }
}
