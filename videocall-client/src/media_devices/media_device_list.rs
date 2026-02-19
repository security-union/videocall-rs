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

#[cfg(feature = "yew-compat")]
use yew::prelude::Callback;

#[cfg(not(feature = "yew-compat"))]
use crate::event_bus::emit_client_event;
#[cfg(not(feature = "yew-compat"))]
use crate::events::ClientEvent;

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
#[cfg(feature = "yew-compat")]
pub struct SelectableDevices {
    devices: Rc<RefCell<Vec<MediaDeviceInfo>>>,
    selected: Rc<RefCell<Option<String>>>,

    /// Callback that will be called as `callback(device_id)` whenever [`select(device_id)`](Self::select) is called with a valid `device_id`
    pub on_selected: Callback<String>,
}

/// A "smart" list of [web_sys::MediaDeviceInfo](web_sys::MediaDeviceInfo) items (framework-agnostic version).
///
/// The list keeps track of a currently selected device, supporting selection and a callback that
/// is triggered when a selection is made.
#[cfg(not(feature = "yew-compat"))]
pub struct SelectableDevices {
    devices: Rc<RefCell<Vec<MediaDeviceInfo>>>,
    selected: Option<String>,

    /// Callback that will be called as `callback(device_id)` whenever [`select(device_id)`](Self::select) is called with a valid `device_id`
    pub on_selected: Rc<dyn Fn(String)>,
}

#[cfg(not(feature = "yew-compat"))]
impl SelectableDevices {
    fn new() -> Self {
        Self {
            devices: Rc::new(RefCell::new(Vec::new())),
            selected: None,
            on_selected: Rc::new(|_| {}),
        }
    }

    /// Set the callback for when a device is selected
    pub fn set_on_selected(&mut self, callback: Rc<dyn Fn(String)>) {
        self.on_selected = callback;
    }

