//! The Mac worker: one background thread that maps core's job stages onto the native domains.
//!
//! Policy lives here and nowhere else in the Mac binding: when a result means "needs action",
//! "waiting", "unknown, detect again" or "refused", which evidence each stage takes, and how a
//! consent is tied to the exact plan that was previewed. Every adapter call is bounded by a
//! deadline; nothing on this thread is ever waited for by the GUI.
//!
//! Evidence is only ever a fresh native read or a Status issued for the job: the worker never
//! reports `Verified` from an acknowledgement, a command's exit status or a cached answer.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crosspane_installer_core::{ApplyOutcome, JobIntent, JobStage, OperationId, WaitKind};

use super::super::native_io::{
    Cancellation, Clock as NativeClock, Deadline, MAX_NATIVE_TIMEOUT_MS,
};
use super::super::permissions::{
    ASK_EXPLANATION, MICROPHONE_DETAIL, MICROPHONE_REASON, SettingsPane,
};
use super::domains::{
    Agents, AudioError, AudioPackages, AudioState, DomainFactory, FixtureLauncher, InstallError,
    InstallState, Installs, RepairFinish, RepairStep, Repairer, Support, SupportOutcome,
    Uninstaller,
};
use super::ports::{Broker, Command};
use super::{AGENT, AUDIO, INSTALL, PERMISSIONS, SUPPORT};
use crate::agent_contract::{
    AgentReply, DecodedReply, HealthSnapshot, KeyStoreProvenance, PendingHealthReason,
    PermissionName, PermissionState, StartupRecovery, StatusAdmission,
};
use crate::live::{
    Clock, Consent, MaintenanceId, MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome,
    NativeReport, StatusEvidence, StepReport,
};

/// A bounded deadline for ordinary reads, the install (which may wait for the old agent to
/// stop) and the short checks.
const READ_MS: u64 = 8_000;
const APPLY_MS: u64 = 100_000;
const REMOVE_MS: u64 = 100_000;
/// A repair stops the old agent, replaces files and starts the new one, in one bounded stage.
const REPAIR_MS: u64 = 100_000;
/// How long a repair that started the new agent waits for it to report healthy before it ends
/// with an honest "outcome unknown, resume required".
const REPAIR_HEALTH_WAIT_MS: u64 = 90_000;

pub struct WorkerParts {
    pub clock: Clock,
    pub native_clock: Arc<dyn NativeClock>,
    /// Built on the worker thread, which then owns every domain.
    pub domains: DomainFactory,
    pub reports: SyncSender<NativeReport>,
    pub broker: Arc<Broker>,
    pub stop: Arc<Cancellation>,
}

pub struct Worker {
    expired: Deadline,
    clock: Clock,
    native_clock: Arc<dyn NativeClock>,
    support: Box<dyn Support>,
    installs: Box<dyn Installs>,
    audio: Box<dyn AudioPackages>,
    agents: Box<dyn Agents>,
    uninstaller: Box<dyn Uninstaller>,
    repairer: Box<dyn Repairer>,
    fixtures: Option<Box<dyn FixtureLauncher>>,
    reports: SyncSender<NativeReport>,
    broker: Arc<Broker>,
    stop: Arc<Cancellation>,
    /// Removal intents persist across runs, so their ids never restart at 1.
    op_base: u64,
    held: Held,
    maintenance: Maintenance,
}

#[derive(Default)]
struct Held {
    /// The plan operation the next install Apply must name.
    install: Option<OperationId>,
    /// The same, for the sound driver.
    audio: Option<OperationId>,
}

#[derive(Default)]
struct Maintenance {
    id: Option<MaintenanceId>,
    planned: Option<(MaintenanceId, OperationId)>,
    running: bool,
    /// The number of the repair preview the next confirmation must name.
    repair_plan: Option<OperationId>,
    /// A confirmed repair that hasn't ended: it is changing things, or watching the new agent.
    repair_active: bool,
    /// When the repair first waited for the new agent's health.
    repair_since: Option<u64>,
}

/// What a stage reports when a gate refuses it.
type Stop = (NativeOutcome, String);

fn wait_contract(text: impl Into<String>) -> Stop {
    (NativeOutcome::Waiting(WaitKind::Contract), text.into())
}

fn wait_user(text: impl Into<String>) -> Stop {
    (NativeOutcome::Waiting(WaitKind::User), text.into())
}

