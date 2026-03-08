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

//! Waiting Room component - shown to non-host users while waiting for admission.
//!
//! Primarily uses an observer WebSocket connection for push notifications.
//! Falls back to lightweight polling (every 5s) when the observer WebSocket
//! is not connected -- e.g. empty observer token, connection failure, or
//! disconnect due to token expiry.

use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use videocall_meeting_types::responses::ParticipantStatusResponse;
use wasm_bindgen::JsCast;
use yew::prelude::*;

pub type JoinMeetingResponse = ParticipantStatusResponse;

/// Polling interval in milliseconds when observer WebSocket is not connected.
const POLL_INTERVAL_MS: i32 = 5000;

#[derive(Properties, Clone, PartialEq)]
pub struct WaitingRoomProps {
    pub meeting_id: String,
    /// The authenticated user's email, used as the observer `userid` so
    /// the server can match push-notification `target_email` to this client.
    pub email: String,
    /// Observer JWT token for receiving push notifications.
    #[prop_or_default]
    pub observer_token: Option<String>,
    /// Called when participant is admitted. Carries the full join response including room_token.
    pub on_admitted: Callback<JoinMeetingResponse>,
    pub on_rejected: Callback<()>,
    pub on_cancel: Callback<()>,
}

#[function_component(WaitingRoom)]
pub fn waiting_room(props: &WaitingRoomProps) -> Html {
    let error = use_state(|| None::<String>);

    // Track whether the observer WebSocket is currently connected.
    // Used by the polling effect to skip HTTP calls when push is active.
    let observer_connected = use_state(|| false);

    // Store the polling interval ID so we can clear it on cleanup.
    let poll_interval_id = use_state(|| None::<i32>);

    // Set up observer WebSocket for push notifications
    {
        let observer_token = props.observer_token.clone();
        let meeting_id = props.meeting_id.clone();
        let email = props.email.clone();
        let on_admitted = props.on_admitted.clone();
        let on_rejected = props.on_rejected.clone();
        let observer_connected = observer_connected.clone();

        use_effect_with(observer_token.clone(), move |token| {
            let observer_client: Option<VideoCallClient> = token.as_ref().map(|token| {
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

                let meeting_id_for_fetch = meeting_id.clone();

                // Clone observer_connected for the callbacks. We use Rc<Cell<bool>>
                // to share between the two callbacks without Yew runtime context.
                let obs_connected_on_connect = observer_connected.clone();
                let obs_connected_on_lost = observer_connected.clone();

                let opts = VideoCallClientOptions {
                    userid: email.clone(),
                    meeting_id: meeting_id.clone(),
                    websocket_urls: observer_ws_urls,
                    webtransport_urls: observer_wt_urls,
                    enable_e2ee: false,
                    enable_webtransport: false, // observer uses WebSocket only
                    on_connected: VcCallback::from(move |_| {
                        log::info!("Observer WebSocket connected for waiting room");
                        obs_connected_on_connect.set(true);
                    }),
                    on_connection_lost: VcCallback::from(move |_| {
                        log::warn!("Observer WebSocket connection lost in waiting room; polling fallback will activate");
                        obs_connected_on_lost.set(false);
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
                    on_meeting_activated: None,
                    on_participant_admitted: Some(VcCallback::from(move |_: ()| {
                        log::info!("Participant admitted via push notification, fetching room token via HTTP");
                        let mid = meeting_id_for_fetch.clone();
                        let on_admitted = on_admitted.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match crate::meeting_api::check_status(&mid).await {
                                Ok(status) => {
                                    if status.room_token.is_some() {
                                        on_admitted.emit(status);
                                    } else {
                                        log::error!("Admitted but check_status returned no room_token");
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to fetch room token after admission: {e}");
                                }
                            }
                        });
                    })),
                    on_participant_rejected: Some(VcCallback::from(move |_| {
                        log::info!("Participant rejected via push notification");
                        on_rejected.emit(());
                    })),
                    on_waiting_room_updated: None,
                    on_speaking_changed: None,
                    vad_threshold: None,
                    session_id: String::new(),
                    display_name: email.clone(),
                    on_peer_display_name_changed: None,
                };

                let mut client = VideoCallClient::new(opts);
                if let Err(e) = client.connect() {
                    log::error!("Failed to connect observer client for waiting room: {e}");
                    observer_connected.set(false);
                }
                client
            });

            if observer_client.is_none() {
                log::warn!("No observer token available for waiting room push notifications; polling fallback will activate");
                observer_connected.set(false);
            }

            // Keep the observer client alive until cleanup
            move || {
                drop(observer_client);
            }
        });
    }

    // Polling fallback: when the observer WebSocket is NOT connected,
    // poll the participant status endpoint every POLL_INTERVAL_MS to
    // detect admission/rejection. This covers three failure modes:
    //   1. Empty/None observer token (old server, no push support)
    //   2. WebSocket connection failed or was rejected
    //   3. WebSocket disconnected (e.g. token expired after 30 min)
    {
        let is_connected = *observer_connected;
        let meeting_id = props.meeting_id.clone();
        let on_admitted = props.on_admitted.clone();
        let on_rejected = props.on_rejected.clone();
        let poll_interval_id = poll_interval_id.clone();

        use_effect_with(is_connected, move |is_connected| {
            // Always clear any previous polling interval first.
            if let Some(prev_id) = *poll_interval_id {
                if let Some(w) = web_sys::window() {
                    w.clear_interval_with_handle(prev_id);
                }
                poll_interval_id.set(None);
                log::debug!("WaitingRoom: cleared previous polling interval");
            }

            // Start polling only when the observer WebSocket is NOT connected.
            let mut cleanup_id: Option<i32> = None;
            if !*is_connected {
                log::info!(
                    "WaitingRoom: observer not connected, starting polling fallback (every {POLL_INTERVAL_MS}ms)"
                );

                if let Some(window) = web_sys::window() {
                    let poll_closure = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                        let meeting_id = meeting_id.clone();
                        let on_admitted = on_admitted.clone();
                        let on_rejected = on_rejected.clone();
                        wasm_bindgen_futures::spawn_local(async move {
                            match crate::meeting_api::check_status(&meeting_id).await {
                                Ok(status) => match status.status.as_str() {
                                    "admitted" => {
                                        if status.room_token.is_some() {
                                            log::info!("Polling fallback: participant admitted");
                                            on_admitted.emit(status);
                                        } else {
                                            log::warn!("Polling fallback: admitted but no room_token, will retry");
                                        }
                                    }
                                    "rejected" => {
                                        log::info!("Polling fallback: participant rejected");
                                        on_rejected.emit(());
                                    }
                                    other => {
                                        log::debug!(
                                            "Polling fallback: status={other}, continuing to poll"
                                        );
                                    }
                                },
                                Err(e) => {
                                    log::warn!("Polling fallback: status check failed: {e}");
                                }
                            }
                        });
                    });

                    let id = window
                        .set_interval_with_callback_and_timeout_and_arguments_0(
                            poll_closure.as_ref().unchecked_ref(),
                            POLL_INTERVAL_MS,
                        )
                        .unwrap_or(-1);

                    // Prevent the closure from being dropped while the interval is active.
                    poll_closure.forget();
                    poll_interval_id.set(Some(id));
                    cleanup_id = Some(id);
                }
            }

            // Cleanup: clear interval when the effect re-runs or component unmounts.
            move || {
                if let Some(id) = cleanup_id {
                    if let Some(w) = web_sys::window() {
                        w.clear_interval_with_handle(id);
                    }
                }
            }
        });
    }

    html! {
        <div class="waiting-room-container">
            <div class="waiting-room-card card-apple">
                <div class="waiting-room-icon">
                    <svg xmlns="http://www.w3.org/2000/svg" width="64" height="64" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
                        <circle cx="12" cy="12" r="10"></circle>
                        <polyline points="12 6 12 12 16 14"></polyline>
                    </svg>
                </div>
                <h2>{"Waiting to be admitted"}</h2>
                <p class="waiting-room-message">
                    {"The meeting host will let you in soon."}
                </p>

                {
                    if let Some(err) = &*error {
                        html! {
                            <p class="waiting-room-error">{err}</p>
                        }
                    } else {
                        html! {}
                    }
                }

                <div class="waiting-room-spinner">
                    <div class="spinner-dot"></div>
                    <div class="spinner-dot"></div>
                    <div class="spinner-dot"></div>
                </div>

                <button
                    class="btn-apple btn-secondary"
                    onclick={props.on_cancel.reform(|_| ())}
                >
                    {"Leave waiting room"}
                </button>
            </div>
        </div>
    }
}
