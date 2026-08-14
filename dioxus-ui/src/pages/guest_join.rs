// SPDX-License-Identifier: MIT OR Apache-2.0

//! Guest join page — allows unauthenticated users to join a meeting
//! without signing in with an OAuth provider. The guest only needs
//! to provide a display name.

use crate::components::attendants::AttendantsComponent;
use crate::components::browser_compatibility::BrowserCompatibility;
use crate::components::meeting_password_prompt::{
    next_prompt_state, password_prompt_reason, MeetingPasswordPrompt, PasswordPromptReason,
    PasswordPromptState,
};
use crate::components::waiting_room::WaitingRoom;
use crate::constants::{
    actix_websocket_base, e2ee_enabled, webtransport_enabled, webtransport_host_base,
};
use crate::context::{
    load_transport_preference_with_source, resolve_transport_config, save_display_name_to_storage,
    validate_display_name, DisplayNameCtx, TransportPreference, TransportPreferenceCtx,
    DISPLAY_NAME_MAX_LEN,
};
use crate::meeting_api::{join_meeting_as_guest, JoinMeetingResponse};
use crate::theme::color as theme_color;
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};

const TEXT_INPUT_CLASSES: &str = "input-apple";

/// Guest participant status — mirrors the authenticated MeetingStatus but
/// scoped to guest-specific transitions.
#[derive(Clone, PartialEq, Debug)]
enum GuestStatus {
    NotJoined,
    Joining,
    /// The server refused the join with one of the two issue-1613 password
    /// codes. The reason, the attempt counter and the in-flight flag live in
    /// the page's `password_prompt` signal, not here: this variant only decides
    /// which view is mounted, and mounting/unmounting it is what drops the
    /// entered plaintext.
    PasswordRequired,
    WaitingForMeeting {
        observer_token: String,
    },
    Waiting {
        observer_token: String,
    },
    Admitted {
        host_display_name: Option<String>,
        host_user_id: Option<String>,
        room_token: String,
        status_observer_token: String,
        waiting_room_enabled: bool,
        admitted_can_admit: bool,
        allow_guests: bool,
        recording_allowed_for_all: bool,
        chat_allowed_for_all: bool,
    },
    Rejected,
    Error(String),
}

// ---------------------------------------------------------------------------
// Extracted helpers
// ---------------------------------------------------------------------------

/// Map a join/status API response to a [`GuestStatus`] variant.
fn guest_status_from_join_response(
    response: &JoinMeetingResponse,
    fallback_status_observer_token: Option<String>,
) -> GuestStatus {
    let host_display_name = response.host_display_name.clone();
    let host_user_id = response.host_user_id.clone();

    match response.status.as_str() {
        "admitted" => {
            if let Some(token) = response.room_token.clone() {
                GuestStatus::Admitted {
                    host_display_name,
                    host_user_id,
                    room_token: token,
                    status_observer_token: resolve_status_observer_token(
                        response.observer_token.clone(),
                        fallback_status_observer_token,
                    ),
                    waiting_room_enabled: response.waiting_room_enabled,
                    admitted_can_admit: response.admitted_can_admit,
                    allow_guests: response.allow_guests,
                    recording_allowed_for_all: response.recording_allowed_for_all,
                    chat_allowed_for_all: response.chat_allowed_for_all,
                }
            } else {
                GuestStatus::Error("Admitted but no room token".to_string())
            }
        }
        "waiting_for_meeting" => GuestStatus::WaitingForMeeting {
            observer_token: response.observer_token.clone().unwrap_or_default(),
        },
        "waiting" => GuestStatus::Waiting {
            observer_token: response.observer_token.clone().unwrap_or_default(),
        },
        "rejected" => GuestStatus::Rejected,
        _ => GuestStatus::Error(format!("Unknown status: {}", response.status)),
    }
}

