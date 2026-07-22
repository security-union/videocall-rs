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

//! Reusable video control button components with SVG icons.

use dioxus::prelude::*;

// =============================================================================
// Microphone Button
// =============================================================================

#[component]
pub fn MicButton(enabled: bool, available: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = match (enabled, available) {
        (true, false) => "video-control-button active error",
        (true, true) => "video-control-button active",
        (false, false) => "video-control-button off error",
        (false, true) => "video-control-button off",
    };

    rsx! {
        button {
            class,
            // Stable hook for E2E (the in-meeting mic toggle). Mirrors the
            // camera button's `camera-toggle-button` testid so the
            // device-permission specs (media-device-permission.spec.ts) can drive
            // the mic ON/OFF and assert the not-disabled retry behavior via a
            // stable selector instead of a fragile tooltip/class match.
            "data-testid": "mic-toggle-button",
            // NOTE: intentionally NOT `disabled: !available`. When a device is
            // unavailable (in use, denied, unplugged) the button must stay
            // clickable so the user can retry acquisition — the `onclick` is the
            // only manual retry path. The `!available` state is conveyed via the
            // warning icon/tooltip/`.device-warning` badge below, not by
            // disabling the control (which previously wedged the user into a
            // leave-and-rejoin).
            onclick: move |evt| onclick.call(evt),
            if enabled {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z" }
                    path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
                    line { x1: "12", y1: "19", x2: "12", y2: "22" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Microphone — Mute" }
                    span { class: "tooltip-desc", "Turn off your microphone so others can't hear you." }
                }
            } else if available {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3z" }
                    path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
                    line { x1: "12", y1: "19", x2: "12", y2: "22" }
                    line { x1: "3", y1: "3", x2: "21", y2: "21" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Microphone — Unmute" }
                    span { class: "tooltip-desc", "Turn your microphone back on so others can hear you." }
                }
            } else {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                    path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V5a3 3 0 0 0-5.94-.6" }
                    path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                    line { x1: "12", y1: "19", x2: "12", y2: "22" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Microphone — Unmute" }
                    span { class: "tooltip-desc", "Microphone unavailable — click to retry." }
                }
                span { class: "device-warning", "!" }
            }
        }
    }
}

// =============================================================================
// Camera Button
// =============================================================================

#[component]
pub fn CameraButton(enabled: bool, available: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = match (enabled, available) {
        (true, false) => "video-control-button active error",
        (true, true) => "video-control-button active",
        (false, false) => "video-control-button off error",
        (false, true) => "video-control-button off",
    };

    rsx! {
        button {
            class,
            // Stable hook for E2E (the in-meeting camera toggle). Used by
            // performance-settings.spec.ts to drive the camera ON/OFF for the
            // send-diagnostics "Camera — off" regression guard (#1101) instead of
            // a fragile tooltip/class selector.
            "data-testid": "camera-toggle-button",
            // NOTE: intentionally NOT `disabled: !available` — see MicButton for
            // the rationale. Keeping the button clickable while unavailable is
            // what lets the user retry a blocked camera without leaving the call.
            onclick: move |evt| onclick.call(evt),
            if enabled {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polygon { points: "23 7 16 12 23 17 23 7" }
                    rect { x: "1", y: "5", width: "15", height: "14", rx: "2", ry: "2" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Camera — Stop Video" }
                    span { class: "tooltip-desc", "Turn off your camera so others can't see you." }
                }
            } else if available {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polygon { points: "23 7 16 12 23 17 23 7" }
                    rect { x: "1", y: "5", width: "15", height: "14", rx: "2", ry: "2" }
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Camera — Start Video" }
                    span { class: "tooltip-desc", "Turn on your camera so others can see you." }
                }
            } else {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Camera — Start Video" }
                    span { class: "tooltip-desc", "Camera unavailable — click to retry." }
                }
                span { class: "device-warning", "!" }
            }
        }
    }
}

// =============================================================================
// Screen Share Button
// =============================================================================

