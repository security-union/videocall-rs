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

use dioxus::prelude::*;

use crate::components::meeting_info::MeetingInfo;
use crate::components::peer_list_item::PeerListItem;
use crate::context::UsernameCtx;

#[derive(Props, Clone, PartialEq)]
pub struct PeerListProps {
    pub peers: Vec<String>,
    pub onclose: EventHandler<()>,
    pub show_meeting_info: bool,
    pub room_id: String,
    pub num_participants: usize,
    pub is_active: bool,
    pub on_toggle_meeting_info: EventHandler<()>,
    #[props(default)]
    pub host_display_name: Option<String>,
}

#[component]
pub fn PeerList(props: PeerListProps) -> Element {
    let mut search_query = use_signal(String::new);
    let mut show_context_menu = use_signal(|| false);

    let username_ctx = use_context::<UsernameCtx>();

    // Filter peers based on search query
    let query = search_query.read().to_lowercase();
    let filtered_peers: Vec<String> = props
        .peers
        .iter()
        .filter(|peer| peer.to_lowercase().contains(&query))
        .cloned()
        .collect();

    // Get current user name and check if they're the host
    let current_user_name = username_ctx.as_ref().and_then(|ctx| ctx.read().clone());

    let display_name = current_user_name
        .clone()
        .map(|name| format!("{name} (You)"))
        .unwrap_or_else(|| "(You)".to_string());

    let host_display_name = props.host_display_name.clone();
    let is_current_user_host = host_display_name
        .as_ref()
        .map(|h| current_user_name.as_ref().map(|c| h == c).unwrap_or(false))
        .unwrap_or(false);

    rsx! {
        div {
            if props.show_meeting_info {
                MeetingInfo {
                    is_open: true,
                    onclose: move |_| props.on_toggle_meeting_info.call(()),
                    room_id: props.room_id.clone(),
                    num_participants: props.num_participants,
                    is_active: props.is_active
                }
            }

            div { class: "sidebar-header",
                h2 { "Attendants" }

                div { class: "header-actions",
                    button {
                        class: "menu-button",
                        onclick: move |e| {
                            e.stop_propagation();
                            show_context_menu.set(!*show_context_menu.read());
                        },
                        aria_label: "More options",
                        svg {
                            xmlns: "http://www.w3.org/2000/svg",
                            width: "20",
                            height: "20",
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
                    button {
                        class: "close-button",
                        onclick: move |_| props.onclose.call(()),
                        "×"
                    }

                    if *show_context_menu.read() {
                        div { class: "context-menu",
                            button {
                                class: "context-menu-item",
                                onclick: move |_| {
                                    show_context_menu.set(false);
                                    props.on_toggle_meeting_info.call(());
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
                                    circle { cx: "12", cy: "12", r: "10" }
                                    line { x1: "12", y1: "16", x2: "12", y2: "12" }
                                    line { x1: "12", y1: "8", x2: "12.01", y2: "8" }
                                }
                                if props.show_meeting_info { "Hide Meeting Info" } else { "Show Meeting Info" }
                            }
                        }
                    }
                }
            }

            // Sidebar content
            div { class: "sidebar-content",
                div { class: "search-container",
                    input {
                        r#type: "text",
                        placeholder: "Search attendants...",
                        value: "{search_query}",
                        oninput: move |evt| search_query.set(evt.value()),
                        class: "search-input"
                    }
                }

                div { class: "attendants-section",
                    h3 { "In call" }
                    div { class: "peer-list",
                        ul {
                            // Show self as the first item
                            li {
                                PeerListItem {
                                    name: display_name.clone(),
                                    is_host: is_current_user_host
                                }
                            }

                            for peer in filtered_peers.iter() {
                                li { key: "{peer}",
                                    PeerListItem {
                                        name: peer.clone(),
                                        is_host: host_display_name.as_ref().map(|h| h == peer).unwrap_or(false)
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
