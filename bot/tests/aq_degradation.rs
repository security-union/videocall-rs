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

//! Integration tests for the bot adaptive-quality loop (issue #1108, Stage 2).
//!
//! Stage 2 removed receiver-reported FPS from the sender AQ entirely. The bot
//! therefore no longer degrades because a *peer* reports low FPS — there is no
//! longer even an API to feed receiver diagnostics into the controller. The bot
//! adapts ONLY to its own signals:
//!
//!   * encoder-queue backpressure (synthesized in tests via
//!     `BotAq::inject_encoder_queue_depth`, then advanced with `BotAq::tick`),
//!   * a self-targeted server CONGESTION cut (`BotAq::force_congestion_cut`).
//!
//! A deterministic [`TestClock`] is advanced manually so each test runs in well
//! under its wall-clock budget while still exercising the full tier /
//! crash-ceiling state machine.

use std::sync::Arc;
use std::time::{Duration, Instant};

use videocall_aq::{Clock, TestClock};
use videocall_netsim::{Admission, Direction, NetSimShim, NetworkProfile};

// The `bot` binary exposes `BotAq` as part of the crate; we pull it in via the
// binary's implicit library target so tests can construct one.
use bot::aq_controller::BotAq;

/// Tick the AQ `count` times, advancing the shared clock `step_ms` between
/// ticks, holding the injected backpressure `depth` constant. Returns the final
/// clock time (ms).
fn tick_n(
    aq: &BotAq,
    clock: &Arc<TestClock>,
    start_ms: u64,
    count: usize,
    step_ms: u64,
    depth: u32,
) -> u64 {
    let mut t = start_ms;
    for _ in 0..count {
        clock.set_ms(t);
        aq.inject_encoder_queue_depth(depth);
        aq.tick();
        t += step_ms;
    }
    t - step_ms
}

/// Sustained HIGH encoder backpressure (the sender's OWN signal) must drive the
/// bot's video tier DOWN. This is the Stage-2 replacement for the old
/// "peer reports low FPS" degradation path.
#[test]
fn bot_degrades_on_synthetic_backpressure() {
    let wall_start = Instant::now();
    let clock = Arc::new(TestClock::new(0));
    let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);

    let initial_index = aq.video_tier_index();
    let initial_snapshot = aq.snapshot_video();
    assert_eq!(aq.tier_epoch(), 0, "fresh BotAq starts at epoch 0");

    // Walk past the AQ warmup (5s) with zero backpressure.
    let mut t = tick_n(&aq, &clock, 5_100, 3, 1_100, 0);

    // Now feed sustained HIGH backpressure (depth = 3 =
    // ENCODER_QUEUE_BACKPRESSURE_HIGH) across spaced ticks until the tier steps
    // down. Each tick advances past the sustain window + min-transition interval.
    let mut degraded = false;
    for _ in 0..12 {
        t += 2_500;
        clock.set_ms(t);
        aq.inject_encoder_queue_depth(3);
        aq.tick();
        if aq.video_tier_index() > initial_index {
            degraded = true;
            break;
        }
    }
    let final_snapshot = aq.snapshot_video();
    assert!(
        degraded,
        "sustained encoder backpressure must step the bot's video tier DOWN \
         (initial={initial_index}, final={})",
        aq.video_tier_index()
    );
    assert!(
        final_snapshot.bitrate_kbps <= initial_snapshot.bitrate_kbps,
        "encoder bitrate snapshot should not increase under sustained backpressure \
         (initial={} kbps, final={} kbps)",
        initial_snapshot.bitrate_kbps,
        final_snapshot.bitrate_kbps,
    );
    assert!(aq.tier_epoch() > 0, "a tier change must bump the epoch");
    assert!(
        wall_start.elapsed() < Duration::from_secs(20),
        "test took too long in wall time: {:?}",
        wall_start.elapsed(),
    );
}