#[component]
pub fn ScreenShareButton(
    active: bool,
    disabled: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = match (active, disabled) {
        (true, true) => "video-control-button active disabled",
        (true, false) => "video-control-button active",
        (false, true) => "video-control-button disabled",
        (false, false) => "video-control-button",
    };

    rsx! {
        button {
            class,
            disabled,
            onclick: move |evt| {
                if !disabled {
                    onclick.call(evt);
                }
            },
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                rect { x: "2", y: "3", width: "20", height: "14", rx: "2", ry: "2" }
                line { x1: "8", y1: "21", x2: "16", y2: "21" }
                line { x1: "12", y1: "17", x2: "12", y2: "21" }
            }
            if active {
                span { class: "tooltip",
                    span { class: "tooltip-title", "Screen share — Stop Screen Share" }
                    span { class: "tooltip-desc", "Stop sharing your screen with everyone in the call." }
                }
            } else {
                span { class: "tooltip",
                    span { class: "tooltip-title", "Screen share — Share Screen" }
                    span { class: "tooltip-desc", "Show a window or your entire screen to everyone in the call." }
                }
            }
        }
    }
}

// =============================================================================
// Peer List Button
// =============================================================================

#[component]
pub fn PeerListButton(
    open: bool,
    // Optional DOM id for the rendered `<button>`. The action-bar call site
    // passes "peer-list-trigger" so the #1790 Escape handler can restore focus
    // here; the customize-mode drag-preview call site passes nothing (empty),
    // which omits the attribute so the id is never duplicated in the DOM.
    #[props(default)] id: String,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            id: if id.is_empty() { None } else { Some(id.clone()) },
            class,
            onclick: move |evt| onclick.call(evt),
            if open {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                    circle { cx: "9", cy: "7", r: "4" }
                    path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                    path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Participants — Close Peers" }
                    span { class: "tooltip-desc", "Hide the participant list." }
                }
            } else {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                    circle { cx: "9", cy: "7", r: "4" }
                    path { d: "M23 21v-2a4 4 0 0 0-3-3.87" }
                    path { d: "M16 3.13a4 4 0 0 1 0 7.75" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Participants — Open Peers" }
                    span { class: "tooltip-desc", "See who's in the call and per-peer host controls." }
                }
            }
        }
    }
}

// =============================================================================
// Diagnostics Button
// =============================================================================

#[component]
pub fn DiagnosticsButton(
    open: bool,
    // Optional DOM id for the rendered `<button>` (see `PeerListButton`). The
    // action-bar call site passes "diagnostics-trigger" for #1790 focus restore;
    // the drag-preview call site passes nothing so the id is never duplicated.
    #[props(default)] id: String,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            id: if id.is_empty() { None } else { Some(id.clone()) },
            class,
            onclick: move |evt| onclick.call(evt),
            if open {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M2 12h2l3.5-7L12 19l2.5-5H20" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Diagnostics — Close Diagnostics" }
                    span { class: "tooltip-desc", "Hide the live connection-quality and stats panel." }
                }
            } else {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M2 12h2l3.5-7L12 19l2.5-5H20" }
                }
                span { class: "tooltip",
                    span { class: "tooltip-title", "Diagnostics — Open Diagnostics" }
                    span { class: "tooltip-desc", "View live connection quality, bitrate, packet loss, and codec stats." }
                }
            }
        }
    }
}

// =============================================================================
// Reactions Button (issue #1884)
// =============================================================================

/// Opens the reactions palette. Mirrors `PeerListButton`/`DiagnosticsButton`,
/// plus the popup a11y contract: `aria-haspopup="menu"` and `aria-expanded`
/// bound to `open` (the palette is `role="menu"`). The call site passes
/// `id="reactions-trigger"` so the Escape/close handlers can restore focus here.
#[component]
pub fn ReactionsButton(
    open: bool,
    #[props(default)] id: String,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            id: if id.is_empty() { None } else { Some(id.clone()) },
            class,
            "data-testid": "reactions-button",
            "aria-label": "Reactions",
            // UX B2: the palette is a role=toolbar, not a menu — drop
            // aria-haspopup (which announces a menu popup), keep aria-expanded,
            // and point aria-controls at the palette's id so AT ties the two.
            "aria-controls": "reactions-palette",
            "aria-expanded": if open { "true" } else { "false" },
            onclick: move |evt| onclick.call(evt),
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "10" }
                path { d: "M8 14s1.5 2 4 2 4-2 4-2" }
                line { x1: "9", y1: "9", x2: "9.01", y2: "9" }
                line { x1: "15", y1: "9", x2: "15.01", y2: "9" }
            }
            span { class: "tooltip",
                span { class: "tooltip-title", "Reactions" }
                span { class: "tooltip-desc", "Send a reaction everyone in the call can see." }
            }
        }
    }
}

