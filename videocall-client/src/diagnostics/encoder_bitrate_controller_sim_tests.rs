//! Closed-loop tests for [`EncoderBitrateController`].
//!
//! These are plain `#[test]`s so they run natively (`cargo test -p
//! videocall-client --lib`). They never touch `Date::now()`: the controller is
//! built with the `_at` constructors and driven through
//! [`EncoderBitrateController::process_diagnostics_packet_with_time`].
//!
//! Frames come from a [`Source`]: a steady camera, a camera turned off and on,
//! or (for the viewer-side tests) a timeline cut from frames measured in
//! Chrome tab capture (`testdata/`). Capture never follows the tier, as in
//! production. Every frame is encoded 1:1 and counted by the real
//! [`FrameRateMeter`] the camera encoder shares with the controller.
//!
//! The link is a bottleneck of `capacity_kbps`: while the encoder is
//! configured above capacity, frames are dropped in proportion. Frames can
//! also be lost at random and arrive with jitter. Each viewer is the real
//! `FpsTracker`, created on its first frame, and reports once per tick; a
//! report reaches the controller `delay_ticks` seconds after it was made.
//!
//! The applied bitrate follows the camera encoder's rule for when to
//! reconfigure the encoder, so assertions are made on what actually goes
//! on the wire, not only on what the controller asks for.

use super::*;
use crate::diagnostics::diagnostics_manager::FpsTracker;
use crate::diagnostics::frame_rate::{FrameEvent, FrameRateMeter};
use std::cell::RefCell;
use std::collections::VecDeque;
use videocall_types::protos::diagnostics_packet::VideoMetrics;

const TICK_MS: f64 = 1000.0;

const CAMERA_TIER: VideoQualityTier = VideoQualityTier {
    label: "sim-camera",
    max_width: 640,
    max_height: 360,
    target_fps: 30,
    ideal_bitrate_kbps: 1000,
    min_bitrate_kbps: 100,
    max_bitrate_kbps: 1500,
    keyframe_interval_frames: 150,
};
/// Identical tiers, so tier transitions never change the PID's base and the
/// tests observe the PID alone. Four entries because the quality manager
/// starts at `DEFAULT_VIDEO_TIER_INDEX` (3).
static CAMERA_TIERS: [VideoQualityTier; 4] = [CAMERA_TIER; 4];

/// Small deterministic PRNG (xorshift64*), so runs are reproducible without
/// pulling in a dev-dependency.
struct Rng(u64);

impl Rng {
    fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64
    }
}

/// What the network does to the stream during one tick.
#[derive(Clone, Copy, Debug)]
struct Link {
    capacity_kbps: f64,
    /// Ticks between the encoder sending at a bitrate and the controller
    /// receiving the report that describes it.
    delay_ticks: usize,
    /// Probability that a frame is lost regardless of congestion.
    random_loss: f64,
    /// Each frame arrives up to this much late (uniform).
    jitter_ms: f64,
}

impl Link {
    fn clean() -> Self {
        Self {
            capacity_kbps: f64::INFINITY,
            delay_ticks: 0,
            random_loss: 0.0,
            jitter_ms: 5.0,
        }
    }

    fn capped(capacity_kbps: f64) -> Self {
        Self {
            capacity_kbps,
            ..Self::clean()
        }
    }

    fn delay(self, delay_ticks: usize) -> Self {
        Self {
            delay_ticks,
            ..self
        }
    }
}

/// Frame capture timestamps (ms) measured in stable Chrome with
/// getDisplayMedia tab capture and MediaStreamTrackProcessor. "static": the
/// share starts on a static page (a short burst, then nothing); "anim": an
/// animated page (a steady ~30 fps); "static2": static again (no frames).
const CHROME_TAB_CAPTURE: &str = include_str!("testdata/tab_capture_frames_chrome.json");

/// One phase of the Chrome recording.
struct Phase {
    duration_ms: f64,
    frames_ms: Vec<f64>,
}

fn chrome_phase(name: &str) -> Phase {
    let recording: serde_json::Value =
        serde_json::from_str(CHROME_TAB_CAPTURE).expect("fixture is valid JSON");
    let phase = &recording[name];
    Phase {
        duration_ms: phase["ms"].as_f64().expect("phase duration"),
        frames_ms: phase["ts"]
            .as_array()
            .expect("phase timestamps")
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .collect(),
    }
}

/// Where captured frames come from. The capture rate depends on the source
/// only: production never applies a tier's `target_fps` to capture.
enum Source {
    /// A camera: one frame every `interval_ms`, forever.
    Steady { interval_ms: f64, next_ms: f64 },
    /// Explicit capture timestamps, ascending; nothing after the last one.
    Timeline(VecDeque<f64>),
}

