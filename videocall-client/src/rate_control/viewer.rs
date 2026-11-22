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

//! Viewer side: one [`ViewerEstimator`] per remote camera stream, keyed by
//! the stream owner's session id.
//!
//! Frames are fed the moment their packet reaches the client, before any
//! decode or jitter buffer, so the arrival time is the network's and not the
//! decoder's. The periodic diagnostics report asks [`ReceivedVideo::report`]
//! for each peer; an early report (the start of a backlog) is returned from
//! [`ReceivedVideo::on_frame`] and goes out at once.
//!
//! A camera restart (sequence back to 0) needs nothing here: the estimator
//! spots a new run of the sequence by itself, and it keeps the arrival
//! clock from going backwards.

use std::collections::HashMap;

use videocall_rate_control::{FrameInfo, Report, ViewerConfig, ViewerEstimator};

use crate::adaptive_quality_constants::DIAGNOSTICS_REPORT_INTERVAL_MS;

/// One received camera frame, as the packet describes it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VideoArrival {
    /// `VideoMetadata.sequence`.
    pub(crate) seq: u64,
    /// `MediaPacket.timestamp`. For camera video this is the WebCodecs chunk
    /// timestamp, which is in microseconds.
    pub(crate) timestamp_us: f64,
    /// The encoded frame's payload size.
    pub(crate) bytes: u32,
}

impl VideoArrival {
    /// What the estimator wants: the timestamp in ms, plus the arrival.
    pub(crate) fn frame_info(&self, arrival_ms: f64) -> FrameInfo {
        FrameInfo {
            seq: self.seq,
            timestamp_ms: self.timestamp_us / 1000.0,
            arrival_ms,
            bytes: self.bytes,
        }
    }
}

/// Estimators for every remote camera stream we receive, keyed by the
/// stream owner's session id (the same key the diagnostics report uses).
#[derive(Debug, Default)]
pub(crate) struct ReceivedVideo {
    streams: HashMap<String, ViewerEstimator>,
}

impl ReceivedVideo {
    /// A camera frame from `peer` arrived at `arrival_ms` (monotonic clock).
    /// Returns a report to send at once when a backlog just started, with
    /// or without a ceiling (before a busy period is measured the onset has
    /// none, and the publisher drains on its backlog).
    pub(crate) fn on_frame(
        &mut self,
        peer: &str,
        arrival: VideoArrival,
        arrival_ms: f64,
    ) -> Option<Report> {
        let frame = arrival.frame_info(arrival_ms);
        // Looked up first so the common case allocates no key.
        if let Some(estimator) = self.streams.get_mut(peer) {
            return estimator.on_frame(frame).early_report;
        }
        let mut estimator = ViewerEstimator::new(ViewerConfig {
            report_interval_ms: DIAGNOSTICS_REPORT_INTERVAL_MS as f64,
        });
        let early_report = estimator.on_frame(frame).early_report;
        self.streams.insert(peer.to_string(), estimator);
        early_report
    }

    /// The periodic report about `peer`'s camera stream. `None` means send
    /// nothing this tick: no frame released since the last tick (paused
    /// camera, frames held behind a gap), or no camera from that peer. The
    /// publisher then ages the report it has. A report may carry no
    /// capacity (the path never queued); it is sent all the same.
    pub(crate) fn report(&mut self, peer: &str, now_ms: f64) -> Option<Report> {
        self.streams.get_mut(peer)?.report(now_ms)
    }

    /// The peer left: forget its stream.
    pub(crate) fn remove(&mut self, peer: &str) {
        self.streams.remove(peer);
    }

    /// Streams currently tracked.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.streams.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_MS: f64 = 1000.0 / 30.0;

    /// A 30 fps camera at `kbps`, timestamps in microseconds as WebCodecs
    /// stamps them, delivered with no queue at all.
    fn feed_clean(
        video: &mut ReceivedVideo,
        peer: &str,
        first_seq: u64,
        frames: u64,
        start_ms: f64,
        kbps: f64,
    ) -> f64 {
        let bytes = (kbps * 1000.0 / 8.0 / 30.0) as u32;
        let mut now = start_ms;
        for i in 0..frames {
            now = start_ms + i as f64 * FRAME_MS;
            let arrival = VideoArrival {
                seq: first_seq + i,
                timestamp_us: now * 1000.0 + 5_000_000.0,
                bytes,
            };
            video.on_frame(peer, arrival, now + 40.0);
        }
        now + 40.0
    }

