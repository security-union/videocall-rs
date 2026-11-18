// Port of WebRTC api/neteq/delay_manager_interface.h at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2019 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The interface `DecisionLogic` uses to talk to its delay manager.

use crate::api::neteq_controller::PacketArrivedInfo;

/// C++ `DelayManagerInterface`. Implemented by `neteq::delay_manager::DelayManager`
/// and by test fakes.
pub trait DelayManagerInterface: Send {
    /// A new packet arrived with delay `arrival_delay_ms`. Updates the
    /// statistics and computes a new target buffer level. `reordered` is true
    /// if the packet was reordered.
    fn update(&mut self, arrival_delay_ms: i32, reordered: bool, info: PacketArrivedInfo);

    /// Resets all state.
    fn reset(&mut self);

    /// Target buffer level in milliseconds.
    fn target_delay_ms(&self) -> i32;
}
