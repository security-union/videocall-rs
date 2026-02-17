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

//! Self-contained timer component for displaying elapsed call duration.

use dioxus::prelude::*;
use gloo_timers::callback::Interval;

/// Placeholder shown when no start time is available.
const NO_TIME_PLACEHOLDER: &str = "--:--";

/// Self-contained timer component that updates independently without
/// triggering parent re-renders. Uses internal state and interval.
///
/// Renders inline text only (no wrapper element).
#[component]
pub fn CallTimer(
    /// Unix timestamp in milliseconds when the timer started.
    /// If `None`, displays "--:--".
    #[props(default)]
    start_time_ms: Option<f64>,
) -> Element {
    let mut duration = use_signal(|| NO_TIME_PLACEHOLDER.to_string());

    // Effect to update duration and set up interval
    use_effect(move || {
        // Initial update
        if let Some(start_ms) = start_time_ms {
            duration.set(format_duration(start_ms));
        } else {
            duration.set(NO_TIME_PLACEHOLDER.to_string());
        }

        // Set up interval for continuous updates
        let interval = if let Some(start_ms) = start_time_ms {
            Some(Interval::new(1000, move || {
                duration.set(format_duration(start_ms));
            }))
        } else {
            None
        };

        // Cleanup on unmount or when start_time changes
        move || {
            drop(interval);
        }
    });

    rsx! { "{duration}" }
}

/// Format duration from start time to now.
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
