//! Windows integration: the frame owns bounded queues; native authority stays on one worker.
#[path = "integration/diagnose.rs"]
pub(crate) mod diagnose;
#[path = "integration/domains.rs"]
pub(crate) mod domains;
#[path = "integration/ports.rs"]
pub(crate) mod ports;
#[path = "integration/worker.rs"]
pub(crate) mod worker;

use crate::{
    agent_contract::{AgentCall, AgentPlatform, AgentPort, AgentReply, CallFailure},
    gui::{ControllerTick, InstallerController},
    live::{
        self, Availability, Clock, MaintenanceOutcome, MaintenanceReport, MaintenanceRequest,
        NativeJob, NativeOutcome, NativeRefusal, NativeReport, Platform, RepairOutcome, StepReport,
    },
    view::{ProgressGroup, ScreenId, WizardAction, WizardView},
};
use crosspane_installer_core::elevated::step::INSTALL_DECLINE_NOTE;
use crosspane_installer_core::{ApplyOutcome, JobStage, ObservationSource, StepId, WaitKind};
use domains::{Domains, ElevatedPlanning, ElevatedRequest, Failure, Operation};
use ports::{CloseState, Command};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
use worker::{Coordinator, ELEVATED_UNCHECKED, Fence, Outcome};
const SUPPORT: StepId = StepId(10);
const INSTALL: StepId = StepId(20);
const AGENT: StepId = StepId(30);
const ADMIN: StepId = StepId(55);
/// The detail of a step that was checked and has nothing more to say.
pub(crate) const STEP_CHECKED: &str = "Checked.";
/// Shown when a Windows observation fails; nothing was changed.
pub(crate) const OBSERVATION_UNAVAILABLE: &str =
    "Windows couldn't be checked just now. Nothing was changed. Check again.";
/// Shown when the plan for a change can't be prepared; nothing was changed.
pub(crate) const PLAN_UNAVAILABLE: &str =
    "Setup couldn't prepare this change. Nothing was changed. Try again.";
/// Shown while the agent has not answered its status check yet.
pub(crate) const AGENT_STARTING: &str =
    "Crosspane isn't answering yet. Setup checks again by itself.";
/// Shown when an installed step's files and sign-in task can't be confirmed yet.
pub(crate) const INSTALL_UNVERIFIED: &str =
    "Setup couldn't confirm Crosspane's files and sign-in task yet. Check again.";
/// The support refusal: the account, the Windows build or the folders can't run Crosspane.
pub(crate) const NOT_SUPPORTED: &str = "Setup couldn't confirm that this Windows account can run Crosspane. It needs your own signed-in session without administrator rights, with your folders on this PC's local drive. Nothing was changed.";
/// The refusal of a build that carries no approved inventory.
pub(crate) const NO_INVENTORY: &str = "This installer build has no approved inventory, so nothing will be copied, started or removed.";
/// The refusal while an earlier change is unsettled.
pub(crate) const UNSETTLED: &str = "An earlier Crosspane change hasn't finished. Open \"Remove or repair Crosspane…\" to settle it first. Nothing was changed.";
/// The refusal when the payload folder is missing.
pub(crate) const NO_PAYLOAD: &str = "This installer can't find its payload folder. Run crosspane-installer.exe from the unpacked Crosspane folder that also holds the payload folder. Nothing was changed.";
/// The refusal when the sign-in task was changed outside setup.
pub(crate) const TASK_PRESERVED: &str = "Crosspane's sign-in task was turned off or changed outside setup, so setup leaves it as it is. Nothing was changed.";
/// The refusal when no more specific reason is known.
pub(crate) const NOT_YET: &str = "Windows facts don't allow this change right now. Nothing was changed. Check again, or open \"Remove or repair Crosspane…\".";
/// The refusal when the process is elevated, impersonating or outside the user's own session.
pub(crate) const NOT_OWN_SESSION: &str = "Crosspane Setup runs only in your own signed-in Windows session, without administrator rights. Close it and open crosspane-installer.exe again normally, not with \"Run as administrator\".";
/// The removal choice that also tears down the firewall rule and the display driver.
const TEARDOWN_CHOICE: u16 = 2;
pub(crate) const TEARDOWN_LABEL: &str = "Also remove the Windows Defender Firewall rule and the Crosspane display driver (Windows asks for administrator approval)";
pub(crate) const ELEVATED_WAITING: &str =
    "Windows asks for administrator approval during this step";
/// The same text `Coordinator::plan_elevated` uses when the domain can't plan the teardown.
static PARENT_EXIT: AtomicBool = AtomicBool::new(false);
pub(crate) fn take_parent_exit() -> bool {
    PARENT_EXIT.swap(false, Ordering::AcqRel)
}

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

