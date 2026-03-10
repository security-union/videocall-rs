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

use web_sys::HtmlInputElement;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::login::{build_login_callback, render_provider_button};
use crate::components::meetings_list::MeetingsList;
use crate::constants::oauth_enabled;
use crate::context::{
    clear_display_name_from_storage, email_to_display_name, is_valid_meeting_id,
    load_display_name_from_storage, save_display_name_to_storage, validate_display_name,
    DisplayNameCtx, DISPLAY_NAME_MAX_LEN,
};
use crate::routing::Route;
use web_time::SystemTime;

const TEXT_INPUT_CLASSES: &str = "input-apple";

#[function_component(Home)]
pub fn home() -> Html {
    let navigator = use_navigator().unwrap();

    let meeting_id_ref = use_node_ref();

    // Track meeting ID value for enabling/disabling the submit button
    let meeting_id_value = use_state(String::new);

    let display_name_ctx = use_context::<DisplayNameCtx>().expect("DisplayName context missing");

    let existing_display_name: String = if let Some(name) = &*display_name_ctx {
        name.clone()
    } else {
        load_display_name_from_storage().unwrap_or_default()
    };

    // Controlled display name value
    let display_name_value = use_state(|| existing_display_name.clone());

    // Inline error messages
    let display_name_error = use_state(|| None as Option<String>);
    let meeting_id_error = use_state(|| None as Option<String>);

    // User profile state (for displaying auth info when OAuth is enabled)
    let user_profile = use_state(|| None as Option<UserProfile>);

    // Dropdown toggle for auth menu
    let show_dropdown = use_state(|| false);

    // If we already have a stored display name, set the Matomo user id early
    use_effect_with((), {
        let uid = existing_display_name.clone();
        move |_| {
            if !uid.is_empty() {
                matomo_logger::set_user_id(&uid);
            }
            || ()
        }
    });

    // Fetch user profile when OAuth is enabled
    {
        let user_profile = user_profile.clone();
        let display_name_ctx = display_name_ctx.clone();
        use_effect_with((), move |_| {
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
                                    display_name_ctx.set(Some(valid_name));
                                }
                            }
                            user_profile.set(Some(profile));
                        }
                    }
                });
            }
            || ()
        });
    }

    // Logout handler — clear profile state and display name, then stay on home page
    let on_logout = {
        let user_profile = user_profile.clone();
        let display_name_ctx = display_name_ctx.clone();
        let display_name_value = display_name_value.clone();
        Callback::from(move |_: web_sys::MouseEvent| {
            let user_profile = user_profile.clone();
            let display_name_ctx = display_name_ctx.clone();
            let display_name_value = display_name_value.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = logout().await;
                user_profile.set(None);
                clear_display_name_from_storage();
                display_name_ctx.set(None);
                display_name_value.set(String::new());
            });
        })
    };

    let onsubmit = {
        let meeting_id_ref = meeting_id_ref.clone();
        let navigator = navigator.clone();
        let display_name_ctx = display_name_ctx.clone();

        let display_name_value = display_name_value.clone();
        let display_name_error = display_name_error.clone();
        let meeting_id_error = meeting_id_error.clone();

        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();

            let display_name_raw = (*display_name_value).clone();
            let meeting_id = meeting_id_ref.cast::<HtmlInputElement>().unwrap().value();

            // Reset errors
            display_name_error.set(None);
            meeting_id_error.set(None);

            // Validate display name (user-friendly)
            let display_name = match validate_display_name(&display_name_raw) {
                Ok(v) => v,
                Err(msg) => {
                    display_name_error.set(Some(msg));
                    return;
                }
            };

            display_name_value.set(display_name.clone());

            if meeting_id.is_empty() || !is_valid_meeting_id(&meeting_id) {
                meeting_id_error.set(Some(
                    "Please provide a valid meeting id (a-z, A-Z, 0-9, _).".to_string(),
                ));
                return;
            }

            save_display_name_to_storage(&display_name);
            display_name_ctx.set(Some(display_name.clone()));
            matomo_logger::set_user_id(&display_name);

            navigator.push(&Route::Meeting { id: meeting_id });
        })
    };

    // let open_github = Callback::from(|_| {
    //     let window = web_sys::window().expect("no global window exists");
    //     let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
    // });

    fn generate_meeting_id() -> String {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("{millis:x}")
    }

    let create_meeting = {
        let navigator = navigator.clone();
        let display_name_ctx = display_name_ctx.clone();

        let display_name_value = display_name_value.clone();
        let display_name_error = display_name_error.clone();
        let meeting_id_error = meeting_id_error.clone();

        Callback::from(move |_| {
            display_name_error.set(None);
            meeting_id_error.set(None);

            let display_name_raw = (*display_name_value).clone();

            let display_name = match validate_display_name(&display_name_raw) {
                Ok(v) => v,
                Err(msg) => {
                    display_name_error.set(Some(msg));
                    return;
                }
            };

            let meeting_id = generate_meeting_id();
            save_display_name_to_storage(&display_name);
            display_name_ctx.set(Some(display_name.clone()));
            matomo_logger::set_user_id(&display_name);

            navigator.push(&Route::Meeting { id: meeting_id });
        })
    };

    let has_meeting_id = !meeting_id_value.is_empty();
    let join_btn_class = if has_meeting_id {
        "btn-apple btn-primary w-full"
    } else {
        "btn-apple btn-secondary w-full"
    };
    let create_btn_class = if has_meeting_id {
        "btn-apple btn-secondary w-full flex items-center justify-center gap-2"
    } else {
        "btn-apple btn-primary w-full flex items-center justify-center gap-2"
    };

    let toggle_dropdown = {
        let show_dropdown = show_dropdown.clone();
        Callback::from(move |_: web_sys::MouseEvent| {
            show_dropdown.set(!*show_dropdown);
        })
    };

    html! {
        <div class="hero-container">
            <BrowserCompatibility/>
            <div class="floating-element floating-element-1"></div>
            <div class="floating-element floating-element-2"></div>
            <div class="floating-element floating-element-3"></div>

            // Auth dropdown — absolutely positioned in top-right of hero-container
            {
                if oauth_enabled().unwrap_or(false) {
                    if let Some(profile) = (*user_profile).as_ref() {
                        html! {
                            <div class="auth-dropdown-container">
                                <button
                                    type="button"
                                    class="auth-dropdown-trigger"
                                    onclick={toggle_dropdown.clone()}
                                >
                                    <span>{&profile.name}</span>
                                    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                        <polyline points="6 9 12 15 18 9"></polyline>
                                    </svg>
                                </button>
                                if *show_dropdown {
                                    <div class="auth-dropdown-menu">
                                        <div class="auth-dropdown-header">
                                            <p class="auth-dropdown-name">{&profile.name}</p>
                                            <p class="auth-dropdown-email">{&profile.user_id}</p>
                                        </div>
                                        <button
                                            type="button"
                                            class="auth-dropdown-signout"
                                            onclick={on_logout.clone()}
                                        >
                                            {"Sign out"}
                                        </button>
                                    </div>
                                }
                            </div>
                        }
                    } else {
                        html! {
                            <div class="auth-dropdown-container">
                                { render_provider_button(build_login_callback()) }
                            </div>
                        }
                    }
                } else {
                    html! {}
                }
            }

            // // GitHub corner ribbon
            // <a href="https://github.com/security-union/videocall-rs" class="github-corner" aria-label="View source on GitHub">
            //     <svg width="80" height="80" viewBox="0 0 250 250" style="fill:#7928CA; color:#0D131F; position: absolute; top: 0; border: 0; left: 0; transform: scaleX(-1);" aria-hidden="true">
            //         <path d="M0,0 L115,115 L130,115 L142,142 L250,250 L250,0 Z"></path>
            //         <path d="M128.3,109.0 C113.8,99.7 119.0,89.6 119.0,89.6 C122.0,82.7 120.5,78.6 120.5,78.6 C119.2,72.0 123.4,76.3 123.4,76.3 C127.3,80.9 125.5,87.3 125.5,87.3 C122.9,97.6 130.6,101.9 134.4,103.2" fill="currentColor" style="transform-origin: 130px 106px;" class="octo-arm"></path>
            //         <path d="M115.0,115.0 C114.9,115.1 118.7,116.5 119.8,115.4 L133.7,101.6 C136.9,99.2 139.9,98.4 142.2,98.6 C133.8,88.0 127.5,74.4 143.8,58.0 C148.5,53.4 154.0,51.2 159.7,51.0 C160.3,49.4 163.2,43.6 171.4,40.1 C171.4,40.1 176.1,42.5 178.8,56.2 C183.1,58.6 187.2,61.8 190.9,65.4 C194.5,69.0 197.7,73.2 200.1,77.6 C213.8,80.2 216.3,84.9 216.3,84.9 C212.7,93.1 206.9,96.0 205.4,96.6 C205.1,102.4 203.0,107.8 198.3,112.5 C181.9,128.9 168.3,122.5 157.7,114.1 C157.9,116.9 156.7,120.9 152.7,124.9 L141.0,136.5 C139.8,137.7 141.6,141.9 141.8,141.8 Z" fill="currentColor" class="octo-body"></path>
            //     </svg>
            // </a>

            <div class="hero-content">
                <h1 class="hero-title text-center">{ "Concept Car" }</h1>
                // <p class="hero-tagline text-center">
                //     {"Built with Rust"}
                //     <span class="tagline-dot">{" · "}</span>
                //     {"WebTransport"}
                //     <span class="tagline-dot">{" · "}</span>
                //     {"WASM"}
                // </p>

                <div class="content-separator"></div>

                // Form section - moved to top for prominence
                <div class="w-full mb-8 card-apple p-8">
                    <form {onsubmit}>
                        <h3 class="text-center text-xl font-semibold mb-6 text-white/90">{"Start or Join a Meeting"}</h3>
                        <div class="space-y-6">
                            <div>
                                <label for="username" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Display Name"}</label>
                                <input
                                    id="username"
                                    class={TEXT_INPUT_CLASSES}
                                    type="text"
                                    placeholder="Enter your display name"
                                    required={true}
                                    autofocus={true}
                                    maxlength={DISPLAY_NAME_MAX_LEN.to_string()}
                                    value={(*display_name_value).clone()}
                                    oninput={{
                                        let display_name_value = display_name_value.clone();
                                        let display_name_error = display_name_error.clone();
                                        Callback::from(move |e: InputEvent| {
                                            let input: HtmlInputElement = e.target_unchecked_into();
                                            display_name_value.set(input.value());
                                            display_name_error.set(None);
                                        })
                                    }}
                                />
                                <p class="text-sm text-foreground-subtle mt-2 ml-1">
                                    {"Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"}
                                </p>
                                {
                                    if let Some(err) = &*display_name_error {
                                        html! { <p class="text-sm mt-2 ml-1" style="color:#ff6b6b;">{err}</p> }
                                    } else {
                                        html! {}
                                    }
                                }
                            </div>

                            <div>
                                <label for="meeting-id" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Meeting ID"}</label>
                                <input
                                    id="meeting-id"
                                    class={TEXT_INPUT_CLASSES}
                                    type="text"
                                    placeholder="Enter meeting code"
                                    ref={meeting_id_ref.clone()}
                                    required={true}
                                    pattern="^[a-zA-Z0-9_]*$"
                                    oninput={
                                        let meeting_id_value = meeting_id_value.clone();
                                        Callback::from(move |e: InputEvent| {
                                            let input: HtmlInputElement = e.target_unchecked_into();
                                            meeting_id_value.set(input.value());
                                        })
                                    }
                                />
                                <p class="text-sm text-foreground-subtle mt-2 ml-1">{ "Characters allowed: a-z, A-Z, 0-9, and _" }</p>
                                {
                                    if let Some(err) = &*meeting_id_error {
                                        html! { <p class="text-sm mt-2 ml-1" style="color:#ff6b6b;">{err}</p> }
                                    } else {
                                        html! {}
                                    }
                                }
                            </div>

                            <div class="mt-4">
                                <button type="submit" class={join_btn_class} disabled={!has_meeting_id}>
                                    <span class="text-lg">{ "Start or Join Meeting" }</span>
                                </button>
                            </div>

                            <div class="mt-2">
                                <button type="button" class={create_btn_class} onclick={create_meeting.clone()}>
                                    <span class="text-lg">{"Create a New Meeting ID"}</span>
                                </button>
                            </div>
                        </div>
                    </form>

                    // Auth removed from here — now rendered as a fixed dropdown in the top-right

                    // Active meetings list — only show when OAuth is disabled
                    // or the user is authenticated (has a profile)
                    {
                        if !oauth_enabled().unwrap_or(false) || (*user_profile).is_some() {
                            html! {
                                <MeetingsList on_select_meeting={
                                    let meeting_id_ref = meeting_id_ref.clone();
                                    let meeting_id_value = meeting_id_value.clone();
                                    Callback::from(move |meeting_id: String| {
                                        if let Some(input) = meeting_id_ref.cast::<HtmlInputElement>() {
                                            input.set_value(&meeting_id);
                                        }
                                        meeting_id_value.set(meeting_id);
                                    })
                                } />
                            }
                        } else {
                            html! {}
                        }
                    }
                </div>

                <div class="content-separator"></div>

                // <div class="grid grid-cols-1 md:grid-cols-2 gap-8" style="margin-top:1em">
                //     <div>
                //         // Developer call-to-action
                //         <button
                //             onclick={open_github}
                //             class="secondary-button flex items-center justify-center mx-auto gap-2"
                //              style="margin-top:1em"
                //         >
                //             <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="currentColor">
                //                 <path d="M12 0c-6.626 0-12 5.373-12 12 0 5.302 3.438 9.8 8.207 11.387.599.111.793-.261.793-.577v-2.234c-3.338.726-4.033-1.416-4.033-1.416-.546-1.387-1.333-1.756-1.333-1.756-1.089-.745.083-.729.083-.729 1.205.084 1.839 1.237 1.839 1.237 1.07 1.834 2.807 1.304 3.492.997.107-.775.418-1.305.762-1.604-2.665-.305-5.467-1.334-5.467-5.931 0-1.311.469-2.381 1.236-3.221-.124-.303-.535-1.524.117-3.176 0 0 1.008-.322 3.301 1.23.957-.266 1.983-.399 3.003-.404 1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.653 1.653.242 2.874.118 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.624-5.479 5.921.43.372.823 1.102.823 2.222v3.293c0 .319.192.694.801.576 4.765-1.589 8.199-6.086 8.199-11.386 0-6.627-5.373-12-12-12z" />
                //             </svg>
                //             <span>{"Contribute on GitHub"}</span>
                //         </button>
                //     </div>
                // </div>
            </div>
        </div>
    }
}
