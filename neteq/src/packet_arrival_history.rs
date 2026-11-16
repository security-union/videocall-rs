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

//! Port of libwebrtc `modules/audio_coding/neteq/packet_arrival_history.{h,cc}`
//! (commit aeb9b3750c30bd1189448138b6431b73116b1f56).
//!
//! Arrival times come from [`crate::clock`] in milliseconds instead of
//! libwebrtc's 10 ms `TickTimer` ticks (a recorded deviation: finer, not
//! coarser).

use std::collections::{BTreeMap, VecDeque};
use web_time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PacketArrival {
    rtp_timestamp: i64,
    arrival_timestamp: i64,
    length_samples: i64,
}

impl PacketArrival {
    fn key(&self) -> i64 {
        self.arrival_timestamp - self.rtp_timestamp
    }
    // packet_arrival_history.h PacketArrival::contains
    fn contains(&self, other: &PacketArrival) -> bool {
        self.rtp_timestamp <= other.rtp_timestamp
            && self.rtp_timestamp + self.length_samples
                >= other.rtp_timestamp + other.length_samples
    }
}

/// Unwraps 32-bit RTP timestamps into a monotonic 64-bit domain
/// (libwebrtc `RtpTimestampUnwrapper`).
#[derive(Debug, Default, Clone)]
struct TimestampUnwrapper {
    last: Option<i64>,
}

impl TimestampUnwrapper {
    fn peek(&self, ts: u32) -> i64 {
        match self.last {
            None => i64::from(ts),
            Some(last) => last + i64::from(ts.wrapping_sub(last as u32) as i32),
        }
    }
    fn unwrap(&mut self, ts: u32) -> i64 {
        let v = self.peek(ts);
        self.last = Some(v);
        v
    }
}

/// Timing of recently received packets over a window of RTP time.
#[derive(Debug)]
pub struct PacketArrivalHistory {
    window_size_ms: i64,
    sample_rate_khz: i64,
    base: Instant,
    unwrapper: TimestampUnwrapper,
    history: BTreeMap<i64, PacketArrival>,
    min_packet_arrivals: VecDeque<PacketArrival>,
    max_packet_arrivals: VecDeque<PacketArrival>,
}

impl PacketArrivalHistory {
    /// `window_size_ms`: libwebrtc uses `kPacketHistorySizeMs` = 2000
    /// (decision_logic.cc).
    pub fn new(window_size_ms: u32, sample_rate_hz: u32) -> Self {
        Self {
            window_size_ms: i64::from(window_size_ms),
            sample_rate_khz: i64::from((sample_rate_hz / 1000).max(1)),
            base: crate::clock::now(),
            unwrapper: TimestampUnwrapper::default(),
            history: BTreeMap::new(),
            min_packet_arrivals: VecDeque::new(),
            max_packet_arrivals: VecDeque::new(),
        }
    }

    fn now_samples(&self) -> i64 {
        let ms = crate::clock::now()
            .saturating_duration_since(self.base)
            .as_millis() as i64;
        ms * self.sample_rate_khz
    }

