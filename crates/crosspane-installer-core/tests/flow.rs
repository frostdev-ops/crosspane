#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::*;
use crosspane_types::id::NodeId;
use std::collections::BTreeMap;

fn step(id: u16) -> StepSpec {
    StepSpec {
        id: StepId(id),
        prerequisites: vec![],
        required_for_installed: false,
        required_for_ready: true,
        requires_fresh_observation: false,
        requires_activity: false,
        requires_human: false,
        requires_fixture: false,
    }
}
fn graph() -> Vec<StepSpec> {
    let mut files = step(1);
    files.required_for_installed = true;
    files.required_for_ready = false;
    let mut health = step(2);
    health.requires_fresh_observation = true;
    vec![files, health]
}
fn sample(time: u64, value: u64) -> CounterSample {
    CounterSample {
        source: ObservationSource::Live,
        observed_at_ms: time,
        binding: EvidenceBinding {
            local_node: NodeId([1; 32]),
            peer: None,
            link_generation: None,
            epochs: Epochs {
                instance_id: 1,
                gate_epoch: 0,
                grants_epoch: 0,
                layout_epoch: 0,
                backends_epoch: 0,
            },
        },
        values: BTreeMap::from([(CounterId(1), value)]),
    }
}
fn verification(time: u64, binding: Option<EvidenceBinding>) -> Verification {
    Verification {
        source: ObservationSource::Live,
        observed_at_ms: time,
        binding,
        activity: None,
        human_attempt: None,
        fixture_attempt: None,
    }
}
fn begin(flow: &mut Flow, id: u16, now: u64) -> JobIntent {
    flow.reduce(FlowEvent::Begin { step: StepId(id) }, now)
        .unwrap()
        .remove(0)
}
fn detect(flow: &mut Flow, job: JobIntent, action: bool, now: u64) -> JobIntent {
    flow.reduce(
        FlowEvent::Detected {
            step: job.step,
            operation: job.operation,
            needs_action: action,
        },
        now,
    )
    .unwrap()
    .remove(0)
}
fn satisfy(flow: &mut Flow, id: u16, now: u64, v: Verification) {
    let job = begin(flow, id, now);
    let job = detect(flow, job, false, now);
    flow.reduce(
        FlowEvent::Verified {
            step: job.step,
            operation: job.operation,
            verification: v,
        },
        now,
    )
    .unwrap();
}
fn state(flow: &Flow, id: u16, now: u64, current: &[CounterSample]) -> StepState {
    flow.summary(now, current)
        .steps
        .into_iter()
        .find(|s| s.id == StepId(id))
        .unwrap()
        .state
}

#[test]
fn graphs_reject_invalid_shapes_and_requirements() {
    assert_eq!(Flow::new(vec![]).unwrap_err(), GraphError::Empty);
    let mut specs = graph();
    specs.push(specs[0].clone());
    assert_eq!(Flow::new(specs).unwrap_err(), GraphError::DuplicateStep);
    let mut specs = graph();
    specs[0].prerequisites = vec![StepId(99)];
    assert_eq!(
        Flow::new(specs).unwrap_err(),
        GraphError::UnknownPrerequisite
    );
    let mut specs = graph();
    specs[0].prerequisites = vec![StepId(2)];
    specs[1].prerequisites = vec![StepId(1)];
    assert_eq!(Flow::new(specs).unwrap_err(), GraphError::Cycle);
    let mut specs = graph();
    specs[1].requires_fresh_observation = false;
    assert_eq!(
        Flow::new(specs).unwrap_err(),
        GraphError::NoReadinessFreshnessGate
    );
    for flag in 0..3 {
        let mut specs = graph();
        match flag {
            0 => specs[1].requires_activity = true,
            1 => specs[0].requires_human = true,
            _ => specs[0].requires_fixture = true,
        }
        assert_eq!(
            Flow::new(specs).unwrap_err(),
            GraphError::InvalidStepRequirement
        );
    }
}

