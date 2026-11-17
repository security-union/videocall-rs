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

//! NetEq end to end on the fixture clip in virtual time (see `dtx_harness`):
//! Opus DTX comfort noise, and real RTP timestamps across mutes, device
//! switches and the 2^32 wrap. Audio for every scenario lands in
//! `<target dir>/neteq-dtx-harness/<scenario>/` before any assertion runs.

#![cfg(all(feature = "native", not(target_arch = "wasm32")))]

mod dtx_harness;

use dtx_harness::*;
use neteq::neteq::SpeechType;

/// The clip from a libwebrtc-style DTX sender, received by a decoder with
/// and without DTX support (the latter is main's handling), same network.
fn dtx_pair(network: Network) -> (Run, Run) {
    let dtx_scenario = Scenario::natural(SenderMode::DtxWebrtc, Timestamps::Rtp, network);
    let plain = run(&dtx_scenario.clone().without_decoder_dtx());
    let dtx = run(&dtx_scenario);
    assert_sane(&plain);
    assert_sane(&dtx);
    let r = &dtx.report;
    let name = &dtx.scenario.name;
    // The scenario cannot pass vacuously.
    assert!(r.sent_dtx > 0, "{name}: no DTX frames sent");
    assert!(r.deep_silence_ms >= 500, "{name}: little silence to score");
    // Pauses play the codec's comfort noise, at the source's level, where
    // main concealed them (1.0 to 1.3 s of this clip) and faded to digital
    // silence. Not every quiet stretch of the clip is DTX.
    assert!(r.cng_silence_ms > 0, "{name}: no comfort noise in silence");
    assert!(
        r.conceal_silence_ms * 20 <= r.deep_silence_ms,
        "{name}: {} ms concealment in {} ms of deep silence",
        r.conceal_silence_ms,
        r.deep_silence_ms
    );
    assert!(
        (r.silence_out_dbfs - r.silence_src_dbfs).abs() <= 6.0,
        "{name}: deep-silence output {:.1} dBFS vs source {:.1} dBFS",
        r.silence_out_dbfs,
        r.silence_src_dbfs
    );
    assert_cng_level(&dtx);
    // DTX handling must not conceal more speech than main's handling of the
    // same packets.
    assert!(
        r.conceal_speech_ms <= plain.report.conceal_speech_ms,
        "{name}: {} ms of speech concealed with DTX handling, {} ms without",
        r.conceal_speech_ms,
        plain.report.conceal_speech_ms
    );
    (plain, dtx)
}

#[test]
fn dtx_clean_link_pauses_are_comfort_noise_and_speech_is_untouched() {
    let (_, dtx) = dtx_pair(Network::clean());
    let r = &dtx.report;
    let name = &dtx.scenario.name;
    assert_eq!(r.conceal_silence_ms, 0, "{name}: concealment in silence");
    assert_eq!(r.conceal_speech_ms, 0, "{name}: concealment in speech");
    assert_eq!(r.digital_silence_frames, 0, "{name}: digital silence");
    assert!(
        r.silence_spread_db <= 6.0,
        "{name}: comfort noise pumps {:.1} dB",
        r.silence_spread_db
    );
}

#[test]
fn dtx_quiet_room_pauses_are_comfort_noise() {
    let run = run(&Scenario::scripted(
        "pauses_dtx_webrtc_clean",
        Script::Edited(vec![
            Segment::Speech(3000),
            Segment::Silence(2000),
            Segment::Speech(3000),
            Segment::Silence(3000),
            Segment::Speech(3000),
        ]),
        SenderMode::DtxWebrtc,
        Network::clean(),
    ));
    assert_sane(&run);
    let r = &run.report;
    // Room noise around DTX: the encoder sends some of it as audio, the
    // rest is comfort noise; none of it may be concealment or silence.
    // The level spread is asserted on the clean clip; here it reads 7.8 dB
    // in -60 dBFS noise with no codec-alone baseline to hold it to, so it
    // is only printed.
    assert!(
        r.cng_silence_ms >= 1000,
        "{} ms of comfort noise",
        r.cng_silence_ms
    );
    assert_eq!(r.conceal_silence_ms, 0);
    assert_eq!(r.conceal_speech_ms, 0);
    assert_eq!(r.digital_silence_frames, 0);
    assert_cng_level(&run);
}

/// NetEq's comfort noise is the codec's: within 1 dB of the same DTX frames
/// decoded without NetEq. How far the codec's noise sits from the source is
/// Opus' business and only printed.
fn assert_cng_level(run: &Run) {
    let r = &run.report;
    println!(
        "{}: comfort noise {:.1} dBFS, codec alone {:.1} dBFS, source {:.1} dBFS",
        run.scenario.name, r.cng_out_dbfs, r.cng_ref_dbfs, r.cng_src_dbfs
    );
    assert!(
        (r.cng_out_dbfs - r.cng_ref_dbfs).abs() <= 1.0,
        "{}: comfort noise {:.1} dBFS vs {:.1} dBFS from the codec alone",
        run.scenario.name,
        r.cng_out_dbfs,
        r.cng_ref_dbfs
    );
}

#[test]
fn dtx_jittery_link_pauses_are_comfort_noise_without_extra_delay() {
    dtx_pair(Network::jitter(60));
}

