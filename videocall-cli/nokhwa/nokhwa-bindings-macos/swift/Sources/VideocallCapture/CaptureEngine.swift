//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//

import Foundation
import AVFoundation
import CoreVideo
import CoreMedia

/// C frame callback: `(ctx, bytes, len, width, height, fourcc)`. `bytes` is only
/// valid for the duration of the call; the Rust side copies it out.
public typealias VccFrameCallback = @convention(c) (
    UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, UInt32, UInt32, UInt32
) -> Void

/// Owns a single `AVCaptureSession` and streams tightly-packed frames to a C
/// callback. All session mutation is confined to `sessionQueue`; frame delivery
/// runs on `dataOutputQueue`. Follows the RemoteShutter `CaptureEngine`
/// patterns (dedicated serial session queue with a re-entrancy guard, explicit
/// begin/commit bracketing, `alwaysDiscardsLateVideoFrames`, and a
/// clamp-into-the-range frame-rate resolver).
public final class CaptureEngine: NSObject, AVCaptureVideoDataOutputSampleBufferDelegate {
    private let captureSession = AVCaptureSession()
    private let videoDataOutput = AVCaptureVideoDataOutput()
    private var videoDeviceInput: AVCaptureDeviceInput?

    private let sessionQueue = DispatchQueue(label: "video.videocall.capture.session")
    private static let sessionQueueKey = DispatchSpecificKey<Void>()
    private let dataOutputQueue = DispatchQueue(label: "video.videocall.capture.frames")

    private let callbackLock = NSLock()
    private var frameCallback: VccFrameCallback?
    private var frameCtx: UnsafeMutableRawPointer?

    /// Reused across frames to avoid per-frame reallocation on the delivery
    /// queue. Only touched on `dataOutputQueue`.
    private var scratch = [UInt8]()

    // Negotiated capture facts, read back after configuration.
    public private(set) var actualWidth: Int = 0
    public private(set) var actualHeight: Int = 0
    public private(set) var actualFps: Int = 0
    public private(set) var actualFormat: VccPixelFormat = .unknown

    /// Configure a session for `uniqueID` at the requested geometry. Returns
    /// `nil` if the device is missing or the session cannot be configured.
    public init?(uniqueID: String, width: Int, height: Int, requested: VccPixelFormat, fps: Int) {
        super.init()
        sessionQueue.setSpecific(key: Self.sessionQueueKey, value: ())
        guard let device = DeviceDiscovery.device(uniqueID: uniqueID) else {
            return nil
        }
        let ok = syncOnSessionQueue {
            self.configureLocked(device: device, width: width, height: height,
                                 requested: requested, fps: fps)
        }
        if !ok { return nil }
    }

    private func syncOnSessionQueue<T>(_ body: () -> T) -> T {
        if DispatchQueue.getSpecific(key: Self.sessionQueueKey) != nil {
            return body()
        }
        return sessionQueue.sync(execute: body)
    }

    private func configureLocked(
        device: AVCaptureDevice, width: Int, height: Int,
        requested: VccPixelFormat, fps: Int
    ) -> Bool {
        dispatchPrecondition(condition: .onQueue(sessionQueue))

        let input: AVCaptureDeviceInput
        do {
            input = try AVCaptureDeviceInput(device: device)
        } catch {
            return false
        }

        captureSession.beginConfiguration()

        guard captureSession.canAddInput(input) else {
            captureSession.commitConfiguration()
            return false
        }
        captureSession.addInput(input)
        videoDeviceInput = input

        guard captureSession.canAddOutput(videoDataOutput) else {
            captureSession.removeInput(input)
            captureSession.commitConfiguration()
            return false
        }
        videoDataOutput.alwaysDiscardsLateVideoFrames = true
        captureSession.addOutput(videoDataOutput)

        // Pin the device format to EXACTLY the requested resolution. The
        // consumer sizes its output (I420) buffer from the requested resolution
        // before the stream opens and never re-reads it, so delivering a
        // different resolution would desync that buffer and crash the decoder.
        // If the device has no format at the requested resolution, fail here so
        // the Rust side surfaces a clean "cannot fulfill request" error (the
        // same contract the old objc backend enforced) rather than silently
        // capturing at the wrong size.
        guard let format = selectFormat(device: device, width: width, height: height,
                                        requested: requested) else {
            captureSession.commitConfiguration()
            return false
        }
        do {
            try device.lockForConfiguration()
            device.activeFormat = format
            device.unlockForConfiguration()
        } catch {
            captureSession.commitConfiguration()
            return false
        }

        applyFrameRate(device: device, fps: fps)

        // Choose the delivered pixel format: prefer the requested one, then the
        // device format's native subtype, then the first available.
        let available = videoDataOutput.availableVideoPixelFormatTypes
        var chosen: OSType?
        if let want = PixelFormatMapping.osType(forRequested: requested), available.contains(want) {
            chosen = want
        } else {
            let native = CMFormatDescriptionGetMediaSubType(device.activeFormat.formatDescription)
            if available.contains(native) {
                chosen = native
            } else {
                chosen = available.first
            }
        }
        if let chosen {
            // Also pin the OUTPUT width/height. On macOS the delivered buffer
            // resolution follows the session/connection, NOT the device's
            // `activeFormat` — so without this the output can hand back
            // preset-sized (e.g. 1080p) buffers even though we selected a 720p
            // format, and the consumer would decode them at the wrong size
            // (corrupt frames). Setting these keys makes AVFoundation deliver
            // exactly the requested geometry, scaling if necessary.
            videoDataOutput.videoSettings = [
                kCVPixelBufferPixelFormatTypeKey as String: chosen,
                kCVPixelBufferWidthKey as String: width,
                kCVPixelBufferHeightKey as String: height,
            ]
            actualFormat = PixelFormatMapping.vccCode(fromOSType: chosen)
        }

        captureSession.commitConfiguration()

        // Fail open: if we could not settle on a pixel format the Rust side
        // understands, abort configuration now. Otherwise `vcc_capture_open`
        // would hand back a live handle whose every frame is dropped, and the
        // consumer's blocking `frame()` would hang forever.
        guard actualFormat != .unknown else {
            return false
        }

        let dims = CMVideoFormatDescriptionGetDimensions(device.activeFormat.formatDescription)
        actualWidth = Int(dims.width)
        actualHeight = Int(dims.height)
        return true
    }

