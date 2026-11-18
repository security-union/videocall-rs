// Port target: WebRTC modules/audio_coding/neteq/decoder_database.{h,cc} at
// rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: decoder_database_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// C++ creates decoders lazily from `const` methods through `mutable` fields.
// Here lazy creation takes `&mut self` (`get_decoder`, `sample_rate_hz`,
// `get_active_decoder`, `get_active_cng_decoder`); `DecoderInfo::decoder()`
// is the non-creating `&self` accessor. `Environment` and
// `AudioCodecPairId` are dropped.

//! Payload type to decoder mapping, with lazy decoder creation.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::audio_decoder::{AudioDecoder, AudioDecoderFactory};
use crate::api::audio_format::SdpAudioFormat;
use crate::codecs::cng::ComfortNoiseDecoder;
use crate::neteq::packet::PacketList;
use crate::neteq::packet_buffer::ComfortNoiseLookup;

/// C++ `DecoderInfo::Subtype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subtype {
    Normal,
    ComfortNoise,
    Dtmf,
    Red,
}

/// C++ `DecoderDatabase::DecoderInfo`.
pub struct DecoderInfo {
    audio_format: SdpAudioFormat,
    factory: Arc<dyn AudioDecoderFactory>,
    decoder: Option<Box<dyn AudioDecoder>>,
    /// `Some(sample_rate_hz)` for CN formats (C++ `CngDecoder`).
    cng_decoder_sample_rate_hz: Option<i32>,
    subtype: Subtype,
}

impl std::fmt::Debug for DecoderInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderInfo")
            .field("audio_format", &self.audio_format)
            .field("has_decoder", &self.decoder.is_some())
            .field(
                "cng_decoder_sample_rate_hz",
                &self.cng_decoder_sample_rate_hz,
            )
            .field("subtype", &self.subtype)
            .finish()
    }
}

impl DecoderInfo {
    /// Info for `audio_format`; the subtype comes from the SDP name
    /// (`CN`, `telephone-event`, `red`, else normal).
    pub fn new(audio_format: SdpAudioFormat, factory: Arc<dyn AudioDecoderFactory>) -> Self {
        let cng_decoder_sample_rate_hz = if audio_format.name.eq_ignore_ascii_case("CN") {
            // CN has a 1:1 RTP clock rate to sample rate ratio.
            let sample_rate_hz = audio_format.clockrate_hz;
            debug_assert!(matches!(sample_rate_hz, 8000 | 16000 | 32000 | 48000));
            Some(sample_rate_hz)
        } else {
            None
        };
        let subtype = Self::subtype_from_format(&audio_format);
        Self {
            audio_format,
            factory,
            decoder: None,
            cng_decoder_sample_rate_hz,
            subtype,
        }
    }

    /// The decoder, created through the factory on first use. `None` for
    /// CN / DTMF / RED or if the factory cannot make one.
    pub fn get_decoder(&mut self) -> Option<&mut dyn AudioDecoder> {
        if self.subtype != Subtype::Normal {
            // These are handled internally, so they have no AudioDecoder
            // objects.
            return None;
        }
        if self.decoder.is_none() {
            self.decoder = self.factory.make_decoder(&self.audio_format);
        }
        debug_assert!(
            self.decoder.is_some(),
            "Failed to create: {:?}",
            self.audio_format
        );
        match self.decoder.as_deref_mut() {
            Some(decoder) => Some(decoder),
            None => None,
        }
    }

    /// The decoder if it already exists. Never creates one.
    pub fn decoder(&self) -> Option<&dyn AudioDecoder> {
        self.decoder
            .as_deref()
            .map(|decoder| decoder as &dyn AudioDecoder)
    }

    /// Drops the decoder; the next `get_decoder` recreates it.
    pub fn drop_decoder(&mut self) {
        self.decoder = None;
    }

