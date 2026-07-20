/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of Apache License, Version 2.0 or MIT license at your
 * option.
 */

//! Safe Rust wrappers over the `VideocallCapture` Swift static library's C ABI.
//!
//! Note: this module compiles for **both** macOS and iOS (the Swift capture
//! core is platform-neutral and `build.rs` builds it for either), but the only
//! higher-level consumer today — the `AVFoundationCaptureDevice` backend
//! adapter in `videocall-nokhwa` — is wired for `target_os = "macos"` and stays
//! `todo!()` on iOS. That is intentional: there is no iOS consumer yet; the
//! capture layer is built toward native so an iOS app can adopt it later.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};

use flume::{Receiver, Sender};
use videocall_nokhwa_core::{
    error::NokhwaError,
    types::{CameraFormat, CameraIndex, CameraInfo, FrameFormat, Resolution},
};

// ---------------------------------------------------------------------------
// Pixel-format codes shared with the Swift side (`VccPixelFormat`).
//
// These integers are ABI: they must stay in lockstep with the `VccPixelFormat`
// enum in `swift/Sources/VideocallCapture/PixelFormatMapping.swift`.
// ---------------------------------------------------------------------------

const VCC_PF_NV12: u32 = 0;
const VCC_PF_YUYV: u32 = 1;
const VCC_PF_BGRA: u32 = 2;
const VCC_PF_MJPEG: u32 = 3;
const VCC_PF_UNKNOWN: u32 = 255;

/// Map a Swift `VccPixelFormat` code to a nokhwa [`FrameFormat`].
///
/// Returns `None` for formats nokhwa cannot represent as a raw frame (`BGRA`,
/// which has no `FrameFormat`, and `unknown`). Callers drop such frames rather
/// than mislabel them.
fn vcc_to_frame_format(code: u32) -> Option<FrameFormat> {
    match code {
        VCC_PF_NV12 => Some(FrameFormat::NV12),
        VCC_PF_YUYV => Some(FrameFormat::YUYV),
        VCC_PF_MJPEG => Some(FrameFormat::MJPEG),
        // BGRA has no FrameFormat equivalent; unknown is unclassifiable.
        VCC_PF_BGRA | VCC_PF_UNKNOWN => None,
        _ => None,
    }
}

/// Map a nokhwa [`FrameFormat`] to the Swift `VccPixelFormat` code to request.
///
/// Formats the capture path does not deliver as raw pixel buffers map to
/// `unknown`, which the Swift side treats as "no preference" and falls back to
/// the device's native subtype.
fn frame_format_to_vcc(format: FrameFormat) -> u32 {
    match format {
        FrameFormat::NV12 => VCC_PF_NV12,
        FrameFormat::YUYV => VCC_PF_YUYV,
        FrameFormat::MJPEG => VCC_PF_MJPEG,
        FrameFormat::GRAY | FrameFormat::RAWRGB | FrameFormat::RAWBGR => VCC_PF_UNKNOWN,
    }
}

/// Copy a borrowed C string into an owned [`String`], treating null as empty.
fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    // SAFETY: the Swift side passes valid, NUL-terminated UTF-8 pointers that
    // outlive the call; we copy immediately.
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// C ABI declarations (implemented by `@_cdecl` functions in the Swift library).
// ---------------------------------------------------------------------------

/// Frame delivery callback: `(ctx, bytes, len, width, height, fourcc)`. `bytes`
/// points at tightly packed pixel data valid only for the call; `len` is a
/// Swift `Int` (signed, pointer-width); `fourcc` is a `VccPixelFormat` code.
type FrameCallback = extern "C" fn(*mut c_void, *const u8, isize, u32, u32, u32);

/// Permission result callback: `(ctx, granted)`.
type AccessCallback = extern "C" fn(*mut c_void, bool);

/// Device enumeration callback: `(ctx, index, uniqueID, localizedName, description)`.
type DeviceCallback =
    extern "C" fn(*mut c_void, usize, *const c_char, *const c_char, *const c_char);

