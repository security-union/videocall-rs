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

use crate::constants::meeting_api_client;
use crate::routing::Route;
use videocall_meeting_types::responses::{ListMeetingsResponse, MeetingSummary};
use yew::prelude::*;
use yew_router::prelude::*;

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

        // The meetings list API returns only meetings owned by the current user,
        // so all meetings in the response belong to us. No need to check ownership.
        let current_user_email = ctx.props().user_email.clone();

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
        let is_active = meeting.state == "active";
        let is_ended = meeting.state == "ended";

        // The meetings list API returns only meetings owned by the current user,
        // so all meetings here are ours and we can always show the delete button.
        let is_owner = true;

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

        // Calculate duration for ended meetings
        let duration_ms = meeting
            .ended_at
            .map(|ended_at| ended_at - meeting.started_at)
            .unwrap_or(0);

        html! {
            <li class={classes!("meeting-item", if is_ended { "meeting-ended" } else { "" })}>
                <div class="meeting-item-content" onclick={on_click}>
                    <div class="meeting-info">
                        <span class="meeting-id">{&meeting.meeting_id}</span>
                        <span class={classes!("meeting-state", state_class)}>{&meeting.state}</span>
                    </div>
                    <div class="meeting-details">
                        // For active meetings: show participants and waiting count
                        if is_active {
                            <span class="meeting-participants" title="Participants in meeting">
                                <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                    <circle cx="9" cy="7" r="4"></circle>
                                    <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                    <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                                </svg>
                                {format!("{} joined", meeting.participant_count)}
                            </span>
                            if meeting.waiting_count > 0 {
                                <span class="meeting-waiting" title="Waiting to join">
                                    <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                        <circle cx="12" cy="12" r="10"></circle>
                                        <line x1="12" y1="8" x2="12" y2="12"></line>
                                        <line x1="12" y1="16" x2="12.01" y2="16"></line>
                                    </svg>
                                    {format!("{} waiting", meeting.waiting_count)}
                                </span>
                            }
                        }
                        // For ended meetings: show duration, start time, and end time
                        if is_ended {
                            <span class="meeting-duration" title="Total duration">
                                <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <circle cx="12" cy="12" r="10"></circle>
                                    <polyline points="12 6 12 12 16 14"></polyline>
                                </svg>
                                {format_duration(duration_ms)}
                            </span>
                            <span class="meeting-time" title={format!("Started at {}", format_time(meeting.started_at))}>
                                {format_time(meeting.started_at)}
                            </span>
                            <span class="meeting-time-separator">{"-"}</span>
                            if let Some(ended_at) = meeting.ended_at {
                                <span class="meeting-time" title={format!("Ended at {}", format_time(ended_at))}>
                                    {format_time(ended_at)}
                                </span>
                            }
                        }
                        // For idle meetings (not yet started): show created info
                        if !is_active && !is_ended {
                            <span class="meeting-participants" title="Participants">
                                <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                    <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                    <circle cx="9" cy="7" r="4"></circle>
                                    <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                    <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                                </svg>
                                {meeting.participant_count}
                            </span>
                        }
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
                                class={classes!("meeting-delete-btn", if is_ended { "meeting-delete-btn-ended" } else { "" })}
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

async fn fetch_meetings() -> Result<ListMeetingsResponse, String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .list_meetings(20, 0)
        .await
        .map_err(|e| format!("{e}"))
}

async fn delete_meeting(meeting_id: &str) -> Result<(), String> {
    let client = meeting_api_client().map_err(|e| format!("Config error: {e}"))?;
    client
        .delete_meeting(meeting_id)
        .await
        .map(|_| ())
        .map_err(|e| format!("{e}"))
}

/// Format a duration in milliseconds to a human-readable string (e.g., "1h 23m" or "45m 12s")
fn format_duration(duration_ms: i64) -> String {
    let total_seconds = duration_ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Format a timestamp in milliseconds to a time string (e.g., "2:30 PM")
fn format_time(timestamp_ms: i64) -> String {
    // Convert to JavaScript Date for formatting
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    let am_pm = if hours >= 12 { "PM" } else { "AM" };
    let hours_12 = if hours == 0 {
        12
    } else if hours > 12 {
        hours - 12
    } else {
        hours
    };
    format!("{hours_12}:{minutes:02} {am_pm}")
}
