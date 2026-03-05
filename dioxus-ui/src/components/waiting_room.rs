/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Waiting Room component - shown to non-host users while waiting for admission.
//!
//! Instead of polling, this component connects a lightweight observer
//! `VideoCallClient` using the `observer_token` and listens for
//! `on_participant_admitted` / `on_participant_rejected` push events.

use crate::constants::{actix_websocket_base, webtransport_enabled, webtransport_host_base};
use crate::meeting_api::{join_meeting, JoinMeetingResponse};
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};

pub type ParticipantStatus = JoinMeetingResponse;

#[component]
pub fn WaitingRoom(
    meeting_id: String,
    observer_token: String,
    on_admitted: EventHandler<ParticipantStatus>,
    on_rejected: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut error = use_signal(|| None::<String>);

    // Create an observer WebSocket client to receive push notifications
    // when the host admits or rejects this participant.
    let mut observer_client = use_signal(|| None::<VideoCallClient>);
    {
        let observer_token = observer_token.clone();
        let meeting_id = meeting_id.clone();
        use_effect(move || {
            if observer_token.is_empty() {
                log::warn!("WaitingRoom: no observer token, push notifications unavailable");
                observer_client.set(None);
                return;
            }

            let lobby_url =
                |base: &str| format!("{base}/lobby?token={observer_token}");
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

            let meeting_id_for_fetch = meeting_id.clone();

            let opts = VideoCallClientOptions {
                userid: format!("observer-{meeting_id}"),
                meeting_id: meeting_id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: false,
                enable_webtransport: webtransport_enabled().unwrap_or(false),
                on_connected: VcCallback::from(move |_| {
                    log::info!("Observer connection established (waiting room)");
                }),
                on_connection_lost: VcCallback::from(move |_| {
                    log::warn!("Observer connection lost (waiting room)");
                }),
                on_peer_added: VcCallback::noop(),
                on_peer_first_frame: VcCallback::noop(),
                on_peer_removed: None,
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
                on_meeting_activated: None,
                on_participant_admitted: Some(VcCallback::from(
                    move |_: ()| {
                        log::info!("Participant admitted push received, fetching room token via HTTP");
                        let mid = meeting_id_for_fetch.clone();
                        // Use spawn_local instead of dioxus::spawn because
                        // this callback fires from a WebSocket message
                        // handler which runs outside any Dioxus runtime
                        // context. Calling dioxus::spawn() here would panic.
                        wasm_bindgen_futures::spawn_local(async move {
                            match join_meeting(&mid, None).await {
                                Ok(status) => {
                                    if status.room_token.is_some() {
                                        on_admitted.call(status);
                                    } else {
                                        log::error!("Admitted but join_meeting returned no room_token");
                                        error.set(Some(
                                            "Admitted but failed to obtain room token".to_string(),
                                        ));
                                    }
                                }
                                Err(e) => {
                                    log::error!("Failed to fetch room token after admission: {e}");
                                    error.set(Some(format!(
                                        "Failed to fetch room token: {e}"
                                    )));
                                }
                            }
                        });
                    },
                )),
                on_participant_rejected: Some(VcCallback::from(move |_| {
                    log::info!("Participant rejected push received");
                    on_rejected.call(());
                })),
                on_waiting_room_updated: None,
            };

            let mut client = VideoCallClient::new(opts);
            if let Err(e) = client.connect() {
                log::error!("Failed to connect observer client for waiting room: {e}");
                error.set(Some(format!("Failed to connect for push updates: {e}")));
                observer_client.set(None);
                return;
            }
            observer_client.set(Some(client));
        });
    }

    rsx! {
        div { class: "waiting-room-container",
            div { class: "waiting-room-card card-apple",
                div { class: "waiting-room-icon",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg", width: "64", height: "64",
                        view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                        stroke_width: "1.5", stroke_linecap: "round", stroke_linejoin: "round",
                        circle { cx: "12", cy: "12", r: "10" }
                        polyline { points: "12 6 12 12 16 14" }
                    }
                }
                h2 { "Waiting to be admitted" }
                p { class: "waiting-room-message",
                    "The meeting host will let you in soon."
                }

                if let Some(err) = error() {
                    p { class: "waiting-room-error", "{err}" }
                }

                div { class: "waiting-room-spinner",
                    div { class: "spinner-dot" }
                    div { class: "spinner-dot" }
                    div { class: "spinner-dot" }
                }

                button {
                    class: "btn-apple btn-secondary",
                    onclick: move |_| on_cancel.call(()),
                    "Leave waiting room"
                }
            }
        }
    }
}