fn resolve_status_observer_token(
    observer_token: Option<String>,
    fallback_status_observer_token: Option<String>,
) -> String {
    observer_token
        .filter(|t| !t.is_empty())
        .or_else(|| fallback_status_observer_token.filter(|t| !t.is_empty()))
        .unwrap_or_default()
}

/// Apply a [`JoinMeetingResponse`] that is already known to represent admission
/// (e.g. from the waiting-room push). Sets host metadata signals and transitions
/// to [`GuestStatus::Admitted`].
fn handle_admitted(
    response: JoinMeetingResponse,
    fallback_status_observer_token: String,
    mut guest_status: Signal<GuestStatus>,
    mut host_display_name: Signal<Option<String>>,
    mut host_user_id: Signal<Option<String>>,
    mut observer_token_signal: Signal<Option<String>>,
) {
    let determined_host = response.host_display_name.clone();
    let determined_host_uid = response.host_user_id.clone();
    let token = response.room_token.unwrap_or_default();
    let status_observer_token = resolve_status_observer_token(
        response.observer_token,
        Some(fallback_status_observer_token),
    );
    host_display_name.set(determined_host.clone());
    host_user_id.set(determined_host_uid.clone());
    observer_token_signal.set(None);
    guest_status.set(GuestStatus::Admitted {
        host_display_name: determined_host,
        host_user_id: determined_host_uid,
        room_token: token,
        status_observer_token,
        waiting_room_enabled: response.waiting_room_enabled,
        admitted_can_admit: response.admitted_can_admit,
        allow_guests: response.allow_guests,
        recording_allowed_for_all: response.recording_allowed_for_all,
        chat_allowed_for_all: response.chat_allowed_for_all,
    });
}

