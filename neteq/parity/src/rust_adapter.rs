//! Adapter for our Rust `neteq` crate (feature `rust-neteq`).
//!
//! Builds the Rust NetEq the same way `cpp/neteq_shim.cc` builds
//! `webrtc::NetEqImpl`: default config fields from [`NetEqConfig`], the
//! default `DecisionLogic` controller, the built-in decoder factory (PCM16B,
//! and Opus through C libopus by default, or `ropus`), and a simulated clock that starts at
//! `start_time_ms`. Packets carry `receive_time = arrival` like the shim's
//! `RtpPacketInfo`.

use std::collections::BTreeMap;
use std::sync::Arc;

use neteq::api::audio_decoder::{AudioDecoder, AudioDecoderFactory};
use neteq::api::audio_format::SdpAudioFormat;
use neteq::api::audio_frame::{AudioFrame, OutputSpeechType};
use neteq::api::clock::SimulatedClock;
use neteq::api::config::Config;
use neteq::api::neteq::{NetEq, NetEqBuilder, Operation as RustOperation};
use neteq::api::rtp::{RtpHeader as RustRtpHeader, RtpPacketInfo};
use neteq::codecs::BuiltinAudioDecoderFactory;

use crate::libopus_decoder::LibopusAudioDecoder;

use crate::error::{HarnessError, Result};
use crate::types::{
    AudioOutput, Codec, LifetimeStatistics, NetEqConfig, NetworkStatistics, Operation,
    OperationsAndState, RtpHeader, SpeechType, VadActivity,
};
use crate::under_test::NetEqUnderTest;

/// Which Opus decoder the Rust NetEq runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusDecoderKind {
    /// C libopus through FFI: the same codec arithmetic as the C++ reference.
    Libopus,
    /// `ropus`, the pure-Rust decoder the neteq crate ships.
    Ropus,
}

/// The builtin factory, with Opus swapped for [`LibopusAudioDecoder`].
struct LibopusFactory {
    builtin: BuiltinAudioDecoderFactory,
}

impl AudioDecoderFactory for LibopusFactory {
    fn make_decoder(&self, format: &SdpAudioFormat) -> Option<Box<dyn AudioDecoder>> {
        if !format.name.eq_ignore_ascii_case("opus") {
            return self.builtin.make_decoder(format);
        }
        // Same SDP rules as `AudioDecoderOpus::SdpToConfig`.
        if format.clockrate_hz != 48000 || format.num_channels != 2 {
            return None;
        }
        let channels = match format.parameters.get("stereo").map(String::as_str) {
            None | Some("0") => 1,
            Some("1") => 2,
            Some(_) => return None,
        };
        LibopusAudioDecoder::new(channels, 48000)
            .map(|decoder| Box::new(decoder) as Box<dyn AudioDecoder>)
    }
}

/// Our Rust NetEq behind the [`NetEqUnderTest`] surface.
pub struct RustNetEq {
    neteq: NetEq,
    clock: SimulatedClock,
    frame: AudioFrame,
}

impl RustNetEq {
    /// Creates the Rust NetEq with `config`; the simulated clock starts at
    /// `start_time_ms`.
    pub fn new(config: &NetEqConfig, start_time_ms: i64) -> Result<Self> {
        Self::with_opus_decoder(config, start_time_ms, OpusDecoderKind::Libopus)
    }

    /// Like [`RustNetEq::new`], choosing the Opus decoder.
    pub fn with_opus_decoder(
        config: &NetEqConfig,
        start_time_ms: i64,
        opus: OpusDecoderKind,
    ) -> Result<Self> {
        let rust_config = Config {
            sample_rate_hz: config.sample_rate_hz,
            max_packets_in_buffer: config.max_packets_in_buffer as usize,
            max_delay_ms: config.max_delay_ms,
            min_delay_ms: config.min_delay_ms,
            enable_fast_accelerate: config.enable_fast_accelerate,
            enable_muted_state: config.enable_muted_state,
            enable_rtx_handling: config.enable_rtx_handling,
            for_test_no_time_stretching: config.for_test_no_time_stretching,
            ..Config::default()
        };
        let clock = SimulatedClock::new(start_time_ms * 1000);
        let factory: Arc<dyn AudioDecoderFactory> = match opus {
            OpusDecoderKind::Libopus => Arc::new(LibopusFactory {
                builtin: BuiltinAudioDecoderFactory::new(),
            }),
            OpusDecoderKind::Ropus => Arc::new(BuiltinAudioDecoderFactory::new()),
        };
        let neteq = NetEqBuilder::new(rust_config)
            .decoder_factory(factory)
            .clock(Arc::new(clock.clone()))
            .build();
        Ok(Self {
            neteq,
            clock,
            frame: AudioFrame::new(),
        })
    }

