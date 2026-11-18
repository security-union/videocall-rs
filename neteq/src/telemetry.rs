//! Dashboard telemetry derived from NetEq statistics, outside the core
//! (PARITY_PLAN.md 8.3).
//!
//! [`NetEqStats`] is the JSON the worker posts to the page. It keeps the
//! field names consumers parse today (`current_buffer_size_ms`,
//! `target_delay_ms`, `packets_awaiting_decode`, `packets_per_sec`,
//! `network.{expand_rate, accelerate_rate, reorder_rate_permyriad,
//! reordered_packets, max_reorder_distance, operation_counters.*}`,
//! `lifetime.{jitter_buffer_delay_ms, jitter_buffer_target_delay_ms}`) and
//! fills them from the WebRTC statistics structs.
//!
//! Per-second operation counters, `packets_per_sec` and the reorder counters
//! are not WebRTC stats. [`TelemetryCollector`] derives them from what the
//! wrapper sees (sequence numbers at insert, the operation of each
//! `GetAudio`), so they never feed back into NetEq and cannot perturb parity.

use crate::api::neteq::Operation;
use crate::api::stats::{NetEqLifetimeStatistics, NetEqNetworkStatistics, NetEqOperationsAndState};
use crate::rtc_base::RtpSequenceNumberUnwrapper;
use serde::{Deserialize, Serialize};

/// Q14 fixed-point helpers for the rate fields.
pub mod q14 {
    /// 2^14. A Q14 value is `ratio * SCALE`.
    pub const SCALE: f64 = 16384.0;

    /// [`SCALE`] as `f32`.
    pub const SCALE_F32: f32 = 16384.0;

    /// Q14 to a ratio in `[0, 1]`.
    ///
    /// ```rust
    /// # use neteq::telemetry::q14;
    /// assert_eq!(q14::to_float(8192), 0.5);
    /// ```
    #[inline]
    pub fn to_float(q14_value: u16) -> f64 {
        f64::from(q14_value) / SCALE
    }

    /// Q14 to per-mille, the unit WebRTC's stats pages display.
    ///
    /// ```rust
    /// # use neteq::telemetry::q14;
    /// assert!((q14::to_per_mille(4096) - 250.0).abs() < 0.01);
    /// ```
    #[inline]
    pub fn to_per_mille(q14_value: u16) -> f32 {
        f32::from(q14_value) / (SCALE_F32 / 1000.0)
    }

    /// Ratio to Q14, clamped to `[0, 1]`.
    ///
    /// ```rust
    /// # use neteq::telemetry::q14;
    /// assert_eq!(q14::from_float(0.25), 4096);
    /// assert_eq!(q14::from_float(2.0), 16384);
    /// ```
    #[inline]
    pub fn from_float(ratio: f64) -> u16 {
        (ratio * SCALE).clamp(0.0, SCALE) as u16
    }

    /// Per-mille to Q14, clamped.
    ///
    /// ```rust
    /// # use neteq::telemetry::q14;
    /// assert_eq!(q14::from_per_mille(500.0), 8192);
    /// ```
    #[inline]
    pub fn from_per_mille(per_mille: f32) -> u16 {
        from_float(f64::from(per_mille / 1000.0))
    }
}

/// How often each NetEq operation ran, per second, over the last window.
/// Field names match the 0.x JSON. CNG covers all three WebRTC CNG
/// operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OperationCounters {
    pub normal_per_sec: f32,
    pub expand_per_sec: f32,
    pub accelerate_per_sec: f32,
    pub fast_accelerate_per_sec: f32,
    pub preemptive_expand_per_sec: f32,
    pub merge_per_sec: f32,
    pub comfort_noise_per_sec: f32,
    pub dtmf_per_sec: f32,
    pub undefined_per_sec: f32,
}

