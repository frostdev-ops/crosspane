//! Windows installer platform: one idempotent lifecycle backend (converge, repair, uninstall), the
//! agent supervisor, the headless commands and the elevated setup launcher.
pub(crate) mod agent_pipe;
pub mod cli;
pub(crate) mod elevated;
pub(crate) mod elevated_kit;
pub mod elevated_launch;
pub(crate) mod engine;
pub(crate) mod error;
pub(crate) mod files;
pub(crate) mod identity;
pub(crate) mod legacy;
pub(crate) mod lock;
pub(crate) mod paths;
pub(crate) mod payload;
pub(crate) mod process;
pub(crate) mod setup_log;
pub(crate) mod shell;
pub(crate) mod state;
pub(crate) mod status;
pub(crate) mod stop;
pub mod supervisor;
pub(crate) mod support;
pub(crate) mod task;
pub(crate) mod wizard;

/// Set by `main` when the process was started with `--maintenance`.
static MAINTENANCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks the wizard to open on the maintenance screen (called by `main` for `--maintenance`).
pub fn request_maintenance() {
    MAINTENANCE.store(true, std::sync::atomic::Ordering::Release);
}

/// Whether `request_maintenance` was called in this process.
pub(crate) fn maintenance_requested() -> bool {
    MAINTENANCE.load(std::sync::atomic::Ordering::Acquire)
}
