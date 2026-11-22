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

//! The rate-control report on the wire: two fields of the camera stream's
//! `VideoMetrics` on the diagnostics packet a viewer already sends every
//! 500 ms.
//!
//! - `capacity_kbps`: what the path to the viewer carries. 0 means "no
//!   report" (proto3 default): the viewer has no capacity yet, or it is an
//!   older client that does not know the field.
//! - `backlog_ms`: the standing queue the viewer sees. Only meaningful next
//!   to a capacity.

use videocall_rate_control::Report;
use videocall_types::protos::diagnostics_packet::{DiagnosticsPacket, VideoMetrics};
use videocall_types::protos::media_packet::media_packet::MediaType;

/// Puts `report` on `metrics`. `None` writes the "no report" encoding.
pub(crate) fn write_report(metrics: &mut VideoMetrics, report: Option<Report>) {
    match report {
        Some(report) => {
            metrics.capacity_kbps = report.capacity_kbps();
            metrics.backlog_ms = report.backlog_ms_rounded();
        }
        None => {
            metrics.capacity_kbps = 0;
            metrics.backlog_ms = 0;
        }
    }
}

/// The report `metrics` carries, if any. A capacity of 0 is "no report",
/// whatever the backlog says.
pub(crate) fn read_report(metrics: &VideoMetrics) -> Option<Report> {
    (metrics.capacity_kbps > 0)
        .then(|| Report::from_wire(metrics.capacity_kbps, metrics.backlog_ms))
}

/// The report in `packet` if it is a viewer's report about our own camera
/// stream: VIDEO, about the stream whose owner is `own_session` (viewers name
/// it by its owner's session id in `sender_id`), and carrying a capacity.
/// Diagnostics reach the whole room, so most packets are about other
/// participants' streams; before the server assigned our session none can
/// be about ours.
pub(crate) fn camera_report_for(
    packet: &DiagnosticsPacket,
    own_session: Option<u64>,
) -> Option<Report> {
    let own_session = own_session?;
    // Compared as a number: no string built per packet.
    if packet.media_type.enum_value_or_default() != MediaType::VIDEO
        || packet.sender_id.parse::<u64>().ok() != Some(own_session)
    {
        return None;
    }
    read_report(packet.video_metrics.as_ref()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(capacity_bps: u64, backlog_ms: f64) -> Report {
        Report {
            capacity_bps,
            backlog_ms,
        }
    }

    fn packet_about(
        stream_owner: u64,
        media_type: MediaType,
        kbps: u32,
        ms: u32,
    ) -> DiagnosticsPacket {
        let mut packet = DiagnosticsPacket::new();
        packet.sender_id = stream_owner.to_string();
        packet.target_id = "viewer".to_string();
        packet.media_type = media_type.into();
        let mut metrics = VideoMetrics::new();
        metrics.fps_received = 30.0;
        metrics.capacity_kbps = kbps;
        metrics.backlog_ms = ms;
        packet.video_metrics = ::protobuf::MessageField::some(metrics);
        packet
    }

    #[test]
    fn a_report_survives_the_round_trip_in_whole_kbps_and_ms() {
        let mut metrics = VideoMetrics::new();
        write_report(&mut metrics, Some(report(1_234_567, 87.6)));
        assert_eq!((metrics.capacity_kbps, metrics.backlog_ms), (1235, 88));
        assert_eq!(read_report(&metrics), Some(report(1_235_000, 88.0)));
    }

    #[test]
    fn no_report_is_written_as_zero_and_read_as_none() {
        let mut metrics = VideoMetrics::new();
        metrics.capacity_kbps = 900;
        metrics.backlog_ms = 40;
        write_report(&mut metrics, None);
        assert_eq!((metrics.capacity_kbps, metrics.backlog_ms), (0, 0));
        assert_eq!(read_report(&metrics), None);
    }

    #[test]
    fn zero_capacity_is_no_report_even_with_a_backlog() {
        let mut metrics = VideoMetrics::new();
        metrics.backlog_ms = 300;
        assert_eq!(read_report(&metrics), None);
    }

    /// What an older viewer sends: fps and bitrate only.
    #[test]
    fn an_old_viewers_metrics_carry_no_report() {
        let mut metrics = VideoMetrics::new();
        metrics.fps_received = 29.0;
        metrics.bitrate_kbps = 800;
        assert_eq!(read_report(&metrics), None);
    }

    /// A congested link with no backlog yet: capacity alone is a report.
    #[test]
    fn zero_backlog_is_a_valid_report() {
        let mut metrics = VideoMetrics::new();
        write_report(&mut metrics, Some(report(450_000, 0.0)));
        assert_eq!(read_report(&metrics), Some(report(450_000, 0.0)));
    }

    /// Room of three: us (session 1), B (session 2) and C (session 3). We
    /// also receive B's reports about C's stream.
    #[test]
    fn only_reports_about_our_camera_stream_count() {
        let ours = packet_about(1, MediaType::VIDEO, 800, 10);
        assert_eq!(
            camera_report_for(&ours, Some(1)),
            Some(report(800_000, 10.0))
        );
        assert_eq!(camera_report_for(&ours, Some(3)), None, "about C's stream");
        let screen = packet_about(1, MediaType::SCREEN, 800, 10);
        assert_eq!(camera_report_for(&screen, Some(1)), None, "screen share");
    }

    #[test]
    fn nothing_counts_before_our_session_is_known() {
        let packet = packet_about(1, MediaType::VIDEO, 800, 10);
        assert_eq!(camera_report_for(&packet, None), None);
    }

    #[test]
    fn a_report_about_us_without_metrics_or_capacity_is_none() {
        let mut packet = packet_about(1, MediaType::VIDEO, 0, 0);
        assert_eq!(camera_report_for(&packet, Some(1)), None);
        packet.video_metrics = ::protobuf::MessageField::none();
        assert_eq!(camera_report_for(&packet, Some(1)), None);
    }
}
