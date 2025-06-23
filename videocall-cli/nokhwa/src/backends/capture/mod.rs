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

#[cfg(all(feature = "input-v4l", target_os = "linux"))]
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-v4l")))]
pub use videocall_nokhwa_bindings_linux::V4LCaptureDevice;
#[cfg(any(
    all(feature = "input-msmf", target_os = "windows"),
    all(feature = "docs-only", feature = "docs-nolink", feature = "input-msmf")
))]
mod msmf_backend;
#[cfg(any(
    all(feature = "input-msmf", target_os = "windows"),
    all(feature = "docs-only", feature = "docs-nolink", feature = "input-msmf")
))]
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-msmf")))]
pub use msmf_backend::MediaFoundationCaptureDevice;
#[cfg(any(
    all(
        feature = "input-avfoundation",
        any(target_os = "macos", target_os = "ios")
    ),
    all(
        feature = "docs-only",
        feature = "docs-nolink",
        feature = "input-avfoundation"
    )
))]
mod avfoundation;
#[cfg(any(
    all(
        feature = "input-avfoundation",
        any(target_os = "macos", target_os = "ios")
    ),
    all(
        feature = "docs-only",
        feature = "docs-nolink",
        feature = "input-avfoundation"
    )
))]
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-avfoundation")))]
pub use avfoundation::AVFoundationCaptureDevice;
// FIXME: Fix Lifetime Issues
// #[cfg(feature = "input-uvc")]
// mod uvc_backend;
// #[cfg(feature = "input-uvc")]
// #[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-uvc")))]
// pub use uvc_backend::UVCCaptureDevice;
// #[cfg(feature = "input-gst")]
// mod gst_backend;
// #[cfg(feature = "input-gst")]
// #[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-gst")))]
// pub use gst_backend::GStreamerCaptureDevice;
// #[cfg(feature = "input-jscam")]
// mod browser_backend;
// #[cfg(feature = "input-jscam")]
// #[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-jscam")))]
// pub use browser_backend::BrowserCaptureDevice;
/// A camera that uses `OpenCV` to access IP (rtsp/http) on the local network
// #[cfg(feature = "input-ipcam")]
// #[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-ipcam")))]
// mod network_camera;
// #[cfg(feature = "input-ipcam")]
// #[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-ipcam")))]
// pub use network_camera::NetworkCamera;
#[cfg(feature = "input-opencv")]
mod opencv_backend;
#[cfg(feature = "input-opencv")]
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-opencv")))]
pub use opencv_backend::OpenCvCaptureDevice;
