/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Shared formatters for meeting list rows.
//!
//! Used by both [`crate::components::meetings_list`] (owned meetings) and
//! [`crate::components::joined_meetings_list`] (previously-joined meetings).
//! Both functions are pure and operate on millisecond integers — no Dioxus
//! signals, no DOM, no async. Kept here so the two list components share one
//! definition rather than maintaining byte-identical copies.

/// Format a duration in milliseconds as a compact human-readable string.
///
/// Output examples:
/// - `3_661_000` -> `"1h 1m"`
/// - `125_000`   -> `"2m 5s"`
/// - `42_000`    -> `"42s"`
pub fn format_duration(duration_ms: i64) -> String {
    let total_seconds = duration_ms / 1000;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Format a Unix-epoch timestamp (in milliseconds) as a 12-hour wall clock
/// time in the user's local timezone, e.g. `"3:07 PM"`.
///
/// Uses `js_sys::Date` so timezone resolution matches the browser the user is
/// running in.
pub fn format_time(timestamp_ms: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    let hours = date.get_hours();
    let minutes = date.get_minutes();
    let am_pm = if hours >= 12 { "PM" } else { "AM" };
    let hours_12 = if hours == 0 {
        12
    } else if hours > 12 {
        hours - 12
    } else {
        hours
    };
    format!("{hours_12}:{minutes:02} {am_pm}")
}
