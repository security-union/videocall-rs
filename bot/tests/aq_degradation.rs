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

//! End-to-end integration test for the bot adaptive-quality loop.
//!
//! Scenario: two in-process `BotAq` instances share a deterministic
//! [`TestClock`]. There is no relay, no network, and no transport — the
//! test directly wires bot B's DiagnosticsPackets (reporting low FPS, i.e.
//! an impaired receiver) into bot A's `process_diagnostics` input. The
//! clock is advanced manually so the whole test runs in well under a
//! second of wall time while still exercising the full PID / tier /
//! ceiling state machine.
//!
//! Pass criteria: bot A's video tier index must increase (lower quality)
//! and its next encoder snapshot must report a lower bitrate than the
//! pre-impairment snapshot. That proves the loop closes end to end:
//!
//! impaired peer → DiagnosticsPacket → PID → AdaptiveQualityManager
//! → tier step-down → encoder settings visible via `snapshot_video()`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use videocall_aq::{Clock, TestClock};
use videocall_types::protos::diagnostics_packet::{DiagnosticsPacket, VideoMetrics};
use videocall_types::protos::media_packet::media_packet::MediaType;

// The `bot` binary exposes `BotAq` as part of the crate; we pull it in via
// the binary's implicit library target so tests can construct one.
// `bot/src/main.rs` has no `lib.rs`, so we reach in through the module path
// used by `cargo test -p bot` which compiles the crate as a test harness.
//
// Using a path-based dep keeps this test decoupled from any re-export
// decisions downstream.
use bot::aq_controller::BotAq;

fn low_fps_packet(target_id: &str, fps: f32, timestamp_ms: u64) -> DiagnosticsPacket {
    let mut packet = DiagnosticsPacket::new();
    packet.sender_id = "bot-b-impaired".to_string();
    packet.target_id = target_id.to_string();
    packet.timestamp_ms = timestamp_ms;
    packet.media_type = MediaType::VIDEO.into();
    let mut video_metrics = VideoMetrics::new();
    video_metrics.fps_received = fps;
    video_metrics.bitrate_kbps = 500;
    packet.video_metrics = ::protobuf::MessageField::some(video_metrics);
    packet
}

/// Verify the end-to-end AQ reaction: given a steady stream of peer
/// diagnostics reporting low FPS (well below the degrade threshold), the
/// healthy bot's tier index must increase and its reported video bitrate
/// must drop within the simulated test window.
#[test]
fn bot_a_degrades_when_bot_b_reports_low_fps() {
    // Shared TestClock across both bots. We advance it manually; the whole
    // test must fit inside a 20s wall-clock budget.
    let wall_start = Instant::now();
    let clock = Arc::new(TestClock::new(0));

    // "Healthy" bot whose adaptive quality we're observing.
    let bot_a = BotAq::new(clock.clone() as Arc<dyn videocall_aq::Clock>);
    // "Impaired" bot acting as the peer reporting quality. The test does
    // not feed diagnostics into bot_b — it simply shows that we can
    // construct a second bot on the shared clock without any interference.
    let _bot_b = BotAq::new(clock.clone() as Arc<dyn videocall_aq::Clock>);

    // Sanity: initial snapshot matches the default (medium) tier.
    let initial_snapshot = bot_a.snapshot_video();
    let initial_index = bot_a.video_tier_index();
    let initial_epoch = bot_a.tier_epoch();
    assert_eq!(
        initial_epoch, 0,
        "fresh BotAq must start with tier_epoch = 0"
    );

    // Phase 1: advance clock past the AQ manager warmup (5000ms) plus a
    // small margin so the very first packet is past warmup. We also push
    // three "good" packets first so `initialization_complete` flips — the
    // PID won't issue corrections until it has >= 3 samples.
    clock.advance_ms(5_100);

    // Three good packets spaced 1100ms apart to pass the PID throttle.
    // Target FPS for the default medium tier is 25; we report 25 = healthy.
    for _ in 0..3 {
        let packet = low_fps_packet("bot-a", 25.0, clock.now_ms() as u64);
        bot_a.process_diagnostics(packet);
        clock.advance_ms(1_100);
    }

    // Phase 2: impaired reports. FPS = 2.0 is well below the lenient
    // degrade threshold (0.30 * 25 = 7.5 fps). Single-peer p75 collapses
    // to min, so this drives fps_ratio to 2 / 25 = 0.08, comfortably
    // under the lenient bar.
    //
    // STEP_DOWN_REACTION_TIME_MS = 1500ms must elapse with sustained
    // degradation, and MIN_TIER_TRANSITION_INTERVAL_MS = 3000ms since the
    // last transition (the manager's last_transition_time_ms is seeded to
    // its creation time, so we need at least ~3s of simulated elapsed
    // time past the warmup — the 5.1s advance above handles that).
    //
    // Feed packets every 1100ms for up to 8 seconds of simulated time.
    // That's ~7 packets, more than enough to cross both the reaction-time
    // bar and the min-transition-interval bar.
    for _ in 0..10 {
        let packet = low_fps_packet("bot-a", 2.0, clock.now_ms() as u64);
        bot_a.process_diagnostics(packet);
        if bot_a.video_tier_index() > initial_index {
            break;
        }
        clock.advance_ms(1_100);
    }

    let final_index = bot_a.video_tier_index();
    let final_snapshot = bot_a.snapshot_video();
    let final_epoch = bot_a.tier_epoch();

    eprintln!(
        "aq_degradation: initial tier_index={} bitrate_kbps={}  =>  \
         final tier_index={} bitrate_kbps={} (epoch {} -> {}, wall_elapsed={:?})",
        initial_index,
        initial_snapshot.bitrate_kbps,
        final_index,
        final_snapshot.bitrate_kbps,
        initial_epoch,
        final_epoch,
        wall_start.elapsed(),
    );

    assert!(
        final_index > initial_index,
        "bot A's AQ should have stepped DOWN at least one tier in response \
         to impaired peer reports (initial={}, final={})",
        initial_index,
        final_index,
    );
    assert!(
        final_snapshot.bitrate_kbps < initial_snapshot.bitrate_kbps,
        "encoder bitrate snapshot should drop along with the tier \
         (initial={} kbps, final={} kbps)",
        initial_snapshot.bitrate_kbps,
        final_snapshot.bitrate_kbps,
    );
    assert!(
        final_epoch > initial_epoch,
        "tier_epoch should have incremented (initial={}, final={})",
        initial_epoch,
        final_epoch,
    );

    // Belt-and-braces wall-clock budget: must be well under 20 seconds
    // since we use TestClock. Failing this hints that the hot path
    // accidentally touched a real Instant/SystemTime.
    assert!(
        wall_start.elapsed() < Duration::from_secs(20),
        "test took too long in wall time: {:?}",
        wall_start.elapsed(),
    );
}
