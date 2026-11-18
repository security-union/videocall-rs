// Port target: WebRTC modules/audio_coding/neteq/packet_arrival_history.{h,cc}
// at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd
// (tests: packet_arrival_history_unittest.cc).
// Copyright (c) 2023 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.
//
// Plan: `BTreeMap<i64, PacketArrival>` plus two `VecDeque` monotonic queues,
// matching `std::map` + `std::deque`. Unwrap with
// `crate::rtc_base::RtpTimestampUnwrapper`.

//! Arrival times of recent packets, for playout delay estimation.

use std::collections::{BTreeMap, VecDeque};

use crate::api::tick_timer::TickTimer;
use crate::rtc_base::RtpTimestampUnwrapper;

/// The virtual surface of C++ `PacketArrivalHistory` that `DecisionLogic`
/// uses. C++ tests override `GetDelayMs` / `GetMaxDelayMs` with gmock; here
/// a test fake implements this trait instead. `PacketArrivalHistory` is the
/// only production implementation.
pub trait PacketArrivalHistoryInterface: Send {
    /// C++ `Insert`.
    fn insert(&mut self, rtp_timestamp: u32, packet_length_samples: i32) -> bool;
    /// C++ `GetDelayMs` (virtual).
    fn get_delay_ms(&self, rtp_timestamp: u32) -> i32;
    /// C++ `GetMaxDelayMs` (virtual).
    fn get_max_delay_ms(&self) -> i32;
    /// C++ `IsNewestRtpTimestamp`.
    fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool;
    /// C++ `Reset`.
    fn reset(&mut self);
    /// C++ `set_sample_rate`.
    fn set_sample_rate(&mut self, sample_rate: i32);
    /// C++ `size`.
    fn size(&self) -> usize;
}

/// C++ `PacketArrivalHistory::PacketArrival`. Timestamps are in samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketArrival {
    rtp_timestamp: i64,
    arrival_timestamp: i64,
    length_samples: i32,
}

impl PacketArrival {
    /// C++ `operator<=`.
    fn le(&self, other: &PacketArrival) -> bool {
        self.arrival_timestamp - self.rtp_timestamp <= other.arrival_timestamp - other.rtp_timestamp
    }

    /// C++ `operator>=`.
    fn ge(&self, other: &PacketArrival) -> bool {
        self.arrival_timestamp - self.rtp_timestamp >= other.arrival_timestamp - other.rtp_timestamp
    }

    /// True if `other` lies entirely within this packet.
    fn contains(&self, other: &PacketArrival) -> bool {
        self.rtp_timestamp <= other.rtp_timestamp
            && self.rtp_timestamp + self.length_samples as i64
                >= other.rtp_timestamp + other.length_samples as i64
    }
}

/// C++ `PacketArrivalHistory`: timing of previously received packets. The
/// history has a fixed window beyond which old data is pruned.
#[derive(Debug)]
pub struct PacketArrivalHistory {
    tick_timer: TickTimer,
    window_size_ms: i32,
    sample_rate_khz: i32,
    timestamp_unwrapper: RtpTimestampUnwrapper,
    /// Packet history ordered by rtp timestamp.
    history: BTreeMap<i64, PacketArrival>,
    /// Min packet arrivals in `history`, ascending. Reordered packets are
    /// excluded.
    min_packet_arrivals: VecDeque<PacketArrival>,
    /// Max packet arrivals in `history`, descending. Reordered packets are
    /// excluded.
    max_packet_arrivals: VecDeque<PacketArrival>,
}

impl PacketArrivalHistory {
    /// History covering `window_size_ms` of arrivals.
    pub fn new(tick_timer: TickTimer, window_size_ms: i32) -> Self {
        Self {
            tick_timer,
            window_size_ms,
            sample_rate_khz: 0,
            timestamp_unwrapper: RtpTimestampUnwrapper::new(),
            history: BTreeMap::new(),
            min_packet_arrivals: VecDeque::new(),
            max_packet_arrivals: VecDeque::new(),
        }
    }

