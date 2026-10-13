//! Enumerate, terminate and wait for processes by image path (L5 step 3).

use std::{path::PathBuf, time::Duration};

/// One running process whose image lives in a directory setup owns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proc {
    pub pid: u32,
    pub image: PathBuf,
}

/// Toolhelp snapshot + `QueryFullProcessImageNameW`; excludes `GetCurrentProcessId()`; skips
/// processes that cannot be opened.
pub(crate) fn under(dirs: &[PathBuf]) -> Vec<Proc> {
    let _ = dirs;
    Vec::new()
}

/// `TerminateProcess` each, wait (total `wait`), and return the ones still alive.
pub(crate) fn terminate(procs: &[Proc], wait: Duration) -> Vec<Proc> {
    let _ = (procs, wait);
    Vec::new()
}

/// Whether `pid` has exited within `wait`.
pub(crate) fn wait_for_exit(pid: u32, wait: Duration) -> bool {
    let _ = (pid, wait);
    false
}
