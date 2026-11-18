// `with_components` is a test seam; it takes the packet arrival history as a
// trait object so tests can fake `GetDelayMs` / `GetMaxDelayMs` like the C++
// gmock does.
//
// Port target: WebRTC modules/audio_coding/neteq/decision_logic.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd (tests: decision_logic_unittest.cc).
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// The `Environment` constructor becomes `new`; the controller factory is a
// closure in `neteq_impl::ControllerFactory`.

//! The default NetEq controller.

use crate::api::delay_manager_interface::DelayManagerInterface;
use crate::api::neteq::{Mode, Operation};
use crate::api::neteq_controller::{
    NetEqController, NetEqControllerConfig, NetEqStatus, PacketArrivedInfo,
};
use crate::api::tick_timer::{Countdown, TickTimer};
use crate::neteq::buffer_level_filter::BufferLevelFilter;
use crate::neteq::delay_constraints::DelayConstraints;
use crate::neteq::packet_arrival_history::{PacketArrivalHistory, PacketArrivalHistoryInterface};
use crate::neteq::packet_buffer::PacketBuffer;
use crate::rtc_base::saturated_cast;

const POSTPONE_DECODING_LEVEL: i32 = 50;
const TARGET_LEVEL_WINDOW_MS: i32 = 100;
/// The granularity of delay adjustments (accelerate/preemptive expand) is
/// 15 ms, but round up since the clock has a granularity of 10 ms.
const DELAY_ADJUSTMENT_GRANULARITY_MS: i32 = 20;
const PACKET_HISTORY_SIZE_MS: i32 = 2000;
const CNG_TIMEOUT_MS: usize = 1000;

/// The value 5 sets maximum time-stretch rate to about 100 ms/s.
const MIN_TIMESCALE_INTERVAL: u64 = 5;

fn is_timestretch(mode: Mode) -> bool {
    matches!(
        mode,
        Mode::AccelerateSuccess
            | Mode::AccelerateLowEnergy
            | Mode::PreemptiveExpandSuccess
            | Mode::PreemptiveExpandLowEnergy
    )
}

fn is_cng(mode: Mode) -> bool {
    matches!(mode, Mode::Rfc3389Cng | Mode::CodecInternalCng)
}

fn is_expand(mode: Mode) -> bool {
    matches!(mode, Mode::Expand | Mode::CodecPlc)
}

/// C++ `DecisionLogic`: the decision tree.
pub struct DecisionLogic {
    delay_manager: Box<dyn DelayManagerInterface>,
    delay_constraints: DelayConstraints,
    buffer_level_filter: BufferLevelFilter,
    packet_arrival_history: Box<dyn PacketArrivalHistoryInterface>,
    tick_timer: TickTimer,
    /// Uninitialized in C++ until `SetSampleRate`; 0 here.
    sample_rate_khz: i32,
    /// Uninitialized in C++ until `SetSampleRate`; 0 here. Stored but never
    /// read, as in C++.
    #[allow(dead_code)]
    output_size_samples: usize,
    noise_fast_forward: usize,
    packet_length_samples: usize,
    sample_memory: i32,
    prev_time_scale: bool,
    disallow_time_stretching: bool,
    /// Always set in C++ (`std::unique_ptr` created in the constructor).
    timescale_countdown: Countdown,
    time_stretched_cn_samples: i32,
    buffer_flush: bool,
}

impl DecisionLogic {
    /// C++ `DecisionLogic(env, config, delay_manager)`: default buffer level
    /// filter and packet arrival history.
    pub fn new(
        config: NetEqControllerConfig,
        delay_manager: Box<dyn DelayManagerInterface>,
    ) -> Self {
        Self::with_components(config, delay_manager, BufferLevelFilter::new(), None)
    }

