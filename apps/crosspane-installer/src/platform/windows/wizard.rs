//! The wizard's `live::Platform` implementation (L16) and the production controller `open()`.
//!
//! One worker thread answers every job the shared controller submits, exactly once, from the
//! lifecycle engine. A step answers Verified, a plain wait or a plain retryable failure; a
//! maintenance request answers its preview, its outcome or a refusal. Nothing is ever "unknown"
//! here: a state that couldn't be proved is shown as something to check again, never as a
//! permanent refusal.

use super::{
    elevated::{self, ElevatedOutcome},
    engine::{
        self, ConvergeOptions, ConvergePlan, ConvergeReport, ElevatedPolicy, Progress, Route,
        UninstallOptions, UninstallReport, text,
    },
    error::{SetupError, SetupResult},
    files::Inventory,
    identity,
    paths::{self, Paths},
    setup_log::setup_note,
    status::{self, SnapshotOptions},
    support::{self, Support},
    task::TaskHealth,
};
use crate::{
    agent_contract::{AgentCall, AgentPlatform, AgentPort, AgentReply, CallFailure, DecodedReply},
    gui::InstallerController,
    live::{
        self, Availability, CheckState, Clock, Consent, LiveController, MaintenanceId,
        MaintenanceOutcome, MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome,
        NativeRefusal, NativeReport, Platform, ReinstallOutcome, RemovalChoice, RepairOutcome,
        StatusEvidence, StepReport, SupportChecklist, SupportChecksSlot,
    },
    view::{ProgressGroup, ScreenId, ToggleRole},
};
use crosspane_installer_core::elevated::{
    FirewallState, RuleScope, Verb,
    status::StatusReport,
    step::{ElevatedPlan, PREVIEW_HEADER},
};
use crosspane_installer_core::{
    ApplyOutcome, JobIntent, JobStage, ObservationSource, OperationId, StepId, WaitKind,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const SUPPORT: StepId = StepId(10);
const INSTALL: StepId = StepId(20);
const AGENT: StepId = StepId(30);
const ADMIN: StepId = StepId(55);

/// The removal choice that also forgets Crosspane's settings and pairings.
const SETTINGS_CHOICE: u16 = 1;
/// The removal choice that also removes the firewall rule and the display driver.
const TEARDOWN_CHOICE: u16 = 2;

/// Jobs the controller may queue before `submit` says the worker is busy. The controller keeps at
/// most a handful outstanding, so a full queue means something is badly wrong.
const JOB_QUEUE: usize = 64;
/// How often the idle worker looks at its stop flag.
const IDLE_POLL_MS: u64 = 100;
/// How long a read-only check waits for the agent to answer.
const DETECT_AGENT_WAIT: Duration = Duration::from_secs(1);
/// How long Verify waits for the agent to answer.
const VERIFY_AGENT_WAIT: Duration = Duration::from_secs(5);
/// The least time between two progress reports of one operation.
const HEARTBEAT_GAP_MS: u64 = 15_000;
/// The most distinct progress lines one operation shows; later reports repeat the last one.
const MAX_DISTINCT_LINES: usize = 4;
/// Plans a maintenance screen may still confirm; older ones are forgotten.
const MAX_STORED_PLANS: usize = 8;
/// The longest detail copied into the setup log; longer text is cut.
const LOG_DETAIL_CHARS: usize = 300;

/// The answer to a job that hit a bug.
const UNEXPECTED: &str = "Setup hit an unexpected problem. Try again.";
/// The refusal when the process is elevated, impersonating or outside the user's own session.
pub(crate) const NOT_OWN_SESSION: &str = "Crosspane Setup runs only in your own signed-in Windows session, without administrator rights. Close it and open crosspane-installer.exe again normally, not with \"Run as administrator\".";
/// The refusal when no more specific reason for "can't run Crosspane here" is known.
const NOT_SUPPORTED: &str = "Setup couldn't confirm that this Windows account can run Crosspane. It needs your own signed-in session without administrator rights, with your folders on this PC's local drive. Nothing was changed.";
/// The refusal while the controller's caller is not the signed-in user.
const CALLER_REFUSED: &str = "Setup runs only in your own signed-in Windows session.";
const WORKER_STOPPED: &str = "Setup stopped working. Close it and open it again.";
const AGENT_STARTING: &str = "Crosspane isn't answering yet. Check again in a moment.";
const INSTALL_UNVERIFIED: &str =
    "Setup couldn't confirm Crosspane's files and sign-in task yet. Check again.";
const NO_CONSENT: &str = "Setup didn't get your go-ahead for this change. Nothing was changed.";
const SUPPORT_NEEDS_NO_PLAN: &str = "This check needs no plan. Check again.";
const NOTHING_TO_CHANGE: &str = "There is nothing to change here.";
const START_PREVIEW: &str = "Start Crosspane now.";
const ADMIN_PREVIEW: &str =
    "Add the Windows Defender Firewall rule and the display driver that Crosspane needs.";
const ADMIN_PREVIEW_FIREWALL: &str = "Add the Windows Defender Firewall rule that Crosspane needs.";
const ADMIN_PREVIEW_DRIVER: &str = "Add the display driver that Crosspane needs.";
const NOTHING_TO_APPROVE: &str = "Nothing needs administrator approval right now. Check again.";
/// The lines under the steps. Each says where its step stands, never just that it was looked at.
const CAN_RUN: &str = "This computer can run Crosspane.";
const INSTALLED_RUNNING: &str = "Crosspane is installed and running.";
const INSTALLED_SILENT: &str = "Crosspane is installed but isn't answering yet.";
const INSTALL_INCOMPLETE: &str = "Crosspane isn't completely installed yet.";
const INSTALL_READY: &str = "Ready to install.";
const AGENT_RUNNING: &str = "Crosspane is running.";
const AGENT_NOT_RUNNING: &str = "Crosspane isn't running yet.";
const AGENT_READY: &str = "Ready to start Crosspane.";
const INSTALL_FALLBACK_PREVIEW: &str = "Install Crosspane for this account.";
const REPAIR_FALLBACK_PREVIEW: &str = "Repair Crosspane.";
const REINSTALL_FALLBACK_PREVIEW: &str = "Reinstall all of Crosspane's files.";
const UNINSTALL_FALLBACK_PREVIEW: &str = "Remove Crosspane from this account.";
const REPAIR_STALE: &str = "The repair preview changed. Review it again.";
const REINSTALL_STALE: &str = "The reinstall preview changed. Review it again.";
const UNINSTALL_STALE: &str = "The removal preview changed. Review it again.";
const NO_EARLIER_REPAIR: &str = "There is no earlier repair to continue.";
const NOT_ON_WINDOWS: &str = "This option isn't available on Windows.";
const FILES_FAILED: &str = "Crosspane's files couldn't all be put in place.";
const TASK_FAILED: &str = "Crosspane couldn't be set to start when you sign in.";
const INSTALL_DONE: &str = "Crosspane is installed.";
const TRY_AGAIN: &str = "Try again.";
const SETTINGS_LABEL: &str = "Also remove settings and pairings";
const TEARDOWN_LABEL: &str =
    "Also remove the firewall rule and display driver (Windows asks for administrator approval)";
const TEARDOWN_NONE_LABEL: &str =
    "Also remove the firewall rule and display driver (none were found)";
const TEARDOWN_UNKNOWN_LABEL: &str =
    "Also remove the firewall rule and display driver (couldn't be checked just now)";

/// The Windows readiness graph: four native steps, in the order the screens show them.
pub(crate) fn description() -> live::PlatformDescription {
    live::PlatformDescription {
        platform: AgentPlatform::Windows,
        machine_label: "Windows PC".into(),
        steps: vec![
            live::NativeStep {
                id: SUPPORT,
                prerequisites: Vec::new(),
                required_for_installed: true,
                required_for_ready: true,
                screen: ScreenId::Compatibility,
                group: ProgressGroup::Install,
                label: "This computer can run Crosspane".into(),
                action_label: "Check this computer".into(),
                uses_status: false,
                settles_with_peer: false,
                agent_apply: None,
            },
            live::NativeStep {
                id: INSTALL,
                prerequisites: vec![SUPPORT],
                required_for_installed: true,
                required_for_ready: true,
                screen: ScreenId::InstallPlan,
                group: ProgressGroup::Install,
                label: "Crosspane is installed and starts when you sign in".into(),
                action_label: "Install".into(),
                uses_status: false,
                settles_with_peer: false,
                agent_apply: None,
            },
            live::NativeStep {
                id: AGENT,
                prerequisites: vec![INSTALL],
                required_for_installed: false,
                required_for_ready: true,
                screen: ScreenId::Installing,
                group: ProgressGroup::Install,
                label: "Crosspane is running on this PC".into(),
                action_label: "Check Crosspane".into(),
                uses_status: true,
                settles_with_peer: false,
                agent_apply: None,
            },
            live::NativeStep {
                id: ADMIN,
                prerequisites: vec![AGENT],
                required_for_installed: false,
                required_for_ready: true,
                screen: ScreenId::Network,
                group: ProgressGroup::PermissionsNetwork,
                label: "Crosspane is allowed through Windows Defender Firewall".into(),
                action_label: "Allow with administrator approval".into(),
                uses_status: false,
                settles_with_peer: true,
                agent_apply: None,
            },
        ],
        connect_after: vec![AGENT],
        ready_after: vec![AGENT, ADMIN],
        hiding_choice: false,
        resume_note: None,
    }
}

/// Builds the font definitions and the production controller; `maintenance` opens the
/// maintenance screen.
pub(crate) fn open(
    payload: Option<PathBuf>,
    maintenance: bool,
) -> anyhow::Result<(eframe::egui::FontDefinitions, Box<dyn InstallerController>)> {
    // The Limited/impersonation guard precedes the font, the worker and the port. Every failure in
    // the chain is the same refusal, and nothing else runs before it.
    if identity::native::current().is_err() {
        setup_note!("identity refused");
        anyhow::bail!(NOT_OWN_SESSION);
    }
    // An explicit --payload must be a folder. Without one, the payload folder beside the installer
    // is the default; the embedded inventory still pins every byte, so the default adds no trust.
    let payload = match payload {
        Some(folder) if folder.is_dir() => Some(folder),
        Some(folder) => anyhow::bail!(
            "The --payload folder {} doesn't exist or isn't a folder. Nothing was changed.",
            folder.display()
        ),
        None => paths::payload_beside_current_exe(),
    };
    let payload_text = payload
        .as_ref()
        .map_or_else(|| "none".to_owned(), |folder| folder.display().to_string());
    setup_note!(
        "installer start version={} payload={payload_text}",
        env!("CARGO_PKG_VERSION")
    );
    let root = std::env::var_os("SystemRoot")
        .ok_or_else(|| anyhow::anyhow!("Windows system font location unavailable"))?;
    let fonts =
        crate::gui::load_review_font(&PathBuf::from(root).join("Fonts").join("segoeui.ttf"))?;
    let started = Instant::now();
    let clock: Clock =
        Arc::new(move || started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
    let platform = WindowsPlatform::start(payload, clock.clone())?;
    let mut controller = LiveController::new(Box::new(platform), clock)?;
    if maintenance {
        controller.open_maintenance();
    }
    Ok((fonts, Box::new(controller)))
}

/// The platform the shared controller drives: a bounded job queue into one worker thread, the
/// reports it answers with, and the agent's pipe.
struct WindowsPlatform {
    jobs: Option<mpsc::SyncSender<NativeJob>>,
    reports: mpsc::Receiver<NativeReport>,
    agent: Box<dyn AgentPort>,
    checks: SupportChecksSlot,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for WindowsPlatform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsPlatform").finish_non_exhaustive()
    }
}

impl WindowsPlatform {
    /// Starts the worker and the agent's pipe port. `clock` is the controller's own clock.
    fn start(payload: Option<PathBuf>, clock: Clock) -> anyhow::Result<Self> {
        let (jobs, queue) = mpsc::sync_channel(JOB_QUEUE);
        let (out, reports) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let checks = SupportChecksSlot::new(SUPPORT);
        // Folders that can't be used are a SUPPORT finding the person can read, not a crash: the
        // agent port simply has nobody to ask.
        let agent: Box<dyn AgentPort> = match Paths::current() {
            Ok(paths) => Box::new(super::agent_pipe::PipePort::start(paths, clock.clone())?),
            Err(error) => {
                setup_note!("no agent port: {}", error.text);
                Box::new(NoAgent)
            }
        };
        let worker = Worker {
            out,
            clock,
            payload,
            checks: checks.clone(),
            step_plans: BTreeMap::new(),
            book: PlanBook::new(),
        };
        let worker_stop = stop.clone();
        std::thread::Builder::new()
            .name("crosspane-windows-wizard".into())
            .spawn(move || worker.run(queue, worker_stop))?;
        Ok(Self {
            jobs: Some(jobs),
            reports,
            agent,
            checks,
            stop,
        })
    }
}

impl Platform for WindowsPlatform {
    fn describe(&self) -> live::PlatformDescription {
        description()
    }

    fn submit(&mut self, job: NativeJob) -> Result<(), NativeRefusal> {
        identity::native::refuse_impersonation()
            .map_err(|_| NativeRefusal::Unavailable(CALLER_REFUSED.into()))?;
        if let NativeJob::Step { job, .. } = &job
            && ![SUPPORT, INSTALL, AGENT, ADMIN].contains(&job.step)
        {
            return Err(NativeRefusal::UnknownStep);
        }
        let stopped = || NativeRefusal::Unavailable(WORKER_STOPPED.into());
        self.jobs
            .as_ref()
            .ok_or_else(stopped)?
            .try_send(job)
            .map_err(|failure| match failure {
                mpsc::TrySendError::Full(_) => NativeRefusal::Busy,
                mpsc::TrySendError::Disconnected(_) => stopped(),
            })
    }

    fn poll(&mut self) -> Vec<NativeReport> {
        self.reports.try_iter().collect()
    }

    fn agent(&mut self) -> &mut dyn AgentPort {
        self.agent.as_mut()
    }

    fn shutdown(&mut self) {
        // No join and no cancellation: a change that is already running finishes on its own.
        self.stop.store(true, Ordering::Release);
        self.jobs.take();
    }

    fn support_checks(&mut self) -> Option<SupportChecklist> {
        self.checks.latest()
    }
}

/// The agent port of a PC whose folders can't be used: every call is refused.
#[derive(Debug)]
struct NoAgent;

impl AgentPort for NoAgent {
    fn submit(&mut self, _call: AgentCall) -> Result<(), CallFailure> {
        Err(CallFailure::Unavailable)
    }

    fn poll(&mut self) -> Vec<AgentReply> {
        Vec::new()
    }
}

// ---- the worker ------------------------------------------------------------------------------

/// What a step job answers: the outcome and the detail line shown with it.
#[derive(Debug, PartialEq)]
struct Answer {
    outcome: NativeOutcome,
    detail: String,
}

impl Answer {
    fn new(outcome: NativeOutcome, detail: impl Into<String>) -> Self {
        Self {
            outcome,
            detail: detail.into(),
        }
    }
}

/// The plan the backend answered last for one step. Apply runs exactly this and nothing else.
#[derive(Debug)]
enum StepPlan {
    Install(ConvergePlan),
    Start,
    Admin(ElevatedPlan),
}

/// The plan a maintenance screen may confirm.
#[derive(Debug)]
enum Stored {
    Uninstall {
        remove_settings: bool,
        teardown: Option<ElevatedPlan>,
    },
    Repair {
        plan: ConvergePlan,
        payload: Option<PathBuf>,
    },
    Reinstall {
        plan: ConvergePlan,
        payload: PathBuf,
    },
}

/// The answered plans of maintenance screens, each with its number. Numbers start at 1 and never
/// repeat, so a confirmation of an older preview can't match a newer one.
#[derive(Debug)]
struct PlanBook<T> {
    next: u64,
    plans: BTreeMap<MaintenanceId, (u64, T)>,
}

impl<T> PlanBook<T> {
    fn new() -> Self {
        Self {
            next: 1,
            plans: BTreeMap::new(),
        }
    }

    /// Remembers `value` as the plan of `id` and returns its number.
    fn store(&mut self, id: MaintenanceId, value: T) -> u64 {
        let number = self.next;
        self.next = self.next.saturating_add(1);
        self.plans.insert(id, (number, value));
        while self.plans.len() > MAX_STORED_PLANS {
            self.plans.pop_first();
        }
        number
    }

    /// Hands out the plan of `id` once: it must carry `number` (when one is named) and be of the
    /// kind `accept` wants. Anything else leaves the stored plan alone.
    fn take_where(
        &mut self,
        id: MaintenanceId,
        number: Option<u64>,
        accept: impl Fn(&T) -> bool,
    ) -> Option<T> {
        let matches = self.plans.get(&id).is_some_and(|(stored, value)| {
            number.is_none_or(|wanted| wanted == *stored) && accept(value)
        });
        if matches {
            self.plans.remove(&id).map(|(_, value)| value)
        } else {
            None
        }
    }
}

struct Worker {
    out: mpsc::Sender<NativeReport>,
    clock: Clock,
    payload: Option<PathBuf>,
    checks: SupportChecksSlot,
    /// Per step, the Plan operation the backend answered last and what it planned.
    step_plans: BTreeMap<StepId, (OperationId, StepPlan)>,
    book: PlanBook<Stored>,
}

impl Worker {
    fn run(mut self, queue: mpsc::Receiver<NativeJob>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::Acquire) {
            match queue.recv_timeout(Duration::from_millis(IDLE_POLL_MS)) {
                Ok(job) => {
                    let report = self.answer(job);
                    log_report(&report);
                    if self.out.send(report).is_err() {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Answers `job` exactly once, even when answering panics.
    fn answer(&mut self, job: NativeJob) -> NativeReport {
        let fallback = fallback_report(&job);
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.dispatch(job))) {
            Ok(report) => report,
            Err(_) => {
                setup_note!("a job hit a bug and was answered as a plain failure");
                fallback
            }
        }
    }

    fn dispatch(&mut self, job: NativeJob) -> NativeReport {
        match job {
            NativeJob::Step {
                job,
                consent,
                status,
            } => {
                let answer = self.step(&job, consent, status.as_ref());
                NativeReport::Step(StepReport {
                    job,
                    outcome: answer.outcome,
                    detail: answer.detail,
                })
            }
            NativeJob::Maintenance(request) => NativeReport::Maintenance(self.maintenance(request)),
        }
    }

    fn verified_now(&self) -> NativeOutcome {
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            observed_at_ms: (self.clock)(),
        }
    }

    fn options(&self, route: Route, elevated: ElevatedPolicy) -> ConvergeOptions {
        ConvergeOptions {
            payload: self.payload.clone(),
            route,
            elevated,
        }
    }

    /// Progress lines of a step job: the same job, outcome `Progress`.
    fn step_heartbeat(&self, job: &JobIntent) -> Heartbeat {
        let job = job.clone();
        Heartbeat::new(self.out.clone(), self.clock.clone(), move |detail| {
            NativeReport::Step(StepReport {
                job: job.clone(),
                outcome: NativeOutcome::Progress,
                detail,
            })
        })
    }

    /// Progress lines of a maintenance request.
    fn maintenance_heartbeat(&self, id: MaintenanceId) -> Heartbeat {
        Heartbeat::new(self.out.clone(), self.clock.clone(), move |detail| {
            NativeReport::Maintenance(MaintenanceReport::Progress { id, detail })
        })
    }

    /// The stored plan a consent covers, handed out once: the consent must be for this Apply and
    /// name the Plan the backend answered last.
    fn consented(&mut self, job: &JobIntent, consent: Option<Consent>) -> Option<StepPlan> {
        let planned = self
            .step_plans
            .get(&job.step)
            .map(|(operation, _)| *operation);
        if !consent_matches(consent, job, planned) {
            return None;
        }
        self.step_plans.remove(&job.step).map(|(_, plan)| plan)
    }

    // ---- steps ----------------------------------------------------------------------------

    fn step(
        &mut self,
        job: &JobIntent,
        consent: Option<Consent>,
        status: Option<&StatusEvidence>,
    ) -> Answer {
        match job.step {
            SUPPORT => self.support_step(job),
            INSTALL => self.install_step(job, consent),
            AGENT => self.agent_step(job, consent, status),
            ADMIN => self.admin_step(job, consent),
            _ => Answer::new(NativeOutcome::Failed, UNEXPECTED),
        }
    }

    fn support_step(&mut self, job: &JobIntent) -> Answer {
        match job.stage {
            JobStage::Detect | JobStage::Verify => {
                let found = support::check(self.payload.as_deref());
                // A pass is published before the report that ends its job.
                self.checks.publish(found.checks.clone());
                match verdict(&found) {
                    Verdict::Supported if job.stage == JobStage::Detect => Answer::new(
                        NativeOutcome::Detected {
                            needs_action: false,
                        },
                        CAN_RUN,
                    ),
                    Verdict::Supported => Answer::new(self.verified_now(), CAN_RUN),
                    Verdict::No(reason) => {
                        Answer::new(NativeOutcome::Waiting(WaitKind::User), reason)
                    }
                    Verdict::Unreadable(reason) => {
                        Answer::new(NativeOutcome::Waiting(WaitKind::Contract), reason)
                    }
                }
            }
            JobStage::Plan => Answer::new(
                NativeOutcome::Waiting(WaitKind::Contract),
                SUPPORT_NEEDS_NO_PLAN,
            ),
            JobStage::Apply => Answer::new(NativeOutcome::NotSubmitted, NOTHING_TO_CHANGE),
        }
    }

    fn install_step(&mut self, job: &JobIntent, consent: Option<Consent>) -> Answer {
        match job.stage {
            JobStage::Detect | JobStage::Verify => {
                let (paths, inventory) = match environment() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                let verifying = job.stage == JobStage::Verify;
                let snapshot = status::snapshot(
                    &paths,
                    &inventory,
                    self.payload.as_deref(),
                    SnapshotOptions {
                        elevated: false,
                        agent_timeout: if verifying {
                            VERIFY_AGENT_WAIT
                        } else {
                            DETECT_AGENT_WAIT
                        },
                    },
                );
                if verifying {
                    install_verify(
                        snapshot.files_ok,
                        snapshot.task == TaskHealth::Healthy,
                        snapshot.agent.answering,
                        (self.clock)(),
                    )
                } else {
                    install_detected(snapshot.converged(), snapshot.agent.answering)
                }
            }
            JobStage::Plan => {
                self.step_plans.remove(&INSTALL);
                let (paths, inventory) = match environment() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                let options = self.options(Route::Install, ElevatedPolicy::IfNeeded);
                match install_plan(engine::plan(&paths, &inventory, &options)) {
                    Ok((preview, plan)) => {
                        self.step_plans
                            .insert(INSTALL, (job.operation, StepPlan::Install(plan)));
                        Answer::new(NativeOutcome::Planned { preview }, INSTALL_READY)
                    }
                    Err(refusal) => refusal,
                }
            }
            JobStage::Apply => {
                let Some(StepPlan::Install(plan)) = self.consented(job, consent) else {
                    return Answer::new(NativeOutcome::NotSubmitted, NO_CONSENT);
                };
                let (paths, inventory) = match environment() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                let options = self.options(Route::Install, policy(&plan));
                let beat = self.step_heartbeat(job);
                install_answer(engine::converge(&paths, &inventory, &options, &beat))
            }
        }
    }

    fn agent_step(
        &mut self,
        job: &JobIntent,
        consent: Option<Consent>,
        status: Option<&StatusEvidence>,
    ) -> Answer {
        match job.stage {
            JobStage::Detect => {
                let running = status_answered(status).is_some();
                Answer::new(
                    NativeOutcome::Detected {
                        needs_action: !running,
                    },
                    if running {
                        AGENT_RUNNING
                    } else {
                        AGENT_NOT_RUNNING
                    },
                )
            }
            JobStage::Plan => {
                self.step_plans
                    .insert(AGENT, (job.operation, StepPlan::Start));
                Answer::new(
                    NativeOutcome::Planned {
                        preview: START_PREVIEW.into(),
                    },
                    AGENT_READY,
                )
            }
            JobStage::Apply => {
                let Some(StepPlan::Start) = self.consented(job, consent) else {
                    return Answer::new(NativeOutcome::NotSubmitted, NO_CONSENT);
                };
                let paths = match Paths::current() {
                    Ok(paths) => paths,
                    Err(error) => return unavailable(job.stage, error.text),
                };
                let beat = self.step_heartbeat(job);
                start_answer(engine::start(&paths, &beat))
            }
            JobStage::Verify => match status_answered(status) {
                Some((source, observed_at_ms)) => Answer::new(
                    NativeOutcome::Verified {
                        source,
                        observed_at_ms,
                    },
                    AGENT_RUNNING,
                ),
                None => Answer::new(NativeOutcome::Waiting(WaitKind::Contract), AGENT_STARTING),
            },
        }
    }

    fn admin_step(&mut self, job: &JobIntent, consent: Option<Consent>) -> Answer {
        match job.stage {
            JobStage::Detect | JobStage::Verify => {
                let (paths, scope) = match admin_scope() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                match elevated::observe(&paths, self.payload.as_deref(), &scope) {
                    Ok(report) if job.stage == JobStage::Detect => Answer::new(
                        NativeOutcome::Detected {
                            needs_action: elevated::setup_needed(&report),
                        },
                        elevated::plain_status(&report),
                    ),
                    Ok(report) => admin_verify(&report, (self.clock)()),
                    Err(why) => {
                        setup_note!("administrator step not checked: {why}");
                        Answer::new(NativeOutcome::Waiting(WaitKind::Contract), why)
                    }
                }
            }
            JobStage::Plan => {
                self.step_plans.remove(&ADMIN);
                let (paths, scope) = match admin_scope() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                // The plan names what is missing now, so the prompt never offers the driver when
                // only the firewall rule is.
                let report = match elevated::observe(&paths, self.payload.as_deref(), &scope) {
                    Ok(report) => report,
                    Err(why) => {
                        setup_note!("administrator step not checked: {why}");
                        return Answer::new(NativeOutcome::Waiting(WaitKind::Contract), why);
                    }
                };
                match admin_plan(&report, scope) {
                    Ok((plan, preview, detail)) => {
                        self.step_plans
                            .insert(ADMIN, (job.operation, StepPlan::Admin(plan)));
                        Answer::new(NativeOutcome::Planned { preview }, detail)
                    }
                    Err(answer) => answer,
                }
            }
            JobStage::Apply => {
                let Some(StepPlan::Admin(plan)) = self.consented(job, consent) else {
                    return Answer::new(NativeOutcome::NotSubmitted, NO_CONSENT);
                };
                let (paths, inventory) = match environment() {
                    Ok(found) => found,
                    Err(why) => return unavailable(job.stage, why),
                };
                let Some(kit) = inventory.kit() else {
                    return Answer::new(NativeOutcome::NotSubmitted, text::NO_INVENTORY);
                };
                let beat = self.step_heartbeat(job);
                admin_answer(elevated::run(&paths, kit, &plan, &beat), plan.verb())
            }
        }
    }

    // ---- maintenance -----------------------------------------------------------------------

    fn maintenance(&mut self, request: MaintenanceRequest) -> MaintenanceReport {
        let id = request_id(&request);
        match request {
            MaintenanceRequest::Inspect { .. } => self.inspect(id),
            MaintenanceRequest::PlanUninstall { choices, .. } => self.plan_uninstall(id, &choices),
            MaintenanceRequest::ConfirmUninstall { .. } => self.confirm_uninstall(id),
            MaintenanceRequest::PlanRepair { .. } => self.plan_repair(id),
            MaintenanceRequest::ConfirmRepair { plan, .. } => self.confirm_repair(id, plan),
            MaintenanceRequest::PlanReinstall { .. } => self.plan_reinstall(id),
            MaintenanceRequest::ConfirmReinstall { plan, .. } => self.confirm_reinstall(id, plan),
            MaintenanceRequest::VerifyRepair { .. }
            | MaintenanceRequest::DiscardRepair { .. }
            | MaintenanceRequest::ResumeRepair { .. } => refused(id, NO_EARLIER_REPAIR),
            MaintenanceRequest::ConfirmFollowUp { .. }
            | MaintenanceRequest::DeclineFollowUp { .. } => refused(id, NOT_ON_WINDOWS),
        }
    }

    fn inspect(&mut self, id: MaintenanceId) -> MaintenanceReport {
        let paths = match Paths::current() {
            Ok(paths) => paths,
            Err(error) => {
                let away = Availability::Unavailable(error.text);
                return MaintenanceReport::Inspected {
                    id,
                    uninstall: away.clone(),
                    repair: away,
                    choices: Vec::new(),
                };
            }
        };
        let installed = engine::is_installed(&paths);
        let availability = if installed {
            Availability::Available
        } else {
            Availability::Unavailable(text::NOT_INSTALLED.to_owned())
        };
        // Nothing to remove while nothing is installed, and no reason to start the helper.
        let teardown = if installed {
            self.teardown_state(&paths)
        } else {
            Teardown::Nothing
        };
        MaintenanceReport::Inspected {
            id,
            uninstall: availability.clone(),
            repair: availability,
            choices: removal_choices(teardown),
        }
    }

    /// Whether the firewall rule or the display driver is there to remove.
    fn teardown_state(&self, paths: &Paths) -> Teardown {
        let report = elevated::scope(paths)
            .map_err(|error| error.text)
            .and_then(|scope| elevated::observe(paths, self.payload.as_deref(), &scope));
        match report {
            Ok(report) if elevated::teardown_needed(&report) => Teardown::Present,
            Ok(_) => Teardown::Nothing,
            Err(why) => {
                setup_note!("inspect: firewall rule and driver not checked: {why}");
                Teardown::Unknown
            }
        }
    }

    fn plan_uninstall(&mut self, id: MaintenanceId, choices: &[(u16, bool)]) -> MaintenanceReport {
        let paths = match Paths::current() {
            Ok(paths) => paths,
            Err(error) => return refused(id, error.text),
        };
        let remove_settings = choice_on(choices, SETTINGS_CHOICE);
        let teardown = choice_on(choices, TEARDOWN_CHOICE);
        match engine::plan_uninstall(&paths, remove_settings, teardown) {
            Ok((preview, teardown)) => {
                let preview = with_elevated_block(
                    non_empty(preview, UNINSTALL_FALLBACK_PREVIEW),
                    teardown.as_ref(),
                );
                self.book.store(
                    id,
                    Stored::Uninstall {
                        remove_settings,
                        teardown,
                    },
                );
                MaintenanceReport::Planned { id, preview }
            }
            Err(error) => refused(id, error.text),
        }
    }

    fn confirm_uninstall(&mut self, id: MaintenanceId) -> MaintenanceReport {
        let Some(Stored::Uninstall {
            remove_settings,
            teardown,
        }) = self.book.take_where(id, None, |stored| {
            matches!(stored, Stored::Uninstall { .. })
        })
        else {
            return refused(id, UNINSTALL_STALE);
        };
        let (paths, inventory) = match environment() {
            Ok(found) => found,
            Err(why) => return refused(id, why),
        };
        let options = UninstallOptions {
            remove_settings,
            teardown: teardown.map_or(ElevatedPolicy::Never, ElevatedPolicy::Consented),
        };
        let beat = self.maintenance_heartbeat(id);
        uninstall_report(id, engine::uninstall(&paths, &inventory, &options, &beat))
    }

    fn plan_repair(&mut self, id: MaintenanceId) -> MaintenanceReport {
        let (paths, inventory) = match environment() {
            Ok(found) => found,
            Err(why) => return refused(id, why),
        };
        // An explicit payload wins; otherwise the folder beside this installer, if there is one.
        let payload = self
            .payload
            .clone()
            .or_else(paths::payload_beside_current_exe);
        let options = ConvergeOptions {
            payload: payload.clone(),
            route: Route::Repair,
            elevated: ElevatedPolicy::IfNeeded,
        };
        match engine::plan(&paths, &inventory, &options) {
            Ok(plan) => {
                let preview = non_empty(plan.preview(Route::Repair), REPAIR_FALLBACK_PREVIEW);
                let number = self.book.store(id, Stored::Repair { plan, payload });
                MaintenanceReport::RepairPlanned {
                    id,
                    plan: number,
                    preview,
                }
            }
            Err(error) => refused(id, error.text),
        }
    }

    fn confirm_repair(&mut self, id: MaintenanceId, number: u64) -> MaintenanceReport {
        let Some(Stored::Repair { plan, payload }) =
            self.book.take_where(id, Some(number), |stored| {
                matches!(stored, Stored::Repair { .. })
            })
        else {
            return refused(id, REPAIR_STALE);
        };
        let (paths, inventory) = match environment() {
            Ok(found) => found,
            Err(why) => return refused(id, why),
        };
        let options = ConvergeOptions {
            payload,
            route: Route::Repair,
            elevated: policy(&plan),
        };
        let beat = self.maintenance_heartbeat(id);
        repair_report(
            id,
            engine::converge(&paths, &inventory, &options, &beat),
            &plan,
        )
    }

    fn plan_reinstall(&mut self, id: MaintenanceId) -> MaintenanceReport {
        // A complete reinstall copies every file again, so it needs the downloaded files.
        let Some(payload) = self
            .payload
            .clone()
            .or_else(paths::payload_beside_current_exe)
        else {
            return refused(id, text::NEEDS_DOWNLOADED_INSTALLER);
        };
        let (paths, inventory) = match environment() {
            Ok(found) => found,
            Err(why) => return refused(id, why),
        };
        let options = ConvergeOptions {
            payload: Some(payload.clone()),
            route: Route::CompleteReinstall,
            elevated: ElevatedPolicy::IfNeeded,
        };
        match engine::plan(&paths, &inventory, &options) {
            Ok(plan) => {
                let preview = non_empty(
                    plan.preview(Route::CompleteReinstall),
                    REINSTALL_FALLBACK_PREVIEW,
                );
                let number = self.book.store(id, Stored::Reinstall { plan, payload });
                MaintenanceReport::ReinstallPlanned {
                    id,
                    plan: number,
                    preview,
                }
            }
            Err(error) => refused(id, error.text),
        }
    }

    fn confirm_reinstall(&mut self, id: MaintenanceId, number: u64) -> MaintenanceReport {
        let Some(Stored::Reinstall { plan, payload }) =
            self.book.take_where(id, Some(number), |stored| {
                matches!(stored, Stored::Reinstall { .. })
            })
        else {
            return refused(id, REINSTALL_STALE);
        };
        let (paths, inventory) = match environment() {
            Ok(found) => found,
            Err(why) => return refused(id, why),
        };
        let options = ConvergeOptions {
            payload: Some(payload),
            route: Route::CompleteReinstall,
            elevated: policy(&plan),
        };
        let beat = self.maintenance_heartbeat(id);
        reinstall_report(id, engine::converge(&paths, &inventory, &options, &beat))
    }
}

// ---- progress heartbeats ---------------------------------------------------------------------

/// Decides which progress lines go out: the first at once, then at most one per
/// `HEARTBEAT_GAP_MS`, and never more than `MAX_DISTINCT_LINES` different lines (after that the
/// last line is repeated as a keep-alive). Every report keeps the window's close guard from
/// firing, and the first lines tell the person what is happening.
#[derive(Debug)]
struct Throttle {
    last_sent_ms: Option<u64>,
    lines: Vec<String>,
}

impl Throttle {
    fn new() -> Self {
        Self {
            last_sent_ms: None,
            lines: Vec::new(),
        }
    }

    /// The line to send at `now_ms` for `text`, if one goes out now.
    fn admit(&mut self, now_ms: u64, text: &str) -> Option<String> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if self
            .last_sent_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < HEARTBEAT_GAP_MS)
        {
            return None;
        }
        self.last_sent_ms = Some(now_ms);
        let known = self.lines.iter().any(|line| line == text);
        if known || self.lines.len() < MAX_DISTINCT_LINES {
            if !known {
                self.lines.push(text.to_owned());
            }
            Some(text.to_owned())
        } else {
            self.lines.last().cloned()
        }
    }
}

