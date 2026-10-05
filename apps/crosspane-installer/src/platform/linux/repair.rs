//! Compatible selected-user repair. Support admits creation; recovery receipts never do.
//! Each native stage takes its ordinary install.lock. No cleanup capability is consumed here.
mod executor;
mod resume;
mod retire;
pub(crate) use retire::validate_removal_journal_for_cleanup;
pub use retire::{RetireUnapplied, RetirementOutcome};

use super::{
    firewall::FirewallError,
    native_io::{Deadline, ExitReader, LinuxNativeIo, NativeError, SupportProof},
    payload::{Interruption, Package, PayloadError, PayloadInstaller},
    removal::{
        Inventory, InventoryFacts, PlanKind, RemovalConsent, RemovalError, RemovalPlan,
        RemovalPlanner, RemovalSelection,
    },
    service::{LinuxService, ServiceError},
};
use crate::agent_contract::{AgentReply, DecodedReply, KeyStoreProvenance, StatusAdmission};
use crosspane_installer_core::{OperationId, ResourceOwnership, ResourceReceipt};
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};

type Result<T> = std::result::Result<T, RepairError>;
type PayloadHook = Arc<dyn Fn(Interruption) -> std::result::Result<(), PayloadError> + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompatibilityIssue {
    /// Foreign/adopted files may be package-managed or edited. No implicit migration/adoption.
    MixedOwnership,
    /// An admitted file-key identity requires its own migration; repair never rotates it.
    FallbackIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RepairError {
    #[error(transparent)]
    Native(#[from] NativeError),
    #[error(transparent)]
    Payload(#[from] PayloadError),
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    Firewall(#[from] FirewallError),
    #[error(transparent)]
    Admission(#[from] RemovalError),
    #[error("manual compatible-install review required: {0:?}")]
    Tier2(CompatibilityIssue),
    #[error("the repair stage needs a fresh observation")]
    NotReady,
    #[error("unfinished or changed repair intent; inspect retained recovery material")]
    RecoveryPending,
}

/// A request's reply ID is correlation, never positive evidence. Native admission remains required.
pub struct RepairInput<'a> {
    pub proof: &'a SupportProof,
    pub package: &'a Package,
    pub service: &'a LinuxService,
    pub reply: Option<&'a AgentReply>,
    pub expected_reply_id: u64,
    pub now_ms: u64,
    pub deadline: &'a Deadline,
}
/// Firewall confirmation is independent of repair interruption consent.
pub struct FirewallRepair<'a> {
    pub proof: &'a SupportProof,
    pub firewall: &'a mut super::firewall::LinuxFirewall,
    pub manager: super::firewall::ManagerSelection,
    pub plan: super::firewall::FirewallPlan,
    pub consent: super::firewall::FirewallConsent,
    pub store: &'a mut super::firewall::receipts::DurableIntentStore,
    pub deadline: &'a Deadline,
}
impl fmt::Debug for RepairInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepairInput { .. }")
    }
}
impl fmt::Debug for FirewallRepair<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FirewallRepair { .. }")
    }
}
impl RepairInput<'_> {
    fn correlated_reply(&self) -> Option<&AgentReply> {
        self.reply
            .filter(|r| self.expected_reply_id != 0 && r.id == self.expected_reply_id)
    }
}

pub struct RepairInventory {
    inventory: Inventory,
    compatibility: Option<CompatibilityIssue>,
}
impl fmt::Debug for RepairInventory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepairInventory { .. }")
    }
}
impl RepairInventory {
    pub fn facts(&self) -> &InventoryFacts {
        self.inventory.facts()
    }
    pub fn compatibility_issue(&self) -> Option<CompatibilityIssue> {
        self.compatibility
    }
}

