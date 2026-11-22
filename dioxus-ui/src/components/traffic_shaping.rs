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

//! Traffic shaping controls: a hidden debug section in the Diagnostics panel.
//!
//! Tapping the Diagnostics title [`REVEAL_TAPS`] times within
//! [`REVEAL_WINDOW_MS`] reveals the section. Nothing is persisted: every join
//! starts hidden and off, because the [`TrafficShapingUi`] signals and the
//! `VideoCallClient` that owns the shaper are both created per join.
//!
//! The shaper itself lives in `videocall_client::traffic_shaping`; this file
//! only drives it and shows its readout.

use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use videocall_client::traffic_shaping::{
    TrafficShapingScope, TrafficShapingSettings, TrafficShapingStatus,
    TRAFFIC_SHAPING_DELAY_PRESETS_MS, TRAFFIC_SHAPING_RATE_PRESETS_BPS,
};
use videocall_client::VideoCallClient;

use crate::components::toggle_switch::ToggleSwitch;

/// Taps on the Diagnostics title needed to reveal the section.
pub const REVEAL_TAPS: usize = 7;

/// The taps must all land within this window.
pub const REVEAL_WINDOW_MS: f64 = 3_000.0;

/// How often the readout refreshes while the section is on screen.
const READOUT_REFRESH_MS: u32 = 500;

/// Per-join UI state, provided as context by the meeting page.
#[derive(Clone, Copy)]
pub struct TrafficShapingUi {
    /// The hidden section has been revealed during this join.
    pub revealed: Signal<bool>,
    /// Shaping is on. Kept in sync by the client's change listener, so it
    /// also flips off when a step test ends with the panel closed.
    pub active: Signal<bool>,
}

// =============================================================================
// Pure helpers (unit tested)
// =============================================================================

/// Record a tap at `now_ms` and say whether it completes the reveal gesture.
/// `taps` keeps only the taps inside the window.
pub fn register_reveal_tap(taps: &mut Vec<f64>, now_ms: f64) -> bool {
    taps.retain(|&t| now_ms - t < REVEAL_WINDOW_MS);
    taps.push(now_ms);
    if taps.len() >= REVEAL_TAPS {
        taps.clear();
        true
    } else {
        false
    }
}

/// "2 Mbps", "500 kbps".
pub fn format_rate(rate_bps: u32) -> String {
    if rate_bps >= 1_000_000 && rate_bps.is_multiple_of(1_000_000) {
        format!("{} Mbps", rate_bps / 1_000_000)
    } else {
        format!("{} kbps", rate_bps / 1_000)
    }
}

/// "812 B", "48.2 KB", "1.30 MB".
pub fn format_bytes(bytes: usize) -> String {
    let b = bytes as f64;
    if b < 1_000.0 {
        format!("{bytes} B")
    } else if b < 1_000_000.0 {
        format!("{:.1} KB", b / 1_000.0)
    } else {
        format!("{:.2} MB", b / 1_000_000.0)
    }
}

fn scope_label(scope: TrafficShapingScope) -> &'static str {
    match scope {
        TrafficShapingScope::Audio => "Audio",
        TrafficShapingScope::Video => "Video",
        TrafficShapingScope::Both => "Both",
    }
}

fn scope_test_id(scope: TrafficShapingScope) -> &'static str {
    match scope {
        TrafficShapingScope::Audio => "traffic-shaping-scope-audio",
        TrafficShapingScope::Video => "traffic-shaping-scope-video",
        TrafficShapingScope::Both => "traffic-shaping-scope-both",
    }
}

const SCOPES: [TrafficShapingScope; 3] = [
    TrafficShapingScope::Audio,
    TrafficShapingScope::Video,
    TrafficShapingScope::Both,
];

// =============================================================================
// Components
// =============================================================================

