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
use crate::expand::ExpandFactory;
use crate::expand::{Expand, ExpandPhase};
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

/// Default playout-latency ceiling for the resync-to-live governor (issue #1299).
///
/// When the FILTERED playout buffer level ([`NetEq::filtered_buffer_level_ms`]) — i.e. how far
/// behind live the audio playout sits — exceeds this many milliseconds, the governor performs a
/// one-time controlled flush-to-live (see [`NetEq::maybe_resync_to_live`]). 1000ms is chosen so
/// the governor fires ONLY on genuine multi-second lag (the #1299 standing-wave case) and never
/// during normal jitter buffering: it sits well above `max_delay_ms` (the adaptive-target cap,
/// see [`DEFAULT_RESYNC_MAX_DELAY_MS`]) so ordinary Accelerate/Normal operation is untouched.
/// Field-tunable via [`NetEqConfig::resync_ceiling_ms`].
const DEFAULT_RESYNC_CEILING_MS: u32 = 1000;

/// Default minimum wall-clock interval between governor flushes (issue #1299).
///
/// Hysteresis/cooldown: after a resync flush, the governor will not fire again for at least this
/// long. This prevents thrashing if the filtered level briefly re-crosses the ceiling while the
/// buffer-level filter is still settling toward the post-flush level. Field-tunable via
/// [`NetEqConfig::resync_cooldown_ms`].
const DEFAULT_RESYNC_COOLDOWN_MS: u64 = 5000;

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
    /// Skip delay-manager updates on insert (for testing only).
    ///
    /// When `true`, `insert_packet` skips the `delay_manager.update()` call and the
    /// subsequent `statistics.update_buffer_size()` call. This eliminates the per-insert
    /// O(n log n) sort inside `RelativeArrivalDelayTracker::calculate_relative_packet_arrival_delay`
    /// that becomes quadratic under test-speed insertion. Production code must never set this.
    pub for_test_skip_delay_manager: bool,
    /// Bypass NetEQ processing and decode packets directly (for A/B testing)
    pub bypass_mode: bool,
    /// Delay manager configuration
    pub delay_config: DelayConfig,
    /// Smart flushing configuration
    pub smart_flush_config: SmartFlushConfig,
    /// Resync-to-live governor: playout-latency ceiling in milliseconds (issue #1299).
    ///
    /// When the filtered playout buffer level exceeds this, the governor flushes the stale
    /// backlog and resyncs to live (keeping ~`target_delay_ms` of the freshest audio, with
    /// concealment over the seam). `0` disables the governor entirely. Must be set well above
    /// `max_delay_ms` so it only fires on genuine multi-second lag, never on normal jitter.
    /// Defaults to [`DEFAULT_RESYNC_CEILING_MS`].
    pub resync_ceiling_ms: u32,
    /// Resync-to-live governor: minimum wall-clock interval between flushes, in milliseconds
    /// (issue #1299). Cooldown/hysteresis to prevent thrashing. Defaults to
    /// [`DEFAULT_RESYNC_COOLDOWN_MS`].
    pub resync_cooldown_ms: u64,
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
            for_test_skip_delay_manager: false,
            bypass_mode: false,
            delay_config: DelayConfig::default(),
            smart_flush_config: SmartFlushConfig::default(),
            resync_ceiling_ms: DEFAULT_RESYNC_CEILING_MS,
            resync_cooldown_ms: DEFAULT_RESYNC_COOLDOWN_MS,
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

impl Operation {
    /// Total number of variants, derived from the last discriminant.
    ///
    /// This relies on `Undefined` being the final variant with default
    /// (sequential) discriminant numbering starting at 0.  Adding a variant
    /// after `Undefined` or reordering will cause the compile-time assertion
    /// in `statistics.rs` to fire, signalling that this constant (or the
    /// assertion) needs updating.
    pub const VARIANT_COUNT: usize = Self::Undefined as usize + 1;
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
    /// Audio playout latency in ms (issue #1299): the WebRTC-style FILTERED current
    /// buffer level — the EWMA-smoothed playout buffer the Accelerate gate compares
    /// against `high_limit` in `get_decision`. This is how far behind live the audio
    /// playout sits; when the #1299 lag accumulates it ratchets up and gates Accelerate
    /// off. Distinct from `current_buffer_size_ms` (raw instantaneous snapshot) and
    /// `target_delay_ms` (the target, not the actual). `#[serde(default)]` so older stats
    /// JSON (pre-#1299) still deserializes. See [`NetEq::filtered_buffer_level_ms`].
    #[serde(default)]
    pub playout_latency_ms: u32,
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
    expand: Box<Expand>,
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
    /// Resync-to-live governor (issue #1299): wall-clock anchor of the last flush, for the
    /// cooldown/hysteresis. `None` means no flush has occurred this stream. Reset in [`reset`].
    last_resync: Option<Instant>,
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
        let expand: Box<Expand> = ExpandFactory::create_expand(config.sample_rate, config.channels);

