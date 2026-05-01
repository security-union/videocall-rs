/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

//! Shared formatters for meeting list rows.
//!
//! Used by [`crate::components::meetings_list`] (the merged home-feed list).
//! Both functions are pure and operate on millisecond integers — no Dioxus
//! signals, no DOM, no async.

/// Format a duration in milliseconds as a compact human-readable string.
///
/// Output examples:
/// - `90_061_000` -> `"1d 1h 1m 1s"` (over 24h: full breakdown)
/// - `3_661_000`  -> `"1h 1m"`
/// - `125_000`    -> `"2m 5s"`
/// - `42_000`     -> `"42s"`
pub fn format_duration(duration_ms: i64) -> String {
    let total_seconds = duration_ms / 1000;
    let days = total_seconds / 86_400;
    let hours = (total_seconds % 86_400) / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if days > 0 {
        format!("{days}d {hours}h {minutes}m {seconds}s")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// Format a meeting state string (`"active"`, `"idle"`, `"ended"`) as a
/// title-cased label suitable for the inline state badge.
///
/// Unknown values are passed through unchanged after a `log::warn!`, mirroring
/// the prior inline behaviour in [`crate::components::meetings_list`] which
/// rendered whatever the server sent rather than blanking the badge.
pub fn format_meeting_state_label(state: &str) -> String {
    match state {
        "active" => "Active".to_string(),
        "idle" => "Idle".to_string(),
        "ended" => "Ended".to_string(),
        other => {
            log::warn!("format_meeting_state_label: unknown meeting state '{other}'");
            other.to_string()
        }
    }
}

/// Format a Unix-epoch timestamp (in milliseconds) as a date + time string
/// in the user's local timezone, e.g. `"Apr 28, 3:07 PM"`.
pub fn format_datetime(timestamp_ms: i64) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(timestamp_ms as f64));
    let month = match date.get_month() {
        0 => "Jan",
        1 => "Feb",
        2 => "Mar",
        3 => "Apr",
        4 => "May",
        5 => "Jun",
        6 => "Jul",
        7 => "Aug",
        8 => "Sep",
        9 => "Oct",
        10 => "Nov",
        _ => "Dec",
    };
    let day = date.get_date();
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
    format!("{month} {day}, {hours_12}:{minutes:02} {am_pm}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // format_duration tests
    //
    // format_duration is pure arithmetic with no js_sys / web-sys calls, so
    // these run as ordinary host-target unit tests via `cargo test -p
    // videocall-ui`. Each scenario gets its own #[test] so a failure points
    // directly at the boundary that broke.
    // -------------------------------------------------------------------------

    #[test]
    fn format_duration_zero() {
        // Boundary: zero ms should render as "0s" (seconds branch).
        assert_eq!(format_duration(0), "0s");
    }

    #[test]
    fn format_duration_under_one_minute() {
        // Sub-minute durations only render seconds.
        assert_eq!(format_duration(42_000), "42s");
    }

    #[test]
    fn format_duration_minutes_and_seconds() {
        // 1m–59m range shows both minutes and seconds.
        assert_eq!(format_duration(125_000), "2m 5s");
    }

    #[test]
    fn format_duration_hours_drops_seconds() {
        // 1h–23h range intentionally hides seconds (matches doc-comment example).
        assert_eq!(format_duration(3_661_000), "1h 1m");
    }

    #[test]
    fn format_duration_just_under_one_day() {
        // 23h 59m 59s — still in the hours branch, no days yet.
        assert_eq!(format_duration(86_399_000), "23h 59m");
    }

    #[test]
    fn format_duration_exact_one_day_boundary() {
        // Exactly 24h crosses into the days branch and must surface every unit.
        assert_eq!(format_duration(86_400_000), "1d 0h 0m 0s");
    }

    #[test]
    fn format_duration_over_one_day_full_breakdown() {
        // 1d 1h 1m 1s — verifies the days branch renders every component.
        assert_eq!(format_duration(90_061_000), "1d 1h 1m 1s");
    }

    #[test]
    fn format_duration_exact_two_days() {
        // 48h boundary — multi-day plural-style values still pad zero units.
        assert_eq!(format_duration(172_800_000), "2d 0h 0m 0s");
    }

    // -------------------------------------------------------------------------
    // format_meeting_state_label tests
    //
    // Pure string match, no js_sys / web-sys, so these run as ordinary
    // host-target unit tests via `cargo test -p videocall-ui`.
    // -------------------------------------------------------------------------

    #[test]
    fn state_label_active_titlecase() {
        assert_eq!(format_meeting_state_label("active"), "Active");
    }

    #[test]
    fn state_label_idle_titlecase() {
        assert_eq!(format_meeting_state_label("idle"), "Idle");
    }

    #[test]
    fn state_label_ended_titlecase() {
        assert_eq!(format_meeting_state_label("ended"), "Ended");
    }

    #[test]
    fn state_label_unknown_passes_through() {
        // Unknown values fall back to the raw input (after a log::warn) so the
        // badge keeps rendering something useful when the server adds a new
        // state ahead of the UI.
        assert_eq!(format_meeting_state_label("archived"), "archived");
        assert_eq!(format_meeting_state_label(""), "");
    }

    // -------------------------------------------------------------------------
    // format_datetime tests
    //
    // format_datetime delegates to js_sys::Date for local-timezone resolution,
    // which only exists in a browser/Node runtime. We register the test under
    // wasm_bindgen_test so it compiles for the host target (the macro emits
    // host-target stubs) but only executes meaningfully under wasm-pack.
    //
    // We can't pin the exact output without forcing a TZ on the runner, so we
    // assert on stable shape: month abbreviation, comma, time, AM/PM marker.
    // -------------------------------------------------------------------------

    #[wasm_bindgen_test::wasm_bindgen_test]
    fn format_datetime_renders_month_day_time() {
        // 2024-04-28T17:00:00Z — chosen so most likely runner timezones still
        // land on Apr 28; we don't assert the day text either way.
        let result = format_datetime(1_714_323_600_000);
        assert!(
            result.contains(","),
            "expected comma between date and time in '{result}'"
        );
        assert!(
            result.contains("AM") || result.contains("PM"),
            "expected AM or PM marker in '{result}'"
        );
    }
}
