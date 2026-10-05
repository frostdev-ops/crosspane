//! Consent and order for one selected installation. Crash records never mint clean authority.
//! WP-4.21 must budget firewall calls separately (at most 120 seconds), then issue fresh bounded
//! deadlines after known completion. An exhausted firewall call keeps durable pending evidence;
//! the next fresh stage call records Unknown before any further mutation, without firewall replay.
//! With no original watch, only a newly consented run may stop the current owned unit, once.
//! That exception supplies no clean authority; identity and recovery remain retained.
use super::super::{CleanAuthority, IdentityChoice, TrackedAgent, admit_erase_output};
use super::*;
use crate::agent_contract::EraseIdentityV1;
use crate::platform::linux::{
    firewall::{FirewallError, RuleKind},
    native_io::{ChildEnvironment, LinuxNativeIo, ManagerMutation, NativeError, PendingOperation},
    payload::{CLEANUP_FILES as FILES, PayloadInstaller},
    service::LinuxService,
};

pub(super) type UninstallResult<T> = std::result::Result<T, UninstallError>;
#[derive(Clone, PartialEq, Eq, thiserror::Error)]
pub enum UninstallError {
    #[error(transparent)]
    Removal(#[from] RemovalError),
    #[error(transparent)]
    Firewall(#[from] FirewallError),
    #[error("retired, superseded or out-of-order uninstall")]
    Stale,
    #[error("intent and current resources do not describe this uninstall")]
    Invalid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UninstallStage {
    Disable,
    Firewall,
    Stop,
    Exit,
    Identity,
    Files,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UninstallForm {
    AwaitingOriginalCleanExit,
    NotClean,
    Complete,
}
#[derive(Clone, PartialEq, Eq)]
pub enum UninstallIssue {
    Autostart(RemovalError),
    Stop(RemovalError),
    CleanExit(RemovalError),
    Identity(RemovalError),
    Rule(RuleKind, FirewallError),
    RuleOutcome(
        RuleKind,
        crate::platform::linux::firewall::RuleResult,
        crate::platform::linux::firewall::Presence,
    ),
    Resource(usize, RemovalError),
    Durability(RemovalError),
    RepairIntent(RemovalError),
}
/// Data-only partial results. Empty Crosspane directories remain: the ledger owns files only.
pub struct UninstallReport {
    pub form: UninstallForm,
    pub stage: UninstallStage,
    pub progress: CleanupProgress,
    pub identity_receipt: Option<EraseIdentityV1>,
    pub issues: Vec<UninstallIssue>,
    pub empty_directories_retained: bool,
    pub recovery_retained: bool,
    pub identity_retained: bool,
}
#[derive(Default)]
struct UninstallBinding {
    last: Option<(u64, OperationId)>,
    active: bool,
}
#[derive(Default)]
pub struct UninstallPlanner {
    current: Arc<Mutex<UninstallBinding>>,
}
#[derive(Clone)]
pub struct UninstallPlan {
    pub(super) cleanup: CleanupPlan,
    pub(super) original: Option<Arc<TrackedAgent>>,
    pub(super) target: Arc<LinuxNativeIo>,
    current: Arc<Mutex<UninstallBinding>>,
}
pub struct UninstallConsent {
    cleanup: CleanupConsent,
    revision: u64,
    operation: OperationId,
    current: Arc<Mutex<UninstallBinding>>,
}
// Minted only after genuine current consent and durable begin/resume; never supplied by callers.
struct RenewedUnitStop;
pub struct UninstallRun {
    pub(super) plan: UninstallPlan,
    consent: UninstallConsent,
    service: Arc<LinuxService>,
    pub(super) progress: CleanupProgress,
    pub(super) stage: UninstallStage,
    pub(super) issues: Vec<UninstallIssue>,
    clean: Option<CleanAuthority>,
    identity_receipt: Option<EraseIdentityV1>,
    pending: Option<PendingOperation>,
    retired: bool,
    pub(super) resume: Option<CleanupProgress>,
    pub(super) rule_serial: u64,
    pub(super) rule_pending: Option<(RuleKind, OperationId, Arc<()>)>,
    renewed_unit_stop: Option<RenewedUnitStop>,
    repair_intent_removed: bool,
}
type_only_debug!(
    UninstallError,
    UninstallIssue,
    UninstallReport,
    UninstallPlanner,
    UninstallPlan,
    UninstallConsent,
    UninstallRun
);

impl UninstallPlanner {
    /// The watch is an immutable original binding. Authenticated context equality associates it; only its
    /// genuine CleanAuthority and the captured CleanupLease authorize later destructive steps.
    pub fn plan(
        &self,
        cleanup: CleanupPlan,
        target: Arc<LinuxNativeIo>,
        original: Option<Arc<TrackedAgent>>,
    ) -> UninstallResult<UninstallPlan> {
        let binding = cleanup.inventory.target_binding();
        if binding != target.target_binding()
            || original
                .as_ref()
                .is_some_and(|watch| watch.target_binding() != binding)
        {
            return Err(RemovalError::NotClean.into());
        }
        let paths = PayloadInstaller::new(target.clone())
            .map_err(RemovalError::from)?
            .cleanup_targets();
        if cleanup.resources().len() != FILES.len()
            || cleanup
                .resources()
                .iter()
                .zip(&paths)
                .zip(FILES)
                .any(|((row, path), name)| {
                    row.receipt.resource_id != name || row.receipt.resolved_path != *path
                })
        {
            return Err(UninstallError::Invalid);
        }
        let mut current = self.current.try_lock().map_err(|_| UninstallError::Stale)?;
        let pair = (cleanup.revision(), cleanup.operation());
        if pair.0 == 0
            || pair.1.0 == 0
            || current
                .last
                .is_some_and(|(r, o)| pair.0 <= r || pair.1.0 <= o.0)
        {
            return Err(UninstallError::Stale);
        }
        if original.as_ref().is_some_and(|watch| {
            watch.original().uid != target.target().paths().uid
                || watch.original().executable != target.target().agent_path()
                || cleanup
                    .resources()
                    .first()
                    .is_none_or(|r| r.receipt.resolved_path != watch.original().executable)
        }) {
            return Err(UninstallError::Invalid);
        }
        current.last = Some(pair);
        current.active = true;
        Ok(UninstallPlan {
            cleanup,
            original,
            target,
            current: self.current.clone(),
        })
    }
    /// Separate consent includes conditional removal of recovery rows after the original clean exit.
    pub fn consent(
        &self,
        plan: &UninstallPlan,
        cleanup: CleanupConsent,
        revision: u64,
        operation: OperationId,
    ) -> UninstallResult<UninstallConsent> {
        let current = self.current.try_lock().map_err(|_| UninstallError::Stale)?;
        if !Arc::ptr_eq(&self.current, &plan.current)
            || !current.active
            || current.last != Some((revision, operation))
            || (revision, operation) != (plan.cleanup.revision(), plan.cleanup.operation())
        {
            return Err(UninstallError::Stale);
        }
        Ok(UninstallConsent {
            cleanup,
            revision,
            operation,
            current: self.current.clone(),
        })
    }
}
impl UninstallPlan {
    pub fn form(&self) -> UninstallForm {
        if self.original.is_some() {
            UninstallForm::AwaitingOriginalCleanExit
        } else {
            UninstallForm::NotClean
        }
    }
    pub fn actions(&self) -> Vec<ResourceAction> {
        self.cleanup
            .resources()
            .iter()
            .map(|r| {
                if r.action == ResourceAction::RetainRecovery && self.original.is_some() {
                    ResourceAction::RemoveAfterCleanExit
                } else {
                    r.action
                }
            })
            .collect()
    }
    pub fn resources(&self) -> &[CleanupResource] {
        self.cleanup.resources()
    }
    pub fn preview(&self) -> String {
        let rows = self
            .cleanup
            .resources()
            .iter()
            .zip(self.actions())
            .map(|(r, action)| format!("{:?}: {}", action, r.receipt.resolved_path))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Disable owned autostart; separately consented global LAN/mDNS removal; stop; original clean exit; selected identity/pairing erase; owned files; conditional recovery removal LAST.\nIdentity choice: {:?}. Remote offline trust remains. Activity is unverified: consent permits interruption of this selected installation. Missing clean proof retains identity and recovery. Empty directories may remain.\n{rows}",
            self.cleanup.selection().identity
        )
    }
    pub fn begin(
        self,
        consent: UninstallConsent,
        service: Arc<LinuxService>,
        deadline: &Deadline,
    ) -> UninstallResult<UninstallRun> {
        self.start(consent, service, false, deadline)
    }
    /// A fresh captured inventory and new consent reconcile the durable record. No process
    /// watch, clean receipt field or finished flag is reconstructed into clean authority.
    pub fn resume(
        self,
        consent: UninstallConsent,
        service: Arc<LinuxService>,
        deadline: &Deadline,
    ) -> UninstallResult<UninstallRun> {
        self.start(consent, service, true, deadline)
    }
    fn start(
        self,
        consent: UninstallConsent,
        service: Arc<LinuxService>,
        resume: bool,
        deadline: &Deadline,
    ) -> UninstallResult<UninstallRun> {
        if service.target_binding() != self.cleanup.inventory.target_binding() {
            return Err(RemovalError::NotClean.into());
        }
        let selection = self.cleanup.selection();
        let mut run = UninstallRun {
            plan: self,
            consent,
            service,
            stage: UninstallStage::Disable,
            progress: CleanupProgress {
                stage: CleanupStage::Prepared,
                resources: [CleanupResult::Pending; FILES.len()],
                autostart: CleanupResult::Pending,
                stop: CleanupResult::Pending,
                identity: if selection.identity == IdentityChoice::Keep {
                    CleanupResult::Kept
                } else {
                    CleanupResult::Pending
                },
                lan: if selection.lan_rule {
                    CleanupResult::Pending
                } else {
                    CleanupResult::Kept
                },
                mdns: if selection.mdns_rule {
                    CleanupResult::Pending
                } else {
                    CleanupResult::Kept
                },
            },
            clean: None,
            identity_receipt: None,
            pending: None,
            retired: false,
            resume: None,
            issues: Vec::new(),
            rule_serial: 0,
            rule_pending: None,
            renewed_unit_stop: None,
            repair_intent_removed: false,
        };
        run.guarded(|run| {
            let old = run
                .plan
                .cleanup
                .read_intent(&run.consent.cleanup, deadline)?;
            match (resume, old) {
                (false, None) => run.persist(deadline),
                (true, Some(record)) => run.admit_resume(record, deadline),
                _ => Err(UninstallError::Invalid),
            }
        })?;
        if run.plan.original.is_none() && !run.resumed_attempt(CleanupStage::StopObserved) {
            run.renewed_unit_stop = Some(RenewedUnitStop);
        }
        Ok(run)
    }
}
impl UninstallRun {
    pub(super) fn guarded<T>(
        &mut self,
        call: impl FnOnce(&mut Self) -> UninstallResult<T>,
    ) -> UninstallResult<T> {
        let owner = self.plan.current.clone();
        let mut binding = owner.try_lock().map_err(|_| UninstallError::Stale)?;
        if self.retired
            || !binding.active
            || binding.last != Some((self.consent.revision, self.consent.operation))
            || !Arc::ptr_eq(&owner, &self.consent.current)
            || self.pending.as_ref().is_some_and(|p| !p.completed())
        {
            return Err(UninstallError::Stale);
        }
        let result = call(self);
        if result
            .as_ref()
            .is_err_and(|e| !matches!(e, UninstallError::Firewall(_)))
        {
            binding.active = false;
            self.retired = true;
            self.clean = None;
        }
        result
    }
    pub(super) fn require(&self, stage: UninstallStage) -> UninstallResult<()> {
        if self.stage != stage {
            return Err(UninstallError::Stale);
        }
        Ok(())
    }
    pub(super) fn persist(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        if let Err(error) =
            self.plan
                .cleanup
                .write_intent(&self.consent.cleanup, self.progress.clone(), deadline)
        {
            self.issues.push(UninstallIssue::Durability(error.clone()));
            return Err(error.into());
        }
        Ok(())
    }
    pub fn stage(&self) -> UninstallStage {
        self.stage
    }
    pub fn pending(&self) -> Option<&PendingOperation> {
        self.pending.as_ref()
    }
    pub fn disable(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Disable)?;
            let observed = run.service.observe(deadline);
            run.mark(CleanupStage::Disabled);
            run.persist(deadline)?;
            if observed.as_ref().is_ok_and(|f| !f.enabled) {
                run.progress.autostart = CleanupResult::AlreadyAbsent;
            } else if run.resumed_attempt(CleanupStage::Disabled) {
                run.progress.autostart = CleanupResult::Unknown;
            } else {
                let mutation =
                    run.plan
                        .cleanup
                        .with_lease(&run.consent.cleanup, deadline, |lease| {
                            Ok(lease.disable(run.service.clone(), deadline))
                        })?;
                run.manager_outcome(mutation, true)?;
                if run.progress.autostart == CleanupResult::Removed
                    && !run.service.observe(deadline).is_ok_and(|f| !f.enabled)
                {
                    run.progress.autostart = CleanupResult::Unknown;
                }
            }
            run.persist(deadline)?;
            run.stage = UninstallStage::Firewall;
            run.rules_done();
            Ok(())
        })
    }
    pub fn stop(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Stop)?;
            run.mark(CleanupStage::StopObserved);
            run.persist(deadline)?;
            let before = run.service.observe(deadline);
            if before.is_err()
                || run.plan.original.as_ref().is_some_and(|watch| {
                    !before.as_ref().is_ok_and(|facts| {
                        facts.main_pid == 0 || facts.main_pid == watch.original().pid
                    })
                })
            {
                run.progress.stop = CleanupResult::Refused;
                run.issues
                    .push(UninstallIssue::Stop(RemovalError::NotClean));
            } else if run.resumed_attempt(CleanupStage::StopObserved) {
                run.progress.stop = if run
                    .service
                    .observe(deadline)
                    .is_ok_and(|f| f.main_pid == 0 && f.active_state == "inactive")
                {
                    CleanupResult::AlreadyAbsent
                } else {
                    CleanupResult::Unknown
                };
            } else {
                let mutation =
                    run.plan
                        .cleanup
                        .with_lease(&run.consent.cleanup, deadline, |lease| {
                            Ok(match &run.plan.original {
                                Some(watch) => watch.revalidate_running(deadline).map(|()| {
                                    lease.stop_original(
                                        run.service.clone(),
                                        watch.running_check(),
                                        deadline,
                                    )
                                }),
                                // Freshly renewed consent plus this call's resource/service checks
                                // permits the owned unit stop, never original clean authority.
                                None if run.renewed_unit_stop.take().is_some() => {
                                    Ok(lease.stop(run.service.clone(), deadline))
                                }
                                None => Err(RemovalError::NotClean),
                            })
                        })?;
                match mutation {
                    Ok(mutation) => run.manager_outcome(mutation, false)?,
                    Err(error) => {
                        run.progress.stop = CleanupResult::Refused;
                        run.issues.push(UninstallIssue::Stop(error));
                    }
                }
            }
            run.persist(deadline)?;
            run.stage = UninstallStage::Exit;
            Ok(())
        })
    }
    fn manager_outcome(&mut self, mutation: ManagerMutation, disable: bool) -> UninstallResult<()> {
        self.pending = mutation.pending;
        let (result, error) = match mutation.result {
            Ok(output) if output.code == Some(0) => (CleanupResult::Removed, None),
            Ok(_) => (
                CleanupResult::Failed,
                Some(RemovalError::Native(NativeError::Unavailable)),
            ),
            Err(error) => (
                if error == NativeError::Foreign {
                    CleanupResult::Refused
                } else {
                    CleanupResult::Unknown
                },
                Some(error.into()),
            ),
        };
        if disable {
            self.progress.autostart = result;
        } else {
            self.progress.stop = result;
        }
        if let Some(error) = error {
            self.issues.push(if disable {
                UninstallIssue::Autostart(error)
            } else {
                UninstallIssue::Stop(error)
            });
        }
        if self.pending.is_some() || result == CleanupResult::Unknown {
            return Err(RemovalError::Native(NativeError::OutcomeUnknown).into());
        }
        Ok(())
    }
    pub fn observe_exit(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Exit)?;
            let stopped = run.service.observe(deadline).is_ok_and(|facts| {
                !facts.enabled && facts.main_pid == 0 && facts.active_state == "inactive"
            });
            if run.resume.is_none()
                && stopped
                && matches!(
                    run.progress.stop,
                    CleanupResult::Removed | CleanupResult::AlreadyAbsent
                )
                && let Some(original) = &run.plan.original
            {
                match original.clean_authority(deadline) {
                    Ok(authority) => {
                        run.clean = Some(authority);
                    }
                    Err(error) => {
                        run.issues.push(UninstallIssue::CleanExit(error));
                    }
                }
            }
            if run.clean.is_none() {
                run.progress.stop = CleanupResult::Unknown;
            }
            run.persist(deadline)?;
            run.stage = UninstallStage::Identity;
            Ok(())
        })
    }
    pub fn identity(
        &mut self,
        digest: [u8; 32],
        environment: ChildEnvironment,
        deadline: &Deadline,
    ) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Identity)?;
            run.mark(CleanupStage::IdentityObserved);
            run.persist(deadline)?;
            if run.plan.cleanup.selection().identity == IdentityChoice::Keep
                || run.clean.is_none()
                || run.resume.is_some()
            {
                run.progress.identity = CleanupResult::Kept;
            } else {
                run.revalidate_clean(deadline)?;
                let command = run
                    .clean
                    .as_ref()
                    .ok_or(UninstallError::Invalid)?
                    .erase_command(digest, environment, deadline)?;
                let mutation =
                    run.plan
                        .cleanup
                        .with_lease(&run.consent.cleanup, deadline, |lease| {
                            Ok(lease.erase_identity(command, deadline))
                        })?;
                run.pending = mutation.pending;
                match mutation.result {
                    Ok(output) => match admit_erase_output(&output) {
                        Ok(receipt) => {
                            run.progress.identity = if receipt.identity_and_pairings_removed() {
                                CleanupResult::Removed
                            } else {
                                CleanupResult::Refused
                            };
                            run.identity_receipt = Some(receipt);
                        }
                        Err(error) => {
                            run.progress.identity = CleanupResult::Unknown;
                            run.issues.push(UninstallIssue::Identity(error));
                        }
                    },
                    Err(error) => {
                        run.progress.identity = CleanupResult::Unknown;
                        run.issues.push(UninstallIssue::Identity(error.into()));
                    }
                }
                if run.pending.is_some() || run.progress.identity == CleanupResult::Unknown {
                    return Err(RemovalError::Native(NativeError::OutcomeUnknown).into());
                }
            }
            run.persist(deadline)?;
            run.stage = UninstallStage::Files;
            Ok(())
        })
    }
    pub fn remove_files(&mut self, deadline: &Deadline) -> UninstallResult<()> {
        self.guarded(|run| {
            run.require(UninstallStage::Files)?;
            if run.clean.is_some() {
                run.revalidate_clean(deadline)?;
            }
            run.mark(CleanupStage::FilesObserved);
            run.persist(deadline)?;
            for index in [6, 7, 8, 9, 5, 2, 4, 1, 3, 0] {
                // Recovery LAST, while the executable and genuine original clean evidence still
                // exist. FilesObserved was persisted before dispatch; resume never retries this
                // deletion or reconstructs its lost clean authority.
                if index == 5 && run.recovery_permitted() {
                    run.clean
                        .as_ref()
                        .ok_or(UninstallError::Invalid)?
                        .revalidate(deadline)?;
                    match run
                        .plan
                        .cleanup
                        .with_lease(&run.consent.cleanup, deadline, |lease| {
                            Ok(lease.delete_repair_journal(deadline))
                        })? {
                        Ok(_) => run.repair_intent_removed = true,
                        Err(error) => {
                            run.issues.push(UninstallIssue::RepairIntent(error.into()));
                            return Err(RemovalError::Native(NativeError::OutcomeUnknown).into());
                        }
                    }
                }
                if run.resume.is_some() && run.progress.resources[index] != CleanupResult::Pending {
                    // Unknown is a published per-file intent without a proved outcome, not retry authority.
                    continue;
                }
                let row = &run.plan.cleanup.resources()[index];
                let recovery = index <= 5;
                let eligible = !recovery || run.recovery_permitted();
                let result = if row.observation == ResourceObservation::Absent {
                    CleanupResult::AlreadyAbsent
                } else if !row.owned
                    || row.observation != ResourceObservation::Matching
                    || !eligible
                {
                    CleanupResult::Kept
                } else {
                    if let Some(authority) = &run.clean {
                        authority.revalidate(deadline)?;
                    }
                    // The captured lease itself checks fresh presence, identity and hash inside its worker.
                    // Pending means unattempted. Publish uncertainty before dispatch so a crash
                    // cannot turn an intended deletion back into an unattempted row on resume.
                    run.progress.resources[index] = CleanupResult::Unknown;
                    run.persist(deadline)?;
                    match run
                        .plan
                        .cleanup
                        .with_lease(&run.consent.cleanup, deadline, |lease| {
                            Ok(lease.delete(index, deadline))
                        })? {
                        Ok(true) => CleanupResult::Removed,
                        Ok(false) => CleanupResult::AlreadyAbsent,
                        Err(error) => {
                            run.progress.resources[index] = CleanupResult::Unknown;
                            run.issues
                                .push(UninstallIssue::Resource(index, error.into()));
                            return Err(RemovalError::Native(NativeError::OutcomeUnknown).into());
                        }
                    }
                };
                run.progress.resources[index] = result;
                run.persist(deadline)?;
            }
            run.mark(CleanupStage::Finished);
            run.persist(deadline)?;
            run.stage = UninstallStage::Finished;
            Ok(())
        })
    }
    fn recovery_permitted(&self) -> bool {
        self.clean.is_some()
            && self.plan.cleanup.inventory.repair_journal_valid
            && self.resume.is_none()
            && matches!(
                self.progress.autostart,
                CleanupResult::Removed | CleanupResult::AlreadyAbsent
            )
            && matches!(
                self.progress.stop,
                CleanupResult::Removed | CleanupResult::AlreadyAbsent
            )
            && (self.plan.cleanup.selection().identity == IdentityChoice::Keep
                || self.progress.identity == CleanupResult::Removed)
            && (!self.plan.cleanup.selection().lan_rule
                || self.progress.lan == CleanupResult::AlreadyAbsent)
            && (!self.plan.cleanup.selection().mdns_rule
                || self.progress.mdns == CleanupResult::AlreadyAbsent)
    }
    fn revalidate_clean(&self, deadline: &Deadline) -> UninstallResult<()> {
        self.clean
            .as_ref()
            .ok_or(UninstallError::Invalid)?
            .revalidate(deadline)?;
        if !self.service.observe(deadline).is_ok_and(|facts| {
            !facts.enabled && facts.main_pid == 0 && facts.active_state == "inactive"
        }) {
            return Err(RemovalError::NotClean.into());
        }
        Ok(())
    }
    pub fn report(&self) -> UninstallReport {
        let complete = self.stage == UninstallStage::Finished
            && self.repair_intent_removed
            && self.recovery_permitted()
            && self
                .progress
                .resources
                .iter()
                .all(|r| matches!(r, CleanupResult::Removed | CleanupResult::AlreadyAbsent));
        UninstallReport {
            form: if complete {
                UninstallForm::Complete
            } else {
                UninstallForm::NotClean
            },
            stage: self.stage,
            progress: self.progress.clone(),
            identity_receipt: self.identity_receipt.clone(),
            issues: self.issues.clone(),
            empty_directories_retained: true,
            recovery_retained: !self.recovery_permitted(),
            identity_retained: self.progress.identity != CleanupResult::Removed,
        }
    }
}