/// The `Progress` handed to every long engine call: throttled lines become reports.
struct Heartbeat {
    out: mpsc::Sender<NativeReport>,
    clock: Clock,
    wrap: Box<dyn Fn(String) -> NativeReport + Send + Sync>,
    throttle: Mutex<Throttle>,
}

impl std::fmt::Debug for Heartbeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Heartbeat").finish_non_exhaustive()
    }
}

impl Heartbeat {
    fn new(
        out: mpsc::Sender<NativeReport>,
        clock: Clock,
        wrap: impl Fn(String) -> NativeReport + Send + Sync + 'static,
    ) -> Self {
        Self {
            out,
            clock,
            wrap: Box::new(wrap),
            throttle: Mutex::new(Throttle::new()),
        }
    }
}

impl Progress for Heartbeat {
    fn step(&self, text: &str) {
        let now = (self.clock)();
        let line = self
            .throttle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .admit(now, text);
        if let Some(line) = line {
            // A closed window has no receiver; the operation itself carries on.
            let _ = self.out.send((self.wrap)(line));
        }
    }
}

// ---- pure decisions --------------------------------------------------------------------------

/// Whether `consent` covers this Apply: it must be for this very operation and name the Plan
/// operation the backend answered last (`planned`).
fn consent_matches(
    consent: Option<Consent>,
    job: &JobIntent,
    planned: Option<OperationId>,
) -> bool {
    matches!(
        (consent, planned),
        (Some(consent), Some(planned))
            if consent.operation == job.operation && consent.plan == planned
    )
}