/// The controls and readout. Rendered inside the Diagnostics panel once
/// revealed. The readout refreshes only while this is mounted.
#[component]
pub fn TrafficShapingSection() -> Element {
    let client = use_context::<VideoCallClient>();
    let ui = use_context::<TrafficShapingUi>();
    let mut settings = use_signal({
        let client = client.clone();
        move || client.traffic_shaping_settings()
    });
    let mut status = use_signal({
        let client = client.clone();
        move || client.traffic_shaping_status()
    });

    // Readout refresh. Scoped to this component: it stops when the section
    // or the panel unmounts.
    {
        let client = client.clone();
        use_future(move || {
            let client = client.clone();
            async move {
                loop {
                    TimeoutFuture::new(READOUT_REFRESH_MS).await;
                    let latest = client.traffic_shaping_status();
                    if *status.peek() != latest {
                        status.set(latest);
                    }
                    let latest = client.traffic_shaping_settings();
                    if *settings.peek() != latest {
                        settings.set(latest);
                    }
                }
            }
        });
    }

    let refresh = {
        let client = client.clone();
        move || {
            settings.set(client.traffic_shaping_settings());
            status.set(client.traffic_shaping_status());
        }
    };

    let apply = {
        let client = client.clone();
        let mut refresh = refresh.clone();
        move |next: TrafficShapingSettings| {
            client.set_traffic_shaping_settings(next);
            refresh();
        }
    };

    let enabled = (ui.active)();
    let readout: Option<TrafficShapingStatus> = status();
    // During a step test the presets show (and lock to) the step's live
    // settings; otherwise they show the user's saved settings, which the step
    // test never overwrites.
    let step_running = readout.is_some_and(|readout| readout.step_test_remaining_ms.is_some());
    let current = match readout {
        Some(readout) if step_running => readout.settings,
        _ => settings(),
    };

    let rate_options: Vec<PresetOption> = TRAFFIC_SHAPING_RATE_PRESETS_BPS
        .iter()
        .map(|&rate_bps| {
            PresetOption::new(
                format_rate(rate_bps),
                format!("traffic-shaping-rate-{}", rate_bps / 1_000),
                current.rate_bps == rate_bps,
                TrafficShapingSettings {
                    rate_bps,
                    ..current
                },
            )
        })
        .collect();
    let delay_options: Vec<PresetOption> = TRAFFIC_SHAPING_DELAY_PRESETS_MS
        .iter()
        .map(|&delay_ms| {
            PresetOption::new(
                format!("{delay_ms} ms"),
                format!("traffic-shaping-delay-{delay_ms}"),
                current.delay_ms == delay_ms,
                TrafficShapingSettings {
                    delay_ms,
                    ..current
                },
            )
        })
        .collect();
    let scope_options: Vec<PresetOption> = SCOPES
        .iter()
        .map(|&scope| {
            PresetOption::new(
                scope_label(scope).to_string(),
                scope_test_id(scope).to_string(),
                current.scope == scope,
                TrafficShapingSettings { scope, ..current },
            )
        })
        .collect();
    let groups = [
        ("Rate", rate_options),
        ("Delay", delay_options),
        ("Scope", scope_options),
    ];

    let readout_lines = readout.map(|readout| {
        (
            format_bytes(readout.queued_bytes),
            readout.queued_packets,
            format!("{} ms", readout.head_delay_ms.round()),
            readout.dropped_by_cap,
            readout.step_test_remaining_ms.map(|remaining| {
                (
                    format!("Step test ends in {} s", (remaining / 1000.0).ceil()),
                    // The step test runs its own settings. The presets above
                    // show them too, locked; the user's choice comes back
                    // when the step ends.
                    format!(
                        "Step test runs {}, {} ms, {}.",
                        format_rate(readout.settings.rate_bps),
                        readout.settings.delay_ms,
                        scope_label(readout.settings.scope).to_lowercase()
                    ),
                )
            }),
        )
    });

    rsx! {
        div {
            class: "diagnostics-section traffic-shaping-section",
            "data-testid": "traffic-shaping-section",
            h3 { "Traffic shaping" }
            p { class: "traffic-shaping-note",
                "Simulates a narrow uplink for your own outgoing media. Only your sending is affected."
            }

            div { class: "traffic-shaping-row",
                span { class: "traffic-shaping-label", id: "traffic-shaping-toggle-label", "Shaping" }
                ToggleSwitch {
                    enabled,
                    test_id: "traffic-shaping-toggle",
                    labelled_by: "traffic-shaping-toggle-label",
                    on_toggle: {
                        let client = client.clone();
                        let mut refresh = refresh.clone();
                        move |on: bool| {
                            client.set_traffic_shaping_enabled(on);
                            refresh();
                        }
                    },
                }
            }

            for (label, options) in groups {
                div { key: "{label}", class: "traffic-shaping-row", role: "group", "aria-label": "{label}",
                    span { class: "traffic-shaping-label", "{label}" }
                    div { class: "traffic-shaping-options",
                        for option in options {
                            button {
                                key: "{option.test_id}",
                                r#type: "button",
                                class: if option.selected { "traffic-shaping-option selected" } else { "traffic-shaping-option" },
                                disabled: step_running,
                                "aria-pressed": if option.selected { "true" } else { "false" },
                                "data-testid": "{option.test_id}",
                                onclick: {
                                    let mut apply = apply.clone();
                                    let next = option.next;
                                    move |_| apply(next)
                                },
                                "{option.label}"
                            }
                        }
                    }
                }
            }

            div { class: "traffic-shaping-row",
                button {
                    r#type: "button",
                    class: "traffic-shaping-step",
                    "data-testid": "traffic-shaping-step-test",
                    onclick: {
                        let client = client.clone();
                        let mut refresh = refresh.clone();
                        move |_| {
                            client.start_traffic_shaping_step_test();
                            refresh();
                        }
                    },
                    "Step test: 500 kbps for 15 s"
                }
            }

            div {
                class: "traffic-shaping-readout",
                "data-testid": "traffic-shaping-readout",
                if let Some((bytes, packets, head_ms, dropped, step)) = readout_lines {
                    div {
                        "Queue: "
                        span { "data-testid": "traffic-shaping-queue-bytes", "{bytes}" }
                        " in {packets} packets, head waiting "
                        span { "data-testid": "traffic-shaping-queue-ms", "{head_ms}" }
                    }
                    div {
                        "Dropped by cap: "
                        span { "data-testid": "traffic-shaping-dropped", "{dropped}" }
                    }
                    if let Some((remaining, live)) = step {
                        div { "data-testid": "traffic-shaping-step-remaining", "{remaining}" }
                        div { class: "traffic-shaping-note", "{live}" }
                    }
                } else {
                    div { "data-testid": "traffic-shaping-off", "Shaping is off." }
                }
            }
        }
    }
}