    /// packet_arrival_history.cc `PacketArrivalHistory::Insert`. Returns
    /// false if the timestamp is too old or already covered.
    pub fn insert(&mut self, rtp_timestamp: u32, packet_length_samples: u32) -> bool {
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.peek(rtp_timestamp),
            arrival_timestamp: self.now_samples(),
            length_samples: i64::from(packet_length_samples),
        };
        if self.is_obsolete(&packet) || self.contains(&packet) {
            return false;
        }
        self.unwrapper.unwrap(rtp_timestamp);
        self.history.insert(packet.rtp_timestamp, packet);
        if self.history.values().next_back() != Some(&packet) {
            return true; // reordered: not used for min/max tracking
        }
        while let Some((&k, first)) = self.history.iter().next() {
            if !self.is_obsolete(first) {
                break;
            }
            let first = *first;
            if self.min_packet_arrivals.front() == Some(&first) {
                self.min_packet_arrivals.pop_front();
            }
            if self.max_packet_arrivals.front() == Some(&first) {
                self.max_packet_arrivals.pop_front();
            }
            self.history.remove(&k);
        }
        while self
            .min_packet_arrivals
            .back()
            .is_some_and(|b| packet.key() <= b.key())
        {
            self.min_packet_arrivals.pop_back();
        }
        while self
            .max_packet_arrivals
            .back()
            .is_some_and(|b| packet.key() >= b.key())
        {
            self.max_packet_arrivals.pop_back();
        }
        self.min_packet_arrivals.push_back(packet);
        self.max_packet_arrivals.push_back(packet);
        true
    }

    /// packet_arrival_history.cc `PacketArrivalHistory::Reset`.
    pub fn reset(&mut self) {
        self.history.clear();
        self.min_packet_arrivals.clear();
        self.max_packet_arrivals.clear();
        self.unwrapper = TimestampUnwrapper::default();
    }

    /// packet_arrival_history.cc `GetDelayMs`: how late `rtp_timestamp` is
    /// now, relative to the earliest-arriving packet in the window.
    pub fn get_delay_ms(&self, rtp_timestamp: u32) -> u32 {
        let packet = PacketArrival {
            rtp_timestamp: self.unwrapper.peek(rtp_timestamp),
            arrival_timestamp: self.now_samples(),
            length_samples: 0,
        };
        self.packet_arrival_delay_ms(&packet)
    }

    /// packet_arrival_history.cc `GetMaxDelayMs`.
    pub fn get_max_delay_ms(&self) -> u32 {
        self.max_packet_arrivals
            .front()
            .map_or(0, |p| self.packet_arrival_delay_ms(p))
    }

    /// packet_arrival_history.cc `IsNewestRtpTimestamp`.
    pub fn is_newest_rtp_timestamp(&self, rtp_timestamp: u32) -> bool {
        match self.history.values().next_back() {
            None => true,
            Some(p) => self.unwrapper.peek(rtp_timestamp) == p.rtp_timestamp,
        }
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    // packet_arrival_history.cc GetPacketArrivalDelayMs
    fn packet_arrival_delay_ms(&self, p: &PacketArrival) -> u32 {
        let Some(min) = self.min_packet_arrivals.front() else {
            return 0;
        };
        let k = self.sample_rate_khz;
        let d = p.arrival_timestamp / k
            - min.arrival_timestamp / k
            - (p.rtp_timestamp / k - min.rtp_timestamp / k);
        d.clamp(0, i64::from(u32::MAX)) as u32
    }

    // packet_arrival_history.cc IsObsolete
    fn is_obsolete(&self, p: &PacketArrival) -> bool {
        match self.history.values().next_back() {
            None => false,
            Some(newest) => {
                p.rtp_timestamp + self.window_size_ms * self.sample_rate_khz < newest.rtp_timestamp
            }
        }
    }

    // packet_arrival_history.cc Contains
    fn contains(&self, p: &PacketArrival) -> bool {
        self.history
            .range(..=p.rtp_timestamp)
            .next_back()
            .is_some_and(|(_, q)| q.contains(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn delay_is_relative_to_the_earliest_packet() {
        crate::clock::freeze();
        let mut h = PacketArrivalHistory::new(2000, 48_000);
        assert!(h.insert(0, 960));
        crate::clock::advance(Duration::from_millis(20));
        assert!(h.insert(960, 960));
        crate::clock::advance(Duration::from_millis(50)); // 30 ms late
        assert!(h.insert(1920, 960));
        assert_eq!(h.get_delay_ms(1920), 30);
        assert_eq!(h.get_max_delay_ms(), 30);
        // Duplicates are rejected.
        assert!(!h.insert(960, 960));
    }

    #[test]
    fn wraps_rtp_timestamps() {
        crate::clock::freeze();
        let mut h = PacketArrivalHistory::new(2000, 48_000);
        assert!(h.insert(u32::MAX - 959, 960));
        crate::clock::advance(Duration::from_millis(20));
        assert!(h.insert(0, 960));
        assert_eq!(h.get_delay_ms(0), 0);
        assert!(h.is_newest_rtp_timestamp(0));
    }
}
