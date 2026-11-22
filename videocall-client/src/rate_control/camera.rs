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

//! Publisher side for the camera: the viewers' reports and the relay's
//! congestion signal in, a per-frame plan out (encode or skip, picture tier,
//! encoder bitrate).
//!
//! Reports arrive with the diagnostics packets ([`CameraRateController::on_report`]),
//! keyed by the viewer's session id; the camera's encode loop asks for a
//! plan on every captured frame ([`CameraRateController::on_camera_frame`]).
//! All times are one monotonic clock in ms, passed in. One controller lives
//! for the whole call: camera restarts, device switches and frame-rate or
//! cap changes reconfigure it and keep what the viewers said.

use std::collections::{HashMap, VecDeque};

use videocall_rate_control::{
    choose_tier, default_tiers, encoder_bitrate_bps, FrameDecision, FrameGate,
    MultiViewerPublisher, PublisherConfig, Report, Tier,
};

use crate::adaptive_quality_constants::{
    CAMERA_KEYFRAME_INTERVAL_SECS, DIAGNOSTICS_REPORT_INTERVAL_MS,
};

/// Capture rate assumed until the camera reports one.
pub(crate) const DEFAULT_CAMERA_FPS: f64 = 30.0;

/// Reports from one viewer closer together than this are dropped: an
/// honest viewer sends one per report period plus at most one early report
/// in between, so half a period is the tightest honest spacing.
pub(crate) const MIN_REPORT_SPACING_MS: f64 = DIAGNOSTICS_REPORT_INTERVAL_MS as f64 / 2.0;

/// The window the encoder reconfigure rate is measured over.
const RECONFIGURE_RATE_WINDOW_MS: f64 = 10_000.0;

/// The round trip from this publisher to its viewers, in ms, from the one
/// round trip it measures: to its relay.
///
/// **An estimate, and an open item.** The publisher only measures RTT to
/// the relay server; the path to a viewer is publisher to relay to viewer,
/// so this assumes the viewer's leg is like ours and doubles it. A viewer
/// far from the relay makes the real cycle longer than this, which makes
/// the probe step a little larger than intended. Before any RTT sample
/// (election still running) it is 0, so the cycle is the report period
/// alone; there are no reports to act on that early anyway.
pub(crate) fn viewer_round_trip_ms(relay_rtt_ms: Option<f64>) -> f64 {
    relay_rtt_ms
        .filter(|rtt| rtt.is_finite() && *rtt >= 0.0)
        .map_or(0.0, |rtt| 2.0 * rtt)
}

/// Whether a bitrate-only change from `current_bps` to `wanted_bps` is worth
/// a `configure()`. Changes under half a probe step are RTT jitter moving
/// the target and are ignored, so a real probe step always passes. A move
/// down while draining applies at once whatever its size: the backlog is
/// growing until it does.
pub(crate) fn bitrate_change_applies(
    current_bps: u32,
    wanted_bps: u32,
    probe_fraction: f64,
    draining: bool,
) -> bool {
    if wanted_bps == current_bps {
        return false;
    }
    if draining && wanted_bps < current_bps {
        return true;
    }
    let change = f64::from(wanted_bps.abs_diff(current_bps));
    change >= f64::from(current_bps) * probe_fraction / 2.0
}

/// Periodic camera keyframes, spaced in frame-timestamp time: frames the
/// gate skips do not stretch the spacing.
#[derive(Debug, Default)]
pub(crate) struct KeyframeClock {
    last_keyframe_ms: Option<f64>,
}

impl KeyframeClock {
    /// Whether the frame stamped `timestamp_ms` is a keyframe: the first
    /// one, one [`CAMERA_KEYFRAME_INTERVAL_SECS`] after the last, or
    /// `forced` (a viewer's request, a resize). A timestamp that went back
    /// restarts the period.
    pub(crate) fn is_keyframe(&mut self, timestamp_ms: f64, forced: bool) -> bool {
        let interval_ms = CAMERA_KEYFRAME_INTERVAL_SECS * 1000.0;
        let due = forced
            || self
                .last_keyframe_ms
                .is_none_or(|last| timestamp_ms - last >= interval_ms || timestamp_ms < last);
        if due {
            self.last_keyframe_ms = Some(timestamp_ms);
        }
        due
    }
}

