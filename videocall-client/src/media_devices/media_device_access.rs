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
#[cfg(feature = "yew-compat")]
pub struct MediaDeviceAccess {
    granted: Arc<AtomicBool>,

    // Callback that is called when the user grants access permission
    pub on_granted: Callback<()>,

    // Callback that is called when the user fails to grant access permission
    pub on_denied: Callback<JsValue>,
}

/// [MediaDeviceAccess] is a utility to request the user's permission to access the microphone and
/// camera (framework-agnostic version).
///
/// Events are emitted to the event bus:
/// - `ClientEvent::PermissionGranted` when permission is granted
/// - `ClientEvent::PermissionDenied(error)` when permission is denied
#[cfg(not(feature = "yew-compat"))]
pub struct MediaDeviceAccess {
    granted: Arc<AtomicBool>,

    // Callback that is called when the user grants access permission
    pub on_granted: Rc<dyn Fn()>,

    // Callback that is called when the user fails to grant access permission
    pub on_denied: Rc<dyn Fn(JsValue)>,
}

#[cfg(feature = "yew-compat")]
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

    /// Returns true if permission has been granted
    pub fn is_granted(&self) -> bool {
        self.granted.load(Ordering::Acquire)
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
                    // Call callback
                    on_granted();
                }
                Err(e) => {
                    // Emit to event bus
                    emit_client_event(ClientEvent::PermissionDenied(format!("{e:?}")));
                    // Call callback
                    on_denied(e);
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
