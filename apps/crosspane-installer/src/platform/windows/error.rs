//! `SetupError` (plain text plus "an OS change may have happened") and the native error re-export.

use super::setup_log::setup_note;

pub(crate) use super::native_io::{NativeError, NativeResult};

/// A failed setup step, in plain words for the person using the installer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{text}")]
pub(crate) struct SetupError {
    pub text: String,
    /// An OS change may have happened before the failure.
    pub changed: bool,
}

impl SetupError {
    /// A failure before any OS change.
    pub(crate) fn before(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: false,
        }
    }

    /// A failure after an OS change may already have happened.
    pub(crate) fn after(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            changed: true,
        }
    }
}

pub(crate) type SetupResult<T> = Result<T, SetupError>;

/// Logs `"{what}: os error {code}"` and nothing else (-1 when the error carries no OS code).
pub(crate) fn note_io(what: &str, error: &std::io::Error) {
    let code = error.raw_os_error().unwrap_or(-1);
    setup_note!("{what}: os error {code}");
}
