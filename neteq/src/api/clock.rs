// Replaces WebRTC's `Environment` clock (system_wrappers Clock /
// SimulatedClock), which NetEq reads only in `NetEqImpl::ExtractPackets` for
// `total_processing_delay_us`. See PARITY_PLAN.md section 3.5.

//! Wall clock injection for NetEq.
//!
//! Jitter estimation never reads this clock; it runs on [`TickTimer`] ticks.
//! The clock only feeds `total_processing_delay_us`
//! (`now - RtpPacketInfo::receive_time_us`).
//!
//! [`TickTimer`]: crate::api::tick_timer::TickTimer

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// Source of the current time in microseconds.
pub trait Clock: Send + Sync {
    /// Current time in microseconds.
    fn current_time_us(&self) -> i64;
}

/// Production clock backed by `web_time` (works natively and on wasm).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn current_time_us(&self) -> i64 {
        let since_epoch = web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(since_epoch.as_micros()).unwrap_or(i64::MAX)
    }
}

/// Manually advanced clock for tests, `tools::NetEqTest` and the parity
/// harness. Clones share the same time.
#[derive(Debug, Clone, Default)]
pub struct SimulatedClock(Arc<AtomicI64>);

impl SimulatedClock {
    /// A clock starting at `initial_time_us`.
    pub fn new(initial_time_us: i64) -> Self {
        Self(Arc::new(AtomicI64::new(initial_time_us)))
    }

    /// Advances the clock by `delta_us` microseconds.
    pub fn advance_time_us(&self, delta_us: i64) {
        self.0.fetch_add(delta_us, Ordering::Relaxed);
    }

    /// Advances the clock by `delta_ms` milliseconds.
    pub fn advance_time_ms(&self, delta_ms: i64) {
        self.advance_time_us(delta_ms * 1000);
    }

    /// Sets the absolute time in microseconds.
    pub fn set_time_us(&self, time_us: i64) {
        self.0.store(time_us, Ordering::Relaxed);
    }

    /// Current time in milliseconds (truncating).
    pub fn time_ms(&self) -> i64 {
        self.current_time_us() / 1000
    }
}

impl Clock for SimulatedClock {
    fn current_time_us(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_clock_advances_and_shares_time() {
        let clock = SimulatedClock::new(1_000);
        let other = clock.clone();
        clock.advance_time_ms(10);
        assert_eq!(11_000, other.current_time_us());
        other.set_time_us(5);
        assert_eq!(5, clock.current_time_us());
        assert_eq!(0, clock.time_ms());
    }

    #[test]
    fn system_clock_is_positive() {
        assert!(SystemClock.current_time_us() > 0);
    }
}