    /// Select a device:
    ///
    /// * `device_id` - The `device_id` field of an entry in [`devices()`](Self::devices)
    ///
    /// Triggers the [`on_selected(device_id)`](Self::on_selected) callback.
    ///
    /// Does nothing if the device_id is not in [`devices()`](Self::devices).
    pub fn select(&mut self, device_id: &str) {
        let devices = self.devices.borrow();
        for device in devices.iter() {
            if device.device_id() == device_id {
                self.selected = Some(device_id.to_string());
                (self.on_selected)(device_id.to_string());
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
        match &self.selected {
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

#[cfg(not(feature = "yew-compat"))]
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
#[cfg(feature = "yew-compat")]
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

/// [MediaDeviceList] (framework-agnostic version) - queries the user's system for the currently
/// available audio and video input devices, and audio output devices.
///
/// Events are emitted to the event bus:
/// - `ClientEvent::DevicesLoaded` after loading is complete
/// - `ClientEvent::DevicesChanged` when devices are connected/disconnected
#[cfg(not(feature = "yew-compat"))]
pub struct MediaDeviceList<P: MediaDevicesProvider + Clone = BrowserMediaDevicesProvider> {
    /// The list of audio input devices.
    pub audio_inputs: SelectableDevices,

    /// The list of video input devices.
    pub video_inputs: SelectableDevices,

    /// The list of audio output devices.
    pub audio_outputs: SelectableDevices,

    /// Callback that is called after loading via [`load()`](Self::load) is complete.
    pub on_loaded: Rc<dyn Fn()>,

    /// Callback that is called when the device list changes.
    pub on_devices_changed: Rc<dyn Fn()>,

    /// The provider for media device functionality
    provider: P,

    /// Keeps the event handler alive for the device change event
    device_change_closure: Option<Closure<dyn FnMut(Event)>>,
}

#[cfg(not(feature = "yew-compat"))]
impl<P: MediaDevicesProvider + Clone> MediaDeviceList<P> {
    /// Constructor for the media devices list struct with a specific provider.
    pub fn with_provider(provider: P) -> Self {
        Self {
            audio_inputs: SelectableDevices::new(),
            video_inputs: SelectableDevices::new(),
            audio_outputs: SelectableDevices::new(),
            on_loaded: Rc::new(|| {}),
            on_devices_changed: Rc::new(|| {}),
            provider,
            device_change_closure: None,
        }
    }

    /// Set the callback for when devices are loaded
    pub fn set_on_loaded(&mut self, callback: Rc<dyn Fn()>) {
        self.on_loaded = callback;
    }

    /// Set the callback for when devices change
    pub fn set_on_devices_changed(&mut self, callback: Rc<dyn Fn()>) {
        self.on_devices_changed = callback;
    }

    /// Sets up the device change listener that will automatically refresh devices when changes occur
    fn setup_device_change_listener(&mut self) {
        let provider_clone = self.provider.clone();
        let on_devices_changed = self.on_devices_changed.clone();
        let on_audio_selected = self.audio_inputs.on_selected.clone();
        let on_video_selected = self.video_inputs.on_selected.clone();
        let on_audio_output_selected = self.audio_outputs.on_selected.clone();
        let audio_input_devices = Rc::clone(&self.audio_inputs.devices);
        let video_input_devices = Rc::clone(&self.video_inputs.devices);
        let audio_output_devices = Rc::clone(&self.audio_outputs.devices);

        let closure = Closure::wrap(Box::new(move |_event: Event| {
            let audio_input_devices_clone = Rc::clone(&audio_input_devices);
            let video_input_devices_clone = Rc::clone(&video_input_devices);
            let audio_output_devices_clone = Rc::clone(&audio_output_devices);
            let on_devices_changed_clone = on_devices_changed.clone();
            let on_audio_selected_clone = on_audio_selected.clone();
            let on_video_selected_clone = on_video_selected.clone();
            let on_audio_output_selected_clone = on_audio_output_selected.clone();
            let provider_promise = provider_clone.enumerate_devices();

            let current_audio_selection = {
                let devices = audio_input_devices.borrow();
                if let Some(first) = devices.first() {
                    first.device_id()
                } else {
                    String::new()
                }
            };

            let current_video_selection = {
                let devices = video_input_devices.borrow();
                if let Some(first) = devices.first() {
                    first.device_id()
                } else {
                    String::new()
                }
            };

            let current_audio_output_selection = {
                let devices = audio_output_devices.borrow();
                if let Some(first) = devices.first() {
                    first.device_id()
                } else {
                    String::new()
                }
            };

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

                let old_audio_devices: Vec<MediaDeviceInfo> =
                    audio_input_devices_clone.borrow().clone();
                let old_video_devices: Vec<MediaDeviceInfo> =
                    video_input_devices_clone.borrow().clone();
                let old_audio_output_devices: Vec<MediaDeviceInfo> =
                    audio_output_devices_clone.borrow().clone();

                *audio_input_devices_clone.borrow_mut() = audio_devices.clone();
                *video_input_devices_clone.borrow_mut() = video_devices.clone();
                *audio_output_devices_clone.borrow_mut() = audio_output_device_list.clone();

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
                    // Emit to event bus
                    emit_client_event(ClientEvent::DevicesChanged);
                    // Call callback
                    on_devices_changed_clone();
                }

                if !audio_device_still_exists {
                    if let Some(device) = audio_devices.first() {
                        on_audio_selected_clone(device.device_id());
                    }
                }

                if !video_device_still_exists {
                    if let Some(device) = video_devices.first() {
                        on_video_selected_clone(device.device_id());
                    }
                }

                if !audio_output_device_still_exists {
                    if let Some(device) = audio_output_device_list.first() {
                        on_audio_output_selected_clone(device.device_id());
                    }
                }
            });
        }) as Box<dyn FnMut(Event)>);

        self.device_change_closure = Some(closure);

        if let Some(closure_ref) = &self.device_change_closure {
            self.provider
                .set_device_change_handler(closure_ref.as_ref().unchecked_ref());
        }
    }

    /// Queries the user's system to find the available audio and video input devices.
    ///
    /// Events are emitted to the event bus:
    /// - `ClientEvent::DevicesLoaded` after loading is complete
    /// - `ClientEvent::DevicesChanged` when devices are connected/disconnected
    pub fn load(&mut self) {
        self.setup_device_change_listener();

        let on_loaded = self.on_loaded.clone();
        let on_audio_selected = self.audio_inputs.on_selected.clone();
        let on_video_selected = self.video_inputs.on_selected.clone();
        let on_audio_output_selected = self.audio_outputs.on_selected.clone();
        let audio_input_devices = Rc::clone(&self.audio_inputs.devices);
        let video_input_devices = Rc::clone(&self.video_inputs.devices);
        let audio_output_devices = Rc::clone(&self.audio_outputs.devices);

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

            // Emit to event bus
            emit_client_event(ClientEvent::DevicesLoaded);
            // Call callback
            on_loaded();

            if let Some(device) = audio_input_devices.borrow().first() {
                on_audio_selected(device.device_id())
            }

            if let Some(device) = video_input_devices.borrow().first() {
                on_video_selected(device.device_id())
            }

            if let Some(device) = audio_output_devices.borrow().first() {
                on_audio_output_selected(device.device_id())
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
    /// After constructing, [`load()`](Self::load) needs to be called to populate the lists.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(not(feature = "yew-compat"))]
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

#[cfg(feature = "yew-compat")]
#[path = "media_device_list_yew.rs"]
mod yew_compat;
