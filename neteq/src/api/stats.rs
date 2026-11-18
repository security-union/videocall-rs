// Port of the statistics structs in WebRTC api/neteq/neteq.h at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2012 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! NetEq statistics structs, field for field with the C++ API.

use serde::{Deserialize, Serialize};

/// Network statistics. Rates are Q14 fractions. Reset by
/// `NetEq::network_statistics()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetEqNetworkStatistics {
    /// Current jitter buffer size in ms.
    pub current_buffer_size_ms: u16,
    /// Target buffer size in ms.
    pub preferred_buffer_size_ms: u16,
    /// 1 if adding extra delay due to peaky jitter; 0 otherwise.
    pub jitter_peaks_found: u16,
    /// Fraction (of original stream) of synthesized audio inserted through
    /// expansion (in Q14).
    pub expand_rate: u16,
    /// Fraction (of original stream) of synthesized speech inserted through
    /// expansion (in Q14).
    pub speech_expand_rate: u16,
    /// Fraction of data inserted through preemptive expansion (in Q14).
    pub preemptive_rate: u16,
    /// Fraction of data removed through acceleration (in Q14).
    pub accelerate_rate: u16,
    /// Fraction of data coming from FEC/RED decoding (in Q14).
    pub secondary_decoded_rate: u16,
    /// Fraction of discarded FEC/RED data (in Q14).
    pub secondary_discarded_rate: u16,
    /// Mean packet waiting time (arrival to decode) in ms, -1 if none.
    pub mean_waiting_time_ms: i32,
    /// Median packet waiting time in ms, -1 if none.
    pub median_waiting_time_ms: i32,
    /// Minimum packet waiting time in ms, -1 if none.
    pub min_waiting_time_ms: i32,
    /// Maximum packet waiting time in ms, -1 if none.
    pub max_waiting_time_ms: i32,
}

/// Statistics that persist over the lifetime of NetEq and are never reset.
/// Fields mirror the W3C `RTCInboundRtpStreamStats` names where they exist.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetEqLifetimeStatistics {
    pub total_samples_received: u64,
    pub concealed_samples: u64,
    pub concealment_events: u64,
    pub jitter_buffer_delay_ms: u64,
    pub jitter_buffer_emitted_count: u64,
    pub jitter_buffer_target_delay_ms: u64,
    pub jitter_buffer_minimum_delay_ms: u64,
    pub inserted_samples_for_deceleration: u64,
    pub removed_samples_for_acceleration: u64,
    pub silent_concealed_samples: u64,
    pub fec_packets_received: u64,
    pub fec_packets_discarded: u64,
    pub packets_discarded: u64,
    // Below stats are not part of the spec.
    pub delayed_packet_outage_samples: u64,
    pub delayed_packet_outage_events: u64,
    /// Sum of relative packet arrival delays of received packets so far.
    pub relative_packet_arrival_delay_ms: u64,
    pub jitter_buffer_packets_received: u64,
    /// Number of loss-concealment events lasting at least 150 ms.
    pub interruption_count: i32,
    /// Total duration of the interruptions counted above.
    pub total_interruption_duration_ms: i32,
    /// Total number of comfort noise samples generated during DTX.
    pub generated_noise_samples: u64,
    pub total_processing_delay_us: u64,
}

/// Operations performed by NetEq and its internal state. Never reset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetEqOperationsAndState {
    /// Cumulative preemptive expand samples.
    pub preemptive_samples: u64,
    /// Cumulative accelerate samples.
    pub accelerate_samples: u64,
    /// Count of the number of buffer flushes.
    pub packet_buffer_flushes: u64,
    // The statistics below are not cumulative.
    /// The waiting time of the last decoded packet.
    pub last_waiting_time_ms: u64,
    /// The sum of the packet and jitter buffer size in ms.
    pub current_buffer_size_ms: u64,
    /// The current frame size in ms.
    pub current_frame_size_ms: u64,
    /// Flag to indicate that the next packet is available.
    pub next_packet_available: bool,
}
