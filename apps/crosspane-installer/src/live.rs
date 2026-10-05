//! The shared live installer controller: one OS-free binding of core `Flow`, the agent client, the
//! nine-role tutorial sequencer and the fixture boundary to the presentation shell.
//!
//! Platforms supply only the [`Platform`] port: their native steps, one non-blocking job channel,
//! the selected agent port and the owned fixture launcher. Core alone decides validity and
//! milestones; every native result is correlated to the outstanding core job before it is reduced.

mod controller;
mod graph;
mod ledger;
mod practice;
mod present;
mod shared;

use std::sync::{Arc, Mutex};

use crosspane_installer_core::{
    ApplyOutcome, AttemptId, JobIntent, ObservationSource, OperationId, StepId, WaitKind,
};

use crate::agent_contract::{AgentPlatform, AgentPort, AgentReply};
use crate::fixture::{FixtureCall, FixtureError, FixtureId, FixtureReceipt};
use crate::tutorial_flow::TutorialSourcePolicy;
use crate::view::{ProgressGroup, ScreenId, ToggleRole};

pub use controller::LiveController;
pub use graph::{ROLES as PRACTICE_ROLES, steps};

/// One monotonic millisecond clock shared by the controller, platform workers, the agent
/// transport and the fixture port.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// One platform-owned step in the readiness graph. Ids 10–59 are reserved for platforms.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeStep {
    pub id: StepId,
    pub prerequisites: Vec<StepId>,
    pub required_for_installed: bool,
    pub required_for_ready: bool,
    pub screen: ScreenId,
    pub group: ProgressGroup,
    pub label: String,
    /// The consent button shown with this step's plan preview.
    pub action_label: String,
    /// Detection, planning and verification need a Status reply issued after the job began. The
    /// controller defers those stages until one arrives and hands it to the platform with the job.
    pub uses_status: bool,
    /// Live traffic from the other computer is part of this step's proof (a firewall rule that
    /// only matters once a peer reaches us). Such a step never blocks Next once it is only
    /// waiting, and it is re-checked after a peer connects.
    pub settles_with_peer: bool,
    /// When set, Apply is this agent request, issued by the controller instead of the platform.
    pub agent_apply: Option<AgentApply>,
}

/// An action the controller itself performs through the agent port when a native step's Apply is
/// consented to. The agent's acknowledgement is only that the request was taken; the step is
/// verified later from a Status, never from the acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentApply {
    /// Ask macOS for every missing required permission.
    AskPermissions,
    /// Restart the agent so a new instance starts.
    Restart,
}

/// Static platform facts read once when the controller is built.
#[derive(Clone, Debug, PartialEq)]
pub struct PlatformDescription {
    pub platform: AgentPlatform,
    /// This machine's name, shown to the person and on the practice fixture.
    pub machine_label: String,
    pub steps: Vec<NativeStep>,
    /// Native steps the shared pairing step depends on.
    pub connect_after: Vec<StepId>,
    /// Native steps every practice step depends on.
    pub practice_after: Vec<StepId>,
    /// Whether the shared hiding-choice step applies (macOS, D7).
    pub hiding_choice: bool,
    /// The source policy used when no hiding choice applies or none is verified.
    pub source_policy: TutorialSourcePolicy,
    /// The output the practice fixture plays its tone into, named per selected peer: every
    /// `{peer}` is replaced with that peer's id (for example `crosspane.{peer}.speaker`).
    pub speakers_device: Option<String>,
    /// A hint that an earlier setup stopped partway. It is shown on the welcome screen only: it
    /// is never evidence, and every step is detected again from the real system.
    pub resume_note: Option<String>,
}

/// Consent for exactly one core operation, given on exactly one view revision to the preview of
/// the plan operation that immediately preceded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Consent {
    pub plan: OperationId,
    pub operation: OperationId,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MaintenanceId(pub u64);

