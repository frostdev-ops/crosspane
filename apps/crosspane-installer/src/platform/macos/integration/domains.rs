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

use std::path::Path;

use super::super::native_io::{Deadline, NativeError, SupportObservation};
use super::super::transport::{SelectedAgent, SelectedLink};
use crate::agent_contract::AgentReply;
use crate::live::{
    Availability, CheckState, MaintenanceOutcome, RemovalChoice, SupportCheck, SupportChecksSlot,
};

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

// ---- the support checklist ----------------------------------------------------------------------
//
// Presentation only: each row restates what one part of support admission observed. The step's
// outcome still comes from `SupportOutcome` alone; nothing here decides support.

/// The checklist rows, in the order they are shown.
pub const MAC_CHECKS: [&str; 5] = [
    "macOS version",
    "Apple silicon",
    "Signed-in console session",
    "Approved installer build",
    "Installer location",
];

/// What a native error means for a fact that couldn't be confirmed, in plain words.
pub fn unconfirmed(what: &str, error: NativeError) -> CheckState {
    CheckState::Unconfirmed(match error {
        NativeError::Timeout => format!("reading {what} took too long"),
        NativeError::Cancelled => format!("the check of {what} was stopped"),
        NativeError::Busy => format!("{what} couldn't be read while another check was running"),
        _ => format!("couldn't read {what}"),
    })
}

fn console_session(observation: &SupportObservation, uid: u32, gui_tmpdir: &Path) -> CheckState {
    let gui = &observation.gui;
    let named = |session: &str| {
        !session.is_empty() && session.len() <= 64 && !session.chars().any(char::is_control)
    };
    if gui.console_uid.is_none() {
        CheckState::Failed("no one is signed in at this Mac's screen".into())
    } else if gui.console_uid != Some(uid) {
        CheckState::Failed("another account is signed in at this Mac's screen".into())
    } else if gui.interactive_uid != Some(uid) {
        CheckState::Failed("this isn't your signed-in session".into())
    } else if !gui.active {
        CheckState::Failed("your session isn't the active one".into())
    } else if !named(&gui.console_session) {
        CheckState::Failed("the screen's session couldn't be identified".into())
    } else if gui.console_session != gui.interactive_session {
        CheckState::Failed("the screen's session and this session differ".into())
    } else if observation.gui_tmpdir != gui_tmpdir {
        CheckState::Failed(
            "this session's temporary folder isn't the one setup started with".into(),
        )
    } else {
        CheckState::Passed(None)
    }
}

/// The checklist from one support pass: the observed facts (or why they couldn't be read), and
/// the build and location rows the caller already knows.
pub fn mac_support_checks(
    facts: Result<&SupportObservation, NativeError>,
    uid: u32,
    gui_tmpdir: &Path,
    build: CheckState,
    location: CheckState,
) -> Vec<SupportCheck> {
    let (version, silicon, session) = match facts {
        Ok(observation) => (
            if observation.macos_major >= 26 {
                CheckState::Passed(Some(format!("macOS {}", observation.macos_major)))
            } else {
                CheckState::Failed(format!(
                    "macOS {} is older than macOS 26, which Crosspane needs",
                    observation.macos_major
                ))
            },
            if observation.apple_silicon {
                CheckState::Passed(None)
            } else {
                CheckState::Failed("Crosspane needs a Mac with Apple silicon".into())
            },
            console_session(observation, uid, gui_tmpdir),
        ),
        Err(error) => (
            unconfirmed("this Mac's version", error),
            unconfirmed("this Mac's processor", error),
            unconfirmed("the signed-in session", error),
        ),
    };
    MAC_CHECKS
        .iter()
        .zip([version, silicon, session, build, location])
        .map(|(label, state)| SupportCheck::new(*label, state))
        .collect()
}

