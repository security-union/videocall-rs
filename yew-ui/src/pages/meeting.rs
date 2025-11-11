use crate::components::attendants::AttendantsComponent;
use crate::constants::{e2ee_enabled, webtransport_enabled};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use web_sys::window;
use web_sys::{HtmlInputElement, KeyboardEvent};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::constants::oauth_enabled;
use crate::Route;

#[derive(Properties, PartialEq, Clone)]
pub struct MeetingPageProps {
    pub id: String,
}

#[function_component(MeetingPage)]
pub fn meeting_page(props: &MeetingPageProps) -> Html {
    // --- ALL Hooks MUST be declared first (unconditionally) ---
    // Retrieve the username context (may be None on first load)
    let username_state =
        use_context::<UsernameCtx>().expect("Username context provider is missing – this is a bug");

    // Check authentication if OAuth is enabled (runtime check)
    let auth_checked = use_state(|| false);
    let navigator = use_navigator().expect("Navigator context missing");

    // User profile state (for displaying in dropdown when OAuth is enabled)
    let user_profile = use_state(|| None as Option<UserProfile>);
    let show_dropdown = use_state(|| false);

    // Local state to track the current value inside the username input. We
    // initialise it from whatever is already stored (if any) so the field
    // is pre-filled instead of blanking out each time we reach this page.
    let error_state = use_state(|| None as Option<String>);

    // Retrieve previously cached username (if any) either from the context
    // or from localStorage and use it as the initial value for the input.
    let initial_username: String = if let Some(name) = &*username_state {
        name.clone()
    } else {
        load_username_from_storage().unwrap_or_default()
    };

    // Keep an internal controlled value so that re-renders do NOT wipe what
    // the user is typing. This fixes the issue where the field kept
    // resetting to an empty string.
    let input_value_state = use_state(|| initial_username);

    // Auth check effect
    {
        let auth_checked = auth_checked.clone();
        let navigator = navigator.clone();
        use_effect_with((), move |_| {
            log::info!("OAuth enabled check: {}", oauth_enabled().unwrap_or(false));
            if oauth_enabled().unwrap_or(false) {
                log::info!("Starting session check...");
                wasm_bindgen_futures::spawn_local(async move {
                    match check_session().await {
                        Ok(_) => {
                            log::info!("Session check passed! Setting auth_checked to true");
                            auth_checked.set(true);
                        }
                        Err(e) => {
                            log::warn!("No active session, redirecting to login. Error: {e:?}");
                            navigator.push(&Route::Login);
                        }
                    }
                });
            } else {
                log::info!("OAuth disabled, skipping auth check");
                auth_checked.set(true);
            }
            || ()
        });
    }

    // Fetch user profile after auth check passes (only when OAuth is enabled)
    {
        let user_profile = user_profile.clone();
        let auth_checked = *auth_checked;
        use_effect_with(auth_checked, move |auth_checked| {
            if *auth_checked && oauth_enabled().unwrap_or(false) {
                wasm_bindgen_futures::spawn_local(async move {
                    if let Ok(profile) = get_user_profile().await {
                        user_profile.set(Some(profile));
                    }
                });
            }
            || ()
        });
    }

    // Logout handler
    let on_logout = {
        let navigator = navigator.clone();
        Callback::from(move |_| {
            let navigator = navigator.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = logout().await;
                navigator.push(&Route::Login);
            });
        })
    };

    // Toggle dropdown
    let on_toggle_dropdown = {
        let show_dropdown = show_dropdown.clone();
        Callback::from(move |_| {
            show_dropdown.set(!*show_dropdown);
        })
    };

    // Early return for auth check (AFTER all hooks are declared)
    if !*auth_checked && oauth_enabled().unwrap_or(false) {
        return html! {
            <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                <p style="color: white; font-size: 1rem;">{"Checking authentication..."}</p>
            </div>
        };
    }

    // Memoised submit handler (depends on username_state, input_value_state, error_state)
    let on_submit = {
        let username_state = username_state.clone();
        let input_value_state = input_value_state.clone();
        let error_state = error_state.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            let value = (*input_value_state).clone();
            if is_valid_username(&value) {
                save_username_to_storage(&value);

                // Check if we are in the username-reset flow (flag set by the
                // "Change name" button). If so, trigger a full page reload
                // *before* creating a new connection. The page will boot
                // fresh, read the new cached username, and initiate a clean
                // connection — the old one is gone.
                if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
                    if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                        if flag == "1" {
                            let _ = storage.remove_item("vc_username_reset");
                            if let Some(win) = window() {
                                let _ = win.location().reload();
                            }
                            return; // skip state update – page is reloading
                        }
                    }
                }

                // Normal flow (first time entering username or via Home page)
                username_state.set(Some(value));
                error_state.set(None);
            } else {
                error_state.set(Some(
                    "Please enter a valid username (letters, numbers, underscore).".to_string(),
                ));
            }
        })
    };

    // Keep the local input value in sync with what the user types.
    let on_input = {
        let input_value_state = input_value_state.clone();
        Callback::from(move |e: InputEvent| {
            let input: HtmlInputElement = e.target_unchecked_into();
            input_value_state.set(input.value());
        })
    };

    // Handle the "Enter" key directly by triggering the form submission when
    // valid (so users can simply press Enter instead of clicking the button).
    let on_keydown = {
        let username_state = username_state.clone();
        let input_value_state = input_value_state.clone();
        let error_state = error_state.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                let value = (*input_value_state).clone();
                if is_valid_username(&value) {
                    save_username_to_storage(&value);

                    if let Some(storage) = window().and_then(|w| w.local_storage().ok().flatten()) {
                        if let Ok(Some(flag)) = storage.get_item("vc_username_reset") {
                            if flag == "1" {
                                let _ = storage.remove_item("vc_username_reset");
                                if let Some(win) = window() {
                                    let _ = win.location().reload();
                                }
                                e.prevent_default();
                                return;
                            }
                        }
                    }

                    username_state.set(Some(value));
                    error_state.set(None);
                } else {
                    error_state.set(Some(
                        "Please enter a valid username (letters, numbers, underscore).".to_string(),
                    ));
                }
                e.prevent_default();
            }
        })
    };

    // Clone values for use inside html! macro
    let maybe_username = (*username_state).clone();
    let error_html = if let Some(err) = &*error_state {
        html! { <p class="error">{ err }</p> }
    } else {
        html! {}
    };

    html! {
        <>
            {
                if let Some(username) = maybe_username {
                    // --- In-meeting view ---
                    html! {
                        <AttendantsComponent
                            email={username}
                            id={props.id.clone()}
                            webtransport_enabled={webtransport_enabled().unwrap_or(false)}
                            e2ee_enabled={e2ee_enabled().unwrap_or(false)}
                            user_name={(*user_profile).as_ref().map(|p| p.name.clone())}
                            user_email={(*user_profile).as_ref().map(|p| p.email.clone())}
                            on_logout={Some(on_logout.clone())}
                        />
                    }
                } else {
                    // --- Username prompt view ---
                    html! {
                        <div id="username-prompt" class="username-prompt-container relative">
                            // User profile dropdown (only show if OAuth is enabled and profile is loaded)
                            {
                                if oauth_enabled().unwrap_or(false) {
                                    if let Some(profile) = (*user_profile).clone() {
                                        html! {
                                            <div class="absolute top-4 right-4 z-50">
                                                <button
                                                    onclick={on_toggle_dropdown.clone()}
                                                    class="flex items-center gap-2 px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-white text-sm transition-colors"
                                                >
                                                    <span>{&profile.name}</span>
                                                    <svg class="w-4 h-4" fill="none" stroke="currentColor" viewBox="0 0 24 24">
                                                        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M19 9l-7 7-7-7" />
                                                    </svg>
                                                </button>

                                                {
                                                    if *show_dropdown {
                                                        html! {
                                                            <div class="absolute right-0 mt-2 w-56 bg-white rounded-lg shadow-lg border border-gray-200 py-1">
                                                                <div class="px-4 py-3 border-b border-gray-200">
                                                                    <p class="text-sm font-medium text-gray-900">{&profile.name}</p>
                                                                    <p class="text-xs text-gray-500 truncate">{&profile.email}</p>
                                                                </div>
                                                                <button
                                                                    onclick={on_logout.reform(|_| ())}
                                                                    class="w-full text-left px-4 py-2 text-sm text-red-600 hover:bg-red-50 transition-colors"
                                                                >
                                                                    {"Sign out"}
                                                                </button>
                                                            </div>
                                                        }
                                                    } else {
                                                        html! {}
                                                    }
                                                }
                                            </div>
                                        }
                                    } else {
                                        html! {}
                                    }
                                } else {
                                    html! {}
                                }
                            }

                            <form onsubmit={on_submit} class="username-form">
                                <h1>{"Choose a username"}</h1>
                                <input
                                    class="username-input"
                                    placeholder="Your name"
                                    pattern="^[a-zA-Z0-9_]*$"
                                    required=true
                                    autofocus=true
                                    onkeydown={on_keydown}
                                    oninput={on_input}
                                    value={(*input_value_state).clone()}
                                />
                                { error_html }
                                <button class="cta-button" type="submit">{"Continue"}</button>
                            </form>
                        </div>
                    }
                }
            }
        </>
    }
}