struct WindowsPlatform {
    sender: Option<mpsc::SyncSender<Command>>,
    reports: mpsc::Receiver<NativeReport>,
    agent: Box<dyn AgentPort>,
    fences: BTreeMap<u16, Fence>,
    stop: Arc<AtomicBool>,
}
impl Platform for WindowsPlatform {
    fn describe(&self) -> live::PlatformDescription {
        description()
    }
    fn submit(&mut self, job: NativeJob) -> Result<(), NativeRefusal> {
        #[cfg(all(windows, not(test)))]
        super::native_io::identity::native::refuse_impersonation()
            .map_err(|_| NativeRefusal::Unavailable("Caller token is not admitted".into()))?;
        let (slot, ticket) = match &job {
            NativeJob::Step { job, .. } => (job.step.0, job.operation.0),
            NativeJob::Maintenance(req) => (100, maintenance_id(req).0),
        };
        if slot != 100 && ![SUPPORT.0, INSTALL.0, AGENT.0, ADMIN.0].contains(&slot) {
            return Err(NativeRefusal::UnknownStep);
        }
        let fence = self.fences.entry(slot).or_default().clone();
        fence.select(ticket);
        if self
            .sender
            .as_ref()
            .ok_or(NativeRefusal::Busy)?
            .try_send(Command::Job { job, fence })
            .is_err()
        {
            return Err(NativeRefusal::Busy);
        }
        Ok(())
    }
    fn poll(&mut self) -> Vec<NativeReport> {
        self.reports.try_iter().take(32).collect()
    }
    fn agent(&mut self) -> &mut dyn AgentPort {
        self.agent.as_mut()
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        for fence in self.fences.values() {
            fence.cancel();
        }
        self.sender.take(); // No join and no cancellation/kill of the committed child.
    }
}
fn maintenance_id(req: &MaintenanceRequest) -> live::MaintenanceId {
    match req {
        MaintenanceRequest::Inspect { id }
        | MaintenanceRequest::PlanUninstall { id, .. }
        | MaintenanceRequest::ConfirmUninstall { id, .. }
        | MaintenanceRequest::ConfirmFollowUp { id, .. }
        | MaintenanceRequest::DeclineFollowUp { id, .. }
        | MaintenanceRequest::PlanRepair { id, .. }
        | MaintenanceRequest::ConfirmRepair { id, .. }
        | MaintenanceRequest::VerifyRepair { id, .. }
        | MaintenanceRequest::DiscardRepair { id, .. }
        | MaintenanceRequest::ResumeRepair { id, .. } => *id,
    }
}
struct WindowsController {
    inner: live::LiveController,
    close: Arc<CloseState>,
}
#[cfg(test)]
pub(crate) fn controller_fixture(
    inner: live::LiveController,
    close: Arc<CloseState>,
) -> Box<dyn InstallerController> {
    Box::new(WindowsController { inner, close })
}
impl InstallerController for WindowsController {
    fn view(&self) -> &WizardView {
        self.inner.view()
    }
    fn accept(&mut self, action: WizardAction) -> bool {
        self.inner.accept(action)
    }
    fn tick(&mut self) -> ControllerTick {
        let tick = self.inner.tick();
        if self.close.handed_off.swap(false, Ordering::AcqRel) {
            PARENT_EXIT.store(true, Ordering::Release);
        }
        tick
    }
    fn close(&mut self) {
        self.inner.close();
    }
    fn request_close(&mut self) -> bool {
        self.inner.request_close()
    }
}

