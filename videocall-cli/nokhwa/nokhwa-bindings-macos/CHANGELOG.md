# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Rewrote the Apple camera backend: the ~2,400-line raw `objc` `msg_send!` AVFoundation binding is replaced by a Swift static library (`VideocallCapture`, in `swift/`) bridged over a hand-written 8-function `vcc_`-prefixed C ABI, with safe RAII wrappers in `src/apple.rs`. `build.rs` cross-builds the Swift package per Cargo target (macOS arm64/x86_64, iOS device, iOS simulator incl. Intel, Mac Catalyst) and is a no-op on non-Apple targets. The public API is unchanged.
- Reduced native dependencies from seven crates (`objc`, `cocoa-foundation`, `block`, `core-media-sys`, `core-video-sys`, `core-foundation`, `once_cell`) to `videocall-nokhwa-core` and `flume`.

### Fixed

- NV12 is now requested as 8-bit `'420v'`/`'420f'` instead of the 10-bit type the old bindings used by mistake.
- Frames now carry the real per-frame format read from the delivered buffer, instead of being tagged `GRAY` unconditionally; 420-biplanar fourccs are no longer misclassified as YUYV.
- `videoDataOutput.videoSettings` now pins the delivered width/height as well as the pixel format, so AVFoundation delivers the negotiated geometry rather than session-preset geometry (which corrupted frames).
- The frame channel is bounded (drop-on-full) rather than unbounded, capping memory under a stalled consumer.

### Added

- Builds now require the Xcode command-line tools (Swift toolchain) on macOS. The Swift package targets macOS 12+ and iOS 15+.

## [0.2.4](https://github.com/security-union/videocall-rs/compare/videocall-nokhwa-bindings-macos-v0.2.3...videocall-nokhwa-bindings-macos-v0.2.4) - 2025-10-30

### Other

- release ([#431](https://github.com/security-union/videocall-rs/pull/431))
- Add new decoder and Add MIT - Apache 2 license to all files ([#285](https://github.com/security-union/videocall-rs/pull/285))
- Bump all crates to 1.0.0 ([#222](https://github.com/security-union/videocall-rs/pull/222))
- Rename to videocall cli ([#185](https://github.com/security-union/videocall-rs/pull/185))

## [0.2.3](https://github.com/security-union/videocall-rs/compare/videocall-nokhwa-bindings-macos-v0.2.2...videocall-nokhwa-bindings-macos-v0.2.3) - 2025-10-13

### Other

- Add new decoder and Add MIT - Apache 2 license to all files ([#285](https://github.com/security-union/videocall-rs/pull/285))
- Bump all crates to 1.0.0 ([#222](https://github.com/security-union/videocall-rs/pull/222))
- Rename to videocall cli ([#185](https://github.com/security-union/videocall-rs/pull/185))
