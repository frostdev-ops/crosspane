//! Typed integration observations are never native ownership or executable approval.
use crosspane_installer_core::ObservationSource;
use crosspane_installer_core::elevated::step::ElevatedPlan;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Install,
    Upgrade,
    Removal {
        erase_identity: bool,
    },
    MetadataRepair,
    PayloadRepair,
    /// The administrator step alone (ADMIN).
    Elevated,
}
/// Setup or teardown of the firewall rule and display driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ElevatedRequest {
    Setup,
    Teardown,
}
/// The administrator step's planning result; `Planned` carries the consent-bound plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ElevatedPlanning {
    Planned(ElevatedPlan),
    Unavailable(&'static str),
    NotNeeded,
}
/// Read-only detection of the firewall rule and display driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ElevatedDetection {
    pub configured: bool,
    pub detail: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Healthy,
    Missing,
    Mismatch,
    Disabled,
    Unavailable,
    Unknown,
}
/// Read-only routing hint, renewed by the cold native facade under the actual lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cold {
    Eligible,
    Observe,
    Partial,
    Stale,
    CompletedRemoval,
    Existing,
    AccessDenied,
    Unknown,
}
pub(crate) fn publication_unsettled(healthy: bool, proven_capacity_only: bool) -> bool {
    !healthy && !proven_capacity_only
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub supported: bool,
    pub inventory: bool,
    pub sources: bool,
    pub payload: State,
    pub task: State,
    pub agent: State,
    pub cold: Cold,
    pub terminal_history: bool,
    pub unsettled: bool,
    /// Private exact diagnostic correlation; never displayed or interpreted as ownership.
    pub correlation: Vec<u8>,
    pub source: ObservationSource,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Snapshot(..)")
    }
}
impl Snapshot {
    pub fn healthy(&self) -> bool {
        self.supported
            && self.inventory
            && self.cold == Cold::Existing
            && self.payload == State::Healthy
            && self.task == State::Healthy
            && self.agent == State::Healthy
            && !self.unsettled
    }
    pub fn verified(&self, operation: Operation) -> bool {
        if self.source != ObservationSource::Live {
            return false;
        }
        match operation {
            Operation::Removal { .. } => {
                self.payload == State::Missing
                    && self.task == State::Missing
                    && self.agent == State::Missing
                    && !self.unsettled
            }
            Operation::Elevated => self.healthy(),
            _ => self.healthy(),
        }
    }
    /// A completed first-install recovery leaves no partial or stale cold state behind.
    pub fn first_recovery_settled(&self) -> bool {
        self.source == ObservationSource::Live
            && !matches!(self.cold, Cold::Partial | Cold::Stale)
            && !self.unsettled
    }
    pub fn install_operation(&self) -> Operation {
        if matches!(self.cold, Cold::Partial | Cold::Stale) {
            return Operation::MetadataRepair;
        }
        if self.cold != Cold::Existing || self.payload == State::Missing {
            Operation::Install
        } else {
            Operation::Upgrade
        }
    }
    pub fn repair_operation(&self) -> Operation {
        if matches!(self.cold, Cold::Partial | Cold::Stale) {
            return Operation::MetadataRepair;
        }
        if self.payload == State::Healthy {
            Operation::MetadataRepair
        } else {
            Operation::PayloadRepair
        }
    }
    pub fn allowed(&self, op: Operation) -> bool {
        if self.supported && matches!(self.cold, Cold::Partial | Cold::Stale) {
            return match op {
                Operation::MetadataRepair => true,
                Operation::Removal {
                    erase_identity: false,
                } => self.cold == Cold::Partial,
                _ => false,
            };
        }
        if !self.supported || !self.inventory || self.unsettled {
            return false;
        }
        if matches!(op, Operation::Elevated) {
            return self.cold == Cold::Existing && self.payload == State::Healthy;
        }
        if matches!(op, Operation::Install) {
            return self.sources
                && match self.cold {
                    Cold::Eligible | Cold::CompletedRemoval => {
                        self.task == State::Missing && self.agent == State::Missing
                    }
                    Cold::Observe => self.task == State::Healthy && self.agent == State::Healthy,
                    _ => false,
                };
        }
        if matches!(
            self.task,
            State::Disabled | State::Mismatch | State::Unknown | State::Unavailable
        ) {
            return false;
        }
        match op {
            Operation::Upgrade | Operation::PayloadRepair => self.sources,
            Operation::Removal { .. } => true,
            Operation::MetadataRepair => {
                self.payload == State::Healthy && self.agent == State::Healthy
            }
            Operation::Install | Operation::Elevated => false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Handoff {
    NotCommitted,
    Committed,
    Complete,
    Unknown,
}
impl Handoff {
    pub fn permits_exit(self) -> bool {
        matches!(self, Self::Committed | Self::Complete)
    }
}
/// The only error that local Commit evidence may turn into a handoff is a bounded
/// observation timeout; Ready and a foreign/refused response never do so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommitObservation {
    Committed,
    Complete,
    Retained,
    Ready,
    NoAnswer,
    Refused,
}
pub(crate) fn committed_handoff(local_commit: bool, observed: CommitObservation) -> Handoff {
    match observed {
        CommitObservation::Committed => Handoff::Committed,
        CommitObservation::Complete => Handoff::Complete,
        CommitObservation::Retained | CommitObservation::NoAnswer if local_commit => {
            Handoff::Committed
        }
        CommitObservation::Ready => Handoff::NotCommitted,
        _ => Handoff::Unknown,
    }
}
pub(crate) fn await_committed_handoff(
    local_commit: bool,
    mut observe: impl FnMut() -> CommitObservation,
    mut budget_available: impl FnMut() -> bool,
    mut wait: impl FnMut(),
) -> Handoff {
    let mut attempted = false;
    loop {
        if !budget_available() {
            return if attempted {
                committed_handoff(local_commit, CommitObservation::NoAnswer)
            } else {
                Handoff::Unknown
            };
        }
        let observed = observe();
        attempted = true;
        if observed != CommitObservation::NoAnswer {
            return committed_handoff(local_commit, observed);
        }
        if !budget_available() {
            return committed_handoff(local_commit, observed);
        }
        wait();
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    NotSubmitted,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dispatch {
    pub handoff: Handoff,
    pub complete: bool,
}
/// The domain owns genuine capabilities and renews them internally. A Snapshot is correlation only.
pub(crate) trait Domains: Send {
    fn observe(&mut self) -> Result<Snapshot, Failure>;
    /// Only explicit Apply/Resume calls this. true means an effect may have been submitted.
    fn settle(&mut self, operation: Operation) -> Result<bool, Failure>;
    fn apply(&mut self, operation: Operation) -> Result<Dispatch, Failure>;
    fn verify(&mut self, operation: Operation) -> Result<bool, Failure>;
    /// Static first-install refusal detail only; consent/submission accounting is unchanged.
    fn take_first_refusal(&mut self) -> Option<&'static str> {
        None
    }
    /// Plans the administrator step for one request; by default there is nothing to ask.
    fn elevated_plan(&mut self, _r: ElevatedRequest) -> Result<ElevatedPlanning, Failure> {
        Ok(ElevatedPlanning::NotNeeded)
    }
    /// Read-only check of the firewall rule and display driver; by default it is not submitted.
    fn elevated_detect(&mut self) -> Result<ElevatedDetection, Failure> {
        Err(Failure::NotSubmitted)
    }
    /// Applies with the consent-bound plan when there is one; by default it is the plain apply.
    fn apply_elevated(
        &mut self,
        op: Operation,
        _e: Option<&ElevatedPlan>,
    ) -> Result<Dispatch, Failure> {
        self.apply(op)
    }
    /// Lines for the outcome detail after the administrator step; by default there are none.
    fn take_elevated_report(&mut self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairArtifact {
    Complete,
    RetiredOrAbsent,
}
/// Explicit settlement has no Stop/Run or task method. A deferred archive is a
/// successful copy settlement, not an Unknown submission of the next operation.
pub(crate) trait ArtifactSettlement {
    type Error;
    fn retire_repair_copy(&mut self) -> Result<(), Self::Error>;
    fn settle_outer_history(&mut self) -> Result<(), Self::Error>;
}
pub(crate) fn settle_artifacts<P: ArtifactSettlement>(
    outer_terminal: bool,
    repair: RepairArtifact,
    port: &mut P,
) -> Result<bool, P::Error> {
    let repair_work = repair == RepairArtifact::Complete;
    if repair_work {
        port.retire_repair_copy()?;
    }
    if outer_terminal {
        // Health/classifier Report is never an error here; metadata-only archival
        // precedes the following operation's independent admission.
        port.settle_outer_history()?;
    }
    Ok(repair_work || outer_terminal)
}

pub(crate) const DEFERRED: &[&str] = &[
    "Windows practice and UI verification (W4.1b)",
    "Windows Firewall rule and display driver",
    "Signing and distribution (W4.2)",
    "Attended Windows checks (W6.2/W6.4)",
];

#[cfg(all(windows, not(test)))]
pub(crate) mod native {
    use super::super::super::{
        native_io::{Cancellation, Deadline, MonotonicClock, WindowsNativeIo},
        payload::{
            inventory::ApprovedInventory,
            recovery::{OuterPhase, OuterUpgradeRecord},
        },
        repair::{
            JournalObservation, RepairDiagnostic as Diagnostic, RepairObservation, SourceKind,
            payload_record::PayloadRepairPhase,
        },
        service,
    };
    use super::*;
    use crosspane_installer_core::elevated::step::{
        Gate, INSTALL_WITHOUT_SETUP, PartResult, REMOVAL_NOT_STARTED, StepResult, admin_gate,
        removal_gate, report_lines,
    };
    use std::{path::PathBuf, sync::Arc};
    // Frozen administrator-step planning reasons (WP-W4.1c2 lead ruling L7), shown verbatim.
    const NO_KIT: &str = "This installer doesn't include the administrator setup kit, so the firewall rule and display driver are left as they are.";
    const RECORD_UNREADABLE: &str = "The record of earlier administrator steps can't be read, so the firewall rule and display driver are left as they are.";
    const NO_SOURCES: &str = "The administrator setup kit isn't installed. Run the installer with --payload set to this release's folder to set up the firewall rule and display driver.";
    const TOO_LONG: &str = "The administrator step is too long to show, so it isn't offered.";
    // NotRun reasons of a step that never reached the helper (the frozen N3 wording of elevated_step.rs).
    const KIT_NOT_RUN: &str =
        "the administrator setup kit isn't available or doesn't match this installer";
    const RECORD_NOT_RUN: &str =
        "the administrator setup record can't be read or belongs to another install";
    // Detect wording when the firewall rule and display driver are not checked (WP-W4.1c2 lead ruling L16).
    const NOT_CHECKED: &str = "Firewall rule: not checked. Display driver: not checked.";
    // The core status_summary pending note, repeated here because the core keeps it private (L16).
    const PENDING_NOTE: &str = " An earlier administrator step was interrupted; it is checked again before the next one runs.";
    pub(crate) struct NativeDomains {
        folder: Option<PathBuf>,
        previous: Option<RepairObservation>,
        version: u64,
        held: Option<Continuation>,
        first_refusal: Option<&'static str>,
        /// Set by an explicit first-install recovery; verification then uses its settled predicate.
        recovered: bool,
        /// Outcome lines of the administrator step, filled by `apply_elevated` and taken by the ADMIN outcome.
        elevated_report: Vec<String>,
    }
    enum Continuation {
        Upgrade(service::KeeperContinuation),
        Removal(Box<service::RemovalContinuation>),
        Repair(service::RepairContinuation),
    }
    fn state(d: Diagnostic) -> State {
        match d {
            Diagnostic::Healthy => State::Healthy,
            Diagnostic::Missing => State::Missing,
            Diagnostic::Mismatch => State::Mismatch,
            Diagnostic::Disabled => State::Disabled,
            Diagnostic::AccessDenied | Diagnostic::UnsafeForeign | Diagnostic::Unavailable => {
                State::Unavailable
            }
            Diagnostic::Unknown => State::Unknown,
        }
    }
    pub(crate) fn deadline(ms: u64) -> Result<Deadline, Failure> {
        Deadline::new(
            ms,
            Arc::new(MonotonicClock::default()),
            Cancellation::default(),
        )
        .map_err(|_| Failure::NotSubmitted)
    }
    impl NativeDomains {
        pub fn new(folder: Option<PathBuf>) -> Self {
            Self {
                folder,
                previous: None,
                version: 0,
                held: None,
                first_refusal: None,
                recovered: false,
                elevated_report: Vec::new(),
            }
        }
        fn inputs(&self) -> Result<[Box<dyn std::io::Read + Send>; 3], Failure> {
            use std::path::{Component, Prefix};
            let folder = self.folder.as_ref().ok_or(Failure::NotSubmitted)?;
            // Explicit local input only; no PATH, sibling, UNC or backup fallback.
            if !folder.is_absolute()
                || !matches!(folder.components().next(),
                Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
            {
                return Err(Failure::NotSubmitted);
            }
            let mut files: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
            for leaf in [
                "crosspane-agent.exe",
                "crosspane-ui.exe",
                "crosspanectl.exe",
            ] {
                let file =
                    std::fs::File::open(folder.join(leaf)).map_err(|_| Failure::NotSubmitted)?;
                if !file
                    .metadata()
                    .map_err(|_| Failure::NotSubmitted)?
                    .is_file()
                {
                    return Err(Failure::NotSubmitted);
                }
                files.push(Box::new(file));
            }
            files.try_into().map_err(|_| Failure::NotSubmitted)
        }
        fn handoff(&self, budget: &Deadline) -> Result<Handoff, Failure> {
            let Some(held) = &self.held else {
                return Ok(Handoff::NotCommitted);
            };
            let h = match held {
                Continuation::Upgrade(v) => v.integration_handoff(budget),
                Continuation::Removal(v) => v.integration_handoff(budget),
                Continuation::Repair(v) => v.integration_handoff(budget),
            }
            .map_err(|_| Failure::Unknown)?;
            Ok(match h {
                service::IntegrationHandoff::NotCommitted => Handoff::NotCommitted,
                service::IntegrationHandoff::Committed => Handoff::Committed,
                service::IntegrationHandoff::Complete => Handoff::Complete,
                service::IntegrationHandoff::Unknown => Handoff::Unknown,
            })
        }
    }
    /// Original read-only probe and bounded record reads. No integration mutation domain,
    /// lock, operation continuation or source reader is constructed by diagnose.
    pub(crate) fn read_observation() -> Result<(Snapshot, RepairObservation), Failure> {
        let budget = deadline(30_000)?;
        let probe = WindowsNativeIo::probe_repair(Arc::new(MonotonicClock::default()), &budget)
            .map_err(|_| Failure::NotSubmitted)?;
        let actual = probe.observation();
        let mut outer_terminal = false;
        let mut repair_terminal = false;
        let mut capacity_only = false;
        let mut cold = Cold::Unknown;
        let mut correlation = Vec::new();
        if let Some(io) = probe.io() {
            let proof = io
                .admit_support(&budget)
                .map_err(|_| Failure::NotSubmitted)?;
            if actual.publication() == Diagnostic::Unknown {
                capacity_only = io
                    .repair_capacity_only(&proof, actual.journals(), &budget)
                    .unwrap_or(false);
            }
            if let Ok(Some(r)) = OuterUpgradeRecord::read(io, &proof, &budget) {
                outer_terminal = matches!(r.phase(), OuterPhase::Complete | OuterPhase::Cancelled);
            }
            if let Ok(Some(r)) = io.read_payload_repair(&proof, &budget) {
                repair_terminal = r.phase() == PayloadRepairPhase::Complete;
            }
            use super::super::super::first_install::{
                self, FirstInstallDisposition as Disposition, FirstInstallFacts, History, Presence,
            };
            let observed = io.preview_first_install(&proof, &budget);
            let first = io.read_first_install(&proof, &budget);
            let active_first = first.as_ref().map_or(true, |r| {
                r.as_ref().is_some_and(|r| {
                    r.phase() != super::super::super::first_install::record::Phase::Complete
                        && !io.first_source_settled(&proof, r, &budget).unwrap_or(false)
                })
            });
            let history = match observed {
                Ok(Disposition::Eligible) => History::None,
                Ok(Disposition::Resume) => History::FirstInstall,
                Ok(Disposition::CompletedRemoval) => History::CompletedRemoval,
                Ok(Disposition::Partial) => History::Partial,
                Ok(Disposition::Existing) => History::Other,
                _ => History::Unknown,
            };
            let presence = |d| match d {
                Diagnostic::Missing => Presence::Missing,
                Diagnostic::Healthy | Diagnostic::Disabled | Diagnostic::Mismatch => {
                    Presence::Present
                }
                Diagnostic::AccessDenied => Presence::AccessDenied,
                _ => Presence::Unknown,
            };
            cold = match first_install::preview(FirstInstallFacts {
                task: presence(actual.task().diagnostic()),
                agent: presence(actual.agent()),
                supervisor: if matches!(observed, Ok(Disposition::Existing)) {
                    Presence::Present
                } else {
                    Presence::Missing
                },
                history,
            }) {
                Disposition::Eligible => Cold::Eligible,
                Disposition::CompletedRemoval => Cold::CompletedRemoval,
                Disposition::Resume => match first
                    .as_ref()
                    .ok()
                    .and_then(|r| r.as_ref())
                    .and_then(|r| first_install::reopen(r, io.target().identity()).ok())
                {
                    Some(first_install::Reopen::Restart(_)) => Cold::Eligible,
                    Some(first_install::Reopen::Observe(_)) => Cold::Observe,
                    None => Cold::Partial,
                },
                Disposition::Partial => Cold::Partial,
                Disposition::Existing => Cold::Existing,
                Disposition::AccessDenied => Cold::AccessDenied,
                _ => Cold::Unknown,
            };
            if let Ok(Some(record)) = &first {
                use super::super::super::first_install::record::Phase;
                let context: super::super::super::payload::recovery::OuterContextCorrelation =
                    serde_json::from_slice(record.context()).map_err(|_| Failure::NotSubmitted)?;
                let current = io.target().identity();
                if context.same_user(current).is_ok()
                    && record.phase() != Phase::Complete
                    && !io
                        .first_source_settled(&proof, record, &budget)
                        .unwrap_or(false)
                {
                    let rank = record.phase().rank();
                    if rank > Phase::Intent.rank() && rank < Phase::RunIntent.rank() {
                        // StageIntent through TaskRegistered: only a rollback or removal applies.
                        cold = Cold::Partial;
                    } else if rank >= Phase::RunIntent.rank()
                        && record.phase() != Phase::Unknown
                        && context.authentication_id() != current.authentication_id
                    {
                        // Stale only across a logon change; the same logon keeps the reopen mapping.
                        cold = Cold::Stale;
                    }
                    // Otherwise keep the a8 mapping: a pristine Intent restarts, and same-logon
                    // RunIntent and later observe.
                }
            }
            if !active_first
                && (actual.task().diagnostic() != Diagnostic::Missing
                    || actual.agent() != Diagnostic::Missing)
            {
                if matches!(
                    actual.task().diagnostic(),
                    Diagnostic::Healthy | Diagnostic::Disabled
                ) || actual.agent() == Diagnostic::Healthy
                {
                    cold = Cold::Existing;
                } else if actual.task().diagnostic() == Diagnostic::AccessDenied
                    || actual.agent() == Diagnostic::AccessDenied
                {
                    cold = Cold::AccessDenied;
                } else {
                    cold = Cold::Unknown;
                }
            }
            if matches!(first, Ok(None)) {
                use super::super::super::first_install::record::{
                    FirstRecoveryCursor as Cursor, FirstRecoveryMode as Mode,
                };
                // A recovery record that outlived first-install.json still owns the partial or stale state.
                match io.read_first_recovery(&proof, &budget) {
                    Ok(Some(recovery)) if recovery.cursor != Cursor::Retired => {
                        cold = match recovery.mode {
                            Mode::Rollback | Mode::Remove => Cold::Partial,
                            Mode::Supersede | Mode::RetireStale => Cold::Stale,
                        };
                    }
                    Ok(_) => {}
                    // An unreadable recovery record fails closed.
                    Err(_) => cold = Cold::Unknown,
                }
            }
            if let Ok(Some(record)) = first {
                let bytes = record.encode().map_err(|_| Failure::NotSubmitted)?;
                correlation.extend_from_slice(
                    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &bytes).as_ref(),
                );
            }
            // Correlation only: an absent record adds nothing, and an unreadable one adds a fixed
            // marker instead of failing the observation.
            use super::super::super::elevated_store;
            match elevated_store::read(io, &budget) {
                Ok(Some((_, sha))) => correlation.extend_from_slice(&sha),
                Ok(None) => {}
                Err(_) => correlation.extend_from_slice(&[0xEE; 32]),
            }
        }
        if [actual.task().diagnostic(), actual.agent(), actual.payload()]
            .contains(&Diagnostic::AccessDenied)
        {
            cold = Cold::AccessDenied;
        }
        let terminal_history = outer_terminal
            || repair_terminal
            || actual
                .journals()
                .iter()
                .any(|j| matches!(j, JournalObservation::Terminal(_)));
        let unsettled = publication_unsettled(actual.publication() == Diagnostic::Healthy, capacity_only) || actual.journals().iter()
                .any(|j| matches!(j, JournalObservation::Retained(kind, _) if !(*kind == SourceKind::OuterUpgrade && outer_terminal)));
        let snapshot = Snapshot {
            supported: probe.io().is_some(),
            inventory: ApprovedInventory::embedded().is_ok(),
            sources: false,
            payload: state(actual.payload()),
            task: state(actual.task().diagnostic()),
            agent: state(actual.agent()),
            cold,
            terminal_history,
            unsettled,
            correlation,
            source: ObservationSource::Live,
        };
        Ok((snapshot, actual.clone()))
    }
    impl NativeDomains {
        /// The first-install outcome mapping, shared by the plain and the administrator-step Install.
        fn first_install_dispatch(
            &mut self,
            outcome: service::FirstInstallOutcome,
        ) -> Result<Dispatch, Failure> {
            match outcome {
                service::FirstInstallOutcome::Complete => Ok(Dispatch {
                    handoff: Handoff::NotCommitted,
                    complete: true,
                }),
                service::FirstInstallOutcome::NotSubmitted(reason) => {
                    self.first_refusal = Some(reason);
                    Err(Failure::NotSubmitted)
                }
                service::FirstInstallOutcome::Unknown => Err(Failure::Unknown),
            }
        }
        /// Install with the administrator step between FilesVerified and TaskIntent (N5). Without the
        /// kit the install runs alone and the report says so.
        fn apply_first_install_elevated(
            &mut self,
            plan: &ElevatedPlan,
        ) -> Result<Dispatch, Failure> {
            use super::super::super::elevated_kit;
            self.first_refusal = None;
            self.recovered = false;
            let Some(manifest) = elevated_kit::embedded() else {
                let (outcome, _) = service::begin_first_install_elevated(
                    self.inputs()?,
                    None,
                    &deadline(120_000)?,
                )
                .map_err(|_| Failure::Unknown)?;
                let dispatch = self.first_install_dispatch(outcome);
                let mut lines = report_lines(&StepResult::not_run(plan.verb(), KIT_NOT_RUN));
                // L16: "installed without them" is said only when the install itself completed.
                if matches!(dispatch, Ok(Dispatch { complete: true, .. })) {
                    lines.push(INSTALL_WITHOUT_SETUP.to_owned());
                }
                self.elevated_report = lines;
                return dispatch;
            };
            // An unreadable source folder counts as no sources, and the placed kit is used instead.
            let sources = self
                .folder
                .as_deref()
                .and_then(|folder| elevated_kit::read_sources(folder, &manifest).ok());
            let elevated = service::ElevatedInstall {
                plan: plan.clone(),
                manifest,
                sources,
            };
            let (outcome, step) = service::begin_first_install_elevated(
                self.inputs()?,
                Some(elevated),
                &deadline(120_000)?,
            )
            .map_err(|_| Failure::Unknown)?;
            let dispatch = self.first_install_dispatch(outcome);
            // The step's report is kept whatever the install's own outcome; the outcome is never changed by it.
            if let Some(step) = step {
                let mut lines = report_lines(&step);
                // L16: "installed without them" is said only when the install completed, no part
                // waits for a restart, and the step is not fully verified.
                let completed = matches!(dispatch, Ok(Dispatch { complete: true, .. }));
                let restart = step
                    .parts
                    .iter()
                    .any(|(_, part)| *part == PartResult::RebootRequired);
                if completed && !restart && !step.all_verified() {
                    lines.push(INSTALL_WITHOUT_SETUP.to_owned());
                }
                self.elevated_report = lines;
            }
            dispatch
        }
        /// Runs one consented administrator step with the journal's own per-write lock. Nothing is held
        /// across it. Any failure before the launch is `NotRun`, so nothing elevated has happened.
        fn run_elevated_step(&self, plan: &ElevatedPlan) -> StepResult {
            use super::super::super::{
                elevated_kit,
                elevated_step::{self, ElevatedRun},
                elevated_store::LockSource,
            };
            let verb = plan.verb();
            let Some(manifest) = elevated_kit::embedded() else {
                return StepResult::not_run(verb, KIT_NOT_RUN);
            };
            let sources = self
                .folder
                .as_deref()
                .and_then(|folder| elevated_kit::read_sources(folder, &manifest).ok());
            let Ok(budget) = deadline(30_000) else {
                return StepResult::not_run(verb, RECORD_NOT_RUN);
            };
            let Ok(probe) =
                WindowsNativeIo::probe_repair(Arc::new(MonotonicClock::default()), &budget)
            else {
                return StepResult::not_run(verb, RECORD_NOT_RUN);
            };
            let Some(io) = probe.io() else {
                return StepResult::not_run(verb, RECORD_NOT_RUN);
            };
            elevated_step::run(ElevatedRun {
                io,
                lock: LockSource::PerWrite,
                plan,
                manifest: &manifest,
                sources: sources.as_ref(),
            })
        }
    }
    impl Domains for NativeDomains {
        fn observe(&mut self) -> Result<Snapshot, Failure> {
            let (mut snapshot, actual) = read_observation()?;
            if self.previous.as_ref() != Some(&actual) {
                self.version = self.version.saturating_add(1);
                self.previous = Some(actual);
            }
            snapshot
                .correlation
                .extend_from_slice(&self.version.to_le_bytes());
            snapshot.sources = self.folder.is_some();
            Ok(snapshot)
        }
        fn settle(&mut self, op: Operation) -> Result<bool, Failure> {
            if matches!(op, Operation::Install | Operation::Elevated)
                || matches!(read_observation()?.0.cold, Cold::Partial | Cold::Stale)
            {
                return Ok(false);
            }
            service::settle_operation_artifacts(op, &deadline(30_000)?)
                .map_err(|_| Failure::Unknown)
        }
        fn apply(&mut self, op: Operation) -> Result<Dispatch, Failure> {
            self.first_refusal = None;
            self.recovered = false;
            if op == Operation::Install {
                let outcome = service::begin_first_install(self.inputs()?, &deadline(120_000)?)
                    .map_err(|_| Failure::Unknown)?;
                return self.first_install_dispatch(outcome);
            }
            let cold = read_observation()?.0.cold;
            if matches!(cold, Cold::Partial | Cold::Stale) {
                use super::super::super::first_install::record::{
                    FirstRecoveryMode as M, FirstRecoveryOutcome as O,
                };
                let mode = match op {
                    Operation::MetadataRepair => {
                        if cold == Cold::Stale {
                            M::Supersede
                        } else {
                            M::Rollback
                        }
                    }
                    Operation::Removal {
                        erase_identity: false,
                    } if cold == Cold::Partial => M::Remove,
                    _ => return Err(Failure::NotSubmitted),
                };
                let budget = deadline(120_000)?;
                self.recovered = true;
                // L17: a partial first install cut after FilesVerified can leave the placed elevated kit in the
                // install root, and the recovery's removal never deletes a non-empty root. The plain kit files
                // and the then-empty driver directory go first. Any error may follow a removal, so it is Unknown.
                if matches!(mode, M::Remove) {
                    let probe_budget = deadline(30_000)?;
                    let probe = WindowsNativeIo::probe_repair(
                        Arc::new(MonotonicClock::default()),
                        &probe_budget,
                    )
                    .map_err(|_| Failure::NotSubmitted)?;
                    let io = probe.io().ok_or(Failure::NotSubmitted)?;
                    let install_text = io.target().paths().install();
                    let install = std::path::Path::new(install_text);
                    if super::super::super::elevated_kit::remove_placed(install).is_err() {
                        return Err(Failure::Unknown);
                    }
                }
                let outcome = match service::recover_first_install(mode, &budget) {
                    Ok(outcome) => outcome,
                    // Pre: refused before any recovery effect, so nothing was submitted.
                    Err(service::RecoveryError::Pre(_)) => return Err(Failure::NotSubmitted),
                    // Post: an effect may have run, so the outcome is unresolved.
                    Err(service::RecoveryError::Post(_)) => return Err(Failure::Unknown),
                };
                if matches!(outcome, O::Retained { .. }) {
                    return Err(Failure::Unknown);
                }
                // Recovery is complete, but no healthy install is inferred and no new install is chained.
                return Ok(Dispatch {
                    handoff: Handoff::NotCommitted,
                    complete: true,
                });
            }
            if matches!(op, Operation::MetadataRepair) {
                let complete = service::integration_metadata_repair(&deadline(30_000)?)
                    .map_err(|_| Failure::Unknown)?;
                return Ok(Dispatch {
                    handoff: Handoff::NotCommitted,
                    complete,
                });
            }
            let continuation = match op {
                Operation::Upgrade => Continuation::Upgrade(
                    service::begin_outer_upgrade(self.inputs()?).map_err(|_| Failure::Unknown)?,
                ),
                Operation::Removal { erase_identity } => Continuation::Removal(Box::new(
                    service::begin_removal(erase_identity).map_err(|_| Failure::Unknown)?,
                )),
                Operation::PayloadRepair => Continuation::Repair(
                    service::begin_payload_repair(self.inputs()?).map_err(|_| Failure::Unknown)?,
                ),
                Operation::Install | Operation::MetadataRepair | Operation::Elevated => {
                    return Err(Failure::NotSubmitted);
                }
            };
            self.held = Some(continuation); // Preserve the actual owner before any late observation.
            // The facade has returned an actual continuation. Any later refusal is
            // Unknown, including failure to construct this fresh observation budget.
            let handoff_budget = deadline(30_000).map_err(|_| Failure::Unknown)?;
            let handoff = self.handoff(&handoff_budget)?;
            if !handoff.permits_exit() {
                return Err(Failure::Unknown);
            }
            Ok(Dispatch {
                handoff,
                complete: handoff == Handoff::Complete,
            })
        }
        fn take_first_refusal(&mut self) -> Option<&'static str> {
            self.first_refusal.take()
        }
        /// Applies with the consent-bound administrator step when there is one (N5). The step runs
        /// first; a removal proceeds only when its gate allows, and the report is kept for the outcome.
        fn apply_elevated(
            &mut self,
            op: Operation,
            e: Option<&ElevatedPlan>,
        ) -> Result<Dispatch, Failure> {
            self.elevated_report.clear();
            let Some(plan) = e else {
                return self.apply(op);
            };
            match op {
                Operation::Install => self.apply_first_install_elevated(plan),
                Operation::Removal { .. } => {
                    let step = self.run_elevated_step(plan);
                    self.elevated_report = report_lines(&step);
                    match removal_gate(&step) {
                        Gate::Proceed => self.apply(op),
                        Gate::NotSubmitted => {
                            self.elevated_report.push(REMOVAL_NOT_STARTED.to_owned());
                            Err(Failure::NotSubmitted)
                        }
                        Gate::Unknown => Err(Failure::Unknown),
                    }
                }
                Operation::Elevated => {
                    let step = self.run_elevated_step(plan);
                    self.elevated_report = report_lines(&step);
                    match admin_gate(&step) {
                        Gate::Proceed => Ok(Dispatch {
                            handoff: Handoff::NotCommitted,
                            complete: step.all_verified(),
                        }),
                        Gate::NotSubmitted => Err(Failure::NotSubmitted),
                        Gate::Unknown => Err(Failure::Unknown),
                    }
                }
                Operation::Upgrade | Operation::MetadataRepair | Operation::PayloadRepair => {
                    self.apply(op)
                }
            }
        }
        /// The outcome lines of the last administrator step, taken once.
        fn take_elevated_report(&mut self) -> Vec<String> {
            std::mem::take(&mut self.elevated_report)
        }
        /// Read-only planning. The record is only read, the kit is only verified or read, and
        /// nothing is elevated or placed here.
        fn elevated_plan(&mut self, r: ElevatedRequest) -> Result<ElevatedPlanning, Failure> {
            use super::super::super::{elevated_kit, elevated_step, elevated_store};
            use crosspane_installer_core::elevated::{
                AgentProgram, DriverState, FirewallState, InstallId, RuleScope, Verb,
            };
            let Some(manifest) = elevated_kit::embedded() else {
                return Ok(ElevatedPlanning::Unavailable(NO_KIT));
            };
            let budget = deadline(30_000)?;
            let probe = WindowsNativeIo::probe_repair(Arc::new(MonotonicClock::default()), &budget)
                .map_err(|_| Failure::NotSubmitted)?;
            // Without a supported target the record cannot be read, so it is never overwritten.
            let Some(io) = probe.io() else {
                return Ok(ElevatedPlanning::Unavailable(RECORD_UNREADABLE));
            };
            let record = match elevated_store::read(io, &budget) {
                Ok(record) => record.map(|(record, _)| record),
                Err(_) => return Ok(ElevatedPlanning::Unavailable(RECORD_UNREADABLE)),
            };
            let install_text = io.target().paths().install();
            let install = std::path::Path::new(install_text);
            let placed = elevated_kit::verify_placed(install, &manifest).is_ok();
            // An unreadable source folder counts as no sources.
            let sources = self
                .folder
                .as_deref()
                .is_some_and(|folder| elevated_kit::read_sources(folder, &manifest).is_ok());
            let verb = match r {
                ElevatedRequest::Setup => {
                    if !placed && !sources {
                        return Ok(ElevatedPlanning::Unavailable(NO_SOURCES));
                    }
                    let scope = match &record {
                        Some(record) => record.scope(),
                        // No record: the install id is fresh, so no earlier rule is named.
                        None => {
                            let mut random = [0; 16];
                            aws_lc_rs::rand::fill(&mut random)
                                .map_err(|_| Failure::NotSubmitted)?;
                            RuleScope {
                                id: InstallId::from_random(random),
                                program: AgentProgram::parse(&format!(
                                    r"{install_text}\crosspane-agent.exe"
                                ))
                                .map_err(|_| Failure::NotSubmitted)?,
                            }
                        }
                    };
                    Verb::Setup(scope)
                }
                ElevatedRequest::Teardown => {
                    // Without a record there is nothing this install set up to remove.
                    let Some(record) = record else {
                        return Ok(ElevatedPlanning::NotNeeded);
                    };
                    let scope = record.scope();
                    let absent = placed
                        && elevated_step::observe(io, &manifest, &scope).is_ok_and(|report| {
                            report.firewall.state == FirewallState::Missing
                                && report.driver.state == DriverState::Absent
                        });
                    if absent {
                        return Ok(ElevatedPlanning::NotNeeded);
                    }
                    if !placed && !sources {
                        return Ok(ElevatedPlanning::Unavailable(NO_SOURCES));
                    }
                    Verb::Teardown(scope)
                }
            };
            Ok(match ElevatedPlan::new(verb) {
                Ok(plan) => ElevatedPlanning::Planned(plan),
                Err(_) => ElevatedPlanning::Unavailable(TOO_LONG),
            })
        }
        /// Read-only detection. With a record it is the record's scope; without one the firewall
        /// rule is not checked, and only the placed helper's unscoped status is read.
        fn elevated_detect(&mut self) -> Result<ElevatedDetection, Failure> {
            use super::super::super::{
                elevated_kit, elevated_launch::ElevatedHelper, elevated_step, elevated_store,
            };
            use crosspane_installer_core::elevated::step::status_summary;
            let manifest = elevated_kit::embedded().ok_or(Failure::NotSubmitted)?;
            let budget = deadline(30_000)?;
            let probe = WindowsNativeIo::probe_repair(Arc::new(MonotonicClock::default()), &budget)
                .map_err(|_| Failure::NotSubmitted)?;
            let io = probe.io().ok_or(Failure::NotSubmitted)?;
            let record = elevated_store::read(io, &budget).map_err(|_| Failure::NotSubmitted)?;
            let Some((record, _)) = record else {
                let install_text = io.target().paths().install();
                let Ok(image) =
                    elevated_kit::verify_placed(std::path::Path::new(install_text), &manifest)
                else {
                    return Ok(ElevatedDetection {
                        configured: false,
                        detail: NOT_CHECKED.to_owned(),
                    });
                };
                let report = ElevatedHelper::locate(image)
                    .and_then(|helper| helper.status(None))
                    .map_err(|_| Failure::NotSubmitted)?;
                let (configured, detail) = status_summary(&report, false);
                return Ok(ElevatedDetection { configured, detail });
            };
            let pending = !record.pending().is_empty();
            // L16: a placed kit that can't be verified is reported as not checked, not refused, so
            // Detect stays available and Plan can place the kit from --payload sources.
            let Ok(report) = elevated_step::observe(io, &manifest, &record.scope()) else {
                let mut detail = NOT_CHECKED.to_owned();
                if pending {
                    detail.push_str(PENDING_NOTE);
                }
                return Ok(ElevatedDetection {
                    configured: false,
                    detail,
                });
            };
            let (configured, detail) = status_summary(&report, pending);
            Ok(ElevatedDetection { configured, detail })
        }
        fn verify(&mut self, op: Operation) -> Result<bool, Failure> {
            if op == Operation::Elevated {
                return self.elevated_detect().map(|check| check.configured);
            }
            let observed = self.observe()?;
            if self.recovered && op == Operation::MetadataRepair {
                return Ok(observed.first_recovery_settled());
            }
            Ok(observed.verified(op))
        }
    }
}
