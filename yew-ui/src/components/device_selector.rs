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

use videocall_client::utils::is_ios;
use wasm_bindgen::JsCast;
use web_sys::{HtmlSelectElement, MediaDeviceInfo};
use yew::prelude::*;

pub struct DeviceSelector;

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSelectorProps {
    pub microphones: Vec<MediaDeviceInfo>,
    pub cameras: Vec<MediaDeviceInfo>,
    pub speakers: Vec<MediaDeviceInfo>,
    pub selected_microphone_id: Option<String>,
    pub selected_camera_id: Option<String>,
    pub selected_speaker_id: Option<String>,
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
}

impl Component for DeviceSelector {
    type Message = ();
    type Properties = DeviceSelectorProps;

    fn create(_ctx: &Context<Self>) -> Self {
        Self
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

        html! {
            <div class={"device-selector-wrapper"}>
                <label for={"audio-select"}>{ "Audio:" }</label>
                <select id={"audio-select"} class={"device-selector"}
                        onchange={ctx.link().callback(move |e: Event| {
                            let device_id = selection(e);
                            on_microphone_select.emit(device_id);
                        })}
                >
                    { for ctx.props().microphones.iter().map(|device| html! {
                        <option value={device.device_id()} selected={ctx.props().selected_microphone_id.as_deref() == Some(&device.device_id())}>
                            { device.label() }
                        </option>
                    }) }
                </select>
                <br/>
                <label for={"video-select"}>{ "Video:" }</label>
                <select id={"video-select"} class={"device-selector"}
                        onchange={ctx.link().callback(move |e:Event| {
                            let device_id = selection(e);
                            on_camera_select.emit(device_id);
                        })}
                >
                    { for ctx.props().cameras.iter().map(|device| html! {
                        <option value={device.device_id()} selected={ctx.props().selected_camera_id.as_deref() == Some(&device.device_id())}>
                            { device.label() }
                        </option>
                    }) }
                </select>
                <br/>
                {
                    if !is_ios_safari {
                        html! {
                            <>
                                <label for={"speaker-select"}>{ "Speaker:" }</label>
                                <select id={"speaker-select"} class={"device-selector"}
                                        onchange={ctx.link().callback(move |e: Event| {
                                            let device_id = selection(e);
                                            on_speaker_select.emit(device_id);
                                        })}
                                >
                                    { for ctx.props().speakers.iter().map(|device| html! {
                                        <option value={device.device_id()} selected={ctx.props().selected_speaker_id.as_deref() == Some(&device.device_id())}>
                                            { device.label() }
                                        </option>
                                    }) }
                                </select>
                            </>
                        }
                    } else {
                        html! {}
                    }
                }
            </div>
        }
    }
}