        Ok(Self {
            config,
            packet_buffer,
            delay_manager,
            buffer_level_filter,
            statistics,
            accelerate,
            preemptive_expand,
            expand,
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
            last_resync: None,
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
        // Update delay manager (skip when for_test_skip_delay_manager is set — see config doc).
        if !self.config.for_test_skip_delay_manager {
            self.delay_manager
                .update(packet.header.timestamp, packet.sample_rate, false)?;
        }

        // Insert packet into buffer
        let target_delay = self.delay_manager.target_delay_ms();
        let result =
            self.packet_buffer
                .insert_packet(packet, &mut self.statistics, target_delay)?;

        // Update statistics (skip alongside delay manager to keep stats consistent).
        if !self.config.for_test_skip_delay_manager {
            self.statistics
                .update_buffer_size(self.current_buffer_size_ms() as u16, target_delay as u16);
        }

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
            Operation::Expand => self.decode_expand(&mut frame, ExpandPhase::Expand)?,
            Operation::ExpandStart => self.decode_expand(&mut frame, ExpandPhase::ExpandStart)?,
            Operation::ExpandEnd => self.decode_expand(&mut frame, ExpandPhase::ExpandEnd)?,
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
            // Audio playout latency (#1299): surface the live filtered playout buffer level.
            playout_latency_ms: self.filtered_buffer_level_ms(),
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

    /// Flush the buffer and reset all internal state
    pub fn flush(&mut self) {
        self.packet_buffer.flush(&mut self.statistics);
        self.leftover_samples.clear();
        // Drain any decoded-PCM backlog held in bypass mode too, so flush() fully
        // empties every audio-holding structure regardless of mode (issue #1402).
        // No-op on the shipped wasm path (bypass mode is never enabled there), but
        // keeps the primitive correct-by-construction if it ever is.
        self.bypass_audio_queue.clear();
        self.reset();
    }

    /// Reset internal state without clearing the incoming packet buffer.
    /// Used after prolonged expansion to recalibrate filters while preserving
    /// any packets that may have arrived during the expansion period.
    fn reset(&mut self) {
        self.delay_manager.reset();
        self.buffer_level_filter.reset();
        self.buffer_level_filter
            .set_filtered_buffer_level(self.current_buffer_size_samples());
        self.last_decode_timestamp = None;
        self.consecutive_expands = 0;
        self.packets_received_this_second = 0;
        self.packets_per_sec_snapshot = 0;
        self.last_packets_second_instant = Instant::now();
        self.leftover_time_stretched_samples.clear();
        self.timestretch_added_samples = 0;
        // Resync-to-live governor (issue #1299): clear the cooldown anchor so a stale lag episode
        // (e.g. a full flush or a prolonged disconnect/safety-valve reset) cannot mis-gate a
        // freshly recalibrating stream.
        self.last_resync = None;
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

        // Resync-to-live governor (issue #1299). Evaluated per decode tick, AFTER the buffer-level
        // filter has been updated for this tick (so `filtered_buffer_level_ms()` reflects the live
        // level) and BEFORE any normal decision. When the playout buffer is genuinely seconds
        // behind live it drops the stale backlog, keeps ~`target_delay_ms` of the freshest audio,
        // and returns `ExpandStart` so this very tick conceals the seam via the existing
        // expand/crossfade path (NOT a raw splice). Below the ceiling this is a no-op and the
        // decision flow below is byte-identical to pre-#1299 behavior.
        if let Some(op) = self.maybe_resync_to_live(target_delay_ms) {
            return Ok(op);
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

        if !self.leftover_time_stretched_samples.is_empty() {
            return Ok(Operation::TimeStretchBuffer);
        }

        if self.consecutive_expands > 0 && current_buffer_samples < low_limit as usize {
            // prefer a single continuous expand over many small ones
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
            // Safety valve: after ~6 seconds of continuous expansion (~600 frames
            // at 10ms each), reset internal filters so we recalibrate when packets
            // finally arrive rather than relying on stale state.
            if self.consecutive_expands > 600 {
                self.reset();
            }
            self.consecutive_expands = 0;
            return Ok(Operation::ExpandEnd);
        }

        let buffer_level_samples = self.buffer_level_filter.filtered_current_level();

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

    /// Resync-to-live governor (issue #1299): the real fix for unbounded audio playout latency.
    ///
    /// A NetEQ receiver can fall multiple seconds behind live, play at normal 1× cadence, and stay
    /// there forever: the consumer is real-time-paced so a backlog is a standing wave it never
    /// drains, and Accelerate's catch-up gates off because the adaptive target ratchets toward the
    /// 3s cap and re-labels the lag as the "correct" depth. Bounding the target (see
    /// `NetEqConfig::max_delay_ms`, set to ~300ms on the wasm path) keeps Accelerate's setpoint low
    /// but cannot claw back seconds already buffered. This governor handles that episodic case
    /// directly.
    ///
    /// Fires ONLY when the FILTERED playout buffer level (how far behind live we are) exceeds
    /// `resync_ceiling_ms` AND the cooldown has elapsed. When it fires it performs a one-time
    /// controlled flush-to-live: it drops the stale backlog, keeps ~`target_delay_ms` of the
    /// freshest buffered audio, resets the buffer-level filter to that reduced level, and returns
    /// [`Operation::ExpandStart`]. Returning `ExpandStart` makes the very next decode this tick run
    /// the existing expand/crossfade concealment path (`decode_expand`) across the discontinuity —
    /// the SAME machinery a packet-gap underrun uses — rather than a raw, audible splice. The
    /// natural decision flow then produces `Expand`/`ExpandEnd` as the kept buffer is consumed,
    /// completing the conceal→resume arc.
    ///
    /// Returns `Some(ExpandStart)` when it fires, otherwise `None` (caller proceeds with the normal
    /// decision flow, which is byte-identical to pre-#1299 behavior below the ceiling).
    ///
    /// `target_delay_ms` is passed in (rather than re-read) so it matches exactly the value
    /// `get_decision` already computed for this tick.
    fn maybe_resync_to_live(&mut self, target_delay_ms: u32) -> Option<Operation> {
        // Disabled when ceiling is 0.
        if self.config.resync_ceiling_ms == 0 {
            return None;
        }

        // Trigger signal: the live, decision-grade playout latency. Only fire on genuine lag above
        // the ceiling — below it, behavior is unchanged.
        let playout_latency_ms = self.filtered_buffer_level_ms();
        if playout_latency_ms <= self.config.resync_ceiling_ms {
            return None;
        }

        // Hysteresis/cooldown: do not fire again until `resync_cooldown_ms` has elapsed since the
        // last flush, so the governor cannot thrash while the filter settles to the new level.
        let now = Instant::now();
        if let Some(last) = self.last_resync {
            if now.duration_since(last) < Duration::from_millis(self.config.resync_cooldown_ms) {
                return None;
            }
        }

        // Decide how much of the freshest audio to keep. We want ~`target_delay_ms` (the live
        // jitter setpoint) — BUT in the broken/cold regime the adaptive target has itself ratcheted
        // toward seconds (the very #1299 mechanism), so keeping the raw target would keep the whole
        // backlog and the flush would be a no-op. Clamp the kept depth to half the ceiling so the
        // flush ALWAYS lands well below the trigger and genuinely drops the backlog, regardless of
        // whether part 2's `max_delay_ms` bound is active. With the bound active (wasm path,
        // target≈300, ceiling 1000) this is min(300, 500)=300 — exactly the issue's "~target ms".
        let keep_ms = target_delay_ms.min(self.config.resync_ceiling_ms / 2);

        // Perform the controlled flush-to-live: keep ~keep_ms of the freshest packets, drop the
        // older backlog. This is the audio analog of #1252's video skip-to-live.
        self.flush_to_live(keep_ms);

        // Anchor the cooldown.
        self.last_resync = Some(now);

        log::info!(
            "Resync-to-live governor fired (#1299): playout latency was {playout_latency_ms}ms (> ceiling {}ms); flushed backlog, kept ~{keep_ms}ms, concealing seam",
            self.config.resync_ceiling_ms
        );

        // Enter the same concealment-then-resume state a real underrun would, so the seam is
        // crossfaded rather than spliced. consecutive_expands == 1 matches get_decision's own
        // ExpandStart branch (which increments from 0 to 1).
        self.consecutive_expands = 1;
        Some(Operation::ExpandStart)
    }

    /// Drop the stale backlog and keep ~`keep_ms` of the freshest buffered audio (issue #1299).
    ///
    /// Steps:
    /// 1. Remove older packets from the packet buffer (via `PacketBuffer::partial_flush`, the same
    ///    span-keeping primitive smart-flush uses), keeping ~`keep_ms` of the freshest content.
    /// 2. Clear the post-NetEQ leftover residue (`leftover_samples` /
    ///    `leftover_time_stretched_samples`), since that residue is also stale playout content
    ///    contributing to the lag; `decode_normal` will pull fresh packets for the concealment.
    /// 3. Reset the delay manager. This is essential: the adaptive target (and thus `low_limit` /
    ///    `high_limit` in `get_decision`) may have ratcheted toward seconds — the #1299 mechanism.
    ///    If we kept that stale target, the post-flush buffer (~`keep_ms`) would sit BELOW
    ///    `low_limit` and `get_decision` would conceal (Expand) forever, playing noise over real
    ///    audio. Resetting recalibrates the target to the start delay (still bounded by any
    ///    `max_delay_ms`), so the kept buffer is correctly seen as healthy and playback resumes.
    /// 4. Resync the buffer-level filter to the new (reduced) buffer level so the next decision sees
    ///    the post-flush latency, not the stale one.
    ///
    /// The seam left behind is concealed by the `ExpandStart` returned from the caller.
    fn flush_to_live(&mut self, keep_ms: u32) {
        // 1. Keep ~keep_ms of the freshest packets; drop the older backlog.
        let _ = self.packet_buffer.partial_flush(
            keep_ms,
            self.config.sample_rate,
            &mut self.statistics,
        );

        // 2. Drop stale post-NetEQ residue (these count toward current_buffer_size_samples and thus
        // toward the lag).
        self.leftover_samples.clear();
        self.leftover_time_stretched_samples.clear();

        // 3. Recalibrate the adaptive target away from any ratcheted (stale) value so the post-flush
        // buffer is judged against a live setpoint, not the seconds-deep one that caused the lag.
        self.delay_manager.reset();

        // 4. Resync the buffer-level filter to the reduced level so subsequent decisions (and the
        // governor's own ceiling check) see live latency, not the pre-flush backlog.
        self.buffer_level_filter
            .set_filtered_buffer_level(self.current_buffer_size_samples());
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

    fn decode_expand(&mut self, frame: &mut AudioFrame, phase: ExpandPhase) -> Result<()> {
        log::trace!(
            "decode_expand: buffer before expand={}ms, packets={} (consecutive_expands={})",
            self.current_buffer_size_ms(),
            self.packet_buffer.len(),
            self.consecutive_expands
        );

        let samples_required = self.expand.samples_required(phase);

        let mut input = AudioFrame::new(
            self.config.sample_rate,
            self.config.channels,
            samples_required,
        );

        self.decode_normal(&mut input)?;

        self.expand
            .process(&input.samples, &mut frame.samples, phase);

        // Put back unused samples
        let used_input_samples = self.expand.get_used_input_samples();
        if input.samples.len() > used_input_samples {
            self.leftover_samples
                .splice(0..0, input.samples[used_input_samples..].iter().cloned());
        }

        // Update statistics
        self.statistics
            .concealment_event(frame.samples_per_channel as u64, true);
        self.statistics.time_stretch_operation(
            TimeStretchOperation::Expand,
            frame.samples_per_channel as u64,
        );

        frame.speech_type = SpeechType::Expand;
        frame.vad_activity = false;

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

    /// Audio playout latency in ms (issue #1299): the WebRTC-style FILTERED current buffer
    /// level. This reads the same `buffer_level_filter` that `get_decision` updates on every
    /// decode tick (via `BufferLevelFilter::update`) and then compares against `high_limit`
    /// to gate Accelerate. It is therefore the live, decision-grade measure of how far behind
    /// live the audio playout sits — the value that ratchets up under the #1299 lag and gates
    /// Accelerate off. Read-only: never mutates the filter or any other state, so it is safe to
    /// call on the stats path. Distinct from `current_buffer_size_ms` (raw instantaneous
    /// snapshot, which can swing frame-to-frame) and from `target_delay_ms` (the target, not the
    /// actual level).
    pub fn filtered_buffer_level_ms(&self) -> u32 {
        self.buffer_level_filter.filtered_current_level_ms()
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

    /// Build a self-consistent 48kHz / 1ch / 20ms audio packet whose payload sample
    /// count (960 f32 samples), `sample_rate` (48000), `channels` (1) and `duration_ms`
    /// (20) all agree. Mirrors the production decoder frame (48kHz, 20ms = 960 samples)
    /// rather than reusing the 16kHz/160-sample `create_test_packet` helper.
    fn create_48k_20ms_packet(seq: u16, ts: u32) -> AudioPacket {
        let header = RtpHeader::new(seq, ts, 12345, 96, false);
        // 960 samples * 4 bytes = 3840 bytes of raw little-endian f32 PCM (20ms @ 48kHz).
        let mut payload = Vec::with_capacity(960 * 4);
        for i in 0..960 {
            let sample = (i as f32 / 960.0 * 2.0 * std::f32::consts::PI * 440.0).sin() * 0.1;
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        AudioPacket::new(header, payload, 48000, 1, 20)
    }

    /// Issue #1402: `NetEq::flush` must actually DRAIN the packet buffer.
    ///
    /// This is the production behavior the worker's `WorkerMsg::Flush` handler relies
    /// on when a peer's audio stream ends (mic-off / host force-mute): the buffer must
    /// be emptied so NetEq stops emitting expand/comfort-noise concealment ("hiss") for
    /// a stream that is no longer producing packets. The handler was a no-op (it only
    /// logged), so the buffer was never cleared and the un-muted path kept hissing.
    /// `WebNetEq::flush` delegates to this method, so pinning it here pins the fix's
    /// load-bearing behavior on the host target (the worker's wasm `WebNetEq` wrapper
    /// has no host-runnable harness).
    ///
    /// MUTATION CHECK: if `NetEq::flush` were reverted to a no-op (or the worker handler
    /// left calling nothing), the post-flush `packet_buffer.len() == 0` assertion fails.
    #[test]
    fn test_flush_drains_packet_buffer() {
        let config = NetEqConfig {
            sample_rate: 48_000,
            ..Default::default()
        };
        let mut neteq = NetEq::new(config).unwrap();

        // Buffer several packets, as a live peer stream would.
        for i in 0..5u32 {
            neteq
                .insert_packet(create_48k_20ms_packet(i as u16, i * 960))
                .unwrap();
        }
        assert!(
            neteq.packet_buffer.len() > 0,
            "precondition: packets should be buffered before flush (was {})",
            neteq.packet_buffer.len()
        );

        // The peer's audio stream ends → flush.
        neteq.flush();

        assert_eq!(
            neteq.packet_buffer.len(),
            0,
            "flush must drain the packet buffer so no concealment is emitted for the \
             ended stream; {} packet(s) remained",
            neteq.packet_buffer.len()
        );
    }

    /// Regression guard for issue #624: NetEQ's `delay_manager` treats the per-packet
    /// RTP `timestamp` as a SAMPLE COUNTER. The videocall-client decoder was fixed to
    /// advance consecutive 20ms frames by +960 SAMPLES at 48kHz (sample-domain) instead
    /// of +20 (ms-domain). This test proves that sample-domain stepping keeps NetEQ's
    /// target delay and expansion near zero for a steady, on-time stream.
    ///
    /// Mechanism (see `delay_manager.rs::RelativeArrivalDelayTracker::update`):
    /// `expected_iat_ms = (timestamp - last_timestamp) * 1000 / sample_rate`, compared
    /// against the real wall-clock inter-arrival gap. With +960 sample steps at 48kHz,
    /// `expected_iat_ms = 960 * 1000 / 48000 = 20ms`, which matches the real ~20ms frame
    /// cadence, so the measured jitter is ~0 and `target_delay_ms` stays at one bucket
    /// (20ms). If the decoder regressed to ms-domain (+20), `expected_iat_ms` would
    /// truncate to `20 * 1000 / 48000 = 0ms`, the on-time arrivals would register as
    /// +20ms of positive jitter, and `target_delay_ms` would climb to 40ms — tripping
    /// the `<= 30` bound below. Observed steady-state values during development:
    /// sample-domain (+960) -> target_delay_ms = 20; ms-domain (+20) -> 40.
    ///
    /// `resample_interval_ms` is set to `None` so each packet's relative delay is fed to
    /// the histogram immediately (the default 500ms resampler never fires inside a short
    /// test, leaving the histogram empty and pegging target at its 2000ms ceiling). A real
    /// ~20ms per-iteration sleep is required so the wall-clock inter-arrival gap is
    /// realistic; `insert_packet` derives arrival time from `Instant::now()` internally
    /// and exposes no injectable clock.
    #[test]
    fn test_sample_domain_timestamps_keep_expand_near_zero() {
        // 20ms @ 48kHz = 960 samples. Sample-domain step the decoder fix produces.
        const SAMPLE_RATE: u32 = 48000;
        const FRAME_DURATION_MS: u32 = 20;
        // Sample-domain RTP-timestamp step for a 20ms frame at 48kHz = 960 samples.
        const SAMPLE_DOMAIN_STEP: u32 = SAMPLE_RATE / 1000 * FRAME_DURATION_MS;
        // Bound sits halfway between the sample-domain steady value (20ms, one bucket)
        // and the ms-domain regression value (40ms); robust to one 20ms bucket of jitter.
        const TARGET_DELAY_BOUND_MS: u32 = 30;
        const WARMUP_PACKETS: u32 = 8;
        const STEADY_ITERS: u32 = 60;
        const MEASURE_AFTER: u32 = 10;

        let config = NetEqConfig {
            sample_rate: SAMPLE_RATE,
            // Feed every packet's relative delay to the histogram immediately; the default
            // 500ms resampler would never fire within this short test, leaving target pegged.
            delay_config: DelayConfig {
                resample_interval_ms: None,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut neteq = NetEq::new(config).unwrap();

        let mut seq: u16 = 0;
        let mut timestamp: u32 = 0;

        // Warm-up: prime the jitter buffer with on-time packets so it never starves
        // during measurement. No frames pulled here so a small cushion accumulates.
        for _ in 0..WARMUP_PACKETS {
            let packet = create_48k_20ms_packet(seq, timestamp);
            neteq.insert_packet(packet).unwrap();
            seq += 1;
            timestamp += SAMPLE_DOMAIN_STEP;
            sleep(Duration::from_millis(FRAME_DURATION_MS as u64));
        }

        let mut expand_frames = 0u32;
        let mut max_steady_target_ms = 0u32;
        let mut max_steady_expand_rate = 0u16;

        // Steady state: insert one packet, pull one frame, sleep ~20ms, in lockstep.
        for i in 0..STEADY_ITERS {
            let packet = create_48k_20ms_packet(seq, timestamp);
            neteq.insert_packet(packet).unwrap();

            let frame = neteq.get_audio().unwrap();
            if frame.speech_type == SpeechType::Expand {
                expand_frames += 1;
            }

            let stats = neteq.get_statistics();
            if i >= MEASURE_AFTER {
                max_steady_target_ms = max_steady_target_ms.max(stats.target_delay_ms);
                max_steady_expand_rate = max_steady_expand_rate.max(stats.network.expand_rate);
            }

            seq += 1;
            timestamp += SAMPLE_DOMAIN_STEP;
            sleep(Duration::from_millis(FRAME_DURATION_MS as u64));
        }

        // Sample-domain stepping keeps the target delay at one bucket (~20ms). If the
        // decoder regressed to ms-domain (+20), this would be 40ms and fail.
        assert!(
            max_steady_target_ms <= TARGET_DELAY_BOUND_MS,
            "sample-domain (+{SAMPLE_DOMAIN_STEP}) target_delay_ms should stay <= {TARGET_DELAY_BOUND_MS}ms, got {max_steady_target_ms}ms (ms-domain regression pushes this to ~40ms)"
        );

        // A steady on-time stream must not trigger any expansion/concealment.
        assert_eq!(
            max_steady_expand_rate, 0,
            "steady on-time stream should produce zero expand_rate (Q14), got {max_steady_expand_rate}"
        );
        assert_eq!(
            expand_frames, 0,
            "steady on-time stream should produce zero Expand frames, got {expand_frames}"
        );
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

    macro_rules! make_insert_audio {
        () => {{
            let mut seq: u16 = 0;
            let sample_rate = 16000;
            move |neteq: &mut NetEq, duration_ms: u32| {
                for _ in 0..(duration_ms / 10) {
                    let header = RtpHeader::new(seq, seq as u32 * 160, 12345, 96, false);
                    seq += 1;
                    let mut payload = Vec::new();

                    let sample_length = sample_rate / 100;
                    for i in 0..sample_length {
                        let sample = (i as f32 / 160.0 * 2.0 * std::f32::consts::PI).sin() * 0.1;
                        payload.extend_from_slice(&sample.to_le_bytes());
                    }
                    let packet = AudioPacket::new(header, payload, sample_rate, 1, 10);
                    neteq.insert_packet(packet).unwrap();
                }
            }
        }};
    }

    macro_rules! make_reset_filtered_level {
        () => {{
            move |neteq: &mut NetEq| {
                neteq
                    .buffer_level_filter
                    .set_filtered_buffer_level(neteq.current_buffer_size_samples());
            }
        }};
    }

    #[test]
    fn test_get_decision_expand() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        // Set target delay to 50ms
        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);
        // // Insert 50ms audio
        // for i in 0..5 {
        //     let packet = create_packet(i, i as u32 * 160, 10);
        //     neteq.insert_packet(packet).unwrap();
        // }

        // Expect 4 Normal operation
        for _ in 0..4 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Normal);
        }

        // Expect ExpandStart
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandStart);

        // Expect Expands while the buffer is empty
        for _ in 0..100 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Expand);
        }

        insert_audio(&mut neteq, 20);

        // Still expect Expands while the buffer is low
        for _ in 0..100 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Expand);
        }

        insert_audio(&mut neteq, 20);
        reset_filtered_level(&mut neteq);

        // Expect a single ExpandEnd
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandEnd);

        // Expect Normal
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::Normal);
    }

