/*
 * Copyright 2025 Security Union LLC
 * Licensed under MIT OR Apache-2.0
 */

use crate::types::DeviceInfo;
use dioxus::prelude::*;
use videocall_client::utils::is_ios;
use videocall_theme::{Field, Select};
use web_sys::MediaDeviceInfo;

#[component]
pub fn DeviceSelector(
    microphones: Vec<MediaDeviceInfo>,
    cameras: Vec<MediaDeviceInfo>,
    speakers: Vec<MediaDeviceInfo>,
    selected_microphone_id: Option<String>,
    selected_camera_id: Option<String>,
    selected_speaker_id: Option<String>,
    on_camera_select: EventHandler<DeviceInfo>,
    on_microphone_select: EventHandler<DeviceInfo>,
    on_speaker_select: EventHandler<DeviceInfo>,
) -> Element {
    let is_ios_safari = is_ios();

    rsx! {
        Field { class: "device-selector-wrapper",
            label { class: "field-label", r#for: "audio-select", "Audio:" }
            Select {
                id: "audio-select",
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
                        selected: selected_microphone_id.as_deref() == Some(&device.device_id()),
                        "{device.label()}"
                    }
                }
            }
            label { class: "field-label", r#for: "video-select", "Video:" }
            Select {
                id: "video-select",
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
                        selected: selected_camera_id.as_deref() == Some(&device.device_id()),
                        "{device.label()}"
                    }
                }
            }
            if !is_ios_safari {
                label { class: "field-label", r#for: "speaker-select", "Speaker:" }
                Select {
                    id: "speaker-select",
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
                            selected: selected_speaker_id.as_deref() == Some(&device.device_id()),
                            "{device.label()}"
                        }
                    }
                }
            }
        }
    }
}

#[allow(dead_code)]
fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}