    /// Records a packet arriving now. Returns false if the timestamp is too
    /// old or already exists (or is fully overlapped).
    pub fn insert(&mut self, rtp_timestamp: u32, packet_length_samples: i32) -> bool {
        let arrival_timestamp = self.now_in_samples();
        let packet = PacketArrival {
            rtp_timestamp: self.timestamp_unwrapper.unwrap(rtp_timestamp),
            arrival_timestamp,
            length_samples: packet_length_samples,
        };
        if self.is_obsolete(&packet) {
            return false;
        }
        if self.contains(&packet) {
            return false;
        }
        // C++ `std::map::emplace`: does nothing if the key exists.
        self.history.entry(packet.rtp_timestamp).or_insert(packet);
        if self.history.last_key_value().map(|(_, newest)| *newest) != Some(packet) {
            // Packet was reordered.
            return true;
        }
        // Remove old packets.
        while let Some((&oldest_key, &oldest)) = self.history.first_key_value() {
            if !self.is_obsolete(&oldest) {
                break;
            }
            if self.min_packet_arrivals.front() == Some(&oldest) {
                self.min_packet_arrivals.pop_front();
            }
            if self.max_packet_arrivals.front() == Some(&oldest) {
                self.max_packet_arrivals.pop_front();
            }
            self.history.remove(&oldest_key);
        }
        // Ensure ordering constraints.
        while self
            .min_packet_arrivals
            .back()
            .is_some_and(|back| packet.le(back))
        {
            self.min_packet_arrivals.pop_back();
        }
        while self
            .max_packet_arrivals
            .back()
            .is_some_and(|back| packet.ge(back))
        {
            self.max_packet_arrivals.pop_back();
        }
        self.min_packet_arrivals.push_back(packet);
        self.max_packet_arrivals.push_back(packet);
        true
    }

    /// The delay for `rtp_timestamp` now: `(now - p.arrival_timestamp) -
    /// (rtp_timestamp - p.rtp_timestamp)` for the packet `p` in the history
    /// that maximizes it.
    pub fn get_delay_ms(&self, rtp_timestamp: u32) -> i32 {
        let unwrapped_rtp_timestamp = self.timestamp_unwrapper.peek_unwrap(rtp_timestamp);
        let current_timestamp = self.now_in_samples();
        let packet = PacketArrival {
            rtp_timestamp: unwrapped_rtp_timestamp,
            arrival_timestamp: current_timestamp,
            length_samples: 0,
        };
        self.get_packet_arrival_delay_ms(&packet)
    }

    /// Maximum packet arrival delay in the history, excluding reordered
    /// packets.
    pub fn get_max_delay_ms(&self) -> i32 {
        match self.max_packet_arrivals.front() {
            None => 0,
            Some(front) => self.get_packet_arrival_delay_ms(front),
        }
    }

    /// True if `rtp_timestamp` is the newest timestamp in the history (or the
    /// history is empty).
    pub fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool {
        let Some((_, newest)) = self.history.last_key_value() else {
            return true;
        };
        let unwrapped_rtp_timestamp = self.timestamp_unwrapper.peek_unwrap(rtp_timestamp);
        unwrapped_rtp_timestamp == newest.rtp_timestamp
    }

    /// Clears the history.
    pub fn reset(&mut self) {
        self.history.clear();
        self.min_packet_arrivals.clear();
        self.max_packet_arrivals.clear();
        self.timestamp_unwrapper.reset();
    }

    /// Sets the sample rate used to convert RTP timestamps to ms.
    pub fn set_sample_rate(&mut self, sample_rate: i32) {
        self.sample_rate_khz = sample_rate / 1000;
    }

    /// Number of packets in the history.
    pub fn size(&self) -> usize {
        self.history.len()
    }

    /// C++ `ticks() * ms_per_tick() * sample_rate_khz_`, computed in
    /// `uint64_t` and stored in `int64_t`.
    fn now_in_samples(&self) -> i64 {
        self.tick_timer
            .ticks()
            .wrapping_mul(self.tick_timer.ms_per_tick() as u64)
            .wrapping_mul(self.sample_rate_khz as u64) as i64
    }

    fn get_packet_arrival_delay_ms(&self, packet_arrival: &PacketArrival) -> i32 {
        let Some(min) = self.min_packet_arrivals.front() else {
            return 0;
        };
        debug_assert_ne!(self.sample_rate_khz, 0);
        let sample_rate_khz = self.sample_rate_khz as i64;
        // Timestamps are first converted to millis for bit-exactness.
        let delay = packet_arrival.arrival_timestamp / sample_rate_khz
            - min.arrival_timestamp / sample_rate_khz
            - (packet_arrival.rtp_timestamp / sample_rate_khz
                - min.rtp_timestamp / sample_rate_khz);
        // C++ `std::max<int>(int64 expression, 0)`: truncate, then clamp.
        std::cmp::max(delay as i32, 0)
    }

