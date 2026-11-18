// Port of WebRTC modules/audio_coding/neteq/statistics_calculator.{h,cc} and
// statistics_calculator_unittest.cc at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// Out of scope (PARITY_PLAN.md section 9): the UMA loggers
// (`PeriodicUmaCount`, `PeriodicUmaAverage`, `ExpandUmaLogger`) and the
// `RTC_HISTOGRAM_*` calls. They never feed back into any returned statistic,
// so the constructor no longer needs the `TickTimer` they used.
//
// The C++ methods marked `virtual` (for gmock) are plain methods here. Tests
// that used `MockStatisticsCalculator` assert on `get_lifetime_statistics()`
// instead.

//! Network and lifetime statistics bookkeeping.

use std::collections::VecDeque;

use crate::api::stats::{NetEqLifetimeStatistics, NetEqNetworkStatistics, NetEqOperationsAndState};

/// Loss-concealment events at least this long count as interruptions.
const K_INTERRUPTION_LEN_MS: u64 = 150;

/// Seconds before the report interval counter auto-resets.
const K_MAX_REPORT_PERIOD: i32 = 60;

/// Number of waiting times kept for the waiting-time statistics.
const K_LEN_WAITING_TIMES: usize = 100;

/// C++ `AddIntToSizeTWithLowerCap`: `b + a`, capped at zero instead of
/// wrapping negative.
fn add_int_to_size_t_with_lower_cap(a: i32, b: usize) -> usize {
    let ret = b.wrapping_add(a as isize as usize);
    // If a + b is negative, resulting in a negative wrap, cap it to zero instead.
    if a < 0 && ret > b {
        0
    } else {
        ret
    }
}

/// C++ `StatisticsCalculator`: collects NetEq's network, lifetime and
/// operations statistics.
#[derive(Debug, Clone, Default)]
pub struct StatisticsCalculator {
    lifetime_stats: NetEqLifetimeStatistics,
    operations_and_state: NetEqOperationsAndState,
    concealed_samples_correction: usize,
    silent_concealed_samples_correction: usize,
    preemptive_samples: usize,
    accelerate_samples: usize,
    expanded_speech_samples: usize,
    expanded_noise_samples: usize,
    concealed_samples_at_event_end: usize,
    timestamps_since_last_report: u32,
    waiting_times: VecDeque<i32>,
    secondary_decoded_samples: u32,
    discarded_secondary_packets: usize,
    decoded_output_played: bool,
}

impl StatisticsCalculator {
    /// A calculator with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Resets most of the counters.
    pub fn reset(&mut self) {
        self.preemptive_samples = 0;
        self.accelerate_samples = 0;
        self.expanded_speech_samples = 0;
        self.expanded_noise_samples = 0;
        self.secondary_decoded_samples = 0;
        self.discarded_secondary_packets = 0;
        self.waiting_times.clear();
    }

    /// Resets the counters that are not handled by `reset()`.
    pub fn reset_mcu(&mut self) {
        self.timestamps_since_last_report = 0;
    }

    /// Reports that `num_samples` samples were produced through expansion, and
    /// that the expansion produced other than just noise samples.
    pub fn expanded_voice_samples(&mut self, num_samples: usize, is_new_concealment_event: bool) {
        if !self.decoded_output_played {
            return;
        }
        self.expanded_speech_samples += num_samples;
        self.concealed_samples_correction(num_samples as i32, true);
        self.lifetime_stats.concealment_events += u64::from(is_new_concealment_event);
    }

    /// Reports that `num_samples` samples were produced through expansion, and
    /// that the expansion produced only noise samples.
    pub fn expanded_noise_samples(&mut self, num_samples: usize, is_new_concealment_event: bool) {
        if !self.decoded_output_played {
            return;
        }
        self.expanded_noise_samples += num_samples;
        self.concealed_samples_correction(num_samples as i32, false);
        self.lifetime_stats.concealment_events += u64::from(is_new_concealment_event);
    }

    /// Corrects the number of samples produced through non-noise expansion by
    /// adding `num_samples` (negative or positive). The result is capped at
    /// zero.
    pub fn expanded_voice_samples_correction(&mut self, num_samples: i32) {
        if !self.decoded_output_played {
            return;
        }
        self.expanded_speech_samples =
            add_int_to_size_t_with_lower_cap(num_samples, self.expanded_speech_samples);
        self.concealed_samples_correction(num_samples, true);
    }