/// The administrator consent a stored plan carries into the engine: the step's own plan when it
/// asked for one, otherwise none.
fn policy(plan: &ConvergePlan) -> ElevatedPolicy {
    plan.elevated
        .clone()
        .map_or(ElevatedPolicy::Never, ElevatedPolicy::Consented)
}

/// The folders and the approved inventory every engine call needs, or why they aren't usable.
fn environment() -> Result<(Paths, Inventory), String> {
    let paths = Paths::current().map_err(|error| error.text)?;
    let inventory = Inventory::embedded().map_err(|error| error.text)?;
    Ok((paths, inventory))
}

/// The folders and the install's firewall rule scope, or why they aren't usable.
fn admin_scope() -> Result<(Paths, RuleScope), String> {
    let paths = Paths::current().map_err(|error| error.text)?;
    let scope = elevated::scope(&paths).map_err(|error| error.text)?;
    Ok((paths, scope))
}

/// What a stage answers when the system couldn't be looked at: nothing was changed, so a read
/// waits and a change is not submitted.
fn unavailable(stage: JobStage, why: String) -> Answer {
    let outcome = match stage {
        JobStage::Detect | JobStage::Verify => NativeOutcome::Waiting(WaitKind::Contract),
        JobStage::Plan => NativeOutcome::Failed,
        JobStage::Apply => NativeOutcome::NotSubmitted,
    };
    Answer::new(outcome, why)
}

