//! The process-wide operation mutex (L15), held only for the length of one converge or uninstall.

use super::error::{SetupError, SetupResult};

pub(crate) const OPERATION_MUTEX: &str = r"Local\Crosspane-Installer";

/// Thread-affine (`ReleaseMutex` must run on the acquiring thread): `!Send` by `PhantomData<*const ()>`.
#[derive(Debug)]
pub(crate) struct OperationLock {
    handle: std::os::windows::io::OwnedHandle,
    _not_send: std::marker::PhantomData<*const ()>,
}

impl OperationLock {
    /// `WAIT_OBJECT_0` or `WAIT_ABANDONED` = acquired; timeout 0 → `Err(before(text::BUSY))`.
    pub(crate) fn acquire() -> SetupResult<Self> {
        Err(SetupError::before("not built"))
    }
}

/// `OpenMutexW` probe only; never waits, never holds.
pub(crate) fn mutex_exists(name: &str) -> bool {
    let _ = name;
    false
}
