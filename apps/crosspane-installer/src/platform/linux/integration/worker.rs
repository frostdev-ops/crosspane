//! The Linux worker: one background thread that maps core's job stages onto the native domains.
//!
//! Policy lives here and nowhere else in the Linux binding: when a result means "needs action",
//! "waiting", "unknown, detect again" or "refused", which fresh proof each stage takes, and how a
//! consent is tied to the exact plan that was previewed. Every adapter call is bounded by a
//! deadline; nothing on this thread is ever waited for by the GUI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crosspane_installer_core::{
    ApplyOutcome, JobIntent, JobStage, ObservationSource, OperationId, ResourceObservation,
    ResourceOwnership, WaitKind,
};

use super::super::firewall::FirewallError;
use super::super::native_io::{
    Cancellation, Deadline, LinuxNativeIo, MAX_NATIVE_TIMEOUT_MS, NativeError, SupportProof,
};
use super::super::payload::{Package, PayloadError};
use super::super::service::{AgentEvidence, ServiceAction, ServiceError};
use super::domains::{
    DomainFactory, Firewalls, Payloads, RepairFinish, RepairStep, Repairer, RuleApply,
    RulePresence, Services, Support, SupportOutcome, UninstallProgress, Uninstaller,
};
use super::ports::Command;
use super::resume;
use crate::agent_contract::{
    AgentReply, DecodedReply, HealthSnapshot, KeyStoreProvenance, StartupRecovery, StatusAdmission,
};
use crate::live::{
    Consent, MaintenanceId, MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome,
    NativeReport, StatusEvidence, StepReport,
};

use super::{AGENT, NETWORK, PAYLOAD, RESTART, SERVICE, SUPPORT};

/// A bounded deadline for ordinary reads and the short single mutations.
const READ_MS: u64 = 8_000;
const APPLY_MS: u64 = 30_000;
const AGENT_MS: u64 = 8_000;
/// A removal has a handful of stages. A run that keeps reporting progress past this many steps
/// without settling is stuck, and is stopped with its honest report instead of spinning.
const MAX_DRIVE_STEPS: u32 = 64;
/// Maintenance operations live far above core operation ids (which start at 1 in each session)
/// and are spaced so the removal adapter's own rule operations, just above each run's operation,
/// never meet the next one.
const MAINTENANCE_OPS: u64 = 500_000;
const MAINTENANCE_SPACING: u64 = 1_000;
/// How long a repair that started the new agent waits for it to report healthy before it ends
/// with an honest "outcome unknown, resume required".
const REPAIR_HEALTH_WAIT_MS: u64 = 90_000;

type ProofResults = Arc<Mutex<BTreeMap<u64, Result<SupportProof, String>>>>;

pub struct WorkerParts {
    pub io: Arc<LinuxNativeIo>,
    pub clock: crate::live::Clock,
    pub payload_dir: Option<PathBuf>,
    pub support: Arc<dyn Support>,
    /// Built on the worker thread, which then owns every domain.
    pub domains: DomainFactory,
    pub reports: SyncSender<NativeReport>,
    pub results: ProofResults,
    pub tutorial_hash: Arc<Mutex<Option<[u8; 32]>>>,
    pub stop: Arc<Cancellation>,
    /// Package source for tests; production reads the staged directory.
    pub package: Option<Package>,
}

pub struct Worker {
    expired: Deadline,
    io: Arc<LinuxNativeIo>,
    clock: crate::live::Clock,
    payload_dir: Option<PathBuf>,
    package: Option<Package>,
    package_error: Option<String>,
    support: Arc<dyn Support>,
    payloads: Box<dyn Payloads>,
    services: Box<dyn Services>,
    firewalls: Box<dyn Firewalls>,
    uninstaller: Box<dyn Uninstaller>,
    repairer: Box<dyn Repairer>,
    reports: SyncSender<NativeReport>,
    results: ProofResults,
    tutorial_hash: Arc<Mutex<Option<[u8; 32]>>>,
    stop: Arc<Cancellation>,
    /// Firewall and cleanup intents persist across runs, so their ids never restart at 1.
    op_base: u64,
    held: Held,
    maintenance: Maintenance,
}

#[derive(Default)]
struct Held {
    /// Plan operations the next Apply must name.
    payload: Option<OperationId>,
    service: Option<(OperationId, Vec<ServiceAction>)>,
    restart: Option<OperationId>,
    network: Option<OperationId>,
    /// The instance that was running before the restart this session applied.
    restarted: Option<Option<u64>>,
}

#[derive(Default)]
struct Maintenance {
    id: Option<MaintenanceId>,
    planned: Option<MaintenanceId>,
    running: bool,
    /// The number of the repair preview the next confirmation must name.
    repair_plan: Option<OperationId>,
    /// A confirmed repair that hasn't ended: it is changing things, or watching the new agent.
    repair_active: bool,
    discard_offered: bool,
    /// When the repair first waited for the new agent's health.
    repair_since: Option<u64>,
    /// Maintenance operations handed out in this session.
    issued: u64,
}

/// What a stage reports when a gate refuses it.
type Stop = (NativeOutcome, String);

fn gate(stage: JobStage, unsupported: bool, text: String) -> Stop {
    let outcome = match (stage, unsupported) {
        (JobStage::Apply, _) => NativeOutcome::Applied(ApplyOutcome::Refused),
        (_, true) => NativeOutcome::Unsupported,
        (_, false) => NativeOutcome::Waiting(WaitKind::Contract),
    };
    (outcome, text)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX / 4096)) as u64)
}

fn status_ok(reply: &AgentReply) -> Option<&HealthSnapshot> {
    match &reply.result {
        Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => Some(health),
        _ => None,
    }
}

fn peer_connected(reply: &AgentReply) -> bool {
    status_ok(reply).is_some_and(|h| h.installer().peers.iter().any(|p| p.connected))
}

impl Worker {
    pub fn new(parts: WorkerParts) -> Result<Self, NativeError> {
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let expired = Deadline::new(1, cancelled)?;
        let domains = (parts.domains)();
        Ok(Self {
            expired,
            io: parts.io,
            clock: parts.clock,
            payload_dir: parts.payload_dir,
            package: parts.package,
            package_error: None,
            support: parts.support,
            payloads: domains.payloads,
            services: domains.services,
            firewalls: domains.firewalls,
            uninstaller: domains.uninstaller,
            repairer: domains.repairer,
            reports: parts.reports,
            results: parts.results,
            tutorial_hash: parts.tutorial_hash,
            stop: parts.stop,
            op_base: unix_ms().saturating_mul(1000),
            held: Held::default(),
            maintenance: Maintenance::default(),
        })
    }

