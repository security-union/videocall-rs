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

use std::collections::VecDeque;
use web_time::{Duration, Instant};

use crate::{histogram::Histogram, Result};

/// Initial target delay when NetEQ starts operation.
///
/// This value provides a buffer against early underruns while the adaptive
/// delay manager learns the network characteristics:
/// - Too low: Risk of underruns before adaptation kicks in
/// - Too high: Unnecessary initial latency
///
/// Engineering rationale:
/// - 80ms accommodates typical network jitter (±40ms)
/// - Allows 4 packets of 20ms each in buffer initially
/// - WebRTC empirically validated this across diverse networks
/// - Provides safety margin during cold start before statistics stabilize
const K_START_DELAY_MS: u32 = 80;

const K_DELAY_BUCKETS: usize = 100;
const K_BUCKET_SIZE_MS: i32 = 20;

/// Configuration for the delay manager
#[derive(Debug, Clone)]
pub struct DelayConfig {
    /// Quantile for delay estimation (0.0 to 1.0)
    pub quantile: f64,
    /// Forget factor for exponential smoothing
    pub forget_factor: f64,
    /// Starting forget weight for rapid adaptation
    pub start_forget_weight: Option<f64>,
    /// Interval for resampling delay estimation
    pub resample_interval_ms: Option<u32>,
    /// Maximum history length in milliseconds
    pub max_history_ms: u32,
    /// Base minimum delay in milliseconds
    pub base_minimum_delay_ms: u32,
    // Maximum delay in milliseconds
    pub base_maximum_delay_ms: u32,
    // Contant additional delay in milliseconds
    pub additional_delay_ms: u32,
    /// Whether to use reorder optimizer
    pub use_reorder_optimizer: bool,
    /// Forget factor for reorder detection
    pub reorder_forget_factor: f64,
    /// Milliseconds per loss percent for reorder detection
    pub ms_per_loss_percent: u32,
}

impl Default for DelayConfig {
    fn default() -> Self {
        Self {
            quantile: 0.97,
            forget_factor: 0.9993,
            start_forget_weight: Some(2.0),
            resample_interval_ms: Some(500),
            max_history_ms: 2000,
            base_minimum_delay_ms: 0,
            base_maximum_delay_ms: 2000,
            additional_delay_ms: 0,
            use_reorder_optimizer: true,
            reorder_forget_factor: 0.9993,
            ms_per_loss_percent: 20,
        }
    }
}

/// Relative arrival delay tracker
#[derive(Debug)]
pub struct RelativeArrivalDelayTracker {
    config: DelayConfig,
    delay_history: VecDeque<PacketDelay>,
    newest_timestamp: Option<u32>,
    last_timestamp: Option<u32>,
    last_packet_time: Option<Instant>,
}

#[derive(Debug, Clone)]
struct PacketDelay {
    iat_delay_ms: i32,
    _timestamp: u32,
    arrival_time: Instant,
}

impl RelativeArrivalDelayTracker {
    pub fn new(config: DelayConfig) -> Self {
        Self {
            config,
            delay_history: VecDeque::new(),
            newest_timestamp: None,
            last_timestamp: None,
            last_packet_time: None,
        }
    }

    /// Update with a new packet arrival
    pub fn update(&mut self, timestamp: u32, sample_rate: u32, arrival_time: Instant) -> i32 {
        // Expected time since the last packet, from the signed RTP timestamp
        // difference modulo 2^32: a reordered packet gives a negative value
        // and a wrap past u32::MAX a small positive one, as in libwebrtc's
        // wrap-aware comparison (packet.h `Packet::operator<`) and 64-bit
        // unwrapping (PacketArrivalHistory::Insert). i64 so a long pause
        // (more than 89 s at 48 kHz) cannot overflow the multiply.
        let expected_iat_ms = match self.last_timestamp {
            Some(last_timestamp) if sample_rate > 0 => {
                i64::from(timestamp.wrapping_sub(last_timestamp) as i32) * 1000
                    / i64::from(sample_rate)
            }
            _ => 0,
        };

        // Calculate actual time since last packet
        let actual_iat_ms = if let Some(last_time) = self.last_packet_time {
            i64::try_from(arrival_time.duration_since(last_time).as_millis()).unwrap_or(i64::MAX)
        } else {
            0
        };

        // Calculate jitter: positive means packet is late, negative means packet is early
        let iat_delay_ms = actual_iat_ms
            .saturating_sub(expected_iat_ms)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;

        self.last_packet_time = Some(arrival_time);

        // Update delay history
        self.update_delay_history(iat_delay_ms, timestamp, sample_rate);

        // Calculate relative packet arrival delay
        let relative_delay = self.calculate_relative_packet_arrival_delay();

        self.newest_timestamp = Some(timestamp);
        self.last_timestamp = Some(timestamp);

        relative_delay
    }

