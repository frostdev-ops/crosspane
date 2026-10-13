//! The `\Crosspane\Agent` sign-in task (L7): XML, registration, inspection, stop, run and delete.

use super::{
    error::{SetupError, SetupResult},
    paths::Paths,
};

pub(crate) const TASK_PATH: &str = r"\Crosspane\Agent";

/// What Task Scheduler says about the sign-in task, compared field by field (never as XML text).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskHealth {
    Healthy,
    Missing,
    Disabled,
    Different,
    Unavailable,
}

/// Pure. XML-escaped; logon trigger + principal by SID, InteractiveToken, LeastPrivilege,
/// IgnoreNew, PT0S, battery false/false, AllowHardTerminate true, Priority 4, Exec
/// `"<install>\crosspane-installer.exe" --windows-supervisor`, WorkingDirectory `<install>`.
pub(crate) fn task_xml(paths: &Paths, user_sid: &str) -> String {
    let _ = (paths, user_sid);
    String::new()
}

/// Healthy = exists, enabled, one Exec whose path `same_path(installer)` and arguments ==
/// `--windows-supervisor`, one LOGON trigger whose user ∈ {SID, `account_name` (DOMAIN\name), bare
/// name}. Never compares XML text.
pub(crate) fn inspect(paths: &Paths) -> TaskHealth {
    let _ = paths;
    TaskHealth::Unavailable
}

/// Folder create-if-missing; `RegisterTask(TASK_CREATE_OR_UPDATE, VARIANT(SID),
/// TASK_LOGON_INTERACTIVE_TOKEN)`.
pub(crate) fn register(paths: &Paths) -> SetupResult<()> {
    let _ = paths;
    Err(SetupError::before("not built"))
}

/// `IRegisteredTask::Stop(0)`; waits ≤5 s for State != Running; a missing task is Ok.
pub(crate) fn stop_instances() -> SetupResult<()> {
    Err(SetupError::before("not built"))
}

/// `IRegisteredTask::Run(VARIANT::default())`
pub(crate) fn run() -> SetupResult<()> {
    Err(SetupError::before("not built"))
}

/// A missing task is Ok; `DeleteFolder("Crosspane")` best effort.
pub(crate) fn delete() -> SetupResult<()> {
    Err(SetupError::before("not built"))
}