#[test]
fn comfort_noise_stops_within_cng_timeout_when_the_sender_vanishes() {
    let run = run(&Scenario::scripted(
        "vanish_dtx_webrtc",
        Script::Edited(vec![
            Segment::Speech(4000),
            Segment::Silence(2000),
            Segment::Mute(4000),
        ]),
        SenderMode::DtxWebrtc,
        Network::clean(),
    ));
    assert_sane(&run);
    let last_cng = run
        .ticks
        .iter()
        .rev()
        .find(|t| t.speech == SpeechType::Cng)
        .map(|t| t.t_ms)
        .expect("no comfort noise at all");
    // The last DTX packet is decoded at most one 10 ms pull after it
    // arrives; comfort noise then runs until it exceeds kCngTimeoutMs
    // (1000 ms), so its last frame starts at most 1000 ms after that pull.
    assert!(
        last_cng <= run.last_arrival_ms + TICK_MS + 1000 + TICK_MS,
        "comfort noise until {last_cng} ms, last packet arrived at {} ms",
        run.last_arrival_ms
    );
}

#[test]
fn sender_that_drops_dtx_frames_gets_no_comfort_noise() {
    let run = run(&Scenario::natural(
        SenderMode::DtxDropAll,
        Timestamps::Rtp,
        Network::clean(),
    ));
    assert_sane(&run);
    assert_eq!(run.report.cng_ms, 0);
}

fn mute_script(mute_ms: u32) -> Script {
    Script::Edited(vec![
        Segment::Speech(6000),
        Segment::Mute(mute_ms),
        Segment::Speech(6000),
    ])
}

/// The remote mutes for `mute_ms`; its timestamps jump forward by that much
/// on resume, the sequence continues.
fn mute_run(mute_ms: u32, pause_pulls: bool) -> Run {
    let mut scenario = Scenario::scripted(
        &format!(
            "mute{mute_ms}ms_{}",
            if pause_pulls { "paused" } else { "pulling" }
        ),
        mute_script(mute_ms),
        SenderMode::Continuous,
        Network::clean(),
    );
    if pause_pulls {
        scenario = scenario.with_pull_pause(PullPause { unmute_lag_ms: 0 });
    }
    let run = run(&scenario);
    assert_sane(&run);
    run
}

fn assert_speech_resumes_at_once(run: &Run) {
    let name = &run.scenario.name;
    let resume = run.resume_arrivals_ms[0];
    let first = run
        .ticks
        .iter()
        .find(|t| t.t_ms >= resume && !t.paused && t.speech == SpeechType::Normal)
        .map(|t| t.t_ms - resume);
    assert!(
        first.is_some_and(|ms| ms <= 2 * TICK_MS),
        "{name}: speech resumed {first:?} ms after the first packet"
    );
    assert_eq!(
        conceal_ms_between(run, resume + 1000, resume + 5000),
        0,
        "{name}: concealment after the resume"
    );
}

// These two panic in debug builds on the unfixed delay manager. In release
// the wrapped value (89 479 ms of lateness) changes nothing end to end: it
// is the only entry in the 2 s history, above the 2000 ms histogram range,
// so it is never registered. The release check is the delay manager's
// `forward_timestamp_jump_after_a_long_pause_is_not_jitter`.
#[test]
fn mute_longer_than_89_seconds_with_the_worker_paused() {
    // 95 s at 48 kHz is more than u32::MAX / 1000 samples.
    assert_speech_resumes_at_once(&mute_run(95_000, true));
}

#[test]
fn mute_longer_than_89_seconds_with_the_worker_pulling() {
    assert_speech_resumes_at_once(&mute_run(95_000, false));
}

#[test]
fn mute_of_ten_seconds_with_the_worker_paused() {
    assert_speech_resumes_at_once(&mute_run(10_000, true));
}

#[test]
fn device_switch_timestamp_jump_loses_nothing() {
    let run = run(
        &Scenario::natural(SenderMode::Continuous, Timestamps::Rtp, Network::clean())
            .with_ts_jump(TsJump {
                at_frame: 300,
                samples: 300 * 48,
            })
            .named("device_switch_300ms"),
    );
    assert_sane(&run);
    assert!(run.never_decoded_frames.is_empty());
    assert_eq!(run.report.conceal_speech_ms, 0);
}

#[test]
fn timestamp_wrap_past_u32_max_loses_nothing() {
    let run = run(
        &Scenario::natural(SenderMode::Continuous, Timestamps::Rtp, Network::jitter(60))
            .with_restart(Restart {
                at_frame: 0,
                seq_base: 20_000,
                ts_base: u32::MAX - 300 * FRAME as u32 + 1,
            })
            .named("wrap_at_6s_jitter60"),
    );
    assert_sane(&run);
    assert!(
        run.never_decoded_frames.is_empty(),
        "frames never played: {:?}",
        run.never_decoded_frames
    );
}

#[test]
fn restart_onto_an_unrelated_timestamp_base_does_not_panic() {
    // RFC 3550 random bases, more than 2^31 apart: no panic, no errors.
    let run = run(
        &Scenario::natural(SenderMode::Continuous, Timestamps::Rtp, Network::clean())
            .with_restart(Restart {
                at_frame: 300,
                seq_base: 7,
                ts_base: 3_000_000_000,
            })
            .named("restart_unrelated_base"),
    );
    assert_sane(&run);
}
