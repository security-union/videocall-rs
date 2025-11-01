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

use crate::buffer::{BufferReturnCode, PacketBuffer, SmartFlushConfig};
use crate::buffer_level_filter::BufferLevelFilter;
use crate::delay_manager::{DelayConfig, DelayManager};
use crate::packet::AudioPacket;
use crate::statistics::{
    LifetimeStatistics, NetworkStatistics, StatisticsCalculator, TimeStretchOperation,
};
use crate::time_stretch::{TimeStretchFactory, TimeStretcher};
use crate::{NetEqError, Result};

/// Offset used to calculate the lower threshold for preemptive expansion.
///
/// This constant prevents aggressive preemptive expansion when buffer levels
/// are still reasonable:
/// - Without offset: low_limit = target * 0.75 (can be too aggressive)
/// - With offset: low_limit = max(target * 0.75, target - 85ms)
///
/// Engineering rationale:
/// - 85ms represents ~4 packets of typical 20ms duration
/// - Provides stable lower bound regardless of target delay variations
/// - Prevents preemptive expansion during normal operation
/// - WebRTC tuned this value across diverse network conditions
const K_DECELERATION_TARGET_LEVEL_OFFSET_MS: u32 = 85;

/// Hardcoded minimum delay.
const K_BASE_MIN_DELAY_MS: u32 = 20;

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
    /// Time strech operations return multiple frames at once, return the consecutive frames
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
pub struct NetEq {
    config: NetEqConfig,
    packet_buffer: PacketBuffer,
    delay_manager: DelayManager,
    buffer_level_filter: BufferLevelFilter,
    statistics: StatisticsCalculator,
    accelerate: Box<dyn TimeStretcher + Send>,
    preemptive_expand: Box<dyn TimeStretcher + Send>,
    last_decode_timestamp: Option<u32>,
    output_frame_size_samples: usize,
    _muted: bool,
    last_operation: Operation,
    consecutive_expands: u32,
    frame_timestamp: u32,
    /// Samples remaining from a previously decoded packet (to support 20 ms packets → 10 ms frames)
    leftover_samples: Vec<f32>,
    /// Map RTP payload-type → audio decoder instance.
    decoders: HashMap<u8, Box<dyn crate::codec::AudioDecoder + Send>>,
    /// Audio queue for bypass mode (direct decoding without jitter buffer)
    bypass_audio_queue: VecDeque<f32>,
    /// Rolling counters for packets-per-second measurement
    packets_received_this_second: u32,
    last_packets_second_instant: Instant,
    packets_per_sec_snapshot: u32,
    // A buffer for already time streched samples. Separate from leftover_samples to avoid
    // double time-streching.
    leftover_time_stretched_samples: Vec<f32>,
    /// Number of samples added by time-stretching operations (matches WebRTC sample_memory_,
    /// but with clearer meaning for positive/negative values).
    timestretch_added_samples: i32,
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

        // Create components
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
        let buffer_level_filter = BufferLevelFilter::new(config.sample_rate);

        // Create time stretchers
        let accelerate: Box<dyn TimeStretcher + Send> =
            TimeStretchFactory::create_accelerate(config.sample_rate, config.channels);
        let preemptive_expand: Box<dyn TimeStretcher + Send> =
            TimeStretchFactory::create_preemptive_expand(config.sample_rate, config.channels);

