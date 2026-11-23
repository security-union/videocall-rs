/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */
use crate::types::DeviceInfo;
use dioxus::prelude::*;
use videocall_client::utils::is_ios;
use videocall_theme::{
    Field, IconButton, ListFooter, Scrim, Select, Sheet, SheetBody, SheetHeader, SheetTitle, Tab,
    TabList,
};
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
        Scrim {
            onclick: move |_| on_close.call(()),

            // Sheet brings role="dialog", aria-modal and the stopped click.
            Sheet {
                wide: true,
                id: "device-settings-dialog",
                class: "device-settings-modal surface-elevated",
                "aria-labelledby": "device-settings-title",
                tabindex: "0",

                onkeydown: move |evt: KeyboardEvent| {
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

                SheetHeader {
                    SheetTitle { id: "device-settings-title", "Settings" }
                    IconButton {
                        label: "Close settings",
                        class: "close-button",
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

                SheetBody { class: "settings-layout",
                    // `role` passed on purpose: TabList then leaves out
                    // aria-orientation, which this list never had.
                    TabList {
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

                    div { class: "settings-content",
                        match active_section() {
                            SettingsSection::Audio => rsx! {
                                div {
                                    id: SettingsSection::Audio.panel_id(),
                                    class: "settings-section",
                                    role: "tabpanel",
                                    "aria-labelledby": SettingsSection::Audio.tab_id(),

                                    h3 { class: "text-title-3", "Audio" }
                                    p { class: "text-subheadline text-secondary",
                                        "Choose the microphone and speaker used during the call."
                                    }

                                    Field { label: "Microphone", label_for: "modal-audio-select",
                                        Select {
                                            id: "modal-audio-select",
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
                                        Field { label: "Speaker", label_for: "modal-speaker-select",
                                            Select {
                                                id: "modal-speaker-select",
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
                                        Field {
                                            ListFooter {
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

                                    h3 { class: "text-title-3", "Video" }
                                    p { class: "text-subheadline text-secondary",
                                        "Choose the camera used during the call."
                                    }

                                    Field { label: "Camera", label_for: "modal-video-select",
                                        Select {
                                            id: "modal-video-select",
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
    // `.active` is kept next to aria-selected: tests select it.
    let class = if active {
        "settings-nav-button active"
    } else {
        "settings-nav-button"
    };

    rsx! {
        Tab {
            selected: active,
            id: section.tab_id(),
            class,
            "aria-controls": section.panel_id(),
            "data-testid": section.test_id(),
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
