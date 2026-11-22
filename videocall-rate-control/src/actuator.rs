//! Actuator decisions, as pure functions over a tier table the caller owns.
//!
//! - Tier choice follows the reported **capacity** only, never the target,
//!   and is evaluated on every report: step down when capacity is below the
//!   current tier's `lo`, to the highest tier whose `lo <= capacity` (else
//!   the bottom one); step up one tier when capacity `>=` the upper tier's
//!   `lo * (1 + p)`, `p` the publisher's probe step. Capacity only rises once
//!   a higher rate was delivered with nothing waiting, so `[lo, lo * (1 + p))`
//!   is the hysteresis band and the probe ripple cannot flap the tier.
//! - Encoder bitrate = `clamp(target, lo, hi)` of the current tier. A drain
//!   (target below `lo`) is a transient, handled by frame decimation, not by
//!   a tier change.
//! - Frame decimation: first the tier's own frame rate, on the frames'
//!   timestamps (a 15 fps tier takes every other frame of a 30 fps camera,
//!   and every frame of a camera that really delivers 15); then, only while
//!   the target is below the tier's floor, a fraction `target / floor` of
//!   the frames, never less than [`MIN_DRAIN_FRACTION`], evenly spaced by an
//!   accumulator. With a constant-bitrate encoder the output follows the
//!   fraction handed to it. The worst case is the bottom tier at half its
//!   frames: a viewer that cannot take that is beyond what one encoding can
//!   serve, and the publisher's capacity floor
//!   ([`crate::PublisherConfig::floor_bps`]) keeps it from asking for less.

use crate::publisher::MIN_DRAIN_FRACTION;

/// One rung of the picture ladder.
#[derive(Debug, Clone, PartialEq)]
pub struct Tier {
    /// Name for logs, e.g. `"720p30"`.
    pub label: String,
    /// Encoded width, px.
    pub width: u32,
    /// Encoded height, px.
    pub height: u32,
    /// Frames per second handed to the encoder.
    pub fps: f64,
    /// Lowest bitrate this picture size looks right at, bps.
    pub lo_bps: f64,
    /// Highest encoder bitrate for this tier, bps.
    pub hi_bps: f64,
}

impl Tier {
    /// The bitrate this tier is held at or above: `lo`, or `hi` when a
    /// table has `hi < lo` (a configured cap below the floor).
    pub fn floor_bps(&self) -> f64 {
        self.lo_bps.min(self.hi_bps)
    }
}

/// The measured default ladder, top first, `hi` = the configured cap:
/// 1280x720@30 from 960 kbps, 854x480@30 from 600 kbps, 640x360@30 from
/// 360 kbps, 640x360@15 from 270 kbps (VP9, WebCodecs, measured in the
/// congestion POC's encoder sweep). Tiers whose `lo` is above the cap are
/// left out; the bottom tier always stays, with its `lo` lowered to the cap
/// when the cap is below 270 kbps (so it never decimates for want of a rate
/// it is not allowed to send).
pub fn default_tiers(max_bps: f64) -> Vec<Tier> {
    let ladder = [
        ("720p30", 1280, 720, 30.0, 960_000.0),
        ("480p30", 854, 480, 30.0, 600_000.0),
        ("360p30", 640, 360, 30.0, 360_000.0),
        ("360p15", 640, 360, 15.0, 270_000.0),
    ];
    let last = ladder.len() - 1;
    ladder
        .iter()
        .enumerate()
        .filter(|(idx, rung)| rung.4 <= max_bps || *idx == last)
        .map(|(_, &(label, width, height, fps, lo_bps))| Tier {
            label: label.to_string(),
            width,
            height,
            fps,
            lo_bps: lo_bps.min(max_bps),
            hi_bps: max_bps,
        })
        .collect()
}

