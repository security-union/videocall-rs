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

use crate::components::browser_compatibility::BrowserCompatibility;
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::Route;
use web_time::SystemTime;

const TEXT_INPUT_CLASSES: &str = "input-apple";

#[function_component(Home)]
pub fn home() -> Html {
    let navigator = use_navigator().unwrap();

    let username_ref = use_node_ref();
    let meeting_id_ref = use_node_ref();

    // Tab state for features section
    let active_tab = use_state(|| 0);

    let username_ctx = use_context::<UsernameCtx>().expect("Username context missing");

    let existing_username: String = if let Some(name) = &*username_ctx {
        name.clone()
    } else {
        load_username_from_storage().unwrap_or_default()
    };

    // If we already have a stored username, set the Matomo user id early
    use_effect_with((), {
        let uid = existing_username.clone();
        move |_| {
            if !uid.is_empty() {
                matomo_logger::set_user_id(&uid);
            }
            || ()
        }
    });

    let onsubmit = {
        let username_ref = username_ref.clone();
        let meeting_id_ref = meeting_id_ref.clone();
        let navigator = navigator.clone();
        let username_ctx = username_ctx.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let username = username_ref.cast::<HtmlInputElement>().unwrap().value();
            let meeting_id = meeting_id_ref.cast::<HtmlInputElement>().unwrap().value();
            if !is_valid_username(&username) || meeting_id.is_empty() {
                let _ = web_sys::window().unwrap().alert_with_message(
                    "Please provide a valid username and meeting id (a-z, A-Z, 0-9, _).",
                );
                return;
            }
            save_username_to_storage(&username);
            username_ctx.set(Some(username));
            // Set Matomo user id for attribution
            if let Some(name) = &*username_ctx {
                matomo_logger::set_user_id(name);
            }
            navigator.push(&Route::Meeting { id: meeting_id });
        })
    };

    // let open_github = Callback::from(|_| {
    //     let window = web_sys::window().expect("no global window exists");
    //     let _ = window.open_with_url("https://github.com/security-union/videocall-rs");
    // });

    let _set_active_tab = {
        let active_tab = active_tab.clone();
        Callback::from(move |tab: usize| {
            active_tab.set(tab);
        })
    };

    fn generate_meeting_id() -> String {
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("{millis:x}")
    }

    let create_meeting = {
        let username_ref = username_ref.clone();
        let navigator = navigator.clone();
        let username_ctx = username_ctx.clone();
        Callback::from(move |_| {
            let username = username_ref.cast::<HtmlInputElement>().unwrap().value();
            if !is_valid_username(&username) {
                let _ = web_sys::window()
                    .unwrap()
                    .alert_with_message("Please enter a valid username before creating a meeting.");
                return;
            }
            let meeting_id = generate_meeting_id();
            save_username_to_storage(&username);
            username_ctx.set(Some(username));
            // Set Matomo user id for attribution
            if let Some(name) = &*username_ctx {
                matomo_logger::set_user_id(name);
            }
            navigator.push(&Route::Meeting { id: meeting_id });
        })
    };

    // Main HTML structure
    html! {
        <div class="hero-container">
            <BrowserCompatibility/>

            <div class="floating-element floating-element-1"></div>
            <div class="floating-element floating-element-2"></div>
            <div class="floating-element floating-element-3"></div>

            <div class="hero-content">
                <h1 class="hero-title text-center">{ "Concept Car POC" }</h1>

                <div class="content-separator"></div>

                // Form section - moved to top for prominence
                <form {onsubmit} class="w-full mb-8 card-apple p-8">
                    <h3 class="text-center text-xl font-semibold mb-6 text-white/90">{"Start or Join a Meeting"}</h3>
                    <div class="space-y-6">
                        <div>
                            <label for="username" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Username"}</label>
                            <input
                                id="username"
                                class={TEXT_INPUT_CLASSES}
                                type="text"
                                placeholder="Enter your name"
                                ref={username_ref}
                                required={true}
                                pattern="^[a-zA-Z0-9_]*$"
                                autofocus={true}
                                value={existing_username.clone()}
                            />
                        </div>

                        <div>
                            <label for="meeting-id" class="block text-white/80 text-sm font-medium mb-2 ml-1">{"Meeting ID"}</label>
                            <input
                                id="meeting-id"
                                class={TEXT_INPUT_CLASSES}
                                type="text"
                                placeholder="Enter meeting code"
                                ref={meeting_id_ref}
                                required={true}
                                pattern="^[a-zA-Z0-9_]*$"
                            />
                            <p class="text-sm text-foreground-subtle mt-2 ml-1">{ "Characters allowed: a-z, A-Z, 0-9, and _" }</p>
                        </div>

                        <div class="mt-8">
                            <button type="submit" class="btn-apple btn-primary w-full flex items-center justify-center gap-2">
                                <span class="text-lg">{ "Join Meeting" }</span>
                                <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <path d="M5 12h14"></path>
                                    <path d="m12 5 7 7-7 7"></path>
                                </svg>
                            </button>
                        </div>

                        <div class="mt-2">
                            <button type="button" class="btn-apple btn-secondary w-full flex items-center justify-center gap-2" onclick={create_meeting.clone()}>
                                <span class="text-lg">{"Create New Meeting"}</span>
                            </button>
                        </div>
                    </div>
                </form>
            </div>
        </div>
    }
}