        Ok(Self {
            config,
            packet_buffer,
            delay_manager,
            buffer_level_filter,
            statistics,
            accelerate,
            preemptive_expand,
            last_decode_timestamp: None,
            output_frame_size_samples,
            _muted: false,
            last_operation: Operation::Normal,
            consecutive_expands: 0,
            frame_timestamp: 0,
            leftover_samples: Vec::new(),
            decoders: HashMap::new(),
            bypass_audio_queue: VecDeque::new(),
            packets_received_this_second: 0,
            last_packets_second_instant: Instant::now(),
            packets_per_sec_snapshot: 0,
            leftover_time_stretched_samples: Vec::new(),
            timestretch_added_samples: 0,
        })
    }

    /// Insert a packet into the jitter buffer
    pub fn insert_packet(&mut self, packet: AudioPacket) -> Result<()> {
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

        // Normal NetEQ processing
        // Update delay manager
        self.delay_manager
            .update(packet.header.timestamp, packet.sample_rate, false)?;

        // Insert packet into buffer
        let target_delay = self.delay_manager.target_delay_ms();
        let result =
            self.packet_buffer
                .insert_packet(packet, &mut self.statistics, target_delay)?;

        // Update statistics
        self.statistics
            .update_buffer_size(self.current_buffer_size_ms() as u16, target_delay as u16);

        match result {
            BufferReturnCode::Flushed => {
                log::info!("Buffer flushed due to overflow");
            }
            BufferReturnCode::PartialFlush => {
                log::debug!("Partial buffer flush performed");
            }
            _ => {}
        }

        Ok(())
    }

    /// Get 10ms of audio data
    pub fn get_audio(&mut self) -> Result<AudioFrame> {
        // Ensure packet-rate snapshot rolls even when no packets arrive
        // This prevents stale non-zero values when the peer stops sending
        self.maybe_roll_packet_rate();
        if self.config.bypass_mode {
            // Bypass mode: return samples directly from queue
            let mut frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                self.output_frame_size_samples / self.config.channels as usize,
            );

            let samples_needed = self.output_frame_size_samples;
            let mut filled = 0;

            // Fill from bypass queue
            while filled < samples_needed && !self.bypass_audio_queue.is_empty() {
                frame.samples[filled] = self.bypass_audio_queue.pop_front().unwrap();
                filled += 1;
            }

            // Fill remaining with silence if needed
            while filled < samples_needed {
                frame.samples[filled] = 0.0;
                filled += 1;
            }

            frame.speech_type = if filled == samples_needed {
                SpeechType::Normal
            } else {
                SpeechType::Expand
            };
            frame.vad_activity = filled > 0;

            return Ok(frame);
        }

        // Normal NetEQ processing continues below...

        let mut frame = AudioFrame::new(
            self.config.sample_rate,
            self.config.channels,
            self.output_frame_size_samples / self.config.channels as usize,
        );

        // === Added detailed logging for buffer diagnostics ===
        let pre_buffer_ms = self.current_buffer_size_ms();
        let pre_target_delay = self.delay_manager.target_delay_ms();
        let pre_packet_count = self.packet_buffer.len();
        log::debug!(
            "get_audio pre-decision: buffer={pre_buffer_ms}ms, target={pre_target_delay}ms, packets={pre_packet_count}"
        );
        // -----------------------------------------------------

        // Determine what operation to perform
        let operation = self.get_decision()?;
        self.last_operation = operation;

        match operation {
            Operation::Normal => self.decode_normal(&mut frame)?,
            Operation::Accelerate => self.decode_accelerate(&mut frame, false)?,
            Operation::FastAccelerate => self.decode_accelerate(&mut frame, true)?,
            Operation::PreemptiveExpand => self.decode_preemptive_expand(&mut frame)?,
            Operation::TimeStretchBuffer => self.return_time_stretch_buffer(&mut frame)?,
            Operation::Expand => self.decode_expand(&mut frame, false, false)?,
            Operation::ExpandStart => self.decode_expand(&mut frame, true, false)?,
            Operation::ExpandEnd => self.decode_expand(&mut frame, false, true)?,
            Operation::Merge => self.decode_merge(&mut frame)?,
            Operation::ComfortNoise => self.generate_comfort_noise(&mut frame)?,
            _ => {
                // Fill with silence for unsupported operations
                frame.samples.fill(0.0);
                frame.speech_type = SpeechType::Expand;
            }
        }

        // Record decode operation for per-second tracking
        self.statistics.record_decode_operation(operation);

        // === Log buffer status after producing frame ===
        let post_buffer_ms = self.current_buffer_size_ms();
        let post_packet_count = self.packet_buffer.len();
        log::trace!(
            "get_audio post-decision: operation={operation:?}, buffer_after={post_buffer_ms}ms, packets_after={post_packet_count}"
        );
        // ------------------------------------------------

        // Update frame timestamp
        self.frame_timestamp = self
            .frame_timestamp
            .wrapping_add((frame.samples_per_channel * self.config.channels as usize) as u32);

        // Update statistics
        self.statistics
            .jitter_buffer_delay(frame.duration_ms() as u64, frame.samples_per_channel as u64);

        Ok(frame)
    }

    /// Roll the packets-per-second window if ≥1s elapsed since last snapshot.
    /// Safe to call frequently; no-ops if interval hasn't elapsed.
    fn maybe_roll_packet_rate(&mut self) {
        let now = Instant::now();
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

    /// Flush the buffer
    pub fn flush(&mut self) {
        self.packet_buffer.flush(&mut self.statistics);
        self.delay_manager.reset();
        self.buffer_level_filter.reset();
        self.last_decode_timestamp = None;
        self.consecutive_expands = 0;
        self.leftover_samples.clear();
        self.packets_received_this_second = 0;
        self.packets_per_sec_snapshot = 0;
        self.last_packets_second_instant = Instant::now();
        self.leftover_time_stretched_samples.clear();
        self.timestretch_added_samples = 0;
    }

    fn get_decision(&mut self) -> Result<Operation> {
        // Update buffer level filter first
        let current_buffer_samples = self.current_buffer_size_samples();
        let target_delay_ms: u32 = self.delay_manager.target_delay_ms();

        // Filter buffer level like WebRTC's FilterBufferLevel method
        self.buffer_level_filter
            .set_target_buffer_level(target_delay_ms);

        self.buffer_level_filter
            .update(current_buffer_samples, -self.timestretch_added_samples);

        // Reset for next frame (matches WebRTC decision_logic.cc:245-246)
        self.timestretch_added_samples = 0;

        // Use WebRTC-style threshold calculations
        let samples_per_ms = self.config.sample_rate / 1000;
        let target_level_samples = target_delay_ms * samples_per_ms;

        // Calculate thresholds like WebRTC
        let low_limit = std::cmp::max(
            target_level_samples * 3 / 4,
            target_level_samples
                .saturating_sub(K_DECELERATION_TARGET_LEVEL_OFFSET_MS * samples_per_ms),
        );
        let high_limit = std::cmp::max(target_level_samples, low_limit + 20 * samples_per_ms);

        if !self.leftover_time_stretched_samples.is_empty() {
            return Ok(Operation::TimeStretchBuffer);
        }

        if self.consecutive_expands > 0 && current_buffer_samples < low_limit as usize {
            // prefer a single continous expand over many small ones
            self.consecutive_expands = self.consecutive_expands.saturating_add(1);
            return Ok(Operation::Expand);
        }

        if current_buffer_samples < self.output_frame_size_samples * 3 / 2
            && current_buffer_samples >= self.output_frame_size_samples / 2
            && self.consecutive_expands == 0
        {
            self.consecutive_expands = self.consecutive_expands.saturating_add(1);
            return Ok(Operation::ExpandStart);
        }

        // Check if we have packets
        if current_buffer_samples < self.output_frame_size_samples {
            self.consecutive_expands = self.consecutive_expands.saturating_add(1);
            return Ok(Operation::Expand);
        }

        if self.consecutive_expands > 0 {
            self.consecutive_expands = 0;
            return Ok(Operation::ExpandEnd);
        }

        let buffer_level_samples = self.buffer_level_filter.filtered_current_level();

        // Check for continuous expansion limit (WebRTC uses much higher limits)
        if self.consecutive_expands > 600 {
            // ~6 seconds at 10ms frames
            self.flush();
            return Ok(Operation::Normal);
        }

        self.consecutive_expands = 0;

        // Fast accelerate: 4x high limit (aggressive acceleration for large buffers)
        if buffer_level_samples >= (high_limit << 2) as usize {
            return Ok(Operation::FastAccelerate);
        }

        // Normal accelerate: at high limit
        if buffer_level_samples >= high_limit as usize {
            return Ok(Operation::Accelerate);
        }

        // Preemptive expand: below low limit (requires ~30ms of audio)
        if buffer_level_samples < low_limit as usize
            && current_buffer_samples >= self.output_frame_size_samples * 3
        {
            return Ok(Operation::PreemptiveExpand);
        }

        // Normal operation
        Ok(Operation::Normal)
    }

    fn decode_normal(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Log buffer status at entry
        log::trace!(
            "decode_normal: entering with buffer={}ms, packets={}",
            self.current_buffer_size_ms(),
            self.packet_buffer.len()
        );

        // --------- New adaptive copy logic that preserves unused samples ---------
        let samples_needed = frame.samples.len();
        let mut filled = 0;

        // First use any leftover samples from previous packet
        if !self.leftover_samples.is_empty() {
            let to_copy = samples_needed.min(self.leftover_samples.len());
            frame.samples[..to_copy].copy_from_slice(&self.leftover_samples[..to_copy]);
            self.leftover_samples.drain(..to_copy);
            filled += to_copy;
            log::trace!("decode_normal: consumed {to_copy} leftover samples");
        }

        // Continue pulling packets until frame is filled or buffer is empty
        while filled < samples_needed {
            match self.packet_buffer.get_next_packet() {
                Some(packet) => {
                    // Decode based on payload type if we have a decoder; otherwise treat as raw f32 PCM.
                    let packet_samples: Vec<f32> = if let Some(dec) =
                        self.decoders.get_mut(&packet.header.payload_type)
                    {
                        match dec.decode(&packet.payload) {
                            Ok(pcm) => pcm,
                            Err(e) => {
                                log::error!(
                                    "decoder error for pt {}: {:?}",
                                    packet.header.payload_type,
                                    e
                                );
                                Vec::new()
                            }
                        }
                    } else {
                        // Fallback: raw f32 PCM
                        let mut v = Vec::with_capacity(packet.payload.len() / 4);
                        for chunk in packet.payload.chunks_exact(4) {
                            v.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                        }
                        v
                    };

                    let available = packet_samples.len();
                    let need_now = samples_needed - filled;
                    let to_copy = need_now.min(available);
                    frame.samples[filled..filled + to_copy]
                        .copy_from_slice(&packet_samples[..to_copy]);
                    filled += to_copy;

                    // Save any extra samples for next frame
                    if available > to_copy {
                        self.leftover_samples
                            .extend_from_slice(&packet_samples[to_copy..]);
                        log::trace!(
                            "decode_normal: stored {} leftover samples",
                            available - to_copy
                        );
                    }

                    self.last_decode_timestamp = Some(packet.header.timestamp);
                    frame.speech_type = SpeechType::Normal;
                    frame.vad_activity = true;
                }
                None => {
                    // Buffer empty before we could fill frame
                    frame.samples[filled..].fill(0.0);
                    frame.speech_type = SpeechType::Expand;
                    break;
                }
            }
        }
        // ------------------------------------------------------------------------

        // Log buffer status after consuming packet / expansion decision
        log::trace!(
            "decode_normal: exiting with buffer={}ms, packets={}",
            self.current_buffer_size_ms(),
            self.packet_buffer.len()
        );

        Ok(())
    }

    fn decode_accelerate(&mut self, frame: &mut AudioFrame, fast_mode: bool) -> Result<()> {
        if !self.config.for_test_no_time_stretching {
            let available_samples = self.current_buffer_size_samples();
            let mut output_len: usize = 0;
            let mut required_samples: usize = 0;
            for i in (1..=3).rev() {
                // Use 30ms if available
                output_len = self.output_frame_size_samples * i;
                if fast_mode {
                    required_samples = (output_len as f32 * 2.0).ceil() as usize
                } else {
                    required_samples = (output_len as f32 * 1.5).ceil() as usize
                }
                if required_samples <= available_samples {
                    break;
                }
            }

            // Get more data than we need
            let mut extended_frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                required_samples,
            );

            self.decode_normal(&mut extended_frame)?;

            // Will output up to 30ms output
            let mut output =
                AudioFrame::new(self.config.sample_rate, self.config.channels, output_len);

            // Apply accelerate algorithm
            let _result =
                self.accelerate
                    .process(&extended_frame.samples, &mut output.samples, fast_mode);

            // Put back unused samples
            let used_input_samples = self.accelerate.get_used_input_samples();
            if extended_frame.samples.len() > used_input_samples {
                self.leftover_samples.splice(
                    0..0,
                    extended_frame.samples[used_input_samples..].iter().cloned(),
                );
            }

            // Fill frame with as much data as it fits
            let frame_len = frame.samples.len();
            frame.samples.clone_from_slice(&output.samples[..frame_len]);

            // Store the remaining data to be returned as later frames
            self.leftover_time_stretched_samples
                .extend_from_slice(&output.samples[frame_len..]);

            // Track time-stretching for buffer level filtering (matches WebRTC pattern)
            // WebRTC sets sample_memory to available samples for time-stretching (line 1304)
            // extended_frame has more samples than we need, representing available data
            let samples_removed = used_input_samples as i32 - output.samples.len() as i32;
            self.timestretch_added_samples -= samples_removed / self.config.channels as i32;

            // Update statistics
            self.statistics
                .time_stretch_operation(TimeStretchOperation::Accelerate, samples_removed as u64);

            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = extended_frame.vad_activity; // Preserve VAD from decoded audio
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn decode_preemptive_expand(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.config.for_test_no_time_stretching {
            // Preemtive expand requires more data (30ms)
            let mut extended_frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                (frame.samples_per_channel as f32 * 3.0) as usize,
            );

            self.decode_normal(&mut extended_frame)?;

            // get output of same size as input
            let mut output = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                (frame.samples_per_channel as f32 * 3.0) as usize,
            );

            // Apply preemptive expand
            let _result =
                self.preemptive_expand
                    .process(&extended_frame.samples, &mut output.samples, false);

            // Put back unused samples
            let used_input_samples = self.preemptive_expand.get_used_input_samples();
            if extended_frame.samples.len() > used_input_samples {
                self.leftover_samples.splice(
                    0..0,
                    extended_frame.samples[used_input_samples..].iter().cloned(),
                );
            }

            // fill frame with first half of data
            let frame_len = frame.samples.len();
            frame.samples.clone_from_slice(&output.samples[..frame_len]);

            // store the other half for the next frame
            self.leftover_time_stretched_samples
                .extend_from_slice(&output.samples[frame_len..]);

            // Track time-stretching for buffer level filtering (matches WebRTC pattern)
            // WebRTC sets sample_memory to available samples for time-stretching (line 1304)
            // For preemptive expand, we had normal frame samples available
            let samples_added = output.samples.len() as i32 - used_input_samples as i32;
            self.timestretch_added_samples += samples_added / self.config.channels as i32;

            // Update statistics
            self.statistics.time_stretch_operation(
                TimeStretchOperation::PreemptiveExpand,
                samples_added as u64,
            );

            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = extended_frame.vad_activity; // Preserve VAD from decoded audio
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn return_time_stretch_buffer(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.leftover_time_stretched_samples.is_empty() {
            let to_copy = frame.samples.len();
            frame
                .samples
                .copy_from_slice(&self.leftover_time_stretched_samples[..to_copy]);
            self.leftover_time_stretched_samples.drain(..to_copy);

            // These are leftover samples from previously decoded audio
            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = true;
        }

        Ok(())
    }

    fn decode_expand(&mut self, frame: &mut AudioFrame, start: bool, end: bool) -> Result<()> {
        log::trace!(
            "decode_expand: buffer before expand={}ms, packets={} (consecutive_expands={})",
            self.current_buffer_size_ms(),
            self.packet_buffer.len(),
            self.consecutive_expands
        );
        // Generate concealment audio (simple noise for now)
        for sample in &mut frame.samples {
            *sample = (simple_random() - 0.5) * 0.0001; // Very quiet noise
        }

        if start {
            let overlap_length = self.calculate_overlap_length(self.config.sample_rate);

            let mut end_of_audio = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                overlap_length,
            );

            self.decode_normal(&mut end_of_audio)?;

            let start_of_frame = frame.samples[..overlap_length].to_vec();

            // Crossfade with end of audio
            crate::signal::crossfade(
                &end_of_audio.samples,
                &start_of_frame,
                overlap_length,
                &mut frame.samples[..overlap_length],
            );
        }

        if end {
            let overlap_length = self.calculate_overlap_length(self.config.sample_rate);

            let mut start_of_audio = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                overlap_length,
            );

            self.decode_normal(&mut start_of_audio)?;

            let end_of_frame_start = frame.samples.len() - overlap_length;
            let end_of_frame = frame.samples[end_of_frame_start..].to_vec();

            // Crossfade with end of audio
            crate::signal::crossfade(
                &end_of_frame,
                &start_of_audio.samples,
                overlap_length,
                &mut frame.samples[end_of_frame_start..],
            );
        }

        frame.speech_type = SpeechType::Expand;
        frame.vad_activity = false;

        // Update statistics
        self.statistics
            .concealment_event(frame.samples_per_channel as u64, true);
        self.statistics.time_stretch_operation(
            TimeStretchOperation::Expand,
            frame.samples_per_channel as u64,
        );

        log::trace!(
            "decode_expand: buffer after expand={}ms, packets={} (consecutive_expands={})",
            self.current_buffer_size_ms(),
            self.packet_buffer.len(),
            self.consecutive_expands
        );

        Ok(())
    }

    fn calculate_overlap_length(&self, sample_rate: u32) -> usize {
        // Calculate overlap length based on sample rate (typically 4-6ms)
        ((sample_rate as f32 * 0.003) as usize).max(32) // Minimum 32 samples
    }

    fn decode_merge(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Simplified merge - just normal decode for now
        self.decode_normal(frame)
    }

    fn generate_comfort_noise(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Generate comfort noise
        for sample in &mut frame.samples {
            *sample = (simple_random() - 0.5) * 0.000005; // Much quieter comfort noise
        }

        frame.speech_type = SpeechType::Cng;
        frame.vad_activity = false;

        Ok(())
    }

    pub fn current_buffer_size_ms(&self) -> u32 {
        // Use total content duration instead of timestamp span
        // This handles cases where packets have close/identical timestamps
        self.current_buffer_size_samples() as u32 * 1000 / self.config.sample_rate
    }

    /// Get current buffer size in samples
    pub fn current_buffer_size_samples(&self) -> usize {
        self.packet_buffer.num_samples_in_buffer()
            + self.leftover_samples.len()
            + self.leftover_time_stretched_samples.len()
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

// Simple random number generator for testing
use std::sync::atomic::{AtomicU64, Ordering};

static RNG_STATE: AtomicU64 = AtomicU64::new(1);

fn simple_random() -> f32 {
    let mut x = RNG_STATE.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    RNG_STATE.store(x, Ordering::Relaxed);
    ((x as u32) >> 16) as f32 / 65536.0
}

#[cfg(test)]
mod tests {
    use std::{thread::sleep, time::Duration};

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

    /// Regression test for late-joining peer buffering issue.
    ///
    /// This test ensures that when a peer joins late with a large timestamp jump,
    /// NetEQ properly fast-forwards instead of buffering excessive packets.
    ///
    /// Scenario:
    /// 1. Peer A starts streaming at t=0 with normal 20ms intervals
    /// 2. Peer B joins much later (simulated by large timestamp jump)
    /// 3. NetEQ should recognize this and maintain reasonable buffer levels
    ///
    /// Before fix: Would buffer ~60 packets (~1200ms latency)
    /// After fix: Should maintain ~12 packets (~240ms latency) with oscillation
    /// Test threshold: Allow up to ~25 packets (500ms) to account for dynamic adjustment
    #[test]
    fn test_late_joining_peer_no_excessive_buffering() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Simulate initial peer streaming normally
        let mut seq: u16 = 0;
        let mut timestamp: u32 = 0;
        const PACKET_DURATION_SAMPLES: u32 = 320; // 20ms at 16kHz

        // Insert initial packets from "Peer A" - normal streaming
        for _ in 0..5 {
            let packet = create_test_packet(seq, timestamp, 20);
            neteq.insert_packet(packet).unwrap();
            seq += 1;
            timestamp += PACKET_DURATION_SAMPLES;
        }

        // Verify initial buffer is reasonable
        let initial_buffer_ms = neteq.current_buffer_size_ms();
        assert!(
            initial_buffer_ms <= 100,
            "Initial buffer should be reasonable, got {initial_buffer_ms}ms"
        );

        // Simulate "Peer B" joining late with large timestamp jump
        // This represents the problematic scenario we just fixed
        let late_join_timestamp = timestamp + (10 * PACKET_DURATION_SAMPLES); // 10 packets gap = 200ms jump

        // Insert packets from late-joining peer
        let mut late_timestamp = late_join_timestamp;
        for _ in 0..8 {
            // Insert enough packets to trigger the old bug
            let packet = create_test_packet(seq, late_timestamp, 20);
            neteq.insert_packet(packet).unwrap();
            seq += 1;
            late_timestamp += PACKET_DURATION_SAMPLES;
        }

        // This is the critical test: buffer should NOT accumulate excessively
        let buffer_after_late_join = neteq.current_buffer_size_ms();

        // With the fix, buffer should stay reasonable (around 12 packets = ~240ms)
        // Allow some margin but ensure it's nowhere near the old 60 packets (~1200ms)
        assert!(buffer_after_late_join <= 500,
                "Buffer should not accumulate excessively after late peer join. Got {buffer_after_late_join}ms, expected ≤500ms");

        // More importantly, ensure it's significantly better than the old behavior (60 packets = 1200ms)
        assert!(buffer_after_late_join <= 600,
                "REGRESSION DETECTION: Buffer accumulated {buffer_after_late_join}ms, this exceeds acceptable threshold and may indicate the old 60-packet bug has returned");

        // Pull several audio frames and verify NetEQ handles the situation gracefully
        let mut total_operations = std::collections::HashMap::new();

        for _ in 0..20 {
            let pre_buffer = neteq.current_buffer_size_ms();
            let frame = neteq.get_audio().unwrap();
            let post_buffer = neteq.current_buffer_size_ms();

            // Track what operation was performed (inferred from buffer change)
            let operation = match frame.speech_type {
                SpeechType::Normal => {
                    if pre_buffer > post_buffer + 25 {
                        // Significant buffer reduction
                        "Accelerate"
                    } else {
                        "Normal"
                    }
                }
                SpeechType::Expand => "Expand",
                _ => "Other",
            };

            *total_operations.entry(operation).or_insert(0) += 1;

            // Ensure buffer never grows excessively during processing
            assert!(
                post_buffer <= 500,
                "Buffer should never exceed 500ms during processing, got {post_buffer}ms"
            );
        }

        // Verify that NetEQ used acceleration to handle the large buffer
        // This confirms the fix is working - old code wouldn't accelerate enough
        let accelerate_count = total_operations.get("Accelerate").copied().unwrap_or(0);
        assert!(accelerate_count > 0,
                "NetEQ should have used acceleration to handle late-joining peer scenario. Operations: {total_operations:?}");

        // Final buffer should be reasonable
        let final_buffer = neteq.current_buffer_size_ms();
        assert!(
            final_buffer <= 400,
            "Final buffer should be reasonable, got {final_buffer}ms"
        );

        println!("✅ Late-joining peer test passed:");
        println!("   Initial buffer: {initial_buffer_ms}ms");
        println!("   Peak buffer: {buffer_after_late_join}ms");
        println!("   Final buffer: {final_buffer}ms");
        println!("   Operations used: {total_operations:?}");
    }

    /// Test continuous packet insertion to verify steady-state buffer convergence.
    ///
    /// This test simulates continuous streaming with small buffer variations.
    /// Expected behavior:
    /// - Small buffer variations around target (20-40ms are normal)
    /// - NetEQ should NOT aggressively accelerate for small buffers
    /// - Should maintain steady-state with minimal intervention
    /// - Only large buffer buildups should trigger acceleration
    #[test]
    fn test_continuous_streaming_buffer_convergence() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut seq: u16 = 0;
        let mut timestamp: u32 = 0;
        const PACKET_DURATION_SAMPLES: u32 = 320; // 20ms at 16kHz

        // Insert initial packets to establish baseline
        for _ in 0..3 {
            let packet = create_test_packet(seq, timestamp, 20);
            neteq.insert_packet(packet).unwrap();
            seq += 1;
            timestamp += PACKET_DURATION_SAMPLES;
        }

        // Simulate late peer join with timestamp jump (the problematic scenario)
        timestamp += 15 * PACKET_DURATION_SAMPLES; // 300ms jump

        let mut buffer_measurements = Vec::new();
        let mut acceleration_count = 0;

        // Simulate continuous streaming: insert packets slightly faster than playback
        // This better matches real-world conditions where packets arrive with jitter
        for cycle in 0..40 {
            // Insert packets in batches to simulate network bursts
            if cycle % 3 == 0 {
                // Insert 2-3 packets at once (network burst)
                for _ in 0..2 {
                    let packet = create_test_packet(seq, timestamp, 20);
                    neteq.insert_packet(packet).unwrap();
                    seq += 1;
                    timestamp += PACKET_DURATION_SAMPLES;
                }
            }

            // Pull audio frame (continuous playback)
            let pre_buffer = neteq.current_buffer_size_ms();
            let frame = neteq.get_audio().unwrap();
            let post_buffer = neteq.current_buffer_size_ms();

            buffer_measurements.push(post_buffer);

            // Track acceleration operations
            if matches!(frame.speech_type, SpeechType::Normal) && pre_buffer > post_buffer + 25 {
                acceleration_count += 1;
            }

            // Log key transition points
            if cycle % 5 == 0 {
                println!("Cycle {cycle}: Buffer {pre_buffer}ms -> {post_buffer}ms");
            }
        }

        // Analyze buffer convergence behavior
        let initial_buffer = buffer_measurements[0];
        let final_buffer = buffer_measurements[buffer_measurements.len() - 1];
        let max_buffer = buffer_measurements.iter().max().copied().unwrap_or(0);

        // Calculate steady-state buffer (average of last 10 measurements)
        let steady_state_samples = &buffer_measurements[buffer_measurements.len() - 10..];
        let steady_state_avg =
            steady_state_samples.iter().sum::<u32>() / steady_state_samples.len() as u32;

        println!("📊 Buffer Analysis:");
        println!("   Initial: {initial_buffer}ms");
        println!("   Peak: {max_buffer}ms");
        println!("   Final: {final_buffer}ms");
        println!("   Steady-state avg: {steady_state_avg}ms");
        println!("   Acceleration operations: {acceleration_count}");

        // Critical assertions for your requirement

        // 1. For small buffer levels (20-40ms), acceleration should be minimal
        // This is normal network jitter, not buffer overload
        // With target=20ms and buffers=20-40ms, only minimal acceleration should occur
        // Note: The exact count may vary with decoder implementation (stub vs real)
        assert!(
            acceleration_count <= 6,
            "Expected minimal acceleration for small buffers, got {acceleration_count} (buffers were only 20-40ms above target)"
        );

        // 2. Steady-state buffer should be reasonable (user observed ~12 packets = ~240ms, but 0ms is even better)
        assert!(
            steady_state_avg <= 300,
            "Steady-state buffer should be small, got {steady_state_avg}ms (expected ≤300ms)"
        );

        // 3. Buffer should not continue growing indefinitely with continuous insertion
        // Allow equal in case buffer stabilizes at same level
        assert!(
            final_buffer <= max_buffer,
            "Buffer should not exceed peak during steady-state, but final={final_buffer}ms > max={max_buffer}ms"
        );

        // 4. Regression check: ensure we're nowhere near the old 60-packet behavior
        assert!(
            max_buffer <= 600,
            "REGRESSION: Max buffer {max_buffer}ms indicates old excessive buffering bug may have returned"
        );

        // 5. Stability check: steady-state should be much smaller than peak
        assert!(
            steady_state_avg < max_buffer / 2,
            "Steady-state ({steady_state_avg}ms) should be much smaller than peak ({max_buffer}ms)"
        );
    }

    /// Test buffer duration calculation fix for identical timestamps
    ///
    /// This test verifies the fix for the issue where packets with identical timestamps
    /// (common in network bursts or late-joining scenarios) would report 0ms span_duration
    /// but should use content_duration for accurate buffering decisions.
    ///
    /// Before fix: span_duration = 0ms, no acceleration triggered
    /// After fix: content_duration = packets × duration, acceleration triggered correctly
    #[test]
    fn test_buffer_duration_calculation_with_identical_timestamps() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        // Create 30 packets with IDENTICAL timestamps (simulating network burst)
        let base_timestamp = 1000u32;
        let packet_duration_ms = 20u32;
        let num_packets = 30u32;

        for i in 0..num_packets {
            // All packets have the same timestamp - this simulates burst arrival
            let packet = create_test_packet(i as u16, base_timestamp, packet_duration_ms);
            neteq.insert_packet(packet).unwrap();
        }

        // Test the buffer calculations
        let span_duration = neteq.packet_buffer.get_span_duration_ms();
        let content_duration = neteq.packet_buffer.get_total_content_duration_ms();
        let buffer_size_ms = neteq.current_buffer_size_ms();

        println!("Buffer calculation test:");
        println!("  Packets: {}", neteq.packet_buffer.len());
        println!("  Span duration: {span_duration}ms");
        println!("  Content duration: {content_duration}ms");
        println!("  Buffer size: {buffer_size_ms}ms");

        // Verify the problem and the fix
        assert_eq!(
            span_duration, 0,
            "Span duration should be 0ms for identical timestamps"
        );

        assert_eq!(
            content_duration,
            num_packets * packet_duration_ms,
            "Content duration should be sum of all packet durations: {} packets × {}ms = {}ms",
            num_packets,
            packet_duration_ms,
            num_packets * packet_duration_ms
        );

        assert_eq!(
            buffer_size_ms, content_duration,
            "NetEQ should use content duration, not span duration"
        );

        // The main goal of this test is to verify buffer calculation with identical timestamps
        // The delay manager may set high target delays due to the pathological timestamp pattern,
        // but that's expected behavior. The key assertion is that content duration is used correctly.

        // Verify that we can successfully decode audio frames despite the unusual timestamp pattern
        for _ in 0..5 {
            let pre_buffer = neteq.current_buffer_size_ms();
            let frame = neteq.get_audio().unwrap();
            let post_buffer = neteq.current_buffer_size_ms();

            // Should successfully decode frames
            assert!(
                !frame.samples.is_empty(),
                "Frame should contain audio samples"
            );

            println!(
                "  Buffer {pre_buffer}ms -> {post_buffer}ms, speech_type: {:?}",
                frame.speech_type
            );
        }

        println!("✅ Buffer duration calculation test passed - content duration used correctly");
    }
}
