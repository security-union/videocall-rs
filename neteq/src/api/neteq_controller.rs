// Port of WebRTC api/neteq/neteq_controller.h at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd.
// Copyright (c) 2019 The WebRTC project authors. BSD-3-Clause, see LICENSE-WEBRTC.

//! The interface between `NetEqImpl` and its decision maker
//! (`DecisionLogic` by default).

use crate::api::neteq::{Mode, Operation};
use crate::api::tick_timer::TickTimer;

/// C++ `NetEqController::Config`. Used to create a controller.
#[derive(Debug, Clone)]
pub struct NetEqControllerConfig {
    pub allow_time_stretching: bool,
    pub enable_rtx_handling: bool,
    pub max_packets_in_buffer: i32,
    pub base_min_delay_ms: i32,
    /// Shared handle onto NetEq's tick timer.
    pub tick_timer: TickTimer,
}

/// C++ `NetEqController::PacketInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketInfo {
    pub timestamp: u32,
    pub is_dtx: bool,
    pub is_cng: bool,
}

/// C++ `NetEqController::PacketBufferInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketBufferInfo {
    pub dtx_or_cng: bool,
    pub num_samples: usize,
    pub span_samples: usize,
    pub span_samples_wait_time: usize,
    pub num_packets: usize,
}

/// C++ `NetEqController::NetEqStatus`: the input to `get_decision`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetEqStatus {
    pub target_timestamp: u32,
    pub expand_mutefactor: i16,
    pub last_packet_samples: usize,
    pub next_packet: Option<PacketInfo>,
    pub last_mode: Mode,
    pub play_dtmf: bool,
    pub generated_noise_samples: usize,
    pub packet_buffer_info: PacketBufferInfo,
    pub sync_buffer_samples: usize,
}

/// C++ `NetEqController::PacketArrivedInfo`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketArrivedInfo {
    pub packet_length_samples: usize,
    pub main_timestamp: u32,
    pub main_sequence_number: u16,
    pub is_cng_or_dtmf: bool,
    pub is_dtx: bool,
    pub buffer_flush: bool,
}

/// C++ `NetEqController`. Decides what NetEq does each `GetAudio`.
pub trait NetEqController: Send {
    /// Resets object to a clean state.
    fn reset(&mut self);

    /// Resets parts of the state. Typically done when switching codecs.
    fn soft_reset(&mut self);

    /// Given the NetEq status, returns the operation. Sets `*reset_decoder`
    /// to true if a decoder reset is required; otherwise leaves it unchanged.
    fn get_decision(&mut self, status: &NetEqStatus, reset_decoder: &mut bool) -> Operation;

    /// Informs the controller that an empty packet has arrived.
    fn register_empty_packet(&mut self);

    /// Sets the sample rate and the output block size.
    fn set_sample_rate(&mut self, fs_hz: i32, output_size_samples: usize);

    /// Sets a maximum delay in ms. Returns true if applied.
    fn set_maximum_delay(&mut self, delay_ms: i32) -> bool;

    /// Sets a minimum delay in ms. Returns true if applied.
    fn set_minimum_delay(&mut self, delay_ms: i32) -> bool;

    /// Sets a base minimum delay in ms. Returns true if applied.
    fn set_base_minimum_delay(&mut self, delay_ms: i32) -> bool;

    /// Returns the base minimum delay in ms.
    fn get_base_minimum_delay(&self) -> i32;

    /// Reports back whether an expand decision remained (NetEqImpl may turn
    /// it into Normal if the sync buffer still has enough data).
    fn expand_decision(&mut self, operation: Operation);

    /// Adds `value` to `sample_memory`.
    fn add_sample_memory(&mut self, value: i32);

    /// Target buffer level in ms.
    fn target_level_ms(&self) -> i32;

    /// Target buffer level in ms ignoring any minimum or maximum delay.
    fn unlimited_target_level_ms(&self) -> i32 {
        0
    }

    /// Notifies the controller that a packet has arrived. Returns the
    /// relative arrival delay, if it can be computed.
    fn packet_arrived(
        &mut self,
        fs_hz: i32,
        should_update_stats: bool,
        info: &PacketArrivedInfo,
    ) -> Option<i32>;

    /// Notifies the controller that NetEq is in muted state.
    fn notify_muted_state(&mut self) {}

    /// True if a delay peak was found.
    fn peak_found(&self) -> bool;

    /// Filtered buffer level in samples.
    fn get_filtered_buffer_level(&self) -> i32;

    fn set_sample_memory(&mut self, value: i32);
    fn noise_fast_forward(&self) -> usize;
    fn packet_length_samples(&self) -> usize;
    fn set_packet_length_samples(&mut self, value: usize);
    fn set_prev_time_scale(&mut self, value: bool);
}
