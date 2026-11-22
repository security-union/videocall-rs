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

//! Traffic shaping: a hidden debug feature that simulates a narrow uplink
//! inside the sender.
//!
//! Outgoing media packets enter one FIFO that drains at a chosen bit rate,
//! plus an optional fixed delay, and only then reach the real connection.
//! Nothing else in the client knows the shaper exists.

mod control;
mod driver;
mod shaper;

pub(crate) use driver::TrafficShaping;
pub(crate) use shaper::ShapedKind;
pub use shaper::{
    TrafficShapingScope, TrafficShapingSettings, TrafficShapingStatus,
    TRAFFIC_SHAPING_DELAY_PRESETS_MS, TRAFFIC_SHAPING_RATE_PRESETS_BPS,
};
