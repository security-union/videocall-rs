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

//! The traffic shaper's control logic, free of browser APIs.
//!
//! [`ShapingControl`] owns the optional shaper (`None` is off), the user's
//! saved settings, the step test, the connection-state edge, and the timer
//! generations. Every operation takes `now_ms` and
//! returns [`Effects`] for the wasm driver to carry out (send packets, set
//! keyframe flags, arm timers, notify the UI). That keeps all decisions under
//! native `cargo test`.

use super::shaper::{
    Admission, DropReport, Polled, ShapedKind, StepTest, TrafficShaper, TrafficShapingSettings,
    TrafficShapingStatus, STEP_TEST_DURATION_MS,
};

/// Keyframes to force on the local encoders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyframeRequest {
    pub camera: bool,
    pub screen: bool,
}

impl KeyframeRequest {
    fn merge(&mut self, other: KeyframeRequest) {
        self.camera |= other.camera;
        self.screen |= other.screen;
    }
}

impl From<DropReport> for KeyframeRequest {
    fn from(report: DropReport) -> Self {
        Self {
            camera: report.camera,
            screen: report.screen,
        }
    }
}

/// What to do with the drain timer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ArmDecision {
    /// A pending wake at or before the new head time covers it.
    Keep,
    /// Arm a wake for this release time.
    Arm(f64),
    /// The queue is empty: cancel any pending wake.
    Cancel,
}

/// Decide the drain timer from the pending wake and the head's release time.
pub(crate) fn arm_decision(armed_ms: Option<f64>, next_ms: Option<f64>) -> ArmDecision {
    match (armed_ms, next_ms) {
        (_, None) => ArmDecision::Cancel,
        (Some(armed), Some(next)) if armed <= next => ArmDecision::Keep,
        (_, Some(next)) => ArmDecision::Arm(next),
    }
}

/// Whether a connection state report must drop the queue: every state other
/// than Connected does, and so does the edge back into Connected. Staying
/// Connected (a quality re-election) keeps it.
pub(crate) fn connection_drops_queue(was_connected: bool, connected: bool) -> bool {
    !connected || !was_connected
}

/// Send `batch` in order while the queue generation is still `generation`.
/// A send can report a connection change that drops the queue; the rest of
/// the batch belongs to the dead connection and is not sent. Returns how many
/// were sent.
pub(crate) fn send_while_current<P>(
    batch: Vec<P>,
    generation: u64,
    current_generation: impl Fn() -> u64,
    mut send: impl FnMut(P),
) -> usize {
    let mut sent = 0;
    for packet in batch {
        if current_generation() != generation {
            break;
        }
        send(packet);
        sent += 1;
    }
    sent
}

/// A drain wake for the driver to arm.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TimerArm {
    pub delay_ms: u32,
    pub generation: u64,
}

/// Side effects for the driver.
#[derive(Debug)]
pub(crate) struct Effects<P> {
    /// Packets to hand to the transport, in order.
    pub send: Vec<P>,
    /// Queue generation the batch belongs to; see [`send_while_current`].
    pub send_generation: u64,
    pub keyframes: KeyframeRequest,
    pub timer: Option<TimerArm>,
    /// Arm the step test's end: (delay, step generation).
    pub step_timer: Option<(u32, u64)>,
    /// Shaping turned on (`true`) or off (`false`).
    pub notify: Option<bool>,
}

impl<P> Effects<P> {
    fn new(send_generation: u64) -> Self {
        Self {
            send: Vec::new(),
            send_generation,
            keyframes: KeyframeRequest::default(),
            timer: None,
            step_timer: None,
            notify: None,
        }
    }
}

/// Result of [`ShapingControl::admit`].
#[derive(Debug)]
pub(crate) enum Admit<P> {
    /// Off, or a kind outside the scope: send this packet now, untouched.
    Send(P),
    /// Queued; carry out the effects.
    Queued(Effects<P>),
}

#[derive(Debug)]
pub(crate) struct ShapingControl<P> {
    /// The simulated link. `None` is off.
    shaper: Option<TrafficShaper<P>>,
    /// The user's saved settings. A step test never overwrites them.
    settings: TrafficShapingSettings,
    step: Option<StepTest>,
    step_generation: u64,
    was_connected: bool,
    /// Bumped whenever the queue is dropped.
    queue_generation: u64,
    /// Bumped whenever a pending drain wake becomes obsolete.
    timer_generation: u64,
    armed_wake_ms: Option<f64>,
}

