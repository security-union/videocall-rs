// SPDX-License-Identifier: MIT OR Apache-2.0

//! Errors and the stable exit codes they map to.
//!
//! | code | meaning                                                             |
//! |------|---------------------------------------------------------------------|
//! | 0    | success                                                             |
//! | 1    | not converged: `pin --check` found stale pins, or `wait-version`    |
//! |      | hit its deadline before the build reported the expected sha         |
//! | 2    | could not decide: bad arguments, I/O, git (including a shallow      |
//! |      | clone), or a registry/network error after retries. Safe to retry.   |
//! |      | Also a closed stdout, which stops the command without a message.    |
//! | 3    | refused: the inputs are wrong or unsafe (bad config, downgrade,     |
//! |      | re-pushed tag, a pinned tag the registry does not have, off-history |
//! |      | or ambiguous sha, wrong repository, malformed pin or values file,   |
//! |      | a render that disagrees with the pin, a write that could not be     |
//! |      | rolled back). Retrying will not help.                               |

use std::fmt;
use std::path::PathBuf;

pub const EXIT_OK: u8 = 0;
pub const EXIT_NOT_CONVERGED: u8 = 1;
pub const EXIT_ERROR: u8 = 2;
pub const EXIT_REFUSED: u8 = 3;

/// Everything that can stop a run.
#[derive(Debug)]
pub enum Error {
    /// Reading or writing a file failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The config file is missing, malformed, or points at things that are not there.
    Config(String),
    /// Running git failed, printed output we do not trust, or the clone is shallow.
    Git(String),
    /// The registry could not be reached or answered with an error, after retries.
    Registry(String),
    /// The registry answered 404: the repository or tag does not exist.
    NotFound(String),
    /// A values file or rendered manifest is not shaped the way we require.
    Values { path: String, message: String },
    /// The change is possible but unsafe, so it was not made.
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Config(m) => write!(f, "config: {m}"),
            Error::Git(m) => write!(f, "git: {m}"),
            Error::Registry(m) | Error::NotFound(m) => write!(f, "registry: {m}"),
            Error::Values { path, message } => write!(f, "{path}: {message}"),
            Error::Refused(m) => write!(f, "refused: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl Error {
    /// The process exit code for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Io { .. } | Error::Git(_) | Error::Registry(_) => EXIT_ERROR,
            Error::Config(_) | Error::NotFound(_) | Error::Values { .. } | Error::Refused(_) => {
                EXIT_REFUSED
            }
        }
    }

    /// Shorthand for a [`Error::Values`] error.
    pub fn values(path: impl Into<String>, message: impl Into<String>) -> Self {
        Error::Values {
            path: path.into(),
            message: message.into(),
        }
    }

    /// Stdout was closed under us (`releases | head -1`).
    pub fn is_broken_pipe(&self) -> bool {
        matches!(self, Error::Io { source, .. } if source.kind() == std::io::ErrorKind::BrokenPipe)
    }

    /// Wrap an I/O error on `path`.
    pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Self {
        let path = path.into();
        move |source| Error::Io { path, source }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