    /// Reset the tracker
    pub fn reset(&mut self) {
        self.delay_history.clear();
        self.newest_timestamp = None;
        self.last_timestamp = None;
        self.last_packet_time = None;
    }

    fn update_delay_history(&mut self, iat_delay_ms: i32, timestamp: u32, _sample_rate: u32) {
        let packet_delay = PacketDelay {
            iat_delay_ms,
            _timestamp: timestamp,
            arrival_time: crate::clock::now(),
        };

        self.delay_history.push_back(packet_delay);

        // Remove old entries based on max_history_ms
        let max_age = Duration::from_millis(self.config.max_history_ms as u64);
        let now = crate::clock::now();

        self.delay_history
            .retain(|delay| now.duration_since(delay.arrival_time) <= max_age);
    }

    /// Calculates the relative arrival delay of packets in the history.
    ///
    /// Returns the 97th percentile of positive jitter values in the recent history.
    /// Negative jitter (early arrivals) is clamped to 0 since we only care about
    /// buffering for late packets.
    pub fn calculate_relative_packet_arrival_delay(&self) -> i32 {
        if self.delay_history.is_empty() {
            return 0;
        }

        // Collect positive jitter values (late arrivals)
        let mut positive_delays: Vec<i32> = self
            .delay_history
            .iter()
            .map(|d| d.iat_delay_ms.max(0))
            .collect();

        if positive_delays.is_empty() {
            return 0;
        }

        positive_delays.sort_unstable();

        // Return 97th percentile
        let index = ((positive_delays.len() - 1) as f64 * self.config.quantile) as usize;
        positive_delays[index]
    }
}

/// Delay manager for adaptive jitter buffer control
#[derive(Debug)]
pub struct DelayManager {
    config: DelayConfig,
    arrival_delay_tracker: RelativeArrivalDelayTracker,
    target_level_ms: u32,
    minimum_delay_ms: u32,
    effective_minimum_delay_ms: u32,
    maximum_delay_ms: u32,
    effective_maximum_delay_ms: u32,
    resampled_relative_delay: i32,
    last_resample_time: Option<Instant>,
    histogram: Histogram,
}

impl DelayManager {
    /// Create a new delay manager
    pub fn new(config: DelayConfig) -> Self {
        let arrival_delay_tracker = RelativeArrivalDelayTracker::new(config.clone());

        Self {
            target_level_ms: K_START_DELAY_MS.max(config.base_minimum_delay_ms), // Start with 80ms or minimum delay
            minimum_delay_ms: 0,
            effective_minimum_delay_ms: config.base_minimum_delay_ms,
            maximum_delay_ms: 0,
            effective_maximum_delay_ms: config.base_maximum_delay_ms,
            resampled_relative_delay: 0,
            last_resample_time: None,
            arrival_delay_tracker,
            histogram: Histogram::new(
                K_DELAY_BUCKETS,
                config.forget_factor,
                config.start_forget_weight,
            ),
            config,
        }
    }

    /// Update the delay manager with a new packet
    pub fn update(&mut self, timestamp: u32, sample_rate: u32, reset: bool) -> Result<()> {
        if reset {
            self.reset();
        }

        let arrival_time = crate::clock::now();

        // Update arrival delay tracking
        let relative_delay =
            self.arrival_delay_tracker
                .update(timestamp, sample_rate, arrival_time);

        // When jitter is high, relative_delay fluctuates between high and near 0 values.
        // resampling helps by calculating the max over some time period.
        if let Some(resample_interval_ms) = self.config.resample_interval_ms {
            if let Some(last_resample_time) = self.last_resample_time {
                let elapsed_ms = arrival_time.duration_since(last_resample_time).as_millis() as u32;
                if elapsed_ms >= resample_interval_ms {
                    self.register_relative_delay(self.resampled_relative_delay);
                    self.resampled_relative_delay = 0;

                    // Round down to nearest multiples, so the interval doesn't drift.
                    let elapsed_multiples =
                        (elapsed_ms / resample_interval_ms) * resample_interval_ms;
                    self.last_resample_time = last_resample_time
                        .checked_add(Duration::from_millis(elapsed_multiples as u64))
                }
            } else {
                self.last_resample_time = Some(arrival_time);
            }

            self.resampled_relative_delay = self.resampled_relative_delay.max(relative_delay);
        } else {
            self.register_relative_delay(relative_delay);
        }

        self.update_target_delay();

        Ok(())
    }

