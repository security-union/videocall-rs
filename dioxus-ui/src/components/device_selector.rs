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
        div { class: "device-selector-wrapper",
            label { r#for: "audio-select", "Audio:" }
            select {
                id: "audio-select",
                class: "device-selector",
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
            br {}
            label { r#for: "video-select", "Video:" }
            select {
                id: "video-select",
                class: "device-selector",
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
            br {}
            if !is_ios_safari {
                label { r#for: "speaker-select", "Speaker:" }
                select {
                    id: "speaker-select",
                    class: "device-selector",
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
        }
    }
}

fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
    devices
        .iter()
        .find(|device| device.device_id() == device_id)
        .map(DeviceInfo::from_media_device_info)
}
