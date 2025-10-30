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

//! Audio production timing logic for neteq worker

use std::cell::Cell;

/// Target frame duration in milliseconds (10ms = 100 Hz)
pub const FRAME_DURATION_MS: f64 = 10.0;

/// Target audio production rate in Hz
pub const TARGET_FRAME_RATE_HZ: f64 = 100.0;

/// Interval between timing statistics logs in milliseconds
pub const TIMING_LOG_INTERVAL_MS: f64 = 5000.0;

/// Interval for audio production timer callback in milliseconds
/// Matches the target frame duration to minimize unnecessary wake-ups
pub const AUDIO_PRODUCTION_INTERVAL_MS: i32 = 10;

/// Timing state for audio production
#[derive(Debug, Clone, Copy, Default)]
pub struct TimingState {
    pub start_time: f64,
    pub last_production_time: f64,
    pub total_frames_produced: u64,
    pub timing_adjustments: i32,
    pub last_timing_log: f64,
}

thread_local! {
    static TIMING_STATE: Cell<TimingState> = Cell::new(TimingState::default());
}

impl TimingState {
    /// Initialize timing state on first call
    pub fn initialize(now: f64) -> Self {
        TimingState {
            start_time: now,
            last_production_time: now,
            total_frames_produced: 0,
            timing_adjustments: 0,
            last_timing_log: 0.0,
        }
    }

    /// Check if this is the first call (uninitialized)
    #[inline]
    pub fn is_uninitialized(&self) -> bool {
        self.start_time == 0.0
    }

    /// Record a frame production
    #[inline]
    pub fn record_frame_production(&mut self, now: f64) {
        self.total_frames_produced += 1;
        self.last_production_time = now;
    }

    /// Record a timing adjustment
    #[inline]
    pub fn record_timing_adjustment(&mut self) {
        self.timing_adjustments += 1;
    }

    /// Update last timing log time
    #[inline]
    pub fn update_last_log(&mut self, now: f64) {
        self.last_timing_log = now;
    }

    /// Sync frame count when muted
    #[inline]
    pub fn sync_muted_frames(&mut self, expected_frames: u64) {
        self.total_frames_produced = expected_frames;
    }
}

/// Calculate how many frames we're behind schedule
#[inline]
pub fn calculate_frames_behind(total_elapsed_ms: f64, frames_produced: u64) -> i32 {
    let expected_frames = (total_elapsed_ms / FRAME_DURATION_MS) as u64;
    expected_frames.saturating_sub(frames_produced) as i32
}

/// Calculate time elapsed since last production
#[inline]
pub fn calculate_interval_since_last(now: f64, last_production_time: f64) -> f64 {
    now - last_production_time
}

/// Determine if we should produce an audio frame this cycle
#[inline]
pub fn should_produce_audio_frame(
    frames_behind: i32,
    interval_since_last_ms: f64,
    is_muted: bool,
) -> bool {
    if is_muted {
        return false;
    }
    frames_behind > 0 || interval_since_last_ms >= FRAME_DURATION_MS
}

/// Calculate maximum frames to produce in catch-up mode
/// Limits catch-up to prevent blocking the worker thread for too long
#[inline]
pub fn max_catchup_frames(frames_behind: i32) -> usize {
    const MAX_CATCHUP_PER_CYCLE: usize = 5; // Max 50ms of audio per cycle

    if frames_behind <= 0 {
        1 // Normal operation: produce one frame
    } else if frames_behind < MAX_CATCHUP_PER_CYCLE as i32 {
        (frames_behind + 1) as usize // Catch up all frames + current
    } else {
        MAX_CATCHUP_PER_CYCLE // Cap to prevent worker starvation
    }
}

/// Determine if we should log timing statistics
#[inline]
pub fn should_log_timing_stats(timing: &TimingState, now: f64) -> bool {
    now - timing.last_timing_log > TIMING_LOG_INTERVAL_MS
}

/// Calculate actual production rate in Hz
#[inline]
pub fn calculate_production_rate(frames_produced: u64, total_elapsed_ms: f64) -> f64 {
    frames_produced as f64 / (total_elapsed_ms / 1000.0)
}

/// Calculate timing error in milliseconds
#[inline]
pub fn calculate_timing_error(frames_produced: u64, total_elapsed_ms: f64) -> f64 {
    (frames_produced as f64 * FRAME_DURATION_MS) - total_elapsed_ms
}

/// Log timing statistics to console
pub fn log_timing_stats(timing: &TimingState, is_muted: bool) {
    let total_elapsed_ms = js_sys::Date::now() - timing.start_time;
    let actual_rate = calculate_production_rate(timing.total_frames_produced, total_elapsed_ms);
    let timing_error_ms = calculate_timing_error(timing.total_frames_produced, total_elapsed_ms);
    let frames_behind = calculate_frames_behind(total_elapsed_ms, timing.total_frames_produced);

    log::debug!(
        "🎯 NetEq ({}ms timer): {actual_rate:.1}Hz actual, {TARGET_FRAME_RATE_HZ:.1}Hz expected, {timing_error_ms:.1}ms timing error, {frames_behind} behind, muted={is_muted}",
        AUDIO_PRODUCTION_INTERVAL_MS
    );
}

