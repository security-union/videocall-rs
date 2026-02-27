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

use crate::components::meeting_info::MeetingInfo;
use crate::components::peer_list_item::PeerListItem;
use crate::context::UsernameCtx;
use dioxus::prelude::*;

#[component]
pub fn PeerList(
    peers: Vec<(String, Option<String>)>,
    onclose: EventHandler<MouseEvent>,
    show_meeting_info: bool,
    room_id: String,
    num_participants: usize,
    is_active: bool,
    on_toggle_meeting_info: EventHandler<()>,
    /// If the current user is the meeting host
    #[props(default)]
    is_current_user_host: bool = false,
    #[props(default)] host_email: Option<String>,
) -> Element {
    let mut search_query = use_signal(String::new);
    let mut show_context_menu = use_signal(|| false);

    let filtered_peers: Vec<(String, Option<String>)> = peers
        .iter()
        .filter(|(display_name, _user_email)| display_name.to_lowercase().contains(&search_query().to_lowercase()))
        .cloned()
        .collect();

    // Get username from context and append (You)
    let username_ctx = use_context::<UsernameCtx>();
    let current_user_name: Option<String> = (username_ctx.0)().clone();

    let display_name = current_user_name
        .clone()
        .map(|name| format!("{name} (You)"))
        .unwrap_or_else(|| "(You)".to_string());

    // Check if current user is host
    let is_current_user_host = is_current_user_host;
    let host_email = host_email.clone();

    rsx! {
        div {
            // Show meeting information at the top when enabled
            if show_meeting_info {
                MeetingInfo {
                    is_open: true,
                    onclose: move |_| on_toggle_meeting_info.call(()),
                    room_id: room_id.clone(),
                    num_participants: num_participants,
                    is_active: is_active,
                }
            }

            div { class: "sidebar-header",
                h2 { "Attendants" }
                div { class: "header-actions",
                    button {
                        class: "menu-button",
                        onclick: move |e: MouseEvent| {
                            e.stop_propagation();
                            show_context_menu.set(!show_context_menu());
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
                        onclick: move |e| onclose.call(e),
                        "\u{00d7}"
                    }
                    if show_context_menu() {
                        div { class: "context-menu",
                            button {
                                class: "context-menu-item",
                                onclick: move |_| on_toggle_meeting_info.call(()),
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
                                if show_meeting_info { "Hide Meeting Info" } else { "Show Meeting Info" }
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
                        oninput: move |e: Event<FormData>| {
                            search_query.set(e.value());
                        },
                        class: "search-input",
                    }
                }

                div { class: "attendants-section",
                    h3 { "In call" }
                    div { class: "peer-list",
                        ul {
                            // show self as the first item with actual username
                            li { PeerListItem { name: display_name.clone(), is_host: is_current_user_host } }

                            for (peer_display_name, peer_user_email) in filtered_peers.iter() {
                                li {
                                    key: "{peer_display_name}",
                                    PeerListItem {
                                        name: peer_display_name.clone(),
                                        is_host: match (&host_email, peer_user_email) {
                                            (Some(h), Some(e)) => h == e,
                                            _ => false,
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
