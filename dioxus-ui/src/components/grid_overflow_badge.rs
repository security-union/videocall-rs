// Copyright 2025 Security Union LLC
// Licensed under MIT OR Apache-2.0

use dioxus::prelude::*;

use crate::components::overflow_participants::{BADGE_ID, LIST_ID};

/// The `+N more in meeting` cell for the last grid slot. `data-overflow-count`
/// repeats the count for `recording.js`, which draws its own grid. Toggles
/// `open`, the list of those participants, calling `on_open` when it opens,
/// and closes it when unmounted.
#[component]
pub fn GridOverflowBadge(
    overflow_count: usize,
    open: Signal<bool>,
    on_open: EventHandler<()>,
) -> Element {
    use_drop(move || {
        if open.try_peek().is_ok_and(|v| *v) {
            if let Ok(mut w) = open.try_write() {
                *w = false;
            }
        }
    });
    rsx! {
        button {
            id: BADGE_ID,
            class: "grid-overflow-badge",
            r#type: "button",
            "data-overflow-count": "{overflow_count}",
            "aria-haspopup": "dialog",
            "aria-expanded": if open() { "true" } else { "false" },
            "aria-controls": LIST_ID,
            onclick: move |e: MouseEvent| {
                e.stop_propagation();
                let next = !*open.peek();
                open.set(next);
                if next {
                    on_open.call(());
                }
            },
            "+{overflow_count}"
            span { "more in meeting" }
        }
    }
}
