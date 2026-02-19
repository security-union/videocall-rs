use crate::components::attendants::AttendantsComponent;
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{e2ee_enabled, webtransport_enabled};
use crate::context::{
    is_valid_username, load_username_from_storage, save_username_to_storage, UsernameCtx,
};
use crate::meeting_api::{get_meeting_info, join_meeting, JoinError};
use gloo_timers::callback::Interval;
use web_sys::window;
use web_sys::{HtmlInputElement, KeyboardEvent};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::auth::{check_session, get_user_profile, logout, UserProfile};
use crate::constants::oauth_enabled;
use crate::routing::Route;

/// Meeting participant status from the API
#[derive(Clone, PartialEq, Debug)]
pub enum MeetingStatus {
    /// Initial state - haven't joined yet
    NotJoined,
    /// Joining in progress
    Joining,
    /// Waiting for the host to start the meeting
    WaitingForMeeting,
    /// In the waiting room, pending host admission
    Waiting,
    /// Admitted to the meeting
    Admitted {
        is_host: bool,
        host_display_name: Option<String>,
        /// Signed JWT room access token for connecting to the media server
        room_token: String,
    },
    /// Rejected by the host
    Rejected,
    /// Error occurred
    Error(String),
}

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

    // Meeting status state
    let meeting_status = use_state(|| MeetingStatus::NotJoined);
    let host_display_name = use_state(|| None::<String>);
    let current_user_email = use_state(|| None::<String>);
    // Track if user came from waiting room (should auto-join when admitted)
    let came_from_waiting_room = use_state(|| false);

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
                            // Redirect to login with returnTo parameter to preserve meeting URL
                            if let Some(win) = window() {
                                if let Ok(current_url) = win.location().href() {
                                    let login_url = format!(
                                        "/login?returnTo={}",
                                        urlencoding::encode(&current_url)
                                    );
                                    log::info!("Redirecting to: {login_url}");
                                    let _ = win.location().set_href(&login_url);
                                }
                            }
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

    // Poll for meeting activation when in WaitingForMeeting state
    {
        let meeting_id = props.id.clone();
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let current_user_email = current_user_email.clone();
        let came_from_waiting_room = came_from_waiting_room.clone();
        let input_value_state = input_value_state.clone();
        let current_status = (*meeting_status).clone();

        use_effect_with(current_status.clone(), move |status| {
            let interval: Option<Interval> = if *status == MeetingStatus::WaitingForMeeting {
                let meeting_id = meeting_id.clone();
                let meeting_status = meeting_status.clone();
                let host_display_name = host_display_name.clone();
                let current_user_email = current_user_email.clone();
                let came_from_waiting_room = came_from_waiting_room.clone();
                let display_name = (*input_value_state).clone();

                // Poll every 2 seconds to check if meeting becomes active
                Some(Interval::new(2000, move || {
                    let meeting_id = meeting_id.clone();
                    let meeting_status = meeting_status.clone();
                    let host_display_name = host_display_name.clone();
                    let current_user_email = current_user_email.clone();
                    let came_from_waiting_room = came_from_waiting_room.clone();
                    let display_name = display_name.clone();

                    wasm_bindgen_futures::spawn_local(async move {
                        // Check if meeting is now active
                        match get_meeting_info(&meeting_id).await {
                            Ok(info) => {
                                log::info!("Meeting state check: {}", info.state);
                                // Meeting is active when state is "active" (not "idle" or "ended")
                                if info.state == "active" {
                                    log::info!("Meeting is now active! Attempting to join...");
                                    // Try to join again
                                    match join_meeting(&meeting_id, Some(&display_name)).await {
                                        Ok(response) => {
                                            log::info!(
                                                "Join after meeting active: status={}, is_host={}",
                                                response.status,
                                                response.is_host
                                            );
                                            current_user_email.set(Some(response.email.clone()));

                                            let determined_host_display_name =
                                                info.host_display_name.clone();
                                            host_display_name
                                                .set(determined_host_display_name.clone());

                                            match response.status.as_str() {
                                                "admitted" => {
                                                    if let Some(token) = response.room_token {
                                                        meeting_status.set(
                                                            MeetingStatus::Admitted {
                                                                is_host: response.is_host,
                                                                host_display_name:
                                                                    determined_host_display_name,
                                                                room_token: token,
                                                            },
                                                        );
                                                    } else {
                                                        meeting_status.set(MeetingStatus::Error(
                                                            "Admitted but no room token received from server".to_string(),
                                                        ));
                                                    }
                                                }
                                                "waiting" => {
                                                    came_from_waiting_room.set(true);
                                                    meeting_status.set(MeetingStatus::Waiting);
                                                }
                                                "rejected" => {
                                                    meeting_status.set(MeetingStatus::Rejected);
                                                }
                                                _ => {
                                                    meeting_status.set(MeetingStatus::Error(
                                                        format!(
                                                            "Unknown status: {}",
                                                            response.status
                                                        ),
                                                    ));
                                                }
                                            }
                                        }
                                        Err(JoinError::MeetingNotActive) => {
                                            // Still not active, keep waiting
                                            log::info!("Meeting still not accepting joins, continuing to wait");
                                        }
                                        Err(e) => {
                                            log::error!("Error joining meeting: {e}");
                                            meeting_status.set(MeetingStatus::Error(e.to_string()));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                log::warn!("Error checking meeting status: {e}");
                                // Don't transition to error state, just keep polling
                            }
                        }
                    });
                }))
            } else {
                None
            };

            // Return cleanup function
            move || {
                drop(interval);
            }
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

    // Join meeting API call
    let on_join_meeting = {
        let meeting_id = props.id.clone();
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let current_user_email = current_user_email.clone();
        let input_value_state = input_value_state.clone();
        let came_from_waiting_room = came_from_waiting_room.clone();

        Callback::from(move |_| {
            let meeting_id = meeting_id.clone();
            let meeting_status = meeting_status.clone();
            let host_display_name = host_display_name.clone();
            let current_user_email = current_user_email.clone();
            let came_from_waiting_room = came_from_waiting_room.clone();
            // Get the username that the user entered
            let display_name = (*input_value_state).clone();

            meeting_status.set(MeetingStatus::Joining);

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting(&meeting_id, Some(&display_name)).await {
                    Ok(response) => {
                        log::info!(
                            "Join meeting success: status={}, is_host={}",
                            response.status,
                            response.is_host
                        );

                        // Store the current user's email from the response
                        current_user_email.set(Some(response.email.clone()));

                        // Get the host's display name from meeting info
                        let determined_host_display_name = if response.is_host {
                            // If we're the host, our display_name is the host_display_name
                            Some(display_name.clone())
                        } else {
                            // We need to get the meeting info to find the host's display name
                            match crate::meeting_api::get_meeting_info(&meeting_id).await {
                                Ok(info) => info.host_display_name,
                                Err(_) => None,
                            }
                        };
                        host_display_name.set(determined_host_display_name.clone());

                        match response.status.as_str() {
                            "admitted" => {
                                if let Some(token) = response.room_token {
                                    meeting_status.set(MeetingStatus::Admitted {
                                        is_host: response.is_host,
                                        host_display_name: determined_host_display_name,
                                        room_token: token,
                                    });
                                } else {
                                    meeting_status.set(MeetingStatus::Error(
                                        "Admitted but no room token received from server"
                                            .to_string(),
                                    ));
                                }
                            }
                            "waiting" => {
                                // Mark that user is going through waiting room
                                came_from_waiting_room.set(true);
                                meeting_status.set(MeetingStatus::Waiting);
                            }
                            "rejected" => {
                                meeting_status.set(MeetingStatus::Rejected);
                            }
                            _ => {
                                meeting_status.set(MeetingStatus::Error(format!(
                                    "Unknown status: {}",
                                    response.status
                                )));
                            }
                        }
                    }
                    Err(JoinError::MeetingNotActive) => {
                        log::info!("Meeting not active yet, waiting for host to start");
                        meeting_status.set(MeetingStatus::WaitingForMeeting);
                    }
                    Err(e) => {
                        log::error!("Join meeting error: {e}");
                        meeting_status.set(MeetingStatus::Error(e.to_string()));
                    }
                }
            });
        })
    };

    // Handle waiting room admission - receives the room_token from WaitingRoom
    let on_admitted = {
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let meeting_id = props.id.clone();

        Callback::from(move |room_token: String| {
            let meeting_status = meeting_status.clone();
            let host_display_name = host_display_name.clone();
            let meeting_id = meeting_id.clone();

            wasm_bindgen_futures::spawn_local(async move {
                // Get the host display name
                let determined_host_display_name =
                    match crate::meeting_api::get_meeting_info(&meeting_id).await {
                        Ok(info) => info.host_display_name,
                        Err(_) => None,
                    };
                host_display_name.set(determined_host_display_name.clone());

                meeting_status.set(MeetingStatus::Admitted {
                    is_host: false,
                    host_display_name: determined_host_display_name,
                    room_token,
                });
            });
        })
    };

    // Handle rejection
    let on_rejected = {
        let meeting_status = meeting_status.clone();
        Callback::from(move |_| {
            meeting_status.set(MeetingStatus::Rejected);
        })
    };

    // Handle cancel waiting - leave the meeting and go home
    let on_cancel_waiting = {
        let meeting_id = props.id.clone();
        Callback::from(move |_| {
            let meeting_id = meeting_id.clone();
            wasm_bindgen_futures::spawn_local(async move {
                // Call leave API to clear waiting status
                let _ = crate::meeting_api::leave_meeting(&meeting_id).await;
                if let Some(window) = web_sys::window() {
                    let _ = window.location().set_href("/");
                }
            });
        })
    };

    // Memoised submit handler (depends on username_state, input_value_state, error_state)
    let on_submit = {
        let username_state = username_state.clone();
        let input_value_state = input_value_state.clone();
        let error_state = error_state.clone();
        let on_join_meeting = on_join_meeting.clone();
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

                // Trigger join meeting API call
                on_join_meeting.emit(());
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
        let on_join_meeting = on_join_meeting.clone();
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
                    on_join_meeting.emit(());
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

    let current_meeting_status = (*meeting_status).clone();
    // let current_host_display_name = (*host_display_name).clone();
    let should_auto_join = *came_from_waiting_room;

    html! {
        <>
            {
                match (&maybe_username, &current_meeting_status) {
                    // User is admitted - show the meeting
                    (Some(username), MeetingStatus::Admitted { is_host, host_display_name, room_token }) => {
                        html! {
                            <AttendantsComponent
                                email={username.clone()}
                                id={props.id.clone()}
                                webtransport_enabled={webtransport_enabled().unwrap_or(false)}
                                e2ee_enabled={e2ee_enabled().unwrap_or(false)}
                                user_name={(*user_profile).as_ref().map(|p| p.name.clone())}
                                user_email={(*current_user_email).clone().or_else(|| (*user_profile).as_ref().map(|p| p.email.clone()))}
                                on_logout={Some(on_logout.clone())}
                                host_display_name={host_display_name.clone()}
                                auto_join={should_auto_join}
                                is_owner={*is_host}
                                room_token={room_token.clone()}
                            />
                        }
                    }

                    // User is waiting in the waiting room
                    (Some(_), MeetingStatus::Waiting) => {
                        html! {
                            <WaitingRoom
                                meeting_id={props.id.clone()}
                                on_admitted={on_admitted}
                                on_rejected={on_rejected}
                                on_cancel={on_cancel_waiting}
                            />
                        }
                    }

                    // Waiting for host to start the meeting
                    (Some(_), MeetingStatus::WaitingForMeeting) => {
                        html! {
                            <div class="waiting-room-container">
                                <div class="waiting-room-card card-apple">
                                    <div class="waiting-room-icon">
                                        <div class="loading-spinner" style="width: 48px; height: 48px;"></div>
                                    </div>
                                    <h2>{"Waiting for meeting to start"}</h2>
                                    <p class="waiting-room-message">
                                        {"The host hasn't started this meeting yet. You'll automatically join once the meeting begins."}
                                    </p>
                                    <button
                                        class="btn-apple btn-secondary"
                                        onclick={Callback::from(move |_| {
                                            if let Some(window) = web_sys::window() {
                                                let _ = window.location().set_href("/");
                                            }
                                        })}
                                    >
                                        {"Leave"}
                                    </button>
                                </div>
                            </div>
                        }
                    }

                    // User was rejected
                    (Some(_), MeetingStatus::Rejected) => {
                        html! {
                            <div class="rejected-container">
                                <div class="rejected-card card-apple">
                                    <svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="#ff6b6b" stroke-width="1.5">
                                        <circle cx="12" cy="12" r="10"></circle>
                                        <line x1="15" y1="9" x2="9" y2="15"></line>
                                        <line x1="9" y1="9" x2="15" y2="15"></line>
                                    </svg>
                                    <h2>{"Entry denied"}</h2>
                                    <p>{"The meeting host has denied your request to join."}</p>
                                    <button
                                        class="btn-apple btn-primary"
                                        onclick={Callback::from(move |_| {
                                            if let Some(window) = web_sys::window() {
                                                let _ = window.location().set_href("/");
                                            }
                                        })}
                                    >
                                        {"Return to Home"}
                                    </button>
                                </div>
                            </div>
                        }
                    }

                    // Error state
                    (Some(_), MeetingStatus::Error(error)) => {
                        html! {
                            <div class="error-container">
                                <div class="error-card card-apple">
                                    <svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="#ff9800" stroke-width="1.5">
                                        <circle cx="12" cy="12" r="10"></circle>
                                        <line x1="12" y1="8" x2="12" y2="12"></line>
                                        <line x1="12" y1="16" x2="12.01" y2="16"></line>
                                    </svg>
                                    <h2>{"Unable to join"}</h2>
                                    <p>{error}</p>
                                    <button
                                        class="btn-apple btn-primary"
                                        onclick={Callback::from(move |_| {
                                            if let Some(window) = web_sys::window() {
                                                let _ = window.location().set_href("/");
                                            }
                                        })}
                                    >
                                        {"Return to Home"}
                                    </button>
                                </div>
                            </div>
                        }
                    }

                    // Joining in progress
                    (Some(_), MeetingStatus::Joining) => {
                        html! {
                            <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                                <div class="loading-spinner" style="width: 40px; height: 40px; margin-bottom: 1rem;"></div>
                                <p style="color: white; font-size: 1rem;">{"Joining meeting..."}</p>
                            </div>
                        }
                    }

                    // Username prompt view (not joined yet or no username)
                    _ => {
                        html! {
                            <div id="username-prompt" class="username-prompt-container relative">
                                // User profile dropdown (only show if OAuth is enabled and profile is loaded)
                                {
                                    if oauth_enabled().unwrap_or(false) {
                                        if let Some(profile) = (*user_profile).clone() {
                                            html! {
                                                <div class="fixed top-4 right-4 z-50">
                                                    <button
                                                        type="button"
                                                        onclick={on_toggle_dropdown.clone()}
                                                        class="flex items-center gap-2 px-4 py-2 bg-gray-800 hover:bg-gray-700 rounded-lg text-white text-sm transition-colors
                                                              focus:outline-none focus-visible:ring-2 focus-visible:ring-blue-500/60 focus-visible:ring-offset-2 focus-visible:ring-offset-black"
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
                                    <h1>{"Choose a display name"}</h1>
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
            }
        </>
    }
}
