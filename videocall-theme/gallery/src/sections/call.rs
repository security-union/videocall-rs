// SPDX-License-Identifier: MIT OR Apache-2.0

//! The in-call screen: a `.theme-dark` subtree (the app pins the call to
//! dark), a looping gradient standing in for video, and the control bar
//! with every `video-control-button` state.

use dioxus::prelude::*;
use videocall_theme::*;

use super::Section;
use crate::icons::{self, Icon};
use crate::Gallery;

#[component]
pub fn Call() -> Element {
    let mut gallery = use_context::<Gallery>();
    let mut mic = use_signal(|| true);
    let mut camera = use_signal(|| false);
    rsx! {
        Section {
            id: "call",
            title: "In-call",
            lede: "Pinned to dark with .theme-dark and scoped with .over-media, so Clear is the only glass over the video, and a bottom fade under the bar. On is inverted and the icon changes too, so the state never rests on color alone. The toast is .toast-solid, and the first and last tooltips line up with their button so they stay on screen. The name label sits on --material-dim-label, 54% black, so it still reads when the picture is white.",
            div { class: "theme-dark over-media gallery-call",
                div { class: "gallery-call-tile gallery-video",
                    MediaDim { edge: DimEdge::Bottom }
                    Toast { solid: true, class: "gallery-call-toast", span { class: "grow", "Sam joined" } }
                    div { class: "gallery-call-name",
                        Badge { "Dana" }
                        StatusDot { tone: StatusTone::Success, label: "Speaking" }
                    }
                    Bar { clear: true, class: "gallery-call-controls", aria_label: "Call controls",
                        ControlButton {
                            label: "Microphone",
                            state: if mic() { ControlState::On } else { ControlState::Off },
                            pressed: mic(),
                            onclick: move |_| mic.toggle(),
                            Icon { paths: if mic() { icons::MIC } else { icons::MIC_OFF } }
                            Tooltip { align: TooltipAlign::Start, if mic() { "Mute" } else { "Unmute" } }
                        }
                        ControlButton {
                            label: "Camera",
                            state: if camera() { ControlState::On } else { ControlState::Off },
                            pressed: camera(),
                            onclick: move |_| camera.toggle(),
                            Icon { paths: if camera() { icons::CAMERA } else { icons::CAMERA_OFF } }
                            Tooltip { if camera() { "Stop video" } else { "Start video" } }
                        }
                        ControlButton {
                            label: "Share screen",
                            disabled: true,
                            Icon { paths: icons::SCREEN }
                            Tooltip { "Not supported on this device" }
                        }
                        ControlButton {
                            label: "Device settings",
                            unavailable: true,
                            Icon { paths: icons::SETTINGS }
                            Tooltip { open: true, "No microphone permission" }
                        }
                        // No label: like dioxus-ui's hang up, the tooltip text names it.
                        ControlButton {
                            state: ControlState::Danger,
                            onclick: move |_| gallery.show_toast("You left the call (not really)"),
                            Icon { paths: icons::HANG_UP }
                            Tooltip { align: TooltipAlign::End, "Hang Up" }
                        }
                    }
                }
            }
        }
    }
}
