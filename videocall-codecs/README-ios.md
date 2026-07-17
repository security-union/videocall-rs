# Building `videocall-codecs` for Apple platforms

This crate ships the **pure-Rust VP9 encoder + decoder** to Swift via
[UniFFI](https://mozilla.github.io/uniffi-rs/). The build produces a single
`VideocallCodecs.xcframework` (zero libvpx, zero C) plus generated Swift
bindings, usable from iOS, macOS, and **watchOS** (the remote-shutter Watch app
decodes the camera preview on-device).

Everything is behind the `uniffi` cargo feature; the codec core is otherwise
unchanged and still builds for wasm and native Rust as before.

---

## What gets built

`./build_ios.sh` emits `target/VideocallCodecs.xcframework` with these slices:

| xcframework slice          | Rust target(s)                          | Toolchain          | Min OS     |
| -------------------------- | --------------------------------------- | ------------------ | ---------- |
| `ios-arm64`                    | `aarch64-apple-ios`                     | stable (Tier 2)    | iOS 15     |
| `ios-arm64-simulator`          | `aarch64-apple-ios-sim`                 | stable (Tier 2)    | iOS 15     |
| `ios-arm64_x86_64-maccatalyst` | `aarch64-apple-ios-macabi` + `x86_64-apple-ios-macabi` (lipo'd) | stable (Tier 2) | iOS 15 |
| `macos-arm64_x86_64`           | `aarch64-apple-darwin` + `x86_64-apple-darwin` (lipo'd) | stable (Tier 2) | macOS 12   |
| `watchos-arm64_arm64_32`       | `arm64_32-apple-watchos` + `aarch64-apple-watchos` (lipo'd) | **nightly (Tier 3)** | watchOS 10 |
| `watchos-arm64-simulator`      | `aarch64-apple-watchos-sim`             | **nightly (Tier 3)** | watchOS 10 |

The watchOS device slice is a single fat static library containing both
`arm64_32` (Apple Watch Series 4-8, the broad-coverage 32-bit-pointer ABI) and
`arm64` (Series 9 / Ultra 2 and newer).

**Mac Catalyst** (`ios-arm64_x86_64-maccatalyst`) is a *distinct* slice from
native `macos-arm64_x86_64`: a Catalyst app (`SUPPORTS_MACCATALYST=YES`,
iOS-on-Mac) links the `-macabi` ABI, which the native macOS slice does not
provide. The remote-shutter app is a Catalyst app, so this slice is required;
both it and the native macOS slice coexist in the framework.

Both macOS-family slices are **fat (arm64 + x86_64)**: Apple Silicon *and* Intel
Macs. This matters for the App Store — a Catalyst app whose deployment target is
below macOS 13 must ship the x86_64 slice or upload is rejected with
`ITMS-90981` (bundle supports Apple Silicon but not Intel).

---

## Prerequisites

Xcode with the iOS, macOS, **and watchOS** SDKs installed (`xcode-select -p`
should point at a full Xcode, not just the Command Line Tools).

### iOS / macOS targets (Tier 2 — simple)

These have prebuilt `std` and install with a plain `rustup target add`
(`-macabi` is the Mac Catalyst slice):

```bash
rustup target add aarch64-apple-ios aarch64-apple-ios-sim \
  aarch64-apple-ios-macabi aarch64-apple-darwin
```

### watchOS targets (Tier 3 — the harder path, needs nightly)

> **Caveat:** watchOS is a **Tier 3** Rust target with **no prebuilt `std`**.
> `rustup target add arm64_32-apple-watchos` fails on stable
> (`toolchain 'stable' does not support target 'arm64_32-apple-watchos'`).
> watchOS therefore requires a **nightly** toolchain plus `-Z build-std`, which
> compiles `std` from source for each watchOS target.

```bash
# A recent nightly is required: the codec uses u*::is_multiple_of, stabilized in
# Rust 1.87 — nightlies older than that fail to build the crate.
rustup toolchain install nightly --component rust-src
```

`build_ios.sh` does **not** run `rustup target add` for the watchOS triples —
under `-Z build-std` the target `std` is compiled on demand, so only the
`rust-src` component (above) is needed.

---

## Build

```bash
./videocall-codecs/build_ios.sh
```

Result: `target/VideocallCodecs.xcframework` and the Swift bindings at
`target/swift-codecs/videocall_codecs.swift`.

Useful environment overrides:

- `BUILD_WATCHOS=0 ./videocall-codecs/build_ios.sh` — skip the watchOS slices
  (e.g. on a machine with no nightly toolchain). The remote-shutter Watch app
  needs watchOS, so it is **on by default**.
- `NIGHTLY=nightly-2026-07-13 ./videocall-codecs/build_ios.sh` — pin a specific
  nightly instead of the `nightly` channel.

The script builds iOS/macOS with the **default (stable)** toolchain and only
switches to `+nightly ... -Z build-std` for the watchOS slices.

---

## Consuming the framework in Xcode

1. **Embed the framework.** Drag `target/VideocallCodecs.xcframework` into your
   Xcode project and add it to *every* target that uses the codec — the iOS app
   target **and** the watchOS extension/app target (Xcode picks the right slice
   per platform automatically).
2. **Add the bindings.** Add `target/swift-codecs/videocall_codecs.swift` to
   your sources (or vendor it into a Swift package). It calls into the static
   library through the framework's module map.
3. **Import and use.**

```swift
import videocall_codecs

// --- Encode ---
let encoder = try Vp9Encoder(
    width: 640, height: 480,
    fps: 30,
    bitrateKbps: 500,
    keyframeInterval: 150,
    minQuantizer: 40, maxQuantizer: 60,
    cpuUsed: 7
)
// `i420` is a full planar I420 buffer: width*height + 2*ceil(w/2)*ceil(h/2) bytes.
if let compressed = try encoder.encode(pts: ptsInTimebaseUnits, i420: i420) {
    send(compressed)          // a VP9 frame; nil means the encoder buffered it
}
try encoder.updateBitrate(kbps: 350)   // adapt to the network at runtime

// --- Decode (e.g. on the Watch) ---
let decoder = Vp9Decoder()               // feed a keyframe first, then inter frames in order
let frame = try decoder.decode(frame: compressed)
// frame.data  -> tightly-packed I420, frame.width x frame.height
```

### Swift API surface

- **`Vp9Encoder`** (class)
  - `init(width: UInt32, height: UInt32, fps: UInt32, bitrateKbps: UInt32, keyframeInterval: UInt32, minQuantizer: UInt32, maxQuantizer: UInt32, cpuUsed: UInt8) throws`
  - `func encode(pts: Int64, i420: Data) throws -> Data?` — compressed VP9 frame bytes, or `nil` if the encoder buffered this frame
  - `func updateBitrate(kbps: UInt32) throws`
- **`Vp9Decoder`** (class, stateful — decode a keyframe first, then inter frames in submission order)
  - `init()`
  - `func decode(frame: Data) throws -> DecodedFrame`
- **`DecodedFrame`** (struct): `data: Data` (tightly-packed I420), `width: UInt32`, `height: UInt32`
- **`CodecError: Error`** (thrown): `.InvalidConfig(message:)`, `.Encode(message:)`, `.Decode(message:)`, `.Internal(message:)`

---

## Notes / gotchas

- **Regenerating bindings after an API change:** the Swift file is generated from
  the compiled library, so re-run `build_ios.sh` (or just the `uniffi-bindgen
  generate` step) whenever the `#[uniffi::export]` surface in `src/ffi.rs`
  changes.
- **Nightly drift:** because watchOS pins to `nightly` + `build-std`, a future
  nightly that breaks `std`-from-source or a dependency will break only the
  watchOS slices. Pin with `NIGHTLY=...` if you hit that, and keep the iOS/macOS
  (stable) build as the stable baseline.
- **Harmless linker warning:** the watchOS simulator build prints
  `overriding '-mwatchos-simulator-version-min=10.0' ... '-target arm64-apple-watchos7.0.0-simulator'`.
  That is clang reconciling the Rust target spec's baseline with our min-version
  flag; it does not affect the produced library.
