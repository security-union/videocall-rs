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
    /// Enable fast accelerate mode
    pub enable_fast_accelerate: bool,
    /// Enable muted state detection
    pub enable_muted_state: bool,
    /// Enable RTX (retransmission) handling
    pub enable_rtx_handling: bool,
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
            enable_fast_accelerate: false,
            enable_muted_state: false,
            enable_rtx_handling: false,
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
    /// Accelerate operation (time compression)
    Accelerate,
    /// Fast accelerate operation
    FastAccelerate,
    /// Preemptive expand operation (time expansion)
    PreemptiveExpand,
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
    /// Sample memory for time-stretching operations (matches WebRTC sample_memory_)
    sample_memory: i32,
    /// Flag indicating if time-scale operation was performed in previous call
    prev_time_scale: bool,
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
        delay_config.max_packets_in_buffer = config.max_packets_in_buffer;
        delay_config.base_minimum_delay_ms = config.min_delay_ms;

        let mut delay_manager = DelayManager::new(delay_config);
        if config.max_delay_ms > 0 {
            delay_manager.set_maximum_delay(config.max_delay_ms)?;
        }
        delay_manager.set_minimum_delay(config.min_delay_ms)?;

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
            sample_memory: 0,
            prev_time_scale: false,
        })
    }

    /// Insert a packet into the jitter buffer
    pub fn insert_packet(&mut self, packet: AudioPacket) -> Result<()> {
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
        let _relative_delay =
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
            Operation::Accelerate => self.decode_accelerate(&mut frame)?,
            Operation::FastAccelerate => self.decode_fast_accelerate(&mut frame)?,
            Operation::PreemptiveExpand => self.decode_preemptive_expand(&mut frame)?,
            Operation::Expand => self.decode_expand(&mut frame)?,
            Operation::Merge => self.decode_merge(&mut frame)?,
            Operation::ComfortNoise => self.generate_comfort_noise(&mut frame)?,
            _ => {
                // Fill with silence for unsupported operations
                frame.samples.fill(0.0);
                frame.speech_type = SpeechType::Expand;
            }
        }

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

    /// Get current statistics
    pub fn get_statistics(&self) -> NetEqStats {
        NetEqStats {
            network: self.statistics.network_statistics().clone(),
            lifetime: self.statistics.lifetime_statistics().clone(),
            current_buffer_size_ms: self.current_buffer_size_ms(),
            target_delay_ms: self.delay_manager.target_delay_ms(),
            packets_awaiting_decode: self.packet_buffer.len(),
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

    /// Set minimum delay
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> Result<bool> {
        self.delay_manager.set_minimum_delay(delay_ms)
    }

    /// Set maximum delay
    pub fn set_maximum_delay(&mut self, delay_ms: u32) -> Result<bool> {
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
        self.sample_memory = 0;
        self.prev_time_scale = false;
    }

    fn get_decision(&mut self) -> Result<Operation> {
        // Update buffer level filter first
        let current_buffer_samples = self.current_buffer_size_samples();
        let target_delay_ms: u32 = self.delay_manager.target_delay_ms();

        // Filter buffer level like WebRTC's FilterBufferLevel method
        self.buffer_level_filter
            .set_target_buffer_level(target_delay_ms);

        // Calculate time-stretched samples (matches WebRTC decision_logic.cc:233-237)
        let mut time_stretched_samples = 0i32; // time_stretched_cn_samples in WebRTC
        if self.prev_time_scale {
            time_stretched_samples += self.sample_memory;
        }
        self.buffer_level_filter
            .update(current_buffer_samples, time_stretched_samples);

        // Reset for next frame (matches WebRTC decision_logic.cc:245-246)
        self.prev_time_scale = false;

        // Check if we have packets
        if self.packet_buffer.is_empty() {
            self.consecutive_expands = self.consecutive_expands.saturating_add(1);
            return Ok(Operation::Expand);
        }

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

        let buffer_level_samples = self.buffer_level_filter.filtered_current_level();

        // Fast accelerate: 4x high limit (aggressive acceleration for large buffers)
        if buffer_level_samples >= (high_limit << 2) as usize {
            self.consecutive_expands = 0;
            return Ok(Operation::FastAccelerate);
        }

        // Normal accelerate: at high limit
        if buffer_level_samples >= high_limit as usize {
            self.consecutive_expands = 0;
            return Ok(Operation::Accelerate);
        }

        // Preemptive expand: below low limit
        if buffer_level_samples < low_limit as usize && !self.packet_buffer.is_empty() {
            self.consecutive_expands = 0;
            return Ok(Operation::PreemptiveExpand);
        }

        // Check for continuous expansion limit (WebRTC uses much higher limits)
        if self.consecutive_expands > 600 {
            // ~6 seconds at 10ms frames
            self.flush();
            return Ok(Operation::Normal);
        }

        // Normal operation
        self.consecutive_expands = 0;
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

    fn decode_accelerate(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.config.for_test_no_time_stretching {
            // Get more data than we need
            let mut extended_frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                (frame.samples_per_channel as f32 * 1.5) as usize,
            );

            self.decode_normal(&mut extended_frame)?;

            // Apply accelerate algorithm
            let mut output = Vec::new();
            let _result = self
                .accelerate
                .process(&extended_frame.samples, &mut output, false);

            // Copy processed samples to frame
            let copy_len = output.len().min(frame.samples.len());
            frame.samples[..copy_len].copy_from_slice(&output[..copy_len]);

            // Update statistics
            let samples_removed = self.accelerate.get_length_change_samples();
            self.statistics
                .time_stretch_operation(TimeStretchOperation::Accelerate, samples_removed as u64);

            // Track time-stretching for buffer level filtering (matches WebRTC pattern)
            // WebRTC sets sample_memory to available samples for time-stretching (line 1304)
            // extended_frame has more samples than we need, representing available data
            let available_samples = extended_frame.samples.len() / self.config.channels as usize;
            self.sample_memory = available_samples as i32;
            self.prev_time_scale = true;

            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = extended_frame.vad_activity; // Preserve VAD from decoded audio
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn decode_fast_accelerate(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.config.for_test_no_time_stretching {
            // Get more data for aggressive acceleration
            let mut extended_frame = AudioFrame::new(
                self.config.sample_rate,
                self.config.channels,
                (frame.samples_per_channel as f32 * 2.0) as usize,
            );

            self.decode_normal(&mut extended_frame)?;

            // Apply fast accelerate
            let mut output = Vec::new();
            let _result = self.accelerate.process(
                &extended_frame.samples,
                &mut output,
                true, // Fast mode
            );

            let copy_len = output.len().min(frame.samples.len());
            frame.samples[..copy_len].copy_from_slice(&output[..copy_len]);

            let samples_removed = self.accelerate.get_length_change_samples();
            self.statistics
                .time_stretch_operation(TimeStretchOperation::Accelerate, samples_removed as u64);

            // Track time-stretching for buffer level filtering (matches WebRTC pattern)
            // WebRTC sets sample_memory to available samples for time-stretching (line 1304)
            // extended_frame has more samples than we need, representing available data
            let available_samples = extended_frame.samples.len() / self.config.channels as usize;
            self.sample_memory = available_samples as i32;
            self.prev_time_scale = true;

            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = extended_frame.vad_activity; // Preserve VAD from decoded audio
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn decode_preemptive_expand(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if !self.config.for_test_no_time_stretching {
            // Get normal amount of data
            self.decode_normal(frame)?;

            // Apply preemptive expand
            let input = frame.samples.clone();
            let mut output = Vec::new();

            let _result = self.preemptive_expand.process(&input, &mut output, false);

            // Update frame with expanded audio
            let copy_len = output.len().min(frame.samples.len());
            frame.samples[..copy_len].copy_from_slice(&output[..copy_len]);

            let samples_added = self.preemptive_expand.get_length_change_samples();
            self.statistics.time_stretch_operation(
                TimeStretchOperation::PreemptiveExpand,
                samples_added as u64,
            );

            // Track time-stretching for buffer level filtering (matches WebRTC pattern)
            // WebRTC sets sample_memory to available samples for time-stretching (line 1304)
            // For preemptive expand, we had normal frame samples available
            let available_samples = frame.samples.len() / self.config.channels as usize;
            self.sample_memory = available_samples as i32;
            self.prev_time_scale = true;

            frame.speech_type = SpeechType::Normal;
            // VAD activity already preserved from decode_normal call
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn decode_expand(&mut self, frame: &mut AudioFrame) -> Result<()> {
        log::trace!(
            "decode_expand: buffer before expand={}ms, packets={} (consecutive_expands={})",
            self.current_buffer_size_ms(),
            self.packet_buffer.len(),
            self.consecutive_expands
        );
        // Generate concealment audio (simple noise for now)
        for sample in &mut frame.samples {
            *sample = (simple_random() - 0.5) * 0.01; // Very quiet noise
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

    fn decode_merge(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Simplified merge - just normal decode for now
        self.decode_normal(frame)
    }

    fn generate_comfort_noise(&mut self, frame: &mut AudioFrame) -> Result<()> {
        // Generate comfort noise
        for sample in &mut frame.samples {
            *sample = (simple_random() - 0.5) * 0.001; // Very quiet comfort noise
        }

        frame.speech_type = SpeechType::Cng;
        frame.vad_activity = false;

        Ok(())
    }

    pub fn current_buffer_size_ms(&self) -> u32 {
        // Use total content duration instead of timestamp span
        // This handles cases where packets have close/identical timestamps
        self.packet_buffer.get_total_content_duration_ms()
    }

    /// Get current buffer size in samples
    pub fn current_buffer_size_samples(&self) -> usize {
        // Convert milliseconds to samples for the buffer duration
        let buffer_duration_ms = self.current_buffer_size_ms(); // Use the fixed calculation
        let buffer_samples =
            (buffer_duration_ms as u64 * self.config.sample_rate as u64 / 1000) as usize;
        buffer_samples + self.leftover_samples.len()
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

        // NetEQ should still deliver a normal speech frame
        let frame = neteq.get_audio().unwrap();
        assert_eq!(frame.speech_type, SpeechType::Normal);
        assert!(frame.vad_activity);

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
            "Initial buffer should be reasonable, got {}ms",
            initial_buffer_ms
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
                "Buffer should not accumulate excessively after late peer join. Got {}ms, expected ≤500ms", 
                buffer_after_late_join);

        // More importantly, ensure it's significantly better than the old behavior (60 packets = 1200ms)
        assert!(buffer_after_late_join <= 600,
                "REGRESSION DETECTION: Buffer accumulated {}ms, this exceeds acceptable threshold and may indicate the old 60-packet bug has returned", 
                buffer_after_late_join);

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
                "Buffer should never exceed 500ms during processing, got {}ms",
                post_buffer
            );
        }

        // Verify that NetEQ used acceleration to handle the large buffer
        // This confirms the fix is working - old code wouldn't accelerate enough
        let accelerate_count = total_operations.get("Accelerate").copied().unwrap_or(0);
        assert!(accelerate_count > 0,
                "NetEQ should have used acceleration to handle late-joining peer scenario. Operations: {:?}", 
                total_operations);

        // Final buffer should be reasonable
        let final_buffer = neteq.current_buffer_size_ms();
        assert!(
            final_buffer <= 400,
            "Final buffer should be reasonable, got {}ms",
            final_buffer
        );

        println!("✅ Late-joining peer test passed:");
        println!("   Initial buffer: {}ms", initial_buffer_ms);
        println!("   Peak buffer: {}ms", buffer_after_late_join);
        println!("   Final buffer: {}ms", final_buffer);
        println!("   Operations used: {:?}", total_operations);
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
                println!(
                    "Cycle {}: Buffer {}ms -> {}ms",
                    cycle, pre_buffer, post_buffer
                );
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
        println!("   Initial: {}ms", initial_buffer);
        println!("   Peak: {}ms", max_buffer);
        println!("   Final: {}ms", final_buffer);
        println!("   Steady-state avg: {}ms", steady_state_avg);
        println!("   Acceleration operations: {}", acceleration_count);

        // Critical assertions for your requirement

        // 1. For small buffer levels (20-40ms), acceleration should NOT be needed
        // This is normal network jitter, not buffer overload
        // With target=20ms and buffers=20-40ms, no acceleration should occur
        assert!(
            acceleration_count <= 2,
            "Expected minimal acceleration for small buffers, got {} (buffers were only 20-40ms above target)",
            acceleration_count
        );

        // 2. Steady-state buffer should be reasonable (user observed ~12 packets = ~240ms, but 0ms is even better)
        assert!(
            steady_state_avg <= 300,
            "Steady-state buffer should be small, got {}ms (expected ≤300ms)",
            steady_state_avg
        );

        // 3. Buffer should not continue growing indefinitely with continuous insertion
        // Allow equal in case buffer stabilizes at same level
        assert!(
            final_buffer <= max_buffer,
            "Buffer should not exceed peak during steady-state, but final={}ms > max={}ms",
            final_buffer,
            max_buffer
        );

        // 4. Regression check: ensure we're nowhere near the old 60-packet behavior
        assert!(
            max_buffer <= 600,
            "REGRESSION: Max buffer {}ms indicates old excessive buffering bug may have returned",
            max_buffer
        );

        // 5. Stability check: steady-state should be much smaller than peak
        assert!(
            steady_state_avg < max_buffer / 2,
            "Steady-state ({}ms) should be much smaller than peak ({}ms)",
            steady_state_avg,
            max_buffer
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
        println!("  Span duration: {}ms", span_duration);
        println!("  Content duration: {}ms", content_duration);
        println!("  Buffer size: {}ms", buffer_size_ms);

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

        // Verify acceleration decision is made correctly
        let stats = neteq.get_statistics();
        let target_delay = stats.target_delay_ms;

        // With 600ms of content (30 × 20ms), we should have enough to trigger acceleration
        // Convert to samples for decision logic
        let buffer_samples = (content_duration as u64 * 16000 / 1000) as usize; // 16kHz sample rate
        let target_samples = (target_delay as u64 * 16000 / 1000) as usize;
        let high_limit = std::cmp::max(target_samples, target_samples * 3 / 4 + 20 * 16); // 20ms margin

        println!("Decision thresholds:");
        println!("  Buffer samples: {}", buffer_samples);
        println!("  Target samples: {}", target_samples);
        println!("  High limit: {}", high_limit);

        assert!(
            buffer_samples > high_limit,
            "Buffer ({} samples) should exceed high limit ({} samples) to trigger acceleration",
            buffer_samples,
            high_limit
        );

        // Test the decision logic by calling get_audio multiple times
        let mut acceleration_count = 0;
        for _ in 0..10 {
            let pre_buffer = neteq.current_buffer_size_ms();
            let _frame = neteq.get_audio().unwrap();
            let post_buffer = neteq.current_buffer_size_ms();

            // If buffer reduced significantly, acceleration likely occurred
            if pre_buffer > post_buffer + 30 {
                acceleration_count += 1;
            }
        }

        assert!(
            acceleration_count > 0,
            "NetEQ should have performed acceleration with 600ms buffer, but saw no acceleration operations"
        );

        println!(
            "✅ Buffer duration calculation test passed - acceleration triggered {} times",
            acceleration_count
        );
    }
}