pub struct RepairPlan {
    plan: RemovalPlan,
}
impl fmt::Debug for RepairPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepairPlan { .. }")
    }
}
pub struct RepairConsent {
    consent: RemovalConsent,
}
impl fmt::Debug for RepairConsent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RepairConsent { .. }")
    }
}
impl RepairPlan {
    pub fn facts(&self) -> &InventoryFacts {
        self.plan.facts()
    }
    pub fn revision(&self) -> u64 {
        self.plan.revision()
    }
    pub fn operation(&self) -> OperationId {
        self.plan.operation()
    }
    pub fn delta(&self) -> Vec<&ResourceReceipt> {
        self.plan.repair_delta()
    }
    pub fn preview(&self) -> String {
        format!(
            "{}\nActive input, projections and audio can be interrupted; unknown activity is unverified.\nOnly the compatible owned delta is replaced. Configuration, dependencies, identity, pairings and grants are kept.\nFirewall changes require a separate exact rule preview and consent.\nBackups remain until a new instance completes startup recovery and actual health verification.",
            self.plan.preview()
        )
    }
    pub fn consent(
        &self,
        revision: u64,
        operation: OperationId,
        accept_interruption: bool,
    ) -> Result<RepairConsent> {
        Ok(RepairConsent {
            consent: self
                .plan
                .consent(revision, operation, accept_interruption)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    /// No mutation and no claim that health or readiness is verified.
    NoDelta,
    AwaitingCleanExit,
    AwaitingAgent,
    Verified,
    /// Fresh new-instance health is verified; some disposable backup/journal cleanup remains.
    HealthVerifiedCleanupIncomplete,
    /// Verify may have persisted health, but a bounded typed re-observation could not prove it.
    /// Resume is required; this result never reconstructs health from private journal fields.
    OutcomeUnknownAfterVerify,
    /// Mutation was attempted. All payload backups/intents and recovery tools remain available.
    RecoveryRetained,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStage {
    Recorded,
    StopPending,
    Stopped,
    PayloadPending,
    PayloadApplied,
    ReloadPending,
    StartPending,
    AwaitingAgent,
    Verified,
}
#[derive(Debug)]
pub struct RepairResult {
    pub operation: OperationId,
    pub stage: RepairStage,
    pub outcome: RepairOutcome,
    pub resources: Vec<ResourceReceipt>,
    pub activity_retired: bool,
    /// Fixed recovery paths still present after observation. Unknown presence is disclosed too.
    pub recovery_material: Vec<RecoveryMaterial>,
    pub error: Option<RepairError>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryMaterial {
    pub path: std::path::PathBuf,
    pub presence: RecoveryPresence,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPresence {
    Present,
    Unknown,
}

pub struct LinuxRepair {
    io: Arc<LinuxNativeIo>,
    installer: PayloadInstaller,
    planner: RemovalPlanner,
    run: Option<executor::Run>,
}
impl fmt::Debug for LinuxRepair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinuxRepair { .. }")
    }
}
impl LinuxRepair {
    pub fn new(io: Arc<LinuxNativeIo>) -> Result<Self> {
        Ok(Self {
            installer: PayloadInstaller::new(io.clone())?,
            planner: RemovalPlanner::new(io.clone()),
            io,
            run: None,
        })
    }
    /// The existing original-process fake seam is admitted only for explicit scratch targets.
    pub fn scratch_exit_reader(&mut self, reader: Arc<dyn ExitReader>) -> Result<()> {
        self.planner.scratch_exit_reader(reader)?;
        Ok(())
    }
    pub fn scratch_payload_interrupt(&mut self, at: Option<Interruption>) -> Result<()> {
        self.installer.scratch_interrupt(at)?;
        Ok(())
    }
    pub fn scratch_payload_hook(&mut self, hook: Option<PayloadHook>) -> Result<()> {
        self.installer.scratch_hook(hook)?;
        Ok(())
    }
    pub fn inventory(&self, input: &RepairInput<'_>) -> Result<RepairInventory> {
        let inventory = self.planner.inventory(
            input.proof,
            input.package,
            input.service,
            input.correlated_reply(),
            input.now_ms,
            input.deadline,
        )?;
        let compatibility = if inventory.facts().resources.as_ref().is_ok_and(|rows| {
            rows.iter()
                .any(|r| r.ownership != ResourceOwnership::Created)
        }) {
            Some(CompatibilityIssue::MixedOwnership)
        } else if inventory.facts().activity.is_some()
            && input.correlated_reply().is_some_and(|r| {
                matches!(&r.result,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(h)))
                    if h.installer().keystore == KeyStoreProvenance::File)
            })
        {
            Some(CompatibilityIssue::FallbackIdentity)
        } else {
            None
        };
        Ok(RepairInventory {
            inventory,
            compatibility,
        })
    }
    pub fn plan(
        &mut self,
        inventory: RepairInventory,
        revision: u64,
        operation: OperationId,
    ) -> Result<RepairPlan> {
        if self.run.is_some() {
            return Err(RepairError::RecoveryPending);
        }
        if let Some(issue) = inventory.compatibility {
            return Err(RepairError::Tier2(issue));
        }
        inventory.facts().resources.as_ref().map_err(Clone::clone)?;
        inventory.facts().service.as_ref().map_err(|e| *e)?;
        Ok(RepairPlan {
            plan: self.planner.plan(
                inventory.inventory,
                revision,
                operation,
                PlanKind::Repair,
                RemovalSelection::default(),
            )?,
        })
    }

    /// A separate exact 4.9 rule plan/consent and its durable store are required. This method
    /// neither builds privileged argv nor retries an unknown outcome. Firewall traffic proof
    /// remains 4.9's current-observation verifier; payload health never substitutes for it.
    pub fn apply_firewall(
        &self,
        request: FirewallRepair<'_>,
    ) -> Result<super::firewall::FirewallResult> {
        request.deadline.check()?;
        request.proof.check(&self.io)?;
        Ok(request.firewall.apply(
            request.proof,
            request.manager,
            request.plan,
            request.consent,
            request.store,
            request.deadline,
        )?)
    }
}