/// Get current timing state
#[inline]
pub fn get_timing_state() -> TimingState {
    TIMING_STATE.with(|cell| cell.get())
}

/// Update timing state
#[inline]
pub fn set_timing_state(state: TimingState) {
    TIMING_STATE.with(|cell| cell.set(state));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frames_behind_on_schedule() {
        // After 100ms, should have produced 10 frames (100ms / 10ms)
        assert_eq!(calculate_frames_behind(100.0, 10), 0);
    }

    #[test]
    fn test_frames_behind_lagging() {
        // After 100ms, only produced 8 frames - 2 behind
        assert_eq!(calculate_frames_behind(100.0, 8), 2);
    }

    #[test]
    fn test_frames_behind_ahead() {
        // After 100ms, produced 12 frames - ahead (saturates to 0)
        assert_eq!(calculate_frames_behind(100.0, 12), 0);
    }

    #[test]
    fn test_should_produce_when_behind() {
        assert!(should_produce_audio_frame(2, 5.0, false));
    }

    #[test]
    fn test_should_not_produce_when_muted() {
        assert!(!should_produce_audio_frame(2, 15.0, true));
    }

    #[test]
    fn test_should_produce_after_full_interval() {
        assert!(should_produce_audio_frame(0, 10.0, false));
        assert!(should_produce_audio_frame(0, 10.1, false));
    }

    #[test]
    fn test_should_not_produce_before_interval() {
        assert!(!should_produce_audio_frame(0, 9.9, false));
        assert!(!should_produce_audio_frame(0, 5.0, false));
    }

    #[test]
    fn test_calculate_production_rate() {
        // 100 frames in 1000ms = 100 Hz
        assert_eq!(calculate_production_rate(100, 1000.0), 100.0);
        // 200 frames in 2000ms = 100 Hz
        assert_eq!(calculate_production_rate(200, 2000.0), 100.0);
        // 50 frames in 1000ms = 50 Hz
        assert_eq!(calculate_production_rate(50, 1000.0), 50.0);
    }

    #[test]
    fn test_calculate_timing_error_perfect() {
        // 100 frames * 10ms = 1000ms - perfect timing
        assert_eq!(calculate_timing_error(100, 1000.0), 0.0);
    }

    #[test]
    fn test_calculate_timing_error_ahead() {
        // 100 frames * 10ms = 1000ms, but only 900ms elapsed - 100ms ahead
        assert_eq!(calculate_timing_error(100, 900.0), 100.0);
    }

    #[test]
    fn test_calculate_timing_error_behind() {
        // 100 frames * 10ms = 1000ms, but 1100ms elapsed - 100ms behind
        assert_eq!(calculate_timing_error(100, 1100.0), -100.0);
    }

    #[test]
    fn test_should_log_timing_stats() {
        let mut timing = TimingState {
            start_time: 1000.0,
            last_timing_log: 1000.0,
            ..Default::default()
        };

        // Should not log before interval
        assert!(!should_log_timing_stats(&timing, 5999.0));

        // Should log after interval
        assert!(should_log_timing_stats(&timing, 6000.0));
        assert!(should_log_timing_stats(&timing, 6001.0));
    }

    #[test]
    fn test_timing_state_initialization() {
        let state = TimingState::initialize(1234.0);
        assert_eq!(state.start_time, 1234.0);
        assert_eq!(state.last_production_time, 1234.0);
        assert_eq!(state.total_frames_produced, 0);
        assert!(!state.is_uninitialized());
    }

    #[test]
    fn test_timing_state_is_uninitialized() {
        let state = TimingState::default();
        assert!(state.is_uninitialized());

        let initialized = TimingState::initialize(123.0);
        assert!(!initialized.is_uninitialized());
    }

    #[test]
    fn test_timing_state_record_frame() {
        let mut state = TimingState::initialize(1000.0);
        state.record_frame_production(1010.0);

        assert_eq!(state.total_frames_produced, 1);
        assert_eq!(state.last_production_time, 1010.0);
    }

    #[test]
    fn test_max_catchup_frames_normal() {
        // Not behind - produce 1 frame
        assert_eq!(max_catchup_frames(0), 1);
        assert_eq!(max_catchup_frames(-1), 1);
    }

    #[test]
    fn test_max_catchup_frames_slight_behind() {
        // 1 frame behind - produce 2 frames (catch up + current)
        assert_eq!(max_catchup_frames(1), 2);
        // 2 frames behind - produce 3 frames
        assert_eq!(max_catchup_frames(2), 3);
    }

    #[test]
    fn test_max_catchup_frames_capped() {
        // Way behind - cap at 5 frames to prevent blocking
        assert_eq!(max_catchup_frames(10), 5);
        assert_eq!(max_catchup_frames(100), 5);
    }

    #[test]
    fn test_max_catchup_frames_boundary() {
        // At boundary (4 behind) - produce 5 frames
        assert_eq!(max_catchup_frames(4), 5);
        // Just over boundary (5 behind) - cap at 5
        assert_eq!(max_catchup_frames(5), 5);
    }
}