/// `text`, or `fallback` when it is blank (a blank preview would leave a screen waiting).
fn non_empty(text: String, fallback: &str) -> String {
    if text.trim().is_empty() {
        fallback.to_owned()
    } else {
        text
    }
}

/// `preview`, plus the administrator block of `plan` when the preview doesn't carry it yet.
fn with_elevated_block(preview: String, plan: Option<&ElevatedPlan>) -> String {
    match plan {
        Some(plan) if !preview.lines().any(|line| line.starts_with(PREVIEW_HEADER)) => {
            format!("{preview}\n\n{}", plan.preview_block())
        }
        _ => preview,
    }
}

/// Whether the removal choice `id` is checked.
fn choice_on(choices: &[(u16, bool)], id: u16) -> bool {
    choices.iter().any(|(choice, on)| *choice == id && *on)
}

/// The SUPPORT step's verdict, from the rows `support::check` reported: a `Failed` row is a
/// definite no; an unsupported machine without one is a fact that couldn't be read.
#[derive(Debug, PartialEq)]
enum Verdict {
    Supported,
    No(String),
    Unreadable(String),
}

fn verdict(found: &Support) -> Verdict {
    if found.supported {
        return Verdict::Supported;
    }
    let row = |wanted: fn(&CheckState) -> Option<&String>| {
        found.checks.iter().find_map(|check| wanted(&check.state))
    };
    let failed = row(|state| match state {
        CheckState::Failed(why) => Some(why),
        _ => None,
    });
    let unconfirmed = row(|state| match state {
        CheckState::Unconfirmed(why) => Some(why),
        _ => None,
    });
    let reason = |other: Option<&String>| {
        found
            .reason
            .clone()
            .or_else(|| other.cloned())
            .unwrap_or_else(|| NOT_SUPPORTED.to_owned())
    };
    match failed {
        Some(why) => Verdict::No(reason(Some(why))),
        None => Verdict::Unreadable(reason(unconfirmed)),
    }
}

/// The agent's Status answer carried by `evidence`, if it is a live reply that shows the agent
/// running: its source and the time it was observed.
fn status_answered(evidence: Option<&StatusEvidence>) -> Option<(ObservationSource, u64)> {
    let reply = &evidence?.0;
    (reply.source == ObservationSource::Live
        && matches!(&reply.result, Ok(DecodedReply::Status(_))))
    .then_some((reply.source, reply.observed_at_ms))
}

/// INSTALL Detect: nothing to do once everything is in place and Crosspane answers; the line says
/// which of the two it is that's missing.
fn install_detected(converged: bool, agent_answering: bool) -> Answer {
    let line = match (converged, agent_answering) {
        (true, true) => INSTALLED_RUNNING,
        (true, false) => INSTALLED_SILENT,
        (false, _) => INSTALL_INCOMPLETE,
    };
    Answer::new(
        NativeOutcome::Detected {
            needs_action: !(converged && agent_answering),
        },
        line,
    )
}

/// INSTALL Verify: files, task and agent must all be good; an agent that is only slow to answer
/// is "installed but not started yet", never a permanent failure.
fn install_verify(files_ok: bool, task_healthy: bool, agent_answering: bool, now: u64) -> Answer {
    let waiting = NativeOutcome::Waiting(WaitKind::Contract);
    if !(files_ok && task_healthy) {
        Answer::new(waiting, INSTALL_UNVERIFIED)
    } else if !agent_answering {
        Answer::new(waiting, text::NOT_STARTED_YET)
    } else {
        Answer::new(
            NativeOutcome::Verified {
                source: ObservationSource::Live,
                observed_at_ms: now,
            },
            INSTALLED_RUNNING,
        )
    }
}

/// INSTALL Plan: the preview to show and the plan to keep, or the plain reason nothing can be
/// planned. Without the downloaded files, files that are missing or damaged can't be put back, so
/// installing is refused up front instead of leaving a half-finished install.
fn install_plan(result: SetupResult<ConvergePlan>) -> Result<(String, ConvergePlan), Answer> {
    match result {
        Ok(plan) if !plan.unrestorable.is_empty() => Err(Answer::new(
            NativeOutcome::Failed,
            text::NEEDS_DOWNLOADED_INSTALLER,
        )),
        Ok(plan) => Ok((
            non_empty(plan.preview(Route::Install), INSTALL_FALLBACK_PREVIEW),
            plan,
        )),
        Err(error) => Err(Answer::new(NativeOutcome::Failed, error.text)),
    }
}

/// INSTALL Apply: files and task in place is Applied (the agent may still be starting); an error
/// before any change leaves the consent unspent.
fn install_answer(result: SetupResult<ConvergeReport>) -> Answer {
    match result {
        Ok(report) if report.files_ok && report.task_ok => Answer::new(
            NativeOutcome::Applied(ApplyOutcome::Applied),
            joined(&success_lines(report.lines, INSTALL_DONE)),
        ),
        Ok(report) => Answer::new(
            NativeOutcome::Applied(ApplyOutcome::Failed),
            joined(&failure_lines(&report)),
        ),
        Err(error) => error_answer(error),
    }
}

/// AGENT Apply: the agent answers within the engine's wait, or the start failed.
fn start_answer(result: SetupResult<bool>) -> Answer {
    match result {
        Ok(true) => Answer::new(NativeOutcome::Applied(ApplyOutcome::Applied), AGENT_RUNNING),
        Ok(false) => Answer::new(
            NativeOutcome::Applied(ApplyOutcome::Failed),
            text::NOT_STARTED_YET,
        ),
        Err(error) => error_answer(error),
    }
}

/// An engine error as a step's Apply answer: before any change nothing was submitted; after one
/// the step failed.
fn error_answer(error: SetupError) -> Answer {
    if error.changed {
        Answer::new(NativeOutcome::Applied(ApplyOutcome::Failed), error.text)
    } else {
        Answer::new(NativeOutcome::NotSubmitted, error.text)
    }
}

/// ADMIN Plan: the plan for what `report` shows is missing (and only that), its preview, and the
/// line under the step. When nothing needs approval any more, or the plan can't be made, the
/// answer to give instead.
fn admin_plan(
    report: &StatusReport,
    scope: RuleScope,
) -> Result<(ElevatedPlan, String, String), Answer> {
    let detail = elevated::plain_status(report);
    let Some(verb) = elevated::setup_verb(report, scope) else {
        return Err(Answer::new(
            NativeOutcome::Waiting(WaitKind::Contract),
            NOTHING_TO_APPROVE,
        ));
    };
    let plan =
        elevated::plan(verb).map_err(|error| Answer::new(NativeOutcome::Failed, error.text))?;
    let preview = format!("{}\n\n{}", admin_preview(plan.verb()), plan.preview_block());
    Ok((plan, preview, detail))
}

/// The sentence above the approval lines, naming what `verb` adds.
fn admin_preview(verb: &Verb) -> &'static str {
    match verb {
        Verb::AddFirewall(_) => ADMIN_PREVIEW_FIREWALL,
        Verb::InstallDriver => ADMIN_PREVIEW_DRIVER,
        _ => ADMIN_PREVIEW,
    }
}

/// ADMIN Apply: a declined prompt leaves Crosspane installed without what was asked for, which is
/// not a failure of the install. `verb` is the step that was shown.
fn admin_answer(outcome: ElevatedOutcome, verb: &Verb) -> Answer {
    match outcome {
        ElevatedOutcome::NotNeeded => Answer::new(
            NativeOutcome::Applied(ApplyOutcome::Applied),
            "Already set up.",
        ),
        ElevatedOutcome::Configured(summary) => {
            Answer::new(NativeOutcome::Applied(ApplyOutcome::Applied), summary)
        }
        ElevatedOutcome::Declined => Answer::new(
            NativeOutcome::Applied(ApplyOutcome::Refused),
            elevated::without_setup_note(verb),
        ),
        ElevatedOutcome::Failed(why) => {
            Answer::new(NativeOutcome::Applied(ApplyOutcome::Failed), why)
        }
    }
}

/// ADMIN Verify: the firewall rule is there, or the person is told what is missing.
fn admin_verify(report: &StatusReport, now: u64) -> Answer {
    let line = elevated::plain_status(report);
    if report.firewall.state == FirewallState::Present {
        Answer::new(
            NativeOutcome::Verified {
                source: ObservationSource::Live,
                observed_at_ms: now,
            },
            line,
        )
    } else {
        Answer::new(NativeOutcome::Waiting(WaitKind::User), line)
    }
}