/// One preset button.
#[derive(Clone, PartialEq)]
struct PresetOption {
    label: String,
    test_id: String,
    selected: bool,
    /// Settings to apply when this button is pressed.
    next: TrafficShapingSettings,
}

impl PresetOption {
    fn new(label: String, test_id: String, selected: bool, next: TrafficShapingSettings) -> Self {
        Self {
            label,
            test_id,
            selected,
            next,
        }
    }
}

/// Small persistent badge shown to the sender while shaping is on, even with
/// the Diagnostics panel closed.
#[component]
pub fn TrafficShapingBadge() -> Element {
    let Some(ui) = try_use_context::<TrafficShapingUi>() else {
        return rsx! {};
    };
    if !(ui.active)() {
        return rsx! {};
    }
    rsx! {
        div {
            class: "traffic-shaping-badge",
            "data-testid": "traffic-shaping-badge",
            role: "status",
            "Traffic shaping on"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seven_quick_taps_reveal() {
        let mut taps = Vec::new();
        for k in 0..REVEAL_TAPS - 1 {
            assert!(!register_reveal_tap(&mut taps, k as f64 * 200.0));
        }
        assert!(register_reveal_tap(&mut taps, 6.0 * 200.0));
        // The counter starts over afterwards.
        assert!(taps.is_empty());
    }

    #[test]
    fn slow_taps_do_not_reveal() {
        let mut taps = Vec::new();
        // One tap every 600 ms: only five fit in a 3 s window.
        for k in 0..20 {
            assert!(!register_reveal_tap(&mut taps, k as f64 * 600.0));
        }
    }

    #[test]
    fn a_pause_restarts_the_count() {
        let mut taps = Vec::new();
        for k in 0..6 {
            register_reveal_tap(&mut taps, k as f64 * 100.0);
        }
        // Long pause: the six earlier taps expire.
        assert!(!register_reveal_tap(&mut taps, 10_000.0));
        assert_eq!(taps.len(), 1);
    }

    #[test]
    fn rates_and_bytes_format() {
        assert_eq!(format_rate(2_000_000), "2 Mbps");
        assert_eq!(format_rate(1_000_000), "1 Mbps");
        assert_eq!(format_rate(500_000), "500 kbps");
        assert_eq!(format_rate(250_000), "250 kbps");
        assert_eq!(format_bytes(812), "812 B");
        assert_eq!(format_bytes(48_200), "48.2 KB");
        assert_eq!(format_bytes(1_300_000), "1.30 MB");
    }
}
