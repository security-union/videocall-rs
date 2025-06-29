use crate::components::{attendants::AttendantsComponent, top_bar::TopBar};
use crate::constants::{E2EE_ENABLED, WEBTRANSPORT_ENABLED};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use wasm_bindgen::JsCast;
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

    // Even if we already have a username we still call the rest of the hooks
    // to keep the hooks order stable across renders.
    let input_ref = use_node_ref();
    let error_state = use_state(|| None as Option<String>);

    // Memoised submit handler (depends on username_state, input_ref, error_state)
    let on_submit = {
        let username_state = username_state.clone();
        let input_ref = input_ref.clone();
        let error_state = error_state.clone();
        Callback::from(move |e: SubmitEvent| {
            e.prevent_default();
            if let Some(input) = input_ref.cast::<HtmlInputElement>() {
                let value = input.value();
                if is_valid_username(&value) {
                    save_username_to_storage(&value);
                    username_state.set(Some(value));
                    error_state.set(None);
                } else {
                    error_state.set(Some(
                        "Please enter a valid username (letters, numbers, underscore).".to_string(),
                    ));
                }
            }
        })
    };

    let on_keydown = {
        let username_state = username_state.clone();
        let error_state = error_state.clone();
        Callback::from(move |e: KeyboardEvent| {
            if e.key() == "Enter" {
                if let Some(target) = e.target() {
                    if let Ok(input) = target.dyn_into::<HtmlInputElement>() {
                        let value = input.value();
                        if is_valid_username(&value) {
                            save_username_to_storage(&value);
                            username_state.set(Some(value));
                            error_state.set(None);
                        } else {
                            error_state.set(Some(
                                "Please enter a valid username (letters, numbers, underscore)."
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
        })
    };

    // Clone values for use inside html! macro
    let maybe_username = (*username_state).clone();
    let existing_username: String = if let Some(name) = &maybe_username {
        name.clone()
    } else {
        load_username_from_storage().unwrap_or_default()
    };
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
                        <>
                            <TopBar/>
                            <AttendantsComponent
                                email={username}
                                id={props.id.clone()}
                                webtransport_enabled={*WEBTRANSPORT_ENABLED}
                                e2ee_enabled={*E2EE_ENABLED}
                            />
                        </>
                    }
                } else {
                    // --- Username prompt view ---
                    html! {
                        <div id="username-prompt" class="username-prompt-container">
                            <form onsubmit={on_submit} class="username-form">
                                <h1>{"Choose a username"}</h1>
                                <input
                                    ref={input_ref}
                                    class="username-input"
                                    placeholder="Your name"
                                    pattern="^[a-zA-Z0-9_]*$"
                                    required=true
                                    autofocus=true
                                    onkeydown={on_keydown}
                                    value={existing_username.clone()}
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