    /// Current simulated time.
    pub fn now_ms(&self) -> i64 {
        self.clock.time_ms()
    }

    fn register(
        &mut self,
        payload_type: u8,
        format: SdpAudioFormat,
        what: &'static str,
    ) -> Result<()> {
        if self
            .neteq
            .register_payload_type(i32::from(payload_type), &format)
        {
            Ok(())
        } else {
            Err(HarnessError::Register(what))
        }
    }
}

fn to_operation(operation: RustOperation) -> Operation {
    match operation {
        RustOperation::Normal => Operation::Normal,
        RustOperation::Merge => Operation::Merge,
        RustOperation::Expand => Operation::Expand,
        RustOperation::Accelerate => Operation::Accelerate,
        RustOperation::FastAccelerate => Operation::FastAccelerate,
        RustOperation::PreemptiveExpand => Operation::PreemptiveExpand,
        RustOperation::Rfc3389Cng => Operation::Rfc3389Cng,
        RustOperation::Rfc3389CngNoPacket => Operation::Rfc3389CngNoPacket,
        RustOperation::CodecInternalCng => Operation::CodecInternalCng,
        RustOperation::Dtmf => Operation::Dtmf,
        RustOperation::Undefined => Operation::Undefined,
    }
}

fn to_speech_type(speech_type: OutputSpeechType) -> SpeechType {
    match speech_type {
        OutputSpeechType::NormalSpeech => SpeechType::NormalSpeech,
        OutputSpeechType::Plc => SpeechType::Plc,
        OutputSpeechType::Cng => SpeechType::Cng,
        OutputSpeechType::PlcCng => SpeechType::PlcCng,
        OutputSpeechType::CodecPlc => SpeechType::CodecPlc,
        OutputSpeechType::Undefined => SpeechType::Undefined,
    }
}

