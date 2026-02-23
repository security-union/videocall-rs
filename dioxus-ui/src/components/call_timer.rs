// SPDX-License-Identifier: MIT OR Apache-2.0

//! Self-contained timer component for displaying elapsed call duration.

use dioxus::prelude::*;
use gloo_timers::callback::Interval;
use std::cell::RefCell;
use std::rc::Rc;

const NO_TIME_PLACEHOLDER: &str = "--:--";

#[component]
pub fn CallTimer(start_time_ms: Option<f64>) -> Element {
    let mut duration = use_signal(|| NO_TIME_PLACEHOLDER.to_string());

    let mut start_signal = use_signal(|| start_time_ms);
    if start_signal() != start_time_ms {
        start_signal.set(start_time_ms);
    }

    // Keep the interval alive via use_hook (stored for component lifetime)
    let _interval: Rc<RefCell<Option<Interval>>> = use_hook(|| {
        let interval = Interval::new(1000, move || {
            if let Some(start_ms) = start_signal() {
                duration.set(format_duration(start_ms));
            } else {
                duration.set(NO_TIME_PLACEHOLDER.to_string());
            }
        });
        Rc::new(RefCell::new(Some(interval)))
    });

    use_effect(move || {
        if let Some(start_ms) = start_signal() {
            duration.set(format_duration(start_ms));
        } else {
            duration.set(NO_TIME_PLACEHOLDER.to_string());
        }
    });

    rsx! { "{duration}" }
}

fn format_duration(start_ms: f64) -> String {
    let now_ms = js_sys::Date::now();
    let elapsed_ms = (now_ms - start_ms).max(0.0);
    let elapsed_secs = (elapsed_ms / 1000.0) as u64;

    let hours = elapsed_secs / 3600;
    let minutes = (elapsed_secs % 3600) / 60;
    let seconds = elapsed_secs % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}
