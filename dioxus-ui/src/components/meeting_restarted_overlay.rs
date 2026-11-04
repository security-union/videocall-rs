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

use crate::components::meeting_footer::trap_tab_in_dialog;
use dioxus::prelude::*;
use std::cell::Cell;
use std::rc::Rc;

pub const MEETING_RESTARTED_OVERLAY_ID: &str = "meeting-restarted-overlay";
pub const MEETING_RESTARTED_CARD_ID: &str = "meeting-restarted-card";
pub const MEETING_RESTARTED_REJOIN_BTN_ID: &str = "meeting-restarted-rejoin-btn";
pub const MEETING_RESTARTED_HOME_BTN_ID: &str = "meeting-restarted-home-btn";
pub const MEETING_RESTARTED_MESSAGE: &str =
    "This meeting ended and a new one started while you were disconnected. Rejoin to continue — you may need to be admitted again.";

pub const NO_LONGER_IN_MEETING_MESSAGE: &str =
    "You are no longer in this meeting. Rejoin to continue — you may need to be admitted again.";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RejoinOverlayVariant {
    #[default]
    MeetingRestarted,
    #[cfg_attr(not(feature = "media-server-jwt-auth"), allow(dead_code))]
    NoLongerInMeeting,
}

impl RejoinOverlayVariant {
    pub fn heading(self) -> &'static str {
        match self {
            Self::MeetingRestarted => "Meeting Restarted",
            Self::NoLongerInMeeting => "Disconnected From Meeting",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Self::MeetingRestarted => MEETING_RESTARTED_MESSAGE,
            Self::NoLongerInMeeting => NO_LONGER_IN_MEETING_MESSAGE,
        }
    }
}

#[component]
pub fn MeetingRestartedOverlay(
    on_rejoin: EventHandler<()>,
    #[props(default)] variant: RejoinOverlayVariant,
) -> Element {
    let rejoin_clicked = use_hook(|| Rc::new(Cell::new(false)));
    rsx! {
        div {
            id: MEETING_RESTARTED_OVERLAY_ID,
            "data-testid": MEETING_RESTARTED_OVERLAY_ID,
            class: "glass-backdrop meeting-restarted-overlay",
            style: "z-index: 9999;",
            onkeydown: move |e: Event<KeyboardData>| e.stop_propagation(),
            div {
                id: MEETING_RESTARTED_CARD_ID,
                class: "card-apple meeting-restarted-card",
                style: "width: 420px; text-align: center;",
                role: "alertdialog",
                "aria-modal": "true",
                "aria-labelledby": "meeting-restarted-title",
                "aria-describedby": "meeting-restarted-message",
                tabindex: "-1",
                onkeydown: move |e: Event<KeyboardData>| {
                    if e.key() == Key::Tab
                        && trap_tab_in_dialog(MEETING_RESTARTED_CARD_ID, e.modifiers().shift())
                    {
                        e.prevent_default();
                    }
                },
                svg {
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "64",
                    height: "64",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "var(--text-secondary)",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    style: "margin: 0 auto var(--space-4);",
                    "aria-hidden": "true",
                    polyline { points: "23 4 23 10 17 10" }
                    path { d: "M20.49 15a9 9 0 1 1-2.12-9.36L23 10" }
                }
                h4 {
                    id: "meeting-restarted-title",
                    style: "margin-top:0; margin-bottom: var(--space-2);",
                    "{variant.heading()}"
                }
                p {
                    id: "meeting-restarted-message",
                    class: "meeting-restarted-message",
                    style: "font-size: var(--fs-7); margin: 1.5rem 0; color: var(--text-secondary);",
                    "{variant.message()}"
                }
                div {
                    style: "display: flex; gap: var(--space-3); justify-content: center; flex-wrap: wrap;",
                    button {
                        id: MEETING_RESTARTED_HOME_BTN_ID,
                        "data-testid": MEETING_RESTARTED_HOME_BTN_ID,
                        class: "btn-apple btn-secondary meeting-restarted-home-btn",
                        onclick: move |_| {
                            #[cfg(target_arch = "wasm32")]
                            crate::components::screen_share_detach::close_all();
                            if let Some(window) = web_sys::window() {
                                let _ = window.location().set_href("/");
                            }
                        },
                        "Return to Home"
                    }
                    button {
                        id: MEETING_RESTARTED_REJOIN_BTN_ID,
                        "data-testid": MEETING_RESTARTED_REJOIN_BTN_ID,
                        class: "btn-apple btn-primary meeting-restarted-rejoin-btn",
                        onmounted: move |element| {
                            let element = element.data();
                            spawn(async move {
                                let _ = element.set_focus(true).await;
                            });
                        },
                        onclick: move |_| {
                            if rejoin_clicked.replace(true) {
                                return;
                            }
                            #[cfg(target_arch = "wasm32")]
                            crate::components::screen_share_detach::close_all();
                            on_rejoin.call(());
                        },
                        "Rejoin"
                    }
                }
            }
        }
    }
}
