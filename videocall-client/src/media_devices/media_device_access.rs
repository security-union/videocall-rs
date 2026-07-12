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
use web_sys::{MediaStream, MediaStreamConstraints, MediaStreamTrack, MediaTrackConstraints};

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
    /// The device exists and the site is permitted, but the OS/browser could
    /// not open it because another application currently holds it
    /// (`getUserMedia` rejects with `NotReadableError`). This is recoverable:
    /// once the other app releases the device, a retry succeeds, so callers
    /// may auto-retry this variant (unlike `PermissionDenied`).
    DeviceInUse,
    Other(JsValue),
}

/// Classify a `getUserMedia` rejection value into a [`MediaPermissionsErrorState`].
///
/// The browser reports the failure kind via the rejected `DOMException`'s
/// `.name` property. This mirrors the spec-defined names:
/// - `NotFoundError` — no matching device is attached.
/// - `NotAllowedError` — the user (or a policy) denied permission.
/// - `NotReadableError` — the device exists and is permitted but cannot be
///   opened, typically because another application is already using it.
///
/// Any other/absent name is preserved as [`MediaPermissionsErrorState::Other`]
/// so the original `JsValue` remains available for diagnostics.
pub(crate) fn classify_get_user_media_error(err: &JsValue) -> MediaPermissionsErrorState {
    let name = js_sys::Reflect::get(err, &JsValue::from_str("name"))
        .ok()
        .and_then(|v| v.as_string());

    // The name→variant decision is the load-bearing, unit-testable part; the
    // `Other` fallthrough needs the original `JsValue`, so it's applied here.
    classify_gum_error_name(name.as_deref())
        .unwrap_or_else(|| MediaPermissionsErrorState::Other(err.clone()))
}

