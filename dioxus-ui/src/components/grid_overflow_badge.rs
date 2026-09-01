// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0

use dioxus::prelude::*;

/// The `+N more in meeting` cell for the last grid slot. `data-overflow-count`
/// repeats the count for `recording.js`, which draws its own grid.
#[component]
pub fn GridOverflowBadge(overflow_count: usize) -> Element {
    rsx! {
        div {
            class: "grid-overflow-badge",
            "data-overflow-count": "{overflow_count}",
            "+{overflow_count}"
            span { "more in meeting" }
        }
    }
}
