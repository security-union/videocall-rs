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

//! Waiting Room component - shown to non-host users while waiting for admission

use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::RefCell;
use std::rc::Rc;

use crate::constants::meeting_api_client;
use videocall_meeting_types::responses::ParticipantStatusResponse;

/// Type alias for participant status (uses shared type)
pub type ParticipantStatus = ParticipantStatusResponse;

#[derive(Props, Clone, PartialEq)]
pub struct WaitingRoomProps {
    pub meeting_id: String,
    /// Called when participant is admitted. Carries the room access JWT token.
    pub on_admitted: EventHandler<String>,
    pub on_rejected: EventHandler<()>,
    pub on_cancel: EventHandler<()>,
}

#[component]
pub fn WaitingRoom(props: WaitingRoomProps) -> Element {
    let mut status = use_signal(|| None::<ParticipantStatus>);
    let mut error = use_signal(|| None::<String>);
    let mut interval_holder: Signal<Option<Rc<RefCell<Option<Interval>>>>> = use_signal(|| None);

    // Track if callbacks have been fired to avoid double-calling
    let mut admitted_fired = use_signal(|| false);
    let mut rejected_fired = use_signal(|| false);

    // Set up polling for status updates
    let meeting_id = props.meeting_id.clone();
    let on_admitted = props.on_admitted.clone();
    let on_rejected = props.on_rejected.clone();

    use_effect(move || {
        // Clean up previous interval (use peek to avoid subscribing to this signal)
        if let Some(interval_rc) = interval_holder.peek().as_ref() {
            interval_rc.borrow_mut().take();
        }

        let meeting_id = meeting_id.clone();
        let on_admitted = on_admitted.clone();
        let on_rejected = on_rejected.clone();

        // Check status function
        let check_status = {
            let meeting_id = meeting_id.clone();
            move || {
                let meeting_id = meeting_id.clone();
                let on_admitted = on_admitted.clone();
                let on_rejected = on_rejected.clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match do_check_status(&meeting_id).await {
                        Ok(participant_status) => {
                            match participant_status.status.as_str() {
                                "admitted" => {
                                    if !*admitted_fired.read() {
                                        admitted_fired.set(true);
                                        if let Some(token) = participant_status.room_token.clone() {
                                            on_admitted.call(token);
                                        } else {
                                            error.set(Some(
                                                "Admitted but no room token received".to_string(),
                                            ));
                                        }
                                    }
                                }
                                "rejected" => {
                                    if !*rejected_fired.read() {
                                        rejected_fired.set(true);
                                        on_rejected.call(());
                                    }
                                }
                                _ => {}
                            }
                            status.set(Some(participant_status));
                            error.set(None);
                        }
                        Err(e) => {
                            error.set(Some(e));
                        }
                    }
                });
            }
        };

        // Check immediately
        check_status();

        // Set up polling interval
        let interval = Interval::new(2000, check_status);
        let interval_rc = Rc::new(RefCell::new(Some(interval)));
        interval_holder.write().replace(interval_rc);
    });

    rsx! {
        div { class: "waiting-room-container",
            div { class: "waiting-room-card card-apple",
                div { class: "waiting-room-icon",
                    svg {
                        xmlns: "http://www.w3.org/2000/svg",
                        width: "64",
                        height: "64",
                        view_box: "0 0 24 24",
                        fill: "none",
                        stroke: "currentColor",
                        stroke_width: "1.5",
                        stroke_linecap: "round",
                        stroke_linejoin: "round",
                        circle { cx: "12", cy: "12", r: "10" }
                        polyline { points: "12 6 12 12 16 14" }
                    }
                }
                h2 { "Waiting to be admitted" }
                p { class: "waiting-room-message",
                    "The meeting host will let you in soon."
                }

                if let Some(err) = error.read().as_ref() {
                    p { class: "waiting-room-error", "{err}" }
                }

                div { class: "waiting-room-spinner",
                    div { class: "spinner-dot" }
                    div { class: "spinner-dot" }
                    div { class: "spinner-dot" }
                }

                button {
                    class: "btn-apple btn-secondary",
                    onclick: move |_| props.on_cancel.call(()),
                    "Leave waiting room"
                }
            }
        }
    }
}

async fn do_check_status(meeting_id: &str) -> Result<ParticipantStatus, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .get_status(meeting_id)
        .await
        .map_err(|e| format!("{e}"))
}
