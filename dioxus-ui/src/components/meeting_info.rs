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
 */

use crate::components::call_timer::CallTimer;
use crate::context::{MeetingTime, MeetingTimeCtx};
use dioxus::prelude::*;

#[component]
pub fn MeetingInfo(
    #[props(default = false)] is_open: bool,
    #[props(default)] onclose: EventHandler<()>,
    #[props(default)] room_id: String,
    #[props(default = 0)] num_participants: usize,
    #[props(default = false)] is_active: bool,
) -> Element {
    let meeting_time_ctx: Option<Signal<MeetingTime>> = try_use_context::<MeetingTimeCtx>();
    let meeting_time = meeting_time_ctx.map(|s| s.read().clone()).unwrap_or_default();

    if !is_open {
        return rsx! {};
    }

    let meeting_start = meeting_time.meeting_start_time;
    let call_start = meeting_time.call_start_time;

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
