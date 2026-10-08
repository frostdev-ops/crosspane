//! Exact pure integration cases; no Windows backend, task, agent, file or GUI is opened.
use crosspane_installer::{agent_contract, gui, live, view};
#[path = "../src/platform/windows/integration.rs"]
#[allow(dead_code)]
mod integration;
use crosspane_installer_core::ObservationSource;
use crosspane_installer_core::elevated::step::{
    ElevatedPlan, INSTALL_DECLINE_NOTE, PREVIEW_HEADER,
};
use crosspane_installer_core::elevated::{AgentProgram, InstallId, RuleScope, Verb};
use domains::*;
use integration::{domains, ports, worker};
use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use worker::*;

fn snapshot() -> Snapshot {
    Snapshot {
        supported: true,
        inventory: true,
        sources: true,
        payload: State::Healthy,
        task: State::Healthy,
        agent: State::Healthy,
        cold: Cold::Existing,
        terminal_history: false,
        unsettled: false,
        correlation: vec![1],
        source: ObservationSource::Live,
    }
}
/// The elevated calls the worker made. `run` takes the fake by value, so tests keep a clone of
/// `Fake::elevated` and read this log once the worker thread has finished.
#[derive(Default)]
struct ElevatedLog {
    requests: Vec<ElevatedRequest>,
    detects: usize,
    applied: Vec<(Operation, Option<ElevatedPlan>)>,
    verified: Vec<Operation>,
}
fn locked(shared: &Mutex<ElevatedLog>) -> MutexGuard<'_, ElevatedLog> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
struct Fake {
    observed: Snapshot,
    applications: usize,
    mutations: usize,
    stops: usize,
    starts: usize,
    settlement: u8,
    interrupt_settlement: bool,
    failure: Option<Failure>,
    handoff: Handoff,
    artifact_work: Option<(bool, RepairArtifact)>,
    copies: usize,
    archives: usize,
    archive_full: bool,
    verify_failure: Option<Failure>,
    first_refusal: Option<&'static str>,
    /// Shared with the test; see `ElevatedLog`.
    elevated: Arc<Mutex<ElevatedLog>>,
    /// Configured `elevated_plan` results; a request without one is `NotNeeded`.
    elevated_plans: Vec<(ElevatedRequest, Result<ElevatedPlanning, Failure>)>,
    elevated_detection: Result<ElevatedDetection, Failure>,
    /// Returned once by `apply_elevated` in place of the plain apply.
    elevated_apply: Option<Result<Dispatch, Failure>>,
    /// Returned once by `take_elevated_report`.
    elevated_report: Vec<String>,
}
impl Fake {
    fn new() -> Self {
        Self {
            observed: snapshot(),
            applications: 0,
            mutations: 0,
            stops: 0,
            starts: 0,
            settlement: 0,
            interrupt_settlement: false,
            failure: None,
            handoff: Handoff::Committed,
            artifact_work: None,
            copies: 0,
            archives: 0,
            archive_full: false,
            verify_failure: None,
            first_refusal: None,
            elevated: Arc::default(),
            elevated_plans: Vec::new(),
            elevated_detection: Err(Failure::NotSubmitted),
            elevated_apply: None,
            elevated_report: Vec::new(),
        }
    }
}
impl Domains for Fake {
    fn observe(&mut self) -> Result<Snapshot, Failure> {
        Ok(self.observed.clone())
    }
    fn settle(&mut self, _op: Operation) -> Result<bool, Failure> {
        if let Some((outer, repair)) = self.artifact_work.take() {
            return settle_artifacts(outer, repair, self);
        }
        if self.settlement == 0 {
            return Ok(false);
        }
        if self.settlement == 1 {
            self.mutations += 1; // Original exact identity + intent has already been admitted by this fake owner.
            self.settlement = 2;
            if self.interrupt_settlement {
                return Err(Failure::Unknown);
            }
        }
        self.settlement = 0;
        self.observed.terminal_history = false;
        Ok(true)
    }
    fn apply(&mut self, op: Operation) -> Result<Dispatch, Failure> {
        self.applications += 1;
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        self.mutations += 1;
        if matches!(
            op,
            Operation::Upgrade | Operation::PayloadRepair | Operation::Removal { .. }
        ) {
            self.stops += 1;
        }
        if matches!(
            op,
            Operation::Install | Operation::Upgrade | Operation::PayloadRepair
        ) {
            self.starts += 1;
        }
        self.observed = snapshot();
        if matches!(op, Operation::Removal { .. }) {
            self.observed.payload = State::Missing;
            self.observed.task = State::Missing;
            self.observed.agent = State::Missing;
        }
        Ok(Dispatch {
            handoff: if op == Operation::Install {
                Handoff::NotCommitted
            } else {
                self.handoff
            },
            complete: op == Operation::Install,
        })
    }
    fn take_first_refusal(&mut self) -> Option<&'static str> {
        self.first_refusal.take()
    }
    fn verify(&mut self, op: Operation) -> Result<bool, Failure> {
        locked(&self.elevated).verified.push(op);
        if let Some(failure) = self.verify_failure {
            return Err(failure);
        }
        Ok(self.observed.verified(op))
    }
    fn elevated_plan(&mut self, request: ElevatedRequest) -> Result<ElevatedPlanning, Failure> {
        locked(&self.elevated).requests.push(request);
        self.elevated_plans
            .iter()
            .find(|(configured, _)| *configured == request)
            .map_or(Ok(ElevatedPlanning::NotNeeded), |(_, result)| {
                result.clone()
            })
    }
    fn elevated_detect(&mut self) -> Result<ElevatedDetection, Failure> {
        locked(&self.elevated).detects += 1;
        self.elevated_detection.clone()
    }
    fn apply_elevated(
        &mut self,
        operation: Operation,
        elevated: Option<&ElevatedPlan>,
    ) -> Result<Dispatch, Failure> {
        locked(&self.elevated)
            .applied
            .push((operation, elevated.cloned()));
        match self.elevated_apply.take() {
            Some(result) => result,
            None => self.apply(operation),
        }
    }
    fn take_elevated_report(&mut self) -> Vec<String> {
        std::mem::take(&mut self.elevated_report)
    }
}
impl ArtifactSettlement for Fake {
    type Error = Failure;
    fn retire_repair_copy(&mut self) -> Result<(), Failure> {
        self.copies += 1;
        Ok(())
    }
    fn settle_outer_history(&mut self) -> Result<(), Failure> {
        self.copies += 1;
        if self.archive_full {
            return Ok(()); // Known read-only capacity refusal; no intent/index/archive write.
        }
        self.archives += 1;
        self.observed.terminal_history = false;
        Ok(())
    }
}
struct NoAgent;
impl agent_contract::AgentPort for NoAgent {
    fn submit(&mut self, _: agent_contract::AgentCall) -> Result<(), agent_contract::CallFailure> {
        Err(agent_contract::CallFailure::Unavailable)
    }
    fn poll(&mut self) -> Vec<agent_contract::AgentReply> {
        vec![]
    }
}
struct QuietPlatform {
    agent: NoAgent,
}
impl live::Platform for QuietPlatform {
    fn describe(&self) -> live::PlatformDescription {
        integration::description()
    }
    fn submit(&mut self, _: live::NativeJob) -> Result<(), live::NativeRefusal> {
        Ok(())
    }
    fn poll(&mut self) -> Vec<live::NativeReport> {
        vec![]
    }
    fn agent(&mut self) -> &mut dyn agent_contract::AgentPort {
        &mut self.agent
    }
    fn shutdown(&mut self) {}
}
#[allow(clippy::unwrap_used)] // Test-only plan setup; failure should fail the fixture.
fn apply<D: Domains>(coordinator: &mut Coordinator<D>, op: Operation) -> Applied {
    coordinator.plan(1, 1, op).unwrap();
    let fence = Fence::default();
    fence.select(2);
    coordinator.apply(1, 2, 1, 4, &fence)
}
#[test]
fn five_operation_stage_mapping_and_fresh_verification() {
    for op in [
        Operation::Install,
        Operation::Upgrade,
        Operation::Removal {
            erase_identity: false,
        },
        Operation::MetadataRepair,
        Operation::PayloadRepair,
    ] {
        let mut c = Coordinator::new(Fake::new());
        c.detect().unwrap();
        let result = apply(&mut c, op);
        if op == Operation::Install {
            assert_eq!(result.outcome, Outcome::NotSubmitted);
            assert_eq!(
                (
                    c.domains.applications,
                    c.domains.mutations,
                    c.domains.stops,
                    c.domains.starts
                ),
                (0, 0, 0, 0)
            );
        } else {
            assert_eq!(result.outcome, Outcome::Submitted);
            assert_eq!(c.domains.applications, 1);
            assert_eq!(c.verify(op), Outcome::Verified);
            c.domains.observed.agent = State::Unknown;
            assert_ne!(c.verify(op), Outcome::Verified);
            c.domains.observed = snapshot();
            c.domains.observed.source = ObservationSource::Demo;
            assert_ne!(c.verify(op), Outcome::Verified);
        }
    }
}
#[test]
fn stale_consent_and_queued_cancel_prevent_dispatch() {
    let mut c = Coordinator::new(Fake::new());
    c.plan(1, 1, Operation::Upgrade).unwrap();
    let f = Fence::default();
    f.select(2);
    assert_eq!(c.apply(1, 2, 8, 1, &f).outcome, Outcome::NotSubmitted);
    assert_eq!(c.apply(1, 2, 1, 0, &f).outcome, Outcome::NotSubmitted);
    f.select(3);
    assert_eq!(c.apply(1, 2, 1, 1, &f).outcome, Outcome::NotSubmitted);
    f.cancel();
    assert_eq!(c.apply(1, 3, 1, 1, &f).outcome, Outcome::NotSubmitted);
    assert_eq!((c.domains.applications, c.domains.mutations), (0, 0));
    let mut c = Coordinator::new(Fake::new());
    c.plan(1, 1, Operation::Upgrade).unwrap();
    c.domains.observed.correlation = vec![2];
    let f = Fence::default();
    f.select(2);
    assert_eq!(c.apply(1, 2, 1, 1, &f).outcome, Outcome::NotSubmitted);
    assert_eq!(c.domains.applications, 0);
}
#[test]
fn not_submitted_refunds_but_unknown_never_replays() {
    let mut c = Coordinator::new(Fake::new());
    c.domains.failure = Some(Failure::NotSubmitted);
    let first = apply(&mut c, Operation::Upgrade);
    assert!(first.outcome.refunds_consent());
    c.domains.failure = None;
    let f = Fence::default();
    f.select(3);
    assert_eq!(c.apply(1, 3, 1, 1, &f).outcome, Outcome::Submitted);
    let mut c = Coordinator::new(Fake::new());
    c.domains.failure = Some(Failure::Unknown);
    let first = apply(&mut c, Operation::Upgrade);
    assert!(!first.outcome.refunds_consent());
    assert_eq!(first.outcome, Outcome::Unknown);
    c.domains.failure = None;
    let f = Fence::default();
    f.select(3);
    assert_eq!(c.apply(1, 3, 1, 1, &f).outcome, Outcome::Unknown);
    assert_eq!(c.domains.applications, 1);
    c.plan(2, 4, Operation::PayloadRepair).unwrap();
    f.select(5);
    assert_eq!(c.apply(2, 5, 4, 1, &f).outcome, Outcome::Unknown);
    assert_eq!(c.domains.applications, 1); // Changing the operation slot cannot bypass Unknown.
    let mut c = Coordinator::new(Fake::new());
    c.domains.settlement = 1;
    c.domains.failure = Some(Failure::NotSubmitted);
    assert_eq!(apply(&mut c, Operation::Upgrade).outcome, Outcome::Unknown);
    assert_eq!(c.domains.mutations, 1);
}
#[test]
fn parent_exit_requires_actual_committed_handoff() {
    assert!(!Handoff::NotCommitted.permits_exit());
    assert!(!Handoff::Unknown.permits_exit());
    for handoff in [Handoff::NotCommitted, Handoff::Unknown] {
        let mut c = Coordinator::new(Fake::new());
        c.domains.handoff = handoff;
        let attempted = apply(&mut c, Operation::Upgrade);
        assert_eq!(attempted.outcome, Outcome::Unknown);
        assert!(!attempted.handoff.permits_exit());
    }
    for handoff in [Handoff::Committed, Handoff::Complete] {
        let mut c = Coordinator::new(Fake::new());
        c.domains.handoff = handoff;
        assert!(
            apply(&mut c, Operation::PayloadRepair)
                .handoff
                .permits_exit()
        );
    }
    for observed in [CommitObservation::Retained, CommitObservation::NoAnswer] {
        assert_eq!(committed_handoff(false, observed), Handoff::Unknown);
        assert_eq!(committed_handoff(true, observed), Handoff::Committed);
    }
    assert_eq!(
        committed_handoff(true, CommitObservation::Ready),
        Handoff::NotCommitted
    );
    assert_eq!(
        committed_handoff(true, CommitObservation::Refused),
        Handoff::Unknown
    );
    let mut sequence = [CommitObservation::NoAnswer, CommitObservation::Retained].into_iter();
    let mut waits = 0;
    assert_eq!(
        await_committed_handoff(true, || sequence.next().unwrap(), || true, || waits += 1),
        Handoff::Committed
    );
    assert_eq!(waits, 1);
    assert_eq!(
        await_committed_handoff(
            true,
            || panic!("expired budget must not call native"),
            || false,
            || {}
        ),
        Handoff::Unknown
    );
    let mut attempted = 0;
    assert_eq!(
        await_committed_handoff(
            true,
            || {
                attempted += 1;
                CommitObservation::Refused
            },
            || true,
            || {}
        ),
        Handoff::Unknown
    );
    assert_eq!(attempted, 1); // A failed completed observe cannot be promoted to timeout.
    let budget = std::cell::Cell::new(true);
    let mut attempted = 0;
    assert_eq!(
        await_committed_handoff(
            true,
            || {
                attempted += 1;
                budget.set(false);
                CommitObservation::NoAnswer
            },
            || budget.get(),
            || {}
        ),
        Handoff::Committed
    );
    assert_eq!(attempted, 1); // Only an actual completed Timeout attempt admits this fallback.

    // Production run -> shared handoff -> actual WindowsController -> PARENT_EXIT.
    // No manual atomic assignment stands in for the Commit result.
    for (op, handoff, failure) in [
        (Operation::Upgrade, Handoff::Committed, None),
        (Operation::PayloadRepair, Handoff::Committed, None),
        (Operation::Upgrade, Handoff::NotCommitted, None),
        (Operation::Upgrade, Handoff::Unknown, None),
        (
            Operation::PayloadRepair,
            Handoff::Unknown,
            Some(Failure::NotSubmitted),
        ),
    ] {
        let (tx, rx) = mpsc::sync_channel(32);
        let (reports, report_rx) = mpsc::sync_channel(32);
        let (agents, _agent_rx) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let close = Arc::new(ports::CloseState::default());
        let clock: live::Clock = Arc::new(|| 1);
        let mut fake = Fake::new();
        fake.handoff = handoff;
        fake.failure = failure;
        if op == Operation::PayloadRepair {
            fake.observed.payload = State::Mismatch;
        }
        let thread_stop = stop.clone();
        let thread_close = close.clone();
        let thread_clock = clock.clone();
        let thread = std::thread::spawn(move || {
            integration::run(
                fake,
                rx,
                reports,
                agents,
                thread_stop,
                thread_close,
                thread_clock,
            )
        });
        let fence = Fence::default();
        let planned_ticket = if op == Operation::PayloadRepair {
            fence.select(1);
            tx.send(ports::Command::Job {
                fence: fence.clone(),
                job: live::NativeJob::Maintenance(live::MaintenanceRequest::PlanRepair {
                    id: live::MaintenanceId(1),
                    status: None,
                }),
            })
            .unwrap();
            match report_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
            {
                live::NativeReport::Maintenance(live::MaintenanceReport::RepairPlanned {
                    plan,
                    ..
                }) => plan,
                _ => panic!("production repair plan expected"),
            }
        } else {
            fence.select(1);
            tx.send(ports::Command::Job {
                fence: fence.clone(),
                job: live::NativeJob::Step {
                    job: crosspane_installer_core::JobIntent {
                        step: crosspane_installer_core::StepId(20),
                        operation: crosspane_installer_core::OperationId(1),
                        stage: crosspane_installer_core::JobStage::Plan,
                    },
                    consent: None,
                    status: None,
                },
            })
            .unwrap();
            assert!(matches!(
                report_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap(),
                live::NativeReport::Step(live::StepReport {
                    outcome: live::NativeOutcome::Planned { .. },
                    ..
                })
            ));
            1
        };
        fence.select(2);
        let job = if op == Operation::PayloadRepair {
            live::NativeJob::Maintenance(live::MaintenanceRequest::ConfirmRepair {
                id: live::MaintenanceId(2),
                plan: planned_ticket,
                revision: 1,
                status: None,
            })
        } else {
            live::NativeJob::Step {
                job: crosspane_installer_core::JobIntent {
                    step: crosspane_installer_core::StepId(20),
                    operation: crosspane_installer_core::OperationId(2),
                    stage: crosspane_installer_core::JobStage::Apply,
                },
                consent: Some(live::Consent {
                    plan: crosspane_installer_core::OperationId(planned_ticket),
                    operation: crosspane_installer_core::OperationId(2),
                    revision: 1,
                }),
                status: None,
            }
        };
        tx.send(ports::Command::Job { job, fence }).unwrap();
        let actual_report = report_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        if failure == Some(Failure::NotSubmitted) {
            assert!(matches!(
                actual_report,
                live::NativeReport::Maintenance(live::MaintenanceReport::Refused { .. })
            ));
        }
        if !handoff.permits_exit() {
            let cancelled = Fence::default();
            cancelled.cancel();
            for request in [
                live::MaintenanceRequest::ResumeRepair {
                    id: live::MaintenanceId(3),
                    status: None,
                },
                live::MaintenanceRequest::DiscardRepair {
                    id: live::MaintenanceId(3),
                    status: None,
                },
            ] {
                tx.send(ports::Command::Job {
                    job: live::NativeJob::Maintenance(request),
                    fence: cancelled.clone(),
                })
                .unwrap();
                assert!(matches!(
                    report_rx
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap(),
                    live::NativeReport::Maintenance(live::MaintenanceReport::Refused { .. })
                ));
            }
        }
        assert_eq!(
            close.handed_off.load(Ordering::Acquire),
            handoff.permits_exit()
        );
        let inner =
            live::LiveController::new(Box::new(QuietPlatform { agent: NoAgent }), clock).unwrap();
        let mut controller = integration::controller_fixture(inner, close);
        assert!(!integration::take_parent_exit());
        controller.tick();
        assert_eq!(integration::take_parent_exit(), handoff.permits_exit());
        controller.tick();
        assert!(!integration::take_parent_exit());
        controller.close();
        assert!(controller.request_close()); // Already closed delegates to the shared early return.
        stop.store(true, Ordering::Release);
        drop(tx);
        thread.join().unwrap();
    }
}
#[test]
fn terminal_artifact_settlement_reopens_without_replay() {
    for op in [
        Operation::Upgrade,
        Operation::PayloadRepair,
        Operation::Removal {
            erase_identity: false,
        },
    ] {
        let mut c = Coordinator::new(Fake::new());
        c.domains.observed.terminal_history = true;
        c.domains.observed.unsettled = publication_unsettled(false, true);
        c.domains.artifact_work = Some((true, RepairArtifact::RetiredOrAbsent));
        c.domains.archive_full = true;
        assert_eq!(apply(&mut c, op).outcome, Outcome::Submitted);
        assert_eq!(
            (c.domains.copies, c.domains.archives, c.domains.applications),
            (1, 0, 1)
        );
    }
    assert!(publication_unsettled(false, false)); // Actual uncertain publication still gates.
    assert!(!publication_unsettled(true, false));
    // These damaged facts would make a6 classify Report. The production settlement
    // driver still archives only outer evidence and reaches the next begin_*.
    for op in [
        Operation::Upgrade,
        Operation::PayloadRepair,
        Operation::Removal {
            erase_identity: false,
        },
    ] {
        let mut c = Coordinator::new(Fake::new());
        c.domains.observed.payload = State::Mismatch;
        c.domains.observed.agent = State::Missing;
        c.domains.observed.terminal_history = true;
        c.domains.artifact_work = Some((true, RepairArtifact::RetiredOrAbsent));
        assert_eq!(apply(&mut c, op).outcome, Outcome::Submitted);
        assert_eq!(
            (c.domains.copies, c.domains.archives, c.domains.applications),
            (1, 1, 1)
        );
    }
    let mut no_work = Fake::new();
    no_work.failure = Some(Failure::NotSubmitted);
    no_work.artifact_work = Some((false, RepairArtifact::RetiredOrAbsent));
    let mut c = Coordinator::new(no_work);
    assert_eq!(
        apply(&mut c, Operation::Upgrade).outcome,
        Outcome::NotSubmitted
    );
    assert_eq!(
        (c.domains.copies, c.domains.archives, c.domains.mutations),
        (0, 0, 0)
    );
    let mut settled = Fake::new();
    settled.observed.payload = State::Mismatch;
    settled.observed.agent = State::Missing;
    settled.observed.terminal_history = true;
    let before = settled.observed.clone();
    assert!(settle_artifacts(true, RepairArtifact::RetiredOrAbsent, &mut settled).unwrap());
    assert_eq!(
        (
            settled.observed.payload,
            settled.observed.task,
            settled.observed.agent
        ),
        (before.payload, before.task, before.agent)
    );
    assert_eq!(
        (
            settled.stops,
            settled.starts,
            settled.applications,
            settled.mutations
        ),
        (0, 0, 0, 0)
    );
    // Production Resume/Discard never says "nothing started" after settlement.
    for settled in [false, true] {
        for request in [
            live::MaintenanceRequest::ResumeRepair {
                id: live::MaintenanceId(9),
                status: None,
            },
            live::MaintenanceRequest::DiscardRepair {
                id: live::MaintenanceId(9),
                status: None,
            },
        ] {
            let mut fake = Fake::new();
            fake.settlement = u8::from(settled);
            fake.observed.terminal_history = settled;
            fake.verify_failure = Some(Failure::NotSubmitted);
            let (tx, commands) = mpsc::sync_channel(32);
            let (reports, rx) = mpsc::sync_channel(32);
            let (agents, _agent_rx) = mpsc::sync_channel(32);
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let thread = std::thread::spawn(move || {
                integration::run(
                    fake,
                    commands,
                    reports,
                    agents,
                    thread_stop,
                    Arc::new(ports::CloseState::default()),
                    Arc::new(|| 1),
                )
            });
            let fence = Fence::default();
            fence.select(9);
            tx.send(ports::Command::Job {
                job: live::NativeJob::Maintenance(request),
                fence,
            })
            .unwrap();
            let report = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            if settled {
                assert!(matches!(
                    report,
                    live::NativeReport::Maintenance(live::MaintenanceReport::RepairFinished {
                        outcome: live::RepairOutcome::OutcomeUnknown,
                        ..
                    })
                ));
            } else {
                assert!(matches!(
                    report,
                    live::NativeReport::Maintenance(live::MaintenanceReport::Refused { .. })
                ));
            }
            stop.store(true, Ordering::Release);
            drop(tx);
            thread.join().unwrap();
        }
    }
    for op in [Operation::Upgrade, Operation::PayloadRepair] {
        let mut c = Coordinator::new(Fake::new());
        c.domains.settlement = 1;
        c.domains.interrupt_settlement = true;
        assert_eq!(apply(&mut c, op).outcome, Outcome::Unknown);
        assert_eq!((c.domains.applications, c.domains.mutations), (0, 1));
        c.domains.interrupt_settlement = false;
        // Explicit reopen settles the original admitted intent without repeating its effect.
        assert!(c.domains.settle(op).unwrap());
        assert!(!c.domains.settle(op).unwrap());
        assert_eq!(c.domains.mutations, 1);
        let f = Fence::default();
        f.select(3);
        assert_eq!(c.apply(1, 3, 1, 1, &f).outcome, Outcome::Unknown);
        assert_eq!((c.domains.stops, c.domains.starts), (0, 0));
    }
}
#[test]
fn healthy_detect_and_deferred_features_never_claim_mutation_or_ready() {
    let mut c = Coordinator::new(Fake::new());
    assert!(c.detect().unwrap().healthy());
    c.domains.observed.terminal_history = true;
    assert!(c.detect().unwrap().healthy());
    assert_eq!(c.verify(Operation::Upgrade), Outcome::Verified);
    assert_eq!((c.domains.applications, c.domains.mutations), (0, 0));
    let description = integration::description();
    assert!(
        description
            .steps
            .iter()
            .all(|s| (10..=59).contains(&s.id.0))
    );
    for label in DEFERRED {
        let step = description
            .steps
            .iter()
            .find(|s| s.label == *label)
            .unwrap();
        assert!(step.required_for_ready);
        assert!(description.ready_after.contains(&step.id));
        assert!(!step.required_for_installed);
    }
    struct NoAgent;
    impl agent_contract::AgentPort for NoAgent {
        fn submit(
            &mut self,
            _: agent_contract::AgentCall,
        ) -> Result<(), agent_contract::CallFailure> {
            Err(agent_contract::CallFailure::Unavailable)
        }
        fn poll(&mut self) -> Vec<agent_contract::AgentReply> {
            vec![]
        }
    }
    // Build the actual shared readiness graph, catching invalid or colliding native step IDs.
    struct Platform {
        agent: NoAgent,
    }
    impl live::Platform for Platform {
        fn describe(&self) -> live::PlatformDescription {
            integration::description()
        }
        fn submit(&mut self, _: live::NativeJob) -> Result<(), live::NativeRefusal> {
            Ok(())
        }
        fn poll(&mut self) -> Vec<live::NativeReport> {
            vec![]
        }
        fn agent(&mut self) -> &mut dyn agent_contract::AgentPort {
            &mut self.agent
        }
        fn shutdown(&mut self) {}
    }
    let _controller = live::LiveController::new(
        Box::new(Platform { agent: NoAgent }),
        std::sync::Arc::new(|| 1),
    )
    .unwrap();
    for state in [
        State::Missing,
        State::Mismatch,
        State::Disabled,
        State::Unavailable,
        State::Unknown,
    ] {
        c.domains.observed.task = state;
        assert!(!c.detect().unwrap().healthy());
    }
}