/// The `network` object: WebRTC `NetEqNetworkStatistics` (non-resetting
/// `CurrentNetworkStatistics`) plus the wrapper-derived counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkTelemetry {
    /// Current jitter buffer size in ms.
    pub current_buffer_size_ms: u16,
    /// Target buffer size in ms.
    pub preferred_buffer_size_ms: u16,
    /// 1 if extra delay is added for peaky jitter.
    pub jitter_peaks_found: u16,
    /// Q14 fraction of audio synthesized by expansion.
    pub expand_rate: u16,
    /// Q14 fraction of speech synthesized by expansion.
    pub speech_expand_rate: u16,
    /// Q14 fraction inserted by preemptive expansion.
    pub preemptive_rate: u16,
    /// Q14 fraction removed by acceleration.
    pub accelerate_rate: u16,
    /// Q14 fraction decoded from FEC/RED.
    pub secondary_decoded_rate: u16,
    /// Q14 fraction of FEC/RED data discarded.
    pub secondary_discarded_rate: u16,
    /// Packet waiting times (arrival to decode) in ms, -1 when unknown.
    pub mean_waiting_time_ms: i32,
    pub median_waiting_time_ms: i32,
    pub min_waiting_time_ms: i32,
    pub max_waiting_time_ms: i32,
    /// Packets inserted behind the newest sequence number seen.
    pub reordered_packets: u32,
    /// Packets inserted in total.
    pub total_packets_received: u32,
    /// `reordered_packets / total_packets_received` in 1/10000.
    pub reorder_rate_permyriad: u16,
    /// Largest distance, in sequence numbers, a packet arrived behind.
    pub max_reorder_distance: u16,
    pub operation_counters: OperationCounters,
}

impl NetworkTelemetry {
    fn from_parts(network: &NetEqNetworkStatistics, derived: &DerivedCounters) -> Self {
        Self {
            current_buffer_size_ms: network.current_buffer_size_ms,
            preferred_buffer_size_ms: network.preferred_buffer_size_ms,
            jitter_peaks_found: network.jitter_peaks_found,
            expand_rate: network.expand_rate,
            speech_expand_rate: network.speech_expand_rate,
            preemptive_rate: network.preemptive_rate,
            accelerate_rate: network.accelerate_rate,
            secondary_decoded_rate: network.secondary_decoded_rate,
            secondary_discarded_rate: network.secondary_discarded_rate,
            mean_waiting_time_ms: network.mean_waiting_time_ms,
            median_waiting_time_ms: network.median_waiting_time_ms,
            min_waiting_time_ms: network.min_waiting_time_ms,
            max_waiting_time_ms: network.max_waiting_time_ms,
            reordered_packets: derived.reordered_packets,
            total_packets_received: derived.total_packets_received,
            reorder_rate_permyriad: derived.reorder_rate_permyriad(),
            max_reorder_distance: derived.max_reorder_distance,
            operation_counters: derived.operation_counters,
        }
    }
}

/// Dashboard-facing NetEq statistics, posted by the worker as JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetEqStats {
    pub network: NetworkTelemetry,
    /// WebRTC `NetEqLifetimeStatistics`, never reset.
    pub lifetime: NetEqLifetimeStatistics,
    /// WebRTC `NetEqOperationsAndState`. New in this schema.
    pub operations: NetEqOperationsAndState,
    /// Same as `network.current_buffer_size_ms`, kept at the top level for
    /// existing consumers.
    pub current_buffer_size_ms: u32,
    /// Target delay: `network.preferred_buffer_size_ms`.
    pub target_delay_ms: u32,
    /// Packets in the packet buffer (`NumPacketsInBuffer`).
    pub packets_awaiting_decode: usize,
    /// Packets inserted during the last full one-second window.
    pub packets_per_sec: u32,
}

impl NetEqStats {
    /// Assemble the JSON from NetEq's stats and the wrapper's counters.
    /// `network` must come from `current_network_statistics()` so taking a
    /// dashboard snapshot does not reset NetEq's rate counters.
    pub fn from_parts(
        network: &NetEqNetworkStatistics,
        lifetime: &NetEqLifetimeStatistics,
        operations: &NetEqOperationsAndState,
        packets_in_buffer: usize,
        collector: &TelemetryCollector,
    ) -> Self {
        let derived = &collector.derived;
        Self {
            network: NetworkTelemetry::from_parts(network, derived),
            lifetime: *lifetime,
            operations: *operations,
            current_buffer_size_ms: u32::from(network.current_buffer_size_ms),
            target_delay_ms: u32::from(network.preferred_buffer_size_ms),
            packets_awaiting_decode: packets_in_buffer,
            packets_per_sec: derived.packets_per_sec,
        }
    }
}