    /// A device format at EXACTLY the requested resolution, preferring one whose
    /// native subtype already matches the requested pixel format (to avoid a
    /// conversion). Returns `nil` when the device offers no format at that
    /// resolution — the caller treats that as an unfulfillable request rather
    /// than silently capturing at a different size (which would break the
    /// consumer's fixed-size decode buffer).
    private func selectFormat(
        device: AVCaptureDevice, width: Int, height: Int, requested: VccPixelFormat
    ) -> AVCaptureDevice.Format? {
        var exactAtRes: AVCaptureDevice.Format?
        for format in device.formats {
            let dims = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
            guard Int(dims.width) == width && Int(dims.height) == height else { continue }
            let native = PixelFormatMapping.vccCode(
                fromOSType: CMFormatDescriptionGetMediaSubType(format.formatDescription))
            if native == requested {
                return format
            }
            if exactAtRes == nil { exactAtRes = format }
        }
        return exactAtRes
    }

    private func applyFrameRate(device: AVCaptureDevice, fps: Int) {
        let ranges = device.activeFormat.videoSupportedFrameRateRanges
        guard let resolved = FrameRateResolver.resolve(
            requested: fps,
            supportedRanges: ranges.map { $0.minFrameRate...$0.maxFrameRate }
        ) else {
            actualFps = fps
            return
        }
        // Clamp the desired duration into the chosen range's OWN CMTimes rather
        // than rebuilding from integers — a UVC camera's "60 fps" is often
        // 59.99976, and an integer 1/60 falls outside the range and throws.
        let range = ranges[resolved.rangeIndex]
        var duration = CMTimeMake(value: 1, timescale: Int32(max(1, resolved.fps)))
        if CMTimeCompare(duration, range.minFrameDuration) < 0 { duration = range.minFrameDuration }
        if CMTimeCompare(duration, range.maxFrameDuration) > 0 { duration = range.maxFrameDuration }
        do {
            try device.lockForConfiguration()
            device.activeVideoMinFrameDuration = duration
            device.activeVideoMaxFrameDuration = duration
            device.unlockForConfiguration()
        } catch {
            // Leave the device default frame rate on failure.
        }
        actualFps = resolved.fps
    }

    /// Install the frame callback and start delivering. Returns `false` if the
    /// session is already running or cannot start.
    public func start(callback: @escaping VccFrameCallback, ctx: UnsafeMutableRawPointer?) -> Bool {
        return syncOnSessionQueue {
            guard !captureSession.isRunning else { return false }
            callbackLock.lock()
            frameCallback = callback
            frameCtx = ctx
            callbackLock.unlock()
            videoDataOutput.setSampleBufferDelegate(self, queue: dataOutputQueue)
            captureSession.startRunning()
            return true
        }
    }

    /// Stop delivery. After this returns, no further callback can fire: the
    /// delegate is cleared and the delivery queue is drained.
    public func stop() {
        syncOnSessionQueue {
            if captureSession.isRunning {
                captureSession.stopRunning()
            }
            videoDataOutput.setSampleBufferDelegate(nil, queue: nil)
        }
        // Flush any in-flight frame already dispatched to the delivery queue.
        dataOutputQueue.sync {}
        callbackLock.lock()
        frameCallback = nil
        frameCtx = nil
        callbackLock.unlock()
    }

