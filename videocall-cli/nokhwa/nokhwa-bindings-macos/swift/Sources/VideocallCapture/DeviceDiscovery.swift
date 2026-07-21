//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//

import Foundation
import AVFoundation

/// One selectable camera as the FFI surface reports it. A Mac exposes N cameras
/// (built-in, Continuity, external UVC) identified by `uniqueID` rather than a
/// front/back pair, so `uniqueID` is the stable key the Rust `CameraIndex`
/// carries.
public struct CaptureDeviceInfo: Equatable {
    public let uniqueID: String
    public let localizedName: String
    /// Human-readable extra info (manufacturer/model/position), matching the
    /// `desc` field the old bindings produced.
    public let description: String
}

public enum DeviceDiscovery {
    /// The device types we enumerate. The set differs by platform: on macOS
    /// `.external` (macOS 14+) / the deprecated `.externalUnknown` cover UVC
    /// webcams and `.continuityCamera` covers iPhone Continuity Camera; iOS has
    /// no external-camera path and instead exposes the built-in wide /
    /// ultra-wide / telephoto lenses. Built-in wide-angle is the FaceTime /
    /// primary camera on both.
    static func videoDeviceTypes() -> [AVCaptureDevice.DeviceType] {
        #if os(macOS)
        var types: [AVCaptureDevice.DeviceType] = [.builtInWideAngleCamera]
        if #available(macOS 14.0, *) {
            types.append(.external)
            types.append(.continuityCamera)
        } else {
            types.append(.externalUnknown)
        }
        return types
        #else
        // `DiscoverySession` silently omits any type the device lacks, so
        // listing ultra-wide/telephoto unconditionally is safe.
        var types: [AVCaptureDevice.DeviceType] = [.builtInWideAngleCamera]
        if #available(iOS 13.0, *) {
            types.append(.builtInUltraWideCamera)
            types.append(.builtInTelephotoCamera)
        }
        return types
        #endif
    }

    /// All video capture devices, discovery order preserved (this is the order
    /// the Rust `CameraIndex::Index(n)` refers to).
    public static func allDevices() -> [AVCaptureDevice] {
        let session = AVCaptureDevice.DiscoverySession(
            deviceTypes: videoDeviceTypes(),
            mediaType: .video,
            position: .unspecified)
        return session.devices
    }

    public static func info(for device: AVCaptureDevice) -> CaptureDeviceInfo {
        let description: String
        #if os(macOS)
        let manufacturer: String
        if #available(macOS 14.0, *) {
            manufacturer = device.manufacturer
        } else {
            manufacturer = "Apple Inc."
        }
        description = "\(manufacturer): \(device.modelID) - \(device.deviceType.rawValue)"
        #else
        // `AVCaptureDevice.manufacturer` is macOS-only; on iOS report the lens
        // position alongside the model and device type instead.
        description =
            "\(device.modelID) - \(device.deviceType.rawValue), position \(device.position.rawValue)"
        #endif
        return CaptureDeviceInfo(
            uniqueID: device.uniqueID,
            localizedName: device.localizedName,
            description: description)
    }

    public static func device(uniqueID: String) -> AVCaptureDevice? {
        AVCaptureDevice(uniqueID: uniqueID)
    }
}