// =============================================================================
// Device Settings Button (Mobile Only)
// =============================================================================

#[component]
pub fn DeviceSettingsButton(open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    // Descriptive role prefix ("Device settings — …") names what the button
    // controls; the action verb after the em-dash names what clicking does.
    // Note: this does NOT preserve substring compatibility for callers that
    // matched the old plain title "Settings" — verify each call site (e2e
    // selectors, screen readers, analytics) before assuming so. The
    // production e2e selector for this button is `data-testid="open-settings"`
    // below, which is stable across tooltip text changes.
    let (tooltip_title, tooltip_desc) = if open {
        ("Device settings — Close", "Hide the device settings panel.")
    } else {
        (
            "Device settings",
            "Switch your microphone, camera, or speaker, and tune audio/video options.",
        )
    };

    rsx! {
        button {
            class,
            "data-testid": "open-settings",
            onclick: move |evt| onclick.call(evt),

            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                circle { cx: "12", cy: "12", r: "3" }
                path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
            }

            span { class: "tooltip",
                span { class: "tooltip-title", "{tooltip_title}" }
                span { class: "tooltip-desc", "{tooltip_desc}" }
            }
        }
    }
}

// =============================================================================
// Meeting Options Button (host-only)
// =============================================================================

/// Host-only in-call control that opens the Meeting Options panel (waiting
/// room, admitted-can-admit, end-on-host-leave, allow-guests). Lets the host
/// change meeting options live without navigating away from the call.
#[component]
pub fn MeetingOptionsButton(open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    // Descriptive role prefix ("Meeting options — …") names what the button
    // controls; the action verb after the em-dash names what clicking does.
    // Note: this does NOT preserve substring compatibility for callers that
    // matched the old plain title "Meeting Options" — verify each call site
    // (e2e selectors, screen readers, analytics) before assuming so. The
    // production e2e selector for this button is
    // `data-testid="open-meeting-options"` below, which is stable across
    // tooltip text changes.
    let (tooltip_title, tooltip_desc) = if open {
        ("Meeting options — Close", "Hide the meeting options panel.")
    } else {
        (
            "Meeting options",
            "Toggle the waiting room, choose who can admit guests, and control end-on-host-leave.",
        )
    };

    rsx! {
        button {
            class,
            "data-testid": "open-meeting-options",
            "aria-label": "Meeting options",
            onclick: move |evt| onclick.call(evt),

            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M12 20h9" }
                path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" }
            }

            span { class: "tooltip",
                span { class: "tooltip-title", "{tooltip_title}" }
                span { class: "tooltip-desc", "{tooltip_desc}" }
            }
        }
    }
}

// =============================================================================
// Mock Peers Button (debug / layout testing)
// =============================================================================

#[component]
pub fn MockPeersButton(open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button { id: "mock-peers-trigger", class, onclick: move |evt| onclick.call(evt),
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                fill: "none",
                stroke: "currentColor",
                stroke_width: "2",
                stroke_linecap: "round",
                stroke_linejoin: "round",
                path { d: "M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2" }
                circle { cx: "9", cy: "7", r: "4" }
                line { x1: "19", y1: "8", x2: "19", y2: "14" }
                line { x1: "22", y1: "11", x2: "16", y2: "11" }
            }
            span { class: "tooltip",
                span { class: "tooltip-title", "Mock peers" }
                span { class: "tooltip-desc", "Add synthetic test participants to preview grid layouts without a second browser." }
            }
        }
    }
}

