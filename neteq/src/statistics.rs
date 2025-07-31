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

/// Q14 fixed-point math constants and utilities
///
/// ## What is Q14 Format?
///
/// Q14 is a **fixed-point number format** used throughout WebRTC's audio processing pipeline.
/// It represents fractional numbers using integers, which was crucial for embedded systems
/// and real-time audio processing when WebRTC was originally developed.
///
/// ### Format Breakdown:
/// ```text
/// Q14 in 16-bit integer:
/// [S][I][F F F F F F F F F F F F F F]
///  ↑  ↑  ← ---- 14 fractional bits ----→
///  │  └── 1 integer bit (range 0-1.99)  
///  └───── 1 sign bit
/// ```
///
/// ### The Math:
/// ```text
/// Q14_value = actual_floating_point_value × 2¹⁴
/// Q14_value = actual_floating_point_value × 16384
///
/// To convert back:
/// actual_value = Q14_value ÷ 16384
/// ```
///
/// ## Why Use Q14? (Historical Context)
///
/// 1. **No Floating Point Unit (FPU)**: Many embedded audio processors lacked FPUs
/// 2. **Deterministic Results**: Same calculation produces identical results across platforms
/// 3. **Performance**: Integer arithmetic was much faster than software floating point
/// 4. **Memory Efficient**: 16-bit integers vs 32-bit floats saved precious RAM
/// 5. **Sufficient Precision**: 14 bits = 1/16384 ≈ 0.00006 resolution (good for audio ratios)
///
/// ## Real-World Examples:
///
/// | Actual Ratio | Meaning | Q14 Value | Per-mille (‰) |
/// |--------------|---------|-----------|----------------|
/// | 0.0 | No expansion | 0 | 0‰ |
/// | 0.25 | 25% expansion | 4096 | 250‰ |
/// | 0.5 | 50% expansion | 8192 | 500‰ |
/// | 1.0 | 100% expansion | 16384 | 1000‰ |
///
/// ## WebRTC Usage Pattern:
///
/// WebRTC calculates audio processing ratios in Q14 format:
/// ```cpp
/// // WebRTC C++ code:
/// stats->expand_rate = CalculateQ14Ratio(expanded_samples, total_samples);
///
/// // Where CalculateQ14Ratio() returns:
/// return (numerator << 14) / denominator;  // Multiply by 16384
/// ```
///
/// Then everywhere they need to display it:
/// ```cpp
/// float display_rate = stats.expand_rate / 16384.0f;  // Convert back to ratio
/// ```
///
/// ## Our Improved Implementation:
///
/// Instead of scattering magic numbers like WebRTC does, we provide:
/// - **Centralized constants** (`SCALE = 16384`)
/// - **Clear conversion functions** (`to_per_mille()`, `from_float()`)
/// - **Self-documenting code** (no magic numbers)
/// - **Type safety** (explicit conversions)
///
/// ### Example Usage:
/// ```rust
/// use neteq::statistics::q14;
/// // Calculate 25% expansion ratio
/// let ratio = 320.0 / 1280.0;  // 0.25
/// let q14_value = q14::from_float(ratio);  // 4096
/// let display_rate = q14::to_per_mille(q14_value);  // ~250.0‰
/// assert_eq!(q14_value, 4096);
/// assert!((display_rate - 250.0).abs() < 0.01);
/// ```
///
/// ## Why Still Use Q14 Today?
///
/// While modern processors have powerful FPUs, we maintain Q14 compatibility because:
/// 1. **WebRTC Compatibility**: Our statistics must match WebRTC's format
/// 2. **Cross-Platform Consistency**: Same results on ARM, x86, WASM, etc.
/// 3. **Proven in Production**: Billions of WebRTC calls validate this approach
/// 4. **Network Protocol**: Q14 values are transmitted over the network
///
/// The key difference: we provide clean abstractions around the legacy format
/// rather than exposing raw magic numbers throughout the codebase.
pub mod q14 {
    /// Q14 scale factor: 2^14 = 16384
    ///
    /// This is the fundamental constant for Q14 fixed-point arithmetic.
    /// All Q14 values are actual_value × SCALE.
    pub const SCALE: f64 = 16384.0;

    /// Q14 scale factor as f32 for performance-critical conversions
    pub const SCALE_F32: f32 = 16384.0;

