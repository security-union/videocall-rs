use std::rc::Rc;
use videocall_client::MediaDeviceList;
use yew::prelude::*;

pub struct DeviceManager {
    media_devices: Rc<MediaDeviceList>,
    devices_loaded: bool,
}

pub enum Msg {
    DevicesLoaded,
    LoadDevices,
}

#[derive(Properties, Debug, PartialEq)]
pub struct DeviceManagerProps {
    pub children: Children,
    pub on_microphone_select: Callback<String>,
    pub on_camera_select: Callback<String>,
    pub on_speaker_select: Callback<String>,
}

impl Component for DeviceManager {
    type Message = Msg;
    type Properties = DeviceManagerProps;

    fn create(ctx: &Context<Self>) -> Self {
        let mut media_devices = MediaDeviceList::new();
        let link = ctx.link().clone();
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

        // Set up callbacks for device list updates
        media_devices.on_loaded = {
            let link = link.clone();
            Callback::from(move |_| link.send_message(Msg::DevicesLoaded))
        };
        media_devices.on_devices_changed = {
            let link = link.clone();
            Callback::from(move |_| link.send_message(Msg::DevicesLoaded))
        };

        let link = ctx.link().clone();
        wasm_bindgen_futures::spawn_local(async move {
            link.send_message(Msg::LoadDevices);
        });

        Self {
            media_devices: Rc::new(media_devices),
            devices_loaded: false,
        }
    }

    fn rendered(&mut self, ctx: &Context<Self>, first_render: bool) {
        if first_render {
            ctx.link().send_message(Msg::LoadDevices);
        }
    }

    fn update(&mut self, _ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            Msg::LoadDevices => {
                // We need to get a mutable reference to load devices
                if let Some(media_devices) = Rc::get_mut(&mut self.media_devices) {
                    media_devices.load();
                }
                false
            }
            Msg::DevicesLoaded => {
                self.devices_loaded = true;
                true
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        html! {
            <>
                { for ctx.props().children.iter() }
            </>
        }
    }
}

impl DeviceManager {
    pub fn get_media_devices(&self) -> Rc<MediaDeviceList> {
        self.media_devices.clone()
    }

    pub fn is_devices_loaded(&self) -> bool {
        self.devices_loaded
    }
}
