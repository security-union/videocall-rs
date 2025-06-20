use videocall_client::utils::is_ios;
use videocall_client::MediaDeviceList;
use wasm_bindgen::JsCast;
use web_sys::HtmlSelectElement;
use yew::prelude::*;

pub struct DeviceSettingsModal {
    media_devices: MediaDeviceList,
    visible: bool,
}

pub enum MsgOn {
    CameraSelect(String),
    MicSelect(String),
    SpeakerSelect(String),
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSettingsModalProps {
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
    pub visible: bool,
    pub on_close: Callback<MouseEvent>,
    pub current_microphone_id: Option<String>,
    pub current_camera_id: Option<String>,
    pub current_speaker_id: Option<String>,
}

impl Component for DeviceSettingsModal {
    type Message = MsgOn;
    type Properties = DeviceSettingsModalProps;

    fn create(ctx: &Context<Self>) -> Self {
        let mut media_devices = MediaDeviceList::new();
        let on_microphone_select = ctx.props().on_microphone_select.clone();
        let on_camera_select = ctx.props().on_camera_select.clone();
        let on_speaker_select = ctx.props().on_speaker_select.clone();

        // Set up callbacks for device selection
        media_devices.audio_inputs.on_selected =
            Callback::from(move |device_id| on_microphone_select.emit(device_id));
        media_devices.video_inputs.on_selected =
            Callback::from(move |device_id| on_camera_select.emit(device_id));
        media_devices.audio_outputs.on_selected =
            Callback::from(move |device_id| on_speaker_select.emit(device_id));

        // Load devices
        media_devices.load();

        Self {
            media_devices,
            visible: ctx.props().visible,
        }
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            MsgOn::CameraSelect(_camera) => {
                // Device selection is handled by the callback passed from Host
                true
            }
            MsgOn::MicSelect(_mic) => {
                // Device selection is handled by the callback passed from Host
                true
            }
            MsgOn::SpeakerSelect(_speaker) => {
                // Device selection is handled by the callback passed from Host
                true
            }
        }
    }

    fn changed(&mut self, ctx: &Context<Self>, _old_props: &Self::Properties) -> bool {
        self.visible = ctx.props().visible;
        true
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

        let mics = self.media_devices.audio_inputs.devices();
        let cameras = self.media_devices.video_inputs.devices();
        let speakers = self.media_devices.audio_outputs.devices();
        let selected_mic = ctx.props().current_microphone_id.as_deref();
        let selected_camera = ctx.props().current_camera_id.as_deref();
        let selected_speaker = ctx.props().current_speaker_id.as_deref();

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
                                        MsgOn::MicSelect(device_id)
                                    })}
                            >
                                { for mics.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={selected_mic.is_some_and(|id| id == device.device_id())}>
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
                                        MsgOn::CameraSelect(device_id)
                                    })}
                            >
                                { for cameras.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={selected_camera.is_some_and(|id| id == device.device_id())}>
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
                                                    MsgOn::SpeakerSelect(device_id)
                                                })}
                                        >
                                            { for speakers.iter().map(|device| html! {
                                                <option value={device.device_id()} selected={selected_speaker.is_some_and(|id| id == device.device_id())}>
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
