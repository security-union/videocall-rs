use videocall_client::utils::is_ios;
use videocall_client::MediaDeviceList;
use wasm_bindgen::JsCast;
use web_sys::HtmlSelectElement;
use yew::prelude::*;

pub struct DeviceSelector {
    media_devices: MediaDeviceList,
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceSelectorProps {
    pub on_camera_select: Callback<String>,
    pub on_microphone_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
}

impl Component for DeviceSelector {
    type Message = ();
    type Properties = DeviceSelectorProps;

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

        Self { media_devices }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let mics = self.media_devices.audio_inputs.devices();
        let cameras = self.media_devices.video_inputs.devices();
        let speakers = self.media_devices.audio_outputs.devices();
        let selected_mic = self.media_devices.audio_inputs.selected();
        let selected_camera = self.media_devices.video_inputs.selected();
        let selected_speaker = self.media_devices.audio_outputs.selected();
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
                    { for mics.iter().map(|device| html! {
                        <option value={device.device_id()} selected={selected_mic == device.device_id()}>
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
                    { for cameras.iter().map(|device| html! {
                        <option value={device.device_id()} selected={selected_camera == device.device_id()}>
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
                                    { for speakers.iter().map(|device| html! {
                                        <option value={device.device_id()} selected={selected_speaker == device.device_id()}>
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
