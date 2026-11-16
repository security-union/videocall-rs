/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::collections::VecDeque;
use web_time::{Duration, Instant};

use crate::background_noise::{BackgroundNoise, K_VEC_LEN};
use crate::buffer::SmartFlushConfig;
use crate::buffer::{is_newer_timestamp, is_obsolete_timestamp, BufferReturnCode, PacketBuffer};
use crate::delay_manager::{DelayConfig, DelayManager};
use crate::expand::ExpandFactory;
use crate::expand::{Expand, ExpandPhase};
use crate::packet::AudioPacket;
use crate::packet_arrival_history::PacketArrivalHistory;
use crate::statistics::{
    LifetimeStatistics, NetworkStatistics, StatisticsCalculator, TimeStretchOperation,
};
use crate::time_stretch::{Accelerate, PreemptiveExpand, TimeStretchResult};
use crate::{NetEqError, Result};

/// Hardcoded minimum delay.
const K_BASE_MIN_DELAY_MS: u32 = 20;

/// decision_logic.cc `kPostponeDecodingLevel` (percent of the target level).
const K_POSTPONE_DECODING_LEVEL: u32 = 50;
/// decision_logic.cc `kTargetLevelWindowMs`.
const K_TARGET_LEVEL_WINDOW_MS: u32 = 100;
/// decision_logic.cc `kDelayAdjustmentGranularityMs`.
const K_DELAY_ADJUSTMENT_GRANULARITY_MS: u32 = 20;
/// decision_logic.cc `kPacketHistorySizeMs`.
const K_PACKET_HISTORY_SIZE_MS: u32 = 2000;
/// decision_logic.cc `kCngTimeoutMs`.
const K_CNG_TIMEOUT_MS: u64 = 1000;
/// decision_logic.h `kMinTimescaleInterval` (ticks between time-stretches).
const K_MIN_TIMESCALE_INTERVAL: u64 = 5;
/// neteq_impl.cc `GetDecision`: `five_seconds_samples`, the horizon for
/// obsolete packets.
const K_OBSOLETE_HORIZON_MS: u32 = 5000;

/// What the last `get_audio` produced (libwebrtc `NetEq::Mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Expand,
    Merge,
    AccelerateSuccess,
    AccelerateLowEnergy,
    AccelerateFail,
    PreemptiveExpandSuccess,
    PreemptiveExpandLowEnergy,
    PreemptiveExpandFail,
    /// Codec-internal comfort noise (Opus DTX).
    CodecInternalCng,
}

impl Mode {
    // decision_logic.cc IsTimestretch
    fn is_timestretch(self) -> bool {
        matches!(
            self,
            Mode::AccelerateSuccess
                | Mode::AccelerateLowEnergy
                | Mode::PreemptiveExpandSuccess
                | Mode::PreemptiveExpandLowEnergy
        )
    }
}

/// NetEQ configuration
#[derive(Debug, Clone)]
pub struct NetEqConfig {
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Number of audio channels
    pub channels: u8,
    /// Maximum number of packets in buffer
    pub max_packets_in_buffer: usize,
    /// Maximum delay in milliseconds (0 = no limit)
    pub max_delay_ms: u32,
    /// Minimum delay in milliseconds
    pub min_delay_ms: u32,
    // Fixed additional delay
    pub additional_delay_ms: u32,
    /// Disable time stretching (for testing)
    pub for_test_no_time_stretching: bool,
    /// neteq.h `Config::enable_fast_accelerate`: let a fast accelerate
    /// remove several pitch periods at once. Off by default, as in
    /// libwebrtc.
    pub enable_fast_accelerate: bool,
    /// Bypass NetEQ processing and decode packets directly (for A/B testing)
    pub bypass_mode: bool,
    /// Delay manager configuration
    pub delay_config: DelayConfig,
    /// Smart flushing configuration
    pub smart_flush_config: SmartFlushConfig,
}

impl Default for NetEqConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            channels: 1,
            max_packets_in_buffer: 200,
            max_delay_ms: 0,
            min_delay_ms: 0,
            additional_delay_ms: 0,
            for_test_no_time_stretching: false,
            enable_fast_accelerate: false,
            bypass_mode: false,
            delay_config: DelayConfig::default(),
            smart_flush_config: SmartFlushConfig::default(),
        }
    }
}

/// NetEQ operation types
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operation {
    /// Normal packet decode
    Normal,
    /// Merge operation (blending)
    Merge,
    /// Expand operation (concealment)
    Expand,
    /// First expand in a row
    ExpandStart,
    /// Last expand in a row
    ExpandEnd,
    /// Accelerate operation (time compression)
    Accelerate,
    /// Fast accelerate operation
    FastAccelerate,
    /// Preemptive expand operation (time expansion)
    PreemptiveExpand,
    /// No longer produced: stretched audio now goes into the sync buffer
    /// like any decoded audio, as in libwebrtc. Kept so the operation
    /// statistics keep their layout.
    TimeStretchBuffer,
    /// Comfort noise generation
    ComfortNoise,
    /// DTMF tone generation
    Dtmf,
    /// Undefined/error state
    Undefined,
}

/// NetEQ statistics summary
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetEqStats {
    pub network: NetworkStatistics,
    pub lifetime: LifetimeStatistics,
    pub current_buffer_size_ms: u32,
    pub target_delay_ms: u32,
    pub packets_awaiting_decode: usize,
    /// Number of audio packets received per second (rolling 1s window)
    #[serde(default)]
    pub packets_per_sec: u32,
}

/// Audio frame output from NetEQ
#[derive(Debug, Clone)]
pub struct AudioFrame {
    /// Audio samples (interleaved for multi-channel)
    pub samples: Vec<f32>,
    /// Sample rate
    pub sample_rate: u32,
    /// Number of channels
    pub channels: u8,
    /// Samples per channel
    pub samples_per_channel: usize,
    /// Speech type classification
    pub speech_type: SpeechType,
    /// Voice activity detection result
    pub vad_activity: bool,
}

/// Speech type classification
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpeechType {
    Normal,
    Cng,    // Comfort noise
    Expand, // Concealment
    Music,  // Music detection
}

impl AudioFrame {
    pub fn new(sample_rate: u32, channels: u8, samples_per_channel: usize) -> Self {
        Self {
            samples: vec![0.0; samples_per_channel * channels as usize],
            sample_rate,
            channels,
            samples_per_channel,
            speech_type: SpeechType::Normal,
            vad_activity: false,
        }
    }

    pub fn duration_ms(&self) -> u32 {
        (self.samples_per_channel as u32 * 1000) / self.sample_rate
    }
}

/// Main NetEQ implementation
///
/// The decision core follows libwebrtc's NetEq (commit aeb9b375): a playout
/// timestamp (`end_timestamp`, the RTP timestamp wanted next), the last mode,
/// a generated-noise stopwatch in 10 ms ticks, and `DecisionLogic`'s rules.
/// `leftover_samples` plays the role of the sync buffer's future samples.
pub struct NetEq {
    config: NetEqConfig,
    packet_buffer: PacketBuffer,
    delay_manager: DelayManager,
    statistics: StatisticsCalculator,
    accelerate: Accelerate,
    preemptive_expand: PreemptiveExpand,
    /// background_noise.h: noise level that tells time stretching what is
    /// active speech.
    background_noise: BackgroundNoise,
    /// Played samples, the sync buffer's history: time stretching borrows
    /// from it and background noise is estimated on it. Always full, like
    /// libwebrtc's zero-initialized sync buffer.
    history: Vec<f32>,
    expand: Box<Expand>,
    output_frame_size_samples: usize,
    _muted: bool,
    last_operation: Operation,
    /// Decoded samples not yet played (the sync buffer's future samples).
    leftover_samples: Vec<f32>,
    /// Map RTP payload-type → audio decoder instance.
    decoders: HashMap<u8, Box<dyn crate::codec::AudioDecoder + Send>>,
    /// Audio queue for bypass mode (direct decoding without jitter buffer)
    bypass_audio_queue: VecDeque<f32>,
    /// Rolling counters for packets-per-second measurement
    packets_received_this_second: u32,
    last_packets_second_instant: Instant,
    packets_per_sec_snapshot: u32,
    /// The clock, read once at the start of each `insert_packet` and
    /// `get_audio` and used throughout it.
    now: Instant,
    /// neteq_impl.h `last_mode_`.
    last_mode: Mode,
    /// tick_timer.h: incremented once per `get_audio`.
    tick: u64,
    /// neteq_impl.h `generated_noise_stopwatch_`: tick it was started.
    noise_start_tick: Option<u64>,
    /// sync_buffer.h `end_timestamp()`: the RTP timestamp just after the
    /// last decoded sample, i.e. the timestamp wanted next.
    end_timestamp: u32,
    /// neteq_impl.h `new_codec_`.
    new_codec: bool,
    /// neteq_impl.h `first_packet_`.
    first_packet: bool,
    /// decision_logic.h `packet_arrival_history_`.
    arrival_history: PacketArrivalHistory,
    /// decision_logic.h `timescale_countdown_`: tick from which a
    /// time-stretch is allowed again.
    timescale_allowed_tick: u64,
    /// decision_logic.h `prev_time_scale_`.
    prev_time_scale: bool,
    /// neteq_impl.h `decoder_frame_length_` (samples per channel).
    decoder_frame_length: usize,
    /// Sequence number and timestamp of the last decoded packet.
    last_decoded: Option<(u16, u32)>,
    /// Sequence number of the last inserted packet.
    last_received_seq: Option<u16>,
    /// Payload type of the last decoded packet (`GetActiveDecoder`).
    active_payload_type: Option<u8>,
}

