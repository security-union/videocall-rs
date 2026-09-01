/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Waiting Room component - shown to non-host users while waiting for admission.
//!
//! Primarily uses an observer WebSocket connection for push notifications,
//! backed by polling that never stops: every 5s while that socket is down,
//! every 15s while it is up.

use std::cell::Cell;
use std::rc::Rc;

use crate::constants::{actix_websocket_base, webtransport_enabled, webtransport_host_base};
use crate::context::{
    load_transport_preference_with_source, resolve_transport_config, TransportPreferenceCtx,
};
use crate::meeting_api::{fetch_participant_status, JoinMeetingResponse};
use dioxus::prelude::*;
use videocall_client::Callback as VcCallback;
use videocall_client::{VideoCallClient, VideoCallClientOptions};
use wasm_bindgen::JsCast;

pub type ParticipantStatus = JoinMeetingResponse;

/// Timer period in milliseconds; see [`should_poll`] for the effective cadence.
const POLL_INTERVAL_MS: i32 = 5000;

const PUSHED_POLL_EVERY_N_TICKS: u32 = 3;

/// Whether the timer tick numbered `tick` (1-based) should issue an HTTP poll.
/// A connected socket slows the cadence but never silences it (issue #2262).
pub(crate) fn should_poll(observer_connected: bool, tick: u32) -> bool {
    !observer_connected || tick.is_multiple_of(PUSHED_POLL_EVERY_N_TICKS)
}

/// How often a page sitting in `waiting_for_meeting` re-reads meeting state.
/// That state has no participant row, so the status endpoints answer
/// `NOT_IN_MEETING` and the page must read `GET /meetings/{id}` instead.
pub const START_WATCH_INTERVAL_MS: u32 = 15_000;

/// Whether `state` from `GET /meetings/{id}` is worth a re-join attempt.
pub fn meeting_has_started(state: &str) -> bool {
    state == "active"
}