impl<P> Default for ShapingControl<P> {
    fn default() -> Self {
        Self {
            shaper: None,
            settings: TrafficShapingSettings::default(),
            step: None,
            step_generation: 0,
            was_connected: false,
            queue_generation: 0,
            timer_generation: 0,
            armed_wake_ms: None,
        }
    }
}

impl<P> ShapingControl<P> {
    pub(crate) fn is_enabled(&self) -> bool {
        self.shaper.is_some()
    }

    /// The user's saved settings.
    pub(crate) fn settings(&self) -> TrafficShapingSettings {
        self.settings
    }

    pub(crate) fn queue_generation(&self) -> u64 {
        self.queue_generation
    }

    /// Whether `kind` would be queued. `false` when off: the hot path's one
    /// presence check.
    pub(crate) fn shapes(&self, kind: ShapedKind) -> bool {
        self.shaper
            .as_ref()
            .is_some_and(|shaper| shaper.settings().scope.shapes(kind))
    }

    /// Offer an outgoing media packet. `bytes_of` is only called when the
    /// packet is actually queued.
    ///
    /// Arrivals clock the shaper too: everything due is popped first and
    /// returned in `send`, so a throttled timer (hidden tab) cannot pile
    /// packets up while media keeps flowing.
    pub(crate) fn admit(
        &mut self,
        now_ms: f64,
        packet: P,
        bytes_of: impl FnOnce(&P) -> usize,
        kind: ShapedKind,
        keyframe: bool,
    ) -> Admit<P> {
        if !self.shapes(kind) {
            return Admit::Send(packet);
        }
        let bytes = bytes_of(&packet);
        let mut effects = Effects::new(self.queue_generation);
        let Some(shaper) = self.shaper.as_mut() else {
            return Admit::Send(packet);
        };
        let Polled { due, report: stale } = shaper.poll(now_ms);
        effects.send = due;
        let report = match shaper.enqueue(now_ms, packet, bytes, kind, keyframe) {
            Admission::Queued(report) => report,
            // Unreachable: the scope was checked above.
            Admission::Bypass(packet) => return Admit::Send(packet),
        };
        let mut dropped = stale;
        dropped.merge(report);
        effects.keyframes = dropped.into();
        effects.timer = self.rearm(now_ms);
        Admit::Queued(effects)
    }

    /// A drain wake fired.
    pub(crate) fn on_timer(&mut self, now_ms: f64, generation: u64) -> Effects<P> {
        let mut effects = Effects::new(self.queue_generation);
        if generation != self.timer_generation {
            return effects;
        }
        self.armed_wake_ms = None;
        let Some(shaper) = self.shaper.as_mut() else {
            return effects;
        };
        let Polled { due, report: stale } = shaper.poll(now_ms);
        effects.send = due;
        effects.keyframes = stale.into();
        effects.timer = self.rearm(now_ms);
        effects
    }

    /// The on/off switch. Using it cancels a running step test; turning on
    /// uses the saved settings.
    pub(crate) fn set_enabled(&mut self, now_ms: f64, enabled: bool) -> Effects<P> {
        let had_step = self.cancel_step();
        if !enabled {
            return self.turn_off();
        }
        let mut effects = self.turn_on(self.settings);
        if had_step {
            // The live shaper ran the step test's settings; go back to the
            // user's.
            effects
                .keyframes
                .merge(self.apply_live(now_ms, self.settings));
            effects.timer = self.rearm(now_ms);
        }
        effects
    }

    /// Save rate, delay or scope. Applies to the live shaper at once, except
    /// during a step test, which keeps its own settings until it ends.
    pub(crate) fn set_settings(
        &mut self,
        now_ms: f64,
        settings: TrafficShapingSettings,
    ) -> Effects<P> {
        self.settings = settings;
        let mut effects = Effects::new(self.queue_generation);
        if self.step.is_none() && self.shaper.is_some() {
            effects.keyframes = self.apply_live(now_ms, settings);
            effects.timer = self.rearm(now_ms);
        }
        effects
    }