impl Source {
    fn steady(fps: f64) -> Self {
        Self::Steady {
            interval_ms: 1000.0 / fps,
            next_ms: 0.0,
        }
    }

    fn frames_before(&mut self, end_ms: f64) -> Vec<f64> {
        match self {
            Self::Steady {
                interval_ms,
                next_ms,
            } => {
                let mut frames = Vec::new();
                while *next_ms < end_ms {
                    frames.push(*next_ms);
                    *next_ms += *interval_ms;
                }
                frames
            }
            Self::Timeline(frames) => {
                let count = frames.iter().take_while(|&&at| at < end_ms).count();
                frames.drain(..count).collect()
            }
        }
    }
}

/// Builds a capture timeline: the Chrome tab-capture recording, or a camera
/// turned on and off.
struct Capture {
    frames_ms: Vec<f64>,
    at_ms: f64,
}

impl Capture {
    fn starting_at(at_ms: f64) -> Self {
        Self {
            frames_ms: Vec::new(),
            at_ms,
        }
    }

    fn append(mut self, phase: &Phase, duration_ms: f64) -> Self {
        let mut offset = 0.0;
        while offset < duration_ms {
            self.frames_ms.extend(
                phase
                    .frames_ms
                    .iter()
                    .map(|t| t + offset)
                    .filter(|t| *t < duration_ms)
                    .map(|t| self.at_ms + t),
            );
            offset += phase.duration_ms;
        }
        self.at_ms += duration_ms;
        self
    }

    /// Animated content, the recorded 15 s looped as needed.
    fn animated(self, seconds: f64) -> Self {
        self.append(&chrome_phase("anim"), seconds * 1000.0)
    }

    /// A camera delivering 30 fps.
    fn camera_on(mut self, seconds: f64) -> Self {
        let end_ms = self.at_ms + seconds * 1000.0;
        let mut at = self.at_ms;
        while at < end_ms {
            self.frames_ms.push(at);
            at += 1000.0 / 30.0;
        }
        self.at_ms = end_ms;
        self
    }

    /// No frames at all: static content in tab capture, or the camera off.
    fn still(mut self, seconds: f64) -> Self {
        self.at_ms += seconds * 1000.0;
        self
    }

    fn source(self) -> Source {
        Source::Timeline(self.frames_ms.into())
    }
}

/// A viewer: the real `FpsTracker`, created on the first frame it receives
/// (as `DiagnosticManager::handle_event` does).
struct Viewer {
    id: String,
    joins_at_ms: f64,
    tracker: Option<FpsTracker>,
    last_arrival_ms: f64,
}

impl Viewer {
    fn new(id: &str, joins_at_ms: f64) -> Self {
        Self {
            id: id.to_string(),
            joins_at_ms,
            tracker: None,
            last_arrival_ms: f64::NEG_INFINITY,
        }
    }

    fn frame(&mut self, at_ms: f64, bytes: u64) {
        if at_ms < self.joins_at_ms {
            return;
        }
        let at_ms = at_ms.max(self.last_arrival_ms);
        self.last_arrival_ms = at_ms;
        self.tracker
            .get_or_insert_with(|| FpsTracker::new_at(MediaType::VIDEO, at_ms))
            .track_frame_at(bytes, at_ms);
    }

    fn report(&self, now_ms: f64) -> Option<f64> {
        self.tracker
            .as_ref()
            .and_then(|tracker| tracker.report_at(now_ms))
            .map(|(fps, _)| fps)
    }
}

/// One simulated second.
#[derive(Clone, Copy, Debug)]
struct Tick {
    /// Bitrate the encoder was configured with at the end of this tick.
    applied_kbps: f64,
    tier_index: usize,
}

struct Sim {
    controller: EncoderBitrateController,
    current_fps: Rc<AtomicU32>,
    /// The encoder's output meter, shared with the controller as the
    /// encoders share it.
    output_meter: Rc<RefCell<FrameRateMeter>>,
    applied_kbps: f64,
    now_ms: f64,
    source: Source,
    viewers: Vec<Viewer>,
    /// Reports in flight back to the sender, one batch per tick, oldest first.
    report_queue: VecDeque<Vec<(String, f64)>>,
    rng: Rng,
    congestion_credit: f64,
}

impl Sim {
    fn camera() -> Self {
        Self::new(&CAMERA_TIERS).with_source(Source::steady(30.0))
    }

    /// The real camera tiers. The camera delivers 30 fps whatever the tier.
    fn real_camera_tiers() -> Self {
        Self::new(crate::adaptive_quality_constants::VIDEO_QUALITY_TIERS)
            .with_source(Source::steady(30.0))
    }