    /// C++ test constructor with injected components. `None` for
    /// `packet_arrival_history` means the default
    /// (`kPacketHistorySizeMs` window on `config.tick_timer`).
    pub fn with_components(
        config: NetEqControllerConfig,
        delay_manager: Box<dyn DelayManagerInterface>,
        buffer_level_filter: BufferLevelFilter,
        packet_arrival_history: Option<Box<dyn PacketArrivalHistoryInterface>>,
    ) -> Self {
        let packet_arrival_history = packet_arrival_history.unwrap_or_else(|| {
            Box::new(PacketArrivalHistory::new(
                config.tick_timer.clone(),
                PACKET_HISTORY_SIZE_MS,
            ))
        });
        let timescale_countdown = config
            .tick_timer
            .get_new_countdown(MIN_TIMESCALE_INTERVAL + 1);
        Self {
            delay_manager,
            delay_constraints: DelayConstraints::new(
                config.max_packets_in_buffer,
                config.base_min_delay_ms,
            ),
            buffer_level_filter,
            packet_arrival_history,
            tick_timer: config.tick_timer,
            sample_rate_khz: 0,
            output_size_samples: 0,
            noise_fast_forward: 0,
            packet_length_samples: 0,
            sample_memory: 0,
            prev_time_scale: false,
            disallow_time_stretching: !config.allow_time_stretching,
            timescale_countdown,
            time_stretched_cn_samples: 0,
            buffer_flush: false,
        }
    }

    /// Checks if enough time has elapsed since the last successful timescale
    /// operation was done (i.e., accelerate or preemptive expand).
    fn timescale_allowed(&self) -> bool {
        self.timescale_countdown.finished()
    }

    /// Updates the buffer level filter with the current buffer level
    /// `buffer_size_samples`.
    fn filter_buffer_level(&mut self, buffer_size_samples: usize) {
        self.buffer_level_filter
            .set_target_buffer_level(self.target_level_ms());
        let mut time_stretched_samples = self.time_stretched_cn_samples;
        if self.prev_time_scale {
            time_stretched_samples = time_stretched_samples.wrapping_add(self.sample_memory);
        }

        if self.buffer_flush {
            // C++ passes `size_t` to an `int` parameter: truncating.
            self.buffer_level_filter
                .set_filtered_buffer_level(buffer_size_samples as i32);
            self.buffer_flush = false;
        } else {
            self.buffer_level_filter
                .update(buffer_size_samples, time_stretched_samples);
        }
        self.prev_time_scale = false;
        self.time_stretched_cn_samples = 0;
    }

    /// The operation given that the next available packet is a comfort noise
    /// payload (RFC 3389 only, not codec-internal).
    fn cng_operation(&mut self, status: &NetEqStatus) -> Operation {
        let next_packet = status
            .next_packet
            .expect("cng_operation requires a next packet");
        // Signed difference between target and available timestamp.
        let mut timestamp_diff: i32 = ((status
            .generated_noise_samples
            .wrapping_add(status.target_timestamp as usize))
            as u32)
            .wrapping_sub(next_packet.timestamp) as i32;
        let optimal_level_samp = self.target_level_ms().wrapping_mul(self.sample_rate_khz);
        let excess_waiting_time_samp: i64 = -(timestamp_diff as i64) - optimal_level_samp as i64;

        if excess_waiting_time_samp > (optimal_level_samp / 2) as i64 {
            // The waiting time for this packet will be longer than 1.5 times
            // the wanted buffer delay. Apply fast-forward to cut the waiting
            // time down to the optimal.
            // C++: `size_t + int64_t` is computed as unsigned 64-bit.
            self.noise_fast_forward = saturated_cast::<usize, _>(
                (self.noise_fast_forward as u64).wrapping_add(excess_waiting_time_samp as u64),
            );
            timestamp_diff =
                saturated_cast::<i32, _>(timestamp_diff as i64 + excess_waiting_time_samp);
        }

        if timestamp_diff < 0 && status.last_mode == Mode::Rfc3389Cng {
            // Not time to play this packet yet. Wait another round before
            // using this packet. Keep on playing CNG from previous CNG
            // parameters.
            Operation::Rfc3389CngNoPacket
        } else {
            // Otherwise, go for the CNG packet now.
            self.noise_fast_forward = 0;
            Operation::Rfc3389Cng
        }
    }

