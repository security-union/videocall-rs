use yew::{function_component, html, Html, Properties};

#[derive(Properties, PartialEq)]
pub struct MeetingInfoProps {
    #[prop_or_default]
    pub is_open: bool,

    #[prop_or_default]
    pub onclose: yew::Callback<()>,

    #[prop_or_default]
    pub room_id: String,

    #[prop_or_default]
    pub num_participants: usize,

    #[prop_or_default]
    pub meeting_duration: String,

    #[prop_or_default]
    pub user_meeting_duration: String,

    #[prop_or_default]
    pub started_at: Option<String>,

    #[prop_or_default]
    pub ended_at: Option<String>,

    #[prop_or_default]
    pub is_active: bool,
}

#[function_component(MeetingInfo)]
pub fn meeting_info(props: &MeetingInfoProps) -> Html {
    if !props.is_open {
        return html! {};
    }

    html! {
        <div class="meeting-info-panel">
            <div class="meeting-info-content">
                <div class="info-section">
                    <div class="info-item">
                        <div class="info-icon">
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z"></path>
                                <polyline points="3.27 6.96 12 12.01 20.73 6.96"></polyline>
                                <line x1="12" y1="22.08" x2="12" y2="12"></line>
                            </svg>
                        </div>
                        <div class="info-details">
                            <span class="info-label">{"Room ID"}</span>
                            <span class="info-value">{&props.room_id}</span>
                        </div>
                    </div>

                    <div class="info-item">
                        <div class="info-icon">
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <circle cx="12" cy="12" r="10"></circle>
                                <polyline points="12 6 12 12 16 14"></polyline>
                            </svg>
                        </div>
                        <div class="info-details">
                            <span class="info-label">{"Meeting Duration"}</span>
                            <span class="info-value">
                                {&props.meeting_duration}
                                {
                                    if props.is_active {
                                        html! { <span class="live-badge">{"LIVE"}</span> }
                                    } else {
                                        html! {}
                                    }
                                }
                            </span>
                        </div>
                    </div>


                    <div class="info-item">
                        <div class="info-icon">
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <circle cx="12" cy="12" r="10"></circle>
                                <polyline points="12 6 12 12 16 14"></polyline>
                            </svg>
                        </div>
                        <div class="info-details">
                            <span class="info-label">{"My Duration"}</span>
                            <span class="info-value">
                                {&props.user_meeting_duration}
                                {
                                    if props.is_active {
                                        html! { <span class="live-badge">{"LIVE"}</span> }
                                    } else {
                                        html! {}
                                    }
                                }
                            </span>
                        </div>
                    </div>

                    <div class="info-item">
                        <div class="info-icon">
                            <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                <path d="M17 21v-2a4 4 0 0 0-4-4H5a4 4 0 0 0-4 4v2"></path>
                                <circle cx="9" cy="7" r="4"></circle>
                                <path d="M23 21v-2a4 4 0 0 0-3-3.87"></path>
                                <path d="M16 3.13a4 4 0 0 1 0 7.75"></path>
                            </svg>
                        </div>
                        <div class="info-details">
                            <span class="info-label">{"Participants"}</span>
                            <span class="info-value">{props.num_participants + 1}</span>
                        </div>
                    </div>

                    <div class="info-item">
                        <div class="info-icon">
                            {
                                if props.is_active {
                                    html! {
                                        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#4ade80" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                            <path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"></path>
                                            <polyline points="22 4 12 14.01 9 11.01"></polyline>
                                        </svg>
                                    }
                                } else {
                                    html! {
                                        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="#ef4444" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                            <circle cx="12" cy="12" r="10"></circle>
                                            <line x1="15" y1="9" x2="9" y2="15"></line>
                                            <line x1="9" y1="9" x2="15" y2="15"></line>
                                        </svg>
                                    }
                                }
                            }
                        </div>
                        <div class="info-details">
                            <span class="info-label">{"Status"}</span>
                            <span class={if props.is_active { "info-value status-active" } else { "info-value status-ended" }}>
                                {if props.is_active { "Active" } else { "Ended" }}
                            </span>
                        </div>
                    </div>

                    {
                        if let Some(ref started) = props.started_at {
                            html! {
                                <div class="info-item">
                                    <div class="info-icon">
                                        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                            <rect x="3" y="4" width="18" height="18" rx="2" ry="2"></rect>
                                            <line x1="16" y1="2" x2="16" y2="6"></line>
                                            <line x1="8" y1="2" x2="8" y2="6"></line>
                                            <line x1="3" y1="10" x2="21" y2="10"></line>
                                        </svg>
                                    </div>
                                    <div class="info-details">
                                        <span class="info-label">{"Started"}</span>
                                        <span class="info-value">{started}</span>
                                    </div>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }

                    {
                        if let Some(ref ended) = props.ended_at {
                            html! {
                                <div class="info-item">
                                    <div class="info-icon">
                                        <svg xmlns="http://www.w3.org/2000/svg" width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                                            <rect x="3" y="4" width="18" height="18" rx="2" ry="2"></rect>
                                            <line x1="16" y1="2" x2="16" y2="6"></line>
                                            <line x1="8" y1="2" x2="8" y2="6"></line>
                                            <line x1="3" y1="10" x2="21" y2="10"></line>
                                        </svg>
                                    </div>
                                    <div class="info-details">
                                        <span class="info-label">{"Ended"}</span>
                                        <span class="info-value">{ended}</span>
                                    </div>
                                </div>
                            }
                        } else {
                            html! {}
                        }
                    }
                </div>
            </div>
        </div>
    }
}