    /// Sample rate: the clock rate for DTMF, the CN rate for comfort noise,
    /// else the decoder's (creating it if needed).
    pub fn sample_rate_hz(&mut self) -> i32 {
        if self.is_dtmf() {
            // DTMF has a 1:1 mapping between clock rate and sample rate.
            return self.audio_format.clockrate_hz;
        }
        let cng_sample_rate_hz = self.cng_decoder_sample_rate_hz;
        let decoder_sample_rate_hz = self.get_decoder().map(|decoder| decoder.sample_rate_hz());
        debug_assert_eq!(
            decoder_sample_rate_hz.is_some() as i32 + cng_sample_rate_hz.is_some() as i32,
            1
        );
        decoder_sample_rate_hz
            .or(cng_sample_rate_hz)
            // C++ dereferences an empty optional here (RED, or a format the
            // factory cannot make). Fall back to the clock rate instead.
            .unwrap_or(self.audio_format.clockrate_hz)
    }

    /// The SDP format.
    pub fn get_format(&self) -> &SdpAudioFormat {
        &self.audio_format
    }

    /// True for comfort noise.
    pub fn is_comfort_noise(&self) -> bool {
        debug_assert_eq!(
            self.cng_decoder_sample_rate_hz.is_some(),
            self.subtype == Subtype::ComfortNoise
        );
        self.subtype == Subtype::ComfortNoise
    }

    /// True for DTMF (`telephone-event`).
    pub fn is_dtmf(&self) -> bool {
        self.subtype == Subtype::Dtmf
    }

    /// True for RED.
    pub fn is_red(&self) -> bool {
        self.subtype == Subtype::Red
    }

    /// True if the SDP name equals `name`, ignoring ASCII case.
    pub fn is_type(&self, name: &str) -> bool {
        self.audio_format.name.eq_ignore_ascii_case(name)
    }

    /// The SDP name.
    pub fn get_name(&self) -> &str {
        &self.audio_format.name
    }

    /// C++ `SubtypeFromFormat`.
    fn subtype_from_format(format: &SdpAudioFormat) -> Subtype {
        if format.name.eq_ignore_ascii_case("CN") {
            Subtype::ComfortNoise
        } else if format.name.eq_ignore_ascii_case("telephone-event") {
            Subtype::Dtmf
        } else if format.name.eq_ignore_ascii_case("red") {
            Subtype::Red
        } else {
            Subtype::Normal
        }
    }
}

/// C++ `DecoderDatabase`.
pub struct DecoderDatabase {
    decoders: BTreeMap<u8, DecoderInfo>,
    active_decoder_type: i32,
    active_cng_decoder_type: i32,
    active_cng_decoder: Option<ComfortNoiseDecoder>,
    decoder_factory: Arc<dyn AudioDecoderFactory>,
}

impl std::fmt::Debug for DecoderDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecoderDatabase")
            .field("decoders", &self.decoders)
            .field("active_decoder_type", &self.active_decoder_type)
            .field("active_cng_decoder_type", &self.active_cng_decoder_type)
            .finish()
    }
}

impl DecoderDatabase {
    /// C++ `kOK`.
    pub const K_OK: i32 = 0;
    /// C++ `kInvalidRtpPayloadType`.
    pub const K_INVALID_RTP_PAYLOAD_TYPE: i32 = -1;
    /// C++ `kCodecNotSupported`.
    pub const K_CODEC_NOT_SUPPORTED: i32 = -2;
    /// C++ `kInvalidSampleRate`.
    pub const K_INVALID_SAMPLE_RATE: i32 = -3;
    /// C++ `kDecoderExists`.
    pub const K_DECODER_EXISTS: i32 = -4;
    /// C++ `kDecoderNotFound`.
    pub const K_DECODER_NOT_FOUND: i32 = -5;
    /// C++ `kInvalidPointer`.
    pub const K_INVALID_POINTER: i32 = -6;
    /// C++ `kRtpPayloadTypeError`.
    pub const K_RTP_PAYLOAD_TYPE_ERROR: u8 = 0xFF;

    /// Empty database using `decoder_factory`.
    pub fn new(decoder_factory: Arc<dyn AudioDecoderFactory>) -> Self {
        Self {
            decoders: BTreeMap::new(),
            active_decoder_type: -1,
            active_cng_decoder_type: -1,
            active_cng_decoder: None,
            decoder_factory,
        }
    }

    /// True if no payload types are registered.
    pub fn empty(&self) -> bool {
        self.decoders.is_empty()
    }

    /// Number of registered payload types.
    pub fn size(&self) -> i32 {
        self.decoders.len() as i32
    }

