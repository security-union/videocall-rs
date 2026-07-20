//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//

import XCTest
import CoreVideo
import CoreMedia
@testable import VideocallCapture

final class FrameRateResolverTests: XCTestCase {
    func testExactMatchInsideSingleRange() {
        let result = FrameRateResolver.resolve(requested: 30, supportedRanges: [1.0...60.0])
        XCTAssertEqual(result?.fps, 30)
        XCTAssertEqual(result?.rangeIndex, 0)
    }

    func testClampsAboveMaximumIntoRange() {
        // A request beyond every range resolves to the clamped in-range value,
        // never merely "below the max" — leaving it out of range throws in AV.
        let result = FrameRateResolver.resolve(requested: 120, supportedRanges: [1.0...60.0])
        XCTAssertEqual(result?.fps, 60)
    }

    func testClampsBelowMinimumIntoRange() {
        let result = FrameRateResolver.resolve(requested: 5, supportedRanges: [15.0...30.0])
        XCTAssertEqual(result?.fps, 15)
    }

    func testPicksNearestRange() {
        // 24 is nearest the 30 range's clamped lower bound (30) vs the 60 range.
        let result = FrameRateResolver.resolve(
            requested: 24, supportedRanges: [30.0...30.0, 60.0...60.0])
        XCTAssertEqual(result?.fps, 30)
        XCTAssertEqual(result?.rangeIndex, 0)
    }

    func testTiePrefersLowerRate() {
        // Requested 45 is equidistant from 30 and 60; prefer the lower.
        let result = FrameRateResolver.resolve(
            requested: 45, supportedRanges: [60.0...60.0, 30.0...30.0])
        XCTAssertEqual(result?.fps, 30)
        XCTAssertEqual(result?.rangeIndex, 1)
    }

    func testEmptyRangesReturnsNil() {
        XCTAssertNil(FrameRateResolver.resolve(requested: 30, supportedRanges: []))
    }

    func testFractionalRateRoundsToNearest() {
        // UVC "60 fps" is often 59.99976; rounding must land on 60, not 59.
        let result = FrameRateResolver.resolve(
            requested: 60, supportedRanges: [1.0...59.99976])
        XCTAssertEqual(result?.fps, 60)
    }
}

final class PixelFormatMappingTests: XCTestCase {
    func testNV12VideoAndFullRangeBothMapToNV12() {
        XCTAssertEqual(
            PixelFormatMapping.vccCode(fromOSType: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange),
            .nv12)
        XCTAssertEqual(
            PixelFormatMapping.vccCode(fromOSType: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange),
            .nv12)
    }

    func testYUYVMapsToYuyv() {
        XCTAssertEqual(
            PixelFormatMapping.vccCode(fromOSType: kCVPixelFormatType_422YpCbCr8_yuvs), .yuyv)
    }

    func testUYVYIsNotYuyv() {
        // '2vuy' (UYVY) has swapped component order and must not be treated as
        // YUYV; it is classified unknown so we never mislabel it.
        XCTAssertEqual(
            PixelFormatMapping.vccCode(fromOSType: kCVPixelFormatType_422YpCbCr8), .unknown)
    }

    func testBGRAMapsToBgra() {
        XCTAssertEqual(
            PixelFormatMapping.vccCode(fromOSType: kCVPixelFormatType_32BGRA), .bgra)
    }

