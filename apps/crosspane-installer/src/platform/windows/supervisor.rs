//! The sign-in supervisor (L6): keeps the agent running inside a job object.

use std::{
    collections::VecDeque,
    ffi::OsString,
    time::{Duration, Instant},
};

pub const SUPERVISOR_FLAG: &str = "--windows-supervisor";
pub const SUPERVISOR_ALIAS: &str = "--supervisor";
pub(crate) const SUPERVISOR_MUTEX: &str = r"Local\Crosspane-Supervisor";

/// Exactly one argument, and it is the flag or its alias.
pub fn is_supervisor(arguments: &[OsString]) -> bool {
    matches!(arguments, [only] if only == SUPERVISOR_FLAG || only == SUPERVISOR_ALIAS)
}

/// Mutex (a second instance → 0); job `KILL_ON_JOB_CLOSE` + completion port associated BEFORE the
/// first assign; `CreateProcessW(agent, CREATE_SUSPENDED | NORMAL_PRIORITY_CLASS, cwd install)` →
/// assign → resume; handles opened on `JOB_OBJECT_MSG_NEW_PROCESS`; on `ACTIVE_PROCESS_ZERO` the
/// exit code of the held handle with the latest `ExitTime` decides via `RestartPolicy`. Logs via
/// `setup_note!`. Returns the process exit code.
pub fn run() -> i32 {
    1
}

/// Whether a supervisor is running in this session (`lock::mutex_exists(SUPERVISOR_MUTEX)`).
pub(crate) fn running() -> bool {
    false
}

/// Backoff and give-up bookkeeping for agent restarts.
#[derive(Debug)]
pub(crate) struct RestartPolicy {
    failures: VecDeque<Instant>,
}

/// What the supervisor does when the job becomes empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Exit(i32),
    RestartAfter(Duration),
}

impl RestartPolicy {
    pub(crate) fn new() -> Self {
        Self {
            failures: VecDeque::new(),
        }
    }

    /// `Some(0)` → `Exit(0)`; `installer_busy` → `Exit(0)` (F6); else a failure: the 5th within
    /// 120 s → `Exit(1)`, otherwise `RestartAfter([1, 2, 4, 8, 16, 30][min(n - 1, 5)] s)`.
    pub(crate) fn on_empty(
        &mut self,
        now: Instant,
        last_exit: Option<u32>,
        installer_busy: bool,
    ) -> Next {
        let _ = (now, last_exit, installer_busy);
        Next::Exit(1)
    }
}