/// Uninstall and repair sit outside the readiness graph.
#[derive(Clone, Debug, PartialEq)]
pub enum MaintenanceRequest {
    Inspect {
        id: MaintenanceId,
    },
    PlanUninstall {
        id: MaintenanceId,
        choices: Vec<(u16, bool)>,
        /// The latest Status, if it is still fresh, for adapters that plan against a running agent.
        status: Option<StatusEvidence>,
    },
    ConfirmUninstall {
        id: MaintenanceId,
        revision: u64,
        status: Option<StatusEvidence>,
    },
    ConfirmFollowUp {
        id: MaintenanceId,
        follow_up: u16,
        revision: u64,
    },
    DeclineFollowUp {
        id: MaintenanceId,
        follow_up: u16,
    },
    /// Plan a compatible repair. `status` is a Status issued after the person asked for the
    /// review, so the plan names what is running right now.
    PlanRepair {
        id: MaintenanceId,
        status: Option<StatusEvidence>,
    },
    /// Confirm the repair preview numbered `plan` (the number its `RepairPlanned` report carried),
    /// on view `revision`. The platform observes everything again with this `status` and starts
    /// only if it still previews exactly what was shown; a stale `plan` is refused.
    ConfirmRepair {
        id: MaintenanceId,
        plan: u64,
        revision: u64,
        status: Option<StatusEvidence>,
    },
    /// One more look at a repair that has started the new agent and waits for it to report
    /// healthy. `status` is a Status issued after the previous attempt.
    VerifyRepair {
        id: MaintenanceId,
        status: Option<StatusEvidence>,
    },
    /// Discard the record of an earlier repair that stopped before replacing any file. The
    /// platform re-proves that fact and the install's health with this fresh Status; one shot.
    DiscardRepair {
        id: MaintenanceId,
        status: Option<StatusEvidence>,
    },
    /// Resume a repair that was interrupted or whose outcome is unknown: it re-checks what is
    /// really there and never replays an uncertain change.
    ResumeRepair {
        id: MaintenanceId,
        status: Option<StatusEvidence>,
    },
}

/// A complete Status reply, delivered unchanged: its call id, original receipt time and source
/// are what the native adapters correlate against. It is evidence only for the job it rides on.
#[derive(Clone, PartialEq)]
pub struct StatusEvidence(pub AgentReply);

