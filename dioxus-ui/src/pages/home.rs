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
    clear_display_name_from_storage, email_to_display_name, is_valid_meeting_id,
    load_display_name_from_storage, save_display_name_to_storage, validate_display_name,
    DisplayNameCtx, DISPLAY_NAME_MAX_LEN,
};
use crate::routing::Route;
use dioxus::prelude::*;
use dioxus::web::WebEventExt;
use videocall_theme::{
    icons, Button, ButtonType, ButtonVariant, Card, CardTitle, Field, Icon, Menu, MenuHeader,
    MenuItem, MenuSeparator, TextField, Toolbar,
};
use wasm_bindgen::JsCast;
use web_sys::HtmlInputElement;
use web_time::SystemTime;

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

    let mut meeting_id_ref = use_signal(|| None::<web_sys::Element>);
    let mut meeting_id_value = use_signal(String::new);
    let mut display_name_ctx = use_context::<DisplayNameCtx>();

    let existing_username: String = if let Some(name) = (display_name_ctx.0)() {
        name
    } else {
        load_display_name_from_storage().unwrap_or_default()
    };

    let mut username_value = use_signal(|| existing_username.clone());
    let mut username_error = use_signal(|| None::<String>);

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
                        if load_display_name_from_storage().is_none() {
                            // Use the profile name directly; only transform if it looks like an email
                            let display_name = if profile.name.contains('@') {
                                email_to_display_name(&profile.name)
                            } else {
                                profile.name.clone()
                            };
                            if let Ok(valid_name) = validate_display_name(&display_name) {
                                save_display_name_to_storage(&valid_name);
                                display_name_ctx.0.set(Some(valid_name.clone()));
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
            clear_display_name_from_storage();
            display_name_ctx.0.set(None);
            username_value.set(String::new());
        });
    };

    let get_meeting_id = move || -> String {
        meeting_id_ref()
            .and_then(|el| el.dyn_into::<HtmlInputElement>().ok())
            .map(|input| input.value())
            .unwrap_or_default()
    };

    let has_meeting_id = !meeting_id_value().is_empty();
    // Whichever action fits what has been typed is the prominent one.
    let (join_variant, create_variant) = if has_meeting_id {
        (ButtonVariant::Prominent, ButtonVariant::Default)
    } else {
        (ButtonVariant::Default, ButtonVariant::Prominent)
    };

    rsx! {
        div { class: "hero-container surface-grouped home-page",
            BrowserCompatibility {}

            // Top row: source link on the left, account menu on the right.
            Toolbar { class: "home-toolbar",
                a {
                    href: "https://github.com/security-union/videocall-rs",
                    class: "icon-btn icon-btn-filled",
                    aria_label: "View source on GitHub",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "20",
                        height: "20",
                        view_box: "0 0 24 24",
                        fill: "currentColor",
                        "aria-hidden": "true",
                        path { d: "M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" }
                    }
                }

                // Auth dropdown: shown only when OAuth is enabled.
                if oauth_enabled().unwrap_or(false) {
                    if let Some(profile) = user_profile() {
                        div { class: "auth-dropdown-container",
                            Button {
                                variant: ButtonVariant::Glass,
                                class: "auth-dropdown-trigger",
                                aria_expanded: show_dropdown(),
                                aria_haspopup: "menu",
                                onclick: move |_| {
                                    show_dropdown.set(!show_dropdown());
                                },
                                span { "{profile.name}" }
                                Icon { glyph: &icons::CHEVRON_DOWN, size: "16" }
                            }
                            if show_dropdown() {
                                // No menu roles today (they promise arrow-key
                                // navigation); `None` keeps it that way.
                                Menu { role: None::<&str>, class: "auth-dropdown-menu",
                                    MenuHeader {
                                        p { class: "text-headline", "{profile.name}" }
                                        p { class: "text-footnote text-secondary", "{profile.user_id}" }
                                    }
                                    MenuSeparator { role: None::<&str> }
                                    MenuItem {
                                        role: None::<&str>,
                                        onclick: move |_| { navigator.push(Route::Devices {}); },
                                        "Devices"
                                    }
                                    MenuItem {
                                        role: None::<&str>,
                                        destructive: true,
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
            }

            div { class: "hero-content",
                h1 { class: "hero-title", "videocall.rs" }
                p { class: "home-tagline",
                    "Built with Rust"
                    span { " \u{00b7} " }
                    "WebTransport"
                    span { " \u{00b7} " }
                    "WASM"
                }

                // Form section
                Card { class: "home-form-card",
                form {
                    class: "stack gap-4",
                    onsubmit: move |e| {
                        e.prevent_default();
                        username_error.set(None);
                        let username = username_value();
                        let meeting_id = get_meeting_id();
                        if meeting_id.is_empty() || !is_valid_meeting_id(&meeting_id) {
                            return;
                        }
                        match validate_display_name(&username) {
                            Ok(valid_name) => {
                                username_value.set(valid_name.clone());
                                save_display_name_to_storage(&valid_name);
                                (display_name_ctx.0).set(Some(valid_name.clone()));
                                matomo_logger::set_user_id(&valid_name);

                                spawn(async move {
                                    gloo_timers::future::TimeoutFuture::new(0).await;
                                    navigator.push(Route::Meeting { id: meeting_id });
                                });
                            }
                            Err(message) => {
                                username_error.set(Some(message));
                            }
                        }
                    },
                    CardTitle { "Start or Join a Meeting" }
                    // The hint stays up while an error shows, so both are
                    // children rather than Field's hint/error props.
                    Field { label: "Display Name", label_for: "username",
                        TextField {
                            id: "username",
                            r#type: "text",
                            placeholder: "Enter your display name",
                            required: true,
                            // Quoted: `autofocus` is both a global and an
                            // input attribute, so the bare name is ambiguous.
                            "autofocus": crate::platform::autofocus_allowed(),
                            maxlength: DISPLAY_NAME_MAX_LEN as i64,
                            value: "{username_value}",
                            invalid: username_error().is_some(),
                            oninput: move |e: Event<FormData>| {
                                username_value.set(e.value());
                                username_error.set(None);
                            },
                        }
                        p { class: "field-hint", "Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes" }
                        if let Some(err) = username_error() {
                            p { class: "field-error", role: "alert", "{err}" }
                        }
                    }
                    Field { label: "Meeting ID", label_for: "meeting-id",
                        // Raw: read and written through the mounted element.
                        input {
                            id: "meeting-id",
                            class: "input",
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
                        p { class: "field-hint", "Characters allowed: a-z, A-Z, 0-9, and _" }
                    }
                    Button {
                        button_type: ButtonType::Submit,
                        variant: join_variant,
                        block: true,
                        disabled: !has_meeting_id,
                        "Start or Join Meeting"
                    }
                    Button {
                        variant: create_variant,
                        block: true,
                        onclick: move |_| {
                            username_error.set(None);
                            let username = username_value();
                            match validate_display_name(&username) {
                                Ok(valid_name) => {
                                    let meeting_id = generate_meeting_id();
                                    username_value.set(valid_name.clone());
                                    save_display_name_to_storage(&valid_name);
                                    (display_name_ctx.0).set(Some(valid_name.clone()));
                                    matomo_logger::set_user_id(&valid_name);

                                    spawn(async move {
                                        gloo_timers::future::TimeoutFuture::new(0).await;
                                        navigator.push(Route::Meeting { id: meeting_id });
                                    });
                                }
                                Err(message) => {
                                    username_error.set(Some(message));
                                }
                            }
                        },
                        "Create a New Meeting ID"
                    }
                }
                }

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

                Button {
                    variant: ButtonVariant::Plain,
                    class: "home-contribute",
                    onclick: move |_| {
                        let window = web_sys::window().expect("no global window exists");
                        let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
                    },
                    "Contribute on GitHub"
                }
            }
        }
    }
}