    /// Checks if the packet is older than the window size.
    fn is_obsolete(&self, packet_arrival: &PacketArrival) -> bool {
        let Some((_, newest)) = self.history.last_key_value() else {
            return false;
        };
        let window_samples = (self.window_size_ms * self.sample_rate_khz) as i64;
        packet_arrival.rtp_timestamp + window_samples < newest.rtp_timestamp
    }

    /// Checks if the packet exists or fully overlaps with a packet in the
    /// history.
    fn contains(&self, packet_arrival: &PacketArrival) -> bool {
        // C++ `--history_.upper_bound(rtp_timestamp)`: the last entry with a
        // key not greater than the packet's.
        match self
            .history
            .range(..=packet_arrival.rtp_timestamp)
            .next_back()
        {
            None => false,
            Some((_, entry)) => entry.contains(packet_arrival),
        }
    }
}

impl PacketArrivalHistoryInterface for PacketArrivalHistory {
    fn insert(&mut self, rtp_timestamp: u32, packet_length_samples: i32) -> bool {
        PacketArrivalHistory::insert(self, rtp_timestamp, packet_length_samples)
    }
    fn get_delay_ms(&self, rtp_timestamp: u32) -> i32 {
        PacketArrivalHistory::get_delay_ms(self, rtp_timestamp)
    }
    fn get_max_delay_ms(&self) -> i32 {
        PacketArrivalHistory::get_max_delay_ms(self)
    }
    fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool {
        PacketArrivalHistory::is_newest_rtp_timestamp(self, rtp_timestamp)
    }
    fn reset(&mut self) {
        PacketArrivalHistory::reset(self)
    }
    fn set_sample_rate(&mut self, sample_rate: i32) {
        PacketArrivalHistory::set_sample_rate(self, sample_rate)
    }
    fn size(&self) -> usize {
        PacketArrivalHistory::size(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: i32 = 8000;
    const FS_KHZ: i32 = FS / 1000;
    const FRAME_SIZE_MS: i32 = 20;
    const FRAME_SIZE_SAMPLES: i32 = FRAME_SIZE_MS * FS_KHZ;
    const WINDOW_SIZE_MS: i32 = 1000;

    /// C++ `PacketArrivalHistoryTest` fixture.
    struct Fixture {
        tick_timer: TickTimer,
        history: PacketArrivalHistory,
        timestamp: u32,
    }

    impl Fixture {
        fn new() -> Self {
            let tick_timer = TickTimer::new();
            let mut history = PacketArrivalHistory::new(tick_timer.clone(), WINDOW_SIZE_MS);
            history.set_sample_rate(FS);
            Self {
                tick_timer,
                history,
                timestamp: 0x12345678,
            }
        }

        fn increment_time(&self, delta_ms: i32) {
            self.tick_timer
                .increment_by((delta_ms / self.tick_timer.ms_per_tick()) as u64);
        }

        fn insert_packet_and_get_delay(&mut self, timestamp_delta_ms: i32) -> i32 {
            let timestamp = self
                .timestamp
                .wrapping_add((timestamp_delta_ms * FS_KHZ) as u32);
            if timestamp_delta_ms > 0 {
                self.timestamp = timestamp;
            }
            assert!(self.history.insert(timestamp, FRAME_SIZE_SAMPLES));
            assert_eq!(
                self.history.is_newest_rtp_timestamp(timestamp),
                timestamp_delta_ms >= 0
            );
            self.history.get_delay_ms(timestamp)
        }
    }

    #[test]
    fn relative_arrival_delay() {
        let mut f = Fixture::new();
        // Insert first packet.
        assert_eq!(f.insert_packet_and_get_delay(0), 0);

        f.increment_time(FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 0);

        f.increment_time(2 * FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 20);

        // Reordered packet.
        assert_eq!(f.insert_packet_and_get_delay(-3 * FRAME_SIZE_MS), 80);

        f.increment_time(2 * FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 40);

        // Move reference packet forward.
        assert_eq!(f.insert_packet_and_get_delay(4 * FRAME_SIZE_MS), 0);

        f.increment_time(2 * FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 20);

        // Earlier packet is now more delayed due to the new reference packet.
        assert_eq!(f.history.get_max_delay_ms(), 80);
    }

    #[test]
    fn reordered_packets() {
        let mut f = Fixture::new();
        // Insert first packet.
        assert_eq!(f.insert_packet_and_get_delay(0), 0);

        // Insert reordered packet.
        assert_eq!(f.insert_packet_and_get_delay(-80), 80);

        // Insert another reordered packet.
        assert_eq!(f.insert_packet_and_get_delay(-FRAME_SIZE_MS), 20);

        // Insert the next packet in order and verify that the relative delay
        // is estimated based on the first inserted packet.
        f.increment_time(4 * FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 60);

        assert_eq!(f.history.get_max_delay_ms(), 60);
    }

    #[test]
    fn max_history_size() {
        let mut f = Fixture::new();
        assert_eq!(f.insert_packet_and_get_delay(0), 0);

        f.increment_time(2 * FRAME_SIZE_MS);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 20);
        assert_eq!(f.history.get_max_delay_ms(), 20);

        // Insert next packet with a timestamp difference larger than maximum
        // history size. This removes the previously inserted packet from the
        // history.
        f.increment_time(WINDOW_SIZE_MS + FRAME_SIZE_MS);
        assert_eq!(
            f.insert_packet_and_get_delay(FRAME_SIZE_MS + WINDOW_SIZE_MS),
            0
        );
        assert_eq!(f.history.get_max_delay_ms(), 0);
    }

    #[test]
    fn timestamp_wraparound() {
        let mut f = Fixture::new();
        f.timestamp = u32::MAX;
        assert_eq!(f.insert_packet_and_get_delay(0), 0);

        f.increment_time(2 * FRAME_SIZE_MS);
        // Insert timestamp that will wrap around.
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), FRAME_SIZE_MS);

