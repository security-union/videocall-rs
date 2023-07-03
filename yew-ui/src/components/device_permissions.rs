use gloo_utils::window;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStreamConstraints;

pub async fn request_permissions() -> anyhow::Result<(), JsValue> {
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