/// Format enumeration callback: `(ctx, width, height, fourcc, minFps, maxFps)`.
type FormatCallback = extern "C" fn(*mut c_void, u32, u32, u32, f64, f64);

extern "C" {
    /// Current camera authorization: 0 = notDetermined, 1 = restricted,
    /// 2 = denied, 3 = authorized.
    fn vcc_authorization_status() -> i32;

    /// Request camera access; `callback(ctx, granted)` fires once on an
    /// arbitrary queue.
    fn vcc_request_access(callback: AccessCallback, ctx: *mut c_void);

    /// Enumerate cameras in discovery order, invoking `callback` per device.
    fn vcc_enumerate_devices(callback: DeviceCallback, ctx: *mut c_void);

    /// Enumerate a device's supported formats, invoking `callback` per format.
    fn vcc_enumerate_formats(unique_id: *const c_char, callback: FormatCallback, ctx: *mut c_void);

    /// Configure (but do not start) a capture session. Returns a retained
    /// opaque handle or null; writes the negotiated geometry to the out-params.
    fn vcc_capture_open(
        unique_id: *const c_char,
        width: u32,
        height: u32,
        fourcc: u32,
        fps: u32,
        out_width: *mut u32,
        out_height: *mut u32,
        out_fourcc: *mut u32,
        out_fps: *mut u32,
    ) -> *mut c_void;

    /// Begin frame delivery via `callback`. Returns 0 on success.
    fn vcc_capture_start(handle: *mut c_void, callback: FrameCallback, ctx: *mut c_void) -> i32;

    /// Stop frame delivery. Guarantees no callback fires after it returns.
    fn vcc_capture_stop(handle: *mut c_void);

    /// Release the handle. Must follow `vcc_capture_stop`.
    fn vcc_capture_close(handle: *mut c_void);
}

// ---------------------------------------------------------------------------
// Authorization.
// ---------------------------------------------------------------------------

/// Camera authorization status, mirroring `AVAuthorizationStatus`.
#[derive(Copy, Clone, Debug, Hash, Ord, PartialOrd, Eq, PartialEq)]
#[repr(isize)]
pub enum AVAuthorizationStatus {
    /// The user has not yet been asked.
    NotDetermined = 0,
    /// Access is restricted (e.g. parental controls) and cannot be granted.
    Restricted = 1,
    /// The user denied access.
    Denied = 2,
    /// The user granted access.
    Authorized = 3,
}

/// Read the current camera authorization status.
#[must_use]
pub fn current_authorization_status() -> AVAuthorizationStatus {
    // SAFETY: pure query with no arguments.
    match unsafe { vcc_authorization_status() } {
        0 => AVAuthorizationStatus::NotDetermined,
        1 => AVAuthorizationStatus::Restricted,
        3 => AVAuthorizationStatus::Authorized,
        // Treat anything unexpected (including 2) as denied.
        _ => AVAuthorizationStatus::Denied,
    }
}

type BoxedAccessCallback = Box<dyn Fn(bool) + Send + Sync>;

extern "C" fn access_trampoline(ctx: *mut c_void, granted: bool) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: `ctx` is the `Box<BoxedAccessCallback>` leaked in
    // `request_permission_with_callback`. The Swift side invokes this exactly
    // once, so reclaiming the box here is sound and frees it.
    let callback = unsafe { Box::from_raw(ctx as *mut BoxedAccessCallback) };
    // This runs arbitrary user code (the `nokhwa_initialize` closure). A panic
    // must not unwind across this `extern "C"` frame back into Swift — that is
    // undefined behavior and aborts the process — so contain it here. The
    // permission result is already delivered by value, so there is nothing to
    // recover; we just swallow the unwind.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(granted)));
}

/// Request camera access, invoking `callback` once with the result.
///
/// The callback may run on an arbitrary thread; it must be `Send + Sync`.
pub fn request_permission_with_callback(callback: impl Fn(bool) + Send + Sync + 'static) {
    let boxed: Box<BoxedAccessCallback> = Box::new(Box::new(callback));
    let ctx = Box::into_raw(boxed) as *mut c_void;
    // SAFETY: `ctx` is a valid, uniquely-owned pointer reclaimed exactly once in
    // `access_trampoline`, which the Swift side is contracted to call.
    unsafe { vcc_request_access(access_trampoline, ctx) };
}

