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

//! NetEq end to end on the unedited fixture clip, with and without Opus DTX,
//! in virtual time (see `dtx_harness`). Every threshold is derived from a
//! requirement, never from what the code produces today; a failing test is
//! a finding. Audio for every scenario lands in
//! `<target dir>/neteq-dtx-harness/<scenario>/` before any assertion runs.

#![cfg(all(feature = "native", not(target_arch = "wasm32")))]

mod dtx_harness;

use dtx_harness::*;

/// A packet: the unit in which audio is lost, late or concealed.
const PACKET_MS: u64 = FRAME_MS;

/// NetEq's start target delay (`K_START_DELAY_MS`): a clean link never needs
/// more than this.
const START_TARGET_MS: u64 = 80;

/// Every talkspurt is delivered: at most 20% of its energy missing or added
/// (about 1 dB; one concealed 20 ms packet in a 300 ms talkspurt is 7%).
fn assert_speech_delivered(run: &Run) {
    let name = &run.scenario.name;
    let spurts = scored_talkspurts(run);
    assert!(
        !spurts.is_empty(),
        "{name}: no talkspurts after startup to score"
    );
    for t in spurts {
        assert!(
            t.delay_ms.is_some(),
            "{name}: talkspurt at {} ms not found in the output (envelope correlation {:.2})",
            t.start_ms,
            t.correlation
        );
        assert!(
            (0.8..=1.25).contains(&t.energy_ratio),
            "{name}: talkspurt at {} ms delivered energy ratio {:.2}, want 0.8..1.25",
            t.start_ms,
            t.energy_ratio
        );
    }
}

/// Pauses with DTX: comfort noise or quiet room noise at the source's level,
/// never concealment, never digital silence, no pumping.
fn assert_pauses_are_comfort_noise(run: &Run) {
    let name = &run.scenario.name;
    let r = &run.report;
    // Self-checks: the scenario cannot pass vacuously.
    assert!(
        r.encoded_dtx > 0,
        "{name}: the encoder produced no DTX frames"
    );
    assert!(
        r.sent < run.source.frames(),
        "{name}: every frame was sent ({}), so DTX did not reduce traffic",
        r.sent
    );
    // At least two 100 ms windows of deep silence to measure.
    assert!(
        r.deep_silence_ms >= 200,
        "{name}: only {} ms of deep silence to score",
        r.deep_silence_ms
    );
    assert_eq!(
        r.conceal_silence_ms, 0,
        "{name}: {} ms of concealment in {} ms of deep silence (pauses must be comfort noise, not loss)",
        r.conceal_silence_ms, r.deep_silence_ms
    );
    assert_eq!(
        r.digital_silence_frames, 0,
        "{name}: {} frames of digital silence (< -80 dBFS) in deep silence",
        r.digital_silence_frames
    );
    // Within 6 dB (a factor of two in amplitude) of the room noise sent.
    assert!(
        (r.silence_out_dbfs - r.silence_src_dbfs).abs() <= 6.0,
        "{name}: deep-silence output {:.1} dBFS vs source {:.1} dBFS",
        r.silence_out_dbfs,
        r.silence_src_dbfs
    );
    // A 6 dB swing between 100 ms windows is audible pumping.
    assert!(
        r.silence_spread_db <= 6.0,
        "{name}: deep-silence level spread {:.1} dB over {} windows",
        r.silence_spread_db,
        r.silence_windows
    );
}

#[test]
fn no_dtx_clean_network_delivers_speech_without_concealment_after_startup() {
    let run = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        Network::clean(),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    // Nothing is lost or late, so nothing may be concealed.
    assert_eq!(
        run.report.conceal_after_startup_ms, 0,
        "{name}: {} ms concealed after startup on a clean network",
        run.report.conceal_after_startup_ms
    );
    assert_speech_delivered(&run);
    let (_, net_max) = run.scenario.network.delay_range_ms();
    // Network + start target + one packet + one output tick + one envelope tick.
    let bound = net_max + START_TARGET_MS + PACKET_MS + TICK_MS + TICK_MS;
    for t in scored_talkspurts(&run) {
        let d = t.delay_ms.unwrap();
        assert!(
            d <= bound,
            "{name}: talkspurt at {} ms delay {d} ms > {bound} ms",
            t.start_ms
        );
    }
}

