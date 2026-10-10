//! The RemoteDesktop portal session that authorizes input injection (WP-G1.3).
//!
//! One worker thread owns the session through `ashpd` (async-io executor, driven with
//! `async_io::block_on` or an equivalent on that thread only). It:
//!
//! 1. creates a RemoteDesktop session, selects the keyboard and pointer device types with
//!    `PersistMode::ExplicitlyRevoked` and the restore token read from `token_path` (if any);
//! 2. starts it, which may show the consent dialog the first time; the start waits as long as the
//!    user takes, but never on a trait call;
//! 3. writes the rotated restore token the start returns to `token_path` (mode 0600, written to a
//!    temporary file and renamed; a missing or unreadable token just means consent is asked again);
//! 4. calls `ConnectToEIS` and hands the socket over through [`RemoteDesktopSession::take_eis`];
//! 5. watches the session's `Closed` signal and the portal's bus name. Either one ends the epoch.
//!
//! **Epochs.** Every successful start is a new epoch (1, 2, …). A result that arrives after
//! [`RemoteDesktopSession::close`] or after its epoch ended is stale and dropped, never reported
//! as `Active`. A denied or cancelled dialog is [`SessionStatus::Denied`] and is not retried
//! automatically; [`RemoteDesktopSession::restart`] asks again (the tray's "Allow remote input").
//! A session that closes (the user revoked it in the desktop's indicator, the portal restarted) is
//! [`SessionStatus::Closed`]; the worker then retries once with the stored token (silent when the
//! desktop still honours it), and otherwise stays `Closed` until `restart`.
//!
//! Portal closure is authorization loss, not lock evidence: the I/O gate is the session backend's
//! (logind) business, not this module's.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;

use crosspane_platform::PlatformError;

/// What the session is asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteDesktopConfig {
    /// Where the restore token lives, e.g. `<state_dir>/portal-remote-desktop.token`.
    pub token_path: PathBuf,
}

/// The session's state, as reported to the status callback and by [`RemoteDesktopSession::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionStatus {
    /// Starting: creating the session or waiting for the user's consent.
    Pending,
    /// Started with keyboard and pointer granted; its EIS socket is (or was) available.
    Active { epoch: u64 },
    /// The user denied or cancelled the dialog, or the desktop granted less than keyboard and
    /// pointer. Not retried until `restart`.
    Denied,
    /// The epoch's session ended (revoked, portal gone, `close`).
    Closed { epoch: u64 },
    /// No RemoteDesktop portal (or a version without `ConnectToEIS`) on this desktop.
    Unavailable,
}

/// Called on the worker thread on every status change, in order. Must not block.
pub type StatusCallback = Arc<dyn Fn(SessionStatus) + Send + Sync>;

/// A handle to the session worker. Dropping it closes the session and joins the worker (bounded).
#[derive(Debug)]
pub struct RemoteDesktopSession {}

impl RemoteDesktopSession {
    /// Start the worker and return immediately (within 2 s). The first status is `Pending`.
    pub fn spawn(
        config: RemoteDesktopConfig,
        on_status: StatusCallback,
    ) -> Result<RemoteDesktopSession, PlatformError> {
        let _ = (config, on_status);
        Err(PlatformError::Unsupported(
            "RemoteDesktop session not implemented yet",
        ))
    }

    /// The current status.
    pub fn status(&self) -> SessionStatus {
        SessionStatus::Unavailable
    }

    /// The EIS socket of the active epoch. Each epoch's socket is handed out exactly once; later
    /// calls in the same epoch, and calls while not `Active`, return `None`.
    pub fn take_eis(&self) -> Option<(u64, OwnedFd)> {
        None
    }

    /// Close the current session (if any) and start a new one, which may ask the user again.
    pub fn restart(&self) -> Result<(), PlatformError> {
        Err(PlatformError::Unsupported(
            "RemoteDesktop session not implemented yet",
        ))
    }

    /// Close the session for good. Idempotent; later results are stale.
    pub fn close(&self) {}
}
