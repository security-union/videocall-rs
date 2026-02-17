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
use videocall_client::utils::is_ios;
use web_sys::MediaDeviceInfo;

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
    on_close: EventHandler<MouseEvent>,
) -> Element {
    let is_ios_safari = is_ios();

    if !visible {
        return rsx! {};
    }

    rsx! {
        div {
            class: if visible { "device-settings-modal-overlay visible" } else { "device-settings-modal-overlay" },
            onclick: move |evt| on_close.call(evt),
            div {
                class: "device-settings-modal",
                onclick: move |evt: MouseEvent| evt.stop_propagation(),
                div { class: "device-settings-header",
                    h2 { "Device Settings" }
                    button {
                        class: "close-button",
                        onclick: move |evt| on_close.call(evt),
                        "x"
                    }
                }
                div { class: "device-settings-content",
                    div { class: "device-setting-group",
                        label { r#for: "modal-audio-select", "Microphone:" }
                        select {
                            id: "modal-audio-select",
                            class: "device-selector-modal",
                            onchange: {
                                let microphones = microphones.clone();
                                move |evt: Event<FormData>| {
                                    let device_id = evt.value();
                                    if let Some(device_info) = find_device_by_id(&microphones, &device_id) {
                                        on_microphone_select.call(device_info);
                                    }
                                }
                            },
                            for device in microphones.iter() {
                                option {
                                    value: "{device.device_id()}",
                                    selected: selected_microphone_id.as_deref() == Some(&device.device_id()),
                                    "{device.label()}"
                                }
                            }
                        }
                    }
                    div { class: "device-setting-group",
                        label { r#for: "modal-video-select", "Camera:" }
                        select {
                            id: "modal-video-select",
                            class: "device-selector-modal",
                            onchange: {
                                let cameras = cameras.clone();
                                move |evt: Event<FormData>| {
                                    let device_id = evt.value();
                                    if let Some(device_info) = find_device_by_id(&cameras, &device_id) {
                                        on_camera_select.call(device_info);
                                    }
                                }
                            },
                            for device in cameras.iter() {
                                option {
                                    value: "{device.device_id()}",
                                    selected: selected_camera_id.as_deref() == Some(&device.device_id()),
                                    "{device.label()}"
                                }
                            }
                        }
                    }
                    if !is_ios_safari {
                        div { class: "device-setting-group",
                            label { r#for: "modal-speaker-select", "Speaker:" }
                            select {
                                id: "modal-speaker-select",
                                class: "device-selector-modal",
                                onchange: {
                                    let speakers = speakers.clone();
                                    move |evt: Event<FormData>| {
                                        let device_id = evt.value();
                                        if let Some(device_info) = find_device_by_id(&speakers, &device_id) {
                                            on_speaker_select.call(device_info);
                                        }
                                    }
                                },
                                for device in speakers.iter() {
                                    option {
                                        value: "{device.device_id()}",
                                        selected: selected_speaker_id.as_deref() == Some(&device.device_id()),
                                        "{device.label()}"
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "device-setting-group",
                            p { class: "ios-speaker-note",
                                "Speaker selection is handled by your device settings on iOS/Safari"
                            }
                        }
                    }
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
