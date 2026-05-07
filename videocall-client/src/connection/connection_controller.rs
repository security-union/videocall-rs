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

use super::connection_manager::{
    monotonic_now_ms, ConnectionManager, ConnectionManagerOptions, ConnectionState,
    CPU_OVERLOADED_DURATION_MS, CPU_OVERLOAD_DRIFT_THRESHOLD_MS,
};
use crate::crypto::aes::Aes128State;
use anyhow::{anyhow, Result};
use gloo::timers::callback::Interval;
use log::{debug, info, warn};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[derive(Debug)]
pub struct ConnectionController {
    manager: Rc<RefCell<ConnectionManager>>,
    _timers: Vec<Interval>, // Keep timers alive
}

impl ConnectionController {
    /// Create a new ConnectionController with timer management
    pub fn new(options: ConnectionManagerOptions, aes: Rc<Aes128State>) -> Result<Self> {
        info!("Creating ConnectionController with timer management");

        let manager = Rc::new(RefCell::new(ConnectionManager::new(
            options.clone(),
            aes.clone(),
        )?));

        // Provide the manager with a weak self-reference so its reconnection
        // callbacks can call `reset_and_start_election` on the real instance
        // instead of creating a throwaway manager.
        manager
            .borrow_mut()
            .set_manager_ref(Rc::downgrade(&manager));

        // Wire the shared CPU-overload signal between this controller's
        // drift watchdog (in `start_timers` below) and the manager's
        // `check_rtt_degradation`. The flag is the kill-switch for spurious
        // re-elections caused by JS event-loop stalls.
        let cpu_overloaded = Rc::new(AtomicBool::new(false));
        let main_thread_drift_ms = Rc::new(RefCell::new(0.0_f64));
        manager
            .borrow_mut()
            .set_cpu_overloaded_signal(cpu_overloaded.clone(), main_thread_drift_ms.clone());

        // Start the initial election AFTER set_manager_ref so that
        // connection-lost callbacks capture a valid Weak back-reference.
        manager.borrow_mut().initialize()?;

        let timers = Self::start_timers(
            Rc::downgrade(&manager),
            cpu_overloaded,
            main_thread_drift_ms,
        );

        info!("ConnectionController created with all timers started");
        Ok(Self {
            manager,
            _timers: timers,
        })
    }

