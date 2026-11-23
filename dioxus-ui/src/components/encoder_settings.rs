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

//! The "Encoders" card of the diagnostics panel: what our encoders are doing
//! right now. The camera rows come from its viewer-driven rate control; the
//! audio encoder runs at a fixed bitrate. This card replaced the "Encoder
//! Settings" section and keeps its test ids.

use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use videocall_client::adaptive_quality_constants::AUDIO_QUALITY;
use videocall_client::rate_control::{CameraRateStats, LimitSource, ReportedCapacity};
use videocall_client::VideoCallClient;

/// How often the card re-reads the rate control.
const REFRESH_MS: u32 = 1_000;

/// One label / value row of the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSettingsRow {
    pub label: &'static str,
    pub test_id: &'static str,
    /// The id the row had in the old Encoder Settings section, kept on an
    /// inner element so existing selectors still find the value.
    pub legacy_test_id: Option<&'static str>,
    /// `data-source` on the value: what set a limit (the enum name).
    pub source: Option<&'static str>,
    pub value: String,
}

/// The limit row's label, in plain words, and the source's enum name for
/// `data-source`.
pub fn limit_label(source: LimitSource) -> (&'static str, &'static str) {
    match source {
        LimitSource::Rise => ("Climbing back", "Rise"),
        LimitSource::RelaySignal => ("Relay congestion limit", "RelaySignal"),
        LimitSource::QuietViewer => ("Quiet viewer limit", "QuietViewer"),
        LimitSource::LinkLost => ("Link lost limit", "LinkLost"),
    }
}

fn kbps(bps: u32) -> String {
    format!("{} kbps", (bps + 500) / 1000)
}

/// The camera rows shown for `stats`. Capacity and backlog read "no reports"
/// until a viewer that runs the rate control has reported; capacity reads
/// "not limited" while no viewer's path has queued. The limit row shows only
/// while something holds the rate below what the reports give, labelled by
/// what does.
pub fn encoder_settings_rows(stats: &CameraRateStats) -> Vec<EncoderSettingsRow> {
    let row = |label, test_id, value| EncoderSettingsRow {
        label,
        test_id,
        legacy_test_id: None,
        source: None,
        value,
    };
    let renamed = |label, test_id, legacy, value| EncoderSettingsRow {
        label,
        test_id,
        legacy_test_id: Some(legacy),
        source: None,
        value,
    };
    let mut rows = vec![
        renamed(
            "Video bitrate",
            "encoders-video-bitrate",
            "encoder-bitrate",
            kbps(stats.encoder_bitrate_bps),
        ),
        renamed(
            "Video target",
            "encoders-video-target",
            "encoder-target-bitrate",
            kbps(stats.target_bps),
        ),
        renamed(
            "Video tier",
            "encoders-video-tier",
            "encoder-tier",
            format!(
                "{} ({}x{} @ {} fps)",
                stats.tier_label, stats.tier_width, stats.tier_height, stats.tier_fps
            ),
        ),
        row(
            "Capacity (combined)",
            "encoder-capacity",
            match stats.capacity {
                ReportedCapacity::NoReports => "no reports".to_string(),
                ReportedCapacity::NotLimited => "not limited".to_string(),
                ReportedCapacity::Kbps(capacity) => format!("{capacity} kbps"),
            },
        ),
        row(
            "Backlog (combined)",
            "encoder-backlog",
            stats
                .backlog_ms
                .map_or_else(|| "no reports".to_string(), |b| format!("{b} ms")),
        ),
        row(
            "Viewers reporting",
            "encoder-viewers",
            stats.viewers_reporting.to_string(),
        ),
        row(
            "Frames skipped",
            "encoder-frames-skipped",
            stats.frames_skipped.to_string(),
        ),
        // The cost of a reconfigure on hardware encoders is unmeasured.
        row(
            "Reconfigures",
            "encoder-reconfigures",
            format!("{:.1}/s", stats.reconfigures_per_sec),
        ),
    ];
    if let Some(limit) = stats.limit {
        let (label, source) = limit_label(limit.source);
        rows.push(EncoderSettingsRow {
            label,
            test_id: "encoder-relay-limit",
            legacy_test_id: None,
            source: Some(source),
            value: format!("{} kbps", limit.kbps),
        });
    }
    rows
}

/// The audio row: the bitrate the Opus encoder is configured with, which is
/// fixed ([`AUDIO_QUALITY`]), or "mic off".
pub fn audio_encoder_row(mic_enabled: bool) -> EncoderSettingsRow {
    EncoderSettingsRow {
        label: "Audio bitrate",
        test_id: "encoders-audio-bitrate",
        legacy_test_id: None,
        source: None,
        value: if mic_enabled {
            format!("{} kbps", AUDIO_QUALITY.bitrate_kbps)
        } else {
            "mic off".to_string()
        },
    }
}

#[component]
fn EncoderRows(rows: Vec<EncoderSettingsRow>) -> Element {
    rsx! {
        div { class: "diagnostics-kv",
            for row in rows {
                span { class: "diagnostics-kv-label", "{row.label}" }
                span {
                    class: "diagnostics-kv-value",
                    "data-testid": row.test_id,
                    "data-source": row.source,
                    if let Some(legacy) = row.legacy_test_id {
                        span { "data-testid": legacy, "{row.value}" }
                    } else {
                        "{row.value}"
                    }
                }
            }
        }
    }
}

