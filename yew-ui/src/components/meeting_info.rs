use crate::components::call_timer::CallTimer;
use crate::context::MeetingTimeCtx;
use yew::prelude::*;

#[derive(Properties, PartialEq)]
pub struct MeetingInfoProps {
    #[prop_or_default]
    pub is_open: bool,

    #[prop_or_default]
    pub onclose: Callback<()>,

    #[prop_or_default]
    pub room_id: String,

    #[prop_or_default]
    pub num_participants: usize,

    #[prop_or_default]
    pub is_active: bool,
}

#[function_component(MeetingInfo)]
pub fn meeting_info(props: &MeetingInfoProps) -> Html {
    let meeting_time = use_context::<MeetingTimeCtx>().unwrap_or_default();

    if !props.is_open {
        return html! {};
    }

    let meeting_start = meeting_time.meeting_start_time;
    let call_start = meeting_time.call_start_time;

    html! {
        <div class="meeting-info-compact">
            <div class="info-row">
                <span class="info-label">{"Room"}</span>
                <span class="info-value">{&props.room_id}</span>
            </div>
            <div class="info-row">
                <span class="info-label">{"Meeting Time"}</span>
                <span class="info-value">
                { if props.is_active { html! { <span class="live-dot"></span> } } else { html! {} } }
                    <CallTimer start_time_ms={meeting_start} />
                </span>
            </div>
            <div class="info-row">
                <span class="info-label">{"Your Time"}</span>
                <span class="info-value">
                    <CallTimer start_time_ms={call_start} />
                </span>
            </div>
            <div class="info-row">
                <span class="info-label">{"Participants"}</span>
                <span class="info-value">{props.num_participants + 1}</span>
            </div>
            <div class="info-row">
                <span class="info-label">{"Status"}</span>
                <span class={if props.is_active { "info-value status-active" } else { "info-value status-ended" }}>
                    {if props.is_active { "Active" } else { "Ended" }}
                </span>
            </div>
        </div>
    }
}
