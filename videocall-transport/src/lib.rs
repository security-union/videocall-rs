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

//! Cross-platform transport layer for videocall.rs.
//!
//! Provides WebSocket and WebTransport wrappers for both WASM (browser) and
//! native (desktop/server) targets.
//!
//! # Features
//!
//! - **`wasm`** — Browser-based transports using `web-sys` APIs
//! - **`native`** — Native transports using `web-transport-quinn` and `tokio`

// ── WASM transports ───────────────────────────────────────────────────────────

#[cfg(feature = "wasm")]
pub mod websocket;

#[cfg(feature = "wasm")]
pub mod webtransport;

// ── Native transports ─────────────────────────────────────────────────────────

#[cfg(feature = "native")]
pub mod native_websocket;

#[cfg(feature = "native")]
pub mod native_webtransport;
