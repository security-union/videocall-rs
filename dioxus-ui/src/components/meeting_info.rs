/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::call_timer::CallTimer;
use crate::context::MeetingTimeCtx;
use dioxus::prelude::*;

#[component]
pub fn MeetingInfo(
    is_open: bool,
    onclose: EventHandler<()>,
    room_id: String,
    num_participants: usize,
    is_active: bool,
) -> Element {
    let meeting_time: MeetingTimeCtx = use_context();

    if !is_open {
        return rsx! {};
    }

    let meeting_start = meeting_time().meeting_start_time;
    let call_start = meeting_time().call_start_time;

    rsx! {
        div { class: "meeting-info-compact",
            div { class: "info-row",
                span { class: "info-label", "Room" }
                span { class: "info-value", "{room_id}" }
            }
            div { class: "info-row",
                span { class: "info-label", "Meeting Time" }
                span { class: "info-value",
                    if is_active {
                        span { class: "live-dot" }
                    }
                    CallTimer { start_time_ms: meeting_start }
                }
            }
            div { class: "info-row",
                span { class: "info-label", "Your Time" }
                span { class: "info-value",
                    CallTimer { start_time_ms: call_start }
                }
            }
            div { class: "info-row",
                span { class: "info-label", "Participants" }
                span { class: "info-value", "{num_participants + 1}" }
            }
            div { class: "info-row",
                span { class: "info-label", "Status" }
                span {
                    class: if is_active { "info-value status-active" } else { "info-value status-ended" },
                    if is_active { "Active" } else { "Ended" }
                }
            }
        }
    }
}