    /// Reset the delay manager
    pub fn reset(&mut self) {
        self.arrival_delay_tracker.reset();
        self.target_level_ms = K_START_DELAY_MS.max(self.config.base_minimum_delay_ms);
        self.resampled_relative_delay = 0;
        self.last_resample_time = None;
        // Reset to 80ms like WebRTC
    }

    /// Get the current target delay in milliseconds
    pub fn target_delay_ms(&self) -> u32 {
        (self.target_level_ms + self.config.additional_delay_ms)
            .max(self.effective_minimum_delay_ms)
            .min(self.effective_maximum_delay_ms)
    }

    /// Set minimum delay constraint
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> u32 {
        self.minimum_delay_ms = delay_ms;
        self.update_effective_delay_bounds();
        self.effective_minimum_delay_ms
    }

    /// Set maximum delay constraint
    pub fn set_maximum_delay(&mut self, delay_ms: u32) -> u32 {
        self.maximum_delay_ms = delay_ms;
        self.update_effective_delay_bounds();
        self.effective_maximum_delay_ms
    }

    /// Set base minimum delay
    pub fn set_base_minimum_delay(&mut self, delay_ms: u32) {
        self.config.base_minimum_delay_ms = delay_ms;
        self.update_effective_delay_bounds();
    }

    /// Get base minimum delay
    pub fn get_base_minimum_delay(&self) -> u32 {
        self.config.base_minimum_delay_ms
    }

    /// Set base maximum delay
    pub fn set_base_maximum_delay(&mut self, delay_ms: u32) {
        self.config.base_maximum_delay_ms = delay_ms;
        self.update_effective_delay_bounds();
    }

    /// Get base maximum delay
    pub fn get_base_maximum_delay(&self) -> u32 {
        self.config.base_maximum_delay_ms
    }

    fn register_relative_delay(&mut self, relative_delay: i32) {
        let index = (relative_delay / K_BUCKET_SIZE_MS) as usize;
        if index < self.histogram.num_buckets() {
            // Maximum delay to register is 2000 ms.
            self.histogram.add(index);
        }
    }

    fn update_target_delay(&mut self) {
        let bucket_index = self.histogram.quantile(self.config.quantile);

        // Update target level based on filtered delay
        self.target_level_ms = (1 + bucket_index as u32) * K_BUCKET_SIZE_MS as u32;

        // Apply constraints
        self.target_level_ms = self
            .target_level_ms
            .max(self.effective_minimum_delay_ms)
            .min(self.effective_maximum_delay_ms);

        log::debug!(
            "Target delay updated: {}ms max {}ms min {}ms",
            self.target_level_ms,
            self.effective_minimum_delay_ms,
            self.effective_maximum_delay_ms
        );
    }