#[test]
fn first_install_apply_uses_worker_consent_fresh_verify_and_keeps_parent_open() {
    let mut fake = Fake::new();
    fake.observed.payload = State::Missing;
    fake.observed.task = State::Missing;
    fake.observed.agent = State::Missing;
    fake.observed.cold = Cold::Eligible;
    let mut coordinator = Coordinator::new(fake);
    let applied = apply(&mut coordinator, Operation::Install);
    assert_eq!(applied.outcome, Outcome::Submitted);
    assert!(applied.complete);
    assert_eq!(applied.handoff, Handoff::NotCommitted);
    assert!(!applied.handoff.permits_exit());
    assert_eq!(
        (
            coordinator.domains.applications,
            coordinator.domains.stops,
            coordinator.domains.starts
        ),
        (1, 0, 1)
    );
    coordinator.domains.observed.source = ObservationSource::Demo;
    assert_ne!(coordinator.verify(Operation::Install), Outcome::Verified);
}
#[test]
fn first_install_partial_and_removal_history_refuse_before_effects() {
    // Partial routes to recovery and CompletedRemoval admits a reinstall; see the a8b tests below.
    for (cold, reason) in [
        (Cold::Unknown, "admission is unknown"),
        (Cold::AccessDenied, "access denied"),
    ] {
        let mut fake = Fake::new();
        fake.observed.task = State::Missing;
        fake.observed.agent = State::Missing;
        fake.observed.cold = cold;
        // Some published leaves must not turn this first operation into upgrade.
        assert_eq!(fake.observed.install_operation(), Operation::Install);
        fake.observed.task = State::Healthy;
        fake.observed.agent = State::Healthy;
        assert_eq!(fake.observed.install_operation(), Operation::Install);
        assert!(!fake.observed.healthy());

        assert!(integration::install_preview(cold).contains(reason));
        let mut coordinator = Coordinator::new(fake);
        assert_eq!(
            apply(&mut coordinator, Operation::Install).outcome,
            Outcome::NotSubmitted
        );
        assert_eq!(
            (
                coordinator.domains.applications,
                coordinator.domains.mutations,
                coordinator.domains.stops,
                coordinator.domains.starts
            ),
            (0, 0, 0, 0)
        );
    }
}
#[test]
fn a8b_completed_removal_admits_install_only_with_missing_task_and_agent() {
    let mut fake = Fake::new();
    fake.observed.payload = State::Missing;
    fake.observed.task = State::Missing;
    fake.observed.agent = State::Missing;
    fake.observed.cold = Cold::CompletedRemoval;
    assert_eq!(fake.observed.install_operation(), Operation::Install);
    assert!(fake.observed.allowed(Operation::Install));
    assert!(
        integration::install_preview(Cold::CompletedRemoval)
            .contains("Preserve completed removal history")
    );
    let mut coordinator = Coordinator::new(fake);
    assert_eq!(
        apply(&mut coordinator, Operation::Install).outcome,
        Outcome::Submitted
    );
    assert_eq!(coordinator.domains.applications, 1);

    let mut task_present = Fake::new();
    task_present.observed.payload = State::Missing;
    task_present.observed.task = State::Healthy;
    task_present.observed.agent = State::Missing;
    task_present.observed.cold = Cold::CompletedRemoval;
    assert!(!task_present.observed.allowed(Operation::Install));
    let mut coordinator = Coordinator::new(task_present);
    assert_eq!(
        apply(&mut coordinator, Operation::Install).outcome,
        Outcome::NotSubmitted
    );
    assert_eq!(
        (
            coordinator.domains.applications,
            coordinator.domains.mutations
        ),
        (0, 0)
    );

    let mut no_sources = Fake::new();
    no_sources.observed.payload = State::Missing;
    no_sources.observed.task = State::Missing;
    no_sources.observed.agent = State::Missing;
    no_sources.observed.cold = Cold::CompletedRemoval;
    no_sources.observed.sources = false;
    assert!(!no_sources.observed.allowed(Operation::Install));
    let mut coordinator = Coordinator::new(no_sources);
    assert_eq!(
        apply(&mut coordinator, Operation::Install).outcome,
        Outcome::NotSubmitted
    );
    assert_eq!(
        (
            coordinator.domains.applications,
            coordinator.domains.mutations
        ),
        (0, 0)
    );
}
#[test]
fn first_install_stale_cold_revision_cancel_and_live_task_have_no_dispatch() {
    let mut fake = Fake::new();
    fake.observed.payload = State::Missing;
    fake.observed.task = State::Missing;
    fake.observed.agent = State::Missing;
    fake.observed.cold = Cold::Eligible;
    let mut coordinator = Coordinator::new(fake);
    coordinator.plan(1, 1, Operation::Install).unwrap();
    let fence = Fence::default();
    fence.select(2);
    coordinator.domains.observed.correlation.push(9);
    assert_eq!(
        coordinator.apply(1, 2, 1, 4, &fence).outcome,
        Outcome::NotSubmitted
    );
    coordinator.plan(1, 1, Operation::Install).unwrap();
    fence.cancel();
    assert_eq!(
        coordinator.apply(1, 2, 1, 4, &fence).outcome,
        Outcome::NotSubmitted
    );
    coordinator.domains.observed.cold = Cold::Existing;
    coordinator.domains.observed.task = State::Healthy;
    coordinator.domains.observed.payload = State::Healthy;
    assert_eq!(
        coordinator.domains.observed.install_operation(),
        Operation::Upgrade
    );
    assert!(!coordinator.domains.observed.allowed(Operation::Install));
    assert_eq!(coordinator.domains.applications, 0);
}

