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

use gloo_utils::window;
use js_sys::{Array, Promise};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
#[cfg(test)]
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Event, MediaDeviceInfo, MediaDeviceKind};
use yew::prelude::Callback;

/// Trait to abstract media device functionality for testing
pub trait MediaDevicesProvider: 'static {
    /// Enumerates the available media devices
    fn enumerate_devices(&self) -> Promise;

    /// Sets a handler for device change events
    fn set_device_change_handler(&self, handler: &js_sys::Function);
}

/// Default implementation using real browser APIs
#[derive(Clone)]
pub struct BrowserMediaDevicesProvider;

impl MediaDevicesProvider for BrowserMediaDevicesProvider {
    fn enumerate_devices(&self) -> Promise {
        window()
            .navigator()
            .media_devices()
            .expect("media devices")
            .enumerate_devices()
            .expect("enumerate devices")
    }

    fn set_device_change_handler(&self, handler: &js_sys::Function) {
        window()
            .navigator()
            .media_devices()
            .expect("media devices")
            .set_ondevicechange(Some(handler));
    }
}

#[cfg(test)]
type DeviceChangeHandler = Rc<RefCell<Option<Closure<dyn FnMut(Event)>>>>;

/// Mock provider for testing purposes
#[cfg(test)]
#[derive(Clone)]
pub struct MockMediaDevicesProvider {
    devices: Rc<RefCell<Vec<MediaDeviceInfo>>>,
    device_change_handler: DeviceChangeHandler,
}

#[cfg(test)]
impl MockMediaDevicesProvider {
    pub fn new(initial_devices: Vec<MediaDeviceInfo>) -> Self {
        Self {
            devices: Rc::new(RefCell::new(initial_devices)),
            device_change_handler: Rc::new(RefCell::new(None)),
        }
    }

    /// Simulate a device change event with a new set of devices
    pub fn simulate_device_change(&self, new_devices: Vec<MediaDeviceInfo>) {
        // Update the devices
        *self.devices.borrow_mut() = new_devices;

        // Trigger the event handler if it exists
        if let Some(handler) = self.device_change_handler.borrow().as_ref() {
            let handler_js = handler.as_ref().unchecked_ref::<js_sys::Function>();
            let _ = handler_js.call0(&JsValue::NULL);
        }
    }
}

#[cfg(test)]
impl MediaDevicesProvider for MockMediaDevicesProvider {
    fn enumerate_devices(&self) -> Promise {
        // Create a Promise that resolves with our mock devices
        let devices = self.devices.borrow().clone();
        let array = Array::new();
        for device in devices {
            array.push(&device);
        }
        Promise::resolve(&array)
    }

    fn set_device_change_handler(&self, handler: &js_sys::Function) {
        // Store the handler for later triggering - we'll just store the function directly
        let handler_cloned = handler.clone();
        *self.device_change_handler.borrow_mut() =
            Some(Closure::wrap(Box::new(move |event: Event| {
                let _ = handler_cloned.call1(&JsValue::NULL, &event);
            }) as Box<dyn FnMut(Event)>));
    }
}

/// A "smart" list of [web_sys::MediaDeviceInfo](web_sys::MediaDeviceInfo) items, used by [MediaDeviceList]
///
/// The list keeps track of a currently selected device, supporting selection and a callback that
/// is triggered when a selection is made.
///
pub struct SelectableDevices {
    devices: Rc<RefCell<Vec<MediaDeviceInfo>>>,
    selected: Rc<RefCell<Option<String>>>,

    /// Callback that will be called as `callback(device_id)` whenever [`select(device_id)`](Self::select) is called with a valid `device_id`
    pub on_selected: Callback<String>,
}

