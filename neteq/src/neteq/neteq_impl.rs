// Port of WebRTC modules/audio_coding/neteq/neteq_impl.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: neteq_impl_unittest.cc,
// neteq_network_stats_unittest.cc, neteq_stereo_unittest.cc,
// neteq_decoder_plc_unittest.cc, neteq_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// `NetEqImpl` owns every component as a plain field and lends collaborators
// per call (PARITY_PLAN.md 3.2). Factories (`ExpandFactory`, ...) and
// `Environment` are dropped; the clock is injected via `Dependencies`.
// Method order follows neteq_impl.cc so the two can be read side by side.

//! The NetEq engine.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::audio_decoder::{AudioDecoderFactory, SpeechType, K_MAX_NUMBER_OF_CHANNELS};
use crate::api::audio_format::SdpAudioFormat;
use crate::api::audio_frame::{AudioFrame, OutputSpeechType};
use crate::api::clock::Clock;
use crate::api::config::Config;
use crate::api::neteq::{DecoderFormat, Mode, Operation};
use crate::api::neteq_controller::{
    NetEqController, NetEqControllerConfig, NetEqStatus, PacketArrivedInfo, PacketInfo,
};
use crate::api::rtp::{RtpHeader, RtpPacketInfo};
use crate::api::stats::{NetEqLifetimeStatistics, NetEqNetworkStatistics, NetEqOperationsAndState};
use crate::api::tick_timer::{Stopwatch, TickTimer};
use crate::neteq::accelerate::Accelerate;
use crate::neteq::audio_multi_vector::AudioMultiVector;
use crate::neteq::background_noise::BackgroundNoise;
use crate::neteq::comfort_noise::ComfortNoise;
use crate::neteq::decision_logic::DecisionLogic;
use crate::neteq::decoder_database::DecoderDatabase;
use crate::neteq::delay_manager::DelayManager;
use crate::neteq::dtmf_buffer::{DtmfBuffer, DtmfEvent};
use crate::neteq::dtmf_tone_generator::DtmfToneGenerator;
use crate::neteq::expand::{Expand, ExpandContext};
use crate::neteq::merge::Merge;
use crate::neteq::normal::Normal;
use crate::neteq::packet::{Packet, PacketList};
use crate::neteq::packet_buffer::PacketBuffer;
use crate::neteq::preemptive_expand::PreemptiveExpand;
use crate::neteq::random_vector::RandomVector;
use crate::neteq::red_payload_splitter::RedPayloadSplitter;
use crate::neteq::statistics_calculator::StatisticsCalculator;
use crate::neteq::sync_buffer::SyncBuffer;
use crate::neteq::time_stretch::TimeStretchReturnCode;
use crate::neteq::timestamp_scaler::TimestampScaler;

/// C++ `NetEqImpl::Error`.
pub use crate::error::NetEqImplError as Error;

/// C++ `NetEq::kOK`.
const K_OK: i32 = 0;
/// C++ `NetEq::kFail`.
const K_FAIL: i32 = -1;

/// C++ `NetEqImpl::OutputType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputType {
    NormalSpeech,
    Plc,
    Cng,
    PlcCng,
    VadPassive,
    CodecPlc,
}

/// C++ `ToSpeechType` (anonymous namespace).
fn to_speech_type(output_type: OutputType) -> OutputSpeechType {
    match output_type {
        OutputType::NormalSpeech => OutputSpeechType::NormalSpeech,
        OutputType::Cng => OutputSpeechType::Cng,
        OutputType::Plc => OutputSpeechType::Plc,
        OutputType::PlcCng => OutputSpeechType::PlcCng,
        OutputType::CodecPlc => OutputSpeechType::CodecPlc,
        OutputType::VadPassive => {
            debug_assert!(false, "unreachable output type");
            OutputSpeechType::Undefined
        }
    }
}

/// C++ `EqualSampleRates` (anonymous namespace): true if both payload types
/// are known and have the same sample rate.
fn equal_sample_rates(pt1: u8, pt2: u8, decoder_database: &mut DecoderDatabase) -> bool {
    let rate1 = decoder_database
        .get_decoder_info_mut(pt1)
        .map(|info| info.sample_rate_hz());
    let rate2 = decoder_database
        .get_decoder_info_mut(pt2)
        .map(|info| info.sample_rate_hz());
    matches!((rate1, rate2), (Some(a), Some(b)) if a == b)
}

/// Maps a `GetAudioInternal` return code to `Error`. Negative codes come
/// from components (DTMF, comfort noise) and map to `OtherError`.
fn error_from_code(code: i32) -> Error {
    const ALL: [Error; 17] = [
        Error::NoError,
        Error::OtherError,
        Error::UnknownRtpPayloadType,
        Error::DecoderNotFound,
        Error::InvalidPointer,
        Error::AccelerateError,
        Error::PreemptiveExpandError,
        Error::ComfortNoiseErrorCode,
        Error::DecoderErrorCode,
        Error::OtherDecoderError,
        Error::InvalidOperation,
        Error::DtmfParsingError,
        Error::DtmfInsertError,
        Error::SampleUnderrun,
        Error::DecodedTooMuch,
        Error::RedundancySplitError,
        Error::PacketBufferCorruption,
    ];
    ALL.into_iter()
        .find(|error| error.code() == code)
        .unwrap_or(Error::OtherError)
}

/// C++ `SampleRateToDefaultChannelSize`.
fn sample_rate_to_default_channel_size(fs_hz: i32) -> usize {
    (fs_hz / 100) as usize
}

/// Builds the controller from the config NetEq computes (C++
/// `NetEqControllerFactory::CreateNetEqController`). The config carries the
/// shared `TickTimer`.
pub type ControllerFactory =
    Box<dyn FnOnce(NetEqControllerConfig) -> Box<dyn NetEqController> + Send>;

/// C++ `NetEqImpl::Dependencies`: every component `NetEqImpl` owns from the
/// start. Tests build this directly to inject fakes.
pub struct Dependencies {
    pub tick_timer: TickTimer,
    pub clock: Arc<dyn Clock>,
    pub stats: StatisticsCalculator,
    pub decoder_database: DecoderDatabase,
    pub dtmf_buffer: DtmfBuffer,
    pub dtmf_tone_generator: DtmfToneGenerator,
    pub packet_buffer: PacketBuffer,
    pub neteq_controller: Box<dyn NetEqController>,
    pub red_payload_splitter: RedPayloadSplitter,
    pub timestamp_scaler: TimestampScaler,
}

impl Dependencies {
    /// The production component set for `config`. `controller_factory`
    /// `None` means `DecisionLogic` with a `DelayManager` built from
    /// `config.delay_manager` (C++ `DefaultNetEqControllerFactory`).
    pub fn new(
        config: &Config,
        clock: Arc<dyn Clock>,
        decoder_factory: Arc<dyn AudioDecoderFactory>,
        controller_factory: Option<ControllerFactory>,
    ) -> Self {
        let tick_timer = TickTimer::new();
        // C++ fills only these four fields; `enable_rtx_handling` keeps its
        // default.
        let controller_config = NetEqControllerConfig {
            allow_time_stretching: !config.for_test_no_time_stretching,
            enable_rtx_handling: false,
            max_packets_in_buffer: config.max_packets_in_buffer as i32,
            base_min_delay_ms: config.min_delay_ms,
            tick_timer: tick_timer.clone(),
        };
        let neteq_controller: Box<dyn NetEqController> = match controller_factory {
            Some(factory) => factory(controller_config),
            None => {
                let delay_manager =
                    DelayManager::new(&config.delay_manager, controller_config.tick_timer.clone());
                Box::new(DecisionLogic::new(
                    controller_config,
                    Box::new(delay_manager),
                ))
            }
        };
        Self {
            stats: StatisticsCalculator::new(),
            decoder_database: DecoderDatabase::new(decoder_factory),
            dtmf_buffer: DtmfBuffer::new(config.sample_rate_hz),
            dtmf_tone_generator: DtmfToneGenerator::new(),
            packet_buffer: PacketBuffer::new(
                config.max_packets_in_buffer,
                tick_timer.clone(),
                config.smart_flushing,
            ),
            neteq_controller,
            red_payload_splitter: RedPayloadSplitter::new(),
            timestamp_scaler: TimestampScaler::new(),
            tick_timer,
            clock,
        }
    }
}

/// What the controller saw and said on the last `get_decision` (parity
/// record only).
#[cfg(feature = "parity")]
#[derive(Clone, Copy)]
struct DecisionRecord {
    status: NetEqStatus,
    raw: Operation,
}

/// C++ `NetEqImpl`.
pub struct NetEqImpl {
    clock: Arc<dyn Clock>,
    tick_timer: TickTimer,
    decoder_database: DecoderDatabase,
    dtmf_buffer: DtmfBuffer,
    dtmf_tone_generator: DtmfToneGenerator,
    packet_buffer: PacketBuffer,
    red_payload_splitter: RedPayloadSplitter,
    timestamp_scaler: TimestampScaler,
    stats: StatisticsCalculator,

    background_noise: BackgroundNoise,
    controller: Box<dyn NetEqController>,
    algorithm_buffer: AudioMultiVector,
    sync_buffer: SyncBuffer,
    expand: Expand,
    normal: Normal,
    merge: Merge,
    accelerate: Accelerate,
    preemptive_expand: PreemptiveExpand,
    random_vector: RandomVector,
    comfort_noise: ComfortNoise,
    fs_hz: i32,
    fs_mult: i32,
    last_output_sample_rate_hz: i32,
    output_size_samples: usize,
    decoder_frame_length: usize,
    last_mode: Mode,
    last_operation: Operation,
    last_decoded_type: Option<SpeechType>,
    decoded_buffer_length: usize,
    decoded_buffer: Vec<i16>,
    playout_timestamp: u32,
    new_codec: bool,
    timestamp: u32,
    reset_decoder: bool,
    current_rtp_payload_type: Option<u8>,
    current_cng_rtp_payload_type: Option<u8>,
    first_packet: bool,
    enable_fast_accelerate: bool,
    enable_muted_state: bool,
    generated_noise_stopwatch: Option<Stopwatch>,
    last_decoded_packet_infos: Vec<RtpPacketInfo>,
    /// Not in C++ (a local in `GetAudioInternal`): kept between calls so a
    /// tick reuses its capacity instead of allocating. Always empty between
    /// calls.
    packet_list: PacketList,
    /// Only used for test.
    #[allow(dead_code)]
    no_time_stretching: bool,
    concealment_audio: Vec<i16>,
    /// Not in C++: the internal error of the last failed `get_audio`, so the
    /// Rust facade can report why it returned `kFail`.
    last_get_audio_error: Error,

    #[cfg(feature = "parity")]
    observer: Option<Box<dyn crate::parity::Observer>>,
    #[cfg(feature = "parity")]
    last_decision: Option<DecisionRecord>,
}

impl NetEqImpl {
    /// C++ `kOutputSizeMs`.
    pub const K_OUTPUT_SIZE_MS: i32 = 10;
    /// 120 ms @ 48 kHz.
    pub const K_MAX_FRAME_SIZE: usize = 5760;
    /// C++ `kSyncBufferSize`. Current value is `kMaxFrameSize` + 60 ms *
    /// 48 kHz, which is enough for calculating correlations of current frame
    /// against history.
    pub const K_SYNC_BUFFER_SIZE: usize = Self::K_MAX_FRAME_SIZE + 60 * 48;

