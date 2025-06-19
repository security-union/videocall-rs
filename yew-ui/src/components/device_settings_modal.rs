use videocall_client::utils::is_ios;
use videocall_client::MediaDeviceList;
use wasm_bindgen::JsCast;
use web_sys::HtmlSelectElement;
use yew::prelude::*;

pub struct DeviceSettingsModal {
    media_devices: MediaDeviceList,
    visible: bool,
}

pub enum Msg {
    DevicesLoaded,
    OnCameraSelect(String),
    OnMicSelect(String),
    OnSpeakerSelect(String),
    LoadDevices(),
    ToggleModal,
    CloseModal,
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSettingsModalProps {
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
    pub visible: bool,
    pub on_close: Callback<()>,
}

impl DeviceSettingsModal {
    fn create_media_device_list(ctx: &Context<DeviceSettingsModal>) -> MediaDeviceList {
        let mut media_devices = MediaDeviceList::new();
        let link = ctx.link().clone();
        let on_microphone_select = ctx.props().on_microphone_select.clone();
        let on_camera_select = ctx.props().on_camera_select.clone();
        let on_speaker_select = ctx.props().on_speaker_select.clone();
        {
            let link = link.clone();
            media_devices.on_loaded =
                Callback::from(move |_| link.send_message(Msg::DevicesLoaded));
        }
        {
            let link = link.clone();
            media_devices.on_devices_changed =
                Callback::from(move |_| link.send_message(Msg::DevicesLoaded));
        }
        let on_microphone_select = on_microphone_select.clone();
        media_devices.audio_inputs.on_selected =
            Callback::from(move |device_id| on_microphone_select.emit(device_id));
        let on_camera_select = on_camera_select.clone();
        media_devices.video_inputs.on_selected =
            Callback::from(move |device_id| on_camera_select.emit(device_id));
        let on_speaker_select = on_speaker_select.clone();
        media_devices.audio_outputs.on_selected =
            Callback::from(move |device_id| on_speaker_select.emit(device_id));
        media_devices
    }
}

impl Component for DeviceSettingsModal {
    type Message = Msg;
    type Properties = DeviceSettingsModalProps;

    fn create(ctx: &Context<Self>) -> Self {
        let link = ctx.link().clone();
        wasm_bindgen_futures::spawn_local(async move {
            link.send_message(Msg::LoadDevices());
        });
        Self {
            media_devices: Self::create_media_device_list(ctx),
            visible: ctx.props().visible,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(Msg::LoadDevices());
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::LoadDevices() => {
                self.media_devices.load();
                false
            }
            Msg::DevicesLoaded => true,
            Msg::OnCameraSelect(camera) => {
                self.media_devices.video_inputs.select(&camera);
                true
            }
            Msg::OnMicSelect(mic) => {
                self.media_devices.audio_inputs.select(&mic);
                true
            }
            Msg::OnSpeakerSelect(speaker) => {
                self.media_devices.audio_outputs.select(&speaker);
                true
            }
            Msg::ToggleModal => {
                self.visible = !self.visible;
                true
            }
            Msg::CloseModal => {
                self.visible = false;
                ctx.props().on_close.emit(());
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mics = self.media_devices.audio_inputs.devices();
        let cameras = self.media_devices.video_inputs.devices();
        let speakers = self.media_devices.audio_outputs.devices();
        let selected_mic = self.media_devices.audio_inputs.selected();
        let selected_camera = self.media_devices.video_inputs.selected();
        let selected_speaker = self.media_devices.audio_outputs.selected();
        let is_ios_safari = is_ios();

        fn selection(event: Event) -> String {
            event
                .target()
                .expect("Event should have a target when dispatched")
                .unchecked_into::<HtmlSelectElement>()
                .value()
        }

        if !self.visible {
            return html! {};
        }

        html! {
            <div class="device-settings-modal-overlay" onclick={ctx.link().callback(|_| Msg::CloseModal)}>
                <div class="device-settings-modal" onclick={|e: MouseEvent| e.stop_propagation()}>
                    <div class="device-settings-header">
                        <h2>{"Device Settings"}</h2>
                        <button class="close-button" onclick={ctx.link().callback(|_| Msg::CloseModal)}>{"×"}</button>
                    </div>
                    <div class="device-settings-content">
                        <div class="device-setting-group">
                            <label for={"modal-audio-select"}>{ "Microphone:" }</label>
                            <select id={"modal-audio-select"} class={"device-selector-modal"}
                                    onchange={ctx.link().callback(|e: Event| Msg::OnMicSelect(selection(e)))}
                            >
                                { for mics.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={selected_mic == device.device_id()}>
                                        { device.label() }
                                    </option>
                                }) }
                            </select>
                        </div>

                        <div class="device-setting-group">
                            <label for={"modal-video-select"}>{ "Camera:" }</label>
                            <select id={"modal-video-select"} class={"device-selector-modal"}
                                    onchange={ctx.link().callback(|e:Event| Msg::OnCameraSelect(selection(e))) }
                            >
                                { for cameras.iter().map(|device| html! {
                                    <option value={device.device_id()} selected={selected_camera == device.device_id()}>
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
                                                onchange={ctx.link().callback(|e: Event| Msg::OnSpeakerSelect(selection(e)))}
                                        >
                                            { for speakers.iter().map(|device| html! {
                                                <option value={device.device_id()} selected={selected_speaker == device.device_id()}>
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
