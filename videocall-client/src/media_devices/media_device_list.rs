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

/// A "smart" list of [web_sys::MediaDeviceInfo](web_sys::MediaDeviceInfo) items, used by [MediaDeviceList]
///
/// The list keeps track of a currently selected device, supporting selection and a callback that
/// is triggered when a selection is made.
///
pub struct SelectableDevices {
    devices: Arc<OnceCell<Vec<MediaDeviceInfo>>>,
    selected: Option<String>,

    /// Callback that will be called as `callback(device_id)` whenever [`select(device_id)`](Self::select) is called with a valid `device_id`
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

    /// Select a device:
    ///
    /// * `device_id` - The `device_id` field of an entry in [`devices()`](Self::devices)
    ///
    /// Triggers the [`on_selected(device_id)`](Self::on_selected) callback.
    ///
    /// Does nothing if the device_id is not in [`devices()`](Self::devices).
    ///
    /// **Note**: Selecting a device here does *not* automatically perform the corresponding
    /// call to [`CameraEncoder::select(device_id)`](CameraEncoder::select) or
    /// [`MicrophoneEncoder::select(device_id)`](MicrophoneEncoder::select) -- the expectation is
    /// that the [`on_selected(device_id)`](Self::on_selected) callback will be set to a function
    /// that calls the `select` method of the appropriate encoder.
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

    /// Returns a reference to an array of [MediaDeviceInfo] entries for the available devices.
    pub fn devices(&self) -> &[MediaDeviceInfo] {
        match self.devices.get() {
            Some(devices) => devices,
            None => &[],
        }
    }

    /// Returns the `device_id` of the currently selected device, or "" if there are no devices.
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

///  [MediaDeviceList] is a utility that queries the user's system for the currently
///  available audio and video input devices, and maintains a current selection for each.
///
///  It does *not* have any explicit connection to [`CameraEncoder`](crate::CameraEncoder) or
///  [`MicrophoneEncoder`](crate::MicrophoneEncoder) -- the calling app is responsible for passing
///  the selection info from this utility to the encoders.
///
///  Outline of usage is:
///
/// ```
/// let media_device_list = MediaDeviceList::new();
/// media_device_list.audio_inputs.on_selected = ...; // callback
/// media_device_access.video_inputs.on_selected = ...; // callback
///
/// media_device_list.load();
///
/// let microphones = media_device_list.audio_inputs.devices();
/// let cameras = media_device_list.video_inputs.devices();
/// media_device_list.audio_inputs.select(&microphones[i].device_id);
/// media_device_list.video_inputs.select(&cameras[i].device_id);
///
/// ```
pub struct MediaDeviceList {
    /// The list of audio input devices.  This field is `pub` for access through it, but should be considerd "read-only".
    pub audio_inputs: SelectableDevices,

    /// The list of video input devices.  This field is `pub` for access through it, but should be considerd "read-only".
    pub video_inputs: SelectableDevices,

    /// Callback that is called as `callback(())` after loading via [`load()`](Self::load) is complete.
    pub on_loaded: Callback<()>,
}

#[allow(clippy::new_without_default)]
impl MediaDeviceList {
    /// Constructor for the media devices list struct.
    ///
    /// After constructing, the user should set the [`on_selected`](SelectableDevices::on_selected)
    /// callbacks, e.g.:
    ///
    /// ```
    /// let media_device_list = MediaDeviceList::new();
    /// media_device_list.audio_inputs.on_selected = ...; // callback
    /// media_device_access.video_inputs.on_selected = ...; // callback
    /// ```
    ///
    /// After constructing, [`load()`](Self::load) needs to be called to populate the lists.
    pub fn new() -> Self {
        Self {
            audio_inputs: SelectableDevices::new(),
            video_inputs: SelectableDevices::new(),
            on_loaded: Callback::noop(),
        }
    }

    /// Queries the user's system to find the available audio and video input devices.
    ///
    /// This is an asynchronous operation; when it is complete the [`on_loaded`](Self::on_loaded)
    /// callback will be triggered.   Additionally, by default the first audio input device and
    /// first video input device are automatically selected, and their
    /// [`on_selected`](SelectableDevices::on_selected) callbacks will be triggered.
    ///
    /// After loading, the [`audio_inputs`](Self::audio_inputs) and [`video_inputs`](Self::video_inputs) lists
    /// will be populated, and can be queried and selected.
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
            on_loaded.emit(());
            if let Some(device) = audio_input_devices.get().unwrap().get(0) {
                on_audio_selected.emit(device.device_id())
            }
            if let Some(device) = video_input_devices.get().unwrap().get(0) {
                on_video_selected.emit(device.device_id())
            }
        });
    }
}
