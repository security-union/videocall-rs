//! Differential test harness: Google WebRTC's C++ NetEq is the reference,
//! our Rust `neteq` is the candidate. Both are driven through the same
//! [`NetEqUnderTest`] surface, in lockstep, over deterministic packet traces,
//! and every 10 ms frame is diffed.
//!
//! See `README.md` for setup (WebRTC checkout, nix shell) and usage.

#![warn(missing_docs)]

pub mod cneteq;
pub mod diff;
pub mod error;
pub mod ffi;
#[cfg(feature = "rust-neteq")]
pub mod libopus_decoder;
pub mod rng;
pub mod runner;
#[cfg(feature = "rust-neteq")]
pub mod rust_adapter;
pub mod scenarios;
pub mod signal;
pub mod trace;
pub mod types;
pub mod under_test;

pub use cneteq::CNetEq;
pub use diff::{compare, DiffReport, RunSummary};
pub use error::{HarnessError, Result};
pub use runner::{run_lockstep, Recording};
pub use scenarios::Scenario;
pub use trace::{Trace, TracePacket};
pub use types::Codec;
pub use under_test::{Implementation, NetEqUnderTest};

/// Runs `trace` through one implementation.
pub fn run_single(trace: &Trace, implementation: Implementation) -> Result<Recording> {
    let mut impls = vec![implementation.build(&trace.config, 0)?];
    Ok(run_lockstep(trace, &mut impls).remove(0))
}

/// Runs `trace` through the C++ reference and `candidate` in lockstep and
/// diffs them. Returns the report plus both recordings (reference first).
pub fn run_diff(trace: &Trace, candidate: Implementation) -> Result<(DiffReport, [Recording; 2])> {
    let mut impls = vec![
        Implementation::Cpp.build(&trace.config, 0)?,
        candidate.build(&trace.config, 0)?,
    ];
    let mut recs = run_lockstep(trace, &mut impls);
    let cand = recs.pop().unwrap_or_default();
    let reference = recs.pop().unwrap_or_default();
    let report = compare(trace, &reference, &cand);
    Ok((report, [reference, cand]))
}