impl SelectableDevices {
    fn new() -> Self {
        Self {
            devices: Rc::new(RefCell::new(Vec::new())),
            selected: Rc::new(RefCell::new(None)),
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
    /// call to [`CameraEncoder::select(device_id)`](crate::CameraEncoder::select) or
    /// [`MicrophoneEncoder::select(device_id)`](crate::MicrophoneEncoder::select) -- the expectation is
    /// that the [`on_selected(device_id)`](Self::on_selected) callback will be set to a function
    /// that calls the `select` method of the appropriate encoder.
    pub fn select(&mut self, device_id: &str) {
        let devices = self.devices.borrow();
        for device in devices.iter() {
            if device.device_id() == device_id {
                *self.selected.borrow_mut() = Some(device_id.to_string());
                self.on_selected.emit(device_id.to_string());
            }
        }
    }

    /// Returns a vector of [MediaDeviceInfo] entries for the available devices.
    pub fn devices(&self) -> Vec<MediaDeviceInfo> {
        self.devices.borrow().clone()
    }

    /// Sets the list of devices
    pub fn set_devices(&self, new_devices: Vec<MediaDeviceInfo>) {
        *self.devices.borrow_mut() = new_devices;
    }

    /// Returns the `device_id` of the currently selected device, or "" if there are no devices.
    pub fn selected(&self) -> String {
        match &*self.selected.borrow() {
            Some(selected) => selected.to_string(),
            // device 0 is the default selection
            None => {
                let devices = self.devices.borrow();
                match devices.first() {
                    Some(device) => device.device_id(),
                    None => "".to_string(),
                }
            }
        }
    }
}

impl Clone for SelectableDevices {
    fn clone(&self) -> Self {
        Self {
            devices: self.devices.clone(),
            selected: self.selected.clone(),
            on_selected: self.on_selected.clone(),
        }
    }
}

///  [MediaDeviceList] is a utility that queries the user's system for the currently
///  available audio and video input devices, and audio output devices, and maintains a current selection for each.
///
///  It does *not* have any explicit connection to [`CameraEncoder`](crate::CameraEncoder) or
///  [`MicrophoneEncoder`](crate::MicrophoneEncoder) -- the calling app is responsible for passing
///  the selection info from this utility to the encoders.
///
///  Outline of usage is:
///
/// ```no_run
/// use videocall_client::MediaDeviceList;
/// use yew::Callback;
///
/// let mut media_device_list = MediaDeviceList::new();
/// media_device_list.audio_inputs.on_selected = Callback::from(|device_id: String| {
///     web_sys::console::log_2(&"Audio input selected:".into(), &device_id.into());
/// });
/// media_device_list.video_inputs.on_selected = Callback::from(|device_id: String| {
///     web_sys::console::log_2(&"Video input selected:".into(), &device_id.into());
/// });
/// media_device_list.audio_outputs.on_selected = Callback::from(|device_id: String| {
///     web_sys::console::log_2(&"Audio output selected:".into(), &device_id.into());
/// });
///
/// media_device_list.load();
///
/// let microphones = media_device_list.audio_inputs.devices();
/// let cameras = media_device_list.video_inputs.devices();
/// let speakers = media_device_list.audio_outputs.devices();
/// if let Some(mic) = microphones.first() {
///     media_device_list.audio_inputs.select(&mic.device_id());
/// }
/// if let Some(camera) = cameras.first() {
///     media_device_list.video_inputs.select(&camera.device_id());
/// }
/// if let Some(speaker) = speakers.first() {
///     media_device_list.audio_outputs.select(&speaker.device_id());
/// }
///
/// ```
pub struct MediaDeviceList<P: MediaDevicesProvider + Clone = BrowserMediaDevicesProvider> {
    /// The list of audio input devices. This field is `pub` for access through it, but should be considerd "read-only".
    pub audio_inputs: SelectableDevices,

    /// The list of video input devices. This field is `pub` for access through it, but should be considerd "read-only".
    pub video_inputs: SelectableDevices,

    /// The list of audio output devices. This field is `pub` for access through it, but should be considerd "read-only".
    pub audio_outputs: SelectableDevices,

    /// Callback that is called as `callback(())` after loading via [`load()`](Self::load) is complete.
    pub on_loaded: Callback<()>,

    /// Callback that is called as `callback(())` when the device list changes (devices connected/disconnected).
    pub on_devices_changed: Callback<()>,

    /// The provider for media device functionality
    provider: P,

    /// Keeps the event handler alive for the device change event
    device_change_closure: Option<Closure<dyn FnMut(Event)>>,
}

impl<P: MediaDevicesProvider + Clone> MediaDeviceList<P> {
    /// Constructor for the media devices list struct with a specific provider.
    ///
    /// This allows for dependency injection for testing.
    pub fn with_provider(provider: P) -> Self {
        Self {
            audio_inputs: SelectableDevices::new(),
            video_inputs: SelectableDevices::new(),
            audio_outputs: SelectableDevices::new(),
            on_loaded: Callback::noop(),
            on_devices_changed: Callback::noop(),
            provider,
            device_change_closure: None,
        }
    }

    /// Sets up the device change listener that will automatically refresh devices when changes occur
    fn setup_device_change_listener(&mut self) {
        // We need a single closure that we'll keep alive in self.device_change_closure
        let provider_clone = self.provider.clone();
        let on_devices_changed = self.on_devices_changed.clone();
        let on_audio_selected = self.audio_inputs.on_selected.clone();
        let on_video_selected = self.video_inputs.on_selected.clone();
        let on_audio_output_selected = self.audio_outputs.on_selected.clone();
        let audio_input_devices = self.audio_inputs.devices.clone();
        let video_input_devices = self.video_inputs.devices.clone();
        let audio_output_devices = self.audio_outputs.devices.clone();
        // Share the actual selection state with the closure so we can
        // read the real selected device and update it if a device disappears.
        let audio_input_selected = self.audio_inputs.selected.clone();
        let video_input_selected = self.video_inputs.selected.clone();
        let audio_output_selected = self.audio_outputs.selected.clone();

        // Create a closure that will call our refresh logic
        let closure = Closure::wrap(Box::new(move |_event: Event| {
            // Clone everything we need to move into the async block
            let audio_input_devices_clone = audio_input_devices.clone();
            let video_input_devices_clone = video_input_devices.clone();
            let audio_output_devices_clone = audio_output_devices.clone();
            let on_devices_changed_clone = on_devices_changed.clone();
            let on_audio_selected_clone = on_audio_selected.clone();
            let on_video_selected_clone = on_video_selected.clone();
            let on_audio_output_selected_clone = on_audio_output_selected.clone();
            let audio_input_selected_for_write = audio_input_selected.clone();
            let video_input_selected_for_write = video_input_selected.clone();
            let audio_output_selected_for_write = audio_output_selected.clone();
            let provider_promise = provider_clone.enumerate_devices();

            // Read the ACTUAL selected device IDs (not just the first device)
            let current_audio_selection = audio_input_selected.borrow().clone().unwrap_or_default();

            let current_video_selection = video_input_selected.borrow().clone().unwrap_or_default();

            let current_audio_output_selection =
                audio_output_selected.borrow().clone().unwrap_or_default();

            wasm_bindgen_futures::spawn_local(async move {
                let future = JsFuture::from(provider_promise);
                let devices = future
                    .await
                    .expect("await devices")
                    .unchecked_into::<Array>();
                let devices = devices.to_vec();
                let devices = devices
                    .into_iter()
                    .map(|d| d.unchecked_into::<MediaDeviceInfo>())
                    .collect::<Vec<MediaDeviceInfo>>();

                let audio_devices = devices
                    .clone()
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Audioinput)
                    .collect::<Vec<MediaDeviceInfo>>();

                let video_devices = devices
                    .clone()
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Videoinput)
                    .collect::<Vec<MediaDeviceInfo>>();

                let audio_output_device_list = devices
                    .into_iter()
                    .filter(|device| device.kind() == MediaDeviceKind::Audiooutput)
                    .collect::<Vec<MediaDeviceInfo>>();

                // Replace the device lists
                let old_audio_devices: Vec<MediaDeviceInfo> =
                    audio_input_devices_clone.borrow().clone();
                let old_video_devices: Vec<MediaDeviceInfo> =
                    video_input_devices_clone.borrow().clone();
                let old_audio_output_devices: Vec<MediaDeviceInfo> =
                    audio_output_devices_clone.borrow().clone();

                // Update the device lists
                *audio_input_devices_clone.borrow_mut() = audio_devices.clone();
                *video_input_devices_clone.borrow_mut() = video_devices.clone();
                *audio_output_devices_clone.borrow_mut() = audio_output_device_list.clone();

                // Check if previously selected devices still exist
                let audio_device_still_exists = !current_audio_selection.is_empty()
                    && audio_devices
                        .iter()
                        .any(|device| device.device_id() == current_audio_selection);

                let video_device_still_exists = !current_video_selection.is_empty()
                    && video_devices
                        .iter()
                        .any(|device| device.device_id() == current_video_selection);

                let audio_output_device_still_exists = !current_audio_output_selection.is_empty()
                    && audio_output_device_list
                        .iter()
                        .any(|device| device.device_id() == current_audio_output_selection);

                // Notify about device changes if the lists actually changed
                let devices_changed = {
                    let old_audio_ids: Vec<String> =
                        old_audio_devices.iter().map(|d| d.device_id()).collect();
                    let new_audio_ids: Vec<String> =
                        audio_devices.iter().map(|d| d.device_id()).collect();

                    let old_video_ids: Vec<String> =
                        old_video_devices.iter().map(|d| d.device_id()).collect();
                    let new_video_ids: Vec<String> =
                        video_devices.iter().map(|d| d.device_id()).collect();

                    let old_audio_output_ids: Vec<String> = old_audio_output_devices
                        .iter()
                        .map(|d| d.device_id())
                        .collect();
                    let new_audio_output_ids: Vec<String> = audio_output_device_list
                        .iter()
                        .map(|d| d.device_id())
                        .collect();

                    old_audio_ids != new_audio_ids
                        || old_video_ids != new_video_ids
                        || old_audio_output_ids != new_audio_output_ids
                };

                if devices_changed {
                    on_devices_changed_clone.emit(());
                }

                // If the selected device disappeared, update the selection to the
                // first available device. We must write directly to the shared Rc
                // because on_selected callbacks are not wired up in the host.
                if !audio_device_still_exists {
                    if let Some(device) = audio_devices.first() {
                        let new_id = device.device_id();
                        *audio_input_selected_for_write.borrow_mut() = Some(new_id.clone());
                        on_audio_selected_clone.emit(new_id);
                    }
                }

                if !video_device_still_exists {
                    if let Some(device) = video_devices.first() {
                        let new_id = device.device_id();
                        *video_input_selected_for_write.borrow_mut() = Some(new_id.clone());
                        on_video_selected_clone.emit(new_id);
                    }
                }

                if !audio_output_device_still_exists {
                    if let Some(device) = audio_output_device_list.first() {
                        let new_id = device.device_id();
                        *audio_output_selected_for_write.borrow_mut() = Some(new_id.clone());
                        on_audio_output_selected_clone.emit(new_id);
                    }
                }
            });
        }) as Box<dyn FnMut(Event)>);

        // Store the closure first so it stays alive
        self.device_change_closure = Some(closure);

        // Then pass a reference to the provider
        if let Some(closure_ref) = &self.device_change_closure {
            self.provider
                .set_device_change_handler(closure_ref.as_ref().unchecked_ref());
        }
    }