// ---------------------------------------------------------------------------
// Device / format enumeration.
// ---------------------------------------------------------------------------

// This trampoline runs no user code and only does infallible work (string
// copies + Vec push), so it cannot panic across the `extern "C"` boundary; no
// `catch_unwind` guard is needed (unlike `access_trampoline`).
extern "C" fn device_trampoline(
    ctx: *mut c_void,
    index: usize,
    unique_id: *const c_char,
    name: *const c_char,
    description: *const c_char,
) {
    if ctx.is_null() {
        return;
    }
    // SAFETY: `ctx` is the `&mut Vec<CameraInfo>` passed by `query_avfoundation`
    // for the duration of the (synchronous) enumeration call.
    let out = unsafe { &mut *(ctx as *mut Vec<CameraInfo>) };
    out.push(CameraInfo::new(
        &cstr_to_string(name),
        &cstr_to_string(description),
        &cstr_to_string(unique_id),
        CameraIndex::Index(index as u32),
    ));
}

/// Enumerate all available cameras in discovery order.
///
/// # Errors
/// Currently infallible on the Swift side; the `Result` is kept for API
/// compatibility with the previous bindings.
pub fn query_avfoundation() -> Result<Vec<CameraInfo>, NokhwaError> {
    let mut out: Vec<CameraInfo> = Vec::new();
    // SAFETY: `device_trampoline` only borrows `out` for the synchronous call.
    unsafe {
        vcc_enumerate_devices(device_trampoline, (&mut out as *mut Vec<CameraInfo>).cast());
    }
    Ok(out)
}

/// Expand one supported (resolution, format, fps-range) into concrete
/// [`CameraFormat`] entries: always the range's max, plus a distinct,
/// non-degenerate min. Mirrors the previous bindings' per-range fps expansion.
///
/// Split out from the FFI trampoline so it can be unit-tested without a device.
fn push_range_formats(
    out: &mut Vec<CameraFormat>,
    resolution: Resolution,
    format: FrameFormat,
    min_fps: f64,
    max_fps: f64,
) {
    let max = max_fps as u32;
    if max != 0 {
        out.push(CameraFormat::new(resolution, format, max));
    }
    let min = min_fps as u32;
    if min != 0 && min != 1 && min != max {
        out.push(CameraFormat::new(resolution, format, min));
    }
}

// Infallible (fourcc classification + Vec push), so it cannot panic across the
// `extern "C"` boundary; no `catch_unwind` needed. The Swift side calls this
// once per supported frame-rate range of each format.
extern "C" fn format_trampoline(
    ctx: *mut c_void,
    width: u32,
    height: u32,
    fourcc: u32,
    min_fps: f64,
    max_fps: f64,
) {
    if ctx.is_null() {
        return;
    }
    let Some(format) = vcc_to_frame_format(fourcc) else {
        return;
    };
    // SAFETY: `ctx` is the `&mut Vec<CameraFormat>` supplied by
    // `supported_formats` for the duration of the synchronous call.
    let out = unsafe { &mut *(ctx as *mut Vec<CameraFormat>) };
    push_range_formats(
        out,
        Resolution::new(width, height),
        format,
        min_fps,
        max_fps,
    );
}

// ---------------------------------------------------------------------------
// Capture device + stream.
// ---------------------------------------------------------------------------

/// A selectable camera, addressable by its stable `uniqueID`.
///
/// Construction only resolves and validates the device; no capture session is
/// created until [`CaptureDevice::open`].
pub struct CaptureDevice {
    unique_id: String,
    info: CameraInfo,
}