/// `lines`, or just `fallback` when the engine had nothing to say.
fn success_lines(lines: Vec<String>, fallback: &str) -> Vec<String> {
    if lines.is_empty() {
        vec![fallback.to_owned()]
    } else {
        lines
    }
}

/// What a converge that didn't finish says. The engine's lines name each problem; with none, the
/// first missing part does.
fn failure_lines(report: &ConvergeReport) -> Vec<String> {
    if !report.lines.is_empty() {
        return report.lines.clone();
    }
    vec![
        if !report.files_ok {
            FILES_FAILED
        } else if !report.task_ok {
            TASK_FAILED
        } else {
            text::NOT_STARTED_YET
        }
        .to_owned(),
    ]
}

fn joined(lines: &[String]) -> String {
    lines.join(" ")
}

/// `lines` without the summary the engine starts a finished report with ("Crosspane is repaired
/// and running.", "Crosspane was uninstalled."). The screens that show a finished repair,
/// reinstall or removal print their own headline above these lines, so the summary would only
/// say it twice. The headless command prints the engine's lines as they are. Every other line
/// stays.
fn without_summary(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|line| !engine::is_summary_line(line))
        .collect()
}

fn refused(id: MaintenanceId, reason: impl Into<String>) -> MaintenanceReport {
    MaintenanceReport::Refused {
        id,
        reason: reason.into(),
    }
}

/// ConfirmRepair: only a repair that ended with Crosspane running is finished; anything else is
/// a plain reason the person can read and try again from.
fn repair_report(
    id: MaintenanceId,
    result: SetupResult<ConvergeReport>,
    plan: &ConvergePlan,
) -> MaintenanceReport {
    match result {
        Ok(report) if report.ok() => MaintenanceReport::RepairFinished {
            id,
            outcome: RepairOutcome::Verified,
            lines: without_summary(report.lines),
            resumable: false,
        },
        Ok(report) => {
            let mut lines = failure_lines(&report);
            // Without the downloaded files a damaged program can't be restored; say what to do.
            if !plan.unrestorable.is_empty()
                && !lines
                    .iter()
                    .any(|line| line.contains(text::NEEDS_DOWNLOADED_INSTALLER))
            {
                lines.insert(0, text::NEEDS_DOWNLOADED_INSTALLER.to_owned());
            }
            refused(id, format!("Repair didn't finish. {}", joined(&lines)))
        }
        Err(error) => refused(id, error.text),
    }
}

/// ConfirmReinstall: Verified, Incomplete (the lines say what is missing) or nothing changed.
fn reinstall_report(id: MaintenanceId, result: SetupResult<ConvergeReport>) -> MaintenanceReport {
    let finished = |outcome, lines| MaintenanceReport::ReinstallFinished { id, outcome, lines };
    match result {
        Ok(report) if report.ok() => {
            finished(ReinstallOutcome::Verified, without_summary(report.lines))
        }
        Ok(report) => finished(ReinstallOutcome::Incomplete, failure_lines(&report)),
        Err(error) if error.changed => finished(ReinstallOutcome::Incomplete, vec![error.text]),
        Err(error) => finished(ReinstallOutcome::NotSubmitted, vec![error.text]),
    }
}

/// ConfirmUninstall: Removed, or Partial with what is left and "Try again."; an error before any
/// change is a refusal.
fn uninstall_report(id: MaintenanceId, result: SetupResult<UninstallReport>) -> MaintenanceReport {
    let finished = |outcome, lines| MaintenanceReport::Finished { id, outcome, lines };
    match result {
        Ok(report) if report.left.is_empty() => {
            finished(MaintenanceOutcome::Removed, without_summary(report.lines))
        }
        Ok(report) => {
            let lines = if report.lines.is_empty() {
                vec![
                    format!("Some parts are still there: {}.", report.left.join(", ")),
                    TRY_AGAIN.to_owned(),
                ]
            } else {
                report.lines
            };
            finished(MaintenanceOutcome::Partial, lines)
        }
        Err(error) if error.changed => finished(
            MaintenanceOutcome::Partial,
            vec![error.text, TRY_AGAIN.to_owned()],
        ),
        Err(error) => refused(id, error.text),
    }
}

/// How much of the firewall rule and display driver is there to remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Teardown {
    Present,
    Nothing,
    Unknown,
}

/// The two removal choices. The second is checked and enabled only when there is something for it
/// to remove.
fn removal_choices(teardown: Teardown) -> Vec<RemovalChoice> {
    let (label, checked, enabled) = match teardown {
        Teardown::Present => (TEARDOWN_LABEL, true, true),
        Teardown::Nothing => (TEARDOWN_NONE_LABEL, false, false),
        Teardown::Unknown => (TEARDOWN_UNKNOWN_LABEL, false, false),
    };
    vec![
        RemovalChoice {
            id: SETTINGS_CHOICE,
            role: ToggleRole::DeleteIdentity,
            label: SETTINGS_LABEL.into(),
            checked: false,
            enabled: true,
        },
        RemovalChoice {
            id: TEARDOWN_CHOICE,
            role: ToggleRole::Grant,
            label: label.into(),
            checked,
            enabled,
        },
    ]
}

fn request_id(request: &MaintenanceRequest) -> MaintenanceId {
    match request {
        MaintenanceRequest::Inspect { id }
        | MaintenanceRequest::PlanUninstall { id, .. }
        | MaintenanceRequest::ConfirmUninstall { id, .. }
        | MaintenanceRequest::ConfirmFollowUp { id, .. }
        | MaintenanceRequest::DeclineFollowUp { id, .. }
        | MaintenanceRequest::PlanRepair { id, .. }
        | MaintenanceRequest::ConfirmRepair { id, .. }
        | MaintenanceRequest::VerifyRepair { id, .. }
        | MaintenanceRequest::DiscardRepair { id, .. }
        | MaintenanceRequest::ResumeRepair { id, .. }
        | MaintenanceRequest::PlanReinstall { id, .. }
        | MaintenanceRequest::ConfirmReinstall { id, .. } => *id,
    }
}

/// The report for a job whose answer hit a bug: a failed step, or a refused request.
fn fallback_report(job: &NativeJob) -> NativeReport {
    match job {
        NativeJob::Step { job, .. } => NativeReport::Step(StepReport {
            job: job.clone(),
            outcome: NativeOutcome::Failed,
            detail: UNEXPECTED.to_owned(),
        }),
        NativeJob::Maintenance(request) => NativeReport::Maintenance(MaintenanceReport::Refused {
            id: request_id(request),
            reason: UNEXPECTED.to_owned(),
        }),
    }
}

// ---- the setup log ---------------------------------------------------------------------------

/// Logs one finished job: its step, stage, outcome kind and detail, or a maintenance report's kind
/// and text. Runs after the outcome is computed and before it is sent. Never logs a preview.
fn log_report(report: &NativeReport) {
    match report {
        NativeReport::Step(step) => {
            setup_note!(
                "step={} stage={:?} outcome={} detail={}",
                step.job.step.0,
                step.job.stage,
                outcome_kind(&step.outcome),
                clipped(&step.detail)
            );
        }
        NativeReport::Maintenance(report) => {
            let (kind, detail) = maintenance_kind(report);
            setup_note!("maintenance outcome={kind} detail={}", clipped(&detail));
        }
    }
}

/// The kind of a native outcome, without its payload: no preview text and no observation time.
fn outcome_kind(outcome: &NativeOutcome) -> String {
    match outcome {
        NativeOutcome::Detected { needs_action } => {
            format!("Detected(needs_action={needs_action})")
        }
        NativeOutcome::Planned { .. } => "Planned".to_owned(),
        NativeOutcome::Applied(result) => format!("Applied({result:?})"),
        NativeOutcome::NotSubmitted => "NotSubmitted".to_owned(),
        NativeOutcome::Verified { source, .. } => format!("Verified({source:?})"),
        NativeOutcome::Waiting(kind) => format!("Waiting({kind:?})"),
        NativeOutcome::Failed => "Failed".to_owned(),
        NativeOutcome::Unsupported => "Unsupported".to_owned(),
        NativeOutcome::Progress => "Progress".to_owned(),
    }
}

/// The kind and the readable text of a maintenance report. Previews and choice labels are left out.
fn maintenance_kind(report: &MaintenanceReport) -> (String, String) {
    let text = |lines: &[String]| lines.join(" ");
    match report {
        MaintenanceReport::Inspected { .. } => ("Inspected".to_owned(), String::new()),
        MaintenanceReport::Planned { .. } => ("Planned".to_owned(), String::new()),
        MaintenanceReport::Progress { detail, .. } => ("Progress".to_owned(), detail.clone()),
        MaintenanceReport::FollowUp { .. } => ("FollowUp".to_owned(), String::new()),
        MaintenanceReport::Finished { outcome, lines, .. } => {
            (format!("Finished({outcome:?})"), text(lines))
        }
        MaintenanceReport::Refused { reason, .. } => ("Refused".to_owned(), reason.clone()),
        MaintenanceReport::RepairPlanned { .. } => ("RepairPlanned".to_owned(), String::new()),
        MaintenanceReport::RepairDiscardable { .. } => {
            ("RepairDiscardable".to_owned(), String::new())
        }
        MaintenanceReport::RepairDiscarded { lines, .. } => {
            ("RepairDiscarded".to_owned(), text(lines))
        }
        MaintenanceReport::RepairResumable { lines, .. } => {
            ("RepairResumable".to_owned(), text(lines))
        }
        MaintenanceReport::RepairWaiting { detail, .. } => {
            ("RepairWaiting".to_owned(), detail.clone())
        }
        MaintenanceReport::RepairFinished { outcome, lines, .. } => {
            (format!("RepairFinished({outcome:?})"), text(lines))
        }
        MaintenanceReport::ReinstallPlanned { .. } => {
            ("ReinstallPlanned".to_owned(), String::new())
        }
        MaintenanceReport::ReinstallFinished { outcome, lines, .. } => {
            (format!("ReinstallFinished({outcome:?})"), text(lines))
        }
    }
}