#[cfg(all(windows, not(test)))]
pub(crate) fn open(
    payload: Option<PathBuf>,
) -> anyhow::Result<(eframe::egui::FontDefinitions, Box<dyn InstallerController>)> {
    // The Limited/impersonation guard precedes the font, the worker and the port. Every failure in
    // the chain is the same refusal, and nothing else runs before it.
    if super::native_io::identity::native::refuse_impersonation()
        .and_then(|()| super::native_io::identity::native::observe())
        .and_then(super::native_io::identity::LimitedIdentity::admit)
        .is_err()
    {
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
        None => std::env::current_exe()
            .ok()
            .as_deref()
            .and_then(payload_beside)
            .filter(|folder| folder.is_dir()),
    };
    let root = std::env::var_os("SystemRoot")
        .ok_or_else(|| anyhow::anyhow!("Windows system font location unavailable"))?;
    let fonts =
        crate::gui::load_review_font(&PathBuf::from(root).join("Fonts").join("segoeui.ttf"))?;
    let started = std::time::Instant::now();
    let clock: Clock =
        Arc::new(move || started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
    let (sender, commands) = mpsc::sync_channel(32);
    let (reports_tx, reports) = mpsc::sync_channel(32);
    let (agent_tx, agent_rx) = mpsc::sync_channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let close = Arc::new(CloseState::default());
    let worker_stop = stop.clone();
    let worker_close = close.clone();
    let worker_clock = clock.clone();
    std::thread::Builder::new()
        .name("crosspane-windows-integration".into())
        .spawn(move || {
            run(
                domains::native::NativeDomains::new(payload),
                commands,
                reports_tx,
                agent_tx,
                worker_stop,
                worker_close,
                worker_clock,
            )
        })?;
    // The slot keeps the agent's reply receiver alive for as long as the platform lives.
    let agent: Box<dyn AgentPort> = Box::new(ports::AgentSlot::new(
        sender.clone(),
        agent_rx,
        clock.clone(),
    ));
    let platform = WindowsPlatform {
        sender: Some(sender),
        reports,
        agent,
        fences: BTreeMap::new(),
        stop,
    };
    let inner = live::LiveController::new(Box::new(platform), clock.clone())?;
    Ok((fonts, Box::new(WindowsController { inner, close })))
}
#[cfg(test)]
pub(crate) fn open(
    _payload: Option<PathBuf>,
) -> anyhow::Result<(eframe::egui::FontDefinitions, Box<dyn InstallerController>)> {
    anyhow::bail!("Native integration is unavailable in the test graph")
}
/// A platform over `domains` and `agent`, with the same worker as `open` and no identity check, font
/// or native domain. The agent's reply channel lives with the worker here, because no `AgentSlot`
/// holds it in this graph.
#[cfg(test)]
pub(crate) fn platform_fixture<D: Domains + 'static>(
    domains: D,
    clock: Clock,
    agent: Box<dyn AgentPort>,
) -> Box<dyn live::Platform> {
    let (sender, commands) = mpsc::sync_channel(32);
    let (reports_tx, reports) = mpsc::sync_channel(32);
    let (agent_tx, agent_rx) = mpsc::sync_channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let close = Arc::new(CloseState::default());
    let worker_stop = stop.clone();
    std::thread::spawn(move || {
        let _replies = agent_rx;
        run(
            domains,
            commands,
            reports_tx,
            agent_tx,
            worker_stop,
            close,
            clock,
        )
    });
    Box::new(WindowsPlatform {
        sender: Some(sender),
        reports,
        agent,
        fences: BTreeMap::new(),
        stop,
    })
}