    func testRequestedNV12AsksForVideoRange() {
        // NV12 requests must ask for 8-bit video range ('420v'), never the
        // 10-bit type the old ObjC bindings used by mistake.
        XCTAssertEqual(
            PixelFormatMapping.osType(forRequested: .nv12),
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
    }

    func testRequestedYuyvAndBgra() {
        XCTAssertEqual(
            PixelFormatMapping.osType(forRequested: .yuyv), kCVPixelFormatType_422YpCbCr8_yuvs)
        XCTAssertEqual(
            PixelFormatMapping.osType(forRequested: .bgra), kCVPixelFormatType_32BGRA)
    }

    func testMjpegIsNotACaptureRequest() {
        XCTAssertNil(PixelFormatMapping.osType(forRequested: .mjpeg))
    }
}

/// Frees the single backing allocation handed to `CVPixelBufferCreateWithPlanarBytes`.
private let freePlanarBackingBytes: CVPixelBufferReleasePlanarBytesCallback = {
    _, dataPtr, _, _, _ in
    if let dataPtr {
        UnsafeMutableRawPointer(mutating: dataPtr).deallocate()
    }
}

final class FramePackerTests: XCTestCase {
    /// Build a planar NV12 `CVPixelBuffer` with fully-controlled per-plane
    /// strides and a deterministic byte pattern, plus the tightly-packed bytes
    /// `packTightly` must produce for it. Padding bytes are set to a `0xEE`
    /// sentinel (the pattern is masked below `0xEE`) so any padding that leaks
    /// into the packed output is detectable.
    private func makePlanarNV12(
        width: Int, height: Int, lumaStride: Int, chromaStride: Int
    ) -> (CVPixelBuffer, [UInt8]) {
        precondition(width % 2 == 0 && height % 2 == 0)
        precondition(lumaStride >= width && chromaStride >= width)
        let chromaW = width / 2
        let chromaH = height / 2
        let lumaSize = lumaStride * height
        let chromaSize = chromaStride * chromaH
        let total = lumaSize + chromaSize

        let block = UnsafeMutableRawPointer.allocate(byteCount: total, alignment: 64)
        memset(block, 0xEE, total)
        let bytes = block.assumingMemoryBound(to: UInt8.self)

        // Pattern is always < 0xEE, so it can never be confused with padding.
        func pattern(_ plane: Int, _ row: Int, _ col: Int) -> UInt8 {
            UInt8((plane * 64 + row * width + col) & 0x7F)
        }

        var expected = [UInt8]()
        // Luma plane: 1 byte/sample, so validBytes per row == width.
        for row in 0..<height {
            for col in 0..<width {
                let value = pattern(0, row, col)
                bytes[row * lumaStride + col] = value
                expected.append(value)
            }
        }
        // Chroma plane: interleaved CbCr, so validBytes per row == chromaW * 2 == width.
        for row in 0..<chromaH {
            for col in 0..<(chromaW * 2) {
                let value = pattern(1, row, col)
                bytes[lumaSize + row * chromaStride + col] = value
                expected.append(value)
            }
        }

        var planeBase: [UnsafeMutableRawPointer?] = [block, block + lumaSize]
        var planeWidth = [width, chromaW]
        var planeHeight = [height, chromaH]
        var planeBytesPerRow = [lumaStride, chromaStride]

        var pixelBuffer: CVPixelBuffer?
        let status = CVPixelBufferCreateWithPlanarBytes(
            kCFAllocatorDefault,
            width, height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            block, total,
            2,
            &planeBase,
            &planeWidth, &planeHeight, &planeBytesPerRow,
            freePlanarBackingBytes,
            nil, nil,
            &pixelBuffer)
        guard status == kCVReturnSuccess, let pixelBuffer else {
            block.deallocate()
            fatalError("CVPixelBufferCreateWithPlanarBytes failed: \(status)")
        }
        return (pixelBuffer, expected)
    }

    private func pack(_ pixelBuffer: CVPixelBuffer) -> [UInt8] {
        CVPixelBufferLockBaseAddress(pixelBuffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(pixelBuffer, .readOnly) }
        var out = [UInt8]()
        FramePacker.packTightly(pixelBuffer, into: &out)
        return out
    }

    func testPacksPlanarNV12StrippingRowPadding() {
        // Padded strides (bytesPerRow > validBytes): the row-by-row slow path.
        // Real Macs delivered contiguous planes, so this path went untested —
        // and a stride bug here is exactly the "packed data read at the wrong
        // width" that produced the side-by-side / banding corruption.
        let (pixelBuffer, expected) = makePlanarNV12(
            width: 4, height: 4, lumaStride: 16, chromaStride: 16)
        XCTAssertGreaterThan(
            CVPixelBufferGetBytesPerRowOfPlane(pixelBuffer, 0),
            CVPixelBufferGetWidthOfPlane(pixelBuffer, 0),
            "test must actually exercise the padded (bytesPerRow > width) path")

        let out = pack(pixelBuffer)

        XCTAssertEqual(out, expected)
        XCTAssertFalse(out.contains(0xEE), "row padding must be stripped, not copied through")
        // Tight size = W*H (luma) + W*(H/2) (interleaved chroma).
        XCTAssertEqual(out.count, 4 * 4 + 4 * (4 / 2))
    }

    func testPacksPlanarNV12ContiguousFastPath() {
        // Contiguous strides (bytesPerRow == validBytes): the one-shot fast path
        // must yield byte-identical output to the padded path.
        let (pixelBuffer, expected) = makePlanarNV12(
            width: 4, height: 4, lumaStride: 4, chromaStride: 4)
        let out = pack(pixelBuffer)
        XCTAssertEqual(out, expected)
        XCTAssertEqual(out.count, 24)
    }
}
