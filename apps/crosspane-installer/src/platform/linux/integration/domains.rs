//! The native domains the Linux worker drives, as small traits with thin production adapters.
//!
//! Every production adapter wraps one merged native module and adds no policy: it translates the
//! module's typed results into the worker's vocabulary. The worker (see `worker.rs`) holds the
//! policy that decides what each result means for a step.

use std::sync::Arc;

use crosspane_installer_core::{ObservationSource, OperationId, ResourceReceipt};

use crate::agent_contract::AgentReply;
use crate::live::Availability;

use super::super::detect::{
    self, Eligibility, NativeSessionProbes, ProbeIssue, RuntimeFacts, UnsupportedReason,
    runtime::RuntimeInput,
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
    match reason {
        UnsupportedReason::OperatingSystem => {
            "This installer supports Arch-based Linux, such as Omarchy, for now.".into()
        }
        UnsupportedReason::Architecture => {
            "This processor isn't supported by this installer yet.".into()
        }
        UnsupportedReason::HyprlandVersion => {
            "Crosspane needs Hyprland 0.56 or newer on this computer.".into()
        }
        UnsupportedReason::RequiredProtocols => {
            "This Hyprland session doesn't offer the Wayland features Crosspane needs.".into()
        }
        UnsupportedReason::Uwsm => {
            "Setup supports Hyprland sessions managed by uwsm for now. Yours isn't.".into()
        }
        UnsupportedReason::SessionType => {
            "Crosspane needs a Wayland session. This one is something else.".into()
        }
        UnsupportedReason::VideoFeature => {
            "The staged Crosspane build doesn't include video support.".into()
        }
        UnsupportedReason::RuntimeLibrary => {
            "A library Crosspane needs isn't installed on this computer.".into()
        }
    }
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

/// The real read-only session and runtime detection.
pub struct NativeSupport {
    pub io: Arc<LinuxNativeIo>,
    pub env: ChildEnvironment,
    pub clock: CallerClock,
}

impl NativeSupport {
    fn runtime_facts(&self, package: Option<&Package>, deadline: &Deadline) -> RuntimeFacts {
        let source = self.io.target().source();
        let now = (self.clock)();
        let unverified = || detect::Fact::issue(ProbeIssue::Unverified, source, now);
        let empty = RuntimeFacts {
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
        let prefix = package.agent_elf_prefix();
        if prefix.is_empty() || prefix.len() > MAX_ELF_PREFIX_BYTES {
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
                agent_elf_prefix: prefix,
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
                    return SupportOutcome::Pending(
                        "This session couldn't be checked. Nothing will be changed.".into(),
                    );
                }
            };
        let runtime = self.runtime_facts(package, deadline);
        let result = probes.detect(runtime, deadline);
        match result.report.eligibility {
            Eligibility::Supported => match result.proof {
                Some(proof) => SupportOutcome::Supported(proof),
                None => SupportOutcome::Pending(
                    "Support was observed but couldn't be admitted. Nothing will be changed."
                        .into(),
                ),
            },
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
            super::session_of(&self.env),
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