/// What one extraction decoded.
#[derive(Default)]
struct Extracted {
    samples: usize,
    /// Samples per channel decoded into `leftover_samples`
    /// (`decoded_length`).
    decoded_samples: usize,
    comfort_noise: bool,
    decoded_any: bool,
}

impl NetEq {
    /// Create a new NetEQ instance
    pub fn new(config: NetEqConfig) -> Result<Self> {
        // Validate configuration
        if config.sample_rate == 0 {
            return Err(NetEqError::InvalidSampleRate(config.sample_rate));
        }
        if config.channels == 0 {
            return Err(NetEqError::InvalidChannelCount(config.channels));
        }

        // Calculate output frame size (10ms of audio)
        let output_frame_size_samples =
            (config.sample_rate / 100) as usize * config.channels as usize;

        let packet_buffer = PacketBuffer::with_config(
            config.max_packets_in_buffer,
            config.smart_flush_config.clone(),
        );

        let mut delay_config = config.delay_config.clone();
        delay_config.base_minimum_delay_ms = K_BASE_MIN_DELAY_MS;
        delay_config.base_maximum_delay_ms = (config.max_packets_in_buffer * 20 * 3 / 4) as u32;
        delay_config.additional_delay_ms = config.additional_delay_ms;

        let mut delay_manager = DelayManager::new(delay_config);
        if config.min_delay_ms > 0 {
            delay_manager.set_minimum_delay(config.min_delay_ms);
        }
        if config.max_delay_ms > 0 {
            delay_manager.set_maximum_delay(config.max_delay_ms);
        }

        let statistics = StatisticsCalculator::new();

        let channels = config.channels as usize;
        let accelerate = Accelerate::new(config.sample_rate, channels);
        // neteq_impl.cc SetSampleRateAndChannels: preemptive expand gets
        // expand.cc's overlap_length_ = 5 * fs / 8000.
        let preemptive_expand = PreemptiveExpand::new(
            config.sample_rate,
            channels,
            5 * config.sample_rate as usize / 8000,
        );
        let history = vec![0.0; Self::history_len(config.sample_rate, channels)];
        let expand: Box<Expand> = ExpandFactory::create_expand(config.sample_rate, config.channels);
        let arrival_history =
            PacketArrivalHistory::new(K_PACKET_HISTORY_SIZE_MS, config.sample_rate);
        let decoder_frame_length = (config.sample_rate / 50) as usize;

        Ok(Self {
            config,
            packet_buffer,
            delay_manager,
            statistics,
            accelerate,
            preemptive_expand,
            background_noise: BackgroundNoise::new(),
            history,
            expand,
            output_frame_size_samples,
            _muted: false,
            last_operation: Operation::Normal,
            leftover_samples: Vec::new(),
            decoders: HashMap::new(),
            bypass_audio_queue: VecDeque::new(),
            packets_received_this_second: 0,
            last_packets_second_instant: crate::clock::now(),
            now: crate::clock::now(),
            packets_per_sec_snapshot: 0,
            last_mode: Mode::Normal,
            tick: 0,
            noise_start_tick: None,
            end_timestamp: 0,
            new_codec: false,
            first_packet: true,
            arrival_history,
            // decision_logic.cc DecisionLogic: countdown kMinTimescaleInterval + 1
            timescale_allowed_tick: K_MIN_TIMESCALE_INTERVAL + 1,
            prev_time_scale: false,
            decoder_frame_length,
            last_decoded: None,
            last_received_seq: None,
            active_payload_type: None,
        })
    }

    fn channels(&self) -> usize {
        self.config.channels as usize
    }

    /// 10 ms, in samples per channel (`output_size_samples_`).
    fn output_size(&self) -> usize {
        (self.config.sample_rate / 100) as usize
    }

    fn sample_rate_khz(&self) -> u32 {
        (self.config.sample_rate / 1000).max(1)
    }

    /// Insert a packet into the jitter buffer (neteq_impl.cc
    /// `NetEqImpl::InsertPacket`).
    pub fn insert_packet(&mut self, packet: AudioPacket) -> Result<()> {
        self.now = crate::clock::now();
        // Update packets-per-second rolling counter and roll snapshot if needed
        self.packets_received_this_second = self.packets_received_this_second.saturating_add(1);
        self.maybe_roll_packet_rate();

        if self.config.bypass_mode {
            // Bypass mode: decode packet immediately and queue audio
            if let Some(decoder) = self.decoders.get_mut(&packet.header.payload_type) {
                match decoder.decode(&packet.payload) {
                    Ok(pcm_samples) => {
                        let sample_count = pcm_samples.len();
                        self.bypass_audio_queue.extend(pcm_samples);
                        log::trace!("Bypass mode: decoded {sample_count} samples");
                    }
                    Err(e) => {
                        log::warn!("Bypass mode decode error: {e:?}");
                    }
                }
            } else {
                log::warn!(
                    "No decoder registered for payload type {} in bypass mode",
                    packet.header.payload_type
                );
            }
            return Ok(());
        }

        // neteq_impl.cc InsertPacket: "payload is empty" -> kFail.
        if packet.payload.is_empty() {
            return Err(NetEqError::InvalidPacket("empty payload".to_string()));
        }
        let timestamp = packet.header.timestamp;
        let sequence_number = packet.header.sequence_number;
        let length_samples = packet.duration_samples() as u32;

        if self.first_packet {
            // neteq_impl.cc InsertPacket: first packet flushes and moves the
            // sync buffer's end timestamp to it.
            self.packet_buffer.flush(&mut self.statistics);
            self.end_timestamp = timestamp;
        } else if self.is_new_stream(sequence_number, timestamp) {
            // Deviation (no SSRC in our API): a sender restart is handled as
            // libwebrtc handles a new stream: flush, then reset on decode.
            log::info!("NetEq: sender restart detected (seq={sequence_number}, ts={timestamp})");
            self.packet_buffer.flush(&mut self.statistics);
            self.new_codec = true;
            // The old stream's last decoded packet says nothing about the
            // new one.
            self.last_decoded = None;
        }
        self.last_received_seq = Some(sequence_number);

        let target_delay = self.delay_manager.target_delay_ms();
        match self
            .packet_buffer
            .insert_packet(packet, &mut self.statistics, target_delay)?
        {
            BufferReturnCode::Flushed => {
                log::info!("Buffer flushed due to overflow");
                self.new_codec = true;
            }
            BufferReturnCode::PartialFlush => {
                // neteq_impl.cc InsertPacket: forward the sync buffer timestamp.
                if let Some(next) = self.packet_buffer.peek_next_timestamp() {
                    self.end_timestamp = next;
                }
            }
            _ => {}
        }

        // decision_logic.cc DecisionLogic::PacketArrived (should_update_stats
        // = !new_codec_).
        if !self.new_codec
            && self
                .arrival_history
                .insert(timestamp, length_samples, self.now)
            && self.arrival_history.len() >= 2
        {
            let arrival_delay_ms = self.arrival_history.get_delay_ms(timestamp, self.now);
            let reordered = !self.arrival_history.is_newest_rtp_timestamp(timestamp);
            self.delay_manager
                .update_arrival_delay(arrival_delay_ms, reordered, self.now);
        }
        if self.first_packet {
            self.first_packet = false;
            self.new_codec = true;
        }

        self.statistics.update_buffer_size(
            self.current_buffer_size_ms() as u16,
            self.delay_manager.target_delay_ms() as u16,
        );
        Ok(())
    }

