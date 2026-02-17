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

use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::meetings_list::MeetingsList;
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::routing::Route;
use web_time::SystemTime;

const TEXT_INPUT_CLASSES: &str = "input-apple";

fn generate_meeting_id() -> String {
    let millis = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{millis:x}")
}

#[component]
pub fn Home() -> Element {
    let navigator = use_navigator();
    let mut username_ctx = use_context::<UsernameCtx>();

    let mut username_input = use_signal(|| {
        username_ctx
            .as_ref()
            .and_then(|ctx| ctx.read().clone())
            .unwrap_or_else(|| load_username_from_storage().unwrap_or_default())
    });

    let mut meeting_id_input = use_signal(String::new);

    // Set Matomo user id early if we have a stored username
    use_effect(move || {
        let uid = username_input.read().clone();
        if !uid.is_empty() {
            matomo_logger::set_user_id(&uid);
        }
    });

    let onsubmit = move |evt: Event<FormData>| {
        evt.prevent_default();
        let username = username_input.read().clone();
        let meeting_id = meeting_id_input.read().clone();

        if !is_valid_username(&username) || meeting_id.is_empty() {
            let _ = web_sys::window()
                .unwrap()
                .alert_with_message("Please provide a valid username and meeting id (a-z, A-Z, 0-9, _).");
            return;
        }

        save_username_to_storage(&username);
        if let Some(mut ctx) = username_ctx {
            ctx.set(Some(username.clone()));
        }
        matomo_logger::set_user_id(&username);
        navigator.push(Route::Meeting { id: meeting_id });
    };

    let create_meeting = move |_| {
        let username = username_input.read().clone();
        if !is_valid_username(&username) {
            let _ = web_sys::window()
                .unwrap()
                .alert_with_message("Please enter a valid username before creating a meeting.");
            return;
        }

        let meeting_id = generate_meeting_id();
        save_username_to_storage(&username);
        if let Some(mut ctx) = username_ctx {
            ctx.set(Some(username.clone()));
        }
        matomo_logger::set_user_id(&username);
        navigator.push(Route::Meeting { id: meeting_id });
    };

    let open_github = |_| {
        let window = web_sys::window().expect("no global window exists");
        let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
    };

    let has_meeting_id = !meeting_id_input.read().is_empty();

    rsx! {
        div { class: "hero-container",
            BrowserCompatibility {}
            div { class: "floating-element floating-element-1" }
            div { class: "floating-element floating-element-2" }
            div { class: "floating-element floating-element-3" }

            // GitHub corner ribbon
            a {
                href: "https://github.com/security-union/videocall-rs",
                class: "github-corner",
                aria_label: "View source on GitHub",
                svg {
                    width: "80",
                    height: "80",
                    view_box: "0 0 250 250",
                    style: "fill:#7928CA; color:#0D131F; position: absolute; top: 0; border: 0; right: 0;",
                    aria_hidden: "true",
                    path { d: "M0,0 L115,115 L130,115 L142,142 L250,250 L250,0 Z" }
                    path {
                        d: "M128.3,109.0 C113.8,99.7 119.0,89.6 119.0,89.6 C122.0,82.7 120.5,78.6 120.5,78.6 C119.2,72.0 123.4,76.3 123.4,76.3 C127.3,80.9 125.5,87.3 125.5,87.3 C122.9,97.6 130.6,101.9 134.4,103.2",
                        fill: "currentColor",
                        style: "transform-origin: 130px 106px;",
                        class: "octo-arm"
                    }
                    path {
                        d: "M115.0,115.0 C114.9,115.1 118.7,116.5 119.8,115.4 L133.7,101.6 C136.9,99.2 139.9,98.4 142.2,98.6 C133.8,88.0 127.5,74.4 143.8,58.0 C148.5,53.4 154.0,51.2 159.7,51.0 C160.3,49.4 163.2,43.6 171.4,40.1 C171.4,40.1 176.1,42.5 178.8,56.2 C183.1,58.6 187.2,61.8 190.9,65.4 C194.5,69.0 197.7,73.2 200.1,77.6 C213.8,80.2 216.3,84.9 216.3,84.9 C212.7,93.1 206.9,96.0 205.4,96.6 C205.1,102.4 203.0,107.8 198.3,112.5 C181.9,128.9 168.3,122.5 157.7,114.1 C157.9,116.9 156.7,120.9 152.7,124.9 L141.0,136.5 C139.8,137.7 141.6,141.9 141.8,141.8 Z",
                        fill: "currentColor",
                        class: "octo-body"
                    }
                }
            }

            div { class: "hero-content",
                h1 { class: "hero-title text-center", "videocall.rs" }
                p { class: "hero-tagline text-center",
                    "Built with Rust"
                    span { class: "tagline-dot", " . " }
                    "WebTransport"
                    span { class: "tagline-dot", " . " }
                    "WASM"
                }

                div { class: "content-separator" }

                // Form section
                form {
                    class: "w-full mb-8 card-apple p-8",
                    onsubmit: onsubmit,
                    h3 { class: "text-center text-xl font-semibold mb-6 text-white/90", "Start or Join a Meeting" }
                    div { class: "space-y-6",
                        div {
                            label {
                                r#for: "username",
                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                "Username"
                            }
                            input {
                                id: "username",
                                class: TEXT_INPUT_CLASSES,
                                r#type: "text",
                                placeholder: "Enter your name",
                                required: true,
                                pattern: "^[a-zA-Z0-9_]*$",
                                autofocus: true,
                                value: "{username_input}",
                                oninput: move |evt| username_input.set(evt.value())
                            }
                        }

                        div {
                            label {
                                r#for: "meeting-id",
                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                "Meeting ID"
                            }
                            input {
                                id: "meeting-id",
                                class: TEXT_INPUT_CLASSES,
                                r#type: "text",
                                placeholder: "Enter meeting code",
                                required: true,
                                pattern: "^[a-zA-Z0-9_]*$",
                                value: "{meeting_id_input}",
                                oninput: move |evt| meeting_id_input.set(evt.value())
                            }
                            p { class: "text-sm text-foreground-subtle mt-2 ml-1", "Characters allowed: a-z, A-Z, 0-9, and _" }
                        }

                        if has_meeting_id {
                            div { class: "mt-4",
                                button {
                                    r#type: "submit",
                                    class: "btn-apple btn-primary w-full",
                                    span { class: "text-lg", "Start or Join Meeting" }
                                }
                            }
                        }

                        div { class: "mt-2",
                            button {
                                r#type: "button",
                                class: "btn-apple btn-secondary w-full flex items-center justify-center gap-2",
                                onclick: create_meeting,
                                span { class: "text-lg", "Create a New Meeting ID" }
                            }
                        }

                        // Active meetings list
                        MeetingsList {
                            on_select_meeting: move |meeting_id: String| {
                                meeting_id_input.set(meeting_id);
                            }
                        }
                    }
                }

                div { class: "content-separator" }

                div { class: "grid grid-cols-1 md:grid-cols-2 gap-8", style: "margin-top:1em",
                    div {
                        // Developer call-to-action
                        button {
                            onclick: open_github,
                            class: "secondary-button flex items-center justify-center mx-auto gap-2",
                            style: "margin-top:1em",
                            svg {
                                xmlns: "http://www.w3.org/2000/svg",
                                width: "18",
                                height: "18",
                                view_box: "0 0 24 24",
                                fill: "currentColor",
                                path { d: "M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" }
                            }
                            span { "Contribute on GitHub" }
                        }
                    }
                }
            }
        }
    }
}
