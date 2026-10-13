//! Windows installer platform: Limited-token native foundations, the live integration and the elevated setup launcher.
pub mod detect;
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod elevated_kit;
#[cfg(windows)]
pub mod elevated_launch;
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod elevated_step;
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod elevated_store;
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod first_install;
#[cfg(windows)]
#[cfg_attr(test, allow(dead_code, unused_imports))]
pub(crate) mod integration;
pub mod native_io;
pub(crate) mod payload;
// Native removal consumers are excluded from the library unit-test graph; integration fakes include this new graph.
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod removal;
// Native repair adapters are excluded from the library unit-test graph; focused fakes include the model.
#[cfg(any(windows, test))]
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod repair;
pub mod service;
#[cfg_attr(test, allow(dead_code))]
pub(crate) mod setup_log;
pub mod transport;

// WP-W4.1e: the simple idempotent lifecycle backend. Stubs until the implementers fill them in.
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod agent_pipe;
#[cfg(windows)]
#[allow(dead_code)]
pub mod cli;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod elevated;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod engine;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod error;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod files;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod identity;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod legacy;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod lock;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod paths;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod process;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod shell;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod state;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod status;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod stop;
#[cfg(windows)]
#[allow(dead_code)]
pub mod supervisor;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod support;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod task;
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) mod wizard;

/// Set by `main` when the process was started with `--maintenance`.
#[cfg(windows)]
static MAINTENANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks the wizard to open on the maintenance screen (called by `main` for `--maintenance`).
#[cfg(windows)]
pub fn request_maintenance() {
    MAINTENANCE.store(true, std::sync::atomic::Ordering::Release);
}

/// Whether `request_maintenance` was called in this process.
#[cfg(windows)]
#[allow(dead_code)]
pub(crate) fn maintenance_requested() -> bool {
    MAINTENANCE.load(std::sync::atomic::Ordering::Acquire)
}

/// Fixed-argument binary entry only; it grants no root, process or operation authority.
#[doc(hidden)]
pub fn replace_helper_mode(arguments: &[std::ffi::OsString]) -> native_io::NativeResult<bool> {
    payload::helper::replace_helper_mode(arguments)
}

/// Run the admitted fixed helper without initializing the installer GUI.
#[cfg(windows)]
#[doc(hidden)]
pub fn replace_helper_entry() -> native_io::NativeResult<()> {
    match payload::helper::replace_helper_entry()? {
        payload::helper::HelperExit::HandoffCompleted => Ok(()),
        payload::helper::HelperExit::RecoveryRetained => {
            Err(native_io::NativeError::OutcomeUnknown)
        }
    }
}
