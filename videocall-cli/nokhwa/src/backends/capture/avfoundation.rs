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

#[cfg(target_os = "macos")]
use videocall_nokhwa_bindings_macos::{CaptureDevice, CaptureStream};
use videocall_nokhwa_core::{
    buffer::Buffer,
    error::NokhwaError,
    pixel_format::RgbFormat,
    traits::CaptureBackendTrait,
    types::{
        ApiBackend, CameraControl, CameraFormat, CameraIndex, CameraInfo, ControlValueSetter,
        FrameFormat, KnownCameraControl, RequestedFormat, RequestedFormatType, Resolution,
    },
};

use std::{borrow::Cow, collections::HashMap};

/// The backend struct that interfaces with V4L2.
/// To see what this does, please see [`CaptureBackendTrait`].
/// # Quirks
/// - While working with `iOS` is allowed, it is not officially supported and may not work.
/// - You **must** call [`nokhwa_initialize`](crate::nokhwa_initialize) **before** doing anything with `AVFoundation`.
/// - This only works on 64 bit platforms.
/// - FPS adjustment does not work.
/// - If permission has not been granted and you call `init()` it will error.
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-avfoundation")))]
#[cfg(target_os = "macos")]
pub struct AVFoundationCaptureDevice {
    device: CaptureDevice,
    info: CameraInfo,
    format: CameraFormat,
    stream: Option<CaptureStream>,
}

#[cfg(target_os = "macos")]
impl AVFoundationCaptureDevice {
    /// Creates a new capture device using the `AVFoundation` backend. Indexes are given to devices by the OS, and usually numbered by order of discovery.
    ///
    /// # Errors
    /// This function will error if the device cannot be found, `AVFoundation` can't read device information, or the requested format cannot be fulfilled.
    pub fn new(index: &CameraIndex, req_fmt: RequestedFormat) -> Result<Self, NokhwaError> {
        let device = CaptureDevice::new(index)?;
        let formats = device.supported_formats()?;
        let camera_fmt = req_fmt.fulfill(&formats).ok_or_else(|| {
            NokhwaError::OpenDeviceError("Cannot fulfill request".to_string(), req_fmt.to_string())
        })?;
        let info = device.info().clone();
        Ok(AVFoundationCaptureDevice {
            device,
            info,
            format: camera_fmt,
            stream: None,
        })
    }

    /// Creates a new capture device using the `AVFoundation` backend with desired settings.
    ///
    /// # Errors
    /// This function will error if the camera is currently busy or if `AVFoundation` can't read device information, or permission was not given by the user.
    #[deprecated(since = "0.10.0", note = "please use `new` instead.")]
    #[allow(clippy::cast_possible_truncation)]
    pub fn new_with(
        index: usize,
        width: u32,
        height: u32,
        fps: u32,
        fourcc: FrameFormat,
    ) -> Result<Self, NokhwaError> {
        let camera_format = CameraFormat::new_from(width, height, fourcc, fps);
        AVFoundationCaptureDevice::new(
            &CameraIndex::Index(index as u32),
            RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(camera_format)),
        )
    }
}

#[cfg(target_os = "macos")]
impl CaptureBackendTrait for AVFoundationCaptureDevice {
    fn backend(&self) -> ApiBackend {
        ApiBackend::AVFoundation
    }

    fn camera_info(&self) -> &CameraInfo {
        &self.info
    }

