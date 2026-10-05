//! The native domains the Linux worker drives, as small traits with thin production adapters.
//!
//! Every production adapter wraps one merged native module and adds no policy: it translates the
//! module's typed results into the worker's vocabulary. The worker (see `worker.rs`) holds the
//! policy that decides what each result means for a step.

use std::sync::Arc;

use crosspane_installer_core::{ObservationSource, OperationId, ResourceReceipt};

use crate::agent_contract::AgentReply;
use crate::live::{Availability, CheckState, SupportCheck, SupportChecksSlot};

use super::super::detect::{
    self, Eligibility, Fact, NativeSessionProbes, OsFamily, ProbeIssue, RuntimeFacts,
    SupportReport, UnsupportedReason, runtime::RuntimeInput,
};
use super::super::firewall::{
    Activity, FirewallError, FirewallPlan, LinuxFirewall, ManagerSelection, PlanRequest, Presence,
    RuleKind, RuleResult, receipts::DurableIntentStore,
};
use super::super::native_io::{
    ChildEnvironment, Deadline, LinuxNativeIo, MAX_ELF_PREFIX_BYTES, SupportProof,
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadPreview {
    pub version: String,
    pub resuming: bool,
}

/// The user-manager unit for the installed agent.
pub trait Services {
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

/// The checklist rows, in the order they are shown.
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
fn yes_no(fact: &Fact<bool>, what: &str, reason: UnsupportedReason) -> CheckState {
    match fact.value {
        Ok(true) => CheckState::Passed(None),
        Ok(false) => CheckState::Failed(unsupported_text(reason)),
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
    match &session.manager_environment.value {
        Ok(effective)
            if effective.runtime_dir == session.selected_environment.runtime_dir
                && effective.wayland_display == session.selected_environment.wayland_display
                && effective.hyprland_instance_signature
                    == session.selected_environment.hyprland_instance_signature => {}
        Ok(_) => parts.push(CheckState::Unconfirmed(
            "the session manager's environment doesn't match this session".into(),
        )),
        Err(issue) => parts.push(unconfirmed("the session environment", *issue)),
    }
    worst(parts, None)
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
    let hyprland = match session.hyprland_version.value {
        Ok(v) if v >= [0, 56, 0] => {
            CheckState::Passed(Some(format!("Hyprland {}.{}.{}", v[0], v[1], v[2])))
        }
        Ok(v) => CheckState::Failed(format!(
            "Hyprland {}.{}.{} is older than 0.56, which Crosspane needs",
            v[0], v[1], v[2]
        )),
        Err(issue) => unconfirmed("the Hyprland version", issue),
    };
    let states = [
        advisory(os),
        advisory(processor),
        advisory(hyprland),
        advisory(yes_no(
            &session.protocols,
            "Hyprland's Wayland protocols",
            UnsupportedReason::RequiredProtocols,
        )),
        yes_no(
            &session.uwsm_managed,
            "how this session is managed",
            UnsupportedReason::Uwsm,
        ),
        graphical_session_active(report),
        signed_in_session(report),
        advisory(required_libraries(&report.runtime)),
        advisory(yes_no(
            &report.runtime.video_feature,
            "the staged payload's features",
            UnsupportedReason::VideoFeature,
        )),
    ];
    LINUX_CHECKS
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
            Err(_) => rows.push(SupportCheck::new("Crosspane runtime", CheckState::Unconfirmed("Runtime state is active or couldn't be proved safe to recover; it will be retained".into()))),
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
                unsupported_text(reason)
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
    held: Option<(OperationId, PayloadPlan)>,
}

impl NativePayloads {
    pub fn new(io: Arc<LinuxNativeIo>) -> Result<Self, PayloadError> {
        Ok(Self {
            installer: PayloadInstaller::new(io)?,
            held: None,
        })
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
        self.installer.detect(proof, package)
    }

    fn observe(
        &mut self,
        proof: &SupportProof,
        package: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        self.installer.detect(proof, package)
    }

    fn plan(
        &mut self,
        proof: &SupportProof,
        package: &Package,
        operation: OperationId,
        resume: bool,
    ) -> Result<PayloadPreview, PayloadError> {
        self.held = None;
        let plan = if resume {
            self.installer.resume_plan(proof, package)?
        } else {
            self.installer
                .plan(proof, package, operation, MatchingFiles::Preserve)?
        };
        let preview = PayloadPreview {
            version: plan.receipt().product_version.clone(),
            resuming: resume,
        };
        self.held = Some((operation, plan));
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
        let Some((planned, plan)) = self.held.take() else {
            return Err(PayloadError::Pending);
        };
        if planned != operation {
            return Err(PayloadError::Pending);
        }
        self.installer
            .apply(proof, package, plan, deadline)
            .map(|_| ())
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
}

/// The real user-manager unit, bound to the staged package's rendered files.
pub struct NativeServices {
    pub io: Arc<LinuxNativeIo>,
    pub env: ChildEnvironment,
    service: Option<Arc<LinuxService>>,
}

impl NativeServices {
    pub fn new(io: Arc<LinuxNativeIo>, env: ChildEnvironment) -> Self {
        Self {
            io,
            env,
            service: None,
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
        let plan = service.plan(proof, action, deadline)?;
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
            hyprland_version: ok([0, 56, 2]),
            protocols: ok(true),
            uwsm_managed: ok(true),
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
                |r| r.session.hyprland_version = ok([0, 55, 1]),
                "Hyprland version",
                "Hyprland 0.55.1 is older than 0.56",
            ),
            (
                |r| r.session.protocols = ok(false),
                "Wayland protocols",
                "Wayland features",
            ),
            (
                |r| r.session.uwsm_managed = ok(false),
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
                |r| r.session.hyprland_version = Fact::issue(ProbeIssue::Timeout, LIVE, 1),
                "Hyprland version",
                "reading the Hyprland version took too long",
            ),
            (
                |r| r.runtime.libraries.clear(),
                "Required libraries",
                "staged payload",
            ),
            (
                |r| r.session.uwsm_managed = Fact::issue(ProbeIssue::Malformed, LIVE, 1),
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
}
