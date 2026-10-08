//! Exact pure integration cases; no Windows backend, task, agent, file or GUI is opened.
use crosspane_installer::{agent_contract, gui, live, view};
#[path = "../src/platform/windows/integration.rs"]
#[allow(dead_code)]
mod integration;
use crosspane_installer_core::ObservationSource;
use domains::*;
use integration::{domains, ports, worker};
use std::sync::{
    Arc,
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
        if let Some(failure) = self.verify_failure {
            return Err(failure);
        }
        Ok(self.observed.verified(op))
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