/// REGRESSION LOCK (the mandate): a receiver's link must NEVER degrade the bot.
///
/// Stage 2 removed `BotAq::process_diagnostics`, so there is no longer any way
/// to feed receiver-reported FPS into the controller — the absence of that
/// method is itself the lock at the type level. Behaviorally, we run the AQ for
/// a long window with the bot's OWN backpressure held at zero (its normal state)
/// and assert the tier never moves, no matter how long it runs. (A peer
/// reporting catastrophic FPS in this scenario simply has no input path.)
#[test]
fn bot_does_not_degrade_on_receiver_fps() {
    let clock = Arc::new(TestClock::new(0));
    let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
    let initial_index = aq.video_tier_index();

    // Long run, well past warmup, with zero backpressure (the bot has no
    // WebCodecs encoder so this is its real steady state). The tier must not
    // budge — receiver FPS is not, and cannot be, an input.
    let _ = tick_n(&aq, &clock, 5_100, 40, 1_100, 0);

    assert_eq!(
        aq.video_tier_index(),
        initial_index,
        "the bot's tier must never change from a receiver's reported FPS \
         (there is no input path for it in Stage 2)"
    );
    assert_eq!(
        aq.tier_epoch(),
        0,
        "no tier transition must occur with zero sender backpressure"
    );
}

/// END-TO-END uplink-saturation shed (issue #1083 V21) — the test that would
/// have caught the inert-trigger blocker.
///
/// This wires the REAL netsim uplink shim to the REAL `BotAq` exactly as the
/// production AQ tick does (`main.rs`: sample `shim.bandwidth_wait_us()`, feed
/// it to `aq.observe_uplink_saturation()`), instead of feeding the controller a
/// hand-crafted counter. It offers producer-rate traffic through the shim with
/// a deliberately LOW `uplink_kbps`, drains each admission the way the real
/// `run_outbound_shim` does (a DETACHED task per `Admission::Delay` — the very
/// drain behavior that made the old `transport_drops_counter` signal inert),
/// and asserts:
///
///   1. the shim's `bandwidth_wait_us` actually climbs (saturation is real and
///      observable despite the full-speed channel drain — `admit` measures it
///      at the source, not via the never-filling channel), and
///   2. that signal, fed through `observe_uplink_saturation`, drives the
///      3-layer ladder to SHED below 3 active.
///
/// To keep the controller's clock-driven sustain/transition windows
/// deterministic (a real-time producer task racing a virtual TestClock makes
/// per-tick deltas flaky), each virtual AQ tick first offers a full tick's worth
/// of producer frames into the shim, then samples the shim's cumulative counter
/// — the exact (sample, feed) order production uses. The drain still mirrors
/// `run_outbound_shim` (detached per-`Delay` task), proving the signal survives
/// it. The negative control is `latency_only_does_not_trip_uplink_shed`.
#[tokio::test]
async fn real_shim_uplink_saturation_sheds_layer() {
    let wall_start = Instant::now();

    // A 3-layer bot offers ~2800 kbps of video. Cap the uplink well below that
    // so the token bucket is in sustained deficit. Deterministic seed.
    let profile = NetworkProfile {
        uplink_kbps: Some(500),
        seed: Some(99),
        ..Default::default()
    };
    let shim = Arc::new(NetSimShim::new(profile, Direction::Up));

    // Offer one AQ tick's worth (1s) of ~30 fps × ~9KB frames (≈ 2.2 Mbps) into
    // the shim, draining each `Delay` via a detached task exactly like
    // `run_outbound_shim` (never head-of-line-block the offered stream — the
    // behavior that made the old try_send-drop signal inert). Async because the
    // detached drain mirrors production; the offered count is fixed so the
    // bandwidth deficit per tick is deterministic.
    async fn offer_one_tick(shim: &Arc<NetSimShim>) {
        for _ in 0..30 {
            if let Admission::Delay(d) = shim.admit(9_000) {
                tokio::spawn(async move {
                    tokio::time::sleep(d).await;
                });
            }
        }
    }

    // Consumer-side: the AQ tick. Real `BotAq`, real shim accessor, 3-layer
    // ladder. Deterministic clock so the controller's sustain/transition timers
    // advance regardless of wall-clock pacing.
    let clock = Arc::new(TestClock::new(0));
    let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
    aq.set_simulcast_layers(3);
    assert_eq!(
        aq.simulcast_snapshot().active,
        3,
        "precondition: bot starts at the full ladder"
    );

    // Warm up past the AQ warmup with zero saturation (no traffic offered),
    // sampling the (still 0) shim counter the way production does.
    let mut t: u64 = 0;
    for _ in 0..8 {
        t += 1_000;
        clock.set_ms(t);
        aq.observe_uplink_saturation(shim.bandwidth_wait_us());
        aq.tick();
    }

    // Now offer a tick's worth of over-budget traffic before EACH sample, so the
    // shim accrues new bandwidth-deficit delay every tick → sustained positive
    // delta → arms the shed timer.
    let mut shed = false;
    for _ in 0..60 {
        offer_one_tick(&shim).await;
        t += 1_000;
        clock.set_ms(t);
        aq.observe_uplink_saturation(shim.bandwidth_wait_us());
        aq.tick();
        if aq.simulcast_snapshot().active < 3 {
            shed = true;
            break;
        }
    }

    // (1) The saturation signal must have been real and observable.
    assert!(
        shim.bandwidth_wait_us() > 0,
        "the real uplink shim must record bandwidth-deficit delay under a 500 kbps \
         cap with ~2.2 Mbps offered (got {}us)",
        shim.bandwidth_wait_us()
    );
    // (2) It must have driven a shed.
    let snap = aq.simulcast_snapshot();
    assert!(
        shed && snap.active < 3,
        "real uplink saturation fed via observe_uplink_saturation must shed the top \
         layer (active ended at {})",
        snap.active
    );
    assert!(snap.active >= 1, "base layer must always keep flowing");
    assert!(
        wall_start.elapsed() < Duration::from_secs(20),
        "test took too long in wall time: {:?}",
        wall_start.elapsed()
    );
}