    /// Deviation: libwebrtc starts a new stream on an SSRC change, which
    /// `insert_packet(seq, ts, payload)` cannot see. A sender restart is
    /// recognised instead by (a) a sequence-number jump larger than the
    /// packet buffer can hold, which no loss burst or reordering NetEq could
    /// use produces, or (b) a timestamp older than the last decoded one on a
    /// sequence number newer than it, which no reordering produces.
    fn is_new_stream(&self, seq: u16, timestamp: u32) -> bool {
        if let Some(last) = self.last_received_seq {
            let d = seq.wrapping_sub(last);
            let distance = d.min(d.wrapping_neg()) as usize;
            if distance > self.config.max_packets_in_buffer {
                return true;
            }
        }
        if let Some((dseq, dts)) = self.last_decoded {
            let seq_newer = seq != dseq && seq.wrapping_sub(dseq) < 0x8000;
            if seq_newer && is_newer_timestamp(dts, timestamp) {
                return true;
            }
        }
        false
    }

    /// Get 10ms of audio data
    pub fn get_audio(&mut self) -> Result<AudioFrame> {
        // Ensure packet-rate snapshot rolls even when no packets arrive
        // This prevents stale non-zero values when the peer stops sending
        self.now = crate::clock::now();
        self.maybe_roll_packet_rate();
        if self.config.bypass_mode {
            let mut frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                self.output_frame_size_samples / self.config.channels as usize,
            );
            let samples_needed = self.output_frame_size_samples;
            let mut filled = 0;
            while filled < samples_needed {
                match self.bypass_audio_queue.pop_front() {
                    Some(x) => frame.samples[filled] = x,
                    None => break,
                }
                filled += 1;
            }
            frame.samples[filled..].fill(0.0);
            frame.speech_type = if filled == samples_needed {
                SpeechType::Normal
            } else {
                SpeechType::Expand
            };
            frame.vad_activity = filled > 0;
            return Ok(frame);
        }

        let mut frame = AudioFrame::new(
            self.config.sample_rate,
            self.config.channels,
            self.output_frame_size_samples / self.config.channels as usize,
        );

        // neteq_impl.cc GetAudioInternal: tick_timer_->Increment().
        self.tick += 1;
        let previous_mode = self.last_mode;
        let (operation, extracted) = self.get_decision()?;
        // neteq_impl.cc GetAudioInternal: a decoded codec-internal SID frame
        // starts a new generated-noise stopwatch.
        if extracted.comfort_noise {
            self.noise_start_tick = Some(self.tick);
        }
        self.last_operation = operation;

        match operation {
            Operation::Normal | Operation::Merge => {
                self.do_normal(&mut frame, &extracted, previous_mode, operation)
            }
            Operation::Accelerate => self.do_accelerate(&mut frame, &extracted, false),
            Operation::FastAccelerate => {
                let fast = self.config.enable_fast_accelerate;
                self.do_accelerate(&mut frame, &extracted, fast)
            }
            Operation::PreemptiveExpand => self.do_preemptive_expand(&mut frame, &extracted),
            Operation::ComfortNoise => self.do_codec_internal_cng(&mut frame),
            _ => self.do_expand(&mut frame),
        }

        // neteq_impl.cc GetAudioInternal: the stopwatch only runs through
        // comfort noise and concealment.
        if !matches!(self.last_mode, Mode::CodecInternalCng | Mode::Expand) {
            self.noise_start_tick = None;
        }

        self.push_history(&frame.samples);
        // neteq_impl.cc GetAudioInternal: update the background noise only
        // after operations that wrote decoded audio unchanged.
        if matches!(
            self.last_mode,
            Mode::Normal
                | Mode::AccelerateFail
                | Mode::PreemptiveExpandFail
                | Mode::CodecInternalCng
        ) {
            let tail = self.reference_channel_tail(K_VEC_LEN);
            self.background_noise.update(&tail);
        }

        self.statistics.record_decode_operation(operation, self.now);
        self.statistics
            .jitter_buffer_delay(frame.duration_ms() as u64, frame.samples_per_channel as u64);

