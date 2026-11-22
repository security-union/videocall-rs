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

//! "Reception Stats" in the Diagnostics panel: what we receive from each
//! remote peer, one row per peer and media type.
//!
//! The rows come from the `"decoder"` diagnostics events that
//! `videocall_client`'s `DiagnosticWorker` broadcasts every 500 ms for every
//! (peer, media type) it has seen a frame for. Those events carry the remote
//! peer's session id in the `to_peer` metric (`stream_id` is always `None`),
//! and keep coming with fps 0 and bitrate 0 after the stream stops, until the
//! peer leaves. So "last update" here means the last report with media in
//! it, and that is what ages a row out.
//!
//! VIDEO rows also show the rate-control report this client sends the
//! peer about its camera: the capacity and backlog we measure, when the
//! estimator has a capacity (the `capacity_kbps` and `backlog_ms` metrics).

use dioxus::prelude::*;
use std::collections::HashMap;
use videocall_diagnostics::{DiagEvent, MetricValue};

/// A row whose stream carried no media for this long disappears.
pub const RECEPTION_STALE_MS: f64 = 10_000.0;

/// The rows are republished at most this often. Ages are shown in whole
/// seconds, so a faster refresh would only add renders.
pub const RECEPTION_PUBLISH_MS: f64 = 1_000.0;

/// Peer label for a decoder event that names no peer.
pub const UNKNOWN_PEER: &str = "unknown";

// =============================================================================
// Pure helpers (unit tested)
// =============================================================================

/// The media types a decoder event can describe, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReceivedMedia {
    Video,
    Screen,
    Audio,
}

impl ReceivedMedia {
    /// Parse the `media_type` metric, which is `format!("{:?}", MediaType)`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "VIDEO" => Some(Self::Video),
            "SCREEN" => Some(Self::Screen),
            "AUDIO" => Some(Self::Audio),
            _ => None,
        }
    }

    /// The wire name, also used for the `data-media-type` attribute.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "VIDEO",
            Self::Screen => "SCREEN",
            Self::Audio => "AUDIO",
        }
    }

    /// Audio "fps" is decoded packets per second, which says nothing useful
    /// here, so only video and screen show a frame rate.
    pub fn shows_fps(self) -> bool {
        !matches!(self, Self::Audio)
    }
}

/// One stream we receive: the remote peer's session id (`None` when the
/// event named no peer) and the media type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReceptionKey {
    pub peer: Option<String>,
    pub media: ReceivedMedia,
}

impl ReceptionKey {
    /// Sort by peer (numeric session ids in numeric order, unknown last),
    /// then VIDEO, SCREEN, AUDIO.
    fn sort_key(&self) -> (bool, Option<u64>, &str, ReceivedMedia) {
        let peer = self.peer.as_deref();
        (
            peer.is_none(),
            peer.and_then(|p| p.parse::<u64>().ok()),
            peer.unwrap_or(""),
            self.media,
        )
    }
}

/// The rate-control report we send a peer about its camera stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateReport {
    pub capacity_kbps: u64,
    pub backlog_ms: u64,
}

/// One decoder event, parsed.
#[derive(Clone, Debug, PartialEq)]
pub struct DecoderReport {
    pub key: ReceptionKey,
    pub fps: f64,
    pub bitrate_kbps: f64,
    /// Present on VIDEO events once we measure a capacity.
    pub rate: Option<RateReport>,
}

impl DecoderReport {
    /// Whether this report saw any media. A silent stream reports 0 and 0.
    fn has_media(&self) -> bool {
        self.fps > 0.0 || self.bitrate_kbps > 0.0
    }
}

/// Parse a `"decoder"` event. `None` for other subsystems or an event with
/// no recognizable media type.
pub fn parse_decoder_event(event: &DiagEvent) -> Option<DecoderReport> {
    if event.subsystem != "decoder" {
        return None;
    }
    let mut media = None;
    let mut peer = None;
    let mut fps = 0.0;
    let mut bitrate_kbps = 0.0;
    let mut capacity_kbps = None;
    let mut backlog_ms = None;
    for metric in &event.metrics {
        match (metric.name, &metric.value) {
            ("media_type", MetricValue::Text(text)) => media = ReceivedMedia::parse(text),
            ("to_peer", MetricValue::Text(text)) if !text.is_empty() => {
                peer = Some(text.clone());
            }
            ("fps", MetricValue::F64(value)) => fps = *value,
            ("bitrate_kbps", MetricValue::F64(value)) => bitrate_kbps = *value,
            ("capacity_kbps", MetricValue::U64(value)) => capacity_kbps = Some(*value),
            ("backlog_ms", MetricValue::U64(value)) => backlog_ms = Some(*value),
            _ => {}
        }
    }
    // Older emitters put the peer in `stream_id`; use it if `to_peer` is absent.
    let peer = peer.or_else(|| event.stream_id.clone().filter(|id| !id.is_empty()));
    Some(DecoderReport {
        key: ReceptionKey {
            peer,
            media: media?,
        },
        fps,
        bitrate_kbps,
        rate: capacity_kbps.map(|capacity_kbps| RateReport {
            capacity_kbps,
            backlog_ms: backlog_ms.unwrap_or(0),
        }),
    })
}

