use gloo_utils::window;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStreamConstraints;
use yew::prelude::Callback;

/// [MediaDeviceAccess] is a utility to request the user's permission to access the microphone and
/// camera.
pub struct MediaDeviceAccess {
    granted: Arc<AtomicBool>,

    // Callback that is called when the user grants access permission
    pub on_granted: Callback<()>,

    // Callback that is called when the user fails to grant access permission
    pub on_denied: Callback<()>,
}

#[allow(clippy::new_without_default)]
impl MediaDeviceAccess {
    /// Constructor for the device access struct.
    ///
    /// After construction, set the callbacks, then call the [`request()`] method to request
    /// access, e.g.:
    ///
    /// ```
    /// let media_device_access = MediaDeviceAccess::new();
    /// media_device_access.on_granted = ...; // callback
    /// media_device_access.on_denied = ...; // callback
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
                    on_granted.emit(());
                }
                Err(_) => on_denied.emit(()),
            }
        });
    }

    async fn request_permissions() -> anyhow::Result<(), JsValue> {
        let navigator = window().navigator();
        let media_devices = navigator.media_devices()?;

        let mut constraints = MediaStreamConstraints::new();

        // Request access to the microphone
        constraints.audio(&JsValue::from_bool(true));

        // Request access to the camera
        constraints.video(&JsValue::from_bool(true));

        let promise = media_devices.get_user_media_with_constraints(&constraints)?;

        JsFuture::from(promise).await?;

        Ok(())
    }
}