impl std::fmt::Debug for StatusEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StatusEvidence(..)")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeJob {
    Step {
        job: JobIntent,
        consent: Option<Consent>,
        /// Present only for the Detect, Plan and Verify stages of a `uses_status` step.
        status: Option<StatusEvidence>,
    },
    Maintenance(MaintenanceRequest),
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeOutcome {
    Detected {
        needs_action: bool,
    },
    Planned {
        preview: String,
    },
    Applied(ApplyOutcome),
    /// Native verification never carries a counter binding.
    Verified {
        source: ObservationSource,
        observed_at_ms: u64,
    },
    Waiting(WaitKind),
    Failed,
    Unsupported,
    /// Progress text only; it changes no state.
    Progress,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StepReport {
    pub job: JobIntent,
    pub outcome: NativeOutcome,
    /// Redacted, person-readable detail.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Availability {
    Available,
    NotAvailableYet(String),
    Unavailable(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemovalChoice {
    pub id: u16,
    pub role: ToggleRole,
    pub label: String,
    pub checked: bool,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceOutcome {
    Removed,
    Partial,
    Refused,
    Failed,
}

/// How a repair that went past planning ended. Each variant says what is known, no more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Only an unapplied repair record was retired; no installed file was replaced.
    Retired,
    /// The new instance reported healthy and the repair is complete.
    Verified,
    /// The new instance reported healthy, but some backup or record cleanup is still left.
    HealthVerifiedCleanupIncomplete,
    /// What the last step did can't be proved. Nothing was retried; resume is required.
    OutcomeUnknown,
    /// A change was attempted and could not be completed. Backups and recovery files are kept.
    RecoveryRetained,
    /// Reopening checked current verified receipts and fresh health, without attributing them
    /// to a new instance or reconstructing the interrupted repair's individual effects.
    CheckedAfterEarlierRepair,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MaintenanceReport {
    Inspected {
        id: MaintenanceId,
        uninstall: Availability,
        repair: Availability,
        choices: Vec<RemovalChoice>,
    },
    Planned {
        id: MaintenanceId,
        preview: String,
    },
    Progress {
        id: MaintenanceId,
        detail: String,
    },
    /// A separately consented follow-up, such as removing a global firewall rule.
    FollowUp {
        id: MaintenanceId,
        follow_up: u16,
        label: String,
        preview: String,
    },
    Finished {
        id: MaintenanceId,
        outcome: MaintenanceOutcome,
        lines: Vec<String>,
    },
    Refused {
        id: MaintenanceId,
        reason: String,
    },
    /// The answer to `PlanRepair`: what a repair would replace and interrupt. `plan` numbers this
    /// preview; only a confirmation naming it can start the repair.
    RepairPlanned {
        id: MaintenanceId,
        plan: u64,
        preview: String,
    },
    /// An earlier repair stopped before replacing any file, and its record can be discarded
    /// (sent right after `Inspected`, instead of `RepairResumable`).
    RepairDiscardable {
        id: MaintenanceId,
    },
    /// The answer to `DiscardRepair`: the record was retired; no installed file was touched.
    RepairDiscarded {
        id: MaintenanceId,
        lines: Vec<String>,
    },
    /// An earlier repair left a record that can be resumed (sent right after `Inspected`).
    RepairResumable {
        id: MaintenanceId,
        lines: Vec<String>,
    },
    /// The repair waits for its next stage: the old agent's clean exit, or the new agent's
    /// health. The controller keeps asking with `VerifyRepair` (with a fresh Status when it has
    /// one) until this ends. `closeable` says closing the window now cuts no change short and
    /// leaves an interrupted repair that the next visit can resume.
    RepairWaiting {
        id: MaintenanceId,
        detail: String,
        closeable: bool,
    },
    /// The repair ended. `resumable` says a Resume can still make progress.
    RepairFinished {
        id: MaintenanceId,
        outcome: RepairOutcome,
        lines: Vec<String>,
        resumable: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum NativeReport {
    Step(StepReport),
    Maintenance(MaintenanceReport),
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NativeRefusal {
    #[error("the platform is busy")]
    Busy,
    #[error("the platform does not own this step")]
    UnknownStep,
    #[error("{0}")]
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureReadiness {
    Idle,
    Launching,
    Ready,
    Failed(FixtureError),
}

/// One owned practice fixture child at a time, launched for one attempt.
pub trait PracticeFixtures {
    fn launch(&mut self, attempt: AttemptId) -> Result<(), FixtureError>;
    fn readiness(&mut self) -> FixtureReadiness;
    fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError>;
    /// Receipts keep the complete-line receipt time the sequencer requires.
    fn poll(&mut self) -> Vec<FixtureReceipt>;
    fn complete_closed(
        &mut self,
        attempt: AttemptId,
        fixture: FixtureId,
    ) -> Result<(), FixtureError>;
    fn retire(&mut self);
}

/// What one support check found, in the person's terms. Text is plain and observed: a failed
/// check names the reason; a check that couldn't be confirmed names what couldn't be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckState {
    /// The check is part of a pass that is running now.
    Checking,
    /// The check passed. The value seen, when it helps (for example "Hyprland 0.56.2").
    Passed(Option<String>),
    /// An established negative, with the one-line reason.
    Failed(String),
    /// The fact couldn't be read or proved, with the issue in plain words.
    Unconfirmed(String),
}

/// One row of the support checklist shown under the "can run Crosspane" card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportCheck {
    pub label: String,
    pub state: CheckState,
}

impl SupportCheck {
    pub fn new(label: impl Into<String>, state: CheckState) -> Self {
        Self {
            label: label.into(),
            state,
        }
    }
}

/// The checks of the most recent finished support detection pass. Presentation only: it is
/// never evidence, and the step's state still comes from its own report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupportChecklist {
    /// The native step whose card the checklist belongs under.
    pub step: StepId,
    /// Increases with every published pass, so a reader can tell a new pass from an old one.
    /// Pass 0 is the empty placeholder before any pass has finished.
    pub pass: u64,
    pub checks: Vec<SupportCheck>,
}

/// A shared slot the support detection writes each finished pass into and the platform port
/// reads from. One slot per platform instance; clones share it.
#[derive(Clone, Debug)]
pub struct SupportChecksSlot {
    step: StepId,
    latest: Arc<Mutex<Option<SupportChecklist>>>,
}

impl SupportChecksSlot {
    /// The most checks one pass may carry; extra rows are dropped.
    pub const MAX_CHECKS: usize = 16;

    /// A slot for `step`'s checks. Until the first pass finishes it holds pass 0 with no
    /// checks, so the reader knows which card the checklist belongs under from the start.
    pub fn new(step: StepId) -> Self {
        Self {
            step,
            latest: Arc::new(Mutex::new(Some(SupportChecklist {
                step,
                pass: 0,
                checks: Vec::new(),
            }))),
        }
    }

    /// Record one finished pass. A poisoned slot is recovered: the checklist is display-only.
    pub fn publish(&self, mut checks: Vec<SupportCheck>) {
        checks.truncate(Self::MAX_CHECKS);
        let mut latest = self
            .latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pass = latest
            .as_ref()
            .map_or(1, |l| l.pass.saturating_add(1))
            .max(1);
        *latest = Some(SupportChecklist {
            step: self.step,
            pass,
            checks,
        });
    }

    pub fn latest(&self) -> Option<SupportChecklist> {
        self.latest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The only platform port. Every method is non-blocking.
pub trait Platform {
    fn describe(&self) -> PlatformDescription;
    fn submit(&mut self, job: NativeJob) -> Result<(), NativeRefusal>;
    fn poll(&mut self) -> Vec<NativeReport>;
    fn agent(&mut self) -> &mut dyn AgentPort;
    fn fixtures(&mut self) -> &mut dyn PracticeFixtures;
    fn shutdown(&mut self);
    /// The checks of the latest finished support detection pass, if the platform reports them.
    fn support_checks(&mut self) -> Option<SupportChecklist> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("platform step ids must be unique and within 10-59")]
    StepIds,
    #[error("a platform anchor or prerequisite names an unknown step")]
    UnknownAnchor,
    #[error("invalid step graph: {0}")]
    Graph(#[from] crosspane_installer_core::GraphError),
}

/// Button and field ids. Stable so tests and the shell agree on meaning.
pub mod ids {
    use crate::tutorial_flow::{HumanConfirmation, TutorialRole};
    use crosspane_installer_core::StepId;

    pub const NEXT: u16 = 1;
    pub const BACK: u16 = 2;
    pub const CLOSE: u16 = 3;
    pub const REMOVE_OR_REPAIR: u16 = 4;
    /// One "Check again" for a screen where more than one step is waiting: it re-checks each.
    /// A screen with a single waiting step uses that step's own [`retry`] id.
    pub const RETRY_ALL: u16 = 5;
    pub const PEER_ADDRESS: u16 = 1;
    pub const PAIR_LISTEN: u16 = 2001;
    pub const PAIR_JOIN: u16 = 2002;
    pub const PAIR_DIAL: u16 = 2003;
    pub const PAIR_SCAN: u16 = 2004;
    pub const PAIR_CONFIRM: u16 = 2101;
    pub const PAIR_REJECT: u16 = 2102;
    pub const GRANTS_APPLY: u16 = 2201;
    pub const LAYOUT_ACCEPT: u16 = 2301;
    pub const HIDING_APPLY: u16 = 2401;
    pub const HIDING_RESTART: u16 = 2402;
    pub const PLAY_TONE: u16 = 3200;
    pub const PRACTICE_CANCEL: u16 = 3201;
    pub const FINAL_CHECK: u16 = 4001;
    pub const REMOVE_REVIEW: u16 = 5001;
    pub const REMOVE_CONFIRM: u16 = 5002;
    pub const REPAIR: u16 = 5005;
    pub const REPAIR_CONFIRM: u16 = 5006;
    pub const REPAIR_RESUME: u16 = 5007;
    pub const REPAIR_DISCARD: u16 = 5008;

    pub fn retry(step: StepId) -> u16 {
        100 + step.0
    }
    pub fn consent(step: StepId) -> u16 {
        1000 + step.0
    }
    pub fn pair_candidate(index: usize) -> u16 {
        2010 + index.min(15) as u16
    }
    pub fn select_peer(index: usize) -> u16 {
        2030 + index.min(15) as u16
    }
    pub fn pair_pick(index: usize) -> u16 {
        2110 + index.min(15) as u16
    }
    pub fn grant_field(index: usize) -> u16 {
        10 + index.min(15) as u16
    }
    pub fn removal_field(choice: u16) -> u16 {
        50 + choice.min(40)
    }
    pub fn practice_start(role: TutorialRole) -> u16 {
        3000 + super::graph::role_index(role) as u16
    }
    pub fn confirm(c: HumanConfirmation) -> u16 {
        3100 + super::practice::confirmation_index(c) as u16
    }
    pub fn remote_window(index: usize) -> u16 {
        3300 + index.min(15) as u16
    }
    pub fn follow_up_confirm(id: u16) -> u16 {
        5100 + id.min(90)
    }
    pub fn follow_up_decline(id: u16) -> u16 {
        5200 + id.min(90)
    }
}
