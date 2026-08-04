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
use videocall_types::Callback;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
#[cfg(test)]
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Event, MediaDeviceInfo, MediaDeviceKind};

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

    /// Replace what `enumerate_devices()` returns *without* dispatching a
    /// `devicechange` event.
    ///
    /// This method never touches `device_change_handler`, so the hot-plug
    /// listener installed by [`MediaDeviceList::load`] stays silent.  That lets
    /// a test change the underlying device set and be certain the only thing
    /// able to update the lists afterwards is an explicit re-enumeration call
    /// such as [`MediaDeviceList::refresh_devices_safely`].
    pub fn set_devices_without_event(&self, new_devices: Vec<MediaDeviceInfo>) {
        *self.devices.borrow_mut() = new_devices;
    }

    /// Simulate a device change event with a new set of devices
    pub fn simulate_device_change(&self, new_devices: Vec<MediaDeviceInfo>) {
        // Update the devices
        self.set_devices_without_event(new_devices);

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
/// use videocall_client::Callback;
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

    /// Re-enumerates devices and updates internal lists/selections without emitting callbacks.
    ///
    /// This is useful for UI-only refreshes (for example opening a settings modal)
    /// where we want up-to-date options without triggering encoder/device-switch side effects.
    pub fn refresh_devices_safely(&self) {
        let audio_input_devices = self.audio_inputs.devices.clone();
        let video_input_devices = self.video_inputs.devices.clone();
        let audio_output_devices = self.audio_outputs.devices.clone();

        let audio_input_selected = self.audio_inputs.selected.clone();
        let video_input_selected = self.video_inputs.selected.clone();
        let audio_output_selected = self.audio_outputs.selected.clone();

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

            *audio_input_devices.borrow_mut() = audio_devices.clone();
            *video_input_devices.borrow_mut() = video_devices.clone();
            *audio_output_devices.borrow_mut() = audio_output_device_list.clone();

            let next_audio_selected = audio_input_selected
                .borrow()
                .as_ref()
                .filter(|selected_id| audio_devices.iter().any(|d| d.device_id() == **selected_id))
                .cloned()
                .or_else(|| audio_devices.first().map(|d| d.device_id()));

            let next_video_selected = video_input_selected
                .borrow()
                .as_ref()
                .filter(|selected_id| video_devices.iter().any(|d| d.device_id() == **selected_id))
                .cloned()
                .or_else(|| video_devices.first().map(|d| d.device_id()));

            let next_audio_output_selected = audio_output_selected
                .borrow()
                .as_ref()
                .filter(|selected_id| {
                    audio_output_device_list
                        .iter()
                        .any(|d| d.device_id() == **selected_id)
                })
                .cloned()
                .or_else(|| audio_output_device_list.first().map(|d| d.device_id()));

            *audio_input_selected.borrow_mut() = next_audio_selected;
            *video_input_selected.borrow_mut() = next_video_selected;
            *audio_output_selected.borrow_mut() = next_audio_output_selected;
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
    /// use videocall_client::Callback;
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
    use std::cell::RefCell;
    use std::rc::Rc;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::*;

    // Helper to create mock device for tests.
    //
    // `web_sys::MediaDeviceInfo` accessors use *structural* getters
    // (`Reflect::get`), so plain properties on a `js_sys::Object` are
    // all that's needed — no function overrides required.
    fn create_mock_device(id: &str, kind: MediaDeviceKind, label: &str) -> MediaDeviceInfo {
        let device = js_sys::Object::new();
        js_sys::Reflect::set(&device, &"deviceId".into(), &id.into()).unwrap();
        js_sys::Reflect::set(&device, &"kind".into(), &kind.into()).unwrap();
        js_sys::Reflect::set(&device, &"label".into(), &label.into()).unwrap();
        js_sys::Reflect::set(&device, &"groupId".into(), &"group1".into()).unwrap();
        device.unchecked_into::<MediaDeviceInfo>()
    }

    // Helper to compare device lists by id rather than by JS identity.
    fn device_ids(devices: &[MediaDeviceInfo]) -> Vec<String> {
        devices.iter().map(|d| d.device_id()).collect()
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

    /// Yield to the microtask queue so that `spawn_local` futures complete.
    ///
    /// A single yield is not enough because `spawn_local` starts on one
    /// microtask tick and then its inner `JsFuture::from(promise).await`
    /// needs another tick to deliver the result.  Three iterations gives
    /// a comfortable margin (similar to Jest's `flushPromises()`).
    async fn flush() {
        for _ in 0..3 {
            wasm_bindgen_futures::JsFuture::from(Promise::resolve(&JsValue::NULL))
                .await
                .unwrap();
        }
    }

    // -----------------------------------------------------------------------
    // Load + initial selection
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_load_populates_device_lists_and_selects_first() {
        let audio1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let video1 = create_mock_device("cam-1", MediaDeviceKind::Videoinput, "Camera 1");
        let spk1 = create_mock_device("spk-1", MediaDeviceKind::Audiooutput, "Speaker 1");
        let provider =
            MockMediaDevicesProvider::new(vec![audio1.clone(), video1.clone(), spk1.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider);

        let loaded = Rc::new(RefCell::new(false));
        let loaded_c = loaded.clone();
        mdl.on_loaded = Callback::from(move |_| *loaded_c.borrow_mut() = true);

        let sel_audio = Rc::new(RefCell::new(String::new()));
        let sel_audio_c = sel_audio.clone();
        mdl.audio_inputs.on_selected = Callback::from(move |id| *sel_audio_c.borrow_mut() = id);

        let sel_video = Rc::new(RefCell::new(String::new()));
        let sel_video_c = sel_video.clone();
        mdl.video_inputs.on_selected = Callback::from(move |id| *sel_video_c.borrow_mut() = id);

        let sel_spk = Rc::new(RefCell::new(String::new()));
        let sel_spk_c = sel_spk.clone();
        mdl.audio_outputs.on_selected = Callback::from(move |id| *sel_spk_c.borrow_mut() = id);

        mdl.load();
        flush().await;

        assert!(*loaded.borrow(), "on_loaded should have been called");
        assert_eq!(mdl.audio_inputs.devices().len(), 1);
        assert_eq!(mdl.video_inputs.devices().len(), 1);
        assert_eq!(mdl.audio_outputs.devices().len(), 1);
        assert_eq!(*sel_audio.borrow(), "mic-1");
        assert_eq!(*sel_video.borrow(), "cam-1");
        assert_eq!(*sel_spk.borrow(), "spk-1");
    }

    // -----------------------------------------------------------------------
    // Switch device
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_switch_device_fires_on_selected() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        let provider = MockMediaDevicesProvider::new(vec![mic1.clone(), mic2.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider);

        let sel = Rc::new(RefCell::new(String::new()));
        let sel_c = sel.clone();
        mdl.audio_inputs.on_selected = Callback::from(move |id| *sel_c.borrow_mut() = id);

        mdl.load();
        flush().await;

        // First device auto-selected on load
        assert_eq!(*sel.borrow(), "mic-1");

        // Switch to second device
        mdl.audio_inputs.select("mic-2");
        assert_eq!(*sel.borrow(), "mic-2");
        assert_eq!(mdl.audio_inputs.selected(), "mic-2");
    }

    // -----------------------------------------------------------------------
    // Hot-plug: device added
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_hot_plug_device_added() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let provider = MockMediaDevicesProvider::new(vec![mic1.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider.clone());

        let changed = Rc::new(RefCell::new(false));
        let changed_c = changed.clone();
        mdl.on_devices_changed = Callback::from(move |_| *changed_c.borrow_mut() = true);
        mdl.audio_inputs.on_selected = Callback::noop();
        mdl.video_inputs.on_selected = Callback::noop();
        mdl.audio_outputs.on_selected = Callback::noop();

        mdl.load();
        flush().await;

        assert_eq!(mdl.audio_inputs.devices().len(), 1);

        // Simulate plugging in a second microphone
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        provider.simulate_device_change(vec![mic1.clone(), mic2.clone()]);
        flush().await;

        assert!(*changed.borrow(), "on_devices_changed should fire");
        assert_eq!(mdl.audio_inputs.devices().len(), 2);
    }

    // -----------------------------------------------------------------------
    // Hot-plug: device removed
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_hot_plug_device_removed() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        let provider = MockMediaDevicesProvider::new(vec![mic1.clone(), mic2.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider.clone());

        let changed = Rc::new(RefCell::new(false));
        let changed_c = changed.clone();
        mdl.on_devices_changed = Callback::from(move |_| *changed_c.borrow_mut() = true);
        mdl.audio_inputs.on_selected = Callback::noop();
        mdl.video_inputs.on_selected = Callback::noop();
        mdl.audio_outputs.on_selected = Callback::noop();

        mdl.load();
        flush().await;

        assert_eq!(mdl.audio_inputs.devices().len(), 2);

        // Simulate unplugging mic-2
        provider.simulate_device_change(vec![mic1.clone()]);
        flush().await;

        assert!(*changed.borrow(), "on_devices_changed should fire");
        assert_eq!(mdl.audio_inputs.devices().len(), 1);
    }

    // -----------------------------------------------------------------------
    // Selected device disappears → falls back to first
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_selected_device_disappears_falls_back() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        let provider = MockMediaDevicesProvider::new(vec![mic1.clone(), mic2.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider.clone());

        let sel = Rc::new(RefCell::new(String::new()));
        let sel_c = sel.clone();
        mdl.audio_inputs.on_selected = Callback::from(move |id| *sel_c.borrow_mut() = id);
        mdl.video_inputs.on_selected = Callback::noop();
        mdl.audio_outputs.on_selected = Callback::noop();

        mdl.load();
        flush().await;

        // Select the second mic
        mdl.audio_inputs.select("mic-2");
        assert_eq!(*sel.borrow(), "mic-2");

        // Now mic-2 disappears
        provider.simulate_device_change(vec![mic1.clone()]);
        flush().await;

        // Should fall back to mic-1
        assert_eq!(
            *sel.borrow(),
            "mic-1",
            "selection should fall back to first device when selected device disappears"
        );
    }

    // -----------------------------------------------------------------------
    // Selected device persists when unrelated device added
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_selected_device_persists_through_change() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        let provider = MockMediaDevicesProvider::new(vec![mic1.clone(), mic2.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider.clone());

        let sel = Rc::new(RefCell::new(String::new()));
        let sel_c = sel.clone();
        mdl.audio_inputs.on_selected = Callback::from(move |id| *sel_c.borrow_mut() = id);
        mdl.video_inputs.on_selected = Callback::noop();
        mdl.audio_outputs.on_selected = Callback::noop();

        mdl.load();
        flush().await;

        mdl.audio_inputs.select("mic-2");
        assert_eq!(*sel.borrow(), "mic-2");

        // Plug in a third mic — mic-2 should stay selected
        let mic3 = create_mock_device("mic-3", MediaDeviceKind::Audioinput, "Mic 3");
        provider.simulate_device_change(vec![mic1, mic2, mic3]);
        flush().await;

        assert_eq!(
            mdl.audio_inputs.selected(),
            "mic-2",
            "selected device should persist when an unrelated device is added"
        );
    }

    // -----------------------------------------------------------------------
    // refresh_devices_safely: re-enumerates, but emits NOTHING
    //
    // "NOTHING" is checked against all FIVE callbacks MediaDeviceList owns:
    // `on_loaded`, `on_devices_changed`, and `on_selected` on each of
    // audio_inputs / video_inputs / audio_outputs.
    //
    // This is the contract the in-meeting settings modal relies on:
    // `dioxus-ui/src/components/host.rs` calls `refresh_devices_safely()` on
    // the modal's rising edge so the user sees current devices *without* the
    // encoder/device-switch side effects an emission would trigger.  Those
    // side effects are real and live on BOTH kinds of callback: host.rs wires
    // `on_devices_changed` to `microphone.select()` / `camera.select()` /
    // `update_speaker_device()` plus a `Timeout::new(1000, .. camera.start())`
    // restart, so an errant emission here would restart the encoder every time
    // the settings modal opens.  Contrast `load()` and the hot-plug listener
    // above, both of which deliberately DO emit -- each of the five counters
    // below is sanity-checked against one of them before being reset to zero,
    // so a trailing zero means "did not fire", never "cannot see it fire".
    // -----------------------------------------------------------------------

    #[wasm_bindgen_test]
    async fn test_refresh_devices_safely_updates_lists_without_emitting_callbacks() {
        let mic1 = create_mock_device("mic-1", MediaDeviceKind::Audioinput, "Mic 1");
        let mic2 = create_mock_device("mic-2", MediaDeviceKind::Audioinput, "Mic 2");
        let cam1 = create_mock_device("cam-1", MediaDeviceKind::Videoinput, "Camera 1");
        let cam2 = create_mock_device("cam-2", MediaDeviceKind::Videoinput, "Camera 2");
        let spk1 = create_mock_device("spk-1", MediaDeviceKind::Audiooutput, "Speaker 1");
        let spk2 = create_mock_device("spk-2", MediaDeviceKind::Audiooutput, "Speaker 2");
        let provider =
            MockMediaDevicesProvider::new(vec![mic1.clone(), cam1.clone(), spk1.clone()]);
        let mut mdl = MediaDeviceList::with_provider(provider.clone());

        // Count every emission of all five callbacks.
        let loaded_n = Rc::new(RefCell::new(0usize));
        let changed_n = Rc::new(RefCell::new(0usize));
        let audio_n = Rc::new(RefCell::new(0usize));
        let video_n = Rc::new(RefCell::new(0usize));
        let output_n = Rc::new(RefCell::new(0usize));

        let loaded_c = loaded_n.clone();
        mdl.on_loaded = Callback::from(move |_| *loaded_c.borrow_mut() += 1);
        let changed_c = changed_n.clone();
        mdl.on_devices_changed = Callback::from(move |_| *changed_c.borrow_mut() += 1);
        let audio_c = audio_n.clone();
        mdl.audio_inputs.on_selected = Callback::from(move |_| *audio_c.borrow_mut() += 1);
        let video_c = video_n.clone();
        mdl.video_inputs.on_selected = Callback::from(move |_| *video_c.borrow_mut() += 1);
        let output_c = output_n.clone();
        mdl.audio_outputs.on_selected = Callback::from(move |_| *output_c.borrow_mut() += 1);

        // Reach a realistic starting state through the production load path.
        mdl.load();
        flush().await;

        // Sanity part 1: load() emits four of the five.
        assert_eq!(*loaded_n.borrow(), 1, "load() should emit on_loaded");
        assert_eq!(*audio_n.borrow(), 1, "load() should emit audio on_selected");
        assert_eq!(*video_n.borrow(), 1, "load() should emit video on_selected");
        assert_eq!(
            *output_n.borrow(),
            1,
            "load() should emit audio-output on_selected"
        );

        // Sanity part 2: load() does NOT emit on_devices_changed, so that
        // counter has to be exercised through the hot-plug listener instead --
        // otherwise its trailing zero would prove nothing.  Plugging in mic-2
        // here also sets up the "selected device disappears" case below.
        assert_eq!(
            *changed_n.borrow(),
            0,
            "load() is not expected to emit on_devices_changed"
        );
        provider.simulate_device_change(vec![
            mic1.clone(),
            mic2.clone(),
            cam1.clone(),
            spk1.clone(),
        ]);
        flush().await;
        assert_eq!(
            *changed_n.borrow(),
            1,
            "the hot-plug listener should emit on_devices_changed, proving this \
             counter observes emissions"
        );

        // Pin an explicit audio selection that is NOT the first device, so the
        // refresh has to repair a selection whose device later disappears --
        // exactly the case where the hot-plug listener DOES emit on_selected.
        mdl.audio_inputs.select("mic-2");
        assert_eq!(mdl.audio_inputs.selected(), "mic-2");
        assert_eq!(mdl.audio_outputs.selected(), "spk-1");

        // Everything after this point must leave all five counters at zero.
        *loaded_n.borrow_mut() = 0;
        *changed_n.borrow_mut() = 0;
        *audio_n.borrow_mut() = 0;
        *video_n.borrow_mut() = 0;
        *output_n.borrow_mut() = 0;

        // Swap the hardware set behind the provider *without* a devicechange
        // event.  Every one of the three lists changes, and two of them lose
        // the device that was selected: mic-2 and spk-1 are unplugged, cam-2
        // and spk-2 are plugged in.
        provider.set_devices_without_event(vec![
            mic1.clone(),
            cam1.clone(),
            cam2.clone(),
            spk2.clone(),
        ]);

        // Guard: the silent swap must not have updated anything by itself,
        // otherwise the post-refresh assertions below would be vacuous.
        assert_eq!(
            device_ids(&mdl.audio_inputs.devices()),
            ["mic-1", "mic-2"],
            "a silent device swap must not update audio_inputs on its own"
        );
        assert_eq!(
            device_ids(&mdl.video_inputs.devices()),
            ["cam-1"],
            "a silent device swap must not update video_inputs on its own"
        );
        assert_eq!(
            device_ids(&mdl.audio_outputs.devices()),
            ["spk-1"],
            "a silent device swap must not update audio_outputs on its own"
        );

        mdl.refresh_devices_safely();
        flush().await;

        // Half 1 -- the refresh actually happened: every list re-enumerated ...
        assert_eq!(
            device_ids(&mdl.audio_inputs.devices()),
            ["mic-1"],
            "refresh should drop the unplugged mic-2 from audio_inputs"
        );
        assert_eq!(
            device_ids(&mdl.video_inputs.devices()),
            ["cam-1", "cam-2"],
            "refresh should pick up the newly plugged cam-2"
        );
        assert_eq!(
            device_ids(&mdl.audio_outputs.devices()),
            ["spk-2"],
            "refresh should swap the unplugged spk-1 for the new spk-2"
        );

        // ... and both now-dangling selections were repaired.
        assert_eq!(
            mdl.audio_inputs.selected(),
            "mic-1",
            "refresh should fall back to the first mic when the selected one disappears"
        );
        assert_eq!(
            mdl.audio_outputs.selected(),
            "spk-2",
            "refresh should fall back to the first speaker when the selected one disappears"
        );

        // Half 2 -- and it did all of that silently, on all five callbacks.
        assert_eq!(
            *loaded_n.borrow(),
            0,
            "refresh_devices_safely must not emit on_loaded"
        );
        assert_eq!(
            *changed_n.borrow(),
            0,
            "refresh_devices_safely must not emit on_devices_changed, even though \
             all three device lists changed -- host.rs restarts the encoder on it"
        );
        assert_eq!(
            *audio_n.borrow(),
            0,
            "refresh_devices_safely must not emit audio_inputs.on_selected, \
             even when the selected device disappeared"
        );
        assert_eq!(
            *video_n.borrow(),
            0,
            "refresh_devices_safely must not emit video_inputs.on_selected"
        );
        assert_eq!(
            *output_n.borrow(),
            0,
            "refresh_devices_safely must not emit audio_outputs.on_selected, \
             even when the selected device disappeared"
        );
    }
}
