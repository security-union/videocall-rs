//! Harness error type.

use thiserror::Error;

/// Errors raised by the harness (not by NetEq decisions, which are data).
#[derive(Debug, Error)]
pub enum HarnessError {
    /// `np_neteq_create` returned NULL.
    #[error("failed to create the C++ NetEq")]
    Create,
    /// A decoder could not be registered.
    #[error("failed to register the {0} decoder")]
    Register(&'static str),
    /// libopus encoder error (negative opus error code).
    #[error("libopus encoder error {0}")]
    OpusEncoder(i32),
    /// The implementation under test does not support this yet.
    #[error("not implemented by {implementation}: {what}")]
    NotImplemented {
        /// Implementation name.
        implementation: &'static str,
        /// What is missing.
        what: &'static str,
    },
    /// Unknown scenario name.
    #[error("unknown scenario `{0}`")]
    UnknownScenario(String),
    /// Trace (de)serialization or I/O.
    #[error("trace I/O: {0}")]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Harness result alias.
pub type Result<T> = std::result::Result<T, HarnessError>;