/// Why this build can't be used here, as checklist rows: which of the build and its location
/// failed, and every Mac fact unconfirmed because nothing is read while blocked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedBy {
    /// No approved inventory is embedded, or it is invalid.
    Inventory,
    /// The inventory is present, but the installer isn't running from a usable home folder.
    Location,
}

pub fn blocked_checks(by: BlockedBy) -> Vec<SupportCheck> {
    let skipped =
        || CheckState::Unconfirmed("not checked, because this installer can't be used here".into());
    let (build, location) = match by {
        BlockedBy::Inventory => (
            CheckState::Failed("this build has no valid approved inventory".into()),
            skipped(),
        ),
        BlockedBy::Location => (
            CheckState::Passed(None),
            CheckState::Failed("the installer isn't open from a folder in your home folder".into()),
        ),
    };
    MAC_CHECKS
        .iter()
        .zip([skipped(), skipped(), skipped(), build, location])
        .map(|(label, state)| SupportCheck::new(*label, state))
        .collect()
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
    /// WP-4.32: what is in the install paths can't be built on. It is saved into the backup
    /// folder (after Crosspane's own sign-in item is stopped) and the install starts fresh.
    pub replacing: bool,
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
    /// What is in the install paths can't be built on as it is. Setup clears it into the
    /// backup folder by itself (WP-4.32); this only reaches the person if that can't be done.
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
    /// Where the last apply saved what it replaced, if it saved anything (WP-4.32).
    fn backup(&self) -> Option<std::path::PathBuf> {
        None
    }
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
    /// An in-memory repair or a strictly read persisted record that can be checked, as lines.
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
    /// Continue a genuine in-memory repair, or reassess a persisted record in a new window.
    /// A persisted hint never authorizes replaying a mutation or reconstructing a clean proof.
    fn resume(
        &mut self,
        status: Option<&AgentReply>,
        deadline: &Deadline,
    ) -> Result<RepairFinish, String>;
}

/// Where the shared sound driver stands. Advisory: nothing here proves working audio.
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
}

pub type DomainFactory = Box<dyn FnOnce() -> Domains + Send>;

// ---- builds that can change nothing -----------------------------------------------------------

/// Every domain refuses with the same typed reason. Detection, planning and applying all report
/// it; nothing is read from the disk, the payload or the network to work around it.
#[derive(Clone, Debug)]
pub struct Blocked {
    pub reason: String,
    /// Where each support pass's checklist goes, and what it says, when the window shows one.
    pub checks: Option<(SupportChecksSlot, BlockedBy)>,
}

impl Support for Blocked {
    fn observe(&mut self, _deadline: &Deadline) -> SupportOutcome {
        if let Some((slot, by)) = &self.checks {
            slot.publish(blocked_checks(*by));
        }
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
        Self::domains_reporting(reason, None)
    }

    /// The blocked domains, with each support pass writing its checklist to `checks`.
    pub fn domains_reporting(
        reason: impl Into<String>,
        checks: Option<(SupportChecksSlot, BlockedBy)>,
    ) -> Domains {
        let blocked = Self {
            reason: reason.into(),
            checks,
        };
        Domains {
            support: Box::new(blocked.clone()),
            installs: Box::new(blocked.clone()),
            audio: Box::new(blocked.clone()),
            agents: Box::new(blocked.clone()),
            uninstaller: Box::new(blocked.clone()),
            repairer: Box::new(blocked),
        }
    }
}

impl std::fmt::Debug for Domains {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Domains")
    }
}