impl NetEqStats {
    /// Snapshot `neteq` without resetting any of its counters.
    ///
    /// `packets_awaiting_decode` is derived from the public API: the packet
    /// buffer's share of `GetOperationsAndState().current_buffer_size_ms`
    /// (packet buffer plus sync buffer) divided by the current frame size.
    /// For fixed-size Opus frames this equals `NumPacketsInBuffer`.
    pub fn from_neteq(neteq: &crate::api::neteq::NetEq, collector: &TelemetryCollector) -> Self {
        let operations = neteq.operations_and_state();
        let sync_buffer_ms = u64::try_from(neteq.sync_buffer_size_ms()).unwrap_or(0);
        let packet_buffer_ms = operations
            .current_buffer_size_ms
            .saturating_sub(sync_buffer_ms);
        let packets_in_buffer = packet_buffer_ms
            .checked_div(operations.current_frame_size_ms)
            .unwrap_or(0) as usize;
        Self::from_parts(
            &neteq.current_network_statistics(),
            &neteq.lifetime_statistics(),
            &operations,
            packets_in_buffer,
            collector,
        )
    }
}

/// Length of the window behind the per-second rates.
pub const RATE_WINDOW_MS: u64 = 1_000;

/// Number of slots in the per-window operation tally, one per `Operation`.
const OPERATION_SLOTS: usize = 11;

#[derive(Debug, Clone, Default)]
struct DerivedCounters {
    reordered_packets: u32,
    total_packets_received: u32,
    max_reorder_distance: u16,
    packets_per_sec: u32,
    operation_counters: OperationCounters,
}

impl DerivedCounters {
    fn reorder_rate_permyriad(&self) -> u16 {
        if self.total_packets_received == 0 {
            return 0;
        }
        (u64::from(self.reordered_packets) * 10_000 / u64::from(self.total_packets_received)) as u16
    }
}

/// Derives the non-WebRTC counters from the wrapper's view of the stream.
/// Times are caller-supplied milliseconds (any monotonic origin), so the
/// collector needs no clock and tests are deterministic.
#[derive(Debug, Clone, Default)]
pub struct TelemetryCollector {
    derived: DerivedCounters,
    unwrapper: RtpSequenceNumberUnwrapper,
    highest_sequence: Option<i64>,
    packet_window_start_ms: Option<u64>,
    packets_in_window: u32,
    operation_window_start_ms: Option<u64>,
    operation_counts: [u32; OPERATION_SLOTS],
}

