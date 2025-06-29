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
use std::time::{Duration, Instant};

/// Network statistics similar to libWebRTC's NetEqNetworkStatistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkStatistics {
    /// Current jitter buffer size in milliseconds
    pub current_buffer_size_ms: u16,
    /// Target buffer size in milliseconds
    pub preferred_buffer_size_ms: u16,
    /// Number of jitter peaks found
    pub jitter_peaks_found: u16,
    /// Fraction of audio data inserted through expansion (Q14 format)
    pub expand_rate: u16,
    /// Fraction of speech audio inserted through expansion (Q14 format)
    pub speech_expand_rate: u16,
    /// Fraction of data inserted through pre-emptive expansion (Q14 format)
    pub preemptive_rate: u16,
    /// Fraction of data removed through acceleration (Q14 format)
    pub accelerate_rate: u16,
    /// Statistics for packet waiting times
    pub mean_waiting_time_ms: i32,
    pub median_waiting_time_ms: i32,
    pub min_waiting_time_ms: i32,
    pub max_waiting_time_ms: i32,
}

/// Lifetime statistics that persist over the NetEQ lifetime
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifetimeStatistics {
    /// Total samples received
    pub total_samples_received: u64,
    /// Total concealed samples
    pub concealed_samples: u64,
    /// Number of concealment events
    pub concealment_events: u64,
    /// Cumulative jitter buffer delay in milliseconds
    pub jitter_buffer_delay_ms: u64,
    /// Number of samples emitted from jitter buffer
    pub jitter_buffer_emitted_count: u64,
    /// Target delay accumulation
    pub jitter_buffer_target_delay_ms: u64,
    /// Samples inserted for deceleration
    pub inserted_samples_for_deceleration: u64,
    /// Samples removed for acceleration
    pub removed_samples_for_acceleration: u64,
    /// Silent concealed samples
    pub silent_concealed_samples: u64,
    /// Relative packet arrival delay accumulation
    pub relative_packet_arrival_delay_ms: u64,
    /// Number of packets received in jitter buffer
    pub jitter_buffer_packets_received: u64,
    /// Buffer flush events
    pub buffer_flushes: u64,
    /// Late packets discarded
    pub late_packets_discarded: u64,
}

/// Operations and internal state metrics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OperationStatistics {
    /// Cumulative preemptive samples
    pub preemptive_samples: u64,
    /// Cumulative accelerate samples
    pub accelerate_samples: u64,
    /// Buffer flush count
    pub packet_buffer_flushes: u64,
    /// Discarded primary packets
    pub discarded_primary_packets: u64,
    /// Last packet waiting time
    pub last_waiting_time_ms: u64,
    /// Current buffer size
    pub current_buffer_size_ms: u64,
    /// Current frame size
    pub current_frame_size_ms: u64,
    /// Next packet available flag
    pub next_packet_available: bool,
}

/// Statistics calculator and tracker
#[derive(Debug)]
pub struct StatisticsCalculator {
    network_stats: NetworkStatistics,
    lifetime_stats: LifetimeStatistics,
    operation_stats: OperationStatistics,
    start_time: Instant,
    waiting_times: Vec<i32>,
    last_update: Instant,
}

impl Default for StatisticsCalculator {
    fn default() -> Self {
        Self::new()
    }
}

