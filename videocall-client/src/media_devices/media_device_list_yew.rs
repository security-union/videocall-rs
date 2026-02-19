use super::*;
use yew::prelude::Callback;

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
            devices: Rc::clone(&self.devices),
            selected: self.selected.clone(),
            on_selected: self.on_selected.clone(),
        }
    }
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

// Tests require yew-compat feature since they use Callback::from
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
    // Selected device disappears -> falls back to first
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
}