pub(crate) fn run<D: Domains>(
    domains: D,
    commands: mpsc::Receiver<Command>,
    reports: mpsc::SyncSender<NativeReport>,
    agent: mpsc::SyncSender<AgentReply>,
    stop: Arc<AtomicBool>,
    close: Arc<CloseState>,
    clock: Clock,
) {
    let mut coordinator = Coordinator::new(domains);
    let mut repair_plan: u64 = 0;
    let mut removal_plan = 0;
    // The last administrator-step report per slot (L14). Verify shows it again; only the slot's next
    // Detect or Plan forgets it.
    let mut elevated_reports: BTreeMap<u16, String> = BTreeMap::new();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        match commands.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(Command::Call { call, queued_at }) => {
                let reply = agent_call(call, queued_at, &clock);
                if agent.send(reply).is_err() {
                    break;
                }
            }
            Ok(Command::Job { job, fence }) => {
                let result = match job {
                    NativeJob::Step { job, consent, .. } => {
                        let mut detail = STEP_CHECKED.to_owned();
                        let slot = job.step.0;
                        if matches!(job.stage, JobStage::Detect | JobStage::Plan) {
                            elevated_reports.remove(&slot);
                        }
                        let outcome = if !fence.admits(job.operation.0) {
                            NativeOutcome::NotSubmitted
                        } else if job.step == SUPPORT || job.step == AGENT {
                            match coordinator.detect() {
                                Ok(snapshot)
                                    if if job.step == SUPPORT {
                                        snapshot.supported && snapshot.inventory
                                    } else {
                                        snapshot.agent == domains::State::Healthy
                                    } =>
                                {
                                    match job.stage {
                                        JobStage::Detect => NativeOutcome::Detected {
                                            needs_action: false,
                                        },
                                        JobStage::Verify => NativeOutcome::Verified {
                                            source: snapshot.source,
                                            observed_at_ms: (clock)(),
                                        },
                                        JobStage::Plan => NativeOutcome::Planned {
                                            preview: "Only a fresh observation is needed".into(),
                                        },
                                        JobStage::Apply => NativeOutcome::NotSubmitted,
                                    }
                                }
                                Ok(snapshot) => {
                                    // The support refusal names the reason; the agent is still starting.
                                    detail = if job.step == SUPPORT {
                                        plan_refusal(&snapshot, Operation::Install).to_owned()
                                    } else {
                                        AGENT_STARTING.to_owned()
                                    };
                                    NativeOutcome::Waiting(WaitKind::Contract)
                                }
                                Err(_) => {
                                    detail = OBSERVATION_UNAVAILABLE.to_owned();
                                    NativeOutcome::Waiting(WaitKind::Contract)
                                }
                            }
                        } else {
                            match job.stage {
                                JobStage::Detect if job.step == ADMIN => {
                                    match coordinator.domains.elevated_detect() {
                                        Ok(check) => {
                                            detail = check.detail;
                                            NativeOutcome::Detected {
                                                needs_action: !check.configured,
                                            }
                                        }
                                        Err(_) => {
                                            detail = ELEVATED_UNCHECKED.to_owned();
                                            NativeOutcome::Waiting(WaitKind::Contract)
                                        }
                                    }
                                }
                                JobStage::Detect => match coordinator.detect() {
                                    Ok(v) => NativeOutcome::Detected {
                                        needs_action: !v.installed(),
                                    },
                                    Err(_) => {
                                        detail = OBSERVATION_UNAVAILABLE.to_owned();
                                        NativeOutcome::Waiting(WaitKind::Contract)
                                    }
                                },
                                JobStage::Plan if job.step == ADMIN => {
                                    // Admission is read first, so a refused administrator step stores no plan.
                                    match coordinator.detect() {
                                        Err(_) => {
                                            detail = PLAN_UNAVAILABLE.to_owned();
                                            NativeOutcome::Failed
                                        }
                                        Ok(snapshot) if !snapshot.allowed(Operation::Elevated) => {
                                            detail = plan_refusal(&snapshot, Operation::Elevated)
                                                .to_owned();
                                            NativeOutcome::Waiting(WaitKind::User)
                                        }
                                        Ok(_) => match coordinator.plan_elevated(
                                            slot,
                                            job.operation.0,
                                            Operation::Elevated,
                                            Some(ElevatedRequest::Setup),
                                        ) {
                                            Ok(v) => match &v.elevated {
                                                Some(ElevatedPlanning::Planned(_)) => {
                                                    NativeOutcome::Planned {
                                                        preview: compose_preview(
                                                            &preview(
                                                                Operation::Elevated,
                                                                v.snapshot.cold,
                                                            ),
                                                            v.elevated.as_ref(),
                                                            false,
                                                        ),
                                                    }
                                                }
                                                Some(ElevatedPlanning::Unavailable(reason)) => {
                                                    detail = (*reason).to_owned();
                                                    NativeOutcome::Waiting(WaitKind::User)
                                                }
                                                Some(ElevatedPlanning::NotNeeded) | None => {
                                                    detail = ELEVATED_UNCHECKED.to_owned();
                                                    NativeOutcome::Waiting(WaitKind::Contract)
                                                }
                                            },
                                            Err(_) => {
                                                detail = PLAN_UNAVAILABLE.to_owned();
                                                NativeOutcome::Failed
                                            }
                                        },
                                    }
                                }
                                JobStage::Plan => match coordinator.detect() {
                                    Err(_) => {
                                        detail = PLAN_UNAVAILABLE.to_owned();
                                        NativeOutcome::Failed
                                    }
                                    Ok(snapshot) => {
                                        let operation = snapshot.install_operation();
                                        if !snapshot.allowed(operation) {
                                            // A refused change stores no plan and no consent, so a later Apply is refunded.
                                            detail = plan_refusal(&snapshot, operation).to_owned();
                                            NativeOutcome::Failed
                                        } else {
                                            let planned = if install_setup_requested(
                                                operation,
                                                snapshot.cold,
                                            ) {
                                                coordinator.plan_elevated(
                                                    slot,
                                                    job.operation.0,
                                                    operation,
                                                    Some(ElevatedRequest::Setup),
                                                )
                                            } else {
                                                coordinator.plan(slot, job.operation.0, operation)
                                            };
                                            match planned {
                                                Ok(v) => NativeOutcome::Planned {
                                                    preview: if v.operation == Operation::Install {
                                                        compose_preview(
                                                            install_preview(v.snapshot.cold),
                                                            v.elevated.as_ref(),
                                                            true,
                                                        )
                                                    } else {
                                                        preview(v.operation, v.snapshot.cold)
                                                    },
                                                },
                                                Err(_) => {
                                                    detail = PLAN_UNAVAILABLE.to_owned();
                                                    NativeOutcome::Failed
                                                }
                                            }
                                        }
                                    }
                                },
                                JobStage::Apply => {
                                    let applied =
                                        consent.filter(|c| c.operation == job.operation).map(|c| {
                                            if coordinator.elevated_planned(slot, c.plan.0) {
                                                let _ =
                                                    reports.send(NativeReport::Step(StepReport {
                                                        job: job.clone(),
                                                        outcome: NativeOutcome::Progress,
                                                        detail: ELEVATED_WAITING.into(),
                                                    }));
                                            }
                                            coordinator.apply(
                                                slot,
                                                job.operation.0,
                                                c.plan.0,
                                                c.revision,
                                                &fence,
                                            )
                                        });
                                    match applied {
                                        Some(v) => {
                                            if v.handoff.permits_exit() {
                                                close.handed_off.store(true, Ordering::Release);
                                            }
                                            if v.outcome == Outcome::NotSubmitted
                                                && let Some(reason) =
                                                    coordinator.domains.take_first_refusal()
                                            {
                                                detail = reason.into();
                                            }
                                            let report = coordinator.domains.take_elevated_report();
                                            if !report.is_empty() {
                                                elevated_reports.insert(slot, report.join(" "));
                                                detail = std::iter::once(detail)
                                                    .chain(report)
                                                    .collect::<Vec<_>>()
                                                    .join(" ");
                                            }
                                            native_outcome(v.outcome, (clock)())
                                        }
                                        None => NativeOutcome::NotSubmitted,
                                    }
                                }
                                JobStage::Verify => {
                                    let checked = coordinator.verify(if job.step == ADMIN {
                                        Operation::Elevated
                                    } else {
                                        Operation::Install
                                    });
                                    if checked != Outcome::Verified && job.step != ADMIN {
                                        detail = INSTALL_UNVERIFIED.to_owned();
                                    }
                                    // The controller replaces the step detail with this one, so the
                                    // remembered report is joined on, as the Apply path joins it.
                                    if let Some(report) = elevated_reports.get(&slot) {
                                        detail = format!("{detail} {report}");
                                    }
                                    match checked {
                                        Outcome::Verified => NativeOutcome::Verified {
                                            source: ObservationSource::Live,
                                            observed_at_ms: (clock)(),
                                        },
                                        _ => NativeOutcome::Waiting(WaitKind::Contract),
                                    }
                                }
                            }
                        };
                        NativeReport::Step(StepReport {
                            job,
                            outcome,
                            detail,
                        })
                    }
                    NativeJob::Maintenance(req) => {
                        let id = maintenance_id(&req);
                        let resume_first = matches!(&req, MaintenanceRequest::ResumeRepair { .. });
                        let report = match req {
                            MaintenanceRequest::Inspect { .. } => {
                                let snapshot = coordinator.detect();
                                if matches!(&snapshot, Ok(s) if s.unsettled || s.terminal_history)
                                    && reports.send(NativeReport::Maintenance(MaintenanceReport::RepairResumable {
                                        id, lines: vec!["An earlier operation has retained evidence. Resume only settles provably completed artifacts and observes again; uncertain Stop or Run is never replayed".into()],
                                    })).is_err() { break; }
                                let availability = |op| {
                                    match &snapshot {
                                    Ok(s) if s.allowed(op) => Availability::Available,
                                    Ok(_) => Availability::NotAvailableYet("Current Windows facts do not admit this operation; a disabled or modified task is preserved".into()),
                                    Err(_) => Availability::Unavailable("Windows observations are unavailable".into()),
                                }
                                };
                                let repair = snapshot
                                    .as_ref()
                                    .map(|s| s.repair_operation())
                                    .unwrap_or(Operation::MetadataRepair);
                                let mut choices = vec![live::RemovalChoice {
                                    id: 1,
                                    role: crate::view::ToggleRole::DeleteIdentity,
                                    label: "Erase the Crosspane identity and trust".into(),
                                    checked: false,
                                    enabled: !matches!(&snapshot,Ok(s) if s.cold==domains::Cold::Partial),
                                }];
                                // Read-only: the teardown plan is made again at PlanUninstall.
                                choices.extend(teardown_choice(
                                    coordinator
                                        .domains
                                        .elevated_plan(ElevatedRequest::Teardown)
                                        .unwrap_or(ElevatedPlanning::Unavailable(
                                            ELEVATED_UNCHECKED,
                                        )),
                                ));
                                MaintenanceReport::Inspected {
                                    id,
                                    uninstall: availability(Operation::Removal {
                                        erase_identity: false,
                                    }),
                                    repair: availability(repair),
                                    choices,
                                }
                            }
                            MaintenanceRequest::PlanUninstall { choices, .. } => {
                                removal_plan = id.0;
                                let teardown = choices
                                    .iter()
                                    .any(|(choice, on)| *choice == TEARDOWN_CHOICE && *on)
                                    .then_some(ElevatedRequest::Teardown);
                                match coordinator.plan_elevated(
                                    101,
                                    id.0,
                                    Operation::Removal {
                                        erase_identity: choices
                                            .iter()
                                            .any(|(id, on)| *id == 1 && *on),
                                    },
                                    teardown,
                                ) {
                                    Ok(p) => MaintenanceReport::Planned {
                                        id,
                                        preview: compose_preview(
                                            &preview(p.operation, p.snapshot.cold),
                                            p.elevated.as_ref(),
                                            false,
                                        ),
                                    },
                                    Err(_) => MaintenanceReport::Refused {
                                        id,
                                        reason: "Removal cannot be admitted".into(),
                                    },
                                }
                            }
                            MaintenanceRequest::PlanRepair { .. } => {
                                repair_plan = repair_plan.saturating_add(1).max(1);
                                match coordinator.detect().and_then(|s| {
                                    coordinator.plan(102, repair_plan, s.repair_operation())
                                }) {
                                    Ok(p) => MaintenanceReport::RepairPlanned {
                                        id,
                                        plan: p.ticket,
                                        preview: preview(p.operation, p.snapshot.cold),
                                    },
                                    Err(_) => MaintenanceReport::Refused {
                                        id,
                                        reason: "Repair cannot be admitted".into(),
                                    },
                                }
                            }
                            MaintenanceRequest::ConfirmUninstall { revision, .. } => {
                                let applied =
                                    coordinator.apply(101, id.0, removal_plan, revision, &fence);
                                if applied.handoff.permits_exit() {
                                    close.handed_off.store(true, Ordering::Release);
                                }
                                let mut lines = vec![format!(
                                    "Removal submission: {:?}; completion is independently verified",
                                    applied.outcome
                                )];
                                lines.extend(coordinator.domains.take_elevated_report());
                                MaintenanceReport::Finished {
                                    id,
                                    outcome: if matches!(
                                        applied.outcome,
                                        Outcome::NotSubmitted | Outcome::Refused
                                    ) {
                                        MaintenanceOutcome::Refused
                                    } else {
                                        MaintenanceOutcome::Partial
                                    },
                                    lines,
                                }
                            }
                            MaintenanceRequest::ConfirmRepair { plan, revision, .. } => {
                                let first_recovery = matches!(coordinator.detect(),Ok(s) if matches!(s.cold,domains::Cold::Partial|domains::Cold::Stale));
                                let applied = coordinator.apply(102, id.0, plan, revision, &fence);
                                if applied.handoff.permits_exit() {
                                    close.handed_off.store(true, Ordering::Release);
                                }
                                if matches!(
                                    applied.outcome,
                                    Outcome::NotSubmitted | Outcome::Refused
                                ) {
                                    MaintenanceReport::Refused {
                                        id,
                                        reason:
                                            "Repair was not started; review the current facts again"
                                                .into(),
                                    }
                                } else {
                                    MaintenanceReport::RepairFinished {
                                        id,
                                        outcome: if applied.complete
                                            && coordinator.verify(Operation::MetadataRepair)
                                                == Outcome::Verified
                                        {
                                            RepairOutcome::Verified
                                        } else if applied.outcome == Outcome::Unknown
                                            // A completed recovery that can't be re-checked is unknown, not retained.
                                            || (first_recovery && applied.complete)
                                        {
                                            RepairOutcome::OutcomeUnknown
                                        } else {
                                            RepairOutcome::RecoveryRetained
                                        },
                                        lines: if first_recovery && applied.complete {
                                            vec!["First-install recovery completed. Review the current facts; a new install requires a new Apply.".into()]
                                        } else {
                                            vec![format!(
                                                "Repair submission: {:?}",
                                                applied.outcome
                                            )]
                                        },
                                        resumable: !applied.complete,
                                    }
                                }
                            }
                            MaintenanceRequest::ResumeRepair { .. }
                            | MaintenanceRequest::DiscardRepair { .. } => {
                                let mut recovery_resume = false;
                                let verified = if fence.admits(id.0) {
                                    coordinator.detect().and_then(|v| {
                                        if !fence.admits(id.0) {
                                            return Err(Failure::NotSubmitted);
                                        }
                                        if resume_first
                                            && matches!(
                                                v.cold,
                                                domains::Cold::Partial | domains::Cold::Stale
                                            )
                                        {
                                            recovery_resume = true;
                                            let outcome =
                                                coordinator.domains.apply(v.repair_operation())?;
                                            if !fence.admits(id.0) {
                                                return Err(Failure::Unknown);
                                            }
                                            return Ok(outcome.complete);
                                        }
                                        let settled =
                                            coordinator.domains.settle(v.repair_operation())?;
                                        if !fence.admits(id.0) {
                                            return Err(Failure::Unknown);
                                        }
                                        match coordinator.domains.verify(v.repair_operation()) {
                                            Err(Failure::NotSubmitted) if settled => {
                                                Err(Failure::Unknown)
                                            }
                                            result => result,
                                        }
                                    })
                                } else {
                                    Err(Failure::NotSubmitted)
                                };
                                if matches!(verified, Err(Failure::NotSubmitted)) {
                                    MaintenanceReport::Refused {
                                        id,
                                        reason: "No settlement was started".into(),
                                    }
                                } else {
                                    MaintenanceReport::RepairFinished {
                                        id,
                                        outcome: if matches!(verified, Ok(true)) {
                                            RepairOutcome::CheckedAfterEarlierRepair
                                        } else {
                                            RepairOutcome::OutcomeUnknown
                                        },
                                        lines: vec![if recovery_resume {
                                            "First-install recovery was requested through its explicit recovery path; no Stop or Run was replayed".to_owned()
                                        } else {
                                            "Only existing artifact settlement and fresh observation were requested; no Stop or Run was replayed".to_owned()
                                        }],
                                        resumable: !matches!(verified, Ok(true)),
                                    }
                                }
                            }
                            MaintenanceRequest::VerifyRepair { .. } => {
                                MaintenanceReport::RepairFinished {
                                    id,
                                    outcome: if coordinator.verify(Operation::MetadataRepair)
                                        == Outcome::Verified
                                    {
                                        RepairOutcome::Verified
                                    } else {
                                        RepairOutcome::OutcomeUnknown
                                    },
                                    lines: vec!["Fresh Windows verification".into()],
                                    resumable: false,
                                }
                            }
                            _ => MaintenanceReport::Refused {
                                id,
                                reason: "This follow-up is not available on Windows".into(),
                            },
                        };
                        NativeReport::Maintenance(report)
                    }
                };
                if reports.send(result).is_err() {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
fn native_outcome(outcome: Outcome, now: u64) -> NativeOutcome {
    if outcome.refunds_consent() {
        return NativeOutcome::NotSubmitted;
    }
    match outcome {
        Outcome::Submitted => NativeOutcome::Applied(ApplyOutcome::Applied),
        Outcome::Verified => NativeOutcome::Verified {
            source: ObservationSource::Live,
            observed_at_ms: now,
        },
        Outcome::Refused => NativeOutcome::Applied(ApplyOutcome::Refused),
        Outcome::Unknown => NativeOutcome::Applied(ApplyOutcome::Unknown),
        Outcome::NotSubmitted => NativeOutcome::NotSubmitted,
    }
}
fn preview(operation: Operation, cold: domains::Cold) -> String {
    match cold {
        domains::Cold::Partial => return "Roll back / remove the partial first install".into(),
        domains::Cold::Stale => return "Settle the stale first-install record".into(),
        _ => {}
    }
    match operation {
        Operation::Install => install_preview(domains::Cold::Eligible).into(),
        Operation::Upgrade => "Verify the supplied release, settle completed artifacts, and transfer this update to an owned keeper".into(),
        Operation::Removal { erase_identity:false } => "Stop Crosspane cleanly and remove only owned installation files; preserve identity and user state".into(),
        Operation::Removal { erase_identity:true } => "Stop Crosspane cleanly, erase its identity once, and remove owned installation files".into(),
        Operation::MetadataRepair => "Repair the exact task or archive settled metadata; preserve a user-disabled task".into(),
        Operation::PayloadRepair => "Verify the supplied release, transfer repair to an owned keeper, stop cleanly, replace and verify the files".into(),
        Operation::Elevated => "Ask Windows once for administrator approval to add the firewall rule and the display driver".into(),
    }
}
pub(crate) fn install_preview(cold: domains::Cold) -> &'static str {
    match cold {
        domains::Cold::Eligible => {
            "Verify the supplied release, install Crosspane, and wait for its agent to be ready"
        }
        domains::Cold::Observe => {
            "Observe the earlier first install; no registration or launch will be replayed"
        }
        domains::Cold::CompletedRemoval => {
            "Preserve completed removal history, verify the supplied release and reinstall Crosspane"
        }
        domains::Cold::Stale => {
            "Observe or retire the previous first-install operation; no activation will be replayed"
        }
        domains::Cold::Partial => {
            "Roll back the partial first install, or remove proven files; uncertain objects are retained"
        }
        domains::Cold::Unknown => "First-install admission is unknown; nothing will be submitted",
        domains::Cold::AccessDenied => "First install access denied; nothing will be submitted",
        domains::Cold::Existing => "An existing installation requires an update",
    }
}
/// Only a fresh or post-removal first install asks for the administrator step inside its preview.
pub(crate) fn install_setup_requested(operation: Operation, cold: domains::Cold) -> bool {
    operation == Operation::Install
        && matches!(
            cold,
            domains::Cold::Eligible | domains::Cold::CompletedRemoval
        )
}
/// The reason a change can't be planned now, checked in a fixed order. Only a refused plan gets a
/// reason; the caller stores no plan and no consent for it.
pub(crate) fn plan_refusal(snapshot: &domains::Snapshot, operation: Operation) -> &'static str {
    if !snapshot.supported {
        NOT_SUPPORTED
    } else if !snapshot.inventory {
        NO_INVENTORY
    } else if snapshot.unsettled {
        UNSETTLED
    } else if matches!(
        operation,
        Operation::Install | Operation::Upgrade | Operation::PayloadRepair
    ) && !snapshot.sources
    {
        NO_PAYLOAD
    } else if operation == Operation::Install
        && matches!(
            snapshot.cold,
            domains::Cold::Unknown | domains::Cold::AccessDenied | domains::Cold::Existing
        )
    {
        install_preview(snapshot.cold)
    } else if matches!(
        snapshot.task,
        domains::State::Disabled | domains::State::Mismatch
    ) {
        TASK_PRESERVED
    } else {
        NOT_YET
    }
}
/// The payload folder the installer expects beside itself: `payload` in the installer's folder.
pub(crate) fn payload_beside(installer: &std::path::Path) -> Option<PathBuf> {
    installer.parent().map(|folder| folder.join("payload"))
}
/// `base`, then the administrator step's block or its reason. A block in an install is followed
/// by the decline note, so a declined step still lets the install complete.
pub(crate) fn compose_preview(
    base: &str,
    elevated: Option<&ElevatedPlanning>,
    install: bool,
) -> String {
    match elevated {
        None | Some(ElevatedPlanning::NotNeeded) => base.to_owned(),
        Some(ElevatedPlanning::Planned(plan)) if install => {
            format!("{base}\n\n{}\n{INSTALL_DECLINE_NOTE}", plan.preview_block())
        }
        Some(ElevatedPlanning::Planned(plan)) => format!("{base}\n\n{}", plan.preview_block()),
        Some(ElevatedPlanning::Unavailable(reason)) => format!("{base}\n\n{reason}"),
    }
}
/// Removal choice 2: checked when Windows will be asked, disabled with its reason when the
/// teardown can't be checked, and absent when no administrator step is needed.
fn teardown_choice(planning: ElevatedPlanning) -> Option<live::RemovalChoice> {
    let (label, checked, enabled) = match planning {
        ElevatedPlanning::NotNeeded => return None,
        ElevatedPlanning::Planned(_) => (TEARDOWN_LABEL.to_owned(), true, true),
        ElevatedPlanning::Unavailable(reason) => {
            (format!("{TEARDOWN_LABEL} — {reason}"), false, false)
        }
    };
    Some(live::RemovalChoice {
        id: TEARDOWN_CHOICE,
        role: crate::view::ToggleRole::Grant,
        label,
        checked,
        enabled,
    })
}
#[cfg(all(windows, not(test)))]
fn agent_call(call: AgentCall, queued_at: u64, clock: &Clock) -> AgentReply {
    use super::native_io::{Cancellation, Deadline, WindowsNativeIo};
    struct SharedClock(Clock);
    impl super::native_io::Clock for SharedClock {
        fn now_ms(&self) -> u64 {
            (self.0)()
        }
    }
    let native_clock: Arc<dyn super::native_io::Clock> = Arc::new(SharedClock(clock.clone()));
    let id = call.id;
    let remaining = call
        .timeout_ms
        .saturating_sub((clock)().saturating_sub(queued_at));
    let result = (|| {
        if remaining == 0 {
            return Err(CallFailure::Unavailable);
        }
        let budget = Deadline::new(remaining, native_clock.clone(), Cancellation::default())
            .map_err(|_| CallFailure::Unavailable)?;
        let io = Arc::new(
            WindowsNativeIo::current(native_clock, &budget)
                .map_err(|_| CallFailure::Unavailable)?,
        );
        let proof = io
            .admit_support(&budget)
            .map_err(|_| CallFailure::Unavailable)?;
        let mut port =
            super::transport::WindowsAgentPort::new(io.clone(), proof, io.bound_clock(), &budget)
                .map_err(|_| CallFailure::Unavailable)?;
        port.submit(call)?;
        loop {
            if let Some(reply) = port.poll().into_iter().next() {
                return Ok(reply);
            }
            budget
                .check()
                .map_err(|_| CallFailure::TimeoutOutcomeUnknown)?;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    })();
    result.unwrap_or_else(|failure| AgentReply {
        id,
        observed_at_ms: (clock)(),
        source: ObservationSource::Demo,
        result: Err(failure),
    })
}
#[cfg(test)]
fn agent_call(call: AgentCall, _at: u64, clock: &Clock) -> AgentReply {
    AgentReply {
        id: call.id,
        observed_at_ms: (clock)(),
        source: ObservationSource::Demo,
        result: Err(CallFailure::Unavailable),
    }
}
