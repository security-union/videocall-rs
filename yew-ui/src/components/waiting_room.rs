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
//! Uses an observer WebSocket connection to receive push notifications
//! for admission/rejection events instead of polling.

use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use videocall_meeting_types::responses::ParticipantStatusResponse;
use yew::prelude::*;

pub type JoinMeetingResponse = ParticipantStatusResponse;

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

    // Set up observer WebSocket for push notifications
    {
        let observer_token = props.observer_token.clone();
        let meeting_id = props.meeting_id.clone();
        let email = props.email.clone();
        let on_admitted = props.on_admitted.clone();
        let on_rejected = props.on_rejected.clone();

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

                let opts = VideoCallClientOptions {
                    userid: email.clone(),
                    meeting_id: meeting_id.clone(),
                    websocket_urls: observer_ws_urls,
                    webtransport_urls: observer_wt_urls,
                    enable_e2ee: false,
                    enable_webtransport: false, // observer uses WebSocket only
                    on_connected: VcCallback::from(|_| {
                        log::info!("Observer WebSocket connected for waiting room");
                    }),
                    on_connection_lost: VcCallback::from(|_| {
                        log::warn!("Observer WebSocket connection lost in waiting room");
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
                            match crate::meeting_api::join_meeting(&mid, None).await {
                                Ok(status) => {
                                    if status.room_token.is_some() {
                                        on_admitted.emit(status);
                                    } else {
                                        log::error!("Admitted but join_meeting returned no room_token");
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
                };

                let mut client = VideoCallClient::new(opts);
                if let Err(e) = client.connect() {
                    log::error!("Failed to connect observer client for waiting room: {e}");
                }
                client
            });

            if observer_client.is_none() {
                log::warn!("No observer token available for waiting room push notifications");
            }

            // Keep the observer client alive until cleanup
            move || {
                drop(observer_client);
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
