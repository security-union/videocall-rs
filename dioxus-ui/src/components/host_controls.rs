/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Host Controls component - allows admitted participants to admit/reject waiting participants

use crate::constants::meeting_api_client;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use videocall_meeting_types::responses::ParticipantStatusResponse;

pub type WaitingParticipant = ParticipantStatusResponse;

#[component]
pub fn HostControls(meeting_id: String, is_admitted: bool) -> Element {
    let mut waiting = use_signal(Vec::<WaitingParticipant>::new);
    let mut error = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| true);

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

    // Start polling when admitted using a spawned async loop (not Interval callback,
    // which runs outside the Dioxus scope and causes spawn() to panic).
    {
        let meeting_id = meeting_id.clone();
        use_effect(move || {
            if !is_admitted {
                return;
            }

            let meeting_id = meeting_id.clone();
            spawn(async move {
                loop {
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
                    TimeoutFuture::new(3_000).await;
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
                            let email = participant.email.clone();
                            let email_admit = email.clone();
                            let email_reject = email.clone();
                            let meeting_id_admit = meeting_id.clone();
                            let meeting_id_reject = meeting_id.clone();
                            let fetch_admit = fetch_waiting_list.clone();
                            let fetch_reject = fetch_waiting_list.clone();
                            rsx! {
                                div { key: "{email}", class: "waiting-participant",
                                    span { class: "participant-email", "{email}" }
                                    div { class: "participant-actions",
                                        button {
                                            class: "btn-admit",
                                            title: "Admit",
                                            onclick: move |_| {
                                                waiting.write().retain(|p| p.email != email_admit);
                                                let email = email_admit.clone();
                                                let meeting_id = meeting_id_admit.clone();
                                                let fetch = fetch_admit.clone();
                                                spawn(async move {
                                                    match admit_participant(&meeting_id, &email).await {
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
                                                waiting.write().retain(|p| p.email != email_reject);
                                                let email = email_reject.clone();
                                                let meeting_id = meeting_id_reject.clone();
                                                let fetch = fetch_reject.clone();
                                                spawn(async move {
                                                    match reject_participant(&meeting_id, &email).await {
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

async fn fetch_waiting(meeting_id: &str) -> Result<Vec<WaitingParticipant>, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    match client.get_waiting_room(meeting_id).await {
        Ok(response) => Ok(response.waiting),
        Err(videocall_meeting_client::ApiError::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(format!("{e}")),
    }
}

async fn admit_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .admit_participant(meeting_id, email)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

async fn reject_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .reject_participant(meeting_id, email)
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
