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

use super::connection_manager::{ConnectionManager, ConnectionManagerOptions, ConnectionState};
use crate::crypto::aes::Aes128State;
use anyhow::{anyhow, Result};
use gloo::timers::callback::Interval;
use log::{debug, info};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[derive(Debug)]
pub struct ConnectionController {
    inner: Rc<RefCell<ConnectionControllerInner>>,
    _timers: Vec<Interval>, // Keep timers alive
}

#[derive(Debug)]
struct ConnectionControllerInner {
    connection_manager: ConnectionManager,
}

impl ConnectionController {
    /// Create a new ConnectionController with timer management
    pub fn new(options: ConnectionManagerOptions, aes: Rc<Aes128State>) -> Result<Self> {
        info!("Creating ConnectionController with timer management");

        let connection_manager = ConnectionManager::new(options.clone(), aes.clone())?;

        let inner = Rc::new(RefCell::new(ConnectionControllerInner {
            connection_manager,
        }));

        let timers = Self::start_timers(Rc::downgrade(&inner));

        info!("ConnectionController created with all timers started");
        Ok(Self {
            inner,
            _timers: timers,
        })
    }

    /// Start all necessary timers for connection management
    fn start_timers(inner_weak: Weak<RefCell<ConnectionControllerInner>>) -> Vec<Interval> {
        let mut timers = Vec::new();

        // 1Hz diagnostics reporting timer
        let inner_ref = inner_weak.clone();
        timers.push(Interval::new(1000, move || {
            if let Some(inner) = inner_ref.upgrade() {
                if let Ok(mut inner) = inner.try_borrow_mut() {
                    // Always drive diagnostics once per second
                    inner.connection_manager.trigger_diagnostics_report();

                    // After election, probe RTT at 1 Hz
                    if matches!(
                        inner.connection_manager.get_connection_state(),
                        ConnectionState::Connected { .. }
                    ) {
                        if let Err(e) = inner.connection_manager.send_rtt_probes() {
                            debug!("Failed to send 1Hz RTT probe post-election: {e}");
                        }
                    }
                }
            }
        }));

        // RTT probing timer (200ms intervals) - only during election (Testing)
        let inner_ref = inner_weak.clone();
        timers.push(Interval::new(200, move || {
            if let Some(inner) = inner_ref.upgrade() {
                if let Ok(mut inner) = inner.try_borrow_mut() {
                    if matches!(
                        inner.connection_manager.get_connection_state(),
                        ConnectionState::Testing { .. }
                    ) {
                        if let Err(e) = inner.connection_manager.send_rtt_probes() {
                            debug!("Failed to send RTT probes during election: {e}");
                        }
                    }
                }
            }
        }));

        // Election completion checking timer (100ms intervals)
        let inner_ref = inner_weak.clone();
        timers.push(Interval::new(100, move || {
            if let Some(inner) = inner_ref.upgrade() {
                if let Ok(mut inner) = inner.try_borrow_mut() {
                    inner.connection_manager.check_and_complete_election();
                }
            }
        }));

        info!("All ConnectionController timers started");
        timers
    }

    // Delegate methods to ConnectionManager

    /// Send packet through active connection
    pub fn send_packet(&self, packet: PacketWrapper) -> Result<()> {
        let inner = self
            .inner
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionController inner"))?;
        inner.connection_manager.send_packet(packet)
    }

    /// Set video enabled on active connection
    pub fn set_video_enabled(&self, enabled: bool) -> Result<()> {
        let inner = self
            .inner
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionController inner"))?;
        inner.connection_manager.set_video_enabled(enabled)
    }

    /// Set audio enabled on active connection
    pub fn set_audio_enabled(&self, enabled: bool) -> Result<()> {
        let inner = self
            .inner
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionController inner"))?;
        inner.connection_manager.set_audio_enabled(enabled)
    }

    /// Set screen enabled on active connection
    pub fn set_screen_enabled(&self, enabled: bool) -> Result<()> {
        let inner = self
            .inner
            .try_borrow()
            .map_err(|_| anyhow!("Failed to borrow ConnectionController inner"))?;
        inner.connection_manager.set_screen_enabled(enabled)
    }

    pub fn set_speaking(&self, speaking: bool) {
        if let Ok(inner) = self.inner.try_borrow() {
            inner.connection_manager.set_speaking(speaking);
        }
    }

    pub fn is_connected(&self) -> bool {
        if let Ok(inner) = self.inner.try_borrow() {
            inner.connection_manager.is_connected()
        } else {
            false
        }
    }

    /// Disconnect from the current connection and clean up resources
    pub fn disconnect(&self) -> anyhow::Result<()> {
        let mut inner = self
            .inner
            .try_borrow_mut()
            .map_err(|_| anyhow!("Failed to borrow ConnectionController inner"))?;
        inner.connection_manager.disconnect()
    }

    /// Get current connection state for UI
    pub fn get_connection_state(&self) -> ConnectionState {
        if let Ok(inner) = self.inner.try_borrow() {
            inner.connection_manager.get_connection_state()
        } else {
            ConnectionState::Failed {
                error: "Failed to borrow ConnectionController inner".to_string(),
                last_known_server: None,
            }
        }
    }

    /// Get current RTT measurements (for debugging) - returns an empty HashMap if borrowing fails
    pub fn get_rtt_measurements_clone(
        &self,
    ) -> std::collections::HashMap<String, super::connection_manager::ServerRttMeasurement> {
        if let Ok(inner) = self.inner.try_borrow() {
            inner.connection_manager.get_rtt_measurements().clone()
        } else {
            std::collections::HashMap::new()
        }
    }
}

impl Drop for ConnectionController {
    fn drop(&mut self) {
        info!("Dropping ConnectionController and cleaning up timers");

        // Timers are automatically cleaned up when the Vec<Interval> is dropped
        // Each Interval's Drop implementation will cancel the timer
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use yew::prelude::Callback;

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
            on_speaking_changed: None,
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