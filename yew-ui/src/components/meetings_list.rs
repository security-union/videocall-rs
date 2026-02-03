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

use crate::constants::app_config;
use crate::Route;
use reqwasm::http::{Request, RequestCredentials};
use serde::Deserialize;
use wasm_bindgen::JsCast;
use yew::prelude::*;
use yew_router::prelude::*;

#[derive(Debug, Clone, Deserialize)]
pub struct MeetingSummary {
    pub meeting_id: String,
    pub host: Option<String>,
    pub state: String,
    pub has_password: bool,
    pub created_at: i64,
    pub participant_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListMeetingsResponse {
    pub meetings: Vec<MeetingSummary>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

pub enum MeetingsListMsg {
    FetchMeetings,
    FetchSuccess(ListMeetingsResponse),
    FetchError(String),
    ToggleExpanded,
    DeleteMeeting(String),
    DeleteSuccess(String),
    DeleteError(String),
}

pub struct MeetingsList {
    meetings: Vec<MeetingSummary>,
    loading: bool,
    error: Option<String>,
    expanded: bool,
    total: i64,
    current_user_email: Option<String>,
}

#[derive(Properties, Clone, PartialEq)]
pub struct MeetingsListProps {
    /// Callback when a meeting is selected for joining
    #[prop_or_default]
    pub on_select_meeting: Option<Callback<String>>,
    /// Current user's email for determining ownership
    #[prop_or_default]
    pub user_email: Option<String>,
}

impl Component for MeetingsList {
    type Message = MeetingsListMsg;
    type Properties = MeetingsListProps;

    fn create(ctx: &Context<Self>) -> Self {
        // Fetch meetings on component creation
        ctx.link().send_message(MeetingsListMsg::FetchMeetings);

        // Try to get email from cookie
        let current_user_email = get_email_from_cookie().or_else(|| ctx.props().user_email.clone());

        Self {
            meetings: Vec::new(),
            loading: true,
            error: None,
            expanded: true, // Show active meetings by default
            total: 0,
            current_user_email,
        }
    }

    fn changed(&mut self, ctx: &Context<Self>, _old_props: &Self::Properties) -> bool {
        // Update email if prop changes
        if let Some(email) = &ctx.props().user_email {
            self.current_user_email = Some(email.clone());
        }
        true
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            MeetingsListMsg::FetchMeetings => {
                self.loading = true;
                self.error = None;

                let link = ctx.link().clone();
                wasm_bindgen_futures::spawn_local(async move {
                    match fetch_meetings().await {
                        Ok(response) => {
                            link.send_message(MeetingsListMsg::FetchSuccess(response));
                        }
                        Err(e) => {
                            link.send_message(MeetingsListMsg::FetchError(e));
                        }
                    }
                });

                true
            }
            MeetingsListMsg::FetchSuccess(response) => {
                self.meetings = response.meetings;
                self.total = response.total;
                self.loading = false;
                self.error = None;
                true
            }
            MeetingsListMsg::FetchError(error) => {
                self.loading = false;
                self.error = Some(error);
                true
            }
            MeetingsListMsg::ToggleExpanded => {
                self.expanded = !self.expanded;
                // Refresh when expanding
                if self.expanded {
                    ctx.link().send_message(MeetingsListMsg::FetchMeetings);
                }
                true
            }
            MeetingsListMsg::DeleteMeeting(meeting_id) => {
                let link = ctx.link().clone();
                let meeting_id_clone = meeting_id.clone();

                // Remove from local list immediately for responsiveness
                self.meetings.retain(|m| m.meeting_id != meeting_id);
                self.total = self.total.saturating_sub(1);

                wasm_bindgen_futures::spawn_local(async move {
                    match delete_meeting(&meeting_id_clone).await {
                        Ok(_) => {
                            link.send_message(MeetingsListMsg::DeleteSuccess(meeting_id_clone));
                        }
                        Err(e) => {
                            link.send_message(MeetingsListMsg::DeleteError(e));
                        }
                    }
                });

                true
            }
            MeetingsListMsg::DeleteSuccess(_meeting_id) => {
                // Refresh the list to ensure consistency
                ctx.link().send_message(MeetingsListMsg::FetchMeetings);
                false
            }
            MeetingsListMsg::DeleteError(error) => {
                self.error = Some(error);
                // Refresh to restore the list
                ctx.link().send_message(MeetingsListMsg::FetchMeetings);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let toggle_expanded = ctx.link().callback(|_| MeetingsListMsg::ToggleExpanded);
        let refresh = ctx.link().callback(|_| MeetingsListMsg::FetchMeetings);

        html! {
            <div class="meetings-list-container">
                <button
                    class="meetings-list-toggle"
                    onclick={toggle_expanded}
                    type="button"
                >
                    <svg
                        class={if self.expanded { "chevron-icon expanded" } else { "chevron-icon" }}
                        xmlns="http://www.w3.org/2000/svg"
                        width="20"
                        height="20"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="2"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <polyline points="6 9 12 15 18 9"></polyline>
                    </svg>
                    <span>{"My Meetings"}</span>
                    <span class="meeting-count">{format!("({})", self.total)}</span>
                </button>

                {
                    if self.expanded {
                        html! {
                            <div class="meetings-list-content">
                                {
                                    if self.loading {
                                        html! {
                                            <div class="meetings-loading">
                                                <span class="loading-spinner"></span>
                                                {"Loading meetings..."}
                                            </div>
                                        }
                                    } else if let Some(error) = &self.error {
                                        html! {
                                            <div class="meetings-error">
                                                <span>{format!("Error: {}", error)}</span>
                                                <button onclick={refresh} class="retry-btn">{"Retry"}</button>
                                            </div>
                                        }
                                    } else if self.meetings.is_empty() {
                                        html! {
                                            <div class="meetings-empty">
                                                {"No meetings yet"}
                                            </div>
                                        }
                                    } else {
                                        html! {
                                            <ul class="meetings-list">
                                                { for self.meetings.iter().map(|meeting| {
                                                    self.render_meeting_item(ctx, meeting)
                                                })}
                                            </ul>
                                        }
                                    }
                                }
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

impl MeetingsList {
    fn render_meeting_item(&self, ctx: &Context<Self>, meeting: &MeetingSummary) -> Html {
        let meeting_id = meeting.meeting_id.clone();
        let navigator = ctx.link().navigator().unwrap();

        // Check if current user is the owner
        let is_owner = self
            .current_user_email
            .as_ref()
            .map(|email| meeting.host.as_ref() == Some(email))
            .unwrap_or(false);

        let on_click = {
            let meeting_id = meeting_id.clone();
            let on_select = ctx.props().on_select_meeting.clone();
            let navigator = navigator.clone();
            Callback::from(move |_| {
                if let Some(ref callback) = on_select {
                    callback.emit(meeting_id.clone());
                } else {
                    navigator.push(&Route::Meeting {
                        id: meeting_id.clone(),
                    });
                }
            })
        };

        let on_delete = {
            let meeting_id = meeting_id.clone();
            let link = ctx.link().clone();
            Callback::from(move |e: MouseEvent| {
                e.stop_propagation(); // Prevent triggering the row click
                if web_sys::window()
                    .and_then(|w| {
                        w.confirm_with_message("Are you sure you want to delete this meeting?")
                            .ok()
                    })
                    .unwrap_or(false)
                {
                    link.send_message(MeetingsListMsg::DeleteMeeting(meeting_id.clone()));
                }
            })
        };

        let state_class = match meeting.state.as_str() {
            "active" => "state-active",
            "idle" => "state-idle",
            _ => "state-ended",
        };

        html! {
            <li class="meeting-item">
                <div class="meeting-item-content" onclick={on_click}>
                    <div class="meeting-info">
                        <span class="meeting-id">{&meeting.meeting_id}</span>
                        <span class={classes!("meeting-state", state_class)}>{&meeting.state}</span>
                    </div>
                    <div class="meeting-details">
                        {
                            if let Some(host) = &meeting.host {
                                html! { <span class="meeting-host">{format!("Host: {}", host)}</span> }
                            } else {
                                html! {}
                            }
                        }
                        <span class="meeting-participants">
                            <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                <circle cx="9" cy="7" r="4"></circle>
                                <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                            </svg>
                            {meeting.participant_count}
                        </span>
                        {
                            if meeting.has_password {
                                html! {
                                    <span class="meeting-password" title="Password protected">
                                        <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                            <rect x="3" y="11" width="18" height="11" rx="2" ry="2"></rect>
                                            <path d="M7 11V7a5 5 0 0 1 10 0v4"></path>
                                        </svg>
                                    </span>
                                }
                            } else {
                                html! {}
                            }
                        }
                    </div>
                </div>
                {
                    if is_owner {
                        html! {
                            <button
                                class="meeting-delete-btn"
                                onclick={on_delete}
                                title="Delete meeting"
                            >
                                <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <polyline points="3 6 5 6 21 6"></polyline>
                                    <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path>
                                    <line x1="10" y1="11" x2="10" y2="17"></line>
                                    <line x1="14" y1="11" x2="14" y2="17"></line>
                                </svg>
                            </button>
                        }
                    } else {
                        html! {}
                    }
                }
            </li>
        }
    }
}

fn get_email_from_cookie() -> Option<String> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.dyn_into::<web_sys::HtmlDocument>().ok())
        .and_then(|d| d.cookie().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                let cookie = cookie.trim();
                if cookie.starts_with("email=") {
                    Some(cookie.trim_start_matches("email=").to_string())
                } else {
                    None
                }
            })
        })
}

async fn fetch_meetings() -> Result<ListMeetingsResponse, String> {
    let config = app_config().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings?limit=20", config.api_base_url);

    let response = Request::get(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => {
            let data: ListMeetingsResponse = response
                .json()
                .await
                .map_err(|e| format!("Failed to parse response: {e}"))?;
            Ok(data)
        }
        401 => Err("Not authenticated. Please log in.".to_string()),
        status => Err(format!("Server error: {status}")),
    }
}

async fn delete_meeting(meeting_id: &str) -> Result<(), String> {
    let config = app_config().map_err(|e| format!("Config error: {e}"))?;
    let url = format!("{}/api/v1/meetings/{}", config.api_base_url, meeting_id);

    let response = Request::delete(&url)
        .credentials(RequestCredentials::Include)
        .send()
        .await
        .map_err(|e| format!("Request failed: {e}"))?;

    match response.status() {
        200 => Ok(()),
        401 => Err("Not authenticated".to_string()),
        403 => Err("Only the meeting owner can delete this meeting".to_string()),
        404 => Err("Meeting not found".to_string()),
        status => Err(format!("Server error: {status}")),
    }
}
