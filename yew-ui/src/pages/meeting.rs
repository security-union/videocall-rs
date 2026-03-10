use crate::components::attendants::AttendantsComponent;
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{e2ee_enabled, webtransport_enabled};
use crate::context::{load_display_name_from_storage, DisplayNameCtx};
use crate::meeting_api::{join_meeting, JoinError};
use web_sys::window;
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
    /// Waiting for the host to start the meeting.
    /// Contains the observer token for receiving push notifications.
    WaitingForMeeting { observer_token: Option<String> },
    /// In the waiting room, pending host admission.
    /// Contains the observer token for receiving push notifications.
    Waiting { observer_token: Option<String> },
    /// Admitted to the meeting
    Admitted {
        is_host: bool,
        host_display_name: Option<String>,
        /// Authenticated user_id of the meeting host (from JWT/DB).
        host_user_id: Option<String>,
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
    // Retrieve the display name context (may be None on first load)
    let display_name_state = use_context::<DisplayNameCtx>()
        .expect("DisplayName context provider is missing – this is a bug");

    // Check authentication if OAuth is enabled (runtime check)
    let auth_checked = use_state(|| false);
    let navigator = use_navigator().expect("Navigator context missing");

    // User profile state (for displaying in dropdown when OAuth is enabled)
    let user_profile = use_state(|| None as Option<UserProfile>);

    // Meeting status state
    let meeting_status = use_state(|| MeetingStatus::NotJoined);
    let host_display_name = use_state(|| None::<String>);
    let host_user_id_state = use_state(|| None::<String>);
    let current_user_id = use_state(|| None::<String>);
    // Track if user came from waiting room (should auto-join when admitted)
    let came_from_waiting_room = use_state(|| false);

    // Retrieve previously cached display name (if any) either from the context
    // or from localStorage and use it as the initial value for the input.
    let initial_display_name: String = if let Some(name) = &*display_name_state {
        name.clone()
    } else {
        load_display_name_from_storage().unwrap_or_default()
    };

    // Keep an internal controlled value so that re-renders do NOT wipe what
    // the user is typing. This fixes the issue where the field kept
    // resetting to an empty string.
    let input_value_state = use_state(|| initial_display_name);

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

    // Observer WebSocket for meeting activation push notifications.
    // When in WaitingForMeeting state with an observer_token, create an
    // observer VideoCallClient that listens for meeting activation events
    // instead of polling.
    {
        let meeting_id = props.id.clone();
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let host_user_id_state = host_user_id_state.clone();
        let current_user_id = current_user_id.clone();
        let came_from_waiting_room = came_from_waiting_room.clone();
        let input_value_state = input_value_state.clone();
        let current_status = (*meeting_status).clone();

        use_effect_with(current_status.clone(), move |status| {
            use videocall_client::Callback as VcCallback;
            use videocall_client::{VideoCallClient, VideoCallClientOptions};

            let observer_token = match status {
                MeetingStatus::WaitingForMeeting { observer_token } => observer_token.clone(),
                _ => None,
            };

            let observer_client: Option<VideoCallClient> = observer_token.map(|token| {
                let meeting_id_for_rejoin = meeting_id.clone();
                let meeting_status_for_rejoin = meeting_status.clone();
                let host_display_name_for_rejoin = host_display_name.clone();
                let host_user_id_for_rejoin = host_user_id_state.clone();
                let current_user_id_for_rejoin = current_user_id.clone();
                let came_from_waiting_room_for_rejoin = came_from_waiting_room.clone();
                let display_name = (*input_value_state).clone();

                // Build observer WebSocket URLs using the observer token
                let ws_base = crate::constants::actix_websocket_base().unwrap_or_default();
                let observer_ws_urls: Vec<String> = ws_base
                    .split(',')
                    .map(|base| format!("{base}/lobby?token={token}"))
                    .collect();
                let wt_base = crate::constants::webtransport_host_base().unwrap_or_default();
                let observer_wt_urls: Vec<String> = wt_base
                    .split(',')
                    .map(|base| format!("{base}/lobby?token={token}"))
                    .collect();

                // Use the user's ID so the server can match
                // push-notification `target_user_id` to this observer client.
                let user_id_value = (*current_user_id).clone()
                    .unwrap_or_else(|| display_name.clone());

                let opts = VideoCallClientOptions {
                    user_id: user_id_value,
                    meeting_id: meeting_id.clone(),
                    websocket_urls: observer_ws_urls,
                    webtransport_urls: observer_wt_urls,
                    enable_e2ee: false,
                    enable_webtransport: false, // observer uses WebSocket only
                    on_connected: VcCallback::from(|_| {
                        log::info!("Observer WebSocket connected for meeting activation");
                    }),
                    on_connection_lost: VcCallback::from(|_| {
                        log::warn!("Observer WebSocket connection lost");
                    }),
                    on_peer_added: VcCallback::from(|_| {}),
                    on_peer_first_frame: VcCallback::from(|_| {}),
                    on_peer_removed: None,
                    get_peer_video_canvas_id: VcCallback::from(|id| id),
                    get_peer_screen_canvas_id: VcCallback::from(|id| id),
                    enable_diagnostics: false,
                    diagnostics_update_interval_ms: None,
                    enable_health_reporting: false,
                    health_reporting_interval_ms: None,
                    on_encoder_settings_update: None,
                    rtt_testing_period_ms: 2000,
                    rtt_probe_interval_ms: None,
                    on_meeting_info: None,
                    on_meeting_ended: None,
                    on_meeting_activated: Some(VcCallback::from(move |_| {
                        log::info!("Meeting activated via push notification, re-joining...");
                        let meeting_id = meeting_id_for_rejoin.clone();
                        let meeting_status = meeting_status_for_rejoin.clone();
                        let host_display_name = host_display_name_for_rejoin.clone();
                        let host_user_id_state = host_user_id_for_rejoin.clone();
                        let current_user_id = current_user_id_for_rejoin.clone();
                        let came_from_waiting_room = came_from_waiting_room_for_rejoin.clone();
                        let display_name = display_name.clone();

                        wasm_bindgen_futures::spawn_local(async move {
                            match join_meeting(&meeting_id, Some(&display_name)).await {
                                Ok(response) => {
                                    log::info!(
                                        "Re-join after activation: status={}, is_host={}",
                                        response.status,
                                        response.is_host
                                    );
                                    current_user_id.set(Some(response.user_id.clone()));
                                    let determined_host = response.host_display_name.clone();
                                    let determined_host_uid = response.host_user_id.clone();
                                    host_display_name.set(determined_host.clone());
                                    host_user_id_state.set(determined_host_uid.clone());

                                    match response.status.as_str() {
                                        "admitted" => {
                                            if let Some(token) = response.room_token {
                                                meeting_status.set(MeetingStatus::Admitted {
                                                    is_host: response.is_host,
                                                    host_display_name: determined_host,
                                                    host_user_id: determined_host_uid,
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
                                            came_from_waiting_room.set(true);
                                            meeting_status.set(MeetingStatus::Waiting {
                                                observer_token: response.observer_token.clone(),
                                            });
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
                                Err(e) => {
                                    log::error!("Error re-joining after activation: {e}");
                                    meeting_status.set(MeetingStatus::Error(e.to_string()));
                                }
                            }
                        });
                    })),
                    on_participant_admitted: None,
                    on_participant_rejected: None,
                    on_waiting_room_updated: None,
                    on_speaking_changed: None,
                    vad_threshold: None,
                    on_peer_left: None,
                    on_peer_joined: None,
                };

                let mut client = VideoCallClient::new(opts);
                if let Err(e) = client.connect() {
                    log::error!("Failed to connect observer client: {e}");
                }
                client
            });

            // Keep the observer client alive until cleanup
            move || {
                drop(observer_client);
            }
        });
    }

    // Join meeting API call — defined before the auto-join effect and early
    // return so that it is available to both.
    let on_join_meeting = {
        let meeting_id = props.id.clone();
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let host_user_id_state = host_user_id_state.clone();
        let current_user_id = current_user_id.clone();
        let input_value_state = input_value_state.clone();
        let came_from_waiting_room = came_from_waiting_room.clone();

        Callback::from(move |_| {
            let meeting_id = meeting_id.clone();
            let meeting_status = meeting_status.clone();
            let host_display_name = host_display_name.clone();
            let host_user_id_state = host_user_id_state.clone();
            let current_user_id = current_user_id.clone();
            let came_from_waiting_room = came_from_waiting_room.clone();
            // Get the display name that the user entered
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

                        // Store the current user's ID from the response
                        current_user_id.set(Some(response.user_id.clone()));

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
                        // Use the API-provided host_user_id; if the current user
                        // is the host and the API didn't include it, fall back to
                        // the user_id from the response.
                        let determined_host_uid = response.host_user_id.clone().or_else(|| {
                            if response.is_host {
                                Some(response.user_id.clone())
                            } else {
                                None
                            }
                        });
                        host_display_name.set(determined_host_display_name.clone());
                        host_user_id_state.set(determined_host_uid.clone());

                        match response.status.as_str() {
                            "admitted" => {
                                if let Some(token) = response.room_token {
                                    meeting_status.set(MeetingStatus::Admitted {
                                        is_host: response.is_host,
                                        host_display_name: determined_host_display_name,
                                        host_user_id: determined_host_uid,
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
                                meeting_status.set(MeetingStatus::Waiting {
                                    observer_token: response.observer_token.clone(),
                                });
                            }
                            "waiting_for_meeting" => {
                                log::info!(
                                    "Meeting not active yet, using observer for push notifications"
                                );
                                meeting_status.set(MeetingStatus::WaitingForMeeting {
                                    observer_token: response.observer_token.clone(),
                                });
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
                        // Legacy fallback: server returned 400 instead of the new status
                        log::info!("Meeting not active (legacy path), waiting for host to start");
                        meeting_status.set(MeetingStatus::WaitingForMeeting {
                            observer_token: None,
                        });
                    }
                    Err(e) => {
                        log::error!("Join meeting error: {e}");
                        meeting_status.set(MeetingStatus::Error(e.to_string()));
                    }
                }
            });
        })
    };

    // Auto-join: when the display name is already set and the meeting status is
    // NotJoined, trigger the join flow automatically so the user does not
    // have to interact with a redundant form.
    {
        let on_join_meeting = on_join_meeting.clone();
        let has_display_name = (*display_name_state).is_some();
        let is_not_joined = matches!(*meeting_status, MeetingStatus::NotJoined);
        let auto_join_attempted = use_mut_ref(|| false);
        use_effect_with((has_display_name, is_not_joined), {
            let auto_join_attempted = auto_join_attempted.clone();
            move |(has_display_name, is_not_joined)| {
                if *has_display_name && *is_not_joined && !*auto_join_attempted.borrow() {
                    *auto_join_attempted.borrow_mut() = true;
                    on_join_meeting.emit(());
                }
                || ()
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

    // Early return for auth check (AFTER all hooks are declared)
    if !*auth_checked && oauth_enabled().unwrap_or(false) {
        return html! {
            <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                <p style="color: white; font-size: 1rem;">{"Checking authentication..."}</p>
            </div>
        };
    }

    // Handle waiting room admission - receives the full join response from WaitingRoom
    let on_admitted = {
        let meeting_status = meeting_status.clone();
        let host_display_name = host_display_name.clone();
        let host_user_id_state = host_user_id_state.clone();

        Callback::from(move |response: crate::meeting_api::JoinMeetingResponse| {
            let determined_host = response.host_display_name.clone();
            let determined_host_uid = response.host_user_id.clone();
            let token = response.room_token.unwrap_or_default();
            host_display_name.set(determined_host.clone());
            host_user_id_state.set(determined_host_uid.clone());
            meeting_status.set(MeetingStatus::Admitted {
                is_host: false,
                host_display_name: determined_host,
                host_user_id: determined_host_uid,
                room_token: token,
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

    // Clone values for use inside html! macro
    let maybe_display_name = (*display_name_state).clone();

    let current_meeting_status = (*meeting_status).clone();
    let should_auto_join = *came_from_waiting_room;
    html! {
        <>
            {
                match (&maybe_display_name, &current_meeting_status) {
                    // User is admitted - show the meeting
                    (Some(display_name), MeetingStatus::Admitted { is_host, host_display_name, host_user_id, room_token }) => {
                        html! {
                            <AttendantsComponent
                                display_name={display_name.clone()}
                                id={props.id.clone()}
                                webtransport_enabled={webtransport_enabled().unwrap_or(false)}
                                e2ee_enabled={e2ee_enabled().unwrap_or(false)}
                                user_name={(*user_profile).as_ref().map(|p| p.name.clone())}
                                user_id={(*current_user_id).clone().or_else(|| (*user_profile).as_ref().map(|p| p.user_id.clone()))}
                                on_logout={Some(on_logout.clone())}
                                host_display_name={host_display_name.clone()}
                                host_user_id={host_user_id.clone()}
                                auto_join={should_auto_join}
                                is_owner={*is_host}
                                room_token={room_token.clone()}
                            />
                        }
                    }

                    // User is waiting in the waiting room
                    (Some(_), MeetingStatus::Waiting { observer_token }) => {
                        html! {
                            <WaitingRoom
                                meeting_id={props.id.clone()}
                                user_id={(*current_user_id).clone().unwrap_or_default()}
                                display_name={(*input_value_state).clone()}
                                observer_token={observer_token.clone()}
                                on_admitted={on_admitted}
                                on_rejected={on_rejected}
                                on_cancel={on_cancel_waiting}
                            />
                        }
                    }

                    // Waiting for host to start the meeting
                    (Some(_), MeetingStatus::WaitingForMeeting { .. }) => {
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
                        let display_name = maybe_display_name.as_deref().unwrap_or("...");
                        html! {
                            <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                                <div class="loading-spinner" style="width: 40px; height: 40px; margin-bottom: 1rem;"></div>
                                <p style="color: white; font-size: 1rem;">
                                    {"Joining as "}
                                    <strong>{display_name}</strong>
                                    {"..."}
                                </p>
                            </div>
                        }
                    }

                    // No display name set, or waiting for auto-join to fire
                    _ => {
                        if maybe_display_name.is_none() {
                            // Show inline display name prompt instead of redirecting
                            let on_display_name_submit = {
                                let input_value_state = input_value_state.clone();
                                let display_name_state = display_name_state.clone();
                                Callback::from(move |e: SubmitEvent| {
                                    e.prevent_default();
                                    let raw = (*input_value_state).clone();
                                    match crate::context::validate_display_name(&raw) {
                                        Ok(valid_name) => {
                                            crate::context::save_display_name_to_storage(&valid_name);
                                            display_name_state.set(Some(valid_name));
                                        }
                                        Err(msg) => {
                                            if let Some(w) = web_sys::window() {
                                                let _ = w.alert_with_message(&msg);
                                            }
                                        }
                                    }
                                })
                            };
                            let on_display_name_input = {
                                let input_value_state = input_value_state.clone();
                                Callback::from(move |e: InputEvent| {
                                    let input: web_sys::HtmlInputElement = e.target_unchecked_into();
                                    input_value_state.set(input.value());
                                })
                            };
                            html! {
                                <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                                    <div class="card-apple p-8" style="max-width: 400px; width: 90%;">
                                        <h2 style="color: white; text-align: center; margin-bottom: 0.5rem;">
                                            {"Enter your display name"}
                                        </h2>
                                        <p style="color: rgba(255,255,255,0.6); text-align: center; font-size: 0.875rem; margin-bottom: 1.5rem;">
                                            {"Choose a name to join the meeting"}
                                        </p>
                                        <form onsubmit={on_display_name_submit}>
                                            <input
                                                class="input-apple"
                                                type="text"
                                                placeholder="Enter your display name"
                                                required={true}
                                                autofocus={true}
                                                value={(*input_value_state).clone()}
                                                oninput={on_display_name_input}
                                            />
                                            <button
                                                type="submit"
                                                class="btn-apple btn-primary w-full"
                                                style="margin-top: 1rem;"
                                            >
                                                {"Join Meeting"}
                                            </button>
                                        </form>
                                    </div>
                                </div>
                            }
                        } else {
                            // Display name is set; the auto-join effect will fire momentarily
                            let display_name = maybe_display_name.as_deref().unwrap_or("Unknown");
                            html! {
                                <div style="position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: #000000;">
                                    <div class="loading-spinner" style="width: 40px; height: 40px; margin-bottom: 1rem;"></div>
                                    <p style="color: white; font-size: 1rem;">
                                        {"Joining as "}
                                        <strong>{display_name}</strong>
                                        {"..."}
                                    </p>
                                </div>
                            }
                        }
                    }
                }
            }
        </>
    }
}