        Ok(frame)
    }

    /// Roll the packets-per-second window if ≥1s elapsed since last snapshot.
    /// Safe to call frequently; no-ops if interval hasn't elapsed.
    fn maybe_roll_packet_rate(&mut self) {
        let now = self.now;
        if now.duration_since(self.last_packets_second_instant) >= Duration::from_secs(1) {
            self.packets_per_sec_snapshot = self.packets_received_this_second;
            self.packets_received_this_second = 0;
            self.last_packets_second_instant = now;
        }
    }

    /// Get current statistics
    pub fn get_statistics(&self) -> NetEqStats {
        NetEqStats {
            network: self.statistics.network_statistics().clone(),
            lifetime: self.statistics.lifetime_statistics().clone(),
            current_buffer_size_ms: self.current_buffer_size_ms(),
            target_delay_ms: self.delay_manager.target_delay_ms(),
            packets_awaiting_decode: self.packet_buffer.len(),
            packets_per_sec: self.packets_per_sec_snapshot,
        }
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.packet_buffer.is_empty()
    }

    /// Get target delay in milliseconds
    pub fn target_delay_ms(&self) -> u32 {
        self.delay_manager.target_delay_ms()
    }

    /// Set minimum delay, return the effective minimum delay
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> u32 {
        self.delay_manager.set_minimum_delay(delay_ms)
    }

    /// Set maximum delay, return the effective maximum delay
    pub fn set_maximum_delay(&mut self, delay_ms: u32) -> u32 {
        self.delay_manager.set_maximum_delay(delay_ms)
    }

    /// The mode the last `get_audio` call left NetEq in.
    pub fn last_mode(&self) -> Mode {
        self.last_mode
    }

    /// The operation the last `get_audio` call performed.
    pub fn last_operation(&self) -> Operation {
        self.last_operation
    }

    /// Flush the buffer and reset all internal state (neteq_impl.cc
    /// `FlushBuffers`: the next packet starts a new stream).
    pub fn flush(&mut self) {
        self.packet_buffer.flush(&mut self.statistics);
        self.leftover_samples.clear();
        // sync_buffer.cc Flush: the history is zeros again.
        self.history.fill(0.0);
        self.first_packet = true;
        self.last_decoded = None;
        self.last_received_seq = None;
        self.soft_reset();
    }

    /// decision_logic.cc `DecisionLogic::SoftReset`.
    fn soft_reset(&mut self) {
        self.prev_time_scale = false;
        self.timescale_allowed_tick = self.tick + K_MIN_TIMESCALE_INTERVAL + 1;
        self.delay_manager.reset();
        self.arrival_history.reset();
    }

    /// neteq_impl.cc `NetEqImpl::GetDecision` around decision_logic.cc
    /// `DecisionLogic::GetDecision`. Packets the operation needs are
    /// extracted and decoded into `leftover_samples` here, as libwebrtc
    /// extracts them in `GetDecision` and decodes them right after.
    fn get_decision(&mut self) -> Result<(Operation, Extracted)> {
        let out = self.output_size();
        let khz = self.sample_rate_khz();
        let horizon = K_OBSOLETE_HORIZON_MS * khz;
        if !self.new_codec {
            // neteq_impl.cc GetDecision: DiscardOldPackets(end_timestamp, 5 s)
            self.packet_buffer.discard_old_packets(
                self.end_timestamp,
                horizon,
                &mut self.statistics,
            );
        }
        let samples_left = self.leftover_samples.len() / self.channels();
        let generated_noise_samples = self
            .noise_start_tick
            .map_or(0, |start| (self.tick - start) * out as u64);

        // decision_logic.cc GetDecision: timescale countdown after a stretch.
        self.prev_time_scale = self.prev_time_scale && self.last_mode.is_timestretch();
        if self.prev_time_scale {
            self.timescale_allowed_tick = self.tick + K_MIN_TIMESCALE_INTERVAL;
        }
        let target_ms = self.delay_manager.target_delay_ms();
        // decision_logic.cc FilterBufferLevel ends with prev_time_scale_ =
        // false. The filtered level itself is not ported: at this commit
        // libwebrtc reads it only for the FilteredCurrentDelayMs statistic
        // and the unused UnderTargetLevel, never in a decision.
        if !matches!(self.last_mode, Mode::CodecInternalCng | Mode::Expand) {
            self.prev_time_scale = false;
        }

        let next = self
            .packet_buffer
            .peek_next_packet()
            .map(|p| (p.header.timestamp, p.is_dtx()));
        let mut operation = match next {
            None => self.no_packet(generated_noise_samples),
            Some(_) if self.postpone_decode(target_ms, generated_noise_samples) => {
                self.no_packet(generated_noise_samples)
            }
            Some((ts, _)) if ts == self.end_timestamp => {
                self.expected_packet_available(target_ms, samples_left)
            }
            Some((ts, _)) if !is_obsolete_timestamp(ts, self.end_timestamp, horizon) => {
                self.future_packet_available(ts, target_ms, generated_noise_samples)
            }
            // decision_logic.cc GetDecision: older than the target means a
            // new stream; signal a reset.
            Some(_) => Operation::Undefined,
        };

        // neteq_impl.cc GetDecision: DTX packets are never time-stretched.
        if next.is_some_and(|(_, dtx)| dtx)
            && matches!(
                operation,
                Operation::Merge
                    | Operation::Accelerate
                    | Operation::FastAccelerate
                    | Operation::PreemptiveExpand
            )
        {
            operation = Operation::Normal;
        }

        // neteq_impl.cc GetDecision: enough samples in the sync buffer.
        if samples_left >= out
            && !matches!(
                operation,
                Operation::Merge
                    | Operation::Accelerate
                    | Operation::FastAccelerate
                    | Operation::PreemptiveExpand
            )
        {
            return Ok((Operation::Normal, Extracted::default()));
        }

        // neteq_impl.cc GetDecision: "Check conditions for reset."
        if self.new_codec || operation == Operation::Undefined {
            let Some((ts, _)) = next else {
                return Ok((Operation::Expand, Extracted::default()));
            };
            self.end_timestamp = ts;
            self.new_codec = false;
            self.soft_reset();
            operation = Operation::Normal;
        }

        let mut required_samples = out;
        let frame_len = self.decoder_frame_length;
        match operation {
            Operation::Expand | Operation::ComfortNoise => {
                return Ok((operation, Extracted::default()))
            }
            Operation::Accelerate | Operation::FastAccelerate => {
                if samples_left >= 3 * out {
                    self.prev_time_scale = true;
                    return Ok((operation, Extracted::default()));
                } else if samples_left >= out && frame_len >= 3 * out {
                    return Ok((Operation::Normal, Extracted::default()));
                } else if samples_left < 2 * out && frame_len < 3 * out {
                    required_samples = 2 * out;
                    operation = Operation::Normal;
                }
            }
            Operation::PreemptiveExpand => {
                if samples_left >= 3 * out || (samples_left >= out && frame_len >= 3 * out) {
                    self.prev_time_scale = true;
                    return Ok((operation, Extracted::default()));
                }
                if samples_left < 2 * out && frame_len < 3 * out {
                    required_samples = 2 * out;
                }
            }
            _ => {}
        }

        let extracted = if next.is_some() {
            self.extract_packets(required_samples)
        } else {
            Extracted::default()
        };
        if matches!(
            operation,
            Operation::Accelerate | Operation::FastAccelerate | Operation::PreemptiveExpand
        ) {
            self.prev_time_scale = true;
        }
        // "Check that we have enough data (30ms) to do accelerate."
        if matches!(operation, Operation::Accelerate | Operation::FastAccelerate)
            && samples_left + extracted.samples < 3 * out
        {
            operation = Operation::Normal;
        }
        Ok((operation, extracted))
    }

    /// decision_logic.cc `DecisionLogic::NoPacket`.
    fn no_packet(&self, generated_noise_samples: u64) -> Operation {
        match self.last_mode {
            Mode::CodecInternalCng => {
                // Stop CNG after a timeout.
                if generated_noise_samples > K_CNG_TIMEOUT_MS * u64::from(self.sample_rate_khz()) {
                    Operation::Expand
                } else {
                    Operation::ComfortNoise
                }
            }
            _ => Operation::Expand,
        }
    }

    /// decision_logic.cc `DecisionLogic::PostponeDecode`.
    fn postpone_decode(&self, target_ms: u32, generated_noise_samples: u64) -> bool {
        let min_level =
            (target_ms * self.sample_rate_khz() * K_POSTPONE_DECODING_LEVEL / 100) as usize;
        let level = self.packet_buffer.get_span_samples(
            self.decoder_frame_length,
            self.config.sample_rate,
            true,
            self.now,
        );
        if level >= min_level {
            return false;
        }
        // Don't postpone decoding if there is a future DTX packet.
        if self.packet_buffer.contains_dtx_packet() {
            return false;
        }
        if self.last_mode == Mode::CodecInternalCng {
            return true;
        }
        // libwebrtc continues expand only once its mute factor is below 0.5
        // (the concealment was long enough to be noticed). Deviation: our
        // expand is near-silent noise with no mute factor, so an expansion
        // longer than one packet stands in for it.
        self.last_mode == Mode::Expand && generated_noise_samples > self.decoder_frame_length as u64
    }

    /// decision_logic.cc `DecisionLogic::ExpectedPacketAvailable`.
    fn expected_packet_available(&self, target_ms: u32, samples_left: usize) -> Operation {
        if !self.config.for_test_no_time_stretching && self.last_mode != Mode::Expand {
            // decision_logic.cc GetPlayoutDelayMs
            let playout_timestamp = self.end_timestamp.wrapping_sub(samples_left as u32);
            let playout_delay_ms = u64::from(
                self.arrival_history
                    .get_delay_ms(playout_timestamp, self.now),
            );
            let low_limit = u64::from(target_ms);
            let high_limit = low_limit
                + u64::from(self.arrival_history.get_max_delay_ms())
                + u64::from(K_DELAY_ADJUSTMENT_GRANULARITY_MS);
            if playout_delay_ms >= high_limit * 4 {
                return Operation::FastAccelerate;
            }
            if self.tick >= self.timescale_allowed_tick {
                if playout_delay_ms >= high_limit {
                    return Operation::Accelerate;
                }
                if playout_delay_ms < low_limit {
                    return Operation::PreemptiveExpand;
                }
            }
        }
        Operation::Normal
    }

    /// decision_logic.cc `DecisionLogic::FuturePacketAvailable` and
    /// `PacketTooEarly`.
    fn future_packet_available(
        &mut self,
        next_timestamp: u32,
        target_ms: u32,
        generated_noise_samples: u64,
    ) -> Operation {
        let buffer_delay_samples = self.packet_buffer.get_span_samples(
            self.decoder_frame_length,
            self.config.sample_rate,
            true,
            self.now,
        );
        let buffer_delay_ms = buffer_delay_samples as u64 / u64::from(self.sample_rate_khz());
        let high_limit = u64::from(target_ms + K_TARGET_LEVEL_WINDOW_MS / 2);
        let above_target_delay = buffer_delay_ms > high_limit;
        let timestamp_leap = u64::from(next_timestamp.wrapping_sub(self.end_timestamp));
        let packet_too_early = timestamp_leap > generated_noise_samples;
        if packet_too_early && !above_target_delay {
            return self.no_packet(generated_noise_samples);
        }
        match self.last_mode {
            Mode::Expand => Operation::Merge,
            Mode::CodecInternalCng => Operation::Normal,
            _ => Operation::Expand,
        }
    }

    /// neteq_impl.cc `NetEqImpl::ExtractPackets` plus the decode
    /// (`Decode`/`DecodeLoop`): contiguous packets from the front of the
    /// buffer until `required_samples` are covered. Comfort noise (an Opus
    /// DTX frame) does not advance the playout timestamp.
    fn extract_packets(&mut self, required_samples: usize) -> Extracted {
        let mut result = Extracted::default();
        let Some(first_timestamp) = self.packet_buffer.peek_next_timestamp() else {
            return result;
        };
        // neteq_impl.cc GetDecision: IncreaseEndTimestamp(packet->timestamp - end_timestamp)
        self.end_timestamp = first_timestamp;
        let mut last_timestamp = first_timestamp;
        while let Some(packet) = self.packet_buffer.get_next_packet() {
            let duration = match packet.duration_samples() {
                0 => self.decoder_frame_length,
                d => d,
            };
            last_timestamp = packet.header.timestamp;
            result.samples =
                packet.header.timestamp.wrapping_sub(first_timestamp) as usize + duration;
            let (pcm, comfort_noise) = self.decode_packet(&packet, duration);
            let decoded_per_channel = pcm.len() / self.channels();
            self.last_decoded = Some((packet.header.sequence_number, packet.header.timestamp));
            self.active_payload_type = Some(packet.header.payload_type);
            if comfort_noise {
                result.comfort_noise = true;
                self.statistics.generated_noise(decoded_per_channel as u64);
            } else if decoded_per_channel == 0 {
                // neteq_impl.cc Decode: on a decoder error the timestamp still
                // moves by one frame and the operation becomes expand.
                self.end_timestamp = packet.header.timestamp.wrapping_add(duration as u32);
            } else {
                result.decoded_any = true;
                self.decoder_frame_length = decoded_per_channel;
                self.end_timestamp = packet
                    .header
                    .timestamp
                    .wrapping_add(decoded_per_channel as u32);
            }
            self.leftover_samples.extend_from_slice(&pcm);
            result.decoded_samples += decoded_per_channel;
            let next_available = self.packet_buffer.peek_next_packet().is_some_and(|n| {
                n.header.payload_type == packet.header.payload_type
                    && n.header.timestamp == packet.header.timestamp.wrapping_add(duration as u32)
            });
            if !(result.samples < required_samples && next_available) {
                break;
            }
        }
        if result.samples > 0 {
            self.packet_buffer
                .discard_old_packets(last_timestamp, 0, &mut self.statistics);
        }
        result
    }

    /// Decode one packet. Returns the samples and whether they are comfort
    /// noise (opus_interface.cc `DetermineAudioType`: a payload of 1 or 2
    /// bytes is comfort noise).
    fn decode_packet(&mut self, packet: &AudioPacket, duration: usize) -> (Vec<f32>, bool) {
        let dtx = packet.is_dtx();
        let Some(decoder) = self.decoders.get_mut(&packet.header.payload_type) else {
            // No decoder: raw little-endian f32 PCM.
            let mut v = Vec::with_capacity(packet.payload.len() / 4);
            for chunk in packet.payload.as_chunks::<4>().0 {
                v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
            }
            return (v, false);
        };
        if dtx {
            if decoder.supports_dtx() {
                if let Ok(pcm) = decoder.decode(&packet.payload) {
                    if !pcm.is_empty() && pcm.iter().all(|x| x.is_finite()) {
                        return (pcm, true);
                    }
                }
            }
            // Deviation: a decoder that cannot take DTX frames (the browser
            // WebCodecs/JS paths) is never handed one; NetEq plays its own
            // quiet noise for the packet's duration instead.
            return (self.quiet_noise(duration), true);
        }
        match decoder.decode(&packet.payload) {
            Ok(pcm) if pcm.iter().all(|x| x.is_finite()) => (pcm, false),
            Ok(_) => {
                log::error!("decoder produced non-finite samples");
                (Vec::new(), false)
            }
            Err(e) => {
                log::error!(
                    "decoder error for pt {}: {:?}",
                    packet.header.payload_type,
                    e
                );
                (Vec::new(), false)
            }
        }
    }

    /// Near-silent noise (the same generator concealment uses).
    fn quiet_noise(&mut self, samples_per_channel: usize) -> Vec<f32> {
        let mut out = vec![0.0; samples_per_channel * self.channels()];
        self.expand.process(&[], &mut out, ExpandPhase::Expand);
        out
    }

    /// neteq_impl.cc `NetEqImpl::DoNormal` / `DoMerge`: play decoded audio.
    /// After concealment the first samples are faded in (normal.cc
    /// `Normal::Process` and merge.cc mix the expansion into the new audio).
    fn do_normal(
        &mut self,
        frame: &mut AudioFrame,
        extracted: &Extracted,
        previous_mode: Mode,
        operation: Operation,
    ) {
        let n = frame.samples.len();
        if self.leftover_samples.len() < n {
            // The decoder came up short: conceal the rest.
            self.do_expand(frame);
            return;
        }
        frame.samples.copy_from_slice(&self.leftover_samples[..n]);
        self.leftover_samples.drain(..n);
        if previous_mode == Mode::Expand {
            let overlap = self.expand_overlap().min(n / self.channels());
            let ch = self.channels();
            for i in 0..overlap {
                let gain = (i + 1) as f32 / (overlap + 1) as f32;
                for c in 0..ch {
                    frame.samples[i * ch + c] *= gain;
                }
            }
        }
        if extracted.comfort_noise {
            self.last_mode = Mode::CodecInternalCng;
        } else if extracted.decoded_any {
            self.last_mode = if operation == Operation::Merge {
                Mode::Merge
            } else {
                Mode::Normal
            };
        }
        // neteq_impl.cc DoNormal: decoded_length == 0 keeps the last mode.
        let cng = self.last_mode == Mode::CodecInternalCng;
        frame.speech_type = if cng {
            SpeechType::Cng
        } else {
            SpeechType::Normal
        };
        frame.vad_activity = !cng;
    }

    fn expand_overlap(&self) -> usize {
        ((self.config.sample_rate as f32 * 0.003) as usize).max(32)
    }

    /// neteq_impl.cc `NetEqImpl::DoExpand`: play what is left of decoded
    /// audio, then concealment. Concealment does not move the playout
    /// timestamp; the generated-noise stopwatch starts if it is not running.
    fn do_expand(&mut self, frame: &mut AudioFrame) {
        let n = frame.samples.len();
        let ch = self.channels();
        let from_leftover = self.leftover_samples.len().min(n);
        frame.samples[..from_leftover].copy_from_slice(&self.leftover_samples[..from_leftover]);
        self.leftover_samples.drain(..from_leftover);
        // Fade the last decoded samples out into the concealment.
        let fade = self.expand_overlap().min(from_leftover / ch);
        for i in 0..fade {
            let gain = (fade - i) as f32 / (fade + 1) as f32;
            for c in 0..ch {
                let idx = from_leftover - fade * ch + i * ch + c;
                frame.samples[idx] *= gain;
            }
        }
        if from_leftover < n {
            self.expand.process(
                &[],
                &mut frame.samples[from_leftover..],
                ExpandPhase::Expand,
            );
        }
        self.statistics
            .concealment_event(frame.samples_per_channel as u64, true);
        self.statistics.time_stretch_operation(
            TimeStretchOperation::Expand,
            frame.samples_per_channel as u64,
        );
        self.last_mode = Mode::Expand;
        if self.noise_start_tick.is_none() {
            self.noise_start_tick = Some(self.tick);
        }
        frame.speech_type = SpeechType::Expand;
        frame.vad_activity = false;
    }

    /// neteq_impl.cc `NetEqImpl::DecodeCng` + `DoCodecInternalCng`:
    /// continue the codec's comfort noise with no packet.
    fn do_codec_internal_cng(&mut self, frame: &mut AudioFrame) {
        let n = frame.samples.len();
        let out = self.output_size();
        let ch = self.channels();
        let mut generated = 0usize;
        while self.leftover_samples.len() < n {
            // Decode straight into the sync buffer's future samples.
            let start = self.leftover_samples.len();
            let decoded = self
                .active_payload_type
                .and_then(|pt| self.decoders.get_mut(&pt))
                .filter(|d| d.supports_dtx())
                .is_some_and(|d| {
                    d.decode_comfort_noise(out, &mut self.leftover_samples)
                        .unwrap_or(false)
                });
            let fresh = &self.leftover_samples[start..];
            if !decoded || fresh.is_empty() || !fresh.iter().all(|x| x.is_finite()) {
                // Deviation for decoders without DTX support: quiet noise.
                self.leftover_samples.truncate(start);
                self.leftover_samples.resize(start + out * ch, 0.0);
                self.expand.process(
                    &[],
                    &mut self.leftover_samples[start..],
                    ExpandPhase::Expand,
                );
            }
            generated += (self.leftover_samples.len() - start) / ch;
        }
        frame.samples.copy_from_slice(&self.leftover_samples[..n]);
        self.leftover_samples.drain(..n);
        self.statistics.generated_noise(generated as u64);
        self.last_mode = Mode::CodecInternalCng;
        frame.speech_type = SpeechType::Cng;
        frame.vad_activity = false;
    }

    /// Interleaved samples of history: enough to borrow 30 ms and to
    /// estimate background noise on `K_VEC_LEN` samples.
    fn history_len(sample_rate: u32, channels: usize) -> usize {
        (sample_rate as usize * 30 / 1000).max(K_VEC_LEN) * channels
    }

    /// Append played audio to the history, keeping its length.
    fn push_history(&mut self, played: &[f32]) {
        let len = self.history.len();
        self.history.extend_from_slice(played);
        let excess = self.history.len() - len;
        self.history.drain(..excess);
    }

    /// The last `n` samples of the first channel of the sync buffer
    /// (history followed by future samples).
    fn reference_channel_tail(&self, n: usize) -> Vec<f32> {
        let ch = self.channels();
        let mut tail: Vec<f32> = self
            .history
            .iter()
            .chain(&self.leftover_samples)
            .step_by(ch)
            .copied()
            .collect();
        let excess = tail.len().saturating_sub(n);
        tail.drain(..excess);
        tail
    }

    /// neteq_impl.cc `DoAccelerate` / `DoPreemptiveExpand`: the input is
    /// the decoded audio, topped up to 30 ms by borrowing from the end of
    /// the sync buffer. Returns the interleaved input and how many samples
    /// per channel of it were already played (`old_borrowed_samples`).
    fn time_stretch_input(&self, decoded_samples: usize) -> (Vec<f32>, usize) {
        let ch = self.channels();
        let required_samples = self.config.sample_rate as usize * 30 / 1000;
        let future = self.leftover_samples.len() / ch;
        let total = self.history.len() / ch + future;
        let input_len = decoded_samples.max(required_samples).min(total);
        let old = input_len.saturating_sub(future);
        let input: Vec<f32> = self.history[self.history.len() - old * ch..]
            .iter()
            .chain(&self.leftover_samples[(future - (input_len - old)) * ch..])
            .copied()
            .collect();
        (input, old)
    }

    /// neteq_impl.cc: copy the stretched audio back over what was taken
    /// (`ReplaceAtIndex` for the borrowed part, `PushBack` for the rest).
    /// Already played samples go back to the history, the rest becomes the
    /// future samples.
    fn put_back_time_stretched(&mut self, input_len: usize, old: usize, output: &[f32]) {
        let ch = self.channels();
        let old_part = (old * ch).min(output.len());
        let history_start = self.history.len() - old * ch;
        self.history[history_start..history_start + old_part].copy_from_slice(&output[..old_part]);
        let keep = self.leftover_samples.len() - (input_len - old) * ch;
        self.leftover_samples.truncate(keep);
        self.leftover_samples.extend_from_slice(&output[old_part..]);
    }

    /// neteq_impl.cc `NetEqImpl::DoAccelerate`.
    fn do_accelerate(&mut self, frame: &mut AudioFrame, extracted: &Extracted, fast: bool) {
        let ch = self.channels();
        let (input, old) = self.time_stretch_input(extracted.decoded_samples);
        let mut output = Vec::with_capacity(input.len());
        let (result, samples_removed) =
            self.accelerate
                .process(&input, fast, &self.background_noise, &mut output);
        self.statistics
            .time_stretch_operation(TimeStretchOperation::Accelerate, samples_removed as u64);
        self.last_mode = match result {
            TimeStretchResult::Success => Mode::AccelerateSuccess,
            TimeStretchResult::SuccessLowEnergy => Mode::AccelerateLowEnergy,
            TimeStretchResult::NoStretch | TimeStretchResult::Error => Mode::AccelerateFail,
        };
        self.put_back_time_stretched(input.len() / ch, old, &output);
        if extracted.comfort_noise {
            self.last_mode = Mode::CodecInternalCng;
        }
        self.play_future(frame);
    }

    /// neteq_impl.cc `NetEqImpl::DoPreemptiveExpand`.
    fn do_preemptive_expand(&mut self, frame: &mut AudioFrame, extracted: &Extracted) {
        let ch = self.channels();
        let (input, old) = self.time_stretch_input(extracted.decoded_samples);
        let mut output = Vec::with_capacity(input.len() * 3 / 2);
        let (result, samples_added) =
            self.preemptive_expand
                .process(&input, old, &self.background_noise, &mut output);
        self.statistics
            .time_stretch_operation(TimeStretchOperation::PreemptiveExpand, samples_added as u64);
        self.last_mode = match result {
            TimeStretchResult::Success => Mode::PreemptiveExpandSuccess,
            TimeStretchResult::SuccessLowEnergy => Mode::PreemptiveExpandLowEnergy,
            TimeStretchResult::NoStretch | TimeStretchResult::Error => Mode::PreemptiveExpandFail,
        };
        self.put_back_time_stretched(input.len() / ch, old, &output);
        if extracted.comfort_noise {
            self.last_mode = Mode::CodecInternalCng;
        }
        self.play_future(frame);
    }

    /// neteq_impl.cc GetAudioInternal: the output frame is the next 10 ms of
    /// the sync buffer. A time-stretch always leaves at least 15 ms.
    fn play_future(&mut self, frame: &mut AudioFrame) {
        let take = frame.samples.len().min(self.leftover_samples.len());
        frame.samples[..take].copy_from_slice(&self.leftover_samples[..take]);
        frame.samples[take..].fill(0.0);
        self.leftover_samples.drain(..take);
        let cng = self.last_mode == Mode::CodecInternalCng;
        frame.speech_type = if cng {
            SpeechType::Cng
        } else {
            SpeechType::Normal
        };
        frame.vad_activity = !cng;
    }

    pub fn current_buffer_size_ms(&self) -> u32 {
        // Use total content duration instead of timestamp span
        (self.current_buffer_size_samples() as u64 * 1000
            / u64::from(self.config.sample_rate)
            / self.channels() as u64) as u32
    }

    /// Get current buffer size in samples
    pub fn current_buffer_size_samples(&self) -> usize {
        self.packet_buffer.num_samples_in_buffer() + self.leftover_samples.len()
    }

    /// Register a decoder for a given RTP payload type.
    pub fn register_decoder(
        &mut self,
        payload_type: u8,
        decoder: Box<dyn crate::codec::AudioDecoder + Send>,
    ) {
        self.decoders.insert(payload_type, decoder);
    }
}

