//! `SetupError` (plain text plus "an OS change may have happened") and the bounded native error.

use super::setup_log::setup_note;

/// A bounded native failure from the identity, support, payload and elevated-helper checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeError {
    #[error("invalid bounded native value")]
    Invalid,
    #[error("native observation unavailable")]
    Unavailable,
    #[error("foreign or unsafe resource")]
    Foreign,
    #[error("unsupported or unknown target")]
    Unsupported,
    #[error("positively missing admitted native leaf")]
    Missing,
    #[error("operation timed out before dispatch")]
    Timeout,
    #[error("operation cancelled before dispatch")]
    Cancelled,
    #[error("bounded native owner or installer lock busy")]
    Busy,
    #[error("bounded record exceeded")]
    Oversize,
    #[error("native mutation outcome unknown; inspect before retry")]
    OutcomeUnknown,
}

/// `Result` over [`NativeError`].
pub type NativeResult<T> = Result<T, NativeError>;

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