#[test]
fn optional_skip_never_satisfies_evidence_and_still_requires_fresh_health() {
    let mut specs = graph();
    specs[1].prerequisites = vec![StepId(62)];
    let pair = step(60);
    let mut layout = step(62);
    layout.prerequisites = vec![StepId(60)];
    specs.extend([pair, layout]);
    let optional = [StepId(60), StepId(62)];
    assert_eq!(
        Flow::new_with_optional_steps(specs.clone(), &[StepId(1)]).unwrap_err(),
        GraphError::InvalidOptionalStep
    );
    assert_eq!(
        Flow::new_with_optional_steps(specs.clone(), &[StepId(2)]).unwrap_err(),
        GraphError::InvalidOptionalStep
    );
    let mut closed = Flow::new(specs.clone()).unwrap();
    assert_eq!(
        closed.reduce(FlowEvent::Skip(StepId(60)), 0),
        Err(FlowError::NotOptional)
    );
    let mut flow = Flow::new_with_optional_steps(specs, &optional).unwrap();
    satisfy(&mut flow, 1, 0, verification(0, None));
    let old = begin(&mut flow, 60, 0);
    for id in optional {
        flow.reduce(FlowEvent::Skip(id), 0).unwrap();
    }
    assert_eq!(
        flow.reduce(
            FlowEvent::Detected {
                step: old.step,
                operation: old.operation,
                needs_action: false
            },
            0
        ),
        Err(FlowError::WrongOperation)
    );
    assert_eq!(flow.summary(0, &[]).milestone, Milestone::InstalledWaiting);
    let live = sample(0, 1);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![live.clone()],
        },
        0,
    )
    .unwrap();
    satisfy(&mut flow, 2, 0, verification(0, Some(live.binding.clone())));
    let ready = flow.summary(0, std::slice::from_ref(&live));
    assert_eq!(ready.milestone, Milestone::WorkspaceReady);
    assert_eq!(ready.skipped, optional);
    assert!(
        ready
            .steps
            .iter()
            .filter(|s| optional.contains(&s.id))
            .all(|s| s.state == StepState::Skipped)
    );
    assert_eq!(
        flow.summary(5001, &[live]).milestone,
        Milestone::InstalledWaiting
    );
    // An explicit reopen retires readiness, but does not reopen the other deferrals.
    begin(&mut flow, 60, 1);
    assert_eq!(flow.summary(1, &[]).skipped, vec![StepId(62)]);
}
#[test]
fn detection_is_not_verification_and_install_is_not_ready() {
    let mut flow = Flow::new(graph()).unwrap();
    let current = sample(100, 0);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![current.clone()],
        },
        100,
    )
    .unwrap();
    assert_eq!(
        flow.summary(100, std::slice::from_ref(&current)).milestone,
        Milestone::NotInstalled
    );
    let job = begin(&mut flow, 1, 100);
    let job = detect(&mut flow, job, false, 100);
    assert_eq!(job.stage, JobStage::Verify);
    assert_eq!(
        flow.summary(100, std::slice::from_ref(&current)).milestone,
        Milestone::NotInstalled
    );
    flow.reduce(
        FlowEvent::Verified {
            step: job.step,
            operation: job.operation,
            verification: verification(100, None),
        },
        100,
    )
    .unwrap();
    assert_eq!(
        flow.summary(100, std::slice::from_ref(&current)).milestone,
        Milestone::InstalledWaiting
    );
    satisfy(
        &mut flow,
        2,
        100,
        verification(100, Some(current.binding.clone())),
    );
    assert_eq!(
        flow.summary(5100, std::slice::from_ref(&current)).milestone,
        Milestone::WorkspaceReady
    );
    assert_eq!(
        flow.summary(5101, std::slice::from_ref(&current)).milestone,
        Milestone::InstalledWaiting
    );
    assert_eq!(
        flow.summary(5101, &[current]).steps[1].state,
        StepState::Stale
    );
}
#[test]
fn explicit_apply_unknown_redetection_and_late_consent() {
    let mut flow = Flow::new(graph()).unwrap();
    let job = begin(&mut flow, 1, 1);
    let old = detect(&mut flow, job, true, 1);
    assert_eq!(
        flow.reduce(
            FlowEvent::ApplyRequested {
                step: old.step,
                operation: old.operation
            },
            1
        ),
        Err(FlowError::WrongStage)
    );
    flow.reduce(
        FlowEvent::Planned {
            step: old.step,
            operation: old.operation,
        },
        1,
    )
    .unwrap();
    flow.reduce(FlowEvent::Cancel { step: old.step }, 1)
        .unwrap();
    let job = begin(&mut flow, 1, 1);
    let new = detect(&mut flow, job, true, 1);
    flow.reduce(
        FlowEvent::Planned {
            step: new.step,
            operation: new.operation,
        },
        1,
    )
    .unwrap();
    assert_eq!(
        flow.reduce(
            FlowEvent::ApplyRequested {
                step: old.step,
                operation: old.operation
            },
            1
        ),
        Err(FlowError::WrongOperation)
    );
    let apply = flow
        .reduce(
            FlowEvent::ApplyRequested {
                step: new.step,
                operation: new.operation,
            },
            1,
        )
        .unwrap()
        .remove(0);
    assert_eq!(apply.stage, JobStage::Apply);
    let detect = flow
        .reduce(
            FlowEvent::Applied {
                step: apply.step,
                operation: apply.operation,
                outcome: ApplyOutcome::Unknown,
            },
            1,
        )
        .unwrap()
        .remove(0);
    assert_eq!(detect.stage, JobStage::Detect);
    assert_eq!(
        flow.reduce(
            FlowEvent::Applied {
                step: apply.step,
                operation: apply.operation,
                outcome: ApplyOutcome::Applied
            },
            1
        ),
        Err(FlowError::WrongOperation)
    );
    flow.reduce(FlowEvent::Invalidate { step: detect.step }, 1)
        .unwrap();
    assert_eq!(
        flow.reduce(
            FlowEvent::Detected {
                step: detect.step,
                operation: detect.operation,
                needs_action: false
            },
            1
        ),
        Err(FlowError::WrongOperation)
    );
}
#[test]
fn optional_expired_ancestor_retires_descendant_jobs_and_summary() {
    let mut specs = graph();
    specs[1].required_for_installed = false;
    let mut optional = step(3);
    optional.required_for_ready = false;
    optional.prerequisites = vec![StepId(2)];
    let mut child = step(4);
    child.prerequisites = vec![StepId(3)];
    specs.extend([optional, child]);
    let mut flow = Flow::new(specs).unwrap();
    let current = sample(0, 0);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![current.clone()],
        },
        0,
    )
    .unwrap();
    assert_eq!(
        flow.reduce(FlowEvent::Begin { step: StepId(4) }, 0),
        Err(FlowError::PrerequisitePending)
    );
    satisfy(&mut flow, 1, 0, verification(0, None));
    satisfy(
        &mut flow,
        2,
        0,
        verification(0, Some(current.binding.clone())),
    );
    satisfy(&mut flow, 3, 0, verification(0, None));
    satisfy(&mut flow, 4, 0, verification(0, None));
    assert_eq!(
        flow.summary(0, std::slice::from_ref(&current)).milestone,
        Milestone::WorkspaceReady
    );
    assert_eq!(
        state(&flow, 4, 5001, std::slice::from_ref(&current)),
        StepState::Stale
    );
    let mut refreshed = current.clone();
    refreshed.observed_at_ms = 5001;
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![refreshed],
        },
        5001,
    )
    .unwrap();
    assert_eq!(
        flow.reduce(FlowEvent::Begin { step: StepId(4) }, 5001),
        Err(FlowError::PrerequisitePending)
    );
    // Explicit invalidation also retires a still-running dependent operation.
    satisfy(
        &mut flow,
        2,
        5001,
        verification(5001, Some(current.binding)),
    );
    satisfy(&mut flow, 3, 5001, verification(5001, None));
    let child = begin(&mut flow, 4, 5001);
    flow.reduce(FlowEvent::Invalidate { step: StepId(3) }, 5001)
        .unwrap();
    assert_eq!(
        flow.reduce(
            FlowEvent::Detected {
                step: child.step,
                operation: child.operation,
                needs_action: false
            },
            5001
        ),
        Err(FlowError::WrongOperation)
    );
}
#[test]
fn waiting_refusal_failure_and_unsupported_are_not_green() {
    for outcome in [ApplyOutcome::Refused, ApplyOutcome::Failed] {
        let mut flow = Flow::new(graph()).unwrap();
        let job = begin(&mut flow, 1, 1);
        let plan = detect(&mut flow, job, true, 1);
        flow.reduce(
            FlowEvent::Planned {
                step: plan.step,
                operation: plan.operation,
            },
            1,
        )
        .unwrap();
        let apply = flow
            .reduce(
                FlowEvent::ApplyRequested {
                    step: plan.step,
                    operation: plan.operation,
                },
                1,
            )
            .unwrap()
            .remove(0);
        assert!(
            flow.reduce(
                FlowEvent::Applied {
                    step: apply.step,
                    operation: apply.operation,
                    outcome
                },
                1
            )
            .unwrap()
            .is_empty()
        );
        assert_ne!(state(&flow, 1, 1, &[]), StepState::Satisfied);
    }
    for kind in [WaitKind::User, WaitKind::Peer, WaitKind::Contract] {
        let mut flow = Flow::new(graph()).unwrap();
        let job = begin(&mut flow, 1, 1);
        flow.reduce(
            FlowEvent::Waiting {
                step: job.step,
                operation: job.operation,
                kind,
            },
            1,
        )
        .unwrap();
        assert_eq!(flow.summary(1, &[]).milestone, Milestone::NotInstalled);
    }
    let mut flow = Flow::new(graph()).unwrap();
    let job = begin(&mut flow, 1, 1);
    flow.reduce(
        FlowEvent::Unsupported {
            step: job.step,
            operation: job.operation,
        },
        1,
    )
    .unwrap();
    assert_eq!(state(&flow, 1, 1, &[]), StepState::Unsupported);
}
#[test]
fn no_demo_future_or_unadmitted_evidence() {
    let mut flow = Flow::new(graph()).unwrap();
    let job = begin(&mut flow, 1, 1);
    let verify = detect(&mut flow, job, false, 1);
    let mut v = verification(1, None);
    v.source = ObservationSource::Demo;
    assert_eq!(
        flow.reduce(
            FlowEvent::Verified {
                step: verify.step,
                operation: verify.operation,
                verification: v
            },
            1
        ),
        Err(FlowError::InvalidEvidence)
    );
    let job = begin(&mut flow, 1, 1);
    let verify = detect(&mut flow, job, false, 1);
    assert_eq!(
        flow.reduce(
            FlowEvent::Verified {
                step: verify.step,
                operation: verify.operation,
                verification: verification(2, None)
            },
            1
        ),
        Err(FlowError::InvalidEvidence)
    );
    let current = sample(1, 0);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![current.clone()],
        },
        1,
    )
    .unwrap();
    satisfy(&mut flow, 1, 1, verification(1, None));
    satisfy(
        &mut flow,
        2,
        1,
        verification(1, Some(current.binding.clone())),
    );
    let mut unadmitted = current;
    unadmitted.observed_at_ms = 2;
    assert_ne!(
        flow.summary(2, &[unadmitted]).milestone,
        Milestone::WorkspaceReady
    );
}