/// Build and connect the observer [`VideoCallClient`] used while waiting for
/// the host to activate the meeting. Factored out of the component
/// `use_effect` to keep the reactive hook body minimal.
#[allow(clippy::too_many_arguments)]
fn start_observer_connection(
    observer_token: String,
    meeting_id: String,
    display_name: String,
    user_id: String,
    transport_pref: TransportPreference,
    mut observer_client: Signal<Option<VideoCallClient>>,
    mut guest_status: Signal<GuestStatus>,
    mut current_user_id: Signal<Option<String>>,
    mut host_display_name: Signal<Option<String>>,
    mut host_user_id: Signal<Option<String>>,
    mut observer_token_signal: Signal<Option<String>>,
    mut came_from_waiting_room: Signal<bool>,
    // Issue 1613: the meeting-activation re-join below has to clear the
    // password gate a second time, so it needs both the value this session
    // already had accepted and somewhere to report a fresh refusal.
    mut password_prompt: Signal<PasswordPromptState>,
    mut pending_password: Signal<Option<String>>,
) {
    let lobby_url = |base: &str| format!("{base}/lobby?token={observer_token}");
    let websocket_urls: Vec<String> = actix_websocket_base()
        .unwrap_or_default()
        .split(',')
        .map(&lobby_url)
        .collect();
    let webtransport_urls: Vec<String> = webtransport_host_base()
        .unwrap_or_default()
        .split(',')
        .map(&lobby_url)
        .collect();

    let (effective_wt_enabled, websocket_urls, webtransport_urls) = resolve_transport_config(
        transport_pref,
        webtransport_enabled().unwrap_or(false),
        websocket_urls,
        webtransport_urls,
    );

    // Issue #1745 PR2 (observability only): record the applied preference + its
    // provenance for the guest lobby OBSERVER client. `transport_pref` is the
    // value passed in from the context signal that filtered the lists above;
    // the source tag comes from the storage the signal was seeded from.
    let (_, pref_source) = load_transport_preference_with_source();
    log::info!(
        "Transport preference applied: pref={} source={} wt_urls={} ws_urls={}",
        transport_pref,
        pref_source,
        webtransport_urls.len(),
        websocket_urls.len()
    );

    let opts = VideoCallClientOptions {
        user_id,
        display_name: display_name.clone(),
        is_guest: true,
        meeting_id: meeting_id.clone(),
        websocket_urls,
        webtransport_urls,
        enable_e2ee: false,
        enable_webtransport: effective_wt_enabled,
        max_received_layer: crate::constants::max_received_layer(),
        skip_canvas_paint: crate::constants::skip_canvas_paint(),
        // Issue #2156: deployment CAMERA ladder for receiver-side READOUTS.
        camera_ladder_variant: crate::constants::camera_ladder_variant(),
        // Issue #1884: guest lobby OBSERVER client — no in-call reaction overlay,
        // so no reaction callback.
        on_reaction: None,
        on_raise_hand: None,
        // Issue 2136: this is an OBSERVER client. The relay's outbound
        // allowlist forwards only MEETING and SESSION_ASSIGNED to an
        // observer, so a MEETING_TIMER can never arrive here -- a
        // callback would be unreachable code, not a missing feature.
        // There is deliberately no timer in the waiting room.
        on_meeting_timer: None,
        on_connected: VcCallback::from(move |_| {
            log::info!("Guest observer connection established (waiting for meeting)");
        }),
        on_connection_lost: VcCallback::from(move |_| {
            log::warn!("Guest observer connection lost (waiting for meeting)");
        }),
        on_peer_added: VcCallback::noop(),
        on_peer_first_frame: VcCallback::noop(),
        on_peer_removed: None,
        on_peers_removed_batch: None,
        get_peer_video_canvas_id: VcCallback::from(|id| id),
        get_peer_screen_canvas_id: VcCallback::from(|id| id),
        enable_diagnostics: false,
        diagnostics_update_interval_ms: None,
        enable_health_reporting: false,
        health_reporting_interval_ms: None,
        on_encoder_settings_update: None,
        rtt_testing_period_ms: 3000,
        rtt_probe_interval_ms: None,
        on_meeting_info: None,
        on_meeting_ended: None,
        on_meeting_activated: Some(VcCallback::from({
            let meeting_id = meeting_id.clone();
            let fallback_status_observer_token = observer_token.clone();
            move |_| {
                log::info!("Guest: Meeting activated push received, re-joining...");
                let meeting_id = meeting_id.clone();
                let display_name = display_name.clone();
                let fallback_status_observer_token = fallback_status_observer_token.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    observer_token_signal.set(None);

                    // Issue 1613: this re-join goes through the password gate
                    // AGAIN. The server verifies the password before the
                    // "waiting_for_meeting" early return, so clearing it once
                    // to reach the lobby did not clear it for the re-join.
                    // Replay the value this session already had accepted so the
                    // guest is not re-prompted the instant the host starts the
                    // meeting — quite possibly while they are away from the
                    // keyboard. `try_peek` because this callback runs from a
                    // WebSocket message handler outside the Dioxus runtime,
                    // where `peek()` (= `try_peek().unwrap()`) would panic on a
                    // scope that has since been dropped.
                    let password = pending_password
                        .try_peek()
                        .ok()
                        .and_then(|value| value.clone());

                    match join_meeting_as_guest(&meeting_id, &display_name, password.as_deref())
                        .await
                    {
                        Ok(response) => {
                            current_user_id.set(Some(response.user_id.clone()));
                            host_display_name.set(response.host_display_name.clone());
                            host_user_id.set(response.host_user_id.clone());
                            let status = guest_status_from_join_response(
                                &response,
                                Some(fallback_status_observer_token),
                            );
                            if matches!(&status, GuestStatus::Waiting { .. }) {
                                came_from_waiting_room.set(true);
                            }
                            // The meeting is active now, so no later re-join
                            // needs the password: drop it.
                            if !matches!(&status, GuestStatus::WaitingForMeeting { .. }) {
                                pending_password.set(None);
                            }
                            guest_status.set(status);
                        }
                        Err(e) => match password_prompt_reason(&e, password.is_some()) {
                            // The host changed (or added) the password while we
                            // waited, or the server throttled/shed the replay.
                            // Re-prompt instead of dead-ending on an error card
                            // with no way back in.
                            Some(reason) => {
                                let supplied = password.is_some();
                                let current = password_prompt
                                    .try_peek()
                                    .map(|state| *state)
                                    .unwrap_or_else(|_| PasswordPromptState::opened(reason));
                                password_prompt.set(next_prompt_state(current, reason, supplied));
                                pending_password.set(None);
                                guest_status.set(GuestStatus::PasswordRequired);
                            }
                            None => {
                                pending_password.set(None);
                                guest_status.set(GuestStatus::Error(e.to_string()));
                            }
                        },
                    }
                });
            }
        })),
        on_participant_admitted: None,
        on_participant_rejected: None,
        on_waiting_room_updated: None,
        on_meeting_settings_updated: None,
        on_host_mute: None,
        on_host_disable_video: None,
        on_participant_kicked: None,
        on_host_granted: None,
        on_host_revoked: None,
        on_peer_event: None,
        on_speaking_changed: None,
        on_audio_level_changed: None,
        vad_threshold: None,
        on_peer_left: None,
        on_peer_joined: None,
        on_display_name_changed: None,
        decode_media: false,
        // Guest observer client: short-lived, recovery is via meeting
        // activation push, not re-election. No post-rebase retry.
        allow_post_rebase_retry: false,
        // Observer mode (guest pre-admission): no refresh callback needed.
        // Observers don't trigger the watchdog re-election path that
        // consumes the callback (their session lifetime is bounded by the
        // meeting state — admission or activation push — not by RTT
        // degradation), so leaving this `None` is the right behaviour.
        // The Phase 3 / AUTH-2 refresh path is for full participants
        // whose JWT might outlive a long-running meeting.
        refresh_room_token_callback: None,
    };

    let mut client = VideoCallClient::new(opts);
    if let Err(e) = client.connect() {
        log::error!("Failed to connect guest observer client: {e}");
    }
    observer_client.set(Some(client));
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