    /// C++ constructor.
    ///
    /// The per-rate DSP components are always built (C++ leaves them null
    /// when `create_components` is false and expects the test to call
    /// `SetSampleRateAndChannels`); with `create_components` false they are
    /// built for `config.sample_rate_hz` and one channel without the side
    /// effects of `SetSampleRateAndChannels` on the controller and stats.
    pub fn new(config: &Config, deps: Dependencies, create_components: bool) -> Self {
        let mut fs = config.sample_rate_hz;
        if fs != 8000 && fs != 16000 && fs != 32000 && fs != 48000 {
            log::error!("Sample rate {fs} Hz not supported. Changing to 8000 Hz.");
            fs = 8000;
        }
        let fs_mult = fs / 8000;
        let output_size_samples = (Self::K_OUTPUT_SIZE_MS * 8 * fs_mult) as usize;
        let channels = 1;
        let expand = Expand::new(fs, channels);
        let mut sync_buffer =
            SyncBuffer::new(channels, Self::K_SYNC_BUFFER_SIZE * fs_mult as usize);
        sync_buffer.set_next_index(sync_buffer.next_index() - expand.overlap_length());
        let preemptive_expand = PreemptiveExpand::new(fs, channels, expand.overlap_length());
        let mut neteq = Self {
            clock: deps.clock,
            tick_timer: deps.tick_timer,
            decoder_database: deps.decoder_database,
            dtmf_buffer: deps.dtmf_buffer,
            dtmf_tone_generator: deps.dtmf_tone_generator,
            packet_buffer: deps.packet_buffer,
            red_payload_splitter: deps.red_payload_splitter,
            timestamp_scaler: deps.timestamp_scaler,
            stats: deps.stats,
            background_noise: BackgroundNoise::new(channels),
            controller: deps.neteq_controller,
            algorithm_buffer: AudioMultiVector::new(channels),
            sync_buffer,
            merge: Merge::new(fs, channels),
            expand,
            normal: Normal::new(fs),
            accelerate: Accelerate::new(fs, channels),
            preemptive_expand,
            random_vector: RandomVector::new(),
            comfort_noise: ComfortNoise::new(fs),
            fs_hz: fs,
            fs_mult,
            last_output_sample_rate_hz: fs,
            output_size_samples,
            decoder_frame_length: 2 * output_size_samples, // 20 ms.
            last_mode: Mode::Normal,
            last_operation: Operation::Undefined,
            last_decoded_type: None,
            decoded_buffer_length: Self::K_MAX_FRAME_SIZE,
            decoded_buffer: vec![0; Self::K_MAX_FRAME_SIZE],
            playout_timestamp: 0,
            new_codec: false,
            timestamp: 0,
            reset_decoder: false,
            current_rtp_payload_type: None,
            current_cng_rtp_payload_type: None,
            first_packet: true,
            enable_fast_accelerate: config.enable_fast_accelerate,
            enable_muted_state: config.enable_muted_state,
            generated_noise_stopwatch: None,
            last_decoded_packet_infos: Vec::new(),
            packet_list: PacketList::new(),
            no_time_stretching: config.for_test_no_time_stretching,
            concealment_audio: Vec::new(),
            last_get_audio_error: Error::NoError,
            #[cfg(feature = "parity")]
            observer: None,
            #[cfg(feature = "parity")]
            last_decision: None,
        };
        neteq.controller.set_maximum_delay(config.max_delay_ms);
        neteq
            .controller
            .set_sample_rate(neteq.fs_hz, neteq.output_size_samples);
        if create_components {
            neteq.set_sample_rate_and_channels(fs, 1); // Default is 1 channel.
        }
        neteq
    }

    /// C++ `InsertPacket`. Returns 0 (`kOK`) or -1 (`kFail`).
    pub fn insert_packet(
        &mut self,
        rtp_header: &RtpHeader,
        payload: &[u8],
        packet_info: RtpPacketInfo,
    ) -> i32 {
        let return_code = self.insert_packet_internal(rtp_header, payload, packet_info);
        #[cfg(feature = "parity")]
        if let Some(observer) = self.observer.as_mut() {
            observer.on_insert(rtp_header, return_code);
        }
        return_code
    }

    fn insert_packet_internal(
        &mut self,
        rtp_header: &RtpHeader,
        payload: &[u8],
        packet_info: RtpPacketInfo,
    ) -> i32 {
        if payload.is_empty() {
            log::error!("InsertPacket: payload is empty");
            return K_FAIL;
        }
        self.stats.received_packet();

        let mut packet_list = PacketList::new();
        // Insert packet in a packet list. Waiting time will be set upon
        // inserting the packet in the buffer.
        packet_list.push_back(Packet {
            payload_type: rtp_header.payload_type,
            sequence_number: rtp_header.sequence_number,
            timestamp: rtp_header.timestamp,
            payload: payload.to_vec(),
            ..Packet::new()
        });

        let mut update_sample_rate_and_channels = self.first_packet;

        if update_sample_rate_and_channels {
            // Reset timestamp scaling.
            self.timestamp_scaler.reset();
        }

        if !self.decoder_database.is_red(rtp_header.payload_type) {
            // Scale timestamp to internal domain (only for some codecs).
            self.timestamp_scaler
                .to_internal_list(&mut packet_list, &mut self.decoder_database);
        }

        // Store these for later use, since the first packet may very well
        // disappear before we need these values.
        let mut main_timestamp = packet_list[0].timestamp;

        // Reinitialize NetEq if it's needed (changed SSRC or first call).
        if update_sample_rate_and_channels {
            // Note: `first_packet` will be cleared further down in this
            // method, once the packet has been successfully inserted into the
            // packet buffer.

            // Flush the packet buffer and DTMF buffer.
            self.packet_buffer.flush(&mut self.stats);
            self.dtmf_buffer.flush();

            // Update audio buffer timestamp.
            self.sync_buffer
                .increase_end_timestamp(main_timestamp.wrapping_sub(self.timestamp));

            // Update codecs.
            self.timestamp = main_timestamp;
        }

        // Check for RED payload type, and separate payloads into several
        // packets.
        if self.decoder_database.is_red(rtp_header.payload_type) {
            if !self.red_payload_splitter.split_red(&mut packet_list) {
                return K_FAIL;
            }
            // Only accept a few RED payloads of the same type as the main
            // data, DTMF events and CNG.
            self.red_payload_splitter
                .check_red_payloads(&mut packet_list, &self.decoder_database);
            if packet_list.is_empty() {
                return K_FAIL;
            }
        }

        // Check payload types.
        if self.decoder_database.check_payload_types(&packet_list)
            == DecoderDatabase::K_DECODER_NOT_FOUND
        {
            return K_FAIL;
        }

        debug_assert!(!packet_list.is_empty());

        // Update main_timestamp, if new packets appear in the list after RED
        // splitting.
        if self.decoder_database.is_red(rtp_header.payload_type) {
            self.timestamp_scaler
                .to_internal_list(&mut packet_list, &mut self.decoder_database);
            main_timestamp = packet_list[0].timestamp;
        }
        let _ = main_timestamp;

        // Process DTMF payloads. Cycle through the list of packets, and pick
        // out any DTMF payloads found.
        let mut it = 0usize;
        while it < packet_list.len() {
            let current_packet = &packet_list[it];
            debug_assert!(!current_packet.payload.is_empty());
            if self.decoder_database.is_dtmf(current_packet.payload_type) {
                let mut event = DtmfEvent::default();
                let ret = DtmfBuffer::parse_event(
                    current_packet.timestamp,
                    &current_packet.payload,
                    &mut event,
                );
                if ret != DtmfBuffer::K_OK {
                    return K_FAIL;
                }
                if self.dtmf_buffer.insert_event(&event) != DtmfBuffer::K_OK {
                    return K_FAIL;
                }
                packet_list.remove(it);
            } else {
                it += 1;
            }
        }

        let mut parsed_packet_list = PacketList::new();
        while let Some(mut packet) = packet_list.pop_front() {
            let Some(info) = self
                .decoder_database
                .get_decoder_info_mut(packet.payload_type)
            else {
                log::warn!("SplitAudio unknown payload type");
                return K_FAIL;
            };

            if info.is_comfort_noise() {
                // Carry comfort noise packets along.
                parsed_packet_list.push_back(packet);
            } else {
                let sequence_number = packet.sequence_number;
                let payload_type = packet.payload_type;
                let original_priority = packet.priority;
                let payload = std::mem::take(&mut packet.payload);
                let timestamp = packet.timestamp;
                let results = match info.get_decoder() {
                    Some(decoder) => decoder.parse_payload(payload, timestamp),
                    None => {
                        // C++ dereferences a null decoder here.
                        log::error!("No decoder for payload type {payload_type}");
                        return K_FAIL;
                    }
                };
                for result in results {
                    debug_assert!(result.priority >= 0);
                    let mut new_packet = Packet {
                        sequence_number,
                        payload_type,
                        timestamp: result.timestamp,
                        ..Packet::new()
                    };
                    new_packet.priority.codec_level = result.priority;
                    new_packet.priority.red_level = original_priority.red_level;
                    // Only associate the header information with the primary
                    // packet.
                    if new_packet.timestamp == packet_info.rtp_timestamp {
                        new_packet.packet_info = Some(packet_info);
                    }
                    new_packet.frame = Some(result.frame);
                    parsed_packet_list.push_back(new_packet);
                }
            }
        }

        // Calculate the number of primary (non-FEC/RED) packets.
        let number_of_primary_packets = parsed_packet_list
            .iter()
            .filter(|packet| packet.priority.codec_level == 0)
            .count();
        if number_of_primary_packets < parsed_packet_list.len() {
            self.stats
                .secondary_packets_received(parsed_packet_list.len() - number_of_primary_packets);
        }

        let target_level_ms = self.controller.target_level_ms();
        for packet in parsed_packet_list {
            let mut info = self.to_packet_arrived_info(&packet);
            if self.maybe_change_payload_type(packet.payload_type) {
                self.packet_buffer.flush(&mut self.stats);
                self.new_codec = true;
                update_sample_rate_and_channels = true;
                info.buffer_flush = true;
            }
            let return_val = self.packet_buffer.insert_packet(
                packet,
                self.decoder_frame_length,
                self.last_output_sample_rate_hz as usize,
                target_level_ms,
                &mut self.stats,
            );
            if return_val == PacketBuffer::K_FLUSHED {
                self.new_codec = true;
                update_sample_rate_and_channels = true;
                info.buffer_flush = true;
            } else if return_val == PacketBuffer::K_PARTIAL_FLUSH {
                // Forward sync buffer timestamp.
                if let Some(next_packet) = self.packet_buffer.peek_next_packet() {
                    self.timestamp = next_packet.timestamp;
                    let increment = self
                        .timestamp
                        .wrapping_sub(self.sync_buffer.end_timestamp());
                    self.sync_buffer.increase_end_timestamp(increment);
                }
                info.buffer_flush = true;
            } else if return_val != PacketBuffer::K_OK {
                // An error occurred.
                return K_FAIL;
            }

            let should_update_stats = !self.new_codec;
            let relative_delay =
                self.controller
                    .packet_arrived(self.fs_hz, should_update_stats, &info);
            if let Some(relative_delay) = relative_delay {
                // C++ converts the `int` to `size_t`.
                self.stats
                    .relative_packet_arrival_delay(relative_delay as isize as usize);
            }
        }

        if self.first_packet {
            self.first_packet = false;
            // Update the codec on the next GetAudio call.
            self.new_codec = true;
        }

        if let Some(current) = self.current_rtp_payload_type {
            debug_assert!(
                self.decoder_database.get_decoder_info(current).is_some(),
                "Payload type {current} is unknown where it shouldn't be"
            );
        }

        if update_sample_rate_and_channels && !self.packet_buffer.empty() {
            // We do not use `current_rtp_payload_type` to set `payload_type`,
            // but get the next RTP header from `packet_buffer` to obtain the
            // payload type. The reason for it is the following corner case.
            // If NetEq receives a CNG packet with a sample rate different than
            // the current CNG then it flushes its buffer, assuming send codec
            // must have been changed. However, payload type of the
            // hypothetically new send codec is not known.
            let payload_type = self
                .packet_buffer
                .peek_next_packet()
                .map(|packet| packet.payload_type)
                .expect("packet buffer is not empty");
            let mut channels = 1;
            if !self.decoder_database.is_comfort_noise(payload_type) {
                let decoder = self.decoder_database.get_decoder(payload_type);
                // Payloads are already checked to be valid.
                debug_assert!(decoder.is_some());
                if let Some(decoder) = decoder {
                    channels = decoder.channels();
                }
                debug_assert!(channels <= K_MAX_NUMBER_OF_CHANNELS);
            }
            let sample_rate_hz = self
                .decoder_database
                .get_decoder_info_mut(payload_type)
                .map(|info| info.sample_rate_hz());
            debug_assert!(sample_rate_hz.is_some());
            if let Some(sample_rate_hz) = sample_rate_hz {
                if sample_rate_hz != self.fs_hz || channels != self.algorithm_buffer.channels() {
                    self.set_sample_rate_and_channels(sample_rate_hz, channels);
                }
            }
        }

        K_OK
    }

