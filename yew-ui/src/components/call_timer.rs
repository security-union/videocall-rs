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

use gloo_timers::callback::Interval;
use yew::prelude::*;

/// Placeholder shown when no start time is available.
const NO_TIME_PLACEHOLDER: &str = "--:--";

#[derive(Properties, PartialEq, Clone)]
pub struct CallTimerProps {
    /// Unix timestamp in milliseconds when the timer started.
    /// If `None`, displays "--:--".
    #[prop_or_default]
    pub start_time_ms: Option<f64>,
}

/// Self-contained timer component that updates independently without
/// triggering parent re-renders. Uses internal state and interval.
///
/// Renders inline text only (no wrapper element).
#[function_component(CallTimer)]
pub fn call_timer(props: &CallTimerProps) -> Html {
    let duration = use_state(|| NO_TIME_PLACEHOLDER.to_string());
    let start_time = props.start_time_ms;

    {
        let duration = duration.clone();
        use_effect_with(start_time, move |start_time| {
            let start_time = *start_time;

            // Initial update
            if let Some(start_ms) = start_time {
                duration.set(format_duration(start_ms));
            } else {
                duration.set(NO_TIME_PLACEHOLDER.to_string());
            }

            // Set up interval for continuous updates
            let interval = if start_time.is_some() {
                let duration = duration.clone();
                Some(Interval::new(1000, move || {
                    if let Some(start_ms) = start_time {
                        duration.set(format_duration(start_ms));
                    }
                }))
            } else {
                None
            };

            // Cleanup on unmount or when start_time changes
            move || {
                drop(interval);
            }
        });
    }

    html! { { (*duration).clone() } }
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