    /// Start all necessary timers for connection management.
    ///
    /// `cpu_overloaded` and `main_thread_drift_ms` are the shared drift-watchdog
    /// signals already handed to the `ConnectionManager` in `new()`. The 1 Hz
    /// timer measures `performance.now()` drift relative to its scheduled
    /// cadence; when a tick runs more than [`CPU_OVERLOAD_DRIFT_THRESHOLD_MS`]
    /// late, the flag is asserted for [`CPU_OVERLOADED_DURATION_MS`] so the
    /// manager-side guard suppresses re-election while the local main thread
    /// is overloaded.
    fn start_timers(
        mgr_weak: Weak<RefCell<ConnectionManager>>,
        cpu_overloaded: Rc<AtomicBool>,
        main_thread_drift_ms: Rc<RefCell<f64>>,
    ) -> Vec<Interval> {
        let mut timers = Vec::new();

        // Drift watchdog state. `last_tick` is the wall-clock reading of the
        // *previous* 1 Hz tick; on each new tick we compute how much real
        // time elapsed and compare against the 1000 ms cadence. `clear_at_ms`
        // is the timestamp when the suppression flag should be allowed to
        // drop back to false.
        let last_tick = Rc::new(RefCell::new(monotonic_now_ms()));
        let clear_at_ms = Rc::new(RefCell::new(0.0_f64));

        // 1Hz diagnostics reporting timer + RTT degradation monitoring
        let mgr_ref = mgr_weak.clone();
        let cpu_overloaded_t = cpu_overloaded.clone();
        let drift_ref = main_thread_drift_ms.clone();
        let last_tick_t = last_tick.clone();
        let clear_at_t = clear_at_ms.clone();
        timers.push(Interval::new(1000, move || {
            // --- Main-thread drift measurement ---------------------------
            // Run BEFORE the diagnostics report so the published metric
            // reflects this tick's drift, not the previous one.
            let now = monotonic_now_ms();
            let actual_delta = now - *last_tick_t.borrow();
            *last_tick_t.borrow_mut() = now;
            // `drift` is how much later than the 1000 ms cadence this tick
            // fired. Negative values are clamped to 0 — running early means
            // the timer was fine.
            let drift = (actual_delta - 1000.0).max(0.0);
            *drift_ref.borrow_mut() = drift;

            if drift > CPU_OVERLOAD_DRIFT_THRESHOLD_MS {
                let new_clear_at = now + CPU_OVERLOADED_DURATION_MS;
                // Scope the RefCell borrow narrowly so it cannot interleave
                // with the warn! call below (defense-in-depth — there is no
                // current code path that re-enters this map, but logging is
                // permitted to acquire arbitrary thread-local resources).
                {
                    let mut clear_at = clear_at_t.borrow_mut();
                    // Extend, never retract: a fresh stall during the
                    // suppression window pushes the deadline forward.
                    if new_clear_at > *clear_at {
                        *clear_at = new_clear_at;
                    }
                }
                if !cpu_overloaded_t.swap(true, Ordering::Relaxed) {
                    log::warn!(
                        "CPU-overload watchdog: main-thread drift {drift:.0}ms exceeded \
                         threshold {CPU_OVERLOAD_DRIFT_THRESHOLD_MS:.0}ms — suppressing \
                         re-election for {CPU_OVERLOADED_DURATION_MS:.0}ms",
                    );
                }
            } else if cpu_overloaded_t.load(Ordering::Relaxed) {
                let clear_at = *clear_at_t.borrow();
                if now >= clear_at {
                    cpu_overloaded_t.store(false, Ordering::Relaxed);
                    log::info!(
                        "CPU-overload watchdog: main-thread drift recovered \
                         (last drift {drift:.0}ms) — clearing suppression flag",
                    );
                }
            }

            if let Some(mgr) = mgr_ref.upgrade() {
                if let Ok(mut mgr) = mgr.try_borrow_mut() {
                    // Always drive diagnostics once per second
                    mgr.trigger_diagnostics_report();

                    // After election, probe RTT at 1 Hz and check for degradation
                    if matches!(
                        mgr.get_connection_state(),
                        ConnectionState::Connected { .. }
                    ) {
                        if let Err(e) = mgr.send_rtt_probes() {
                            debug!("Failed to send 1Hz RTT probe post-election: {e}");
                        }

                        // Check whether the active connection's RTT has degraded
                        // enough to warrant a quality re-election.
                        //
                        // We call `request_reelection` instead of
                        // `start_reelection` directly so that, when a
                        // refresh callback is configured (Phase 3 /
                        // AUTH-2), the manager refreshes the room token
                        // before spawning candidates. This prevents
                        // post-TTL re-elections from cascade-failing with
                        // all candidates rejected by the relay. See
                        // discussion #562.
                        if mgr.check_rtt_degradation() {
                            if let Err(e) = mgr.request_reelection() {
                                log::error!("Failed to start re-election: {e}");
                            }
                        }
                    }
                } else {
                    warn!("1Hz diagnostics timer: skipped — ConnectionManager already borrowed");
                }
            }
        }));

        // RTT probing timer (200ms intervals) - only during election (Testing)
        let mgr_ref = mgr_weak.clone();
        timers.push(Interval::new(200, move || {
            if let Some(mgr) = mgr_ref.upgrade() {
                if let Ok(mut mgr) = mgr.try_borrow_mut() {
                    if matches!(mgr.get_connection_state(), ConnectionState::Testing { .. }) {
                        if let Err(e) = mgr.send_rtt_probes() {
                            debug!("Failed to send RTT probes during election: {e}");
                        }
                    }
                } else {
                    warn!("200ms RTT probe timer: skipped — ConnectionManager already borrowed");
                }
            }
        }));

        // Election completion checking timer (100ms intervals)
        let mgr_ref = mgr_weak.clone();
        timers.push(Interval::new(100, move || {
            if let Some(mgr) = mgr_ref.upgrade() {
                if let Ok(mut mgr) = mgr.try_borrow_mut() {
                    mgr.check_and_complete_election();
                } else {
                    warn!(
                        "100ms election check timer: skipped — ConnectionManager already borrowed"
                    );
                }
            }
        }));

        info!("All ConnectionController timers started");
        timers
    }

