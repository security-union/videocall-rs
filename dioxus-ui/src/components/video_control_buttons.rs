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
pub fn MicButton(enabled: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if enabled {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            class: class,
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
                span { class: "tooltip", "Mute" }
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
                span { class: "tooltip", "Unmute" }
            }
        }
    }
}

// =============================================================================
// Camera Button
// =============================================================================

#[component]
pub fn CameraButton(enabled: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if enabled {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            class: class,
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
                span { class: "tooltip", "Stop Video" }
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
                span { class: "tooltip", "Start Video" }
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
    #[props(default = false)] disabled: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if disabled {
        if active {
            "video-control-button active disabled"
        } else {
            "video-control-button disabled"
        }
    } else if active {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            class: class,
            disabled: disabled,
            onclick: move |evt| {
                if !disabled {
                    onclick.call(evt);
                }
            },
            if active {
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
                span { class: "tooltip", "Stop Screen Share" }
            } else {
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    path { d: "M13 3H4a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-3" }
                    polyline { points: "8 21 12 17 16 21" }
                    polyline { points: "16 7 20 7 20 3" }
                    line { x1: "10", y1: "14", x2: "21", y2: "3" }
                }
                span { class: "tooltip", "Share Screen" }
            }
        }
    }
}

// =============================================================================
// Peer List Button
// =============================================================================

#[component]
pub fn PeerListButton(open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            class: class,
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
                    line { x1: "1", y1: "1", x2: "23", y2: "23" }
                }
                span { class: "tooltip", "Close Peers" }
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
                span { class: "tooltip", "Open Peers" }
            }
        }
    }
}

// =============================================================================
// Diagnostics Button
// =============================================================================

#[component]
pub fn DiagnosticsButton(open: bool, onclick: EventHandler<MouseEvent>) -> Element {
    let class = if open {
        "video-control-button active"
    } else {
        "video-control-button"
    };

    rsx! {
        button {
            class: class,
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
                    line { x1: "3", y1: "3", x2: "21", y2: "21" }
                }
                span { class: "tooltip", "Close Diagnostics" }
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
                span { class: "tooltip", "Open Diagnostics" }
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
        "video-control-button mobile-only-device-settings active"
    } else {
        "video-control-button mobile-only-device-settings"
    };

    rsx! {
        button {
            class: class,
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
                    circle { cx: "12", cy: "12", r: "3" }
                    path { d: "M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 0 1 0 2.83 2 2 0 0 1-2.83 0l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-2 2 2 2 0 0 1-2-2v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06-.06a2 2 0 0 1-2.83 0 2 2 0 0 1 0-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1-2-2 2 2 0 0 1 2-2h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 0 1 0-2.83 2 2 0 0 1 2.83 0l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1 1.51V3a2 2 0 0 1 2-2 2 2 0 0 1 2 2v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 0 1 2.83 0 2 2 0 0 1 0 2.83l-.06-.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 2 2 2 2 0 0 1-2 2h-.09a1.65 1.65 0 0 0-1.51 1z" }
                }
                span { class: "tooltip", "Close Settings" }
            } else {
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
                span { class: "tooltip", "Device Settings" }
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
            span { class: "tooltip", "Hang Up" }
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