/// How a refused stage is reported: an apply that was refused changed nothing.
fn refused(stage: JobStage, text: impl Into<String>) -> Stop {
    match stage {
        JobStage::Apply => (NativeOutcome::Applied(ApplyOutcome::Refused), text.into()),
        _ => wait_user(text),
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX / 4096)) as u64)
}

/// The permissions Crosspane needs: the microphone only once audio is on.
fn required_permissions(health: &HealthSnapshot) -> Vec<PermissionName> {
    let mut required = vec![
        PermissionName::ScreenRecording,
        PermissionName::Accessibility,
        PermissionName::InputMonitoring,
    ];
    if health.installer().audio.enabled {
        required.push(PermissionName::Microphone);
    }
    required
}

fn missing_permissions(health: &HealthSnapshot) -> Vec<PermissionName> {
    required_permissions(health)
        .into_iter()
        .filter(|name| {
            health
                .installer()
                .permissions
                .iter()
                .find(|fact| fact.name == *name)
                .map(|fact| fact.state)
                != Some(PermissionState::Granted)
        })
        .collect()
}

fn pane(name: PermissionName) -> SettingsPane {
    match name {
        PermissionName::ScreenRecording => SettingsPane::ScreenRecording,
        PermissionName::Accessibility => SettingsPane::Accessibility,
        PermissionName::InputMonitoring => SettingsPane::InputMonitoring,
        PermissionName::Microphone => SettingsPane::Microphone,
    }
}

fn permission_label(name: PermissionName) -> &'static str {
    match name {
        PermissionName::ScreenRecording => "Screen Recording",
        PermissionName::Accessibility => "Accessibility",
        PermissionName::InputMonitoring => "Input Monitoring",
        PermissionName::Microphone => "Microphone",
    }
}

/// The agent's complete health, or why it can't be read right now. `reply` must be a Status
/// issued for this job; an agent that didn't answer proves nothing either way.
fn health_of(status: Option<&AgentReply>) -> Result<(&AgentReply, &HealthSnapshot), Stop> {
    let Some(reply) = status else {
        return Err(wait_contract("The agent hasn't answered yet."));
    };
    match &reply.result {
        Ok(DecodedReply::Status(StatusAdmission::Supported(health))) => Ok((reply, health)),
        Ok(DecodedReply::Status(StatusAdmission::PendingHealthContract(reason))) => {
            Err(wait_contract(match reason {
                PendingHealthReason::UnsupportedVersion => {
                    "This agent build doesn't report the health facts setup needs. An agent \
                     update is required."
                }
                _ => "The agent is up but hasn't reported its complete health yet.",
            }))
        }
        _ => Err(wait_contract(
            "Crosspane's agent isn't answering. It may not be running yet.",
        )),
    }
}

impl Worker {
    pub fn new(parts: WorkerParts) -> Result<Self, super::super::native_io::NativeError> {
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let expired = Deadline::new(1, parts.native_clock.clone(), cancelled)?;
        let domains = (parts.domains)();
        Ok(Self {
            expired,
            clock: parts.clock,
            native_clock: parts.native_clock,
            support: domains.support,
            installs: domains.installs,
            audio: domains.audio,
            agents: domains.agents,
            uninstaller: domains.uninstaller,
            repairer: domains.repairer,
            fixtures: domains.fixtures,
            reports: parts.reports,
            broker: parts.broker,
            stop: parts.stop,
            op_base: unix_ms().saturating_mul(1000),
            held: Held::default(),
            maintenance: Maintenance::default(),
        })
    }