fn peer_sample(peer: u8, instance: u64, time: u64, value: u64) -> CounterSample {
    let mut s = sample(time, value);
    s.binding.peer = Some(NodeId([peer; 32]));
    s.binding.link_generation = Some(1);
    s.binding.epochs.instance_id = instance;
    s
}
fn interactive_graph() -> Vec<StepSpec> {
    let mut specs = graph();
    let mut practice = step(3);
    practice.requires_activity = true;
    practice.requires_human = true;
    practice.requires_fixture = true;
    specs.push(practice);
    specs
}
fn activity_verification(start: &CounterSample, end: &CounterSample) -> Verification {
    Verification {
        source: ObservationSource::Live,
        observed_at_ms: end.observed_at_ms,
        binding: Some(end.binding.clone()),
        activity: Some(
            check_activity(
                AttemptId(7),
                start,
                end,
                EpochDependencies {
                    gate: true,
                    grants: true,
                    layout: true,
                    backends: true,
                },
                &[CounterId(1)],
                &[],
            )
            .unwrap(),
        ),
        human_attempt: Some(AttemptId(7)),
        fixture_attempt: Some(AttemptId(7)),
    }
}
fn verify_result(
    flow: &mut Flow,
    id: u16,
    now: u64,
    v: Verification,
) -> Result<Vec<JobIntent>, FlowError> {
    let detect = begin(flow, id, now);
    let verify = detect_job(flow, detect, now);
    flow.reduce(
        FlowEvent::Verified {
            step: verify.step,
            operation: verify.operation,
            verification: v,
        },
        now,
    )
}
fn detect_job(flow: &mut Flow, job: JobIntent, now: u64) -> JobIntent {
    detect(flow, job, false, now)
}

