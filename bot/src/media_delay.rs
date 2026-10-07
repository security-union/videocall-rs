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
 */

//! One-way media delay from the sender's embedded wall-clock timestamp.
//!
//! `owd` is `arrival − MediaPacket.timestamp` and needs synchronized clocks to be
//! absolute. `excess` is `owd` minus the stream's lowest `owd` over a sliding
//! window, so a constant clock offset cancels. Senders that stamp a media time
//! instead of wall-clock ms (browser video) fail the plausibility check.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// A sender timestamp more than this far from the receiver clock is not wall-clock ms.
pub const MAX_PLAUSIBLE_DELAY_MS: f64 = 60_000.0;

/// Window over which the per-stream best (lowest) delay is tracked.
pub const DEFAULT_FLOOR_WINDOW: Duration = Duration::from_secs(30);

const FLOOR_BUCKETS: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DelayKind {
    Audio,
    Video,
}

impl DelayKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DelayKind::Audio => "audio",
            DelayKind::Video => "video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DelaySample {
    /// Arrival minus sender timestamp, ms (absolute only with synchronized clocks).
    pub owd_ms: f64,
    /// `owd_ms` above the stream's lowest `owd_ms` in the window, ms (≥ 0).
    pub excess_ms: f64,
}

/// `now_ms − sender_ts_ms` when the sender timestamp looks like wall-clock ms.
pub fn plausible_owd(now_ms: f64, sender_ts_ms: f64) -> Option<f64> {
    if !sender_ts_ms.is_finite() || sender_ts_ms <= 0.0 || !now_ms.is_finite() {
        return None;
    }
    let owd = now_ms - sender_ts_ms;
    (owd.abs() <= MAX_PLAUSIBLE_DELAY_MS).then_some(owd)
}

#[derive(Debug)]
struct WindowedMin {
    bucket_width: Duration,
    window: Duration,
    buckets: VecDeque<(Instant, f64)>,
}

impl WindowedMin {
    fn new(window: Duration) -> Self {
        Self {
            bucket_width: window / FLOOR_BUCKETS,
            window,
            buckets: VecDeque::new(),
        }
    }

    /// Record `v` at `now` and return the minimum over the window, `v` included.
    fn observe(&mut self, now: Instant, v: f64) -> f64 {
        while let Some(&(start, _)) = self.buckets.front() {
            if now.duration_since(start) >= self.window {
                self.buckets.pop_front();
            } else {
                break;
            }
        }
        match self.buckets.back_mut() {
            Some((start, min)) if now.duration_since(*start) < self.bucket_width => {
                if v < *min {
                    *min = v;
                }
            }
            _ => self.buckets.push_back((now, v)),
        }
        self.buckets
            .iter()
            .map(|&(_, m)| m)
            .fold(f64::INFINITY, f64::min)
    }

    fn last_update(&self) -> Option<Instant> {
        self.buckets.back().map(|&(start, _)| start)
    }
}

/// Per-stream delay tracker. A stream is (sender session id, media kind).
#[derive(Debug)]
pub struct MediaDelayTracker {
    window: Duration,
    floors: HashMap<(u64, DelayKind), WindowedMin>,
    implausible: u64,
}

impl Default for MediaDelayTracker {
    fn default() -> Self {
        Self::with_window(DEFAULT_FLOOR_WINDOW)
    }
}

impl MediaDelayTracker {
    pub fn with_window(window: Duration) -> Self {
        Self {
            window,
            floors: HashMap::new(),
            implausible: 0,
        }
    }

    /// Observe one packet. Returns `None` (and counts it) when the sender
    /// timestamp is not plausible wall-clock ms.
    pub fn observe(
        &mut self,
        stream: u64,
        kind: DelayKind,
        now_ms: f64,
        sender_ts_ms: f64,
        now: Instant,
    ) -> Option<DelaySample> {
        let Some(owd_ms) = plausible_owd(now_ms, sender_ts_ms) else {
            self.implausible += 1;
            return None;
        };
        let window = self.window;
        let floor = self
            .floors
            .entry((stream, kind))
            .or_insert_with(|| WindowedMin::new(window))
            .observe(now, owd_ms);
        Some(DelaySample {
            owd_ms,
            excess_ms: (owd_ms - floor).max(0.0),
        })
    }

    pub fn implausible(&self) -> u64 {
        self.implausible
    }

    /// Forget streams not seen for a whole window (bounds memory across session churn).
    pub fn evict_idle(&mut self, now: Instant) {
        let window = self.window;
        self.floors.retain(|_, f| {
            f.last_update()
                .is_some_and(|t| now.duration_since(t) < window)
        });
    }

