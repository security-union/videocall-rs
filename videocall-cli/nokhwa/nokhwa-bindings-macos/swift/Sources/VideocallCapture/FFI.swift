//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//
// C ABI consumed by `nokhwa-bindings-macos/src/lib.rs`. Every symbol is
// prefixed `vcc_`. Strings cross as UTF-8 C strings valid only for the
// duration of the call (the Rust side copies them); frame bytes likewise.
//

import Foundation
import AVFoundation

// MARK: - Authorization

/// 0 = notDetermined, 1 = restricted, 2 = denied, 3 = authorized.
@_cdecl("vcc_authorization_status")
public func vcc_authorization_status() -> Int32 {
    switch AVCaptureDevice.authorizationStatus(for: .video) {
    case .notDetermined: return 0
    case .restricted: return 1
    case .denied: return 2
    case .authorized: return 3
    @unknown default: return 2
    }
}

/// Request camera access; `callback(ctx, granted)` fires on an arbitrary queue.
@_cdecl("vcc_request_access")
public func vcc_request_access(
    _ callback: @escaping @convention(c) (UnsafeMutableRawPointer?, Bool) -> Void,
    _ ctx: UnsafeMutableRawPointer?
) {
    AVCaptureDevice.requestAccess(for: .video) { granted in
        callback(ctx, granted)
    }
}

// MARK: - Enumeration

/// Enumerate devices in discovery order, invoking
/// `callback(ctx, index, uniqueID, localizedName, description)` per device.
@_cdecl("vcc_enumerate_devices")
public func vcc_enumerate_devices(
    _ callback: @convention(c) (
        UnsafeMutableRawPointer?, UInt, UnsafePointer<CChar>, UnsafePointer<CChar>,
        UnsafePointer<CChar>
    ) -> Void,
    _ ctx: UnsafeMutableRawPointer?
) {
    let devices = DeviceDiscovery.allDevices()
    for (index, device) in devices.enumerated() {
        let info = DeviceDiscovery.info(for: device)
        info.uniqueID.withCString { uid in
            info.localizedName.withCString { name in
                info.description.withCString { desc in
                    callback(ctx, UInt(index), uid, name, desc)
                }
            }
        }
    }
}

/// Enumerate a device's supported formats, invoking
/// `callback(ctx, width, height, fourcc, minFps, maxFps)` once **per supported
/// frame-rate range** of each format. `fourcc` is a `VccPixelFormat` raw value.
///
/// Emitting per range (rather than collapsing a format's ranges into one global
/// min/max) matters: a format advertising both `1...30` and `60...60` must
/// offer 30 *and* 60 fps, so an exact 30 fps request can still be fulfilled. A
/// single collapsed `1...60` range would drop the discrete 30 fps option.
@_cdecl("vcc_enumerate_formats")
public func vcc_enumerate_formats(
    _ uniqueID: UnsafePointer<CChar>,
    _ callback: @convention(c) (
        UnsafeMutableRawPointer?, UInt32, UInt32, UInt32, Double, Double
    ) -> Void,
    _ ctx: UnsafeMutableRawPointer?
) {
    let id = String(cString: uniqueID)
    guard let device = DeviceDiscovery.device(uniqueID: id) else { return }
    for format in device.formats {
        let dims = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
        let subtype = CMFormatDescriptionGetMediaSubType(format.formatDescription)
        let vcc = PixelFormatMapping.vccCode(fromOSType: subtype)
        for range in format.videoSupportedFrameRateRanges {
            guard range.maxFrameRate > 0 else { continue }
            callback(
                ctx, UInt32(dims.width), UInt32(dims.height), vcc.rawValue,
                range.minFrameRate, range.maxFrameRate)
        }
    }
}

// MARK: - Capture session lifecycle

/// Open (configure but do not start) a capture session. Returns an opaque
/// retained handle, or `nil` on failure. The negotiated geometry is written to
/// the out-params (`fourcc` is a `VccPixelFormat` raw value).
@_cdecl("vcc_capture_open")
public func vcc_capture_open(
    _ uniqueID: UnsafePointer<CChar>,
    _ width: UInt32, _ height: UInt32, _ fourcc: UInt32, _ fps: UInt32,
    _ outWidth: UnsafeMutablePointer<UInt32>,
    _ outHeight: UnsafeMutablePointer<UInt32>,
    _ outFourcc: UnsafeMutablePointer<UInt32>,
    _ outFps: UnsafeMutablePointer<UInt32>
) -> UnsafeMutableRawPointer? {
    let id = String(cString: uniqueID)
    let requested = VccPixelFormat(rawValue: fourcc) ?? .unknown
    guard let engine = CaptureEngine(
        uniqueID: id, width: Int(width), height: Int(height),
        requested: requested, fps: Int(fps)
    ) else {
        return nil
    }
    outWidth.pointee = UInt32(engine.actualWidth)
    outHeight.pointee = UInt32(engine.actualHeight)
    outFourcc.pointee = engine.actualFormat.rawValue
    outFps.pointee = UInt32(engine.actualFps)
    return Unmanaged.passRetained(engine).toOpaque()
}

/// Start delivering frames via `callback(ctx, bytes, len, width, height,
/// fourcc)`. Returns 0 on success, -1 otherwise.
@_cdecl("vcc_capture_start")
public func vcc_capture_start(
    _ handle: UnsafeMutableRawPointer,
    _ callback: @escaping VccFrameCallback,
    _ ctx: UnsafeMutableRawPointer?
) -> Int32 {
    let engine = Unmanaged<CaptureEngine>.fromOpaque(handle).takeUnretainedValue()
    return engine.start(callback: callback, ctx: ctx) ? 0 : -1
}

/// Stop delivery. Guarantees no callback fires after it returns.
@_cdecl("vcc_capture_stop")
public func vcc_capture_stop(_ handle: UnsafeMutableRawPointer) {
    let engine = Unmanaged<CaptureEngine>.fromOpaque(handle).takeUnretainedValue()
    engine.stop()
}

/// Release the handle. Call `vcc_capture_stop` first.
@_cdecl("vcc_capture_close")
public func vcc_capture_close(_ handle: UnsafeMutableRawPointer) {
    Unmanaged<CaptureEngine>.fromOpaque(handle).release()
}
