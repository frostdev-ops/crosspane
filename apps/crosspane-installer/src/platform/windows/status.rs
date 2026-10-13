//! The status snapshot behind `--status`, the wizard's steps and `--diagnose`.

use super::{
    files::{FileState, Inventory},
    paths::Paths,
    state::Installed,
    task::TaskHealth,
};
use std::{path::Path, time::Duration};

/// Everything `--status` prints, as one JSON object.
#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct Snapshot {
    pub schema: u32,
    pub installer_version: String,
    pub install_dir: String,
    pub files: Vec<FileReport>,
    pub files_ok: bool,
    pub task: TaskHealth,
    pub supervisor: bool,
    pub agent: AgentReport,
    pub firewall: Option<String>,
    pub driver: Option<String>,
    pub start_menu: bool,
    pub uninstall_entry: bool,
    pub legacy_leftovers: bool,
    pub installed: Option<Installed>,
    pub healthy: bool,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct FileReport {
    pub name: String,
    pub state: FileState,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct AgentReport {
    pub answering: bool,
    pub instance: Option<u64>,
    pub pid: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SnapshotOptions {
    pub elevated: bool,
    pub agent_timeout: Duration,
}

pub(crate) fn snapshot(
    paths: &Paths,
    inventory: &Inventory,
    payload: Option<&Path>,
    options: SnapshotOptions,
) -> Snapshot {
    let _ = (paths, inventory, payload, options);
    Snapshot {
        schema: 1,
        installer_version: String::new(),
        install_dir: String::new(),
        files: Vec::new(),
        files_ok: false,
        task: TaskHealth::Unavailable,
        supervisor: false,
        agent: AgentReport {
            answering: false,
            instance: None,
            pid: None,
        },
        firewall: None,
        driver: None,
        start_menu: false,
        uninstall_entry: false,
        legacy_leftovers: false,
        installed: None,
        healthy: false,
    }
}

impl Snapshot {
    /// `files_ok && task Healthy && start_menu && uninstall_entry && !legacy_leftovers`
    pub(crate) fn converged(&self) -> bool {
        self.files_ok
            && self.task == TaskHealth::Healthy
            && self.start_menu
            && self.uninstall_entry
            && !self.legacy_leftovers
    }
}

/// `--diagnose` on Windows: maps `Snapshot` fields onto the shared `crate::diagnose::Report`.
pub(crate) fn diagnose(payload: Option<&Path>, report: &mut crate::diagnose::Report) {
    let _ = (payload, report);
}