    /// Same as `expanded_voice_samples_correction` but for noise samples.
    pub fn expanded_noise_samples_correction(&mut self, num_samples: i32) {
        if !self.decoded_output_played {
            return;
        }
        self.expanded_noise_samples =
            add_int_to_size_t_with_lower_cap(num_samples, self.expanded_noise_samples);
        self.concealed_samples_correction(num_samples, false);
    }

    /// Marks that decoded output has been played. Most counters stay frozen
    /// until this is called.
    pub fn decoded_output_played(&mut self) {
        self.decoded_output_played = true;
    }

    /// Marks the end of an expand event; may log an interruption.
    pub fn end_expand_event(&mut self, fs_hz: i32) {
        if !self.decoded_output_played {
            return;
        }
        debug_assert!(
            self.lifetime_stats.concealed_samples >= self.concealed_samples_at_event_end as u64
        );
        // C++ evaluates this in uint64_t, then narrows to int.
        let event_duration_ms = (1000u64.wrapping_mul(
            self.lifetime_stats
                .concealed_samples
                .wrapping_sub(self.concealed_samples_at_event_end as u64),
        ) / fs_hz as u64) as i32;
        if event_duration_ms as i64 >= K_INTERRUPTION_LEN_MS as i64 && self.decoded_output_played {
            self.lifetime_stats.interruption_count += 1;
            self.lifetime_stats.total_interruption_duration_ms += event_duration_ms;
        }
        self.concealed_samples_at_event_end = self.lifetime_stats.concealed_samples as usize;
    }

    /// Reports that `num_samples` samples were produced through preemptive
    /// expansion.
    pub fn preemptive_expanded_samples(&mut self, num_samples: usize) {
        if !self.decoded_output_played {
            return;
        }
        self.preemptive_samples += num_samples;
        self.operations_and_state.preemptive_samples += num_samples as u64;
        self.lifetime_stats.inserted_samples_for_deceleration += num_samples as u64;
    }

    /// Reports that `num_samples` samples were removed through accelerate.
    pub fn accelerated_samples(&mut self, num_samples: usize) {
        if !self.decoded_output_played {
            return;
        }
        self.accelerate_samples += num_samples;
        self.operations_and_state.accelerate_samples += num_samples as u64;
        self.lifetime_stats.removed_samples_for_acceleration += num_samples as u64;
    }

    /// Reports that `num_samples` comfort noise samples were generated.
    pub fn generated_noise_samples(&mut self, num_samples: usize) {
        if !self.decoded_output_played {
            return;
        }
        self.lifetime_stats.generated_noise_samples += num_samples as u64;
    }

    /// Reports that `num_packets` packets were discarded.
    pub fn packets_discarded(&mut self, num_packets: usize) {
        self.lifetime_stats.packets_discarded += num_packets as u64;
    }

    /// Reports that `num_packets` secondary (FEC) packets were discarded.
    pub fn secondary_packets_discarded(&mut self, num_packets: usize) {
        self.discarded_secondary_packets += num_packets;
        self.lifetime_stats.fec_packets_discarded += num_packets as u64;
    }

    /// Reports that `num_packets` secondary (FEC) packets were received.
    pub fn secondary_packets_received(&mut self, num_packets: usize) {
        self.lifetime_stats.fec_packets_received += num_packets as u64;
    }

    /// Advances the report interval counter by `num_samples` at `fs_hz`. This
    /// is how the calculator learns that time is passing.
    pub fn increase_counter(&mut self, num_samples: usize, fs_hz: i32) {
        if !self.decoded_output_played {
            return;
        }
        // C++ computes `CheckedDivExact(1000 * num_samples, fs_hz)` here for
        // the UMA loggers only; keep the exactness check.
        debug_assert_eq!(0, (1000 * num_samples as i64) % fs_hz as i64);
        self.timestamps_since_last_report = self
            .timestamps_since_last_report
            .wrapping_add(num_samples as u32);
        if self.timestamps_since_last_report > fs_hz.wrapping_mul(K_MAX_REPORT_PERIOD) as u32 {
            self.timestamps_since_last_report = 0;
        }
        self.lifetime_stats.total_samples_received += num_samples as u64;
    }