/// NEGATIVE CONTROL for the end-to-end shed: a pure latency+jitter profile (NO
/// `uplink_kbps`) drives the REAL shim and REAL `BotAq` for a long window, and
/// the ladder must stay at the full 3 layers. This is the regression lock that
/// guards every existing latency/jitter/loss preset (e.g. lossy_mobile): a
/// 150ms link is NOT a bandwidth-limited link and must never trip the shed.
#[tokio::test]
async fn latency_only_does_not_trip_uplink_shed() {
    let profile = NetworkProfile {
        latency_ms: 150,
        jitter_ms: 30,
        loss_pct: 1.0,
        seed: Some(7),
        // NO uplink_kbps — this is the whole point.
        ..Default::default()
    };
    let shim = Arc::new(NetSimShim::new(profile, Direction::Up));

    let clock = Arc::new(TestClock::new(0));
    let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
    aq.set_simulcast_layers(3);

    let mut t: u64 = 0;
    for _ in 0..50 {
        // Same heavy offered rate as the positive test; with no rate cap the
        // shim still delays every packet (150ms latency), but bandwidth_wait_us
        // must stay flat because the token bucket is absent.
        for _ in 0..30 {
            let _ = shim.admit(9_000);
        }
        t += 1_000;
        clock.set_ms(t);
        aq.observe_uplink_saturation(shim.bandwidth_wait_us());
        aq.tick();
    }

    assert_eq!(
        shim.bandwidth_wait_us(),
        0,
        "a latency/jitter/loss profile with no rate cap must NEVER advance the \
         bandwidth-saturation counter (got {}us)",
        shim.bandwidth_wait_us()
    );
    assert_eq!(
        aq.simulcast_snapshot().active,
        3,
        "latency-only impairment must NOT shed any simulcast layer"
    );
}

/// A self-targeted server CONGESTION cut (the bot's OWN relay-drop signal) must
/// degrade the bot immediately. This path is KEPT verbatim from before #1108.
#[test]
fn bot_degrades_on_congestion_cut() {
    let clock = Arc::new(TestClock::new(0));
    let aq = BotAq::new(clock.clone() as Arc<dyn Clock>);
    let initial_index = aq.video_tier_index();

    // Walk past warmup so the forced cut is allowed.
    let t = tick_n(&aq, &clock, 5_100, 3, 1_100, 0);
    clock.set_ms(t + 2_000);

    aq.force_congestion_cut();

    assert!(
        aq.video_tier_index() > initial_index,
        "a self-targeted CONGESTION cut must step the bot's video tier DOWN \
         (initial={initial_index}, final={})",
        aq.video_tier_index()
    );
}
