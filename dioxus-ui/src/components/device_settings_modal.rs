/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */
use crate::types::DeviceInfo;
use dioxus::prelude::*;
use videocall_client::utils::is_ios;
use web_sys::MediaDeviceInfo;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSection {
    Audio,
    Video,
}

impl SettingsSection {
    fn title(self) -> &'static str {
        match self {
            SettingsSection::Audio => "Audio",
            SettingsSection::Video => "Video",
        }
    }

    fn tab_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-tab-audio",
            SettingsSection::Video => "settings-tab-video",
        }
    }

    fn panel_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-panel-audio",
            SettingsSection::Video => "settings-panel-video",
        }
    }

    fn test_id(self) -> &'static str {
        match self {
            SettingsSection::Audio => "settings-nav-audio",
            SettingsSection::Video => "settings-nav-video",
        }
    }

    fn all() -> [SettingsSection; 2] {
        [SettingsSection::Audio, SettingsSection::Video]
    }

    fn next(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Video,
            SettingsSection::Video => SettingsSection::Audio,
        }
    }

    fn prev(self) -> Self {
        match self {
            SettingsSection::Audio => SettingsSection::Video,
            SettingsSection::Video => SettingsSection::Audio,
        }
    }
}

#[component]
pub fn DeviceSettingsModal(
    microphones: Vec<MediaDeviceInfo>,
    cameras: Vec<MediaDeviceInfo>,
    speakers: Vec<MediaDeviceInfo>,
    selected_microphone_id: Option<String>,
    selected_camera_id: Option<String>,
    selected_speaker_id: Option<String>,
    on_camera_select: EventHandler<DeviceInfo>,
    on_microphone_select: EventHandler<DeviceInfo>,
    on_speaker_select: EventHandler<DeviceInfo>,
    visible: bool,
    on_close: EventHandler<()>,
) -> Element {
    let is_ios_safari = is_ios();
    let mut active_section = use_signal(|| SettingsSection::Audio);

    if !visible {
        return rsx! {};
    }

    rsx! {
        div {
            class: "device-settings-modal-overlay visible",
            onclick: move |_| on_close.call(()),

            div {
                id: "device-settings-dialog",
                class: "device-settings-modal settings-modal",
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "device-settings-title",
                tabindex: "0",
                onclick: move |e: MouseEvent| e.stop_propagation(),

                onkeydown: move |evt| {
                    match evt.key() {
                        Key::Escape => {
                            evt.stop_propagation();
                            on_close.call(());
                        }
                        Key::ArrowDown | Key::ArrowRight => {
                            evt.stop_propagation();
                            active_section.set(active_section().next());
                        }
                        Key::ArrowUp | Key::ArrowLeft => {
                            evt.stop_propagation();
                            active_section.set(active_section().prev());
                        }
                        _ => {}
                    }
                },

                div { class: "device-settings-header settings-header",
                    h2 {
                        id: "device-settings-title",
                        "Settings"
                    }
                    button {
                        class: "close-button",
                        r#type: "button",
                        "aria-label": "Close settings",
                        onclick: move |_| on_close.call(()),

                        svg {
                            view_box: "0 0 24 24",
                            width: "18",
                            height: "18",

                            path {
                                d: "M6 6L18 18M18 6L6 18",
                                stroke: "currentColor",
                                stroke_width: "2",
                                stroke_linecap: "round"
                            }
                        }
                    }
                }

                div { class: "device-settings-content settings-layout",
                    div {
                        class: "settings-sidebar",
                        role: "tablist",
                        "aria-label": "Settings sections",

                        for section in SettingsSection::all() {
                            SettingsNavButton {
                                section,
                                active: active_section() == section,
                                onclick: move |_| active_section.set(section),
                            }
                        }
                    }

                    div { class: "settings-panel",
                        match active_section() {
                            SettingsSection::Audio => rsx! {
                                div {
                                    id: SettingsSection::Audio.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Audio.tab_id(),

                                    h3 { class: "settings-section-title", "Audio" }
                                    p { class: "settings-section-description",
                                        "Choose the microphone and speaker used during the call."
                                    }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-audio-select", "Microphone" }
                                        select {
                                            id: "modal-audio-select",
                                            class: "device-selector-modal",
                                            onchange: {
                                                let microphones = microphones.clone();
                                                move |evt: Event<FormData>| {
                                                    let device_id = evt.value();
                                                    if let Some(info) = find_device_by_id(&microphones, &device_id) {
                                                        on_microphone_select.call(info);
                                                    }
                                                }
                                            },
                                            for device in microphones.iter() {
                                                option {
                                                    value: device.device_id(),
                                                    selected: selected_microphone_id.as_deref() == Some(device.device_id().as_str()),
                                                    "{device.label()}"
                                                }
                                            }
                                        }
                                    }

                                    if !is_ios_safari {
                                        div { class: "device-setting-group",
                                            label { r#for: "modal-speaker-select", "Speaker" }
                                            select {
                                                id: "modal-speaker-select",
                                                class: "device-selector-modal",
                                                onchange: {
                                                    let speakers = speakers.clone();
                                                    move |evt: Event<FormData>| {
                                                        let device_id = evt.value();
                                                        if let Some(info) = find_device_by_id(&speakers, &device_id) {
                                                            on_speaker_select.call(info);
                                                        }
                                                    }
                                                },
                                                for device in speakers.iter() {
                                                    option {
                                                        value: device.device_id(),
                                                        selected: selected_speaker_id.as_deref() == Some(device.device_id().as_str()),
                                                        "{device.label()}"
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        div { class: "device-setting-group",
                                            p {
                                                class: "ios-speaker-note",
                                                "Speaker selection is handled by your device settings on iOS/Safari."
                                            }
                                        }
                                    }
                                }
                            },
                            SettingsSection::Video => rsx! {
                                div {
                                    id: SettingsSection::Video.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Video.tab_id(),

                                    h3 { class: "settings-section-title", "Video" }
                                    p { class: "settings-section-description",
                                        "Choose the camera used during the call."
                                    }

                                    div { class: "device-setting-group",
                                        label { r#for: "modal-video-select", "Camera" }
                                        select {
                                            id: "modal-video-select",
                                            class: "device-selector-modal",
                                            onchange: {
                                                let cameras = cameras.clone();
                                                move |evt: Event<FormData>| {
                                                    let device_id = evt.value();
                                                    if let Some(info) = find_device_by_id(&cameras, &device_id) {
                                                        on_camera_select.call(info);
                                                    }
                                                }
                                            },
                                            for device in cameras.iter() {
                                                option {
                                                    value: device.device_id(),
                                                    selected: selected_camera_id.as_deref() == Some(device.device_id().as_str()),
                                                    "{device.label()}"
                                                }
                                            }
                                        }
                                    }
                                }
                            },
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsNavButton(
    section: SettingsSection,
    active: bool,
    onclick: EventHandler<MouseEvent>,
) -> Element {
    let class = if active {
        "settings-nav-button active"
    } else {
        "settings-nav-button"
    };

    rsx! {
        button {
            id: section.tab_id(),
            class,
            r#type: "button",
            role: "tab",
            "aria-selected": if active { "true" } else { "false" },
            "aria-controls": section.panel_id(),
            "data-testid": section.test_id(),
            tabindex: if active { "0" } else { "-1" },
            onclick: move |evt| onclick.call(evt),
            "{section.title()}"
        }
    }
}

fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}