    /// Updates the jitter buffer delay counters.
    pub fn jitter_buffer_delay(
        &mut self,
        num_samples: usize,
        waiting_time_ms: u64,
        target_delay_ms: u64,
        unlimited_target_delay_ms: u64,
        processing_delay_us: u64,
    ) {
        let num_samples = num_samples as u64;
        let lifetime = &mut self.lifetime_stats;
        lifetime.jitter_buffer_delay_ms = lifetime
            .jitter_buffer_delay_ms
            .wrapping_add(waiting_time_ms.wrapping_mul(num_samples));
        lifetime.jitter_buffer_target_delay_ms = lifetime
            .jitter_buffer_target_delay_ms
            .wrapping_add(target_delay_ms.wrapping_mul(num_samples));
        lifetime.jitter_buffer_minimum_delay_ms = lifetime
            .jitter_buffer_minimum_delay_ms
            .wrapping_add(unlimited_target_delay_ms.wrapping_mul(num_samples));
        lifetime.jitter_buffer_emitted_count = lifetime
            .jitter_buffer_emitted_count
            .wrapping_add(num_samples);
        lifetime.total_processing_delay_us = lifetime
            .total_processing_delay_us
            .wrapping_add(num_samples.wrapping_mul(processing_delay_us));
    }

    /// Stores a packet waiting time for the waiting-time statistics.
    pub fn store_waiting_time(&mut self, waiting_time_ms: i32) {
        debug_assert!(self.waiting_times.len() <= K_LEN_WAITING_TIMES);
        if self.waiting_times.len() == K_LEN_WAITING_TIMES {
            // Erase first value.
            self.waiting_times.pop_front();
        }
        self.waiting_times.push_back(waiting_time_ms);
        self.operations_and_state.last_waiting_time_ms = waiting_time_ms as u64;
    }

    /// Reports that `num_samples` samples were decoded from secondary packets.
    pub fn secondary_decoded_samples(&mut self, num_samples: i32) {
        self.secondary_decoded_samples = self
            .secondary_decoded_samples
            .wrapping_add(num_samples as u32);
    }

    /// Reports that the packet buffer was flushed.
    pub fn flushed_packet_buffer(&mut self) {
        self.operations_and_state.packet_buffer_flushes += 1;
    }

    /// Reports that the jitter buffer received a packet.
    pub fn received_packet(&mut self) {
        self.lifetime_stats.jitter_buffer_packets_received += 1;
    }

    /// Reports that a received packet was delayed by `delay_ms` milliseconds.
    pub fn relative_packet_arrival_delay(&mut self, delay_ms: usize) {
        self.lifetime_stats.relative_packet_arrival_delay_ms += delay_ms as u64;
    }

    /// Logs a delayed packet outage event of `num_samples` expanded at
    /// `fs_hz`: an expand period caused by a late packet, not a lost one.
    pub fn log_delayed_packet_outage_event(&mut self, num_samples: i32, fs_hz: i32) {
        let _ = fs_hz; // Only used for the UMA histogram in C++.
        self.lifetime_stats.delayed_packet_outage_samples = self
            .lifetime_stats
            .delayed_packet_outage_samples
            .wrapping_add(num_samples as u64);
        self.lifetime_stats.delayed_packet_outage_events += 1;
    }