    /// 500 kbps, no added delay, audio and video, for 15 s; then off. The
    /// saved settings are left alone.
    pub(crate) fn start_step_test(&mut self, now_ms: f64) -> Effects<P> {
        self.cancel_step();
        let step_settings = TrafficShapingSettings::step_test();
        let mut effects = if self.shaper.is_some() {
            let mut effects = Effects::new(self.queue_generation);
            effects.keyframes = self.apply_live(now_ms, step_settings);
            effects.timer = self.rearm(now_ms);
            effects
        } else {
            self.turn_on(step_settings)
        };
        self.step = Some(StepTest::start(now_ms));
        effects.step_timer = Some((STEP_TEST_DURATION_MS as u32, self.step_generation));
        effects
    }

    /// The step test's timer fired.
    pub(crate) fn on_step_timer(&mut self, generation: u64) -> Effects<P> {
        if generation != self.step_generation || self.step.is_none() {
            return Effects::new(self.queue_generation);
        }
        self.step = None;
        self.turn_off()
    }

    /// A connection state report. See [`connection_drops_queue`]. Settings
    /// and on/off are kept.
    pub(crate) fn on_connection_state(&mut self, connected: bool) -> Effects<P> {
        let was_connected = std::mem::replace(&mut self.was_connected, connected);
        if connection_drops_queue(was_connected, connected) {
            self.drop_queue()
        } else {
            Effects::new(self.queue_generation)
        }
    }

    /// `disconnect()`: drop the queue and end a running step test (shaping
    /// goes off, as it would have at the step's end).
    pub(crate) fn on_disconnect(&mut self) -> Effects<P> {
        let mut effects = self.on_connection_state(false);
        if self.cancel_step() {
            let off = self.turn_off();
            effects.keyframes.merge(off.keyframes);
            effects.notify = off.notify;
            effects.send_generation = off.send_generation;
        }
        effects
    }

    pub(crate) fn status(&self, now_ms: f64) -> Option<TrafficShapingStatus> {
        let shaper = self.shaper.as_ref()?;
        let stats = shaper.stats(now_ms);
        Some(TrafficShapingStatus {
            settings: shaper.settings(),
            queued_bytes: stats.queued_bytes,
            queued_packets: stats.queued_packets,
            head_delay_ms: stats.head_delay_ms,
            dropped_by_cap: stats.dropped_by_cap,
            step_test_remaining_ms: self.step.map(|step| step.remaining_ms(now_ms)),
        })
    }

    /// Returns whether a step test was running.
    fn cancel_step(&mut self) -> bool {
        self.step_generation += 1;
        self.step.take().is_some()
    }

    fn turn_on(&mut self, settings: TrafficShapingSettings) -> Effects<P> {
        let mut effects = Effects::new(self.queue_generation);
        if self.shaper.is_none() {
            self.shaper = Some(TrafficShaper::new(settings));
            effects.notify = Some(true);
        }
        effects
    }

    /// Off drops the queue (it does not flush it): flushing would send a
    /// burst of stale media the instant shaping stops.
    fn turn_off(&mut self) -> Effects<P> {
        let mut effects = self.drop_queue();
        if self.shaper.take().is_some() {
            effects.notify = Some(false);
        }
        effects
    }

    fn drop_queue(&mut self) -> Effects<P> {
        let report = self.shaper.as_mut().map(|shaper| shaper.clear());
        if report.is_some() {
            self.queue_generation += 1;
            self.cancel_timer();
        }
        let mut effects = Effects::new(self.queue_generation);
        if let Some(report) = report {
            effects.keyframes = report.into();
        }
        effects
    }

    fn apply_live(&mut self, now_ms: f64, settings: TrafficShapingSettings) -> KeyframeRequest {
        match self.shaper.as_mut() {
            Some(shaper) => {
                let report = shaper.update_settings(now_ms, settings);
                report.into()
            }
            None => KeyframeRequest::default(),
        }
    }

    fn cancel_timer(&mut self) {
        self.timer_generation += 1;
        self.armed_wake_ms = None;
    }

