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

//! Frame-rate measurement shared by the encoders (frames sent) and the
//! receiver's `FpsTracker` (frames received), so both sides use the same
//! formula.
//!
//! Screen capture is content-driven: Chrome delivers no frames at all while
//! the shared content is static, then a burst when it changes. A gap is a
//! pause, not a slow rate, so it is never divided into a frame count.

/// A gap between two frames longer than this is a pause: the next frame
/// starts a new measurement instead of closing a window that spans the gap.
/// It is also how long without a frame counts as silence.
pub(crate) const FRAME_GAP_MS: f64 = 1000.0;

/// Length of one counting window (ms).
pub(crate) const FPS_WINDOW_MS: f64 = 1000.0;

/// What a [`FrameRateMeter`] can say at a given time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FrameRate {
    /// No frame for more than [`FRAME_GAP_MS`] (or none yet).
    Silent,
    /// Frames are flowing but the first full window since they (re)started
    /// has not closed yet.
    Measuring,
    /// Frames per second over the last full window.
    Rate(f64),
}

/// What one frame did to the meter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum FrameEvent {
    /// First frame after silence: a new measurement starts here.
    Restarted,
    /// Counted in the current window.
    Counted,
    /// Closed a window of `elapsed_ms` at `fps`.
    WindowClosed { fps: f64, elapsed_ms: f64 },
}

/// Counts frames in windows of at least [`FPS_WINDOW_MS`]. A window closes on
/// the first frame at least that long after it opened, and the rate is
/// `frames * 1000 / elapsed`, where the opening frame belongs to the previous
/// window.
#[derive(Debug, Clone, Default)]
pub(crate) struct FrameRateMeter {
    window_start_ms: f64,
    frames_in_window: u32,
    last_frame_ms: Option<f64>,
    /// Rate of the last window closed since frames (re)started.
    fps: Option<f64>,
}

impl FrameRateMeter {
    /// Record one frame at `now_ms`.
    pub(crate) fn on_frame(&mut self, now_ms: f64) -> FrameEvent {
        let resumed = self
            .last_frame_ms
            .is_none_or(|last| now_ms - last > FRAME_GAP_MS);
        self.last_frame_ms = Some(now_ms);
        if resumed {
            self.window_start_ms = now_ms;
            self.frames_in_window = 0;
            self.fps = None;
            return FrameEvent::Restarted;
        }
        self.frames_in_window += 1;
        let elapsed_ms = now_ms - self.window_start_ms;
        if elapsed_ms < FPS_WINDOW_MS {
            return FrameEvent::Counted;
        }
        let fps = self.frames_in_window as f64 * 1000.0 / elapsed_ms;
        self.fps = Some(fps);
        self.window_start_ms = now_ms;
        self.frames_in_window = 0;
        FrameEvent::WindowClosed { fps, elapsed_ms }
    }

    /// The frame rate as of `now_ms`.
    pub(crate) fn rate_at(&self, now_ms: f64) -> FrameRate {
        match self.last_frame_ms {
            Some(last) if now_ms - last <= FRAME_GAP_MS => {
                self.fps.map_or(FrameRate::Measuring, FrameRate::Rate)
            }
            _ => FrameRate::Silent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(meter: &mut FrameRateMeter, frames: impl IntoIterator<Item = f64>) {
        for at in frames {
            meter.on_frame(at);
        }
    }

    #[test]
    fn steady_stream_reads_its_rate() {
        let mut meter = FrameRateMeter::default();
        feed(&mut meter, (0..100).map(|i| i as f64 * 1000.0 / 30.0));
        let FrameRate::Rate(fps) = meter.rate_at(3300.0) else {
            panic!("expected a rate, got {:?}", meter.rate_at(3300.0));
        };
        assert!((fps - 30.0).abs() < 0.1, "fps {fps}");
    }

    #[test]
    fn gap_restarts_the_measurement() {
        let mut meter = FrameRateMeter::default();
        feed(&mut meter, (0..60).map(|i| i as f64 * 1000.0 / 30.0));
        assert_eq!(
            meter.rate_at(3000.0),
            FrameRate::Silent,
            "1 s after the last frame"
        );
        assert_eq!(meter.on_frame(30_000.0), FrameEvent::Restarted);
        feed(
            &mut meter,
            (1..20).map(|i| 30_000.0 + i as f64 * 1000.0 / 30.0),
        );
        assert_eq!(meter.rate_at(30_640.0), FrameRate::Measuring);
        feed(
            &mut meter,
            (20..40).map(|i| 30_000.0 + i as f64 * 1000.0 / 30.0),
        );
        assert!(
            matches!(meter.rate_at(31_300.0), FrameRate::Rate(fps) if (fps - 30.0).abs() < 0.1),
            "{:?}",
            meter.rate_at(31_300.0)
        );
    }

    #[test]
    fn nothing_yet_is_silent() {
        assert_eq!(FrameRateMeter::default().rate_at(0.0), FrameRate::Silent);
    }
}