/// Numbers frames for the transport. A number is used only once the frame
/// was handed to the transport: a frame the [`FrameGate`] skipped never
/// gets here, and one the send path refused at once gives its number back.
/// So a gap at the viewer means a frame lost after the hand-off (in the
/// shaper, the relay or the network), which the viewer's estimator expects.
#[derive(Debug, Default)]
pub(crate) struct FrameSequence {
    next: u64,
}

impl FrameSequence {
    /// Builds the packet for the next number with `send`, which returns
    /// whether the packet was handed off. The number advances only then.
    pub(crate) fn send_numbered(&mut self, send: impl FnOnce(u64) -> bool) -> bool {
        let handed_off = send(self.next);
        if handed_off {
            self.next += 1;
        }
        handed_off
    }
}

/// What to do with one captured camera frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FramePlan {
    /// Hand the frame to the encoder. A skipped frame is closed unencoded.
    pub encode: bool,
    /// Tier size cap (landscape; the encoder turns it to the frame).
    pub width: u32,
    pub height: u32,
    /// Tier frame rate, also the encoder's `framerate`.
    pub fps: f64,
    /// Encoder bitrate (constant bitrate mode), bps.
    pub bitrate_bps: u32,
    /// The target is below the tier's floor: frames are being skipped to
    /// drain a backlog.
    pub draining: bool,
    /// The publisher's probe step now, for [`bitrate_change_applies`].
    pub probe_fraction: f64,
}

/// The camera's rate control state, for the diagnostics panel.
#[derive(Debug, Clone, PartialEq)]
pub struct CameraRateStats {
    /// The publisher's target, bps.
    pub target_bps: u32,
    /// What the encoder is set to (target clamped to the tier), bps.
    pub encoder_bitrate_bps: u32,
    /// Current tier.
    pub tier_label: String,
    pub tier_width: u32,
    pub tier_height: u32,
    pub tier_fps: f64,
    /// The combined report the publisher acts on: the lowest capacity and
    /// the largest backlog among the viewers. `None` before any report.
    pub capacity_kbps: Option<u32>,
    pub backlog_ms: Option<u32>,
    /// The relay congestion signal's limit, while one is in force.
    pub relay_limit_kbps: Option<u32>,
    /// Viewers with a report.
    pub viewers_reporting: usize,
    /// Captured frames the gate kept from the encoder (tier frame rate and
    /// drain), since the call started.
    pub frames_skipped: u64,
    /// Encoder `configure()` calls per second, over the last 10 s.
    pub reconfigures_per_sec: f64,
}

/// The camera's publisher: one [`MultiViewerPublisher`] keyed by viewer
/// session id, the tier table, and the frame gate.
#[derive(Debug)]
pub struct CameraRateController {
    publisher: MultiViewerPublisher<u64>,
    tiers: Vec<Tier>,
    tier: usize,
    gate: FrameGate,
    rtt_ms: f64,
    frames_skipped: u64,
    /// When each viewer's last report was accepted, for the rate limit.
    /// Holds exactly the viewers the publisher has a report from.
    last_report_ms: HashMap<u64, f64>,
    /// When the encoder was configured, within the rate window.
    configures: VecDeque<f64>,
}

impl CameraRateController {
    /// A controller capped at `max_bps` (the configured camera bitrate),
    /// for a camera capturing at `camera_fps`. Before any report it runs at
    /// the cap, on the highest tier the cap allows.
    pub fn new(max_bps: f64, camera_fps: f64) -> Self {
        let tiers = default_tiers(max_bps);
        let config = PublisherConfig {
            start_bps: max_bps,
            max_bps,
            frame_interval_ms: 1000.0 / sane_fps(camera_fps),
            report_interval_ms: DIAGNOSTICS_REPORT_INTERVAL_MS as f64,
            floor_bps: bottom_floor_bps(&tiers),
        };
        let tier = start_tier(&tiers, config.start_bps);
        Self {
            publisher: MultiViewerPublisher::new(config),
            tiers,
            tier,
            gate: FrameGate::new(),
            rtt_ms: 0.0,
            frames_skipped: 0,
            last_report_ms: HashMap::new(),
            configures: VecDeque::new(),
        }
    }

