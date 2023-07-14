use std::sync::Arc;
use std::cell::Cell;
use gloo_utils::window;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStreamConstraints;
use yew::prelude::Callback;

pub struct MediaDeviceAccess {
    granted: Arc<Cell<bool>>,
    pub on_granted: Callback<()>,
    pub on_denied: Callback<()>,
}

impl MediaDeviceAccess {
    pub fn new() -> Self {
        Self {
            granted: Arc::new(Cell::new(false)),
            on_granted: Callback::noop(),
            on_denied: Callback::noop(),
        }
    }

    pub fn is_granted(&self) -> bool {
        self.granted.get()
    }

    pub fn request(&self) {
        let future = Self::request_permissions();
        let on_granted = self.on_granted.clone();
        let on_denied = self.on_granted.clone();
        let granted = Arc::clone(&self.granted);
        wasm_bindgen_futures::spawn_local(async move {
            match future.await {
                Ok(_) => {
                    granted.set(true);
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