impl CaptureDevice {
    /// Resolve a device by [`CameraIndex`] (discovery-order index or `uniqueID`).
    ///
    /// # Errors
    /// Returns [`NokhwaError::OpenDeviceError`] if no matching device exists.
    pub fn new(index: &CameraIndex) -> Result<Self, NokhwaError> {
        match index {
            CameraIndex::Index(idx) => {
                let devices = query_avfoundation()?;
                let info = devices.into_iter().nth(*idx as usize).ok_or_else(|| {
                    NokhwaError::OpenDeviceError(idx.to_string(), "Not Found".to_string())
                })?;
                Ok(CaptureDevice {
                    unique_id: info.misc(),
                    info,
                })
            }
            CameraIndex::String(id) => Self::from_id(id, None),
        }
    }

    /// Resolve a device by its `uniqueID`.
    ///
    /// # Errors
    /// Returns [`NokhwaError::OpenDeviceError`] if the device is not present.
    pub fn from_id(id: &str, index_hint: Option<CameraIndex>) -> Result<Self, NokhwaError> {
        let mut info = query_avfoundation()?
            .into_iter()
            .find(|d| d.misc() == id)
            .ok_or_else(|| {
                NokhwaError::OpenDeviceError(id.to_string(), "Device not found".to_string())
            })?;
        if let Some(hint) = index_hint {
            info.set_index(hint);
        }
        Ok(CaptureDevice {
            unique_id: id.to_string(),
            info,
        })
    }

    /// The device's [`CameraInfo`].
    #[must_use]
    pub fn info(&self) -> &CameraInfo {
        &self.info
    }

