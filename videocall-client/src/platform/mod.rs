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

//! Platform abstraction layer for videocall-client.
//!
//! This module provides cross-platform primitives that abstract over the differences
//! between WASM (browser) and native (desktop/server) environments:
//!
//! - **`now_ms()`** — current time in milliseconds since the Unix epoch
//! - **`IntervalHandle`** — a repeating timer that fires a callback at a fixed interval
//! - **`spawn(future)`** — spawn an async task on the platform's executor
//!
//! The correct implementation is selected at compile time via `cfg(target_arch = "wasm32")`,
//! following the same pattern used by `videocall-codecs` and `neteq`.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use web::*;

/// The error type used for connection failures.
///
/// On WASM this is `wasm_bindgen::JsValue` (rich JS error objects).
/// On native this is a plain `String`.
#[cfg(target_arch = "wasm32")]
pub type ConnectionError = wasm_bindgen::JsValue;

#[cfg(not(target_arch = "wasm32"))]
pub type ConnectionError = String;
