//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.
//

import Foundation

/// Pure frame-rate selection, split out from `AVCaptureDevice` so it is
/// unit-testable. Mirrors the RemoteShutter `resolveFrameRate` logic.
public enum FrameRateResolver {
    /// Pick the supported frame-rate range whose clamped rate is nearest the
    /// request. AVFoundation frame-rate ranges are inclusive on both ends
    /// (some cameras report a degenerate `60...60`), and a rate outside every
    /// range raises an Objective-C exception Swift cannot catch — so the answer
    /// is always the clamped rate *inside* a range, never merely below the
    /// maximum. Ties prefer the lower rate so we never exceed the request
    /// unnecessarily. Returns `nil` when no ranges are reported.
    public static func resolve(
        requested: Int,
        supportedRanges: [ClosedRange<Double>]
    ) -> (fps: Int, rangeIndex: Int)? {
        let requestedFPS = Double(requested)
        let nearest = supportedRanges.enumerated()
            .map {
                (index: $0.offset,
                 fps: min(max(requestedFPS, $0.element.lowerBound), $0.element.upperBound))
            }
            .min { (abs($0.fps - requestedFPS), $0.fps) < (abs($1.fps - requestedFPS), $1.fps) }
        return nearest.map { (fps: Int($0.fps.rounded()), rangeIndex: $0.index) }
    }
}
