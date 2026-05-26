//! `wasm-bindgen-test` smoke: exercise the shim under `wasm32-unknown-unknown`
//! using a deterministic seed so the test is reproducible across runs.
//!
//! Native test coverage lives inline in `src/shim.rs` and `src/profiles.rs`.
//! This file's job is just to confirm the crate links + executes on the
//! browser target (clock source = `web_time`, RNG seeded from `getrandom`
//! with the `js` feature).

#![cfg(target_arch = "wasm32")]

use videocall_netsim::{resolve_profile, Admission, Direction, NetSimShim, NetworkProfile};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
fn passthrough_admits_on_wasm() {
    let shim = NetSimShim::new(NetworkProfile::passthrough(), Direction::Up);
    assert_eq!(shim.admit(1234), Admission::Pass);
}

#[wasm_bindgen_test]
fn lossy_mobile_resolves_and_admits_on_wasm() {
    let profile = resolve_profile("lossy_mobile").expect("preset should resolve");
    let seeded = NetworkProfile {
        seed: Some(42),
        ..profile
    };
    let shim = NetSimShim::new(seeded, Direction::Up);
    // The exact admission depends on the seed; we just want to confirm
    // the shim returns *some* admission without panicking on the
    // wasm32 target (no native Instant, no native thread_rng).
    let _ = shim.admit(500);
}
