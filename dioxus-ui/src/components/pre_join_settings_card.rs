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
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use crate::types::DeviceInfo;
use dioxus::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::MediaDeviceInfo;

/// Stable selectors for the pre-join device preview (issue #959). Exposed as
/// constants so the E2E suite and this component cannot drift apart.
pub const PREVIEW_VIDEO_ID: &str = "prejoin-camera-preview";
pub const PREVIEW_CAMERA_TOGGLE_TESTID: &str = "prejoin-camera-toggle";
pub const PREVIEW_MIC_TOGGLE_TESTID: &str = "prejoin-mic-toggle";
pub const PREVIEW_CAMERA_SELECT_ID: &str = "prejoin-camera-select";
pub const PREVIEW_MIC_SELECT_ID: &str = "prejoin-mic-select";
pub const PREVIEW_SPEAKER_SELECT_ID: &str = "prejoin-speaker-select";
pub const PREVIEW_MIC_METER_TESTID: &str = "prejoin-mic-meter";
/// Id of the meter container element (the `role="meter"` div). The preview
/// engine updates its `aria-valuenow`/`aria-valuetext` directly (throttled).
pub const PREVIEW_MIC_METER_ID: &str = "prejoin-mic-meter";
/// Id of the inner fill element. The preview engine writes `style.width` to it
/// every animation frame WITHOUT going through a Dioxus signal, so the meter
/// never re-diffs the surrounding card. (perf review)
pub const PREVIEW_MIC_METER_FILL_ID: &str = "prejoin-mic-meter-fill";
pub const PREVIEW_PERMISSION_PROMPT_TESTID: &str = "prejoin-permission-prompt";

#[allow(clippy::too_many_arguments)]
#[component]
pub fn PreJoinSettingsCard(
    is_owner: bool,
    meeting_id: String,
    waiting_room_toggle: Signal<bool>,
    admitted_can_admit_toggle: Signal<bool>,
    end_on_host_leave_toggle: Signal<bool>,
    allow_guests_toggle: Signal<bool>,
    recording_allowed_for_all_toggle: Signal<bool>,
    saving: Signal<bool>,
    toggle_error: Signal<Option<String>>,
    connection_error: Signal<Option<String>>,
    on_join: EventHandler<()>,
    // ── Device preview (issue #959) ────────────────────────────────────
    /// Whether getUserMedia permission has been granted. Until then device
    /// labels are empty and the live preview cannot run, so we show a prompt.
    #[props(default)]
    media_access_granted: bool,
    /// `true` once setSinkId is supported (Chromium). When false the speaker
    /// dropdown is rendered read-only with an explanatory note.
    #[props(default)]
    speaker_selection_supported: bool,
    #[props(default)] cameras: Vec<MediaDeviceInfo>,
    #[props(default)] microphones: Vec<MediaDeviceInfo>,
    #[props(default)] speakers: Vec<MediaDeviceInfo>,
    #[props(default)] selected_camera_id: Option<String>,
    #[props(default)] selected_microphone_id: Option<String>,
    #[props(default)] selected_speaker_id: Option<String>,
    /// Pre-join camera/mic on-off state (lifted to the parent so the join
    /// handler can read them and the preview engine can react).
    #[props(default)]
    camera_on: Signal<bool>,
    #[props(default)] mic_on: Signal<bool>,
    #[props(default)] on_camera_toggle: EventHandler<bool>,
    #[props(default)] on_mic_toggle: EventHandler<bool>,
    #[props(default)] on_camera_select: EventHandler<DeviceInfo>,
    #[props(default)] on_microphone_select: EventHandler<DeviceInfo>,
    #[props(default)] on_speaker_select: EventHandler<DeviceInfo>,
    /// Fired when the user clicks "Allow camera & mic" before granting.
    #[props(default)]
    on_request_permission: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "settings-card",
            if is_owner {
                h3 { class: "settings-card-title", "Meeting Options" }
            } else {
                div { class: "join-meeting-header",
                    h2 { class: "join-meeting-title",
                        span { class: "join-meeting-title-text", "Join Meeting" }
                        span { class: "join-meeting-id", "{meeting_id}" }
                    }
                    p { class: "join-meeting-subtitle",
                        "Click the button to participate in the meeting."
                    }
                }
            }

            if let Some(err) = connection_error() {
                p { class: "toggle-error", "{err}" }
            }

            DevicePreview {
                media_access_granted,
                speaker_selection_supported,
                cameras: cameras.clone(),
                microphones: microphones.clone(),
                speakers: speakers.clone(),
                selected_camera_id: selected_camera_id.clone(),
                selected_microphone_id: selected_microphone_id.clone(),
                selected_speaker_id: selected_speaker_id.clone(),
                camera_on,
                mic_on,
                on_camera_toggle,
                on_mic_toggle,
                on_camera_select,
                on_microphone_select,
                on_speaker_select,
                on_request_permission,
            }

            if is_owner {
                crate::components::meeting_options_controls::MeetingOptionsControls {
                    meeting_id: meeting_id.clone(),
                    waiting_room_toggle,
                    admitted_can_admit_toggle,
                    end_on_host_leave_toggle,
                    allow_guests_toggle,
                    recording_allowed_for_all_toggle,
                    saving,
                    toggle_error,
                }
            }

            div { class: "settings-action-row",
                button {
                    class: "btn-apple btn-primary settings-action-btn",
                    onclick: move |_| {
                        on_join.call(());
                    },
                    if is_owner { "Start Meeting" } else { "Join Meeting" }
                }
            }
        }
    }
}

fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}

/// Max number of animation frames `sync_select_value` will retry the imperative
/// `select.value` write before giving up. The restore needs the matching
/// `<option>` to be committed to the DOM first (setting `.value` to an id whose
/// `<option>` does not yet exist is a SILENT no-op), and the enumeration render
/// commit can lag the signal write by a frame or two. A handful of frames is
/// plenty (≈ up to ~10 frames ≈ 160ms at 60fps) and costs nothing once the
/// value has taken (we stop as soon as `select.value === id`).
const SELECT_SYNC_MAX_FRAMES: u32 = 10;

/// Decide what to do on one attempt of the select-value sync. Pure +
/// host-testable so the retry/stop logic is covered without a DOM.
///
/// * `current` — the `<select>`'s current `.value`.
/// * `target` — the device id we want selected.
/// * `frames_left` — remaining retry budget BEFORE this attempt.
///
/// Returns `(should_set, should_retry)`:
/// * already correct → don't set, don't retry (done);
/// * not correct, budget remains → set, then retry next frame (the set is a
///   no-op until the `<option>` exists, so we keep trying);
/// * not correct, no budget → set one last time, don't retry.
fn select_sync_step(current: &str, target: &str, frames_left: u32) -> (bool, bool) {
    if current == target {
        (false, false)
    } else {
        (true, frames_left > 0)
    }
}

/// Set a `<select>`'s DOM `.value` (IDL property) to `value` by element id,
/// retrying across animation frames until it takes.
///
/// Why the retry: the IDL `value` setter only moves the control's selection if
/// an `<option>` with that value already exists in the DOM. On restore-after-
/// reload the selection signal is set in the SAME render commit that first
/// populates the `<option>` list, so a synchronous set runs before the options
/// are committed and silently no-ops — leaving the control on the implicit
/// first option ("default"). Deferring to `requestAnimationFrame` and re-trying
/// until `select.value === value` (or the frame budget runs out) sidesteps the
/// render-commit lag entirely. Stops early once correct, so steady state costs
/// a single frame. Never fights a user mid-interaction: if `.value` already
/// matches, it does nothing.
pub fn sync_select_value(select_id: &str, value: Option<&str>) {
    let Some(value) = value.filter(|v| !v.is_empty()) else {
        return;
    };
    schedule_select_sync(
        select_id.to_string(),
        value.to_string(),
        SELECT_SYNC_MAX_FRAMES,
    );
}

