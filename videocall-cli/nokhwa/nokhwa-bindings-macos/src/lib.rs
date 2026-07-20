/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! AVFoundation camera bindings for `nokhwa`, on macOS and iOS.
//!
//! The Apple-side capture logic lives in the `VideocallCapture` Swift package
//! (`swift/`), compiled to a static library by `build.rs` and linked into the
//! final binary. This module is the thin, safe Rust boundary over that
//! library's hand-written C ABI (every symbol is prefixed `vcc_`): it declares
//! the `extern "C"` surface and wraps it in RAII types that own the Swift-side
//! resources and free them exactly once.
//!
//! Historically this file was ~2,400 lines of raw `objc` `msg_send!` calls; the
//! Swift rewrite deletes that in favor of real AVFoundation code. See the
//! `swift/` package for the capture engine, device discovery, pixel-format
//! mapping, and frame-rate resolution.

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple;

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub use apple::*;