/// The tier to use after a report with `capacity_bps`, given the current
/// tier index into `tiers` (top first, `lo` decreasing) and the publisher's
/// probe step `probe_fraction`. Pure; returns `current` when nothing changes.
/// Pass the capacity from
/// [`crate::MultiViewerPublisher::tier_capacity_bps`], never the target.
pub fn choose_tier(
    tiers: &[Tier],
    current: usize,
    capacity_bps: f64,
    probe_fraction: f64,
) -> usize {
    let Some(tier) = tiers.get(current) else {
        return tiers.len().saturating_sub(1);
    };
    if capacity_bps < tier.floor_bps() {
        let down = tiers
            .iter()
            .position(|t| t.floor_bps() <= capacity_bps)
            .unwrap_or(tiers.len() - 1);
        return down.max(current);
    }
    // Up at one probe step above the upper floor, or at the upper tier's
    // own maximum when that is lower (a capacity clamped to the cap must
    // still reach a tier whose floor is just under the cap).
    if current > 0
        && capacity_bps
            >= (tiers[current - 1].floor_bps() * (1.0 + probe_fraction))
                .min(tiers[current - 1].hi_bps)
    {
        return current - 1;
    }
    current
}

/// Encoder bitrate for `tier` at publisher target `target_bps`:
/// `clamp(target, lo, hi)` (`hi` wins if a table has `hi < lo`).
pub fn encoder_bitrate_bps(tier: &Tier, target_bps: f64) -> f64 {
    target_bps.max(tier.floor_bps()).min(tier.hi_bps)
}

/// What to do with one camera frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDecision {
    /// Hand it to the encoder.
    Encode,
    /// Skip: the tier runs at a lower frame rate than the camera.
    SkipTierRate,
    /// Skip: the target is below the tier's `lo` (a drain).
    SkipDrain,
}

/// Frame decimation state: the tier-rate schedule and the drain
/// accumulator.
#[derive(Debug, Clone, Default)]
pub struct FrameGate {
    /// When the next frame is due at the tier's rate (frame timestamps).
    next_due_ms: Option<f64>,
    /// Timestamp of the previous camera frame.
    last_timestamp_ms: Option<f64>,
    drain_acc: f64,
}

/// Accumulator slack against float rounding.
const ACC_EPSILON: f64 = 1e-9;

impl FrameGate {
    /// A gate with nothing scheduled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decides for the next camera frame. `timestamp_ms` is the frame's
    /// capture timestamp (any clock that only goes forward), `target_bps`
    /// the publisher target now. A frame skipped here is not handed to the
    /// transport, so it takes no sequence number.
    ///
    /// Tier rate: a frame is taken when it is closer to the next due time
    /// than the camera's following frame will be (`timestamp + half the
    /// last camera gap >= due`), and the schedule then moves on by one tier
    /// interval. So the real camera rate counts, not the one it claims.
    pub fn admit(&mut self, tier: &Tier, timestamp_ms: f64, target_bps: f64) -> FrameDecision {
        if self
            .last_timestamp_ms
            .is_some_and(|last| timestamp_ms < last)
        {
            // The clock went back (a new camera track): start the schedule
            // over instead of waiting for the old clock's due time.
            self.next_due_ms = None;
            self.last_timestamp_ms = None;
        }
        let half_gap = self
            .last_timestamp_ms
            .map_or(0.0, |last| (timestamp_ms - last).max(0.0) / 2.0);
        self.last_timestamp_ms = Some(timestamp_ms);
        let interval = if tier.fps > 0.0 {
            1000.0 / tier.fps
        } else {
            f64::INFINITY
        };
        match self.next_due_ms {
            Some(due) if timestamp_ms + half_gap < due => return FrameDecision::SkipTierRate,
            Some(due) if due + interval > timestamp_ms => self.next_due_ms = Some(due + interval),
            _ => self.next_due_ms = Some(timestamp_ms + interval),
        }
        let floor = tier.floor_bps();
        if target_bps < floor {
            self.drain_acc += (target_bps / floor).max(MIN_DRAIN_FRACTION);
            if self.drain_acc >= 1.0 - ACC_EPSILON {
                self.drain_acc -= 1.0;
            } else {
                return FrameDecision::SkipDrain;
            }
        } else {
            self.drain_acc = 0.0;
        }
        FrameDecision::Encode
    }
}