/// One attempt + self-rescheduling rAF tail for [`sync_select_value`].
fn schedule_select_sync(select_id: String, value: String, frames_left: u32) {
    let Some(window) = web_sys::window() else {
        return;
    };

    let current = window
        .document()
        .and_then(|d| d.get_element_by_id(&select_id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlSelectElement>().ok())
        .map(|select| {
            let cur = select.value();
            let (should_set, should_retry) = select_sync_step(&cur, &value, frames_left);
            if should_set {
                // No-op until the matching <option> is committed; harmless to
                // call repeatedly. After it takes, `select.value()` reads back
                // the target and the next step stops.
                select.set_value(&value);
            }
            (select.value(), should_retry)
        });

    // If the element is gone (route changed / unmounted), stop.
    let Some((after, should_retry)) = current else {
        return;
    };
    if after == value || !should_retry {
        return;
    }

    // Re-try on the next frame, by which point the enumeration render commit
    // (and thus the <option> elements) should be in the DOM.
    let cb = wasm_bindgen::closure::Closure::once_into_js(move || {
        schedule_select_sync(select_id, value, frames_left.saturating_sub(1));
    });
    let _ = window.request_animation_frame(cb.unchecked_ref());
}

// ── Status icons ──────────────────────────────────────────────────────
// Stroke-only, `currentColor` so they inherit the button/placeholder color.

fn camera_on_icon() -> Element {
    rsx! {
        svg {
            class: "prejoin-toggle-icon",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "18",
            height: "18",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M23 7l-7 5 7 5V7z" }
            rect { x: "1", y: "5", width: "15", height: "14", rx: "2", ry: "2" }
        }
    }
}

fn camera_off_icon() -> Element {
    rsx! {
        svg {
            class: "prejoin-toggle-icon",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "18",
            height: "18",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M16 16v1a2 2 0 0 1-2 2H3a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2h2m5.66 0H14a2 2 0 0 1 2 2v3.34l1 1L23 7v10" }
            line { x1: "1", y1: "1", x2: "23", y2: "23" }
        }
    }
}

fn mic_on_icon() -> Element {
    rsx! {
        svg {
            class: "prejoin-toggle-icon",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "18",
            height: "18",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" }
            path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
            line { x1: "12", y1: "19", x2: "12", y2: "23" }
            line { x1: "8", y1: "23", x2: "16", y2: "23" }
        }
    }
}

fn mic_off_icon() -> Element {
    rsx! {
        svg {
            class: "prejoin-toggle-icon",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "18",
            height: "18",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            line { x1: "1", y1: "1", x2: "23", y2: "23" }
            path { d: "M9 9v3a3 3 0 0 0 5.12 2.12M15 9.34V4a3 3 0 0 0-5.94-.6" }
            path { d: "M17 16.95A7 7 0 0 1 5 12v-2m14 0v2a7 7 0 0 1-.11 1.23" }
            line { x1: "12", y1: "19", x2: "12", y2: "23" }
            line { x1: "8", y1: "23", x2: "16", y2: "23" }
        }
    }
}

fn mic_glyph() -> Element {
    rsx! {
        svg {
            class: "prejoin-meter-glyph",
            xmlns: "http://www.w3.org/2000/svg",
            view_box: "0 0 24 24",
            width: "14",
            height: "14",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            path { d: "M12 1a3 3 0 0 0-3 3v8a3 3 0 0 0 6 0V4a3 3 0 0 0-3-3z" }
            path { d: "M19 10v2a7 7 0 0 1-14 0v-2" }
        }
    }
}

/// The pre-join device preview: live camera `<video>` + on/off toggle, mic
/// on/off + live level meter, audio-output state, and camera/mic/speaker
/// selectors. Renders a permission prompt until getUserMedia is granted.
#[allow(clippy::too_many_arguments)]
#[component]
fn DevicePreview(
    media_access_granted: bool,
    speaker_selection_supported: bool,
    cameras: Vec<MediaDeviceInfo>,
    microphones: Vec<MediaDeviceInfo>,
    speakers: Vec<MediaDeviceInfo>,
    selected_camera_id: Option<String>,
    selected_microphone_id: Option<String>,
    selected_speaker_id: Option<String>,
    camera_on: Signal<bool>,
    mic_on: Signal<bool>,
    on_camera_toggle: EventHandler<bool>,
    on_mic_toggle: EventHandler<bool>,
    on_camera_select: EventHandler<DeviceInfo>,
    on_microphone_select: EventHandler<DeviceInfo>,
    on_speaker_select: EventHandler<DeviceInfo>,
    on_request_permission: EventHandler<()>,
) -> Element {
    let camera_is_on = camera_on();
    let mic_is_on = mic_on();

    rsx! {
        div { class: "prejoin-preview", "data-testid": "prejoin-preview",
            // ── Live camera preview ────────────────────────────────────
            div { class: "prejoin-video-frame",
                // Always mounted so the engine can attach srcObject by id.
                video {
                    id: PREVIEW_VIDEO_ID,
                    "data-testid": PREVIEW_VIDEO_ID,
                    class: "prejoin-video",
                    style: if camera_is_on { "display:block;" } else { "display:none;" },
                    autoplay: true,
                    muted: true,
                    playsinline: "true",
                    controls: false,
                }
                // Placeholder: distinguish "not yet granted" from "you turned
                // it off" so a black frame never reads as "camera broken".
                if !media_access_granted {
                    div { class: "prejoin-video-placeholder",
                        {camera_off_icon()}
                        span { class: "prejoin-video-placeholder-text",
                            "Preview appears here once you allow access"
                        }
                    }
                } else if !camera_is_on {
                    div { class: "prejoin-video-placeholder",
                        {camera_off_icon()}
                        span { class: "prejoin-video-placeholder-text", "Camera is off" }
                    }
                }
            }

            if !media_access_granted {
                // ── Pre-permission state ──────────────────────────────
                // Access is requested automatically on the pre-join screen
                // (issue 1134), so this prompt reflects the in-flight
                // auto-request. The "Allow camera & mic" button below is a
                // manual fallback only for that brief in-flight window (e.g. if
                // the auto-request is slow): once permission resolves either
                // way, `on_result` sets `media_access_granted` unconditionally,
                // which replaces this whole block with the device UI — so the
                // button is never the thing shown after a block/denial.
                div {
                    class: "prejoin-permission-prompt",
                    "data-testid": PREVIEW_PERMISSION_PROMPT_TESTID,
                    role: "note",
                    p { class: "prejoin-permission-text",
                        "Requesting camera & microphone access…"
                    }
                    button {
                        r#type: "button",
                        class: "btn-apple btn-secondary prejoin-permission-allow",
                        "data-testid": "prejoin-permission-allow",
                        onclick: move |_| on_request_permission.call(()),
                        "Allow camera & mic"
                    }
                }
            } else {
                // ── Toggles + meter ───────────────────────────────────
                div { class: "prejoin-controls-row",
                    button {
                        r#type: "button",
                        class: if camera_is_on { "prejoin-toggle on" } else { "prejoin-toggle off danger" },
                        "data-testid": PREVIEW_CAMERA_TOGGLE_TESTID,
                        "aria-pressed": if camera_is_on { "true" } else { "false" },
                        "aria-label": if camera_is_on { "Turn camera off" } else { "Turn camera on" },
                        onclick: move |_| on_camera_toggle.call(!camera_on()),
                        {if camera_is_on { camera_on_icon() } else { camera_off_icon() }}
                        span { class: "prejoin-toggle-text",
                            if camera_is_on { "Camera on" } else { "Camera off" }
                        }
                    }
                    button {
                        r#type: "button",
                        class: if mic_is_on { "prejoin-toggle on" } else { "prejoin-toggle off danger" },
                        "data-testid": PREVIEW_MIC_TOGGLE_TESTID,
                        "aria-pressed": if mic_is_on { "true" } else { "false" },
                        "aria-label": if mic_is_on { "Turn microphone off" } else { "Turn microphone on" },
                        onclick: move |_| on_mic_toggle.call(!mic_on()),
                        {if mic_is_on { mic_on_icon() } else { mic_off_icon() }}
                        span { class: "prejoin-toggle-text",
                            if mic_is_on { "Mic on" } else { "Mic off" }
                        }
                    }
                }

                // Live mic input-level meter. The fill width + ARIA value are
                // written directly to the DOM by the preview engine's rAF loop
                // (no per-frame Dioxus re-render); this markup is the static
                // shell. The container is dimmed via `.muted` when the mic is
                // off so empty-when-off reads differently from silent-when-on.
                div { class: if mic_is_on { "prejoin-meter-row" } else { "prejoin-meter-row muted" },
                    span { class: "prejoin-meter-caption",
                        {mic_glyph()}
                        if mic_is_on { "Speak to test your mic" } else { "Mic off" }
                    }
                    div {
                        id: PREVIEW_MIC_METER_ID,
                        class: "prejoin-meter",
                        "data-testid": PREVIEW_MIC_METER_TESTID,
                        role: "meter",
                        "aria-label": "Microphone input level",
                        "aria-valuemin": "0",
                        "aria-valuemax": "100",
                        // Initial values; the engine updates these live (throttled).
                        "aria-valuenow": "0",
                        "aria-valuetext": if mic_is_on { "No input detected" } else { "Microphone muted" },
                        div {
                            id: PREVIEW_MIC_METER_FILL_ID,
                            class: "prejoin-meter-fill",
                            style: "width: 0%;",
                        }
                    }
                }

                // ── Device selectors ──────────────────────────────────
                div { class: "prejoin-selectors",
                    div { class: "prejoin-select-group",
                        label { r#for: PREVIEW_CAMERA_SELECT_ID, "Camera" }
                        select {
                            id: PREVIEW_CAMERA_SELECT_ID,
                            "data-testid": PREVIEW_CAMERA_SELECT_ID,
                            class: "prejoin-select",
                            onchange: {
                                let cameras = cameras.clone();
                                move |evt: Event<FormData>| {
                                    if let Some(info) = find_device_by_id(&cameras, &evt.value()) {
                                        on_camera_select.call(info);
                                    }
                                }
                            },
                            for device in cameras.iter() {
                                option {
                                    value: device.device_id(),
                                    selected: selected_camera_id.as_deref() == Some(&device.device_id()),
                                    "{device.label()}"
                                }
                            }
                        }
                    }

                    div { class: "prejoin-select-group",
                        label { r#for: PREVIEW_MIC_SELECT_ID, "Microphone" }
                        select {
                            id: PREVIEW_MIC_SELECT_ID,
                            "data-testid": PREVIEW_MIC_SELECT_ID,
                            class: "prejoin-select",
                            onchange: {
                                let microphones = microphones.clone();
                                move |evt: Event<FormData>| {
                                    if let Some(info) = find_device_by_id(&microphones, &evt.value()) {
                                        on_microphone_select.call(info);
                                    }
                                }
                            },
                            for device in microphones.iter() {
                                option {
                                    value: device.device_id(),
                                    selected: selected_microphone_id.as_deref() == Some(&device.device_id()),
                                    "{device.label()}"
                                }
                            }
                        }
                    }

                    div { class: "prejoin-select-group",
                        label { r#for: PREVIEW_SPEAKER_SELECT_ID, "Speaker" }
                        select {
                            id: PREVIEW_SPEAKER_SELECT_ID,
                            "data-testid": PREVIEW_SPEAKER_SELECT_ID,
                            class: "prejoin-select",
                            // Read-only where setSinkId is unsupported (Firefox/Safari).
                            disabled: !speaker_selection_supported,
                            "aria-disabled": if speaker_selection_supported { "false" } else { "true" },
                            onchange: {
                                let speakers = speakers.clone();
                                move |evt: Event<FormData>| {
                                    if let Some(info) = find_device_by_id(&speakers, &evt.value()) {
                                        on_speaker_select.call(info);
                                    }
                                }
                            },
                            if speaker_selection_supported {
                                for device in speakers.iter() {
                                    option {
                                        value: device.device_id(),
                                        selected: selected_speaker_id.as_deref() == Some(&device.device_id()),
                                        "{device.label()}"
                                    }
                                }
                            } else {
                                option { value: "", selected: true, "System default" }
                            }
                        }
                        if !speaker_selection_supported {
                            p {
                                class: "prejoin-select-note",
                                "data-testid": "prejoin-speaker-unsupported-note",
                                "System default — output selection is not supported in this browser."
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::select_sync_step;

    #[test]
    fn step_stops_when_already_correct() {
        // Value already matches → no set, no retry (steady state, one frame).
        assert_eq!(select_sync_step("mic-b", "mic-b", 10), (false, false));
        // Even with no budget, a correct value is just done.
        assert_eq!(select_sync_step("mic-b", "mic-b", 0), (false, false));
    }

    #[test]
    fn step_sets_and_retries_while_budget_remains() {
        // Not correct yet (option not committed → set no-ops), retry next frame.
        assert_eq!(select_sync_step("default", "mic-b", 5), (true, true));
        assert_eq!(select_sync_step("default", "mic-b", 1), (true, true));
    }

    #[test]
    fn step_sets_last_time_then_gives_up() {
        // Out of budget: one final set attempt, then stop retrying.
        assert_eq!(select_sync_step("default", "mic-b", 0), (true, false));
    }
}
