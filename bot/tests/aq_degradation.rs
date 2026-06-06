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