    pub fn tracked_streams(&self) -> usize {
        self.floors.len()
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DelayWindowStats {
    pub count: u64,
    pub owd_sum_ms: f64,
    pub owd_max_ms: f64,
    pub excess_max_ms: f64,
}

impl DelayWindowStats {
    pub fn record(&mut self, s: DelaySample) {
        if self.count == 0 {
            self.owd_max_ms = s.owd_ms;
        } else {
            self.owd_max_ms = self.owd_max_ms.max(s.owd_ms);
        }
        self.count += 1;
        self.owd_sum_ms += s.owd_ms;
        self.excess_max_ms = self.excess_max_ms.max(s.excess_ms);
    }

    pub fn owd_mean_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.owd_sum_ms / self.count as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: f64 = 1_759_200_000_000.0;

    #[test]
    fn wall_clock_timestamps_give_the_one_way_delay() {
        assert_eq!(plausible_owd(NOW_MS, NOW_MS - 120.0), Some(120.0));
        assert_eq!(
            plausible_owd(NOW_MS, NOW_MS + 5.0),
            Some(-5.0),
            "clock skew kept visible"
        );
    }

    #[test]
    fn media_time_and_missing_timestamps_are_rejected() {
        assert_eq!(plausible_owd(NOW_MS, 33_366.0), None);
        assert_eq!(plausible_owd(NOW_MS, 0.0), None);
        assert_eq!(plausible_owd(NOW_MS, f64::NAN), None);
    }

    #[test]
    fn excess_is_delay_above_the_streams_best_in_the_window() {
        let mut t = MediaDelayTracker::default();
        let start = Instant::now();
        let s1 = t
            .observe(1, DelayKind::Audio, NOW_MS, NOW_MS - 80.0, start)
            .unwrap();
        assert_eq!(s1.excess_ms, 0.0);
        let s2 = t
            .observe(
                1,
                DelayKind::Audio,
                NOW_MS,
                NOW_MS - 230.0,
                start + Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(s2.owd_ms, 230.0);
        assert_eq!(s2.excess_ms, 150.0);
    }

    #[test]
    fn a_constant_clock_offset_cancels_in_excess() {
        let mut t = MediaDelayTracker::default();
        let start = Instant::now();
        let skew = 500.0;
        t.observe(1, DelayKind::Video, NOW_MS, NOW_MS - 80.0 - skew, start);
        let s = t
            .observe(
                1,
                DelayKind::Video,
                NOW_MS,
                NOW_MS - 120.0 - skew,
                start + Duration::from_millis(500),
            )
            .unwrap();
        assert_eq!(s.excess_ms, 40.0);
    }

    #[test]
    fn the_floor_expires_after_the_window() {
        let window = Duration::from_secs(6);
        let mut t = MediaDelayTracker::with_window(window);
        let start = Instant::now();
        t.observe(1, DelayKind::Audio, NOW_MS, NOW_MS - 50.0, start);
        let later = t
            .observe(
                1,
                DelayKind::Audio,
                NOW_MS,
                NOW_MS - 200.0,
                start + Duration::from_secs(7),
            )
            .unwrap();
        assert_eq!(later.excess_ms, 0.0, "the old 50 ms floor has aged out");
    }

    #[test]
    fn streams_and_kinds_keep_separate_floors() {
        let mut t = MediaDelayTracker::default();
        let now = Instant::now();
        t.observe(1, DelayKind::Audio, NOW_MS, NOW_MS - 50.0, now);
        let other = t
            .observe(2, DelayKind::Audio, NOW_MS, NOW_MS - 300.0, now)
            .unwrap();
        let video = t
            .observe(1, DelayKind::Video, NOW_MS, NOW_MS - 300.0, now)
            .unwrap();
        assert_eq!(other.excess_ms, 0.0);
        assert_eq!(video.excess_ms, 0.0);
    }

    #[test]
    fn implausible_packets_are_counted_and_idle_streams_evicted() {
        let window = Duration::from_secs(6);
        let mut t = MediaDelayTracker::with_window(window);
        let start = Instant::now();
        assert!(t.observe(1, DelayKind::Video, NOW_MS, 1.0, start).is_none());
        assert_eq!(t.implausible(), 1);
        t.observe(1, DelayKind::Audio, NOW_MS, NOW_MS - 10.0, start);
        assert_eq!(t.tracked_streams(), 1);
        t.evict_idle(start + Duration::from_secs(10));
        assert_eq!(t.tracked_streams(), 0);
    }

    #[test]
    fn window_stats_track_mean_and_max() {
        let mut w = DelayWindowStats::default();
        w.record(DelaySample {
            owd_ms: 100.0,
            excess_ms: 0.0,
        });
        w.record(DelaySample {
            owd_ms: 300.0,
            excess_ms: 200.0,
        });
        assert_eq!(w.owd_mean_ms(), 200.0);
        assert_eq!(w.owd_max_ms, 300.0);
        assert_eq!(w.excess_max_ms, 200.0);
    }
}