// =============================================================================
// Density Mode Button (layout density selector)
// =============================================================================

#[component]
pub fn DensityModeButton(label: String, open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            id: "density-mode-trigger",
            class,
            title: "Layout density: {label}",
            "aria-haspopup": "menu",
            "aria-expanded": if open { "true" } else { "false" },
            onclick: move |evt| onclick.call(evt),
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                view_box: "0 0 24 24",
                width: "24",
                height: "24",
                fill: "currentColor",
                rect { x: "3", y: "3", width: "8", height: "8", rx: "1" }
                rect { x: "13", y: "3", width: "8", height: "8", rx: "1" }
                rect { x: "3", y: "13", width: "8", height: "8", rx: "1" }
                rect { x: "13", y: "13", width: "8", height: "8", rx: "1" }
            }
            span { class: "tooltip",
                span { class: "tooltip-title", "Layout density: {label}" }
                span { class: "tooltip-desc", "Switch how tightly participant tiles are packed on screen." }
            }
        }
    }
}

// =============================================================================
// Hang Up Button
// =============================================================================

#[component]
pub fn HangUpButton(onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button {
            class: "video-control-button danger",
            onclick: move |evt| onclick.call(evt),
            span { class: "tooltip",
                span { class: "tooltip-title", "Hang up" }
                span { class: "tooltip-desc", "Leave the call. Other participants stay connected." }
            }
            svg {
                xmlns: "http://www.w3.org/2000/svg",
                width: "24",
                height: "24",
                fill: "currentColor",
                view_box: "0 0 24 24",
                path { d: "M12.017 6.995c-2.306 0-4.534.408-6.215 1.507-1.737 1.135-2.788 2.944-2.797 5.451a4.8 4.8 0 0 0 .01.62c.015.193.047.512.138.763a2.557 2.557 0 0 0 2.579 1.677H7.31a2.685 2.685 0 0 0 2.685-2.684v-.645a.684.684 0 0 1 .684-.684h2.647a.686.686 0 0 1 .686.687v.645c0 .712.284 1.395.787 1.898.478.478 1.101.787 1.847.787h1.647a2.555 2.555 0 0 0 2.575-1.674c.09-.25.123-.57.137-.763.015-.2.022-.433.01-.617-.002-2.508-1.049-4.32-2.785-5.458-1.68-1.1-3.907-1.51-6.213-1.51Z" }
            }
        }
    }
}

// =============================================================================
// Record Button
// =============================================================================

/// Visual state of the recording button.
#[derive(Clone, PartialEq, Debug)]
pub enum RecordButtonState {
    /// No recording in progress — show "Start Recording".
    Idle,
    /// MediaRecorder is being set up — button is disabled.
    Activating,
    /// Recording is active — show "Stop Recording".
    Recording,
    /// Recording is being stopped.
    Stopping,
    /// Recording stopped; file is being saved.
    Saving,
}

impl RecordButtonState {
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            RecordButtonState::Activating | RecordButtonState::Stopping | RecordButtonState::Saving
        )
    }
}

#[component]
pub fn RecordButton(state: RecordButtonState, onclick: EventHandler<MouseEvent>) -> Element {
    let is_recording = matches!(state, RecordButtonState::Recording);
    let is_busy = state.is_busy();

    let class = match &state {
        RecordButtonState::Recording => "video-control-button record-active",
        _ if is_busy => "video-control-button disabled",
        _ => "video-control-button",
    };

    let tooltip = match &state {
        RecordButtonState::Idle => "Start Recording",
        RecordButtonState::Activating => "Starting recording\u{2026}",
        RecordButtonState::Recording => "Stop Recording",
        RecordButtonState::Stopping => "Stopping recording\u{2026}",
        RecordButtonState::Saving => "Saving recording\u{2026}",
    };

    rsx! {
        button {
            class,
            disabled: is_busy,
            "data-testid": "record-button",
            onclick: move |evt| {
                if !is_busy {
                    onclick.call(evt);
                }
            },
            if is_recording {
                // Stop icon: solid square
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "currentColor",
                    stroke: "none",
                    rect { x: "5", y: "5", width: "14", height: "14", rx: "2" }
                }
            } else {
                // Record icon: filled circle with outer ring
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    circle { cx: "12", cy: "12", r: "9" }
                    circle { cx: "12", cy: "12", r: "4", fill: "currentColor", stroke: "none" }
                }
            }
            span { class: "tooltip", "{tooltip}" }
        }
    }
}

