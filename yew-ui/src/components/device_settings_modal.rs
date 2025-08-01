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
use videocall_client::utils::is_ios;
use wasm_bindgen::JsCast;
use web_sys::{Event, HtmlSelectElement, MediaDeviceInfo, MouseEvent};
use yew::prelude::*;

pub struct DeviceSettingsModal;

pub enum MsgOnSelect {
    Camera(DeviceInfo),
    Mic(DeviceInfo),
    Speaker(DeviceInfo),
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSettingsModalProps {
    pub microphones: Vec<MediaDeviceInfo>,
    pub cameras: Vec<MediaDeviceInfo>,
    pub speakers: Vec<MediaDeviceInfo>,
    pub selected_microphone_id: Option<String>,
    pub selected_camera_id: Option<String>,
    pub selected_speaker_id: Option<String>,
    pub on_camera_select: Callback<DeviceInfo>,
    pub on_microphone_select: Callback<DeviceInfo>,
    pub on_speaker_select: Callback<DeviceInfo>,
    pub visible: bool,
    pub on_close: Callback<MouseEvent>,
}

impl Component for DeviceSettingsModal {
    type Message = MsgOnSelect;
    type Properties = DeviceSettingsModalProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            MsgOnSelect::Camera(_camera) => {
                // Device selection is handled by the callback passed from Host
                true
            }
            MsgOnSelect::Mic(_mic) => {
                // Device selection is handled by the callback passed from Host
                true
            }
            MsgOnSelect::Speaker(_speaker) => {
                // Device selection is handled by the callback passed from Host
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let is_ios_safari = is_ios();
        let on_microphone_select = ctx.props().on_microphone_select.clone();
        let on_camera_select = ctx.props().on_camera_select.clone();
        let on_speaker_select = ctx.props().on_speaker_select.clone();

        fn selection(event: Event) -> String {
            event
                .target()
                .expect("Event should have a target when dispatched")
                .unchecked_into::<HtmlSelectElement>()
                .value()
        }

        fn find_device_by_id(devices: &[MediaDeviceInfo], device_id: &str) -> Option<DeviceInfo> {
            devices
                .iter()
                .find(|device| device.device_id() == device_id)
                .map(DeviceInfo::from_media_device_info)
        }

        if !ctx.props().visible {
            return html! {};
        }

        html! {
            <div class={classes!("device-settings-modal-overlay", ctx.props().visible.then_some("visible"))} onclick={ctx.props().on_close.clone()}>
                <div class="device-settings-modal" onclick={|e: MouseEvent| e.stop_propagation()}>
                    <div class="device-settings-header">
                        <h2>{"Device Settings"}</h2>
                        <button class="close-button" onclick={ctx.props().on_close.clone()}>{"×"}</button>
                    </div>
                    <div class="device-settings-content">
                        <div class="device-setting-group">
                            <label for={"modal-audio-select"}>{ "Microphone:" }</label>
                            <select id={"modal-audio-select"} class={"device-selector-modal"}
                                    onchange={
                                        let microphones = ctx.props().microphones.clone();
                                        ctx.link().callback(move |e: Event| {
                                            let device_id = selection(e);
                                            if let Some(device_info) = find_device_by_id(&microphones, &device_id) {
                                                on_microphone_select.emit(device_info.clone());
                                                MsgOnSelect::Mic(device_info)
                                            } else {
                                                MsgOnSelect::Mic(DeviceInfo::new(device_id, "Unknown Device".to_string()))
                                            }
                                        })
                                    }
                            >
                                { for ctx.props().microphones.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={ctx.props().selected_microphone_id.as_deref() == Some(&device.device_id())}>
                                        { device.label() }
                                    </option>
                                }) }
                            </select>
                        </div>

                        <div class="device-setting-group">
                            <label for={"modal-video-select"}>{ "Camera:" }</label>
                            <select id={"modal-video-select"} class={"device-selector-modal"}
                                    onchange={
                                        let cameras = ctx.props().cameras.clone();
                                        ctx.link().callback(move |e:Event| {
                                            let device_id = selection(e);
                                            if let Some(device_info) = find_device_by_id(&cameras, &device_id) {
                                                on_camera_select.emit(device_info.clone());
                                                MsgOnSelect::Camera(device_info)
                                            } else {
                                                MsgOnSelect::Camera(DeviceInfo::new(device_id, "Unknown Device".to_string()))
                                            }
                                        })
                                    }
                            >
                                { for ctx.props().cameras.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={ctx.props().selected_camera_id.as_deref() == Some(&device.device_id())}>
                                        { device.label() }
                                    </option>
                                }) }
                            </select>
                        </div>

                        {
                            if !is_ios_safari {
                                html! {
                                    <div class="device-setting-group">
                                        <label for={"modal-speaker-select"}>{ "Speaker:" }</label>
                                        <select id={"modal-speaker-select"} class={"device-selector-modal"}
                                                onchange={
                                                    let speakers = ctx.props().speakers.clone();
                                                    ctx.link().callback(move |e: Event| {
                                                        let device_id = selection(e);
                                                        if let Some(device_info) = find_device_by_id(&speakers, &device_id) {
                                                            on_speaker_select.emit(device_info.clone());
                                                            MsgOnSelect::Speaker(device_info)
                                                        } else {
                                                            MsgOnSelect::Speaker(DeviceInfo::new(device_id, "Unknown Device".to_string()))
                                                        }
                                                    })
                                                }
                                        >
                                            { for ctx.props().speakers.iter().map(|device| html! {
                                                <option value={device.device_id()} selected={ctx.props().selected_speaker_id.as_deref() == Some(&device.device_id())}>
                                                    { device.label() }
                                                </option>
                                            }) }
                                        </select>
                                    </div>
                                }
                            } else {
                                html! {
                                    <div class="device-setting-group">
                                        <p class="ios-speaker-note">{"Speaker selection is handled by your device settings on iOS/Safari"}</p>
                                    </div>
                                }
                            }
                        }
                    </div>
                </div>
            </div>
        }
    }
}
