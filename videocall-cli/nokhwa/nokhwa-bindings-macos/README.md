# Disclaimer
This is a fork of [nokhwa](https://github.com/l1npengtul/nokhwa), tailored to the videocall ecosystem needs.

# nokhwa-bindings-macos
Apple (macOS + iOS) camera-capture bindings for the `nokhwa` crate.

It is not meant for general consumption. No support or API stability is given; it is subject to change at any time. If you are looking for a general-purpose macOS camera crate, use `nokhwa` with the `input-native` feature.

## Architecture

The AVFoundation work is written in Swift and reached from Rust over a hand-written C ABI — no third-party interop crates. Three layers:

```
 Rust  ─  src/apple.rs
          safe RAII wrappers (CaptureDevice, CaptureStream), extern "C" decls
            │
 C ABI ─  8 @_cdecl functions, all vcc_-prefixed
            │
 Swift ─  VideocallCapture static library (swift/)
          CaptureEngine · DeviceDiscovery · PixelFormatMapping · FrameRateResolver · FramePacker
            └─ AVFoundation / CoreMedia / CoreVideo
```

- **Swift layer** (`swift/Sources/VideocallCapture/`) drives an `AVCaptureSession` on a dedicated serial queue with `beginConfiguration`/`commitConfiguration` bracketing. It selects a device format at the *exact* requested resolution or fails, pins `videoDataOutput.videoSettings` to both the negotiated pixel format and width/height, resolves the frame rate by clamping into a supported `CMTime` range, and hands Rust tightly-packed planes (row padding stripped by `FramePacker`).
- **C ABI** (`swift/Sources/VideocallCapture/FFI.swift`) is 8 `@_cdecl` functions. Strings and frame bytes cross as pointers valid only for the duration of the call; the Rust side copies them out.
- **Rust layer** (`src/apple.rs`) declares the `extern "C"` surface and wraps it in RAII types that own the Swift-side resources and free them exactly once. `src/lib.rs` is a `cfg` gate that exposes this module only on `macos`/`ios`.

Historically `src/lib.rs` was ~2,400 lines of raw `objc` `msg_send!` calls; the Swift rewrite replaces all of it. The bindings crate's native dependencies dropped from seven (`objc`, `cocoa-foundation`, `block`, `core-media-sys`, `core-video-sys`, `core-foundation`, `once_cell`) to just `videocall-nokhwa-core` and `flume`.

## C ABI surface

Every symbol is prefixed `vcc_` (**v**ideo**c**all **c**apture, after the `VideocallCapture` Swift package) — C has no namespaces, so the prefix keeps our exported symbols collision-free at link time. Full signatures live in `src/apple.rs` (`extern "C"` block) and `swift/Sources/VideocallCapture/FFI.swift` (`@_cdecl` definitions); they must stay in lockstep.

| Function | Purpose |
|----------|---------|
| `vcc_authorization_status` | Current camera authorization (0 notDetermined, 1 restricted, 2 denied, 3 authorized). |
| `vcc_request_access` | Request camera access; the callback fires once on an arbitrary queue. |
| `vcc_enumerate_devices` | Invoke a callback per camera, in discovery order. |
| `vcc_enumerate_formats` | Invoke a callback once per supported frame-rate range of each format. |
| `vcc_capture_open` | Configure (but do not start) a session; return an opaque handle and the negotiated geometry. |
| `vcc_capture_start` | Begin frame delivery via a per-frame C callback; returns 0 on success. |
| `vcc_capture_stop` | Stop delivery; guarantees no callback fires after it returns. |
| `vcc_capture_close` | Release the handle (call after `vcc_capture_stop`). |

The pixel-format codes carried across the ABI (`VccPixelFormat` in `PixelFormatMapping.swift`, mirrored as `VCC_PF_*` in `apple.rs`) are also part of the contract: `0` NV12, `1` YUYV, `2` BGRA, `3` MJPEG, `255` unknown. Their integer values must not change without updating both sides.

## Frame-delivery contract

- Frames arrive as **tightly packed** bytes — `FramePacker` strips each plane's `bytesPerRow` padding. NV12 is the Y plane followed by interleaved CbCr; YUYV and BGRA are a single packed plane.
- Each frame carries the **real** format tag read from the delivered buffer's `OSType`, not the requested one. `bgra` and `unknown` have no `FrameFormat` equivalent and such frames are dropped rather than mislabeled.
- The negotiated resolution is **exactly** what was requested. If the device has no format at that resolution, `vcc_capture_open` fails and the Rust side returns an error (rather than silently capturing at a different size, which would desync the fixed-size decode buffer downstream).
- The Rust side buffers frames in a `flume::bounded(2)` channel: on a full channel the capture callback drops the frame, so a stalled consumer cannot make capture grow memory without bound. `CaptureStream::recv` blocks for the next frame.

## Building

`build.rs` compiles and links the Swift package automatically:

- On Apple targets (`macos`, `ios`) it derives the Swift target triple and SDK from the Cargo target, runs `swift build -c release`, then emits the flags to statically link `libVideocallCapture.a` plus the AVFoundation, CoreMedia, CoreVideo, and Foundation frameworks and the Swift runtime search paths. The Swift library is always release-built. Build artifacts are isolated under `OUT_DIR`, so the source tree stays clean and different Cargo targets do not clobber one another.
- On every other target it is a **no-op**, so Linux/Windows builds and CI are unaffected.

Supported Apple targets: macOS (arm64 and x86_64), iOS device, iOS simulator (including the Intel `x86_64-apple-ios` triple), and Mac Catalyst. iOS compiles today but has no higher-level consumer yet — the backend adapter (`nokhwa/src/backends/capture/avfoundation.rs`) is wired for `target_os = "macos"` and stays `todo!()` on iOS; the capture layer is built toward a future native iOS app.

### Requirements

- macOS host with the **Xcode command-line tools** (provides the Swift toolchain, `swift`, and `xcrun`). Install with `xcode-select --install`.
- The Swift package targets **macOS 12+** and **iOS 15+** (see `swift/Package.swift`).

## Testing

```sh
# Swift unit tests: frame-rate resolution, fourcc mapping, and the
# padded-stride FramePacker oracle (synthetic pixel buffers, no camera needed).
cd swift && swift test

# Rust unit tests: FFI fourcc mapping, per-range fps enumeration, and the
# bounded-channel drop-on-full behavior.
cargo test -p videocall-nokhwa-bindings-macos
```

## License

Dual-licensed under Apache-2.0 or MIT, at your option. This crate is a fork of [nokhwa](https://github.com/l1npengtul/nokhwa) by l1npengtul; upstream attribution is retained in the package authors.