impl NetEqUnderTest for RustNetEq {
    fn name(&self) -> &'static str {
        "rust"
    }

    fn register_codec(&mut self, codec: &Codec, with_cn: bool) -> Result<()> {
        match *codec {
            Codec::L16 {
                sample_rate_hz,
                channels,
            } => self.register(
                codec.payload_type(),
                SdpAudioFormat::new("L16", sample_rate_hz as i32, usize::from(channels)),
                "L16",
            )?,
            Codec::Opus { channels } => {
                let mut parameters = BTreeMap::new();
                if channels == 2 {
                    parameters.insert("stereo".to_string(), "1".to_string());
                }
                self.register(
                    codec.payload_type(),
                    SdpAudioFormat::with_parameters("opus", 48000, 2, parameters),
                    "opus",
                )?
            }
        }
        if with_cn {
            self.register(
                codec.cn_payload_type(),
                SdpAudioFormat::new("CN", codec.rtp_clock_hz() as i32, 1),
                "CN",
            )?;
        }
        Ok(())
    }

    fn insert_packet(
        &mut self,
        header: &RtpHeader,
        payload: &[u8],
        receive_time_ms: i64,
    ) -> Result<i32> {
        let rust_header = RustRtpHeader {
            marker_bit: header.marker,
            payload_type: header.payload_type,
            sequence_number: header.sequence_number,
            timestamp: header.timestamp,
            ssrc: header.ssrc,
        };
        let info = RtpPacketInfo::new(&rust_header, Some(receive_time_ms * 1000));
        Ok(
            match self.neteq.insert_packet(&rust_header, payload, info) {
                Ok(()) => 0,
                Err(_) => -1,
            },
        )
    }

    fn get_audio(&mut self) -> Result<AudioOutput> {
        let result = self.neteq.get_audio(&mut self.frame, None);
        let (return_code, muted) = match result {
            Ok(result) => (0, result.muted),
            Err(_) => (-1, self.frame.muted()),
        };
        Ok(AudioOutput {
            samples: self.frame.data().to_vec(),
            samples_per_channel: self.frame.samples_per_channel,
            num_channels: self.frame.num_channels,
            sample_rate_hz: self.frame.sample_rate_hz,
            speech_type: to_speech_type(self.frame.speech_type),
            // NetEq never sets VAD activity at this rev; C++ leaves the
            // frame's default `kVadUnknown`.
            vad_activity: VadActivity::Unknown,
            muted,
            operation: to_operation(self.neteq.last_operation_for_test()),
            playout_timestamp: self.neteq.playout_timestamp(),
            return_code,
        })
    }

    fn advance_time_ms(&mut self, ms: i64) {
        self.clock.advance_time_ms(ms);
    }

    fn target_delay_ms(&self) -> Result<i32> {
        Ok(self.neteq.target_delay_ms())
    }

    fn filtered_current_delay_ms(&self) -> Result<i32> {
        Ok(self.neteq.filtered_current_delay_ms())
    }

    fn network_statistics(&mut self, reset: bool) -> Result<NetworkStatistics> {
        let s = if reset {
            self.neteq.network_statistics()
        } else {
            self.neteq.current_network_statistics()
        };
        Ok(NetworkStatistics {
            current_buffer_size_ms: s.current_buffer_size_ms,
            preferred_buffer_size_ms: s.preferred_buffer_size_ms,
            jitter_peaks_found: s.jitter_peaks_found,
            expand_rate: s.expand_rate,
            speech_expand_rate: s.speech_expand_rate,
            preemptive_rate: s.preemptive_rate,
            accelerate_rate: s.accelerate_rate,
            secondary_decoded_rate: s.secondary_decoded_rate,
            secondary_discarded_rate: s.secondary_discarded_rate,
            mean_waiting_time_ms: s.mean_waiting_time_ms,
            median_waiting_time_ms: s.median_waiting_time_ms,
            min_waiting_time_ms: s.min_waiting_time_ms,
            max_waiting_time_ms: s.max_waiting_time_ms,
        })
    }

    fn lifetime_statistics(&self) -> Result<LifetimeStatistics> {
        let s = self.neteq.lifetime_statistics();
        Ok(LifetimeStatistics {
            total_samples_received: s.total_samples_received,
            concealed_samples: s.concealed_samples,
            concealment_events: s.concealment_events,
            jitter_buffer_delay_ms: s.jitter_buffer_delay_ms,
            jitter_buffer_emitted_count: s.jitter_buffer_emitted_count,
            jitter_buffer_target_delay_ms: s.jitter_buffer_target_delay_ms,
            jitter_buffer_minimum_delay_ms: s.jitter_buffer_minimum_delay_ms,
            inserted_samples_for_deceleration: s.inserted_samples_for_deceleration,
            removed_samples_for_acceleration: s.removed_samples_for_acceleration,
            silent_concealed_samples: s.silent_concealed_samples,
            fec_packets_received: s.fec_packets_received,
            fec_packets_discarded: s.fec_packets_discarded,
            packets_discarded: s.packets_discarded,
            delayed_packet_outage_samples: s.delayed_packet_outage_samples,
            delayed_packet_outage_events: s.delayed_packet_outage_events,
            relative_packet_arrival_delay_ms: s.relative_packet_arrival_delay_ms,
            jitter_buffer_packets_received: s.jitter_buffer_packets_received,
            interruption_count: s.interruption_count,
            total_interruption_duration_ms: s.total_interruption_duration_ms,
            generated_noise_samples: s.generated_noise_samples,
            total_processing_delay_us: s.total_processing_delay_us,
        })
    }

    fn operations_and_state(&self) -> Result<OperationsAndState> {
        let s = self.neteq.operations_and_state();
        Ok(OperationsAndState {
            preemptive_samples: s.preemptive_samples,
            accelerate_samples: s.accelerate_samples,
            packet_buffer_flushes: s.packet_buffer_flushes,
            last_waiting_time_ms: s.last_waiting_time_ms,
            current_buffer_size_ms: s.current_buffer_size_ms,
            current_frame_size_ms: s.current_frame_size_ms,
            next_packet_available: s.next_packet_available,
        })
    }

    fn set_base_minimum_delay_ms(&mut self, delay_ms: i32) -> Result<bool> {
        Ok(self.neteq.set_base_minimum_delay_ms(delay_ms))
    }

    fn flush(&mut self) -> Result<()> {
        self.neteq.flush_buffers();
        Ok(())
    }
}
