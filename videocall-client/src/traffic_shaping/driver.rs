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

//! Browser-side driver for [`ShapingControl`].
//!
//! It only carries out [`Effects`]: hand due packets to the same
//! `ConnectionController::send_packet` call the unshaped path uses (so
//! WebSocket and WebTransport see them like normal media), set the encoders'
//! keyframe flags, arm one-shot timers, and notify the UI. Every decision is
//! in `control.rs`, under native tests.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use gloo_timers::future::TimeoutFuture;
use protobuf::Message;
use videocall_types::protos::packet_wrapper::PacketWrapper;
use videocall_types::Callback;

use super::control::{send_while_current, Admit, Effects, ShapingControl};
use super::shaper::{ShapedKind, TrafficShapingSettings, TrafficShapingStatus};
use crate::client::send_media_to_transport;
use crate::connection::ConnectionController;

/// Monotonic milliseconds from `performance.now()`, falling back to
/// `Date.now()` where the Performance API is missing.
fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map(|performance| performance.now())
        .unwrap_or_else(js_sys::Date::now)
}

#[derive(Debug)]
pub(crate) struct TrafficShaping {
    self_ref: Weak<TrafficShaping>,
    control: RefCell<ShapingControl<PacketWrapper>>,
    listener: RefCell<Option<Callback<bool>>>,
    connection_controller: Rc<RefCell<Option<ConnectionController>>>,
    force_camera_keyframe: Arc<AtomicBool>,
    force_screen_keyframe: Arc<AtomicBool>,
}

impl TrafficShaping {
    pub(crate) fn new(
        connection_controller: Rc<RefCell<Option<ConnectionController>>>,
        force_camera_keyframe: Arc<AtomicBool>,
        force_screen_keyframe: Arc<AtomicBool>,
    ) -> Rc<Self> {
        Rc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            control: RefCell::new(ShapingControl::default()),
            listener: RefCell::new(None),
            connection_controller,
            force_camera_keyframe,
            force_screen_keyframe,
        })
    }

    /// Hot path, called for every encoded media packet.
    ///
    /// Off (or a kind outside the scope): hands the packet straight back for
    /// the caller to send as usual. On: queues it and returns `None`.
    pub(crate) fn shape(
        &self,
        packet: PacketWrapper,
        kind: ShapedKind,
        keyframe: bool,
    ) -> Option<PacketWrapper> {
        let Ok(mut control) = self.control.try_borrow_mut() else {
            // Only reachable if the shaper is re-entered from inside itself.
            log::warn!("Traffic shaping: control busy, sending {kind:?} packet unshaped");
            return Some(packet);
        };
        // Off: one `Option` check, nothing else.
        if !control.shapes(kind) {
            return Some(packet);
        }
        // Size on the wire: the serialized PacketWrapper. Transport framing
        // (WebSocket frame header, WebTransport stream prefix) is not counted.
        let admitted = control.admit(
            now_ms(),
            packet,
            |packet| packet.compute_size() as usize,
            kind,
            keyframe,
        );
        drop(control);
        match admitted {
            Admit::Send(packet) => Some(packet),
            Admit::Queued(effects) => {
                self.run(effects);
                None
            }
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.control.borrow().is_enabled()
    }

    pub(crate) fn settings(&self) -> TrafficShapingSettings {
        self.control.borrow().settings()
    }

    pub(crate) fn set_enabled(&self, enabled: bool) {
        let effects = self.control.borrow_mut().set_enabled(now_ms(), enabled);
        self.run(effects);
    }

    pub(crate) fn set_settings(&self, settings: TrafficShapingSettings) {
        let effects = self.control.borrow_mut().set_settings(now_ms(), settings);
        self.run(effects);
    }

    pub(crate) fn start_step_test(&self) {
        let effects = self.control.borrow_mut().start_step_test(now_ms());
        self.run(effects);
    }

    pub(crate) fn status(&self) -> Option<TrafficShapingStatus> {
        self.control.borrow().status(now_ms())
    }

    /// Set or clear (`None`) the on/off listener.
    pub(crate) fn set_listener(&self, listener: Option<Callback<bool>>) {
        *self.listener.borrow_mut() = listener;
    }

    /// Called on every connection state report.
    pub(crate) fn on_connection_state(&self, connected: bool) {
        let effects = self.control.borrow_mut().on_connection_state(connected);
        self.run(effects);
    }

    /// Called from `VideoCallClient::disconnect()`.
    pub(crate) fn on_disconnect(&self) {
        let effects = self.control.borrow_mut().on_disconnect();
        self.run(effects);
    }

    /// Carry out effects. Must be called with the control borrow released:
    /// a send may report a connection change, which re-enters the control.
    fn run(&self, effects: Effects<PacketWrapper>) {
        let Effects {
            send,
            send_generation,
            keyframes,
            timer,
            step_timer,
            notify,
        } = effects;

        // Ask the local encoders for a keyframe through the same flags an
        // incoming KEYFRAME_REQUEST sets.
        if keyframes.camera {
            self.force_camera_keyframe.store(true, Ordering::Release);
        }
        if keyframes.screen {
            self.force_screen_keyframe.store(true, Ordering::Release);
        }

        // Browsers clamp short timers and throttle hidden tabs. Arrivals also
        // clock the shaper, and packets overdue by more than the cap are
        // dropped, so a late wake cannot burst stale media.
        if let Some(arm) = timer {
            let weak = self.self_ref.clone();
            wasm_bindgen_futures::spawn_local(async move {
                TimeoutFuture::new(arm.delay_ms).await;
                if let Some(this) = weak.upgrade() {
                    let effects = this.control.borrow_mut().on_timer(now_ms(), arm.generation);
                    this.run(effects);
                }
            });
        }
        if let Some((delay_ms, generation)) = step_timer {
            let weak = self.self_ref.clone();
            wasm_bindgen_futures::spawn_local(async move {
                TimeoutFuture::new(delay_ms).await;
                if let Some(this) = weak.upgrade() {
                    let effects = this.control.borrow_mut().on_step_timer(generation);
                    this.run(effects);
                }
            });
        }

        send_while_current(
            send,
            send_generation,
            || self.control.borrow().queue_generation(),
            |packet| {
                send_media_to_transport(&self.connection_controller, packet);
            },
        );

        if let Some(enabled) = notify {
            log::info!("Traffic shaping {}", if enabled { "on" } else { "off" });
            let listener = self.listener.borrow().clone();
            if let Some(listener) = listener {
                listener.emit(enabled);
            }
        }
    }
}