    /// The device's stable `uniqueID`.
    #[must_use]
    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    /// The formats this device advertises, one [`CameraFormat`] per
    /// resolution/fourcc/frame-rate combination.
    ///
    /// # Errors
    /// Returns [`NokhwaError::StructureError`] if the `uniqueID` is not a valid
    /// C string.
    pub fn supported_formats(&self) -> Result<Vec<CameraFormat>, NokhwaError> {
        let id =
            CString::new(self.unique_id.as_str()).map_err(|why| NokhwaError::StructureError {
                structure: "uniqueID CString".to_string(),
                error: why.to_string(),
            })?;
        let mut out: Vec<CameraFormat> = Vec::new();
        // SAFETY: `id` outlives the call; `format_trampoline` only borrows `out`
        // for the synchronous duration of the enumeration.
        unsafe {
            vcc_enumerate_formats(
                id.as_ptr(),
                format_trampoline,
                (&mut out as *mut Vec<CameraFormat>).cast(),
            );
        }
        // Per-range enumeration can yield the same (resolution, format, fps)
        // from overlapping ranges; collapse duplicates.
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Open and start a capture session at the requested [`CameraFormat`].
    ///
    /// On success, frames flow into the returned stream's channel until it is
    /// dropped.
    ///
    /// # Errors
    /// Returns [`NokhwaError::OpenDeviceError`] if the session cannot be
    /// configured, or [`NokhwaError::OpenStreamError`] if it cannot start.
    pub fn open(&self, format: CameraFormat) -> Result<CaptureStream, NokhwaError> {
        let id =
            CString::new(self.unique_id.as_str()).map_err(|why| NokhwaError::StructureError {
                structure: "uniqueID CString".to_string(),
                error: why.to_string(),
            })?;

        // Bounded so a stalled consumer (e.g. a busy VP9 encoder) cannot make
        // the capture callback grow memory without limit: the callback drops
        // frames when the channel is full (see `frame_trampoline`). Depth 2
        // keeps one frame in flight plus one queued; the consumer drains to the
        // freshest frame each `frame()`.
        let (sender, receiver) = flume::bounded(2);
        let sink = Box::new(FrameSink { sender });
        let ctx = Box::into_raw(sink) as *mut c_void;

        let mut out_width: u32 = 0;
        let mut out_height: u32 = 0;
        let mut out_fourcc: u32 = 0;
        let mut out_fps: u32 = 0;

        // SAFETY: `id` outlives the call; the out-pointers are valid locals.
        let handle = unsafe {
            vcc_capture_open(
                id.as_ptr(),
                format.resolution().width(),
                format.resolution().height(),
                frame_format_to_vcc(format.format()),
                format.frame_rate(),
                &mut out_width,
                &mut out_height,
                &mut out_fourcc,
                &mut out_fps,
            )
        };

        if handle.is_null() {
            // Reclaim the leaked sink; the Swift side never took ownership.
            // SAFETY: `ctx` came from `Box::into_raw` above and was not shared.
            drop(unsafe { Box::from_raw(ctx as *mut FrameSink) });
            return Err(NokhwaError::OpenDeviceError(
                self.unique_id.clone(),
                format!(
                    "Swift capture session failed to open — the device likely does not \
                     support the requested resolution {}x{}. Run `info --list-formats` \
                     to see supported resolutions.",
                    format.resolution().width(),
                    format.resolution().height(),
                ),
            ));
        }

        // The Swift side reads back the negotiated geometry. If it reports a
        // format we cannot represent, fall back to the requested one.
        let negotiated = CameraFormat::new(
            Resolution::new(out_width, out_height),
            vcc_to_frame_format(out_fourcc).unwrap_or_else(|| format.format()),
            out_fps,
        );

        // SAFETY: `handle` is the retained engine; `ctx` is the sink pointer,
        // handed to the Swift side which keeps it until `vcc_capture_stop`.
        let rc = unsafe { vcc_capture_start(handle, frame_trampoline, ctx) };
        if rc != 0 {
            // SAFETY: start failed, so no callback can be in flight. Close the
            // engine and reclaim the sink.
            unsafe {
                vcc_capture_close(handle);
                drop(Box::from_raw(ctx as *mut FrameSink));
            }
            return Err(NokhwaError::OpenStreamError(
                "Swift capture session failed to start".to_string(),
            ));
        }

        Ok(CaptureStream {
            handle,
            ctx,
            receiver,
            format: negotiated,
        })
    }
}

/// Boxed and handed to the Swift side as the frame-callback context. Holds the
/// channel end frames are pushed into.
struct FrameSink {
    sender: Sender<(Vec<u8>, FrameFormat)>,
}

extern "C" fn frame_trampoline(
    ctx: *mut c_void,
    bytes: *const u8,
    len: isize,
    _width: u32,
    _height: u32,
    fourcc: u32,
) {
    if ctx.is_null() || bytes.is_null() || len <= 0 {
        return;
    }
    let Some(format) = vcc_to_frame_format(fourcc) else {
        return;
    };
    // SAFETY: `ctx` is the `FrameSink` pointer, kept alive by the owning
    // `CaptureStream` until `vcc_capture_stop` guarantees no further callbacks.
    // `bytes`/`len` describe a valid, tightly-packed buffer for this call only.
    let sink = unsafe { &*(ctx as *const FrameSink) };
    let data = unsafe { std::slice::from_raw_parts(bytes, len as usize) }.to_vec();
    // `try_send` (not `send`) so a stalled consumer can never make this callback
    // block the AVFoundation delivery queue or grow memory: on a full channel
    // the frame is simply dropped. A disconnected receiver (stream stopping) is
    // likewise not worth propagating. This path runs no user code and does not
    // panic across the `extern "C"` boundary.
    let _ = sink.sender.try_send((data, format));
}

/// A running capture session. Frames arrive on an internal channel drained via
/// [`CaptureStream::recv`]. Dropping the stream stops and releases the Swift
/// session and reclaims the callback context.
pub struct CaptureStream {
    handle: *mut c_void,
    ctx: *mut c_void,
    receiver: Receiver<(Vec<u8>, FrameFormat)>,
    format: CameraFormat,
}

impl CaptureStream {
    /// The geometry the Swift side actually negotiated (may differ from the
    /// request if the device could not honor it exactly).
    #[must_use]
    pub fn negotiated_format(&self) -> CameraFormat {
        self.format
    }

    /// Block until the next frame arrives, returning its bytes and real format.
    ///
    /// # Errors
    /// Returns [`NokhwaError::ReadFrameError`] if the capture side has hung up.
    pub fn recv(&self) -> Result<(Vec<u8>, FrameFormat), NokhwaError> {
        self.receiver
            .recv()
            .map_err(|why| NokhwaError::ReadFrameError(why.to_string()))
    }