    fn update_effective_delay_bounds(&mut self) {
        // base maximum is based on the buffer size, so this is the strongest
        let upper_bound = self.config.base_maximum_delay_ms;
        let lower_bound = self.config.base_minimum_delay_ms.min(upper_bound);

        if self.minimum_delay_ms > 0 {
            self.effective_minimum_delay_ms =
                self.minimum_delay_ms.max(lower_bound).min(upper_bound);
        } else {
            self.effective_minimum_delay_ms = lower_bound;
        }

        if self.maximum_delay_ms > 0 {
            self.effective_maximum_delay_ms = self
                .maximum_delay_ms
                .max(self.effective_minimum_delay_ms)
                .min(upper_bound);
        } else {
            self.effective_maximum_delay_ms = upper_bound;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_delay_manager_creation() {
        let config = DelayConfig::default();
        let delay_manager = DelayManager::new(config);

        assert_eq!(delay_manager.target_delay_ms(), 80); // Now starts with kStartDelayMs
        assert_eq!(delay_manager.get_base_minimum_delay(), 0);
    }

    #[test]
    fn test_delay_manager_update() {
        let config = DelayConfig::default();
        let mut delay_manager = DelayManager::new(config);

        // Simulate packets arriving with some jitter
        let sample_rate = 16000;
        let mut timestamp = 0;

        for i in 0..10 {
            // Add some simulated jitter
            if i % 3 == 0 {
                thread::sleep(Duration::from_millis(5));
            }

            delay_manager.update(timestamp, sample_rate, false).unwrap();
            timestamp += 320; // 20ms at 16kHz
        }

        // Target delay should adapt to the jitter
        let target_delay = delay_manager.target_delay_ms();
        assert!(target_delay >= 20); // At least one packet duration
    }

    #[test]
    fn test_minimum_delay_constraints() {
        let config = DelayConfig {
            base_minimum_delay_ms: 20,
            base_maximum_delay_ms: 200,
            ..Default::default()
        };
        let mut delay_manager = DelayManager::new(config);

        assert_eq!(delay_manager.effective_minimum_delay_ms, 20);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 200);

        // Set minimum delay
        assert_eq!(delay_manager.set_minimum_delay(50), 50);
        assert_eq!(delay_manager.target_delay_ms(), 80); // max(80, 50) = 80
        assert_eq!(delay_manager.effective_minimum_delay_ms, 50);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 200);

        // Set maximum delay
        assert_eq!(delay_manager.set_maximum_delay(150), 150);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 50);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 150);

        // Set maximum below minimum
        assert_eq!(delay_manager.set_maximum_delay(40), 50);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 50);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 50);

        // Lower minumum
        assert_eq!(delay_manager.set_minimum_delay(30), 30);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 30);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 40);

        // Set minumum lower than base max
        assert_eq!(delay_manager.set_minimum_delay(300), 200);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 200);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 200);

        // Reset max
        assert_eq!(delay_manager.set_maximum_delay(0), 200);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 200);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 200);

        // Reset min
        assert_eq!(delay_manager.set_minimum_delay(0), 20);
        assert_eq!(delay_manager.effective_minimum_delay_ms, 20);
        assert_eq!(delay_manager.effective_maximum_delay_ms, 200);
    }

    /// 48 kHz RTP, 20 ms packets.
    const RATE: u32 = 48_000;
    const FRAME: u32 = 960;

    #[test]
    fn forward_timestamp_jump_after_a_long_pause_is_not_jitter() {
        crate::clock::freeze();
        let mut tracker = RelativeArrivalDelayTracker::new(DelayConfig::default());
        tracker.update(1_000, RATE, crate::clock::now());
        // The sender was muted for 95 s; it resumes with the timestamp 95 s
        // ahead, exactly as far as the wall clock moved.
        crate::clock::advance(Duration::from_secs(95));
        let delay = tracker.update(1_000 + 95 * RATE, RATE, crate::clock::now());
        assert_eq!(delay, 0);
    }

    #[test]
    fn timestamp_jump_beyond_half_the_range_does_not_overflow() {
        crate::clock::freeze();
        let mut tracker = RelativeArrivalDelayTracker::new(DelayConfig::default());
        tracker.update(100_000_000, RATE, crate::clock::now());
        crate::clock::advance(Duration::from_millis(20));
        // A restart onto an unrelated base; any finite answer is fine.
        tracker.update(3_000_000_000, RATE, crate::clock::now());
        crate::clock::advance(Duration::from_millis(20));
        tracker.update(3_000_000_000 + FRAME, RATE, crate::clock::now());
    }

    #[test]
    fn reordered_packet_registers_its_lateness() {
        crate::clock::freeze();
        let mut tracker = RelativeArrivalDelayTracker::new(DelayConfig::default());
        // Every 7th packet is 30 ms late, so it arrives after its successor.
        let mut arrivals: Vec<(u64, u32)> = (0..70u32)
            .map(|frame| {
                let late = if frame % 7 == 3 { 30 } else { 0 };
                (u64::from(frame) * 20 + late, frame * FRAME)
            })
            .collect();
        arrivals.sort();
        let mut now_ms = 0;
        let mut delay = 0;
        for (at, ts) in arrivals {
            crate::clock::advance(Duration::from_millis(at - now_ms));
            now_ms = at;
            delay = tracker.update(ts, RATE, crate::clock::now());
        }
        assert!(delay >= 30, "relative delay {delay} ms, want >= 30");
    }

    #[test]
    fn test_delay_manager_reset() {
        let config = DelayConfig::default();
        let mut delay_manager = DelayManager::new(config);

        // Update with some packets
        delay_manager.update(0, 16000, false).unwrap();
        delay_manager.update(320, 16000, false).unwrap();

        // Reset and check state
        delay_manager.reset();
        assert_eq!(delay_manager.target_delay_ms(), 80); // Reset to kStartDelayMs
    }
}