#[test]
fn first_install_observation_only_reopen_is_install_until_fresh_completion() {
    let mut fake = Fake::new();
    fake.observed.cold = Cold::Observe;
    assert_eq!(fake.observed.install_operation(), Operation::Install);
    assert!(fake.observed.allowed(Operation::Install));
    assert!(!fake.observed.healthy());
    assert!(integration::install_preview(Cold::Observe).contains("no registration or launch"));
    let mut coordinator = Coordinator::new(fake);
    let applied = apply(&mut coordinator, Operation::Install);
    assert!(applied.complete);
    assert_eq!(applied.handoff, Handoff::NotCommitted);
}
#[test]
fn first_install_not_submitted_reason_survives_the_production_report_path() {
    use crosspane_installer_core::{JobIntent, JobStage, OperationId, StepId};
    let mut fake = Fake::new();
    fake.observed.payload = State::Missing;
    fake.observed.task = State::Missing;
    fake.observed.agent = State::Missing;
    fake.observed.cold = Cold::Eligible;
    fake.failure = Some(Failure::NotSubmitted);
    fake.first_refusal = Some("first install access denied; permissions unchanged");
    let (tx, commands) = mpsc::sync_channel(32);
    let (reports, rx) = mpsc::sync_channel(32);
    let (agents, _agent_rx) = mpsc::sync_channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        integration::run(
            fake,
            commands,
            reports,
            agents,
            thread_stop,
            Arc::new(ports::CloseState::default()),
            Arc::new(|| 1),
        )
    });
    let fence = Fence::default();
    fence.select(1);
    tx.send(ports::Command::Job {
        fence: fence.clone(),
        job: live::NativeJob::Step {
            job: JobIntent {
                step: StepId(20),
                operation: OperationId(1),
                stage: JobStage::Plan,
            },
            consent: None,
            status: None,
        },
    })
    .unwrap();
    rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    fence.select(2);
    tx.send(ports::Command::Job {
        fence,
        job: live::NativeJob::Step {
            job: JobIntent {
                step: StepId(20),
                operation: OperationId(2),
                stage: JobStage::Apply,
            },
            consent: Some(live::Consent {
                plan: OperationId(1),
                operation: OperationId(2),
                revision: 1,
            }),
            status: None,
        },
    })
    .unwrap();
    let report = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    match report {
        live::NativeReport::Step(report) => {
            assert_eq!(report.outcome, live::NativeOutcome::NotSubmitted);
            assert_eq!(
                report.detail,
                "first install access denied; permissions unchanged"
            );
        }
        _ => panic!("step report expected"),
    }
    stop.store(true, Ordering::Release);
    drop(tx);
    thread.join().unwrap();
}