    /// The configured cap changed. The reports, the relay limit and a
    /// lowered start rate are kept; the tier table follows the cap.
    pub fn set_max_bitrate(&mut self, max_bps: f64, now_ms: f64) {
        let old = *self.publisher.config();
        if max_bps == old.max_bps {
            return;
        }
        // A start rate still at the old cap follows the cap; one lowered
        // after a failed link stays lowered.
        let start_bps = if old.start_bps >= old.max_bps {
            max_bps
        } else {
            old.start_bps.min(max_bps)
        };
        self.tiers = default_tiers(max_bps);
        self.publisher.reconfigure(PublisherConfig {
            start_bps,
            max_bps,
            floor_bps: bottom_floor_bps(&self.tiers),
            ..old
        });
        self.tier = start_tier(&self.tiers, start_bps);
        self.update_tier(now_ms);
    }

    /// The camera's capture rate is known (or changed). Only the probe step
    /// depends on it; everything the viewers said is kept. A missing or
    /// nonsensical rate means [`DEFAULT_CAMERA_FPS`].
    pub fn set_camera_fps(&mut self, camera_fps: Option<f64>) {
        let camera_fps = sane_fps(camera_fps.unwrap_or(DEFAULT_CAMERA_FPS));
        self.publisher.set_frame_interval_ms(1000.0 / camera_fps);
    }

    /// The latest round trip to the relay; see [`viewer_round_trip_ms`].
    pub fn set_relay_rtt(&mut self, relay_rtt_ms: Option<f64>) {
        self.rtt_ms = viewer_round_trip_ms(relay_rtt_ms);
    }

    /// A viewer's report about our camera stream arrived at `now_ms`.
    /// Returns `false` when it was dropped: a viewer gets at most one report
    /// per [`MIN_REPORT_SPACING_MS`].
    pub fn on_report(&mut self, viewer: u64, now_ms: f64, report: Report) -> bool {
        if self
            .last_report_ms
            .get(&viewer)
            .is_some_and(|last| now_ms - last < MIN_REPORT_SPACING_MS)
        {
            return false;
        }
        self.last_report_ms.insert(viewer, now_ms);
        self.publisher.on_report(viewer, now_ms, report);
        self.update_tier(now_ms);
        true
    }

    /// The relay's CONGESTION signal: a receiver is dropping our packets.
    /// `from` is that receiver's session when it is known. Returns whether
    /// the limit was cut.
    pub fn on_congestion_signal(&mut self, now_ms: f64, from: Option<u64>) -> bool {
        let cut = self
            .publisher
            .on_congestion_signal(now_ms, self.rtt_ms, from.as_ref());
        self.update_tier(now_ms);
        cut
    }

    /// The viewer left: its report no longer counts.
    pub fn remove_viewer(&mut self, viewer: u64) {
        self.last_report_ms.remove(&viewer);
        self.publisher.remove_viewer(&viewer);
    }

    /// The link failed and every peer is being dropped. Their reports go,
    /// but the rate does not snap back to the cap: what the publisher was
    /// sending becomes its start rate, which it holds until reports come in
    /// on the new link. If no viewer ever reports again (only older or
    /// native clients, which send no capacity), it holds that rate for the
    /// rest of the call; the relay's CONGESTION signal still protects them.
    pub fn on_link_lost(&mut self, now_ms: f64) {
        let held = self.publisher.target_bps(now_ms, self.rtt_ms);
        for viewer in self.last_report_ms.drain().map(|(viewer, _)| viewer) {
            self.publisher.remove_viewer(&viewer);
        }
        let config = *self.publisher.config();
        self.publisher.reconfigure(PublisherConfig {
            start_bps: held.min(config.max_bps),
            ..config
        });
    }

