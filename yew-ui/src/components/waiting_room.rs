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

use crate::constants::meeting_api_client;
use gloo_timers::callback::Interval;
use videocall_meeting_types::responses::ParticipantStatusResponse;
use yew::prelude::*;

/// Type alias for participant status (uses shared type)
pub type ParticipantStatus = ParticipantStatusResponse;

#[derive(Properties, Clone, PartialEq)]
pub struct WaitingRoomProps {
    pub meeting_id: String,
    /// Called when participant is admitted. Carries the room access JWT token.
    pub on_admitted: Callback<String>,
    pub on_rejected: Callback<()>,
    pub on_cancel: Callback<()>,
}

pub enum WaitingRoomMsg {
    CheckStatus,
    StatusReceived(ParticipantStatus),
    StatusError(String),
}

pub struct WaitingRoom {
    status: Option<ParticipantStatus>,
    error: Option<String>,
    _poll_interval: Option<Interval>,
}

impl Component for WaitingRoom {
    type Message = WaitingRoomMsg;
    type Properties = WaitingRoomProps;

    fn create(ctx: &Context<Self>) -> Self {
        // Start polling for status updates
        let link = ctx.link().clone();
        let interval = Interval::new(2000, move || {
            link.send_message(WaitingRoomMsg::CheckStatus);
        });

        // Check immediately
        ctx.link().send_message(WaitingRoomMsg::CheckStatus);

        Self {
            status: None,
            error: None,
            _poll_interval: Some(interval),
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            WaitingRoomMsg::CheckStatus => {
                let meeting_id = ctx.props().meeting_id.clone();
                let link = ctx.link().clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match check_status(&meeting_id).await {
                        Ok(status) => link.send_message(WaitingRoomMsg::StatusReceived(status)),
                        Err(e) => link.send_message(WaitingRoomMsg::StatusError(e)),
                    }
                });

                false
            }
            WaitingRoomMsg::StatusReceived(status) => {
                match status.status.as_str() {
                    "admitted" => {
                        if let Some(token) = status.room_token.clone() {
                            ctx.props().on_admitted.emit(token);
                        } else {
                            self.error = Some("Admitted but no room token received".to_string());
                        }
                    }
                    "rejected" => {
                        ctx.props().on_rejected.emit(());
                    }
                    _ => {}
                }
                self.status = Some(status);
                self.error = None;
                true
            }
            WaitingRoomMsg::StatusError(error) => {
                self.error = Some(error);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
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
                        if let Some(error) = &self.error {
                            html! {
                                <p class="waiting-room-error">{error}</p>
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
                        onclick={ctx.props().on_cancel.reform(|_| ())}
                    >
                        {"Leave waiting room"}
                    </button>
                </div>
            </div>
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