    fn new(tiers: &'static [VideoQualityTier]) -> Self {
        let start = &tiers[crate::adaptive_quality_constants::DEFAULT_VIDEO_TIER_INDEX];
        let current_fps = Rc::new(AtomicU32::new(0));
        let controller = EncoderBitrateController::new_with_tiers_at(
            start.ideal_bitrate_kbps,
            current_fps.clone(),
            tiers,
            0.0,
        );
        Self::with_controller(controller, current_fps, start)
    }

    fn with_controller(
        controller: EncoderBitrateController,
        current_fps: Rc<AtomicU32>,
        start: &VideoQualityTier,
    ) -> Self {
        let output_meter = Rc::new(RefCell::new(FrameRateMeter::default()));
        Self {
            controller: controller.with_source_meter(output_meter.clone()),
            current_fps,
            output_meter,
            applied_kbps: start.ideal_bitrate_kbps as f64,
            now_ms: 0.0,
            source: Source::steady(30.0),
            viewers: vec![Viewer::new("worst_peer", 0.0)],
            report_queue: VecDeque::new(),
            rng: Rng(0x9E37_79B9_7F4A_7C15),
            congestion_credit: 0.0,
        }
    }

    fn with_source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    /// Another viewer that starts receiving at `joins_at_ms`.
    fn with_viewer(mut self, id: &str, joins_at_ms: f64) -> Self {
        self.viewers.push(Viewer::new(id, joins_at_ms));
        self
    }

    /// The encoders' rule for reconfiguring the encoder with a new bitrate.
    fn apply(&mut self, proposed_kbps: f64) {
        let change = (proposed_kbps - self.applied_kbps).abs() / self.applied_kbps;
        if change > crate::adaptive_quality_constants::BITRATE_CHANGE_THRESHOLD {
            self.applied_kbps = proposed_kbps;
        }
    }

    fn step(&mut self, link: Link) -> Tick {
        let pass_ratio = (link.capacity_kbps / self.applied_kbps).min(1.0);
        let tick_end = self.now_ms + TICK_MS;
        let frames = self.source.frames_before(tick_end);
        let bytes = (self.applied_kbps * 125.0 / 30.0) as u64;
        for &sent_at in &frames {
            // Encoded 1:1; the output handler counts the chunk.
            if let FrameEvent::WindowClosed { fps, .. } =
                self.output_meter.borrow_mut().on_frame(sent_at)
            {
                self.current_fps
                    .store(fps.round() as u32, Ordering::Relaxed);
            }

            self.congestion_credit += pass_ratio;
            if self.congestion_credit < 1.0 {
                continue;
            }
            self.congestion_credit -= 1.0;
            if self.rng.uniform() < link.random_loss {
                continue;
            }
            let arrival = sent_at + self.rng.uniform() * link.jitter_ms;
            for viewer in &mut self.viewers {
                viewer.frame(arrival, bytes);
            }
        }
        self.now_ms = tick_end;

        // Each viewer's report takes `delay_ticks` to come back.
        let mut batch = Vec::new();
        for viewer in &self.viewers {
            if let Some(fps) = viewer.report(self.now_ms) {
                batch.push((viewer.id.clone(), fps));
            }
        }
        self.report_queue.push_back(batch);
        if self.report_queue.len() > link.delay_ticks {
            for (id, fps) in self.report_queue.pop_front().unwrap_or_default() {
                self.feed_from(&id, fps);
            }
        }

        self.controller.take_tier_changed();
        Tick {
            applied_kbps: self.applied_kbps,
            tier_index: self.controller.video_tier_index(),
        }
    }

    /// Deliver one report with an explicit fps at the current time.
    fn feed(&mut self, received_fps: f64) -> Option<f64> {
        self.feed_from("worst_peer", received_fps)
    }

    fn feed_from(&mut self, viewer: &str, received_fps: f64) -> Option<f64> {
        let out = self
            .controller
            .process_diagnostics_packet_with_time(report_packet(viewer, received_fps), self.now_ms);
        if let Some(bitrate) = out {
            self.apply(bitrate);
        }
        out
    }

    fn run(&mut self, seconds: usize, link: Link) -> Vec<Tick> {
        (0..seconds).map(|_| self.step(link)).collect()
    }
}

fn video_packet(fps: f64) -> DiagnosticsPacket {
    report_packet("worst_peer", fps)
}