/// Pure (host-testable) core of [`classify_get_user_media_error`]: map a
/// `DOMException` `.name` to the corresponding [`MediaPermissionsErrorState`],
/// or `None` for an unrecognized/absent name (which the caller renders as
/// `Other`). Split out from the `JsValue` plumbing so it can be unit-tested on
/// the native host target without a browser/`js_sys`.
fn classify_gum_error_name(name: Option<&str>) -> Option<MediaPermissionsErrorState> {
    match name {
        Some("NotFoundError") => Some(MediaPermissionsErrorState::NoDevice),
        Some("NotAllowedError") => Some(MediaPermissionsErrorState::PermissionDenied),
        Some("NotReadableError") => Some(MediaPermissionsErrorState::DeviceInUse),
        _ => None,
    }
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
            MediaAccessKind::AudioCheck => {
                matches!(self.current_permission.audio, PermissionState::Granted)
            }
            MediaAccessKind::VideoCheck => {
                matches!(self.current_permission.video, PermissionState::Granted)
            }
            MediaAccessKind::BothCheck => true,
        }
    }

    /// Causes the browser to request the user's permission to access the microphone and camera.
    ///
    /// This function returns immediately.  Eventually, either the [`on_resut`](Self::on_result)
    /// callback will be called.
    pub fn request(&self) {
        let on_result = self.on_result.clone();
        log::info!("start request of permission");

        wasm_bindgen_futures::spawn_local(async move {
            let perm_result = Self::request_media_permission().await;
            on_result.emit(perm_result);
        });
    }

    /// Probe ONLY the microphone and fire `on_result`. The video side of the
    /// emitted [`MediaPermission`] is left as [`PermissionState::Unknown`], a
    /// sentinel the UI's `on_result` handler treats as "not probed — leave the
    /// camera's state untouched." Used by the background auto-retry loop so
    /// re-probing a blocked mic never re-opens (and risks glitching) a healthy
    /// live camera on low-power devices.
    pub fn request_audio_only(&self) {
        let on_result = self.on_result.clone();
        log::info!("start audio-only permission probe");

        wasm_bindgen_futures::spawn_local(async move {
            let audio = Self::request_audio_permissions().await;
            on_result.emit(MediaPermission {
                audio: Self::to_permission_state(audio),
                video: PermissionState::Unknown,
            });
        });
    }

    /// Probe ONLY the camera and fire `on_result`. The audio side is left as
    /// [`PermissionState::Unknown`] (see [`Self::request_audio_only`]).
    pub fn request_video_only(&self) {
        let on_result = self.on_result.clone();
        log::info!("start video-only permission probe");

        wasm_bindgen_futures::spawn_local(async move {
            let video = Self::request_video_permissions().await;
            on_result.emit(MediaPermission {
                audio: PermissionState::Unknown,
                video: Self::to_permission_state(video),
            });
        });
    }

    fn to_permission_state(result: Result<(), MediaPermissionsErrorState>) -> PermissionState {
        match result {
            Ok(_) => PermissionState::Granted,
            Err(e) => PermissionState::Denied(e),
        }
    }

    async fn request_media_permission() -> MediaPermission {
        use futures::join;

        let (audio, video) = join!(
            Self::request_audio_permissions(),
            Self::request_video_permissions()
        );

        MediaPermission {
            audio: Self::to_permission_state(audio),
            video: Self::to_permission_state(video),
        }
    }

    /// Stop all tracks on a MediaStream so the browser releases the hardware
    /// (camera light / microphone indicator turn off).
    fn stop_tracks(stream: &MediaStream) {
        for track in stream.get_tracks().iter() {
            let track: MediaStreamTrack = track.unchecked_into();
            track.stop();
        }
    }

    async fn request_audio_permissions() -> Result<(), MediaPermissionsErrorState> {
        let navigator = window().navigator();
        let media_devices = navigator
            .media_devices()
            .map_err(MediaPermissionsErrorState::Other)?;

        let constraints = MediaStreamConstraints::new();

        // Request access to the microphone with the same audio-processing
        // hints we use on the live mic stream (see `microphone_encoder.rs`).
        // These are non-breaking "ideal" hints: the browser will still grant
        // the probe even if it can't honor a flag, so probe-grant behavior
        // is unchanged. Keeping them in sync with the live constraints
        // avoids subtle permission/grant-state differences across browsers.
        let audio_constraints = MediaTrackConstraints::new();
        audio_constraints.set_echo_cancellation(&JsValue::TRUE);
        audio_constraints.set_noise_suppression(&JsValue::TRUE);
        audio_constraints.set_auto_gain_control(&JsValue::TRUE);
        constraints.set_audio(&audio_constraints.into());

        let promise = media_devices
            .get_user_media_with_constraints(&constraints)
            .map_err(MediaPermissionsErrorState::Other)?;

        match JsFuture::from(promise).await {
            Ok(stream) => {
                Self::stop_tracks(&stream.unchecked_into());
                Ok(())
            }

            Err(err) => Err(classify_get_user_media_error(&err)),
        }
    }

    async fn request_video_permissions() -> Result<(), MediaPermissionsErrorState> {
        let navigator = window().navigator();
        let media_devices = navigator
            .media_devices()
            .map_err(MediaPermissionsErrorState::Other)?;

        let constraints = MediaStreamConstraints::new();

        // Request access to the camera
        constraints.set_video(&JsValue::from_bool(true));

        let promise = media_devices
            .get_user_media_with_constraints(&constraints)
            .map_err(MediaPermissionsErrorState::Other)?;

        match JsFuture::from(promise).await {
            Ok(stream) => {
                Self::stop_tracks(&stream.unchecked_into());
                Ok(())
            }

            Err(err) => Err(classify_get_user_media_error(&err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Native `#[test]`s (run by `cargo test -p videocall-client --lib`, the
    // crate's real execution gate) over the pure name→variant classifier. The
    // `JsValue` plumbing in `classify_get_user_media_error` is browser-only glue;
    // the classification decision it delegates to is what these guard.

    #[test]
    fn classifies_not_found_as_no_device() {
        assert_eq!(
            classify_gum_error_name(Some("NotFoundError")),
            Some(MediaPermissionsErrorState::NoDevice)
        );
    }

    #[test]
    fn classifies_not_allowed_as_permission_denied() {
        assert_eq!(
            classify_gum_error_name(Some("NotAllowedError")),
            Some(MediaPermissionsErrorState::PermissionDenied)
        );
    }

    #[test]
    fn classifies_not_readable_as_device_in_use() {
        // The new case: NotReadableError must map to DeviceInUse (previously it
        // fell into the generic `Other` bucket). Reverting the classifier's
        // `NotReadableError` arm makes this return `None` and fails the assert.
        assert_eq!(
            classify_gum_error_name(Some("NotReadableError")),
            Some(MediaPermissionsErrorState::DeviceInUse)
        );
    }

    #[test]
    fn classifies_unknown_name_as_other_fallthrough() {
        // Unrecognized names return `None`, which the caller renders as `Other`.
        assert_eq!(classify_gum_error_name(Some("SomethingElseError")), None);
        assert_eq!(classify_gum_error_name(None), None);
    }
}