    /// The tier for the capacity the publisher sees now. Run on every
    /// report and signal, and on every frame, so a relay limit that grows
    /// back (no report to trigger it) lifts the tier too. Allocates nothing.
    ///
    /// With no capacity at all (no fresh report, no relay limit) the target
    /// is the start rate, so the tier is the start rate's: this is how the
    /// tier comes back once a relay limit has grown out.
    fn update_tier(&mut self, now_ms: f64) {
        let Some(capacity) = self.publisher.tier_capacity_bps(now_ms, self.rtt_ms) else {
            let start = start_tier(&self.tiers, self.publisher.config().start_bps);
            if start != self.tier {
                log::info!(
                    "camera rate control: tier {} -> {} (no capacity reported, start rate)",
                    self.tiers[self.tier].label,
                    self.tiers[start].label,
                );
                self.tier = start;
            }
            return;
        };
        let probe = self.publisher.config().probe_fraction(self.rtt_ms);
        let next = choose_tier(&self.tiers, self.tier, capacity, probe);
        if next != self.tier {
            log::info!(
                "camera rate control: tier {} -> {} (capacity {:.0} kbps)",
                self.tiers[self.tier].label,
                self.tiers[next].label,
                capacity / 1000.0,
            );
            self.tier = next;
        }
    }

    /// The plan for the camera frame captured at `timestamp_ms` (the
    /// frame's own timestamp), seen at `now_ms`.
    pub fn on_camera_frame(&mut self, now_ms: f64, timestamp_ms: f64) -> FramePlan {
        self.update_tier(now_ms);
        let target = self.publisher.target_bps(now_ms, self.rtt_ms);
        let tier = &self.tiers[self.tier];
        let decision = self.gate.admit(tier, timestamp_ms, target);
        let encode = decision == FrameDecision::Encode;
        if !encode {
            self.frames_skipped += 1;
        }
        FramePlan {
            encode,
            width: tier.width,
            height: tier.height,
            fps: tier.fps,
            bitrate_bps: encoder_bitrate_bps(tier, target).round() as u32,
            draining: target < tier.floor_bps(),
            probe_fraction: self.publisher.config().probe_fraction(self.rtt_ms),
        }
    }

    /// The encoder was configured at `now_ms`.
    pub fn on_encoder_configured(&mut self, now_ms: f64) {
        self.configures.push_back(now_ms);
        self.prune_configures(now_ms);
    }

    fn prune_configures(&mut self, now_ms: f64) {
        while self
            .configures
            .front()
            .is_some_and(|at| now_ms - at > RECONFIGURE_RATE_WINDOW_MS)
        {
            self.configures.pop_front();
        }
    }

    /// The state at `now_ms`, for display.
    pub fn stats(&self, now_ms: f64) -> CameraRateStats {
        let tier = &self.tiers[self.tier];
        let target = self.publisher.target_bps(now_ms, self.rtt_ms);
        let combined = self.publisher.combined(now_ms, self.rtt_ms);
        let recent = self
            .configures
            .iter()
            .filter(|at| now_ms - **at <= RECONFIGURE_RATE_WINDOW_MS)
            .count();
        CameraRateStats {
            target_bps: target.round() as u32,
            encoder_bitrate_bps: encoder_bitrate_bps(tier, target).round() as u32,
            tier_label: tier.label.clone(),
            tier_width: tier.width,
            tier_height: tier.height,
            tier_fps: tier.fps,
            capacity_kbps: combined.map(|c| c.report.capacity_kbps()),
            backlog_ms: combined.map(|c| c.report.backlog_ms_rounded()),
            relay_limit_kbps: self
                .publisher
                .congestion_limit_bps(now_ms, self.rtt_ms)
                .map(|limit| (limit / 1000.0).round() as u32),
            viewers_reporting: self.publisher.viewers(),
            frames_skipped: self.frames_skipped,
            reconfigures_per_sec: recent as f64 * 1000.0 / RECONFIGURE_RATE_WINDOW_MS,
        }
    }
}