    /// The operation given that no packets are available (except maybe a
    /// DTMF event, flagged by `play_dtmf`).
    fn no_packet(&self, status: &NetEqStatus) -> Operation {
        match status.last_mode {
            Mode::Rfc3389Cng => Operation::Rfc3389CngNoPacket,
            Mode::CodecInternalCng => {
                // Stop CNG after a timeout.
                if status.generated_noise_samples
                    > CNG_TIMEOUT_MS.wrapping_mul(self.sample_rate_khz as usize)
                {
                    return Operation::Expand;
                }
                Operation::CodecInternalCng
            }
            _ => {
                if status.play_dtmf {
                    Operation::Dtmf
                } else {
                    Operation::Expand
                }
            }
        }
    }

    /// The operation given that the expected packet is available.
    fn expected_packet_available(&self, status: &NetEqStatus) -> Operation {
        if !self.disallow_time_stretching && status.last_mode != Mode::Expand && !status.play_dtmf {
            let playout_delay_ms = self.get_playout_delay_ms(status) as i64;
            let low_limit = self.target_level_ms() as i64;
            let high_limit = low_limit
                + self.packet_arrival_history.get_max_delay_ms() as i64
                + DELAY_ADJUSTMENT_GRANULARITY_MS as i64;
            if playout_delay_ms >= high_limit * 4 {
                return Operation::FastAccelerate;
            }
            if self.timescale_allowed() {
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

    /// The operation given that the expected packet is not available, but a
    /// packet further into the future is at hand.
    fn future_packet_available(&mut self, status: &NetEqStatus) -> Operation {
        let next_packet = status
            .next_packet
            .expect("future_packet_available requires a next packet");
        // Required packet is not available, but a future packet is.
        // Check if we should continue with an ongoing concealment because the
        // new packet is too far into the future.
        let buffer_delay_samples = status.packet_buffer_info.span_samples_wait_time as i32;
        let buffer_delay_ms = buffer_delay_samples / self.sample_rate_khz;
        let high_limit = self.target_level_ms() + TARGET_LEVEL_WINDOW_MS / 2;
        let above_target_delay = buffer_delay_ms > high_limit;
        if self.packet_too_early(status) && !above_target_delay {
            return self.no_packet(status);
        }
        let timestamp_leap = next_packet.timestamp.wrapping_sub(status.target_timestamp);
        if timestamp_leap as usize != status.generated_noise_samples {
            // The delay was adjusted, reinitialize the buffer level filter.
            self.buffer_level_filter
                .set_filtered_buffer_level(buffer_delay_samples);
        }

        // Time to play the next packet.
        match status.last_mode {
            Mode::Expand => Operation::Merge,
            Mode::CodecPlc | Mode::Rfc3389Cng | Mode::CodecInternalCng => Operation::Normal,
            _ => {
                if status.play_dtmf {
                    Operation::Dtmf
                } else {
                    Operation::Expand
                }
            }
        }
    }

    /// Checks if the current (filtered) buffer level is under the target
    /// level. Unused in C++ too; kept so the file diffs against its twin.
    #[allow(dead_code)]
    fn under_target_level(&self) -> bool {
        self.buffer_level_filter.filtered_current_level()
            < self.target_level_ms().wrapping_mul(self.sample_rate_khz)
    }

    /// Checks if an ongoing concealment should be continued due to low buffer
    /// level, even though the next packet is available.
    fn postpone_decode(&self, status: &NetEqStatus) -> bool {
        // Make sure we don't restart audio too soon after CNG or expand to
        // avoid running out of data right away again.
        let min_buffer_level_samples = (self
            .target_level_ms()
            .wrapping_mul(self.sample_rate_khz)
            .wrapping_mul(POSTPONE_DECODING_LEVEL)
            / 100) as usize;
        let buffer_level_samples = status.packet_buffer_info.span_samples_wait_time;
        if buffer_level_samples >= min_buffer_level_samples {
            return false;
        }
        // Don't postpone decoding if there is a future DTX packet in the
        // packet buffer.
        if status.packet_buffer_info.dtx_or_cng {
            return false;
        }
        // Continue CNG until the buffer is at least at the minimum level.
        if is_cng(status.last_mode) {
            return true;
        }
        // Only continue expand if the mute factor is low enough (otherwise
        // the expansion was short enough to not be noticable). Note that the
        // MuteFactor is in Q14, so a value of 16384 corresponds to 1.
        if is_expand(status.last_mode) && status.expand_mutefactor < 16384 / 2 {
            return true;
        }
        false
    }

    /// Checks if we still have not done enough expands to cover the distance
    /// from the last decoded packet to the next available packet.
    fn packet_too_early(&self, status: &NetEqStatus) -> bool {
        let next_packet = status
            .next_packet
            .expect("packet_too_early requires a next packet");
        let timestamp_leap = next_packet.timestamp.wrapping_sub(status.target_timestamp);
        timestamp_leap as usize > status.generated_noise_samples
    }

    fn get_playout_delay_ms(&self, status: &NetEqStatus) -> i32 {
        // C++: `uint32_t - size_t`, stored in `uint32_t`.
        let playout_timestamp =
            (status.target_timestamp as usize).wrapping_sub(status.sync_buffer_samples) as u32;
        self.packet_arrival_history.get_delay_ms(playout_timestamp)
    }
}

impl NetEqController for DecisionLogic {
    /// Not used.
    fn reset(&mut self) {}

    fn soft_reset(&mut self) {
        self.packet_length_samples = 0;
        self.sample_memory = 0;
        self.prev_time_scale = false;
        self.timescale_countdown = self
            .tick_timer
            .get_new_countdown(MIN_TIMESCALE_INTERVAL + 1);
        self.time_stretched_cn_samples = 0;
        self.delay_manager.reset();
        self.buffer_level_filter.reset();
        self.packet_arrival_history.reset();
    }

    fn get_decision(&mut self, status: &NetEqStatus, _reset_decoder: &mut bool) -> Operation {
        self.prev_time_scale = self.prev_time_scale && is_timestretch(status.last_mode);
        if self.prev_time_scale {
            self.timescale_countdown = self.tick_timer.get_new_countdown(MIN_TIMESCALE_INTERVAL);
        }
        if !is_cng(status.last_mode) && !is_expand(status.last_mode) {
            self.filter_buffer_level(status.packet_buffer_info.span_samples);
        }

        // Guard for errors, to avoid getting stuck in error mode.
        if status.last_mode == Mode::Error {
            if status.next_packet.is_none() {
                return Operation::Expand;
            } else {
                // Use Undefined to flag for a reset.
                return Operation::Undefined;
            }
        }

        let Some(next_packet) = status.next_packet else {
            // Handle the case with no packet at all available (except maybe
            // DTMF).
            return self.no_packet(status);
        };

        if next_packet.is_cng {
            return self.cng_operation(status);
        }

        if self.postpone_decode(status) {
            return self.no_packet(status);
        }

        let five_seconds_samples = 5000i32.wrapping_mul(self.sample_rate_khz) as u32;
        // Check if the required packet is available.
        if status.target_timestamp == next_packet.timestamp {
            return self.expected_packet_available(status);
        }
        if !PacketBuffer::is_obsolete_timestamp(
            next_packet.timestamp,
            status.target_timestamp,
            five_seconds_samples,
        ) {
            return self.future_packet_available(status);
        }
        // This implies that available_timestamp < target_timestamp, which can
        // happen when a new stream or codec is received. Signal for a reset.
        Operation::Undefined
    }

    fn register_empty_packet(&mut self) {}

    fn set_sample_rate(&mut self, fs_hz: i32, output_size_samples: usize) {
        debug_assert!(fs_hz == 8000 || fs_hz == 16000 || fs_hz == 32000 || fs_hz == 48000);
        self.sample_rate_khz = fs_hz / 1000;
        self.output_size_samples = output_size_samples;
        self.packet_arrival_history.set_sample_rate(fs_hz);
    }

    fn set_maximum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_maximum_delay(delay_ms)
    }

    fn set_minimum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_minimum_delay(delay_ms)
    }

    fn set_base_minimum_delay(&mut self, delay_ms: i32) -> bool {
        self.delay_constraints.set_base_minimum_delay(delay_ms)
    }

    fn get_base_minimum_delay(&self) -> i32 {
        self.delay_constraints.get_base_minimum_delay()
    }

    fn expand_decision(&mut self, _operation: Operation) {}

    fn add_sample_memory(&mut self, value: i32) {
        self.sample_memory = self.sample_memory.wrapping_add(value);
    }

    fn target_level_ms(&self) -> i32 {
        self.delay_constraints
            .clamp(self.unlimited_target_level_ms())
    }

    fn unlimited_target_level_ms(&self) -> i32 {
        self.delay_manager.target_delay_ms()
    }

    fn packet_arrived(
        &mut self,
        fs_hz: i32,
        should_update_stats: bool,
        info: &PacketArrivedInfo,
    ) -> Option<i32> {
        self.buffer_flush = self.buffer_flush || info.buffer_flush;
        if !should_update_stats || info.is_cng_or_dtmf {
            return None;
        }
        if info.packet_length_samples > 0
            && fs_hz > 0
            && info.packet_length_samples != self.packet_length_samples
        {
            self.packet_length_samples = info.packet_length_samples;
            // C++: `size_t * 1000 / int` in size_t, passed as `int`.
            self.delay_constraints.set_packet_audio_length(
                (self.packet_length_samples.wrapping_mul(1000) / fs_hz as usize) as i32,
            );
        }
        let inserted = self
            .packet_arrival_history
            .insert(info.main_timestamp, info.packet_length_samples as i32);
        if !inserted || self.packet_arrival_history.size() < 2 {
            // No meaningful delay estimate unless at least 2 packets have
            // arrived.
            return None;
        }
        let arrival_delay_ms = self
            .packet_arrival_history
            .get_delay_ms(info.main_timestamp);
        let reordered = !self
            .packet_arrival_history
            .is_newest_rtp_timestamp(info.main_timestamp);
        self.delay_manager
            .update(arrival_delay_ms, reordered, *info);
        Some(arrival_delay_ms)
    }

    fn peak_found(&self) -> bool {
        false
    }

    fn get_filtered_buffer_level(&self) -> i32 {
        self.buffer_level_filter.filtered_current_level()
    }

    fn set_sample_memory(&mut self, value: i32) {
        self.sample_memory = value;
    }

    fn noise_fast_forward(&self) -> usize {
        self.noise_fast_forward
    }

    fn packet_length_samples(&self) -> usize {
        self.packet_length_samples
    }

    fn set_packet_length_samples(&mut self, value: usize) {
        self.packet_length_samples = value;
    }

    fn set_prev_time_scale(&mut self, value: bool) {
        self.prev_time_scale = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::config::DelayManagerConfig;
    use crate::api::neteq_controller::PacketInfo;
    use crate::neteq::delay_manager::DelayManager;

    const SAMPLE_RATE: i32 = 8000;
    const SAMPLES_PER_MS: i32 = SAMPLE_RATE / 1000;
    const OUTPUT_SIZE_SAMPLES: usize = (SAMPLES_PER_MS * 10) as usize;
    const MIN_TIMESCALE_INTERVAL: u64 = 5;

    /// Stands in for C++ `MockDelayManager` (only `TargetDelayMs` mocked).
    struct FakeDelayManager {
        target_delay_ms: i32,
    }

    impl DelayManagerInterface for FakeDelayManager {
        fn update(&mut self, _arrival_delay_ms: i32, _reordered: bool, _info: PacketArrivedInfo) {}
        fn reset(&mut self) {}
        fn target_delay_ms(&self) -> i32 {
            self.target_delay_ms
        }
    }

    /// Stands in for C++ `MockPacketArrivalHistory`: a real history with a
    /// zero window, with `GetDelayMs` and `GetMaxDelayMs` overridden. gmock
    /// returns 0 for calls without an expectation, so 0 is the default.
    struct FakePacketArrivalHistory {
        real: PacketArrivalHistory,
        delay_ms: i32,
        max_delay_ms: i32,
    }

    impl PacketArrivalHistoryInterface for FakePacketArrivalHistory {
        fn insert(&mut self, rtp_timestamp: u32, packet_length_samples: i32) -> bool {
            self.real.insert(rtp_timestamp, packet_length_samples)
        }
        fn get_delay_ms(&self, _rtp_timestamp: u32) -> i32 {
            self.delay_ms
        }
        fn get_max_delay_ms(&self) -> i32 {
            self.max_delay_ms
        }
        fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool {
            self.real.is_newest_rtp_timestamp(rtp_timestamp)
        }
        fn reset(&mut self) {
            self.real.reset()
        }
        fn set_sample_rate(&mut self, sample_rate: i32) {
            self.real.set_sample_rate(sample_rate)
        }
        fn size(&self) -> usize {
            self.real.size()
        }
    }

    fn controller_config(tick_timer: &TickTimer) -> NetEqControllerConfig {
        NetEqControllerConfig {
            allow_time_stretching: true,
            enable_rtx_handling: false,
            max_packets_in_buffer: 200,
            base_min_delay_ms: 0,
            tick_timer: tick_timer.clone(),
        }
    }

    /// C++ `DecisionLogicTest` fixture, with the mock return values passed in.
    fn create_decision_logic(
        tick_timer: &TickTimer,
        target_delay_ms: i32,
        delay_ms: i32,
        max_delay_ms: i32,
    ) -> DecisionLogic {
        let config = controller_config(tick_timer);
        let packet_arrival_history = FakePacketArrivalHistory {
            real: PacketArrivalHistory::new(tick_timer.clone(), 0),
            delay_ms,
            max_delay_ms,
        };
        let mut decision_logic = DecisionLogic::with_components(
            config,
            Box::new(FakeDelayManager { target_delay_ms }),
            BufferLevelFilter::new(),
            Some(Box::new(packet_arrival_history)),
        );
        decision_logic.set_sample_rate(SAMPLE_RATE, OUTPUT_SIZE_SAMPLES);
        decision_logic
    }

    fn create_neteq_status(last_mode: Mode, current_delay_ms: i32) -> NetEqStatus {
        let samples = (current_delay_ms * SAMPLES_PER_MS) as usize;
        let target_timestamp = 1234;
        NetEqStatus {
            target_timestamp,
            expand_mutefactor: 0,
            last_packet_samples: 0,
            next_packet: Some(PacketInfo {
                timestamp: target_timestamp,
                is_dtx: false,
                is_cng: false,
            }),
            last_mode,
            play_dtmf: false,
            generated_noise_samples: 0,
            packet_buffer_info: crate::api::neteq_controller::PacketBufferInfo {
                dtx_or_cng: false,
                num_samples: samples,
                span_samples: samples,
                span_samples_wait_time: samples,
                num_packets: 0,
            },
            sync_buffer_samples: 0,
        }
    }

    #[test]
    fn normal_operation() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 100, 0);
        let mut reset_decoder = false;
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL + 1);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::Normal
        );
        assert!(!reset_decoder);
    }

    #[test]
    fn accelerate() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 150, 0);
        let mut reset_decoder = false;
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL + 1);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::Accelerate
        );
        assert!(!reset_decoder);
    }

    #[test]
    fn fast_accelerate() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 500, 0);
        let mut reset_decoder = false;
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL + 1);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::FastAccelerate
        );
        assert!(!reset_decoder);
    }

    #[test]
    fn preemptive_expand() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 50, 0);
        let mut reset_decoder = false;
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL + 1);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::PreemptiveExpand
        );
        assert!(!reset_decoder);
    }

    #[test]
    fn postpone_decode_after_expand() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 500, 0, 0);

        // Below 50% target delay threshold.
        let mut reset_decoder = false;
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Expand, 200), &mut reset_decoder),
            Operation::Expand
        );
        assert!(!reset_decoder);

        // Above 50% target delay threshold.
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Expand, 250), &mut reset_decoder),
            Operation::Normal
        );
        assert!(!reset_decoder);
    }

    #[test]
    fn time_strech_comfort_noise() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 500, 0, 0);

        {
            let mut reset_decoder = false;
            // Below target window.
            let mut status = create_neteq_status(Mode::CodecInternalCng, 200);
            status.generated_noise_samples = (400 * SAMPLES_PER_MS) as usize;
            status.next_packet.as_mut().unwrap().timestamp =
                status.target_timestamp + (400 * SAMPLES_PER_MS) as u32;
            assert_eq!(
                decision_logic.get_decision(&status, &mut reset_decoder),
                Operation::CodecInternalCng
            );
            assert!(!reset_decoder);
        }

        {
            let mut reset_decoder = false;
            // Above target window.
            let mut status = create_neteq_status(Mode::CodecInternalCng, 600);
            status.generated_noise_samples = (200 * SAMPLES_PER_MS) as usize;
            status.next_packet.as_mut().unwrap().timestamp =
                status.target_timestamp + (400 * SAMPLES_PER_MS) as u32;
            assert_eq!(
                decision_logic.get_decision(&status, &mut reset_decoder),
                Operation::Normal
            );
            assert!(!reset_decoder);
        }
    }

    #[test]
    fn cng_timeout() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 0, 0, 0);
        let mut status = create_neteq_status(Mode::CodecInternalCng, 0);
        status.next_packet = None;
        status.generated_noise_samples = (SAMPLES_PER_MS * 500) as usize;
        let mut reset_decoder = false;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::CodecInternalCng
        );
        status.generated_noise_samples = (SAMPLES_PER_MS * 1010) as usize;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Expand
        );
    }

    // The tests below are not in C++; they pin branches the C++ suite does
    // not reach.

    #[test]
    fn timescale_blocked_until_countdown_finishes() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 150, 0);
        let mut reset_decoder = false;
        // The countdown starts at kMinTimescaleInterval + 1 ticks.
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::Normal
        );
        tick_timer.increment();
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::Accelerate
        );
        // After a successful time stretch the countdown restarts at
        // kMinTimescaleInterval ticks.
        decision_logic.set_prev_time_scale(true);
        assert_eq!(
            decision_logic.get_decision(
                &create_neteq_status(Mode::AccelerateSuccess, 100),
                &mut reset_decoder
            ),
            Operation::Normal
        );
        tick_timer.increment_by(MIN_TIMESCALE_INTERVAL);
        assert_eq!(
            decision_logic
                .get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder),
            Operation::Accelerate
        );
    }

    #[test]
    fn error_mode_guard() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 0, 0);
        let mut reset_decoder = false;
        let mut status = create_neteq_status(Mode::Error, 100);
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Undefined
        );
        status.next_packet = None;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Expand
        );
    }

    #[test]
    fn future_packet_merges_after_expand() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 0, 0);
        let mut reset_decoder = false;
        let mut status = create_neteq_status(Mode::Expand, 100);
        status.expand_mutefactor = 16384;
        status.next_packet.as_mut().unwrap().timestamp = status.target_timestamp + 160;
        status.generated_noise_samples = 160;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Merge
        );
        // Not enough expansion yet to cover the gap: keep expanding.
        status.generated_noise_samples = 80;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Expand
        );
        // From normal mode a future packet means expand.
        let mut status = create_neteq_status(Mode::Normal, 100);
        status.next_packet.as_mut().unwrap().timestamp = status.target_timestamp + 160;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Expand
        );
    }

    #[test]
    fn old_packet_signals_reset() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 0, 0);
        let mut reset_decoder = false;
        let mut status = create_neteq_status(Mode::Normal, 100);
        status.next_packet.as_mut().unwrap().timestamp = status.target_timestamp - 1;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Undefined
        );
    }

    #[test]
    fn rfc3389_cng_operation() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 0, 0);
        let mut reset_decoder = false;
        let mut status = create_neteq_status(Mode::Rfc3389Cng, 100);
        status.next_packet = Some(PacketInfo {
            timestamp: status.target_timestamp + 80,
            is_dtx: false,
            is_cng: true,
        });
        // Packet is 10 ms in the future and we are in CNG: keep playing CNG.
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Rfc3389CngNoPacket
        );
        assert_eq!(decision_logic.noise_fast_forward(), 0);
        // Packet 2 s in the future: fast-forward down to the target level.
        // excess = 16000 - 800 = 15200 > 400.
        status.next_packet.as_mut().unwrap().timestamp = status.target_timestamp + 16000;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Rfc3389CngNoPacket
        );
        assert_eq!(decision_logic.noise_fast_forward(), 15200);
        // Due packet: play it and clear the fast-forward.
        status.next_packet.as_mut().unwrap().timestamp = status.target_timestamp;
        assert_eq!(
            decision_logic.get_decision(&status, &mut reset_decoder),
            Operation::Rfc3389Cng
        );
        assert_eq!(decision_logic.noise_fast_forward(), 0);
    }

    #[test]
    fn packet_arrived_feeds_delay_manager() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = DecisionLogic::new(
            controller_config(&tick_timer),
            Box::new(DelayManager::new(
                &DelayManagerConfig::default(),
                tick_timer.clone(),
            )),
        );
        decision_logic.set_sample_rate(SAMPLE_RATE, OUTPUT_SIZE_SAMPLES);
        let info = |timestamp: u32| PacketArrivedInfo {
            packet_length_samples: 160,
            main_timestamp: timestamp,
            main_sequence_number: 0,
            is_cng_or_dtmf: false,
            is_dtx: false,
            buffer_flush: false,
        };
        // First packet: no delay estimate yet.
        assert_eq!(
            decision_logic.packet_arrived(SAMPLE_RATE, true, &info(0)),
            None
        );
        assert_eq!(decision_logic.packet_length_samples(), 160);
        // Not updating stats: nothing happens.
        assert_eq!(
            decision_logic.packet_arrived(SAMPLE_RATE, false, &info(160)),
            None
        );
        tick_timer.increment_by(4);
        // Second packet 20 ms late relative to the first.
        assert_eq!(
            decision_logic.packet_arrived(SAMPLE_RATE, true, &info(160)),
            Some(20)
        );
        // Duplicate is rejected.
        assert_eq!(
            decision_logic.packet_arrived(SAMPLE_RATE, true, &info(160)),
            None
        );
        assert_eq!(decision_logic.target_level_ms(), 80);
        assert_eq!(decision_logic.unlimited_target_level_ms(), 80);
        assert!(decision_logic.set_maximum_delay(60));
        assert_eq!(decision_logic.target_level_ms(), 60);
        assert_eq!(decision_logic.unlimited_target_level_ms(), 80);
    }

    #[test]
    fn buffer_flush_resets_filtered_level() {
        let tick_timer = TickTimer::new();
        let mut decision_logic = create_decision_logic(&tick_timer, 100, 100, 0);
        let mut reset_decoder = false;
        decision_logic.get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder);
        // Filtered: (256 - 253) * 800 in Q8, rounded.
        assert_eq!(decision_logic.get_filtered_buffer_level(), 9);
        let info = PacketArrivedInfo {
            buffer_flush: true,
            ..PacketArrivedInfo::default()
        };
        decision_logic.packet_arrived(SAMPLE_RATE, false, &info);
        decision_logic.get_decision(&create_neteq_status(Mode::Normal, 100), &mut reset_decoder);
        assert_eq!(decision_logic.get_filtered_buffer_level(), 800);
    }
}