#[cfg(test)]
mod checklist_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::PathBuf;

    use super::super::super::native_io::GuiObservation;
    use super::*;

    fn facts() -> SupportObservation {
        SupportObservation {
            macos_major: 27,
            apple_silicon: true,
            gui: GuiObservation {
                console_uid: Some(501),
                interactive_uid: Some(501),
                console_session: "100".into(),
                interactive_session: "100".into(),
                active: true,
            },
            gui_tmpdir: PathBuf::from("/private/var/folders/x/T"),
        }
    }

    fn rows(observation: &SupportObservation) -> Vec<SupportCheck> {
        mac_support_checks(
            Ok(observation),
            501,
            Path::new("/private/var/folders/x/T"),
            CheckState::Passed(None),
            CheckState::Passed(None),
        )
    }

    #[test]
    fn observed_facts_map_to_their_rows_with_plain_reasons() {
        let good = rows(&facts());
        assert_eq!(
            good.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            MAC_CHECKS.to_vec()
        );
        assert_eq!(good[0].state, CheckState::Passed(Some("macOS 27".into())));
        assert!(
            good.iter()
                .all(|c| matches!(c.state, CheckState::Passed(_)))
        );
        type Edit = fn(&mut SupportObservation);
        let cases: [(Edit, usize, &str); 7] = [
            (|f| f.macos_major = 15, 0, "macOS 15 is older than macOS 26"),
            (|f| f.apple_silicon = false, 1, "Apple silicon"),
            (|f| f.gui.console_uid = None, 2, "no one is signed in"),
            (|f| f.gui.console_uid = Some(502), 2, "another account"),
            (|f| f.gui.active = false, 2, "isn't the active one"),
            (|f| f.gui.interactive_session = "101".into(), 2, "differ"),
            (
                |f| f.gui_tmpdir = PathBuf::from("/tmp/other"),
                2,
                "temporary folder",
            ),
        ];
        for (edit, row, reason) in cases {
            let mut observation = facts();
            edit(&mut observation);
            let checks = rows(&observation);
            for (index, check) in checks.iter().enumerate() {
                if index == row {
                    match &check.state {
                        CheckState::Failed(text) => assert!(text.contains(reason), "{text}"),
                        other => panic!("{reason}: {other:?}"),
                    }
                } else {
                    assert!(
                        matches!(check.state, CheckState::Passed(_)),
                        "{reason}: {check:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn unreadable_facts_are_unconfirmed_with_the_issue() {
        let checks = mac_support_checks(
            Err(NativeError::Timeout),
            501,
            Path::new("/"),
            CheckState::Passed(None),
            CheckState::Passed(None),
        );
        assert_eq!(
            checks[2].state,
            CheckState::Unconfirmed("reading the signed-in session took too long".into())
        );
        assert!(matches!(checks[0].state, CheckState::Unconfirmed(_)));
        assert!(matches!(checks[3].state, CheckState::Passed(_)));
    }

    #[test]
    fn a_blocked_build_names_what_blocked_it_and_reads_nothing_else() {
        let inventory = blocked_checks(BlockedBy::Inventory);
        assert!(matches!(inventory[3].state, CheckState::Failed(_)));
        let location = blocked_checks(BlockedBy::Location);
        assert_eq!(location[3].state, CheckState::Passed(None));
        assert!(matches!(location[4].state, CheckState::Failed(_)));
        for checks in [&inventory, &location] {
            assert!(
                checks[..3]
                    .iter()
                    .all(|c| matches!(c.state, CheckState::Unconfirmed(_)))
            );
        }
        // Every support pass of a blocked build republishes, so "Check again" ends a new pass.
        let slot = SupportChecksSlot::new(crosspane_installer_core::StepId(10));
        let mut blocked = Blocked {
            reason: "blocked".into(),
            checks: Some((slot.clone(), BlockedBy::Location)),
        };
        let deadline = Deadline::new(
            1_000,
            std::sync::Arc::new(super::super::super::native_io::MonotonicClock::default()),
            super::super::super::native_io::Cancellation::default(),
        )
        .unwrap();
        assert_eq!(
            blocked.observe(&deadline),
            SupportOutcome::Unavailable("blocked".into())
        );
        blocked.observe(&deadline);
        let latest = slot.latest().unwrap();
        assert_eq!(latest.pass, 2);
        assert_eq!(latest.checks, location);
    }
}