    pub fn run(mut self, commands: Receiver<Command>) {
        self.load_package();
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            match commands.recv_timeout(Duration::from_millis(20)) {
                Ok(Command::Job(job)) => self.handle(job),
                Ok(Command::Proof(id)) => self.mint(id),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    // ---- plumbing ----------------------------------------------------------------------------

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn source(&self) -> ObservationSource {
        self.support.source()
    }

    fn deadline(&self, ms: u64) -> Deadline {
        // The bound is clamped into the valid range, so this can't fail; if it ever did, the
        // already-expired deadline makes every native call refuse safely.
        Deadline::new(ms.clamp(1, MAX_NATIVE_TIMEOUT_MS), (*self.stop).clone())
            .unwrap_or_else(|_| self.expired.clone())
    }

    fn emit(&self, job: JobIntent, outcome: NativeOutcome, detail: impl Into<String>) {
        let _ = self.reports.send(NativeReport::Step(StepReport {
            job,
            outcome,
            detail: detail.into(),
        }));
    }

    fn emit_stop(&self, job: JobIntent, stop: Stop) {
        self.emit(job, stop.0, stop.1);
    }

    fn maint(&self, report: MaintenanceReport) {
        let _ = self.reports.send(NativeReport::Maintenance(report));
    }

    fn verified(&self) -> NativeOutcome {
        NativeOutcome::Verified {
            source: self.source(),
            observed_at_ms: self.now(),
        }
    }

    fn firewall_op(&self, operation: OperationId) -> OperationId {
        OperationId(self.op_base.saturating_add(operation.0))
    }

    fn load_package(&mut self) {
        if self.package.is_some() {
            self.note_tutorial_hash();
            return;
        }
        match &self.payload_dir {
            None => {
                self.package_error = Some(
                    "No staged payload was given. Start setup with --payload <folder> holding \
                     payload.tar and payload.sha256. Nothing will be changed."
                        .into(),
                );
            }
            Some(dir) => match super::read_package(dir) {
                Ok(package) => {
                    self.package = Some(package);
                    self.note_tutorial_hash();
                }
                Err(text) => self.package_error = Some(text),
            },
        }
    }

    fn note_tutorial_hash(&mut self) {
        let hash = self.package.as_ref().and_then(|package| {
            package
                .manifest()
                .members
                .iter()
                .find(|m| m.name == "bin/crosspane-tutorial")
                .and_then(|m| super::hex_hash(&m.sha256).ok())
        });
        if let (Some(hash), Ok(mut slot)) = (hash, self.tutorial_hash.lock()) {
            *slot = Some(hash);
        }
    }

    fn package(&self) -> Result<&Package, String> {
        self.package.as_ref().ok_or_else(|| {
            self.package_error.clone().unwrap_or_else(|| {
                "The staged payload is missing or unreadable. Nothing will be changed.".into()
            })
        })
    }

    /// One fresh support proof, or why there isn't one.
    fn proof(&self) -> Result<SupportProof, (bool, String)> {
        let deadline = self.deadline(READ_MS);
        match self.support.detect(self.package.as_ref(), &deadline) {
            SupportOutcome::Supported(proof) => Ok(proof),
            SupportOutcome::NotSupported(text) => Err((true, text)),
            SupportOutcome::Pending(text) => Err((false, text)),
        }
    }

    fn mint(&mut self, id: u64) {
        let result = self.proof().map_err(|(_, text)| text);
        if let Ok(mut results) = self.results.lock() {
            // Requests that were never collected can't pile up.
            while results.len() >= 16 {
                let Some(oldest) = results.keys().next().copied() else {
                    break;
                };
                results.remove(&oldest);
            }
            results.insert(id, result);
        }
    }

    /// The consent names the held plan and this very Apply job.
    fn consented(held: Option<OperationId>, consent: &Option<Consent>, job: &JobIntent) -> bool {
        matches!((held, consent), (Some(plan), Some(c)) if c.plan == plan && c.operation == job.operation)
    }

    /// A held plan is single use: a new Plan replaces it, and any Apply (taken, refused by a
    /// gate, or failed) consumes it, so a superseded preview can never be consented to.
    fn clear_held(&mut self, step: crosspane_installer_core::StepId) {
        match step {
            PAYLOAD => self.held.payload = None,
            SERVICE => self.held.service = None,
            RESTART => self.held.restart = None,
            NETWORK => self.held.network = None,
            _ => {}
        }
    }

    fn handle(&mut self, job: NativeJob) {
        match job {
            NativeJob::Step {
                job,
                consent,
                status,
            } => self.step(job, consent, status),
            NativeJob::Maintenance(request) => self.maintenance(request),
        }
    }

    fn step(&mut self, job: JobIntent, consent: Option<Consent>, status: Option<StatusEvidence>) {
        let status = status.as_ref().map(|s| &s.0);
        let (step, stage) = (job.step, job.stage);
        if stage == JobStage::Plan {
            self.clear_held(step);
        }
        self.dispatch_step(job, consent, status);
        if stage == JobStage::Apply {
            self.clear_held(step);
        }
    }

    fn dispatch_step(
        &mut self,
        job: JobIntent,
        consent: Option<Consent>,
        status: Option<&AgentReply>,
    ) {
        match job.step {
            SUPPORT => self.support_step(job),
            PAYLOAD => self.payload_step(job, consent),
            SERVICE => self.service_step(job, consent),
            RESTART => self.restart_step(job, consent, status),
            AGENT => self.agent_step(job, status),
            NETWORK => self.network_step(job, consent, status),
            other => self.emit(
                job,
                NativeOutcome::Failed,
                format!("This installer does not own step {}.", other.0),
            ),
        }
    }

    // ---- support -----------------------------------------------------------------------------

    fn support_step(&mut self, job: JobIntent) {
        match job.stage {
            JobStage::Detect => match self.proof() {
                Ok(_) => self.emit(
                    job,
                    NativeOutcome::Detected {
                        needs_action: false,
                    },
                    "This computer can run Crosspane.",
                ),
                Err((unsupported, text)) => {
                    self.emit_stop(job.clone(), gate(job.stage, unsupported, text))
                }
            },
            JobStage::Verify => match self.proof() {
                Ok(_) => {
                    let outcome = self.verified();
                    self.emit(job, outcome, "Support is current.");
                }
                Err((unsupported, text)) => {
                    self.emit_stop(job.clone(), gate(job.stage, unsupported, text))
                }
            },
            JobStage::Plan | JobStage::Apply => self.emit(
                job,
                NativeOutcome::Failed,
                "Checking support never changes anything.",
            ),
        }
    }

    // ---- payload -----------------------------------------------------------------------------

    fn payload_step(&mut self, job: JobIntent, consent: Option<Consent>) {
        if let Some(text) = self.package().err() {
            // An absent or unreadable source is pending, with zero mutation.
            let outcome = match job.stage {
                JobStage::Detect | JobStage::Verify => NativeOutcome::Waiting(WaitKind::User),
                JobStage::Plan => NativeOutcome::Waiting(WaitKind::User),
                JobStage::Apply => NativeOutcome::Applied(ApplyOutcome::Refused),
            };
            self.emit(job, outcome, text);
            return;
        }
        let proof = match self.proof() {
            Ok(proof) => proof,
            Err((unsupported, text)) => {
                self.emit_stop(job.clone(), gate(job.stage, unsupported, text));
                return;
            }
        };
        let Some(package) = self.package.take() else {
            return;
        };
        self.payload_stage(&job, consent, &proof, &package);
        self.package = Some(package);
    }

    fn rows_match(rows: &[crosspane_installer_core::ResourceReceipt], owned: bool) -> bool {
        !rows.is_empty()
            && rows.iter().all(|r| {
                r.before == ResourceObservation::Matching
                    && (!owned || r.ownership == ResourceOwnership::Created)
            })
    }

    fn payload_stage(
        &mut self,
        job: &JobIntent,
        consent: Option<Consent>,
        proof: &SupportProof,
        package: &Package,
    ) {
        let job = job.clone();
        match job.stage {
            JobStage::Detect => match self.payloads.detect(proof, package) {
                Ok(rows) if Self::rows_match(&rows, true) => self.emit(
                    job,
                    NativeOutcome::Detected {
                        needs_action: false,
                    },
                    "The installed files already match this payload.",
                ),
                Ok(_) => self.emit(
                    job,
                    NativeOutcome::Detected { needs_action: true },
                    "Crosspane's files need to be installed for your account.",
                ),
                Err(PayloadError::Pending) => self.emit(
                    job,
                    NativeOutcome::Detected { needs_action: true },
                    "An earlier install didn't finish. What is already in place is checked again \
                     before anything is finished.",
                ),
                Err(error) => self.emit_stop(job.clone(), payload_problem(job.stage, error)),
            },
            JobStage::Plan => {
                self.held.payload = None;
                let mut planned = self.payloads.plan(proof, package, job.operation, false);
                let mut resuming = false;
                if matches!(planned, Err(PayloadError::Pending)) {
                    planned = self.payloads.plan(proof, package, job.operation, true);
                    resuming = true;
                }
                match planned {
                    Ok(preview) => {
                        self.held.payload = Some(job.operation);
                        resume::write(&self.io, proof, "payload", "planned");
                        let prefix = self.io.target().paths().prefix.display().to_string();
                        let text = if resuming || preview.resuming {
                            format!(
                                "Finish the interrupted install of Crosspane {}. What is already \
                                 in place is kept; only what is missing is written.",
                                preview.version
                            )
                        } else {
                            format!(
                                "Install Crosspane {} for your account only: programs in {prefix}/bin, \
                                 a startup entry in your user systemd folder, and menu entries in \
                                 your applications folder. Files that already match are kept, and \
                                 nothing outside your home folder changes.",
                                preview.version
                            )
                        };
                        self.emit(
                            job,
                            NativeOutcome::Planned { preview: text },
                            "Review this install.",
                        );
                    }
                    Err(error) => self.emit_stop(job.clone(), payload_problem(job.stage, error)),
                }
            }
            JobStage::Apply => {
                if !Self::consented(self.held.payload, &consent, &job) {
                    self.held.payload = None;
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "That consent was for a different preview. Review the install again.",
                    );
                    return;
                }
                self.held.payload = None;
                let deadline = self.deadline(APPLY_MS);
                let plan_operation = consent.map_or(job.operation, |c| c.plan);
                match self
                    .payloads
                    .apply(proof, package, plan_operation, &deadline)
                {
                    Ok(()) => {
                        resume::write(&self.io, proof, "payload", "applied");
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Applied),
                            "Crosspane's files were written. They are checked next.",
                        );
                    }
                    Err(PayloadError::OutcomeUnknown) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Unknown),
                        "What was written is unknown, so it is checked again before anything is \
                         retried.",
                    ),
                    Err(error) => self.emit_stop(job.clone(), payload_problem(job.stage, error)),
                }
            }
            JobStage::Verify => match self.payloads.observe(proof, package) {
                Ok(rows) if Self::rows_match(&rows, false) => {
                    let outcome = self.verified();
                    self.emit(job, outcome, "The installed files match this payload.");
                }
                _ => self.emit(
                    job,
                    NativeOutcome::Waiting(WaitKind::Contract),
                    "The installed files don't match this payload yet.",
                ),
            },
        }
    }

    // ---- service -----------------------------------------------------------------------------

    /// Everything the service stages need besides a proof: the staged package's rendered files.
    fn service_ready(&mut self, job: &JobIntent) -> Result<(), ()> {
        let deadline = self.deadline(READ_MS);
        let Some(package) = self.package.as_ref() else {
            let text = self.package().err().unwrap_or_default();
            self.emit(job.clone(), NativeOutcome::Waiting(WaitKind::User), text);
            return Err(());
        };
        // Disjoint fields: the package is only read while the service binds its files.
        let result = self.services.prepare(package, &deadline);
        if let Err(error) = result {
            let stop = service_problem(job.stage, error);
            self.emit_stop(job.clone(), stop);
            return Err(());
        }
        Ok(())
    }

    fn service_step(&mut self, job: JobIntent, consent: Option<Consent>) {
        if self.service_ready(&job).is_err() {
            return;
        }
        let deadline = self.deadline(READ_MS);
        match job.stage {
            JobStage::Detect => match self.services.observe(&deadline) {
                Ok(f) if f.enabled && f.active_state == "active" && f.main_pid != 0 => self.emit(
                    job,
                    NativeOutcome::Detected {
                        needs_action: false,
                    },
                    "Crosspane already starts when you sign in, and it is running.",
                ),
                Ok(_) => self.emit(
                    job,
                    NativeOutcome::Detected { needs_action: true },
                    "Crosspane needs to be set up to start when you sign in.",
                ),
                Err(error) => self.emit_stop(job.clone(), service_problem(job.stage, error)),
            },
            JobStage::Plan => {
                self.held.service = None;
                // Planning a change needs the session proved, like making it.
                if let Err((unsupported, text)) = self.proof() {
                    self.emit_stop(job.clone(), gate(job.stage, unsupported, text));
                    return;
                }
                // The read gets its whole budget after the proof, not what the proof left.
                let deadline = self.deadline(READ_MS);
                match self.services.observe(&deadline) {
                    Ok(f) => {
                        let mut actions = Vec::new();
                        let mut lines = Vec::new();
                        if f.needs_reload {
                            actions.push(ServiceAction::Reload);
                            lines.push("reload your user service list");
                        }
                        if !f.enabled {
                            actions.push(ServiceAction::Enable);
                            lines.push("start Crosspane when you sign in");
                        }
                        if f.main_pid == 0 || f.active_state != "active" {
                            actions.push(ServiceAction::Start);
                            lines.push("start Crosspane now");
                        }
                        if actions.is_empty() {
                            // A Plan job can only be answered with a plan or a wait; the step is
                            // checked again from the start.
                            self.emit(
                                job,
                                NativeOutcome::Waiting(WaitKind::User),
                                "Nothing needs to change any more. Check again to confirm it.",
                            );
                            return;
                        }
                        self.held.service = Some((job.operation, actions));
                        let preview = format!(
                            "For your account only (crosspane-agent.service in your user \
                             systemd manager): {}.",
                            lines.join(", then ")
                        );
                        self.emit(job, NativeOutcome::Planned { preview }, "Review startup.");
                    }
                    Err(error) => self.emit_stop(job.clone(), service_problem(job.stage, error)),
                }
            }
            JobStage::Apply => {
                let held = self.held.service.take();
                let (plan_operation, actions) = match held {
                    Some((operation, actions))
                        if Self::consented(Some(operation), &consent, &job) =>
                    {
                        (operation, actions)
                    }
                    _ => {
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Refused),
                            "That consent was for a different preview. Review startup again.",
                        );
                        return;
                    }
                };
                let _ = plan_operation;
                self.apply_service(job, actions);
            }
            JobStage::Verify => match self.services.observe(&deadline) {
                Ok(f) if f.enabled && f.active_state == "active" && f.main_pid != 0 => {
                    let outcome = self.verified();
                    self.emit(
                        job,
                        outcome,
                        "Crosspane starts at sign in, and it is running.",
                    );
                }
                Ok(_) => self.emit(
                    job,
                    NativeOutcome::Waiting(WaitKind::Contract),
                    "The service isn't enabled and running yet.",
                ),
                Err(error) => self.emit_stop(job.clone(), service_problem(job.stage, error)),
            },
        }
    }

    /// Each manager command is its own fresh plan and is never resent after an uncertain result.
    fn apply_service(&mut self, job: JobIntent, actions: Vec<ServiceAction>) {
        for action in actions {
            let proof = match self.proof() {
                Ok(proof) => proof,
                Err((_, text)) => {
                    self.emit(job, NativeOutcome::Applied(ApplyOutcome::Refused), text);
                    return;
                }
            };
            let deadline = self.deadline(APPLY_MS);
            match self.services.apply(&proof, action, &deadline) {
                Ok(result) => {
                    let ok = match action {
                        ServiceAction::Start | ServiceAction::Restart => result
                            .after
                            .as_ref()
                            .is_some_and(|a| a.active_state == "active" && a.main_pid != 0),
                        _ => result.outcome == crosspane_installer_core::MutationOutcome::Verified,
                    };
                    if !ok {
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Unknown),
                            "The user service manager's answer is unclear, so it is checked again \
                             before anything is retried.",
                        );
                        return;
                    }
                }
                Err(ServiceError::Foreign) => {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "A service or agent that Crosspane didn't set up is already in place. \
                         Nothing was changed.",
                    );
                    return;
                }
                Err(_) => {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Unknown),
                        "The user service manager didn't give a clear answer, so it is checked \
                         again before anything is retried.",
                    );
                    return;
                }
            }
        }
        if let Ok(proof) = self.proof() {
            resume::write(&self.io, &proof, "service", "applied");
        }
        self.emit(
            job,
            NativeOutcome::Applied(ApplyOutcome::Applied),
            "Startup was set up and Crosspane was started.",
        );
    }

    // ---- restart -----------------------------------------------------------------------------

    fn restart_step(
        &mut self,
        job: JobIntent,
        consent: Option<Consent>,
        status: Option<&AgentReply>,
    ) {
        if self.service_ready(&job).is_err() {
            return;
        }
        match job.stage {
            JobStage::Detect => {
                // Once restarted this session, only the new instance is looked at again.
                let needs = self.held.restarted.is_none();
                self.emit(
                    job,
                    NativeOutcome::Detected { needs_action: needs },
                    if needs {
                        "Restart Crosspane once so the manager is seen starting a new healthy instance."
                    } else {
                        "Crosspane was restarted. The new instance is checked again."
                    },
                );
            }
            JobStage::Plan => {
                if let Err((unsupported, text)) = self.proof() {
                    self.emit_stop(job.clone(), gate(job.stage, unsupported, text));
                    return;
                }
                self.held.restart = Some(job.operation);
                self.emit(
                    job,
                    NativeOutcome::Planned {
                        preview: "Restart crosspane-agent.service once in your user service \
                                  manager. Crosspane disconnects for a moment while a new \
                                  instance starts; practice then runs against that instance."
                            .into(),
                    },
                    "Review this restart.",
                );
            }
            JobStage::Apply => {
                let held = self.held.restart.take();
                if !Self::consented(held, &consent, &job) {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "That consent was for a different preview. Review the restart again.",
                    );
                    return;
                }
                let proof = match self.proof() {
                    Ok(proof) => proof,
                    Err((_, text)) => {
                        self.emit(job, NativeOutcome::Applied(ApplyOutcome::Refused), text);
                        return;
                    }
                };
                let deadline = self.deadline(APPLY_MS);
                match self
                    .services
                    .apply(&proof, ServiceAction::Restart, &deadline)
                {
                    Ok(result) if result.after.is_some() => {
                        self.held.restarted = Some(result.previous_instance);
                        resume::write(&self.io, &proof, "restart", "applied");
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Applied),
                            "A restart was requested. The new instance is checked next.",
                        );
                    }
                    Ok(_) | Err(ServiceError::OutcomeUnknown) | Err(ServiceError::Native(_)) => {
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Unknown),
                            "Whether the restart happened is unclear, so it is checked again \
                             before anything is retried.",
                        );
                    }
                    Err(_) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "The restart wasn't started. Nothing was changed.",
                    ),
                }
            }
            JobStage::Verify => {
                let previous = self.held.restarted.flatten();
                match self.agent_check(status, previous) {
                    Ok(_) => {
                        let outcome = self.verified();
                        self.emit(
                            job,
                            outcome,
                            "A new, healthy Crosspane instance is running.",
                        );
                    }
                    Err(stop) => self.emit_stop(job, stop),
                }
            }
        }
    }

    // ---- agent -------------------------------------------------------------------------------

    /// The agent is up, matched to the service, holds its key in the system keyring and has
    /// finished recovering. Evidence is the status issued for this job, never a cached one.
    fn agent_check(
        &mut self,
        status: Option<&AgentReply>,
        previous: Option<u64>,
    ) -> Result<(Box<HealthSnapshot>, AgentReply), Stop> {
        let deadline = self.deadline(AGENT_MS);
        let wait = |text: &str| (NativeOutcome::Waiting(WaitKind::Contract), text.to_owned());
        let facts = self
            .services
            .observe(&deadline)
            .map_err(|e| service_problem(JobStage::Verify, e))?;
        let reply = status;
        let id = reply.map_or(0, |r| r.id);
        let evidence = self
            .services
            .agent(&facts, reply, id, self.now(), previous, &deadline)
            .map_err(|e| service_problem(JobStage::Verify, e))?;
        match evidence {
            AgentEvidence::ManagerInactive => Err(wait("Crosspane's service isn't running.")),
            AgentEvidence::Starting(_) => Err(wait("Crosspane is starting.")),
            AgentEvidence::WaitingForKeystore(_) => Err((
                NativeOutcome::Waiting(WaitKind::User),
                "Crosspane is waiting for your system keyring. Unlock it when asked and setup \
                 carries on by itself."
                    .into(),
            )),
            AgentEvidence::Failed(_) => Err((
                NativeOutcome::Failed,
                "Crosspane's agent stopped while starting.".into(),
            )),
            AgentEvidence::PendingStatus(_) => {
                Err(wait("The agent is up but hasn't reported its health yet."))
            }
            AgentEvidence::PendingHealthContract(..) => Err(wait(
                "This agent build doesn't report the health facts setup needs. An agent update is \
                 required.",
            )),
            AgentEvidence::StatusFailure(_) => Err(wait("The agent didn't answer.")),
            AgentEvidence::Matched(health) => {
                let installer = health.installer();
                if installer.keystore != KeyStoreProvenance::OsStore {
                    return Err((
                        NativeOutcome::Waiting(WaitKind::User),
                        "Crosspane isn't holding its key in the system keyring. Setup needs the \
                         keyring (Secret Service) to be available and unlocked."
                            .into(),
                    ));
                }
                if installer.startup_recovery == StartupRecovery::Failed
                    || installer.recovery_pending != 0
                {
                    return Err(wait(
                        "Crosspane is still recovering windows from an earlier session.",
                    ));
                }
                let Some(reply) = reply else {
                    return Err(wait("The agent hasn't reported yet."));
                };
                Ok((health, reply.clone()))
            }
        }
    }

    fn agent_step(&mut self, job: JobIntent, status: Option<&AgentReply>) {
        if self.service_ready(&job).is_err() {
            return;
        }
        match job.stage {
            // The agent is installed by the earlier steps; this step only looks at it.
            JobStage::Detect => self.emit(
                job,
                NativeOutcome::Detected {
                    needs_action: false,
                },
                "Looking for Crosspane's agent.",
            ),
            JobStage::Verify => {
                let previous = self.held.restarted.flatten();
                let (_health, reply) = match self.agent_check(status, previous) {
                    Ok(ok) => ok,
                    Err(stop) => {
                        self.emit_stop(job, stop);
                        return;
                    }
                };
                // The install receipt is completed only against this fresh, matched status.
                let package_error = self.package().err();
                if let Some(text) = package_error {
                    self.emit(job, NativeOutcome::Waiting(WaitKind::User), text);
                    return;
                }
                let proof = match self.proof() {
                    Ok(proof) => proof,
                    Err((unsupported, text)) => {
                        self.emit_stop(job.clone(), gate(job.stage, unsupported, text));
                        return;
                    }
                };
                let deadline = self.deadline(AGENT_MS);
                let Some(package) = self.package.take() else {
                    return;
                };
                let result = self
                    .payloads
                    .verify(&proof, &package, &reply, self.now(), &deadline);
                self.package = Some(package);
                match result {
                    Ok(()) => {
                        resume::write(&self.io, &proof, "agent", "done");
                        let outcome = self.verified();
                        self.emit(
                            job,
                            outcome,
                            "Crosspane is running from the files setup installed, with its key \
                             in the system keyring.",
                        );
                    }
                    Err(PayloadError::Pending) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::Contract),
                        "The install isn't finished being recorded yet.",
                    ),
                    Err(PayloadError::Foreign) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::User),
                        "The running agent isn't the one just installed. Restart Crosspane and \
                         check again.",
                    ),
                    Err(_) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::Contract),
                        "The installed files couldn't be matched to the running agent yet.",
                    ),
                }
            }
            JobStage::Plan | JobStage::Apply => self.emit(
                job,
                NativeOutcome::Failed,
                "Checking the agent never changes anything.",
            ),
        }
    }

    // ---- network -----------------------------------------------------------------------------

    fn network_step(
        &mut self,
        job: JobIntent,
        consent: Option<Consent>,
        status: Option<&AgentReply>,
    ) {
        let source = self.source();
        let connected = status.is_some_and(|r| r.source == source && peer_connected(r));
        let deadline = self.deadline(READ_MS);
        match job.stage {
            JobStage::Detect => {
                if connected {
                    self.emit(
                        job,
                        NativeOutcome::Detected {
                            needs_action: false,
                        },
                        "Another computer is already connected.",
                    );
                    return;
                }
                match self.firewalls.read(&deadline) {
                    Ok(reading) => match (reading.active, reading.lan_rule) {
                        (Some(false), _) => self.emit(
                            job,
                            NativeOutcome::Detected {
                                needs_action: false,
                            },
                            "ufw isn't active, so it isn't blocking anything. A connection from \
                             the other computer confirms this.",
                        ),
                        (Some(true), RulePresence::Present) => self.emit(
                            job,
                            NativeOutcome::Detected {
                                needs_action: false,
                            },
                            "ufw is active and already allows Crosspane on your network.",
                        ),
                        (Some(true), RulePresence::Absent) => self.emit(
                            job,
                            NativeOutcome::Detected { needs_action: true },
                            "ufw is active and doesn't yet allow Crosspane on your network.",
                        ),
                        (Some(true), RulePresence::Modified) => self.emit(
                            job,
                            NativeOutcome::Waiting(WaitKind::User),
                            "ufw has a Crosspane rule an administrator changed. Crosspane keeps \
                             it as it is; allow UDP 47811-47812 from your network yourself if \
                             needed.",
                        ),
                        (Some(true), RulePresence::Unknown) => self.emit(
                            job,
                            NativeOutcome::Waiting(WaitKind::Contract),
                            "ufw is active but its rules can't be read without administrator \
                             rights, so they aren't assumed to be missing or present. If pairing \
                             can't connect, allow UDP 47811-47812 from your network.",
                        ),
                        (None, _) => self.emit(
                            job,
                            NativeOutcome::Waiting(WaitKind::Contract),
                            "Whether a firewall is active can't be read. If you use one, allow \
                             UDP 47811-47812 from your network. A connection from the other \
                             computer confirms it works.",
                        ),
                    },
                    Err(_) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::Contract),
                        "The firewall state couldn't be read just now.",
                    ),
                }
            }
            JobStage::Plan => {
                self.held.network = None;
                let proof = match self.proof() {
                    Ok(proof) => proof,
                    Err((unsupported, text)) => {
                        self.emit_stop(job.clone(), gate(job.stage, unsupported, text));
                        return;
                    }
                };
                let operation = self.firewall_op(job.operation);
                let deadline = self.deadline(READ_MS);
                match self.firewalls.plan_lan(&proof, operation, &deadline) {
                    Ok(preview) => {
                        self.held.network = Some(job.operation);
                        self.emit(
                            job,
                            NativeOutcome::Planned {
                                preview: format!(
                                    "This adds one rule to ufw for your local network. It runs as \
                                     administrator after you approve it in the system prompt.\n{preview}\nmDNS \
                                     discovery is a separate rule, offered only if connecting by \
                                     address works and discovery then doesn't."
                                ),
                            },
                            "Review the network change.",
                        );
                    }
                    Err(error) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::Contract),
                        format!(
                            "The firewall change can't be planned right now ({}).",
                            firewall_why(error)
                        ),
                    ),
                }
            }
            JobStage::Apply => {
                let held = self.held.network.take();
                if !Self::consented(held, &consent, &job) {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "That consent was for a different preview. Review the network change again.",
                    );
                    return;
                }
                let proof = match self.proof() {
                    Ok(proof) => proof,
                    Err((_, text)) => {
                        self.emit(job, NativeOutcome::Applied(ApplyOutcome::Refused), text);
                        return;
                    }
                };
                let operation = self.firewall_op(consent.map_or(job.operation, |c| c.plan));
                // The system prompt is the person's, so this stage may wait as long as the
                // frozen native contract allows. It never extends an expired call.
                let long = self.deadline(MAX_NATIVE_TIMEOUT_MS);
                let applied = self.firewalls.apply_lan(&proof, operation, &long);
                // The firewall journal recorded this rule under `operation`, whether or not the
                // outcome was clean. Keep its receipt so a later removal can name exactly it.
                if matches!(applied, Ok(RuleApply::Dispatched) | Ok(RuleApply::Unknown))
                    && let Some(bytes) = self.firewalls.receipt_lan(&proof, operation)
                {
                    resume::write_receipt(&self.io, &proof, &bytes);
                }
                match applied {
                    Ok(RuleApply::Dispatched) | Ok(RuleApply::AlreadyPresent) => {
                        resume::write(&self.io, &proof, "network", "applied");
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Applied),
                            "The command finished. That alone proves nothing: the rule is read \
                             back, and a connection from the other computer confirms it.",
                        );
                    }
                    Ok(RuleApply::NotDispatched) | Ok(RuleApply::Changed) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "Nothing was changed: the firewall or network changed after the preview.",
                    ),
                    Ok(RuleApply::NoPromptAgent(manual)) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        match manual {
                            Some(command) => format!(
                                "No system prompt could be shown, so nothing was changed. To do \
                                 it yourself, run: {command}"
                            ),
                            None => "No system prompt could be shown, so nothing was changed."
                                .to_owned(),
                        },
                    ),
                    Ok(RuleApply::Unknown) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Unknown),
                        "A timeout or a dismissed prompt leaves the result unknown, so the \
                         firewall is read again before anything is retried.",
                    ),
                    Ok(RuleApply::Failed) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Failed),
                        "The firewall rule couldn't be added.",
                    ),
                    Err(FirewallError::Native(
                        NativeError::Timeout | NativeError::Cancelled | NativeError::OutcomeUnknown,
                    )) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Unknown),
                        "A timeout leaves the result unknown, so the firewall is read again \
                         before anything is retried.",
                    ),
                    Err(_) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "Nothing was changed: the preview is no longer current.",
                    ),
                }
            }
            JobStage::Verify => {
                if connected {
                    let outcome = self.verified();
                    self.emit(
                        job,
                        outcome,
                        "The other computer connected over your network, so it isn't blocked.",
                    );
                    return;
                }
                // Neither a command's exit status nor a rule's presence proves the network.
                self.emit(
                    job,
                    NativeOutcome::Waiting(WaitKind::Peer),
                    "Waiting for the other computer to connect. Setup confirms the network when \
                     it does.",
                );
            }
        }
    }

    // ---- uninstall ---------------------------------------------------------------------------

    fn maintenance(&mut self, request: MaintenanceRequest) {
        match request {
            MaintenanceRequest::Inspect { id } => {
                // A running removal waiting for a follow-up answer is never silently abandoned.
                if self.maintenance.running {
                    self.maint(MaintenanceReport::Refused {
                        id,
                        reason: "A removal is still running. Finish it first.".into(),
                    });
                    return;
                }
                self.maintenance = Maintenance {
                    id: Some(id),
                    issued: self.maintenance.issued,
                    ..Maintenance::default()
                };
                let deadline = self.deadline(READ_MS);
                let offer = self.uninstaller.inspect(self.package.as_ref(), &deadline);
                let repair = self.repairer.inspect(self.package.as_ref(), self.now());
                self.maintenance.discard_offered = repair.discardable;
                self.maint(MaintenanceReport::Inspected {
                    id,
                    uninstall: offer.uninstall,
                    repair: repair.repair,
                    choices: offer.choices,
                });
                // An earlier repair that didn't finish is offered for resume.
                if repair.discardable {
                    self.maint(MaintenanceReport::RepairDiscardable { id });
                } else if let Some(lines) = repair.resumable {
                    self.maint(MaintenanceReport::RepairResumable { id, lines });
                }
            }
            MaintenanceRequest::PlanRepair { id, status } => {
                if self.maintenance.id != Some(id)
                    || self.maintenance.running
                    || self.maintenance.repair_active
                {
                    self.stale_repair(id, "That repair preview is no longer current.");
                    return;
                }
                self.maintenance.repair_plan = None;
                let operation = self.maintenance_op();
                let status = status.as_ref().map(|s| &s.0);
                match self
                    .repairer
                    .plan(self.package.as_ref(), status, operation, self.now())
                {
                    Ok(preview) => {
                        self.maintenance.repair_plan = Some(operation);
                        self.maint(MaintenanceReport::RepairPlanned {
                            id,
                            plan: operation.0,
                            preview,
                        });
                    }
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::ConfirmRepair {
                id, plan, status, ..
            } => {
                // The preview is single use: whatever happens next, it is gone.
                let held = self.maintenance.repair_plan.take();
                if self.maintenance.id != Some(id)
                    || self.maintenance.running
                    || self.maintenance.repair_active
                    || held.map(|o| o.0) != Some(plan)
                {
                    self.stale_repair(
                        id,
                        "Confirm the current repair preview before repairing Crosspane.",
                    );
                    return;
                }
                self.maintenance.repair_active = true;
                self.maintenance.repair_since = None;
                let operation = self.maintenance_op();
                let status = status.as_ref().map(|s| &s.0);
                let result = self.repairer.confirm(
                    self.package.as_ref(),
                    status,
                    OperationId(plan),
                    operation,
                    self.now(),
                );
                match result {
                    Ok(step) => self.repair_step(id, step),
                    Err(reason) => {
                        self.maintenance.repair_active = false;
                        self.maint(MaintenanceReport::Refused { id, reason });
                    }
                }
            }
            MaintenanceRequest::VerifyRepair { id, status } => {
                if self.maintenance.id != Some(id) || !self.maintenance.repair_active {
                    return;
                }
                let status = status.as_ref().map(|s| &s.0);
                let step = self
                    .repairer
                    .verify(self.package.as_ref(), status, self.now());
                self.repair_step(id, step);
            }
            MaintenanceRequest::DiscardRepair { id, status } => {
                if self.maintenance.id != Some(id)
                    || self.maintenance.running
                    || self.maintenance.repair_active
                    || self.maintenance.planned.is_some()
                    || self.maintenance.repair_plan.is_some()
                    || !self.maintenance.discard_offered
                {
                    self.stale_repair(id, "That discard offer is no longer current.");
                    return;
                }
                self.maintenance.discard_offered = false;
                let status = status.as_ref().map(|s| &s.0);
                match self
                    .repairer
                    .discard(self.package.as_ref(), status, self.now())
                {
                    Ok(finish) if finish.outcome == crate::live::RepairOutcome::Retired => {
                        self.maint(MaintenanceReport::RepairDiscarded {
                            id,
                            lines: finish.lines,
                        });
                    }
                    Ok(_) => self.stale_repair(id, "The repair record could not be discarded."),
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::ResumeRepair { id, status } => {
                if self.maintenance.id != Some(id)
                    || self.maintenance.running
                    || self.maintenance.repair_active
                {
                    self.stale_repair(id, "That repair can't be resumed from this window.");
                    return;
                }
                self.maintenance.repair_active = true;
                self.maintenance.repair_since = None;
                let status = status.as_ref().map(|s| &s.0);
                let result = self
                    .repairer
                    .resume(self.package.as_ref(), status, self.now());
                self.maintenance.repair_active = false;
                match result {
                    Ok(finish) => self.repair_finished(id, finish),
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::PlanUninstall { id, choices, .. } => {
                if self.maintenance.id != Some(id)
                    || self.maintenance.running
                    || self.maintenance.repair_active
                {
                    self.maint(MaintenanceReport::Refused {
                        id,
                        reason: "That removal preview is no longer current.".into(),
                    });
                    return;
                }
                self.maintenance.planned = None;
                let operation = self.maintenance_op();
                let deadline = self.deadline(READ_MS);
                match self
                    .uninstaller
                    .plan(self.package.as_ref(), &choices, operation, &deadline)
                {
                    Ok(preview) => {
                        self.maintenance.planned = Some(id);
                        self.maint(MaintenanceReport::Planned { id, preview });
                    }
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::ConfirmUninstall { id, .. } => {
                if self.maintenance.id != Some(id)
                    || self.maintenance.planned != Some(id)
                    || self.maintenance.running
                {
                    self.maint(MaintenanceReport::Refused {
                        id,
                        reason: "Confirm the current preview before removing Crosspane.".into(),
                    });
                    return;
                }
                self.maintenance.planned = None;
                let operation = self.maintenance_op();
                match self.uninstaller.begin(self.package.as_ref(), operation) {
                    Ok(()) => {
                        self.maintenance.running = true;
                        self.drive(id, None);
                    }
                    Err(reason) => self.maint(MaintenanceReport::Finished {
                        id,
                        outcome: crate::live::MaintenanceOutcome::Refused,
                        lines: vec![
                            "Removal didn't start.".into(),
                            reason,
                            "Nothing was removed.".into(),
                        ],
                    }),
                }
            }
            MaintenanceRequest::ConfirmFollowUp { id, follow_up, .. } => {
                if self.maintenance.id != Some(id) || !self.maintenance.running {
                    return;
                }
                self.drive(id, Some((follow_up, true)));
            }
            MaintenanceRequest::DeclineFollowUp { id, follow_up } => {
                if self.maintenance.id != Some(id) || !self.maintenance.running {
                    return;
                }
                self.drive(id, Some((follow_up, false)));
            }
        }
    }

    fn stale_repair(&self, id: MaintenanceId, reason: &str) {
        self.maint(MaintenanceReport::Refused {
            id,
            reason: format!("{reason} Nothing was changed."),
        });
    }

    fn repair_finished(&self, id: MaintenanceId, finish: RepairFinish) {
        self.maint(MaintenanceReport::RepairFinished {
            id,
            outcome: finish.outcome,
            lines: finish.lines,
            resumable: finish.resumable,
        });
    }

    /// Report where a repair stands. The wait for the new agent's health is bounded here: when it
    /// runs out the repair ends with the adapter's honest typed result, never a guess.
    fn repair_step(&mut self, id: MaintenanceId, step: RepairStep) {
        match step {
            RepairStep::Waiting {
                detail,
                if_timed_out,
                closeable,
            } => {
                let now = self.now();
                let since = *self.maintenance.repair_since.get_or_insert(now);
                if now.saturating_sub(since) > REPAIR_HEALTH_WAIT_MS {
                    self.maintenance.repair_active = false;
                    self.repair_finished(id, if_timed_out);
                } else {
                    self.maint(MaintenanceReport::RepairWaiting {
                        id,
                        detail,
                        closeable,
                    });
                }
            }
            RepairStep::Finished(finish) => {
                self.maintenance.repair_active = false;
                self.repair_finished(id, finish);
            }
        }
    }

    /// A fresh maintenance operation, disjoint from every install operation of this session.
    fn maintenance_op(&mut self) -> OperationId {
        self.maintenance.issued += 1;
        OperationId(
            self.op_base
                .saturating_add(MAINTENANCE_OPS)
                .saturating_add(self.maintenance.issued.saturating_mul(MAINTENANCE_SPACING)),
        )
    }

    /// Advance the run until it needs the person or ends. Each stage has its own deadline inside
    /// the adapter, and an expired call is never extended.
    fn drive(&mut self, id: MaintenanceId, answer: Option<(u16, bool)>) {
        let mut next = match answer {
            Some((follow_up, confirm)) => {
                self.uninstaller
                    .follow_up(self.package.as_ref(), follow_up, confirm)
            }
            None => self.uninstaller.advance(self.package.as_ref()),
        };
        let mut steps = 0u32;
        loop {
            match next {
                UninstallProgress::Progress(detail) => {
                    self.maint(MaintenanceReport::Progress { id, detail });
                    steps += 1;
                    if steps > MAX_DRIVE_STEPS {
                        self.maintenance.running = false;
                        self.maint(MaintenanceReport::Finished {
                            id,
                            outcome: crate::live::MaintenanceOutcome::Failed,
                            lines: vec![
                                "Removal stopped making progress, so it was stopped here.".into(),
                                "What was already removed stays removed; nothing uncertain was \
                                 retried. Check again before removing anything else."
                                    .into(),
                            ],
                        });
                        return;
                    }
                    next = self.uninstaller.advance(self.package.as_ref());
                }
                UninstallProgress::FollowUp {
                    id: follow_up,
                    label,
                    preview,
                } => {
                    self.maint(MaintenanceReport::FollowUp {
                        id,
                        follow_up,
                        label,
                        preview,
                    });
                    return;
                }
                UninstallProgress::Finished(outcome, lines) => {
                    self.maintenance.running = false;
                    self.maint(MaintenanceReport::Finished { id, outcome, lines });
                    return;
                }
            }
            if self.stop.is_cancelled() {
                return;
            }
        }
    }
}

fn payload_problem(stage: JobStage, error: PayloadError) -> Stop {
    match error {
        PayloadError::Foreign => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Refused)
            } else {
                NativeOutcome::Waiting(WaitKind::User)
            },
            "Some files where Crosspane installs weren't put there by Crosspane. They are left \
             exactly as they are, and nothing was changed."
                .into(),
        ),
        PayloadError::Pending => (
            NativeOutcome::Waiting(WaitKind::Contract),
            "An earlier install is unfinished. It is checked again before anything continues."
                .into(),
        ),
        other => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Failed)
            } else {
                NativeOutcome::Failed
            },
            format!("The install couldn't continue ({other})."),
        ),
    }
}

fn service_problem(stage: JobStage, error: ServiceError) -> Stop {
    match error {
        ServiceError::Foreign => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Refused)
            } else {
                NativeOutcome::Failed
            },
            "A service or agent that Crosspane didn't set up is already in place. Nothing was \
             changed."
                .into(),
        ),
        ServiceError::OutcomeUnknown => (
            NativeOutcome::Waiting(WaitKind::Contract),
            "The service changed while it was being read. Check again.".into(),
        ),
        _ => (
            NativeOutcome::Waiting(WaitKind::Contract),
            "Crosspane's service couldn't be read just now.".into(),
        ),
    }
}

fn firewall_why(error: FirewallError) -> &'static str {
    match error {
        FirewallError::Manual => "the firewall isn't one Crosspane can change",
        FirewallError::Changed => "the network changed",
        FirewallError::Kept => "an administrator's rule is kept",
        FirewallError::CurrentRequired | FirewallError::MdnsPending => {
            "a current connection reading is required"
        }
        _ => "the state couldn't be read",
    }
}
