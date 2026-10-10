//! The native domains the Linux worker drives, as small traits with thin production adapters.
//!
//! Every production adapter wraps one merged native module and adds no policy: it translates the
//! module's typed results into the worker's vocabulary. The worker (see `worker.rs`) holds the
//! policy that decides what each result means for a step.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use crosspane_installer_core::{ObservationSource, OperationId, ResourceReceipt};

use crate::agent_contract::AgentReply;
use crate::live::{Availability, CheckState, SupportCheck, SupportChecksSlot};

use super::super::detect::{
    self, Desktop, Eligibility, Fact, NativeSessionProbes, OsFamily, ProbeIssue, RuntimeFacts,
    SessionFacts, SupportReport, UnsupportedReason, runtime::RuntimeInput,
};
use super::super::firewall::{
    Activity, FirewallError, FirewallPlan, LinuxFirewall, ManagerSelection, PlanRequest, Presence,
    RuleKind, RuleResult, receipts::DurableIntentStore,
};
use super::super::native_io::{
    ChildEnvironment, Deadline, LinuxNativeIo, MAX_ELF_PREFIX_BYTES, NativeError, SupportProof,
};
use super::super::payload::{
    Architecture, MatchingFiles, Package, PayloadError, PayloadInstaller, PayloadPlan,
};
use super::super::service::{
    AgentEvidence, LinuxService, ServiceAction, ServiceError, ServiceFacts, ServiceResult,
};
use super::super::transport::CallerClock;

/// Support detection: the only source of a `SupportProof`, which lives five seconds.
#[derive(Debug)]
pub enum SupportOutcome {
    Supported(SupportProof),
    /// A known, established negative. Nothing will be changed.
    NotSupported(String),
    /// Unknown or ambiguous. Nothing will be changed until it is resolved.
    Pending(String),
}

pub trait Support: Send + Sync {
    /// One read-only detection pass. `package` supplies the staged agent's ELF prefix for the
    /// runtime-library check; without a payload the runtime facts stay unverified.
    fn detect(&self, package: Option<&Package>, deadline: &Deadline) -> SupportOutcome;
    /// Whether this target's observations are live. A scratch target is never live, so core
    /// refuses everything it observes.
    fn source(&self) -> ObservationSource;
    /// Where each finished pass's checklist is written, when this detection reports one.
    fn checks(&self) -> Option<SupportChecksSlot> {
        None
    }
}

