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

use crate::constants::meeting_api_base_url;
use gloo_timers::callback::Interval;
use reqwasm::http::{Request, RequestCredentials};
use serde::Serialize;
use videocall_meeting_types::responses::{
    APIResponse, ParticipantStatusResponse, WaitingRoomResponse,
};
use yew::prelude::*;

/// Type alias for waiting participant (uses shared type)
pub type WaitingParticipant = ParticipantStatusResponse;

#[derive(Debug, Clone, Serialize)]
struct AdmitRequest {
    email: String,
}

#[derive(Properties, Clone, PartialEq)]
pub struct HostControlsProps {
    pub meeting_id: String,
    /// Whether the current user is admitted to the meeting (all admitted users can manage waiting room)
    pub is_admitted: bool,
}

pub enum HostControlsMsg {
    FetchWaiting,
    WaitingReceived(Vec<WaitingParticipant>),
    FetchError(String),
    Admit(String),
    AdmitAll,
    Reject(String),
    ActionComplete,
    ActionError(String),
    ToggleExpanded,
}

pub struct HostControls {
    waiting: Vec<WaitingParticipant>,
    error: Option<String>,
    expanded: bool,
    _poll_interval: Option<Interval>,
}

impl Component for HostControls {
    type Message = HostControlsMsg;
    type Properties = HostControlsProps;

    fn create(ctx: &Context<Self>) -> Self {
        let mut poll_interval = None;

        if ctx.props().is_admitted {
            // Start polling for waiting users
            let link = ctx.link().clone();
            poll_interval = Some(Interval::new(3000, move || {
                link.send_message(HostControlsMsg::FetchWaiting);
            }));

            // Fetch immediately
            ctx.link().send_message(HostControlsMsg::FetchWaiting);
        }

        Self {
            waiting: Vec::new(),
            error: None,
            expanded: true,
            _poll_interval: poll_interval,
        }
    }

    fn changed(&mut self, ctx: &Context<Self>, _old_props: &Self::Properties) -> bool {
        // If we became host, start polling
        if ctx.props().is_admitted && self._poll_interval.is_none() {
            let link = ctx.link().clone();
            self._poll_interval = Some(Interval::new(3000, move || {
                link.send_message(HostControlsMsg::FetchWaiting);
            }));
            ctx.link().send_message(HostControlsMsg::FetchWaiting);
        }
        true
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            HostControlsMsg::FetchWaiting => {
                if !ctx.props().is_admitted {
                    return false;
                }

                let meeting_id = ctx.props().meeting_id.clone();
                let link = ctx.link().clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match fetch_waiting(&meeting_id).await {
                        Ok(waiting) => link.send_message(HostControlsMsg::WaitingReceived(waiting)),
                        Err(e) => link.send_message(HostControlsMsg::FetchError(e)),
                    }
                });

