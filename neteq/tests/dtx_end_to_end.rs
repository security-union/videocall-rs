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

fn jitter_target_covers_jitter(network: Network, late_ms: u64) {
    let run = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        network,
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    // To ride out packets up to `late_ms` late the buffer must hold that much.
    let target = median_target(&run, CONVERGED_MS, run.source.duration_ms());
    assert!(
        u64::from(target) >= late_ms,
        "{name}: median target delay {target} ms after convergence, packets are up to {late_ms} ms late"
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
    jitter_target_covers_jitter(Network::jitter(60), 60);
}

/// Reordering: one packet in ten is 30 ms late, so it arrives after its
/// successor.
#[test]
fn no_dtx_reordering_target_delay_covers_the_reordered_packets() {
    jitter_target_covers_jitter(Network::straggler(10, 30), 30);
}

/// Bursty delivery: every 5th packet is held 100 ms.
#[test]
fn no_dtx_every_fifth_packet_held_100ms_target_delay_covers_it() {
    jitter_target_covers_jitter(Network::straggler(5, 100), 100);
}

/// Clean-network delay bound: network + start target + one packet + one
/// output tick + one envelope tick.
fn clean_delay_bound(run: &Run) -> u64 {
    let (_, net_max) = run.scenario.network.delay_range_ms();
    net_max + START_TARGET_MS + PACKET_MS + TICK_MS + TICK_MS
}

#[test]
fn no_dtx_loss5_conceals_only_what_was_lost() {
    let run = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        Network::clean().with_loss(5),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    let r = &run.report;
    assert!(r.lost > 0, "{name}: nothing was lost");
    // Each lost packet costs at most its own length plus the merge back:
    // twice the loss rate.
    assert!(
        r.conceal_speech_ms * 100 <= r.deep_speech_ms * 2 * 5,
        "{name}: {} ms concealed in {} ms of deep speech (> 10% for 5% loss)",
        r.conceal_speech_ms,
        r.deep_speech_ms
    );
    // Loss is not delay: lost packets never arrive, so they must not raise
    // the target above the start target.
    let target = median_target(&run, CONVERGED_MS, run.source.duration_ms());
    assert!(
        u64::from(target) <= START_TARGET_MS,
        "{name}: median target {target} ms on a lossy but on-time network"
    );
    let bound = clean_delay_bound(&run);
    for t in scored_talkspurts(&run) {
        let d = t.delay_ms.unwrap_or_else(|| {
            panic!(
                "{name}: talkspurt at {} ms not found (correlation {:.2})",
                t.start_ms, t.correlation
            )
        });
        assert!(
            d <= bound,
            "{name}: talkspurt at {} ms delay {d} ms > {bound} ms (loss must not add delay)",
            t.start_ms
        );
    }
}

/// A duplicate carries no new information: the output must be
/// sample-identical to the same network without the duplicates.
#[test]
fn no_dtx_duplicates_change_nothing() {
    let network = Network::jitter(60);
    let without = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        network,
    ));
    let with = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Rtp,
        network.with_duplicates(10),
    ));
    assert_sane(&without);
    assert_sane(&with);
    let first_diff = without
        .out
        .iter()
        .zip(&with.out)
        .position(|(x, y)| x.to_bits() != y.to_bits());
    assert!(
        first_diff.is_none(),
        "{}: output differs from the run without duplicates from sample {:?} (tick {:?})",
        with.scenario.name,
        first_diff,
        first_diff.map(|i| i / TICK)
    );
}