        // Insert reordered packet before the wraparound.
        assert_eq!(
            f.insert_packet_and_get_delay(-2 * FRAME_SIZE_MS),
            3 * FRAME_SIZE_MS
        );

        // Insert another in-order packet after the wraparound.
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 0);

        assert_eq!(f.history.get_max_delay_ms(), FRAME_SIZE_MS);
    }

    #[test]
    fn timestamp_wraparound_backwards() {
        let mut f = Fixture::new();
        f.timestamp = 0;
        assert_eq!(f.insert_packet_and_get_delay(0), 0);

        f.increment_time(2 * FRAME_SIZE_MS);
        // Insert timestamp that will wrap around.
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), FRAME_SIZE_MS);

        // Insert reordered packet before the wraparound.
        assert_eq!(
            f.insert_packet_and_get_delay(-2 * FRAME_SIZE_MS),
            3 * FRAME_SIZE_MS
        );

        // Insert another in-order packet after the wraparound.
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 0);

        assert_eq!(f.history.get_max_delay_ms(), FRAME_SIZE_MS);
    }

    #[test]
    fn old_packet_should_not_be_inserted() {
        let mut f = Fixture::new();
        // Insert first packet as reference.
        assert_eq!(f.insert_packet_and_get_delay(0), 0);
        // Insert packet with timestamp older than the window size compared to
        // the first packet.
        let old = f
            .timestamp
            .wrapping_sub((WINDOW_SIZE_MS * FS_KHZ) as u32)
            .wrapping_sub(1);
        assert!(!f.history.insert(old, FRAME_SIZE_SAMPLES));
    }

    #[test]
    fn duplicate_packet_should_not_be_inserted() {
        let mut f = Fixture::new();
        // Insert first packet as reference.
        let first_timestamp = f.timestamp;
        assert_eq!(f.insert_packet_and_get_delay(0), 0);
        assert_eq!(f.insert_packet_and_get_delay(FRAME_SIZE_MS), 0);
        // Same timestamp as the first packet.
        assert!(!f.history.insert(first_timestamp, FRAME_SIZE_SAMPLES));
    }

    #[test]
    fn overlapping_packet_should_not_be_inserted() {
        let mut f = Fixture::new();
        // Insert first packet as reference.
        assert_eq!(f.insert_packet_and_get_delay(0), 0);
        // 10 ms overlap with the previous packet.
        assert!(!f.history.insert(
            f.timestamp + (FRAME_SIZE_SAMPLES / 2) as u32,
            FRAME_SIZE_SAMPLES / 2
        ));
    }

    // Not in C++: `reset` clears the history and the unwrapper.
    #[test]
    fn reset_clears_history() {
        let mut f = Fixture::new();
        assert_eq!(f.insert_packet_and_get_delay(0), 0);
        assert_eq!(f.history.size(), 1);
        f.history.reset();
        assert_eq!(f.history.size(), 0);
        assert_eq!(f.history.get_max_delay_ms(), 0);
        assert!(f.history.is_newest_rtp_timestamp(12345));
    }
}
