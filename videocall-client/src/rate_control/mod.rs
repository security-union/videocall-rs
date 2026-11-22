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

//! Glue between the client and the `videocall-rate-control` crate, which
//! holds the algorithm. The viewer measures, the publisher decides:
//!
//! - [`viewer`]: every remote camera frame is fed to that stream's
//!   estimator when its packet arrives; its capacity and backlog ride on the
//!   periodic diagnostics report ([`wire`]).
//! - [`camera`]: our own camera's publisher. Reports about our camera stream
//!   come in keyed by viewer session; the encode loop asks it for a plan on
//!   every captured frame.
//!
//! Screen share and audio are not driven by it.

pub mod camera;
pub(crate) mod viewer;
pub(crate) mod wire;

pub use camera::{CameraRateController, CameraRateStats, FramePlan, RateLimit, ReportedCapacity};
pub use videocall_rate_control::LimitSource;
/// `capacity_kbps` for "no ceiling seen", as the decoder diagnostics carry it.
pub use videocall_rate_control::NO_CEILING_KBPS;
