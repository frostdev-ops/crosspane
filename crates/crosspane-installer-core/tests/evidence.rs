#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::*;
use crosspane_types::id::NodeId;
use std::collections::BTreeMap;

fn sample(time: u64, value: u64) -> CounterSample {
    CounterSample {
        source: ObservationSource::Live,
        observed_at_ms: time,
        binding: EvidenceBinding {
            local_node: NodeId([1; 32]),
            peer: Some(NodeId([2; 32])),
            link_generation: Some(1),
            epochs: Epochs {
                instance_id: 1,
                gate_epoch: 0,
                grants_epoch: 0,
                layout_epoch: 0,
                backends_epoch: 0,
            },
        },
        values: BTreeMap::from([(CounterId(1), value), (CounterId(2), 0)]),
    }
}
fn all() -> EpochDependencies {
    EpochDependencies {
        gate: true,
        grants: true,
        layout: true,
        backends: true,
    }
}
fn proof(start: &CounterSample, end: &CounterSample) -> Result<ActivityProof, EvidenceError> {
    check_activity(
        AttemptId(1),
        start,
        end,
        all(),
        &[CounterId(1)],
        &[CounterId(2)],
    )
}
#[test]
fn binding_epoch_and_time_table() {
    let start = sample(1, 0);
    let end = sample(2, 1);
    assert!(activity_is_current(&proof(&start, &end).unwrap(), &end));
    for field in 0..9 {
        let mut changed = end.clone();
        match field {
            0 => changed.binding.local_node = NodeId([3; 32]),
            1 => changed.binding.peer = Some(NodeId([3; 32])),
            2 => changed.binding.link_generation = Some(2),
            3 => changed.binding.epochs.instance_id = 2,
            4 => changed.binding.epochs.gate_epoch = 2,
            5 => changed.binding.epochs.grants_epoch = 2,
            6 => changed.binding.epochs.layout_epoch = 2,
            7 => changed.binding.epochs.backends_epoch = 2,
            _ => changed.observed_at_ms = 0,
        }
        assert!(proof(&start, &changed).is_err());
        assert!(!activity_is_current(
            &proof(&start, &end).unwrap(),
            &changed
        ));
    }
    let deps = EpochDependencies {
        layout: false,
        ..all()
    };
    let mut changed = end.clone();
    changed.binding.epochs.layout_epoch = 2;
    let p = check_activity(AttemptId(1), &start, &changed, deps, &[CounterId(1)], &[]).unwrap();
    assert!(activity_is_current(&p, &changed));
    changed.binding.epochs.gate_epoch = 2;
    assert!(!activity_is_current(&p, &changed));
    // Close/reopen is observable even if current boolean gate state looks unchanged.
    assert_eq!(
        proof(&start, &changed).unwrap_err(),
        EvidenceError::EpochChanged
    );
}
#[test]
fn counter_requirements_demo_missing_wrap_and_forbidden_advance() {
    let start = sample(1, 0);
    let end = sample(2, 1);
    for first in [true, false] {
        let mut a = start.clone();
        let mut b = end.clone();
        if first {
            a.source = ObservationSource::Demo;
        } else {
            b.source = ObservationSource::Demo;
        }
        assert_eq!(proof(&a, &b).unwrap_err(), EvidenceError::NotLive);
    }
    let mut missing = end.clone();
    missing.values.remove(&CounterId(1));
    assert_eq!(
        proof(&start, &missing).unwrap_err(),
        EvidenceError::MissingCounter
    );
    assert_eq!(
        proof(&start, &sample(2, 0)).unwrap_err(),
        EvidenceError::CounterDidNotAdvance
    );
    assert_eq!(
        proof(&sample(1, u64::MAX), &sample(2, 0)).unwrap_err(),
        EvidenceError::CounterRegressed
    );
    let mut failed = end.clone();
    failed.values.insert(CounterId(2), 1);
    assert_eq!(
        proof(&start, &failed).unwrap_err(),
        EvidenceError::ForbiddenCounterAdvanced
    );
    assert_eq!(
        check_activity(AttemptId(1), &start, &end, all(), &[], &[]).unwrap_err(),
        EvidenceError::EmptyRequirements
    );
    for forbidden in [vec![CounterId(1)], vec![CounterId(2), CounterId(2)]] {
        assert_eq!(
            check_activity(
                AttemptId(1),
                &start,
                &end,
                all(),
                &[CounterId(1)],
                &forbidden
            )
            .unwrap_err(),
            EvidenceError::InvalidRequirements
        );
    }
    assert_eq!(
        check_activity(
            AttemptId(1),
            &start,
            &end,
            all(),
            &[CounterId(1), CounterId(1)],
            &[]
        )
        .unwrap_err(),
        EvidenceError::InvalidRequirements
    );
}
fn graph() -> Vec<StepSpec> {
    (1..=3)
        .map(|id| StepSpec {
            id: StepId(id),
            prerequisites: vec![],
            required_for_installed: id == 1,
            required_for_ready: true,
            requires_fresh_observation: id == 2,
            requires_activity: id == 3,
            requires_human: id == 3,
            requires_fixture: id == 3,
        })
        .collect()
}
fn satisfy(
    flow: &mut Flow,
    id: u16,
    v: Verification,
    now: u64,
) -> Result<Vec<JobIntent>, FlowError> {
    let detect = flow
        .reduce(FlowEvent::Begin { step: StepId(id) }, now)?
        .remove(0);
    let verify = flow
        .reduce(
            FlowEvent::Detected {
                step: detect.step,
                operation: detect.operation,
                needs_action: false,
            },
            now,
        )?
        .remove(0);
    flow.reduce(
        FlowEvent::Verified {
            step: verify.step,
            operation: verify.operation,
            verification: v,
        },
        now,
    )
}
fn passive(time: u64, binding: Option<EvidenceBinding>) -> Verification {
    Verification {
        source: ObservationSource::Live,
        observed_at_ms: time,
        binding,
        activity: None,
        human_attempt: None,
        fixture_attempt: None,
    }
}
fn activity(start: &CounterSample, end: &CounterSample) -> Verification {
    Verification {
        source: ObservationSource::Live,
        observed_at_ms: end.observed_at_ms,
        binding: Some(end.binding.clone()),
        activity: Some(proof(start, end).unwrap()),
        human_attempt: Some(AttemptId(1)),
        fixture_attempt: Some(AttemptId(1)),
    }
}
#[test]
fn confirmations_and_exact_proof_end_binding_are_required() {
    let start = sample(1, 0);
    let end = sample(2, 1);
    for field in 0..5 {
        let mut flow = Flow::new(graph()).unwrap();
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![end.clone()],
            },
            2,
        )
        .unwrap();
        let mut v = activity(&start, &end);
        match field {
            0 => v.human_attempt = None,
            1 => v.fixture_attempt = Some(AttemptId(2)),
            2 => v.binding = None,
            3 => v.observed_at_ms = 1,
            _ => v.binding.as_mut().unwrap().epochs.layout_epoch = 2,
        }
        assert_eq!(satisfy(&mut flow, 3, v, 2), Err(FlowError::InvalidEvidence));
    }
}
#[test]
fn regression_above_old_proof_endpoint_cannot_revive() {
    let start = sample(1, 0);
    let end = sample(2, 1);
    let mut flow = Flow::new(graph()).unwrap();
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![end.clone()],
        },
        2,
    )
    .unwrap();
    satisfy(&mut flow, 1, passive(2, None), 2).unwrap();
    satisfy(&mut flow, 2, passive(2, Some(end.binding.clone())), 2).unwrap();
    satisfy(&mut flow, 3, activity(&start, &end), 2).unwrap();
    assert_eq!(flow.summary(2, &[end]).milestone, Milestone::WorkspaceReady);
    let high = sample(3, 10);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![high.clone()],
        },
        3,
    )
    .unwrap();
    assert_eq!(
        flow.summary(3, &[high]).milestone,
        Milestone::WorkspaceReady
    );
    let lower = sample(4, 5);
    assert_eq!(
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![lower]
            },
            4
        ),
        Err(FlowError::InvalidEvidence)
    );
    let recovered = sample(5, 11);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![recovered.clone()],
        },
        5,
    )
    .unwrap();
    assert_eq!(
        flow.summary(5, std::slice::from_ref(&recovered)).milestone,
        Milestone::InstalledWaiting
    );
    satisfy(&mut flow, 2, passive(5, Some(recovered.binding.clone())), 5).unwrap();
    assert_eq!(
        satisfy(&mut flow, 3, activity(&sample(4, 10), &recovered), 5),
        Err(FlowError::InvalidEvidence)
    );
    let mut new_start = sample(6, 0);
    new_start.binding.epochs.instance_id = 2;
    let mut new_end = new_start.clone();
    new_end.observed_at_ms = 7;
    new_end.values.insert(CounterId(1), 1);
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![new_end.clone()],
        },
        7,
    )
    .unwrap();
    satisfy(&mut flow, 2, passive(7, Some(new_end.binding.clone())), 7).unwrap();
    satisfy(&mut flow, 3, activity(&new_start, &new_end), 7).unwrap();
    assert_eq!(
        flow.summary(7, &[new_end]).milestone,
        Milestone::WorkspaceReady
    );
}
#[test]
fn absent_scope_retires_pass_and_preserves_regression_baseline() {
    let start = sample(1, 0);
    let end = sample(2, 10);
    let mut flow = Flow::new(graph()).unwrap();
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![end.clone()],
        },
        2,
    )
    .unwrap();
    satisfy(&mut flow, 3, activity(&start, &end), 2).unwrap();
    flow.reduce(FlowEvent::Observe { samples: vec![] }, 3)
        .unwrap();
    assert_eq!(flow.summary(3, &[]).steps[2].state, StepState::Stale);
    assert_eq!(
        flow.reduce(
            FlowEvent::Observe {
                samples: vec![sample(4, 5)]
            },
            4
        ),
        Err(FlowError::InvalidEvidence)
    );
}