    #[test]
    fn camera_timestamps_are_converted_from_microseconds() {
        let arrival = VideoArrival {
            seq: 7,
            timestamp_us: 1_500_000.0,
            bytes: 900,
        };
        let info = arrival.frame_info(2_000.0);
        assert_eq!(info.timestamp_ms, 1_500.0);
        assert_eq!((info.seq, info.arrival_ms, info.bytes), (7, 2_000.0, 900));
    }

    #[test]
    fn a_clean_stream_gets_a_capacity_report() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 60, 0.0, 600.0);
        let report = video.report("42", now).expect("a report after 2 s");
        // A clean path never queued: the report has no ceiling.
        assert_eq!(report.capacity_bps, None);
        assert_eq!(report.backlog_ms, 0.0);
    }

    /// Every early report the estimator produces goes out, with or without
    /// a ceiling: the crate sends the onset as (no ceiling, backlog) until a
    /// busy period is measured, and the publisher drains on it. Checked
    /// against a bare estimator fed the same frames.
    #[test]
    fn every_early_report_goes_out_as_the_estimator_made_it() {
        let mut video = ReceivedVideo::default();
        let mut bare = ViewerEstimator::new(ViewerConfig {
            report_interval_ms: DIAGNOSTICS_REPORT_INTERVAL_MS as f64,
        });
        let mut early = 0;
        for i in 0..200u64 {
            let sent = i as f64 * FRAME_MS;
            // A queue builds from frame 60 on, 20 ms more per frame.
            let wait = i.saturating_sub(60) as f64 * 20.0;
            let arrival = VideoArrival {
                seq: i,
                timestamp_us: sent * 1000.0,
                bytes: 2_500,
            };
            let arrival_ms = sent + 40.0 + wait;
            let expected = bare.on_frame(arrival.frame_info(arrival_ms)).early_report;
            assert_eq!(video.on_frame("42", arrival, arrival_ms), expected);
            early += usize::from(expected.is_some());
        }
        assert!(early > 0, "the ramp makes an onset");
    }

    #[test]
    fn no_report_for_an_unknown_peer() {
        let mut video = ReceivedVideo::default();
        assert_eq!(video.report("7", 1_000.0), None);
    }

    #[test]
    fn leaving_drops_the_stream_and_its_report() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 60, 0.0, 600.0);
        feed_clean(&mut video, "43", 0, 60, 0.0, 600.0);
        video.remove("42");
        assert_eq!(video.len(), 1);
        assert_eq!(video.report("42", now), None);
        assert!(video.report("43", now).is_some());
    }

    /// A rejoining peer gets a new session id, so it is a new key: nothing
    /// carries over from the old one.
    #[test]
    fn a_rejoin_under_a_new_session_starts_from_scratch() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 60, 0.0, 600.0);
        video.remove("42");
        video.on_frame(
            "43",
            VideoArrival {
                seq: 0,
                timestamp_us: 0.0,
                bytes: 2_000,
            },
            now + 10.0,
        );
        // One frame: no ceiling, nothing from the old session's run.
        let report = video
            .report("43", now + 10.0)
            .expect("a frame was released");
        assert_eq!(report.capacity_bps, None);
    }

    /// The camera restarted (sequence back to 0, timestamps going on): the
    /// estimator spots the new run by itself and keeps measuring it, with
    /// no reset from the glue.
    #[test]
    fn a_restarted_sequence_keeps_being_measured() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 60, 0.0, 600.0);
        assert!(video.report("42", now).is_some());
        let later = feed_clean(&mut video, "42", 0, 60, now + 1_000.0, 600.0);
        assert!(
            video.report("42", later).is_some(),
            "the new run is measured"
        );
    }

    /// Nothing new since the last tick: send nothing, not a copy.
    #[test]
    fn no_new_frames_means_no_report() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 60, 0.0, 600.0);
        assert!(video.report("42", now).is_some());
        assert_eq!(video.report("42", now + 500.0), None, "paused camera");
        let later = feed_clean(&mut video, "42", 60, 15, now + 500.0, 600.0);
        assert!(video.report("42", later).is_some(), "frames again");
    }

    /// The estimator needs a clock that never goes backwards; it clamps it.
    #[test]
    fn a_backwards_arrival_clock_is_clamped() {
        let mut video = ReceivedVideo::default();
        let now = feed_clean(&mut video, "42", 0, 30, 0.0, 600.0);
        // Must not panic or poison the estimator.
        video.on_frame(
            "42",
            VideoArrival {
                seq: 30,
                timestamp_us: (now + 1.0) * 1000.0,
                bytes: 2_000,
            },
            now - 500.0,
        );
        let later = feed_clean(&mut video, "42", 31, 40, now + 40.0, 600.0);
        assert!(video.report("42", later).is_some());
    }
}