    // Delegate methods to ConnectionManager

    /// Send packet through active connection via reliable stream.
    pub fn send_packet(&self, packet: PacketWrapper) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.send_packet(packet)
    }

    /// Send packet through active connection via datagram (unreliable, low-latency).
    ///
    /// Used for control packets (heartbeats, RTT probes, diagnostics) that are
    /// periodic and expendable — lower overhead matters more than guaranteed
    /// delivery. Falls back to reliable stream for WebSocket connections or
    /// oversized packets.
    #[allow(dead_code)]
    pub fn send_packet_datagram(&self, packet: PacketWrapper) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.send_packet_datagram(packet)
    }

    /// Set video enabled on active connection
    pub fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.set_video_enabled(enabled)
    }

    /// Set audio enabled on active connection
    pub fn set_audio_enabled(&self, enabled: bool) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.set_audio_enabled(enabled)
    }

    /// Set screen enabled on active connection
    pub fn set_screen_enabled(&self, enabled: bool) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.set_screen_enabled(enabled)
    }

    /// Set speaking state on active connection
    pub fn set_speaking(&self, speaking: bool) {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.set_speaking(speaking);
        }
    }

    /// Set own session_id for filtering self-packets
    pub fn set_own_session_id(&self, session_id: u64) -> Result<()> {
        let mgr = self
            .manager
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.set_own_session_id(session_id);
        Ok(())
    }

    /// Replace the WebSocket / WebTransport server URLs that the underlying
    /// `ConnectionManager` will consider on the next election or post-rebase
    /// retry.
    ///
    /// Without this hop, `VideoCallClient::update_server_urls` would only
    /// update the outer client's options copy and the manager would keep
    /// reading its stale URL list, defeating the post-rebase retry's whole
    /// reason for existing (re-evaluating candidate availability after a
    /// token refresh).
    pub fn update_server_urls(
        &self,
        websocket_urls: Vec<String>,
        webtransport_urls: Vec<String>,
    ) -> Result<()> {
        let mut mgr = self
            .manager
            .try_borrow_mut()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.update_server_urls(websocket_urls, webtransport_urls);
        Ok(())
    }

    /// Check if manager has an active connection
    pub fn is_connected(&self) -> bool {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.is_connected()
        } else {
            false
        }
    }

    /// Disconnect from the current connection and clean up resources
    pub fn disconnect(&self) -> anyhow::Result<()> {
        let mut mgr = self
            .manager
            .try_borrow_mut()
            .map_err(|_| anyhow!("Failed to borrow ConnectionManager"))?;
        mgr.disconnect()
    }

    /// Get current connection state for UI
    pub fn get_connection_state(&self) -> ConnectionState {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.get_connection_state()
        } else {
            ConnectionState::Failed {
                error: "Failed to borrow ConnectionManager".to_string(),
                last_known_server: None,
            }
        }
    }

    /// Get current RTT measurements (for debugging) - returns an empty HashMap if borrowing fails
    pub fn get_rtt_measurements_clone(
        &self,
    ) -> std::collections::HashMap<String, super::connection_manager::ServerRttMeasurement> {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.get_rtt_measurements().clone()
        } else {
            std::collections::HashMap::new()
        }
    }

    /// Calculate packet rates per second for health reporting
    pub fn calculate_packet_rates(&self) {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.calculate_packet_rates();
        }
    }

    /// Get packets received per second (should be called after calculate_packet_rates)
    pub fn get_packets_received_per_sec(&self) -> f64 {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.get_packets_received_per_sec()
        } else {
            0.0
        }
    }

    /// Get packets sent per second (should be called after calculate_packet_rates)
    pub fn get_packets_sent_per_sec(&self) -> f64 {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.get_packets_sent_per_sec()
        } else {
            0.0
        }
    }

    /// Get send queue depth from the active connection (bufferedAmount for WebSocket)
    pub fn get_send_queue_depth(&self) -> Option<u64> {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.get_send_queue_depth()
        } else {
            None
        }
    }

    /// Returns the shared re-election completed signal.
    ///
    /// Forwards to [`ConnectionManager::reelection_completed_signal`].
    pub fn reelection_completed_signal(&self) -> Rc<AtomicBool> {
        if let Ok(mgr) = self.manager.try_borrow() {
            mgr.reelection_completed_signal()
        } else {
            // Fallback: return a standalone AtomicBool. This only happens if
            // the manager is already borrowed (should not occur during setup).
            log::warn!("ConnectionController: manager borrowed during reelection_completed_signal — returning disconnected signal");
            Rc::new(AtomicBool::new(false))
        }
    }
}

