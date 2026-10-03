//! Errors every backend returns.

use thiserror::Error;

/// An OS permission a backend may need.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Permission {
    /// macOS Screen Recording (ScreenCaptureKit).
    ScreenRecording,
    /// macOS Accessibility (posting events, controlling windows).
    Accessibility,
    /// macOS Input Monitoring (event taps).
    InputMonitoring,
    /// macOS Microphone (audio capture through public CoreAudio APIs).
    Microphone,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PlatformError {
    /// The user hasn't granted a permission. Backends check with the OS's preflight APIs and never
    /// read or edit permission databases (04 §7).
    #[error("missing permission: {0:?}")]
    PermissionDenied(Permission),
    /// The I/O gate is closed: the session is locked, inactive, asleep, its state is unknown, or the
    /// engine closed it (04 §7: fail closed).
    #[error("input is not permitted now (locked, inactive, or unknown session state)")]
    Locked,
    /// Keyboard input can't be observed because macOS Secure Event Input is on.
    #[error("secure keyboard input is active")]
    SecureInput,
    /// A pointer button is held, so capture can't begin (04 §8: no crossing with a button held).
    #[error("a pointer button is held")]
    PointerButtonHeld,
    /// The OS needs the user to unlock or approve something (e.g. a locked keyring). Backends never
    /// show UI from a call; the caller decides whether to ask the user.
    #[error("the OS needs user interaction first")]
    InteractionRequired,
    /// The backend can't do this on this OS or compositor version.
    #[error("not supported by this backend: {0}")]
    Unsupported(&'static str),
    /// The display, window or item doesn't exist (any more).
    #[error("not found")]
    NotFound,
    /// A bounded wait ran out (e.g. a Hyprland IPC request; R19).
    #[error("timed out")]
    Timeout,
    /// The requested clipboard content exceeds the caller's byte limit; never a truncation.
    #[error("clipboard content exceeds the byte limit")]
    TooLarge,
    /// Any other OS or compositor failure, described for logs. Never include key contents.
    #[error("{0}")]
    Backend(String),
}
