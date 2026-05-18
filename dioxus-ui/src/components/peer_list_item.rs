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

use crate::components::icons::mic::MicIcon;
use crate::components::icons::peer::PeerIcon;
use crate::context::AppearanceSettingsCtx;
use dioxus::prelude::*;

#[component]
pub fn PeerListItem(
    name: String,
    #[props(default)] tooltip: String,
    #[props(default)] is_host: bool,
    #[props(default)] is_self: bool,
    #[props(default)] is_guest: bool,
    #[props(default = true)] muted: bool,
    #[props(default = false)] speaking: bool,
    #[props(default = true)] video_disabled: bool,
    #[props(default)] on_edit_name: EventHandler<()>,
    #[props(default)] on_mute: Option<EventHandler<()>>,
    #[props(default)] on_disable_video: Option<EventHandler<()>>,
) -> Element {
    let effective_tooltip = if tooltip.is_empty() {
        name.clone()
    } else {
        tooltip.clone()
    };
    let title = if is_host {
        format!("Host: {effective_tooltip}")
    } else {
        effective_tooltip
    };

    let mic_class = if speaking {
        "peer_item_mic speaking"
    } else {
        "peer_item_mic"
    };

    let appearance_ctx = use_context::<AppearanceSettingsCtx>();
    let appearance = (appearance_ctx.0)();
    let mic_style = if speaking && appearance.glow_enabled {
        let hex = appearance.glow_color.to_hex();
        format!("color: {hex};")
    } else {
        String::new()
    };

    let indicator = match (is_self, is_host) {
        (true, true) => Some("(You/Host)"),
        (true, false) => Some("(You)"),
        (false, true) => Some("(Host)"),
        (false, false) => None,
    };

    let mut peer_menu_open = use_signal(|| false);

    rsx! {
        div { class: "peer_item", title,
            div { class: "peer_item_icon",
                PeerIcon {}
            }
            div { class: "peer_item_text",
                div { class: "peer_item_name_container",
                    "{name}"
                    if let Some(label) = indicator {
                        span { class: "peer-indicator", "{label}" }
                    }
                    if is_guest {
                        span { class: "guest-badge", "Guest" }
                    }
                    if is_self {
                        button {
                            class: "peer_item_edit_btn",
                            title: "Edit your display name",
                            onclick: move |e: MouseEvent| {
                                e.stop_propagation();
                                on_edit_name.call(());
                            },
                            aria_label: "Edit display name",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "14",
                                height: "14",
                                view_box: "0 0 24 24",
                                fill: "none",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round",
                                stroke_linejoin: "round",
                                path { d: "M12 20h9" }
                                path { d: "M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4Z" }
                            }
                        }
                    }
                }
            }
            if on_mute.is_some() || on_disable_video.is_some() {
                div { class: "peer_item_menu_wrapper",
                    button {
                        class: "peer_item_menu_btn",
                        title: "More options",
                        aria_label: "More options",
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            peer_menu_open.set(!peer_menu_open());
                        },
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "14",
                            height: "14",
                            view_box: "0 0 24 24",
                            fill: "none",
                            stroke: "currentColor",
                            stroke_width: "2",
                            stroke_linecap: "round",
                            stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "1" }
                            circle { cx: "12", cy: "5", r: "1" }
                            circle { cx: "12", cy: "19", r: "1" }
                        }
                    }
                    if peer_menu_open() {
                        div { class: "context-menu peer_item_context_menu",
                            if let Some(on_mute) = on_mute {
                                button {
                                    class: "context-menu-item",
                                    onclick: move |e: MouseEvent| {
                                        e.stop_propagation();
                                        peer_menu_open.set(false);
                                        on_mute.call(());
                                    },
                                    svg {
                                        xmlns: "http://www.w3.org/2000/svg",
                                        width: "16",
                                        height: "16",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                        path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
                                        path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
                                        line { x1: "12", y1: "19", x2: "12", y2: "23" }
                                        line { x1: "8", y1: "23", x2: "16", y2: "23" }
                                    }
                                    "Mute"
                                }
                            }
                            if let Some(on_disable_video) = on_disable_video {
                                button {
                                    class: "context-menu-item",
                                    onclick: move |e: MouseEvent| {
                                        e.stop_propagation();
                                        peer_menu_open.set(false);
                                        on_disable_video.call(());
                                    },
                                    svg {
                                        xmlns: "http://www.w3.org/2000/svg",
                                        width: "16",
                                        height: "16",
                                        view_box: "0 0 24 24",
                                        fill: "none",
                                        stroke: "currentColor",
                                        stroke_width: "2",
                                        stroke_linecap: "round",
                                        stroke_linejoin: "round",
                                        path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
                                        line { x1: "1", y1: "1", x2: "23", y2: "23" }
                                    }
                                    "Disable video"
                                }
                            }
                        }
                    }
                }
            }
            div { class: "{mic_class}", style: "{mic_style}",
                MicIcon { muted: muted }
            }
        }
    }
}
