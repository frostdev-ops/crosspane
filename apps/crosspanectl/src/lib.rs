//! Shared authenticated Windows control transport. The native adapter has one owner and source.

#[cfg(windows)]
mod windows;

/// Windows control endpoints and JSON-line exchange, with server identity checked before JSON.
#[cfg(windows)]
pub mod windows_ctl {
    pub use super::windows::{control_endpoint, exchange};
}

/// Command-line presentation and diagnostics, separate from the shared control API.
#[cfg(windows)]
#[doc(hidden)]
pub mod windows_cli {
    pub use super::windows::{choose_terminal, diag};
}