impl TelemetryCollector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a packet handed to `NetEq::insert_packet`.
    pub fn on_packet_inserted(&mut self, sequence_number: u16, now_ms: u64) {
        let unwrapped = self.unwrapper.unwrap(sequence_number);
        self.derived.total_packets_received = self.derived.total_packets_received.wrapping_add(1);
        match self.highest_sequence {
            Some(highest) if unwrapped < highest => {
                self.derived.reordered_packets = self.derived.reordered_packets.wrapping_add(1);
                let distance = u16::try_from(highest - unwrapped).unwrap_or(u16::MAX);
                self.derived.max_reorder_distance = self.derived.max_reorder_distance.max(distance);
            }
            _ => self.highest_sequence = Some(unwrapped),
        }

        let start = *self.packet_window_start_ms.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(start);
        if elapsed >= RATE_WINDOW_MS {
            // Normalize to one second so a late window does not inflate it.
            self.derived.packets_per_sec =
                (u64::from(self.packets_in_window) * 1_000 / elapsed) as u32;
            self.packets_in_window = 0;
            self.packet_window_start_ms = Some(now_ms);
        }
        self.packets_in_window += 1;
    }

    /// Record the operation of one `GetAudio` call (its final operation).
    pub fn on_get_audio(&mut self, operation: Operation, now_ms: u64) {
        let start = *self.operation_window_start_ms.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(start);
        if elapsed >= RATE_WINDOW_MS {
            self.derived.operation_counters = self.rates(elapsed);
            self.operation_counts = [0; OPERATION_SLOTS];
            self.operation_window_start_ms = Some(now_ms);
        }
        self.operation_counts[operation_slot(operation)] += 1;
    }

    /// Forget the stream (after `flush_buffers`). Counters since the
    /// collector was created are kept, as in the 0.x JSON.
    pub fn on_flush(&mut self) {
        self.unwrapper.reset();
        self.highest_sequence = None;
    }

    fn rates(&self, elapsed_ms: u64) -> OperationCounters {
        let secs = elapsed_ms as f32 / 1000.0;
        let rate = |operations: &[Operation]| -> f32 {
            operations
                .iter()
                .map(|operation| self.operation_counts[operation_slot(*operation)])
                .sum::<u32>() as f32
                / secs
        };
        OperationCounters {
            normal_per_sec: rate(&[Operation::Normal]),
            expand_per_sec: rate(&[Operation::Expand]),
            accelerate_per_sec: rate(&[Operation::Accelerate]),
            fast_accelerate_per_sec: rate(&[Operation::FastAccelerate]),
            preemptive_expand_per_sec: rate(&[Operation::PreemptiveExpand]),
            merge_per_sec: rate(&[Operation::Merge]),
            comfort_noise_per_sec: rate(&[
                Operation::Rfc3389Cng,
                Operation::Rfc3389CngNoPacket,
                Operation::CodecInternalCng,
            ]),
            dtmf_per_sec: rate(&[Operation::Dtmf]),
            undefined_per_sec: rate(&[Operation::Undefined]),
        }
    }
}