    fn rearm(&mut self, now_ms: f64) -> Option<TimerArm> {
        let next = self
            .shaper
            .as_ref()
            .and_then(|shaper| shaper.next_wake_ms());
        match arm_decision(self.armed_wake_ms, next) {
            ArmDecision::Keep => None,
            ArmDecision::Cancel => {
                if self.armed_wake_ms.is_some() {
                    self.cancel_timer();
                }
                None
            }
            ArmDecision::Arm(wake_ms) => {
                self.timer_generation += 1;
                self.armed_wake_ms = Some(wake_ms);
                Some(TimerArm {
                    delay_ms: (wake_ms - now_ms).ceil().max(0.0) as u32,
                    generation: self.timer_generation,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::shaper::TrafficShapingScope;
    use super::*;

    fn queued(
        control: &mut ShapingControl<u32>,
        now: f64,
        id: u32,
        bytes: usize,
        kind: ShapedKind,
    ) -> Effects<u32> {
        match control.admit(now, id, |_| bytes, kind, false) {
            Admit::Queued(effects) => effects,
            Admit::Send(_) => panic!("packet {id} was not queued"),
        }
    }

    fn queued_packets(control: &ShapingControl<u32>) -> usize {
        control
            .status(0.0)
            .map_or(0, |status| status.queued_packets)
    }

    fn on(control: &mut ShapingControl<u32>) {
        control.on_connection_state(true);
        let effects = control.set_enabled(0.0, true);
        assert_eq!(effects.notify, Some(true));
    }

    #[test]
    fn off_returns_the_packet_untouched_without_sizing_it() {
        let mut control = ShapingControl::<u32>::default();
        match control.admit(
            0.0,
            42,
            |_| panic!("sized while off"),
            ShapedKind::Camera,
            false,
        ) {
            Admit::Send(packet) => assert_eq!(packet, 42),
            Admit::Queued(_) => panic!("queued while off"),
        }
        assert!(!control.is_enabled());
    }

    #[test]
    fn unshaped_kind_bypasses_without_sizing() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        control.set_settings(
            0.0,
            TrafficShapingSettings {
                scope: TrafficShapingScope::Video,
                ..Default::default()
            },
        );
        assert!(matches!(
            control.admit(0.0, 1, |_| panic!("sized"), ShapedKind::Audio, false),
            Admit::Send(1)
        ));
    }

    #[test]
    fn every_non_connected_state_drops_the_queue() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        queued(&mut control, 0.0, 0, 1_000, ShapedKind::Camera);
        let generation = control.queue_generation();
        let effects = control.on_connection_state(false);
        assert_eq!(queued_packets(&control), 0);
        assert!(effects.keyframes.camera);
        assert_ne!(control.queue_generation(), generation);
        // Still on, settings kept.
        assert!(control.is_enabled());
        // Another non-connected report while queued drops again.
        queued(&mut control, 2.0, 1, 1_000, ShapedKind::Audio);
        control.on_connection_state(false);
        assert_eq!(queued_packets(&control), 0);
    }

    #[test]
    fn edge_back_into_connected_drops_and_reelection_keeps() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        control.on_connection_state(false);
        queued(&mut control, 1.0, 0, 1_000, ShapedKind::Camera);
        control.on_connection_state(true); // edge into Connected
        assert_eq!(queued_packets(&control), 0);
        queued(&mut control, 3.0, 1, 1_000, ShapedKind::Camera);
        control.on_connection_state(true); // re-election: stays Connected
        assert_eq!(queued_packets(&control), 1);
    }

    #[test]
    fn connection_drop_rule_table() {
        assert!(connection_drops_queue(true, false));
        assert!(connection_drops_queue(false, false));
        assert!(connection_drops_queue(false, true));
        assert!(!connection_drops_queue(true, true));
    }

    #[test]
    fn manual_toggle_cancels_the_step_test() {
        let mut control = ShapingControl::<u32>::default();
        let start = control.start_step_test(0.0);
        let (_, generation) = start.step_timer.expect("step timer");
        assert_eq!(start.notify, Some(true));
        // The user turns it off and on again before the step ends.
        control.set_enabled(1.0, false);
        control.set_enabled(2.0, true);
        // The old step timer fires: nothing happens, shaping stays on with the
        // saved settings.
        let effects = control.on_step_timer(generation);
        assert_eq!(effects.notify, None);
        assert!(control.is_enabled());
        assert_eq!(
            control.status(15_000.0).unwrap().settings,
            TrafficShapingSettings::default()
        );
        assert_eq!(
            control.status(15_000.0).unwrap().step_test_remaining_ms,
            None
        );
    }

    #[test]
    fn step_test_ends_with_shaping_off() {
        let mut control = ShapingControl::<u32>::default();
        let (_, generation) = control.start_step_test(0.0).step_timer.unwrap();
        let effects = control.on_step_timer(generation);
        assert_eq!(effects.notify, Some(false));
        assert!(!control.is_enabled());
    }

    #[test]
    fn step_test_does_not_overwrite_saved_settings() {
        let mut control = ShapingControl::<u32>::default();
        let user = TrafficShapingSettings {
            rate_bps: 250_000,
            delay_ms: 150,
            scope: TrafficShapingScope::Video,
        };
        control.set_settings(0.0, user);
        let (_, generation) = control.start_step_test(0.0).step_timer.unwrap();
        assert_eq!(
            control.status(0.0).unwrap().settings,
            TrafficShapingSettings::step_test()
        );
        assert_eq!(control.settings(), user);
        // A preset change during the step is saved but leaves the step alone.
        let user2 = TrafficShapingSettings {
            delay_ms: 50,
            ..user
        };
        control.set_settings(1.0, user2);
        assert_eq!(
            control.status(1.0).unwrap().settings,
            TrafficShapingSettings::step_test()
        );
        control.on_step_timer(generation);
        assert_eq!(control.settings(), user2);
        // Turning on afterwards uses the user's settings.
        control.set_enabled(16_000.0, true);
        assert_eq!(control.status(16_000.0).unwrap().settings, user2);
    }

    #[test]
    fn step_test_on_top_of_running_shaping_restores_user_settings_on_cancel() {
        let mut control = ShapingControl::<u32>::default();
        let user = TrafficShapingSettings {
            rate_bps: 2_000_000,
            ..Default::default()
        };
        control.set_settings(0.0, user);
        on(&mut control);
        control.start_step_test(1.0);
        assert_eq!(control.status(1.0).unwrap().settings.rate_bps, 500_000);
        control.set_enabled(2.0, true); // cancels the step, stays on
        assert_eq!(control.status(2.0).unwrap().settings, user);
    }

    #[test]
    fn disconnect_ends_the_step_test() {
        let mut control = ShapingControl::<u32>::default();
        let (_, generation) = control.start_step_test(0.0).step_timer.unwrap();
        let effects = control.on_disconnect();
        assert_eq!(effects.notify, Some(false));
        assert!(!control.is_enabled());
        // The stale step timer is a no-op.
        assert_eq!(control.on_step_timer(generation).notify, None);
    }

    #[test]
    fn disconnect_without_step_keeps_shaping_on() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        queued(&mut control, 0.0, 0, 1_000, ShapedKind::Camera);
        let effects = control.on_disconnect();
        assert_eq!(effects.notify, None);
        assert!(control.is_enabled());
        assert_eq!(queued_packets(&control), 0);
    }

    #[test]
    fn arrivals_clock_the_shaper() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        // 1 Mbps: 1250 bytes take 10 ms.
        queued(&mut control, 0.0, 0, 1_250, ShapedKind::Camera);
        queued(&mut control, 0.0, 1, 1_250, ShapedKind::Camera);
        // No timer ran; an arrival at 25 ms releases both due packets first.
        let effects = queued(&mut control, 25.0, 2, 1_250, ShapedKind::Camera);
        assert_eq!(effects.send, vec![0, 1]);
        assert_eq!(queued_packets(&control), 1);
    }

