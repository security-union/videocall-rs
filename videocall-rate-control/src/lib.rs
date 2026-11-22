//! Viewer-driven video rate control.
//!
//! The viewer measures, the publisher decides:
//!
//! - [`viewer::ViewerEstimator`] (one per remote video stream) turns each
//!   received frame `(timestamp, sequence, bytes, arrival)` into a periodic
//!   [`Report`]: the path capacity and the standing backlog, plus one early
//!   report when a backlog starts.
//! - [`publisher::Publisher`] (or [`viewers::MultiViewerPublisher`]) turns
//!   the latest report, its age, the round trip and the report period into
//!   an encoder target with a drain/probe/hold policy.
//! - [`actuator`] maps target and capacity onto a tier table the caller
//!   owns: tier, encoder bitrate, and which camera frames to encode.
//! - [`signal`]: the one rule for going up (one probe step per feedback
//!   cycle, whatever the cause) and the one-bit cuts (the relay's
//!   CONGESTION signal, a viewer that goes quiet) it brings back from.
//!
//! Pure std, no I/O and no clocks: every time is an argument in ms, so the
//! crate builds for `wasm32-unknown-unknown` and native alike and every
//! decision can be replayed in a test. See the README for the wiring.

#![deny(missing_docs)]

pub mod actuator;
pub mod publisher;
pub mod signal;
pub mod viewer;
pub mod viewers;

pub use actuator::{
    choose_tier, default_tiers, encoder_bitrate_bps, FrameDecision, FrameGate, Tier,
};
pub use publisher::{drain_target_bps, Publisher, PublisherConfig, MIN_DRAIN_FRACTION};
pub use signal::LimitSource;
pub use viewer::{FrameInfo, FrameOutcome, Report, ViewerConfig, ViewerEstimator, NO_CEILING_KBPS};
pub use viewers::{combine_reports, Combined, MultiViewerPublisher};
