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

//! "About" modal — surfaces client + server build information on the homepage.
//!
//! Renders a glass-backdrop overlay with a card listing the running
//! `videocall-ui` build (compiled-in via `env!("CARGO_PKG_VERSION")`,
//! `GIT_SHA`, `BUILD_TIMESTAMP`) and the aggregated server-side build
//! info returned by `GET /api/v1/versions` (one row per registered
//! service: meeting-api, websocket, webtransport, ...).
//!
//! Lifecycle: the server fetch fires only when the modal opens, so
//! visitors who never tap "About" do not pay for the request.

use dioxus::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct BuildInfo {
    #[serde(default)]
    service: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    git_sha: String,
    #[serde(default)]
    git_branch: String,
    #[serde(default)]
    build_timestamp: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ServerVersionsResponse {
    #[serde(default)]
    components: Vec<BuildInfo>,
}

#[derive(Clone, PartialEq, Eq)]
enum FetchState {
    Loading,
    Ready(Vec<BuildInfo>),
    Error(String),
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

#[component]
pub fn AboutModal(mut open: Signal<bool>) -> Element {
    let mut state = use_signal(|| FetchState::Loading);

    use_effect(move || {
        if !open() {
            return;
        }
        state.set(FetchState::Loading);
        spawn(async move {
            let base_url = match crate::constants::meeting_api_base_url() {
                Ok(url) => url,
                Err(e) => {
                    state.set(FetchState::Error(format!("Config error: {e}")));
                    return;
                }
            };
            let url = format!("{base_url}/api/v1/versions");
            let resp = match reqwest::get(&url).await {
                Ok(r) => r,
                Err(e) => {
                    state.set(FetchState::Error(format!("Network error: {e}")));
                    return;
                }
            };
            if !resp.status().is_success() {
                state.set(FetchState::Error(format!(
                    "Server returned HTTP {}",
                    resp.status().as_u16()
                )));
                return;
            }
            match resp.json::<ServerVersionsResponse>().await {
                Ok(body) => state.set(FetchState::Ready(body.components)),
                Err(e) => state.set(FetchState::Error(format!("Invalid response: {e}"))),
            }
        });
    });

    if !open() {
        return rsx! {};
    }

    // Issue #1480: github info (commit + branch) is gated; version + built are not.
    let show_git = crate::constants::show_build_git_info();
    let client_version = env!("CARGO_PKG_VERSION");
    let client_sha = crate::constants::short_sha(env!("GIT_SHA"));
    let client_branch = env!("GIT_BRANCH");
    let client_ts = env!("BUILD_TIMESTAMP");
    // Issue #1789: render the Built value as date + full time (to the second) +
    // short zone label, converted from UTC into the viewer's local timezone
    // (matches the diagnostics build-info table). Falls back to the raw ts only if
    // `build_datetime_local` returns None (sentinel/empty).
    let client_built =
        crate::constants::build_datetime_local(client_ts).unwrap_or_else(|| client_ts.to_string());

    let server_section = match state() {
        FetchState::Loading => rsx! {
            div { class: "about-modal-status", "Loading server versions..." }
        },
        FetchState::Error(msg) => rsx! {
            div { class: "about-modal-status about-modal-status--error",
                "Couldn't reach the server: {msg}"
            }
        },
        FetchState::Ready(components) if components.is_empty() => rsx! {
            div { class: "about-modal-status", "No server components reported." }
        },
        FetchState::Ready(components) => {
            // Issue #1480: drop the Commit column entirely in production (github
            // info hidden) so the server table is Service/Version/Built (3 cols);
            // the --server-nogit modifier collapses the grid to match the spans.
            let row_class = if show_git {
                "about-modal-row"
            } else {
                "about-modal-row about-modal-row--server-nogit"
            };
            let header_class = if show_git {
                "about-modal-row about-modal-row--header"
            } else {
                "about-modal-row about-modal-row--header about-modal-row--server-nogit"
            };
            rsx! {
                div { class: "about-modal-table",
                    div { class: "{header_class}",
                        span { class: "about-modal-label", "Service" }
                        span { class: "about-modal-value", "Version" }
                        if show_git {
                            span { class: "about-modal-value", "Commit" }
                        }
                        span { class: "about-modal-value", "Built" }
                    }
                    for comp in components.iter() {
                        div { class: "{row_class}",
                            span { class: "about-modal-value about-modal-value--strong",
                                "{comp.service}"
                            }
                            span { class: "about-modal-value about-modal-value--mono",
                                "{dash_if_empty(&comp.version)}"
                            }
                            if show_git {
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{crate::constants::short_sha(&comp.git_sha)}"
                                }
                            }
                            // Issue 1789: the Built value is now a proportional
                            // locale string (e.g. "Jun 19, 2026, 6:48:11 AM PDT"),
                            // so it drops the --mono class (Version/Commit keep it);
                            // matches the diagnostics build-info rendering.
                            span { class: "about-modal-value",
                                "{crate::constants::build_datetime_local(&comp.build_timestamp).unwrap_or_else(|| dash_if_empty(&comp.build_timestamp).to_string())}"
                            }
                        }
                    }
                }
            }
        }
    };

    rsx! {
        div {
            class: "glass-backdrop",
            "data-testid": "about-modal",
            // Click-outside dismiss.  The inner `.card-apple` stops
            // propagation so this fires only for backdrop clicks.
            onclick: move |_| open.set(false),

            div {
                class: "card-apple about-modal-card",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "about-modal-heading",
                tabindex: "0",
                "data-testid": "about-modal-dialog",
                onclick: move |e| e.stop_propagation(),
                onkeydown: move |e: Event<KeyboardData>| {
                    if e.key() == Key::Escape {
                        open.set(false);
                    }
                },
                // Autofocus the dialog when it first mounts so keyboard
                // users can press Escape (or Tab) immediately without
                // clicking inside first.  Mirrors `search_modal.rs` and
                // `device_settings_modal.rs` accessibility patterns.
                onmounted: move |element| {
                    let element = element.data();
                    spawn(async move {
                        let _ = element.set_focus(true).await;
                    });
                },

                div { class: "about-modal-header",
                    h3 {
                        id: "about-modal-heading",
                        class: "about-modal-title",
                        "About"
                    }
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary btn-sm about-modal-close",
                        "aria-label": "Close About dialog",
                        onclick: move |_| open.set(false),
                        "Close"
                    }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "about-client-heading",

                    h4 {
                        id: "about-client-heading",
                        class: "about-modal-section-title",
                        "Client"
                    }
                    div { class: "about-modal-table",
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Component" }
                            span { class: "about-modal-value about-modal-value--strong",
                                "videocall-ui"
                            }
                        }
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Version" }
                            span { class: "about-modal-value about-modal-value--strong",
                                "v{client_version}"
                            }
                        }
                        if show_git {
                            div { class: "about-modal-row",
                                span { class: "about-modal-label", "Commit" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{client_sha}"
                                }
                            }
                            div { class: "about-modal-row",
                                span { class: "about-modal-label", "Branch" }
                                span { class: "about-modal-value about-modal-value--mono",
                                    "{client_branch}"
                                }
                            }
                        }
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Built" }
                            // Issue 1789: proportional locale Built value → drop
                            // --mono (matches the server rows + diagnostics).
                            span { class: "about-modal-value",
                                "{client_built}"
                            }
                        }
                    }
                }

                section {
                    class: "about-modal-section",
                    "aria-labelledby": "about-server-heading",

                    h4 {
                        id: "about-server-heading",
                        class: "about-modal-section-title",
                        "Server"
                    }
                    {server_section}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::dash_if_empty;

    #[test]
    fn dash_if_empty_returns_dash_for_empty() {
        assert_eq!(dash_if_empty(""), "-");
    }

    #[test]
    fn dash_if_empty_passes_through_non_empty() {
        assert_eq!(dash_if_empty("1.2.3"), "1.2.3");
    }
}
