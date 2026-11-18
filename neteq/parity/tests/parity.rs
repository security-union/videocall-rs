//! C++ vs Rust comparison tests (feature `rust-neteq`).
//!
//! Each test runs one scenario over every codec and requires the Rust NetEq
//! to be identical to the C++ reference: bit-exact samples, same operation
//! every frame, same delays and stats. On failure the printed summary names
//! the first divergent frame.
//!
//! Opus runs on C libopus on both sides (see `libopus_decoder`), so the gate
//! tests NetEq and not the codec. `ropus_drift_report` prints the same diff
//! with the `ropus` decoder and never fails.
//!
//! Narrow a run with `NETEQ_PARITY_CODEC=opus-mono` and a test name filter:
//! `cargo test --features rust-neteq --test parity burst_loss`.

#![cfg(feature = "rust-neteq")]

use neteq_parity::{run_diff, Codec, Implementation, Scenario};

fn codecs() -> Vec<Codec> {
    match std::env::var("NETEQ_PARITY_CODEC") {
        Ok(label) => vec![Codec::parse(&label).expect("NETEQ_PARITY_CODEC is a codec label")],
        Err(_) => Codec::all(),
    }
}

fn assert_parity(scenario: Scenario) {
    let mut failures = Vec::new();
    for codec in codecs() {
        let trace = scenario
            .generate(codec, 1, scenario.default_duration_ms())
            .expect("generate");
        let (report, _) = run_diff(&trace, Implementation::Rust).expect("run");
        print!("{}", report.summary());
        if !report.identical {
            failures.push(codec.label());
        }
    }
    assert!(
        failures.is_empty(),
        "{}: not identical for {failures:?}",
        scenario.name()
    );
}

macro_rules! parity_tests {
    ($($name:ident => $scenario:expr),* $(,)?) => {
        $(
            #[test]
            fn $name() {
                assert_parity($scenario);
            }
        )*
    };
}

parity_tests! {
    clean => Scenario::Clean,
    uniform_jitter => Scenario::UniformJitter,
    gaussian_spikes => Scenario::GaussianSpikes,
    burst_loss => Scenario::BurstLoss,
    reordering => Scenario::Reordering,
    clock_drift_fast => Scenario::ClockDriftFast,
    clock_drift_slow => Scenario::ClockDriftSlow,
    dtx => Scenario::Dtx,
    duplicates => Scenario::Duplicates,
    long_mixed => Scenario::LongMixed,
}

#[test]
fn every_scenario_is_covered() {
    assert_eq!(
        Scenario::ALL.len(),
        10,
        "add a parity test for the new scenario"
    );
}

/// Non-gating: how far `ropus` drifts from libopus through NetEq. Run with
/// `cargo test --features rust-neteq --test parity ropus -- --nocapture`.
#[test]
fn ropus_drift_report() {
    for scenario in [Scenario::Dtx, Scenario::LongMixed] {
        for codec in Codec::all()
            .into_iter()
            .filter(|c| matches!(c, Codec::Opus { .. }))
        {
            let trace = scenario
                .generate(codec, 1, scenario.default_duration_ms())
                .expect("generate");
            let (report, _) = run_diff(&trace, Implementation::RustRopus).expect("run");
            print!("{}", report.summary());
        }
    }
}