                false
            }
            HostControlsMsg::WaitingReceived(waiting) => {
                self.waiting = waiting;
                self.error = None;
                true
            }
            HostControlsMsg::FetchError(error) => {
                log::warn!("Failed to fetch waiting room: {error}");
                self.error = Some(error);
                true
            }
            HostControlsMsg::Admit(email) => {
                let meeting_id = ctx.props().meeting_id.clone();
                let link = ctx.link().clone();
                let email_for_api = email.clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match admit_participant(&meeting_id, &email_for_api).await {
                        Ok(_) => link.send_message(HostControlsMsg::ActionComplete),
                        Err(e) => link.send_message(HostControlsMsg::ActionError(e)),
                    }
                });

                // Remove from local list immediately for responsiveness
                self.waiting.retain(|p| p.email != email);
                true
            }
            HostControlsMsg::AdmitAll => {
                let meeting_id = ctx.props().meeting_id.clone();
                let link = ctx.link().clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match admit_all_participants(&meeting_id).await {
                        Ok(_) => link.send_message(HostControlsMsg::ActionComplete),
                        Err(e) => link.send_message(HostControlsMsg::ActionError(e)),
                    }
                });

                // Clear local list immediately for responsiveness
                self.waiting.clear();
                true
            }
            HostControlsMsg::Reject(email) => {
                let meeting_id = ctx.props().meeting_id.clone();
                let link = ctx.link().clone();
                let email_for_api = email.clone();

                wasm_bindgen_futures::spawn_local(async move {
                    match reject_participant(&meeting_id, &email_for_api).await {
                        Ok(_) => link.send_message(HostControlsMsg::ActionComplete),
                        Err(e) => link.send_message(HostControlsMsg::ActionError(e)),
                    }
                });

                // Remove from local list immediately for responsiveness
                self.waiting.retain(|p| p.email != email);
                true
            }
            HostControlsMsg::ActionComplete => {
                // Refresh the list
                ctx.link().send_message(HostControlsMsg::FetchWaiting);
                false
            }
            HostControlsMsg::ActionError(error) => {
                self.error = Some(error);
                ctx.link().send_message(HostControlsMsg::FetchWaiting);
                true
            }
            HostControlsMsg::ToggleExpanded => {
                self.expanded = !self.expanded;
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        if !ctx.props().is_admitted || self.waiting.is_empty() {
            return html! {};
        }

        let toggle = ctx.link().callback(|_| HostControlsMsg::ToggleExpanded);
        let on_admit_all = ctx.link().callback(|_| HostControlsMsg::AdmitAll);
        let show_admit_all = self.waiting.len() > 1;

        html! {
            <div class="host-controls-container">
                <button class="host-controls-toggle" onclick={toggle}>
                    <span class="waiting-badge">{self.waiting.len()}</span>
                    <span>{"Waiting to join"}</span>
                    <svg
                        class={if self.expanded { "chevron-icon expanded" } else { "chevron-icon" }}
                        xmlns="http://www.w3.org/2000/svg"
                        width="16"
                        height="16"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <polyline points="6 9 12 15 18 9"></polyline>
                    </svg>
                </button>

                {
                    if self.expanded {
                        html! {
                            <div class="host-controls-list">
                                {
                                    if show_admit_all {
                                        html! {
                                            <div class="admit-all-container">
                                                <button
                                                    class="btn-admit-all"
                                                    onclick={on_admit_all}
                                                >
                                                    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                        <polyline points="20 6 9 17 4 12"></polyline>
                                                    </svg>
                                                    {format!("Admit all ({})", self.waiting.len())}
                                                </button>
                                            </div>
                                        }
                                    } else {
                                        html! {}
                                    }
                                }
                                { for self.waiting.iter().map(|participant| {
                                    let email = participant.email.clone();
                                    let email_for_admit = email.clone();
                                    let email_for_reject = email.clone();

                                    let on_admit = ctx.link().callback(move |_| {
                                        HostControlsMsg::Admit(email_for_admit.clone())
                                    });
                                    let on_reject = ctx.link().callback(move |_| {
                                        HostControlsMsg::Reject(email_for_reject.clone())
                                    });

                                    html! {
                                        <div class="waiting-participant">
                                            <span class="participant-email">{&email}</span>
                                            <div class="participant-actions">
                                                <button
                                                    class="btn-admit"
                                                    onclick={on_admit}
                                                    title="Admit"
                                                >
                                                    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                        <polyline points="20 6 9 17 4 12"></polyline>
                                                    </svg>
                                                </button>
                                                <button
                                                    class="btn-reject"
                                                    onclick={on_reject}
                                                    title="Reject"
                                                >
                                                    <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                                        <line x1="18" y1="6" x2="6" y2="18"></line>
                                                        <line x1="6" y1="6" x2="18" y2="18"></line>
                                                    </svg>
                                                </button>
                                            </div>
                                        </div>
                                    }
                                })}
                            </div>
                        }
                    } else {
                        html! {}
                    }
                }
            </div>
        }
    }
}

async fn fetch_waiting(meeting_id: &str) -> Result<Vec<WaitingParticipant>, String> {
    let base_url = meeting_api_base_url().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings/{}/waiting", base_url, meeting_id);

    let response = Request::get(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => {
            let wrapper: APIResponse<WaitingRoomResponse> = response
                .json()
                .await
                .map_err(|e| format!("Failed to parse response: {e}"))?;
            Ok(wrapper.result.waiting)
        }
        401 => Err("Not authenticated".to_string()),
        403 => Err("Not authorized".to_string()),
        404 => Ok(Vec::new()), // Meeting not found, return empty
        status => Err(format!("Server error: {status}")),
    }
}

async fn admit_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let base_url = meeting_api_base_url().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings/{}/admit", base_url, meeting_id);

    let body = serde_json::to_string(&AdmitRequest {
        email: email.to_string(),
    })
    .map_err(|e| format!("Failed to serialize: {e}"))?;

    let response = Request::post(&url)
        .credentials(RequestCredentials::Include)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => Ok(()),
        401 => Err("Not authenticated".to_string()),
        403 => Err("Not authorized".to_string()),
        404 => Err("Participant not found".to_string()),
        status => Err(format!("Server error: {status}")),
    }
}

async fn reject_participant(meeting_id: &str, email: &str) -> Result<(), String> {
    let base_url = meeting_api_base_url().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings/{}/reject", base_url, meeting_id);

    let body = serde_json::to_string(&AdmitRequest {
        email: email.to_string(),
    })
    .map_err(|e| format!("Failed to serialize: {e}"))?;

    let response = Request::post(&url)
        .credentials(RequestCredentials::Include)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => Ok(()),
        401 => Err("Not authenticated".to_string()),
        403 => Err("Not authorized".to_string()),
        404 => Err("Participant not found".to_string()),
        status => Err(format!("Server error: {status}")),
    }
}

async fn admit_all_participants(meeting_id: &str) -> Result<(), String> {
    let base_url = meeting_api_base_url().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings/{}/admit-all", base_url, meeting_id);

    let response = Request::post(&url)
        .credentials(RequestCredentials::Include)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => Ok(()),
        401 => Err("Not authenticated".to_string()),
        403 => Err("Not authorized".to_string()),
        404 => Err("Meeting not found".to_string()),
        status => Err(format!("Server error: {status}")),
    }
}
