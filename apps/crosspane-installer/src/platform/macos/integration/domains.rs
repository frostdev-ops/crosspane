//! The native domains the Mac worker drives, as small traits.
//!
//! Every production adapter (see `native.rs`) wraps one merged native module and adds no policy:
//! it translates the module's typed results into the vocabulary below. The worker (see
//! `worker.rs`) holds the policy that decides what each result means for a step.
//!
//! A build that cannot prove the Mac's session or Crosspane's signature has no production
//! adapters at all: it gets the `Blocked` domains, which refuse everything with one typed reason
//! and change nothing.

use crosspane_installer_core::{ObservationSource, OperationId};

use super::super::native_io::{AdmittedTutorialChild, Deadline};
use super::super::transport::{SelectedAgent, SelectedLink};
use crate::agent_contract::AgentReply;
use crate::live::{Availability, MaintenanceOutcome, RemovalChoice};

/// Support detection. Each call is a fresh read: the proofs it takes live five seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SupportOutcome {
    /// Supported, observed from this source. A scratch target is never `Live`, so core refuses
    /// everything it observes.
    Supported(ObservationSource),
    /// A known, established negative. Nothing will be changed.
    Unsupported(String),
    /// Unknown, ambiguous or not provable by this build. Nothing will be changed.
    Unavailable(String),
}

pub trait Support: Send {
    fn observe(&mut self, deadline: &Deadline) -> SupportOutcome;
}