#[cfg(test)]
mod tests {
    use std::thread::sleep;

    use super::*;
    use crate::packet::RtpHeader;

    fn create_test_packet(seq: u16, ts: u32, duration_ms: u32) -> AudioPacket {
        let header = RtpHeader::new(seq, ts, 12345, 96, false);
        // Create payload with test audio data (160 samples * 4 bytes = 640 bytes for 10ms at 16kHz)
        let mut payload = Vec::new();
        for i in 0..160 {
            let sample = (i as f32 / 160.0 * 2.0 * std::f32::consts::PI * 440.0).sin() * 0.1;
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        AudioPacket::new(header, payload, 16000, 1, duration_ms)
    }

    #[test]
    fn test_neteq_creation() {
        let config = NetEqConfig::default();
        let neteq = NetEq::new(config).unwrap();

        assert!(neteq.is_empty());
        assert_eq!(neteq.target_delay_ms(), 80); // Now starts with kStartDelayMs
    }

    #[test]
    fn test_packet_insertion() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let packet = create_test_packet(1, 0, 10);
        neteq.insert_packet(packet).unwrap();

        assert!(!neteq.is_empty());
    }

    #[test]
    fn test_audio_generation() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Insert a few packets
        for i in 0..5 {
            let packet = create_test_packet(i, i as u32 * 160, 10);
            neteq.insert_packet(packet).unwrap();
        }