    fn refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
        // Once a stream is open, the Swift side is the source of truth for the
        // negotiated geometry; before that, keep the requested format.
        if let Some(stream) = &self.stream {
            self.format = stream.negotiated_format();
        }
        Ok(())
    }

    fn camera_format(&self) -> CameraFormat {
        self.format
    }

    fn set_camera_format(&mut self, new_fmt: CameraFormat) -> Result<(), NokhwaError> {
        // Takes effect on the next `open_stream`; AVFoundation cannot re-pin the
        // format of a running session without tearing it down.
        self.format = new_fmt;
        Ok(())
    }

    #[allow(clippy::cast_possible_truncation)]
    #[allow(clippy::cast_sign_loss)]
    fn compatible_list_by_resolution(
        &mut self,
        fourcc: FrameFormat,
    ) -> Result<HashMap<Resolution, Vec<u32>>, NokhwaError> {
        let supported_cfmt = self
            .device
            .supported_formats()?
            .into_iter()
            .filter(|x| x.format() != fourcc);
        let mut res_list = HashMap::new();
        for format in supported_cfmt {
            match res_list.get_mut(&format.resolution()) {
                Some(fpses) => Vec::push(fpses, format.frame_rate()),
                None => {
                    res_list.insert(format.resolution(), vec![format.frame_rate()]);
                }
            }
        }
        Ok(res_list)
    }

    fn compatible_fourcc(&mut self) -> Result<Vec<FrameFormat>, NokhwaError> {
        let mut formats = self
            .device
            .supported_formats()?
            .into_iter()
            .map(|fmt| fmt.format())
            .collect::<Vec<FrameFormat>>();
        formats.sort();
        formats.dedup();
        Ok(formats)
    }

    fn resolution(&self) -> Resolution {
        self.camera_format().resolution()
    }

    fn set_resolution(&mut self, new_res: Resolution) -> Result<(), NokhwaError> {
        let mut format = self.camera_format();
        format.set_resolution(new_res);
        self.set_camera_format(format)
    }

    fn frame_rate(&self) -> u32 {
        self.camera_format().frame_rate()
    }

    fn set_frame_rate(&mut self, new_fps: u32) -> Result<(), NokhwaError> {
        let mut format = self.camera_format();
        format.set_frame_rate(new_fps);
        self.set_camera_format(format)
    }

    fn frame_format(&self) -> FrameFormat {
        self.camera_format().format()
    }

    fn set_frame_format(&mut self, fourcc: FrameFormat) -> Result<(), NokhwaError> {
        let mut format = self.camera_format();
        format.set_format(fourcc);
        self.set_camera_format(format)
    }

    fn camera_control(&self, control: KnownCameraControl) -> Result<CameraControl, NokhwaError> {
        // Camera controls (focus/exposure/zoom) are unused by videocall-cli and
        // not exposed by the Swift capture layer.
        Err(NokhwaError::GetPropertyError {
            property: control.to_string(),
            error: "Camera controls are unsupported".to_string(),
        })
    }

    fn camera_controls(&self) -> Result<Vec<CameraControl>, NokhwaError> {
        Ok(Vec::new())
    }

    fn set_camera_control(
        &mut self,
        _id: KnownCameraControl,
        _value: ControlValueSetter,
    ) -> Result<(), NokhwaError> {
        Err(NokhwaError::UnsupportedOperationError(
            ApiBackend::AVFoundation,
        ))
    }

    fn open_stream(&mut self) -> Result<(), NokhwaError> {
        let stream = self.device.open(self.format)?;
        // Adopt the geometry the Swift side actually negotiated.
        self.format = stream.negotiated_format();
        self.stream = Some(stream);
        Ok(())
    }

    fn is_stream_open(&self) -> bool {
        self.stream.is_some()
    }

    fn frame(&mut self) -> Result<Buffer, NokhwaError> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| NokhwaError::ReadFrameError("Stream is not open".to_string()))?;
        let (data, format) = stream.recv()?;
        // Take ownership of the frame bytes rather than re-copying them.
        let buffer = Buffer::new_from_vec(self.format.resolution(), data, format);
        // Drop any frame that queued behind this one so the next `frame()`
        // returns fresh data instead of a backlog (the channel is bounded, so
        // this is at most one buffered frame).
        stream.drain();
        Ok(buffer)
    }

    fn frame_raw(&mut self) -> Result<Cow<[u8]>, NokhwaError> {
        let stream = self
            .stream
            .as_ref()
            .ok_or_else(|| NokhwaError::ReadFrameError("Stream is not open".to_string()))?;
        let (data, _format) = stream.recv()?;
        Ok(Cow::from(data))
    }

    fn stop_stream(&mut self) -> Result<(), NokhwaError> {
        // Dropping the stream stops the Swift session and releases its handle.
        self.stream = None;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl Drop for AVFoundationCaptureDevice {
    fn drop(&mut self) {
        let _ = self.stop_stream();
    }
}

/// The backend struct that interfaces with V4L2.
/// To see what this does, please see [`CaptureBackendTrait`].
/// # Quirks
/// - While working with `iOS` is allowed, it is not officially supported and may not work.
/// - You **must** call [`nokhwa_initialize`](crate::nokhwa_initialize) **before** doing anything with `AVFoundation`.
/// - This only works on 64 bit platforms.
/// - FPS adjustment does not work.
/// - If permission has not been granted and you call `init()` it will error.
#[cfg_attr(feature = "docs-features", doc(cfg(feature = "input-avfoundation")))]
#[cfg(not(target_os = "macos"))]
pub struct AVFoundationCaptureDevice {}

#[cfg(not(target_os = "macos"))]
#[allow(unused_variables)]
#[allow(unreachable_code)]
impl AVFoundationCaptureDevice {
    /// Creates a new capture device using the `AVFoundation` backend. Indexes are gives to devices by the OS, and usually numbered by order of discovery.
    ///
    /// If `camera_format` is `None`, it will be spawned with with 640x480@15 FPS, MJPEG [`CameraFormat`] default.
    /// # Errors
    /// This function will error if the camera is currently busy or if `AVFoundation` can't read device information, or permission was not given by the user.
    pub fn new(index: &CameraIndex, req_fmt: RequestedFormat) -> Result<Self, NokhwaError> {
        todo!()
    }

    /// Creates a new capture device using the `AVFoundation` backend with desired settings.
    ///
    /// # Errors
    /// This function will error if the camera is currently busy or if `AVFoundation` can't read device information, or permission was not given by the user.
    #[deprecated(since = "0.10.0", note = "please use `new` instead.")]
    #[allow(clippy::cast_possible_truncation)]
    pub fn new_with(
        index: usize,
        width: u32,
        height: u32,
        fps: u32,
        fourcc: FrameFormat,
    ) -> Result<Self, NokhwaError> {
        todo!()
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(unreachable_code)]
impl CaptureBackendTrait for AVFoundationCaptureDevice {
    fn backend(&self) -> ApiBackend {
        todo!()
    }

    fn camera_info(&self) -> &CameraInfo {
        todo!()
    }

    fn refresh_camera_format(&mut self) -> Result<(), NokhwaError> {
        todo!()
    }

    fn camera_format(&self) -> CameraFormat {
        todo!()
    }

    fn set_camera_format(&mut self, _: CameraFormat) -> Result<(), NokhwaError> {
        todo!()
    }

    fn compatible_list_by_resolution(
        &mut self,
        _: FrameFormat,
    ) -> Result<HashMap<Resolution, Vec<u32>>, NokhwaError> {
        todo!()
    }

    fn compatible_fourcc(&mut self) -> Result<Vec<FrameFormat>, NokhwaError> {
        todo!()
    }

    fn resolution(&self) -> Resolution {
        todo!()
    }

    fn set_resolution(&mut self, _: Resolution) -> Result<(), NokhwaError> {
        todo!()
    }

    fn frame_rate(&self) -> u32 {
        todo!()
    }

    fn set_frame_rate(&mut self, _: u32) -> Result<(), NokhwaError> {
        todo!()
    }

    fn frame_format(&self) -> FrameFormat {
        todo!()
    }

    fn set_frame_format(&mut self, _: FrameFormat) -> Result<(), NokhwaError> {
        todo!()
    }

    fn camera_control(&self, _: KnownCameraControl) -> Result<CameraControl, NokhwaError> {
        todo!()
    }

    fn camera_controls(&self) -> Result<Vec<CameraControl>, NokhwaError> {
        todo!()
    }

    fn set_camera_control(
        &mut self,
        _: KnownCameraControl,
        _: ControlValueSetter,
    ) -> Result<(), NokhwaError> {
        todo!()
    }

    fn open_stream(&mut self) -> Result<(), NokhwaError> {
        todo!()
    }

    fn is_stream_open(&self) -> bool {
        todo!()
    }

    fn frame(&mut self) -> Result<Buffer, NokhwaError> {
        todo!()
    }

    fn frame_raw(&mut self) -> Result<Cow<[u8]>, NokhwaError> {
        todo!()
    }

    fn stop_stream(&mut self) -> Result<(), NokhwaError> {
        todo!()
    }
}

#[cfg(not(target_os = "macos"))]
#[allow(unreachable_code)]
impl Drop for AVFoundationCaptureDevice {
    fn drop(&mut self) {
        todo!()
    }
}
