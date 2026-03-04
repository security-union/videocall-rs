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
use videocall_types::Callback;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::MediaStreamConstraints;

#[derive(Clone, Copy)]
pub enum MediaAccessKind {
    AudioCheck,
    VideoCheck,
    BothCheck,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MediaPermissionsErrorState {
    NoDevice,
    PermissionDenied,
    Other(JsValue),
}

#[derive(Clone, PartialEq, Debug)]
pub enum PermissionState {
    Unknown,
    Granted,
    Denied(MediaPermissionsErrorState),
}

#[derive(Debug)]
pub struct MediaPermission {
    pub audio: PermissionState,
    pub video: PermissionState,
}

/// [MediaDeviceAccess] is a utility to request the user's permission to access the microphone and
/// camera.
pub struct MediaDeviceAccess {
    current_permission: MediaPermission,
    pub on_result: Callback<MediaPermission>,
}

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
    /// # use videocall_client::Callback;
    /// let mut media_device_access = MediaDeviceAccess::new();
    /// media_device_access.on_result = Callback::from(|permission| {
    ///     // Handle audio and video state
    /// });
    /// media_device_access.request();
    /// ```
    pub fn new() -> Self {
        Self {
            current_permission: MediaPermission {
                audio: PermissionState::Unknown,
                video: PermissionState::Unknown,
            },
            on_result: Callback::noop(),
        }
    }

    /// Returns true if permission has been granted
    pub fn is_granted(&self, device: MediaAccessKind) -> bool {
        match device {
            MediaAccessKind::AudioCheck => matches!(self.current_permission.audio, PermissionState::Granted),
            MediaAccessKind::VideoCheck => matches!(self.current_permission.video, PermissionState::Granted),
            MediaAccessKind::BothCheck =>  true,
        }

    }

    /// Causes the browser to request the user's permission to access the microphone and camera.
    ///
    /// This function returns immediately.  Eventually, either the [`on_resut`](Self::on_result)
    /// callback will be called.
    pub fn request(&self) {
        let on_result = self.on_result.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let perm_result = Self::request_media_permission().await;
            on_result.emit(perm_result);
        });
    }

    async fn request_media_permission() -> MediaPermission {
        use futures::join;

        let (audio, video) = join!(
            Self::request_audio_permissions(),
            Self::request_video_permissions()
        );

        MediaPermission {
            audio: match audio {
                Ok(_) => PermissionState::Granted,
                Err(e) => PermissionState::Denied(e),
            },
            video: match video {
                Ok(_) => PermissionState::Granted,
                Err(e) => PermissionState::Denied(e),
            },
        }
    }

    async fn request_audio_permissions() -> Result<(), MediaPermissionsErrorState> {
        let navigator = window().navigator();
        let media_devices = navigator.media_devices().map_err(MediaPermissionsErrorState::Other)?;

        let constraints = MediaStreamConstraints::new();

        // Request access to the microphone
        constraints.set_audio(&JsValue::from_bool(true));

        let promise = media_devices
            .get_user_media_with_constraints(&constraints)
            .map_err(MediaPermissionsErrorState::Other)?;

        match JsFuture::from(promise).await {
            Ok(_) => Ok(()),

            Err(err) => {
                let name =js_sys::Reflect::get(&err, &JsValue::from_str("name"))
                    .ok()
                    .and_then(|v| v.as_string());

                match name.as_deref() {
                    Some("NotFoundError") => Err(MediaPermissionsErrorState::NoDevice),
                    Some("NotAllowedError") => Err(MediaPermissionsErrorState::PermissionDenied),
                    _ => Err(MediaPermissionsErrorState::Other(err)),
                }
            }
        }
    }

    async fn request_video_permissions() -> Result<(), MediaPermissionsErrorState> {
        let navigator = window().navigator();
        let media_devices = navigator.media_devices().map_err(MediaPermissionsErrorState::Other)?;

        let constraints = MediaStreamConstraints::new();

        // Request access to the camera
        constraints.set_video(&JsValue::from_bool(true));

        let promise = media_devices
            .get_user_media_with_constraints(&constraints)
            .map_err(MediaPermissionsErrorState::Other)?;

        match JsFuture::from(promise).await {
            Ok(_) => Ok(()),

            Err(err) => {
                let name =js_sys::Reflect::get(&err, &JsValue::from_str("name"))
                    .ok()
                    .and_then(|v| v.as_string());

                match name.as_deref() {
                    Some("NotFoundError") => Err(MediaPermissionsErrorState::NoDevice),
                    Some("NotAllowedError") => Err(MediaPermissionsErrorState::PermissionDenied),
                    _ => Err(MediaPermissionsErrorState::Other(err)),
                }
            }
        }
    }
}