/// Recovery model mirroring `NativeDomains`. A Partial/Stale snapshot is a real recovery effect
/// and sets `recovered`, which selects `first_recovery_settled` for MetadataRepair verification.
/// A healthy MetadataRepair is a zero-effect no-op; a fresh Install is a real effect.
struct RecoveryDomains {
    observed: Snapshot,
    applications: usize,
    mutations: usize,
    failure: bool,
    recovered: bool,
}
impl RecoveryDomains {
    fn new(observed: Snapshot, failure: bool) -> Self {
        Self {
            observed,
            applications: 0,
            mutations: 0,
            failure,
            recovered: false,
        }
    }
}
impl Domains for RecoveryDomains {
    fn observe(&mut self) -> Result<Snapshot, Failure> {
        Ok(self.observed.clone())
    }
    fn settle(&mut self, _: Operation) -> Result<bool, Failure> {
        Ok(false)
    }
    fn apply(&mut self, op: Operation) -> Result<Dispatch, Failure> {
        assert!(self.observed.allowed(op));
        self.applications += 1;
        self.recovered = false;
        let recovering = matches!(self.observed.cold, Cold::Partial | Cold::Stale);
        if recovering {
            self.recovered = true;
            self.mutations += 1; // The recovery effect may land even when its reply is lost.
        } else if op == Operation::Install {
            self.mutations += 1;
        }
        if self.failure {
            return Err(Failure::Unknown);
        }
        if recovering {
            if self.observed.cold == Cold::Stale {
                self.observed = snapshot();
            } else {
                self.observed.cold = Cold::Eligible;
                self.observed.payload = State::Missing;
                self.observed.agent = State::Missing;
                self.observed.task = State::Missing;
                self.observed.unsettled = false;
            }
        } else if op == Operation::Install {
            self.observed = snapshot();
        }
        Ok(Dispatch {
            handoff: Handoff::NotCommitted,
            complete: true,
        })
    }
    fn verify(&mut self, op: Operation) -> Result<bool, Failure> {
        if self.recovered && op == Operation::MetadataRepair {
            return Ok(self.observed.first_recovery_settled());
        }
        Ok(self.observed.verified(op))
    }
}
fn partial_snapshot() -> Snapshot {
    let mut s = snapshot();
    s.cold = Cold::Partial;
    s.payload = State::Unknown;
    s.agent = State::Missing;
    s.task = State::Mismatch;
    s.inventory = false;
    s.sources = false;
    s.unsettled = true;
    s
}
#[test]
fn a8b_partial_routes_without_payload_readers_but_never_admits_erase_or_activation() {
    let s = partial_snapshot();
    assert_eq!(s.install_operation(), Operation::MetadataRepair);
    assert_eq!(s.repair_operation(), Operation::MetadataRepair);
    assert!(s.allowed(Operation::MetadataRepair));
    assert!(s.allowed(Operation::Removal {
        erase_identity: false
    }));
    for op in [
        Operation::Install,
        Operation::Upgrade,
        Operation::PayloadRepair,
        Operation::Removal {
            erase_identity: true,
        },
    ] {
        assert!(!s.allowed(op));
    }
    let mut c = Coordinator::new(RecoveryDomains::new(s, false));
    c.detect().unwrap();
    assert_eq!(c.domains.mutations, 0);
    let result = apply(&mut c, Operation::MetadataRepair);
    assert_eq!(result.outcome, Outcome::Submitted);
    assert!(result.complete);
    assert_eq!(result.handoff, Handoff::NotCommitted);
    assert_eq!((c.domains.applications, c.domains.mutations), (1, 1));
    // The settled rollback verifies through first_recovery_settled, not the healthy predicate.
    assert_eq!(c.verify(Operation::MetadataRepair), Outcome::Verified);
    assert_eq!(c.domains.observed.cold, Cold::Eligible);
    assert_eq!(c.domains.observed.task, State::Missing);
    assert_eq!(c.domains.observed.agent, State::Missing);
    assert!(!c.domains.observed.unsettled);
}
#[test]
fn a8b_recovery_unknown_never_refunds_or_replays_and_healthy_stays_zero_effects() {
    let mut c = Coordinator::new(RecoveryDomains::new(partial_snapshot(), true));
    let result = apply(&mut c, Operation::MetadataRepair);
    assert_eq!(result.outcome, Outcome::Unknown);
    assert!(!result.outcome.refunds_consent());
    assert_eq!((c.domains.applications, c.domains.mutations), (1, 1));
    // Coordinator::apply never reports Verified; a healthy repair submits with zero effects.
    let mut healthy = Coordinator::new(RecoveryDomains::new(snapshot(), false));
    let result = apply(&mut healthy, Operation::MetadataRepair);
    assert_eq!(result.outcome, Outcome::Submitted);
    assert!(result.complete);
    assert_eq!(
        (healthy.domains.applications, healthy.domains.mutations),
        (1, 0)
    );
    assert_eq!(healthy.verify(Operation::MetadataRepair), Outcome::Verified);
}
#[test]
fn a8b_recovery_complete_clears_uncertainty_and_allows_a_new_install() {
    let mut s = partial_snapshot();
    // A fresh install needs release sources and inventory, which the routing case leaves false.
    s.inventory = true;
    s.sources = true;
    let mut c = Coordinator::new(RecoveryDomains::new(s, false));
    let repair = apply(&mut c, Operation::MetadataRepair);
    assert_eq!(repair.outcome, Outcome::Submitted);
    assert!(repair.complete);
    // A dispatch reservation is per slot, so the new install is planned on its own slot.
    c.plan(2, 3, Operation::Install).unwrap();
    let fence = Fence::default();
    fence.select(4);
    let install = c.apply(2, 4, 3, 4, &fence);
    assert_eq!(install.outcome, Outcome::Submitted);
    assert!(install.complete);
    assert_eq!(c.domains.applications, 2);
}
#[test]
fn a8b_partial_removal_through_coordinator_is_submitted_not_unknown() {
    let mut c = Coordinator::new(RecoveryDomains::new(partial_snapshot(), false));
    let removal = apply(
        &mut c,
        Operation::Removal {
            erase_identity: false,
        },
    );
    assert_eq!(removal.outcome, Outcome::Submitted);
    assert!(removal.complete);
    assert_eq!(removal.handoff, Handoff::NotCommitted);
    assert!(!removal.handoff.permits_exit());
    assert_eq!((c.domains.applications, c.domains.mutations), (1, 1));
}
/// Plans through the production report path and returns the preview the operator sees.
#[allow(clippy::unwrap_used)] // Test-only fixture; a failed send or join should fail the test.
fn plan_preview(fake: Fake) -> String {
    use crosspane_installer_core::{JobIntent, JobStage, OperationId, StepId};
    let (tx, commands) = mpsc::sync_channel(32);
    let (reports, rx) = mpsc::sync_channel(32);
    let (agents, _agent_rx) = mpsc::sync_channel(32);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::spawn(move || {
        integration::run(
            fake,
            commands,
            reports,
            agents,
            thread_stop,
            Arc::new(ports::CloseState::default()),
            Arc::new(|| 1),
        )
    });
    let fence = Fence::default();
    fence.select(1);
    tx.send(ports::Command::Job {
        fence,
        job: live::NativeJob::Step {
            job: JobIntent {
                step: StepId(20),
                operation: OperationId(1),
                stage: JobStage::Plan,
            },
            consent: None,
            status: None,
        },
    })
    .unwrap();
    let report = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    stop.store(true, Ordering::Release);
    drop(tx);
    thread.join().unwrap();
    match report {
        live::NativeReport::Step(report) => match report.outcome {
            live::NativeOutcome::Planned { preview } => preview,
            _ => panic!("planned outcome expected"),
        },
        _ => panic!("step report expected"),
    }
}
#[test]
fn a8b_partial_and_stale_previews_name_recovery_not_install() {
    for (cold, expected) in [
        (
            Cold::Partial,
            "Roll back / remove the partial first install",
        ),
        (Cold::Stale, "Settle the stale first-install record"),
    ] {
        let mut fake = Fake::new();
        fake.observed.cold = cold;
        let preview = plan_preview(fake);
        assert_eq!(preview, expected);
        assert!(!preview.contains("Verify the supplied release"));
    }
}
#[test]
fn a8b_stale_routing_is_observation_only_and_completion_reprobes_fresh_facts() {
    let mut s = snapshot();
    s.cold = Cold::Stale;
    s.unsettled = true;
    assert!(s.allowed(Operation::MetadataRepair));
    assert!(!s.allowed(Operation::Install));
    assert!(!s.allowed(Operation::Removal {
        erase_identity: false
    }));
    let mut c = Coordinator::new(RecoveryDomains::new(s, false));
    let result = apply(&mut c, Operation::MetadataRepair);
    assert_eq!(result.outcome, Outcome::Submitted);
    assert_eq!(result.handoff, Handoff::NotCommitted);
    assert_eq!(c.verify(Operation::MetadataRepair), Outcome::Verified);
}