#[component]
pub fn WaitingRoom(
    meeting_id: String,
    user_id: String,
    display_name: String,
    observer_token: String,
    #[props(default = false)] is_guest: bool,
    on_admitted: EventHandler<ParticipantStatus>,
    on_rejected: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let transport_pref_ctx = use_context::<TransportPreferenceCtx>();
    let mut error = use_signal(|| None::<String>);

    // Guard against duplicate on_admitted / on_rejected calls.
    // Multiple concurrent code paths (mount poll, post-connect poll,
    // WebSocket push, interval poll) can all detect admission at once.
    // The first to set this flag wins; the rest bail out, preventing a
    // panic on the already-dropped EventHandler after the parent
    // unmounts WaitingRoom.
    let resolved = use_hook(|| Rc::new(Cell::new(false)));

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
        let display_name = display_name.clone();
        let observer_connected = observer_connected.clone();
        let resolved = resolved.clone();
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
            let applied_pref = (transport_pref_ctx.0)();
            let (effective_wt_enabled, websocket_urls, webtransport_urls) =
                resolve_transport_config(
                    applied_pref,
                    server_wt_enabled,
                    websocket_urls,
                    webtransport_urls,
                );

            // Issue #1745 PR2 (observability only): record the applied
            // preference + its provenance for the waiting-room OBSERVER client,
            // mirroring the in-call join sites.
            let (_, pref_source) = load_transport_preference_with_source();
            log::info!(
                "Transport preference applied: pref={} source={} wt_urls={} ws_urls={}",
                applied_pref,
                pref_source,
                webtransport_urls.len(),
                websocket_urls.len()
            );

            let meeting_id_for_fetch = meeting_id.clone();
            let meeting_id_for_post_connect = meeting_id.clone();
            let obs_conn_on_connect = observer_connected.clone();
            let obs_conn_on_lost = observer_connected.clone();
            let observer_token_for_post_connect = observer_token.clone();
            let observer_token_for_fetch = observer_token.clone();
            let resolved_on_connect = resolved.clone();
            let resolved_on_push = resolved.clone();
            let resolved_on_push_reject = resolved.clone();

            let opts = VideoCallClientOptions {
                user_id: user_id.clone(),
                display_name: display_name.clone(),
                is_guest,
                meeting_id: meeting_id.clone(),
                websocket_urls,
                webtransport_urls,
                enable_e2ee: false,
                enable_webtransport: effective_wt_enabled,
                max_received_layer: crate::constants::max_received_layer(),
                skip_canvas_paint: crate::constants::skip_canvas_paint(),
                // Issue #1884: waiting-room OBSERVER client — no in-call reaction
                // overlay, so no reaction callback.
                on_reaction: None,
                on_raise_hand: None,
                // Issue 2136: this is an OBSERVER client. The relay's outbound
                // allowlist forwards only MEETING and SESSION_ASSIGNED to an
                // observer, so a MEETING_TIMER can never arrive here -- a
                // callback would be unreachable code, not a missing feature.
                // There is deliberately no timer in the waiting room.
                on_meeting_timer: None,
                on_connected: VcCallback::from(move |_| {
                    log::info!("Observer connection established (waiting room)");
                    obs_conn_on_connect.set(true);
                    // Poll once immediately after connection is established.
                    // This catches admissions that occurred during the WebSocket
                    // handshake window (NATS event already published but observer
                    // wasn't subscribed yet).
                    let mid = meeting_id_for_post_connect.clone();
                    let token = observer_token_for_post_connect.clone();
                    let resolved = resolved_on_connect.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        if resolved.get() {
                            return;
                        }
                        let status_result = fetch_participant_status(&mid, &token, is_guest).await;
                        match status_result {
                            Ok(status) => match status.status.as_str() {
                                "admitted" if status.room_token.is_some() => {
                                    if !resolved.get() {
                                        resolved.set(true);
                                        log::info!(
                                            "Post-connect poll: participant already admitted"
                                        );
                                        on_admitted.call(status);
                                    }
                                }
                                "rejected" => {
                                    if !resolved.get() {
                                        resolved.set(true);
                                        log::info!("Post-connect poll: participant rejected");
                                        on_rejected.call(());
                                    }
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
                on_meeting_activated: None,
                on_participant_admitted: Some(VcCallback::from(move |_: ()| {
                    log::info!("Participant admitted push received, fetching room token via HTTP");
                    let mid = meeting_id_for_fetch.clone();
                    let token = observer_token_for_fetch.clone();
                    let resolved = resolved_on_push.clone();
                    // Use spawn_local instead of dioxus::spawn because
                    // this callback fires from a WebSocket message
                    // handler which runs outside any Dioxus runtime
                    // context. Calling dioxus::spawn() here would panic.
                    wasm_bindgen_futures::spawn_local(async move {
                        if resolved.get() {
                            return;
                        }
                        let status_result = fetch_participant_status(&mid, &token, is_guest).await;
                        match status_result {
                            Ok(status) => {
                                if status.room_token.is_some() {
                                    if !resolved.get() {
                                        resolved.set(true);
                                        on_admitted.call(status);
                                    }
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
                    if !resolved_on_push_reject.get() {
                        resolved_on_push_reject.set(true);
                        log::info!("Participant rejected push received");
                        on_rejected.call(());
                    }
                })),
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
                // Observer-only client: never decode or play back media.
                // Participants in the waiting room must not hear audio from
                // the active call.
                decode_media: false,
                // Observer client: re-election machinery is only meaningful
                // for full participants. Disable the post-rebase retry — the
                // observer is short-lived and the user transitions out of it
                // via admission, not via re-election.
                allow_post_rebase_retry: false,
                // Observer mode (waiting room): no refresh callback needed.
                // Observers don't trigger the watchdog re-election path that
                // consumes the callback (their session lifetime is bounded
                // by the meeting state — admission or activation push —
                // not by RTT degradation), so leaving this `None` is the
                // right behaviour. The Phase 3 / AUTH-2 refresh path is
                // for full participants whose JWT might outlive a
                // long-running meeting.
                refresh_room_token_callback: None,
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

    // Polling safety net, running whether or not the observer socket is up;
    // `should_poll` owns the cadence.
    //
    // The interval_id is stored in an Rc<Cell<i32>> so use_drop can
    // clear it when the component unmounts, preventing leaked timers.
    let poll_interval_id: Rc<Cell<i32>> = use_hook(|| Rc::new(Cell::new(-1)));
    let poll_tick: Rc<Cell<u32>> = use_hook(|| Rc::new(Cell::new(0)));
    {
        let meeting_id = meeting_id.clone();
        let observer_token = observer_token.clone();
        let poll_interval_id = poll_interval_id.clone();
        let poll_tick = poll_tick.clone();
        let resolved_mount = resolved.clone();
        let resolved_interval = resolved.clone();
        let observer_connected = observer_connected.clone();
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
                let resolved_mount = resolved_mount.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    if resolved_mount.get() {
                        return;
                    }
                    let status_result =
                        fetch_participant_status(&meeting_id, &token, is_guest).await;
                    match status_result {
                        Ok(status) => match status.status.as_str() {
                            "admitted" if status.room_token.is_some() => {
                                if !resolved_mount.get() {
                                    resolved_mount.set(true);
                                    log::info!(
                                        "Immediate mount poll: participant already admitted"
                                    );
                                    on_admitted.call(status);
                                }
                            }
                            "rejected" => {
                                if !resolved_mount.get() {
                                    resolved_mount.set(true);
                                    log::info!("Immediate mount poll: participant rejected");
                                    on_rejected.call(());
                                }
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
            let resolved_interval = resolved_interval.clone();
            let observer_connected = observer_connected.clone();
            let poll_tick = poll_tick.clone();
            let poll_closure = wasm_bindgen::closure::Closure::<dyn Fn()>::new(move || {
                if resolved_interval.get() {
                    return;
                }
                let tick = poll_tick.get().wrapping_add(1);
                poll_tick.set(tick);
                if !should_poll(observer_connected.get(), tick) {
                    return;
                }
                let meeting_id = meeting_id.clone();
                let token = observer_token.clone();
                let resolved_interval = resolved_interval.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let status_result =
                        fetch_participant_status(&meeting_id, &token, is_guest).await;
                    match status_result {
                        Ok(status) => match status.status.as_str() {
                            "admitted" => {
                                if status.room_token.is_some() {
                                    if !resolved_interval.get() {
                                        resolved_interval.set(true);
                                        log::info!("Polling fallback: participant admitted");
                                        on_admitted.call(status);
                                    }
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
        // `data-testid` added for the bots-app waiting-room detection
        // path (see e2e/bots-app/src/meeting-join.ts). The bot uses it
        // to distinguish "parked waiting for host admission" from the
        // post-admit grid. Behaviourally inert.
        div { class: "waiting-room-container", "data-testid": "meeting-waiting-room",
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

#[cfg(test)]
mod tests {
    use super::{meeting_has_started, should_poll, POLL_INTERVAL_MS, PUSHED_POLL_EVERY_N_TICKS};

    #[test]
    fn only_an_active_meeting_is_worth_re_joining() {
        assert!(meeting_has_started("active"));
        assert!(!meeting_has_started("idle"));
        assert!(!meeting_has_started("ended"));
        assert!(!meeting_has_started(""));
    }

    #[test]
    fn polls_every_tick_while_observer_is_down() {
        assert!(should_poll(false, 1));
        assert!(should_poll(false, 2));
        assert!(should_poll(false, 3));
        assert!(should_poll(false, 4));
    }

    #[test]
    fn connected_observer_slows_polling_but_never_stops_it() {
        assert!(!should_poll(true, 1));
        assert!(!should_poll(true, 2));
        assert!(should_poll(true, 3));
        assert!(!should_poll(true, 4));
        assert!(should_poll(true, 6));
    }

    #[test]
    fn a_connected_observer_polls_at_least_once_per_minute() {
        let ticks_per_minute = 60_000 / POLL_INTERVAL_MS as u32;
        let polls = (1..=ticks_per_minute)
            .filter(|tick| should_poll(true, *tick))
            .count();
        assert!(
            polls > 0,
            "a connected observer must still poll within a minute; got {polls} polls \
             over {ticks_per_minute} ticks"
        );
    }

    #[test]
    fn tick_counter_wraparound_still_polls() {
        let wrapped = u32::MAX.wrapping_add(1);
        assert!(should_poll(true, wrapped));
        assert!(wrapped.is_multiple_of(PUSHED_POLL_EVERY_N_TICKS));
    }
}