    /// Queries the user's system to find the available audio and video input devices.
    ///
    /// This is an asynchronous operation; when it is complete the [`on_loaded`](Self::on_loaded)
    /// callback will be triggered.   Additionally, by default the first audio input device and
    /// first video input device are automatically selected, and their
    /// [`on_selected`](SelectableDevices::on_selected) callbacks will be triggered.
    ///
    /// After loading, the [`audio_inputs`](Self::audio_inputs), [`video_inputs`](Self::video_inputs), and [`audio_outputs`](Self::audio_outputs) lists
    /// will be populated, and can be queried and selected.
    ///
    /// This method also sets up a listener for device change events, which will automatically
    /// refresh the device lists and trigger the [`on_devices_changed`](Self::on_devices_changed)
    /// callback when devices are connected or disconnected.
    pub fn load(&mut self) {
        // Set up the device change listener
        self.setup_device_change_listener();

        // Then do the initial load as before
        let on_loaded = self.on_loaded.clone();
        let on_audio_selected = self.audio_inputs.on_selected.clone();
        let on_video_selected = self.video_inputs.on_selected.clone();
        let on_audio_output_selected = self.audio_outputs.on_selected.clone();
        let audio_input_devices = self.audio_inputs.devices.clone();
        let video_input_devices = self.video_inputs.devices.clone();
        let audio_output_devices = self.audio_outputs.devices.clone();

        let provider_promise = self.provider.enumerate_devices();

        wasm_bindgen_futures::spawn_local(async move {
            let future = JsFuture::from(provider_promise);
            let devices = future
                .await
                .expect("await devices")
                .unchecked_into::<Array>();
            let devices = devices.to_vec();
            let devices = devices
                .into_iter()
                .map(|d| d.unchecked_into::<MediaDeviceInfo>())
                .collect::<Vec<MediaDeviceInfo>>();

            let audio_devices = devices
                .clone()
                .into_iter()
                .filter(|device| device.kind() == MediaDeviceKind::Audioinput)
                .collect::<Vec<MediaDeviceInfo>>();

            let video_devices = devices
                .clone()
                .into_iter()
                .filter(|device| device.kind() == MediaDeviceKind::Videoinput)
                .collect::<Vec<MediaDeviceInfo>>();

            let audio_output_device_list = devices
                .into_iter()
                .filter(|device| device.kind() == MediaDeviceKind::Audiooutput)
                .collect::<Vec<MediaDeviceInfo>>();

            *audio_input_devices.borrow_mut() = audio_devices;
            *video_input_devices.borrow_mut() = video_devices;
            *audio_output_devices.borrow_mut() = audio_output_device_list;

            on_loaded.emit(());

            if let Some(device) = audio_input_devices.borrow().first() {
                on_audio_selected.emit(device.device_id())
            }

            if let Some(device) = video_input_devices.borrow().first() {
                on_video_selected.emit(device.device_id())
            }

            if let Some(device) = audio_output_devices.borrow().first() {
                on_audio_output_selected.emit(device.device_id())
            }
        });
    }
}