    /// Replaces the registered codecs with `codecs`. Returns the payload
    /// types that were removed or changed.
    pub fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>) -> Vec<i32> {
        // First collect all payload types that we'll remove or reassign, then
        // remove them from the database.
        let mut changed_payload_types = Vec::new();
        for (&payload_type, info) in &self.decoders {
            match codecs.get(&(payload_type as i32)) {
                Some(format) if format == info.get_format() => {}
                _ => changed_payload_types.push(payload_type as i32),
            }
        }
        for &pl_type in &changed_payload_types {
            self.remove(pl_type as u8);
        }

        // Enter the new and changed payload type mappings into the database.
        for (&rtp_payload_type, audio_format) in codecs {
            debug_assert!(rtp_payload_type >= 0);
            debug_assert!(rtp_payload_type <= 0x7f);
            let rtp_payload_type = rtp_payload_type as u8;
            if !self.decoders.contains_key(&rtp_payload_type) {
                self.decoders.insert(
                    rtp_payload_type,
                    DecoderInfo::new(audio_format.clone(), Arc::clone(&self.decoder_factory)),
                );
            } else {
                // The mapping for this payload type hasn't changed.
            }
        }

        changed_payload_types
    }

    /// Registers `rtp_payload_type` for `audio_format`. Returns a `K_*` code.
    pub fn register_payload(
        &mut self,
        rtp_payload_type: i32,
        audio_format: &SdpAudioFormat,
    ) -> i32 {
        if !(0..=0x7f).contains(&rtp_payload_type) {
            return Self::K_INVALID_RTP_PAYLOAD_TYPE;
        }
        match self.decoders.entry(rtp_payload_type as u8) {
            Entry::Occupied(_) => {
                // Database already contains a decoder with type
                // `rtp_payload_type`.
                Self::K_DECODER_EXISTS
            }
            Entry::Vacant(entry) => {
                entry.insert(DecoderInfo::new(
                    audio_format.clone(),
                    Arc::clone(&self.decoder_factory),
                ));
                Self::K_OK
            }
        }
    }

    /// Removes `rtp_payload_type`. Returns `K_OK` or `K_DECODER_NOT_FOUND`.
    pub fn remove(&mut self, rtp_payload_type: u8) -> i32 {
        if self.decoders.remove(&rtp_payload_type).is_none() {
            // No decoder with that `rtp_payload_type`.
            return Self::K_DECODER_NOT_FOUND;
        }
        if self.active_decoder_type == rtp_payload_type as i32 {
            self.active_decoder_type = -1; // No active decoder.
        }
        if self.active_cng_decoder_type == rtp_payload_type as i32 {
            self.active_cng_decoder_type = -1; // No active CNG decoder.
        }
        Self::K_OK
    }

    /// Removes all payload types.
    pub fn remove_all(&mut self) {
        self.decoders.clear();
        self.active_decoder_type = -1; // No active decoder.
        self.active_cng_decoder_type = -1; // No active CNG decoder.
    }

    /// Info for `rtp_payload_type`, if registered.
    pub fn get_decoder_info(&self, rtp_payload_type: u8) -> Option<&DecoderInfo> {
        self.decoders.get(&rtp_payload_type)
    }

    /// Mutable info for `rtp_payload_type`, if registered.
    pub fn get_decoder_info_mut(&mut self, rtp_payload_type: u8) -> Option<&mut DecoderInfo> {
        self.decoders.get_mut(&rtp_payload_type)
    }

    /// Makes `rtp_payload_type` the active decoder. Sets `new_decoder` to
    /// true if it changed. Returns a `K_*` code.
    pub fn set_active_decoder(&mut self, rtp_payload_type: u8, new_decoder: &mut bool) -> i32 {
        // Check that `rtp_payload_type` exists in the database.
        let Some(info) = self.get_decoder_info(rtp_payload_type) else {
            // Decoder not found.
            return Self::K_DECODER_NOT_FOUND;
        };
        assert!(!info.is_comfort_noise());
        *new_decoder = false;
        if self.active_decoder_type < 0 {
            // This is the first active decoder.
            *new_decoder = true;
        } else if self.active_decoder_type != rtp_payload_type as i32 {
            // Moving from one active decoder to another. Delete the first one.
            let old_info = self.decoders.get_mut(&(self.active_decoder_type as u8));
            debug_assert!(old_info.is_some());
            if let Some(old_info) = old_info {
                old_info.drop_decoder();
            }
            *new_decoder = true;
        }
        self.active_decoder_type = rtp_payload_type as i32;
        Self::K_OK
    }

    /// The active decoder, if any.
    pub fn get_active_decoder(&mut self) -> Option<&mut dyn AudioDecoder> {
        if self.active_decoder_type < 0 {
            // No active decoder.
            return None;
        }
        self.get_decoder(self.active_decoder_type as u8)
    }

    /// Makes `rtp_payload_type` the active CNG decoder. Returns a `K_*` code.
    pub fn set_active_cng_decoder(&mut self, rtp_payload_type: u8) -> i32 {
        // Check that `rtp_payload_type` exists in the database.
        if self.get_decoder_info(rtp_payload_type).is_none() {
            // Decoder not found.
            return Self::K_DECODER_NOT_FOUND;
        }
        if self.active_cng_decoder_type >= 0
            && self.active_cng_decoder_type != rtp_payload_type as i32
        {
            // Moving from one active CNG decoder to another. Delete the first
            // one.
            debug_assert!(self.active_cng_decoder.is_some());
            self.active_cng_decoder = None;
        }
        self.active_cng_decoder_type = rtp_payload_type as i32;
        Self::K_OK
    }

    /// The active CNG decoder, created on first use.
    pub fn get_active_cng_decoder(&mut self) -> Option<&mut ComfortNoiseDecoder> {
        if self.active_cng_decoder_type < 0 {
            // No active CNG decoder.
            return None;
        }
        Some(
            self.active_cng_decoder
                .get_or_insert_with(ComfortNoiseDecoder::new),
        )
    }

    /// The decoder for `rtp_payload_type`, created on first use.
    pub fn get_decoder(&mut self, rtp_payload_type: u8) -> Option<&mut dyn AudioDecoder> {
        self.decoders
            .get_mut(&rtp_payload_type)
            .and_then(DecoderInfo::get_decoder)
    }

    /// True if `rtp_payload_type` is comfort noise.
    pub fn is_comfort_noise(&self, rtp_payload_type: u8) -> bool {
        self.get_decoder_info(rtp_payload_type)
            .is_some_and(DecoderInfo::is_comfort_noise)
    }

    /// True if `rtp_payload_type` is DTMF.
    pub fn is_dtmf(&self, rtp_payload_type: u8) -> bool {
        self.get_decoder_info(rtp_payload_type)
            .is_some_and(DecoderInfo::is_dtmf)
    }

    /// True if `rtp_payload_type` is RED.
    pub fn is_red(&self, rtp_payload_type: u8) -> bool {
        self.get_decoder_info(rtp_payload_type)
            .is_some_and(DecoderInfo::is_red)
    }

    /// Returns `K_OK` if every packet's payload type is registered, else
    /// `K_DECODER_NOT_FOUND`.
    pub fn check_payload_types(&self, packet_list: &PacketList) -> i32 {
        for packet in packet_list {
            if self.get_decoder_info(packet.payload_type).is_none() {
                // Payload type is not found.
                log::warn!(
                    "CheckPayloadTypes: unknown RTP payload type {}",
                    packet.payload_type
                );
                return Self::K_DECODER_NOT_FOUND;
            }
        }
        Self::K_OK
    }
}