impl Drop for ConnectionController {
    fn drop(&mut self) {
        info!("Dropping ConnectionController and cleaning up timers");

        // Signal intentional disconnect so any in-flight reconnection loops
        // stop instead of continuing to run against a dropped controller.
        if let Ok(mut mgr) = self.manager.try_borrow_mut() {
            mgr.disconnect().ok();
        }

        // Timers are automatically cleaned up when the Vec<Interval> is dropped
        // Each Interval's Drop implementation will cancel the timer
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use videocall_types::Callback;

    // Test helper to capture state changes
    #[derive(Debug, Clone)]
    struct StateCapture {
        states: Arc<Mutex<Vec<ConnectionState>>>,
    }

    impl StateCapture {
        fn new() -> Self {
            Self {
                states: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn callback(&self) -> Callback<ConnectionState> {
            let states = self.states.clone();
            Callback::from(move |state: ConnectionState| {
                states.lock().unwrap().push(state);
            })
        }

        fn get_states(&self) -> Vec<ConnectionState> {
            self.states.lock().unwrap().clone()
        }

        fn last_state(&self) -> Option<ConnectionState> {
            self.states.lock().unwrap().last().cloned()
        }
    }

    // Helper to create test options
    fn create_test_options(state_capture: &StateCapture) -> ConnectionManagerOptions {
        ConnectionManagerOptions {
            websocket_urls: vec!["ws://localhost:8080".to_string()],
            webtransport_urls: vec!["https://localhost:8443".to_string()],
            userid: "test_user".to_string(),
            on_inbound_media: Callback::from(|_| {}),
            on_state_changed: state_capture.callback(),
            peer_monitor: Callback::from(|_| {}),
            election_period_ms: 1000,
            instance_id: "test-instance-id".to_string(),
            reelection_completed_signal: Rc::new(AtomicBool::new(false)),
            allow_post_rebase_retry: true,
            refresh_room_token_callback: None,
        }
    }

    // Create test AES state
    fn create_test_aes() -> Rc<Aes128State> {
        let key = vec![1u8; 16];
        let iv = vec![2u8; 16];
        Rc::new(Aes128State::from_vecs(key, iv, true))
    }

    #[test]
    fn test_connection_controller_creation() {
        let state_capture = StateCapture::new();
        let options = create_test_options(&state_capture);
        let aes = create_test_aes();

        // Test that ConnectionController can be created
        let _controller = ConnectionController::new(options, aes);
        // Just testing that creation works without panicking
    }

    #[test]
    fn test_connection_controller_delegation() {
        let state_capture = StateCapture::new();
        let options = create_test_options(&state_capture);
        let aes = create_test_aes();

        if let Ok(controller) = ConnectionController::new(options, aes) {
            // Test that methods can be called without panicking
            let _is_connected = controller.is_connected();
            let _state = controller.get_connection_state();
            let _measurements = controller.get_rtt_measurements_clone();
        }
    }
}
