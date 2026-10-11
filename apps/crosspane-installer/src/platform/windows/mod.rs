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