/// The first `LOG_DETAIL_CHARS` characters of `text`.
fn clipped(text: &str) -> String {
    text.chars().take(LOG_DETAIL_CHARS).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{
        agent_contract::{PendingHealthReason, StatusAdmission},
        live::SupportCheck,
    };
    use crosspane_installer_core::elevated::{
        AgentProgram, DriverState, InstallId,
        status::{DriverStatus, FirewallStatus},
        step::INSTALL_WITHOUT_SETUP,
    };
    use std::sync::atomic::AtomicU64;

    const BANNED: [&str; 7] = [
        "foreign",
        "unsettled",
        "admission",
        "submission",
        "epoch",
        "journal",
        "outcomeunknown",
    ];

    fn job(step: StepId, stage: JobStage, operation: u64) -> JobIntent {
        JobIntent {
            step,
            operation: OperationId(operation),
            stage,
        }
    }

    fn consent(plan: u64, operation: u64) -> Consent {
        Consent {
            plan: OperationId(plan),
            operation: OperationId(operation),
            revision: 7,
        }
    }

    fn report(
        files_ok: bool,
        task_ok: bool,
        agent_answering: bool,
        lines: &[&str],
    ) -> ConvergeReport {
        ConvergeReport {
            files_ok,
            task_ok,
            agent_answering,
            elevated: ElevatedOutcome::NotNeeded,
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    fn plan(unrestorable: Vec<super::super::paths::PayloadFile>) -> ConvergePlan {
        ConvergePlan {
            copies: Vec::new(),
            unrestorable,
            restart: false,
            legacy: false,
            task: false,
            shell: false,
            start: true,
            elevated: None,
            elevated_note: None,
        }
    }

    fn setup_plan() -> ElevatedPlan {
        ElevatedPlan::new(Verb::Setup(rule_scope())).unwrap()
    }

    fn rule_scope() -> RuleScope {
        RuleScope {
            id: InstallId::parse("0123456789abcdef").unwrap(),
            program: AgentProgram::parse(
                r"C:\Users\someone\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
            )
            .unwrap(),
        }
    }

    fn status_report(firewall: FirewallState, driver: DriverState) -> StatusReport {
        StatusReport {
            schema: 1,
            elevated: false,
            driver: DriverStatus {
                state: driver,
                packages: Vec::new(),
                devices: Vec::new(),
            },
            firewall: FirewallStatus {
                state: firewall,
                family_count: 0,
                local_rules_apply: None,
            },
        }
    }

    fn evidence(source: ObservationSource, ok: bool) -> StatusEvidence {
        StatusEvidence(AgentReply {
            id: 3,
            observed_at_ms: 1_234,
            source,
            result: if ok {
                Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(PendingHealthReason::Absent),
                ))
            } else {
                Err(CallFailure::Unavailable)
            },
        })
    }

    // ---- consent --------------------------------------------------------------------------

    #[test]
    fn consent_must_name_this_apply_and_the_last_plan() {
        let apply = job(INSTALL, JobStage::Apply, 5);
        let planned = Some(OperationId(4));
        assert!(consent_matches(Some(consent(4, 5)), &apply, planned));
        // No consent, or none for this Apply.
        assert!(!consent_matches(None, &apply, planned));
        assert!(!consent_matches(Some(consent(4, 6)), &apply, planned));
        // A consent for a Plan the backend didn't answer last, or no Plan answered at all.
        assert!(!consent_matches(Some(consent(3, 5)), &apply, planned));
        assert!(!consent_matches(Some(consent(4, 5)), &apply, None));
    }

    #[test]
    fn a_step_plan_is_handed_out_once_for_a_matching_consent() {
        let (out, _reports) = mpsc::channel();
        let clock: Clock = Arc::new(|| 0);
        let mut worker = Worker {
            out,
            clock,
            payload: None,
            checks: SupportChecksSlot::new(SUPPORT),
            step_plans: BTreeMap::new(),
            book: PlanBook::new(),
        };
        worker
            .step_plans
            .insert(AGENT, (OperationId(4), StepPlan::Start));
        let apply = job(AGENT, JobStage::Apply, 5);
        // A consent for a different plan leaves the stored plan in place.
        assert!(worker.consented(&apply, Some(consent(3, 5))).is_none());
        assert!(worker.consented(&apply, None).is_none());
        assert!(matches!(
            worker.consented(&apply, Some(consent(4, 5))),
            Some(StepPlan::Start)
        ));
        // Once spent, the same consent can't run it again.
        assert!(worker.consented(&apply, Some(consent(4, 5))).is_none());
    }

    // ---- plan numbers ---------------------------------------------------------------------

    #[test]
    fn plan_numbers_start_at_one_and_never_repeat() {
        let mut book = PlanBook::new();
        assert_eq!(book.store(MaintenanceId(10), "first"), 1);
        assert_eq!(book.store(MaintenanceId(10), "second"), 2);
        assert_eq!(book.store(MaintenanceId(11), "third"), 3);
    }

    #[test]
    fn a_stale_plan_number_is_refused_and_keeps_the_newer_plan() {
        let mut book = PlanBook::new();
        let id = MaintenanceId(2);
        book.store(id, "old");
        let current = book.store(id, "new");
        assert_eq!(book.take_where(id, Some(current - 1), |_| true), None);
        assert_eq!(book.take_where(id, Some(current + 1), |_| true), None);
        assert_eq!(book.take_where(id, Some(current), |_| true), Some("new"));
        // Handed out once.
        assert_eq!(book.take_where(id, Some(current), |_| true), None);
        assert_eq!(book.take_where(MaintenanceId(9), None, |_| true), None);
    }

    #[test]
    fn a_plan_of_another_kind_is_not_taken() {
        let mut book = PlanBook::new();
        let id = MaintenanceId(2);
        let number = book.store(id, 5_u32);
        assert_eq!(book.take_where(id, Some(number), |value| *value == 6), None);
        assert_eq!(
            book.take_where(id, Some(number), |value| *value == 5),
            Some(5)
        );
    }

    #[test]
    fn the_plan_book_forgets_the_oldest_plans() {
        let mut book = PlanBook::new();
        for id in 0..(MAX_STORED_PLANS as u64 + 3) {
            book.store(MaintenanceId(id), id);
        }
        assert_eq!(book.plans.len(), MAX_STORED_PLANS);
        assert_eq!(book.take_where(MaintenanceId(0), None, |_| true), None);
        assert!(
            book.take_where(MaintenanceId(MAX_STORED_PLANS as u64 + 2), None, |_| true)
                .is_some()
        );
    }

    // ---- progress -------------------------------------------------------------------------

    #[test]
    fn the_first_line_goes_out_and_the_next_waits_for_the_gap() {
        let mut throttle = Throttle::new();
        assert_eq!(
            throttle.admit(0, "Stopping Crosspane…").as_deref(),
            Some("Stopping Crosspane…")
        );
        assert_eq!(throttle.admit(1, "Copying files…"), None);
        assert_eq!(throttle.admit(HEARTBEAT_GAP_MS - 1, "Copying files…"), None);
        assert_eq!(
            throttle
                .admit(HEARTBEAT_GAP_MS, "Copying files…")
                .as_deref(),
            Some("Copying files…")
        );
        // The gap counts from the last line that went out.
        assert_eq!(throttle.admit(HEARTBEAT_GAP_MS + 1, "Starting…"), None);
    }

    #[test]
    fn a_fifth_distinct_line_repeats_the_last_one() {
        let mut throttle = Throttle::new();
        let mut at = 0;
        for line in ["one", "two", "three", "four"] {
            assert_eq!(throttle.admit(at, line).as_deref(), Some(line));
            at += HEARTBEAT_GAP_MS;
        }
        assert_eq!(throttle.admit(at, "five").as_deref(), Some("four"));
        at += HEARTBEAT_GAP_MS;
        // A line shown before still goes out as itself.
        assert_eq!(throttle.admit(at, "two").as_deref(), Some("two"));
    }

    #[test]
    fn blank_lines_and_a_clock_that_runs_backwards_send_nothing() {
        let mut throttle = Throttle::new();
        assert_eq!(throttle.admit(0, "   "), None);
        assert!(throttle.admit(50_000, "one").is_some());
        assert_eq!(throttle.admit(10, "two"), None);
    }

    #[test]
    fn heartbeats_are_throttled_by_the_injected_clock() {
        let (out, reports) = mpsc::channel();
        let ticks = Arc::new(AtomicU64::new(0));
        let clock: Clock = {
            let ticks = ticks.clone();
            Arc::new(move || ticks.load(Ordering::Relaxed))
        };
        let operation = job(INSTALL, JobStage::Apply, 9);
        let beat = {
            let operation = operation.clone();
            Heartbeat::new(out, clock, move |detail| {
                NativeReport::Step(StepReport {
                    job: operation.clone(),
                    outcome: NativeOutcome::Progress,
                    detail,
                })
            })
        };
        beat.step("Stopping Crosspane…");
        ticks.store(5_000, Ordering::Relaxed);
        beat.step("Copying files…");
        ticks.store(HEARTBEAT_GAP_MS, Ordering::Relaxed);
        beat.step("Starting Crosspane…");
        let sent: Vec<NativeReport> = reports.try_iter().collect();
        let progress = |detail: &str| {
            NativeReport::Step(StepReport {
                job: operation.clone(),
                outcome: NativeOutcome::Progress,
                detail: detail.to_owned(),
            })
        };
        // Every report carries the same job intent, which the controller needs to accept it.
        assert_eq!(
            sent,
            vec![
                progress("Stopping Crosspane…"),
                progress("Starting Crosspane…")
            ]
        );
    }

    #[test]
    fn maintenance_heartbeats_carry_the_request_id() {
        let (out, reports) = mpsc::channel();
        let clock: Clock = Arc::new(|| 0);
        let id = MaintenanceId(12);
        let beat = Heartbeat::new(out, clock, move |detail| {
            NativeReport::Maintenance(MaintenanceReport::Progress { id, detail })
        });
        beat.step("Removing the sign-in task…");
        assert_eq!(
            reports.try_recv().unwrap(),
            NativeReport::Maintenance(MaintenanceReport::Progress {
                id,
                detail: "Removing the sign-in task…".to_owned()
            })
        );
    }

    // ---- SUPPORT and status ---------------------------------------------------------------

    #[test]
    fn a_failed_row_is_a_definite_no_and_an_unreadable_fact_is_not() {
        let supported = Support {
            supported: true,
            reason: None,
            checks: Vec::new(),
        };
        assert_eq!(verdict(&supported), Verdict::Supported);

        let no = Support {
            supported: false,
            reason: Some("Setup is elevated.".to_owned()),
            checks: vec![SupportCheck::new(
                "Your own Windows session",
                CheckState::Failed("Setup is elevated.".to_owned()),
            )],
        };
        assert_eq!(verdict(&no), Verdict::No("Setup is elevated.".to_owned()));

        let unreadable = Support {
            supported: false,
            reason: Some("The Windows version couldn't be read.".to_owned()),
            checks: vec![SupportCheck::new(
                "Windows version",
                CheckState::Unconfirmed("The Windows version couldn't be read.".to_owned()),
            )],
        };
        assert_eq!(
            verdict(&unreadable),
            Verdict::Unreadable("The Windows version couldn't be read.".to_owned())
        );

        // No reason given: the row's text, else the general refusal.
        let bare = Support {
            supported: false,
            reason: None,
            checks: vec![SupportCheck::new(
                "Your folders",
                CheckState::Failed("Folders unusable.".to_owned()),
            )],
        };
        assert_eq!(verdict(&bare), Verdict::No("Folders unusable.".to_owned()));
        let empty = Support {
            supported: false,
            reason: None,
            checks: Vec::new(),
        };
        assert_eq!(
            verdict(&empty),
            Verdict::Unreadable(NOT_SUPPORTED.to_owned())
        );
    }

    #[test]
    fn only_a_live_status_reply_shows_the_agent_running() {
        let live = evidence(ObservationSource::Live, true);
        assert_eq!(
            status_answered(Some(&live)),
            Some((ObservationSource::Live, 1_234))
        );
        assert_eq!(status_answered(None), None);
        assert_eq!(
            status_answered(Some(&evidence(ObservationSource::Live, false))),
            None
        );
        assert_eq!(
            status_answered(Some(&evidence(ObservationSource::Demo, true))),
            None
        );
    }

    // ---- step answers ---------------------------------------------------------------------

    #[test]
    fn install_verify_waits_instead_of_refusing() {
        let waiting = NativeOutcome::Waiting(WaitKind::Contract);
        assert_eq!(
            install_verify(true, true, true, 40),
            Answer::new(
                NativeOutcome::Verified {
                    source: ObservationSource::Live,
                    observed_at_ms: 40
                },
                INSTALLED_RUNNING
            )
        );
        assert_eq!(
            install_verify(true, true, false, 40),
            Answer::new(waiting.clone(), text::NOT_STARTED_YET)
        );
        assert_eq!(
            install_verify(false, true, true, 40),
            Answer::new(waiting.clone(), INSTALL_UNVERIFIED)
        );
        assert_eq!(
            install_verify(true, false, true, 40),
            Answer::new(waiting, INSTALL_UNVERIFIED)
        );
    }

    #[test]
    fn install_plan_refuses_files_it_cannot_restore_and_keeps_the_rest() {
        use super::super::paths::PayloadFile;
        let refusal = install_plan(Ok(plan(vec![PayloadFile::Agent]))).unwrap_err();
        assert_eq!(
            refusal,
            Answer::new(NativeOutcome::Failed, text::NEEDS_DOWNLOADED_INSTALLER)
        );
        let (preview, kept) = install_plan(Ok(plan(Vec::new()))).unwrap();
        assert!(!preview.trim().is_empty());
        assert!(kept.unrestorable.is_empty());
        assert_eq!(
            install_plan(Err(SetupError::before(text::SOURCES_DAMAGED))).unwrap_err(),
            Answer::new(NativeOutcome::Failed, text::SOURCES_DAMAGED)
        );
    }

    #[test]
    fn install_apply_is_applied_once_files_and_task_are_in_place() {
        let applied = NativeOutcome::Applied(ApplyOutcome::Applied);
        // The agent isn't up yet: still Applied, Verify decides.
        let answer = install_answer(Ok(report(true, true, false, &["Waiting for Crosspane."])));
        assert_eq!(answer.outcome, applied);
        assert_eq!(answer.detail, "Waiting for Crosspane.");
        assert_eq!(
            install_answer(Ok(report(true, true, true, &[]))).detail,
            INSTALL_DONE
        );
        // Files or task missing: Failed with the engine's lines.
        let failed = install_answer(Ok(report(false, true, false, &["One.", "Two."])));
        assert_eq!(failed.outcome, NativeOutcome::Applied(ApplyOutcome::Failed));
        assert_eq!(failed.detail, "One. Two.");
        assert_eq!(
            install_answer(Ok(report(true, false, false, &[]))).detail,
            TASK_FAILED
        );
    }

    #[test]
    fn install_apply_errors_split_on_whether_anything_changed() {
        let before = install_answer(Err(SetupError::before(text::BUSY)));
        assert_eq!(before, Answer::new(NativeOutcome::NotSubmitted, text::BUSY));
        let after = install_answer(Err(SetupError::after(text::STILL_RUNNING)));
        assert_eq!(
            after,
            Answer::new(
                NativeOutcome::Applied(ApplyOutcome::Failed),
                text::STILL_RUNNING
            )
        );
    }

    #[test]
    fn starting_the_agent_answers_applied_or_failed() {
        assert_eq!(
            start_answer(Ok(true)).outcome,
            NativeOutcome::Applied(ApplyOutcome::Applied)
        );
        assert_eq!(
            start_answer(Ok(false)),
            Answer::new(
                NativeOutcome::Applied(ApplyOutcome::Failed),
                text::NOT_STARTED_YET
            )
        );
        assert_eq!(
            start_answer(Err(SetupError::before(text::NOT_INSTALLED))).outcome,
            NativeOutcome::NotSubmitted
        );
    }

    #[test]
    fn a_declined_administrator_prompt_is_refused_not_failed() {
        let both = Verb::Setup(rule_scope());
        assert_eq!(
            admin_answer(ElevatedOutcome::Declined, &both),
            Answer::new(
                NativeOutcome::Applied(ApplyOutcome::Refused),
                INSTALL_WITHOUT_SETUP
            )
        );
        assert_eq!(
            admin_answer(ElevatedOutcome::Configured("Done.".to_owned()), &both),
            Answer::new(NativeOutcome::Applied(ApplyOutcome::Applied), "Done.")
        );
        assert_eq!(
            admin_answer(ElevatedOutcome::NotNeeded, &both).outcome,
            NativeOutcome::Applied(ApplyOutcome::Applied)
        );
        assert_eq!(
            admin_answer(ElevatedOutcome::Failed("No.".to_owned()), &both),
            Answer::new(NativeOutcome::Applied(ApplyOutcome::Failed), "No.")
        );
    }

    #[test]
    fn a_declined_prompt_says_what_that_leaves_undone_for_what_was_asked() {
        let declined = |verb: &Verb| admin_answer(ElevatedOutcome::Declined, verb);
        let firewall = declined(&Verb::AddFirewall(rule_scope()));
        assert_eq!(
            firewall,
            Answer::new(
                NativeOutcome::Applied(ApplyOutcome::Refused),
                "Until the firewall rule is added, other computers may not reach this PC."
            )
        );
        assert!(!firewall.detail.contains("display driver"));
        let driver = declined(&Verb::InstallDriver);
        assert_eq!(
            driver,
            Answer::new(
                NativeOutcome::Applied(ApplyOutcome::Refused),
                "Without the display driver, Crosspane mirrors windows in place."
            )
        );
        assert!(!driver.detail.contains("firewall"));
    }

    #[test]
    fn the_administrator_plan_names_only_what_is_missing() {
        // The live state that prompted this: the firewall rule needs widening, the driver is fine.
        let (plan, preview, detail) = admin_plan(
            &status_report(FirewallState::Mismatch, DriverState::Installed),
            rule_scope(),
        )
        .unwrap();
        assert!(matches!(plan.verb(), Verb::AddFirewall(_)));
        assert!(preview.starts_with(ADMIN_PREVIEW_FIREWALL));
        assert!(preview.lines().any(|line| line.starts_with(PREVIEW_HEADER)));
        assert!(
            !preview.to_lowercase().contains("display driver"),
            "{preview}"
        );
        assert_eq!(
            detail,
            "Needs administrator approval to allow Crosspane through the firewall."
        );

        let (plan, preview, detail) = admin_plan(
            &status_report(FirewallState::Present, DriverState::Absent),
            rule_scope(),
        )
        .unwrap();
        assert_eq!(plan.verb(), &Verb::InstallDriver);
        assert!(preview.starts_with(ADMIN_PREVIEW_DRIVER));
        assert!(!preview.contains("Firewall rule"), "{preview}");
        assert_eq!(
            detail,
            "Needs administrator approval to add the display driver."
        );

        let (plan, preview, _) = admin_plan(
            &status_report(FirewallState::Missing, DriverState::Absent),
            rule_scope(),
        )
        .unwrap();
        assert!(matches!(plan.verb(), Verb::Setup(_)));
        assert!(preview.starts_with(ADMIN_PREVIEW));
        // The consent for the stored plan is for exactly the lines shown.
        assert_eq!(plan.consent().unwrap().action(), plan.verb());
    }

    #[test]
    fn nothing_to_approve_is_a_wait_not_a_prompt() {
        for (firewall, driver) in [
            (FirewallState::Present, DriverState::Installed),
            (FirewallState::Unavailable, DriverState::Unavailable),
        ] {
            let answer = admin_plan(&status_report(firewall, driver), rule_scope()).unwrap_err();
            assert_eq!(
                answer,
                Answer::new(
                    NativeOutcome::Waiting(WaitKind::Contract),
                    NOTHING_TO_APPROVE
                )
            );
        }
    }

    #[test]
    fn the_administrator_step_always_says_where_it_stands() {
        for firewall in [
            FirewallState::Present,
            FirewallState::Missing,
            FirewallState::Mismatch,
            FirewallState::Unavailable,
        ] {
            for driver in [
                DriverState::Installed,
                DriverState::Absent,
                DriverState::PackageOnly,
                DriverState::Mismatch,
                DriverState::Unavailable,
            ] {
                let verified = admin_verify(&status_report(firewall, driver), 9);
                assert!(!verified.detail.is_empty());
                assert_ne!(verified.detail, "Checked.");
                if let Ok((_, _, detail)) =
                    admin_plan(&status_report(firewall, driver), rule_scope())
                {
                    assert_ne!(detail, "Checked.");
                    assert!(
                        detail.starts_with("Needs administrator approval"),
                        "{detail}"
                    );
                }
            }
        }
        // Needing approval is said as such, not as a check that passed.
        let missing = admin_verify(
            &status_report(FirewallState::Missing, DriverState::Installed),
            9,
        );
        assert_eq!(missing.outcome, NativeOutcome::Waiting(WaitKind::User));
        assert_eq!(
            missing.detail,
            "Needs administrator approval to allow Crosspane through the firewall."
        );
        let set_up = admin_verify(
            &status_report(FirewallState::Present, DriverState::Installed),
            9,
        );
        assert_eq!(
            set_up.detail,
            "Firewall rule and display driver are set up."
        );
    }

    #[test]
    fn the_install_and_agent_lines_say_where_each_stands() {
        assert_eq!(
            install_detected(true, true),
            Answer::new(
                NativeOutcome::Detected {
                    needs_action: false
                },
                INSTALLED_RUNNING
            )
        );
        assert_eq!(
            install_detected(true, false),
            Answer::new(
                NativeOutcome::Detected { needs_action: true },
                INSTALLED_SILENT
            )
        );
        for converged_agent in [false, true] {
            assert_eq!(
                install_detected(false, converged_agent),
                Answer::new(
                    NativeOutcome::Detected { needs_action: true },
                    INSTALL_INCOMPLETE
                )
            );
        }
    }

    #[test]
    fn the_firewall_step_is_verified_only_by_the_rule_being_present() {
        let present = admin_verify(
            &status_report(FirewallState::Present, DriverState::Absent),
            9,
        );
        assert_eq!(
            present.outcome,
            NativeOutcome::Verified {
                source: ObservationSource::Live,
                observed_at_ms: 9
            }
        );
        for state in [FirewallState::Missing, FirewallState::Mismatch] {
            let waiting = admin_verify(&status_report(state, DriverState::Installed), 9);
            assert_eq!(waiting.outcome, NativeOutcome::Waiting(WaitKind::User));
            assert!(!waiting.detail.is_empty());
        }
    }

    #[test]
    fn a_stage_that_could_not_look_changes_nothing() {
        let why = || "Windows couldn't be checked.".to_owned();
        assert_eq!(
            unavailable(JobStage::Detect, why()).outcome,
            NativeOutcome::Waiting(WaitKind::Contract)
        );
        assert_eq!(
            unavailable(JobStage::Verify, why()).outcome,
            NativeOutcome::Waiting(WaitKind::Contract)
        );
        assert_eq!(
            unavailable(JobStage::Plan, why()).outcome,
            NativeOutcome::Failed
        );
        assert_eq!(
            unavailable(JobStage::Apply, why()).outcome,
            NativeOutcome::NotSubmitted
        );
    }

    // ---- maintenance answers --------------------------------------------------------------

    #[test]
    fn a_repair_that_ends_running_is_verified_and_never_resumable() {
        let id = MaintenanceId(3);
        // The screen's own headline says it; the engine's summary line is not repeated.
        let done = repair_report(
            id,
            Ok(report(
                true,
                true,
                true,
                &["Crosspane is repaired and running."],
            )),
            &plan(Vec::new()),
        );
        assert_eq!(
            done,
            MaintenanceReport::RepairFinished {
                id,
                outcome: RepairOutcome::Verified,
                lines: Vec::new(),
                resumable: false
            }
        );
        // Every other line stays.
        let done = repair_report(
            id,
            Ok(report(
                true,
                true,
                true,
                &[
                    "Crosspane is repaired and running.",
                    "Firewall rule: present. Display driver: installed.",
                ],
            )),
            &plan(Vec::new()),
        );
        assert_eq!(
            done,
            MaintenanceReport::RepairFinished {
                id,
                outcome: RepairOutcome::Verified,
                lines: vec!["Firewall rule: present. Display driver: installed.".to_owned()],
                resumable: false
            }
        );
    }

    #[test]
    fn the_engines_summary_line_is_left_out_but_nothing_else() {
        // Every summary the engine can start a finished report with.
        for summary in [
            "Crosspane is installed and running.",
            "Crosspane is repaired and running.",
            "Crosspane was reinstalled and is running.",
            "Crosspane was uninstalled.",
        ] {
            assert!(engine::is_summary_line(summary), "{summary}");
            assert_eq!(
                without_summary(vec![summary.to_owned(), "Next.".to_owned()]),
                vec!["Next.".to_owned()]
            );
        }
        // Wherever it stands, and only when it is the whole line.
        let lines = vec![
            "First.".to_owned(),
            "Crosspane was uninstalled.".to_owned(),
            "Crosspane was uninstalled. But not really.".to_owned(),
            "Crosspane is repaired.".to_owned(),
            "Administrator approval was declined. Until the firewall rule is added, other computers may not reach this PC.".to_owned(),
        ];
        assert_eq!(
            without_summary(lines.clone()),
            [&lines[0], &lines[2], &lines[3], &lines[4]]
                .map(|line| line.to_owned())
                .to_vec()
        );
        assert!(without_summary(Vec::new()).is_empty());
        // The engine's real texts are what the filter matches: a change there must show up here.
        assert!(!engine::is_summary_line(text::NOT_STARTED_YET));
        assert!(!engine::is_summary_line(TASK_FAILED));
    }

    #[test]
    fn a_repair_that_did_not_finish_is_a_plain_refusal() {
        let id = MaintenanceId(3);
        let MaintenanceReport::Refused { reason, .. } = repair_report(
            id,
            Ok(report(true, true, false, &[text::NOT_STARTED_YET])),
            &plan(Vec::new()),
        ) else {
            panic!("a repair that left Crosspane stopped must be refused");
        };
        assert!(reason.starts_with("Repair didn't finish."));
        assert!(reason.contains(text::NOT_STARTED_YET));

        let MaintenanceReport::Refused { reason, .. } =
            repair_report(id, Err(SetupError::before(text::BUSY)), &plan(Vec::new()))
        else {
            panic!("an engine error must be refused");
        };
        assert_eq!(reason, text::BUSY);
    }

    #[test]
    fn a_repair_without_the_downloaded_files_says_what_to_run() {
        use super::super::paths::PayloadFile;
        let id = MaintenanceId(3);
        let MaintenanceReport::Refused { reason, .. } = repair_report(
            id,
            Ok(report(
                false,
                true,
                true,
                &["Some files are still different."],
            )),
            &plan(vec![PayloadFile::Agent]),
        ) else {
            panic!("an unrestorable repair must be refused");
        };
        assert!(reason.contains(text::NEEDS_DOWNLOADED_INSTALLER));
        // Said once when the engine already said it.
        let MaintenanceReport::Refused { reason, .. } = repair_report(
            id,
            Ok(report(
                false,
                true,
                true,
                &[text::NEEDS_DOWNLOADED_INSTALLER],
            )),
            &plan(vec![PayloadFile::Agent]),
        ) else {
            panic!("an unrestorable repair must be refused");
        };
        assert_eq!(reason.matches(text::NEEDS_DOWNLOADED_INSTALLER).count(), 1);
    }

    #[test]
    fn a_reinstall_is_verified_incomplete_or_not_submitted() {
        let id = MaintenanceId(4);
        let finished = |outcome, lines: &[&str]| MaintenanceReport::ReinstallFinished {
            id,
            outcome,
            lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        };
        assert_eq!(
            reinstall_report(id, Ok(report(true, true, true, &[]))),
            finished(ReinstallOutcome::Verified, &[])
        );
        // The engine's summary repeats the screen's headline, so it is left out.
        assert_eq!(
            reinstall_report(
                id,
                Ok(report(
                    true,
                    true,
                    true,
                    &["Crosspane was reinstalled and is running.", "Kept."]
                ))
            ),
            finished(ReinstallOutcome::Verified, &["Kept."])
        );
        // Files and task in place but the agent silent: incomplete, never unknown.
        assert_eq!(
            reinstall_report(id, Ok(report(true, true, false, &[text::NOT_STARTED_YET]))),
            finished(ReinstallOutcome::Incomplete, &[text::NOT_STARTED_YET])
        );
        assert_eq!(
            reinstall_report(id, Ok(report(false, false, false, &[]))),
            finished(ReinstallOutcome::Incomplete, &[FILES_FAILED])
        );
        assert_eq!(
            reinstall_report(id, Err(SetupError::before(text::BUSY))),
            finished(ReinstallOutcome::NotSubmitted, &[text::BUSY])
        );
        assert_eq!(
            reinstall_report(id, Err(SetupError::after(text::STILL_RUNNING))),
            finished(ReinstallOutcome::Incomplete, &[text::STILL_RUNNING])
        );
    }

    #[test]
    fn an_uninstall_is_removed_partial_or_refused() {
        let id = MaintenanceId(5);
        let removed = uninstall_report(
            id,
            Ok(UninstallReport {
                left: Vec::new(),
                lines: vec!["Crosspane was uninstalled.".to_owned()],
            }),
        );
        assert_eq!(
            removed,
            MaintenanceReport::Finished {
                id,
                outcome: MaintenanceOutcome::Removed,
                lines: Vec::new()
            }
        );
        // A note after the summary stays.
        let noted = uninstall_report(
            id,
            Ok(UninstallReport {
                left: Vec::new(),
                lines: vec![
                    "Crosspane was uninstalled.".to_owned(),
                    "Your settings were removed but the sign-in key couldn't be erased.".to_owned(),
                ],
            }),
        );
        assert_eq!(
            noted,
            MaintenanceReport::Finished {
                id,
                outcome: MaintenanceOutcome::Removed,
                lines: vec![
                    "Your settings were removed but the sign-in key couldn't be erased.".to_owned()
                ]
            }
        );
        let partial = uninstall_report(
            id,
            Ok(UninstallReport {
                left: vec!["program files".to_owned()],
                lines: vec![
                    "Some parts are still there: program files.".to_owned(),
                    "Try again.".to_owned(),
                ],
            }),
        );
        assert_eq!(
            partial,
            MaintenanceReport::Finished {
                id,
                outcome: MaintenanceOutcome::Partial,
                lines: vec![
                    "Some parts are still there: program files.".to_owned(),
                    "Try again.".to_owned()
                ]
            }
        );
        // The engine said nothing: the left items and a retry are still shown.
        let MaintenanceReport::Finished { lines, .. } = uninstall_report(
            id,
            Ok(UninstallReport {
                left: vec!["sign-in task".to_owned()],
                lines: Vec::new(),
            }),
        ) else {
            panic!("a partial uninstall must finish");
        };
        assert_eq!(lines.last().map(String::as_str), Some(TRY_AGAIN));
        assert!(lines[0].contains("sign-in task"));
        assert_eq!(
            uninstall_report(id, Err(SetupError::before(text::BUSY))),
            MaintenanceReport::Refused {
                id,
                reason: text::BUSY.to_owned()
            }
        );
        let MaintenanceReport::Finished { outcome, lines, .. } =
            uninstall_report(id, Err(SetupError::after(text::STILL_RUNNING)))
        else {
            panic!("an uninstall that changed something must finish");
        };
        assert_eq!(outcome, MaintenanceOutcome::Partial);
        assert_eq!(lines.last().map(String::as_str), Some(TRY_AGAIN));
    }

    #[test]
    fn removal_choices_follow_what_there_is_to_remove() {
        let present = removal_choices(Teardown::Present);
        assert_eq!(present.len(), 2);
        assert_eq!(present[0].id, 1);
        assert_eq!(present[0].role, ToggleRole::DeleteIdentity);
        assert!(!present[0].checked && present[0].enabled);
        assert_eq!(present[1].id, 2);
        assert_eq!(present[1].role, ToggleRole::Grant);
        assert!(present[1].checked && present[1].enabled);
        for teardown in [Teardown::Nothing, Teardown::Unknown] {
            let choices = removal_choices(teardown);
            assert!(!choices[1].checked && !choices[1].enabled);
            assert!(!choices[0].checked && choices[0].enabled);
        }
    }

    #[test]
    fn removal_choices_are_read_by_id() {
        let choices = [(1, true), (2, false)];
        assert!(choice_on(&choices, 1));
        assert!(!choice_on(&choices, 2));
        assert!(!choice_on(&choices, 3));
    }

    #[test]
    fn the_administrator_block_is_added_once() {
        let plan = setup_plan();
        let bare = with_elevated_block("Remove Crosspane.".to_owned(), Some(&plan));
        assert!(bare.lines().any(|line| line.starts_with(PREVIEW_HEADER)));
        assert_eq!(with_elevated_block(bare.clone(), Some(&plan)), bare);
        assert_eq!(
            with_elevated_block("Remove Crosspane.".to_owned(), None),
            "Remove Crosspane."
        );
    }

    #[test]
    fn a_blank_preview_gets_a_fallback() {
        assert_eq!(non_empty("  \n".to_owned(), "Fallback."), "Fallback.");
        assert_eq!(non_empty("Real.".to_owned(), "Fallback."), "Real.");
    }

    #[test]
    fn requests_the_platform_cannot_answer_name_their_id() {
        let id = MaintenanceId(8);
        let requests = [
            MaintenanceRequest::VerifyRepair { id, status: None },
            MaintenanceRequest::DiscardRepair { id, status: None },
            MaintenanceRequest::ResumeRepair { id, status: None },
            MaintenanceRequest::ConfirmFollowUp {
                id,
                follow_up: 1,
                revision: 2,
            },
            MaintenanceRequest::DeclineFollowUp { id, follow_up: 1 },
        ];
        for request in requests {
            assert_eq!(request_id(&request), id);
        }
        let fallback = fallback_report(&NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
        assert_eq!(
            fallback,
            NativeReport::Maintenance(MaintenanceReport::Refused {
                id,
                reason: UNEXPECTED.to_owned()
            })
        );
        let step = job(ADMIN, JobStage::Plan, 2);
        assert_eq!(
            fallback_report(&NativeJob::Step {
                job: step.clone(),
                consent: None,
                status: None
            }),
            NativeReport::Step(StepReport {
                job: step,
                outcome: NativeOutcome::Failed,
                detail: UNEXPECTED.to_owned()
            })
        );
    }

    // ---- shape ----------------------------------------------------------------------------

    #[test]
    fn the_four_native_steps_keep_their_ids_and_flags() {
        let description = description();
        let ids: Vec<u16> = description.steps.iter().map(|step| step.id.0).collect();
        assert_eq!(ids, [10, 20, 30, 55]);
        let flags: Vec<(bool, bool)> = description
            .steps
            .iter()
            .map(|step| (step.uses_status, step.settles_with_peer))
            .collect();
        assert_eq!(
            flags,
            [(false, false), (false, false), (true, false), (false, true)]
        );
        assert_eq!(description.connect_after, [AGENT]);
        assert_eq!(description.ready_after, [AGENT, ADMIN]);
        assert!(!description.hiding_choice);
    }

    #[test]
    fn the_texts_use_plain_words() {
        let texts = [
            NOT_OWN_SESSION,
            NOT_SUPPORTED,
            CALLER_REFUSED,
            WORKER_STOPPED,
            UNEXPECTED,
            AGENT_STARTING,
            INSTALL_UNVERIFIED,
            NO_CONSENT,
            SUPPORT_NEEDS_NO_PLAN,
            NOTHING_TO_CHANGE,
            START_PREVIEW,
            ADMIN_PREVIEW,
            ADMIN_PREVIEW_FIREWALL,
            ADMIN_PREVIEW_DRIVER,
            NOTHING_TO_APPROVE,
            CAN_RUN,
            INSTALLED_RUNNING,
            INSTALLED_SILENT,
            INSTALL_INCOMPLETE,
            INSTALL_READY,
            AGENT_RUNNING,
            AGENT_NOT_RUNNING,
            AGENT_READY,
            INSTALL_FALLBACK_PREVIEW,
            REPAIR_FALLBACK_PREVIEW,
            REINSTALL_FALLBACK_PREVIEW,
            UNINSTALL_FALLBACK_PREVIEW,
            REPAIR_STALE,
            REINSTALL_STALE,
            UNINSTALL_STALE,
            NO_EARLIER_REPAIR,
            NOT_ON_WINDOWS,
            FILES_FAILED,
            TASK_FAILED,
            INSTALL_DONE,
            SETTINGS_LABEL,
            TEARDOWN_LABEL,
            TEARDOWN_NONE_LABEL,
            TEARDOWN_UNKNOWN_LABEL,
        ];
        for text in texts {
            let lower = text.to_lowercase();
            for word in BANNED {
                assert!(!lower.contains(word), "{text:?} uses {word:?}");
            }
        }
    }
}
