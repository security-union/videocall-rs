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

//! The "Encoder Settings" section of the diagnostics panel: what our camera's
//! viewer-driven rate control is doing right now.

use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use videocall_client::rate_control::CameraRateStats;
use videocall_client::VideoCallClient;

/// How often the section re-reads the rate control (about 2 Hz, the rate the
/// viewers' reports arrive at).
const REFRESH_MS: u32 = 500;

/// One row of the section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderSettingsRow {
    pub label: &'static str,
    pub test_id: &'static str,
    pub value: String,
}

fn kbps(bps: u32) -> String {
    format!("{} kbps", (bps + 500) / 1000)
}

/// The rows shown for `stats`. Capacity and backlog read "no reports" until a
/// viewer that runs the rate control has reported. The relay limit row shows
/// only while a relay congestion limit is in force.
pub fn encoder_settings_rows(stats: &CameraRateStats) -> Vec<EncoderSettingsRow> {
    let row = |label, test_id, value| EncoderSettingsRow {
        label,
        test_id,
        value,
    };
    let mut rows = vec![
        row(
            "Target bitrate",
            "encoder-target-bitrate",
            kbps(stats.target_bps),
        ),
        row(
            "Encoder bitrate",
            "encoder-bitrate",
            kbps(stats.encoder_bitrate_bps),
        ),
        row(
            "Tier",
            "encoder-tier",
            format!(
                "{} ({}x{} @ {} fps)",
                stats.tier_label, stats.tier_width, stats.tier_height, stats.tier_fps
            ),
        ),
        row(
            "Capacity (combined)",
            "encoder-capacity",
            stats
                .capacity_kbps
                .map_or_else(|| "no reports".to_string(), |c| format!("{c} kbps")),
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
    if let Some(limit) = stats.relay_limit_kbps {
        rows.push(row(
            "Relay limit",
            "encoder-relay-limit",
            format!("{limit} kbps"),
        ));
    }
    rows
}

/// The section. It polls the client only while mounted, and the diagnostics
/// panel is mounted only while open.
#[component]
pub fn EncoderSettingsSection(video_enabled: bool) -> Element {
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

    let rows = encoder_settings_rows(&stats());
    rsx! {
        div { class: "diagnostics-section", "data-testid": "encoder-settings",
            h3 { "Encoder Settings" }
            if video_enabled {
                div { class: "encoder-settings-grid",
                    for row in rows {
                        span { class: "encoder-settings-label", "{row.label}" }
                        span {
                            class: "encoder-settings-value",
                            "data-testid": row.test_id,
                            "{row.value}"
                        }
                    }
                }
            } else {
                p { "data-testid": "encoder-settings-camera-off", "Camera is off." }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> CameraRateStats {
        CameraRateStats {
            target_bps: 812_400,
            encoder_bitrate_bps: 800_000,
            tier_label: "480p30".to_string(),
            tier_width: 854,
            tier_height: 480,
            tier_fps: 30.0,
            capacity_kbps: Some(950),
            backlog_ms: Some(12),
            relay_limit_kbps: None,
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
        assert_eq!(value(&rows, "encoder-target-bitrate"), "812 kbps");
        assert_eq!(value(&rows, "encoder-bitrate"), "800 kbps");
        assert_eq!(value(&rows, "encoder-tier"), "480p30 (854x480 @ 30 fps)");
        assert_eq!(value(&rows, "encoder-capacity"), "950 kbps");
        assert_eq!(value(&rows, "encoder-backlog"), "12 ms");
        assert_eq!(value(&rows, "encoder-viewers"), "2");
        assert_eq!(value(&rows, "encoder-frames-skipped"), "41");
        assert_eq!(value(&rows, "encoder-reconfigures"), "1.5/s");
        assert!(rows.iter().all(|row| row.test_id != "encoder-relay-limit"));
    }

    #[test]
    fn the_relay_limit_shows_while_in_force() {
        let mut limited = stats();
        limited.relay_limit_kbps = Some(450);
        let rows = encoder_settings_rows(&limited);
        assert_eq!(value(&rows, "encoder-relay-limit"), "450 kbps");
    }

    #[test]
    fn no_reports_yet_says_so() {
        let mut quiet = stats();
        quiet.capacity_kbps = None;
        quiet.backlog_ms = None;
        quiet.viewers_reporting = 0;
        let rows = encoder_settings_rows(&quiet);
        assert_eq!(value(&rows, "encoder-capacity"), "no reports");
        assert_eq!(value(&rows, "encoder-backlog"), "no reports");
    }
}