/// Map the state-string emitted by `recording.js` `onStateChange` callback to
/// the corresponding `RecordButtonState`.  Extracted as a pure function so it
/// can be unit-tested independently of the Dioxus runtime.
///
/// The string values must match what `recording.js` passes to `setState()`:
/// `"activating"`, `"recording"`, `"stopping"`, `"saving"`, `"saved"`,
/// `"idle"`.  Unknown strings fall back to `Idle` (log-level warn in the
/// call-site closure).
pub fn js_state_to_record_button_state(s: &str) -> RecordButtonState {
    match s {
        "activating" => RecordButtonState::Activating,
        "recording" => RecordButtonState::Recording,
        "stopping" => RecordButtonState::Stopping,
        "saving" => RecordButtonState::Saving,
        "saved" | "idle" => RecordButtonState::Idle,
        _ => RecordButtonState::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── js_state_to_record_button_state ────────────────────────────────────
    // Each arm of the JS→Rust mapping must round-trip correctly.
    // These tests FAIL if any arm is removed or mistyped — a rename in
    // recording.js that is not mirrored here would silently leave the button
    // stuck in Idle because the unknown-string fallback also returns Idle.

    #[test]
    fn js_state_activating_maps_to_activating() {
        assert_eq!(
            js_state_to_record_button_state("activating"),
            RecordButtonState::Activating,
        );
    }

    #[test]
    fn js_state_recording_maps_to_recording() {
        assert_eq!(
            js_state_to_record_button_state("recording"),
            RecordButtonState::Recording,
        );
    }

    #[test]
    fn js_state_stopping_maps_to_stopping() {
        assert_eq!(
            js_state_to_record_button_state("stopping"),
            RecordButtonState::Stopping,
        );
    }

    #[test]
    fn js_state_saving_maps_to_saving() {
        assert_eq!(
            js_state_to_record_button_state("saving"),
            RecordButtonState::Saving,
        );
    }

    /// "saved" → Idle: normal recording completion path.
    #[test]
    fn js_state_saved_maps_to_idle() {
        assert_eq!(
            js_state_to_record_button_state("saved"),
            RecordButtonState::Idle,
        );
    }

    /// "idle" → Idle: abort path (cancel on file picker, MediaRecorder error).
    /// This is the path fixed in the abort-cleanup bug: if "idle" were mapped
    /// to any non-Idle state the → Idle cleanup branch in the callback would
    /// never fire on abort.
    #[test]
    fn js_state_idle_maps_to_idle() {
        assert_eq!(
            js_state_to_record_button_state("idle"),
            RecordButtonState::Idle,
        );
    }

    /// Unknown strings fall back to Idle (not a panic, not a stuck state).
    #[test]
    fn js_state_unknown_falls_back_to_idle() {
        assert_eq!(
            js_state_to_record_button_state("bogus"),
            RecordButtonState::Idle,
        );
        assert_eq!(js_state_to_record_button_state(""), RecordButtonState::Idle,);
    }

    /// "saved" and "idle" must both map to Idle — they are the two terminal
    /// states recording.js can emit.  If either were forgotten the cleanup
    /// path (the STOPPED fan-out) would not fire.
    #[test]
    fn both_terminal_js_states_reach_idle_branch() {
        let saved = js_state_to_record_button_state("saved");
        let idle = js_state_to_record_button_state("idle");
        assert_eq!(saved, RecordButtonState::Idle);
        assert_eq!(idle, RecordButtonState::Idle);
        // The two must be equal to each other — both land on the same variant.
        assert_eq!(saved, idle);
    }
}