/// The card. It polls the client only while mounted, and the diagnostics
/// panel mounts it only while open.
#[component]
pub fn EncodersSection(video_enabled: bool, mic_enabled: bool) -> Element {
    let client = use_context::<VideoCallClient>();
    let mut stats = use_signal({
        let client = client.clone();
        move || client.camera_rate_stats()
    });
    use_future(move || {
        let client = client.clone();
        async move {
            loop {
                TimeoutFuture::new(REFRESH_MS).await;
                let latest = client.camera_rate_stats();
                if *stats.peek() != latest {
                    stats.set(latest);
                }
            }
        }
    });

    rsx! {
        div { class: "diagnostics-section", "data-testid": "encoders-card",
            h3 { class: "card-title", "Encoders" }
            div { "data-testid": "encoder-settings",
                if video_enabled {
                    EncoderRows { rows: encoder_settings_rows(&stats()) }
                } else {
                    p { class: "diagnostics-kv-note", "data-testid": "encoder-settings-camera-off", "Camera is off." }
                }
                EncoderRows { rows: vec![audio_encoder_row(mic_enabled)] }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use videocall_client::rate_control::RateLimit;

    fn stats() -> CameraRateStats {
        CameraRateStats {
            target_bps: 812_400,
            encoder_bitrate_bps: 800_000,
            tier_label: "480p30".to_string(),
            tier_width: 854,
            tier_height: 480,
            tier_fps: 30.0,
            capacity: ReportedCapacity::Kbps(950),
            backlog_ms: Some(12),
            limit: None,
            viewers_reporting: 2,
            frames_skipped: 41,
            reconfigures_per_sec: 1.5,
        }
    }

    fn value(rows: &[EncoderSettingsRow], test_id: &str) -> String {
        rows.iter()
            .find(|row| row.test_id == test_id)
            .map(|row| row.value.clone())
            .unwrap_or_else(|| panic!("no row {test_id}"))
    }

    #[test]
    fn every_field_has_a_row() {
        let rows = encoder_settings_rows(&stats());
        assert_eq!(value(&rows, "encoders-video-target"), "812 kbps");
        assert_eq!(value(&rows, "encoders-video-bitrate"), "800 kbps");
        assert_eq!(
            value(&rows, "encoders-video-tier"),
            "480p30 (854x480 @ 30 fps)"
        );
        assert_eq!(value(&rows, "encoder-capacity"), "950 kbps");
        assert_eq!(value(&rows, "encoder-backlog"), "12 ms");
        assert_eq!(value(&rows, "encoder-viewers"), "2");
        assert_eq!(value(&rows, "encoder-frames-skipped"), "41");
        assert_eq!(value(&rows, "encoder-reconfigures"), "1.5/s");
        assert!(rows.iter().all(|row| row.test_id != "encoder-relay-limit"));
    }

    /// The old Encoder Settings ids stay on the values they named, so the
    /// existing selectors keep working.
    #[test]
    fn the_renamed_rows_keep_their_old_ids() {
        let rows = encoder_settings_rows(&stats());
        let legacy = |test_id: &str| {
            rows.iter()
                .find(|row| row.test_id == test_id)
                .and_then(|row| row.legacy_test_id)
        };
        assert_eq!(legacy("encoders-video-bitrate"), Some("encoder-bitrate"));
        assert_eq!(
            legacy("encoders-video-target"),
            Some("encoder-target-bitrate")
        );
        assert_eq!(legacy("encoders-video-tier"), Some("encoder-tier"));
        assert_eq!(legacy("encoder-capacity"), None);
    }

    #[test]
    fn the_audio_row_shows_the_configured_bitrate() {
        let on = audio_encoder_row(true);
        assert_eq!(on.test_id, "encoders-audio-bitrate");
        assert_eq!(on.value, format!("{} kbps", AUDIO_QUALITY.bitrate_kbps));
        assert_eq!(audio_encoder_row(false).value, "mic off");
    }

    #[test]
    fn a_report_without_a_ceiling_reads_not_limited() {
        let mut open = stats();
        open.capacity = ReportedCapacity::NotLimited;
        open.backlog_ms = Some(0);
        let rows = encoder_settings_rows(&open);
        assert_eq!(value(&rows, "encoder-capacity"), "not limited");
        assert_eq!(value(&rows, "encoder-backlog"), "0 ms");
    }

    /// The limit row shows while a limit is in force, labelled by what set
    /// it, with the source's name on `data-source`.
    #[test]
    fn the_limit_shows_while_in_force_labelled_by_source() {
        for (source, label, name) in [
            (LimitSource::Rise, "Climbing back", "Rise"),
            (
                LimitSource::RelaySignal,
                "Relay congestion limit",
                "RelaySignal",
            ),
            (
                LimitSource::QuietViewer,
                "Quiet viewer limit",
                "QuietViewer",
            ),
            (LimitSource::LinkLost, "Link lost limit", "LinkLost"),
        ] {
            let mut limited = stats();
            limited.limit = Some(RateLimit { source, kbps: 450 });
            let rows = encoder_settings_rows(&limited);
            let row = rows
                .iter()
                .find(|row| row.test_id == "encoder-relay-limit")
                .expect("limit row");
            assert_eq!((row.label, row.source), (label, Some(name)));
            assert_eq!(row.value, "450 kbps");
        }
    }

    #[test]
    fn no_reports_yet_says_so() {
        let mut quiet = stats();
        quiet.capacity = ReportedCapacity::NoReports;
        quiet.backlog_ms = None;
        quiet.viewers_reporting = 0;
        let rows = encoder_settings_rows(&quiet);
        assert_eq!(value(&rows, "encoder-capacity"), "no reports");
        assert_eq!(value(&rows, "encoder-backlog"), "no reports");
    }
}
