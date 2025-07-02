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

use std::time::{Duration, Instant};
use std::thread::sleep;

use crate::buffer::{BufferReturnCode, PacketBuffer, SmartFlushConfig};
use crate::delay_manager::{DelayConfig, DelayManager};
use crate::packet::{AudioPacket, RtpHeader};
use crate::statistics::{
    LifetimeStatistics, NetworkStatistics, StatisticsCalculator, TimeStretchOperation,
};
use crate::time_stretch::{TimeStretchFactory, TimeStretchResult, TimeStretcher};
use crate::{NetEqError, Result};

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
#[derive(Debug, Clone)]
pub struct NetEqStats {
    pub network: NetworkStatistics,
    pub lifetime: LifetimeStatistics,
    pub current_buffer_size_ms: u32,
    pub target_delay_ms: u32,
    pub packet_count: usize,
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
    statistics: StatisticsCalculator,
    accelerate: Box<dyn TimeStretcher + Send>,
    preemptive_expand: Box<dyn TimeStretcher + Send>,
    last_decode_timestamp: Option<u32>,
    output_frame_size_samples: usize,
    muted: bool,
    last_operation: Operation,
    consecutive_expands: u32,
    frame_timestamp: u32,
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

        // Create time stretchers
        let accelerate: Box<dyn TimeStretcher + Send> =
            TimeStretchFactory::create_accelerate(config.sample_rate, config.channels);
        let preemptive_expand: Box<dyn TimeStretcher + Send> =
            TimeStretchFactory::create_preemptive_expand(config.sample_rate, config.channels);

        Ok(Self {
            config,
            packet_buffer,
            delay_manager,
            statistics,
            accelerate,
            preemptive_expand,
            last_decode_timestamp: None,
            output_frame_size_samples,
            muted: false,
            last_operation: Operation::Normal,
            consecutive_expands: 0,
            frame_timestamp: 0,
        })
    }

    /// Insert a packet into the jitter buffer
    pub fn insert_packet(&mut self, packet: AudioPacket) -> Result<()> {
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
        let mut frame = AudioFrame::new(
            self.config.sample_rate,
            self.config.channels,
            self.output_frame_size_samples / self.config.channels as usize,
        );

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
            packet_count: self.packet_buffer.len(),
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
        self.last_decode_timestamp = None;
        self.consecutive_expands = 0;
    }

    fn get_decision(&mut self) -> Result<Operation> {
        // Check if we have packets
        if self.packet_buffer.is_empty() {
            self.consecutive_expands += 1;
            return Ok(Operation::Expand);
        }

        let current_buffer_ms = self.current_buffer_size_ms();
        let target_delay_ms = self.delay_manager.target_delay_ms();

        // Check if buffer is too full (need to accelerate)
        if current_buffer_ms > target_delay_ms + 20 {
            self.consecutive_expands = 0;
            if self.config.enable_fast_accelerate && current_buffer_ms > target_delay_ms + 40 {
                return Ok(Operation::FastAccelerate);
            } else {
                return Ok(Operation::Accelerate);
            }
        }

        // Check if buffer is getting low (preemptive expand)
        if current_buffer_ms < target_delay_ms.saturating_sub(10) && !self.packet_buffer.is_empty()
        {
            self.consecutive_expands = 0;
            return Ok(Operation::PreemptiveExpand);
        }

        // Check for continuous expansion limit
        if self.consecutive_expands > 100 {
            // Reset after too many expands
            self.flush();
            return Ok(Operation::Normal);
        }

        // Normal operation
        self.consecutive_expands = 0;
        Ok(Operation::Normal)
    }

    fn decode_normal(&mut self, frame: &mut AudioFrame) -> Result<()> {
        if let Some(packet) = self.packet_buffer.get_next_packet() {
            // Simple PCM decode (assuming packet contains raw f32 samples)
            let samples_per_channel = frame.samples_per_channel;
            let channels = self.config.channels as usize;

            if packet.payload.len() >= samples_per_channel * channels * 4 {
                // Convert bytes to f32 (assuming little-endian IEEE 754)
                for i in 0..(samples_per_channel * channels) {
                    let byte_offset = i * 4;
                    if byte_offset + 3 < packet.payload.len() {
                        let bytes = [
                            packet.payload[byte_offset],
                            packet.payload[byte_offset + 1],
                            packet.payload[byte_offset + 2],
                            packet.payload[byte_offset + 3],
                        ];
                        frame.samples[i] = f32::from_le_bytes(bytes);
                    }
                }
            } else {
                // Fill with silence if not enough data
                frame.samples.fill(0.0);
                frame.speech_type = SpeechType::Expand;
            }

            self.last_decode_timestamp = Some(packet.header.timestamp);
            frame.speech_type = SpeechType::Normal;
            frame.vad_activity = true;
        } else {
            // No packet available, expand
            self.decode_expand(frame)?;
        }

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

            frame.speech_type = SpeechType::Normal;
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

            frame.speech_type = SpeechType::Normal;
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

            frame.speech_type = SpeechType::Normal;
        } else {
            self.decode_normal(frame)?;
        }

        Ok(())
    }

    fn decode_expand(&mut self, frame: &mut AudioFrame) -> Result<()> {
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
        self.packet_buffer.get_span_duration_ms()
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
    use super::*;
    use crate::packet::RtpHeader;
    use rand::seq::SliceRandom;
    use rand::thread_rng;

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
        assert_eq!(neteq.target_delay_ms(), 0);
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
            neteq.insert_packet(p).unwrap();
            assert_eq!(neteq.current_buffer_size_ms(), counter);
            counter += 10;
        }

        assert_eq!(neteq.current_buffer_size_ms(), 40);

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
            // Simulate playout interval
            sleep(Duration::from_millis(10));
        }

        let stats = neteq.get_statistics();
        assert!(expand_frames > 0, "Expected at least one expand frame due to late packets");
        assert!(stats.lifetime.concealment_events >= expand_frames as u64, "Concealment events should grow");
        assert!(stats.lifetime.concealed_samples > 0, "Concealed samples should increase");
        assert!(stats.network.max_waiting_time_ms > 0, "Waiting time stats should reflect packet delays");
    }
}