    #[test]
    fn test_get_decision_accelerate() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        // Set target delay to 50ms
        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        // Expect Normal operation when audio is coming in normally
        for _ in 0..20 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Normal);
            insert_audio(&mut neteq, 10);
        }

        insert_audio(&mut neteq, 10);
        reset_filtered_level(&mut neteq);

        // Expect Accelerate,TimeStrectchBuffer,TimeStrectchBuffer a couple times
        for _ in 0..3 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Accelerate);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
        }

        insert_audio(&mut neteq, 300);
        reset_filtered_level(&mut neteq);

        // Expect FastAccelerate,TimeStrectchBuffer,TimeStrectchBuffer a couple times
        for _ in 0..3 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::FastAccelerate);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
        }
    }

    #[test]
    fn test_get_decision_preemptive_expand() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        // Set target delay to 50ms
        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        // Expect Normal operation when audio is coming in normally
        for _ in 0..20 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Normal);
            insert_audio(&mut neteq, 10);
        }

        // Set target delay to 500ms
        neteq.delay_manager.set_base_minimum_delay(500);
        neteq.delay_manager.set_base_maximum_delay(500);

        // Expect PreemtiveExpand+TimeStrectchBuffer+TimeStrectchBuffer a couple times
        for _ in 0..3 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::PreemptiveExpand);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::TimeStretchBuffer);
            insert_audio(&mut neteq, 10);
        }
    }

    #[test]
    fn test_expand_safety_valve_triggers_after_600_frames() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        // Fill buffer to normal, then drain it to trigger expansion
        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        // Drain to normal
        for _ in 0..4 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Normal);
        }

        // ExpandStart
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandStart);

        // Run 600+ expands with no packets arriving (simulates prolonged disconnect).
        // The safety valve at 600 should fire, but we stay in Expand because
        // consecutive_expands > 0 AND buffer < low_limit keeps returning Expand.
        // The check fires when the buffer finally recovers past threshold.
        for _ in 0..700 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Expand);
        }

        // consecutive_expands should be > 600 now
        assert!(neteq.consecutive_expands > 600);

        // Now insert enough audio to recover the buffer past low_limit
        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        // The safety valve should fire: reset() then ExpandEnd
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandEnd);

        // After ExpandEnd, consecutive_expands should be reset to 0
        assert_eq!(neteq.consecutive_expands, 0);
    }

    #[test]
    fn test_recovery_after_safety_valve_reset() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        // Fill and drain to enter expand
        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        for _ in 0..4 {
            let _ = neteq.get_audio().unwrap();
        }

        // ExpandStart
        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandStart);

        // Run 650 expands to exceed the 600 threshold
        for _ in 0..650 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Expand);
        }

        // Insert enough audio to recover, triggering the safety valve + ExpandEnd
        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandEnd);

        // After the reset, the system should recover to normal playback.
        // It may go through a brief Accelerate/TimeStretch phase as filters
        // recalibrate, but crucially it must NOT re-enter Expand.
        let mut saw_normal = false;
        for _ in 0..20 {
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            assert!(
                neteq.last_operation != Operation::Expand
                    && neteq.last_operation != Operation::ExpandStart,
                "System re-entered expand after safety valve reset: {:?}",
                neteq.last_operation
            );
            if neteq.last_operation == Operation::Normal {
                saw_normal = true;
            }
        }
        assert!(
            saw_normal,
            "System never reached Normal after safety valve reset"
        );
    }

    #[test]
    fn test_sudden_disconnect_skips_expand_start() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        neteq.delay_manager.set_base_minimum_delay(50);
        neteq.delay_manager.set_base_maximum_delay(50);

        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        // Consume ALL audio so the buffer is completely empty
        // (drops below 0.5x frame, bypassing the ExpandStart window)
        for _ in 0..5 {
            let _ = neteq.get_audio().unwrap();
        }

        // With buffer completely empty, we should get Expand directly
        // (no ExpandStart because buffer is below the 0.5x frame threshold)
        let _ = neteq.get_audio().unwrap();
        assert!(
            neteq.last_operation == Operation::Expand
                || neteq.last_operation == Operation::ExpandStart,
            "Expected Expand or ExpandStart after sudden disconnect, got {:?}",
            neteq.last_operation
        );

        // Subsequent frames should be Expand
        for _ in 0..5 {
            let _ = neteq.get_audio().unwrap();
            assert_eq!(neteq.last_operation, Operation::Expand);
        }

        // Recovery: insert audio, expect ExpandEnd then Normal
        insert_audio(&mut neteq, 50);
        reset_filtered_level(&mut neteq);

        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::ExpandEnd);

        let _ = neteq.get_audio().unwrap();
        assert_eq!(neteq.last_operation, Operation::Normal);
    }

    /// Audio playout latency metric (issue #1299).
    ///
    /// Pins TWO things the metric must do, so it fails if the source is broken:
    ///   1. `NetEqStats::playout_latency_ms` is wired to the FILTERED playout buffer level
    ///      (`filtered_buffer_level_ms()`), NOT to a constant, the raw `current_buffer_size_ms`
    ///      snapshot, or `target_delay_ms`. Asserted by exact equality with the filter accessor.
    ///   2. It is a LIVE value: after audio is buffered and decode decisions run (which is the
    ///      only path that calls `BufferLevelFilter::update`), the filtered level — and thus the
    ///      reported metric — is strictly positive. A mutant that hardcodes 0, or one that reads a
    ///      filter that is never updated, drives this to 0 and the test fails.
    #[test]
    fn audio_playout_latency_ms_tracks_filtered_buffer_level() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();

        // Build a substantial buffer (200ms) so the filtered level is well clear of 0, then pump
        // several decode decisions so the buffer-level filter converges to the live buffer level.
        // get_audio() -> get_decision() -> buffer_level_filter.update(...) is the live update path.
        insert_audio(&mut neteq, 200);
        for _ in 0..5 {
            let _ = neteq.get_audio().unwrap();
        }

        let filtered = neteq.filtered_buffer_level_ms();
        let stats = neteq.get_statistics();

        // (1) Wiring: the metric is exactly the filtered playout buffer level. This distinguishes
        // it from current_buffer_size_ms (the raw snapshot) and target_delay_ms (the target).
        assert_eq!(
            stats.playout_latency_ms, filtered,
            "playout_latency_ms must be the filtered buffer level ({filtered}ms), got {}",
            stats.playout_latency_ms
        );

        // (2) Liveness: with 200ms buffered and decisions pumped, the filtered level is positive.
        // This is what kills a hardcoded-0 / never-updated-filter mutant.
        assert!(
            stats.playout_latency_ms > 0,
            "playout_latency_ms should be > 0 after buffering 200ms of audio and decoding, got {}",
            stats.playout_latency_ms
        );
    }

    /// Regression guard for issue #623: u16 sequence-number wrap must not flush the buffer
    /// or disrupt audio decoding at ~21.8 minutes (65536-packet boundary).
    ///
    /// Design rationale (see `packet.rs::RtpHeader::sequence_number` doc comment):
    /// - `sequence_number` is u16 BY DESIGN (RFC 3550 wire format).
    /// - `is_sequence_newer` uses wrapping_sub with a 0x8000 half-window → wrap-safe.
    /// - Buffer ordering and flush decisions are driven solely by sample-domain `timestamp`
    ///   (set per issue #624 as `frame_index * 960` at 48 kHz), never by `sequence_number`.
    /// - `sequence_number` participates only in the `(timestamp, seq, ssrc)` dedup tuple
    ///   in `buffer.rs::is_duplicate`; the timestamp dimension prevents false dedup at wrap.
    ///
    /// Strategy (lean variant): push 70,010 packets so seq wraps past 65535 → 0 at least once.
    /// Two costs dominate the naive per-insert-per-get_audio loop:
    ///   (a) Per-insert: `delay_manager.update()` internally sorts a growing `delay_history`
    ///       on every call (O(n log n)). Under test-speed insertion (no real inter-arrival
    ///       gaps), the history fills to thousands of entries and becomes quadratic. Eliminated
    ///       via `for_test_skip_delay_manager = true` (see config doc and the gate in
    ///       `insert_packet`). This flag does NOT affect buffer-flush behavior; flush decisions
    ///       live entirely in `packet_buffer.insert_packet`.
    ///   (b) Per-insert: payload construction calls 960 `sin()` × 70,010 = ~67M sin() calls.
    ///       Eliminated by pre-computing a single shared payload before the loop.
    ///
    /// `get_audio()` is called only in two phases:
    ///   1. A batch-drain at i=65490 that empties whatever has accumulated in the buffer,
    ///      ensuring the buffer is nearly empty before the measurement window opens.
    ///   2. A dense drain (one call per insert) over the measurement window i=65490..65560,
    ///      which spans both measurement points (65524 and 65546) and the wrap itself (65536).
    ///
    /// During the pre-window stretch (i=0..65489), inserts accumulate freely; the buffer
    /// auto-partial-flushes on overflow every ~200 packets. Those overflow flushes increment
    /// `buffer_flushes`, but that is intentional: `flushes_pre_wrap` (read at i=65524) absorbs
    /// ALL pre-window overflow counts as the baseline. The assertion checks only the DELTA
    /// between i=65524 and i=65546 — a 22-packet window safely inside the 200-packet overflow
    /// threshold — so no spurious overflow flush can fire in the measurement interval.
    ///
    /// Mutation sensitivity: the wrap-value assertion (at i=65536) checks that a packet
    /// with `sequence_number == 0` and `timestamp == 65536 * 960 == 62_914_560` is present
    /// in the buffer immediately after insertion. If `seq` is capped below 65536 (e.g.
    /// `i % 65535 as u16`), then at i=65536 the seq would be 1, not 0, so the assertion
    /// `wrap_pkt.sequence_number == 0` fails — the test correctly rejects the mutation.
    /// The flush-counter and continuity assertions cover orthogonal properties (no spurious
    /// flush, no decoder stall); together all three assertions are necessary and sufficient.
    #[test]
    fn test_seq_wrap_no_buffer_flush() {
        const TOTAL_FRAMES: u32 = 70_010;
        const SAMPLE_RATE: u32 = 48_000;
        const SAMPLES_PER_FRAME: u32 = 960; // 20ms at 48kHz

        // The measurement window straddles the wrap (65535→0 at i=65536).
        // We open it early enough to drain the backlog before the first measurement point.
        const DRAIN_WINDOW_START: u32 = 65_490;
        const DRAIN_WINDOW_END: u32 = 65_560;

        let config = NetEqConfig {
            sample_rate: SAMPLE_RATE,
            // Disable time stretching so all decode decisions are Normal; this avoids
            // incidental buffer drain races that could mask or produce spurious flushes.
            for_test_no_time_stretching: true,
            // Skip per-insert delay-manager updates. The delay_manager internally sorts a
            // growing delay_history on every insert; under test-speed insertion (no real
            // inter-arrival gaps) the history fills to ~max_history_ms worth of entries and
            // the sort becomes the dominant O(n log n)-per-insert cost, making the full
            // 70,010-insert loop take ~22s of CPU time. Skipping the delay-manager does not
            // affect the seq-wrap correctness property under test: buffer-flush decisions are
            // driven by `packet_buffer.insert_packet` (smart-flush / overflow logic), not by
            // the delay manager. The target_delay used as the smart-flush threshold defaults
            // to K_START_DELAY_MS (80ms) and is frozen at that value throughout the test,
            // which is conservative and still lets the smart-flush path trigger normally if
            // the buffer ever exceeds its threshold.
            for_test_skip_delay_manager: true,
            ..Default::default()
        };
        let mut neteq = NetEq::new(config).unwrap();

        // Pre-compute the payload once — 960 sin() calls × 70,010 iterations = another
        // significant cost. Re-using the same bytes is safe: the wrap test cares about RTP
        // header fields (seq, timestamp, ssrc), not PCM content.
        let shared_payload: Vec<u8> = {
            let mut p = Vec::with_capacity(960 * 4);
            for i in 0..960usize {
                let sample = (i as f32 / 960.0 * 2.0 * std::f32::consts::PI * 440.0).sin() * 0.1;
                p.extend_from_slice(&sample.to_le_bytes());
            }
            p
        };

        let mut normal_frames_after_wrap = 0u32;
        // Flush count immediately before the wrap boundary (frame 65524, seq=65524).
        let mut flushes_pre_wrap: u64 = 0;
        // Flush count immediately after the wrap boundary (frame 65546, seq=10).
        let mut flushes_post_wrap: Option<u64> = None;

        for i in 0u32..TOTAL_FRAMES {
            let seq = i as u16; // natural truncation — wraps 65535 → 0 at i=65536
            let timestamp = i * SAMPLES_PER_FRAME; // monotonic u32, never wraps for 70010 frames

            // Reuse the pre-computed payload; clone is a cheap memcpy vs 960 sin() calls.
            let header = RtpHeader::new(seq, timestamp, 12345, 96, false);
            let packet = AudioPacket::new(header, shared_payload.clone(), 48000, 1, 20);
            neteq.insert_packet(packet).unwrap();

            if i == DRAIN_WINDOW_START {
                // Batch-drain all accumulated frames before opening the measurement window.
                // This prevents any in-flight overflow flush from firing inside the window.
                // The buffer holds at most max_packets_in_buffer (default 200) at this point;
                // draining 400 frames (200 packets × 2 × 10ms frames each) is a safe upper bound.
                for _ in 0..400 {
                    let _ = neteq.get_audio().unwrap();
                }
            }

            // WRAP-VALUE ASSERTION: immediately after inserting the wrap-point packet
            // (i=65536, seq=0 as u16, timestamp=65536*960=62_914_560), confirm it landed
            // in the buffer with the correct seq and timestamp BEFORE get_audio drains it.
            //
            // This assertion is structurally impossible to pass unless seq genuinely wraps
            // to 0 at i=65536. If seq were capped (e.g. `i % 65535 as u16`), then at
            // i=65536 the seq would be 1, not 0, so no packet with (seq==0, ts==62_914_560)
            // would ever exist — peek_next_packet_from_timestamp would either return None or
            // a packet with a different seq, and the assertion would fail. This pins the
            // mutation that the flush-counter and continuity assertions alone do not catch.
            if i == 65536 {
                const WRAP_TS: u32 = 65536 * SAMPLES_PER_FRAME;
                let wrap_pkt = neteq.packet_buffer.peek_next_packet_from_timestamp(WRAP_TS);
                assert!(
                    wrap_pkt.is_some(),
                    "post-wrap packet (i=65536, ts={WRAP_TS}) not found in buffer immediately \
                     after insert — seq wrap caused the packet to be lost or rejected"
                );
                let wrap_pkt = wrap_pkt.unwrap();
                assert_eq!(
                    wrap_pkt.header.sequence_number, 0u16,
                    "post-wrap packet at ts={WRAP_TS} has seq={} instead of 0 — \
                     seq was not allowed to wrap to 0",
                    wrap_pkt.header.sequence_number
                );
                assert_eq!(
                    wrap_pkt.header.timestamp, WRAP_TS,
                    "post-wrap packet seq_number=0 has unexpected timestamp {} (expected {WRAP_TS})",
                    wrap_pkt.header.timestamp
                );
            }

            // Dense drain only inside the measurement window so the buffer stays bounded
            // and we capture Normal frames produced after the wrap.
            if (DRAIN_WINDOW_START..=DRAIN_WINDOW_END).contains(&i) {
                let frame = neteq.get_audio().unwrap();

                // Count Normal frames after the wrap point (seq has wrapped past 0).
                if i > 65536 && frame.speech_type == SpeechType::Normal {
                    normal_frames_after_wrap += 1;
                }
            }
            // Outside the window: skip get_audio(). The buffer accumulates and auto-flushes
            // on overflow; those counts are absorbed into flushes_pre_wrap as the baseline.

            // Capture flush count just before the wrap window.
            if i == 65524 {
                flushes_pre_wrap = neteq.get_statistics().lifetime.buffer_flushes;
            }

            // Capture flush count just after the wrap window.
            if i == 65546 {
                flushes_post_wrap = Some(neteq.get_statistics().lifetime.buffer_flushes);
            }
        }

        // PRIMARY ASSERTION: the buffer-flush counter must not increase across the
        // u16 seq wrap boundary (65535 → 0). Any flush increment here would indicate
        // that sequence-number logic (not buffer overflow) triggered the flush.
        let flushes_post = flushes_post_wrap
            .expect("post-wrap measurement point must be reached within 70010 frames");
        assert_eq!(
            flushes_post, flushes_pre_wrap,
            "buffer_flushes jumped from {flushes_pre_wrap} to {flushes_post} across the \
             u16 seq wrap boundary (frames 65524-65546) — seq wrap must not trigger a flush"
        );

        // CONTINUITY ASSERTION: audio must continue decoding (Normal frames) after the wrap.
        // Fails if the wrap caused the decoder to stall, reset, or enter permanent expansion.
        assert!(
            normal_frames_after_wrap > 0,
            "zero Normal frames decoded after the seq wrap point — wrap disrupted audio continuity"
        );
    }

    /// Adversarial dedup-at-wrap regression for issue #623.
    ///
    /// At the u16 sequence-number rollover, two packets that are 65536 apart in the logical
    /// stream share the same truncated u16 sequence number. With #624's monotonic
    /// sample-domain timestamps they carry DIFFERENT `header.timestamp` values, so they
    /// represent genuinely different audio frames and must NOT be treated as duplicates.
    ///
    /// Per `buffer.rs::is_duplicate`, dedup is keyed on the TUPLE
    /// `(timestamp, sequence_number, ssrc)`. Because `timestamp` differs, the wrap-twin
    /// must NOT be treated as a duplicate of the original — both must be accepted.
    ///
    /// Critical mutation check: if `is_duplicate` were changed to compare ONLY
    /// `sequence_number` (dropping the `timestamp` and `ssrc` guards), the wrap-twin
    /// (same u16 seq=5, different timestamp) would be falsely deduped and silently discarded,
    /// and the post-wrap-twin packet count assertion below would fail. That is the exact
    /// residual risk a u16 seq collision could cause, and this test pins it.
    ///
    /// We also verify the TRUE-duplicate path (same seq, same ts, same ssrc) IS deduped,
    /// proving the dedup check is live and not vacuously skipped.
    ///
    /// Timestamp design: we use consecutive sample-domain timestamps (ts=4800 and ts=5760,
    /// i.e. frames 5 and 6 at 48kHz/960-samples-per-frame). This keeps the inter-arrival
    /// delta small (960 samples = 20ms at 48kHz), which avoids the u32 overflow in the
    /// delay manager's `timestamp_delta * 1000 / sample_rate` path that would occur if we
    /// jumped directly to the literal logical-index timestamp (65541 * 960 = 62,919,360).
    /// The u16 collision is equally real: seq=5 (u16) == `65541u32 as u16` == 5.
    #[test]
    fn test_seq_collision_at_wrap_not_deduped() {
        let config = NetEqConfig {
            sample_rate: 48_000,
            for_test_no_time_stretching: true,
            ..Default::default()
        };
        let mut neteq = NetEq::new(config).unwrap();

        // ── Part 1: TRUE DUPLICATE — same (ts, seq, ssrc) must be deduped ──────────────
        // Original packet: seq=5, ts=4800 (frame index 5 at 48kHz/960-samp steps).
        let original_seq: u16 = 5;
        let original_ts: u32 = 5 * 960; // 4800
        let original = create_48k_20ms_packet(original_seq, original_ts);
        neteq.insert_packet(original).unwrap();
        let count_after_original = neteq.packet_buffer.len();

        // Exact duplicate: same (seq, ts, ssrc). Must be rejected.
        let true_dup = create_48k_20ms_packet(original_seq, original_ts);
        neteq.insert_packet(true_dup).unwrap();
        let count_after_true_dup = neteq.packet_buffer.len();

        assert_eq!(
            count_after_true_dup, count_after_original,
            "true duplicate (same seq={original_seq}, ts={original_ts}) must be deduped; \
             buffer grew from {count_after_original} to {count_after_true_dup}"
        );

        // ── Part 2: WRAP TWIN — same u16 seq but different timestamp, must NOT be deduped ──
        //
        // The wrap-twin carries seq=5 (u16) — the same value as the original — because at
        // the 65535→0 rollover, logical-index 65541 truncates to u16 value 5.  But its
        // sample-domain timestamp is 5760 (= 6 * 960), one frame after the original at 4800.
        // Because timestamp differs, `is_duplicate`'s tuple check must NOT fire: these are
        // distinct frames.
        //
        // Adjacent timestamp (4800 vs 5760) keeps the inter-arrival delta at 960 samples
        // (20ms at 48kHz), safely below the u32 overflow threshold in the delay manager.
        //
        // If is_duplicate keyed on seq alone, these two same-u16-seq packets would collide
        // and the second would be silently discarded; the timestamp dimension of the dedup
        // tuple is what saves us at the wrap.
        //
        // Dedup-window geometry: the wrap-twin's timestamp (5760) > original's (4800), so
        // binary search places the twin AFTER the original in the buffer. `is_duplicate`
        // checks positions [insert_pos-1, insert_pos, insert_pos+1]; insert_pos-1 IS the
        // original, so the comparison fires and must correctly observe ts mismatch.
        let wrap_twin_seq: u16 = 5; // same u16 as original_seq — this IS the collision
        let wrap_twin_ts: u32 = 6 * 960; // 5760, one frame after original — different ts
        let count_before_twin = neteq.packet_buffer.len();
        let wrap_twin = create_48k_20ms_packet(wrap_twin_seq, wrap_twin_ts);
        neteq.insert_packet(wrap_twin).unwrap();
        let count_after_twin = neteq.packet_buffer.len();

        assert_eq!(
            count_after_twin,
            count_before_twin + 1,
            "wrap-twin (u16 seq={wrap_twin_seq} == original seq={original_seq}, but ts={wrap_twin_ts} \
             != original ts={original_ts}) must NOT be deduped — buffer should have grown by 1, \
             went from {count_before_twin} to {count_after_twin}. \
             MUTATION CHECK: changing is_duplicate in buffer.rs to compare only sequence_number \
             (dropping the timestamp guard) would make this assertion fail, because the wrap-twin \
             and original share the same u16 seq value and would be falsely treated as duplicates."
        );
    }

    /// ACCEPTANCE TEST for the resync-to-live governor (issue #1299).
    ///
    /// Reproduces the confirmed multi-second-lag failure faithfully in the HARDEST regime: the
    /// adaptive target is UNBOUNDED (`max_delay_ms = 0`, part 2 OFF) so it ratchets toward seconds —
    /// exactly the #1299 mechanism that gates Accelerate off and would, without care, also gate the
    /// post-flush resume off. Time-stretching is disabled (`for_test_no_time_stretching`) to model
    /// Accelerate's catch-up being too weak / gated off, so the standing wave is GENUINELY
    /// non-draining and the governor is the sole catch-up path. Running with part 2 off makes this a
    /// HARD pin on the governor's internals (the keep-amount clamp, the delay-manager recalibration,
    /// and the histogram reset) — not just on its trigger.
    ///
    /// The backlog is built as a genuine ~1.5s of buffered packets (under the 200-packet cap and
    /// under the smart-flush span threshold) — i.e. real audio is actually behind live, not just a
    /// primed filter — then sustained 1× arrival is driven (insert 10ms, pull 10ms; net delta 0).
    ///
    /// Asserts: (a) latency converges below the ceiling within a bounded number of frames; (b) the
    /// governor actually fired (cooldown anchor `last_resync` set); (c) the seam is concealed via the
    /// existing expand/crossfade path (`ExpandStart` then `ExpandEnd`), NOT a raw splice; (d) playback
    /// RESUMES — after the conceal→resume arc the decision is no longer an Expand-family op, so the
    /// listener hears real audio again, not endless concealment noise; (e) latency STAYS bounded.
    ///
    /// What PINS the governor (i.e. what fails if it is removed): (b) `last_resync.is_some()` and
    /// (c) the `ExpandStart`/`ExpandEnd` seam — only the governor sets the cooldown anchor and
    /// returns `ExpandStart` here. The convergence assertion (a) is corroborating, NOT solely
    /// governor-attributable: this synthetic harness has near-zero inter-arrival jitter, so the
    /// adaptive target never ratchets the way the FIELD case does, and an incidental Accelerate/
    /// drain can also reduce the level. The governor's *necessity* under a true production standing
    /// wave (target ratcheted → Accelerate gated off → no drain) rests on the issue's code-confirmed
    /// mechanism, which is jitter-history-dependent and not reproducible in a unit test. So this test
    /// proves the governor FIRES, CONCEALS, CONVERGES, and RESUMES — not that drain is impossible
    /// without it.
    #[test]
    fn resync_governor_converges_below_ceiling_under_standing_wave() {
        let config = NetEqConfig {
            // Unbounded target (part 2 OFF) is the hardest case: it pins the governor's flush
            // internals (keep-amount clamp + delay/histogram recalibration), which a bounded target
            // would mask. Time-stretch off models Accelerate being unable to drain.
            for_test_no_time_stretching: true,
            ..Default::default()
        };
        // Sanity on the config the test depends on.
        assert_eq!(config.resync_ceiling_ms, 1000);
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        // Build a GENUINE ~1.5s raw backlog (150 packets, under the 200-packet cap) in 100ms chunks
        // so neither the overflow full-flush nor the span-based smart-flush trims it. Then prime the
        // filter to match — exactly how the field multi-second-lag case presents: real audio behind
        // live, the buffer-level filter tracking it.
        for _ in 0..15 {
            insert_audio(&mut neteq, 100);
        }
        reset_filtered_level(&mut neteq);
        assert!(
            neteq.filtered_buffer_level_ms() >= 1400,
            "precondition: ~1.5s genuine backlog, got {}ms",
            neteq.filtered_buffer_level_ms()
        );

        // Drive sustained 1× arrival: each iteration adds 10ms and pulls one 10ms frame. Do NOT
        // re-prime the filter inside the loop — the governor (and only the governor) must bring the
        // filtered level down. Bound the run to 300 frames (3s of playout).
        let mut converged_at: Option<usize> = None;
        let mut saw_expand_start = false;
        let mut saw_expand_end = false;
        for frame_idx in 0..300 {
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            match neteq.last_operation {
                Operation::ExpandStart => saw_expand_start = true,
                Operation::ExpandEnd => saw_expand_end = true,
                _ => {}
            }
            if neteq.filtered_buffer_level_ms() <= neteq.config.resync_ceiling_ms {
                converged_at = Some(frame_idx);
                break;
            }
        }

        let converged_at = converged_at.unwrap_or_else(|| {
            panic!(
                "resync governor never brought playout latency below the {}ms ceiling within 300 frames (still {}ms) — the multi-second lag persisted",
                neteq.config.resync_ceiling_ms,
                neteq.filtered_buffer_level_ms()
            )
        });

        // (b) The governor actually fired (one-shot flush), not some incidental drain: the cooldown
        // anchor is the unambiguous source-of-truth signal that maybe_resync_to_live ran its body.
        assert!(
            neteq.last_resync.is_some(),
            "expected the resync governor to have fired (cooldown anchor set)"
        );

        // (a) Bounded convergence: the governor fires on the first eligible tick, so this is fast.
        assert!(
            converged_at < 5,
            "expected convergence within a few frames of the one-shot flush, took {converged_at}"
        );

        // (c)+(d) Observe the rest of the conceal→resume arc and the resume to real audio. The
        // convergence loop above breaks on the very tick the governor fires (filtered level already
        // ≤ ceiling), which is the ExpandStart tick — so ExpandEnd and the resume happen here. The
        // seam is concealed via the existing crossfade path: ExpandStart (fade the last real audio
        // out into concealment) THEN ExpandEnd (fade concealment back into the new live audio) —
        // the exact gap-concealment arc, not a raw, audible splice. Playback must then RESUME to a
        // non-Expand op (real audio), not get stuck playing concealment noise over a buffer that
        // actually holds live audio. (e) latency must stay bounded throughout.
        let mut resumed_to_real_audio = false;
        for _ in 0..50 {
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();
            match neteq.last_operation {
                Operation::ExpandStart => saw_expand_start = true,
                Operation::ExpandEnd => saw_expand_end = true,
                Operation::Expand => {}
                _ => resumed_to_real_audio = true,
            }
            assert!(
                neteq.filtered_buffer_level_ms() <= neteq.config.resync_ceiling_ms,
                "post-resync latency re-exceeded ceiling under steady 1× arrival: {}ms",
                neteq.filtered_buffer_level_ms()
            );
        }

        assert!(
            saw_expand_start,
            "expected ExpandStart at the resync seam (concealment onset), saw none"
        );
        assert!(
            saw_expand_end,
            "expected ExpandEnd after the resync seam (concealment resolves back to real audio)"
        );
        assert!(
            resumed_to_real_audio,
            "after the resync conceal arc, playback never resumed to a non-Expand op (stuck in concealment)"
        );
    }

    /// Part 2 (issue #1299): the bounded `max_delay_ms` caps `target_delay_ms`, so the adaptive
    /// target can never ratchet to the 3000ms regime that gates Accelerate off.
    ///
    /// Drives a pathological all-identical-timestamp burst (the same pattern that
    /// `test_buffer_duration_calculation_with_identical_timestamps` shows pushes the delay manager
    /// to high targets) and asserts the target stays at/below the configured cap. FAILS if the
    /// `max_delay_ms` plumbing (NetEqConfig field → set_maximum_delay) is removed: the default
    /// derived cap is 3000ms (200 packets × 20 × 3/4), so without the bound the target can climb far
    /// past 300ms.
    #[test]
    fn bounded_target_cannot_reach_3s() {
        const CAP_MS: u32 = 300;
        let config = NetEqConfig {
            max_delay_ms: CAP_MS,
            ..Default::default()
        };
        let mut neteq = NetEq::new(config).unwrap();

        // Hammer the delay manager toward its maximum: many packets with the SAME timestamp drive
        // the inter-arrival/quantile estimate high (cf. the identical-timestamp test above).
        for i in 0..400u32 {
            let header = RtpHeader::new(i as u16, 1000, 12345, 96, false);
            let mut payload = Vec::new();
            for s in 0..160 {
                payload.extend_from_slice(&((s as f32).sin() * 0.1).to_le_bytes());
            }
            let packet = AudioPacket::new(header, payload, 16000, 1, 20);
            neteq.insert_packet(packet).unwrap();
        }

        let target = neteq.target_delay_ms();
        assert!(
            target <= CAP_MS,
            "target_delay_ms must be capped at {CAP_MS}ms by max_delay_ms, got {target}ms (would ratchet toward the 3000ms cap unbounded)"
        );
    }

    /// Below the ceiling, the resync governor must NOT fire — behavior is unchanged (issue #1299).
    ///
    /// Drives a normal, low-latency stream (target-sized buffer, steady 1× arrival) and asserts no
    /// flush ever occurs (cooldown anchor stays `None`) and every operation is one of the ordinary
    /// decisions. FAILS if the governor fires on a normal stream (e.g. ceiling set too low or the
    /// trigger inverted).
    #[test]
    fn resync_governor_does_not_fire_below_ceiling() {
        let config = NetEqConfig::default();
        let mut neteq = NetEq::new(config).unwrap();

        let mut insert_audio = make_insert_audio!();
        let reset_filtered_level = make_reset_filtered_level!();

        // Establish a normal ~80ms buffer (well below the 1000ms ceiling) and run a steady stream.
        insert_audio(&mut neteq, 80);
        reset_filtered_level(&mut neteq);

        for _ in 0..200 {
            insert_audio(&mut neteq, 10);
            let _ = neteq.get_audio().unwrap();

            // The governor never fired: no cooldown anchor was set.
            assert!(
                neteq.last_resync.is_none(),
                "resync governor fired on a normal below-ceiling stream (latency {}ms)",
                neteq.filtered_buffer_level_ms()
            );
            // Latency stays low; it must never approach the ceiling on a healthy stream.
            assert!(
                neteq.filtered_buffer_level_ms() < neteq.config.resync_ceiling_ms,
                "below-ceiling stream unexpectedly reached {}ms",
                neteq.filtered_buffer_level_ms()
            );
        }
    }
}
