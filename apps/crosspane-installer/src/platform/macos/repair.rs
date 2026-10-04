//! Receipt-bound full reinstall repair. All I/O methods run on a detached installer worker.
//! The existing payload lock, real clean stop, staging and backup verifier remain the authorities.
//! No erase, package, cleanup-authority or identity/config/trust mutation is exposed here.
use super::{
    launch_agent::*,
    native_io::*,
    payload::{ApprovedInventory, MacPayload, PayloadState},
    transport::SelectedAgent,
};
use crate::agent_contract::{
    AgentReply, BootstrapPhase, DecodedReply, HealthSnapshot, KeyStoreProvenance, StartupRecovery,
    StatusAdmission,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

#[path = "repair/record.rs"]
mod record;
pub(crate) use record::removal_hint;
pub use record::{RepairBoundary, RepairRecord};

/// Reopening observes the current install. It never resumes a recorded mutation or clean proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairReassessment {
    CurrentInstallHealthy,
    CurrentInstallHealthyCleanupIncomplete,
    AgentStopped,
    RecoveryRetained,
}

/// Present but stopped/crashed/incomplete agent installs cannot mint a clean-stop proof.
pub const UNINSTALL_THEN_INSTALL: &str = "Uninstall (keeping identity by default), then install.";
pub const USER_DISABLED: &str =
    "Enable the agent manually before repair; repair never re-enables it.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairGuidance {
    EnableManually,
    RestoreOrRemoveFileOrUninstallThenInstall,
    UninstallThenInstall,
    ReDetect,
}
impl RepairGuidance {
    /// Guidance never supplies mutation authority or performs any operation.
    pub fn message(self) -> &'static str {
        match self {
            Self::EnableManually => USER_DISABLED,
            Self::RestoreOrRemoveFileOrUninstallThenInstall => {
                "Restore or remove the file, or uninstall then install."
            }
            Self::UninstallThenInstall => UNINSTALL_THEN_INSTALL,
            Self::ReDetect => "Inspect the retained installation and obtain fresh observations.",
        }
    }
}
/// Interprets a plan refusal; apply/verification errors use their precise progress instead.
pub fn plan_guidance(error: NativeError) -> RepairGuidance {
    match error {
        NativeError::Unsupported => RepairGuidance::EnableManually,
        NativeError::Foreign => RepairGuidance::RestoreOrRemoveFileOrUninstallThenInstall,
        NativeError::Refused => RepairGuidance::UninstallThenInstall,
        _ => RepairGuidance::ReDetect,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairEffect {
    StopTrackedOriginal { pid: u32, instance: u64 },
    ReplaceSignedApp(PathBuf),
    ReplaceCtl(PathBuf),
    RewritePlist(PathBuf),
    BootstrapSelectedAgent,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairActivity {
    pub input_active: bool,
    pub projections: usize,
    pub audio_peers: usize,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairPreview {
    pub effects: Vec<RepairEffect>,
    pub activity: Option<RepairActivity>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairProgress {
    WaitingForCleanStop,
    Published,
    RecoveryRetained,
    HealthVerifiedCleanupIncomplete,
    Verified,
}
pub struct RepairPlan {
    owner: Arc<()>,
    binding: Arc<()>,
    revision: u64,
    operation: u64,
    launch: LaunchPlan,
    current: Option<Vec<u8>>,
    config_revision: Option<String>,
    preview: RepairPreview,
}
pub struct RepairConsent {
    owner: Arc<()>,
    binding: Arc<()>,
    revision: u64,
    operation: u64,
    launch: LaunchConsent,
}
pub struct PendingRepair {
    owner: Arc<()>,
    operation: u64,
    launch: PendingLaunch,
    config_revision: Option<String>,
    progress: RepairProgress,
}
pub struct RepairCompletion {
    pub progress: RepairProgress,
    pub startup: Option<StartupFacts>,
}
pub struct MacRepair {
    io: Arc<MacNativeIo>,
    launch: MacLaunchAgent,
    owner: Arc<()>,
    active: Option<Arc<()>>,
    last: (u64, u64),
    last_reply: (u64, u64),
    attempted: bool,
    inventory: ApprovedInventory,
    record: Mutex<Option<RepairRecord>>,
}
macro_rules! opaque { ($($name:ty),+) => { $(impl std::fmt::Debug for $name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(stringify!($name))
    }
})+ }; }
opaque!(
    MacRepair,
    RepairPlan,
    RepairConsent,
    PendingRepair,
    RepairCompletion
);
impl RepairPlan {
    pub fn operation_id(&self) -> u64 {
        self.operation
    }
    pub fn view_revision(&self) -> u64 {
        self.revision
    }
    pub fn preview(&self) -> &RepairPreview {
        &self.preview
    }
    /// Explicit interruption acknowledgement covers the complete typed reinstall effects.
    pub fn consent(
        &self,
        revision: u64,
        operation: u64,
        acknowledge: bool,
    ) -> NativeResult<RepairConsent> {
        if !acknowledge || revision != self.revision || operation != self.operation {
            return Err(NativeError::Refused);
        }
        Ok(RepairConsent {
            owner: self.owner.clone(),
            binding: self.binding.clone(),
            revision,
            operation,
            launch: self.launch.consent(revision, operation, false, false)?,
        })
    }
}
impl PendingRepair {
    pub fn progress(&self) -> RepairProgress {
        self.progress
    }
    pub fn error(&self) -> Option<NativeError> {
        self.launch.error()
    }
    pub fn retained_prior(&self) -> Option<&std::path::Path> {
        self.launch.retained_prior()
    }
}
fn status(reply: &AgentReply) -> NativeResult<&HealthSnapshot> {
    match reply
        .result
        .as_ref()
        .map_err(|_| NativeError::Unavailable)?
    {
        DecodedReply::Status(StatusAdmission::Supported(health)) => Ok(health),
        _ => Err(NativeError::Unavailable),
    }
}
// Bind activity and all relevant generation/configuration/session facts, excluding progress counters.
fn activity_binding(health: &HealthSnapshot) -> NativeResult<Vec<u8>> {
    let value = health.installer();
    let peers: Vec<_> = value
        .peers
        .iter()
        .map(|peer| {
            (
                peer.node,
                peer.connected,
                peer.link_generation,
                &peer.features,
                &peer.grants_given,
                &peer.last_source_parking,
            )
        })
        .collect();
    serde_json::to_vec(&(
        &value.build,
        &value.instance,
        value.node,
        &value.config_revision,
        &value.epochs,
        &value.gate,
        value.recovery_pending,
        &value.startup_recovery,
        &value.keystore,
        &value.permissions,
        &value.backends,
        value.audio.enabled,
        &value.audio.active_peers,
        peers,
        health.terminal(),
        health.display_layout(),
    ))
    .map_err(|_| NativeError::Invalid)
}
fn progress(pending: &PendingLaunch) -> RepairProgress {
    match pending.phase() {
        LaunchPhase::WaitingForCleanStop => RepairProgress::WaitingForCleanStop,
        LaunchPhase::BootstrapRequested => RepairProgress::Published,
        _ => RepairProgress::RecoveryRetained,
    }
}
impl MacRepair {
    pub fn admit(
        io: Arc<MacNativeIo>,
        inventory: ApprovedInventory,
        approval: Arc<dyn ApprovalProbe>,
        deadline: &Deadline,
    ) -> NativeResult<Self> {
        let launch = MacLaunchAgent::admit(io.clone(), inventory.clone(), approval, deadline)?;
        let record = record::load(&io, &inventory, deadline)?;
        let watermark = record.as_ref().map_or(0, RepairRecord::status_watermark);
        Ok(Self {
            io,
            launch,
            owner: Arc::new(()),
            active: None,
            last: (0, 0),
            last_reply: (watermark, 0),
            attempted: false,
            inventory,
            record: Mutex::new(record),
        })
    }
    /// Only fresh monotonic caller receipts are accepted; watermark survives plan retirement.
    fn advance_reply(
        &mut self,
        selected: &SelectedAgent,
        reply: &AgentReply,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        deadline.check()?;
        let now = self.io.clock().now_ms();
        if selected.io.target().paths() != self.io.target().paths()
            || reply.source != selected.io.target().source()
            || reply.id <= self.last_reply.0
            || reply.observed_at_ms < self.last_reply.1
            || reply.observed_at_ms > now
            || now - reply.observed_at_ms > SUPPORT_LIFETIME_MS
        {
            return Err(NativeError::Foreign);
        }
        status(reply)?;
        self.last_reply = (reply.id, reply.observed_at_ms);
        Ok(())
    }
    pub fn plan(
        &mut self,
        revision: u64,
        operation: u64,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<RepairPlan> {
        if self.attempted || revision <= self.last.0 || operation <= self.last.1 {
            return Err(NativeError::Refused);
        }
        self.active = None;
        self.last = (revision, operation);
        let binding = Arc::new(());
        let mut current_binding = None;
        let mut config_revision = None;
        let mut activity = None;
        if let Some((selected, reply)) = current {
            self.advance_reply(selected, reply, deadline)?;
            let health = status(reply)?;
            current_binding = Some(activity_binding(health)?);
            config_revision = Some(health.installer().config_revision.clone());
            let terminal = health.terminal();
            activity = Some(RepairActivity {
                input_active: terminal.controlling.is_some() || terminal.controlled_by.is_some(),
                projections: terminal.projections.len(),
                audio_peers: health.installer().audio.active_peers.len(),
            });
        }
        let launch = self
            .launch
            .plan_repair(revision, operation, current, deadline)?;
        if launch.interrupts_agent() != current.is_some() {
            return Err(NativeError::Foreign);
        }
        let target = self.io.target();
        let mut effects = Vec::new();
        if let Some((selected, _)) = current {
            effects.push(RepairEffect::StopTrackedOriginal {
                pid: selected.instance.process().pid,
                instance: selected.instance.bootstrap().instance_id,
            });
        }
        effects.extend([
            RepairEffect::ReplaceSignedApp(target.app_path()),
            RepairEffect::ReplaceCtl(target.paths().home.join(".local/bin/crosspanectl")),
            RepairEffect::RewritePlist(
                target
                    .paths()
                    .home
                    .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist"),
            ),
            RepairEffect::BootstrapSelectedAgent,
        ]);
        deadline.check()?;
        self.active = Some(binding.clone());
        Ok(RepairPlan {
            owner: self.owner.clone(),
            binding,
            revision,
            operation,
            launch,
            current: current_binding,
            config_revision,
            preview: RepairPreview { effects, activity },
        })
    }
    pub fn apply(
        &mut self,
        plan: RepairPlan,
        consent: RepairConsent,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<PendingRepair> {
        // All stale/foreign events are pure refusals: zero probe, command or filesystem work.
        if self.attempted
            || !Arc::ptr_eq(&self.owner, &plan.owner)
            || !Arc::ptr_eq(&self.owner, &consent.owner)
            || !Arc::ptr_eq(&plan.binding, &consent.binding)
            || self
                .active
                .as_ref()
                .is_none_or(|p| !Arc::ptr_eq(p, &plan.binding))
            || (plan.revision, plan.operation) != (consent.revision, consent.operation)
            || (plan.revision, plan.operation) != self.last
        {
            return Err(NativeError::Refused);
        }
        self.active = None;
        self.attempted = true;
        match (current, &plan.current) {
            (Some((selected, reply)), Some(expected)) => {
                self.advance_reply(selected, reply, deadline)?;
                if activity_binding(status(reply)?)? != *expected {
                    return Err(NativeError::Foreign);
                }
            }
            (None, None) => deadline.check()?,
            _ => return Err(NativeError::Foreign),
        }
        self.launch
            .revalidate_repair(&plan.launch, current, deadline)?;
        let saved = record::begin(
            &self.io,
            &self.inventory,
            &plan,
            self.last_reply.0,
            deadline,
        )?;
        *self.record.lock().map_err(|_| NativeError::Busy)? = Some(saved);
        // An execute error is before its dispatch phase: installed payload/plist stay unchanged.
        // A returned pending Unknown is retained and never re-dispatched by this coordinator.
        let launch = match self.launch.execute(plan.launch, consent.launch, deadline) {
            Ok(launch) => launch,
            Err(error) => {
                // The frozen executor returns an Err only before its dispatch phase. A record that
                // can't be retired stays for a later reassessment; the executor's error is kept.
                let _ = self.retire_record(&self.io, deadline);
                return Err(error);
            }
        };
        let phase = progress(&launch);
        // After dispatch the boundary is only a hint: a failed update never drops the genuine
        // pending repair (the before-stop record and reserved Status ids are already durable).
        let _ = self.update_record(
            &self.io,
            match phase {
                RepairProgress::WaitingForCleanStop => RepairBoundary::Stopped,
                RepairProgress::Published => RepairBoundary::Applied,
                _ => RepairBoundary::Applying,
            },
            self.last_reply.0,
            deadline,
        );
        Ok(PendingRepair {
            owner: self.owner.clone(),
            operation: plan.operation,
            launch,
            config_revision: plan.config_revision,
            progress: phase,
        })
    }
    /// Recheck the actual tracked exit, without ever issuing bootout a second time.
    pub fn continue_clean_stop(
        &self,
        pending: &mut PendingRepair,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        self.check_pending(pending)?;
        if pending.progress != RepairProgress::WaitingForCleanStop {
            return Err(NativeError::Refused);
        }
        // Boundary hints are best effort here: the genuine pending token, not the record, is
        // the continuation authority, and a fresh window never reads the boundary as one.
        let _ = self.update_record(
            &self.io,
            RepairBoundary::Applying,
            self.last_reply.0,
            deadline,
        );
        if let Err(error) = self.launch.resume_clean_stop(&mut pending.launch, deadline) {
            pending.progress = RepairProgress::RecoveryRetained;
            return Err(error);
        }
        pending.progress = progress(&pending.launch);
        let _ = self.update_record(
            &self.io,
            if pending.progress == RepairProgress::Published {
                RepairBoundary::Applied
            } else {
                RepairBoundary::Stopped
            },
            self.last_reply.0,
            deadline,
        );
        Ok(())
    }
    fn check_pending(&self, pending: &PendingRepair) -> NativeResult<()> {
        if !self.attempted
            || !Arc::ptr_eq(&self.owner, &pending.owner)
            || pending.operation != self.last.1
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    pub fn expect_health(&self, pending: &mut PendingRepair, id: u64) -> NativeResult<()> {
        self.check_pending(pending)?;
        if pending.progress != RepairProgress::Published || id <= self.last_reply.0 {
            return Err(NativeError::Refused);
        }
        pending.launch.expect_health(id)
    }
    pub fn observe(
        &mut self,
        pending: &mut PendingRepair,
        selected: &SelectedAgent,
        reply: AgentReply,
        deadline: &Deadline,
    ) -> NativeResult<RepairCompletion> {
        self.check_pending(pending)?;
        if pending.progress != RepairProgress::Published {
            return Err(NativeError::Refused);
        }
        self.advance_reply(selected, &reply, deadline)?;
        // The adapter reserved this id durably before sending; the boundary is only a hint.
        let _ = self.update_record(&selected.io, RepairBoundary::HealthWait, reply.id, deadline);
        let (startup, incomplete) = self.launch.observe_repair(
            &mut pending.launch,
            selected,
            reply,
            pending.config_revision.as_deref(),
            deadline,
        )?;
        pending.progress = if incomplete {
            RepairProgress::HealthVerifiedCleanupIncomplete
        } else {
            RepairProgress::Verified
        };
        if self.retire_record(&selected.io, deadline).is_err() {
            pending.progress = RepairProgress::HealthVerifiedCleanupIncomplete;
        }
        Ok(RepairCompletion {
            progress: pending.progress,
            startup,
        })
    }

    /// Strict read only. Saved process information and receipt fingerprints grant no authority.
    pub fn saved_record(
        io: &MacNativeIo,
        inventory: &ApprovedInventory,
        deadline: &Deadline,
    ) -> NativeResult<Option<RepairRecord>> {
        record::load(io, inventory, deadline)
    }

    /// Reserve a Status id durably before the adapter sends it, including unsuccessful reads.
    pub fn reserve_status(
        io: &MacNativeIo,
        inventory: &ApprovedInventory,
        id: u64,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        if let Some(record) = record::load(io, inventory, deadline)? {
            if id <= record.status_watermark() {
                return Err(NativeError::IdExhausted);
            }
            let step = if record.boundary() == RepairBoundary::Applied {
                RepairBoundary::HealthWait
            } else {
                record.boundary()
            };
            record::update(io, inventory, &record, step, id, deadline)?;
        }
        Ok(())
    }

    fn update_record(
        &self,
        io: &MacNativeIo,
        step: RepairBoundary,
        watermark: u64,
        deadline: &Deadline,
    ) -> NativeResult<()> {
        let mut held = self.record.lock().map_err(|_| NativeError::Busy)?;
        let current = record::load(io, &self.inventory, deadline)?.ok_or(NativeError::Foreign)?;
        // The adapter may have reserved a newer Status while this coordinator was waiting.
        if held.as_ref().is_none_or(|old| {
            old.operation_id() != current.operation_id()
                || old.data_binding() != current.data_binding()
        }) {
            return Err(NativeError::Foreign);
        }
        let next = record::update(
            io,
            &self.inventory,
            &current,
            step,
            watermark.max(current.status_watermark()),
            deadline,
        )?;
        *held = Some(next);
        Ok(())
    }

    fn retire_record(&self, io: &MacNativeIo, deadline: &Deadline) -> NativeResult<()> {
        let mut held = self.record.lock().map_err(|_| NativeError::Busy)?;
        let current = record::load(io, &self.inventory, deadline)?.ok_or(NativeError::Foreign)?;
        if held
            .as_ref()
            .is_none_or(|old| old.data_binding() != current.data_binding())
        {
            return Err(NativeError::Foreign);
        }
        record::retire(io, &self.inventory, &current, deadline)?;
        *held = None;
        Ok(())
    }

    /// Record the health-wait boundary hint. A new window reassesses; it never replays a step.
    pub fn health_wait(&self, pending: &PendingRepair, deadline: &Deadline) -> NativeResult<()> {
        self.check_pending(pending)?;
        if pending.progress != RepairProgress::Published {
            return Err(NativeError::Refused);
        }
        self.update_record(
            &self.io,
            RepairBoundary::HealthWait,
            self.last_reply.0,
            deadline,
        )
    }

    /// Re-admitted observations determine the result, independently of the recorded boundary.
    /// No stop, publication, bootstrap, backup deletion or reconstructed pending token occurs.
    pub fn resume(
        &mut self,
        record: RepairRecord,
        current: Option<(&SelectedAgent, &AgentReply)>,
        deadline: &Deadline,
    ) -> NativeResult<RepairReassessment> {
        if self.attempted || self.active.is_some() {
            return Err(NativeError::Refused);
        }
        if self
            .record
            .lock()
            .map_err(|_| NativeError::Busy)?
            .as_ref()
            .is_none_or(|initial| initial.data_binding() != record.data_binding())
        {
            return Err(NativeError::Foreign);
        }
        record.check_current(&self.io, &self.inventory, deadline)?;
        let payload = MacPayload::admit(self.io.clone(), self.inventory.clone(), deadline)?;
        let verified = payload
            .plan(1, 1, None, deadline)
            .is_ok_and(|plan| plan.state() == PayloadState::Matching);
        if !verified {
            return Ok(RepairReassessment::RecoveryRetained);
        }
        if let Some((selected, reply)) = current {
            self.advance_reply(selected, reply, deadline)?;
            let health = status(reply)?.installer();
            let mut features = health.build.features.clone();
            features.sort();
            let mut expected = self.inventory.features.clone();
            expected.sort();
            if selected.instance.bootstrap().phase != BootstrapPhase::Ready
                || health.startup_recovery == StartupRecovery::Failed
                || health.recovery_pending != 0
                || health.keystore != KeyStoreProvenance::OsStore
                || health.build.version != self.inventory.product_version
                || features != expected
            {
                return Ok(RepairReassessment::RecoveryRetained);
            }
            // Read-only detection, not `plan_repair`: an interrupted repair's launch journal is
            // never `Observed`, so only the owned receipt, the exact plist and the admitted
            // running job are judged here. Nothing is planned for execution.
            let owned = self
                .launch
                .plan(1, 1, current, deadline)
                .is_ok_and(|plan| plan.state() == LaunchState::Owned);
            if !owned {
                return Ok(RepairReassessment::RecoveryRetained);
            }
            // Payload `Matching` already proves no previous app/ctl copy is left (an unfinished
            // payload recovery is never `Matching`); the prior plist is kept by design.
            *self.record.lock().map_err(|_| NativeError::Busy)? = Some(record);
            return Ok(if self.retire_record(&selected.io, deadline).is_ok() {
                RepairReassessment::CurrentInstallHealthy
            } else {
                RepairReassessment::CurrentInstallHealthyCleanupIncomplete
            });
        }
        // A successfully read launch snapshot with no supplied agent is necessarily Job::Absent:
        // the frozen planner rejects Running without a current admission and marks Unknown.
        let stopped = self.launch.plan(1, 1, None, deadline).is_ok_and(|plan| {
            matches!(
                plan.state(),
                LaunchState::Owned | LaunchState::Absent | LaunchState::AdoptionRequired
            ) && !plan.interrupts_agent()
        });
        let plist = self
            .io
            .target()
            .paths()
            .home
            .join("Library/LaunchAgents/io.frostdev.crosspane.agent.plist");
        // An absent, unreadable or different plist is unverifiable, never an assessment error.
        let rendered = render_plist(self.io.target()).ok();
        if stopped
            && rendered.is_some()
            && self.io.read(&plist, 64 * 1024, false, deadline).ok() == rendered
        {
            Ok(RepairReassessment::AgentStopped)
        } else {
            Ok(RepairReassessment::RecoveryRetained)
        }
    }
}