    #[test]
    fn stale_packets_found_by_an_arrival_are_dropped_with_a_keyframe() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        queued(&mut control, 0.0, 0, 1_250, ShapedKind::Screen);
        let effects = queued(&mut control, 20_000.0, 1, 1_250, ShapedKind::Camera);
        assert!(effects.send.is_empty());
        assert!(effects.keyframes.screen);
        assert_eq!(control.status(20_000.0).unwrap().dropped_by_cap, 1);
    }

    #[test]
    fn narrowing_scope_forces_keyframes_for_dropped_video() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        queued(&mut control, 0.0, 0, 1_250, ShapedKind::Audio);
        queued(&mut control, 0.0, 1, 1_250, ShapedKind::Camera);
        let effects = control.set_settings(
            1.0,
            TrafficShapingSettings {
                scope: TrafficShapingScope::Audio,
                ..Default::default()
            },
        );
        assert!(effects.keyframes.camera);
        assert_eq!(queued_packets(&control), 1);
    }

    #[test]
    fn arm_decision_table() {
        assert_eq!(arm_decision(None, None), ArmDecision::Cancel);
        assert_eq!(arm_decision(Some(5.0), None), ArmDecision::Cancel);
        assert_eq!(arm_decision(None, Some(5.0)), ArmDecision::Arm(5.0));
        assert_eq!(arm_decision(Some(5.0), Some(7.0)), ArmDecision::Keep);
        assert_eq!(arm_decision(Some(5.0), Some(5.0)), ArmDecision::Keep);
        assert_eq!(arm_decision(Some(9.0), Some(5.0)), ArmDecision::Arm(5.0));
    }

    #[test]
    fn timer_is_armed_only_while_queue_is_non_empty() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        let first = queued(&mut control, 0.0, 0, 1_250, ShapedKind::Camera);
        let arm = first.timer.expect("armed on first packet");
        assert_eq!(arm.delay_ms, 10);
        // A second packet behind it does not re-arm.
        assert!(queued(&mut control, 0.0, 1, 1_250, ShapedKind::Camera)
            .timer
            .is_none());
        // Wake: releases id 0, re-arms for id 1.
        let wake = control.on_timer(10.0, arm.generation);
        assert_eq!(wake.send, vec![0]);
        let rearm = wake.timer.expect("re-armed");
        // Last wake empties the queue: no timer.
        let last = control.on_timer(20.0, rearm.generation);
        assert_eq!(last.send, vec![1]);
        assert!(last.timer.is_none());
        // A stale wake is ignored.
        assert!(control.on_timer(30.0, arm.generation).send.is_empty());
    }

    #[test]
    fn send_stops_when_the_queue_is_dropped_mid_batch() {
        let generation = std::cell::Cell::new(7);
        let mut sent = Vec::new();
        let count = send_while_current(
            vec![1, 2, 3],
            7,
            || generation.get(),
            |packet| {
                sent.push(packet);
                // The first send reports a lost connection.
                generation.set(8);
            },
        );
        assert_eq!(count, 1);
        assert_eq!(sent, vec![1]);
    }

    #[test]
    fn no_deferred_keyframe_request_survives_off_and_on() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        control.set_settings(
            0.0,
            TrafficShapingSettings {
                rate_bps: 250_000,
                delay_ms: 0,
                scope: TrafficShapingScope::Both,
            },
        );
        // A kept keyframe whose tail the cap drops: a request is deferred until
        // the keyframe reaches the link at 320 ms.
        queued(&mut control, 0.0, 0, 10_000, ShapedKind::Camera);
        let key = control.admit(0.0, 1, |_| 200_000, ShapedKind::Camera, true);
        assert!(matches!(key, Admit::Queued(_)));
        queued(&mut control, 0.0, 2, 100_000, ShapedKind::Camera);
        assert!(
            !queued(&mut control, 10.0, 3, 50_000, ShapedKind::Camera)
                .keyframes
                .camera
        );
        // Off drops the queue and asks for a keyframe at once.
        assert!(control.set_enabled(20.0, false).keyframes.camera);
        control.set_enabled(30.0, true);
        let mut now = 40.0;
        while now < 2_000.0 {
            let effects = queued(&mut control, now, now as u32, 100, ShapedKind::Camera);
            assert!(
                !effects.keyframes.camera,
                "request fired at {now} ms after off/on"
            );
            assert!(!control.on_timer(now, u64::MAX).keyframes.camera);
            now += 33.0;
        }
    }

    #[test]
    fn dropped_by_cap_resets_when_shaping_is_turned_back_on() {
        let mut control = ShapingControl::<u32>::default();
        on(&mut control);
        queued(&mut control, 0.0, 0, 1_250, ShapedKind::Camera);
        queued(&mut control, 20_000.0, 1, 1_250, ShapedKind::Camera); // id 0 stale
        assert_eq!(control.status(20_000.0).unwrap().dropped_by_cap, 1);
        control.set_enabled(20_001.0, false);
        control.set_enabled(20_002.0, true);
        assert_eq!(control.status(20_002.0).unwrap().dropped_by_cap, 0);
    }
}
