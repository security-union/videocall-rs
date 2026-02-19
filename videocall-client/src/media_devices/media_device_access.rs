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
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStreamConstraints;

#[cfg(feature = "yew-compat")]
use yew::prelude::Callback;

use crate::event_bus::emit_client_event;
use crate::events::ClientEvent;

/// [MediaDeviceAccess] is a utility to request the user's permission to access the microphone and
/// camera.
pub struct MediaDeviceAccess {
    granted: Arc<AtomicBool>,

    #[cfg(feature = "yew-compat")]
    pub on_granted: Callback<()>,
    #[cfg(not(feature = "yew-compat"))]
    pub on_granted: Rc<dyn Fn()>,

    #[cfg(feature = "yew-compat")]
    pub on_denied: Callback<JsValue>,
    #[cfg(not(feature = "yew-compat"))]
    pub on_denied: Rc<dyn Fn(JsValue)>,
}

impl MediaDeviceAccess {
    /// Returns true if permission has been granted
    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::Acquire)
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

/// Shared request logic used by both yew and non-yew modes.
/// The closures abstract the difference between `Callback::emit` and direct function calls.
fn run_request(
    granted: Arc<AtomicBool>,
    on_granted: Rc<dyn Fn()>,
    on_denied: Rc<dyn Fn(JsValue)>,
) {
    let future = MediaDeviceAccess::request_permissions();
    wasm_bindgen_futures::spawn_local(async move {
        match future.await {
            Ok(_) => {
                granted.store(true, Ordering::Release);
                emit_client_event(ClientEvent::PermissionGranted);
                on_granted();
            }
            Err(e) => {
                emit_client_event(ClientEvent::PermissionDenied(format!("{e:?}")));
                on_denied(e);
            }
        }
    });
}

#[cfg(not(feature = "yew-compat"))]
#[allow(clippy::new_without_default)]
impl MediaDeviceAccess {
    /// Constructor for the device access struct (framework-agnostic version).
    ///
    /// After construction, optionally set the callbacks, then call the [`request()`](Self::request)
    /// method to request access. Events are also emitted to the event bus.
    pub fn new() -> Self {
        Self {
            granted: Arc::new(AtomicBool::new(false)),
            on_granted: Rc::new(|| {}),
            on_denied: Rc::new(|_| {}),
        }
    }

    /// Set the callback for when permission is granted
    pub fn set_on_granted(&mut self, callback: Rc<dyn Fn()>) {
        self.on_granted = callback;
    }

    /// Set the callback for when permission is denied
    pub fn set_on_denied(&mut self, callback: Rc<dyn Fn(JsValue)>) {
        self.on_denied = callback;
    }

    /// Causes the browser to request the user's permission to access the microphone and camera.
    ///
    /// This function returns immediately. Events are emitted to the event bus:
    /// - `ClientEvent::PermissionGranted` when permission is granted
    /// - `ClientEvent::PermissionDenied(error)` when permission is denied
    pub fn request(&self) {
        run_request(
            Arc::clone(&self.granted),
            self.on_granted.clone(),
            self.on_denied.clone(),
        );
    }
}

#[cfg(feature = "yew-compat")]
#[path = "media_device_access_yew.rs"]
mod yew_compat;
