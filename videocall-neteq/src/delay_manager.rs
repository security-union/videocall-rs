use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::{NetEqError, Result};

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
    /// Maximum number of packets in buffer
    pub max_packets_in_buffer: usize,
    /// Base minimum delay in milliseconds
    pub base_minimum_delay_ms: u32,
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
            quantile: 0.95,
            forget_factor: 0.983,
            start_forget_weight: Some(2.0),
            resample_interval_ms: Some(500),
            max_history_ms: 2000,
            max_packets_in_buffer: 200,
            base_minimum_delay_ms: 0,
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
    timestamp: u32,
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
    pub fn update(&mut self, timestamp: u32, sample_rate: u32) -> Option<i32> {
        let arrival_time = Instant::now();

        // Calculate inter-arrival time delay
        let iat_delay_ms = if let Some(last_time) = self.last_packet_time {
            arrival_time.duration_since(last_time).as_millis() as i32
        } else {
            0
        };

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

    fn update_delay_history(&mut self, iat_delay_ms: i32, timestamp: u32, sample_rate: u32) {
        let packet_delay = PacketDelay {
            iat_delay_ms,
            timestamp,
            arrival_time: Instant::now(),
        };

        self.delay_history.push_back(packet_delay);

        // Remove old entries based on max_history_ms
        let max_age = Duration::from_millis(self.config.max_history_ms as u64);
        let now = Instant::now();

        self.delay_history
            .retain(|delay| now.duration_since(delay.arrival_time) <= max_age);
    }

    fn calculate_relative_packet_arrival_delay(&self) -> Option<i32> {
        if self.delay_history.len() < 2 {
            return None;
        }

        // Calculate mean inter-arrival time
        let total_iat: i32 = self.delay_history.iter().map(|d| d.iat_delay_ms).sum();
        let mean_iat = total_iat as f64 / self.delay_history.len() as f64;

        // Calculate relative delay using quantile estimation
        let mut delays: Vec<i32> = self.delay_history.iter().map(|d| d.iat_delay_ms).collect();
        delays.sort_unstable();

        let quantile_index = ((delays.len() - 1) as f64 * self.config.quantile) as usize;
        let quantile_delay = delays[quantile_index] as f64;

        // Return the difference from expected inter-arrival time
        Some((quantile_delay - mean_iat) as i32)
    }
}

/// Delay manager for adaptive jitter buffer control
#[derive(Debug)]
pub struct DelayManager {
    config: DelayConfig,
    arrival_delay_tracker: RelativeArrivalDelayTracker,
    target_level_ms: u32,
    effective_minimum_delay_ms: u32,
    minimum_delay_ms: u32,
    maximum_delay_ms: u32,
    packet_len_ms: u32,
    filtered_delay: f64,
    initialized: bool,
    last_update_time: Instant,
}

impl DelayManager {
    /// Create a new delay manager
    pub fn new(config: DelayConfig) -> Self {
        let arrival_delay_tracker = RelativeArrivalDelayTracker::new(config.clone());

        Self {
            target_level_ms: config.base_minimum_delay_ms,
            effective_minimum_delay_ms: config.base_minimum_delay_ms,
            minimum_delay_ms: config.base_minimum_delay_ms,
            maximum_delay_ms: 0, // No maximum by default
            packet_len_ms: 20,   // Default 20ms packets
            filtered_delay: 0.0,
            initialized: false,
            last_update_time: Instant::now(),
            arrival_delay_tracker,
            config,
        }
    }

    /// Update the delay manager with a new packet
    pub fn update(&mut self, timestamp: u32, sample_rate: u32, reset: bool) -> Result<Option<i32>> {
        if reset {
            self.reset();
        }

        // Update arrival delay tracking
        let relative_delay = self.arrival_delay_tracker.update(timestamp, sample_rate);

        if let Some(delay) = relative_delay {
            self.update_target_delay(delay);
        }

        self.last_update_time = Instant::now();

        Ok(relative_delay)
    }

    /// Reset the delay manager
    pub fn reset(&mut self) {
        self.arrival_delay_tracker.reset();
        self.filtered_delay = 0.0;
        self.initialized = false;
        self.target_level_ms = self.config.base_minimum_delay_ms;
    }

    /// Get the current target delay in milliseconds
    pub fn target_delay_ms(&self) -> u32 {
        self.target_level_ms.max(self.effective_minimum_delay_ms)
    }

    /// Set the packet audio length in milliseconds
    pub fn set_packet_audio_length(&mut self, length_ms: u32) -> Result<()> {
        if length_ms == 0 {
            return Err(NetEqError::InvalidConfig(
                "Packet length cannot be zero".to_string(),
            ));
        }

        self.packet_len_ms = length_ms;
        Ok(())
    }

