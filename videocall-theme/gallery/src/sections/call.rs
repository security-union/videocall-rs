// SPDX-License-Identifier: MIT OR Apache-2.0

//! The in-call screen: a `.theme-dark` subtree (the app pins the call to
//! dark), a looping gradient standing in for video, and the control bar as
//! dioxus-ui renders it, at desktop width and at phone width.
//!
//! Both bars come from `CallControl`, the same description dioxus-ui's bar
//! uses, so the names, icons, tooltips and states here are the app's.

use dioxus::prelude::*;
use videocall_theme::*;

use super::{Section, Subhead};
use crate::Gallery;

/// The state of every control, shared by the two bars so they agree.
#[derive(Clone, Copy)]
struct CallState {
    mic: Signal<bool>,
    camera: Signal<bool>,
    share: Signal<bool>,
    peers: Signal<bool>,
    diagnostics: Signal<bool>,
    settings: Signal<bool>,
}

impl CallState {
    fn on(self, control: CallControl) -> bool {
        match control {
            CallControl::Microphone => (self.mic)(),
            CallControl::Camera => (self.camera)(),
            CallControl::ScreenShare => (self.share)(),
            CallControl::Peers => (self.peers)(),
            CallControl::Diagnostics => (self.diagnostics)(),
            CallControl::DeviceSettings => (self.settings)(),
            CallControl::BabyMode | CallControl::HangUp => false,
        }
    }

    /// What a click does. The panels are exclusive, as in the app.
    fn click(self, control: CallControl, mut gallery: Gallery) {
        let Self {
            mut mic,
            mut camera,
            mut share,
            mut peers,
            mut diagnostics,
            mut settings,
        } = self;
        match control {
            CallControl::Microphone => mic.toggle(),
            CallControl::Camera => camera.toggle(),
            CallControl::ScreenShare => share.toggle(),
            CallControl::Peers => {
                peers.toggle();
                if peers() {
                    diagnostics.set(false);
                }
            }
            CallControl::Diagnostics => {
                diagnostics.toggle();
                if diagnostics() {
                    peers.set(false);
                }
            }
            CallControl::DeviceSettings => {
                settings.toggle();
                if settings() {
                    peers.set(false);
                    diagnostics.set(false);
                }
            }
            CallControl::BabyMode => gallery.show_toast("Controls locked (not really)"),
            CallControl::HangUp => gallery.show_toast("You left the call (not really)"),
        }
    }
}

#[component]
pub fn Call() -> Element {
    let gallery = use_context::<Gallery>();
    let state = CallState {
        mic: use_signal(|| true),
        camera: use_signal(|| false),
        share: use_signal(|| false),
        peers: use_signal(|| false),
        diagnostics: use_signal(|| false),
        settings: use_signal(|| false),
    };
    // Open on load, so the page shows the menu.
    let mut more_open = use_signal(|| true);
    let menu_controls = CallControl::ALL
        .into_iter()
        .filter(|control| !control.in_phone_bar());
    rsx! {
        Section {
            id: "call",
            title: "In-call",
            lede: "Pinned to dark with .theme-dark and scoped with .over-media, so Clear is the only glass over the video, and a bottom fade under the bar. On is inverted and the icon changes too, so the state never rests on color alone. The toast is .toast-solid, and the first and last tooltips line up with their button so they stay on screen. The name label sits on --material-dim-label, 54% black, so it still reads when the picture is white. Both bars are rendered from CallControl, the description dioxus-ui's bar uses.",
            Subhead { "Wide screens: every control in the bar" }
            div { class: "theme-dark over-media gallery-call gallery-call-wide",
                div { class: "gallery-call-tile gallery-video",
                    MediaDim { edge: DimEdge::Bottom }
                    Toast { solid: true, class: "gallery-call-toast", span { class: "grow", "Sam joined" } }
                    div { class: "gallery-call-name",
                        Badge { "Dana" }
                        StatusDot { tone: StatusTone::Success, label: "Speaking" }
                    }
                    Bar { clear: true, class: "gallery-call-controls", aria_label: "Call controls",
                        for control in CallControl::ALL {
                            CallButton {
                                key: "{control:?}",
                                control,
                                on: state.on(control),
                                onclick: move |_| state.click(control, gallery),
                            }
                        }
                    }
                }
            }
            Subhead { "Phones: mic, camera, More and hang up" }
            p { class: "gallery-lede",
                "Under 37.5em the bar keeps four buttons at the normal gap, and the rest are rows of the More menu: the same names and icons, with aria-checked for anything that has an on state. The menu is a sibling of the bar, so on this screen it is an opaque elevated fill, not glass. Down, Up, Home and End move between rows; Escape and Tab close it."
            }
            div { class: "theme-dark over-media gallery-call gallery-call-phone",
                div { class: "gallery-call-tile gallery-video",
                    MediaDim { edge: DimEdge::Bottom }
                    div { class: "gallery-call-controls gallery-call-phone-controls",
                        Bar { clear: true, aria_label: "Call controls",
                            for control in CallControl::ALL.into_iter().filter(|control| control.in_phone_bar() && *control != CallControl::HangUp) {
                                CallButton {
                                    key: "{control:?}",
                                    control,
                                    on: state.on(control),
                                    onclick: move |_| state.click(control, gallery),
                                }
                            }
                            CallMoreButton {
                                open: more_open(),
                                menu_id: "gallery-call-more-menu",
                                onclick: move |_| more_open.toggle(),
                            }
                            CallButton {
                                control: CallControl::HangUp,
                                onclick: move |_| state.click(CallControl::HangUp, gallery),
                            }
                        }
                        if more_open() {
                            Menu {
                                id: "gallery-call-more-menu",
                                class: "gallery-call-more",
                                aria_label: "More",
                                onclose: move |_| more_open.set(false),
                                for control in menu_controls {
                                    CallMenuItem {
                                        key: "{control:?}",
                                        control,
                                        on: state.on(control),
                                        onclick: move |_| {
                                            state.click(control, gallery);
                                            more_open.set(false);
                                        },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