    /// Convert Q14 integer to floating point ratio
    ///
    /// # Examples
    /// ```rust
    /// # use neteq::statistics::q14;
    /// assert_eq!(q14::to_float(0), 0.0);
    /// assert_eq!(q14::to_float(8192), 0.5);
    /// assert_eq!(q14::to_float(16384), 1.0);
    /// ```
    #[inline]
    pub fn to_float(q14_value: u16) -> f64 {
        q14_value as f64 / SCALE
    }

    /// Convert Q14 integer to per-mille (‰) for UI display
    ///
    /// Per-mille (‰) = parts per thousand = 0.1%
    /// This matches WebRTC's statistics display format.
    ///
    /// # Examples
    /// ```rust
    /// # use neteq::statistics::q14;
    /// assert_eq!(q14::to_per_mille(0), 0.0);       // 0‰
    /// assert!((q14::to_per_mille(4096) - 250.0).abs() < 0.01); // ~250‰ (25%)
    /// assert!((q14::to_per_mille(8192) - 500.0).abs() < 0.01); // ~500‰ (50%)
    /// ```
    #[inline]
    pub fn to_per_mille(q14_value: u16) -> f32 {
        q14_value as f32 / (SCALE_F32 / 1000.0) // Equivalent to / 16.384
    }

    /// Convert floating point ratio to Q14 integer
    ///
    /// Clamps the input to valid Q14 range [0.0, 1.0] to prevent overflow.
    ///
    /// # Examples
    /// ```rust
    /// # use neteq::statistics::q14;
    /// assert_eq!(q14::from_float(0.0), 0);
    /// assert_eq!(q14::from_float(0.25), 4096);
    /// assert_eq!(q14::from_float(1.0), 16384);
    /// assert_eq!(q14::from_float(2.0), 16384);  // Clamped to max
    /// ```
    #[inline]
    pub fn from_float(ratio: f64) -> u16 {
        ((ratio * SCALE).min(SCALE).max(0.0)) as u16
    }

    /// Convert per-mille (‰) back to Q14 integer
    ///
    /// Useful for testing or when receiving per-mille values that need
    /// to be stored in Q14 format.
    ///
    /// # Examples
    /// ```rust
    /// # use neteq::statistics::q14;
    /// assert_eq!(q14::from_per_mille(0.0), 0);
    /// assert_eq!(q14::from_per_mille(250.0), 4096);
    /// assert_eq!(q14::from_per_mille(500.0), 8192);
    /// ```
    #[inline]
    pub fn from_per_mille(per_mille: f32) -> u16 {
        from_float((per_mille / 1000.0) as f64)
    }
}