impl StatisticsCalculator {
    /// Create a new statistics calculator
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            network_stats: NetworkStatistics::default(),
            lifetime_stats: LifetimeStatistics::default(),
            operation_stats: OperationStatistics::default(),
            start_time: now,
            waiting_times: Vec::new(),
            last_update: now,
        }
    }

    /// Update buffer size statistics
    pub fn update_buffer_size(&mut self, current_ms: u16, preferred_ms: u16) {
        self.network_stats.current_buffer_size_ms = current_ms;
        self.network_stats.preferred_buffer_size_ms = preferred_ms;
        self.operation_stats.current_buffer_size_ms = current_ms as u64;
    }

    /// Record a packet arrival and calculate waiting time
    pub fn packet_arrived(&mut self, arrival_delay_ms: i32) {
        self.lifetime_stats.jitter_buffer_packets_received += 1;
        self.waiting_times.push(arrival_delay_ms);

        // Keep only recent waiting times (last 100 packets)
        if self.waiting_times.len() > 100 {
            self.waiting_times.remove(0);
        }

        self.update_waiting_time_stats();
    }

    /// Record jitter buffer delay
    pub fn jitter_buffer_delay(&mut self, delay_ms: u64, emitted_samples: u64) {
        self.lifetime_stats.jitter_buffer_delay_ms += delay_ms;
        self.lifetime_stats.jitter_buffer_emitted_count += emitted_samples;
    }

    /// Record concealment event
    pub fn concealment_event(&mut self, concealed_samples: u64, is_silent: bool) {
        self.lifetime_stats.concealment_events += 1;
        self.lifetime_stats.concealed_samples += concealed_samples;

        if is_silent {
            self.lifetime_stats.silent_concealed_samples += concealed_samples;
        }
    }

    /// Record time-stretching operation
    pub fn time_stretch_operation(&mut self, operation: TimeStretchOperation, samples: u64) {
        match operation {
            TimeStretchOperation::Accelerate => {
                self.lifetime_stats.removed_samples_for_acceleration += samples;
                self.operation_stats.accelerate_samples += samples;
                // Update accelerate rate (simplified calculation)
                self.network_stats.accelerate_rate =
                    ((samples as f32 / 1000.0) * (1 << 14) as f32) as u16;
            }
            TimeStretchOperation::PreemptiveExpand => {
                self.lifetime_stats.inserted_samples_for_deceleration += samples;
                self.operation_stats.preemptive_samples += samples;
                // Update preemptive rate (simplified calculation)
                self.network_stats.preemptive_rate =
                    ((samples as f32 / 1000.0) * (1 << 14) as f32) as u16;
            }
            TimeStretchOperation::Expand => {
                // Update expand rate (simplified calculation)
                self.network_stats.expand_rate =
                    ((samples as f32 / 1000.0) * (1 << 14) as f32) as u16;
            }
        }
    }

    /// Record buffer flush event
    pub fn buffer_flush(&mut self) {
        self.lifetime_stats.buffer_flushes += 1;
        self.operation_stats.packet_buffer_flushes += 1;
    }

    /// Record discarded packet
    pub fn packet_discarded(&mut self, is_late: bool) {
        if is_late {
            self.lifetime_stats.late_packets_discarded += 1;
        }
        self.operation_stats.discarded_primary_packets += 1;
    }

    /// Get current network statistics
    pub fn network_statistics(&self) -> &NetworkStatistics {
        &self.network_stats
    }

    /// Get lifetime statistics
    pub fn lifetime_statistics(&self) -> &LifetimeStatistics {
        &self.lifetime_stats
    }

    /// Get operation statistics
    pub fn operation_statistics(&self) -> &OperationStatistics {
        &self.operation_stats
    }

    /// Get uptime duration
    pub fn uptime(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Reset all statistics
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    fn update_waiting_time_stats(&mut self) {
        if self.waiting_times.is_empty() {
            return;
        }

        let mut sorted_times = self.waiting_times.clone();
        sorted_times.sort_unstable();

        self.network_stats.min_waiting_time_ms = *sorted_times.first().unwrap();
        self.network_stats.max_waiting_time_ms = *sorted_times.last().unwrap();

        let sum: i32 = sorted_times.iter().sum();
        self.network_stats.mean_waiting_time_ms = sum / sorted_times.len() as i32;

        let median_idx = sorted_times.len() / 2;
        self.network_stats.median_waiting_time_ms = sorted_times[median_idx];

        self.operation_stats.last_waiting_time_ms = *self.waiting_times.last().unwrap() as u64;
    }
}

/// Types of time-stretching operations
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeStretchOperation {
    Accelerate,
    PreemptiveExpand,
    Expand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_statistics_calculator() {
        let mut calc = StatisticsCalculator::new();

        // Test buffer size update
        calc.update_buffer_size(100, 120);
        assert_eq!(calc.network_statistics().current_buffer_size_ms, 100);
        assert_eq!(calc.network_statistics().preferred_buffer_size_ms, 120);

        // Test packet arrival
        calc.packet_arrived(50);
        assert_eq!(calc.lifetime_statistics().jitter_buffer_packets_received, 1);
        assert_eq!(calc.network_statistics().mean_waiting_time_ms, 50);

        // Test concealment
        calc.concealment_event(160, false);
        assert_eq!(calc.lifetime_statistics().concealment_events, 1);
        assert_eq!(calc.lifetime_statistics().concealed_samples, 160);

        // Test time stretch
        calc.time_stretch_operation(TimeStretchOperation::Accelerate, 80);
        assert_eq!(
            calc.lifetime_statistics().removed_samples_for_acceleration,
            80
        );

        // Test buffer flush
        calc.buffer_flush();
        assert_eq!(calc.lifetime_statistics().buffer_flushes, 1);
    }

    #[test]
    fn test_waiting_time_statistics() {
        let mut calc = StatisticsCalculator::new();

        // Add several waiting times
        calc.packet_arrived(10);
        calc.packet_arrived(20);
        calc.packet_arrived(30);
        calc.packet_arrived(15);
        calc.packet_arrived(25);

        let stats = calc.network_statistics();
        assert_eq!(stats.min_waiting_time_ms, 10);
        assert_eq!(stats.max_waiting_time_ms, 30);
        assert_eq!(stats.mean_waiting_time_ms, 20); // (10+20+30+15+25)/5 = 20
        assert_eq!(stats.median_waiting_time_ms, 20); // Middle value when sorted
    }
}
