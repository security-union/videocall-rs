use super::*;
use yew::prelude::Callback;

#[allow(clippy::new_without_default)]
impl MediaDeviceAccess {
    /// Constructor for the device access struct.
    ///
    /// After construction, set the callbacks, then call the [`request()`](Self::request) method to request
    /// access, e.g.:
    ///
    /// ```no_run
    /// # use videocall_client::MediaDeviceAccess;
    /// # use wasm_bindgen::JsValue;
    /// # use yew::Callback;
    /// let mut media_device_access = MediaDeviceAccess::new();
    /// media_device_access.on_granted = Callback::from(|_| {
    ///     // Handle granted permission
    /// });
    /// media_device_access.on_denied = Callback::from(|_err: JsValue| {
    ///     // Handle denied permission
    /// });
    /// media_device_access.request();
    /// ```
    pub fn new() -> Self {
        Self {
            granted: Arc::new(AtomicBool::new(false)),
            on_granted: Callback::noop(),
            on_denied: Callback::noop(),
        }
    }

    /// Returns true if permission has been granted
    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::Acquire)
    }

    /// Causes the browser to request the user's permission to access the microphone and camera.
    ///
    /// This function returns immediately.  Eventually, either the [`on_granted`](Self::on_granted)
    /// or [`on_denied`](Self::on_denied) callback will be called.
    pub fn request(&self) {
        let future = Self::request_permissions();
        let on_granted = self.on_granted.clone();
        let on_denied = self.on_denied.clone();
        let granted = Arc::clone(&self.granted);
        wasm_bindgen_futures::spawn_local(async move {
            match future.await {
                Ok(_) => {
                    granted.store(true, Ordering::Release);
                    // Emit to event bus
                    emit_client_event(ClientEvent::PermissionGranted);
                    // Call Yew callback
                    on_granted.emit(());
                }
                Err(e) => {
                    // Emit to event bus
                    emit_client_event(ClientEvent::PermissionDenied(format!("{e:?}")));
                    // Call Yew callback
                    on_denied.emit(e);
                }
            }
        });
    }

    async fn request_permissions() -> anyhow::Result<(), JsValue> {
        let navigator = window().navigator();
        let media_devices = navigator.media_devices()?;

        let constraints = MediaStreamConstraints::new();

        // Request access to the microphone
        constraints.set_audio(&JsValue::from_bool(true));

        // Request access to the camera
        constraints.set_video(&JsValue::from_bool(true));

        let promise = media_devices.get_user_media_with_constraints(&constraints)?;

        JsFuture::from(promise).await?;

        Ok(())
    }
}
