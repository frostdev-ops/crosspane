//! Operation-bound task and supervisor decisions. Model facts never grant native authority.
//! The only production executable approval source will be A4's genuine fixed-role inventory.
//! Until then the supervisor entry and start paths fail closed with Unsupported.

#[path = "service/journal.rs"]
pub mod journal;
#[path = "service/supervisor.rs"]
pub mod supervisor;
#[path = "service/task.rs"]
pub mod task;

use super::native_io::{NativeError, NativeResult};

/// Not constructible outside this module, and deliberately without a production constructor.
struct TrustedImages {
    _sealed: (),
}
impl TrustedImages {
    fn current() -> NativeResult<Self> {
        // Lead1fae562c: current file bytes, metadata, argv and record values are not approval.
        Err(NativeError::Unsupported)
    }
    #[cfg(test)]
    fn fixture() -> Self {
        Self { _sealed: () }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorExit {
    NormalQuit,
    InstallerStopped,
}

/// Windows-only early entry; it cannot start a GUI or manufacture image approval.
#[cfg(windows)]
pub fn supervisor_entry() -> NativeResult<SupervisorExit> {
    let trusted = TrustedImages::current()?;
    supervisor::native_entry(&trusted)
}

/// A4 must supply genuine fixed installer/agent role and PE/hash approval before this can start.
pub fn start_supported() -> NativeResult<()> {
    let trusted = TrustedImages::current()?;
    task::native_start(&trusted)
}

/// This fixed mode accepts no additional argv and confers no execution authority.
pub fn supervisor_mode(arguments: &[std::ffi::OsString]) -> NativeResult<bool> {
    if !arguments
        .iter()
        .any(|argument| argument == task::SUPERVISOR_ARGUMENT)
    {
        return Ok(false);
    }
    if arguments.len() != 1 {
        return Err(NativeError::Invalid);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_start_has_no_current_byte_or_receipt_approval_fallback() {
        let _fixture = TrustedImages::fixture();
        assert_eq!(start_supported(), Err(NativeError::Unsupported));
    }
}