    /// Discard any frames buffered in the channel.
    pub fn drain(&self) {
        let _ = self.receiver.drain();
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        // Order matters: stop first (after this returns no callback can fire),
        // then close the engine, then reclaim the sink the Swift side held.
        // SAFETY: `handle`/`ctx` were produced by `CaptureDevice::open` and are
        // freed exactly once here.
        unsafe {
            vcc_capture_stop(self.handle);
            vcc_capture_close(self.handle);
            drop(Box::from_raw(self.ctx as *mut FrameSink));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vcc_frame_format_mapping_roundtrips() {
        assert_eq!(vcc_to_frame_format(VCC_PF_NV12), Some(FrameFormat::NV12));
        assert_eq!(vcc_to_frame_format(VCC_PF_YUYV), Some(FrameFormat::YUYV));
        assert_eq!(vcc_to_frame_format(VCC_PF_MJPEG), Some(FrameFormat::MJPEG));
        // BGRA and unknown have no FrameFormat and must be dropped, not guessed.
        assert_eq!(vcc_to_frame_format(VCC_PF_BGRA), None);
        assert_eq!(vcc_to_frame_format(VCC_PF_UNKNOWN), None);
        assert_eq!(frame_format_to_vcc(FrameFormat::NV12), VCC_PF_NV12);
        assert_eq!(frame_format_to_vcc(FrameFormat::YUYV), VCC_PF_YUYV);
    }

    #[test]
    fn push_range_formats_emits_max_and_distinct_min() {
        let res = Resolution::new(640, 480);

        // A `15..30` range offers both endpoints.
        let mut out = Vec::new();
        push_range_formats(&mut out, res, FrameFormat::YUYV, 15.0, 30.0);
        assert!(out.contains(&CameraFormat::new(res, FrameFormat::YUYV, 30)));
        assert!(out.contains(&CameraFormat::new(res, FrameFormat::YUYV, 15)));
        assert_eq!(out.len(), 2);

        // A `1..30` range: min == 1 is degenerate and skipped, only 30 remains.
        let mut out = Vec::new();
        push_range_formats(&mut out, res, FrameFormat::NV12, 1.0, 30.0);
        assert_eq!(out, vec![CameraFormat::new(res, FrameFormat::NV12, 30)]);

        // A `60..60` range yields a single 60 fps entry (min == max).
        let mut out = Vec::new();
        push_range_formats(&mut out, res, FrameFormat::NV12, 60.0, 60.0);
        assert_eq!(out, vec![CameraFormat::new(res, FrameFormat::NV12, 60)]);
    }

    #[test]
    fn per_range_enumeration_offers_discrete_30_and_60() {
        // Regression for the collapsed-range bug: a format exposing `1..30` and
        // `60..60` (two callbacks) must offer both 30 and 60 fps so an exact
        // 30 fps request can be fulfilled.
        let res = Resolution::new(1280, 720);
        let mut out = Vec::new();
        push_range_formats(&mut out, res, FrameFormat::NV12, 1.0, 30.0);
        push_range_formats(&mut out, res, FrameFormat::NV12, 60.0, 60.0);
        out.sort_unstable();
        out.dedup();
        let fpses: Vec<u32> = out.iter().map(CameraFormat::frame_rate).collect();
        assert!(fpses.contains(&30));
        assert!(fpses.contains(&60));
    }

    #[test]
    fn bounded_channel_drops_when_full_and_drains_to_empty() {
        // Mirrors the capture callback (try_send, drop-on-full) plus the
        // consumer's recv-then-drain, with the same depth the stream uses.
        let (tx, rx) = flume::bounded::<(Vec<u8>, FrameFormat)>(2);
        for i in 0u8..5 {
            let _ = tx.try_send((vec![i], FrameFormat::NV12));
        }
        // Only two frames are retained; the rest are dropped (no unbounded growth).
        assert_eq!(rx.len(), 2);
        // The consumer takes the oldest, then drains the backlog.
        let (first, _) = rx.recv().unwrap();
        assert_eq!(first, vec![0u8]);
        let _ = rx.drain();
        assert_eq!(rx.len(), 0);
    }
}