use serde::{Deserialize, Serialize};
use web_time::{Duration, Instant};

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
    /// Packet reordering statistics
    pub reordered_packets: u32,
    pub total_packets_received: u32,
    pub reorder_rate_permyriad: u16, // Reordering rate in per-myriad (‰)
    pub max_reorder_distance: u16,   // Maximum sequence number distance for reordered packets
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
    _last_update: Instant,
    /// Total output samples for rate calculations
    total_output_samples: u64,
    /// Total expanded samples for rate calculations
    total_expanded_samples: u64,
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
            _last_update: now,
            total_output_samples: 0,
            total_expanded_samples: 0,
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
        // Track total output samples for rate calculations
        self.total_output_samples += samples;

        match operation {
            TimeStretchOperation::Accelerate => {
                self.lifetime_stats.removed_samples_for_acceleration += samples;
                self.operation_stats.accelerate_samples += samples;
                // Calculate cumulative accelerate rate: (removed_samples / total_output_samples) in Q14
                if self.total_output_samples > 0 {
                    let ratio = self.lifetime_stats.removed_samples_for_acceleration as f64
                        / self.total_output_samples as f64;
                    self.network_stats.accelerate_rate = q14::from_float(ratio);
                }
            }
            TimeStretchOperation::PreemptiveExpand => {
                self.lifetime_stats.inserted_samples_for_deceleration += samples;
                self.operation_stats.preemptive_samples += samples;
                // Calculate cumulative preemptive rate: (inserted_samples / total_output_samples) in Q14
                if self.total_output_samples > 0 {
                    let ratio = self.lifetime_stats.inserted_samples_for_deceleration as f64
                        / self.total_output_samples as f64;
                    self.network_stats.preemptive_rate = q14::from_float(ratio);
                }
            }
            TimeStretchOperation::Expand => {
                self.total_expanded_samples += samples;
                // Calculate cumulative expand rate: (expanded_samples / total_output_samples) in Q14
                if self.total_output_samples > 0 {
                    let ratio =
                        self.total_expanded_samples as f64 / self.total_output_samples as f64;
                    self.network_stats.expand_rate = q14::from_float(ratio);
                }
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

    /// Record packet reordering event
    pub fn packet_reordered(&mut self, sequence_distance: u16) {
        self.network_stats.reordered_packets += 1;
        self.network_stats.total_packets_received += 1;

        // Update maximum reorder distance
        if sequence_distance > self.network_stats.max_reorder_distance {
            self.network_stats.max_reorder_distance = sequence_distance;
        }

        // Calculate reorder rate in per-myriad
        let rate = (self.network_stats.reordered_packets as f64
            / self.network_stats.total_packets_received as f64)
            * 10000.0;
        self.network_stats.reorder_rate_permyriad = rate as u16;
    }

    /// Record normal packet arrival (in order)
    pub fn packet_in_order(&mut self) {
        self.network_stats.total_packets_received += 1;

        // Recalculate reorder rate
        let rate = (self.network_stats.reordered_packets as f64
            / self.network_stats.total_packets_received as f64)
            * 10000.0;
        self.network_stats.reorder_rate_permyriad = rate as u16;
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

    #[test]
    fn test_expand_rate_calculation_fix() {
        let mut stats = StatisticsCalculator::new();

        // Simulate normal operation: 10 frames of normal audio (160 samples each)
        for _ in 0..10 {
            stats.time_stretch_operation(TimeStretchOperation::Accelerate, 160);
            // Not expand, just to track output
        }

        // Reset accelerate stats to focus on expand rate
        stats.network_stats.accelerate_rate = 0;
        stats.lifetime_stats.removed_samples_for_acceleration = 0;

        // Simulate 2 expand operations (160 samples each)
        stats.time_stretch_operation(TimeStretchOperation::Expand, 160);
        stats.time_stretch_operation(TimeStretchOperation::Expand, 160);

        // Total: 1600 + 320 = 1920 output samples
        // Expanded: 320 samples
        // Expected rate: (320 / 1920) * 16384 = 2730.67 ≈ 2731 (Q14)
        // Expected UI display: 2731 / 16.384 = 166.7‰

        let expected_q14 = ((320.0 / 1920.0) * 16384.0) as u16;
        assert_eq!(stats.network_stats.expand_rate, expected_q14);

        // The UI should now show ~166.7‰ instead of 7864.0
        let ui_display_rate = stats.network_stats.expand_rate as f32 / 16.384;
        assert!(
            (ui_display_rate - 166.7).abs() < 1.0,
            "Expected ~166.7‰, got {:.1}‰",
            ui_display_rate
        );

        println!("✅ Expand rate calculation fix verified:");
        println!("   Q14 value: {}", stats.network_stats.expand_rate);
        println!(
            "   UI display: {:.1}‰ (was showing 7864.0)",
            ui_display_rate
        );
    }

    #[test]
    fn test_expand_rate_no_longer_spikes_to_7864() {
        let mut stats = StatisticsCalculator::new();

        // Simulate the scenario: peer starts talking → buffer empty → expansion
        // This used to cause expand_rate = 7864.0

        // First, some normal operation
        for _ in 0..5 {
            stats.time_stretch_operation(TimeStretchOperation::Accelerate, 160);
        }

        // Reset to focus on expansion
        stats.network_stats.accelerate_rate = 0;
        stats.lifetime_stats.removed_samples_for_acceleration = 0;

        // Expansion happens (this used to set expand_rate = 7864.0)
        stats.time_stretch_operation(TimeStretchOperation::Expand, 480); // 30ms at 16kHz

        // With the fix, the rate should be reasonable
        let ui_rate = stats.network_stats.expand_rate as f32 / 16.384;

        // Should be much less than the old broken value of 7864.0
        assert!(
            ui_rate < 1000.0,
            "Expand rate still too high: {:.1}‰ (was 7864.0)",
            ui_rate
        );

        // Should be approximately: (480 / (800 + 480)) * 1000 = 375‰
        let expected_rate = (480.0 / 1280.0) * 1000.0; // ≈ 375‰
        assert!(
            (ui_rate - expected_rate).abs() < 50.0,
            "Expected ~{:.1}‰, got {:.1}‰",
            expected_rate,
            ui_rate
        );

        println!("🚀 Bug fix verified: expand rate no longer spikes to 7864.0");
        println!("   Old broken value: 7864.0‰");
        println!("   New correct value: {:.1}‰", ui_rate);
    }

    #[test]
    fn test_q14_documentation_examples() {
        use super::q14;

        println!("🧮 Testing Q14 documentation examples...");

        // Test basic conversions from documentation table
        assert_eq!(q14::to_float(0), 0.0);
        assert_eq!(q14::to_float(4096), 0.25);
        assert_eq!(q14::to_float(8192), 0.5);
        assert_eq!(q14::to_float(16384), 1.0);

        // Test per-mille conversions (use tolerance for floating point)
        assert_eq!(q14::to_per_mille(0), 0.0);
        assert!((q14::to_per_mille(4096) - 250.0).abs() < 0.01);
        assert!((q14::to_per_mille(8192) - 500.0).abs() < 0.01);
        assert!((q14::to_per_mille(16384) - 1000.0).abs() < 0.01);

        // Test round-trip conversions
        assert_eq!(q14::from_float(0.0), 0);
        assert_eq!(q14::from_float(0.25), 4096);
        assert_eq!(q14::from_float(0.5), 8192);
        assert_eq!(q14::from_float(1.0), 16384);

        // Test clamping
        assert_eq!(q14::from_float(2.0), 16384); // Clamped to max
        assert_eq!(q14::from_float(-1.0), 0); // Clamped to min

        // Test per-mille round-trip
        assert_eq!(q14::from_per_mille(0.0), 0);
        assert_eq!(q14::from_per_mille(250.0), 4096);
        assert_eq!(q14::from_per_mille(500.0), 8192);
        assert_eq!(q14::from_per_mille(1000.0), 16384);

        // Test the worked example from documentation
        let ratio = 320.0 / 1280.0; // 0.25
        let q14_value = q14::from_float(ratio); // Should be 4096
        let display_rate = q14::to_per_mille(q14_value); // Should be 250.0‰

        assert_eq!(q14_value, 4096);
        assert!((display_rate - 250.0).abs() < 0.01);

        println!("✅ All Q14 documentation examples validated!");

        // Demonstrate the precision of Q14
        let precision = 1.0 / q14::SCALE;
        println!("📏 Q14 precision: {:.6} (~0.00006)", precision);

        // Show the difference between our approach and WebRTC's magic numbers
        let webrtc_conversion = 4096.0 / 16384.0; // WebRTC style
        let our_conversion = q14::to_float(4096); // Our style
        assert_eq!(webrtc_conversion, our_conversion);
        println!(
            "🎯 WebRTC compatibility: {} == {}",
            webrtc_conversion, our_conversion
        );
    }

    #[test]
    fn test_q14_real_world_scenarios() {
        use super::q14;

        println!("🌍 Testing real-world Q14 scenarios...");

        // Scenario 1: 15% audio expansion (realistic NetEQ operation)
        let expanded_samples = 150_u64;
        let total_samples = 1000_u64;
        let ratio = expanded_samples as f64 / total_samples as f64;
        let q14_rate = q14::from_float(ratio);
        let ui_display = q14::to_per_mille(q14_rate);

        println!(
            "📊 15% expansion: {} samples / {} total",
            expanded_samples, total_samples
        );
        println!("   Ratio: {:.3}", ratio);
        println!("   Q14:   {}", q14_rate);
        println!("   UI:    {:.1}‰", ui_display);

        assert_eq!(ratio, 0.15);
        assert_eq!(q14_rate, 2457); // (0.15 * 16384) = 2457.6 ≈ 2457
        assert!((ui_display - 150.0).abs() < 0.1);

        // Scenario 2: The original bug case (480ms expansion in 1000ms)
        let bug_scenario_ratio = 480.0 / 1000.0;
        let bug_q14 = q14::from_float(bug_scenario_ratio);
        let bug_display = q14::to_per_mille(bug_q14);

        println!("🐛 Original bug scenario:");
        println!(
            "   WebRTC raw magic: {:.1} (broken)",
            bug_scenario_ratio * 16384.0
        );
        println!("   Our Q14 value:    {}", bug_q14);
        println!("   Our UI display:   {:.1}‰ (correct)", bug_display);

        assert_eq!(bug_q14, 7864); // Correct Q14 encoding
                                   // Note: due to precision, 7864/16.384 = 479.98..., which is very close to 480
        assert!((bug_display - 480.0).abs() < 0.1); // Correct per-mille display (within 0.1‰)

        // Demonstrate that raw 7864 would be wrong to display directly
        assert_ne!(7864.0, bug_display); // Proves the original bug

        println!("✅ Real-world scenarios validated!");
    }
}
