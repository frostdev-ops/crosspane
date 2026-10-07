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
use crosspane_installer_core::{ApplyOutcome, JobStage, ObservationSource, StepId, WaitKind};
use domains::{Domains, Failure, Operation};
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
use worker::{Coordinator, Fence, Outcome};
const SUPPORT: StepId = StepId(10);
const INSTALL: StepId = StepId(20);
const AGENT: StepId = StepId(30);
const PRACTICE: StepId = StepId(40);
const DISTRIBUTION: StepId = StepId(50);
const ADMIN: StepId = StepId(55);
const ATTENDED: StepId = StepId(56);
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
            vec![SUPPORT],
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
                        let slot = job.step.0;
                        let outcome = if !fence.admits(job.operation.0) {
                            NativeOutcome::NotSubmitted
                        } else if [PRACTICE, DISTRIBUTION, ADMIN, ATTENDED].contains(&job.step) {
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
                                JobStage::Detect => match coordinator.detect() {
                                    Ok(v) => NativeOutcome::Detected {
                                        needs_action: !v.healthy(),
                                    },
                                    Err(_) => NativeOutcome::Unsupported,
                                },
                                JobStage::Plan => match coordinator.detect().and_then(|v| {
                                    coordinator.plan(slot, job.operation.0, v.install_operation())
                                }) {
                                    Ok(v) => NativeOutcome::Planned {
                                        preview: preview(v.operation),
                                    },
                                    Err(_) => NativeOutcome::Unsupported,
                                },
                                JobStage::Apply => {
                                    let applied =
                                        consent.filter(|c| c.operation == job.operation).map(|c| {
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
                                            native_outcome(v.outcome, (clock)())
                                        }
                                        None => NativeOutcome::NotSubmitted,
                                    }
                                }
                                JobStage::Verify => match coordinator.verify(Operation::Upgrade) {
                                    Outcome::Verified => NativeOutcome::Verified {
                                        source: ObservationSource::Live,
                                        observed_at_ms: (clock)(),
                                    },
                                    _ => NativeOutcome::Waiting(WaitKind::Contract),
                                },
                            }
                        };
                        NativeReport::Step(StepReport { job, outcome, detail: "Windows operation checked; deferred capabilities remain unavailable".into() })
                    }
                    NativeJob::Maintenance(req) => {
                        let id = maintenance_id(&req);
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
                                MaintenanceReport::Inspected {
                                    id,
                                    uninstall: availability(Operation::Removal {
                                        erase_identity: false,
                                    }),
                                    repair: availability(repair),
                                    choices: vec![live::RemovalChoice {
                                        id: 1,
                                        role: crate::view::ToggleRole::DeleteIdentity,
                                        label: "Erase the Crosspane identity and trust".into(),
                                        checked: false,
                                        enabled: true,
                                    }],
                                }
                            }
                            MaintenanceRequest::PlanUninstall { choices, .. } => {
                                removal_plan = id.0;
                                match coordinator.plan(
                                    101,
                                    id.0,
                                    Operation::Removal {
                                        erase_identity: choices
                                            .iter()
                                            .any(|(id, on)| *id == 1 && *on),
                                    },
                                ) {
                                    Ok(p) => MaintenanceReport::Planned {
                                        id,
                                        preview: preview(p.operation),
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
                                        preview: preview(p.operation),
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
                                    lines: vec![format!(
                                        "Removal submission: {:?}; completion is independently verified",
                                        applied.outcome
                                    )],
                                }
                            }
                            MaintenanceRequest::ConfirmRepair { plan, revision, .. } => {
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
                                        } else if applied.outcome == Outcome::Unknown {
                                            RepairOutcome::OutcomeUnknown
                                        } else {
                                            RepairOutcome::RecoveryRetained
                                        },
                                        lines: vec![format!(
                                            "Repair submission: {:?}",
                                            applied.outcome
                                        )],
                                        resumable: !applied.complete,
                                    }
                                }
                            }
                            MaintenanceRequest::ResumeRepair { .. }
                            | MaintenanceRequest::DiscardRepair { .. } => {
                                let verified = if fence.admits(id.0) {
                                    coordinator.detect().and_then(|v| {
                                        if !fence.admits(id.0) {
                                            return Err(Failure::NotSubmitted);
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
                                    MaintenanceReport::RepairFinished { id, outcome: if matches!(verified,Ok(true)) { RepairOutcome::CheckedAfterEarlierRepair } else { RepairOutcome::OutcomeUnknown },
                                    lines: vec!["Only existing artifact settlement and fresh observation were requested; no Stop or Run was replayed".into()], resumable: !matches!(verified,Ok(true)) }
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
fn preview(operation: Operation) -> String {
    match operation {
        Operation::Install => "Fresh Windows install is not available yet (W4.1a8); nothing will be changed".into(),
        Operation::Upgrade => "Verify the supplied release, settle completed artifacts, and transfer this update to an owned keeper".into(),
        Operation::Removal { erase_identity:false } => "Stop Crosspane cleanly and remove only owned installation files; preserve identity and user state".into(),
        Operation::Removal { erase_identity:true } => "Stop Crosspane cleanly, erase its identity once, and remove owned installation files".into(),
        Operation::MetadataRepair => "Repair the exact task or archive settled metadata; preserve a user-disabled task".into(),
        Operation::PayloadRepair => "Verify the supplied release, transfer repair to an owned keeper, stop cleanly, replace and verify the files".into(),
    }
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