// Backward compatibility constructor - this is the main way the app should create MediaDeviceList
impl Default for MediaDeviceList {
    fn default() -> Self {
        Self::with_provider(BrowserMediaDevicesProvider)
    }
}

// For backward compatibility with existing code
#[allow(clippy::new_without_default)]
impl MediaDeviceList {
    /// Constructor for the media devices list struct using the real browser API.
    ///
    /// After constructing, the user should set the [`on_selected`](SelectableDevices::on_selected)
    /// callbacks, e.g.:
    ///
    /// ```no_run
    /// use videocall_client::MediaDeviceList;
    /// use yew::Callback;
    ///
    /// let mut media_device_list = MediaDeviceList::new();
    /// media_device_list.audio_inputs.on_selected = Callback::from(|device_id: String| {
    ///     web_sys::console::log_2(&"Audio input selected:".into(), &device_id.into());
    /// });
    /// media_device_list.video_inputs.on_selected = Callback::from(|device_id: String| {
    ///     web_sys::console::log_2(&"Video input selected:".into(), &device_id.into());
    /// });
    /// media_device_list.audio_outputs.on_selected = Callback::from(|device_id: String| {
    ///     web_sys::console::log_2(&"Audio output selected:".into(), &device_id.into());
    /// });
    /// ```
    ///
    /// After constructing, [`load()`](Self::load) needs to be called to populate the lists.
    pub fn new() -> Self {
        Self::default()
    }
}

