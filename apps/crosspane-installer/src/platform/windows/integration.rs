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
use ports::{AgentSlot, CloseState, Command};
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
const PRACTICE: StepId = StepId(40);
const DISTRIBUTION: StepId = StepId(50);
const ADMIN: StepId = StepId(55);
const ATTENDED: StepId = StepId(56);
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

pub(crate) fn description() -> live::PlatformDescription {
    let specs = [
        (
            SUPPORT,
            Vec::new(),
            true,
            true,
            ScreenId::Compatibility,
            "Check this computer",
        ),
        (
            INSTALL,
            vec![SUPPORT],
            true,
            true,
            ScreenId::InstallPlan,
            "Install or update Crosspane",
        ),
        (
            AGENT,
            vec![INSTALL],
            true,
            true,
            ScreenId::InstallPlan,
            "Verify the running agent",
        ),
        (
            PRACTICE,
            vec![AGENT],
            false,
            true,
            ScreenId::Compatibility,
            domains::DEFERRED[0],
        ),
        (
            DISTRIBUTION,
            vec![SUPPORT],
            false,
            true,
            ScreenId::Compatibility,
            domains::DEFERRED[2],
        ),
        (
            ADMIN,
            vec![INSTALL],
            false,
            true,
            ScreenId::Compatibility,
            domains::DEFERRED[1],
        ),
        (
            ATTENDED,
            vec![AGENT],
            false,
            true,
            ScreenId::Compatibility,
            domains::DEFERRED[3],
        ),
    ];
    live::PlatformDescription {
        platform: AgentPlatform::Windows,
        machine_label: "Windows PC".into(),
        steps: specs
            .into_iter()
            .map(
                |(id, prerequisites, required_for_installed, required_for_ready, screen, label)| {
                    live::NativeStep {
                        id,
                        prerequisites,
                        required_for_installed,
                        required_for_ready,
                        screen,
                        group: ProgressGroup::Install,
                        label: label.into(),
                        action_label: "Review".into(),
                        uses_status: false,
                        settles_with_peer: false,
                        agent_apply: None,
                    }
                },
            )
            .collect(),
        connect_after: vec![AGENT],
        ready_after: vec![AGENT, PRACTICE, DISTRIBUTION, ADMIN, ATTENDED],
        hiding_choice: false,
        resume_note: Some(domains::DEFERRED[3].into()),
    }
}

struct WindowsPlatform {
    sender: Option<mpsc::SyncSender<Command>>,
    reports: mpsc::Receiver<NativeReport>,
    agent: AgentSlot,
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
        if slot != 100
            && ![
                SUPPORT.0,
                INSTALL.0,
                AGENT.0,
                PRACTICE.0,
                DISTRIBUTION.0,
                ADMIN.0,
                ATTENDED.0,
            ]
            .contains(&slot)
        {
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
        &mut self.agent
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
    // The original GUI caller's Limited/impersonation guard precedes the font and worker.
    super::native_io::identity::native::refuse_impersonation()?;
    let token = super::native_io::identity::native::observe()?;
    super::native_io::identity::LimitedIdentity::admit(token)?;
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
    let agent = AgentSlot::new(sender.clone(), agent_rx, clock.clone());
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
                        let mut detail =
                            "Windows operation checked; deferred capabilities remain unavailable"
                                .to_owned();
                        let slot = job.step.0;
                        if matches!(job.stage, JobStage::Detect | JobStage::Plan) {
                            elevated_reports.remove(&slot);
                        }
                        let outcome = if !fence.admits(job.operation.0) {
                            NativeOutcome::NotSubmitted
                        } else if [PRACTICE, DISTRIBUTION, ATTENDED].contains(&job.step) {
                            NativeOutcome::Unsupported
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
                                _ => NativeOutcome::Waiting(WaitKind::Contract),
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
                                        Err(_) => NativeOutcome::Unsupported,
                                    }
                                }
                                JobStage::Detect => match coordinator.detect() {
                                    Ok(v) => NativeOutcome::Detected {
                                        needs_action: !v.healthy(),
                                    },
                                    Err(_) => NativeOutcome::Unsupported,
                                },
                                JobStage::Plan if job.step == ADMIN => match coordinator
                                    .plan_elevated(
                                        slot,
                                        job.operation.0,
                                        Operation::Elevated,
                                        Some(ElevatedRequest::Setup),
                                    ) {
                                    Ok(v) => NativeOutcome::Planned {
                                        preview: compose_preview(
                                            &preview(Operation::Elevated, v.snapshot.cold),
                                            v.elevated.as_ref(),
                                            false,
                                        ),
                                    },
                                    Err(_) => NativeOutcome::Unsupported,
                                },
                                JobStage::Plan => match coordinator.detect().and_then(|v| {
                                    let operation = v.install_operation();
                                    if install_setup_requested(operation, v.cold) {
                                        coordinator.plan_elevated(
                                            slot,
                                            job.operation.0,
                                            operation,
                                            Some(ElevatedRequest::Setup),
                                        )
                                    } else {
                                        coordinator.plan(slot, job.operation.0, operation)
                                    }
                                }) {
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
                                    Err(_) => NativeOutcome::Unsupported,
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
                                        Operation::Upgrade
                                    });
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
