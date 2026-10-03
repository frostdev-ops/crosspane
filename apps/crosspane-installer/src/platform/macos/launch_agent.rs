//! Inert selected-GUI LaunchAgent operations, called only from a detached worker.
//! Startup observations are facts, not Core readiness. LaunchAgent-only flows retain every
//! prior-state copy; C1's real verifier alone releases its payload backups. Native mode/ACL
//! and same-UID final-window limits are those of the frozen Mac foundation.
use super::{native_io::*, payload::*, transport::SelectedAgent};
use crate::agent_contract::{AgentReply, DecodedReply, StatusAdmission, parse_bootstrap};
use crosspane_installer_core::{
    InstallReceipt, MutationOutcome, OperationId, ResourceObservation, ResourceOwnership,
    ResourceReceipt, StepId,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

const TEMPLATE: &str =
    include_str!("../../../../../packaging/macos/installer/io.frostdev.crosspane.agent.plist.in");
const LIMIT: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    Allowed,
    Denied,
    Unknown,
}
/// Only reliably observed public approval is Allowed; no private database or default success.
pub trait ApprovalProbe: Send + Sync {
    fn observe(&self, target: &MacTarget, deadline: &Deadline) -> NativeResult<Approval>;
}
#[derive(Debug)]
pub struct UnobservableApproval;
impl ApprovalProbe for UnobservableApproval {
    fn observe(&self, _: &MacTarget, deadline: &Deadline) -> NativeResult<Approval> {
        deadline.check()?;
        Ok(Approval::Unknown)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disabled {
    No,
    Yes,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchState {
    Absent,
    Owned,
    AdoptionRequired,
    Conflict,
    UserDisabled,
    Unobservable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LaunchPhase {
    Intent,
    WaitingForCleanStop,
    Published,
    BootstrapRequested,
    Observed,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginEvidence {
    SameSession,
    DifferentInteractiveSession,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    Absent,
    Running(u32),
    Unknown,
}
#[derive(Clone, PartialEq, Eq)]
struct Snapshot {
    identity: Option<FileIdentity>,
    bytes: Vec<u8>,
    job: Job,
    disabled: Disabled,
}
pub struct LaunchPlan {
    owner: Arc<()>,
    binding: Arc<()>,
    revision: u64,
    operation: u64,
    state: LaunchState,
    snapshot: Snapshot,
    original: Option<Arc<OriginalAgent>>,
    selected: Option<SelectedAgent>,
    installed_main: Option<SignatureProof>,
    payload: Option<PayloadPlan>,
    matching: bool,
    session: String,
    baseline: Option<u64>,
}
pub struct LaunchConsent {
    binding: Arc<()>,
    payload: Option<PayloadConsent>,
}
impl LaunchPlan {
    /// Preview must explain that the exact selected agent and its active sessions will stop.
    pub fn interrupts_agent(&self) -> bool {
        self.original.is_some()
    }
    pub fn state(&self) -> LaunchState {
        self.state
    }
    pub fn consent(
        &self,
        revision: u64,
        operation: u64,
        adopt_plist: bool,
        adopt_payload: bool,
    ) -> NativeResult<LaunchConsent> {
        if self.revision != revision
            || self.operation != operation
            || matches!(
                self.state,
                LaunchState::Conflict | LaunchState::UserDisabled | LaunchState::Unobservable
            )
            || (self.state == LaunchState::AdoptionRequired && !adopt_plist)
        {
            return Err(NativeError::Refused);
        }
        Ok(LaunchConsent {
            binding: self.binding.clone(),
            payload: Some(self.payload.as_ref().ok_or(NativeError::Invalid)?.consent(
                revision,
                operation,
                adopt_payload,
            )?),
        })
    }
}
#[derive(Serialize, Deserialize)]
struct Record {
    phase: LaunchPhase,
    #[serde(default)]
    stop_attempted: bool,
    session: String,
    baseline: Option<u64>,
    prior: Option<String>,
    receipt: InstallReceipt,
}
pub struct PendingLaunch {
    plan: LaunchPlan,
    consent: LaunchConsent,
    phase: LaunchPhase,
    error: Option<NativeError>,
    payload: Option<PendingPayload>,
    requested: bool,
    stop_attempted: bool,
    requested_at: u64,
    admission_refused: bool,
    health_call: Option<u64>,
    last_health: u64,
    prior: Option<PathBuf>,
}
impl PendingLaunch {
    pub fn phase(&self) -> LaunchPhase {
        self.phase
    }
    pub fn error(&self) -> Option<NativeError> {
        self.error
    }
    pub fn retained_prior(&self) -> Option<&Path> {
        self.prior.as_deref()
    }
    pub fn expect_health(&mut self, id: u64) -> NativeResult<()> {
        if !self.requested || id == 0 || id <= self.last_health {
            return Err(NativeError::Invalid);
        }
        if let Some(payload) = &mut self.payload {
            payload.expect_health(id)?;
        }
        self.health_call = Some(id);
        self.last_health = id;
        Ok(())
    }
}
/// Denied/Unknown approval needs manual guidance. Different-session evidence is reported
/// separately; neither this structure nor a bootstrap exit code asserts overall readiness.
pub struct StartupFacts {
    pub reply: AgentReply,
    pub approval: Approval,
    pub disabled: Disabled,
    pub login: LoginEvidence,
    pub payload_verified: Option<VerifiedPayload>,
    pub retained_prior: Option<PathBuf>,
}
pub struct MacLaunchAgent {
    io: Arc<MacNativeIo>,
    payload: MacPayload,
    inventory: ApprovedInventory,
    requirement: SigningRequirement,
    main: SignatureProof,
    support: SupportProof,
    approval: Arc<dyn ApprovalProbe>,
    owner: Arc<()>,
    last: (u64, u64),
    version: String,
    xml: Vec<u8>,
}
macro_rules! opaque { ($($t:ty),+) => { $(impl std::fmt::Debug for $t {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(stringify!($t)) }
})+ }; }
opaque!(
    LaunchPlan,
    LaunchConsent,
    PendingLaunch,
    StartupFacts,
    MacLaunchAgent
);
#[path = "launch_agent/detection.rs"]
mod detection;
#[path = "launch_agent/executor.rs"]
mod executor;
#[path = "launch_agent/intent.rs"]
mod intent;
use detection::checked_reply;
use intent::digest;
pub use intent::render_plist;
