/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Waiting Room component - shown to non-host users while waiting for admission.
//!
//! Primarily uses an observer WebSocket connection for push notifications.
//! Falls back to lightweight polling (every 5s) when the observer WebSocket
//! is not connected -- e.g. empty observer token, connection failure, or
//! disconnect due to token expiry.

use std::cell::Cell;
use std::rc::Rc;

use crate::constants::{actix_websocket_base, webtransport_enabled, webtransport_host_base};
use crate::context::{resolve_transport_config, TransportPreferenceCtx};
use crate::meeting_api::{check_guest_status, check_status, JoinMeetingResponse};
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use wasm_bindgen::JsCast;

pub type ParticipantStatus = JoinMeetingResponse;

/// Polling interval in milliseconds when observer WebSocket is not connected.
const POLL_INTERVAL_MS: i32 = 5000;

#[component]
pub fn WaitingRoom(
    meeting_id: String,
    user_id: String,
    display_name: String,
    observer_token: String,
    #[props(default = false)]
    is_guest: bool,
    on_admitted: EventHandler<ParticipantStatus>,
    on_rejected: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let mut error = use_signal(|| None::<String>);

    // Track whether the observer WebSocket is currently connected.
    // Uses Rc<Cell<bool>> instead of Signal because VcCallback::from()
    // requires Fn (not FnMut), and Dioxus Signal::set() makes closures
    // FnMut. Rc<Cell<bool>> provides interior mutability compatible with Fn.
    let observer_connected = use_hook(|| Rc::new(Cell::new(false)));

    // Create an observer WebSocket client to receive push notifications
    // when the host admits or rejects this participant.
    let mut observer_client = use_signal(|| None::<VideoCallClient>);
    {
        let observer_token = observer_token.clone();
        let meeting_id = meeting_id.clone();
        let user_id = user_id.clone();
        let observer_connected = observer_connected.clone();
        use_effect(move || {
            if observer_token.is_empty() {
                log::warn!("WaitingRoom: no observer token, push notifications unavailable; polling fallback will activate");
                observer_client.set(None);
                observer_connected.set(false);
                return;
            }

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

            // Apply user's transport preference
            let server_wt_enabled = webtransport_enabled().unwrap_or(false);
            let (effective_wt_enabled, websocket_urls, webtransport_urls) =
                resolve_transport_config(
                    (transport_pref_ctx.0)(),
                    server_wt_enabled,
                    websocket_urls,
                    webtransport_urls,
                );

            let meeting_id_for_fetch = meeting_id.clone();
            let meeting_id_for_post_connect = meeting_id.clone();
            let obs_conn_on_connect = observer_connected.clone();
            let obs_conn_on_lost = observer_connected.clone();
            let observer_token_for_post_connect = observer_token.clone();
            let observer_token_for_fetch = observer_token.clone();

            let opts = VideoCallClientOptions {
                user_id: user_id.clone(),
                display_name: String::new(),
                meeting_id: meeting_id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: false,
                enable_webtransport: effective_wt_enabled,
                on_connected: VcCallback::from(move |_| {
                    log::info!("Observer connection established (waiting room)");
                    obs_conn_on_connect.set(true);
                    // Poll once immediately after connection is established.
                    // This catches admissions that occurred during the WebSocket
                    // handshake window (NATS event already published but observer
                    // wasn't subscribed yet).
                    let mid = meeting_id_for_post_connect.clone();
                    let token = observer_token_for_post_connect.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        let status_result = if is_guest {
                            check_guest_status(&mid, &token).await
                        } else {
                            check_status(&mid).await
                        };
                        match status_result {
                            Ok(status) => match status.status.as_str() {
                                "admitted" if status.room_token.is_some() => {
                                    log::info!("Post-connect poll: participant already admitted");
                                    on_admitted.call(status);
                                }
                                "rejected" => {
                                    log::info!("Post-connect poll: participant rejected");
                                    on_rejected.call(());
                                }
                                other => {
                                    log::debug!(
                                        "Post-connect poll: status={other}, waiting for push"
                                    );
                                }
                            },
                            Err(e) => {
                                log::warn!("Post-connect poll: status check failed: {e}");
                            }
                        }
                    });
                }),
                on_connection_lost: VcCallback::from(move |_| {
                    log::warn!(
                        "Observer connection lost (waiting room); polling fallback will activate"
                    );
                    obs_conn_on_lost.set(false);
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
                on_participant_admitted: Some(VcCallback::from(move |_: ()| {
                    log::info!("Participant admitted push received, fetching room token via HTTP");
                    let mid = meeting_id_for_fetch.clone();
                    let token = observer_token_for_fetch.clone();
                    // Use spawn_local instead of dioxus::spawn because
                    // this callback fires from a WebSocket message
                    // handler which runs outside any Dioxus runtime
                    // context. Calling dioxus::spawn() here would panic.
                    wasm_bindgen_futures::spawn_local(async move {
                        let status_result = if is_guest {
                            check_guest_status(&mid, &token).await
                        } else {
                            check_status(&mid).await
                        };
                        match status_result {
                            Ok(status) => {
                                if status.room_token.is_some() {
                                    on_admitted.call(status);
                                } else {
                                    log::error!("Admitted but check_status returned no room_token");
                                    error.set(Some(
                                        "Admitted but failed to obtain room token".to_string(),
                                    ));
                                }
                            }
                            Err(e) => {
                                log::error!("Failed to fetch room token after admission: {e}");
                                error.set(Some(format!("Failed to fetch room token: {e}")));
                            }
                        }
                    });
                })),
                on_participant_rejected: Some(VcCallback::from(move |_| {
                    log::info!("Participant rejected push received");
                    on_rejected.call(());
                })),
                on_waiting_room_updated: None,
                on_speaking_changed: None,
                on_audio_level_changed: None,
                vad_threshold: None,
                on_peer_left: None,
                on_peer_joined: None,
                on_display_name_changed: None,
            };

            let mut client = VideoCallClient::new(opts);
            if let Err(e) = client.connect() {
                log::error!("Failed to connect observer client for waiting room: {e}");
                error.set(Some(format!("Failed to connect for push updates: {e}")));
                observer_client.set(None);
                observer_connected.set(false);
                return;
            }
            observer_client.set(Some(client));
        });
    }

    // Polling safety net: always poll participant status every
    // POLL_INTERVAL_MS regardless of observer WebSocket state. The push
    // path provides instant notification when it works, but polling
    // ensures we never miss an admission/rejection if a NATS event is lost.
    //
    // The interval_id is stored in an Rc<Cell<i32>> so use_drop can
    // clear it when the component unmounts, preventing leaked timers.
    let poll_interval_id: Rc<Cell<i32>> = use_hook(|| Rc::new(Cell::new(-1)));
    {
        let meeting_id = meeting_id.clone();
        let observer_token = observer_token.clone();
        let poll_interval_id = poll_interval_id.clone();
        use_effect(move || {
            let window = match web_sys::window() {
                Some(w) => w,
                None => return,
            };

            log::info!(
                "WaitingRoom: starting polling safety net timer (every {POLL_INTERVAL_MS}ms)"
            );

            // Poll once immediately on mount to catch admissions that
            // occurred before any connection was established (host admitted
            // during the join -> connect gap).
            {
                let meeting_id = meeting_id.clone();
                let token = observer_token.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let status_result = if is_guest {
                        check_guest_status(&meeting_id, &token).await
                    } else {
                        check_status(&meeting_id).await
                    };
                    match status_result {
                        Ok(status) => match status.status.as_str() {
                            "admitted" if status.room_token.is_some() => {
                                log::info!("Immediate mount poll: participant already admitted");
                                on_admitted.call(status);
                            }
                            "rejected" => {
                                log::info!("Immediate mount poll: participant rejected");
                                on_rejected.call(());
                            }
                            other => {
                                log::debug!(
                                    "Immediate mount poll: status={other}, will continue polling"
                                );
                            }
                        },
                        Err(e) => {
                            log::warn!("Immediate mount poll: status check failed: {e}");
                        }
                    }
                });
            }

            let meeting_id = meeting_id.clone();
            let observer_token = observer_token.clone();
            let poll_closure = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                let meeting_id = meeting_id.clone();
                let token = observer_token.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let status_result = if is_guest {
                        check_guest_status(&meeting_id, &token).await
                    } else {
                        check_status(&meeting_id).await
                    };
                    match status_result {
                        Ok(status) => match status.status.as_str() {
                            "admitted" => {
                                if status.room_token.is_some() {
                                    log::info!("Polling fallback: participant admitted");
                                    on_admitted.call(status);
                                } else {
                                    // Admitted but no token yet -- keep polling.
                                    log::warn!(
                                        "Polling fallback: admitted but no room_token, will retry"
                                    );
                                }
                            }
                            "rejected" => {
                                log::info!("Polling fallback: participant rejected");
                                on_rejected.call(());
                            }
                            // "waiting" | "waiting_for_meeting" | _ => continue polling
                            other => {
                                log::debug!("Polling fallback: status={other}, continuing to poll");
                            }
                        },
                        Err(e) => {
                            log::warn!("Polling fallback: status check failed: {e}");
                        }
                    }
                });
            });

            let interval_id = window
                .set_interval_with_callback_and_timeout_and_arguments_0(
                    poll_closure.as_ref().unchecked_ref(),
                    POLL_INTERVAL_MS,
                )
                .unwrap_or(-1);

            // Prevent the closure from being dropped while the interval is active.
            poll_closure.forget();

            // Store the interval ID so use_drop can clear it on unmount.
            poll_interval_id.set(interval_id);
        });
    }

    // Clean up the polling interval when the component unmounts.
    {
        let poll_interval_id = poll_interval_id.clone();
        use_drop(move || {
            let id = poll_interval_id.get();
            if id >= 0 {
                if let Some(window) = web_sys::window() {
                    window.clear_interval_with_handle(id);
                    log::debug!("WaitingRoom: cleared polling interval {id} on unmount");
                }
            }
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
                if !display_name.trim().is_empty() {
                    p { class: "waiting-room-identity", "Joining as {display_name}" }
                }
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
