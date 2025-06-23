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

use crate::types::{ApiBackend, FrameFormat};
use thiserror::Error;

/// All errors in `nokhwa`.
#[allow(clippy::module_name_repetitions)]
#[derive(Error, Debug, Clone)]
pub enum NokhwaError {
    #[error("Unitialized Camera. Call `init()` first!")]
    UnitializedError,
    #[error("Could not initialize {backend}: {error}")]
    InitializeError { backend: ApiBackend, error: String },
    #[error("Could not shutdown {backend}: {error}")]
    ShutdownError { backend: ApiBackend, error: String },
    #[error("Error: {0}")]
    GeneralError(String),
    #[error("Could not generate required structure {structure}: {error}")]
    StructureError { structure: String, error: String },
    #[error("Could not open device {0}: {1}")]
    OpenDeviceError(String, String),
    #[error("Could not get device property {property}: {error}")]
    GetPropertyError { property: String, error: String },
    #[error("Could not set device property {property} with value {value}: {error}")]
    SetPropertyError {
        property: String,
        value: String,
        error: String,
    },
    #[error("Could not open device stream: {0}")]
    OpenStreamError(String),
    #[error("Could not capture frame: {0}")]
    ReadFrameError(String),
    #[error("Could not process frame {src} to {destination}: {error}")]
    ProcessFrameError {
        src: FrameFormat,
        destination: String,
        error: String,
    },
    #[error("Could not stop stream: {0}")]
    StreamShutdownError(String),
    #[error("This operation is not supported by backend {0}.")]
    UnsupportedOperationError(ApiBackend),
    #[error("This operation is not implemented yet: {0}")]
    NotImplementedError(String),
}
