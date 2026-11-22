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

//! When an encoder emits keyframes.

/// Decides per frame whether to encode a keyframe.
///
/// Periodic keyframes are spaced `interval_frames` frames after the last
/// keyframe of any kind, so a forced keyframe (PLI request, encoder
/// reconfigure) restarts the period and is never followed a few frames later
/// by a periodic one. The first frame is a periodic keyframe. An
/// `interval_frames` of 0 disables periodic keyframes.
pub(crate) struct KeyframeSchedule {
    /// Frames since the last keyframe, counting the next frame. Starts at
    /// `u32::MAX` so the first frame is a keyframe.
    frames_since_keyframe: u32,
}

impl KeyframeSchedule {
    pub(crate) fn new() -> Self {
        Self {
            frames_since_keyframe: u32::MAX,
        }
    }

    /// Returns whether the next frame is a keyframe, and records it.
    pub(crate) fn next_is_keyframe(&mut self, interval_frames: u32, forced: bool) -> bool {
        let periodic = interval_frames > 0 && self.frames_since_keyframe >= interval_frames;
        let keyframe = periodic || forced;
        self.frames_since_keyframe = if keyframe {
            1
        } else {
            self.frames_since_keyframe.saturating_add(1)
        };
        keyframe
    }
}

#[cfg(test)]
mod tests {
    use super::KeyframeSchedule;

    fn run(
        schedule: &mut KeyframeSchedule,
        interval_frames: u32,
        forced_at: &[usize],
        frames: usize,
    ) -> Vec<usize> {
        (0..frames)
            .filter(|frame| schedule.next_is_keyframe(interval_frames, forced_at.contains(frame)))
            .collect()
    }

    #[test]
    fn periodic_keyframes_start_at_the_first_frame() {
        let mut schedule = KeyframeSchedule::new();
        assert_eq!(run(&mut schedule, 10, &[], 31), vec![0, 10, 20, 30]);
    }

    #[test]
    fn forced_keyframe_restarts_the_period() {
        // Without the restart, frame 10 would be a second keyframe 2 frames
        // after the forced one at 8.
        let mut schedule = KeyframeSchedule::new();
        assert_eq!(run(&mut schedule, 10, &[8], 30), vec![0, 8, 18, 28]);
    }

    #[test]
    fn shorter_interval_takes_effect_immediately() {
        let mut schedule = KeyframeSchedule::new();
        assert_eq!(run(&mut schedule, 100, &[], 50), vec![0]);
        // 50 frames since the last keyframe already exceeds the new interval.
        assert!(schedule.next_is_keyframe(30, false));
    }

    #[test]
    fn zero_interval_only_emits_forced_keyframes() {
        let mut schedule = KeyframeSchedule::new();
        assert_eq!(run(&mut schedule, 0, &[5], 20), vec![5]);
    }
}
