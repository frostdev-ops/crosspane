//! Elevated setup orchestration (L8): firewall rule + display driver through the helper.

use super::{
    engine::Progress,
    error::{SetupError, SetupResult},
    paths::Paths,
};
use crosspane_installer_core::elevated::{
    RuleScope, Verb, kit::KitManifest, status::StatusReport, step::ElevatedPlan,
};
use std::path::Path;

/// What the administrator step did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ElevatedOutcome {
    NotNeeded,
    Configured(String),
    Declined,
    Failed(String),
}

/// Install id: the existing rule (`INetFwPolicy2`, unelevated) named `RULE_PREFIX*` whose
/// application `same_path(agent)`; else random (`aws_lc_rs::rand`). Program = `paths.file(Agent)`.
pub(crate) fn scope(paths: &Paths) -> SetupResult<RuleScope> {
    let _ = paths;
    Err(SetupError::before("not built"))
}

/// Unelevated `status` via the installed helper, else `payload\crosspane-elevated-setup.exe`
/// (verified source).
pub(crate) fn observe(
    paths: &Paths,
    payload: Option<&Path>,
    scope: &RuleScope,
) -> Result<StatusReport, String> {
    let _ = (paths, payload, scope);
    Err("not built".to_owned())
}

/// firewall ∈ {Missing, Mismatch} || driver ∈ {Absent, PackageOnly, DeviceWithoutDriver}
pub(crate) fn setup_needed(report: &StatusReport) -> bool {
    let _ = report;
    false
}

/// Whether there is a firewall rule or driver left for `teardown` to remove.
pub(crate) fn teardown_needed(report: &StatusReport) -> bool {
    let _ = report;
    false
}

/// `ElevatedPlan::new`
pub(crate) fn plan(verb: Verb) -> SetupResult<ElevatedPlan> {
    let _ = verb;
    Err(SetupError::before("not built"))
}

/// `hold_placed` for the run; `ElevatedHelper::run` on a dedicated STA thread
/// (`CoInitializeEx` APARTMENTTHREADED); log-only `ElevatedJournal`; heartbeat progress every
/// 15 s; text from core `status_summary`.
pub(crate) fn run(
    paths: &Paths,
    kit: &KitManifest,
    plan: &ElevatedPlan,
    progress: &dyn Progress,
) -> ElevatedOutcome {
    let _ = (paths, kit, plan, progress);
    ElevatedOutcome::NotNeeded
}

/// Core `status_summary` text for `report`.
pub(crate) fn summary(report: &StatusReport) -> String {
    let _ = report;
    String::new()
}
