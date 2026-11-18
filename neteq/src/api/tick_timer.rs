// Port of WebRTC api/neteq/tick_timer.{h,cc} and
// api/neteq/tick_timer_unittest.cc at rev 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2016 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The NetEq time base: one tick per `GetAudio` call (10 ms by default).
//!
//! In C++ every component keeps a `const TickTimer*`. Here [`TickTimer`] is a
//! cheap cloneable handle onto one shared counter, so `PacketBuffer`,
//! `PacketArrivalHistory`, `UnderrunOptimizer`, `DecisionLogic` and `NetEqImpl`
//! can each hold a clone and observe the same ticks. Cloning does **not**
//! create a new timer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct TickTimerInner {
    ticks: AtomicU64,
    ms_per_tick: i32,
}

/// Shared tick counter. One tick corresponds to `ms_per_tick` milliseconds
/// (10 by default). Ticks wrap around at `u64::MAX`, like the C++ `uint64_t`.
#[derive(Debug, Clone)]
pub struct TickTimer(Arc<TickTimerInner>);

impl Default for TickTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl TickTimer {
    /// A timer with 10 ms per tick.
    pub fn new() -> Self {
        Self::with_ms_per_tick(10)
    }

    /// A timer with a custom tick length. Panics in debug builds if
    /// `ms_per_tick <= 0`, like `RTC_DCHECK_GT`.
    pub fn with_ms_per_tick(ms_per_tick: i32) -> Self {
        debug_assert!(ms_per_tick > 0);
        Self(Arc::new(TickTimerInner {
            ticks: AtomicU64::new(0),
            ms_per_tick,
        }))
    }

    /// Advances the counter by one tick.
    pub fn increment(&self) {
        self.increment_by(1);
    }

    /// Advances the counter by `x` ticks (wrapping). Mainly for tests.
    pub fn increment_by(&self, x: u64) {
        // Atomic fetch_add wraps on overflow, matching C++ `ticks_ += x`.
        // Relaxed is enough: NetEq is single-threaded; the atomic only exists
        // so the handle is `Send + Sync`.
        self.0.ticks.fetch_add(x, Ordering::Relaxed);
    }

    /// Current tick count.
    pub fn ticks(&self) -> u64 {
        self.0.ticks.load(Ordering::Relaxed)
    }

    /// Milliseconds per tick.
    pub fn ms_per_tick(&self) -> i32 {
        self.0.ms_per_tick
    }

    /// Returns a stopwatch started at the current tick (C++ `GetNewStopwatch`).
    pub fn get_new_stopwatch(&self) -> Stopwatch {
        Stopwatch::new(self)
    }

    /// Returns a countdown of `ticks_to_count` ticks starting now
    /// (C++ `GetNewCountdown`).
    pub fn get_new_countdown(&self, ticks_to_count: u64) -> Countdown {
        Countdown::new(self, ticks_to_count)
    }
}

/// Measures time elapsed since it was started, in ticks of its [`TickTimer`].
#[derive(Debug, Clone)]
pub struct Stopwatch {
    ticktimer: TickTimer,
    starttick: u64,
}

impl Stopwatch {
    /// Starts a stopwatch at the timer's current tick.
    pub fn new(ticktimer: &TickTimer) -> Self {
        Self {
            ticktimer: ticktimer.clone(),
            starttick: ticktimer.ticks(),
        }
    }

    /// Ticks elapsed since start (wrapping, like the C++ unsigned subtraction).
    pub fn elapsed_ticks(&self) -> u64 {
        self.ticktimer.ticks().wrapping_sub(self.starttick)
    }

    /// Milliseconds elapsed since start, saturating at `u64::MAX`.
    pub fn elapsed_ms(&self) -> u64 {
        let elapsed_ticks = self.elapsed_ticks();
        let ms_per_tick = self.ticktimer.ms_per_tick() as u64;
        if elapsed_ticks < u64::MAX / ms_per_tick {
            elapsed_ticks * ms_per_tick
        } else {
            u64::MAX
        }
    }
}

/// Counts down from a start value with each tick until zero is reached.
#[derive(Debug, Clone)]
pub struct Countdown {
    stopwatch: Stopwatch,
    ticks_to_count: u64,
}