fn report_packet(viewer: &str, fps: f64) -> DiagnosticsPacket {
    let mut packet = DiagnosticsPacket::new();
    packet.sender_id = "remote".to_string();
    packet.target_id = viewer.to_string();
    packet.media_type = MediaType::VIDEO.into();
    let mut metrics = VideoMetrics::new();
    metrics.fps_received = fps as f32;
    packet.video_metrics = ::protobuf::MessageField::some(metrics);
    packet
}

// ---------------------------------------------------------------------------
// Content-driven sources: pauses, outages and joining viewers
// ---------------------------------------------------------------------------

/// Frame arrival times for a viewer on a clean link: the recorded animated
/// content for 15 s, 30 s of static content (no frames), animated again.
fn static_pause_arrivals() -> Vec<f64> {
    match Capture::starting_at(0.0)
        .animated(15.0)
        .still(30.0)
        .animated(15.0)
        .source()
    {
        Source::Timeline(frames) => frames.into(),
        Source::Steady { .. } => unreachable!("a capture timeline"),
    }
}

/// What a viewer reports every 500 ms (the diagnostics heartbeat) while
/// receiving `arrivals`, as (report time, reported fps or None if skipped).
fn viewer_reports(arrivals: &[f64], until_ms: f64) -> Vec<(f64, Option<f64>)> {
    let mut viewer = Viewer::new("viewer", 0.0);
    let mut next = arrivals.iter().peekable();
    let mut reports = Vec::new();
    let mut now = 500.0;
    while now <= until_ms {
        while let Some(at) = next.next_if(|at| **at <= now) {
            viewer.frame(*at, 1000);
        }
        reports.push((now, viewer.report(now)));
        now += 500.0;
    }
    reports
}

/// With static content Chrome delivers no frames. The viewer must say so
/// (0 fps) instead of repeating the last rate, and the first frames after the
/// pause must not be divided by the length of the pause (23 frames / 30.7 s
/// = 0.75 fps reads as a 97% collapse).
#[test]
fn viewer_reports_a_pause_as_silence_and_resumes_at_the_real_rate() {
    let reports = viewer_reports(&static_pause_arrivals(), 60_000.0);
    let during_pause: Vec<_> = reports
        .iter()
        .filter(|(at, _)| (17_000.0..44_000.0).contains(at))
        .collect();
    assert!(
        during_pause.iter().all(|(_, fps)| *fps == Some(0.0)),
        "reports during the pause: {during_pause:?}"
    );
    let misleading: Vec<_> = reports
        .iter()
        .filter(|(_, fps)| fps.is_some_and(|fps| fps > 0.0 && fps < 20.0))
        .collect();
    assert!(
        misleading.is_empty(),
        "reports that are neither silence nor the ~30 fps the content runs at: {misleading:?}"
    );
}

/// A viewer that just started receiving has not measured anything yet; a
/// 0 fps report from it would read as a total collapse.
#[test]
fn viewer_does_not_report_before_its_first_full_window() {
    let arrivals: Vec<f64> = (0..60).map(|i| 10_000.0 + i as f64 * 33.3).collect();
    let mut viewer = Viewer::new("viewer", 0.0);
    for at in arrivals.iter().take_while(|at| **at < 10_500.0) {
        viewer.frame(*at, 1000);
    }
    assert_eq!(
        viewer.report(10_500.0),
        None,
        "report 500 ms after the first frame"
    );
}

/// The camera turned off and on, on a clean link. While it is off the
/// viewers honestly report 0 fps; that must not cost a step-down or a
/// bitrate reduction.
#[test]
fn camera_off_and_on_cycles_do_not_step_down() {
    for delay in [0, 1] {
        let mut camera = Capture::starting_at(0.0).camera_on(60.0);
        for _ in 0..6 {
            camera = camera.still(30.0).camera_on(15.0);
        }
        let mut sim = Sim::real_camera_tiers().with_source(camera.source());
        let ticks = sim.run(60 + 6 * 45, Link::clean().delay(delay));
        let step_downs = ticks
            .windows(2)
            .filter(|pair| pair[1].tier_index > pair[0].tier_index)
            .count();
        let reductions = ticks
            .windows(2)
            .filter(|pair| pair[1].applied_kbps < pair[0].applied_kbps)
            .count();
        assert_eq!(
            (step_downs, reductions),
            (0, 0),
            "delay {delay}: step-downs and bitrate reductions on a clean link"
        );
    }
}

