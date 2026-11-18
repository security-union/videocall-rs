//! C++-only smoke tests. These always run: they prove the reference NetEq
//! builds, links and behaves sanely on every scenario, and that the harness
//! itself (generators, trace format, lockstep runner, diff) is deterministic.

use neteq_parity::runner::frame_count;
use neteq_parity::types::{NetEqConfig, Operation, RtpHeader, SpeechType};
use neteq_parity::{
    compare, run_diff, run_single, CNetEq, Codec, HarnessError, Implementation, Scenario, Trace,
};

const SMOKE_CODECS: [&str; 4] = ["l16-16k-mono", "l16-48k-stereo", "opus-mono", "opus-stereo"];

fn codec(label: &str) -> Codec {
    Codec::parse(label).expect("valid codec label")
}

fn smoke_duration(s: Scenario) -> u32 {
    s.default_duration_ms().min(10_000)
}

fn op_count(rec: &neteq_parity::Recording, op: Operation) -> usize {
    rec.frames.iter().filter(|f| f.operation == op).count()
}

#[test]
fn every_scenario_runs_on_cpp_for_every_smoke_codec() {
    for scenario in Scenario::ALL {
        for label in SMOKE_CODECS {
            let c = codec(label);
            let trace = scenario
                .generate(c, 7, smoke_duration(scenario))
                .expect("generate");
            let rec = run_single(&trace, Implementation::Cpp).expect("run");
            let ctx = format!("{} / {label}", scenario.name());

            assert!(rec.error.is_none(), "{ctx}: {:?}", rec.error);
            assert_eq!(rec.frames.len(), frame_count(&trace), "{ctx}");
            assert!(
                rec.frames.iter().all(|f| f.return_code == 0),
                "{ctx}: GetAudio failed"
            );
            assert!(
                rec.insert_return_codes.iter().all(|&c| c == 0),
                "{ctx}: InsertPacket failed"
            );
            let last = rec.frames.last().expect("frames");
            assert_eq!(last.sample_rate_hz as u32, c.rtp_clock_hz(), "{ctx}");
            assert_eq!(last.num_channels, usize::from(c.channels()), "{ctx}");
            for f in &rec.frames {
                assert_eq!(
                    f.samples.len(),
                    (f.sample_rate_hz / 100) as usize * f.num_channels,
                    "{ctx}: frame is not 10 ms"
                );
            }
            // Stereo L16 DTX pauses without CN (NetEq CN is mono-only), so
            // those silences are concealed by expand by design.
            let pauses_without_cn = matches!(scenario, Scenario::Dtx | Scenario::LongMixed)
                && matches!(c, Codec::L16 { channels: 2, .. });
            let expand = op_count(&rec, Operation::Expand);
            assert!(
                pauses_without_cn || expand * 3 < rec.frames.len(),
                "{ctx}: {expand} of {} frames are expand",
                rec.frames.len()
            );
            let lifetime = rec.lifetime.expect("lifetime stats");
            assert!(lifetime.total_samples_received > 0, "{ctx}");
            assert!(!rec.network_intervals.is_empty(), "{ctx}");
        }
    }
}

