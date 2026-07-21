// swift-tools-version:5.7
//
// Copyright 2025 Security Union LLC
//
// Licensed under either of Apache License, Version 2.0 or MIT license at your
// option.

import PackageDescription

let package = Package(
    name: "VideocallCapture",
    platforms: [
        .macOS(.v12),
        .iOS(.v15)
    ],
    products: [
        // Static library: linked directly into the Rust `videocall-cli` binary
        // by `nokhwa-bindings-macos/build.rs`.
        .library(
            name: "VideocallCapture",
            type: .static,
            targets: ["VideocallCapture"]
        )
    ],
    targets: [
        .target(
            name: "VideocallCapture",
            path: "Sources/VideocallCapture"
        ),
        .testTarget(
            name: "VideocallCaptureTests",
            dependencies: ["VideocallCapture"],
            path: "Tests/VideocallCaptureTests"
        )
    ]
)