fn jitter_target_covers_jitter(timestamps: Timestamps) {
    let jitter_ms = 60;
    let run = run(&Scenario::natural(
        SenderMode::Continuous,
        timestamps,
        Network::jitter(jitter_ms),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    // To ride out packets up to 60 ms late the buffer must hold 60 ms.
    let target = median_target(&run, CONVERGED_MS, run.source.duration_ms());
    assert!(
        u64::from(target) >= jitter_ms,
        "{name}: median target delay {target} ms after convergence, jitter is {jitter_ms} ms"
    );
    // The delay manager aims at the 97th percentile: up to 3% of packets may
    // be late, and each late one costs at most twice its length (concealment
    // plus the merge back): 6% of deep speech.
    let r = &run.report;
    assert!(
        r.conceal_speech_ms * 100 <= r.deep_speech_ms * 6,
        "{name}: {} ms concealed in {} ms of deep speech (> 6%)",
        r.conceal_speech_ms,
        r.deep_speech_ms
    );
    assert_speech_delivered(&run);
}

#[test]
fn no_dtx_jitter60_rtp_timestamps_target_delay_covers_jitter() {
    jitter_target_covers_jitter(Timestamps::Rtp);
}

#[test]
fn no_dtx_jitter60_saturated_timestamps_target_delay_covers_jitter() {
    jitter_target_covers_jitter(Timestamps::Saturated);
}

#[test]
fn dtx_webrtc_clean_network_pauses_are_comfort_noise_not_concealment() {
    let run = run(&Scenario::natural(
        SenderMode::DtxWebrtc,
        Timestamps::Rtp,
        Network::clean(),
    ));
    assert_sane(&run);
    assert!(
        run.report.sent_dtx > 0,
        "{}: no DTX frame was transmitted",
        run.scenario.name
    );
    assert_pauses_are_comfort_noise(&run);
}

#[test]
fn dtx_dropall_clean_network_pauses_are_comfort_noise_not_concealment() {
    let run = run(&Scenario::natural(
        SenderMode::DtxDropAll,
        Timestamps::Rtp,
        Network::clean(),
    ));
    assert_sane(&run);
    assert_eq!(
        run.report.sent_dtx, 0,
        "{}: a DTX frame was transmitted",
        run.scenario.name
    );
    assert_pauses_are_comfort_noise(&run);
}

/// Same clip, same network seed: speech with DTX is not worse than without.
fn dtx_speech_not_worse(mode: SenderMode, network: Network) {
    let without = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        network,
    ));
    let with = run(&Scenario::natural(mode, Timestamps::Rtp, network));
    assert_sane(&without);
    assert_sane(&with);
    let name = &with.scenario.name;
    let a = scored_talkspurts(&without);
    let b = scored_talkspurts(&with);
    assert_eq!(
        a.len(),
        b.len(),
        "{name}: same source, different talkspurts"
    );
    assert!(!b.is_empty(), "{name}: no talkspurts to compare");
    let mut excess = Vec::new();
    let mut failures = Vec::new();
    for (c, d) in a.iter().zip(&b) {
        let at = d.start_ms;
        let (Some(dc), Some(dd)) = (c.delay_ms, d.delay_ms) else {
            failures.push(format!(
                "talkspurt {at} ms: not found (corr without {:.2}, with {:.2})",
                c.correlation, d.correlation
            ));
            continue;
        };
        // Two packets: one target-delay bucket (20 ms) plus one packet of
        // envelope/time-stretch alignment.
        if dd > dc + 2 * PACKET_MS {
            failures.push(format!(
                "talkspurt {at} ms: delay {dd} ms with DTX vs {dc} ms without"
            ));
        }
        excess.push(dd as i64 - dc as i64);
        // At most one extra concealed packet per talkspurt.
        if d.conceal_ms > c.conceal_ms + PACKET_MS {
            failures.push(format!(
                "talkspurt {at} ms: {} ms concealed with DTX vs {} ms without",
                d.conceal_ms, c.conceal_ms
            ));
        }
        // 0.1 of the first 200 ms is one 20 ms packet.
        if d.onset_ratio < c.onset_ratio - 0.1 {
            failures.push(format!(
                "talkspurt {at} ms: first-200 ms energy {:.2} with DTX vs {:.2} without",
                d.onset_ratio, c.onset_ratio
            ));
        }
    }
    // The extra delay must not creep up talkspurt by talkspurt: the last
    // third may exceed the first third by at most one packet.
    if excess.len() >= 3 {
        let third = excess.len() / 3;
        let mean = |s: &[i64]| s.iter().sum::<i64>() as f64 / s.len() as f64;
        let first = mean(&excess[..third]);
        let last = mean(&excess[excess.len() - third..]);
        if last - first > PACKET_MS as f64 {
            failures.push(format!(
                "extra delay grows from {first:.0} ms (first third) to {last:.0} ms (last third)"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{name}: speech worse with DTX:\n{}",
        failures.join("\n")
    );
}

#[test]
fn dtx_webrtc_vs_no_dtx_clean_network_speech_not_worse() {
    dtx_speech_not_worse(SenderMode::DtxWebrtc, Network::clean());
}

#[test]
fn dtx_webrtc_vs_no_dtx_jitter60_speech_not_worse() {
    dtx_speech_not_worse(SenderMode::DtxWebrtc, Network::jitter(60));
}

#[test]
fn dtx_dropall_vs_no_dtx_clean_network_speech_not_worse() {
    dtx_speech_not_worse(SenderMode::DtxDropAll, Network::clean());
}

#[test]
fn dtx_dropall_vs_no_dtx_jitter60_speech_not_worse() {
    dtx_speech_not_worse(SenderMode::DtxDropAll, Network::jitter(60));
}

#[test]
fn same_scenario_twice_gives_byte_identical_neteq_wav() {
    let scenario = Scenario::natural(SenderMode::DtxWebrtc, Timestamps::Rtp, Network::jitter(60));
    let first = run(&scenario.clone().named("determinism_first"));
    // The second run on another thread: thread-local clocks must not matter.
    let second = std::thread::spawn(move || run(&scenario.named("determinism_second")))
        .join()
        .expect("second run");
    let a = std::fs::read(first.out_dir.join("neteq.wav")).expect("first neteq.wav");
    let b = std::fs::read(second.out_dir.join("neteq.wav")).expect("second neteq.wav");
    let first_diff = first
        .out
        .iter()
        .zip(&second.out)
        .position(|(x, y)| x.to_bits() != y.to_bits());
    assert!(
        a == b && first_diff.is_none(),
        "two runs differ: wav {} vs {} bytes, first differing sample {:?} (tick {:?})",
        a.len(),
        b.len(),
        first_diff,
        first_diff.map(|i| i / TICK)
    );
}
