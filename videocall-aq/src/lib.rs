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
 */

//! Adaptive-quality primitives for videocall.rs.
//!
//! This crate extracts the adaptive-quality (AQ) logic out of `videocall-client`
//! so it can be reused by native consumers (e.g. the load-test bot) in addition
//! to the browser UI. It contains:
//!
//! - [`constants`] — tuning constants (tier definitions, PID gains, thresholds)
//! - [`manager`] — the [`AdaptiveQualityManager`] tier state machine
//! - [`controller`] — the [`EncoderBitrateController`] PID loop + tier glue
//! - [`screen_bitrate`] — the screen-share uplink backoff ladder (issue #2343)
//! - [`clock`] — a [`Clock`] trait that abstracts over browser `Date.now()`
//!   and native `SystemTime`, so AQ logic runs identically on both targets
//!   (and can be tested deterministically with `TestClock`).
//!
//! Browser callers continue to import AQ types from `videocall_client::*`;
//! the old paths are preserved via re-export shims.

pub mod aspect;
pub mod clock;
pub mod constants;
pub mod controller;
pub mod manager;
pub mod screen_bitrate;

pub use aspect::{
    capture_exceeds_encode_ceiling, fit_within_preserving_aspect, fit_within_tier_box,
    orient_box_to_source, screen_encode_box_for_capture, simulcast_layer_target_dims,
    SimulcastLayerDims,
};

pub use clock::{default_clock, Clock, TestClock};

#[cfg(not(target_arch = "wasm32"))]
pub use clock::SystemClock;

#[cfg(target_arch = "wasm32")]
pub use clock::JsDateClock;

pub use manager::{AdaptiveQualityManager, TierTransitionRecord};

pub use controller::{EncoderBitrateController, EncoderControl};

pub use screen_bitrate::{
    queued_ms_for, screen_bitrate_at_floor, screen_effective_bitrate_kbps, ScreenBaselineKbps,
    ScreenPressureStep, ScreenTargetKbps, ScreenUplinkGovernor, ScreenUplinkSample,
};