/// A configured `elevated_plan` answer for one request.
type PlanningAnswer = Result<ElevatedPlanning, Failure>;
/// The removal choice 2 the Inspect report should show: checked, enabled and its label; `None`
/// when the choice must be absent.
type TeardownChoice = Option<(bool, bool, String)>;
/// One PlanUninstall case: the teardown answer, the removal choices, the expected preview and the
/// teardown requests the worker must make.
type UninstallCase = (
    PlanningAnswer,
    Vec<(u16, bool)>,
    String,
    Vec<ElevatedRequest>,
);
/// The administrator step's rule scope; the core consent path accepts it.
#[allow(clippy::unwrap_used)] // Test-only fixture; an invalid scope should fail the fixture.
fn rule_scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(
            r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        )
        .unwrap(),
    }
}
/// The consent-bound plan the real core builds for the firewall rule and display driver.
#[allow(clippy::unwrap_used)] // Test-only fixture; the core consent path must accept the scope.
fn setup_plan() -> ElevatedPlan {
    ElevatedPlan::new(Verb::Setup(rule_scope())).unwrap()
}
/// The consent-bound plan the real core builds for the firewall rule and driver removal.
#[allow(clippy::unwrap_used)] // Test-only fixture; the core consent path must accept the scope.
fn teardown_plan() -> ElevatedPlan {
    ElevatedPlan::new(Verb::Teardown(rule_scope())).unwrap()
}
/// A first-install candidate with nothing installed, in the cold state `cold`.
fn fresh(cold: Cold) -> Fake {
    let mut fake = Fake::new();
    fake.observed.payload = State::Missing;
    fake.observed.task = State::Missing;
    fake.observed.agent = State::Missing;
    fake.observed.cold = cold;
    fake
}
/// Runs `jobs` through the production worker, in order. Each job gets its own fence, selected to
/// the job's ticket, so one job's selection cannot change another job's admission.
#[allow(clippy::unwrap_used)] // Test-only harness; a failed send or join should fail the test.
fn run_jobs(fake: Fake, jobs: Vec<(u64, live::NativeJob)>) -> Vec<live::NativeReport> {
    let (tx, commands) = mpsc::sync_channel(32);
    let (reports, rx) = mpsc::sync_channel(32);
    let (agents, _agent_rx) = mpsc::sync_channel(32);
    let thread = std::thread::spawn(move || {
        integration::run(
            fake,
            commands,
            reports,
            agents,
            Arc::new(AtomicBool::new(false)),
            Arc::new(ports::CloseState::default()),
            Arc::new(|| 1),
        )
    });
    for (ticket, job) in jobs {
        let fence = Fence::default();
        fence.select(ticket);
        tx.send(ports::Command::Job { job, fence }).unwrap();
    }
    drop(tx); // The worker answers everything queued, then returns on disconnect.
    let mut sent = Vec::new();
    while let Ok(report) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
        sent.push(report);
    }
    thread.join().unwrap();
    sent
}
fn step_of(report: &live::NativeReport) -> &live::StepReport {
    match report {
        live::NativeReport::Step(step) => step,
        _ => panic!("step report expected"),
    }
}
fn maintenance_of(report: &live::NativeReport) -> &live::MaintenanceReport {
    match report {
        live::NativeReport::Maintenance(report) => report,
        _ => panic!("maintenance report expected"),
    }
}
fn planned_preview(report: &live::NativeReport) -> String {
    match maintenance_of(report) {
        live::MaintenanceReport::Planned { preview, .. } => preview.clone(),
        _ => panic!("planned maintenance report expected"),
    }
}
fn step_job(
    step: u16,
    operation: u64,
    stage: crosspane_installer_core::JobStage,
    consent: Option<live::Consent>,
) -> live::NativeJob {
    live::NativeJob::Step {
        job: crosspane_installer_core::JobIntent {
            step: crosspane_installer_core::StepId(step),
            operation: crosspane_installer_core::OperationId(operation),
            stage,
        },
        consent,
        status: None,
    }
}
/// The consent an Apply carries: the plan ticket it previews, on view revision 1.
fn consent_for(plan: u64, operation: u64) -> live::Consent {
    live::Consent {
        plan: crosspane_installer_core::OperationId(plan),
        operation: crosspane_installer_core::OperationId(operation),
        revision: 1,
    }
}
fn uninstall_plan(id: u64, choices: Vec<(u16, bool)>) -> live::NativeJob {
    live::NativeJob::Maintenance(live::MaintenanceRequest::PlanUninstall {
        id: live::MaintenanceId(id),
        choices,
        status: None,
    })
}
fn confirm_uninstall(id: u64) -> live::NativeJob {
    live::NativeJob::Maintenance(live::MaintenanceRequest::ConfirmUninstall {
        id: live::MaintenanceId(id),
        revision: 1,
        status: None,
    })
}
#[test]
fn elevated_install_preview_carries_the_core_block_and_the_same_plan_reaches_apply() {
    use crosspane_installer_core::JobStage;
    for cold in [Cold::Eligible, Cold::CompletedRemoval] {
        let plan = setup_plan();
        let mut fake = fresh(cold);
        fake.elevated_plans.push((
            ElevatedRequest::Setup,
            Ok(ElevatedPlanning::Planned(plan.clone())),
        ));
        let elevated = fake.elevated.clone();
        let reports = run_jobs(
            fake,
            vec![
                (1, step_job(20, 1, JobStage::Plan, None)),
                (2, step_job(20, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            ],
        );
        assert_eq!(
            reports.len(),
            3,
            "plan, then Progress, then the apply result"
        );
        let expected = format!(
            "{}\n\n{}\n{INSTALL_DECLINE_NOTE}",
            integration::install_preview(cold),
            plan.preview_block()
        );
        assert_eq!(
            step_of(&reports[0]).outcome,
            live::NativeOutcome::Planned { preview: expected }
        );
        let progress = step_of(&reports[1]);
        assert_eq!(progress.outcome, live::NativeOutcome::Progress);
        assert_eq!(progress.detail, integration::ELEVATED_WAITING);
        assert_eq!(
            step_of(&reports[2]).outcome,
            live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
        );
        let seen = locked(&elevated);
        assert_eq!(seen.requests, vec![ElevatedRequest::Setup]);
        assert_eq!(seen.applied, vec![(Operation::Install, Some(plan))]);
    }
}
#[test]
fn elevated_unavailable_install_previews_the_reason_and_gives_apply_none() {
    use crosspane_installer_core::JobStage;
    // A planning failure is the same Unavailable state, with the worker's own reason.
    let cases: [(Result<ElevatedPlanning, Failure>, &str); 2] = [
        (
            Ok(ElevatedPlanning::Unavailable(
                "Windows Defender Firewall is turned off.",
            )),
            "Windows Defender Firewall is turned off.",
        ),
        (Err(Failure::Unknown), ELEVATED_UNCHECKED),
    ];
    for (planning, reason) in cases {
        let mut fake = fresh(Cold::Eligible);
        fake.elevated_plans.push((ElevatedRequest::Setup, planning));
        let elevated = fake.elevated.clone();
        let reports = run_jobs(
            fake,
            vec![
                (1, step_job(20, 1, JobStage::Plan, None)),
                (2, step_job(20, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            ],
        );
        // Nothing is planned, so there is no Progress report and the decline note is not added.
        assert_eq!(reports.len(), 2);
        assert_eq!(
            step_of(&reports[0]).outcome,
            live::NativeOutcome::Planned {
                preview: format!(
                    "{}\n\n{reason}",
                    integration::install_preview(Cold::Eligible)
                ),
            }
        );
        assert_eq!(
            step_of(&reports[1]).outcome,
            live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
        );
        let seen = locked(&elevated);
        assert_eq!(seen.requests, vec![ElevatedRequest::Setup]);
        assert_eq!(seen.applied, vec![(Operation::Install, None)]);
    }
}
#[test]
fn elevated_is_never_requested_for_upgrade_or_a_non_eligible_install() {
    use crosspane_installer_core::JobStage;
    let plan_and_apply = || {
        vec![
            (1, step_job(20, 1, JobStage::Plan, None)),
            (2, step_job(20, 2, JobStage::Apply, Some(consent_for(1, 2)))),
        ]
    };
    // An existing, healthy install is an upgrade: no administrator step is planned.
    let upgrade = Fake::new();
    let elevated = upgrade.elevated.clone();
    let reports = run_jobs(upgrade, plan_and_apply());
    assert_eq!(
        reports.len(),
        2,
        "no Progress without a planned administrator step"
    );
    assert_eq!(
        step_of(&reports[0]).outcome,
        live::NativeOutcome::Planned {
            preview: "Verify the supplied release, settle completed artifacts, and transfer this update to an owned keeper".into(),
        }
    );
    assert_eq!(
        step_of(&reports[1]).outcome,
        live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
    );
    {
        let seen = locked(&elevated);
        assert!(seen.requests.is_empty());
        assert_eq!(seen.applied, vec![(Operation::Upgrade, None)]);
    }
    // Observation-only first install: admitted, but no administrator step is planned.
    let mut observe = Fake::new();
    observe.observed.cold = Cold::Observe;
    let elevated = observe.elevated.clone();
    let reports = run_jobs(observe, plan_and_apply());
    assert_eq!(reports.len(), 2);
    assert_eq!(
        step_of(&reports[0]).outcome,
        live::NativeOutcome::Planned {
            preview: integration::install_preview(Cold::Observe).to_owned(),
        }
    );
    assert_eq!(
        step_of(&reports[1]).outcome,
        live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
    );
    {
        let seen = locked(&elevated);
        assert!(seen.requests.is_empty());
        assert_eq!(seen.applied, vec![(Operation::Install, None)]);
    }
    // An Install operation on an existing installation is not eligible: nothing is planned and
    // Apply is not admitted, so no administrator step is ever requested.
    let mut existing = Fake::new();
    existing.observed.payload = State::Missing;
    let elevated = existing.elevated.clone();
    let reports = run_jobs(existing, plan_and_apply());
    assert_eq!(reports.len(), 2);
    assert_eq!(
        step_of(&reports[0]).outcome,
        live::NativeOutcome::Planned {
            preview: integration::install_preview(Cold::Existing).to_owned(),
        }
    );
    assert_eq!(
        step_of(&reports[1]).outcome,
        live::NativeOutcome::NotSubmitted
    );
    {
        let seen = locked(&elevated);
        assert!(seen.requests.is_empty());
        assert!(seen.applied.is_empty());
    }
    // Recovery and unknown admissions are planned without the administrator step.
    for cold in [
        Cold::Partial,
        Cold::Stale,
        Cold::Unknown,
        Cold::AccessDenied,
    ] {
        let mut fake = Fake::new();
        fake.observed.cold = cold;
        let elevated = fake.elevated.clone();
        let reports = run_jobs(fake, vec![(1, step_job(20, 1, JobStage::Plan, None))]);
        let expected = match cold {
            Cold::Partial => "Roll back / remove the partial first install".to_owned(),
            Cold::Stale => "Settle the stale first-install record".to_owned(),
            _ => integration::install_preview(cold).to_owned(),
        };
        assert_eq!(reports.len(), 1);
        assert_eq!(
            step_of(&reports[0]).outcome,
            live::NativeOutcome::Planned { preview: expected }
        );
        assert!(locked(&elevated).requests.is_empty());
    }
}
#[test]
fn admin_step_detects_plans_applies_and_verifies_on_the_elevated_routes() {
    use crosspane_installer_core::JobStage;
    let plan = setup_plan();
    let mut fake = Fake::new();
    fake.elevated_detection = Ok(ElevatedDetection {
        configured: false,
        detail: "The firewall rule and display driver are not set up.".to_owned(),
    });
    fake.elevated_plans.push((
        ElevatedRequest::Setup,
        Ok(ElevatedPlanning::Planned(plan.clone())),
    ));
    let elevated = fake.elevated.clone();
    let reports = run_jobs(
        fake,
        vec![
            (1, step_job(55, 1, JobStage::Detect, None)),
            (1, step_job(55, 1, JobStage::Plan, None)),
            (2, step_job(55, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            (2, step_job(55, 2, JobStage::Verify, None)),
        ],
    );
    assert_eq!(reports.len(), 5, "detect, plan, progress, apply, verify");
    let detected = step_of(&reports[0]);
    assert_eq!(
        detected.outcome,
        live::NativeOutcome::Detected { needs_action: true }
    );
    assert_eq!(
        detected.detail,
        "The firewall rule and display driver are not set up."
    );
    let planned = match &step_of(&reports[1]).outcome {
        live::NativeOutcome::Planned { preview } => preview.clone(),
        _ => panic!("planned outcome expected"),
    };
    assert_eq!(
        planned,
        format!(
            "Ask Windows once for administrator approval to add the firewall rule and the display driver\n\n{}",
            plan.preview_block()
        )
    );
    assert!(!planned.contains(INSTALL_DECLINE_NOTE));
    assert!(planned.starts_with("Ask Windows once"));
    assert!(planned.contains(PREVIEW_HEADER));
    let progress = step_of(&reports[2]);
    assert_eq!(progress.outcome, live::NativeOutcome::Progress);
    assert_eq!(progress.detail, integration::ELEVATED_WAITING);
    assert_eq!(
        step_of(&reports[3]).outcome,
        live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
    );
    assert!(matches!(
        step_of(&reports[4]).outcome,
        live::NativeOutcome::Verified { .. }
    ));
    assert!(
        reports
            .iter()
            .all(|report| step_of(report).outcome != live::NativeOutcome::Unsupported)
    );
    let seen = locked(&elevated);
    assert_eq!(seen.detects, 1);
    assert_eq!(seen.requests, vec![ElevatedRequest::Setup]);
    assert_eq!(seen.applied, vec![(Operation::Elevated, Some(plan))]);
    assert_eq!(seen.verified, vec![Operation::Elevated]);
}
#[test]
fn admin_detect_needs_action_only_when_the_step_is_not_configured() {
    use crosspane_installer_core::JobStage;
    for configured in [false, true] {
        let mut fake = Fake::new();
        fake.elevated_detection = Ok(ElevatedDetection {
            configured,
            detail: "Firewall rule checked.".to_owned(),
        });
        let reports = run_jobs(fake, vec![(1, step_job(55, 1, JobStage::Detect, None))]);
        assert_eq!(reports.len(), 1);
        let detected = step_of(&reports[0]);
        assert_eq!(
            detected.outcome,
            live::NativeOutcome::Detected {
                needs_action: !configured
            }
        );
        assert_eq!(detected.detail, "Firewall rule checked.");
    }
}
#[test]
fn elevated_report_lines_join_the_apply_detail_only_when_present() {
    use crosspane_installer_core::JobStage;
    let base = "Windows operation checked; deferred capabilities remain unavailable";
    for (lines, expected) in [
        (Vec::new(), base.to_owned()),
        (
            vec![
                "Windows Defender Firewall rule added.".to_owned(),
                "Display driver verified.".to_owned(),
            ],
            format!("{base} Windows Defender Firewall rule added. Display driver verified."),
        ),
    ] {
        let mut fake = Fake::new();
        fake.elevated_plans.push((
            ElevatedRequest::Setup,
            Ok(ElevatedPlanning::Planned(setup_plan())),
        ));
        fake.elevated_report = lines;
        let reports = run_jobs(
            fake,
            vec![
                (1, step_job(55, 1, JobStage::Plan, None)),
                (2, step_job(55, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            ],
        );
        let applied = step_of(reports.last().unwrap());
        assert_eq!(
            applied.outcome,
            live::NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
        );
        assert_eq!(applied.detail, expected);
    }
}
#[test]
fn removal_inspect_offers_the_teardown_choice_only_when_it_applies() {
    let reason = "Windows Defender is turned off.";
    let base_label = integration::TEARDOWN_LABEL;
    let cases: [(PlanningAnswer, TeardownChoice); 4] = [
        (
            Ok(ElevatedPlanning::Planned(teardown_plan())),
            Some((true, true, base_label.to_owned())),
        ),
        (
            Ok(ElevatedPlanning::Unavailable(reason)),
            Some((false, false, format!("{base_label} — {reason}"))),
        ),
        (Ok(ElevatedPlanning::NotNeeded), None),
        (
            Err(Failure::Unknown),
            Some((false, false, format!("{base_label} — {ELEVATED_UNCHECKED}"))),
        ),
    ];
    for (planning, expected) in cases {
        let mut fake = Fake::new();
        fake.elevated_plans
            .push((ElevatedRequest::Teardown, planning));
        let elevated = fake.elevated.clone();
        let reports = run_jobs(
            fake,
            vec![(
                1,
                live::NativeJob::Maintenance(live::MaintenanceRequest::Inspect {
                    id: live::MaintenanceId(1),
                }),
            )],
        );
        assert_eq!(reports.len(), 1);
        let choices = match maintenance_of(&reports[0]) {
            live::MaintenanceReport::Inspected { choices, .. } => choices.clone(),
            _ => panic!("inspected report expected"),
        };
        assert_eq!(choices[0].id, 1, "the identity choice stays first");
        let teardown = choices.iter().find(|choice| choice.id == 2);
        match expected {
            None => assert!(teardown.is_none(), "NotNeeded offers no teardown choice"),
            Some((checked, enabled, label)) => {
                let teardown = teardown.unwrap();
                assert_eq!(
                    (teardown.checked, teardown.enabled, teardown.role),
                    (checked, enabled, view::ToggleRole::Grant)
                );
                assert_eq!(teardown.label, label);
            }
        }
        assert_eq!(locked(&elevated).requests, vec![ElevatedRequest::Teardown]);
    }
}
#[test]
fn removal_plan_asks_for_the_teardown_only_when_the_choice_is_on() {
    let base = "Stop Crosspane cleanly and remove only owned installation files; preserve identity and user state";
    let cases: [UninstallCase; 3] = [
        // Choice 2 on and planned: the teardown block follows the removal preview.
        (
            Ok(ElevatedPlanning::Planned(teardown_plan())),
            vec![(1, false), (2, true)],
            format!("{base}\n\n{}", teardown_plan().preview_block()),
            vec![ElevatedRequest::Teardown],
        ),
        // Choice 2 on and unavailable: the reason follows the removal preview.
        (
            Ok(ElevatedPlanning::Unavailable(
                "Windows Defender is turned off.",
            )),
            vec![(1, false), (2, true)],
            format!("{base}\n\nWindows Defender is turned off."),
            vec![ElevatedRequest::Teardown],
        ),
        // Choice 2 off: the teardown is never planned.
        (
            Ok(ElevatedPlanning::Planned(teardown_plan())),
            vec![(1, false), (2, false)],
            base.to_owned(),
            Vec::new(),
        ),
    ];
    for (planning, choices, expected, requests) in cases {
        let mut fake = Fake::new();
        fake.elevated_plans
            .push((ElevatedRequest::Teardown, planning));
        let elevated = fake.elevated.clone();
        let reports = run_jobs(fake, vec![(1, uninstall_plan(1, choices))]);
        assert_eq!(reports.len(), 1);
        assert_eq!(planned_preview(&reports[0]), expected);
        assert_eq!(locked(&elevated).requests, requests);
    }
}
#[test]
fn teardown_not_submitted_refunds_keeps_the_report_lines_and_a_fresh_removal_still_applies() {
    let mut fake = Fake::new();
    fake.elevated_plans.push((
        ElevatedRequest::Teardown,
        Ok(ElevatedPlanning::Planned(teardown_plan())),
    ));
    fake.elevated_apply = Some(Err(Failure::NotSubmitted));
    fake.elevated_report = vec!["Windows did not start the firewall teardown.".to_owned()];
    let elevated = fake.elevated.clone();
    let reports = run_jobs(
        fake,
        vec![
            (1, uninstall_plan(1, vec![(1, false), (2, true)])),
            (1, confirm_uninstall(1)),
            (2, uninstall_plan(2, vec![(1, false), (2, true)])),
            (2, confirm_uninstall(2)),
        ],
    );
    assert_eq!(reports.len(), 4);
    assert!(matches!(
        maintenance_of(&reports[0]),
        live::MaintenanceReport::Planned { .. }
    ));
    // The refused teardown is refunded: NotSubmitted, with the worker's report lines after it.
    match maintenance_of(&reports[1]) {
        live::MaintenanceReport::Finished { id, outcome, lines } => {
            assert_eq!(*id, live::MaintenanceId(1));
            assert_eq!(*outcome, live::MaintenanceOutcome::Refused);
            assert_eq!(
                lines,
                &vec![
                    "Removal submission: NotSubmitted; completion is independently verified"
                        .to_owned(),
                    "Windows did not start the firewall teardown.".to_owned(),
                ]
            );
        }
        _ => panic!("finished report expected"),
    }
    assert!(matches!(
        maintenance_of(&reports[2]),
        live::MaintenanceReport::Planned { .. }
    ));
    // A fresh removal is planned and applied: nothing stayed dispatched or uncertain.
    match maintenance_of(&reports[3]) {
        live::MaintenanceReport::Finished { outcome, lines, .. } => {
            assert_eq!(*outcome, live::MaintenanceOutcome::Partial);
            assert_eq!(
                lines,
                &vec![
                    "Removal submission: Submitted; completion is independently verified"
                        .to_owned()
                ]
            );
        }
        _ => panic!("finished report expected"),
    }
    let removal = Operation::Removal {
        erase_identity: false,
    };
    let seen = locked(&elevated);
    assert_eq!(
        seen.applied,
        vec![
            (removal, Some(teardown_plan())),
            (removal, Some(teardown_plan())),
        ]
    );
    assert_eq!(
        seen.requests,
        vec![ElevatedRequest::Teardown, ElevatedRequest::Teardown]
    );
}
#[test]
fn elevated_report_survives_verify_until_the_next_detect_or_plan_clears_it() {
    use crosspane_installer_core::{JobStage, WaitKind};
    let base = "Windows operation checked; deferred capabilities remain unavailable";
    let lines = || {
        vec![
            "Windows Defender Firewall rule added.".to_owned(),
            "Display driver verified.".to_owned(),
        ]
    };
    let joined = format!("{base} Windows Defender Firewall rule added. Display driver verified.");
    // Apply reports the lines and Verify shows them; a Detect forgets them before the next Verify.
    let mut fake = Fake::new();
    fake.elevated_plans.push((
        ElevatedRequest::Setup,
        Ok(ElevatedPlanning::Planned(setup_plan())),
    ));
    fake.elevated_detection = Ok(ElevatedDetection {
        configured: true,
        detail: "Firewall rule checked.".to_owned(),
    });
    fake.elevated_report = lines();
    let reports = run_jobs(
        fake,
        vec![
            (1, step_job(55, 1, JobStage::Plan, None)),
            (2, step_job(55, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            (2, step_job(55, 2, JobStage::Verify, None)),
            (3, step_job(55, 3, JobStage::Detect, None)),
            (4, step_job(55, 4, JobStage::Verify, None)),
        ],
    );
    assert_eq!(
        reports.len(),
        6,
        "plan, progress, apply, verify, detect, verify"
    );
    let verified = step_of(&reports[3]);
    assert!(matches!(
        verified.outcome,
        live::NativeOutcome::Verified { .. }
    ));
    assert_eq!(verified.detail, joined);
    assert_eq!(
        step_of(&reports[4]).outcome,
        live::NativeOutcome::Detected {
            needs_action: false
        }
    );
    let reverified = step_of(&reports[5]);
    assert!(matches!(
        reverified.outcome,
        live::NativeOutcome::Verified { .. }
    ));
    assert_eq!(reverified.detail, base);
    // While Verify still waits, the lines stay on it; a fresh Plan forgets them.
    let mut fake = Fake::new();
    fake.elevated_plans.push((
        ElevatedRequest::Setup,
        Ok(ElevatedPlanning::Planned(setup_plan())),
    ));
    fake.elevated_report = lines();
    fake.verify_failure = Some(Failure::Unknown);
    let reports = run_jobs(
        fake,
        vec![
            (1, step_job(55, 1, JobStage::Plan, None)),
            (2, step_job(55, 2, JobStage::Apply, Some(consent_for(1, 2)))),
            (2, step_job(55, 2, JobStage::Verify, None)),
            (3, step_job(55, 3, JobStage::Plan, None)),
            (4, step_job(55, 4, JobStage::Verify, None)),
        ],
    );
    assert_eq!(
        reports.len(),
        6,
        "plan, progress, apply, verify, plan, verify"
    );
    let waiting = step_of(&reports[3]);
    assert_eq!(
        waiting.outcome,
        live::NativeOutcome::Waiting(WaitKind::Contract)
    );
    assert_eq!(waiting.detail, joined);
    assert!(matches!(
        step_of(&reports[4]).outcome,
        live::NativeOutcome::Planned { .. }
    ));
    let waiting_again = step_of(&reports[5]);
    assert_eq!(
        waiting_again.outcome,
        live::NativeOutcome::Waiting(WaitKind::Contract)
    );
    assert_eq!(waiting_again.detail, base);
}
#[test]
fn refused_teardown_refunds_even_when_settle_archived_metadata() {
    let mut fake = Fake::new();
    fake.elevated_plans.push((
        ElevatedRequest::Teardown,
        Ok(ElevatedPlanning::Planned(teardown_plan())),
    ));
    // The first settle returns true (an earlier terminal record was archived); the teardown gate
    // then declines before any other effect.
    fake.settlement = 1;
    fake.elevated_apply = Some(Err(Failure::NotSubmitted));
    fake.elevated_report = vec!["Windows did not start the firewall teardown.".to_owned()];
    let elevated = fake.elevated.clone();
    let reports = run_jobs(
        fake,
        vec![
            (1, uninstall_plan(1, vec![(1, false), (2, true)])),
            (1, confirm_uninstall(1)),
            (2, uninstall_plan(2, vec![(1, false), (2, true)])),
            (2, confirm_uninstall(2)),
        ],
    );
    assert_eq!(reports.len(), 4);
    // The declined teardown is refunded: Refused, with the worker's report lines after it.
    match maintenance_of(&reports[1]) {
        live::MaintenanceReport::Finished { id, outcome, lines } => {
            assert_eq!(*id, live::MaintenanceId(1));
            assert_eq!(*outcome, live::MaintenanceOutcome::Refused);
            assert_eq!(
                lines,
                &vec![
                    "Removal submission: NotSubmitted; completion is independently verified"
                        .to_owned(),
                    "Windows did not start the firewall teardown.".to_owned(),
                ]
            );
        }
        _ => panic!("finished report expected"),
    }
    // A fresh removal is planned and applied: nothing stayed dispatched or uncertain.
    assert!(matches!(
        maintenance_of(&reports[2]),
        live::MaintenanceReport::Planned { .. }
    ));
    match maintenance_of(&reports[3]) {
        live::MaintenanceReport::Finished { outcome, lines, .. } => {
            assert_eq!(*outcome, live::MaintenanceOutcome::Partial);
            assert_eq!(
                lines,
                &vec![
                    "Removal submission: Submitted; completion is independently verified"
                        .to_owned()
                ]
            );
        }
        _ => panic!("finished report expected"),
    }
    let removal = Operation::Removal {
        erase_identity: false,
    };
    let seen = locked(&elevated);
    assert_eq!(
        seen.applied,
        vec![
            (removal, Some(teardown_plan())),
            (removal, Some(teardown_plan())),
        ]
    );
    assert_eq!(
        seen.requests,
        vec![ElevatedRequest::Teardown, ElevatedRequest::Teardown]
    );
}