#[derive(Clone, Debug, PartialEq)]
struct StreamEntry {
    fps: f64,
    bitrate_kbps: f64,
    rate: Option<RateReport>,
    last_media_ms: f64,
}

/// Latest stats per (peer, media type).
#[derive(Default, Debug)]
pub struct ReceptionStore {
    streams: HashMap<ReceptionKey, StreamEntry>,
}

/// A row ready to render.
#[derive(Clone, Debug, PartialEq)]
pub struct ReceptionRow {
    pub key: ReceptionKey,
    pub fps: f64,
    pub bitrate_kbps: f64,
    pub rate: Option<RateReport>,
    /// Whole seconds since the stream last carried media.
    pub age_secs: u64,
}

impl ReceptionStore {
    /// Record a report received at `ts_ms`. A silent report zeroes the
    /// numbers but does not refresh the age; it creates no row by itself.
    pub fn record(&mut self, report: DecoderReport, ts_ms: f64) {
        let has_media = report.has_media();
        match self.streams.get_mut(&report.key) {
            Some(entry) => {
                entry.fps = report.fps;
                entry.bitrate_kbps = report.bitrate_kbps;
                entry.rate = report.rate;
                if has_media {
                    entry.last_media_ms = ts_ms;
                }
            }
            None if has_media => {
                self.streams.insert(
                    report.key,
                    StreamEntry {
                        fps: report.fps,
                        bitrate_kbps: report.bitrate_kbps,
                        rate: report.rate,
                        last_media_ms: ts_ms,
                    },
                );
            }
            None => {}
        }
    }

    /// Drop streams with no media for [`RECEPTION_STALE_MS`] and streams of
    /// peers that `peer_present` says have left (events with no peer id
    /// cannot be checked and only age out), then return the rest in display
    /// order.
    pub fn rows(&mut self, now_ms: f64, peer_present: impl Fn(&str) -> bool) -> Vec<ReceptionRow> {
        self.streams.retain(|key, entry| {
            now_ms - entry.last_media_ms <= RECEPTION_STALE_MS
                && key.peer.as_deref().is_none_or(&peer_present)
        });
        let mut rows: Vec<ReceptionRow> = self
            .streams
            .iter()
            .map(|(key, entry)| ReceptionRow {
                key: key.clone(),
                fps: entry.fps,
                bitrate_kbps: entry.bitrate_kbps,
                rate: entry.rate,
                age_secs: ((now_ms - entry.last_media_ms).max(0.0) / 1000.0) as u64,
            })
            .collect();
        rows.sort_by(|a, b| a.key.sort_key().cmp(&b.key.sort_key()));
        rows
    }
}

/// "just now" under a second, then "N s ago".
pub fn format_age(age_secs: u64) -> String {
    if age_secs == 0 {
        "just now".to_string()
    } else {
        format!("{age_secs} s ago")
    }
}

/// The FPS cell: a rate for video and screen, a dash for audio.
pub fn format_fps(media: ReceivedMedia, fps: f64) -> String {
    if media.shows_fps() {
        format!("{fps:.1}")
    } else {
        "\u{2013}".to_string()
    }
}

/// The bitrate cell.
pub fn format_bitrate(bitrate_kbps: f64) -> String {
    format!("{bitrate_kbps:.0} kbps")
}

/// The capacity / backlog cell: "950 / 12 ms" for a VIDEO row we report on,
/// a dash otherwise.
pub fn format_rate(rate: Option<RateReport>) -> String {
    match rate {
        Some(rate) => format!("{} / {} ms", rate.capacity_kbps, rate.backlog_ms),
        None => "\u{2013}".to_string(),
    }
}

// =============================================================================
// Component
// =============================================================================