#[component]
pub fn GuestJoinPage(id: String) -> Element {
    let mut display_name_ctx = use_context::<DisplayNameCtx>();
    let mut guest_status = use_signal(|| GuestStatus::NotJoined);
    let mut host_display_name = use_signal(|| None::<String>);
    let mut host_user_id = use_signal(|| None::<String>);
    let mut current_user_id = use_signal(|| None::<String>);
    let mut input_value = use_signal(String::new);
    let mut input_error = use_signal(|| None::<String>);
    let mut observer_token_signal = use_signal(|| None::<String>);
    let mut came_from_waiting_room = use_signal(|| false);

    // Issue 1613 — meeting password.
    //
    // `password_prompt` carries what the prompt renders; `GuestStatus::PasswordRequired`
    // decides whether it is mounted at all. The initial value is never shown —
    // nothing reads it until a 403 sets both.
    let mut password_prompt =
        use_signal(|| PasswordPromptState::opened(PasswordPromptReason::Required));
    // The accepted password, held ONLY for as long as a later request in this
    // same join can still need it (the meeting-activation re-join out of
    // `WaitingForMeeting`). In memory only: never storage, cookie, URL or log.
    let mut pending_password = use_signal(|| None::<String>);

    // When WaitingForMeeting, create an observer WebSocket client that receives
    // a push notification when the host activates the meeting, matching the
    // authenticated flow in meeting.rs.
    let mut observer_client = use_signal(|| None::<VideoCallClient>);
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    {
        let meeting_id = id.clone();
        use_effect(move || {
            let observer_token = match observer_token_signal() {
                Some(t) if !t.is_empty() => t,
                _ => {
                    if let Some(client) = observer_client.write().take() {
                        let _ = client.disconnect();
                    }
                    return;
                }
            };

            let display_name = input_value();
            let user_id_for_client = current_user_id().unwrap_or(display_name.clone());

            start_observer_connection(
                observer_token,
                meeting_id.clone(),
                display_name,
                user_id_for_client,
                (transport_pref_ctx.0)(),
                observer_client,
                guest_status,
                current_user_id,
                host_display_name,
                host_user_id,
                observer_token_signal,
                came_from_waiting_room,
                password_prompt,
                pending_password,
            );
        });
    }

    // Join as guest handler.
    //
    // Issue 1613: `password` is `None` for the first attempt and `Some(_)` for
    // every retry driven by the prompt. Everything else about the attempt —
    // meeting id, display name, the observer-token fallback — is recomputed
    // from the same signals, so a retry re-issues an identical join and the
    // user loses nothing by getting the password wrong.
    let on_join_guest = {
        let meeting_id = id.clone();
        move |password: Option<String>| {
            let meeting_id = meeting_id.clone();
            let display_name = input_value();
            let fallback_status_observer_token = match guest_status() {
                GuestStatus::Waiting { observer_token }
                | GuestStatus::WaitingForMeeting { observer_token } => Some(observer_token),
                GuestStatus::Admitted {
                    status_observer_token,
                    ..
                } => Some(status_observer_token),
                _ => None,
            };
            let supplied_password = password.is_some();
            if supplied_password {
                // Keep the prompt mounted (and its field read-only) while the
                // attempt is in flight, rather than swapping in the spinner and
                // remounting it on failure. The peek is bound out first so its
                // read guard is dropped before the write — `set` while a `peek`
                // guard is live is a borrow conflict, not a no-op.
                let in_flight = password_prompt.peek().submitted();
                password_prompt.set(in_flight);
            } else {
                guest_status.set(GuestStatus::Joining);
            }

            wasm_bindgen_futures::spawn_local(async move {
                match join_meeting_as_guest(&meeting_id, &display_name, password.as_deref()).await {
                    Ok(response) => {
                        current_user_id.set(Some(response.user_id.clone()));
                        host_display_name.set(response.host_display_name.clone());
                        host_user_id.set(response.host_user_id.clone());
                        let status = guest_status_from_join_response(
                            &response,
                            fallback_status_observer_token,
                        );
                        match &status {
                            GuestStatus::WaitingForMeeting { observer_token } => {
                                observer_token_signal.set(Some(observer_token.clone()));
                                // Issue 1613: the activation re-join out of this
                                // state hits the password gate again, so this is
                                // the ONE outcome that keeps the value.
                                pending_password.set(password);
                            }
                            GuestStatus::Waiting { .. } => {
                                observer_token_signal.set(None);
                                came_from_waiting_room.set(true);
                                pending_password.set(None);
                            }
                            _ => {
                                observer_token_signal.set(None);
                                pending_password.set(None);
                            }
                        }
                        guest_status.set(status);
                    }
                    Err(e) => match password_prompt_reason(&e, supplied_password) {
                        Some(reason) => {
                            // Issue 1613. Note this is driven by the server's
                            // 403, not by the meeting's `has_password` flag —
                            // see `components::meeting_password_prompt`.
                            observer_token_signal.set(None);
                            pending_password.set(None);
                            let current = *password_prompt.peek();
                            password_prompt.set(next_prompt_state(
                                current,
                                reason,
                                supplied_password,
                            ));
                            guest_status.set(GuestStatus::PasswordRequired);
                        }
                        None => {
                            observer_token_signal.set(None);
                            pending_password.set(None);
                            guest_status.set(GuestStatus::Error(e.to_string()));
                        }
                    },
                }
            });
        }
    };

    // Handle waiting room admission
    let on_admitted = {
        move |status: JoinMeetingResponse| {
            let fallback_status_observer_token = match guest_status() {
                GuestStatus::Waiting { observer_token }
                | GuestStatus::WaitingForMeeting { observer_token } => observer_token,
                GuestStatus::Admitted {
                    status_observer_token,
                    ..
                } => status_observer_token,
                _ => String::new(),
            };
            handle_admitted(
                status,
                fallback_status_observer_token,
                guest_status,
                host_display_name,
                host_user_id,
                observer_token_signal,
            );
        }
    };

    let on_rejected = move |_| {
        observer_token_signal.set(None);
        crate::auth::clear_guest_session_id();
        guest_status.set(GuestStatus::Rejected);
    };

    let on_cancel_waiting = {
        let meeting_id = id.clone();
        move |_| {
            let meeting_id = meeting_id.clone();
            let token = observer_token_signal().unwrap_or_default();
            wasm_bindgen_futures::spawn_local(async move {
                let _ = crate::meeting_api::leave_meeting_as_guest(&meeting_id, &token).await;
                if let Some(w) = web_sys::window() {
                    let _ = w.location().set_href("/");
                }
            });
        }
    };

    let current_guest_status = guest_status();
    let should_auto_join = came_from_waiting_room();
    let display_name_for_render = input_value();

    rsx! {
        match &current_guest_status {
            // Admitted — show the meeting
            GuestStatus::Admitted { host_display_name, host_user_id, room_token, status_observer_token, waiting_room_enabled, admitted_can_admit, allow_guests, recording_allowed_for_all, chat_allowed_for_all } => rsx! {
                AttendantsComponent {
                    display_name: display_name_for_render.clone(),
                    id: id.clone(),
                    e2ee_enabled: e2ee_enabled().unwrap_or(false),
                    user_id: current_user_id(),
                    host_display_name: host_display_name.clone(),
                    host_user_id: host_user_id.clone(),
                    auto_join: should_auto_join,
                    is_owner: false,
                    is_guest: true,
                    room_token: room_token.clone(),
                    status_observer_token: status_observer_token.clone(),
                    waiting_room_enabled: *waiting_room_enabled,
                    admitted_can_admit: *admitted_can_admit,
                    allow_guests: *allow_guests,
                    recording_allowed_for_all: *recording_allowed_for_all,
                    chat_allowed_for_all: *chat_allowed_for_all,
                }
            },

            // Waiting room
            GuestStatus::Waiting { observer_token } => rsx! {
                WaitingRoom {
                    meeting_id: id.clone(),
                    user_id: current_user_id().unwrap_or_default(),
                    display_name: display_name_for_render.clone(),
                    observer_token: observer_token.clone(),
                    is_guest: true,
                    on_admitted: on_admitted,
                    on_rejected: on_rejected,
                    on_cancel: on_cancel_waiting,
                }
            },

            // Issue 1613 — the meeting is password-protected and the server
            // refused this join. Replaces the guest form so nothing else on the
            // page competes for focus (which is what makes `aria-modal` on the
            // prompt's card truthful).
            GuestStatus::PasswordRequired => {
                let mut on_join = on_join_guest.clone();
                rsx! {
                    MeetingPasswordPrompt {
                        state: password_prompt,
                        // Short on purpose: these two buttons are `flex: 1` in a
                        // 400px card, so each gets ~170px. "Use a different
                        // name" wrapped to three lines at every common phone
                        // width. The CSS `min-height` now contains a wrap
                        // safely; this keeps the common case to one line.
                        cancel_label: "Change name",
                        on_submit: move |password: String| on_join(Some(password)),
                        // Back to the guest form with the typed name intact.
                        // Focus follows without any imperative step here: this
                        // arm and the `NotJoined` arm are different subtrees, so
                        // the form remounts and the `onmounted` handler on
                        // `#guest-name` (below) moves focus into it. Focus is
                        // never dropped to `<body>`.
                        on_cancel: move |_| guest_status.set(GuestStatus::NotJoined),
                    }
                }
            },

            // Waiting for host to start
            GuestStatus::WaitingForMeeting { .. } => rsx! {
                div { class: "waiting-room-container",
                    div { class: "waiting-room-card card-apple",
                        div { class: "waiting-room-icon",
                            div { class: "loading-spinner", style: "width: 48px; height: 48px;" }
                        }
                        h2 { "Waiting for meeting to start" }
                        p { class: "waiting-room-message",
                            "The host hasn't started this meeting yet. You'll automatically join once the meeting begins."
                        }
                        button {
                            class: "btn-apple btn-secondary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() {
                                    let _ = w.location().set_href("/");
                                }
                            },
                            "Leave"
                        }
                    }
                }
            },

            // Rejected
            GuestStatus::Rejected => rsx! {
                div { class: "rejected-container",
                    div { class: "rejected-card card-apple",
                        svg { xmlns: "http://www.w3.org/2000/svg", width: "64", height: "64", view_box: "0 0 24 24", fill: "none", stroke: "#ff6b6b", stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "15", y1: "9", x2: "9", y2: "15" }
                            line { x1: "9", y1: "9", x2: "15", y2: "15" }
                        }
                        h2 { "Entry denied" }
                        p { "The meeting host has denied your request to join." }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() { let _ = w.location().set_href("/"); }
                            },
                            "Return to Home"
                        }
                    }
                }
            },

            // Error
            GuestStatus::Error(error) => rsx! {
                div { class: "error-container",
                    div { class: "error-card card-apple",
                        svg { xmlns: "http://www.w3.org/2000/svg", width: "64", height: "64", view_box: "0 0 24 24", fill: "none", stroke: "#ff9800", stroke_width: "1.5",
                            circle { cx: "12", cy: "12", r: "10" }
                            line { x1: "12", y1: "8", x2: "12", y2: "12" }
                            line { x1: "12", y1: "16", x2: "12.01", y2: "16" }
                        }
                        h2 { "Unable to join" }
                        p { "{error}" }
                        button {
                            class: "btn-apple btn-primary",
                            onclick: move |_| {
                                if let Some(w) = web_sys::window() { let _ = w.location().set_href("/"); }
                            },
                            "Return to Home"
                        }
                    }
                }
            },

            // Joining in progress
            GuestStatus::Joining => {
                let name = input_value();
                rsx! {
                    div { style: "position: fixed; top: 0; left: 0; width: 100vw; height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; background: {theme_color::BG};",
                        div { class: "loading-spinner", style: "width: 40px; height: 40px; margin-bottom: var(--space-4);" }
                        p { style: "color: {theme_color::TEXT_PRIMARY}; font-size: var(--fs-7);",
                            "Joining as guest: "
                            strong { "{name}" }
                            "..."
                        }
                    }
                }
            },

            // Not yet joined — show the guest join form
            GuestStatus::NotJoined => {
                let mut on_join = on_join_guest.clone();
                rsx! {
                    div { class: "hero-container",
                        BrowserCompatibility {}
                        div { class: "floating-element floating-element-1" }
                        div { class: "floating-element floating-element-2" }
                        div { class: "floating-element floating-element-3" }
                        div { class: "hero-content",
                            h1 { class: "hero-title text-center", "Join as Guest" }
                            div { class: "content-separator" }
                            div { class: "w-full mb-8 card-apple p-8",
                                form {
                                    onsubmit: move |e| {
                                        e.prevent_default();
                                        input_error.set(None);
                                        let raw = input_value();
                                        match validate_display_name(&raw) {
                                            Ok(valid_name) => {
                                                input_value.set(valid_name.clone());
                                                save_display_name_to_storage(&valid_name);
                                                display_name_ctx.0.set(Some(valid_name));
                                                // Issue 1613: no password on the first attempt.
                                                // The prompt only exists once the server has said
                                                // one is needed.
                                                on_join(None);
                                            }
                                            Err(msg) => {
                                                input_error.set(Some(msg));
                                            }
                                        }
                                    },
                                    h3 { class: "text-center text-xl font-semibold mb-6 text-white/90",
                                        "Join Meeting as Guest"
                                    }
                                    div { class: "space-y-6",
                                        div {
                                            label {
                                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                                "Meeting ID"
                                            }
                                            div {
                                                class: "input-apple",
                                                style: "opacity: 0.7; cursor: default; user-select: all; display: flex; align-items: center;",
                                                "{id}"
                                            }
                                        }
                                        div {
                                            label {
                                                r#for: "guest-name",
                                                class: "block text-white/80 text-sm font-medium mb-2 ml-1",
                                                "Your Name"
                                            }
                                            input {
                                                id: "guest-name",
                                                class: TEXT_INPUT_CLASSES,
                                                r#type: "text",
                                                placeholder: "Enter your display name",
                                                required: true,
                                                autofocus: true,
                                                maxlength: DISPLAY_NAME_MAX_LEN as i64,
                                                value: "{input_value}",
                                                oninput: move |e: Event<FormData>| {
                                                    input_value.set(e.value());
                                                    input_error.set(None);
                                                },
                                                // Issue 1613: this is also the focus anchor the
                                                // password prompt returns to. Backing out of the
                                                // prompt swaps this whole subtree back in, so the
                                                // mount handler runs and focus lands here rather
                                                // than on `<body>`. `autofocus` above is not
                                                // enough on its own — browsers honour it on
                                                // document load, not reliably on every re-insert.
                                                onmounted: move |e| {
                                                    let element = e.data();
                                                    spawn(async move {
                                                        let _ = element.set_focus(true).await;
                                                    });
                                                },
                                            }
                                            p { class: "text-sm text-foreground-subtle mt-2 ml-1",
                                                "Allowed: letters, numbers, spaces, hyphens, underscores, apostrophes"
                                            }
                                            if let Some(err) = input_error() {
                                                p {
                                                    class: "text-sm mt-2 ml-1",
                                                    style: "color: {theme_color::ERROR_TEXT};",
                                                    "{err}"
                                                }
                                            }
                                        }
                                        div { class: "mt-4",
                                            button {
                                                r#type: "submit",
                                                class: "btn-apple btn-primary w-full",
                                                disabled: input_value().trim().is_empty(),
                                                span { class: "text-lg", "Join as Guest" }
                                            }
                                        }
                                        p { class: "text-sm text-foreground-subtle text-center mt-4",
                                            "You are joining without an account. "
                                            "Some features may be limited."
                                        }
                                    }
                                }
                            }
                            div { class: "content-separator" }
                        }
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_response(
        status: &str,
        room_token: Option<&str>,
        observer_token: Option<&str>,
    ) -> JoinMeetingResponse {
        JoinMeetingResponse {
            user_id: "guest-1".to_string(),
            display_name: Some("Guest".to_string()),
            status: status.to_string(),
            is_host: false,
            is_guest: true,
            joined_at: 0,
            admitted_at: None,
            room_token: room_token.map(ToString::to_string),
            observer_token: observer_token.map(ToString::to_string),
            waiting_room_enabled: true,
            admitted_can_admit: false,
            end_on_host_leave: true,
            host_display_name: Some("Host".to_string()),
            host_user_id: Some("host-1".to_string()),
            allow_guests: true,
            recording_allowed_for_all: false,
            chat_allowed_for_all: true,
        }
    }

    #[test]
    fn admitted_mapping_uses_fallback_when_response_observer_missing() {
        let response = make_response("admitted", Some("room-token"), None);

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "fallback-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[test]
    fn admitted_mapping_uses_fallback_when_response_observer_empty() {
        let response = make_response("admitted", Some("room-token"), Some(""));

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "fallback-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }

    #[test]
    fn admitted_mapping_prefers_non_empty_response_observer_token() {
        let response = make_response("admitted", Some("room-token"), Some("response-token"));

        let status = guest_status_from_join_response(&response, Some("fallback-token".to_string()));

        match status {
            GuestStatus::Admitted {
                status_observer_token,
                ..
            } => {
                assert_eq!(status_observer_token, "response-token");
            }
            other => panic!("expected Admitted, got {other:?}"),
        }
    }
}
