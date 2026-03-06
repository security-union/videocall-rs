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

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::login::{do_login, ProviderButton};
use crate::components::meetings_list::MeetingsList;
use crate::constants::oauth_enabled;
use crate::context::{
    clear_username_from_storage, email_to_display_name, load_username_from_storage,
    save_username_to_storage, validate_display_name, UsernameCtx,
};
use crate::routing::Route;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
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

    let mut username_ref = use_signal(|| None::<web_sys::Element>);
    let mut meeting_id_ref = use_signal(|| None::<web_sys::Element>);
    let mut meeting_id_value = use_signal(String::new);
    let mut username_ctx = use_context::<UsernameCtx>();

    let existing_username: String = if let Some(name) = (username_ctx.0)() {
        name
    } else {
        load_username_from_storage().unwrap_or_default()
    };

    let mut username_value = use_signal(|| existing_username.clone());

    // User profile state (for displaying auth info when OAuth is enabled)
    let mut user_profile = use_signal(|| None::<UserProfile>);

    // Dropdown toggle for auth menu
    let mut show_dropdown = use_signal(|| false);

    // Set Matomo user ID early if available
    use_effect({
        let uid = existing_username.clone();
        move || {
            if !uid.is_empty() {
                matomo_logger::set_user_id(&uid);
            }
        }
    });

    // Fetch user profile when OAuth is enabled
    use_effect(move || {
        if oauth_enabled().unwrap_or(false) {
            wasm_bindgen_futures::spawn_local(async move {
                // Only fetch profile if session is valid
                if check_session().await.is_ok() {
                    if let Ok(profile) = get_user_profile().await {
                        // Auto-set display name from auth profile if not already saved
                        if load_username_from_storage().is_none() {
                            // Use the profile name directly; only transform if it looks like an email
                            let display_name = if profile.name.contains('@') {
                                email_to_display_name(&profile.name)
                            } else {
                                profile.name.clone()
                            };
                            if let Ok(valid_name) = validate_display_name(&display_name) {
                                save_username_to_storage(&valid_name);
                                username_ctx.0.set(Some(valid_name.clone()));
                                username_value.set(valid_name);
                            }
                        }
                        user_profile.set(Some(profile));
                    }
                }
            });
        }
    });

    // Logout handler — clear profile state and display name, then stay on home page
    let on_logout = move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = logout().await;
            user_profile.set(None);
            clear_username_from_storage();
            username_ctx.0.set(None);
            username_value.set(String::new());
        });
    };

    let get_meeting_id = move || -> String {
        meeting_id_ref()
            .and_then(|el| el.dyn_into::<HtmlInputElement>().ok())
            .map(|input| input.value())
            .unwrap_or_default()
    };

    rsx! {
        div { class: "hero-container",
            BrowserCompatibility {}
            div { class: "floating-element floating-element-1" }
            div { class: "floating-element floating-element-2" }
            div { class: "floating-element floating-element-3" }

            // Auth dropdown — absolutely positioned in top-right of hero-container
            if oauth_enabled().unwrap_or(false) {
                if let Some(profile) = user_profile() {
                    div { class: "auth-dropdown-container",
                        button {
                            r#type: "button",
                            class: "auth-dropdown-trigger",
                            onclick: move |_| {
                                show_dropdown.set(!show_dropdown());
                            },
                            span { "{profile.name}" }
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
                                polyline { points: "6 9 12 15 18 9" }
                            }
                        }
                        if show_dropdown() {
                            div { class: "auth-dropdown-menu",
                                div { class: "auth-dropdown-header",
                                    p { class: "auth-dropdown-name", "{profile.name}" }
                                    p { class: "auth-dropdown-email", "{profile.email}" }
                                }
                                button {
                                    r#type: "button",
                                    class: "auth-dropdown-signout",
                                    onclick: on_logout,
                                    "Sign out"
                                }
                            }
                        }
                    }
                } else {
                    div { class: "auth-dropdown-container",
                        ProviderButton { onclick: move |_| do_login() }
                    }
                }
            }

            // GitHub corner ribbon
            a {
                href: "https://github.com/security-union/videocall-rs",
                class: "github-corner",
                aria_label: "View source on GitHub",
                svg {
                    width: "80",
                    height: "80",
                    view_box: "0 0 250 250",
                    style: "fill:#7928CA; color:#0D131F; position: absolute; top: 0; border: 0; left: 0; transform: scaleX(-1);",
                    "aria-hidden": "true",
                    path { d: "M0,0 L115,115 L130,115 L142,142 L250,250 L250,0 Z" }
                    path { d: "M128.3,109.0 C113.8,99.7 119.0,89.6 119.0,89.6 C122.0,82.7 120.5,78.6 120.5,78.6 C119.2,72.0 123.4,76.3 123.4,76.3 C127.3,80.9 125.5,87.3 125.5,87.3 C122.9,97.6 130.6,101.9 134.4,103.2", fill: "currentColor", style: "transform-origin: 130px 106px;", class: "octo-arm" }
                    path { d: "M115.0,115.0 C114.9,115.1 118.7,116.5 119.8,115.4 L133.7,101.6 C136.9,99.2 139.9,98.4 142.2,98.6 C133.8,88.0 127.5,74.4 143.8,58.0 C148.5,53.4 154.0,51.2 159.7,51.0 C160.3,49.4 163.2,43.6 171.4,40.1 C171.4,40.1 176.1,42.5 178.8,56.2 C183.1,58.6 187.2,61.8 190.9,65.4 C194.5,69.0 197.7,73.2 200.1,77.6 C213.8,80.2 216.3,84.9 216.3,84.9 C212.7,93.1 206.9,96.0 205.4,96.6 C205.1,102.4 203.0,107.8 198.3,112.5 C181.9,128.9 168.3,122.5 157.7,114.1 C157.9,116.9 156.7,120.9 152.7,124.9 L141.0,136.5 C139.8,137.7 141.6,141.9 141.8,141.8 Z", fill: "currentColor", class: "octo-body" }
                }
            }

            div { class: "hero-content",
                h1 { class: "hero-title text-center", "videocall.rs" }
                p { class: "hero-tagline text-center",
                    "Built with Rust"
                    span { class: "tagline-dot", " \u{00b7} " }
                    "WebTransport"
                    span { class: "tagline-dot", " \u{00b7} " }
                    "WASM"
                }

                div { class: "content-separator" }

                // Form section
                div { class: "w-full mb-8 card-apple p-8",
                form {
                    onsubmit: move |e| {
                        e.prevent_default();
                        let username = username_value();
                        let meeting_id = get_meeting_id();
                        if meeting_id.is_empty() {
                            let _ = web_sys::window().unwrap().alert_with_message(
                                "Please provide a meeting ID.",
                            );
                            return;
                        }
                        match validate_display_name(&username) {
                            Ok(valid_name) => {
                                save_username_to_storage(&valid_name);
                                (username_ctx.0).set(Some(valid_name));
                                if let Some(name) = (username_ctx.0)() {
                                    matomo_logger::set_user_id(&name);
                                }
                                navigator.push(Route::Meeting { id: meeting_id });
                            }
                            Err(message) => {
                                let _ = web_sys::window().unwrap().alert_with_message(&message);
                            }
                        }
                    },
                    h3 { class: "text-center text-xl font-semibold mb-6 text-white/90", "Start or Join a Meeting" }
                    div { class: "space-y-6",
                        div {
                            label { r#for: "username", class: "block text-white/80 text-sm font-medium mb-2 ml-1", "Display Name" }
                            input {
                                id: "username",
                                class: TEXT_INPUT_CLASSES,
                                r#type: "text",
                                placeholder: "Enter your display name",
                                required: true,
                                autofocus: true,
                                value: "{username_value}",
                                oninput: move |e: Event<FormData>| {
                                    username_value.set(e.value());
                                },
                                onmounted: move |evt| {
                                    if let Some(elem) = evt.try_as_web_event() {
                                        username_ref.set(Some(elem));
                                    }
                                },
                            }
                        }
                        div {
                            label { r#for: "meeting-id", class: "block text-white/80 text-sm font-medium mb-2 ml-1", "Meeting ID" }
                            input {
                                id: "meeting-id",
                                class: TEXT_INPUT_CLASSES,
                                r#type: "text",
                                placeholder: "Enter meeting code",
                                required: true,
                                pattern: "^[a-zA-Z0-9_]*$",
                                oninput: move |e: Event<FormData>| {
                                    meeting_id_value.set(e.value());
                                },
                                onmounted: move |evt| {
                                    if let Some(elem) = evt.try_as_web_event() {
                                        meeting_id_ref.set(Some(elem));
                                    }
                                },
                            }
                            p { class: "text-sm text-foreground-subtle mt-2 ml-1", "Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes" }
                        }
                        {
                            let has_meeting_id = !meeting_id_value().is_empty();
                            let join_btn_class = if has_meeting_id {
                                "btn-apple btn-primary w-full"
                            } else {
                                "btn-apple btn-secondary w-full"
                            };
                            rsx! {
                                div { class: "mt-4",
                                    button {
                                        r#type: "submit",
                                        class: join_btn_class,
                                        disabled: !has_meeting_id,
                                        span { class: "text-lg", "Start or Join Meeting" }
                                    }
                                }
                            }
                        }
                        div { class: "mt-2",
                            button {
                                r#type: "button",
                                class: {
                                    let has_meeting_id = !meeting_id_value().is_empty();
                                    if has_meeting_id {
                                        "btn-apple btn-secondary w-full flex items-center justify-center gap-2"
                                    } else {
                                        "btn-apple btn-primary w-full flex items-center justify-center gap-2"
                                    }
                                },
                                onclick: move |_| {
                                    let username = username_value();
                                    match validate_display_name(&username) {
                                        Ok(valid_name) => {
                                            let meeting_id = generate_meeting_id();
                                            save_username_to_storage(&valid_name);
                                            (username_ctx.0).set(Some(valid_name));
                                            if let Some(name) = (username_ctx.0)() {
                                                matomo_logger::set_user_id(&name);
                                            }
                                            navigator.push(Route::Meeting { id: meeting_id });
                                        }
                                        Err(message) => {
                                            let _ = web_sys::window().unwrap().alert_with_message(&message);
                                        }
                                    }
                                },
                                span { class: "text-lg", "Create a New Meeting ID" }
                            }
                        }
                    }
                }

                // Auth removed from here — now rendered as a fixed dropdown in the top-right

                // Active meetings list — only show when OAuth is disabled
                // or the user is authenticated (has a profile)
                if !oauth_enabled().unwrap_or(false) || user_profile().is_some() {
                    MeetingsList {
                        on_select_meeting: move |meeting_id: String| {
                            if let Some(el) = meeting_id_ref() {
                                if let Ok(input) = el.dyn_into::<HtmlInputElement>() {
                                    input.set_value(&meeting_id);
                                }
                            }
                            meeting_id_value.set(meeting_id);
                        },
                    }
                }
                }

                div { class: "content-separator" }

                div { class: "grid grid-cols-1 md:grid-cols-2 gap-8", style: "margin-top:1em",
                    div {
                        button {
                            onclick: move |_| {
                                let window = web_sys::window().expect("no global window exists");
                                let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
                            },
                            class: "secondary-button flex items-center justify-center mx-auto gap-2",
                            style: "margin-top:1em",
                            svg { xmlns: "http://www.w3.org/2000/svg", width: "18", height: "18", view_box: "0 0 24 24", fill: "currentColor",
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