    /// Set minimum delay constraint
    pub fn set_minimum_delay(&mut self, delay_ms: u32) -> Result<bool> {
        if self.is_valid_minimum_delay(delay_ms) {
            self.minimum_delay_ms = delay_ms;
            self.update_effective_minimum_delay();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Set maximum delay constraint
    pub fn set_maximum_delay(&mut self, delay_ms: u32) -> Result<bool> {
        // Validate maximum delay
        if delay_ms > 0 && delay_ms < self.minimum_delay_ms {
            return Ok(false);
        }

        self.maximum_delay_ms = delay_ms;
        self.update_effective_minimum_delay();
        Ok(true)
    }

    /// Set base minimum delay
    pub fn set_base_minimum_delay(&mut self, delay_ms: u32) -> Result<bool> {
        if self.is_valid_base_minimum_delay(delay_ms) {
            self.config.base_minimum_delay_ms = delay_ms;
            self.update_effective_minimum_delay();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Get base minimum delay
    pub fn get_base_minimum_delay(&self) -> u32 {
        self.config.base_minimum_delay_ms
    }

    fn update_target_delay(&mut self, relative_delay: i32) {
        if !self.initialized {
            // Initialize the filtered delay
            self.filtered_delay = relative_delay as f64;
            self.initialized = true;
        } else {
            // Apply exponential smoothing
            let forget_factor = if let Some(start_weight) = self.config.start_forget_weight {
                // Use stronger initial adaptation
                let adaptation_factor = (start_weight - 1.0)
                    * (-self.last_update_time.elapsed().as_secs_f64() / 10.0).exp()
                    + 1.0;
                self.config.forget_factor * adaptation_factor
            } else {
                self.config.forget_factor
            };

            self.filtered_delay = self.filtered_delay * forget_factor
                + (relative_delay as f64) * (1.0 - forget_factor);
        }

        // Update target level based on filtered delay
        let quantile_delay = self.filtered_delay * self.config.quantile;
        self.target_level_ms = (quantile_delay as u32)
            .max(self.config.base_minimum_delay_ms)
            .max(self.packet_len_ms); // At least one packet duration

        // Apply constraints
        if self.maximum_delay_ms > 0 {
            self.target_level_ms = self.target_level_ms.min(self.maximum_delay_ms);
        }

        log::debug!(
            "Target delay updated: {}ms (relative_delay: {}ms, filtered: {:.2}ms)",
            self.target_level_ms,
            relative_delay,
            self.filtered_delay
        );
    }

    fn update_effective_minimum_delay(&mut self) {
        let upper_bound = self.minimum_delay_upper_bound();

        self.effective_minimum_delay_ms = self
            .minimum_delay_ms
            .max(self.config.base_minimum_delay_ms)
            .min(upper_bound);
    }

    fn minimum_delay_upper_bound(&self) -> u32 {
        // Calculate upper bound based on buffer size and maximum delay
        let buffer_based_limit =
            (self.config.max_packets_in_buffer as u32 * self.packet_len_ms * 3) / 4;

        if self.maximum_delay_ms > 0 {
            buffer_based_limit.min(self.maximum_delay_ms)
        } else {
            buffer_based_limit
        }
    }

    fn is_valid_minimum_delay(&self, delay_ms: u32) -> bool {
        delay_ms <= self.minimum_delay_upper_bound()
    }

    fn is_valid_base_minimum_delay(&self, delay_ms: u32) -> bool {
        delay_ms <= self.minimum_delay_upper_bound()
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

        assert_eq!(delay_manager.target_delay_ms(), 0);
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
        let config = DelayConfig::default();
        let mut delay_manager = DelayManager::new(config);

        // Set minimum delay
        assert!(delay_manager.set_minimum_delay(50).unwrap());
        assert_eq!(delay_manager.target_delay_ms(), 50);

        // Set maximum delay
        assert!(delay_manager.set_maximum_delay(200).unwrap());

        // Try to set minimum delay higher than maximum
        assert!(!delay_manager.set_maximum_delay(30).unwrap());
    }

    #[test]
    fn test_packet_length_setting() {
        let config = DelayConfig::default();
        let mut delay_manager = DelayManager::new(config);

        assert!(delay_manager.set_packet_audio_length(10).is_ok());
        assert!(delay_manager.set_packet_audio_length(0).is_err());
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
        assert_eq!(delay_manager.target_delay_ms(), 0);
    }
}