#[test]
fn scenarios_exercise_the_paths_they_are_named_after() {
    let run = |s: Scenario, label: &str| {
        let trace = s
            .generate(codec(label), 3, s.default_duration_ms())
            .unwrap();
        run_single(&trace, Implementation::Cpp).unwrap()
    };

    let loss = run(Scenario::BurstLoss, "l16-16k-mono");
    assert!(loss.lifetime.unwrap().concealment_events > 5);
    assert!(op_count(&loss, Operation::Merge) > 0);

    let dtx_l16 = run(Scenario::Dtx, "l16-16k-mono");
    assert!(op_count(&dtx_l16, Operation::Rfc3389Cng) > 0);
    assert!(op_count(&dtx_l16, Operation::Rfc3389CngNoPacket) > 0);
    assert!(dtx_l16
        .frames
        .iter()
        .any(|f| f.speech_type == SpeechType::Cng));

    let dtx_opus = run(Scenario::Dtx, "opus-mono");
    assert!(op_count(&dtx_opus, Operation::CodecInternalCng) > 0);

    let dups = run(Scenario::Duplicates, "l16-8k-mono");
    assert!(dups.lifetime.unwrap().packets_discarded > 0);

    let spikes = run(Scenario::GaussianSpikes, "l16-32k-mono");
    let max_target = spikes
        .frames
        .iter()
        .map(|f| f.target_delay_ms)
        .max()
        .unwrap();
    assert!(max_target > 100, "target delay never grew: {max_target}");
    assert!(
        op_count(&spikes, Operation::Accelerate) + op_count(&spikes, Operation::FastAccelerate) > 0
    );

    let clean = run(Scenario::Clean, "l16-16k-mono");
    let clean_lifetime = clean.lifetime.unwrap();
    assert_eq!(clean_lifetime.packets_discarded, 0);
    assert!(clean_lifetime.concealment_events <= 2, "{clean_lifetime:?}");
}

#[test]
fn cpp_against_itself_is_identical_on_every_scenario() {
    for scenario in Scenario::ALL {
        let label = if scenario == Scenario::LongMixed {
            "opus-stereo"
        } else {
            "l16-16k-mono"
        };
        let trace = scenario
            .generate(codec(label), 11, smoke_duration(scenario))
            .unwrap();
        let (report, _) = run_diff(&trace, Implementation::Cpp).unwrap();
        assert!(report.identical, "{}", report.summary());
        assert_eq!(report.samples.bitexact_ratio, 1.0);
        assert_eq!(report.decisions.operation_agreement, 1.0);
        assert!(report.samples.first_divergence.is_none());
        assert!(report.samples.snr_db.is_none());
    }
}

#[test]
fn diff_detects_a_perturbed_candidate() {
    let trace = Scenario::BurstLoss
        .generate(codec("l16-16k-mono"), 5, 5_000)
        .unwrap();
    let reference = run_single(&trace, Implementation::Cpp).unwrap();
    let mut candidate = reference.clone();
    candidate.implementation = "perturbed".into();
    candidate.frames[120].samples[3] = candidate.frames[120].samples[3].wrapping_add(7);
    candidate.frames[200].operation = Operation::Accelerate;
    candidate.frames[300].target_delay_ms += 20;

    let report = compare(&trace, &reference, &candidate);
    assert!(!report.identical);
    let div = report.samples.first_divergence.expect("divergence");
    assert_eq!(div.frame, 120);
    assert_eq!(div.first_sample, Some(3));
    assert_eq!(report.samples.max_abs_diff, 7);
    assert!(report.samples.snr_db.is_some());
    assert_eq!(report.decisions.first_operation_divergence, Some(200));
    assert_eq!(report.delay.target_delay_first_divergence, Some(300));
    assert_eq!(report.delay.target_delay_max_abs_diff_ms, 20);
}

#[test]
fn reference_runs_are_deterministic() {
    let trace = Scenario::LongMixed
        .generate(codec("opus-mono"), 2, 20_000)
        .unwrap();
    let a = run_single(&trace, Implementation::Cpp).unwrap();
    let b = run_single(&trace, Implementation::Cpp).unwrap();
    assert_eq!(a.frames, b.frames);
    assert_eq!(a.network_intervals, b.network_intervals);
    assert_eq!(a.insert_return_codes, b.insert_return_codes);
}

#[test]
fn generators_are_seeded_and_deterministic() {
    for scenario in Scenario::ALL {
        let c = codec("l16-8k-mono");
        let a = scenario.generate(c, 99, 4_000).unwrap();
        let b = scenario.generate(c, 99, 4_000).unwrap();
        let other = scenario.generate(c, 100, 4_000).unwrap();
        assert_eq!(a, b, "{}", scenario.name());
        assert_ne!(a.packets, other.packets, "{}", scenario.name());
    }
    let a = Scenario::Dtx
        .generate(codec("opus-stereo"), 1, 4_000)
        .unwrap();
    let b = Scenario::Dtx
        .generate(codec("opus-stereo"), 1, 4_000)
        .unwrap();
    assert_eq!(a, b, "opus encoding must be deterministic");
}