#[test]
fn rejected_other_instance_cannot_erase_quarantine() {
    let mut flow = Flow::new(interactive_graph()).unwrap();
    let b = peer_sample(3, 2, 1, 10);
    flow.reduce(FlowEvent::Observe { samples: vec![b] }, 1)
        .unwrap();
    let start = peer_sample(2, 1, 1, 0);
    let a = peer_sample(2, 1, 2, 10);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![a.clone()],
        },
        2,
    )
    .unwrap();
    satisfy(&mut flow, 1, 2, verification(2, None));
    satisfy(&mut flow, 2, 2, verification(2, Some(a.binding.clone())));
    let old_proof = activity_verification(&start, &a);
    satisfy(&mut flow, 3, 2, old_proof.clone());
    assert_eq!(flow.summary(2, &[a]).milestone, Milestone::WorkspaceReady);
    assert_eq!(
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![peer_sample(2, 1, 3, 9)]
            },
            3
        ),
        Err(FlowError::InvalidEvidence)
    );
    assert_eq!(
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![peer_sample(3, 2, 4, 9)]
            },
            4
        ),
        Err(FlowError::InvalidEvidence)
    );
    let recovered = peer_sample(2, 1, 5, 11);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![recovered.clone()],
        },
        5,
    )
    .unwrap();
    satisfy(
        &mut flow,
        2,
        5,
        verification(5, Some(recovered.binding.clone())),
    );
    assert_eq!(
        verify_result(&mut flow, 3, 5, old_proof),
        Err(FlowError::InvalidEvidence)
    );
    assert_eq!(
        flow.summary(5, &[recovered]).milestone,
        Milestone::InstalledWaiting
    );
}
#[test]
fn peer_switch_cannot_admit_delayed_time_or_known_old_epochs() {
    for delayed_time in [15, 21] {
        let mut flow = Flow::new(interactive_graph()).unwrap();
        let p = peer_sample(2, 1, 10, 0);
        flow.reduce(FlowEvent::Observe { samples: vec![p] }, 10)
            .unwrap();
        let mut q = peer_sample(3, 1, 20, 0);
        q.binding.epochs.gate_epoch = 1;
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![q.clone()],
            },
            20,
        )
        .unwrap();
        let delayed = peer_sample(2, 1, delayed_time, 1);
        assert_eq!(
            flow.reduce(
                FlowEvent::Observe {
                    samples: vec![delayed]
                },
                21
            ),
            Err(FlowError::InvalidEvidence)
        );
        assert_ne!(flow.summary(21, &[q]).milestone, Milestone::WorkspaceReady);
    }
}
#[test]
fn inconsistent_batch_keeps_each_instance_taint_in_both_orders() {
    for reverse in [false, true] {
        let mut flow = Flow::new(interactive_graph()).unwrap();
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![peer_sample(2, 1, 1, 10)],
            },
            1,
        )
        .unwrap();
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![peer_sample(3, 2, 2, 10)],
            },
            2,
        )
        .unwrap();
        let mut bad = vec![peer_sample(2, 1, 3, 5), peer_sample(3, 2, 3, 5)];
        if reverse {
            bad.reverse();
        }
        assert_eq!(
            flow.reduce(FlowEvent::Observe { samples: bad }, 3),
            Err(FlowError::InvalidEvidence)
        );
        let start = peer_sample(3, 2, 4, 11);
        let end = peer_sample(3, 2, 5, 12);
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![start.clone()],
            },
            4,
        )
        .unwrap();
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![end.clone()],
            },
            5,
        )
        .unwrap();
        assert_eq!(
            verify_result(&mut flow, 3, 5, activity_verification(&start, &end)),
            Err(FlowError::InvalidEvidence)
        );
        // A retired instance cannot re-enter through its retained peer baseline either.
        assert_eq!(
            flow.reduce(
                FlowEvent::Observe {
                    samples: vec![peer_sample(2, 1, 6, 11)]
                },
                6
            ),
            Err(FlowError::InvalidEvidence)
        );
        assert_ne!(flow.summary(6, &[end]).milestone, Milestone::WorkspaceReady);
    }
}