/// An outage while the camera is on is the strongest congestion signal there
/// is: the viewer receives nothing, and the sender must react.
#[test]
fn outage_while_active_steps_down() {
    for delay in [0, 1, 2] {
        let mut sim = Sim::real_camera_tiers();
        sim.run(180, Link::clean().delay(delay));
        let tier = sim.controller.video_tier_index();
        let applied = sim.applied_kbps;
        let outage = sim.run(30, Link::capped(0.0).delay(delay));
        let reduced = outage.iter().position(|t| t.applied_kbps < applied);
        let stepped_down = outage.iter().position(|t| t.tier_index > tier);
        let bound = 10 + delay;
        assert!(
            reduced.is_some_and(|s| s < bound) && stepped_down.is_some_and(|s| s < bound),
            "delay {delay}: outage reduced after {reduced:?} s, stepped down after \
             {stepped_down:?} s (bound {bound} s)"
        );
    }
}

/// A viewer joining mid-call must not read as congestion, whether it joins
/// while the camera is on or while it is off.
#[test]
fn viewer_joining_mid_call_does_not_step_down() {
    let mut camera = Capture::starting_at(0.0).camera_on(150.0);
    for _ in 0..3 {
        camera = camera.still(30.0).camera_on(15.0);
    }
    for (joins_at_s, delay) in [(120.0, 0), (120.0, 1), (165.0, 0), (165.0, 1)] {
        let mut sim = Sim::real_camera_tiers()
            .with_source(capture_clone(&camera))
            .with_viewer("late", joins_at_s * 1000.0);
        let link = Link::clean().delay(delay);
        sim.run(110, link);
        let tier = sim.controller.video_tier_index();
        let applied = sim.applied_kbps;
        let after = sim.run(170, link);
        let worst_tier = after.iter().map(|t| t.tier_index).max().unwrap_or(tier);
        let lowest = after.iter().map(|t| t.applied_kbps).fold(applied, f64::min);
        assert!(
            worst_tier <= tier && lowest >= applied,
            "joins at {joins_at_s} s, delay {delay}: tier {tier} -> worst {worst_tier}, \
             {applied:.0} -> lowest {lowest:.0} kbps"
        );
    }
}

fn capture_clone(capture: &Capture) -> Source {
    Source::Timeline(capture.frames_ms.iter().copied().collect())
}

// ---------------------------------------------------------------------------
// Time steps
// ---------------------------------------------------------------------------

/// Reports stop for a while (backgrounded tab, peer briefly gone, lossy
/// link). The next report describes about one second of reception, so it must
/// not be integrated as if it had lasted the whole silence.
#[test]
fn long_report_gap_does_not_overweight_one_sample() {
    let mut sim = Sim::camera();
    sim.run(20, Link::clean());
    let integral_before = sim.controller.pid_state().integral_contribution;

    sim.now_ms += 20_000.0; // 20 s of silence
    sim.feed(20.0);

    let deadband = sim.controller.pid.config().deadband();
    let setpoint = sim.controller.pid.config().setpoint();
    let max_step = PID_KI * (setpoint - 20.0 - deadband) * PID_MAX_DT_SEC;
    let step = sim.controller.pid_state().integral_contribution - integral_before;
    assert!(
        step <= max_step + 1e-9,
        "one report after a 20 s gap moved the integral by {step:.2} \
         (cap is Ki * error * {PID_MAX_DT_SEC} s = {max_step:.2})"
    );
}

// ---------------------------------------------------------------------------
// Bad inputs
// ---------------------------------------------------------------------------

/// A report whose fps is not a frame rate must not become the window's
/// minimum (where it would sit for the whole window).
#[test]
fn window_ignores_reports_that_are_not_frame_rates() {
    for bad in [f64::NEG_INFINITY, f64::INFINITY, f64::NAN, -1.0] {
        let mut window = DiagnosticPacketWindow::new(10);
        window.add_packet(1000.0, video_packet(28.0));
        window.add_packet(2000.0, video_packet(bad));
        assert_eq!(
            window.min_fps(),
            Some(28.0),
            "fps={bad} entered the window min"
        );
        assert_eq!(window.latest_fps(), None, "fps={bad} reported as latest");
    }
}

/// If the PID step itself fails (it rejects a non-finite reading or a
/// non-positive dt), the controller holds its previous command instead of
/// dropping to zero reduction (full bitrate) in the middle of congestion.
#[test]
fn pid_step_error_holds_previous_output() {
    let mut sim = Sim::camera();
    sim.run(60, Link::capped(300.0));
    let held = sim.controller.pid_state().last_output;
    assert!(held > 0.0, "precondition: reducing bitrate");
    for (fps, dt) in [
        (f64::NEG_INFINITY, 1.0),
        (f64::NAN, 1.0),
        (20.0, 0.0),
        (20.0, -1.0),
    ] {
        let output = sim.controller.pid_step(fps, dt);
        assert_eq!(output, held, "fps={fps}, dt={dt}");
    }
}
