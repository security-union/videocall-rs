use gloo_utils::window;
use js_sys::Array;
use js_sys::Promise;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::EventTarget;
use web_sys::HtmlSelectElement;
use web_sys::MediaDeviceInfo;
use web_sys::MediaDeviceKind;
use yew::prelude::*;

pub struct DeviceSelector {
    audio_devices: Vec<MediaDeviceInfo>,
    video_devices: Vec<MediaDeviceInfo>,
    video_selected: Option<String>,
    audio_selected: Option<String>,
}

pub enum Msg {
    DevicesLoaded(Vec<MediaDeviceInfo>),
    OnCameraSelect(String),
    OnMicSelect(String),
    LoadDevices(),
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSelectorProps {
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
}

impl Component for DeviceSelector {
    type Message = Msg;
    type Properties = DeviceSelectorProps;

    fn create(ctx: &Context<Self>) -> Self {
        let link = ctx.link().clone();
        wasm_bindgen_futures::spawn_local(async move {
            link.send_message(Msg::LoadDevices());
        });
        Self {
            audio_devices: Vec::new(),
            video_devices: Vec::new(),
            audio_selected: None,
            video_selected: None,
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
                let link = ctx.link().clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let navigator = window().navigator();
                    let media_devices = navigator.media_devices().unwrap();

                    let promise: Promise = media_devices
                        .enumerate_devices()
                        .expect("enumerate devices");
                    let future = JsFuture::from(promise);
                    let devices = future
                        .await
                        .expect("await devices")
                        .unchecked_into::<Array>();
                    let devices = devices.to_vec();
                    let devices = devices
                        .into_iter()
                        .map(|d| d.unchecked_into::<MediaDeviceInfo>())
                        .collect::<Vec<MediaDeviceInfo>>();
                    link.send_message(Msg::DevicesLoaded(devices));
                });
                false
            }
            Msg::DevicesLoaded(devices) => {
                self.audio_devices = devices
                    .clone()
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Audioinput)
                    .collect();
                self.video_devices = devices
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Videoinput)
                    .collect();
                ctx.props()
                    .on_camera_select
                    .emit(self.video_devices[0].device_id());
                ctx.props()
                    .on_microphone_select
                    .emit(self.audio_devices[0].device_id());
                self.video_selected = Some(self.video_devices[0].device_id());
                self.audio_selected = Some(self.audio_devices[0].device_id());
                true
            }
            Msg::OnCameraSelect(camera) => {
                ctx.props().on_camera_select.emit(camera.clone());
                self.video_selected = Some(camera);
                true
            }
            Msg::OnMicSelect(mic) => {
                ctx.props().on_microphone_select.emit(mic.clone());
                self.audio_selected = Some(mic);
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let selected_mic = self.audio_selected.clone().unwrap_or_else(|| "".to_string());
        let selected_camera = self.video_selected.clone().unwrap_or_else(|| "".to_string());
        html! {
            <div class={"device-selector-wrapper"}>
                <label for={"audio-select"}>{ "Audio:" }</label>
                <select id={"audio-select"} class={"device-selector"}
                onchange={ctx.link().callback(|e: Event| {
                    let target: EventTarget = e
                    .target()
                    .expect("Event should have a target when dispatched");
                    let new_audio = target.unchecked_into::<HtmlSelectElement>().value();
                    Msg::OnMicSelect(new_audio)
                })}>
                    { for self.audio_devices.iter().map(|device| html! {
                        <option value={device.device_id()} selected={selected_mic == device.device_id()}>
                            { device.label() }
                        </option>
                    }) }
                </select>
                <br/>
                <label for={"video-select"}>{ "Video:" }</label>
                <select id={"video-select"} class={"device-selector"} onchange={ctx.link().callback(|e:Event| {
                    let target: EventTarget = e
                    .target()
                    .expect("Event should have a target when dispatched");
                    let new_audio = target.unchecked_into::<HtmlSelectElement>().value();
                    Msg::OnCameraSelect(new_audio)
                })}>
                    { for self.video_devices.iter().map(|device| html! {
                        <option value={device.device_id()} selected={selected_camera == device.device_id()}>
                            { device.label() }
                        </option>
                    }) }
                </select>
            </div>
        }
    }
}