    /// C++ `GetAudio`. Returns 0 (`kOK`) or -1 (`kFail`).
    pub fn get_audio(
        &mut self,
        audio_frame: &mut AudioFrame,
        muted: Option<&mut bool>,
        current_sample_rate_hz: Option<&mut i32>,
        action_override: Option<Operation>,
    ) -> i32 {
        let mut packet_list = std::mem::take(&mut self.packet_list);
        let error_code = self.get_audio_internal(audio_frame, action_override, &mut packet_list);
        // Drops what the local list in C++ would drop, keeping the capacity.
        packet_list.clear();
        self.packet_list = packet_list;
        if error_code != Error::NoError.code() {
            self.last_get_audio_error = error_from_code(error_code);
            #[cfg(feature = "parity")]
            self.notify_get_audio(audio_frame);
            return K_FAIL;
        }
        self.stats
            .increase_counter(self.output_size_samples, self.fs_hz);
        debug_assert_eq!(
            audio_frame.sample_rate_hz,
            (audio_frame.samples_per_channel * 100) as i32
        );
        if let Some(muted) = muted {
            *muted = audio_frame.muted();
        }
        audio_frame.speech_type = to_speech_type(self.last_output_type());
        self.last_output_sample_rate_hz = audio_frame.sample_rate_hz;
        debug_assert!(
            matches!(
                self.last_output_sample_rate_hz,
                8000 | 16000 | 32000 | 48000
            ),
            "Unexpected sample rate {}",
            self.last_output_sample_rate_hz
        );

        if let Some(current_sample_rate_hz) = current_sample_rate_hz {
            *current_sample_rate_hz = self.last_output_sample_rate_hz;
        }

        #[cfg(feature = "parity")]
        self.notify_get_audio(audio_frame);
        K_OK
    }

    /// Not in C++: why the last `get_audio` returned `kFail`.
    pub fn last_get_audio_error(&self) -> Error {
        self.last_get_audio_error
    }

    #[cfg(feature = "parity")]
    fn notify_get_audio(&mut self, audio_frame: &AudioFrame) {
        if let Some(observer) = self.observer.as_mut() {
            observer.on_get_audio(audio_frame, audio_frame.muted(), self.last_mode);
        }
    }

    /// C++ `SetCodecs`.
    pub fn set_codecs(&mut self, codecs: &BTreeMap<i32, SdpAudioFormat>) {
        let changed_payload_types = self.decoder_database.set_codecs(codecs);
        for pt in changed_payload_types {
            self.packet_buffer
                .discard_packets_with_payload_type(pt as u8, &mut self.stats);
        }
    }

    /// C++ `SetMaximumBufferPackets`.
    pub fn set_maximum_buffer_packets(&mut self, max_packets: usize) {
        self.packet_buffer
            .set_max_number_of_packets(max_packets, &mut self.stats);
    }

    /// C++ `SetFastAccelerate`.
    pub fn set_fast_accelerate(&mut self, enable: bool) {
        self.enable_fast_accelerate = enable;
    }

    /// C++ `RegisterPayloadType`.
    pub fn register_payload_type(
        &mut self,
        rtp_payload_type: i32,
        audio_format: &SdpAudioFormat,
    ) -> bool {
        log::trace!(
            "NetEqImpl::RegisterPayloadType: payload type {rtp_payload_type}, codec {audio_format:?}"
        );
        self.decoder_database
            .register_payload(rtp_payload_type, audio_format)
            == DecoderDatabase::K_OK
    }

    /// C++ `CreateDecoder`.
    pub fn create_decoder(&mut self, rtp_payload_type: i32) -> bool {
        u8::try_from(rtp_payload_type)
            .ok()
            .and_then(|pt| self.decoder_database.get_decoder(pt))
            .is_some()
    }

    /// C++ `RemovePayloadType`. Returns 0 or -1.
    pub fn remove_payload_type(&mut self, rtp_payload_type: u8) -> i32 {
        let ret = self.decoder_database.remove(rtp_payload_type);
        if ret == DecoderDatabase::K_OK || ret == DecoderDatabase::K_DECODER_NOT_FOUND {
            self.packet_buffer
                .discard_packets_with_payload_type(rtp_payload_type, &mut self.stats);
            return K_OK;
        }
        K_FAIL
    }

    /// C++ `RemoveAllPayloadTypes`.
    pub fn remove_all_payload_types(&mut self) {
        self.decoder_database.remove_all();
    }