impl Countdown {
    /// Starts a countdown of `ticks_to_count` ticks.
    pub fn new(ticktimer: &TickTimer, ticks_to_count: u64) -> Self {
        Self {
            stopwatch: ticktimer.get_new_stopwatch(),
            ticks_to_count,
        }
    }

    /// True once `ticks_to_count` ticks have elapsed.
    pub fn finished(&self) -> bool {
        self.stopwatch.elapsed_ticks() >= self.ticks_to_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verify that the default value for ms_per_tick is 10.
    #[test]
    fn default_ms_per_tick() {
        let tt = TickTimer::new();
        assert_eq!(10, tt.ms_per_tick());
    }

    #[test]
    fn custom_ms_per_tick() {
        let tt = TickTimer::with_ms_per_tick(17);
        assert_eq!(17, tt.ms_per_tick());
    }

    #[test]
    fn increment() {
        let tt = TickTimer::new();
        assert_eq!(0, tt.ticks());
        tt.increment();
        assert_eq!(1, tt.ticks());
        for _ in 0..17 {
            tt.increment();
        }
        assert_eq!(18, tt.ticks());
        tt.increment_by(17);
        assert_eq!(35, tt.ticks());
    }

    #[test]
    fn wrap_around() {
        let tt = TickTimer::new();
        tt.increment_by(u64::MAX);
        assert_eq!(u64::MAX, tt.ticks());
        tt.increment();
        assert_eq!(0, tt.ticks());
    }

    #[test]
    fn stopwatch() {
        let tt = TickTimer::new();
        // Increment it a "random" number of steps.
        tt.increment_by(17);
        let sw = tt.get_new_stopwatch();
        assert_eq!(0, sw.elapsed_ticks()); // Starts at zero.
        assert_eq!(0, sw.elapsed_ms());
        tt.increment();
        assert_eq!(1, sw.elapsed_ticks()); // Increases with the TickTimer.
        assert_eq!(10, sw.elapsed_ms());
    }

    #[test]
    fn stopwatch_wrap_around() {
        let tt = TickTimer::new();
        tt.increment_by(u64::MAX);
        let sw = tt.get_new_stopwatch();
        tt.increment();
        assert_eq!(0, tt.ticks());
        assert_eq!(1, sw.elapsed_ticks());
        assert_eq!(10, sw.elapsed_ms());
        tt.increment();
        assert_eq!(1, tt.ticks());
        assert_eq!(2, sw.elapsed_ticks());
        assert_eq!(20, sw.elapsed_ms());
    }

    #[test]
    fn stopwatch_ms_overflow() {
        let tt = TickTimer::new();
        let sw = tt.get_new_stopwatch();
        tt.increment_by(u64::MAX / 10);
        assert_eq!(u64::MAX, sw.elapsed_ms());
        tt.increment();
        assert_eq!(u64::MAX, sw.elapsed_ms());
        tt.increment_by(u64::MAX - tt.ticks());
        assert_eq!(u64::MAX, tt.ticks());
        assert_eq!(u64::MAX, sw.elapsed_ms());
    }

    #[test]
    fn stopwatch_with_custom_ticktime() {
        const MS_PER_TICK: i32 = 17;
        let tt = TickTimer::with_ms_per_tick(MS_PER_TICK);
        let sw = tt.get_new_stopwatch();
        assert_eq!(0, sw.elapsed_ms());
        tt.increment();
        assert_eq!(MS_PER_TICK as u64, sw.elapsed_ms());
    }

    #[test]
    fn countdown() {
        let tt = TickTimer::new();
        // Increment it a "random" number of steps.
        tt.increment_by(4711);
        let cd = tt.get_new_countdown(17);
        assert!(!cd.finished());
        tt.increment();
        assert!(!cd.finished());
        tt.increment_by(16); // Total increment is now 17.
        assert!(cd.finished());
        // Further increments do not change the state.
        tt.increment();
        assert!(cd.finished());
        tt.increment_by(1234);
        assert!(cd.finished());
    }

    #[test]
    fn clones_share_one_counter() {
        let tt = TickTimer::new();
        let other = tt.clone();
        tt.increment_by(3);
        assert_eq!(3, other.ticks());
    }
}