/// The Reception Stats section. `peer_names` maps a session id to the name
/// shown; ids it does not know are shown as is.
#[component]
pub fn ReceptionStatsSection(
    rows: Vec<ReceptionRow>,
    peer_names: HashMap<String, String>,
) -> Element {
    rsx! {
        div { class: "diagnostics-section", "data-testid": "reception-stats",
            h3 { "Reception Stats" }
            if rows.is_empty() {
                p { "No reception data available." }
            } else {
                div { class: "reception-stats-table", role: "table",
                    div { class: "reception-stats-row reception-stats-header", role: "row",
                        span { class: "reception-stats-cell", role: "columnheader", "Peer" }
                        span { class: "reception-stats-cell", role: "columnheader", "Media" }
                        span { class: "reception-stats-cell", role: "columnheader", "FPS" }
                        span { class: "reception-stats-cell", role: "columnheader", "Bitrate" }
                        span {
                            class: "reception-stats-cell",
                            role: "columnheader",
                            title: "Capacity (kbps) and backlog we report to the peer about its camera",
                            "Cap / Q"
                        }
                        span { class: "reception-stats-cell", role: "columnheader", "Updated" }
                    }
                    for row in rows.iter() {
                        {
                            let peer_id = row.key.peer.clone().unwrap_or_else(|| UNKNOWN_PEER.to_string());
                            let name = peer_names.get(&peer_id).cloned().unwrap_or_else(|| peer_id.clone());
                            let media = row.key.media.as_str();
                            let fps = format_fps(row.key.media, row.fps);
                            let bitrate = format_bitrate(row.bitrate_kbps);
                            let rate = format_rate(row.rate);
                            let age = format_age(row.age_secs);
                            rsx! {
                                div {
                                    key: "{peer_id}-{media}",
                                    class: "reception-stats-row",
                                    role: "row",
                                    "data-testid": "reception-stats-row",
                                    "data-peer": "{peer_id}",
                                    "data-media-type": "{media}",
                                    span { class: "reception-stats-cell reception-stats-peer", role: "cell", title: "{name}", "{name}" }
                                    span { class: "reception-stats-cell", role: "cell", "{media}" }
                                    span { class: "reception-stats-cell", role: "cell", "data-testid": "reception-stats-fps", "{fps}" }
                                    span { class: "reception-stats-cell", role: "cell", "data-testid": "reception-stats-bitrate", "{bitrate}" }
                                    span { class: "reception-stats-cell", role: "cell", "data-testid": "reception-stats-rate", title: "{rate}", "{rate}" }
                                    span { class: "reception-stats-cell", role: "cell", "data-testid": "reception-stats-age", "{age}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_diagnostics::metric;

    fn decoder_event(media: &str, to_peer: Option<&str>, fps: f64, kbps: f64) -> DiagEvent {
        let mut metrics = vec![
            metric!("fps", fps),
            metric!("bitrate_kbps", kbps),
            metric!("media_type", media.to_string()),
            metric!("from_peer", "me".to_string()),
        ];
        if let Some(peer) = to_peer {
            metrics.push(metric!("to_peer", peer.to_string()));
        }
        DiagEvent {
            subsystem: "decoder",
            stream_id: None,
            ts_ms: 0,
            metrics,
        }
    }

    fn report(peer: &str, media: ReceivedMedia, fps: f64, kbps: f64) -> DecoderReport {
        DecoderReport {
            key: ReceptionKey {
                peer: Some(peer.to_string()),
                media,
            },
            fps,
            bitrate_kbps: kbps,
            rate: None,
        }
    }

    fn keys(rows: &[ReceptionRow]) -> Vec<(Option<&str>, ReceivedMedia)> {
        rows.iter()
            .map(|row| (row.key.peer.as_deref(), row.key.media))
            .collect()
    }

    #[test]
    fn parses_the_peer_from_to_peer() {
        let parsed = parse_decoder_event(&decoder_event("SCREEN", Some("42"), 15.0, 900.0));
        assert_eq!(
            parsed,
            Some(report("42", ReceivedMedia::Screen, 15.0, 900.0))
        );
    }

    #[test]
    fn missing_peer_falls_back_to_stream_id_then_none() {
        let mut event = decoder_event("AUDIO", None, 50.0, 40.0);
        assert_eq!(parse_decoder_event(&event).unwrap().key.peer, None);
        event.stream_id = Some("7".to_string());
        assert_eq!(
            parse_decoder_event(&event).unwrap().key.peer.as_deref(),
            Some("7")
        );
    }

    #[test]
    fn ignores_other_subsystems_and_unknown_media() {
        let mut event = decoder_event("VIDEO", Some("1"), 30.0, 500.0);
        event.subsystem = "video";
        assert_eq!(parse_decoder_event(&event), None);
        assert_eq!(
            parse_decoder_event(&decoder_event("HEARTBEAT", Some("1"), 1.0, 1.0)),
            None
        );
    }

    #[test]
    fn audio_does_not_overwrite_video() {
        // The bug: the last event of any media type won the single slot.
        let mut store = ReceptionStore::default();
        store.record(report("2", ReceivedMedia::Video, 30.0, 800.0), 0.0);
        store.record(report("2", ReceivedMedia::Audio, 50.0, 40.0), 10.0);
        let rows = store.rows(500.0, |_| true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].fps, 30.0);
        assert_eq!(rows[1].bitrate_kbps, 40.0);
    }

    #[test]
    fn rows_sort_by_peer_then_video_screen_audio() {
        let mut store = ReceptionStore::default();
        for (peer, media) in [
            ("10", ReceivedMedia::Audio),
            ("9", ReceivedMedia::Audio),
            ("10", ReceivedMedia::Video),
            ("9", ReceivedMedia::Screen),
            ("9", ReceivedMedia::Video),
        ] {
            store.record(report(peer, media, 1.0, 1.0), 0.0);
        }
        store.record(
            DecoderReport {
                key: ReceptionKey {
                    peer: None,
                    media: ReceivedMedia::Audio,
                },
                fps: 1.0,
                bitrate_kbps: 1.0,
                rate: None,
            },
            0.0,
        );
        let rows = store.rows(0.0, |_| true);
        assert_eq!(
            keys(&rows),
            vec![
                (Some("9"), ReceivedMedia::Video),
                (Some("9"), ReceivedMedia::Screen),
                (Some("9"), ReceivedMedia::Audio),
                (Some("10"), ReceivedMedia::Video),
                (Some("10"), ReceivedMedia::Audio),
                (None, ReceivedMedia::Audio),
            ]
        );
    }

    #[test]
    fn silent_reports_zero_the_row_and_let_it_age_out() {
        let mut store = ReceptionStore::default();
        // A silent report alone creates nothing.
        store.record(report("3", ReceivedMedia::Screen, 0.0, 0.0), 0.0);
        assert!(store.rows(0.0, |_| true).is_empty());

        store.record(report("3", ReceivedMedia::Video, 30.0, 800.0), 1_000.0);
        // Camera off: the worker keeps reporting 0 / 0 every 500 ms.
        store.record(report("3", ReceivedMedia::Video, 0.0, 0.0), 4_000.0);
        let rows = store.rows(4_000.0, |_| true);
        assert_eq!(rows[0].fps, 0.0);
        assert_eq!(rows[0].age_secs, 3);

        assert_eq!(store.rows(1_000.0 + RECEPTION_STALE_MS, |_| true).len(), 1);
        assert!(store
            .rows(1_001.0 + RECEPTION_STALE_MS, |_| true)
            .is_empty());
    }

    #[test]
    fn departed_peers_disappear_and_unknown_peers_only_age_out() {
        let mut store = ReceptionStore::default();
        store.record(report("1", ReceivedMedia::Video, 30.0, 800.0), 0.0);
        store.record(report("2", ReceivedMedia::Video, 30.0, 800.0), 0.0);
        store.record(
            DecoderReport {
                key: ReceptionKey {
                    peer: None,
                    media: ReceivedMedia::Video,
                },
                fps: 30.0,
                bitrate_kbps: 800.0,
                rate: None,
            },
            0.0,
        );
        let rows = store.rows(0.0, |peer| peer == "2");
        assert_eq!(
            keys(&rows),
            vec![
                (Some("2"), ReceivedMedia::Video),
                (None, ReceivedMedia::Video)
            ]
        );
        // Peer 1 does not come back if it is reported present again.
        assert_eq!(store.rows(0.0, |_| true).len(), 2);
    }

    #[test]
    fn parses_the_rate_report_on_video() {
        let mut event = decoder_event("VIDEO", Some("4"), 30.0, 800.0);
        assert_eq!(parse_decoder_event(&event).unwrap().rate, None);
        event.metrics.push(metric!("capacity_kbps", 950u64));
        event.metrics.push(metric!("backlog_ms", 12u64));
        let parsed = parse_decoder_event(&event).unwrap();
        let rate = Some(RateReport {
            capacity_kbps: 950,
            backlog_ms: 12,
        });
        assert_eq!(parsed.rate, rate);

        let mut store = ReceptionStore::default();
        store.record(parsed, 0.0);
        assert_eq!(store.rows(0.0, |_| true)[0].rate, rate);
        // The estimator lost its capacity (stream reset): the cell clears.
        store.record(report("4", ReceivedMedia::Video, 30.0, 800.0), 500.0);
        assert_eq!(store.rows(500.0, |_| true)[0].rate, None);
    }

    #[test]
    fn formats_cells() {
        assert_eq!(format_age(0), "just now");
        assert_eq!(format_age(2), "2 s ago");
        assert_eq!(format_fps(ReceivedMedia::Video, 29.97), "30.0");
        assert_eq!(format_fps(ReceivedMedia::Screen, 5.0), "5.0");
        assert_eq!(format_fps(ReceivedMedia::Audio, 50.0), "\u{2013}");
        assert_eq!(format_bitrate(812.4), "812 kbps");
        assert_eq!(
            format_rate(Some(RateReport {
                capacity_kbps: 950,
                backlog_ms: 12
            })),
            "950 / 12 ms"
        );
        assert_eq!(format_rate(None), "\u{2013}");
    }
}
