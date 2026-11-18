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

//! # neteq
//!
//! A line-by-line Rust port of Google WebRTC's NetEq (rev
//! `6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd`): jitter buffer, decision
//! logic, fixed-point packet loss concealment and time stretching, with
//! bit-exact `i16` output against the C++ implementation. See
//! `PARITY_PLAN.md`.
//!
//! The WebRTC-shaped API lives in [`api`] and is re-exported at the crate
//! root. Dashboard statistics live in [`telemetry`]. The browser worker
//! wrapper is `web::WebNetEq` (feature `web`, wasm32 only).
//!
//! Portions are derived from WebRTC and remain under its BSD-3-Clause
//! license; see `LICENSE-WEBRTC`.

pub mod api;
pub mod codecs;
pub mod error;
pub mod neteq;
pub mod rtc_base;
pub mod spl;
pub mod telemetry;

#[cfg(feature = "parity")]
pub mod parity;
#[cfg(feature = "tools")]
pub mod tools;

#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub mod web;

pub use api::{
    AudioFrame, Config, GetAudioResult, Mode, NetEq, NetEqBuilder, NetEqLifetimeStatistics,
    NetEqNetworkStatistics, NetEqOperationsAndState, Operation, OutputSpeechType, RtpHeader,
    RtpPacketInfo, SdpAudioFormat,
};
pub use error::{NetEqError, NetEqImplError, Result};
pub use telemetry::{q14, NetEqStats, TelemetryCollector};

#[cfg(all(feature = "web", target_arch = "wasm32"))]
pub use web::WebNetEq;