    /// Fills the rate and waiting-time fields of `stats`, then resets the
    /// rate counters. Does not touch `current_buffer_size_ms`,
    /// `preferred_buffer_size_ms` or `jitter_peaks_found`; `NetEqImpl` fills
    /// those. `samples_per_packet` converts discarded FEC packets to samples.
    pub fn get_network_statistics(
        &mut self,
        samples_per_packet: usize,
        stats: &mut NetEqNetworkStatistics,
    ) {
        stats.accelerate_rate =
            Self::calculate_q14_ratio(self.accelerate_samples, self.timestamps_since_last_report);

        stats.preemptive_rate =
            Self::calculate_q14_ratio(self.preemptive_samples, self.timestamps_since_last_report);

        stats.expand_rate = Self::calculate_q14_ratio(
            self.expanded_speech_samples + self.expanded_noise_samples,
            self.timestamps_since_last_report,
        );

        stats.speech_expand_rate = Self::calculate_q14_ratio(
            self.expanded_speech_samples,
            self.timestamps_since_last_report,
        );

        stats.secondary_decoded_rate = Self::calculate_q14_ratio(
            self.secondary_decoded_samples as usize,
            self.timestamps_since_last_report,
        );

        let discarded_secondary_samples = self
            .discarded_secondary_packets
            .wrapping_mul(samples_per_packet);
        stats.secondary_discarded_rate = Self::calculate_q14_ratio(
            discarded_secondary_samples,
            discarded_secondary_samples.wrapping_add(self.secondary_decoded_samples as usize)
                as u32,
        );

        if self.waiting_times.is_empty() {
            stats.mean_waiting_time_ms = -1;
            stats.median_waiting_time_ms = -1;
            stats.min_waiting_time_ms = -1;
            stats.max_waiting_time_ms = -1;
        } else {
            self.waiting_times.make_contiguous().sort_unstable();
            let len = self.waiting_times.len();
            // Find mid-point elements. If the size is odd, `middle_left` and
            // `middle_right` are the same middle element; if even, they are
            // the two neighbors at the middle.
            let middle_left = self.waiting_times[(len - 1) / 2];
            let middle_right = self.waiting_times[len / 2];
            // Average of the two. (Works also for odd sizes.)
            stats.median_waiting_time_ms = (middle_left + middle_right) / 2;
            stats.min_waiting_time_ms = self.waiting_times[0];
            stats.max_waiting_time_ms = self.waiting_times[len - 1];
            let mut sum = 0f64;
            for &time in &self.waiting_times {
                sum += time as f64;
            }
            stats.mean_waiting_time_ms = (sum / len as f64) as i32;
        }

        // Reset counters.
        self.reset_mcu();
        self.reset();
    }

    /// A copy of the lifetime statistics. These are never reset.
    pub fn get_lifetime_statistics(&self) -> NetEqLifetimeStatistics {
        self.lifetime_stats
    }

    /// A copy of the operations-and-state statistics.
    pub fn get_operations_and_state(&self) -> NetEqOperationsAndState {
        self.operations_and_state
    }

    /// Corrects the concealed samples counter in `lifetime_stats`. Positive
    /// corrections are added directly; negative ones are cached and subtracted
    /// from future additions, so the stat never decreases.
    fn concealed_samples_correction(&mut self, num_samples: i32, is_voice: bool) {
        if !self.decoded_output_played {
            return;
        }
        if num_samples < 0 {
            // Store negative correction to subtract from future positive additions.
            let magnitude = num_samples.unsigned_abs() as usize;
            self.concealed_samples_correction += magnitude;
            if !is_voice {
                self.silent_concealed_samples_correction += magnitude;
            }
            return;
        }

        let num_samples = num_samples as usize;
        let canceled_out = num_samples.min(self.concealed_samples_correction);
        self.concealed_samples_correction -= canceled_out;
        self.lifetime_stats.concealed_samples += (num_samples - canceled_out) as u64;

        if !is_voice {
            let silent_canceled_out = num_samples.min(self.silent_concealed_samples_correction);
            self.silent_concealed_samples_correction -= silent_canceled_out;
            self.lifetime_stats.silent_concealed_samples +=
                (num_samples - silent_canceled_out) as u64;
        }
    }

