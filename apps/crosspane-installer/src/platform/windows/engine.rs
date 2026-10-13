//! The lifecycle engine: plan, converge, start and uninstall, plus the user-facing texts.

use super::{
    elevated::ElevatedOutcome,
    error::{SetupError, SetupResult},
    files::Inventory,
    paths::{Paths, PayloadFile},
};
use crosspane_installer_core::elevated::step::ElevatedPlan;
use std::path::PathBuf;

/// Receives plain progress lines from long operations.
pub(crate) trait Progress {
    fn step(&self, text: &str);
}

/// A `Progress` that drops every line.
#[derive(Debug)]
pub(crate) struct NoProgress;

impl Progress for NoProgress {
    fn step(&self, _: &str) {}
}

/// Whether, and with what consent, the administrator step may run.
#[derive(Clone, Debug)]
pub(crate) enum ElevatedPolicy {
    Never,
    IfNeeded,
    Consented(ElevatedPlan),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    Install,
    Repair,
    CompleteReinstall,
}

#[derive(Clone, Debug)]
pub(crate) struct ConvergeOptions {
    pub payload: Option<PathBuf>,
    pub route: Route,
    pub elevated: ElevatedPolicy,
}

/// What a converge would do, computed without changing anything.
#[derive(Clone, Debug)]
pub(crate) struct ConvergePlan {
    pub copies: Vec<PayloadFile>,
    pub unrestorable: Vec<PayloadFile>,
    pub restart: bool,
    pub legacy: bool,
    pub task: bool,
    pub shell: bool,
    pub start: bool,
    pub elevated: Option<ElevatedPlan>,
    pub elevated_note: Option<String>,
}

impl ConvergePlan {
    /// The preview text, plus the elevated `preview_block` and `INSTALL_DECLINE_NOTE` when needed.
    pub(crate) fn preview(&self, route: Route) -> String {
        let _ = route;
        String::new()
    }
}

/// Read-only. `restart = route != Install || copies non-empty || legacy || (processes under
/// install && !supervisor::running())`. `CompleteReinstall` without a payload →
/// `Err(before(text::NEEDS_DOWNLOADED_INSTALLER))`.
pub(crate) fn plan(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
) -> SetupResult<ConvergePlan> {
    let _ = (paths, inventory, options);
    Err(SetupError::before("not built"))
}

#[derive(Clone, Debug)]
pub(crate) struct ConvergeReport {
    pub files_ok: bool,
    pub task_ok: bool,
    pub agent_answering: bool,
    pub elevated: ElevatedOutcome,
    pub lines: Vec<String>,
}

impl ConvergeReport {
    /// `files_ok && task_ok && agent_answering`
    pub(crate) fn ok(&self) -> bool {
        self.files_ok && self.task_ok && self.agent_answering
    }
}

/// lock → sources → plan → [stop] → `legacy::clean` → `files::place` → task (if !Healthy || route
/// != Install) → `shell::ensure` → elevated per policy → start (`task::run`, fallback direct
/// spawn) → wait ≤45 s → `state::write` (only if `files_ok && task_ok`) → `legacy::prune(5)`.
/// Idempotent; reads no prior state.
pub(crate) fn converge(
    paths: &Paths,
    inventory: &Inventory,
    options: &ConvergeOptions,
    progress: &dyn Progress,
) -> SetupResult<ConvergeReport> {
    let _ = (paths, inventory, options, progress);
    Err(SetupError::before("not built"))
}

/// Lock; if `!supervisor::running()`: `task::run` (fallback spawn); wait ≤45 s.
pub(crate) fn start(paths: &Paths, progress: &dyn Progress) -> SetupResult<bool> {
    let _ = (paths, progress);
    Err(SetupError::before("not built"))
}

#[derive(Clone, Debug)]
pub(crate) struct UninstallOptions {
    pub remove_settings: bool,
    pub teardown: ElevatedPolicy,
}

#[derive(Clone, Debug)]
pub(crate) struct UninstallReport {
    pub left: Vec<String>,
    pub lines: Vec<String>,
}

pub(crate) fn plan_uninstall(
    paths: &Paths,
    remove_settings: bool,
    teardown: bool,
) -> SetupResult<(String, Option<ElevatedPlan>)> {
    let _ = (paths, remove_settings, teardown);
    Err(SetupError::before("not built"))
}

/// lock → stop → `task::delete` → [teardown] → [erase-identity if `remove_settings`] →
/// `shell::remove` → remove the install dir (move our own exe to backups or temp first) → [remove
/// the state and config dirs].
pub(crate) fn uninstall(
    paths: &Paths,
    inventory: &Inventory,
    options: &UninstallOptions,
    progress: &dyn Progress,
) -> SetupResult<UninstallReport> {
    let _ = (paths, inventory, options, progress);
    Err(SetupError::before("not built"))
}

pub(crate) fn is_installed(paths: &Paths) -> bool {
    let _ = paths;
    false
}

/// The user-facing texts. Plain words only: never ownership or bookkeeping vocabulary.
pub(crate) mod text {
    pub(crate) const NEEDS_DOWNLOADED_INSTALLER: &str =
        "Run the Crosspane installer you downloaded to restore Crosspane's files.";
    pub(crate) const NOT_STARTED_YET: &str =
        "Crosspane is installed but hasn't started yet. Check again.";
    pub(crate) const BUSY: &str =
        "Another Crosspane Setup is working right now. Try again when it finishes.";
    pub(crate) const STILL_RUNNING: &str = "Crosspane couldn't be stopped. Close it and try again.";
    pub(crate) const SOURCES_DAMAGED: &str =
        "The Crosspane files beside this installer don't match it. Download Crosspane again.";
    pub(crate) const NO_INVENTORY: &str =
        "This installer build doesn't list Crosspane's files, so it can't install them.";
    pub(crate) const NOT_INSTALLED: &str = "Crosspane isn't installed for this account.";
}