    /// C++ `SetMinimumDelay`.
    pub fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        if (0..=10000).contains(&delay_ms) {
            return self.controller.set_minimum_delay(delay_ms);
        }
        false
    }

    /// C++ `SetMaximumDelay`.
    pub fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        if (0..=10000).contains(&delay_ms) {
            return self.controller.set_maximum_delay(delay_ms);
        }
        false
    }

    /// C++ `SetBaseMinimumDelayMs`.
    pub fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> bool {
        if (0..=10000).contains(&delay_ms) {
            return self.controller.set_base_minimum_delay(delay_ms);
        }
        false
    }

    /// C++ `GetBaseMinimumDelayMs`.
    pub fn get_base_minimum_delay_ms(&self) -> i32 {
        self.controller.get_base_minimum_delay()
    }

    /// C++ `TargetDelayMs`.
    pub fn target_delay_ms(&self) -> i32 {
        self.controller.target_level_ms()
    }

    /// C++ `FilteredCurrentDelayMs`.
    pub fn filtered_current_delay_ms(&self) -> i32 {
        // Sum up the filtered packet buffer level with the future length of
        // the sync buffer.
        let delay_samples = self
            .controller
            .get_filtered_buffer_level()
            .wrapping_add(self.sync_buffer.future_length() as i32);
        // The division below will truncate. The return value is in ms.
        delay_samples / (self.fs_hz / 1000)
    }

    /// C++ `NetworkStatistics`: fills `stats` and resets. Returns 0.
    pub fn network_statistics(&mut self, stats: &mut NetEqNetworkStatistics) -> i32 {
        *stats = self.current_network_statistics_internal();
        self.stats
            .get_network_statistics(self.decoder_frame_length, stats);
        0
    }

    /// C++ `CurrentNetworkStatistics`.
    pub fn current_network_statistics(&self) -> NetEqNetworkStatistics {
        self.current_network_statistics_internal()
    }

    fn current_network_statistics_internal(&self) -> NetEqNetworkStatistics {
        let total_samples_in_buffers = self
            .packet_buffer
            .num_samples_in_buffer(self.decoder_frame_length)
            + self.sync_buffer.future_length();

        debug_assert!(self.fs_hz > 0);
        NetEqNetworkStatistics {
            preferred_buffer_size_ms: self.controller.target_level_ms() as u16,
            jitter_peaks_found: self.controller.peak_found() as u16,
            current_buffer_size_ms: (total_samples_in_buffers * 1000 / self.fs_hz as usize) as u16,
            ..NetEqNetworkStatistics::default()
        }
    }

    /// C++ `GetLifetimeStatistics`.
    pub fn get_lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        self.stats.get_lifetime_statistics()
    }

    /// C++ `GetOperationsAndState`.
    pub fn get_operations_and_state(&self) -> NetEqOperationsAndState {
        let mut result = self.stats.get_operations_and_state();
        result.current_buffer_size_ms = ((self
            .packet_buffer
            .num_samples_in_buffer(self.decoder_frame_length)
            + self.sync_buffer.future_length())
            * 1000
            / self.fs_hz as usize) as u64;
        result.current_frame_size_ms =
            (self.decoder_frame_length * 1000 / self.fs_hz as usize) as u64;
        result.next_packet_available = self
            .packet_buffer
            .peek_next_packet()
            .is_some_and(|packet| packet.timestamp == self.sync_buffer.end_timestamp());
        result
    }

    /// C++ `GetPlayoutTimestamp`.
    pub fn get_playout_timestamp(&self) -> Option<u32> {
        if self.first_packet
            || self.last_mode == Mode::Rfc3389Cng
            || self.last_mode == Mode::CodecInternalCng
        {
            // We don't have a valid RTP timestamp until we have decoded our
            // first RTP packet. Also, the RTP timestamp is not accurate while
            // playing CNG, which is indicated by returning an empty value.
            return None;
        }
        Some(self.timestamp_scaler.to_external(self.playout_timestamp))
    }

    /// C++ `last_output_sample_rate_hz`.
    pub fn last_output_sample_rate_hz(&self) -> i32 {
        self.last_output_sample_rate_hz
    }

    /// C++ `GetCurrentDecoderFormat`. Takes `&self` like C++; the decoder's
    /// channel count is read only if the decoder already exists (C++ would
    /// create it here; it always exists once a packet of that type has been
    /// decoded).
    pub fn get_current_decoder_format(&self) -> Option<DecoderFormat> {
        let payload_type = self.current_rtp_payload_type?;
        let di = self.decoder_database.get_decoder_info(payload_type)?;
        let num_channels = di
            .decoder()
            .map(|decoder| decoder.channels() as i32)
            .unwrap_or(di.get_format().num_channels as i32);
        let sample_rate_hz = di
            .decoder()
            .map(|decoder| decoder.sample_rate_hz())
            .unwrap_or(di.get_format().clockrate_hz);
        Some(DecoderFormat {
            payload_type: payload_type as i32,
            sample_rate_hz,
            num_channels,
            sdp_format: di.get_format().clone(),
        })
    }

    /// C++ `FlushBuffers`: flushes both the packet buffer and the sync
    /// buffer.
    pub fn flush_buffers(&mut self) {
        log::trace!("FlushBuffers");
        self.packet_buffer.flush(&mut self.stats);
        self.sync_buffer.flush();
        let next_index = self.sync_buffer.next_index() - self.expand.overlap_length();
        self.sync_buffer.set_next_index(next_index);
        // Set to wait for new codec.
        self.first_packet = true;
    }

    /// C++ `SyncBufferSizeMs`.
    pub fn sync_buffer_size_ms(&self) -> i32 {
        (self.sync_buffer.future_length() / (self.fs_hz / 1000) as usize) as i32
    }

    /// C++ `sync_buffer_for_test`.
    pub fn sync_buffer_for_test(&self) -> &SyncBuffer {
        &self.sync_buffer
    }

    /// C++ `last_operation_for_test`.
    pub fn last_operation_for_test(&self) -> Operation {
        self.last_operation
    }

    /// C++ `last_mode_` (protected); exposed for the parity record.
    pub fn last_mode_for_test(&self) -> Mode {
        self.last_mode
    }

    /// Installs the parity observer.
    #[cfg(feature = "parity")]
    pub fn set_observer(&mut self, observer: Box<dyn crate::parity::Observer>) {
        self.observer = Some(observer);
    }

    // Methods below this line are private in C++.

    /// C++ `MaybeChangePayloadType`: true if the payload type changed (this
    /// should be followed by resetting various state).
    fn maybe_change_payload_type(&mut self, payload_type: u8) -> bool {
        let mut changed = false;
        if self.decoder_database.is_comfort_noise(payload_type) {
            if self
                .current_cng_rtp_payload_type
                .is_some_and(|current| current != payload_type)
            {
                // New CNG payload type implies new codec type.
                self.current_rtp_payload_type = None;
                changed = true;
            }
            self.current_cng_rtp_payload_type = Some(payload_type);
        } else if !self.decoder_database.is_dtmf(payload_type) {
            // This must be speech.
            let rtp_changed = self
                .current_rtp_payload_type
                .is_some_and(|current| current != payload_type);
            let cng_changed = rtp_changed
                || self.current_cng_rtp_payload_type.is_some_and(|cng| {
                    !equal_sample_rates(payload_type, cng, &mut self.decoder_database)
                });
            if cng_changed {
                self.current_cng_rtp_payload_type = None;
                changed = true;
            }
            self.current_rtp_payload_type = Some(payload_type);
        }
        changed
    }

    /// C++ `GetAudioInternal`: delivers 10 ms of audio into `audio_frame`.
    /// Returns 0 on success, otherwise an error code.
    fn get_audio_internal(
        &mut self,
        audio_frame: &mut AudioFrame,
        action_override: Option<Operation>,
        packet_list: &mut PacketList,
    ) -> i32 {
        debug_assert!(packet_list.is_empty());
        let mut dtmf_event = DtmfEvent::default();
        let mut operation = Operation::Undefined;
        let mut play_dtmf = false;
        self.last_decoded_packet_infos.clear();
        self.tick_timer.increment();

        // Sanity check - should already be taken care of when setting
        // output_size_samples.
        debug_assert!(
            self.output_size_samples * self.sync_buffer.channels()
                <= AudioFrame::MAX_DATA_SIZE_SAMPLES
        );

        // Check for muted state.
        if self.enable_muted_state && self.expand.muted() && self.packet_buffer.empty() {
            debug_assert_eq!(self.last_mode, Mode::Expand);
            audio_frame.reset();
            debug_assert!(audio_frame.muted()); // Reset() should mute the frame.
            self.playout_timestamp = self
                .playout_timestamp
                .wrapping_add(self.output_size_samples as u32);
            audio_frame.sample_rate_hz = self.fs_hz;
            audio_frame.samples_per_channel = self.output_size_samples;
            audio_frame.timestamp = if self.first_packet {
                0
            } else {
                self.timestamp_scaler
                    .to_external(self.playout_timestamp)
                    .wrapping_sub(audio_frame.samples_per_channel as u32)
            };
            audio_frame.num_channels = self.sync_buffer.channels();
            self.stats
                .expanded_noise_samples(self.output_size_samples, false);
            self.controller.notify_muted_state();
            return 0;
        }
        let mut return_value = self.get_decision(
            &mut operation,
            packet_list,
            &mut dtmf_event,
            &mut play_dtmf,
            action_override,
        );
        if return_value != 0 {
            self.last_mode = Mode::Error;
            return return_value;
        }

        let mut speech_type = SpeechType::Speech;
        let mut length: i32 = 0;
        let start_num_packets = packet_list.len();
        let decode_return_value =
            self.decode(packet_list, &mut operation, &mut length, &mut speech_type);
        if length > 0 {
            self.last_decoded_type = Some(speech_type);
        }

        let sid_frame_available = operation == Operation::Rfc3389Cng && !packet_list.is_empty();

        // This is the criterion that we did decode some data through the
        // speech decoder, and the operation resulted in comfort noise.
        let codec_internal_sid_frame =
            speech_type == SpeechType::ComfortNoise && start_num_packets > packet_list.len();

        if sid_frame_available || codec_internal_sid_frame {
            // Start a new stopwatch since we are decoding a new CNG packet.
            self.generated_noise_stopwatch = Some(self.tick_timer.get_new_stopwatch());
        }

        self.algorithm_buffer.clear();
        let length = length as usize;
        match operation {
            Operation::Normal => {
                self.do_normal(length, speech_type, play_dtmf);
                if length > 0 {
                    self.stats.decoded_output_played();
                }
            }
            Operation::Merge => {
                self.do_merge(length, speech_type, play_dtmf);
            }
            Operation::Expand => {
                debug_assert_eq!(return_value, 0);
                if self.current_rtp_payload_type.is_none() || !self.do_codec_plc() {
                    return_value = self.do_expand(play_dtmf);
                }
                debug_assert!(
                    self.sync_buffer
                        .future_length()
                        .wrapping_sub(self.expand.overlap_length())
                        >= self.output_size_samples
                );
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                let fast_accelerate =
                    self.enable_fast_accelerate && operation == Operation::FastAccelerate;
                return_value = self.do_accelerate(length, speech_type, play_dtmf, fast_accelerate);
            }
            Operation::PreemptiveExpand => {
                return_value = self.do_preemptive_expand(length, speech_type, play_dtmf);
            }
            Operation::Rfc3389Cng | Operation::Rfc3389CngNoPacket => {
                return_value = self.do_rfc3389_cng(packet_list, play_dtmf);
            }
            Operation::CodecInternalCng => {
                // This handles the case when there is no transmission and the
                // decoder should produce internal comfort noise.
                self.do_codec_internal_cng(length);
            }
            Operation::Dtmf => {
                return_value = self.do_dtmf(&dtmf_event, &mut play_dtmf);
            }
            Operation::Undefined => {
                log::error!("Invalid operation kUndefined.");
                debug_assert!(false, "This should not happen.");
                self.last_mode = Mode::Error;
                return Error::InvalidOperation.code();
            }
        }
        self.last_operation = operation;
        if return_value < 0 {
            return return_value;
        }

        if self.last_mode != Mode::Rfc3389Cng {
            self.comfort_noise.reset();
        }

        // We treat it as if all packets referenced to by
        // `last_decoded_packet_infos` were mashed together when creating the
        // samples in `algorithm_buffer`.
        let packet_infos = std::mem::take(&mut self.last_decoded_packet_infos);

        // Copy samples from `algorithm_buffer` to `sync_buffer`.
        self.sync_buffer.push_back(&self.algorithm_buffer);

        // Extract data from `sync_buffer` to `output`.
        audio_frame.reset_without_muting();
        audio_frame.sample_rate_hz = self.fs_hz;
        audio_frame.samples_per_channel = sample_rate_to_default_channel_size(self.fs_hz);
        let channels = self.sync_buffer.channels();
        let got_audio = {
            let view = audio_frame.mutable_data(self.output_size_samples, channels);
            self.sync_buffer.get_next_audio_interleaved(view)
        };

        // We don't have the ability to properly track individual packets once
        // their audio samples have entered `sync_buffer`. So for now, treat it
        // as if `packet_infos` from packets decoded by the current
        // `GetAudioInternal()` call were all consumed assembling the current
        // audio frame and the current audio frame only.
        // Swap rather than assign: the frame's previous (cleared) vector goes
        // back to `last_decoded_packet_infos`, so steady state allocates
        // nothing here.
        let mut packet_infos = packet_infos;
        std::mem::swap(&mut audio_frame.packet_infos, &mut packet_infos);
        self.last_decoded_packet_infos = packet_infos;
        if self.sync_buffer.future_length() < self.expand.overlap_length() {
            // The sync buffer should always contain `overlap_length` samples,
            // but now too many samples have been extracted. Reinstall the
            // `overlap_length` lookahead by moving the index.
            let missing_lookahead_samples =
                self.expand.overlap_length() - self.sync_buffer.future_length();
            debug_assert!(self.sync_buffer.next_index() >= missing_lookahead_samples);
            let next_index = self.sync_buffer.next_index() - missing_lookahead_samples;
            self.sync_buffer.set_next_index(next_index);
        }

        if !got_audio {
            log::error!(
                "audio_frame.samples_per_channel ({}) != output_size_samples ({})",
                audio_frame.samples_per_channel,
                self.output_size_samples
            );
            audio_frame.mute();
            return Error::SampleUnderrun.code();
        }

        // Should always have overlap samples left in the `sync_buffer`.
        debug_assert!(self.sync_buffer.future_length() >= self.expand.overlap_length());

        if play_dtmf {
            let total = self.output_size_samples * channels;
            let output = &mut audio_frame.mutable_data(self.output_size_samples, channels)[..total];
            return_value = self.dtmf_overdub(&dtmf_event, channels, output);
        }

        // Update the background noise parameters if last operation wrote data
        // straight from the decoder to the `sync_buffer`. That is, none of the
        // operations that modify the signal can be followed by a parameter
        // update.
        if matches!(
            self.last_mode,
            Mode::Normal
                | Mode::AccelerateFail
                | Mode::PreemptiveExpandFail
                | Mode::Rfc3389Cng
                | Mode::CodecInternalCng
        ) {
            self.background_noise.update(&self.sync_buffer);
        }

        if operation == Operation::Dtmf {
            // DTMF data was written the end of `sync_buffer`.
            // Update index to end of DTMF data in `sync_buffer`.
            let size = self.sync_buffer.size();
            self.sync_buffer.set_dtmf_index(size);
        }

        if self.last_mode != Mode::Expand && self.last_mode != Mode::CodecPlc {
            // If last operation was not expand, calculate the
            // `playout_timestamp` from the `sync_buffer`. However, do not
            // update the `playout_timestamp` if it would be moved "backwards".
            let temp_timestamp = self
                .sync_buffer
                .end_timestamp()
                .wrapping_sub(self.sync_buffer.future_length() as u32);
            if (temp_timestamp.wrapping_sub(self.playout_timestamp) as i32) > 0 {
                self.playout_timestamp = temp_timestamp;
            }
        } else {
            // Use dead reckoning to estimate the `playout_timestamp`.
            self.playout_timestamp = self
                .playout_timestamp
                .wrapping_add(self.output_size_samples as u32);
        }
        // Set the timestamp in the audio frame to zero before the first packet
        // has been inserted. Otherwise, subtract the frame size in samples to
        // get the timestamp of the first sample in the frame
        // (playout_timestamp is the last + 1).
        audio_frame.timestamp = if self.first_packet {
            0
        } else {
            self.timestamp_scaler
                .to_external(self.playout_timestamp)
                .wrapping_sub(audio_frame.samples_per_channel as u32)
        };

        if !matches!(
            self.last_mode,
            Mode::Rfc3389Cng | Mode::CodecInternalCng | Mode::Expand | Mode::CodecPlc
        ) {
            self.generated_noise_stopwatch = None;
        }

        if decode_return_value != 0 {
            return decode_return_value;
        }
        return_value
    }

    /// C++ `GetDecision`. Writes the operation to `operation`, the packets
    /// to decode to `packet_list` and a DTMF event to `dtmf_event`; sets
    /// `play_dtmf` when DTMF should be played. Returns 0 on success,
    /// otherwise an error code.
    fn get_decision(
        &mut self,
        operation: &mut Operation,
        packet_list: &mut PacketList,
        dtmf_event: &mut DtmfEvent,
        play_dtmf: &mut bool,
        action_override: Option<Operation>,
    ) -> i32 {
        #[cfg(feature = "parity")]
        {
            self.last_decision = None;
        }
        let ret = self.get_decision_internal(
            operation,
            packet_list,
            dtmf_event,
            play_dtmf,
            action_override,
        );
        #[cfg(feature = "parity")]
        if let (Some(observer), Some(record)) = (self.observer.as_mut(), self.last_decision) {
            observer.on_decision(&record.status, record.raw, *operation);
        }
        ret
    }

    fn get_decision_internal(
        &mut self,
        operation: &mut Operation,
        packet_list: &mut PacketList,
        dtmf_event: &mut DtmfEvent,
        play_dtmf: &mut bool,
        action_override: Option<Operation>,
    ) -> i32 {
        // Initialize output variables.
        *play_dtmf = false;
        *operation = Operation::Undefined;

        let mut end_timestamp = self.sync_buffer.end_timestamp();
        if !self.new_codec {
            let five_seconds_samples = (5 * self.fs_hz) as u32;
            self.packet_buffer.discard_old_packets(
                end_timestamp,
                five_seconds_samples,
                &mut self.stats,
            );
        }

        debug_assert!(self
            .generated_noise_stopwatch
            .as_ref()
            .is_none_or(|stopwatch| stopwatch.elapsed_ticks() >= 1));
        let mut generated_noise_samples: u64 = match &self.generated_noise_stopwatch {
            Some(stopwatch) => (stopwatch.elapsed_ticks().wrapping_sub(1))
                .wrapping_mul(self.output_size_samples as u64)
                .wrapping_add(self.controller.noise_fast_forward() as u64),
            None => 0,
        };

        if self.last_mode == Mode::Rfc3389Cng {
            // Because of timestamp peculiarities, we have to "manually"
            // disallow using a CNG packet with the same timestamp as the one
            // that was last played. This can happen when using redundancy and
            // will cause the timing to shift.
            while let Some(packet) = self.packet_buffer.peek_next_packet() {
                // C++ compares `uint32_t + uint64_t` in 64 bits.
                if !(self.decoder_database.is_comfort_noise(packet.payload_type)
                    && (end_timestamp >= packet.timestamp
                        || (end_timestamp as u64).wrapping_add(generated_noise_samples)
                            > packet.timestamp as u64))
                {
                    break;
                }
                // Don't use this packet, discard it.
                if self.packet_buffer.discard_next_packet(&mut self.stats) != PacketBuffer::K_OK {
                    debug_assert!(false, "Must be ok by design.");
                }
                // Check buffer again.
                if !self.new_codec {
                    self.packet_buffer.discard_old_packets(
                        end_timestamp,
                        (5 * self.fs_hz) as u32,
                        &mut self.stats,
                    );
                }
            }
        }

        let samples_left =
            (self.sync_buffer.future_length() as i64 - self.expand.overlap_length() as i64) as i32;
        if matches!(
            self.last_mode,
            Mode::AccelerateSuccess
                | Mode::AccelerateLowEnergy
                | Mode::PreemptiveExpandSuccess
                | Mode::PreemptiveExpandLowEnergy
        ) {
            // Subtract (samples_left + output_size_samples) from sampleMemory.
            self.controller
                .add_sample_memory(-(samples_left + self.output_size_samples as i32));
        }

        // Check if it is time to play a DTMF event.
        if self.dtmf_buffer.get_event(
            (end_timestamp as u64).wrapping_add(generated_noise_samples) as u32,
            dtmf_event,
        ) {
            *play_dtmf = true;
        }

        // Get instruction.
        generated_noise_samples = match &self.generated_noise_stopwatch {
            Some(stopwatch) => stopwatch
                .elapsed_ticks()
                .wrapping_mul(self.output_size_samples as u64)
                .wrapping_add(self.controller.noise_fast_forward() as u64),
            None => 0,
        };
        let next_packet = self
            .packet_buffer
            .peek_next_packet()
            .map(|packet| PacketInfo {
                timestamp: packet.timestamp,
                is_dtx: packet
                    .frame
                    .as_ref()
                    .is_some_and(|frame| frame.is_dtx_packet()),
                is_cng: self.decoder_database.is_comfort_noise(packet.payload_type),
            });
        let status = NetEqStatus {
            packet_buffer_info: crate::api::neteq_controller::PacketBufferInfo {
                dtx_or_cng: self
                    .packet_buffer
                    .contains_dtx_or_cng_packet(&self.decoder_database),
                num_samples: self
                    .packet_buffer
                    .num_samples_in_buffer(self.decoder_frame_length),
                span_samples: self.packet_buffer.get_span_samples(
                    self.decoder_frame_length,
                    self.last_output_sample_rate_hz as usize,
                    false,
                ),
                span_samples_wait_time: self.packet_buffer.get_span_samples(
                    self.decoder_frame_length,
                    self.last_output_sample_rate_hz as usize,
                    true,
                ),
                num_packets: self.packet_buffer.num_packets_in_buffer(),
            },
            target_timestamp: self.sync_buffer.end_timestamp(),
            expand_mutefactor: self.expand.mute_factor(0),
            last_packet_samples: self.decoder_frame_length,
            last_mode: self.last_mode,
            play_dtmf: *play_dtmf,
            generated_noise_samples: generated_noise_samples as usize,
            sync_buffer_samples: self.sync_buffer.future_length(),
            next_packet,
        };
        *operation = self
            .controller
            .get_decision(&status, &mut self.reset_decoder);
        #[cfg(feature = "parity")]
        {
            self.last_decision = Some(DecisionRecord {
                status,
                raw: *operation,
            });
        }

        // Disallow time stretching if this packet is DTX, because such a
        // decision may be based on earlier buffer level estimate, as we do not
        // update buffer level during DTX. When we have a better way to update
        // buffer level during DTX, this can be discarded.
        if next_packet.is_some_and(|packet| packet.is_dtx)
            && matches!(
                *operation,
                Operation::Merge
                    | Operation::Accelerate
                    | Operation::FastAccelerate
                    | Operation::PreemptiveExpand
            )
        {
            *operation = Operation::Normal;
        }

        if let Some(action_override) = action_override {
            // Use the provided action instead of the decision NetEq decided
            // on.
            *operation = action_override;
        }
        // Check if we already have enough samples in the `sync_buffer`. If so,
        // change decision to normal, unless the decision was merge,
        // accelerate, or preemptive expand.
        if samples_left >= self.output_size_samples as i32
            && !matches!(
                *operation,
                Operation::Merge
                    | Operation::Accelerate
                    | Operation::FastAccelerate
                    | Operation::PreemptiveExpand
            )
        {
            *operation = Operation::Normal;
            return 0;
        }

        self.controller.expand_decision(*operation);
        if self.last_mode == Mode::CodecPlc && *operation != Operation::Expand {
            // Getting out of the PLC expand mode, reporting interruptions.
            // NetEq PLC reports this metrics in expand.cc
            self.stats.end_expand_event(self.fs_hz);
        }

        // Check conditions for reset.
        if self.new_codec || *operation == Operation::Undefined {
            // The only valid reason to get kUndefined is that new_codec is
            // set.
            debug_assert!(self.new_codec);
            if *play_dtmf && next_packet.is_none() {
                self.timestamp = dtmf_event.timestamp;
            } else {
                let Some(packet) = next_packet else {
                    log::error!("Packet missing where it shouldn't.");
                    return -1;
                };
                self.timestamp = packet.timestamp;
                if *operation == Operation::Rfc3389CngNoPacket && packet.is_cng {
                    // Change decision to CNG packet, since we do have a CNG
                    // packet, but it was considered too early to use. Now,
                    // use it anyway.
                    *operation = Operation::Rfc3389Cng;
                } else if *operation != Operation::Rfc3389Cng {
                    *operation = Operation::Normal;
                }
            }
            // Adjust `sync_buffer` timestamp before setting `end_timestamp` to
            // the new value.
            self.sync_buffer
                .increase_end_timestamp(self.timestamp.wrapping_sub(end_timestamp));
            end_timestamp = self.timestamp;
            self.new_codec = false;
            self.controller.soft_reset();
            self.stats.reset_mcu();
        }

        let mut required_samples = self.output_size_samples;
        let samples_10_ms = (80 * self.fs_mult) as usize;
        let samples_20_ms = 2 * samples_10_ms;
        let samples_30_ms = 3 * samples_10_ms;

        match *operation {
            Operation::Expand => {
                self.timestamp = end_timestamp;
                return 0;
            }
            Operation::Rfc3389CngNoPacket | Operation::CodecInternalCng => {
                return 0;
            }
            Operation::Dtmf => {
                // Update timestamp.
                self.timestamp = end_timestamp;
                let generated_noise_samples: u64 = match &self.generated_noise_stopwatch {
                    Some(stopwatch) => stopwatch
                        .elapsed_ticks()
                        .wrapping_mul(self.output_size_samples as u64)
                        .wrapping_add(self.controller.noise_fast_forward() as u64),
                    None => 0,
                };
                if generated_noise_samples > 0 && self.last_mode != Mode::Dtmf {
                    // Make a jump in timestamp due to the recently played
                    // comfort noise.
                    let timestamp_jump = generated_noise_samples as u32;
                    self.sync_buffer.increase_end_timestamp(timestamp_jump);
                    self.timestamp = self.timestamp.wrapping_add(timestamp_jump);
                }
                return 0;
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                // In order to do an accelerate we need at least 30 ms of audio
                // data.
                if samples_left >= samples_30_ms as i32 {
                    // Already have enough data, so we do not need to extract
                    // any more.
                    self.controller.set_sample_memory(samples_left);
                    self.controller.set_prev_time_scale(true);
                    return 0;
                } else if samples_left >= samples_10_ms as i32
                    && self.decoder_frame_length >= samples_30_ms
                {
                    // Avoid decoding more data as it might overflow the
                    // playout buffer.
                    *operation = Operation::Normal;
                    return 0;
                } else if samples_left < samples_20_ms as i32
                    && self.decoder_frame_length < samples_30_ms
                {
                    // Build up decoded data by decoding at least 20 ms of
                    // audio data. Do not perform accelerate yet, but wait
                    // until we only need to do one decoding.
                    required_samples = 2 * self.output_size_samples;
                    *operation = Operation::Normal;
                }
                // If none of the above is true, we have one of two possible
                // situations:
                // (1) 20 ms <= samples_left < 30 ms and decoder_frame_length
                //     < 30 ms; or
                // (2) samples_left < 10 ms and decoder_frame_length >= 30 ms.
                // In either case, we move on with the accelerate decision, and
                // decode one frame now.
            }
            Operation::PreemptiveExpand => {
                // In order to do a preemptive expand we need at least 30 ms of
                // decoded audio data.
                if samples_left >= samples_30_ms as i32
                    || (samples_left >= samples_10_ms as i32
                        && self.decoder_frame_length >= samples_30_ms)
                {
                    // Already have enough data, so we do not need to extract
                    // any more. Or, avoid decoding more data as it might
                    // overflow the playout buffer. Still try preemptive
                    // expand, though.
                    self.controller.set_sample_memory(samples_left);
                    self.controller.set_prev_time_scale(true);
                    return 0;
                }
                if samples_left < samples_20_ms as i32 && self.decoder_frame_length < samples_30_ms
                {
                    // Build up decoded data by decoding at least 20 ms of
                    // audio data. Still try to perform preemptive expand.
                    required_samples = 2 * self.output_size_samples;
                }
                // Move on with the preemptive expand decision.
            }
            Operation::Merge => {
                required_samples =
                    std::cmp::max(self.merge.required_future_samples(), required_samples);
            }
            _ => {
                // Do nothing.
            }
        }

        // Get packets from buffer.
        let mut extracted_samples: i32 = 0;
        if let Some(packet) = self.packet_buffer.peek_next_packet() {
            let increment = packet.timestamp.wrapping_sub(end_timestamp);
            self.sync_buffer.increase_end_timestamp(increment);
            extracted_samples = self.extract_packets(required_samples, packet_list);
            if extracted_samples < 0 {
                return Error::PacketBufferCorruption.code();
            }
        }

        if matches!(
            *operation,
            Operation::Accelerate | Operation::FastAccelerate | Operation::PreemptiveExpand
        ) {
            self.controller
                .set_sample_memory(samples_left + extracted_samples);
            self.controller.set_prev_time_scale(true);
        }

        if matches!(
            *operation,
            Operation::Accelerate | Operation::FastAccelerate
        ) {
            // Check that we have enough data (30ms) to do accelerate.
            if extracted_samples + samples_left < samples_30_ms as i32 {
                // Not enough, do normal operation instead.
                *operation = Operation::Normal;
            }
        }

        self.timestamp = self.sync_buffer.end_timestamp();
        0
    }

    /// C++ `Decode`: decodes the speech packets in `packet_list` into
    /// `decoded_buffer`. The decoded length is written to `decoded_length`
    /// and the speech type to `speech_type`. SID frames for RFC 3389 comfort
    /// noise are not decoded.
    fn decode(
        &mut self,
        packet_list: &mut PacketList,
        operation: &mut Operation,
        decoded_length: &mut i32,
        speech_type: &mut SpeechType,
    ) -> i32 {
        *speech_type = SpeechType::Speech;

        // When packet_list is empty, we may be in kCodecInternalCng mode, and
        // for that we use current active decoder. `decoder_payload_type`
        // stands in for the C++ `AudioDecoder*`: `None` is a null decoder,
        // `Some(None)` the active decoder.
        let mut decoder_payload_type: Option<Option<u8>> =
            if self.decoder_database.get_active_decoder().is_some() {
                Some(None)
            } else {
                None
            };

        if let Some(packet) = packet_list.front() {
            let payload_type = packet.payload_type;
            if !self.decoder_database.is_comfort_noise(payload_type) {
                let decoder_exists = self.decoder_database.get_decoder(payload_type).is_some();
                debug_assert!(decoder_exists);
                if !decoder_exists {
                    log::warn!("Unknown payload type {payload_type}");
                    packet_list.clear();
                    return Error::DecoderNotFound.code();
                }
                decoder_payload_type = Some(Some(payload_type));
                let mut decoder_changed = false;
                self.decoder_database
                    .set_active_decoder(payload_type, &mut decoder_changed);
                if decoder_changed {
                    // We have a new decoder. Re-init some values.
                    let Some(sample_rate_hz) = self
                        .decoder_database
                        .get_decoder_info_mut(payload_type)
                        .map(|info| info.sample_rate_hz())
                    else {
                        log::warn!("Unknown payload type {payload_type}");
                        packet_list.clear();
                        return Error::DecoderNotFound.code();
                    };
                    let channels = self
                        .decoder_database
                        .get_decoder(payload_type)
                        .map(|decoder| decoder.channels())
                        .unwrap_or(1);
                    // If sampling rate or number of channels has changed, we
                    // need to make a reset.
                    if sample_rate_hz != self.fs_hz || channels != self.algorithm_buffer.channels()
                    {
                        self.set_sample_rate_and_channels(sample_rate_hz, channels);
                    }
                    self.sync_buffer.set_end_timestamp(self.timestamp);
                    self.playout_timestamp = self.timestamp;
                }
            }
        }

        if self.reset_decoder {
            if let Some(decoder) = self.lookup_decoder(decoder_payload_type) {
                decoder.reset();
            }

            // Reset comfort noise decoder.
            if let Some(cng_decoder) = self.decoder_database.get_active_cng_decoder() {
                cng_decoder.reset();
            }

            self.reset_decoder = false;
        }

        *decoded_length = 0;
        // Update codec-internal PLC state.
        if *operation == Operation::Merge {
            let start = *decoded_length as usize;
            let mut buffer = std::mem::take(&mut self.decoded_buffer);
            if let Some(decoder) = self.lookup_decoder(decoder_payload_type) {
                if decoder.has_decode_plc() {
                    decoder.decode_plc(1, &mut buffer[start..]);
                }
            }
            self.decoded_buffer = buffer;
        }

        let mut return_value;
        if *operation == Operation::CodecInternalCng {
            debug_assert!(packet_list.is_empty());
            return_value = self.decode_cng(decoder_payload_type, decoded_length, speech_type);
        } else {
            return_value = self.decode_loop(
                packet_list,
                *operation,
                decoder_payload_type,
                decoded_length,
                speech_type,
            );
        }

        if *decoded_length < 0 {
            // Error returned from the decoder.
            *decoded_length = 0;
            self.sync_buffer
                .increase_end_timestamp(self.decoder_frame_length as u32);
            let error_code = self
                .lookup_decoder(decoder_payload_type)
                .map(|decoder| decoder.error_code())
                .unwrap_or(0);
            if error_code != 0 {
                // Got some error code from the decoder.
                return_value = Error::DecoderErrorCode.code();
                log::warn!("Decoder returned error code: {error_code}");
            } else {
                // Decoder does not implement error codes. Return generic
                // error.
                return_value = Error::OtherDecoderError.code();
                log::warn!("Decoder error (no error code)");
            }
            *operation = Operation::Expand; // Do expansion to get data instead.
        }
        if *speech_type != SpeechType::ComfortNoise {
            // Don't increment timestamp if codec returned CNG speech type
            // since in this case, the we will increment the CNGplayedTS
            // counter. Increase with number of samples per channel.
            let channels = self.sync_buffer.channels() as i32;
            self.sync_buffer
                .increase_end_timestamp((*decoded_length / channels) as u32);
        }
        return_value
    }

    /// Resolves the stand-in for the C++ `AudioDecoder*` used inside
    /// `Decode` (see `decode`).
    fn lookup_decoder(
        &mut self,
        decoder_payload_type: Option<Option<u8>>,
    ) -> Option<&mut dyn crate::api::audio_decoder::AudioDecoder> {
        match decoder_payload_type {
            None => None,
            Some(None) => self.decoder_database.get_active_decoder(),
            Some(Some(payload_type)) => self.decoder_database.get_decoder(payload_type),
        }
    }

    /// C++ `DecodeCng`: codec-internal CNG.
    fn decode_cng(
        &mut self,
        decoder_payload_type: Option<Option<u8>>,
        decoded_length: &mut i32,
        speech_type: &mut SpeechType,
    ) -> i32 {
        let fs_hz = self.fs_hz;
        let output_size_samples = self.output_size_samples as i32;
        let decoded_buffer_length = self.decoded_buffer_length;
        let mut buffer = std::mem::take(&mut self.decoded_buffer);
        let result = (|| {
            let Some(decoder) = self.lookup_decoder(decoder_payload_type) else {
                // This happens when active decoder is not defined.
                *decoded_length = -1;
                return 0;
            };

            while *decoded_length < output_size_samples {
                let start = *decoded_length as usize;
                let length = decoder.decode(
                    &[],
                    fs_hz,
                    &mut buffer[start..decoded_buffer_length],
                    speech_type,
                );
                if length > 0 {
                    *decoded_length += length;
                } else {
                    // Error.
                    log::warn!("Failed to decode CNG");
                    *decoded_length = -1;
                    break;
                }
                if *decoded_length > decoded_buffer_length as i32 {
                    // Guard against overflow.
                    log::warn!("Decoded too much CNG.");
                    return Error::DecodedTooMuch.code();
                }
            }
            self.stats.generated_noise_samples(*decoded_length as usize);
            0
        })();
        self.decoded_buffer = buffer;
        result
    }

    /// C++ `DecodeLoop`: performs the actual decoding.
    fn decode_loop(
        &mut self,
        packet_list: &mut PacketList,
        operation: Operation,
        decoder_payload_type: Option<Option<u8>>,
        decoded_length: &mut i32,
        speech_type: &mut SpeechType,
    ) -> i32 {
        debug_assert!(self.last_decoded_packet_infos.is_empty());

        // Do decoding.
        while let Some(front) = packet_list.front() {
            if self.decoder_database.is_comfort_noise(front.payload_type) {
                break;
            }
            let decoded_buffer_length = self.decoded_buffer_length;
            let sync_channels = self.sync_buffer.channels();
            let start = *decoded_length as usize;
            let mut buffer = std::mem::take(&mut self.decoded_buffer);
            let packet = packet_list.pop_front().expect("front exists");
            let (opt_result, decoder_channels) = {
                // At this point, we must have a decoder object.
                let decoder = self
                    .lookup_decoder(decoder_payload_type)
                    .expect("DecodeLoop needs a decoder");
                // The number of channels in the `sync_buffer` should be the
                // same as the number decoder channels.
                debug_assert_eq!(sync_channels, decoder.channels());
                debug_assert!(decoded_buffer_length >= Self::K_MAX_FRAME_SIZE * decoder.channels());
                debug_assert!(matches!(
                    operation,
                    Operation::Normal
                        | Operation::Accelerate
                        | Operation::FastAccelerate
                        | Operation::Merge
                        | Operation::PreemptiveExpand
                ));
                let channels = decoder.channels();
                let result = packet
                    .frame
                    .as_ref()
                    .expect("speech packet has a frame")
                    .decode(decoder, &mut buffer[start..decoded_buffer_length]);
                (result, channels)
            };
            self.decoded_buffer = buffer;
            if let Some(packet_info) = packet.packet_info {
                self.last_decoded_packet_infos.push(packet_info);
            }
            if let Some(result) = opt_result {
                *speech_type = result.speech_type;
                if result.num_decoded_samples > 0 {
                    *decoded_length += result.num_decoded_samples as i32;
                    // Update `decoder_frame_length` with number of samples per
                    // channel.
                    self.decoder_frame_length = result.num_decoded_samples / decoder_channels;
                }
            } else {
                // Error.
                log::warn!("Decode error");
                *decoded_length = -1;
                self.last_decoded_packet_infos.clear();
                packet_list.clear();
                break;
            }
            if *decoded_length > decoded_buffer_length as i32 {
                // Guard against overflow.
                log::warn!("Decoded too much.");
                packet_list.clear();
                return Error::DecodedTooMuch.code();
            }
        } // End of decode loop.

        // If the list is not empty at this point, either a decoding error
        // terminated the while-loop, or list must hold exactly one CNG packet.
        debug_assert!(
            packet_list.is_empty()
                || *decoded_length < 0
                || (packet_list.len() == 1
                    && self
                        .decoder_database
                        .is_comfort_noise(packet_list[0].payload_type))
        );
        0
    }

    /// C++ `DoNormal`.
    fn do_normal(&mut self, decoded_length: usize, speech_type: SpeechType, play_dtmf: bool) {
        let mut ctx = ExpandContext {
            sync_buffer: &mut self.sync_buffer,
            background_noise: &mut self.background_noise,
            random_vector: &mut self.random_vector,
            statistics: &mut self.stats,
        };
        self.normal.process(
            &self.decoded_buffer,
            decoded_length,
            self.last_mode,
            &mut self.algorithm_buffer,
            &mut self.expand,
            &mut ctx,
            &mut self.decoder_database,
        );
        if decoded_length != 0 {
            self.last_mode = Mode::Normal;
        }

        // If last packet was decoded as an inband CNG, set mode to CNG
        // instead.
        if speech_type == SpeechType::ComfortNoise
            || (self.last_mode == Mode::CodecInternalCng && decoded_length == 0)
        {
            self.last_mode = Mode::CodecInternalCng;
        }

        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }
    }

    /// C++ `DoMerge`.
    fn do_merge(&mut self, decoded_length: usize, speech_type: SpeechType, play_dtmf: bool) {
        let mut ctx = ExpandContext {
            sync_buffer: &mut self.sync_buffer,
            background_noise: &mut self.background_noise,
            random_vector: &mut self.random_vector,
            statistics: &mut self.stats,
        };
        let new_length = self.merge.process(
            &mut self.decoded_buffer,
            decoded_length,
            &mut self.algorithm_buffer,
            &mut self.expand,
            &mut ctx,
        );
        // Correction can be negative.
        let expand_length_correction =
            new_length as i32 - (decoded_length / self.algorithm_buffer.channels()) as i32;

        // Update in-call and post-call statistics.
        if self.expand.muted() || self.last_decoded_type == Some(SpeechType::ComfortNoise) {
            // Expand generates only noise.
            self.stats
                .expanded_noise_samples_correction(expand_length_correction);
        } else {
            // Expansion generates more than only noise.
            self.stats
                .expanded_voice_samples_correction(expand_length_correction);
        }

        self.last_mode = Mode::Merge;
        // If last packet was decoded as an inband CNG, set mode to CNG
        // instead.
        if speech_type == SpeechType::ComfortNoise {
            self.last_mode = Mode::CodecInternalCng;
        }
        self.expand.reset();
        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }
    }

    /// C++ `DoCodecPlc`.
    fn do_codec_plc(&mut self) -> bool {
        let channels = self.algorithm_buffer.channels();
        // C++ computes this in `size_t`, which wraps when the sync buffer
        // holds more future audio than one output block plus the overlap.
        let requested_samples_per_channel = self.output_size_samples.wrapping_sub(
            self.sync_buffer
                .future_length()
                .wrapping_sub(self.expand.overlap_length()),
        );
        self.concealment_audio.clear();
        let mut concealment_audio = std::mem::take(&mut self.concealment_audio);
        {
            let Some(decoder) = self.decoder_database.get_active_decoder() else {
                self.concealment_audio = concealment_audio;
                return false;
            };
            decoder.generate_plc(requested_samples_per_channel, &mut concealment_audio);
        }
        self.concealment_audio = concealment_audio;
        if self.concealment_audio.is_empty() {
            // Nothing produced. Resort to regular expand.
            return false;
        }
        assert!(
            self.concealment_audio.len() >= requested_samples_per_channel.wrapping_mul(channels)
        );
        self.sync_buffer
            .push_back_interleaved(&self.concealment_audio);
        debug_assert_ne!(self.algorithm_buffer.channels(), 0);
        let concealed_samples_per_channel = self.concealment_audio.len() / channels;

        // Update in-call and post-call statistics.
        let is_new_concealment_event = self.last_mode != Mode::CodecPlc;
        if self.concealment_audio.iter().all(|&sample| sample == 0) {
            // Expand operation generates only noise.
            self.stats
                .expanded_noise_samples(concealed_samples_per_channel, is_new_concealment_event);
        } else {
            // Expand operation generates more than only noise.
            self.stats
                .expanded_voice_samples(concealed_samples_per_channel, is_new_concealment_event);
        }
        self.last_mode = Mode::CodecPlc;
        if self.generated_noise_stopwatch.is_none() {
            // Start a new stopwatch since we may be covering for a lost CNG
            // packet.
            self.generated_noise_stopwatch = Some(self.tick_timer.get_new_stopwatch());
        }
        true
    }

    /// C++ `DoExpand`.
    fn do_expand(&mut self, play_dtmf: bool) -> i32 {
        // `size_t` in C++: wraps (and ends the loop) if the sync buffer ever
        // holds less than the overlap.
        while self
            .sync_buffer
            .future_length()
            .wrapping_sub(self.expand.overlap_length())
            < self.output_size_samples
        {
            self.algorithm_buffer.clear();
            let return_value = {
                let mut ctx = ExpandContext {
                    sync_buffer: &mut self.sync_buffer,
                    background_noise: &mut self.background_noise,
                    random_vector: &mut self.random_vector,
                    statistics: &mut self.stats,
                };
                self.expand.process(&mut ctx, &mut self.algorithm_buffer)
            };
            let length = self.algorithm_buffer.size();
            let is_new_concealment_event = self.last_mode != Mode::Expand;

            // Update in-call and post-call statistics.
            if self.expand.muted() || self.last_decoded_type == Some(SpeechType::ComfortNoise) {
                // Expand operation generates only noise.
                self.stats
                    .expanded_noise_samples(length, is_new_concealment_event);
            } else {
                // Expand operation generates more than only noise.
                self.stats
                    .expanded_voice_samples(length, is_new_concealment_event);
            }

            self.last_mode = Mode::Expand;

            if return_value < 0 {
                return return_value;
            }

            self.sync_buffer.push_back(&self.algorithm_buffer);
            self.algorithm_buffer.clear();
        }
        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }

        if self.generated_noise_stopwatch.is_none() {
            // Start a new stopwatch since we may be covering for a lost CNG
            // packet.
            self.generated_noise_stopwatch = Some(self.tick_timer.get_new_stopwatch());
        }

        0
    }

    /// C++ `DoAccelerate`.
    fn do_accelerate(
        &mut self,
        mut decoded_length: usize,
        speech_type: SpeechType,
        play_dtmf: bool,
        fast_accelerate: bool,
    ) -> i32 {
        let required_samples = (240 * self.fs_mult) as usize; // Must have 30 ms.
        let mut borrowed_samples_per_channel = 0usize;
        let num_channels = self.algorithm_buffer.channels();
        let decoded_length_per_channel = decoded_length / num_channels;
        if decoded_length_per_channel < required_samples {
            // Must move data from the `sync_buffer` in order to get 30 ms.
            borrowed_samples_per_channel = required_samples - decoded_length_per_channel;
            self.decoded_buffer.copy_within(
                0..decoded_length,
                borrowed_samples_per_channel * num_channels,
            );
            self.sync_buffer
                .read_interleaved_from_end(borrowed_samples_per_channel, &mut self.decoded_buffer);
            decoded_length = required_samples * num_channels;
        }

        let mut samples_removed = 0usize;
        let return_code = self.accelerate.process(
            &self.decoded_buffer,
            decoded_length,
            fast_accelerate,
            &mut self.algorithm_buffer,
            &mut samples_removed,
            &self.background_noise,
        );
        self.stats.accelerated_samples(samples_removed);
        match return_code {
            TimeStretchReturnCode::Success => self.last_mode = Mode::AccelerateSuccess,
            TimeStretchReturnCode::SuccessLowEnergy => self.last_mode = Mode::AccelerateLowEnergy,
            TimeStretchReturnCode::NoStretch => self.last_mode = Mode::AccelerateFail,
            TimeStretchReturnCode::Error => {
                self.last_mode = Mode::AccelerateFail;
                return Error::AccelerateError.code();
            }
        }

        if borrowed_samples_per_channel > 0 {
            // Copy borrowed samples back to the `sync_buffer`.
            let length = self.algorithm_buffer.size();
            if length < borrowed_samples_per_channel {
                // This destroys the beginning of the buffer, but will not
                // cause any problems.
                let position = self.sync_buffer.size() - borrowed_samples_per_channel;
                self.sync_buffer
                    .replace_at_index_all(&self.algorithm_buffer, position);
                self.sync_buffer
                    .push_front_zeros(borrowed_samples_per_channel - length);
                self.algorithm_buffer.pop_front(length);
                debug_assert!(self.algorithm_buffer.empty());
            } else {
                let position = self.sync_buffer.size() - borrowed_samples_per_channel;
                self.sync_buffer.replace_at_index(
                    &self.algorithm_buffer,
                    borrowed_samples_per_channel,
                    position,
                );
                self.algorithm_buffer
                    .pop_front(borrowed_samples_per_channel);
            }
        }

        // If last packet was decoded as an inband CNG, set mode to CNG
        // instead.
        if speech_type == SpeechType::ComfortNoise {
            self.last_mode = Mode::CodecInternalCng;
        }
        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }
        self.expand.reset();
        0
    }

    /// C++ `DoPreemptiveExpand`.
    fn do_preemptive_expand(
        &mut self,
        mut decoded_length: usize,
        speech_type: SpeechType,
        play_dtmf: bool,
    ) -> i32 {
        let required_samples = (240 * self.fs_mult) as usize; // Must have 30 ms.
        let num_channels = self.algorithm_buffer.channels();
        let mut borrowed_samples_per_channel = 0usize;
        let mut old_borrowed_samples_per_channel = 0usize;
        let decoded_length_per_channel = decoded_length / num_channels;
        if decoded_length_per_channel < required_samples {
            // Must move data from the `sync_buffer` in order to get 30 ms.
            borrowed_samples_per_channel = required_samples - decoded_length_per_channel;
            // Calculate how many of these were already played out.
            let future_length = self.sync_buffer.future_length();
            old_borrowed_samples_per_channel =
                borrowed_samples_per_channel.saturating_sub(future_length);
            self.decoded_buffer.copy_within(
                0..decoded_length,
                borrowed_samples_per_channel * num_channels,
            );
            self.sync_buffer
                .read_interleaved_from_end(borrowed_samples_per_channel, &mut self.decoded_buffer);
            decoded_length = required_samples * num_channels;
        }

        let mut samples_added = 0usize;
        let return_code = self.preemptive_expand.process(
            &self.decoded_buffer,
            decoded_length,
            old_borrowed_samples_per_channel,
            &mut self.algorithm_buffer,
            &mut samples_added,
            &self.background_noise,
        );
        self.stats.preemptive_expanded_samples(samples_added);
        match return_code {
            TimeStretchReturnCode::Success => self.last_mode = Mode::PreemptiveExpandSuccess,
            TimeStretchReturnCode::SuccessLowEnergy => {
                self.last_mode = Mode::PreemptiveExpandLowEnergy
            }
            TimeStretchReturnCode::NoStretch => self.last_mode = Mode::PreemptiveExpandFail,
            TimeStretchReturnCode::Error => {
                self.last_mode = Mode::PreemptiveExpandFail;
                return Error::PreemptiveExpandError.code();
            }
        }

        if borrowed_samples_per_channel > 0 {
            // Copy borrowed samples back to the `sync_buffer`.
            let position = self.sync_buffer.size() - borrowed_samples_per_channel;
            self.sync_buffer.replace_at_index(
                &self.algorithm_buffer,
                borrowed_samples_per_channel,
                position,
            );
            self.algorithm_buffer
                .pop_front(borrowed_samples_per_channel);
        }

        // If last packet was decoded as an inband CNG, set mode to CNG
        // instead.
        if speech_type == SpeechType::ComfortNoise {
            self.last_mode = Mode::CodecInternalCng;
        }
        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }
        self.expand.reset();
        0
    }

    /// C++ `DoRfc3389Cng`: generates RFC 3389 comfort noise. `packet_list`
    /// holds either one SID frame to update the noise parameters, or nothing,
    /// in which case the previously received parameters are used.
    fn do_rfc3389_cng(&mut self, packet_list: &mut PacketList, play_dtmf: bool) -> i32 {
        if let Some(packet) = packet_list.front() {
            // Must have exactly one SID frame at this point.
            debug_assert_eq!(packet_list.len(), 1);
            if !self.decoder_database.is_comfort_noise(packet.payload_type) {
                log::error!("Trying to decode non-CNG payload as CNG.");
                return Error::OtherError.code();
            }
            if self
                .comfort_noise
                .update_parameters(packet, &mut self.decoder_database)
                == ComfortNoise::K_INTERNAL_ERROR
            {
                self.algorithm_buffer.zeros(self.output_size_samples);
                return -self.comfort_noise.internal_error_code();
            }
        }
        let cn_return = self.comfort_noise.generate(
            self.output_size_samples,
            &mut self.algorithm_buffer,
            &mut self.decoder_database,
            &mut self.sync_buffer,
        );
        self.expand.reset();
        self.last_mode = Mode::Rfc3389Cng;
        if !play_dtmf {
            self.dtmf_tone_generator.reset();
        }
        if cn_return == ComfortNoise::K_INTERNAL_ERROR {
            log::warn!(
                "Comfort noise generator returned error code: {}",
                self.comfort_noise.internal_error_code()
            );
            return Error::ComfortNoiseErrorCode.code();
        } else if cn_return == ComfortNoise::K_UNKNOWN_PAYLOAD_TYPE {
            return Error::UnknownRtpPayloadType.code();
        }
        0
    }

    /// C++ `DoCodecInternalCng`: the decoder generates codec-internal comfort
    /// noise when no packet was received.
    fn do_codec_internal_cng(&mut self, decoded_length: usize) {
        let mut ctx = ExpandContext {
            sync_buffer: &mut self.sync_buffer,
            background_noise: &mut self.background_noise,
            random_vector: &mut self.random_vector,
            statistics: &mut self.stats,
        };
        self.normal.process(
            &self.decoded_buffer,
            decoded_length,
            self.last_mode,
            &mut self.algorithm_buffer,
            &mut self.expand,
            &mut ctx,
            &mut self.decoder_database,
        );
        self.last_mode = Mode::CodecInternalCng;
        self.expand.reset();
    }

    /// C++ `DoDtmf`. The commented-out `dtmf_switch` block of the C++ is
    /// equivalent to `dtmf_switch` always being false and is not ported.
    fn do_dtmf(&mut self, dtmf_event: &DtmfEvent, play_dtmf: &mut bool) -> i32 {
        let mut dtmf_return_value = 0;
        if !self.dtmf_tone_generator.initialized() {
            // Initialize if not already done.
            dtmf_return_value =
                self.dtmf_tone_generator
                    .init(self.fs_hz, dtmf_event.event_no, dtmf_event.volume);
        }

        if dtmf_return_value == 0 {
            // Generate DTMF signal.
            dtmf_return_value = self
                .dtmf_tone_generator
                .generate(self.output_size_samples, &mut self.algorithm_buffer);
        }

        if dtmf_return_value < 0 {
            self.algorithm_buffer.zeros(self.output_size_samples);
            return dtmf_return_value;
        }

        self.sync_buffer
            .increase_end_timestamp(self.output_size_samples as u32);
        self.expand.reset();
        self.last_mode = Mode::Dtmf;

        // Set to false because the DTMF is already in the algorithm buffer.
        *play_dtmf = false;
        0
    }

    /// C++ `DtmfOverdub`: overdubs DTMF on top of `output`. (C++ declares it
    /// `const` but mutates the tone generator through its pointer.)
    fn dtmf_overdub(
        &mut self,
        dtmf_event: &DtmfEvent,
        num_channels: usize,
        output: &mut [i16],
    ) -> i32 {
        let mut out_index = 0usize;
        let mut overdub_length = self.output_size_samples; // Default value.

        if self.sync_buffer.dtmf_index() > self.sync_buffer.next_index() {
            // Special operation for transition from "DTMF only" to "DTMF
            // overdub".
            out_index = std::cmp::min(
                self.sync_buffer.dtmf_index() - self.sync_buffer.next_index(),
                self.output_size_samples,
            );
            overdub_length = self.output_size_samples - out_index;
        }

        let mut dtmf_output = AudioMultiVector::new(num_channels);
        let mut dtmf_return_value = 0;
        if !self.dtmf_tone_generator.initialized() {
            dtmf_return_value =
                self.dtmf_tone_generator
                    .init(self.fs_hz, dtmf_event.event_no, dtmf_event.volume);
        }
        if dtmf_return_value == 0 {
            dtmf_return_value = self
                .dtmf_tone_generator
                .generate(overdub_length, &mut dtmf_output);
            debug_assert_eq!(overdub_length, dtmf_output.size());
        }
        dtmf_output.read_interleaved(overdub_length, &mut output[out_index..]);
        if dtmf_return_value < 0 {
            dtmf_return_value
        } else {
            0
        }
    }

    /// C++ `ExtractPackets`: moves packets from `packet_buffer` into
    /// `packet_list` until they produce at least `required_samples`. Returns
    /// the number of samples the extracted packets will produce, or -1.
    fn extract_packets(&mut self, required_samples: usize, packet_list: &mut PacketList) -> i32 {
        let Some(next_packet) = self.packet_buffer.peek_next_packet() else {
            log::error!("Packet buffer unexpectedly empty.");
            debug_assert!(false);
            return -1;
        };
        let first_timestamp = next_packet.timestamp;
        let mut next_timestamp = next_packet.timestamp;
        let mut extracted_samples: usize;

        // Packet extraction loop.
        loop {
            self.timestamp = next_timestamp;
            let Some(packet) = self.packet_buffer.get_next_packet() else {
                log::error!("Should always be able to extract a packet here");
                debug_assert!(false);
                return -1;
            };
            let waiting_time_ms = packet
                .waiting_time
                .as_ref()
                .map(|stopwatch| stopwatch.elapsed_ms())
                .unwrap_or(0);
            self.stats.store_waiting_time(waiting_time_ms as i32);
            debug_assert!(!packet.empty());

            let has_cng_packet = self.decoder_database.is_comfort_noise(packet.payload_type);
            // Store number of extracted samples.
            let mut packet_duration = 0usize;
            if let Some(frame) = packet.frame.as_ref() {
                packet_duration = frame.duration();
                if packet.priority.codec_level > 0 {
                    self.stats.secondary_decoded_samples(packet_duration as i32);
                }
            } else if !has_cng_packet {
                log::warn!("Unknown payload type {}", packet.payload_type);
                debug_assert!(false);
            }

            if packet_duration == 0 {
                // Decoder did not return a packet duration. Assume that the
                // packet contains the same number of samples as the previous
                // one.
                packet_duration = self.decoder_frame_length;
            }
            extracted_samples = (packet.timestamp.wrapping_sub(first_timestamp) as usize)
                .wrapping_add(packet_duration);

            let mut processing_time_us: i64 = 0;
            if let Some(receive_time_us) = packet
                .packet_info
                .as_ref()
                .and_then(|info| info.receive_time_us)
            {
                processing_time_us = self.clock.current_time_us() - receive_time_us;
            }

            self.stats.jitter_buffer_delay(
                packet_duration,
                waiting_time_ms,
                self.controller.target_level_ms() as i64 as u64,
                self.controller.unlimited_target_level_ms() as i64 as u64,
                processing_time_us as u64,
            );

            // Check what packet is available next.
            let next_packet_available = match self.packet_buffer.peek_next_packet() {
                Some(next) => {
                    next_timestamp = next.timestamp;
                    next.payload_type == packet.payload_type
                        && next.timestamp == packet.timestamp.wrapping_add(packet_duration as u32)
                        && !has_cng_packet
                }
                None => false,
            };

            packet_list.push_back(packet); // Store packet in list.
            if !(extracted_samples < required_samples && next_packet_available) {
                break;
            }
        }

        if extracted_samples > 0 {
            // Delete old packets only when we are going to decode something.
            // Otherwise, we could end up in the situation where we never
            // decode anything, since all incoming packets are considered too
            // old but the buffer will also never be flooded and flushed.
            self.packet_buffer
                .discard_all_old_packets(self.timestamp, &mut self.stats);
        }

        extracted_samples as i32
    }

    /// C++ `UpdatePlcComponents`: rebuilds Expand and Merge.
    fn update_plc_components(&mut self, fs_hz: i32, channels: usize) {
        // Delete objects and create new ones.
        self.expand = Expand::new(fs_hz, channels);
        self.merge = Merge::new(fs_hz, channels);
    }

    /// C++ `SetSampleRateAndChannels`: resets the per-rate state for `fs_hz`
    /// and `channels`. Panics on unsupported values, like the C++
    /// `RTC_CHECK`s.
    fn set_sample_rate_and_channels(&mut self, fs_hz: i32, channels: usize) {
        log::trace!("SetSampleRateAndChannels {fs_hz} {channels}");
        assert!(matches!(fs_hz, 8000 | 16000 | 32000 | 48000));
        assert!(channels > 0);
        assert!(channels <= K_MAX_NUMBER_OF_CHANNELS);

        // The format must fit in an AudioFrame. Situations where this could
        // theoratically happen but aren't supported is e.g. if receiving 24
        // channels of 10ms 48 kHz buffers.
        self.output_size_samples = sample_rate_to_default_channel_size(fs_hz);
        assert!(channels * self.output_size_samples <= AudioFrame::MAX_DATA_SIZE_SAMPLES);

        // Before changing the sample rate, end and report any ongoing expand
        // event.
        self.stats.end_expand_event(self.fs_hz);
        self.fs_hz = fs_hz;
        self.fs_mult = fs_hz / 8000;
        debug_assert_eq!(
            self.output_size_samples,
            (Self::K_OUTPUT_SIZE_MS * 8 * self.fs_mult) as usize
        );
        self.decoder_frame_length = 3 * self.output_size_samples; // Initialize to 30ms.

        self.last_mode = Mode::Normal;

        if let Some(cng_decoder) = self.decoder_database.get_active_cng_decoder() {
            cng_decoder.reset();
        }

        // Delete algorithm buffer and create a new one.
        self.algorithm_buffer = AudioMultiVector::new(channels);

        // Delete sync buffer and create a new one.
        self.sync_buffer =
            SyncBuffer::new(channels, Self::K_SYNC_BUFFER_SIZE * self.fs_mult as usize);

        // Delete BackgroundNoise object and create a new one.
        self.background_noise = BackgroundNoise::new(channels);

        // Reset random vector.
        self.random_vector.reset();

        self.update_plc_components(fs_hz, channels);

        // Move index so that we create a small set of future samples (all 0).
        let next_index = self.sync_buffer.next_index() - self.expand.overlap_length();
        self.sync_buffer.set_next_index(next_index);

        self.normal = Normal::new(fs_hz);
        self.accelerate = Accelerate::new(fs_hz, channels);
        self.preemptive_expand =
            PreemptiveExpand::new(fs_hz, channels, self.expand.overlap_length());

        // Delete ComfortNoise object and create a new one.
        self.comfort_noise = ComfortNoise::new(fs_hz);

        // Verify that `decoded_buffer` is long enough.
        if self.decoded_buffer_length < Self::K_MAX_FRAME_SIZE * channels {
            // Reallocate to larger size.
            self.decoded_buffer_length = Self::K_MAX_FRAME_SIZE * channels;
            self.decoded_buffer = vec![0; self.decoded_buffer_length];
        }
        self.controller
            .set_sample_rate(self.fs_hz, self.output_size_samples);
    }

    /// C++ `LastOutputType`: the output type for the audio produced by the
    /// latest `GetAudio`.
    fn last_output_type(&self) -> OutputType {
        if self.last_mode == Mode::CodecInternalCng || self.last_mode == Mode::Rfc3389Cng {
            OutputType::Cng
        } else if self.last_mode == Mode::Expand && self.expand.mute_factor(0) == 0 {
            // Expand mode has faded down to background noise only (very long
            // expand).
            OutputType::PlcCng
        } else if self.last_mode == Mode::Expand {
            OutputType::Plc
        } else if self.last_mode == Mode::CodecPlc {
            OutputType::CodecPlc
        } else {
            OutputType::NormalSpeech
        }
    }

    /// C++ `ToPacketArrivedInfo`.
    fn to_packet_arrived_info(&self, packet: &Packet) -> PacketArrivedInfo {
        let dec_info = self.decoder_database.get_decoder_info(packet.payload_type);
        PacketArrivedInfo {
            is_cng_or_dtmf: dec_info.is_some_and(|info| info.is_comfort_noise() || info.is_dtmf()),
            packet_length_samples: packet
                .frame
                .as_ref()
                .map(|frame| frame.duration())
                .unwrap_or(self.decoder_frame_length),
            main_timestamp: packet.timestamp,
            main_sequence_number: packet.sequence_number,
            is_dtx: packet
                .frame
                .as_ref()
                .is_some_and(|frame| frame.is_dtx_packet()),
            buffer_flush: false,
        }
    }
}