impl ComfortNoiseLookup for DecoderDatabase {
    fn is_comfort_noise(&self, rtp_payload_type: u8) -> bool {
        DecoderDatabase::is_comfort_noise(self, rtp_payload_type)
    }
}

#[cfg(test)]
mod tests {
    //! Port of decoder_database_unittest.cc.

    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::api::audio_decoder::SpeechType;
    use crate::codecs::BuiltinAudioDecoderFactory;
    use crate::neteq::packet::Packet;

    /// Minimal decoder standing in for C++ `MockAudioDecoder`.
    struct FakeDecoder {
        sample_rate_hz: i32,
    }

    impl AudioDecoder for FakeDecoder {
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

    /// C++ `MockAudioDecoderFactory`: records the formats it was asked for
    /// and makes a [`FakeDecoder`] at the format's clock rate.
    #[derive(Default)]
    struct MockAudioDecoderFactory {
        created: AtomicUsize,
        names: std::sync::Mutex<Vec<String>>,
    }

    impl AudioDecoderFactory for MockAudioDecoderFactory {
        fn make_decoder(&self, format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
            self.created.fetch_add(1, Ordering::SeqCst);
            self.names.lock().unwrap().push(format.name.clone());
            Some(Box::new(FakeDecoder {
                sample_rate_hz: format.clockrate_hz,
            }))
        }
    }

