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

/// Re-export shim for [`videocall_aq::manager`].
///
/// Preserves the `videocall_client::diagnostics::adaptive_quality_manager`
/// import path for existing browser callers even though the code now lives
/// in the sibling `videocall-aq` crate.
pub mod adaptive_quality_manager {
    pub use videocall_aq::manager::*;
}

pub mod diagnostics_manager;

/// Re-export shim for [`videocall_aq::controller`].
pub mod encoder_bitrate_controller {
    pub use videocall_aq::controller::*;
}

// Re-export the modules
pub use adaptive_quality_manager::AdaptiveQualityManager;
pub use diagnostics_manager::{DiagnosticEvent, DiagnosticManager, SenderDiagnosticManager};
pub use encoder_bitrate_controller::{EncoderBitrateController, EncoderControl};