/// NetEq treats packets that share an RTP timestamp as duplicates (the
/// browser client used to send every packet with `u32::MAX`): only the
/// first one with a given timestamp is played.
#[test]
fn packets_sharing_a_timestamp_are_duplicates() {
    let run = run(&Scenario::natural(
        SenderMode::Continuous,
        Timestamps::Saturated,
        Network::clean(),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    // One packet is 20 ms; allow one more for frames around it.
    assert!(
        run.report.normal_ms <= 2 * PACKET_MS,
        "{name}: {} ms of decoded audio from {} packets that all share one timestamp",
        run.report.normal_ms,
        run.report.sent
    );
}

/// A sender restart starts a new stream: the restart may cost at most what
/// a fresh start costs (the start target) plus one packet, after which
/// speech plays on time again.
fn restart_is_a_new_stream(restart: Restart, label: &str) {
    let run = run(
        &Scenario::natural(SenderMode::Continuous, Timestamps::Rtp, Network::clean())
            .with_restart(restart)
            .named(&format!("restart_{label}_continuous_rtp_clean")),
    );
    assert_sane(&run);
    let name = &run.scenario.name;
    let at_ms = restart.at_frame as u64 * FRAME_MS;
    let end_ms = run.source.duration_ms();
    let around = conceal_ms_between(&run, at_ms, at_ms + 2000);
    assert!(
        around <= START_TARGET_MS + PACKET_MS,
        "{name}: {around} ms concealed within 2 s of the restart at {at_ms} ms"
    );
    let after = conceal_ms_between(&run, at_ms + 2000, end_ms);
    assert_eq!(
        after, 0,
        "{name}: {after} ms concealed on a clean network more than 2 s after the restart"
    );
    let target = median_target(&run, at_ms + 2000, end_ms);
    assert!(
        u64::from(target) <= START_TARGET_MS,
        "{name}: median target {target} ms after the restart on a clean network"
    );
    let bound = clean_delay_bound(&run);
    let spurts: Vec<_> = scored_talkspurts(&run)
        .into_iter()
        .filter(|t| t.start_ms >= at_ms + 1000)
        .collect();
    assert!(
        !spurts.is_empty(),
        "{name}: no talkspurts after the restart"
    );
    for t in spurts {
        let d = t.delay_ms.unwrap_or_else(|| {
            panic!(
                "{name}: talkspurt at {} ms not found after the restart (correlation {:.2})",
                t.start_ms, t.correlation
            )
        });
        assert!(
            d <= bound,
            "{name}: talkspurt at {} ms delay {d} ms > {bound} ms after the restart",
            t.start_ms
        );
    }
}

/// Restart at 10 s; RFC 3550 bases are random, so pick ones well away from
/// the old stream: 7000 packets and 10 minutes of RTP time.
const RESTART_FRAME: usize = 500;

#[test]
fn sender_restart_with_lower_bases_is_a_new_stream() {
    restart_is_a_new_stream(
        Restart {
            at_frame: RESTART_FRAME,
            seq_base: 20_000u16
                .wrapping_add(RESTART_FRAME as u16)
                .wrapping_sub(7_000),
            ts_base: 100_000_000u32
                .wrapping_add((RESTART_FRAME * FRAME) as u32)
                .wrapping_sub(48_000 * 600),
        },
        "lower",
    );
}

#[test]
fn sender_restart_with_higher_bases_is_a_new_stream() {
    restart_is_a_new_stream(
        Restart {
            at_frame: RESTART_FRAME,
            seq_base: 20_000u16
                .wrapping_add(RESTART_FRAME as u16)
                .wrapping_add(7_000),
            ts_base: 100_000_000u32
                .wrapping_add((RESTART_FRAME * FRAME) as u32)
                .wrapping_add(48_000 * 600),
        },
        "higher",
    );
}

/// A 95 s mute (the sender sends nothing, its timestamps skip the gap) and
/// then speech: the speech after the mute plays on schedule, as on a fresh
/// clean link, with nothing concealed in it.
#[test]
fn mute_95s_then_speech_plays_on_time() {
    let script = Script::Edited(vec![
        Segment::Speech(6_000),
        Segment::Mute(95_000),
        Segment::Speech(8_000),
    ]);
    let run = run(&Scenario::scripted(
        "mute95s_continuous_rtp_clean",
        script,
        SenderMode::Continuous,
        Network::clean(),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    let resume_ms = 101_000;
    let bound = clean_delay_bound(&run);
    let spurts: Vec<_> = scored_talkspurts(&run)
        .into_iter()
        .filter(|t| t.start_ms >= resume_ms)
        .collect();
    assert!(!spurts.is_empty(), "{name}: no talkspurts after the mute");
    for t in spurts {
        let d = t.delay_ms.unwrap_or_else(|| {
            panic!(
                "{name}: talkspurt at {} ms not found after the mute (correlation {:.2})",
                t.start_ms, t.correlation
            )
        });
        assert!(
            d <= bound,
            "{name}: talkspurt at {} ms delay {d} ms > {bound} ms after the mute",
            t.start_ms
        );
        assert_eq!(
            t.conceal_ms, 0,
            "{name}: talkspurt at {} ms has {} ms concealed on a clean network",
            t.start_ms, t.conceal_ms
        );
        assert!(
            (0.8..=1.25).contains(&t.energy_ratio),
            "{name}: talkspurt at {} ms delivered energy ratio {:.2}, want 0.8..1.25",
            t.start_ms,
            t.energy_ratio
        );
    }
}

/// The browser worker's mute path: the remote peer turns its mic off for
/// 10 s (its sender sends nothing and its timestamps skip the gap). The
/// client flushes and mutes the worker, which keeps pulling every 10 ms and
/// discards the audio. Speech resumes with a forward timestamp jump and the
/// un-mute message arrives 60 ms after the first audio packet.
fn muted_worker_run(name: &str, mute: ReceiverMute) -> Run {
    let script = Script::Edited(vec![
        Segment::Speech(6_000),
        Segment::Mute(10_000),
        Segment::Speech(8_000),
    ]);
    run(
        &Scenario::scripted(name, script, SenderMode::Continuous, Network::clean())
            .with_receiver_mute(mute),
    )
}

/// The first talkspurt after the mute must not lose its onset, must play
/// on schedule, and the muted pulls must not count as concealment.
fn assert_speech_after_mute_is_whole(run: &Run) {
    assert_sane(run);
    let name = &run.scenario.name;
    assert_eq!(
        run.muted_concealment, 0,
        "{name}: {} samples counted as concealed while muted",
        run.muted_concealment
    );
    assert!(run.unmute_signal_ms.is_some(), "{name}: never un-muted");
    // The un-mute message is late, but the worker does not wait for it:
    // the clean-network bound applies, with no allowance for the lag.
    let bound = clean_delay_bound(run);
    let spurts: Vec<_> = scored_talkspurts(run)
        .into_iter()
        .filter(|t| t.start_ms >= 16_000)
        .collect();
    assert!(!spurts.is_empty(), "{name}: no talkspurts after the mute");
    for t in spurts {
        let d = t.delay_ms.unwrap_or_else(|| {
            panic!(
                "{name}: talkspurt at {} ms not found after the mute (correlation {:.2})",
                t.start_ms, t.correlation
            )
        });
        assert!(
            d <= bound,
            "{name}: talkspurt at {} ms delay {d} ms > {bound} ms after the mute",
            t.start_ms
        );
        assert_eq!(
            t.conceal_ms, 0,
            "{name}: talkspurt at {} ms has {} ms concealed on a clean network",
            t.start_ms, t.conceal_ms
        );
        for (what, ratio) in [("energy", t.energy_ratio), ("onset", t.onset_ratio)] {
            assert!(
                (0.8..=1.25).contains(&ratio),
                "{name}: talkspurt at {} ms {what} ratio {ratio:.2}, want 0.8..1.25",
                t.start_ms
            );
        }
    }
}

/// The mute heartbeat lands 200 ms after the mic went off: everything sent
/// before it has played, so the flush drops nothing.
#[test]
fn muted_worker_keeps_pulling_and_speech_after_unmute_is_whole() {
    let run = muted_worker_run(
        "mute10s_worker_muted_pulls_unmute_late60_continuous_rtp_clean",
        ReceiverMute {
            from_ms: 6_000 + 200,
            unmute_lag_ms: 60,
            stale_mute_at_ms: None,
        },
    );
    assert_speech_after_mute_is_whole(&run);
}

/// The mute lands while the last packets before the mic went off are still
/// buffered: the flush drops them and a straggler arrives after it. The
/// decoder must still see every packet (see `NetEq::flush`), or the onset
/// 10 s later plays at 2.5 times its level.
#[test]
fn mute_landing_on_buffered_audio_leaves_the_next_onset_at_its_level() {
    let run = muted_worker_run(
        "mute10s_worker_flush_on_buffered_audio_straggler_continuous_rtp_clean",
        ReceiverMute {
            from_ms: 6_000,
            unmute_lag_ms: 60,
            stale_mute_at_ms: None,
        },
    );
    assert_speech_after_mute_is_whole(&run);
}

/// A stale Mute{true} reaches the worker after speech has resumed. The
/// worker discards pulls until the next packet un-mutes it, so the audio
/// lost is at most one packet interval (20 ms) on a clean network, and
/// none of it counts as concealment.
#[test]
fn stale_mute_after_speech_resumed_loses_at_most_one_packet() {
    // Between two packet arrivals (they land on 20 ms boundaries), so at
    // least one pull is discarded before the next packet un-mutes.
    let stale_at = 16_510;
    let run = muted_worker_run(
        "mute10s_worker_stale_mute_at_16510_continuous_rtp_clean",
        ReceiverMute {
            from_ms: 6_000 + 200,
            unmute_lag_ms: 60,
            stale_mute_at_ms: Some(stale_at),
        },
    );
    let name = &run.scenario.name;
    let lost_ms = run
        .ticks
        .iter()
        .filter(|t| t.paused && t.t_ms >= stale_at)
        .count() as u64
        * TICK_MS;
    assert!(lost_ms > 0, "{name}: the stale mute never took effect");
    assert!(
        lost_ms <= PACKET_MS,
        "{name}: {lost_ms} ms discarded after the stale mute, want <= {PACKET_MS} ms"
    );
    assert_speech_after_mute_is_whole(&run);
}

/// Three loops of the clip (60 s) over 60 ms jitter: the buffer and the
/// target delay must not drift.
#[test]
fn long_run_buffer_and_delay_do_not_drift() {
    let run = run(&Scenario::scripted(
        "loop3_continuous_rtp_jitter60",
        Script::Edited(vec![Segment::Speech(60_000)]),
        SenderMode::Continuous,
        Network::jitter(60),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    // One delay-histogram bucket (20 ms) is the target's own resolution.
    let bucket = 20;
    let (t_first, t_last) = (
        median_target(&run, 10_000, 20_000),
        median_target(&run, 50_000, 60_000),
    );
    assert!(
        t_last <= t_first + bucket,
        "{name}: median target {t_first} ms in 10..20 s, {t_last} ms in 50..60 s"
    );
    let (b_first, b_last) = (
        median_buffer(&run, 10_000, 20_000),
        median_buffer(&run, 50_000, 60_000),
    );
    assert!(
        b_last <= b_first + bucket,
        "{name}: median buffer {b_first} ms in 10..20 s, {b_last} ms in 50..60 s"
    );
    let delays: Vec<u64> = scored_talkspurts(&run)
        .iter()
        .filter_map(|t| t.delay_ms)
        .collect();
    assert!(
        delays.len() >= 6,
        "{name}: only {} talkspurts found",
        delays.len()
    );
    let third = delays.len() / 3;
    let mean = |s: &[u64]| s.iter().sum::<u64>() as f64 / s.len() as f64;
    let (first, last) = (
        mean(&delays[..third]),
        mean(&delays[delays.len() - third..]),
    );
    assert!(
        last <= first + PACKET_MS as f64,
        "{name}: talkspurt delay grows from {first:.0} ms (first third) to {last:.0} ms (last third)"
    );
    let r = &run.report;
    assert!(
        r.conceal_speech_ms * 100 <= r.deep_speech_ms * 6,
        "{name}: {} ms concealed in {} ms of deep speech (> 6%)",
        r.conceal_speech_ms,
        r.deep_speech_ms
    );
}

/// Hostile payloads through `insert_packet` never panic, never produce a
/// short or non-finite frame, and do not wedge NetEq: once valid packets
/// return, they play.
#[test]
fn hostile_payloads_never_panic_and_do_not_wedge_neteq() {
    use neteq::codec::NativeOpusDecoder;
    use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};
    let valid = encoded(&Script::Natural, false);
    let mut rng = 0x1234_5678u32;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };
    let fixed: [&[u8]; 5] = [
        &[],
        &[0x08],
        &[0x08, 0x00],
        &[0x1B, 0x3F],
        &[0xFF, 0xFF, 0xFF],
    ];
    neteq::clock::freeze();
    let mut neteq = NetEq::new(NetEqConfig {
        sample_rate: RATE,
        channels: 1,
        ..NetEqConfig::default()
    })
    .expect("neteq");
    neteq.register_decoder(
        PT,
        Box::new(NativeOpusDecoder::new(RATE, 1).expect("decoder")),
    );
    // 0.5 s valid, 2 s hostile, 2 s valid again.
    let (hostile_from, hostile_to, frames) = (25usize, 125usize, 225usize);
    let mut speech = Vec::new();
    for frame in 0..frames {
        let payload: Vec<u8> = if (hostile_from..hostile_to).contains(&frame) {
            let k = frame - hostile_from;
            if k % 2 == 0 {
                fixed[(k / 2) % fixed.len()].to_vec()
            } else {
                let len = 3 + next() as usize % 400;
                (0..len).map(|_| next() as u8).collect()
            }
        } else {
            valid[frame].clone()
        };
        let header = RtpHeader::new(
            20_000u16.wrapping_add(frame as u16),
            100_000_000u32.wrapping_add((frame * FRAME) as u32),
            0x5EED_0001,
            PT,
            false,
        );
        // Errors are allowed (an empty payload is invalid); panics are not.
        let _ = neteq.insert_packet(AudioPacket::new(header, payload, RATE, 1, 20));
        for _ in 0..2 {
            neteq::clock::advance(std::time::Duration::from_millis(TICK_MS));
            let frame_out = neteq.get_audio().expect("get_audio");
            assert_eq!(frame_out.samples.len(), TICK, "short frame");
            assert!(
                frame_out.samples.iter().all(|x| x.is_finite()),
                "non-finite sample"
            );
            speech.push(frame_out.speech_type);
        }
    }
    // The last second of valid packets must play: at least 90% decoded audio.
    let last_second = &speech[speech.len() - 100..];
    let normal = last_second
        .iter()
        .filter(|s| **s == neteq::neteq::SpeechType::Normal)
        .count();
    assert!(
        normal >= 90,
        "only {normal}% of the last second after hostile payloads was decoded audio: {:?}",
        &last_second[..20]
    );
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

/// A sender that drops every DTX frame gets no special handling: with real
/// RTP timestamps the pause is concealment (fading) until the next packet,
/// and the speech after it plays on time. Speech against the no-DTX run is
/// compared in `dtx_dropall_vs_no_dtx_*`; here nothing in deep speech may
/// be concealed on a clean network.
#[test]
fn dtx_dropall_clean_network_pauses_conceal_and_speech_is_untouched() {
    let run = run(&Scenario::natural(
        SenderMode::DtxDropAll,
        Timestamps::Rtp,
        Network::clean(),
    ));
    assert_sane(&run);
    let name = &run.scenario.name;
    assert_eq!(
        run.report.sent_dtx, 0,
        "{name}: a DTX frame was transmitted"
    );
    assert!(
        run.report.sent < run.source.frames(),
        "{name}: every frame was sent, so the pauses were not exercised"
    );
    assert_eq!(
        run.report.conceal_speech_ms, 0,
        "{name}: {} ms concealed in {} ms of deep speech after DTX pauses",
        run.report.conceal_speech_ms, run.report.deep_speech_ms
    );
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

/// What NetEq hands its decoder, in order.
struct RecordingDecoder {
    inner: neteq::codec::NativeOpusDecoder,
    log: std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl neteq::codec::AudioDecoder for RecordingDecoder {
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn channels(&self) -> u8 {
        self.inner.channels()
    }
    fn decode(&mut self, encoded: &[u8]) -> neteq::Result<Vec<f32>> {
        self.log.lock().expect("decoder log").push(encoded.to_vec());
        self.inner.decode(encoded)
    }
    fn supports_dtx(&self) -> bool {
        self.inner.supports_dtx()
    }
    fn decode_comfort_noise(&mut self, n: usize, out: &mut Vec<f32>) -> neteq::Result<bool> {
        self.inner.decode_comfort_noise(n, out)
    }
}

struct GapResume {
    /// Packets sent, in order.
    sent: Vec<Vec<u8>>,
    /// Packets NetEq handed the decoder, in order.
    decoded: Vec<Vec<u8>>,
    /// Energy of the first 200 ms NetEq played after the resume, over the
    /// same audio from an ideal receiver (every sent packet decoded in
    /// order, no jitter buffer).
    onset_ratio: f64,
    /// When the resumed audio started to play, after its first packet
    /// arrived.
    resume_delay_ms: u64,
}

/// A handful of packets, 10 s of nothing while the receiver keeps pulling
/// every 10 ms, then the stream resumes with a forward timestamp jump (an
/// outage, a mute, a peer that says one word). With `flush`, NetEq played
/// the stream from its start and was flushed just before the last `k`
/// packets of the first segment arrived: whatever it held and had not
/// decoded is dropped. Without, NetEq starts fresh on those `k` packets.
/// The sender encodes only what it sends, as a muted browser mic does.
fn gap_resume(k: usize, flush: bool) -> GapResume {
    use neteq::codec::{AudioDecoder, NativeOpusDecoder};
    use neteq::{AudioPacket, NetEq, NetEqConfig, Operation, RtpHeader};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    let script = Script::Edited(vec![
        Segment::Speech(6_000),
        Segment::Mute(10_000),
        Segment::Speech(8_000),
    ]);
    let payloads = encoded(&script, false);
    let (stop, resume) = (300usize, 800usize); // frames
    let first = if flush { 0 } else { stop - k };
    let frames: Vec<usize> = (first..stop).chain(resume..resume + 50).collect();
    let arrival_ms = |frame: usize| frame as u64 * FRAME_MS + BASE_NET_DELAY_MS;
    let flush_at_ms = arrival_ms(stop - k);

    neteq::clock::freeze();
    let mut neteq = NetEq::new(NetEqConfig {
        sample_rate: RATE,
        channels: 1,
        ..NetEqConfig::default()
    })
    .expect("neteq");
    let log = Arc::new(Mutex::new(Vec::new()));
    neteq.register_decoder(
        PT,
        Box::new(RecordingDecoder {
            inner: NativeOpusDecoder::new(RATE, 1).expect("decoder"),
            log: log.clone(),
        }),
    );
    let start_ms = arrival_ms(first) / TICK_MS * TICK_MS;
    let end_ms = arrival_ms(resume + 50) + 500; // play out the last packets
    let mut out = Vec::new();
    let mut operations = Vec::new();
    let mut next = 0;
    let mut flushed = !flush;
    for t in (start_ms..end_ms).step_by(TICK_MS as usize) {
        if !flushed && t >= flush_at_ms {
            neteq.flush();
            flushed = true;
        }
        while next < frames.len() && arrival_ms(frames[next]) <= t {
            let frame = frames[next];
            let header = RtpHeader::new(
                20_000u16.wrapping_add(next as u16),
                100_000_000u32.wrapping_add((frame * FRAME) as u32),
                0x5EED_0002,
                PT,
                false,
            );
            neteq
                .insert_packet(AudioPacket::new(
                    header,
                    payloads[frame].clone(),
                    RATE,
                    1,
                    20,
                ))
                .expect("insert");
            next += 1;
        }
        neteq::clock::advance(Duration::from_millis(TICK_MS));
        let frame_out = neteq.get_audio().expect("get_audio");
        out.extend_from_slice(&frame_out.samples);
        operations.push((t, neteq.last_operation()));
    }

    let sent: Vec<Vec<u8>> = frames.iter().map(|&f| payloads[f].clone()).collect();
    let mut ideal_decoder = NativeOpusDecoder::new(RATE, 1).expect("decoder");
    let mut ideal = Vec::new();
    for &frame in &frames {
        let pcm = ideal_decoder.decode(&payloads[frame]).expect("decode");
        if frame >= resume {
            ideal.extend_from_slice(&pcm);
        }
    }
    let onset = 10 * FRAME; // 200 ms
    let (played_at, _) = *operations
        .iter()
        .find(|(t, op)| *t >= arrival_ms(resume) && *op != Operation::Expand)
        .expect("resumed audio never played");
    let from = ((played_at - start_ms) / TICK_MS) as usize * TICK;
    let energy = |x: &[f32]| x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>();
    let decoded = log.lock().expect("decoder log").clone();
    GapResume {
        sent,
        decoded,
        onset_ratio: energy(&out[from..from + onset]) / energy(&ideal[..onset]).max(1e-12),
        resume_delay_ms: played_at - arrival_ms(resume),
    }
}

/// After a fresh start or a flush, a handful of packets, 10 s of pulls and
/// a forward timestamp jump: the speech that resumes plays at its own
/// level and on time. The decoder must see every packet the sender sent,
/// in order: an Opus decoder that skips an encoded packet without being
/// told keeps the wrong inter-frame state, and through a gap with nothing
/// to decode that error lands on the next onset (C libopus 1.5 and the
/// ropus port both decode the frame after one skipped packet here at 2.6x
/// the level, and the onset 10 s later at 2.5x).
#[test]
fn speech_after_a_flush_or_fresh_start_and_a_gap_is_at_its_level() {
    let mut failures = Vec::new();
    for flush in [false, true] {
        for k in [1usize, 3, 5, 10] {
            let name = format!(
                "{} then {k} packets",
                if flush { "flush" } else { "fresh start" }
            );
            let run = gap_resume(k, flush);
            println!(
                "GAP {name}: decoded {}/{} onset {:.2} delay {} ms",
                run.decoded.len(),
                run.sent.len(),
                run.onset_ratio,
                run.resume_delay_ms
            );
            if run.decoded != run.sent {
                failures.push(format!(
                    "{name}: the decoder saw {} of the {} packets sent, or not in order",
                    run.decoded.len(),
                    run.sent.len()
                ));
            }
            if !(0.8..=1.25).contains(&run.onset_ratio) {
                failures.push(format!(
                    "{name}: onset energy ratio {:.2} after the gap, want 0.8..1.25",
                    run.onset_ratio
                ));
            }
            // The start-up target (80 ms, delay_manager.cc kStartDelayMs)
            // plus one packet.
            if run.resume_delay_ms > 80 + FRAME_MS {
                failures.push(format!(
                    "{name}: resumed audio played {} ms after it arrived",
                    run.resume_delay_ms
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
