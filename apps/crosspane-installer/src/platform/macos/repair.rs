//! Receipt-bound full reinstall repair. All I/O methods run on a detached installer worker.
//! The existing payload lock, real clean stop, staging and backup verifier remain the authorities.
//! No erase, package, cleanup-authority or identity/config/trust mutation is exposed here.
use super::{launch_agent::*, native_io::*, payload::ApprovedInventory, transport::SelectedAgent};
use crate::agent_contract::{AgentReply, DecodedReply, HealthSnapshot, StatusAdmission};
use std::{path::PathBuf, sync::Arc};

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
        let launch = MacLaunchAgent::admit(io.clone(), inventory, approval, deadline)?;
        Ok(Self {
            io,
            launch,
            owner: Arc::new(()),
            active: None,
            last: (0, 0),
            last_reply: (0, 0),
            attempted: false,
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
        // An execute error is before its dispatch phase: installed payload/plist stay unchanged.
        // A returned pending Unknown is retained and never re-dispatched by this coordinator.
        let launch = self.launch.execute(plan.launch, consent.launch, deadline)?;
        let phase = progress(&launch);
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
        if let Err(error) = self.launch.resume_clean_stop(&mut pending.launch, deadline) {
            pending.progress = RepairProgress::RecoveryRetained;
            return Err(error);
        }
        pending.progress = progress(&pending.launch);
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
        Ok(RepairCompletion {
            progress: pending.progress,
            startup,
        })
    }
}