fn bottom_floor_bps(tiers: &[Tier]) -> f64 {
    tiers.last().map_or(0.0, Tier::floor_bps)
}

/// The highest tier the start rate affords.
fn start_tier(tiers: &[Tier], start_bps: f64) -> usize {
    tiers
        .iter()
        .position(|tier| tier.floor_bps() <= start_bps)
        .unwrap_or(tiers.len().saturating_sub(1))
}

fn sane_fps(fps: f64) -> f64 {
    if fps.is_finite() && fps > 0.0 {
        fps
    } else {
        DEFAULT_CAMERA_FPS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_BPS: f64 = 1_000_000.0;
    const FRAME_MS: f64 = 1000.0 / 30.0;

    fn report(capacity_kbps: u32, backlog_ms: u32) -> Report {
        Report::from_wire(capacity_kbps, backlog_ms)
    }

    fn controller() -> CameraRateController {
        CameraRateController::new(MAX_BPS, 30.0)
    }

    /// Frame `n` of a 30 fps camera, plan asked at its capture time.
    fn frame(camera: &mut CameraRateController, start_ms: f64, n: u32) -> FramePlan {
        let at = start_ms + f64::from(n) * FRAME_MS;
        camera.on_camera_frame(at, at)
    }

    #[test]
    fn round_trip_is_twice_the_relay_rtt() {
        assert_eq!(viewer_round_trip_ms(Some(80.0)), 160.0);
        assert_eq!(viewer_round_trip_ms(None), 0.0);
        assert_eq!(viewer_round_trip_ms(Some(f64::NAN)), 0.0);
        assert_eq!(viewer_round_trip_ms(Some(-5.0)), 0.0);
    }

    #[test]
    fn the_publisher_floor_is_the_bottom_tiers() {
        let camera = controller();
        assert_eq!(camera.publisher.config().floor_bps, 270_000.0);
        let low_cap = CameraRateController::new(200_000.0, 30.0);
        assert_eq!(low_cap.publisher.config().floor_bps, 200_000.0);
    }

    #[test]
    fn before_any_report_it_runs_at_the_cap_on_the_top_tier() {
        let mut camera = controller();
        let plan = frame(&mut camera, 0.0, 0);
        assert!(plan.encode);
        assert_eq!((plan.width, plan.height, plan.fps), (1280, 720, 30.0));
        assert_eq!(plan.bitrate_bps, 1_000_000);
        let stats = camera.stats(0.0);
        assert_eq!(stats.viewers_reporting, 0);
        assert_eq!(stats.capacity_kbps, None);
        assert_eq!(stats.relay_limit_kbps, None);
    }

    /// An old viewer sends no capacity, so it never reaches `on_report`: the
    /// publisher keeps its start rate (the cap), every frame encoded.
    #[test]
    fn with_only_old_viewers_it_holds_the_start_rate() {
        let mut camera = controller();
        for n in 0..300 {
            let plan = frame(&mut camera, 0.0, n);
            assert!(plan.encode);
            assert_eq!(plan.bitrate_bps, 1_000_000);
        }
        assert_eq!(camera.stats(10_000.0).frames_skipped, 0);
    }

    #[test]
    fn a_low_capacity_report_steps_the_tier_down() {
        let mut camera = controller();
        assert!(camera.on_report(2, 1_000.0, report(400, 0)));
        let plan = camera.on_camera_frame(1_010.0, 1_010.0);
        assert_eq!((plan.width, plan.height, plan.fps), (640, 360, 30.0));
        let stats = camera.stats(1_010.0);
        assert_eq!(stats.tier_label, "360p30");
        assert_eq!(stats.capacity_kbps, Some(400));
        assert_eq!(stats.viewers_reporting, 1);
    }

    /// The slowest viewer sets the pace; when it leaves, its report no
    /// longer counts.
    #[test]
    fn viewers_join_and_leave() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(900, 0));
        camera.on_report(3, 1_000.0, report(300, 0));
        assert_eq!(camera.stats(1_000.0).capacity_kbps, Some(300));
        assert_eq!(camera.stats(1_000.0).viewers_reporting, 2);
        camera.remove_viewer(3);
        assert_eq!(camera.stats(1_000.0).capacity_kbps, Some(900));
        assert_eq!(camera.stats(1_000.0).viewers_reporting, 1);
        camera.remove_viewer(2);
        assert_eq!(camera.stats(1_000.0).capacity_kbps, None);
        // Removing a viewer that never reported is harmless.
        camera.remove_viewer(99);
    }

    /// A viewer that left and came back has a new session id: the old key
    /// is gone and the new one counts on its own.
    #[test]
    fn a_rejoining_viewer_is_a_new_key() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(300, 0));
        camera.remove_viewer(2);
        camera.on_report(5, 1_500.0, report(800, 0));
        let stats = camera.stats(1_500.0);
        assert_eq!(stats.viewers_reporting, 1);
        assert_eq!(stats.capacity_kbps, Some(800));
    }

    /// One report per half report period per viewer; other viewers are not
    /// affected, and the window reopens.
    #[test]
    fn a_viewer_gets_one_report_per_half_period() {
        let mut camera = controller();
        assert!(camera.on_report(2, 1_000.0, report(900, 0)));
        assert!(!camera.on_report(2, 1_100.0, report(100, 0)), "flood");
        assert_eq!(camera.stats(1_100.0).capacity_kbps, Some(900));
        assert!(
            camera.on_report(3, 1_100.0, report(800, 0)),
            "another viewer"
        );
        assert!(camera.on_report(2, 1_000.0 + MIN_REPORT_SPACING_MS, report(700, 0)));
        assert_eq!(camera.stats(1_300.0).capacity_kbps, Some(700));
    }

    /// The window is per viewer key: after a leave and rejoin the new
    /// session's first report is accepted at once.
    #[test]
    fn the_report_window_is_forgotten_with_the_viewer() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(900, 0));
        camera.remove_viewer(2);
        assert!(camera.on_report(2, 1_010.0, report(900, 0)));
    }

    /// A drain below the tier's floor skips frames instead of dropping the
    /// tier, and the encoder bitrate stays at the floor.
    #[test]
    fn a_backlog_drains_by_skipping_frames() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(400, 400));
        let mut encoded = 0;
        let mut plans = Vec::new();
        for n in 0..15 {
            let plan = frame(&mut camera, 1_000.0, n);
            encoded += usize::from(plan.encode);
            plans.push(plan);
        }
        assert!(
            camera.stats(1_400.0).target_bps < 360_000,
            "draining below 360p30's floor"
        );
        assert!(encoded < 15, "some frames skipped");
        assert!(encoded > 0, "never all of them");
        assert!(plans.iter().all(|plan| plan.bitrate_bps == 360_000));
        assert!(plans.iter().all(|plan| plan.draining));
        assert_eq!(camera.stats(1_500.0).frames_skipped, 15 - encoded as u64);
    }

    #[test]
    fn the_15_fps_tier_encodes_every_other_frame_of_a_30_fps_camera() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(280, 0));
        let encoded: Vec<bool> = (0..6)
            .map(|n| frame(&mut camera, 1_000.0, n).encode)
            .collect();
        assert_eq!(encoded.iter().filter(|e| **e).count(), 3);
        assert_eq!(camera.stats(1_200.0).tier_label, "360p15");
    }

    /// The gate goes by the frames' own timestamps: a camera that delivers
    /// late (plan asked long after capture) is still decimated by when the
    /// frames were taken.
    #[test]
    fn the_gate_uses_the_capture_timestamp() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(280, 0));
        let encoded = (0..6)
            .filter(|n| {
                let captured = 1_000.0 + f64::from(*n) * FRAME_MS;
                camera.on_camera_frame(1_500.0, captured).encode
            })
            .count();
        assert_eq!(encoded, 3);
    }

    /// The one publisher survives a camera restart, a frame-rate change and
    /// a new cap: the rate goes on from the viewers' reports instead of
    /// jumping back to the cap.
    #[test]
    fn the_publisher_keeps_its_rate_across_camera_restarts() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(500, 0));
        let before = camera.stats(1_010.0);
        assert!(before.target_bps < 600_000);
        camera.set_camera_fps(Some(24.0));
        camera.set_camera_fps(None);
        camera.set_max_bitrate(1_200_000.0, 1_020.0);
        let after = camera.stats(1_020.0);
        assert_eq!(after.viewers_reporting, 1, "reports kept");
        assert_eq!(after.capacity_kbps, Some(500));
        assert!(after.target_bps < 600_000, "no jump to the cap");
        assert_eq!(after.tier_label, before.tier_label);
    }

    #[test]
    fn a_lower_cap_moves_the_tier_and_keeps_the_skip_count() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(280, 0));
        frame(&mut camera, 1_000.0, 0);
        frame(&mut camera, 1_000.0, 1);
        let skipped = camera.stats(1_100.0).frames_skipped;
        assert!(skipped > 0);
        camera.on_report(2, 1_600.0, report(5_000, 0));
        camera.set_max_bitrate(800_000.0, 1_600.0);
        let stats = camera.stats(1_600.0);
        assert_eq!(stats.frames_skipped, skipped);
        // 720p's floor is above an 800 kbps cap.
        assert_eq!(stats.tier_label, "480p30");
        assert!(stats.target_bps <= 800_000);
    }

    /// A failed link drops every viewer's report, but the start rate becomes
    /// what was being sent, not the cap.
    #[test]
    fn a_lost_link_holds_the_rate_it_had() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(400, 0));
        camera.on_report(3, 1_000.0, report(900, 0));
        let held = camera.stats(1_010.0).target_bps;
        camera.on_link_lost(1_010.0);
        // What clear_all_peers then does, peer by peer.
        camera.remove_viewer(2);
        camera.remove_viewer(3);
        let stats = camera.stats(5_000.0);
        assert_eq!(stats.viewers_reporting, 0);
        assert_eq!(stats.target_bps, held);
        assert_eq!(stats.tier_label, "360p30");
        // Reports on the new link take over.
        camera.on_report(7, 6_000.0, report(900, 0));
        assert!(camera.stats(6_000.0).target_bps > held);
    }

    /// The relay signal with no reporting viewer halves the target, the
    /// tier follows, and both grow back without any report.
    #[test]
    fn a_relay_signal_cuts_and_then_recovers() {
        let mut camera = controller();
        assert!(camera.on_congestion_signal(1_000.0, None));
        let stats = camera.stats(1_000.0);
        assert_eq!(stats.target_bps, 500_000);
        assert_eq!(stats.relay_limit_kbps, Some(500));
        assert_eq!(stats.tier_label, "360p30");
        // About 7% a cycle: past 600 kbps after a few cycles, one tier up.
        camera.on_camera_frame(5_000.0, 5_000.0);
        let mid = camera.stats(5_000.0);
        assert!(mid.relay_limit_kbps.is_some_and(|limit| limit > 640));
        assert_eq!(mid.tier_label, "480p30", "one step");
        // Well past the cap after a minute: the limit is gone, the start
        // rate and its tier are back.
        let plan = camera.on_camera_frame(61_000.0, 61_000.0);
        assert_eq!(plan.bitrate_bps, 1_000_000);
        assert_eq!(camera.stats(61_000.0).relay_limit_kbps, None);
        assert_eq!(camera.stats(61_000.0).tier_label, "720p30");
    }

    /// A signal blamed on a viewer that reports is the crate's to ignore.
    #[test]
    fn a_relay_signal_from_a_reporting_viewer_is_ignored() {
        let mut camera = controller();
        camera.on_report(2, 1_000.0, report(900, 0));
        assert!(!camera.on_congestion_signal(1_100.0, Some(2)));
        assert!(camera.on_congestion_signal(1_100.0, Some(3)));
    }

    #[test]
    fn the_target_never_exceeds_the_cap() {
        let mut camera = controller();
        camera.set_relay_rtt(Some(50.0));
        camera.on_report(2, 1_000.0, report(5_000, 0));
        let plan = camera.on_camera_frame(1_010.0, 1_010.0);
        assert!(f64::from(camera.stats(1_010.0).target_bps) <= MAX_BPS);
        assert_eq!(plan.bitrate_bps, 1_000_000);
    }

    /// The probe step (frame interval over the cycle) always reconfigures;
    /// RTT jitter does not; a drain applies at once.
    #[test]
    fn small_bitrate_moves_are_ignored_but_probe_steps_and_drains_apply() {
        let mut camera = controller();
        camera.set_relay_rtt(Some(40.0));
        let probe = frame(&mut camera, 0.0, 0).probe_fraction;
        let current = 600_000;
        let probe_step = (f64::from(current) * (1.0 + probe)) as u32;
        assert!(bitrate_change_applies(current, probe_step, probe, false));
        // RTT 40 -> 50 ms changes the probe step by a fraction of a percent.
        let jitter = (f64::from(current) * 1.003) as u32;
        assert!(!bitrate_change_applies(current, jitter, probe, false));
        assert!(!bitrate_change_applies(
            current,
            current - 1_000,
            probe,
            false
        ));
        assert!(bitrate_change_applies(
            current,
            current - 1_000,
            probe,
            true
        ));
        assert!(!bitrate_change_applies(current, current, probe, true));
    }

    #[test]
    fn the_reconfigure_rate_counts_the_last_ten_seconds() {
        let mut camera = controller();
        for at in [0.0, 1_000.0, 2_000.0, 9_000.0] {
            camera.on_encoder_configured(at);
        }
        assert_eq!(camera.stats(9_000.0).reconfigures_per_sec, 0.4);
        assert_eq!(camera.stats(11_500.0).reconfigures_per_sec, 0.2);
        assert_eq!(camera.stats(12_500.0).reconfigures_per_sec, 0.1);
    }

    /// Keyframes every 5 s of capture time, even when the tier skips every
    /// other frame.
    #[test]
    fn keyframes_are_spaced_in_time_not_frames() {
        let mut clock = KeyframeClock::default();
        let encoded_15fps: Vec<f64> = (0..=40)
            .map(|n| f64::from(n) * 2.0 * FRAME_MS * 3.0)
            .collect();
        let keyframes: Vec<f64> = encoded_15fps
            .iter()
            .copied()
            .filter(|ts| clock.is_keyframe(*ts, false))
            .collect();
        assert_eq!(keyframes.len(), 2, "{keyframes:?}");
        assert_eq!(keyframes[0], 0.0);
        assert!((5_000.0..5_200.0).contains(&keyframes[1]));
    }

    #[test]
    fn a_forced_keyframe_restarts_the_period() {
        let mut clock = KeyframeClock::default();
        assert!(clock.is_keyframe(0.0, false));
        assert!(clock.is_keyframe(3_000.0, true));
        assert!(!clock.is_keyframe(7_000.0, false));
        assert!(clock.is_keyframe(8_000.0, false));
        assert!(clock.is_keyframe(100.0, false), "timestamps went back");
    }

    /// The number advances only when the send path took the packet; a
    /// refused hand-off leaves no gap behind it.
    #[test]
    fn a_refused_hand_off_gives_its_number_back() {
        let mut sequence = FrameSequence::default();
        let mut sent = Vec::new();
        let accepted = [true, false, false, true, true];
        for accept in accepted {
            sequence.send_numbered(|seq| {
                if accept {
                    sent.push(seq);
                }
                accept
            });
        }
        assert_eq!(sent, vec![0, 1, 2]);
    }
}