// Add Clone implementation for MediaDeviceList to use in the device change callback
impl<P: MediaDevicesProvider + Clone> Clone for MediaDeviceList<P> {
    fn clone(&self) -> Self {
        Self {
            audio_inputs: self.audio_inputs.clone(),
            video_inputs: self.video_inputs.clone(),
            audio_outputs: self.audio_outputs.clone(),
            on_loaded: self.on_loaded.clone(),
            on_devices_changed: self.on_devices_changed.clone(),
            provider: self.provider.clone(),
            device_change_closure: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use js_sys::Function;
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::*;

    // Helper to create mock device for tests
    fn create_mock_device(id: &str, kind: MediaDeviceKind, label: &str) -> MediaDeviceInfo {
        let device = js_sys::Object::new();
        js_sys::Reflect::set(&device, &"deviceId".into(), &id.into()).unwrap();
        js_sys::Reflect::set(&device, &"kind".into(), &kind.into()).unwrap();
        js_sys::Reflect::set(&device, &"label".into(), &label.into()).unwrap();
        js_sys::Reflect::set(&device, &"groupId".into(), &"group1".into()).unwrap();

        // Add the required MediaDeviceInfo methods
        let device_id_fn = Function::new_with_args("", "return this.deviceId;");
        js_sys::Reflect::set(&device, &"deviceId".into(), &device_id_fn).unwrap();

        let kind_fn = Function::new_with_args("", "return this.kind;");
        js_sys::Reflect::set(&device, &"kind".into(), &kind_fn).unwrap();

        device.unchecked_into::<MediaDeviceInfo>()
    }

    // Basic functionality test for MediaDeviceList
    #[wasm_bindgen_test]
    fn test_basic_media_device_list_functionality() {
        // Create a new MediaDeviceList with default browser provider
        let mut media_device_list = MediaDeviceList::new();

        // Verify initial state - empty device lists
        assert_eq!(media_device_list.audio_inputs.devices().len(), 0);
        assert_eq!(media_device_list.video_inputs.devices().len(), 0);
        assert_eq!(media_device_list.audio_outputs.devices().len(), 0);

        // Verify initial selection is empty string
        assert_eq!(media_device_list.audio_inputs.selected(), "");
        assert_eq!(media_device_list.video_inputs.selected(), "");
        assert_eq!(media_device_list.audio_outputs.selected(), "");

        // Track when on_loaded is called
        let loaded_called = Rc::new(RefCell::new(false));
        let loaded_called_clone = loaded_called.clone();

        media_device_list.on_loaded = Callback::from(move |_| {
            *loaded_called_clone.borrow_mut() = true;
        });

        // Track audio device selection
        let selected_audio = Rc::new(RefCell::new(String::new()));
        let selected_audio_clone = selected_audio.clone();

        media_device_list.audio_inputs.on_selected = Callback::from(move |device_id| {
            *selected_audio_clone.borrow_mut() = device_id;
        });

        // Track video device selection
        let selected_video = Rc::new(RefCell::new(String::new()));
        let selected_video_clone = selected_video.clone();

        media_device_list.video_inputs.on_selected = Callback::from(move |device_id| {
            *selected_video_clone.borrow_mut() = device_id;
        });

        // Track audio output device selection
        let selected_audio_output = Rc::new(RefCell::new(String::new()));
        let selected_audio_output_clone = selected_audio_output.clone();

        media_device_list.audio_outputs.on_selected = Callback::from(move |device_id| {
            *selected_audio_output_clone.borrow_mut() = device_id;
        });

        // Manual selection test - with no devices, should do nothing
        media_device_list.audio_inputs.select("non-existent-device");
        assert_eq!(*selected_audio.borrow(), "");
        media_device_list.video_inputs.select("non-existent-device");
        assert_eq!(*selected_video.borrow(), "");
        media_device_list
            .audio_outputs
            .select("non-existent-device");
        assert_eq!(*selected_audio_output.borrow(), "");
    }

    // Test with mock provider
    #[wasm_bindgen_test]
    async fn test_with_mock_provider() {
        // Create mock devices
        let audio1 = create_mock_device("audio1", MediaDeviceKind::Audioinput, "Mic 1");
        let video1 = create_mock_device("video1", MediaDeviceKind::Videoinput, "Camera 1");
        let audio_output1 =
            create_mock_device("audio_output1", MediaDeviceKind::Audiooutput, "Speaker 1");

        // Create a mock provider with initial devices
        let mock_provider = MockMediaDevicesProvider::new(vec![
            audio1.clone(),
            video1.clone(),
            audio_output1.clone(),
        ]);

        // Create MediaDeviceList with mock provider
        let mut media_device_list = MediaDeviceList::with_provider(mock_provider.clone());

        // Track when on_loaded is called
        let loaded_called = Rc::new(RefCell::new(false));
        let loaded_called_clone = loaded_called.clone();

        media_device_list.on_loaded = Callback::from(move |_| {
            *loaded_called_clone.borrow_mut() = true;
        });

        // Track when devices change
        let devices_changed_called = Rc::new(RefCell::new(false));
        let devices_changed_called_clone = devices_changed_called.clone();

        media_device_list.on_devices_changed = Callback::from(move |_| {
            *devices_changed_called_clone.borrow_mut() = true;
        });

        // Track audio device selection
        let selected_audio = Rc::new(RefCell::new(String::new()));
        let selected_audio_clone = selected_audio.clone();

        media_device_list.audio_inputs.on_selected = Callback::from(move |device_id| {
            *selected_audio_clone.borrow_mut() = device_id;
        });

        // Track video device selection
        let selected_video = Rc::new(RefCell::new(String::new()));
        let selected_video_clone = selected_video.clone();

        media_device_list.video_inputs.on_selected = Callback::from(move |device_id| {
            *selected_video_clone.borrow_mut() = device_id;
        });

        // Track audio output device selection
        let selected_audio_output = Rc::new(RefCell::new(String::new()));
        let selected_audio_output_clone = selected_audio_output.clone();

        media_device_list.audio_outputs.on_selected = Callback::from(move |device_id| {
            *selected_audio_output_clone.borrow_mut() = device_id;
        });

        // Initial load
        media_device_list.load();

        // We would wait for the Promise to resolve, but in tests we can just
        // use a simple delay since the mock Promise resolves synchronously
        wasm_bindgen_futures::JsFuture::from(Promise::new(&mut |resolve, _| {
            let _ = resolve.call0(&JsValue::NULL);
        }))
        .await
        .unwrap();

        // Basic test structure - in a real test we would check that:
        // - loaded_called is true
        // - Selected devices match our mock devices
        // - After a device change event, on_devices_changed was called
    }
}
