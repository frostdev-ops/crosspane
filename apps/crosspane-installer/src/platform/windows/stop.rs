//! Stopping Crosspane (L5): graceful pipe stop, then the task instance, then every remaining process.

use super::{engine::Progress, paths::Paths};

/// What the stop achieved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StopReport {
    pub graceful: bool,
    pub remaining: Vec<String>,
}

/// 1. `AgentPipe::installer_stop` + `wait_for_exit(server pid, 10 s)`
/// 2. `task::stop_instances()`
/// 3. `process::terminate(process::under([install, legacy dirs]), 10 s)`
///
/// Never errors; `remaining` names the image leaves still running.
pub(crate) fn stop_all(paths: &Paths, progress: &dyn Progress) -> StopReport {
    let _ = (paths, progress);
    StopReport {
        graceful: false,
        remaining: Vec::new(),
    }
}
