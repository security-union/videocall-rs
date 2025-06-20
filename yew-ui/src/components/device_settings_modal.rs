use videocall_client::utils::is_ios;
use wasm_bindgen::JsCast;
use web_sys::{HtmlSelectElement, MediaDeviceInfo};
use yew::prelude::*;

pub struct DeviceSettingsModal;

pub enum MsgOnSelect {
    Camera(String),
    Mic(String),
    Speaker(String),
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSettingsModalProps {
    pub microphones: Vec<MediaDeviceInfo>,
    pub cameras: Vec<MediaDeviceInfo>,
    pub speakers: Vec<MediaDeviceInfo>,
    pub selected_microphone_id: Option<String>,
    pub selected_camera_id: Option<String>,
    pub selected_speaker_id: Option<String>,
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
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
                                    onchange={ctx.link().callback(move |e: Event| {
                                        let device_id = selection(e);
                                        on_microphone_select.emit(device_id.clone());
                                        MsgOnSelect::Mic(device_id)
                                    })}
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
                                    onchange={ctx.link().callback(move |e:Event| {
                                        let device_id = selection(e);
                                        on_camera_select.emit(device_id.clone());
                                        MsgOnSelect::Camera(device_id)
                                    })}
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
                                                onchange={ctx.link().callback(move |e: Event| {
                                                    let device_id = selection(e);
                                                    on_speaker_select.emit(device_id.clone());
                                                    MsgOnSelect::Speaker(device_id)
                                                })}
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