        // Get audio frame
        let frame = neteq.get_audio().unwrap();
        assert_eq!(frame.sample_rate, 16000);
        assert_eq!(frame.channels, 1);
        assert!(!frame.samples.is_empty());
    }

    #[test]
    fn test_empty_buffer_expansion() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Get audio without inserting packets (should expand)
        let frame = neteq.get_audio().unwrap();
        assert_eq!(frame.speech_type, SpeechType::Expand);
        assert!(!frame.vad_activity);
    }

    #[test]
    fn test_packet_buffer_with_out_of_order_jitter() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Create five consecutive packets (seq 0-5, timestamps 0,160,320,480,640)
        let packets: Vec<AudioPacket> = (0u16..=4)
            .map(|i| create_test_packet(i, i as u32 * 160, 10))
            .collect();

        let mut counter = 0;
        for p in packets {
            counter += 10;
            neteq.insert_packet(p).unwrap();
            assert_eq!(neteq.current_buffer_size_ms(), counter);
        }

        assert_eq!(neteq.current_buffer_size_ms(), 50);

        // NetEQ should deliver speech frames with VAD activity
        let frame = neteq.get_audio().unwrap();
        assert!(frame.vad_activity, "Frame should have VAD activity");

        println!("before stats: {:?}\n", neteq.get_statistics());

        // We want to produce "jitter" in the output, so we need to insert a packet with a different timestamp
        let packet = create_test_packet(5, 1000, 10);
        neteq.insert_packet(packet).unwrap();

        // assert that the jitter is detected
        println!("before jitter: {:?}\n", neteq.get_statistics());

        // add more jitter by inserting a packet with a timestamp that is way ahead of the current timestamp
        let packet = create_test_packet(6, 10000, 10);
        neteq.insert_packet(packet).unwrap();

        // assert that the jitter is detected
        println!("after jitter: {:?}", neteq.get_statistics());

        // get audio frame
    }

    #[test]
    fn test_escalating_packet_delays() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Insert packets with progressively larger real-time delays between arrivals
        let mut seq: u16 = 0;
        let mut ts: u32 = 0;
        // Delays in milliseconds – each entry is the sleep before inserting the next packet
        let delays_ms = [0u64, 10, 30, 70, 120];

        for delay in &delays_ms {
            if *delay > 0 {
                sleep(Duration::from_millis(*delay));
            }
            let packet = create_test_packet(seq, ts, 10);
            neteq.insert_packet(packet).unwrap();
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(160); // 10 ms at 16 kHz
        }

        // Pull a series of audio frames; some should be normal, some should be expands
        let mut expand_frames = 0;
        for _ in 0..(delays_ms.len() + 3) {
            let frame = neteq.get_audio().unwrap();
            if frame.speech_type == SpeechType::Expand {
                expand_frames += 1;
            }
        }

        // We expect some expansion due to the increasing delays
        assert!(expand_frames > 0);
    }

    // ---- Tests of the libwebrtc decision structure (48 kHz, 20 ms packets) ----

    struct MockDecoder;

    impl crate::codec::AudioDecoder for MockDecoder {
        fn sample_rate(&self) -> u32 {
            48_000
        }
        fn channels(&self) -> u8 {
            1
        }
        fn decode(&mut self, encoded: &[u8]) -> Result<Vec<f32>> {
            let level = if encoded.len() <= 2 { 0.001 } else { 0.1 };
            Ok(vec![level; 960])
        }
        fn supports_dtx(&self) -> bool {
            true
        }
        fn decode_comfort_noise(&mut self, n: usize, out: &mut Vec<f32>) -> Result<bool> {
            out.extend(std::iter::repeat_n(0.001, n));
            Ok(true)
        }
    }

    fn neteq48() -> NetEq {
        crate::clock::freeze();
        let mut neteq = NetEq::new(NetEqConfig {
            sample_rate: 48_000,
            channels: 1,
            ..NetEqConfig::default()
        })
        .unwrap();
        neteq.register_decoder(111, Box::new(MockDecoder));
        neteq
    }

    fn opus_like(seq: u16, ts: u32, payload: &[u8]) -> AudioPacket {
        AudioPacket::new(
            RtpHeader::new(seq, ts, 1, 111, false),
            payload.to_vec(),
            48_000,
            1,
            20,
        )
    }

    fn tick(neteq: &mut NetEq) -> AudioFrame {
        crate::clock::advance(Duration::from_millis(10));
        neteq.get_audio().unwrap()
    }

    #[test]
    fn packets_sharing_a_timestamp_are_duplicates() {
        let mut neteq = neteq48();
        for seq in 0..30u16 {
            neteq.insert_packet(opus_like(seq, 1000, &[1; 40])).unwrap();
        }
        assert_eq!(neteq.packet_buffer.len(), 1);
        let normal = (0..10)
            .map(|_| tick(&mut neteq).speech_type)
            .filter(|s| *s == SpeechType::Normal)
            .count();
        assert_eq!(normal, 2, "only one 20 ms packet may play");
    }

    #[test]
    fn first_packet_plays_at_once() {
        // neteq_impl.cc GetDecision: new_codec_ -> reset -> kNormal.
        let mut neteq = neteq48();
        neteq.insert_packet(opus_like(0, 5000, &[1; 40])).unwrap();
        let frame = tick(&mut neteq);
        assert_eq!(neteq.last_operation(), Operation::Normal);
        assert_eq!(frame.speech_type, SpeechType::Normal);
    }

    #[test]
    fn future_packet_waits_until_concealment_reaches_its_timestamp() {
        // decision_logic.cc PacketTooEarly: timestamp_leap > generated noise.
        let mut neteq = neteq48();
        neteq.insert_packet(opus_like(0, 0, &[1; 40])).unwrap();
        tick(&mut neteq);
        tick(&mut neteq);
        neteq.insert_packet(opus_like(2, 1920, &[1; 40])).unwrap(); // seq 1 lost
        let ops: Vec<Operation> = (0..3)
            .map(|_| {
                tick(&mut neteq);
                neteq.last_operation()
            })
            .collect();
        assert_eq!(
            ops,
            vec![Operation::Expand, Operation::Expand, Operation::Merge]
        );
    }

    #[test]
    fn dtx_comfort_noise_continues_for_one_second_then_conceals() {
        let mut neteq = neteq48();
        neteq.insert_packet(opus_like(0, 0, &[1; 40])).unwrap();
        tick(&mut neteq);
        tick(&mut neteq);
        neteq.insert_packet(opus_like(1, 960, &[0x08])).unwrap();
        let mut kinds = Vec::new();
        for _ in 0..103 {
            kinds.push(tick(&mut neteq).speech_type);
        }
        // The DTX frame itself plus 1 s of continued comfort noise.
        assert!(
            kinds[..101].iter().all(|k| *k == SpeechType::Cng),
            "{kinds:?}"
        );
        assert_eq!(kinds[102], SpeechType::Expand);
        let stats = neteq.get_statistics().lifetime;
        assert!(stats.generated_noise_samples >= 48_000);
        assert!(stats.concealed_samples <= 2 * 480);
        // Speech after the pause plays when the playout clock reaches it.
        neteq
            .insert_packet(opus_like(2, 960 + 960 * 51, &[1; 40]))
            .unwrap();
        // decision_logic.cc PostponeDecode: after a noticeable concealment
        // wait until half the 80 ms start target is buffered.
        let waited = (0..6)
            .take_while(|_| {
                tick(&mut neteq);
                neteq.last_operation() == Operation::Expand
            })
            .count();
        assert_eq!(neteq.last_operation(), Operation::Merge);
        assert!(waited <= 4, "waited {waited} ticks");
    }

    /// A NetEq whose next 10 ms plays RTP timestamp 960 with `delay_ms` of
    /// playout delay (decision_logic.cc GetPlayoutDelayMs): two packets
    /// arrived on time 20 ms apart, then `delay_ms` passed. `samples_left`
    /// samples per channel wait in the sync buffer; the timescale countdown
    /// has expired unless `countdown_running`.
    fn neteq_with_playout_delay(
        delay_ms: u64,
        samples_left: usize,
        countdown_running: bool,
    ) -> NetEq {
        let mut neteq = neteq48();
        neteq.first_packet = false;
        neteq.arrival_history.insert(0, 960, crate::clock::now());
        crate::clock::advance(Duration::from_millis(20));
        neteq.arrival_history.insert(960, 960, crate::clock::now());
        crate::clock::advance(Duration::from_millis(delay_ms));
        neteq.now = crate::clock::now();
        neteq.leftover_samples = vec![0.1; samples_left];
        neteq.end_timestamp = 960 + samples_left as u32;
        neteq.tick = 100;
        neteq.timescale_allowed_tick = if countdown_running { 101 } else { 100 };
        neteq
    }

    /// The operation `ExpectedPacketAvailable` picks with the 80 ms start
    /// target. On-time arrivals: high limit = 80 + 0 + 20 = 100 ms.
    fn expected_operation(delay_ms: u64, countdown_running: bool) -> Operation {
        let neteq = neteq_with_playout_delay(delay_ms, 0, countdown_running);
        assert_eq!(neteq.delay_manager.target_delay_ms(), 80);
        neteq.expected_packet_available(80, 0)
    }

    #[test]
    fn expected_packet_thresholds_follow_libwebrtc() {
        // decision_logic.cc ExpectedPacketAvailable.
        // >= 4 * high limit: fast accelerate, even inside the countdown.
        assert_eq!(expected_operation(400, false), Operation::FastAccelerate);
        assert_eq!(expected_operation(400, true), Operation::FastAccelerate);
        // >= high limit: accelerate, once the countdown has expired.
        assert_eq!(expected_operation(399, false), Operation::Accelerate);
        assert_eq!(expected_operation(100, false), Operation::Accelerate);
        assert_eq!(expected_operation(150, true), Operation::Normal);
        // Between the target and the high limit: normal.
        assert_eq!(expected_operation(99, false), Operation::Normal);
        assert_eq!(expected_operation(80, false), Operation::Normal);
        // Below the target: preemptive expand, once the countdown expired.
        assert_eq!(expected_operation(79, false), Operation::PreemptiveExpand);
        assert_eq!(expected_operation(0, false), Operation::PreemptiveExpand);
        assert_eq!(expected_operation(0, true), Operation::Normal);
    }

    #[test]
    fn no_time_stretch_right_after_concealment() {
        let mut neteq = neteq_with_playout_delay(150, 0, false);
        neteq.last_mode = Mode::Expand;
        assert_eq!(neteq.expected_packet_available(80, 0), Operation::Normal);
    }

    #[test]
    fn dtx_packets_are_never_time_stretched() {
        // neteq_impl.cc GetDecision: a DTX packet next in line turns any
        // time-stretch into kNormal. 400 ms of playout delay with 40 ms in
        // the sync buffer is a fast accelerate for speech.
        let decide = |payload: &[u8]| {
            let mut neteq = neteq_with_playout_delay(400, 1920, false);
            let ts = neteq.end_timestamp;
            neteq
                .packet_buffer
                .insert_packet(opus_like(5, ts, payload), &mut neteq.statistics, 80)
                .unwrap();
            neteq.get_decision().unwrap().0
        };
        assert_eq!(decide(&[1; 40]), Operation::FastAccelerate);
        assert_eq!(decide(&[0x08]), Operation::Normal);
    }

    #[test]
    fn on_time_stream_builds_its_buffer_by_preemptive_expand() {
        let mut neteq = neteq48();
        let mut ops = Vec::new();
        for k in 0..50u16 {
            neteq
                .insert_packet(opus_like(k, u32::from(k) * 960, &[1; 40]))
                .unwrap();
            for _ in 0..2 {
                tick(&mut neteq);
                ops.push(neteq.last_operation());
            }
        }
        assert!(ops.contains(&Operation::PreemptiveExpand), "{ops:?}");
        assert!(!ops[4..].contains(&Operation::Expand), "{ops:?}");
    }

    #[test]
    fn sender_restart_starts_a_new_stream() {
        let mut neteq = neteq48();
        for k in 0..5u16 {
            neteq
                .insert_packet(opus_like(100 + k, 1_000_000 + u32::from(k) * 960, &[1; 40]))
                .unwrap();
            tick(&mut neteq);
            tick(&mut neteq);
        }
        // New random bases, timestamps going backwards.
        neteq.insert_packet(opus_like(9_000, 7, &[1; 40])).unwrap();
        let frame = tick(&mut neteq);
        assert_eq!(neteq.last_operation(), Operation::Normal);
        assert_eq!(frame.speech_type, SpeechType::Normal);
    }

    #[test]
    fn a_restart_does_not_flush_the_new_streams_first_packets() {
        let mut neteq = neteq48();
        for k in 0..5u16 {
            neteq
                .insert_packet(opus_like(100 + k, 1_000_000 + u32::from(k) * 960, &[1; 40]))
                .unwrap();
            tick(&mut neteq);
            tick(&mut neteq);
        }
        // New bases, two packets before the next decode.
        neteq.insert_packet(opus_like(9_000, 7, &[1; 40])).unwrap();
        neteq
            .insert_packet(opus_like(9_001, 967, &[1; 40]))
            .unwrap();
        assert_eq!(neteq.packet_buffer.len(), 2);
    }

    #[test]
    fn small_backward_jump_is_obsolete_until_it_catches_up() {
        // Sequence numbers and timestamps both go back, by 20 packets and
        // 400 ms: no restart is detected. libwebrtc behaviour: packets
        // older than the playout timestamp are obsolete (neteq_impl.cc
        // GetDecision DiscardOldPackets), concealment does not move the
        // playout timestamp, so audio resumes once the sender's timestamps
        // pass the point where playout stopped.
        let mut neteq = neteq48();
        let mut seq = 0u16;
        let mut ts = 0u32;
        for _ in 0..50 {
            neteq.insert_packet(opus_like(seq, ts, &[1; 40])).unwrap();
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(960);
            tick(&mut neteq);
            tick(&mut neteq);
        }
        seq = seq.wrapping_sub(20);
        ts = ts.wrapping_sub(20 * 960);
        let mut speech = Vec::new();
        for _ in 0..60 {
            neteq.insert_packet(opus_like(seq, ts, &[1; 40])).unwrap();
            seq = seq.wrapping_add(1);
            ts = ts.wrapping_add(960);
            for _ in 0..2 {
                speech.push(tick(&mut neteq).speech_type);
                assert!(neteq.packet_buffer.len() <= 3);
            }
        }
        let first_expand = speech
            .iter()
            .position(|s| *s == SpeechType::Expand)
            .expect("the obsolete packets are concealed");
        let first_normal = first_expand
            + speech[first_expand..]
                .iter()
                .position(|s| *s == SpeechType::Normal)
                .expect("audio resumes");
        // 400 ms of obsolete packets, plus up to the 80 ms start target and
        // one packet.
        assert!(
            (first_normal - first_expand) * 10 <= 400 + 80 + 20,
            "{speech:?}"
        );
        assert!(
            speech[first_normal..]
                .iter()
                .all(|s| *s == SpeechType::Normal),
            "{speech:?}"
        );
    }

    #[test]
    fn empty_payload_is_rejected() {
        let mut neteq = neteq48();
        assert!(neteq.insert_packet(opus_like(0, 0, &[])).is_err());
        assert!(neteq.is_empty());
    }
}
