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

//! Test utilities for the VP9 encoder TDD harness.
//!
//! Everything here is pure Rust except [`oracle`], which wraps libvpx's VP9
//! decoder and is therefore gated behind the `libvpx` feature on native targets.
//! Enabled with the `test-utils` feature.

pub mod i420;
pub mod ivf;
pub mod psnr;

#[cfg(all(feature = "libvpx", not(target_arch = "wasm32")))]
pub mod oracle;
