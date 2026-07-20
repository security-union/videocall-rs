//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//

import CoreVideo
import CoreMedia

/// Stable pixel-format codes shared verbatim with the Rust side
/// (`nokhwa-bindings-macos/src/lib.rs`). The integer values are ABI and must
/// not change without updating both sides.
public enum VccPixelFormat: UInt32 {
    /// 4:2:0 bi-planar, 8-bit — Y plane then interleaved CbCr. Maps to the
    /// CoreVideo `420v` (video range) / `420f` (full range) types.
    case nv12 = 0
    /// Packed 4:2:2 `yuvs` (a.k.a. YUY2).
    case yuyv = 1
    /// Packed 32-bit BGRA.
    case bgra = 2
    /// Motion JPEG.
    case mjpeg = 3
    /// Anything we do not model. Reported to Rust, which drops it.
    case unknown = 255
}

/// Pure fourcc <-> `VccPixelFormat` mapping. Kept free of `AVCaptureSession`
/// state so it is unit-testable in `swift test`.
public enum PixelFormatMapping {
    /// The CoreVideo pixel-format type a capture output should be asked to
    /// deliver for a requested `VccPixelFormat`, or `nil` if the format is not
    /// a valid capture request (BGRA is capture-capable; MJPEG/unknown are
    /// not delivered as raw pixel buffers here).
    public static func osType(forRequested format: VccPixelFormat) -> OSType? {
        switch format {
        case .nv12: return kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        case .yuyv: return kCVPixelFormatType_422YpCbCr8_yuvs
        case .bgra: return kCVPixelFormatType_32BGRA
        case .mjpeg, .unknown: return nil
        }
    }

    /// Classify a CoreVideo/CoreMedia fourcc into a `VccPixelFormat`. Note that
    /// `2vuy` is UYVY (component order swapped vs. `yuvs`) and is intentionally
    /// left `unknown` — treating it as YUYV would corrupt colors.
    public static func vccCode(fromOSType osType: OSType) -> VccPixelFormat {
        switch osType {
        case kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange:
            return .nv12
        case kCVPixelFormatType_422YpCbCr8_yuvs:
            return .yuyv
        case kCVPixelFormatType_32BGRA:
            return .bgra
        case kCMVideoCodecType_JPEG, kCMVideoCodecType_JPEG_OpenDML:
            return .mjpeg
        default:
            return .unknown
        }
    }
}
