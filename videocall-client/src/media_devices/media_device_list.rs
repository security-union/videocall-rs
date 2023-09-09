use gloo_utils::window;
use js_sys::Array;
use js_sys::Promise;
use std::cell::OnceCell;
use std::sync::Arc;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaDeviceInfo;
use web_sys::MediaDeviceKind;
use yew::prelude::Callback;

pub struct SelectableDevices {
    devices: Arc<OnceCell<Vec<MediaDeviceInfo>>>,
    selected: Option<String>,
    pub on_selected: Callback<String>,
}

impl SelectableDevices {
    fn new() -> Self {
        Self {
            devices: Arc::new(OnceCell::new()),
            selected: None,
            on_selected: Callback::noop(),
        }
    }

    pub fn select(&mut self, device_id: &str) {
        if let Some(devices) = self.devices.get() {
            for device in devices.iter() {
                if device.device_id() == device_id {
                    self.selected = Some(device_id.to_string());
                    self.on_selected.emit(device_id.to_string());
                }
            }
        }
    }

    pub fn devices(&self) -> &[MediaDeviceInfo] {
        match self.devices.get() {
            Some(devices) => devices,
            None => &[],
        }
    }

    pub fn selected(&self) -> String {
        match &self.selected {
            Some(selected) => selected.to_string(),
            // device 0 is the default selection
            None => match self.devices().get(0) {
                Some(device) => device.device_id(),
                None => "".to_string(),
            },
        }
    }
}

pub struct MediaDeviceList {
    pub audio_inputs: SelectableDevices,
    pub video_inputs: SelectableDevices,
    pub on_loaded: Callback<()>,
}

#[allow(clippy::new_without_default)]
impl MediaDeviceList {
    pub fn new() -> Self {
        Self {
            audio_inputs: SelectableDevices::new(),
            video_inputs: SelectableDevices::new(),
            on_loaded: Callback::noop(),
        }
    }

    pub fn load(&self) {
        let on_loaded = self.on_loaded.clone();
        let on_audio_selected = self.audio_inputs.on_selected.clone();
        let on_video_selected = self.video_inputs.on_selected.clone();
        let audio_input_devices = Arc::clone(&self.audio_inputs.devices);
        let video_input_devices = Arc::clone(&self.video_inputs.devices);
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
            _ = audio_input_devices.set(
                devices
                    .clone()
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Audioinput)
                    .collect(),
            );
            _ = video_input_devices.set(
                devices
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Videoinput)
                    .collect(),
            );
            if let Some(device) = audio_input_devices.get().unwrap().get(0) {
                on_audio_selected.emit(device.device_id())
            }
            if let Some(device) = video_input_devices.get().unwrap().get(0) {
                on_video_selected.emit(device.device_id())
            }
            on_loaded.emit(());
        });
    }
}