    /// `numerator / denominator` in Q14, capped at 1.0 (16384).
    fn calculate_q14_ratio(numerator: usize, denominator: u32) -> u16 {
        if numerator == 0 {
            0
        } else if numerator < denominator as usize {
            // Ratio must be smaller than 1 in Q14.
            debug_assert!((numerator << 14) / (denominator as usize) < (1 << 14));
            ((numerator << 14) / denominator as usize) as u16
        } else {
            // Will not produce a ratio larger than 1, since this is probably an error.
            1 << 14
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetime_statistics_total_samples_received() {
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        for _ in 0..10 {
            stats.increase_counter(480, 48000); // 10 ms at 48 kHz.
        }
        assert_eq!(
            10 * 480,
            stats.get_lifetime_statistics().total_samples_received
        );
    }

    #[test]
    fn lifetime_statistics_samples_concealed() {
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        stats.expanded_voice_samples(100, false);
        stats.expanded_noise_samples(17, false);
        assert_eq!(100 + 17, stats.get_lifetime_statistics().concealed_samples);
    }

    // A negative correction of concealed_samples does not decrease the stat
    // (consumers would not expect it to). The correction applies to future
    // increments instead.
    #[test]
    fn lifetime_statistics_samples_concealed_correction() {
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        stats.expanded_voice_samples(100, false);
        assert_eq!(100, stats.get_lifetime_statistics().concealed_samples);
        stats.expanded_voice_samples_correction(-10);
        // Do not subtract directly, but keep the correction for later.
        assert_eq!(100, stats.get_lifetime_statistics().concealed_samples);
        stats.expanded_voice_samples_correction(20);
        // The total correction is 20 - 10.
        assert_eq!(110, stats.get_lifetime_statistics().concealed_samples);

        // Also test correction done to the next expanded_voice_samples call.
        stats.expanded_voice_samples_correction(-17);
        assert_eq!(110, stats.get_lifetime_statistics().concealed_samples);
        stats.expanded_voice_samples(100, false);
        assert_eq!(
            110 + 100 - 17,
            stats.get_lifetime_statistics().concealed_samples
        );
    }

    // Neither accelerate nor preemptive expand modifies concealed_samples.
    // Only PLC operations (expand and merge) do.
    #[test]
    fn lifetime_statistics_no_update_on_time_stretch() {
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        stats.expanded_voice_samples(100, false);
        stats.accelerated_samples(4711);
        stats.preemptive_expanded_samples(17);
        stats.expanded_voice_samples(100, false);
        assert_eq!(200, stats.get_lifetime_statistics().concealed_samples);
    }

    #[test]
    fn statistics_calculator_expanded_samples_correction() {
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        let mut stats_output = NetEqNetworkStatistics::default();
        const K_SAMPLE_RATE_HZ: i32 = 48000;
        const K_10MS_SAMPLES: usize = (K_SAMPLE_RATE_HZ / 100) as usize;
        const K_PACKET_SIZE_MS: usize = 20;
        const K_SAMPLES_PER_PACKET: usize = K_PACKET_SIZE_MS * K_SAMPLE_RATE_HZ as usize / 1000;

        // Advance time by 10 ms.
        stats.increase_counter(K_10MS_SAMPLES, K_SAMPLE_RATE_HZ);

        stats.get_network_statistics(K_SAMPLES_PER_PACKET, &mut stats_output);

        assert_eq!(0, stats_output.expand_rate);
        assert_eq!(0, stats_output.speech_expand_rate);

        // Correct with a negative value.
        stats.expanded_voice_samples_correction(-100);
        stats.expanded_noise_samples_correction(-100);
        stats.increase_counter(K_10MS_SAMPLES, K_SAMPLE_RATE_HZ);
        stats.get_network_statistics(K_SAMPLES_PER_PACKET, &mut stats_output);
        // Expect no change, since negative values are disallowed.
        assert_eq!(0, stats_output.expand_rate);
        assert_eq!(0, stats_output.speech_expand_rate);

        // Correct with a positive value.
        stats.expanded_voice_samples_correction(50);
        stats.expanded_noise_samples_correction(200);
        stats.increase_counter(K_10MS_SAMPLES, K_SAMPLE_RATE_HZ);
        stats.get_network_statistics(K_SAMPLES_PER_PACKET, &mut stats_output);
        // Expected rates in Q14. Expand rate is noise + voice, while speech
        // expand rate is only voice.
        assert_eq!(
            (((50 + 200) << 14) / K_10MS_SAMPLES) as u16,
            stats_output.expand_rate
        );
        assert_eq!(
            ((50 << 14) / K_10MS_SAMPLES) as u16,
            stats_output.speech_expand_rate
        );
    }

    #[test]
    fn statistics_calculator_relative_packet_arrival_delay() {
        let mut stats = StatisticsCalculator::new();

        stats.relative_packet_arrival_delay(50);
        let mut stats_output = stats.get_lifetime_statistics();
        assert_eq!(50, stats_output.relative_packet_arrival_delay_ms);

        stats.relative_packet_arrival_delay(20);
        stats_output = stats.get_lifetime_statistics();
        assert_eq!(70, stats_output.relative_packet_arrival_delay_ms);
    }

    #[test]
    fn statistics_calculator_received_packet() {
        let mut stats = StatisticsCalculator::new();

        stats.received_packet();
        let mut stats_output = stats.get_lifetime_statistics();
        assert_eq!(1, stats_output.jitter_buffer_packets_received);

        stats.received_packet();
        stats_output = stats.get_lifetime_statistics();
        assert_eq!(2, stats_output.jitter_buffer_packets_received);
    }

    #[test]
    fn statistics_calculator_interruption_counter() {
        const FS_KHZ: usize = 48;
        const FS_HZ: i32 = (FS_KHZ * 1000) as i32;
        let mut stats = StatisticsCalculator::new();
        stats.decoded_output_played();
        stats.end_expand_event(FS_HZ);
        let mut lts = stats.get_lifetime_statistics();
        assert_eq!(0, lts.interruption_count);
        assert_eq!(0, lts.total_interruption_duration_ms);

        // Add an event that is shorter than 150 ms. Should not be logged.
        stats.expanded_voice_samples(10 * FS_KHZ, false); // 10 ms.
        stats.expanded_noise_samples(139 * FS_KHZ, false); // 139 ms.
        stats.end_expand_event(FS_HZ);
        lts = stats.get_lifetime_statistics();
        assert_eq!(0, lts.interruption_count);

        // Add an event that is longer than 150 ms. Should be logged.
        stats.expanded_voice_samples(140 * FS_KHZ, false); // 140 ms.
        stats.expanded_noise_samples(11 * FS_KHZ, false); // 11 ms.
        stats.end_expand_event(FS_HZ);
        lts = stats.get_lifetime_statistics();
        assert_eq!(1, lts.interruption_count);
        assert_eq!(151, lts.total_interruption_duration_ms);

        // Add one more long event.
        stats.expanded_voice_samples(100 * FS_KHZ, false); // 100 ms.
        stats.expanded_noise_samples(5000 * FS_KHZ, false); // 5000 ms.
        stats.end_expand_event(FS_HZ);
        lts = stats.get_lifetime_statistics();
        assert_eq!(2, lts.interruption_count);
        assert_eq!(5100 + 151, lts.total_interruption_duration_ms);
    }

    #[test]
    fn statistics_calculator_interruption_counter_do_not_log_before_decoding() {
        const FS_KHZ: usize = 48;
        const FS_HZ: i32 = (FS_KHZ * 1000) as i32;
        let mut stats = StatisticsCalculator::new();

        // Add an event that is longer than 150 ms. Should normally be logged,
        // but decoded_output_played() has not been called yet.
        stats.expanded_voice_samples(151 * FS_KHZ, false); // 151 ms.
        stats.end_expand_event(FS_HZ);
        let mut lts = stats.get_lifetime_statistics();
        assert_eq!(0, lts.interruption_count);

        // Call decoded_output_played(). Logging should happen after this.
        stats.decoded_output_played();

        // Add one more long event.
        stats.expanded_voice_samples(151 * FS_KHZ, false); // 151 ms.
        stats.end_expand_event(FS_HZ);
        lts = stats.get_lifetime_statistics();
        assert_eq!(1, lts.interruption_count);
    }

    #[test]
    fn statistics_calculator_discarded_packets() {
        let mut statistics_calculator = StatisticsCalculator::new();
        assert_eq!(
            0,
            statistics_calculator
                .get_lifetime_statistics()
                .packets_discarded
        );

        statistics_calculator.packets_discarded(1);
        assert_eq!(
            1,
            statistics_calculator
                .get_lifetime_statistics()
                .packets_discarded
        );

        statistics_calculator.packets_discarded(10);
        assert_eq!(
            11,
            statistics_calculator
                .get_lifetime_statistics()
                .packets_discarded
        );

        // Calling `secondary_packets_discarded` does not modify `packets_discarded`.
        statistics_calculator.secondary_packets_discarded(1);
        assert_eq!(
            11,
            statistics_calculator
                .get_lifetime_statistics()
                .packets_discarded
        );

        // Calling `flushed_packet_buffer` does not modify `packets_discarded`.
        statistics_calculator.flushed_packet_buffer();
        assert_eq!(
            11,
            statistics_calculator
                .get_lifetime_statistics()
                .packets_discarded
        );
    }

    #[test]
    fn statistics_calculator_jitter_buffer_delay() {
        let mut stats = StatisticsCalculator::new();
        let mut lts = stats.get_lifetime_statistics();
        assert_eq!(0, lts.total_processing_delay_us);
        stats.jitter_buffer_delay(
            /*num_samples=*/ 480,
            /*waiting_time_ms=*/ 90,
            /*target_delay_ms=*/ 80,
            /*unlimited_target_delay_ms=*/ 70,
            /*processing_delay_us=*/ 100 * 1000,
        );
        lts = stats.get_lifetime_statistics();
        assert_eq!(90, lts.jitter_buffer_delay_ms / 480);
        assert_eq!(80, lts.jitter_buffer_target_delay_ms / 480);
        assert_eq!(70, lts.jitter_buffer_minimum_delay_ms / 480);
        assert_eq!(100 * 1000, lts.total_processing_delay_us / 480);
        assert_eq!(480, lts.jitter_buffer_emitted_count);
        stats.jitter_buffer_delay(
            /*num_samples=*/ 480,
            /*waiting_time_ms=*/ 90,
            /*target_delay_ms=*/ 80,
            /*unlimited_target_delay_ms=*/ 70,
            /*processing_delay_us=*/ 100 * 1000,
        );
        lts = stats.get_lifetime_statistics();
        assert_eq!(90, lts.jitter_buffer_delay_ms / 960);
        assert_eq!(80, lts.jitter_buffer_target_delay_ms / 960);
        assert_eq!(70, lts.jitter_buffer_minimum_delay_ms / 960);
        assert_eq!(100 * 1000, lts.total_processing_delay_us / 960);
        assert_eq!(960, lts.jitter_buffer_emitted_count);
    }

    #[test]
    fn statistics_calculator_count_stats_after_first_decoded_packet() {
        let mut stats = StatisticsCalculator::new();
        stats.increase_counter(/*num_samples=*/ 480, /*fs_hz=*/ 48000);
        stats.expanded_voice_samples(
            /*num_samples=*/ 480, /*is_new_concealment_event=*/ true,
        );
        let mut lts = stats.get_lifetime_statistics();
        assert_eq!(0, lts.total_samples_received);
        assert_eq!(0, lts.concealed_samples);
        stats.decoded_output_played();
        stats.increase_counter(/*num_samples=*/ 480, /*fs_hz=*/ 48000);
        lts = stats.get_lifetime_statistics();
        assert_eq!(480, lts.total_samples_received);
    }

    // Not in the C++ suite: pins the waiting-time and Q14 helpers that the
    // C++ tests only reach through NetEqImpl.

    #[test]
    fn waiting_time_statistics() {
        let mut stats = StatisticsCalculator::new();
        let mut out = NetEqNetworkStatistics::default();
        stats.get_network_statistics(960, &mut out);
        assert_eq!(-1, out.mean_waiting_time_ms);
        assert_eq!(-1, out.median_waiting_time_ms);
        assert_eq!(-1, out.min_waiting_time_ms);
        assert_eq!(-1, out.max_waiting_time_ms);

        for waiting_time in [40, 10, 30, 20] {
            stats.store_waiting_time(waiting_time);
        }
        assert_eq!(20, stats.get_operations_and_state().last_waiting_time_ms);
        stats.get_network_statistics(960, &mut out);
        assert_eq!(25, out.mean_waiting_time_ms);
        assert_eq!(25, out.median_waiting_time_ms);
        assert_eq!(10, out.min_waiting_time_ms);
        assert_eq!(40, out.max_waiting_time_ms);

        // Reset after the read.
        stats.get_network_statistics(960, &mut out);
        assert_eq!(-1, out.mean_waiting_time_ms);
    }

    #[test]
    fn waiting_times_keep_last_100() {
        let mut stats = StatisticsCalculator::new();
        for waiting_time in 0..150 {
            stats.store_waiting_time(waiting_time);
        }
        let mut out = NetEqNetworkStatistics::default();
        stats.get_network_statistics(960, &mut out);
        assert_eq!(50, out.min_waiting_time_ms);
        assert_eq!(149, out.max_waiting_time_ms);
    }

    #[test]
    fn q14_ratio_caps_at_one() {
        assert_eq!(0, StatisticsCalculator::calculate_q14_ratio(0, 0));
        assert_eq!(1 << 13, StatisticsCalculator::calculate_q14_ratio(240, 480));
        assert_eq!(1 << 14, StatisticsCalculator::calculate_q14_ratio(480, 480));
        assert_eq!(1 << 14, StatisticsCalculator::calculate_q14_ratio(5, 0));
    }

    #[test]
    fn add_int_to_size_t_caps_at_zero() {
        assert_eq!(0, add_int_to_size_t_with_lower_cap(-10, 5));
        assert_eq!(15, add_int_to_size_t_with_lower_cap(10, 5));
        assert_eq!(0, add_int_to_size_t_with_lower_cap(-5, 5));
    }
}
