// NetEqImpl calls these hooks; the parity harness under `neteq/parity`
// codes against the trait (PARITY_PLAN.md 7.3). Compiled only with feature
// `parity`.

//! Per-tick observer hooks for the C++ parity harness.

use crate::api::audio_frame::AudioFrame;
use crate::api::neteq::{Mode, Operation};
use crate::api::neteq_controller::NetEqStatus;
use crate::api::rtp::RtpHeader;

/// Receives NetEq's internal events, in call order. All methods default to
/// doing nothing.
pub trait Observer: Send {
    /// After each `insert_packet`, with the C++ return code (0 or -1).
    fn on_insert(&mut self, header: &RtpHeader, return_code: i32) {
        let _ = (header, return_code);
    }

    /// After the controller decides: its input, its raw decision and the
    /// operation `NetEqImpl` finally uses after its overrides.
    fn on_decision(&mut self, status: &NetEqStatus, raw: Operation, final_op: Operation) {
        let _ = (status, raw, final_op);
    }

    /// At the end of each `get_audio`.
    fn on_get_audio(&mut self, frame: &AudioFrame, muted: bool, last_mode: Mode) {
        let _ = (frame, muted, last_mode);
    }
}
