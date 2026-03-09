/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Host Controls component - allows admitted participants to admit/reject waiting participants.
//!
//! Instead of polling every 3 seconds, this component receives a
//! `waiting_room_version` counter from the parent that is incremented
//! whenever the `on_waiting_room_updated` push event fires on the main
//! `VideoCallClient`. The `use_effect` reacts to changes in this counter
//! and fetches the waiting room list once per notification.

use crate::constants::meeting_api_client;
use dioxus::prelude::*;
use videocall_meeting_types::responses::ParticipantStatusResponse;
use web_sys::HtmlAudioElement;

pub type WaitingParticipant = ParticipantStatusResponse;

#[component]
pub fn HostControls(
    meeting_id: String,
    is_admitted: bool,
    /// Counter incremented by the parent whenever a `on_waiting_room_updated`
    /// push event is received. The component fetches the waiting list each
    /// time this value changes.
    waiting_room_version: Signal<u64>,
) -> Element {
    let mut waiting = use_signal(Vec::<WaitingParticipant>::new);
    let mut error = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| true);
    let mut prev_waiting_count = use_signal(|| 0usize);

    let fetch_waiting_list = {
        let meeting_id = meeting_id.clone();
        move || {
            if !is_admitted {
                return;
            }
            let meeting_id = meeting_id.clone();
            spawn(async move {
                match fetch_waiting(&meeting_id).await {
                    Ok(w) => {
                        waiting.set(w);
                        error.set(None);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch waiting room: {e}");
                        error.set(Some(e));
                    }
                }
            });
        }
    };

    // Fetch on mount and whenever waiting_room_version changes (push notification).
    {
        let meeting_id = meeting_id.clone();
        use_effect(move || {
            // Read the version so Dioxus tracks it as a reactive dependency.
            let _version = waiting_room_version();
            if !is_admitted {
                return;
            }

            let meeting_id = meeting_id.clone();
            spawn(async move {
                match fetch_waiting(&meeting_id).await {
                    Ok(w) => {
                        let new_count = w.len();
                        let old_count = *prev_waiting_count.peek();
                        if new_count > old_count {
                            play_knock_sound();
                        }
                        prev_waiting_count.set(new_count);
                        waiting.set(w);
                        error.set(None);
                    }
                    Err(e) => {
                        log::warn!("Failed to fetch waiting room: {e}");
                        error.set(Some(e));
                    }
                }
            });
        });
    }

    if !is_admitted || waiting().is_empty() {
        return rsx! {};
    }

    let show_admit_all = waiting().len() > 1;

    let on_admit_all = {
        let meeting_id = meeting_id.clone();
        let fetch_waiting_list = fetch_waiting_list.clone();
        move |_| {
            waiting.write().clear();
            let meeting_id = meeting_id.clone();
            let fetch_waiting_list = fetch_waiting_list.clone();
            spawn(async move {
                match admit_all_participants(&meeting_id).await {
                    Ok(_) => fetch_waiting_list(),
                    Err(e) => {
                        error.set(Some(e));
                        fetch_waiting_list();
                    }
                }
            });
        }
    };

    rsx! {
        div { class: "host-controls-container",
            button { class: "host-controls-toggle", onclick: move |_| expanded.set(!expanded()),
                span { class: "waiting-badge", "{waiting().len()}" }
                span { "Waiting to join" }
                svg {
                    class: if expanded() { "chevron-icon expanded" } else { "chevron-icon" },
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "16", height: "16",
                    view_box: "0 0 24 24",
                    fill: "none", stroke: "currentColor",
                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
            }

            if expanded() {
                div { class: "host-controls-list",
                    if show_admit_all {
                        div { class: "admit-all-container",
                            button { class: "btn-admit-all", onclick: on_admit_all,
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                    view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                    stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                    polyline { points: "20 6 9 17 4 12" }
                                }
                                "Admit all ({waiting().len()})"
                            }
                        }
                    }
                    for participant in waiting().iter() {
                        {
                            let peer_user_id = participant.user_id.clone();
                            let display_name = participant.display_name.clone();

                            let uid_for_key = peer_user_id.clone();
                            let uid_for_view = peer_user_id.clone();
                            let uid_admit = peer_user_id.clone();
                            let uid_reject = peer_user_id.clone();

                            let meeting_id_admit = meeting_id.clone();
                            let meeting_id_reject = meeting_id.clone();

                            let fetch_admit = fetch_waiting_list.clone();
                            let fetch_reject = fetch_waiting_list.clone();

                            let mut waiting_admit = waiting.clone();
                            let mut waiting_reject = waiting.clone();

                            let mut error_admit = error.clone();
                            let mut error_reject = error.clone();

                            rsx! {
                                div { key: "{uid_for_key}", class: "waiting-participant",
                                    div { class: "participant-info",
                                        if let Some(name) = display_name.clone() {
                                            if !name.trim().is_empty() {
                                                div { class: "participant-name", "{name}" }
                                                div { class: "participant-email", "{uid_for_view}" }
                                            } else {
                                                div { class: "participant-name", "{uid_for_view}" }
                                            }
                                        } else {
                                            div { class: "participant-name", "{uid_for_view}" }
                                        }
                                    }
                                    div { class: "participant-actions",
                                        button {
                                            class: "btn-admit",
                                            title: "Admit",
                                            onclick: move |_| {
                                                waiting_admit.write().retain(|p| p.user_id != uid_admit);
                                                let uid = uid_admit.clone();
                                                let meeting_id = meeting_id_admit.clone();
                                                let fetch = fetch_admit.clone();
                                                let mut error = error_admit.clone();

                                                spawn(async move {
                                                    match admit_participant(&meeting_id, &uid).await {
                                                        Ok(_) => fetch(),
                                                        Err(e) => {
                                                            error.set(Some(e));
                                                            fetch();
                                                        }
                                                    }
                                                });
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                                polyline { points: "20 6 9 17 4 12" }
                                            }
                                        }
                                        button {
                                            class: "btn-reject",
                                            title: "Reject",
                                            onclick: move |_| {
                                                waiting_reject.write().retain(|p| p.user_id != uid_reject);
                                                let uid = uid_reject.clone();
                                                let meeting_id = meeting_id_reject.clone();
                                                let fetch = fetch_reject.clone();
                                                let mut error = error_reject.clone();

                                                spawn(async move {
                                                    match reject_participant(&meeting_id, &uid).await {
                                                        Ok(_) => fetch(),
                                                        Err(e) => {
                                                            error.set(Some(e));
                                                            fetch();
                                                        }
                                                    }
                                                });
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg", width: "16", height: "16",
                                                view_box: "0 0 24 24", fill: "none", stroke: "currentColor",
                                                stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
                                                line { x1: "18", y1: "6", x2: "6", y2: "18" }
                                                line { x1: "6", y1: "6", x2: "18", y2: "18" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn play_knock_sound() {
    if let Ok(audio) = HtmlAudioElement::new_with_src("/assets/knock.wav") {
        audio.set_volume(0.5);
        if let Err(e) = audio.play() {
            log::warn!("Failed to play knock sound: {e:?}");
        }
    }
}

async fn fetch_waiting(meeting_id: &str) -> Result<Vec<WaitingParticipant>, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    match client.get_waiting_room(meeting_id).await {
        Ok(response) => Ok(response.waiting),
        Err(videocall_meeting_client::ApiError::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(format!("{e}")),
    }
}

async fn admit_participant(meeting_id: &str, user_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .admit_participant(meeting_id, user_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

async fn reject_participant(meeting_id: &str, user_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .reject_participant(meeting_id, user_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

async fn admit_all_participants(meeting_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .admit_all(meeting_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}
