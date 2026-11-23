/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::components::call_timer::CallTimer;
use crate::context::MeetingTimeCtx;
use dioxus::prelude::*;
use videocall_theme::{
    Badge, BadgeTone, ListGroup, ListRow, ListRowLabel, ListRowValue, StatusDot, StatusTone,
};

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
        ListGroup { class: "meeting-info",
            ListRow {
                ListRowLabel { "Room" }
                ListRowValue { class: "text-mono", "{room_id}" }
            }
            ListRow {
                ListRowLabel { "Meeting Time" }
                ListRowValue { class: "row gap-2",
                    if is_active {
                        StatusDot { tone: StatusTone::Success, label: "Live" }
                    }
                    CallTimer { start_time_ms: meeting_start }
                }
            }
            ListRow {
                ListRowLabel { "Your Time" }
                ListRowValue {
                    CallTimer { start_time_ms: call_start }
                }
            }
            ListRow {
                ListRowLabel { "Participants" }
                ListRowValue { class: "text-tabular", "{num_participants + 1}" }
            }
            ListRow {
                ListRowLabel { "Status" }
                Badge {
                    tone: if is_active { BadgeTone::Success } else { BadgeTone::Neutral },
                    class: "list-row-value",
                    if is_active { "Active" } else { "Ended" }
                }
            }
        }
    }
}