/// Exhaustive so a new `Operation` variant fails to compile here.
fn operation_slot(operation: Operation) -> usize {
    match operation {
        Operation::Normal => 0,
        Operation::Merge => 1,
        Operation::Expand => 2,
        Operation::Accelerate => 3,
        Operation::FastAccelerate => 4,
        Operation::PreemptiveExpand => 5,
        Operation::Rfc3389Cng => 6,
        Operation::Rfc3389CngNoPacket => 7,
        Operation::CodecInternalCng => 8,
        Operation::Dtmf => 9,
        Operation::Undefined => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serde uses Rust field names verbatim (no renames in this module), so
    /// reading every path the consumers parse pins the JSON keys at compile
    /// time: dioxus-ui `neteq_chart.rs`, videocall-client `health_reporter.rs`
    /// and `neteq_audio_decoder.rs`.
    #[test]
    fn stats_fill_the_field_paths_consumers_read() {
        let network = NetEqNetworkStatistics {
            current_buffer_size_ms: 60,
            preferred_buffer_size_ms: 80,
            expand_rate: 4096,
            accelerate_rate: 8192,
            ..Default::default()
        };
        let lifetime = NetEqLifetimeStatistics {
            jitter_buffer_delay_ms: 1234,
            jitter_buffer_target_delay_ms: 5678,
            ..Default::default()
        };
        let mut collector = TelemetryCollector::new();
        for (i, seq) in [1u16, 3, 2].into_iter().enumerate() {
            collector.on_packet_inserted(seq, i as u64 * 20);
        }
        let stats = NetEqStats::from_parts(
            &network,
            &lifetime,
            &NetEqOperationsAndState::default(),
            3,
            &collector,
        );

        assert_eq!(stats.current_buffer_size_ms, 60);
        assert_eq!(stats.target_delay_ms, 80);
        assert_eq!(stats.packets_awaiting_decode, 3);
        assert_eq!(stats.packets_per_sec, 0);
        assert_eq!(stats.network.current_buffer_size_ms, 60);
        assert_eq!(stats.network.expand_rate, q14::from_per_mille(250.0));
        assert_eq!(stats.network.accelerate_rate, q14::from_per_mille(500.0));
        assert_eq!(stats.network.reordered_packets, 1);
        assert_eq!(stats.network.total_packets_received, 3);
        assert_eq!(stats.network.max_reorder_distance, 1);
        assert_eq!(stats.network.reorder_rate_permyriad, 3333);
        let counters = stats.network.operation_counters;
        let _all_counter_paths = [
            counters.normal_per_sec,
            counters.expand_per_sec,
            counters.accelerate_per_sec,
            counters.fast_accelerate_per_sec,
            counters.preemptive_expand_per_sec,
            counters.merge_per_sec,
            counters.comfort_noise_per_sec,
            counters.dtmf_per_sec,
            counters.undefined_per_sec,
        ];
        assert_eq!(stats.lifetime.jitter_buffer_delay_ms, 1234);
        assert_eq!(stats.lifetime.jitter_buffer_target_delay_ms, 5678);
        assert!(!stats.operations.next_packet_available);
    }

    #[test]
    fn reorder_counters_survive_sequence_wrap() {
        let mut collector = TelemetryCollector::new();
        for (i, seq) in [65_534u16, 65_535, 1, 0, 2].into_iter().enumerate() {
            collector.on_packet_inserted(seq, i as u64 * 20);
        }
        let derived = &collector.derived;
        assert_eq!(derived.total_packets_received, 5);
        assert_eq!(derived.reordered_packets, 1);
        assert_eq!(derived.max_reorder_distance, 1);
        assert_eq!(derived.reorder_rate_permyriad(), 2000);
    }

    #[test]
    fn packets_per_sec_counts_a_full_window() {
        let mut collector = TelemetryCollector::new();
        for i in 0..=50u16 {
            collector.on_packet_inserted(i, u64::from(i) * 20);
        }
        // 50 packets in [0, 1000) ms, reported when the 1000 ms packet lands.
        assert_eq!(collector.derived.packets_per_sec, 50);
    }

    #[test]
    fn operation_rates_cover_every_operation() {
        let mut collector = TelemetryCollector::new();
        let operations = [
            Operation::Normal,
            Operation::Merge,
            Operation::Expand,
            Operation::Accelerate,
            Operation::FastAccelerate,
            Operation::PreemptiveExpand,
            Operation::Rfc3389Cng,
            Operation::Rfc3389CngNoPacket,
            Operation::CodecInternalCng,
            Operation::Dtmf,
            Operation::Undefined,
        ];
        for (i, operation) in operations.iter().enumerate() {
            collector.on_get_audio(*operation, i as u64 * 10);
        }
        collector.on_get_audio(Operation::Normal, 1_000);
        let counters = collector.derived.operation_counters;
        assert_eq!(counters.normal_per_sec, 1.0);
        assert_eq!(counters.merge_per_sec, 1.0);
        assert_eq!(counters.expand_per_sec, 1.0);
        assert_eq!(counters.accelerate_per_sec, 1.0);
        assert_eq!(counters.fast_accelerate_per_sec, 1.0);
        assert_eq!(counters.preemptive_expand_per_sec, 1.0);
        assert_eq!(counters.comfort_noise_per_sec, 3.0);
        assert_eq!(counters.dtmf_per_sec, 1.0);
        assert_eq!(counters.undefined_per_sec, 1.0);
    }

    #[test]
    fn flush_restarts_reorder_tracking() {
        let mut collector = TelemetryCollector::new();
        collector.on_packet_inserted(500, 0);
        collector.on_flush();
        collector.on_packet_inserted(10, 20);
        assert_eq!(collector.derived.reordered_packets, 0);
    }

    #[test]
    fn q14_helpers_round_trip() {
        for (value, ratio, per_mille) in [
            (0u16, 0.0, 0.0f32),
            (4096, 0.25, 250.0),
            (8192, 0.5, 500.0),
            (16384, 1.0, 1000.0),
        ] {
            assert_eq!(q14::to_float(value), ratio);
            assert!((q14::to_per_mille(value) - per_mille).abs() < 1e-3);
            assert_eq!(q14::from_float(ratio), value);
            assert_eq!(q14::from_per_mille(per_mille), value);
        }
    }
}
