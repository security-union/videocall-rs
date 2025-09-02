use crate::components::attendants::AttendantsComponent;
use crate::constants::{diagnostics_enabled, e2ee_enabled, webtransport_enabled};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use web_sys::window;
use web_sys::{HtmlInputElement, KeyboardEvent};
use yew::prelude::*;

#[derive(Properties, PartialEq, Clone)]
pub struct MeetingPageProps {
    pub id: String,
}

#[function_component(MeetingPage)]
pub fn meeting_page(props: &MeetingPageProps) -> Html {
    // --- Hooks (must be called unconditionally) ---
    // Retrieve the username context (may be None on first load)
    let username_state =
        use_context::<UsernameCtx>().expect("Username context provider is missing – this is a bug");

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
                            enable_diagnostics={diagnostics_enabled().unwrap_or(true)}
                        />
                    }
                } else {
                    // --- Username prompt view ---
                    html! {
                        <div id="username-prompt" class="username-prompt-container">
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
