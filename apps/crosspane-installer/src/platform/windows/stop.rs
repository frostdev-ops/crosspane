//! Stopping Crosspane (L5): graceful pipe stop, then the task instance, then every remaining process.

use super::{
    agent_pipe::AgentPipe,
    engine::Progress,
    paths::Paths,
    process::{self, Proc},
    setup_log::setup_note,
    task,
};
use std::{path::PathBuf, time::Duration};

/// How long the agent has to accept the graceful stop request.
const STOP_CALL: Duration = Duration::from_secs(5);
/// How long the agent has to exit after accepting it (it releases input and unparks windows).
const GRACEFUL_EXIT: Duration = Duration::from_secs(10);
/// How long forced termination waits for the processes to go away.
const TERMINATE_WAIT: Duration = Duration::from_secs(10);
/// Folder-name prefix of the older installer's quarantine folders under `Programs\`.
const QUARANTINE_PREFIX: &str = "crosspane-reinstall-";

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
    progress.step("Stopping Crosspane…");
    let graceful = graceful_stop(paths);

    if let Err(error) = task::stop_instances() {
        setup_note!(
            "stop: ending the sign-in task instance failed: {}",
            error.text
        );
    }

    let dirs = stop_dirs(paths);
    let running = process::under(&dirs);
    if !running.is_empty() {
        progress.step("Ending the Crosspane programs that are still running…");
        let alive = process::terminate(&running, TERMINATE_WAIT);
        if !alive.is_empty() {
            setup_note!("stop: {} process(es) survived termination", alive.len());
        }
    }

    let remaining = leaf_names(&process::under(&dirs));
    if !remaining.is_empty() {
        setup_note!("stop: still running: {}", remaining.join(", "));
    }
    StopReport {
        graceful,
        remaining,
    }
}

/// Asks the running agent to stop itself and waits for it to exit. `false` when there is no
/// answering agent, the request fails, or the agent outlives the wait.
fn graceful_stop(paths: &Paths) -> bool {
    let Some(pipe) = AgentPipe::locate(paths) else {
        return false;
    };
    match pipe.installer_stop(STOP_CALL) {
        Ok(pid) => {
            let exited = process::wait_for_exit(pid, GRACEFUL_EXIT);
            if !exited {
                setup_note!("stop: agent process {pid} did not exit after the graceful stop");
            }
            exited
        }
        Err(failure) => {
            setup_note!("stop: graceful stop unavailable: {failure:?}");
            false
        }
    }
}

/// Every existing folder a Crosspane process may run from: the install dir, the older installer's
/// quarantine folders, and the folders its helper copies ran from.
fn stop_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs = vec![paths.install.clone()];
    if let Ok(entries) = std::fs::read_dir(&paths.programs) {
        for entry in entries.flatten() {
            let quarantine = entry
                .file_name()
                .to_str()
                .is_some_and(is_quarantine_folder_name);
            if quarantine && entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
    }
    dirs.push(paths.runtime.join("reinstall"));
    dirs.push(paths.runtime.join("removal"));
    dirs.push(paths.installer_state.clone());
    dirs.retain(|dir| dir.is_dir());
    dirs
}

/// Whether `name` is one of the older installer's `crosspane-reinstall-<hex>` quarantine folders.
fn is_quarantine_folder_name(name: &str) -> bool {
    name.len() > QUARANTINE_PREFIX.len()
        && name
            .get(..QUARANTINE_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(QUARANTINE_PREFIX))
}

/// The distinct file names of `procs`, sorted.
fn leaf_names(procs: &[Proc]) -> Vec<String> {
    let mut names: Vec<String> = procs
        .iter()
        .map(|proc| {
            proc.image.file_name().map_or_else(
                || proc.image.display().to_string(),
                |leaf| leaf.to_string_lossy().into_owned(),
            )
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, image: &str) -> Proc {
        Proc {
            pid,
            image: PathBuf::from(image),
        }
    }

    #[test]
    fn quarantine_folder_names() {
        assert!(is_quarantine_folder_name("crosspane-reinstall-0a1b2c"));
        assert!(is_quarantine_folder_name("Crosspane-Reinstall-DEADBEEF"));
        assert!(!is_quarantine_folder_name("crosspane-reinstall-"));
        assert!(!is_quarantine_folder_name("Crosspane"));
        assert!(!is_quarantine_folder_name("crosspane-reinstal"));
        assert!(!is_quarantine_folder_name("é"));
    }

    #[test]
    fn leaf_names_are_distinct_and_sorted() {
        let procs = [
            proc(10, r"C:\Programs\Crosspane\crosspane-ui.exe"),
            proc(11, r"C:\Programs\Crosspane\crosspane-agent.exe"),
            proc(12, r"C:\Programs\Crosspane\crosspane-agent.exe"),
        ];
        assert_eq!(
            leaf_names(&procs),
            ["crosspane-agent.exe", "crosspane-ui.exe"]
        );
        assert!(leaf_names(&[]).is_empty());
    }
}