/// The user files of the install: detect, plan once, apply that plan, verify against the agent.
pub trait Payloads {
    /// What is on disk, judged against the payload: unfinished work first (an install that was
    /// cut off, or applied but not yet confirmed by the agent reports `Pending`), then each file.
    fn detect(
        &mut self,
        proof: &SupportProof,
        package: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError>;
    /// The files alone, ignoring any journal: used to confirm that what was just written is there.
    fn observe(
        &mut self,
        proof: &SupportProof,
        package: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError>;
    /// Plan for `operation` (resuming an interrupted install when `resume`) and keep the plan.
    fn plan(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        resume: bool,
    ) -> Result<PayloadPreview, PayloadError>;
    /// Apply exactly the plan kept for `operation`.
    fn apply(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<(), PayloadError>;
    fn verify(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        reply: &AgentReply,
        now_ms: u64,
        deadline: &Deadline,
    ) -> Result<(), PayloadError>;
    /// Where the last apply saved what it replaced, if it saved anything (WP-4.32).
    fn backup(&self) -> Option<PathBuf> {
        None
    }
    /// The receipt this adapter successfully applied in this run; an on-disk record alone
    /// cannot authorize restarting a deleted old executable.
    fn applied_operation(&self) -> Option<OperationId> {
        None
    }
    /// GNOME and KDE: a sentence for the preview naming what this install adds for the desktop
    /// (the desktop entry, and on GNOME the Shell extension). `None` when it adds nothing, as on
    /// Hyprland.
    fn desktop_note(&self) -> Option<String> {
        None
    }
    /// Things the last apply did not manage but that don't fail it (for example an extension
    /// installed but not turned on), and what the person needs to do next (log out and in).
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadPreview {
    pub version: String,
    pub resuming: bool,
    /// WP-4.32: what is in the install paths can't be built on. It is moved into the backup
    /// folder and the install starts fresh.
    pub replacing: bool,
}

/// The user-manager unit for the installed agent.
pub trait Services {
    fn payload_applied(&mut self, _operation: Option<OperationId>) {}
    /// Bind the rendered unit and desktop files of `package`. Required before anything else.
    fn prepare(&mut self, package: &Package, deadline: &Deadline) -> Result<(), ServiceError>;
    fn observe(&mut self, deadline: &Deadline) -> Result<ServiceFacts, ServiceError>;
    /// One manager command for one fresh plan; never resent.
    fn apply(
        &mut self,
        proof: &SupportProof,
        action: ServiceAction,
        deadline: &Deadline,
    ) -> Result<ServiceResult, ServiceError>;
    fn agent(
        &mut self,
        facts: &ServiceFacts,
        reply: Option<&AgentReply>,
        expected_id: u64,
        now_ms: u64,
        previous_instance: Option<u64>,
        deadline: &Deadline,
    ) -> Result<AgentEvidence, ServiceError>;
}

/// What the ordinary-user reads say about the firewall; never a privileged read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirewallReading {
    /// `None` when ufw's state can't be read.
    pub active: Option<bool>,
    pub lan_rule: RulePresence,
    /// The interface and network the rule would name, when one link is unambiguous.
    pub link: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulePresence {
    Present,
    Absent,
    /// The rule files can't be read: never assumed absent.
    Unknown,
    /// An administrator-modified rule: kept as it is.
    Modified,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleApply {
    /// The command exited cleanly. That is not verification.
    Dispatched,
    /// The rule was already there; nothing was dispatched.
    AlreadyPresent,
    /// Nothing was dispatched.
    NotDispatched,
    /// No polkit agent could ask; the manual command is shown instead.
    NoPromptAgent(Option<String>),
    /// A timeout or dismissed dialog: the outcome is unknown.
    Unknown,
    /// The firewall or link changed between preview and apply.
    Changed,
    Failed,
}

pub trait Firewalls {
    fn read(&mut self, deadline: &Deadline) -> Result<FirewallReading, FirewallError>;
    /// Plan the LAN rule for `operation` and return the exact preview.
    fn plan_lan(
        &mut self,
        proof: &SupportProof,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<String, FirewallError>;
    /// Consume the plan made for `operation`.
    fn apply_lan(
        &mut self,
        proof: &SupportProof,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<RuleApply, FirewallError>;
    /// The firewall journal's minimal receipt for the rule added under `operation`, if one was
    /// recorded. Kept privately so a later removal can name exactly that rule.
    fn receipt_lan(&mut self, proof: &SupportProof, operation: OperationId) -> Option<Vec<u8>>;
}

/// What compatible repair offers for the install on this computer.
#[derive(Clone, Debug, PartialEq)]
pub struct RepairOffer {
    pub discardable: bool,
    /// `Available` only when the producer's inventory says the install is compatible; otherwise
    /// the typed guidance (for example "remove Crosspane and install it again").
    pub repair: Availability,
    /// An earlier repair left a record that can be resumed, as person-readable lines.
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

/// Compatible repair of an existing install. Every call re-reads what it needs, and an adapter
/// never repeats a change whose outcome is uncertain. An `Err` is a refusal with guidance: it
/// means nothing was changed.
pub trait Repairer {
    fn inspect(&mut self, package: Option<&Package>, now_ms: u64) -> RepairOffer;
    /// Plan for `operation`, returning exactly the preview the person is asked to consent to.
    fn plan(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<String, String>;
    /// Consent to the plan made for `plan`. The adapter observes everything again (a preview's
    /// evidence lives only seconds), starts only if it still previews exactly what was shown,
    /// and then runs every stage up to starting the new agent. `operation` is a fresh id for
    /// that second observation.
    fn confirm(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        plan: OperationId,
        operation: OperationId,
        now_ms: u64,
    ) -> Result<RepairStep, String>;
    /// Look again for the new instance's health with a Status issued after the last look.
    fn verify(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        now_ms: u64,
    ) -> RepairStep;
    /// Retire only a proved unapplied record, using fresh current evidence; default is refusal.
    fn discard(
        &mut self,
        _package: Option<&Package>,
        _status: Option<&AgentReply>,
        _now_ms: u64,
    ) -> Result<RepairFinish, String> {
        Err("That earlier repair cannot be discarded. Nothing was changed.".into())
    }
    /// Resume an interrupted repair from its record; one attempt, never a replay.
    fn resume(
        &mut self,
        package: Option<&Package>,
        status: Option<&AgentReply>,
        now_ms: u64,
    ) -> Result<RepairFinish, String>;
}

/// The domains owned by the worker thread. Built on that thread, because the firewall controller
/// holds a reader that must never cross threads.
pub struct Domains {
    pub payloads: Box<dyn Payloads>,
    pub services: Box<dyn Services>,
    pub firewalls: Box<dyn Firewalls>,
    pub uninstaller: Box<dyn Uninstaller>,
    pub repairer: Box<dyn Repairer>,
}

pub type DomainFactory = Box<dyn FnOnce() -> Domains + Send>;

/// Progress of a removal run after consent.
#[derive(Clone, Debug, PartialEq)]
pub enum UninstallProgress {
    Progress(String),
    /// A separately consented step, for example removing a global firewall rule.
    FollowUp {
        id: u16,
        label: String,
        preview: String,
    },
    Finished(crate::live::MaintenanceOutcome, Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct UninstallOffer {
    pub uninstall: Availability,
    pub choices: Vec<crate::live::RemovalChoice>,
}

pub trait Uninstaller {
    fn inspect(&mut self, package: Option<&Package>, deadline: &Deadline) -> UninstallOffer;
    fn plan(
        &mut self,
        package: Option<&Package>,
        choices: &[(u16, bool)],
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<String, String>;
    /// Begin the run for the previewed plan, then advance one stage per call.
    fn begin(&mut self, package: Option<&Package>, operation: OperationId) -> Result<(), String>;
    fn advance(&mut self, package: Option<&Package>) -> UninstallProgress;
    fn follow_up(&mut self, package: Option<&Package>, id: u16, confirm: bool)
    -> UninstallProgress;
}

// ---- production adapters ----------------------------------------------------------------------

pub fn unsupported_text(reason: UnsupportedReason) -> String {
    detect::unsupported_text(reason)
}

pub fn pending_text(issue: ProbeIssue) -> String {
    match issue {
        ProbeIssue::Ambiguous => {
            "More than one graphical session could be this one, so nothing will be changed until \
             the signed-in session is unique."
                .into()
        }
        ProbeIssue::Unavailable | ProbeIssue::Timeout | ProbeIssue::Cancelled => {
            "This session couldn't be read just now. Nothing will be changed.".into()
        }
        ProbeIssue::Foreign => {
            "This session doesn't match the one setup was started from. Nothing will be changed."
                .into()
        }
        _ => "Some facts about this session couldn't be confirmed yet. Nothing will be changed."
            .into(),
    }
}

// ---- the support checklist ----------------------------------------------------------------------
//
// Presentation only: each row restates one part of the detection report in plain words. The
// step's outcome still comes from `Eligibility` alone; nothing here decides support.

/// The checklist rows of a Hyprland session, in the order they are shown.
pub const LINUX_CHECKS: [&str; 9] = [
    "Operating system",
    "Processor",
    "Hyprland version",
    "Wayland protocols",
    "uwsm session",
    "Graphical session active",
    "This session is the signed-in one",
    "Required libraries",
    "Video support in the payload",
];

/// The same nine rows for a GNOME session.
pub const GNOME_CHECKS: [&str; 9] = [
    "Operating system",
    "Processor",
    "GNOME Shell version",
    "Wayland protocols",
    "GNOME session",
    "Graphical session active",
    "This session is the signed-in one",
    "Required libraries",
    "Video support in the payload",
];

/// The same nine rows for a KDE Plasma session.
pub const KDE_CHECKS: [&str; 9] = [
    "Operating system",
    "Processor",
    "Plasma version",
    "Wayland protocols",
    "Plasma session",
    "Graphical session active",
    "This session is the signed-in one",
    "Required libraries",
    "Video support in the payload",
];

/// The same nine rows when the desktop is one the agent has no backend for.
pub const OTHER_DESKTOP_CHECKS: [&str; 9] = [
    "Operating system",
    "Processor",
    "Desktop",
    "Wayland protocols",
    "Session manager",
    "Graphical session active",
    "This session is the signed-in one",
    "Required libraries",
    "Video support in the payload",
];

/// The rows for the desktop a report describes. Hyprland keeps the original labels.
pub fn check_labels(desktop: &Result<Desktop, UnsupportedReason>) -> &'static [&'static str; 9] {
    match desktop {
        Ok(Desktop::Hyprland) => &LINUX_CHECKS,
        Ok(Desktop::Gnome) => &GNOME_CHECKS,
        Ok(Desktop::Kde) => &KDE_CHECKS,
        Err(_) => &OTHER_DESKTOP_CHECKS,
    }
}

/// What couldn't be confirmed about `what`, in plain words.
fn unconfirmed(what: &str, issue: ProbeIssue) -> CheckState {
    CheckState::Unconfirmed(match issue {
        ProbeIssue::Missing => format!("{what} wasn't found"),
        ProbeIssue::WrongVersion => format!("{what} reported an unexpected version"),
        ProbeIssue::Timeout => format!("reading {what} took too long"),
        ProbeIssue::Cancelled => format!("the check of {what} was stopped"),
        ProbeIssue::Oversize | ProbeIssue::Malformed => {
            format!("{what} couldn't be understood")
        }
        ProbeIssue::Foreign => format!("{what} belongs to a different session"),
        ProbeIssue::Ambiguous => "more than one graphical session could be this one".into(),
        ProbeIssue::Unverified => format!("{what} hasn't been verified yet"),
        _ => format!("couldn't read {what}"),
    })
}

/// A yes/no fact: true passes, false fails with `reason`, an issue is unconfirmed.
fn yes_no(
    fact: &Fact<bool>,
    what: &str,
    reason: UnsupportedReason,
    desktop: Option<Desktop>,
) -> CheckState {
    match fact.value {
        Ok(true) => CheckState::Passed(None),
        Ok(false) => CheckState::Failed(detect::unsupported_text_for(reason, desktop)),
        Err(issue) => unconfirmed(what, issue),
    }
}

/// The first failure wins, then the first unconfirmed part, else passed with `value`.
fn worst(parts: Vec<CheckState>, value: Option<String>) -> CheckState {
    if let Some(failed) = parts
        .iter()
        .find(|part| matches!(part, CheckState::Failed(_)))
    {
        return failed.clone();
    }
    parts
        .into_iter()
        .find(|part| matches!(part, CheckState::Unconfirmed(_) | CheckState::Checking))
        .unwrap_or(CheckState::Passed(value))
}

fn graphical_session_active(report: &SupportReport) -> CheckState {
    let session = &report.session;
    let mut parts = vec![match session.graphical_target_active.value {
        Ok(true) => CheckState::Passed(None),
        Ok(false) => CheckState::Unconfirmed("the graphical session isn't active yet".into()),
        Err(issue) => unconfirmed("the graphical session state", issue),
    }];
    if let Ok(Some(chosen)) = &session.selected_session.value
        && (chosen.session.seat.as_deref().is_none_or(str::is_empty)
            || chosen.session.active != Some(true))
    {
        parts.push(CheckState::Unconfirmed(
            "this session isn't shown as the active one on its seat".into(),
        ));
    }
    worst(parts, None)
}

fn signed_in_session(report: &SupportReport) -> CheckState {
    let session = &report.session;
    let chosen = match &session.selected_session.value {
        Ok(Some(chosen)) => &chosen.session,
        Ok(None) => {
            return CheckState::Unconfirmed(
                "more than one graphical session could be this one".into(),
            );
        }
        Err(issue) => return unconfirmed("this session", *issue),
    };
    let mut parts = Vec::new();
    match chosen.kind.as_deref() {
        None => parts.push(CheckState::Unconfirmed(
            "couldn't read the session type".into(),
        )),
        Some("wayland") => {}
        Some(_) => parts.push(CheckState::Failed(unsupported_text(
            UnsupportedReason::SessionType,
        ))),
    }
    if chosen.uid != Some(session.uid) {
        parts.push(CheckState::Unconfirmed(
            "this session belongs to another account".into(),
        ));
    }
    match session.graphical_sessions.value {
        Ok(1) => {}
        Ok(0) => parts.push(CheckState::Unconfirmed(
            "no graphical session was found".into(),
        )),
        Ok(count) => parts.push(CheckState::Unconfirmed(format!(
            "{count} graphical sessions are open, so this one isn't unique"
        ))),
        Err(issue) => parts.push(unconfirmed("the list of graphical sessions", issue)),
    }
    // An unsupported desktop never gets here with a verdict of its own: its row says so above.
    let desktop = session.desktop.unwrap_or(Desktop::Hyprland);
    match &session.manager_environment.value {
        Ok(effective) if effective.agrees_with(&session.selected_environment, desktop) => {}
        Ok(_) => parts.push(CheckState::Unconfirmed(
            "the session manager's environment doesn't match this session".into(),
        )),
        Err(issue) => parts.push(unconfirmed("the session environment", *issue)),
    }
    worst(parts, None)
}

/// The version row: Hyprland's floor is eligibility; GNOME's and KDE's version is information
/// (plus a note when the Shell is too old for the Crosspane extension, which then isn't installed).
fn compositor_row(session: &SessionFacts) -> CheckState {
    let version = session.compositor_version.value;
    match session.desktop {
        Ok(Desktop::Hyprland) => match version {
            Ok(v) if v >= [0, 56, 0] => {
                CheckState::Passed(Some(format!("Hyprland {}.{}.{}", v[0], v[1], v[2])))
            }
            Ok(v) => CheckState::Failed(format!(
                "Hyprland {}.{}.{} is older than 0.56, which Crosspane needs",
                v[0], v[1], v[2]
            )),
            Err(issue) => unconfirmed("the Hyprland version", issue),
        },
        Ok(Desktop::Gnome) => match version {
            Ok(v) if v[0] >= super::super::extension::MIN_SHELL => {
                CheckState::Passed(Some(format!("GNOME Shell {}.{}", v[0], v[1])))
            }
            Ok(v) => CheckState::Note(format!(
                "GNOME Shell {}.{} is older than {}: Crosspane works without its Shell extension, \
                 which won't be installed",
                v[0],
                v[1],
                super::super::extension::MIN_SHELL
            )),
            Err(issue) => unconfirmed("the GNOME Shell version", issue),
        },
        Ok(Desktop::Kde) => match version {
            Ok(v) => CheckState::Passed(Some(format!("Plasma {}.{}", v[0], v[1]))),
            Err(issue) => unconfirmed("the Plasma version", issue),
        },
        Err(reason) => CheckState::Failed(detect::unsupported_text(reason)),
    }
}

fn required_libraries(runtime: &RuntimeFacts) -> CheckState {
    let mut parts = Vec::new();
    // A named library that is missing or the wrong version is the clearest reason: it comes first.
    for library in runtime.libraries.iter().filter(|l| l.required) {
        let name = bounded_name(&library.name);
        match library.resolved.value {
            Ok(_) => {}
            Err(ProbeIssue::Missing) => {
                parts.push(CheckState::Failed(format!("{name} isn't installed")));
            }
            Err(ProbeIssue::WrongVersion) => parts.push(CheckState::Failed(format!(
                "{name} isn't the version Crosspane needs"
            ))),
            Err(ProbeIssue::Foreign) => parts.push(CheckState::Unconfirmed(format!(
                "{name} was found outside the system library folder"
            ))),
            Err(issue) => parts.push(unconfirmed(&name, issue)),
        }
    }
    for (fact, name) in [
        (&runtime.ffmpeg, "FFmpeg"),
        (&runtime.opus, "Opus"),
        (&runtime.pipewire_library, "PipeWire"),
        (&runtime.xkb, "xkbcommon"),
        (&runtime.wayland_library, "the Wayland client library"),
        (&runtime.software_video, "software video decoding"),
    ] {
        parts.push(match fact.value {
            Ok(true) => CheckState::Passed(None),
            Ok(false) => CheckState::Failed(format!("{name} isn't installed")),
            Err(issue) => unconfirmed(name, issue),
        });
    }
    if !runtime.libraries.iter().any(|l| l.required) {
        parts.push(CheckState::Unconfirmed(
            "the libraries can't be checked until a staged payload is read".into(),
        ));
    }
    if runtime.libei_required {
        parts.push(CheckState::Unconfirmed(
            "libei support hasn't been verified yet".into(),
        ));
    }
    if let Err(issue) = runtime.dependency_graph.value {
        parts.push(unconfirmed("the dependency graph", issue));
    }
    let found = runtime.libraries.iter().filter(|l| l.required).count();
    worst(parts, Some(format!("{found} found")))
}

/// A library name as shown: bounded and printable.
fn bounded_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// One row per check, mapped from a finished detection report.
fn advisory(state: CheckState) -> CheckState {
    match state {
        CheckState::Unconfirmed(issue) => CheckState::Note(format!("{issue}; setup can continue")),
        other => other,
    }
}
pub fn support_checks(report: &SupportReport) -> Vec<SupportCheck> {
    let session = &report.session;
    let os = match &session.os.value {
        Ok(OsFamily::Arch) => CheckState::Passed(Some("Arch-based".into())),
        Ok(OsFamily::Other(_)) => {
            CheckState::Failed(unsupported_text(UnsupportedReason::OperatingSystem))
        }
        Err(issue) => unconfirmed("the operating system", *issue),
    };
    let processor = match &session.architecture.value {
        Ok(detect::Architecture::X86_64) => CheckState::Passed(Some("x86-64".into())),
        Ok(detect::Architecture::Aarch64) => CheckState::Passed(Some("ARM64".into())),
        Ok(detect::Architecture::Other(_)) => {
            CheckState::Failed(unsupported_text(UnsupportedReason::Architecture))
        }
        Err(issue) => unconfirmed("the processor type", *issue),
    };
    let desktop = session.desktop.ok();
    let (protocols_what, managed_reason) = match desktop {
        Some(Desktop::Gnome) => (
            "GNOME's Wayland protocols",
            UnsupportedReason::SessionManager,
        ),
        Some(Desktop::Kde) => (
            "Plasma's Wayland protocols",
            UnsupportedReason::SessionManager,
        ),
        _ => ("Hyprland's Wayland protocols", UnsupportedReason::Uwsm),
    };
    let states = [
        advisory(os),
        advisory(processor),
        // Hyprland's floor is eligibility and an established negative fails; an unknown or a
        // GNOME/KDE version never blocks setup (the agent probes at run time).
        advisory(compositor_row(session)),
        advisory(yes_no(
            &session.protocols,
            protocols_what,
            UnsupportedReason::RequiredProtocols,
            desktop,
        )),
        yes_no(
            &session.compositor_managed,
            "how this session is managed",
            managed_reason,
            desktop,
        ),
        graphical_session_active(report),
        signed_in_session(report),
        advisory(required_libraries(&report.runtime)),
        advisory(yes_no(
            &report.runtime.video_feature,
            "the staged payload's features",
            UnsupportedReason::VideoFeature,
            desktop,
        )),
    ];
    check_labels(&session.desktop)
        .iter()
        .zip(states)
        .map(|(label, state)| SupportCheck::new(*label, state))
        .collect()
}

/// Every row unconfirmed for one reason, when the pass couldn't run at all.
pub fn unchecked_support(issue: &str) -> Vec<SupportCheck> {
    LINUX_CHECKS
        .iter()
        .map(|label| SupportCheck::new(*label, CheckState::Unconfirmed(issue.to_owned())))
        .collect()
}

/// The real read-only session and runtime detection.
pub struct NativeSupport {
    pub io: Arc<LinuxNativeIo>,
    pub env: ChildEnvironment,
    pub clock: CallerClock,
    /// Each finished pass's checklist is written here for the installer window.
    pub checks: SupportChecksSlot,
}

impl NativeSupport {
    pub(super) fn runtime_facts(
        &self,
        package: Option<&Package>,
        deadline: &Deadline,
    ) -> RuntimeFacts {
        let source = self.io.target().source();
        let now = (self.clock)();
        let unverified = || detect::Fact::issue(ProbeIssue::Unverified, source, now);
        let empty = RuntimeFacts {
            dependency_graph: unverified(),
            libraries: Vec::new(),
            video_feature: unverified(),
            ffmpeg: unverified(),
            opus: unverified(),
            pipewire_library: unverified(),
            xkb: unverified(),
            wayland_library: unverified(),
            software_video: unverified(),
            gpu: unverified(),
            libei_required: false,
            pipewire: unverified(),
            session_manager: unverified(),
            secret_service: unverified(),
            keystore: detect::Fact::issue(ProbeIssue::Unverified, source, now),
        };
        let Some(package) = package else {
            return empty;
        };
        let agent = package.agent_elf();
        if agent.is_empty() || agent.len() > MAX_ELF_PREFIX_BYTES {
            return empty;
        }
        let Ok(architecture) = Architecture::native() else {
            return empty;
        };
        let features = package
            .manifest()
            .members
            .iter()
            .find(|m| m.name == "bin/crosspane-agent")
            .map(|m| m.features.as_slice())
            .unwrap_or(&[]);
        detect::runtime::inspect(
            self.io.clone(),
            self.env.clone(),
            deadline,
            RuntimeInput {
                architecture,
                features,
                agent_elf: agent,
                keystore: None,
            },
            &|| (self.clock)(),
        )
    }
}

impl Support for NativeSupport {
    fn detect(&self, package: Option<&Package>, deadline: &Deadline) -> SupportOutcome {
        let probes =
            match NativeSessionProbes::new(self.io.clone(), self.env.clone(), self.clock.clone()) {
                Ok(probes) => probes,
                Err(_) => {
                    self.checks
                        .publish(unchecked_support("this session's checks couldn't start"));
                    return SupportOutcome::Pending(
                        "This session couldn't be checked. Nothing will be changed.".into(),
                    );
                }
            };
        let runtime = self.runtime_facts(package, deadline);
        let result = probes.detect(runtime, deadline);
        let mut rows = support_checks(&result.report);
        let font = detect::fonts::discover(&self.io, &self.env, deadline);
        rows.push(SupportCheck::new("System font", match font {
            Ok(_) => CheckState::Passed(None),
            Err(_) => CheckState::Note("A system font couldn't be confirmed; setup can continue with the font already loaded".into()),
        }));
        match self.io.dead_runtime() {
            Ok(Some(_)) => rows.push(SupportCheck::new("Stopped Crosspane runtime", CheckState::Note("The owned dead runtime will be cleaned under the install lock when you install".into()))),
            // WP-4.32: a running Crosspane is the ordinary update case; it is restarted on the new files.
            Err(_) => rows.push(SupportCheck::new("Crosspane runtime", CheckState::Note("Crosspane is running; it is restarted on the new files after the install".into()))),
            Ok(None) => {},
        }
        self.checks.publish(rows);
        if let Some(proof) = result.proof {
            return SupportOutcome::Supported(proof);
        }
        match result.report.eligibility {
            Eligibility::Supported => SupportOutcome::Pending(
                "Support was observed but couldn't be admitted. Nothing will be changed.".into(),
            ),
            Eligibility::NotSupported(reason) => SupportOutcome::NotSupported(format!(
                "Not supported yet. {} Nothing will be changed.",
                detect::unsupported_text_for(reason, result.report.session.desktop.ok())
            )),
            Eligibility::Pending(issue) => SupportOutcome::Pending(pending_text(issue)),
        }
    }

    fn source(&self) -> ObservationSource {
        self.io.target().source()
    }

    fn checks(&self) -> Option<SupportChecksSlot> {
        Some(self.checks.clone())
    }
}

/// The real user-file installer.
pub struct NativePayloads {
    installer: PayloadInstaller,
    held: Option<(OperationId, Held)>,
    /// An earlier apply in this run didn't finish: the next plan starts fresh (WP-4.32).
    start_fresh: bool,
    applied_operation: Option<OperationId>,
    /// The selected session's bus address, for the GNOME settings. Without it the Shell
    /// extension is installed but cannot be turned on, which is a warning.
    session: BTreeMap<String, String>,
    note: Option<String>,
    warnings: Vec<String>,
}

/// A kept plan: the ordinary one, or a fresh start over whatever is in the install paths.
enum Held {
    Plan(Box<PayloadPlan>),
    Fresh,
}

/// WP-4.32: these mean "what is in the install paths can't be built on". The installer owns
/// those paths, so they are cleared into the backup folder and the install starts fresh.
/// Anything else (no support, no session, a busy lock, an invalid package, time) is not about
/// the install paths, and stays what it is.
fn start_fresh_over(error: &PayloadError) -> bool {
    matches!(
        error,
        PayloadError::Foreign
            | PayloadError::Pending
            | PayloadError::OutcomeUnknown
            | PayloadError::Native(
                NativeError::Foreign | NativeError::OutcomeUnknown | NativeError::Unavailable
            )
    )
}

impl NativePayloads {
    pub fn new(io: Arc<LinuxNativeIo>) -> Result<Self, PayloadError> {
        Ok(Self {
            installer: PayloadInstaller::new(io)?,
            held: None,
            start_fresh: false,
            applied_operation: None,
            session: BTreeMap::new(),
            note: None,
            warnings: Vec::new(),
        })
    }

    /// Let this adapter reach the selected session's settings (GNOME's extension switch).
    pub fn with_session(mut self, env: &ChildEnvironment) -> Self {
        self.session = env
            .values()
            .iter()
            .filter(|(key, _)| key.as_str() == "DBUS_SESSION_BUS_ADDRESS")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        self
    }

    /// The desktop files as resource rows, after the core files' own.
    fn with_desktop_rows(
        &self,
        proof: &SupportProof,
        package: &Package,
        mut rows: Vec<ResourceReceipt>,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        let plan = self.installer.desktop_plan_for(package, proof)?;
        rows.extend(self.installer.desktop_rows(proof, &plan)?);
        Ok(rows)
    }

    /// Put the desktop files in place and turn the extension on. A file that can't be written
    /// fails the apply; the extension's switch only ever warns.
    fn apply_desktop(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<(), PayloadError> {
        self.warnings.clear();
        self.warnings =
            self.installer
                .desktop_install(package, proof, &self.session, operation, deadline)?;
        Ok(())
    }

    /// Clear the install paths into the backup folder, then install from nothing.
    fn fresh(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<(), PayloadError> {
        self.installer.reclaim(proof)?;
        deadline.check()?;
        let plan = self
            .installer
            .plan(proof, package, operation, MatchingFiles::Preserve)?;
        let receipt = self.installer.apply(proof, package, plan, deadline)?;
        self.applied_operation = Some(receipt.operation_id);
        self.start_fresh = false;
        Ok(())
    }
}

impl Payloads for NativePayloads {
    fn detect(
        &mut self,
        proof: &SupportProof,
        package: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        // An install that was cut off, or applied but not yet confirmed by the running agent,
        // leaves a journal with unfinished work. Say so before judging any file: files that are
        // half in place look foreign until the journal that placed them is taken into account.
        // Reading the journal for a resume plan changes nothing.
        if let Ok(plan) = self.installer.resume_plan(proof, package)
            && !plan.receipt().unfinished.is_empty()
        {
            return Err(PayloadError::Pending);
        }
        let rows = self.installer.detect(proof, package)?;
        // Matching files with no finished install record behind them are installed (recorded)
        // once more; nothing else would ever confirm them.
        if rows
            .iter()
            .all(|row| row.before == crosspane_installer_core::ResourceObservation::Matching)
            && !self.installer.recorded(proof)?
        {
            return Err(PayloadError::Pending);
        }
        // GNOME and KDE: the desktop files are judged the same way, after the core files.
        self.with_desktop_rows(proof, package, rows)
    }

    fn observe(
        &mut self,
        proof: &SupportProof,
        package: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        let rows = self.installer.detect(proof, package)?;
        self.with_desktop_rows(proof, package, rows)
    }

    fn plan(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        resume: bool,
    ) -> Result<PayloadPreview, PayloadError> {
        self.held = None;
        self.note = self.installer.desktop_plan_for(package, proof)?.preview();
        let attempt = if self.start_fresh {
            Err(PayloadError::Foreign)
        } else if resume {
            self.installer.resume_plan(proof, package)
        } else {
            self.installer
                .plan(proof, package, operation, MatchingFiles::Preserve)
        };
        let (held, preview) = match attempt {
            Ok(plan) => {
                let preview = PayloadPreview {
                    version: plan.receipt().product_version.clone(),
                    resuming: resume,
                    replacing: false,
                };
                (Held::Plan(Box::new(plan)), preview)
            }
            // An unfinished earlier install is first offered as a resume (the caller asks).
            Err(PayloadError::Pending) if !resume && !self.start_fresh => {
                return Err(PayloadError::Pending);
            }
            Err(error) if start_fresh_over(&error) => (
                Held::Fresh,
                PayloadPreview {
                    version: package.manifest().product_version.clone(),
                    resuming: false,
                    replacing: true,
                },
            ),
            Err(error) => return Err(error),
        };
        self.held = Some((operation, held));
        Ok(preview)
    }

    fn apply(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<(), PayloadError> {
        // The plan is single use: whatever happens next, it is gone.
        self.applied_operation = None;
        self.warnings.clear();
        let Some((planned, plan)) = self.held.take() else {
            return Err(PayloadError::Pending);
        };
        if planned != operation {
            return Err(PayloadError::Pending);
        }
        let result = match plan {
            Held::Plan(plan) => match self.installer.apply(proof, package, *plan, deadline) {
                Ok(receipt) => {
                    self.applied_operation = Some(receipt.operation_id);
                    Ok(())
                }
                // The kept plan no longer fits what is there: start fresh, once, right now.
                Err(error) if start_fresh_over(&error) => {
                    self.fresh(proof, package, operation, deadline)
                }
                Err(error) => Err(error),
            },
            Held::Fresh => self.fresh(proof, package, operation, deadline),
        };
        if result.is_err() {
            self.start_fresh = true;
            return result;
        }
        // The nine core files are in place. The desktop files follow; a failure there is its own
        // outcome and does not send the next plan down the clear-and-reinstall path.
        self.apply_desktop(proof, package, operation, deadline)
    }

    fn verify(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        reply: &AgentReply,
        now_ms: u64,
        deadline: &Deadline,
    ) -> Result<(), PayloadError> {
        self.installer
            .verify(proof, package, reply.id, now_ms, reply, deadline)
            .map(|_| ())
    }

    fn backup(&self) -> Option<PathBuf> {
        self.installer.backup_used()
    }
    fn applied_operation(&self) -> Option<OperationId> {
        self.applied_operation
    }
    fn desktop_note(&self) -> Option<String> {
        self.note.clone()
    }
    fn warnings(&self) -> Vec<String> {
        self.warnings.clone()
    }
}

/// The real user-manager unit, bound to the staged package's rendered files.
pub struct NativeServices {
    pub io: Arc<LinuxNativeIo>,
    pub env: ChildEnvironment,
    service: Option<Arc<LinuxService>>,
    applied_operation: Option<OperationId>,
}

impl NativeServices {
    pub fn new(io: Arc<LinuxNativeIo>, env: ChildEnvironment) -> Self {
        Self {
            io,
            env,
            service: None,
            applied_operation: None,
        }
    }

    pub fn shared(&self) -> Option<Arc<LinuxService>> {
        self.service.clone()
    }

    fn bound(&self) -> Result<&Arc<LinuxService>, ServiceError> {
        self.service.as_ref().ok_or(ServiceError::Unknown)
    }
}

impl Services for NativeServices {
    fn payload_applied(&mut self, operation: Option<OperationId>) {
        self.applied_operation = operation;
    }
    fn prepare(&mut self, package: &Package, deadline: &Deadline) -> Result<(), ServiceError> {
        if self.service.is_some() {
            return Ok(());
        }
        let installer =
            PayloadInstaller::new(self.io.clone()).map_err(|_| ServiceError::Unknown)?;
        let resources = installer
            .rendered_resources(package)
            .map_err(|_| ServiceError::Unknown)?;
        let service = LinuxService::new(
            self.io.clone(),
            super::manager_session_of(self.env.values()),
            resources,
            deadline,
        )?;
        self.service = Some(Arc::new(service));
        Ok(())
    }

    fn observe(&mut self, deadline: &Deadline) -> Result<ServiceFacts, ServiceError> {
        self.bound()?.observe(deadline)
    }

    fn apply(
        &mut self,
        proof: &SupportProof,
        action: ServiceAction,
        deadline: &Deadline,
    ) -> Result<ServiceResult, ServiceError> {
        if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
            proof
                .check_agent_compatibility()
                .map_err(ServiceError::Native)?;
        }
        let service = self.bound()?.clone();
        let plan = if action == ServiceAction::Restart
            && let Some(operation) = self.applied_operation
        {
            service.plan_restart_after_payload(proof, operation, deadline)?
        } else {
            service.plan(proof, action, deadline)?
        };
        service.apply(proof, plan, deadline)
    }

    fn agent(
        &mut self,
        facts: &ServiceFacts,
        reply: Option<&AgentReply>,
        expected_id: u64,
        now_ms: u64,
        previous_instance: Option<u64>,
        deadline: &Deadline,
    ) -> Result<AgentEvidence, ServiceError> {
        self.bound()?.agent(
            facts,
            reply,
            expected_id,
            now_ms,
            previous_instance,
            deadline,
        )
    }
}

/// The real ufw reads and the exact single-use LAN rule plan.
pub struct NativeFirewalls {
    firewall: LinuxFirewall,
    store: Option<DurableIntentStore>,
    planned: Option<(OperationId, FirewallPlan)>,
}

impl NativeFirewalls {
    pub fn new(io: Arc<LinuxNativeIo>) -> Self {
        Self {
            firewall: LinuxFirewall::new(io),
            store: None,
            planned: None,
        }
    }
}

impl Firewalls for NativeFirewalls {
    fn read(&mut self, deadline: &Deadline) -> Result<FirewallReading, FirewallError> {
        // Any detection retires an earlier plan; the controller re-plans before consent.
        self.planned = None;
        let snapshot = self.firewall.detect(ManagerSelection::Ufw, deadline)?;
        let facts = snapshot.facts();
        let active = match facts.activity {
            Activity::Active => Some(true),
            Activity::Inactive => Some(false),
            Activity::Unknown => None,
        };
        let link = facts.links.as_ref().ok().and_then(|links| {
            let preferred: Vec<_> = links.iter().filter(|l| l.default_route).collect();
            let chosen = if preferred.len() == 1 {
                Some(preferred[0])
            } else if links.len() == 1 {
                Some(&links[0])
            } else {
                None
            };
            chosen.map(|l| format!("{} on {}", l.cidr.as_str(), l.interface))
        });
        let lan_rule = match facts
            .links
            .as_ref()
            .ok()
            .and_then(|links| {
                let preferred: Vec<_> = links.iter().filter(|l| l.default_route).collect();
                if preferred.len() == 1 {
                    Some(preferred[0].clone())
                } else if links.len() == 1 {
                    Some(links[0].clone())
                } else {
                    None
                }
            })
            .map(|l| facts.presence(&l.cidr, RuleKind::Lan))
        {
            Some(Presence::Owned | Presence::Equivalent) => RulePresence::Present,
            Some(Presence::Absent) => RulePresence::Absent,
            Some(Presence::Modified) => RulePresence::Modified,
            Some(Presence::Unknown(_)) | None => RulePresence::Unknown,
        };
        Ok(FirewallReading {
            active,
            lan_rule,
            link,
        })
    }

    fn plan_lan(
        &mut self,
        proof: &SupportProof,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<String, FirewallError> {
        self.planned = None;
        if self.store.is_none() {
            self.store = Some(DurableIntentStore::open(&mut self.firewall, proof)?);
        }
        let snapshot = self.firewall.detect(ManagerSelection::Ufw, deadline)?;
        let plan = self.firewall.plan(
            &snapshot,
            PlanRequest {
                operation,
                revision: operation.0,
                kind: RuleKind::Lan,
                selected: None,
                ports: [47811, 47812],
            },
        )?;
        let preview = plan.preview();
        self.planned = Some((operation, plan));
        Ok(preview)
    }

    fn apply_lan(
        &mut self,
        proof: &SupportProof,
        operation: OperationId,
        deadline: &Deadline,
    ) -> Result<RuleApply, FirewallError> {
        let Some((planned, plan)) = self.planned.take() else {
            return Err(FirewallError::Stale);
        };
        if planned != operation {
            return Err(FirewallError::Stale);
        }
        let consent = plan.consent(operation, operation.0)?;
        let store = self.store.as_mut().ok_or(FirewallError::Stale)?;
        let result =
            self.firewall
                .apply(proof, ManagerSelection::Ufw, plan, consent, store, deadline)?;
        Ok(match result.result {
            RuleResult::PendingVerification => RuleApply::Dispatched,
            RuleResult::Kept => RuleApply::AlreadyPresent,
            RuleResult::NotDispatched => RuleApply::NotDispatched,
            RuleResult::PromptUnavailable => RuleApply::NoPromptAgent(result.manual),
            RuleResult::OutcomeUnknown => RuleApply::Unknown,
            RuleResult::Absent => RuleApply::Failed,
        })
    }

    fn receipt_lan(&mut self, proof: &SupportProof, operation: OperationId) -> Option<Vec<u8>> {
        self.store.as_ref()?.receipt(proof, operation).ok()
    }
}

/// Stands in when the install paths themselves can't be admitted: nothing is ever touched.
pub struct BrokenPayloads;

impl Payloads for BrokenPayloads {
    fn detect(
        &mut self,
        _: &SupportProof,
        _: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        Err(PayloadError::Foreign)
    }
    fn observe(
        &mut self,
        _: &SupportProof,
        _: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        Err(PayloadError::Foreign)
    }
    fn plan(
        &mut self,
        _: &SupportProof,
        _: &Package,
        _: OperationId,
        _: bool,
    ) -> Result<PayloadPreview, PayloadError> {
        Err(PayloadError::Foreign)
    }
    fn apply(
        &mut self,
        _: &SupportProof,
        _: &Package,
        _: OperationId,
        _: &Deadline,
    ) -> Result<(), PayloadError> {
        Err(PayloadError::Foreign)
    }
    fn verify(
        &mut self,
        _: &SupportProof,
        _: &Package,
        _: &AgentReply,
        _: u64,
        _: &Deadline,
    ) -> Result<(), PayloadError> {
        Err(PayloadError::Foreign)
    }
}

/// Stands in when removal can't be offered at all, saying why.
pub struct NoUninstaller {
    pub reason: String,
}

impl Uninstaller for NoUninstaller {
    fn inspect(&mut self, _: Option<&Package>, _: &Deadline) -> UninstallOffer {
        UninstallOffer {
            uninstall: Availability::Unavailable(self.reason.clone()),
            choices: Vec::new(),
        }
    }
    fn plan(
        &mut self,
        _: Option<&Package>,
        _: &[(u16, bool)],
        _: OperationId,
        _: &Deadline,
    ) -> Result<String, String> {
        Err(self.reason.clone())
    }
    fn begin(&mut self, _: Option<&Package>, _: OperationId) -> Result<(), String> {
        Err(self.reason.clone())
    }
    fn advance(&mut self, _: Option<&Package>) -> UninstallProgress {
        UninstallProgress::Finished(
            crate::live::MaintenanceOutcome::Refused,
            vec![self.reason.clone()],
        )
    }
    fn follow_up(&mut self, _: Option<&Package>, _: u16, _: bool) -> UninstallProgress {
        UninstallProgress::Finished(
            crate::live::MaintenanceOutcome::Refused,
            vec![self.reason.clone()],
        )
    }
}

/// Stands in when repair can't be offered at all, saying why. Nothing is ever touched.
pub struct NoRepairer {
    pub reason: String,
}

impl Repairer for NoRepairer {
    fn inspect(&mut self, _: Option<&Package>, _: u64) -> RepairOffer {
        RepairOffer {
            discardable: false,
            repair: Availability::Unavailable(self.reason.clone()),
            resumable: None,
        }
    }
    fn plan(
        &mut self,
        _: Option<&Package>,
        _: Option<&AgentReply>,
        _: OperationId,
        _: u64,
    ) -> Result<String, String> {
        Err(self.reason.clone())
    }
    fn confirm(
        &mut self,
        _: Option<&Package>,
        _: Option<&AgentReply>,
        _: OperationId,
        _: OperationId,
        _: u64,
    ) -> Result<RepairStep, String> {
        Err(self.reason.clone())
    }
    fn verify(&mut self, _: Option<&Package>, _: Option<&AgentReply>, _: u64) -> RepairStep {
        RepairStep::Finished(RepairFinish {
            outcome: crate::live::RepairOutcome::OutcomeUnknown,
            lines: vec![self.reason.clone()],
            resumable: false,
        })
    }
    fn resume(
        &mut self,
        _: Option<&Package>,
        _: Option<&AgentReply>,
        _: u64,
    ) -> Result<RepairFinish, String> {
        Err(self.reason.clone())
    }
}

opaque_debug!(
    Domains,
    NativeSupport,
    NativePayloads,
    NativeServices,
    NativeFirewalls,
    NoUninstaller,
    NoRepairer,
    BrokenPayloads,
);

#[cfg(test)]
mod checklist_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use std::path::PathBuf;

    use super::super::super::detect::{
        Architecture, EffectiveEnvironment, LibraryFact, SelectedSession, SessionCandidate,
        SessionFacts, SessionSelection, classify,
    };
    use super::*;

    const LIVE: ObservationSource = ObservationSource::Live;

    fn ok<T>(value: T) -> Fact<T> {
        Fact::known(value, LIVE, 1)
    }

    fn env() -> EffectiveEnvironment {
        EffectiveEnvironment {
            runtime_dir: PathBuf::from("/run/user/1000"),
            wayland_display: "wayland-1".into(),
            hyprland_instance_signature: "sig".into(),
            session_id: Some("2".into()),
            xdg_current_desktop: None,
            xdg_session_type: None,
        }
    }

    /// A GNOME session's environment: no Hyprland signature, the two variables the agent reads.
    fn gnome_env() -> EffectiveEnvironment {
        EffectiveEnvironment {
            runtime_dir: PathBuf::from("/run/user/1000"),
            wayland_display: "wayland-0".into(),
            hyprland_instance_signature: String::new(),
            session_id: Some("2".into()),
            xdg_current_desktop: Some("GNOME".into()),
            xdg_session_type: Some("wayland".into()),
        }
    }

    fn library(name: &str) -> LibraryFact {
        LibraryFact {
            name: name.into(),
            required: true,
            resolved: ok(PathBuf::from(format!("/usr/lib/{name}"))),
        }
    }

    /// A report every check passes.
    fn supported() -> SupportReport {
        let session = SessionFacts {
            uid: 1000,
            os: ok(OsFamily::Arch),
            architecture: ok(Architecture::X86_64),
            desktop: Ok(Desktop::Hyprland),
            compositor_version: ok([0, 56, 2]),
            protocols: ok(true),
            compositor_managed: ok(true),
            graphical_target_active: ok(true),
            graphical_sessions: ok(1),
            selected_session: ok(Some(SelectedSession {
                selection: SessionSelection::Environment,
                session: SessionCandidate {
                    id: "2".into(),
                    path: "/org/freedesktop/login1/session/_32".into(),
                    kind: Some("wayland".into()),
                    uid: Some(1000),
                    seat: Some("seat0".into()),
                    active: Some(true),
                    locked_hint: Some(false),
                },
            })),
            selected_environment: env(),
            manager_environment: ok(env()),
        };
        let unverified = || Fact::issue(ProbeIssue::Unverified, LIVE, 1);
        let runtime = RuntimeFacts {
            dependency_graph: ok(true),
            libraries: vec![library("libopus.so.0"), library("libxkbcommon.so.0")],
            video_feature: ok(true),
            ffmpeg: ok(true),
            opus: ok(true),
            pipewire_library: ok(true),
            xkb: ok(true),
            wayland_library: ok(true),
            software_video: ok(true),
            gpu: unverified(),
            libei_required: false,
            pipewire: unverified(),
            session_manager: unverified(),
            secret_service: unverified(),
            keystore: Fact::issue(ProbeIssue::Unverified, LIVE, 1),
        };
        SupportReport {
            eligibility: classify(&session, &runtime),
            session,
            runtime,
            installed_agent: Fact::issue(ProbeIssue::Unverified, LIVE, 1),
            reduced_motion: Fact::issue(ProbeIssue::Unverified, LIVE, 1),
        }
    }

    fn reclassify(mut report: SupportReport) -> SupportReport {
        report.eligibility = classify(&report.session, &report.runtime);
        report
    }

    /// The labels of every row that didn't pass, with their states.
    fn not_passed(report: &SupportReport) -> Vec<(String, CheckState)> {
        support_checks(report)
            .into_iter()
            .filter(|c| !matches!(c.state, CheckState::Passed(_)))
            .map(|c| (c.label, c.state))
            .collect()
    }

    #[test]
    fn a_supported_report_passes_every_row_in_order() {
        let report = supported();
        assert_eq!(report.eligibility, Eligibility::Supported);
        let checks = support_checks(&report);
        assert_eq!(
            checks.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
            LINUX_CHECKS.to_vec()
        );
        assert!(not_passed(&report).is_empty(), "{checks:?}");
        assert_eq!(
            checks[2].state,
            CheckState::Passed(Some("Hyprland 0.56.2".into()))
        );
        assert_eq!(checks[7].state, CheckState::Passed(Some("2 found".into())));
    }

    #[test]
    fn each_established_negative_fails_exactly_its_row_with_a_reason() {
        type Edit = fn(&mut SupportReport);
        let cases: [(Edit, &str, &str); 9] = [
            (
                |r| r.session.os = ok(OsFamily::Other("fedora".into())),
                "Operating system",
                "Arch-based Linux",
            ),
            (
                |r| r.session.architecture = ok(Architecture::Other("riscv64".into())),
                "Processor",
                "processor isn't supported",
            ),
            (
                |r| r.session.compositor_version = ok([0, 55, 1]),
                "Hyprland version",
                "Hyprland 0.55.1 is older than 0.56",
            ),
            (
                |r| r.session.protocols = ok(false),
                "Wayland protocols",
                "Wayland features",
            ),
            (
                |r| r.session.compositor_managed = ok(false),
                "uwsm session",
                "managed by uwsm",
            ),
            (
                |r| {
                    if let Ok(Some(s)) = r.session.selected_session.value.as_mut() {
                        s.session.kind = Some("x11".into());
                    }
                },
                "This session is the signed-in one",
                "Wayland session",
            ),
            (
                |r| r.runtime.libraries[0].resolved = Fact::issue(ProbeIssue::Missing, LIVE, 1),
                "Required libraries",
                "libopus.so.0 isn't installed",
            ),
            (
                |r| r.runtime.xkb = ok(false),
                "Required libraries",
                "xkbcommon isn't installed",
            ),
            (
                |r| r.runtime.video_feature = ok(false),
                "Video support in the payload",
                "doesn't include video support",
            ),
        ];
        for (edit, label, reason) in cases {
            let mut report = supported();
            edit(&mut report);
            let report = reclassify(report);
            assert!(
                matches!(report.eligibility, Eligibility::NotSupported(_)),
                "{label}: {:?}",
                report.eligibility
            );
            let rows = not_passed(&report);
            assert_eq!(rows.len(), 1, "{label}: {rows:?}");
            assert_eq!(rows[0].0, label);
            match &rows[0].1 {
                CheckState::Failed(text) => assert!(text.contains(reason), "{label}: {text}"),
                other => panic!("{label}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_pending_report_shows_exactly_the_blocking_rows() {
        type Edit = fn(&mut SupportReport);
        let cases: [(Edit, &str, &str); 9] = [
            (
                |r| r.session.manager_environment = Fact::issue(ProbeIssue::Unavailable, LIVE, 1),
                "This session is the signed-in one",
                "couldn't read the session environment",
            ),
            (
                |r| {
                    let mut other = env();
                    other.wayland_display = "wayland-9".into();
                    r.session.manager_environment = ok(other);
                },
                "This session is the signed-in one",
                "doesn't match this session",
            ),
            (
                |r| r.session.selected_session = ok(None),
                "This session is the signed-in one",
                "more than one graphical session",
            ),
            (
                |r| r.session.graphical_sessions = ok(2),
                "This session is the signed-in one",
                "2 graphical sessions are open",
            ),
            (
                |r| r.session.graphical_target_active = ok(false),
                "Graphical session active",
                "isn't active yet",
            ),
            (
                |r| {
                    if let Ok(Some(s)) = r.session.selected_session.value.as_mut() {
                        s.session.active = Some(false);
                    }
                },
                "Graphical session active",
                "isn't shown as the active one",
            ),
            (
                |r| r.session.compositor_version = Fact::issue(ProbeIssue::Timeout, LIVE, 1),
                "Hyprland version",
                "reading the Hyprland version took too long",
            ),
            (
                |r| r.runtime.libraries.clear(),
                "Required libraries",
                "staged payload",
            ),
            (
                |r| r.session.compositor_managed = Fact::issue(ProbeIssue::Malformed, LIVE, 1),
                "uwsm session",
                "couldn't be understood",
            ),
        ];
        for (edit, label, issue) in cases {
            let mut report = supported();
            edit(&mut report);
            let report = reclassify(report);
            assert!(
                matches!(report.eligibility, Eligibility::Pending(_)),
                "{label}: {:?}",
                report.eligibility
            );
            let rows = not_passed(&report);
            assert_eq!(rows.len(), 1, "{label}: {rows:?}");
            assert_eq!(rows[0].0, label);
            match &rows[0].1 {
                CheckState::Unconfirmed(text) | CheckState::Note(text) => {
                    assert!(text.contains(issue), "{label}: {text}")
                }
                other => panic!("{label}: {other:?}"),
            }
        }
    }

    #[test]
    fn without_a_payload_only_the_runtime_rows_are_unconfirmed() {
        let mut report = supported();
        let unverified = || Fact::issue(ProbeIssue::Unverified, LIVE, 1);
        report.runtime = RuntimeFacts {
            dependency_graph: unverified(),
            libraries: Vec::new(),
            video_feature: unverified(),
            ffmpeg: unverified(),
            opus: unverified(),
            pipewire_library: unverified(),
            xkb: unverified(),
            wayland_library: unverified(),
            software_video: unverified(),
            gpu: unverified(),
            libei_required: false,
            pipewire: unverified(),
            session_manager: unverified(),
            secret_service: unverified(),
            keystore: Fact::issue(ProbeIssue::Unverified, LIVE, 1),
        };
        let report = reclassify(report);
        let labels: Vec<String> = not_passed(&report).into_iter().map(|r| r.0).collect();
        assert_eq!(
            labels,
            vec!["Required libraries", "Video support in the payload"]
        );
    }

    #[test]
    fn a_pass_that_cannot_start_leaves_every_row_unconfirmed() {
        let rows = unchecked_support("this session's checks couldn't start");
        assert_eq!(rows.len(), LINUX_CHECKS.len());
        assert!(
            rows.iter().all(|r| r.state
                == CheckState::Unconfirmed("this session's checks couldn't start".into()))
        );
    }

    /// The supported Hyprland report, turned into a GNOME or KDE one.
    fn portal(desktop: Desktop, version: Option<[u16; 3]>) -> SupportReport {
        let mut report = supported();
        report.session.desktop = Ok(desktop);
        report.session.compositor_version = match version {
            Some(v) => ok(v),
            None => Fact::issue(ProbeIssue::Unverified, LIVE, 1),
        };
        report.session.selected_environment = gnome_env();
        report.session.manager_environment = ok(gnome_env());
        reclassify(report)
    }

    #[test]
    fn gnome_and_kde_pass_with_their_own_row_names_and_hyprland_keeps_the_original_ones() {
        let rows = |report: &SupportReport| -> Vec<String> {
            support_checks(report)
                .into_iter()
                .map(|c| c.label)
                .collect()
        };
        assert_eq!(rows(&supported()), LINUX_CHECKS.to_vec());
        let gnome = portal(Desktop::Gnome, Some([50, 4, 0]));
        assert_eq!(gnome.eligibility, Eligibility::Supported);
        assert_eq!(rows(&gnome), GNOME_CHECKS.to_vec());
        assert!(not_passed(&gnome).is_empty(), "{:?}", not_passed(&gnome));
        let kde = portal(Desktop::Kde, None);
        assert_eq!(kde.eligibility, Eligibility::Supported);
        assert_eq!(rows(&kde), KDE_CHECKS.to_vec());
        // An unknown Plasma version is a note: setup can continue.
        let flagged = not_passed(&kde);
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert_eq!(flagged[0].0, "Plasma version");
        assert!(
            matches!(flagged[0].1, CheckState::Note(_)),
            "{:?}",
            flagged[0].1
        );
        // Nothing in the GNOME or KDE rows talks about Hyprland or uwsm.
        for label in GNOME_CHECKS.iter().chain(&KDE_CHECKS) {
            assert!(
                !label.contains("Hyprland") && !label.contains("uwsm"),
                "{label}"
            );
        }
    }

    #[test]
    fn an_old_gnome_shell_is_a_note_not_a_refusal_and_gnome_failures_speak_of_gnome() {
        let old = portal(Desktop::Gnome, Some([47, 2, 0]));
        assert_eq!(old.eligibility, Eligibility::Supported);
        let flagged = not_passed(&old);
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        match &flagged[0].1 {
            CheckState::Note(text) => {
                assert!(text.contains("47.2") && text.contains("without its Shell extension"))
            }
            other => panic!("{other:?}"),
        }
        let mut unmanaged = portal(Desktop::Gnome, Some([50, 4, 0]));
        unmanaged.session.compositor_managed = ok(false);
        let unmanaged = reclassify(unmanaged);
        assert_eq!(
            unmanaged.eligibility,
            Eligibility::NotSupported(UnsupportedReason::SessionManager)
        );
        let flagged = not_passed(&unmanaged);
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert_eq!(flagged[0].0, "GNOME session");
        match &flagged[0].1 {
            CheckState::Failed(text) => {
                assert!(text.contains("GNOME") && !text.contains("uwsm"), "{text}")
            }
            other => panic!("{other:?}"),
        }
        let mut protocols = portal(Desktop::Kde, None);
        protocols.session.protocols = ok(false);
        let flagged = not_passed(&reclassify(protocols));
        assert!(
            flagged
                .iter()
                .any(|(label, state)| label == "Wayland protocols"
                    && matches!(state, CheckState::Failed(text) if text.contains("KDE Plasma")
                    && !text.contains("Hyprland"))),
            "{flagged:?}"
        );
    }

    #[test]
    fn a_manager_environment_for_another_desktop_is_unconfirmed_not_matched() {
        let mut gnome = portal(Desktop::Gnome, Some([50, 4, 0]));
        let mut other = gnome_env();
        other.xdg_current_desktop = Some("KDE".into());
        gnome.session.manager_environment = ok(other);
        let gnome = reclassify(gnome);
        assert_eq!(gnome.eligibility, Eligibility::Pending(ProbeIssue::Foreign));
        let flagged = not_passed(&gnome);
        assert_eq!(flagged.len(), 1, "{flagged:?}");
        assert_eq!(flagged[0].0, "This session is the signed-in one");
        assert!(matches!(flagged[0].1, CheckState::Unconfirmed(_)));
    }

    #[test]
    fn an_unsupported_desktop_has_its_own_rows_and_one_plain_refusal() {
        for reason in [UnsupportedReason::Desktop, UnsupportedReason::SessionType] {
            let mut report = supported();
            report.session.desktop = Err(reason);
            let report = reclassify(report);
            assert_eq!(report.eligibility, Eligibility::NotSupported(reason));
            let rows = support_checks(&report);
            assert_eq!(
                rows.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(),
                OTHER_DESKTOP_CHECKS.to_vec()
            );
            let desktop_row = &rows[2];
            assert_eq!(desktop_row.label, "Desktop");
            match &desktop_row.state {
                CheckState::Failed(text) => assert_eq!(text, &detect::unsupported_text(reason)),
                other => panic!("{other:?}"),
            }
        }
    }
}
