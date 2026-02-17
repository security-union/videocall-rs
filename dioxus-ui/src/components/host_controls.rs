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

//! Host Controls component - allows admitted participants to admit/reject waiting participants

use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::RefCell;
use std::rc::Rc;

use crate::constants::meeting_api_client;
use videocall_meeting_types::responses::ParticipantStatusResponse;

/// Type alias for waiting participant (uses shared type)
pub type WaitingParticipant = ParticipantStatusResponse;

#[derive(Props, Clone, PartialEq)]
pub struct HostControlsProps {
    pub meeting_id: String,
    /// Whether the current user is admitted to the meeting (all admitted users can manage waiting room)
    pub is_admitted: bool,
}

#[component]
pub fn HostControls(props: HostControlsProps) -> Element {
    let mut waiting = use_signal(Vec::<WaitingParticipant>::new);
    let mut error = use_signal(|| None::<String>);
    let mut expanded = use_signal(|| true);
    let mut interval_holder: Signal<Option<Rc<RefCell<Option<Interval>>>>> = use_signal(|| None);

    let meeting_id = props.meeting_id.clone();
    let is_admitted = props.is_admitted;

    // Set up polling for waiting users when admitted
    use_effect(move || {
        // Clean up previous interval (use peek to avoid subscribing to this signal)
        if let Some(interval_rc) = interval_holder.peek().as_ref() {
            interval_rc.borrow_mut().take();
        }

        if !is_admitted {
            return;
        }

        let meeting_id = meeting_id.clone();

        // Fetch waiting users
        let fetch_waiting = {
            let meeting_id = meeting_id.clone();
            move || {
                let meeting_id = meeting_id.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match do_fetch_waiting(&meeting_id).await {
                        Ok(participants) => {
                            waiting.set(participants);
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

        // Fetch immediately
        fetch_waiting();

        // Set up polling interval
        let interval = Interval::new(3000, fetch_waiting);
        let interval_rc = Rc::new(RefCell::new(Some(interval)));
        interval_holder.write().replace(interval_rc);
    });

    // Don't render if not admitted or no waiting participants
    if !props.is_admitted || waiting.read().is_empty() {
        return rsx! {};
    }

    let waiting_count = waiting.read().len();
    let show_admit_all = waiting_count > 1;

    rsx! {
        div { class: "host-controls-container",
            button {
                class: "host-controls-toggle",
                onclick: move |_| {
                    let current = *expanded.read();
                    expanded.set(!current);
                },
                span { class: "waiting-badge", "{waiting_count}" }
                span { "Waiting to join" }
                svg {
                    class: if *expanded.read() { "chevron-icon expanded" } else { "chevron-icon" },
                    xmlns: "http://www.w3.org/2000/svg",
                    width: "16",
                    height: "16",
                    view_box: "0 0 24 24",
                    fill: "none",
                    stroke: "currentColor",
                    stroke_width: "2",
                    stroke_linecap: "round",
                    stroke_linejoin: "round",
                    polyline { points: "6 9 12 15 18 9" }
                }
            }

            if *expanded.read() {
                div { class: "host-controls-list",
                    if show_admit_all {
                        div { class: "admit-all-container",
                            button {
                                class: "btn-admit-all",
                                onclick: {
                                    let meeting_id = props.meeting_id.clone();
                                    move |_| {
                                        let meeting_id = meeting_id.clone();
                                        waiting.set(vec![]);
                                        wasm_bindgen_futures::spawn_local(async move {
                                            if let Err(e) = do_admit_all(&meeting_id).await {
                                                log::error!("Failed to admit all: {e}");
                                            }
                                        });
                                    }
                                },
                                svg {
                                    xmlns: "http://www.w3.org/2000/svg",
                                    width: "16",
                                    height: "16",
                                    view_box: "0 0 24 24",
                                    fill: "none",
                                    stroke: "currentColor",
                                    stroke_width: "2",
                                    stroke_linecap: "round",
                                    stroke_linejoin: "round",
                                    polyline { points: "20 6 9 17 4 12" }
                                }
                                "Admit all ({waiting_count})"
                            }
                        }
                    }

                    for participant in waiting.read().iter() {
                        {
                            let email = participant.email.clone();
                            let email_for_admit = email.clone();
                            let email_for_reject = email.clone();
                            let meeting_id_admit = props.meeting_id.clone();
                            let meeting_id_reject = props.meeting_id.clone();

                            rsx! {
                                div { class: "waiting-participant", key: "{email}",
                                    span { class: "participant-email", "{email}" }
                                    div { class: "participant-actions",
                                        button {
                                            class: "btn-admit",
                                            title: "Admit",
                                            onclick: move |_| {
                                                let email = email_for_admit.clone();
                                                let meeting_id = meeting_id_admit.clone();
                                                waiting.write().retain(|p| p.email != email);
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    if let Err(e) = do_admit_participant(&meeting_id, &email).await {
                                                        log::error!("Failed to admit: {e}");
                                                    }
                                                });
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
                                                polyline { points: "20 6 9 17 4 12" }
                                            }
                                        }
                                        button {
                                            class: "btn-reject",
                                            title: "Reject",
                                            onclick: move |_| {
                                                let email = email_for_reject.clone();
                                                let meeting_id = meeting_id_reject.clone();
                                                waiting.write().retain(|p| p.email != email);
                                                wasm_bindgen_futures::spawn_local(async move {
                                                    if let Err(e) = do_reject_participant(&meeting_id, &email).await {
                                                        log::error!("Failed to reject: {e}");
                                                    }
                                                });
                                            },
                                            svg {
                                                xmlns: "http://www.w3.org/2000/svg",
                                                width: "16",
                                                height: "16",
                                                view_box: "0 0 24 24",
                                                fill: "none",
                                                stroke: "currentColor",
                                                stroke_width: "2",
                                                stroke_linecap: "round",
                                                stroke_linejoin: "round",
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

async fn do_fetch_waiting(meeting_id: &str) -> Result<Vec<WaitingParticipant>, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    match client.get_waiting_room(meeting_id).await {
        Ok(response) => Ok(response.waiting),
        Err(videocall_meeting_client::ApiError::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(format!("{e}")),
    }
}

async fn do_admit_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .admit_participant(meeting_id, email)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

async fn do_reject_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .reject_participant(meeting_id, email)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

async fn do_admit_all(meeting_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .admit_all(meeting_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}