    // MARK: - AVCaptureVideoDataOutputSampleBufferDelegate

    public func captureOutput(
        _ output: AVCaptureOutput,
        didOutput sampleBuffer: CMSampleBuffer,
        from connection: AVCaptureConnection
    ) {
        callbackLock.lock()
        let callback = frameCallback
        let ctx = frameCtx
        callbackLock.unlock()
        guard let callback,
              let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }

        CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        let osType = CVPixelBufferGetPixelFormatType(pixelBuffer)
        let vcc = PixelFormatMapping.vccCode(fromOSType: osType)

        // `scratch` is an independent copy, so the pixel buffer can be unlocked
        // and returned to AVFoundation's pool the instant packing finishes —
        // before the (potentially slower) C callback runs.
        FramePacker.packTightly(pixelBuffer, into: &scratch)
        CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly)

        scratch.withUnsafeBufferPointer { buf in
            callback(ctx, buf.baseAddress, buf.count, UInt32(width), UInt32(height), vcc.rawValue)
        }
    }

}

/// Copies a `CVPixelBuffer` into a caller-owned buffer with per-row padding
/// (`bytesPerRow`) stripped, so the Rust side receives tightly-packed planes.
///
/// Kept as a free `enum` of static methods — with no `AVCaptureSession` state —
/// so the padding-stripping math is unit-testable in `swift test` against
/// synthetic pixel buffers, without a live capture device.
enum FramePacker {
    /// Pack `pixelBuffer` into `dst`, stripping row padding: NV12 = Y plane then
    /// interleaved CbCr; YUYV/BGRA = one packed plane. `dst` is cleared first
    /// (retaining capacity) so a single reused buffer avoids per-frame
    /// reallocation on the hot delivery path. The caller must already hold the
    /// pixel buffer's base-address lock.
    static func packTightly(_ pixelBuffer: CVPixelBuffer, into dst: inout [UInt8]) {
        dst.removeAll(keepingCapacity: true)
        if CVPixelBufferIsPlanar(pixelBuffer) {
            let planeCount = CVPixelBufferGetPlaneCount(pixelBuffer)
            for plane in 0..<planeCount {
                guard let base = CVPixelBufferGetBaseAddressOfPlane(pixelBuffer, plane) else {
                    continue
                }
                let bytesPerRow = CVPixelBufferGetBytesPerRowOfPlane(pixelBuffer, plane)
                let rows = CVPixelBufferGetHeightOfPlane(pixelBuffer, plane)
                let planeWidth = CVPixelBufferGetWidthOfPlane(pixelBuffer, plane)
                // For 4:2:0 bi-planar: plane 0 (luma) is 1 byte/sample; plane 1
                // (chroma) is interleaved CbCr, 2 bytes per sample column.
                let validBytes = plane == 0 ? planeWidth : planeWidth * 2
                appendRows(
                    base: base, bytesPerRow: bytesPerRow, rows: rows,
                    validBytes: validBytes, into: &dst)
            }
        } else {
            guard let base = CVPixelBufferGetBaseAddress(pixelBuffer) else { return }
            let bytesPerRow = CVPixelBufferGetBytesPerRow(pixelBuffer)
            let rows = CVPixelBufferGetHeight(pixelBuffer)
            let width = CVPixelBufferGetWidth(pixelBuffer)
            let osType = CVPixelBufferGetPixelFormatType(pixelBuffer)
            let bytesPerPixel = osType == kCVPixelFormatType_32BGRA ? 4 : 2
            appendRows(
                base: base, bytesPerRow: bytesPerRow, rows: rows,
                validBytes: width * bytesPerPixel, into: &dst)
        }
    }

    private static func appendRows(
        base: UnsafeMutableRawPointer, bytesPerRow: Int, rows: Int, validBytes: Int,
        into dst: inout [UInt8]
    ) {
        // Fast path: when there is no row padding the whole plane is already
        // tightly packed, so copy it in one shot instead of row-by-row (~1080
        // appends/frame at 720p).
        if bytesPerRow == validBytes {
            let ptr = base.assumingMemoryBound(to: UInt8.self)
            dst.append(contentsOf: UnsafeBufferPointer(start: ptr, count: rows * validBytes))
            return
        }
        for row in 0..<rows {
            let rowPtr = base.advanced(by: row * bytesPerRow)
                .assumingMemoryBound(to: UInt8.self)
            dst.append(contentsOf: UnsafeBufferPointer(start: rowPtr, count: validBytes))
        }
    }
}
