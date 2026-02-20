/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Waiting Room component - shown to non-host users while waiting for admission

use crate::constants::meeting_api_client;
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use videocall_meeting_types::responses::ParticipantStatusResponse;

pub type ParticipantStatus = ParticipantStatusResponse;

#[component]
pub fn WaitingRoom(
    meeting_id: String,
    on_admitted: EventHandler<String>,
    on_rejected: EventHandler<()>,
    on_cancel: EventHandler<()>,
) -> Element {
    let mut error = use_signal(|| None::<String>);

    // Poll for status updates using a spawned async loop (not Interval callback,
    // which runs outside the Dioxus scope and causes spawn() to panic).
    {
        let meeting_id = meeting_id.clone();
        use_effect(move || {
            let meeting_id = meeting_id.clone();
            spawn(async move {
                loop {
                    handle_status_check(&meeting_id, &on_admitted, &on_rejected, &mut error)
                        .await;
                    TimeoutFuture::new(2_000).await;
                }
            });
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

async fn handle_status_check(
    meeting_id: &str,
    on_admitted: &EventHandler<String>,
    on_rejected: &EventHandler<()>,
    error: &mut Signal<Option<String>>,
) {
    match check_status(meeting_id).await {
        Ok(status) => {
            match status.status.as_str() {
                "admitted" => {
                    if let Some(token) = status.room_token {
                        on_admitted.call(token);
                    } else {
                        error.set(Some(
                            "Admitted but no room token received".to_string(),
                        ));
                    }
                }
                "rejected" => {
                    on_rejected.call(());
                }
                _ => {}
            }
            error.set(None);
        }
        Err(e) => {
            error.set(Some(e));
        }
    }
}

async fn check_status(meeting_id: &str) -> Result<ParticipantStatus, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .get_status(meeting_id)
        .await
        .map_err(|e| format!("{e}"))
}