/// Where the payload and the sign-in item stand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallState {
    /// The files and the sign-in item are Crosspane's, match this payload and were verified.
    Current,
    /// Absent, or Crosspane's own but stale or unfinished: a plan can bring it up to date.
    Needed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallPreview {
    pub version: String,
    /// The running agent will be stopped (and its sessions with it) before its files change.
    pub interrupts_agent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallApplied {
    /// The sign-in item was started: the new agent comes up by itself and is checked next.
    Requested,
    /// Something may have changed but the outcome is unknown: detect again before any retry.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// Something Crosspane didn't put there is in the way. It is left exactly as it is.
    Foreign,
    /// The person turned Crosspane's login item off in System Settings.
    UserDisabled,
    /// The service state can't be read, so nothing is assumed.
    Unobservable,
    /// This build refuses to change anything, for this reason.
    Blocked(String),
    /// Unavailable right now (not admitted, nothing readable). Nothing was changed.
    Unavailable,
    /// The adapter refused a stale or mismatched request. Nothing was changed.
    Refused,
    /// A change may have happened: detect again before anything is retried.
    OutcomeUnknown,
    /// The adapter doesn't support this target.
    Unsupported,
    /// Anything else, with no change made.
    Failed,
}

/// The payload and the sign-in item as one unit: the merged adapters drive the install, the
/// sign-in item and the agent's first start together, so they are one native flow.
pub trait Installs: Send {
    /// A read-only look. `status` is the Status issued for this job, when an agent answered.
    fn detect(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<InstallState, InstallError>;
    /// Plan for `operation` and keep the plan for exactly one `apply`.
    fn plan(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<Option<InstallPreview>, InstallError>;
    /// Apply exactly the plan kept for `operation`.
    fn apply(
        &mut self,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<InstallApplied, InstallError>;
    /// Confirm against a Status issued after the apply: the running agent is the installed one.
    fn verify(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<ObservationSource, InstallError>;
}

/// A freshly admitted agent: the endpoint, process and signature the exchange rests on. Opaque;
/// only the agent port that consumes it can use it.
pub struct Admitted {
    pub(super) inner: AdmittedInner,
}

pub(super) enum AdmittedInner {
    Native(Box<SelectedAgent>),
    Injected,
}

impl std::fmt::Debug for Admitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Admitted")
    }
}

impl Admitted {
    /// An admission that carries no native proof, for injected agent ports in tests.
    #[doc(hidden)]
    pub fn injected() -> Self {
        Self {
            inner: AdmittedInner::Injected,
        }
    }
}

/// Re-admits the running agent: signature, support and instance, in that order, right now.
pub trait Agents: Send {
    fn admit(
        &mut self,
        link: Option<SelectedLink>,
        deadline: &Deadline,
    ) -> Result<Admitted, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub struct UninstallOffer {
    pub uninstall: Availability,
    pub choices: Vec<RemovalChoice>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UninstallResult {
    pub outcome: MaintenanceOutcome,
    pub lines: Vec<String>,
}

pub trait Uninstaller: Send {
    fn inspect(&mut self, deadline: &Deadline) -> UninstallOffer;
    /// Plan the removal for `operation` and return its preview text.
    fn plan(
        &mut self,
        choices: &[(u16, bool)],
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<String, String>;
    /// Run the removal for the previewed plan. The adapter re-observes everything first and
    /// refuses if anything differs from what was shown.
    fn apply(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<UninstallResult, String>;
}

/// What compatible repair offers for the install on this Mac.
#[derive(Clone, Debug, PartialEq)]
pub struct RepairOffer {
    /// `Available` only when the producer's admission and plan say the install is compatible;
    /// otherwise the typed guidance (for example "uninstall, then install").
    pub repair: Availability,
    /// A repair that is still unfinished in this window and can be resumed, as lines.
    pub resumable: Option<Vec<String>>,
}

/// How a repair that went past planning ended, as the adapter proved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairFinish {
    pub outcome: crate::live::RepairOutcome,
    pub lines: Vec<String>,
    /// A resume can still make progress.
    pub resumable: bool,
}

/// Where a repair stands after a stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepairStep {
    /// Everything that changes something is done; the new agent's health isn't proved yet. The
    /// worker keeps asking for it, and ends the wait with `if_timed_out`.
    Waiting {
        detail: String,
        if_timed_out: RepairFinish,
        /// Closing the window now can't cut a change short, and what is left can still be
        /// settled later: only the new agent's health is being watched, and an interrupted
        /// repair is found again on the next visit. `false` while the old agent is stopped and
        /// files are still to be replaced, or when nothing would be left to resume.
        closeable: bool,
    },
    Finished(RepairFinish),
}

/// Receipt-bound compatible repair of an existing Crosspane install. Every call re-reads what it
/// needs, and an adapter never repeats a change whose outcome is uncertain. An `Err` is a
/// refusal with guidance: it means nothing was changed.
pub trait Repairer: Send {
    fn inspect(&mut self, deadline: &Deadline) -> RepairOffer;
    /// Plan for `operation`, returning exactly the preview the person is asked to consent to.
    fn plan(
        &mut self,
        status: Option<&AgentReply>,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<String, String>;
    /// Consent to the plan made for `plan`. The adapter observes everything again (a preview's
    /// evidence lives seconds), starts only if it still previews exactly what was shown, and
    /// then runs every stage up to starting the new agent. `operation` is a fresh id for that
    /// second observation.
    fn confirm(
        &mut self,
        status: Option<&AgentReply>,
        plan: OperationId,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<RepairStep, String>;
    /// Look again for the new instance's health.
    fn verify(&mut self, status: Option<&AgentReply>, deadline: &Deadline) -> RepairStep;
    /// Resume a repair that is still unfinished in this window; one attempt, never a replay.
    fn resume(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<RepairFinish, String>;
}

/// Where the shared sound driver stands. Advisory: nothing here proves working audio, which only
/// the practice steps (a real tone, heard) can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioState {
    /// The driver is in place and the installer's own outcome says so (or it was already there).
    Installed,
    /// Absent: the package can be opened.
    Needed,
    /// The installer window was opened and hasn't reported an outcome yet.
    InProgress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioPreview {
    pub version: String,
    /// Installing replaces a shared system component, which briefly interrupts this Mac's sound.
    pub interrupts_system_audio: bool,
    /// An earlier copy is kept for manual recovery.
    pub keeps_previous: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioError {
    /// This build refuses to change anything, for this reason.
    Blocked(String),
    /// The driver folders or the packages can't be read right now. Nothing is assumed.
    Unavailable,
    /// The adapter refused (unsafe folder metadata, another volume, a stale request).
    Refused(String),
    /// The installer reported that it failed. Any earlier copy is kept for manual recovery.
    Failed(String),
    /// The installer window was opened and hasn't finished.
    Waiting,
    /// The outcome can't be told apart from an earlier run: check again before retrying.
    Unknown,
}

/// The shared sound driver, installed by macOS Installer from a package the build ships.
pub trait AudioPackages: Send {
    fn detect(&mut self, deadline: &Deadline) -> Result<AudioState, AudioError>;
    /// Plan for `operation` and keep the plan for exactly one `apply`.
    fn plan(
        &mut self,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<AudioPreview, AudioError>;
    /// Open macOS Installer on the previewed package. It does the work, with the person's
    /// administrator password; closing its window doesn't cancel what it already started.
    fn apply(&mut self, operation: OperationId, deadline: &Deadline) -> Result<(), AudioError>;
    fn verify(&mut self, deadline: &Deadline) -> Result<ObservationSource, AudioError>;
}

/// The worker's native domains, built on the worker thread.
pub struct Domains {
    pub support: Box<dyn Support>,
    pub installs: Box<dyn Installs>,
    pub audio: Box<dyn AudioPackages>,
    pub agents: Box<dyn Agents>,
    pub uninstaller: Box<dyn Uninstaller>,
    pub repairer: Box<dyn Repairer>,
    /// The tutorial executable's admitted launcher, when the build can launch one.
    pub fixtures: Option<Box<dyn FixtureLauncher>>,
}

pub type DomainFactory = Box<dyn FnOnce() -> Domains + Send>;

/// A launched practice fixture: opaque, handed from the worker to the GUI-thread fixture port.
pub struct FixtureChild {
    pub(super) inner: FixtureChildInner,
}

pub(super) enum FixtureChildInner {
    Native(Box<AdmittedTutorialChild>),
}

impl std::fmt::Debug for FixtureChild {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FixtureChild")
    }
}

pub trait FixtureLauncher: Send {
    /// Launch the installed tutorial executable against a fresh support proof.
    fn launch(
        &mut self,
        font: &std::path::Path,
        deadline: &Deadline,
    ) -> Result<FixtureChild, String>;
}

// ---- builds that can change nothing -----------------------------------------------------------

/// Every domain refuses with the same typed reason. Detection, planning and applying all report
/// it; nothing is read from the disk, the payload or the network to work around it.
#[derive(Clone, Debug)]
pub struct Blocked {
    pub reason: String,
}

impl Support for Blocked {
    fn observe(&mut self, _deadline: &Deadline) -> SupportOutcome {
        SupportOutcome::Unavailable(self.reason.clone())
    }
}

impl Installs for Blocked {
    fn detect(
        &mut self,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<InstallState, InstallError> {
        Err(InstallError::Blocked(self.reason.clone()))
    }
    fn plan(
        &mut self,
        _operation: OperationId,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<Option<InstallPreview>, InstallError> {
        Err(InstallError::Blocked(self.reason.clone()))
    }
    fn apply(
        &mut self,
        _operation: OperationId,
        _deadline: &Deadline,
    ) -> Result<InstallApplied, InstallError> {
        Err(InstallError::Blocked(self.reason.clone()))
    }
    fn verify(
        &mut self,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<ObservationSource, InstallError> {
        Err(InstallError::Blocked(self.reason.clone()))
    }
}

impl Agents for Blocked {
    fn admit(
        &mut self,
        _link: Option<SelectedLink>,
        _deadline: &Deadline,
    ) -> Result<Admitted, String> {
        Err(self.reason.clone())
    }
}

impl Uninstaller for Blocked {
    fn inspect(&mut self, _deadline: &Deadline) -> UninstallOffer {
        UninstallOffer {
            uninstall: Availability::Unavailable(self.reason.clone()),
            choices: Vec::new(),
        }
    }
    fn plan(
        &mut self,
        _choices: &[(u16, bool)],
        _operation: OperationId,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<String, String> {
        Err(self.reason.clone())
    }
    fn apply(
        &mut self,
        _operation: OperationId,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<UninstallResult, String> {
        Err(self.reason.clone())
    }
}

impl Repairer for Blocked {
    fn inspect(&mut self, _deadline: &Deadline) -> RepairOffer {
        RepairOffer {
            repair: Availability::Unavailable(self.reason.clone()),
            resumable: None,
        }
    }
    fn plan(
        &mut self,
        _status: Option<&AgentReply>,
        _operation: OperationId,
        _deadline: &Deadline,
    ) -> Result<String, String> {
        Err(self.reason.clone())
    }
    fn confirm(
        &mut self,
        _status: Option<&AgentReply>,
        _plan: OperationId,
        _operation: OperationId,
        _deadline: &Deadline,
    ) -> Result<RepairStep, String> {
        Err(self.reason.clone())
    }
    fn verify(&mut self, _status: Option<&AgentReply>, _deadline: &Deadline) -> RepairStep {
        RepairStep::Finished(RepairFinish {
            outcome: crate::live::RepairOutcome::OutcomeUnknown,
            lines: vec![self.reason.clone()],
            resumable: false,
        })
    }
    fn resume(
        &mut self,
        _status: Option<&AgentReply>,
        _deadline: &Deadline,
    ) -> Result<RepairFinish, String> {
        Err(self.reason.clone())
    }
}

impl AudioPackages for Blocked {
    fn detect(&mut self, _deadline: &Deadline) -> Result<AudioState, AudioError> {
        Err(AudioError::Blocked(self.reason.clone()))
    }
    fn plan(
        &mut self,
        _operation: OperationId,
        _deadline: &Deadline,
    ) -> Result<AudioPreview, AudioError> {
        Err(AudioError::Blocked(self.reason.clone()))
    }
    fn apply(&mut self, _operation: OperationId, _deadline: &Deadline) -> Result<(), AudioError> {
        Err(AudioError::Blocked(self.reason.clone()))
    }
    fn verify(&mut self, _deadline: &Deadline) -> Result<ObservationSource, AudioError> {
        Err(AudioError::Blocked(self.reason.clone()))
    }
}

impl Blocked {
    pub fn domains(reason: impl Into<String>) -> Domains {
        let blocked = Self {
            reason: reason.into(),
        };
        Domains {
            support: Box::new(blocked.clone()),
            installs: Box::new(blocked.clone()),
            audio: Box::new(blocked.clone()),
            agents: Box::new(blocked.clone()),
            uninstaller: Box::new(blocked.clone()),
            repairer: Box::new(blocked),
            fixtures: None,
        }
    }
}

impl std::fmt::Debug for Domains {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Domains")
    }
}