    fn mock_db() -> DecoderDatabase {
        DecoderDatabase::new(Arc::new(MockAudioDecoderFactory::default()))
    }

    #[test]
    fn create_and_destroy() {
        let db = mock_db();
        assert_eq!(0, db.size());
        assert!(db.empty());
    }

    #[test]
    fn insert_and_remove() {
        let mut db = mock_db();
        const K_PAYLOAD_TYPE: u8 = 0;
        let codec_name = "Robert'); DROP TABLE Students;";
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(
                K_PAYLOAD_TYPE as i32,
                &SdpAudioFormat::new(codec_name, 8000, 1)
            )
        );
        assert_eq!(1, db.size());
        assert!(!db.empty());
        assert_eq!(DecoderDatabase::K_OK, db.remove(K_PAYLOAD_TYPE));
        assert_eq!(0, db.size());
        assert!(db.empty());
    }

    #[test]
    fn insert_and_remove_all() {
        let mut db = mock_db();
        let codec_name1 = "Robert'); DROP TABLE Students;";
        let codec_name2 = "https://xkcd.com/327/";
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(0, &SdpAudioFormat::new(codec_name1, 8000, 1))
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(1, &SdpAudioFormat::new(codec_name2, 8000, 1))
        );
        assert_eq!(2, db.size());
        assert!(!db.empty());
        db.remove_all();
        assert_eq!(0, db.size());
        assert!(db.empty());
    }

    #[test]
    fn get_decoder_info() {
        let factory = Arc::new(MockAudioDecoderFactory::default());
        let mut db = DecoderDatabase::new(factory.clone());
        const K_PAYLOAD_TYPE: u8 = 0;
        let codec_name = "pcmu";
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(
                K_PAYLOAD_TYPE as i32,
                &SdpAudioFormat::new(codec_name, 8000, 1)
            )
        );
        let info = db.get_decoder_info(K_PAYLOAD_TYPE).expect("registered");
        assert!(info.is_type("pcmu"));
        assert!(info.is_type("PCMU"));
        assert_eq!(codec_name, info.get_name());
        assert!(info.decoder().is_none());
        assert!(db.get_decoder(K_PAYLOAD_TYPE).is_some());
        // Created exactly once, for "pcmu"; the second lookup reuses it.
        assert!(db.get_decoder(K_PAYLOAD_TYPE).is_some());
        assert_eq!(1, factory.created.load(Ordering::SeqCst));
        assert_eq!(vec!["pcmu".to_string()], *factory.names.lock().unwrap());
        assert!(db.get_decoder_info(K_PAYLOAD_TYPE + 1).is_none()); // Other payload type.
    }

    #[test]
    fn get_decoder() {
        let mut db = DecoderDatabase::new(Arc::new(BuiltinAudioDecoderFactory::new()));
        const K_PAYLOAD_TYPE: u8 = 0;
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(K_PAYLOAD_TYPE as i32, &SdpAudioFormat::new("l16", 8000, 1))
        );
        let dec = db.get_decoder(K_PAYLOAD_TYPE).expect("decoder");
        assert_eq!(8000, dec.sample_rate_hz());
    }

    #[test]
    fn type_tests() {
        let mut db = mock_db();
        const K_PAYLOAD_TYPE_PCMU: u8 = 0;
        const K_PAYLOAD_TYPE_CNG: u8 = 13;
        const K_PAYLOAD_TYPE_DTMF: u8 = 100;
        const K_PAYLOAD_TYPE_RED: u8 = 101;
        const K_PAYLOAD_NOT_USED: u8 = 102;
        // Load into database.
        let register = |db: &mut DecoderDatabase, pt: u8, name: &str| {
            db.register_payload(pt as i32, &SdpAudioFormat::new(name, 8000, 1))
        };
        assert_eq!(
            DecoderDatabase::K_OK,
            register(&mut db, K_PAYLOAD_TYPE_PCMU, "pcmu")
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            register(&mut db, K_PAYLOAD_TYPE_CNG, "cn")
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            register(&mut db, K_PAYLOAD_TYPE_DTMF, "telephone-event")
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            register(&mut db, K_PAYLOAD_TYPE_RED, "red")
        );
        assert_eq!(4, db.size());
        // Test.
        assert!(!db.is_comfort_noise(K_PAYLOAD_NOT_USED));
        assert!(!db.is_dtmf(K_PAYLOAD_NOT_USED));
        assert!(!db.is_red(K_PAYLOAD_NOT_USED));
        assert!(!db.is_comfort_noise(K_PAYLOAD_TYPE_PCMU));
        assert!(!db.is_dtmf(K_PAYLOAD_TYPE_PCMU));
        assert!(!db.is_red(K_PAYLOAD_TYPE_PCMU));
        assert!(db.is_comfort_noise(K_PAYLOAD_TYPE_CNG));
        assert!(db.is_dtmf(K_PAYLOAD_TYPE_DTMF));
        assert!(db.is_red(K_PAYLOAD_TYPE_RED));
        // CN, DTMF and RED never get an AudioDecoder.
        assert!(db.get_decoder(K_PAYLOAD_TYPE_CNG).is_none());
        assert!(db.get_decoder(K_PAYLOAD_TYPE_DTMF).is_none());
        assert!(db.get_decoder(K_PAYLOAD_TYPE_RED).is_none());
    }

    #[test]
    fn check_payload_types() {
        const K_NUM_PAYLOADS: u8 = 10;
        let mut db = mock_db();
        // Load a number of payloads into the database. Payload types are 0,
        // 1, ..., while the decoder type is the same for all payload types
        // (this does not matter for the test).
        for payload_type in 0..K_NUM_PAYLOADS {
            assert_eq!(
                DecoderDatabase::K_OK,
                db.register_payload(payload_type as i32, &SdpAudioFormat::new("pcmu", 8000, 1))
            );
        }
        let mut packet_list = PacketList::new();
        for i in 0..K_NUM_PAYLOADS + 1 {
            // Create packet with payload type `i`. The last packet will have a
            // payload type that is not registered in the decoder database.
            let mut packet = Packet::new();
            packet.payload_type = i;
            packet_list.push_back(packet);
        }

        // Expect to return false, since the last packet is of an unknown type.
        assert_eq!(
            DecoderDatabase::K_DECODER_NOT_FOUND,
            db.check_payload_types(&packet_list)
        );

        packet_list.pop_back(); // Remove the unknown one.

        assert_eq!(DecoderDatabase::K_OK, db.check_payload_types(&packet_list));
    }

    /// C++ `ActiveDecoders` (disabled there without iSAC); here with two
    /// L16 rates from the builtin factory.
    #[test]
    fn active_decoders() {
        let mut db = DecoderDatabase::new(Arc::new(BuiltinAudioDecoderFactory::new()));
        // Load payload types.
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(0, &SdpAudioFormat::new("L16", 8000, 1))
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(103, &SdpAudioFormat::new("L16", 16000, 1))
        );
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(13, &SdpAudioFormat::new("cn", 8000, 1))
        );
        // Verify that no decoders are active from the start.
        assert!(db.get_active_decoder().is_none());
        assert!(db.get_active_cng_decoder().is_none());

        // Set active speech codec.
        let mut changed = false; // Should be true when the active decoder changed.
        assert_eq!(
            DecoderDatabase::K_OK,
            db.set_active_decoder(0, &mut changed)
        );
        assert!(changed);
        assert_eq!(
            8000,
            db.get_active_decoder().expect("decoder").sample_rate_hz()
        );

        // Set the same again. Expect no change.
        assert_eq!(
            DecoderDatabase::K_OK,
            db.set_active_decoder(0, &mut changed)
        );
        assert!(!changed);
        assert!(db.get_active_decoder().is_some());

        // Change active decoder. The old one is dropped.
        assert_eq!(
            DecoderDatabase::K_OK,
            db.set_active_decoder(103, &mut changed)
        );
        assert!(changed);
        assert!(db.get_decoder_info(0).unwrap().decoder().is_none());
        assert_eq!(
            16000,
            db.get_active_decoder().expect("decoder").sample_rate_hz()
        );

        // Remove the active decoder, and verify that the active becomes NULL.
        assert_eq!(DecoderDatabase::K_OK, db.remove(103));
        assert!(db.get_active_decoder().is_none());

        // Try to set non-existing codecs as active.
        assert_eq!(
            DecoderDatabase::K_DECODER_NOT_FOUND,
            db.set_active_decoder(17, &mut changed)
        );
        assert_eq!(
            DecoderDatabase::K_DECODER_NOT_FOUND,
            db.set_active_cng_decoder(17)
        );

        // Set active CNG codec.
        assert_eq!(DecoderDatabase::K_OK, db.set_active_cng_decoder(13));

        // Remove the active CNG decoder, and verify that the active becomes
        // NULL.
        assert_eq!(DecoderDatabase::K_OK, db.remove(13));
        assert!(db.get_active_cng_decoder().is_none());
    }

    /// The CNG decoder half of C++ `ActiveDecoders`.
    #[test]
    fn active_cng_decoder_is_created_lazily() {
        let mut db = mock_db();
        assert_eq!(
            DecoderDatabase::K_OK,
            db.register_payload(13, &SdpAudioFormat::new("cn", 8000, 1))
        );
        assert_eq!(DecoderDatabase::K_OK, db.set_active_cng_decoder(13));
        assert!(db.get_active_cng_decoder().is_some());
        assert_eq!(DecoderDatabase::K_OK, db.remove(13));
        assert!(db.get_active_cng_decoder().is_none());
    }

    #[test]
    fn register_payload_rejects_bad_types_and_duplicates() {
        let mut db = mock_db();
        let format = SdpAudioFormat::new("L16", 8000, 1);
        assert_eq!(
            DecoderDatabase::K_INVALID_RTP_PAYLOAD_TYPE,
            db.register_payload(-1, &format)
        );
        assert_eq!(
            DecoderDatabase::K_INVALID_RTP_PAYLOAD_TYPE,
            db.register_payload(0x80, &format)
        );
        assert_eq!(DecoderDatabase::K_OK, db.register_payload(0x7f, &format));
        assert_eq!(
            DecoderDatabase::K_DECODER_EXISTS,
            db.register_payload(0x7f, &format)
        );
        assert_eq!(DecoderDatabase::K_DECODER_NOT_FOUND, db.remove(5));
    }

    #[test]
    fn set_codecs_reports_removed_and_changed_payload_types() {
        let mut db = mock_db();
        let l16 = SdpAudioFormat::new("L16", 8000, 1);
        let opus = SdpAudioFormat::new("opus", 48000, 2);
        let cn = SdpAudioFormat::new("CN", 8000, 1);
        let mut codecs = BTreeMap::new();
        codecs.insert(96, l16.clone());
        codecs.insert(111, opus.clone());
        assert!(db.set_codecs(&codecs).is_empty());
        assert_eq!(2, db.size());

        // 96 is kept, 111 changes format, 13 is new.
        let mut codecs = BTreeMap::new();
        codecs.insert(96, l16);
        codecs.insert(111, cn.clone());
        codecs.insert(13, cn);
        assert_eq!(vec![111], db.set_codecs(&codecs));
        assert_eq!(3, db.size());
        assert!(db.is_comfort_noise(111));

        // Removing everything reports every payload type, in order.
        assert_eq!(vec![13, 96, 111], db.set_codecs(&BTreeMap::new()));
        assert!(db.empty());
    }

    #[test]
    fn sample_rate_hz_by_subtype() {
        let mut db = mock_db();
        db.register_payload(0, &SdpAudioFormat::new("L16", 16000, 1));
        db.register_payload(13, &SdpAudioFormat::new("CN", 32000, 1));
        db.register_payload(100, &SdpAudioFormat::new("telephone-event", 48000, 1));
        assert_eq!(16000, db.get_decoder_info_mut(0).unwrap().sample_rate_hz());
        assert_eq!(32000, db.get_decoder_info_mut(13).unwrap().sample_rate_hz());
        assert_eq!(
            48000,
            db.get_decoder_info_mut(100).unwrap().sample_rate_hz()
        );
    }
}