    pub fn run(mut self, commands: Receiver<Command>) {
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            match commands.recv_timeout(Duration::from_millis(20)) {
                Ok(Command::Job(job)) => self.handle(job),
                Ok(Command::Admit { ticket, link }) => {
                    let deadline = self.deadline(READ_MS);
                    let result = self.agents.admit(link, &deadline);
                    self.broker.put_admission(ticket, result);
                }
                Ok(Command::Fixture { ticket, font }) => {
                    let deadline = self.deadline(READ_MS);
                    let result = match self.fixtures.as_mut() {
                        Some(launcher) => launcher.launch(&font, &deadline),
                        None => Err("This build can't start the practice window.".into()),
                    };
                    self.broker.put_launch(ticket, result);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    // ---- plumbing ----------------------------------------------------------------------------

    fn now(&self) -> u64 {
        (self.clock)()
    }

    fn deadline(&self, ms: u64) -> Deadline {
        // The bound is clamped into the valid range, so this can't fail; if it ever did, the
        // already-expired deadline makes every native call refuse safely.
        Deadline::new(
            ms.clamp(1, MAX_NATIVE_TIMEOUT_MS),
            self.native_clock.clone(),
            (*self.stop).clone(),
        )
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

    /// The consent names the held plan and this very Apply job.
    fn consented(held: Option<OperationId>, consent: &Option<Consent>, job: &JobIntent) -> bool {
        matches!((held, consent), (Some(plan), Some(c)) if c.plan == plan && c.operation == job.operation)
    }

    /// A held plan is single use: a new Plan replaces it, and any Apply (taken, refused by a
    /// gate, or failed) consumes it, so a superseded preview can never be consented to.
    fn clear_held(&mut self, step: crosspane_installer_core::StepId) {
        match step {
            INSTALL => self.held.install = None,
            AUDIO => self.held.audio = None,
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
            INSTALL => self.install_step(job, consent, status),
            AGENT => self.agent_step(job, status),
            AUDIO => self.audio_step(job, consent),
            PERMISSIONS => self.permissions_step(job, status),
            other => self.emit(
                job,
                NativeOutcome::Failed,
                format!("This installer does not own step {}.", other.0),
            ),
        }
    }

    // ---- support -----------------------------------------------------------------------------

    fn support_step(&mut self, job: JobIntent) {
        let deadline = self.deadline(READ_MS);
        match job.stage {
            JobStage::Detect | JobStage::Verify => match self.support.observe(&deadline) {
                SupportOutcome::Supported(source) => {
                    if job.stage == JobStage::Detect {
                        self.emit(
                            job,
                            NativeOutcome::Detected {
                                needs_action: false,
                            },
                            "This Mac can run Crosspane.",
                        );
                    } else {
                        let observed_at_ms = self.now();
                        self.emit(
                            job,
                            NativeOutcome::Verified {
                                source,
                                observed_at_ms,
                            },
                            "Support is current.",
                        );
                    }
                }
                SupportOutcome::Unsupported(text) => {
                    self.emit(job, NativeOutcome::Unsupported, text);
                }
                SupportOutcome::Unavailable(text) => self.emit_stop(job, wait_user(text)),
            },
            JobStage::Plan | JobStage::Apply => self.emit(
                job,
                NativeOutcome::Failed,
                "Checking support never changes anything.",
            ),
        }
    }

    /// Support must hold for every stage of a step that changes the Mac: planning a change needs
    /// the session proved, like making it. A Mac the support check refuses is never touched, and
    /// the refusal is reported for `job` here.
    fn supported(&mut self, job: &JobIntent) -> bool {
        let deadline = self.deadline(READ_MS);
        match self.support.observe(&deadline) {
            SupportOutcome::Supported(_) => true,
            SupportOutcome::Unsupported(text) => {
                let outcome = if job.stage == JobStage::Apply {
                    NativeOutcome::Applied(ApplyOutcome::Refused)
                } else {
                    NativeOutcome::Unsupported
                };
                self.emit(job.clone(), outcome, text);
                false
            }
            SupportOutcome::Unavailable(text) => {
                self.emit_stop(job.clone(), refused(job.stage, text));
                false
            }
        }
    }

    // ---- install -----------------------------------------------------------------------------

    fn install_step(
        &mut self,
        job: JobIntent,
        consent: Option<Consent>,
        status: Option<&AgentReply>,
    ) {
        if !self.supported(&job) {
            return;
        }
        match job.stage {
            JobStage::Detect => {
                let deadline = self.deadline(READ_MS);
                match self.installs.detect(status, &deadline) {
                    Ok(InstallState::Current) => self.emit(
                        job,
                        NativeOutcome::Detected {
                            needs_action: false,
                        },
                        "Crosspane is already installed for this account and starts when you \
                         sign in.",
                    ),
                    Ok(InstallState::Needed) => self.emit(
                        job,
                        NativeOutcome::Detected { needs_action: true },
                        "Crosspane needs to be installed for this account.",
                    ),
                    Err(error) => self.emit_stop(job.clone(), install_problem(job.stage, error)),
                }
            }
            JobStage::Plan => {
                self.held.install = None;
                let deadline = self.deadline(READ_MS);
                match self.installs.plan(job.operation, status, &deadline) {
                    Ok(Some(preview)) => {
                        self.held.install = Some(job.operation);
                        let interrupt = if preview.replacing {
                            " If Crosspane is running, it is stopped first. What an older or \
                             unfinished install left there is saved in ~/Library/Application \
                             Support/Crosspane/Backups first; your settings, pairings and \
                             identity stay as they are."
                        } else if preview.interrupts_agent {
                            " The Crosspane that is running now is stopped first, which ends any \
                             session in progress."
                        } else {
                            ""
                        };
                        let text = format!(
                            "Install Crosspane {} for this account only: Crosspane.app goes in \
                             your Applications folder, crosspanectl in ~/.local/bin, and a sign-in \
                             item (a LaunchAgent in ~/Library/LaunchAgents) starts Crosspane now \
                             and at every sign-in.{interrupt} Files that already match are kept, \
                             and nothing outside your home folder changes.",
                            preview.version
                        );
                        self.emit(
                            job,
                            NativeOutcome::Planned { preview: text },
                            "Review this install.",
                        );
                    }
                    // A Plan job is answered with a plan or a wait; the step is checked again.
                    Ok(None) => self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::User),
                        "Nothing needs to change any more. Check again to confirm it.",
                    ),
                    Err(error) => self.emit_stop(job.clone(), install_problem(job.stage, error)),
                }
            }
            JobStage::Apply => {
                let held = self.held.install.take();
                if !Self::consented(held, &consent, &job) {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "That consent was for a different preview. Review the install again.",
                    );
                    return;
                }
                let deadline = self.deadline(APPLY_MS);
                let plan_operation = consent.map_or(job.operation, |c| c.plan);
                match self.installs.apply(plan_operation, &deadline) {
                    Ok(super::domains::InstallApplied::Requested) => {
                        let text = match self.installs.backup() {
                            Some(folder) => format!(
                                "Crosspane was installed and started. Replaced an older copy of \
                                 Crosspane (saved in {}). It is checked next.",
                                folder.display()
                            ),
                            None => {
                                "Crosspane was installed and started. It is checked next.".into()
                            }
                        };
                        self.emit(job, NativeOutcome::Applied(ApplyOutcome::Applied), text);
                    }
                    Ok(super::domains::InstallApplied::Unknown)
                    | Err(InstallError::OutcomeUnknown) => {
                        self.emit(
                            job,
                            NativeOutcome::Applied(ApplyOutcome::Unknown),
                            "What was changed is unknown, so it is checked again before anything \
                             is retried.",
                        );
                    }
                    Err(error) => self.emit_stop(job.clone(), install_problem(job.stage, error)),
                }
            }
            JobStage::Verify => {
                let deadline = self.deadline(READ_MS);
                match self.installs.verify(status, &deadline) {
                    Ok(source) => {
                        let observed_at_ms =
                            status.map_or_else(|| self.now(), |r| r.observed_at_ms);
                        self.emit(
                            job,
                            NativeOutcome::Verified {
                                source,
                                observed_at_ms,
                            },
                            "Crosspane is installed from the files setup copied, and it starts \
                             when you sign in.",
                        );
                    }
                    Err(InstallError::Unavailable | InstallError::Unobservable) => self.emit_stop(
                        job,
                        wait_contract("Crosspane isn't running from the new files yet."),
                    ),
                    Err(error) => self.emit_stop(job.clone(), install_problem(job.stage, error)),
                }
            }
        }
    }

    // ---- sound driver ------------------------------------------------------------------------

    fn audio_step(&mut self, job: JobIntent, consent: Option<Consent>) {
        if !self.supported(&job) {
            return;
        }
        let deadline = self.deadline(READ_MS);
        match job.stage {
            JobStage::Detect => match self.audio.detect(&deadline) {
                Ok(AudioState::Installed) => self.emit(
                    job,
                    NativeOutcome::Detected {
                        needs_action: false,
                    },
                    "The Crosspane sound driver is installed.",
                ),
                Ok(AudioState::Needed) => self.emit(
                    job,
                    NativeOutcome::Detected { needs_action: true },
                    "The Crosspane sound driver isn't installed yet.",
                ),
                Ok(AudioState::InProgress) => self.emit_stop(job, installer_open()),
                Err(error) => self.emit_stop(job.clone(), audio_problem(job.stage, error)),
            },
            JobStage::Plan => {
                self.held.audio = None;
                match self.audio.plan(job.operation, &deadline) {
                    Ok(preview) => {
                        self.held.audio = Some(job.operation);
                        let interrupt = if preview.interrupts_system_audio {
                            " Installing replaces the driver and briefly interrupts this Mac's \
                             sound."
                        } else {
                            ""
                        };
                        let previous = if preview.keeps_previous {
                            " An earlier copy of the driver is kept for manual recovery."
                        } else {
                            ""
                        };
                        let text = format!(
                            "Install the Crosspane sound driver {}. It is a shared component for \
                             every user on this Mac: macOS Installer opens and asks for an \
                             administrator password, which Crosspane never sees.{interrupt}\
                             {previous} The driver only carries the test sounds and the speakers \
                             you choose; your real microphone is never opened.",
                            preview.version
                        );
                        self.emit(
                            job,
                            NativeOutcome::Planned { preview: text },
                            "Review the sound driver install.",
                        );
                    }
                    Err(error) => self.emit_stop(job.clone(), audio_problem(job.stage, error)),
                }
            }
            JobStage::Apply => {
                let held = self.held.audio.take();
                if !Self::consented(held, &consent, &job) {
                    self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Refused),
                        "That consent was for a different preview. Review the sound driver again.",
                    );
                    return;
                }
                let deadline = self.deadline(APPLY_MS);
                let plan_operation = consent.map_or(job.operation, |c| c.plan);
                match self.audio.apply(plan_operation, &deadline) {
                    Ok(()) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Applied),
                        "macOS Installer was opened on the sound driver package. Finish it there \
                         with your administrator password; closing its window doesn't cancel \
                         what it already started.",
                    ),
                    Err(AudioError::Unknown) => self.emit(
                        job,
                        NativeOutcome::Applied(ApplyOutcome::Unknown),
                        "Whether the installer opened is unknown, so it is checked again before \
                         anything is retried.",
                    ),
                    Err(error) => self.emit_stop(job.clone(), audio_problem(job.stage, error)),
                }
            }
            JobStage::Verify => match self.audio.verify(&deadline) {
                Ok(source) => {
                    let observed_at_ms = self.now();
                    self.emit(
                        job,
                        NativeOutcome::Verified {
                            source,
                            observed_at_ms,
                        },
                        "The sound driver is in place. A test sound later shows it works.",
                    );
                }
                Err(error) => self.emit_stop(job.clone(), audio_problem(job.stage, error)),
            },
        }
    }

    // ---- agent -------------------------------------------------------------------------------

    /// The agent is up, matches the install, holds its key in the Keychain and has finished
    /// recovering. Evidence is the Status issued for this job, never a cached one.
    fn agent_check(status: Option<&AgentReply>) -> Result<&AgentReply, Stop> {
        let (reply, health) = health_of(status)?;
        let installer = health.installer();
        if installer.keystore != KeyStoreProvenance::OsStore {
            return Err(wait_user(
                "Crosspane isn't holding its key in the Keychain. Allow Keychain access if \
                 macOS asks, then check again.",
            ));
        }
        if installer.startup_recovery == StartupRecovery::Failed || installer.recovery_pending != 0
        {
            return Err(wait_contract(
                "Crosspane is still recovering windows from an earlier session.",
            ));
        }
        Ok(reply)
    }

    fn agent_step(&mut self, job: JobIntent, status: Option<&AgentReply>) {
        match job.stage {
            // The agent is installed by the earlier step; this step only looks at it.
            JobStage::Detect => self.emit(
                job,
                NativeOutcome::Detected {
                    needs_action: false,
                },
                "Looking for Crosspane's agent.",
            ),
            JobStage::Verify => match Self::agent_check(status) {
                Ok(reply) => self.emit(
                    job,
                    NativeOutcome::Verified {
                        source: reply.source,
                        observed_at_ms: reply.observed_at_ms,
                    },
                    "Crosspane is running with its key in the Keychain.",
                ),
                Err(stop) => self.emit_stop(job, stop),
            },
            JobStage::Plan | JobStage::Apply => self.emit(
                job,
                NativeOutcome::Failed,
                "Checking the agent never changes anything.",
            ),
        }
    }

    // ---- permissions -------------------------------------------------------------------------

    fn permissions_step(&mut self, job: JobIntent, status: Option<&AgentReply>) {
        let (reply, health) = match health_of(status) {
            Ok(ok) => ok,
            Err(stop) => {
                self.emit_stop(job, stop);
                return;
            }
        };
        let missing = missing_permissions(health);
        match job.stage {
            JobStage::Detect => {
                if missing.is_empty() {
                    self.emit(
                        job,
                        NativeOutcome::Detected {
                            needs_action: false,
                        },
                        "Crosspane already has every permission it needs.",
                    );
                } else {
                    self.emit(
                        job,
                        NativeOutcome::Detected { needs_action: true },
                        "Crosspane needs permissions in System Settings.",
                    );
                }
            }
            JobStage::Plan => {
                if missing.is_empty() {
                    // A Plan job is answered with a plan or a wait; the step is checked again.
                    self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::User),
                        "Crosspane already has every permission it needs. Check again to confirm \
                         it.",
                    );
                    return;
                }
                let places: Vec<String> = missing
                    .iter()
                    .map(|name| {
                        format!("{}: {}", permission_label(*name), pane(*name).breadcrumbs())
                    })
                    .collect();
                let microphone = if missing.contains(&PermissionName::Microphone) {
                    format!(" The microphone permission {MICROPHONE_REASON}. {MICROPHONE_DETAIL}")
                } else {
                    String::new()
                };
                self.emit(
                    job,
                    NativeOutcome::Planned {
                        preview: format!(
                            "{ASK_EXPLANATION} Turn Crosspane on in: {}. If macOS offers to quit \
                             and reopen Crosspane, accept: it restarts by itself.{microphone}",
                            places.join("; ")
                        ),
                    },
                    "Review the permission request.",
                );
            }
            // The controller asks the agent itself and never hands this worker an Apply.
            JobStage::Apply => self.emit(
                job,
                NativeOutcome::Failed,
                "Permissions are requested from the agent, not by this worker.",
            ),
            JobStage::Verify => {
                if missing.is_empty() {
                    self.emit(
                        job,
                        NativeOutcome::Verified {
                            source: reply.source,
                            observed_at_ms: reply.observed_at_ms,
                        },
                        "Crosspane reports every permission it needs.",
                    );
                } else {
                    let names: Vec<&str> = missing.iter().map(|n| permission_label(*n)).collect();
                    self.emit(
                        job,
                        NativeOutcome::Waiting(WaitKind::User),
                        format!(
                            "Waiting for you to turn Crosspane on for {}. Opening a settings pane \
                             doesn't count: setup carries on once the agent reports each one.",
                            names.join(", ")
                        ),
                    );
                }
            }
        }
    }

    // ---- uninstall ---------------------------------------------------------------------------

    fn maintenance(&mut self, request: MaintenanceRequest) {
        match request {
            MaintenanceRequest::Inspect { id } => {
                self.maintenance = Maintenance {
                    id: Some(id),
                    ..Maintenance::default()
                };
                let deadline = self.deadline(READ_MS);
                let offer = self.uninstaller.inspect(&deadline);
                let deadline = self.deadline(READ_MS);
                let repair = self.repairer.inspect(&deadline);
                self.maint(MaintenanceReport::Inspected {
                    id,
                    uninstall: offer.uninstall,
                    repair: repair.repair,
                    choices: offer.choices,
                });
                // In-memory continuation and persisted read-only reassessment share this offer.
                if let Some(lines) = repair.resumable {
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
                let deadline = self.deadline(READ_MS);
                let status = status.as_ref().map(|s| &s.0);
                match self.repairer.plan(status, operation, &deadline) {
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
                let deadline = self.deadline(REPAIR_MS);
                let status = status.as_ref().map(|s| &s.0);
                let result = self
                    .repairer
                    .confirm(status, OperationId(plan), operation, &deadline);
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
                let deadline = self.deadline(READ_MS);
                let status = status.as_ref().map(|s| &s.0);
                let step = self.repairer.verify(status, &deadline);
                self.repair_step(id, step);
            }
            MaintenanceRequest::DiscardRepair { id, .. } => {
                self.maint(MaintenanceReport::Refused {
                    id,
                    reason: "Discarding an earlier repair is not supported on this platform."
                        .into(),
                });
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
                let deadline = self.deadline(READ_MS);
                let status = status.as_ref().map(|s| &s.0);
                let result = self.repairer.resume(status, &deadline);
                self.maintenance.repair_active = false;
                match result {
                    Ok(finish) => self.repair_finished(id, finish),
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::PlanUninstall {
                id,
                choices,
                status,
            } => {
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
                let status = status.as_ref().map(|s| &s.0);
                match self
                    .uninstaller
                    .plan(&choices, operation, status, &deadline)
                {
                    Ok(preview) => {
                        self.maintenance.planned = Some((id, operation));
                        self.maint(MaintenanceReport::Planned { id, preview });
                    }
                    Err(reason) => self.maint(MaintenanceReport::Refused { id, reason }),
                }
            }
            MaintenanceRequest::ConfirmUninstall { id, status, .. } => {
                let planned = self.maintenance.planned.take();
                let Some((_, operation)) = planned.filter(|(planned_id, _)| *planned_id == id)
                else {
                    self.maint(MaintenanceReport::Refused {
                        id,
                        reason: "Confirm the current preview before removing Crosspane.".into(),
                    });
                    return;
                };
                if self.maintenance.id != Some(id) || self.maintenance.running {
                    self.maint(MaintenanceReport::Refused {
                        id,
                        reason: "Confirm the current preview before removing Crosspane.".into(),
                    });
                    return;
                }
                self.maintenance.running = true;
                let deadline = self.deadline(REMOVE_MS);
                let status = status.as_ref().map(|s| &s.0);
                let result = self.uninstaller.apply(operation, status, &deadline);
                self.maintenance.running = false;
                match result {
                    Ok(result) => self.maint(MaintenanceReport::Finished {
                        id,
                        outcome: result.outcome,
                        lines: result.lines,
                    }),
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
            MaintenanceRequest::ConfirmFollowUp { id, .. }
            | MaintenanceRequest::DeclineFollowUp { id, .. } => {
                // Removal on a Mac has no separate follow-up consent.
                self.maint(MaintenanceReport::Progress {
                    id,
                    detail: "That follow-up is no longer waiting.".into(),
                });
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

    /// A fresh maintenance operation. These live far above core operation ids and are spaced, so
    /// the removal adapter's own re-observation operation (the next id up) never meets the next
    /// one, and the removal journal always sees increasing operations.
    fn maintenance_op(&self) -> OperationId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        OperationId(
            self.op_base
                .saturating_add(500_000)
                .saturating_add(NEXT.fetch_add(1, Ordering::Relaxed).saturating_mul(1_000)),
        )
    }
}

fn installer_open() -> Stop {
    wait_user(
        "The macOS installer for the sound driver is open. Finish it there; setup carries on by \
         itself once it reports a result.",
    )
}

fn audio_problem(stage: JobStage, error: AudioError) -> Stop {
    match error {
        AudioError::Blocked(text) | AudioError::Refused(text) => refused(stage, text),
        AudioError::Failed(text) => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Failed)
            } else {
                NativeOutcome::Failed
            },
            text,
        ),
        AudioError::Waiting => installer_open(),
        AudioError::Unavailable => wait_contract(
            "The sound driver's folders or packages can't be read just now. Nothing was assumed.",
        ),
        AudioError::Unknown => wait_contract(
            "The installer's result can't be told apart from an earlier run. It is checked again \
             before anything is retried.",
        ),
    }
}

fn install_problem(stage: JobStage, error: InstallError) -> Stop {
    match error {
        InstallError::Foreign => refused(
            stage,
            "Crosspane's install folders couldn't be cleared for the new copy. Check again to \
             retry.",
        ),
        InstallError::UserDisabled => refused(
            stage,
            "Crosspane's sign-in item is turned off in System Settings > General > Login Items & \
             Extensions. Turn it on, then check again.",
        ),
        InstallError::Blocked(text) => refused(stage, text),
        InstallError::Unsupported => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Refused)
            } else {
                NativeOutcome::Unsupported
            },
            "This Mac or this build isn't supported for installing Crosspane. Nothing was changed."
                .into(),
        ),
        InstallError::Refused => refused(
            stage,
            "What was checked changed before it could be used. Nothing was changed; review again.",
        ),
        InstallError::OutcomeUnknown => wait_user(
            "The install didn't finish this time. Check again: setup stops Crosspane, saves what \
             is there in ~/Library/Application Support/Crosspane/Backups and installs fresh.",
        ),
        InstallError::Unobservable | InstallError::Unavailable => wait_contract(
            "The sign-in item or the installed files can't be read just now. Nothing was assumed.",
        ),
        InstallError::Failed => (
            if stage == JobStage::Apply {
                NativeOutcome::Applied(ApplyOutcome::Failed)
            } else {
                NativeOutcome::Failed
            },
            "The install couldn't continue. Nothing was assumed.".into(),
        ),
    }
}