#[test]
fn backward_caller_time_cannot_skip_regression_quarantine() {
    let mut flow = Flow::new(interactive_graph()).unwrap();
    let start = peer_sample(2, 1, 9, 0);
    let end = peer_sample(2, 1, 10, 10);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![end.clone()],
        },
        10,
    )
    .unwrap();
    satisfy(&mut flow, 1, 10, verification(10, None));
    satisfy(
        &mut flow,
        2,
        10,
        verification(10, Some(end.binding.clone())),
    );
    let old_proof = activity_verification(&start, &end);
    satisfy(&mut flow, 3, 10, old_proof.clone());
    assert_eq!(
        flow.summary(10, &[end]).milestone,
        Milestone::WorkspaceReady
    );
    assert_eq!(
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![peer_sample(2, 1, 9, 9)]
            },
            9
        ),
        Err(FlowError::InvalidEvidence)
    );
    let recovered = peer_sample(2, 1, 11, 11);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![recovered.clone()],
        },
        11,
    )
    .unwrap();
    satisfy(&mut flow, 1, 11, verification(11, None));
    satisfy(
        &mut flow,
        2,
        11,
        verification(11, Some(recovered.binding.clone())),
    );
    assert_eq!(
        verify_result(&mut flow, 3, 11, old_proof),
        Err(FlowError::InvalidEvidence)
    );
    assert_eq!(
        flow.summary(11, &[recovered]).milestone,
        Milestone::InstalledWaiting
    );
}