#[test]
fn trace_round_trips_with_and_without_payloads() {
    let dir = std::env::temp_dir().join(format!("neteq-parity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let trace = Scenario::Reordering
        .generate(codec("opus-mono"), 4, 3_000)
        .unwrap();

    let full = dir.join("full.json");
    trace.save(&full, true).unwrap();
    assert_eq!(Trace::load(&full).unwrap(), trace);

    let slim = dir.join("slim.json");
    trace.save(&slim, false).unwrap();
    assert!(std::fs::metadata(&slim).unwrap().len() < std::fs::metadata(&full).unwrap().len());
    assert_eq!(Trace::load(&slim).unwrap(), trace, "payloads regenerated");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cneteq_api_surface() {
    let mut neteq = CNetEq::new(&NetEqConfig::default(), 1_000).unwrap();
    assert_eq!(neteq.now_ms(), 1_000);
    neteq.register_l16(96, 16_000, 1).unwrap();
    neteq.register_cn(105, 16_000).unwrap();
    assert!(neteq.set_base_minimum_delay_ms(100));
    assert_eq!(neteq.base_minimum_delay_ms(), 100);

    let payload = vec![0u8; 640];
    for i in 0..50u16 {
        let header = RtpHeader {
            payload_type: 96,
            marker: i == 0,
            sequence_number: i,
            timestamp: u32::from(i) * 320,
            ssrc: 1,
        };
        assert_eq!(neteq.insert_packet(&header, &payload, neteq.now_ms()), 0);
        for _ in 0..2 {
            let out = neteq.get_audio();
            assert_eq!(out.return_code, 0);
            assert_eq!(out.samples.len(), 160);
            neteq.advance_time_ms(10);
        }
    }
    assert!(neteq.target_delay_ms() >= 100, "base minimum delay applies");
    assert!(neteq.filtered_current_delay_ms() >= 0);
    let current = neteq.network_statistics(false);
    let reset = neteq.network_statistics(true);
    assert_eq!(current.current_buffer_size_ms, reset.current_buffer_size_ms);
    assert_eq!(neteq.last_output_sample_rate_hz(), 16_000);
    assert!(neteq.sync_buffer_size_ms() >= 0);
    let ops = neteq.operations_and_state();
    // NetEq flushes once when the very first packet arrives.
    assert_eq!(ops.packet_buffer_flushes, 1);

    // Deprecated upstream no-op; must not disturb anything.
    neteq.insert_empty_packet(&RtpHeader {
        payload_type: 96,
        marker: false,
        sequence_number: 50,
        timestamp: 50 * 320,
        ssrc: 1,
    });
    neteq.flush();
    assert!(neteq.set_minimum_delay_ms(0));
    assert!(neteq.set_maximum_delay_ms(500));
    assert_eq!(neteq.remove_payload_type(105), 0);
    let out = neteq.get_audio();
    assert_eq!(out.operation, Operation::Expand);
}

#[test]
fn rust_candidate_builds_only_with_feature() {
    let result = Implementation::Rust.build(&NetEqConfig::default(), 0);
    if cfg!(feature = "rust-neteq") {
        // The real adapter: an empty NetEq plays out silence as Expand,
        // the same as the C++ build.
        let mut neteq = result.expect("rust neteq builds");
        let out = neteq.get_audio().expect("get_audio");
        assert_eq!(out.return_code, 0);
        assert_eq!(out.operation, Operation::Expand);
    } else {
        assert!(matches!(
            result.err(),
            Some(HarnessError::NotImplemented { .. })
        ));
    }
}
