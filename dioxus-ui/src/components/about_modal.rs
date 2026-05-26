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

/// Truncate a git SHA to its first 7 characters (the canonical short
/// form).  Returns `"unknown"` when empty and the input unchanged when
/// already shorter than 7 chars (or non-ASCII).
fn short_sha(sha: &str) -> String {
    if sha.is_empty() {
        return "unknown".to_string();
    }
    sha.chars().take(7).collect()
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

#[component]
pub fn AboutModal(open: Signal<bool>) -> Element {
    let mut state = use_signal(|| FetchState::Loading);
    let mut open_sig = open;

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

    let client_version = env!("CARGO_PKG_VERSION");
    let client_sha = short_sha(env!("GIT_SHA"));
    let client_branch = env!("GIT_BRANCH");
    let client_ts = env!("BUILD_TIMESTAMP");

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
        FetchState::Ready(components) => rsx! {
            div { class: "about-modal-table",
                div { class: "about-modal-row about-modal-row--header",
                    span { class: "about-modal-label", "Service" }
                    span { class: "about-modal-value", "Version" }
                    span { class: "about-modal-value", "Commit" }
                    span { class: "about-modal-value", "Built" }
                }
                for comp in components.iter() {
                    div { class: "about-modal-row",
                        span { class: "about-modal-value about-modal-value--strong",
                            "{comp.service}"
                        }
                        span { class: "about-modal-value about-modal-value--mono",
                            "{dash_if_empty(&comp.version)}"
                        }
                        span { class: "about-modal-value about-modal-value--mono",
                            "{short_sha(&comp.git_sha)}"
                        }
                        span { class: "about-modal-value about-modal-value--mono",
                            "{dash_if_empty(&comp.build_timestamp)}"
                        }
                    }
                }
            }
        },
    };

    rsx! {
        div {
            class: "glass-backdrop",
            role: "dialog",
            "aria-modal": "true",
            "aria-labelledby": "about-modal-heading",
            tabindex: -1,
            "data-testid": "about-modal",
            onclick: move |e| {
                e.stop_propagation();
                open_sig.set(false);
            },
            onkeydown: move |e: Event<KeyboardData>| {
                if e.key().to_string() == "Escape" {
                    open_sig.set(false);
                }
            },

            div {
                class: "card-apple about-modal-card",
                onclick: move |e| e.stop_propagation(),

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
                        onclick: move |_| open_sig.set(false),
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
                        div { class: "about-modal-row",
                            span { class: "about-modal-label", "Built" }
                            span { class: "about-modal-value about-modal-value--mono",
                                "{client_ts}"
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
    use super::*;

    #[test]
    fn short_sha_truncates_long_sha() {
        assert_eq!(short_sha("abcdef1234567890"), "abcdef1");
    }

    #[test]
    fn short_sha_keeps_short_input() {
        assert_eq!(short_sha("abc"), "abc");
    }

    #[test]
    fn short_sha_handles_empty() {
        assert_eq!(short_sha(""), "unknown");
    }

    #[test]
    fn short_sha_handles_unicode_safely() {
        // chars().take(7) operates on Unicode scalar values, so a string of
        // four 4-byte chars is shorter than 7 chars and stays intact.
        let emoji = "abc\u{1F600}\u{1F601}";
        assert_eq!(short_sha(emoji), emoji);
    }

    #[test]
    fn dash_if_empty_returns_dash_for_empty() {
        assert_eq!(dash_if_empty(""), "-");
    }

    #[test]
    fn dash_if_empty_passes_through_non_empty() {
        assert_eq!(dash_if_empty("1.2.3"), "1.2.3");
    }
}
