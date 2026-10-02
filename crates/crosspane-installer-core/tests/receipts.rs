#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::*;
use crosspane_types::id::NodeId;
use std::collections::BTreeMap;

#[test]
fn receipt_roundtrip_preserves_ownership_and_unknown_outcome() {
    let receipt = InstallReceipt {
        schema_version: 1,
        operation_id: OperationId(2),
        product_version: "0.0.0".into(),
        manifest_sha256: [1; 32],
        payload_sha256: [2; 32],
        unfinished: vec![StepId(7)],
        resources: [
            ResourceOwnership::Created,
            ResourceOwnership::Adopted,
            ResourceOwnership::Foreign,
        ]
        .into_iter()
        .map(|ownership| ResourceReceipt {
            resource_id: "fixture".into(),
            resolved_path: "/scratch/fixture".into(),
            ownership,
            before: ResourceObservation::Unknown,
            after: ResourceObservation::Matching,
            outcome: if ownership == ResourceOwnership::Created {
                MutationOutcome::Verified
            } else {
                MutationOutcome::Unknown
            },
        })
        .collect(),
    };
    let bytes = serde_json::to_vec(&receipt).unwrap();
    let restored: InstallReceipt = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(receipt, restored);
    let specs: Vec<_> = (1..=3)
        .map(|id| StepSpec {
            id: StepId(id),
            prerequisites: vec![],
            required_for_installed: id == 1,
            required_for_ready: id != 1,
            requires_fresh_observation: id == 2,
            requires_activity: id == 3,
            requires_human: id == 3,
            requires_fixture: id == 3,
        })
        .collect();
    let mut flow = Flow::new(specs).unwrap();
    assert!(
        restored
            .resources
            .iter()
            .any(|r| r.outcome == MutationOutcome::Verified)
    );
    assert_eq!(flow.summary(0, &[]).milestone, Milestone::NotInstalled);
    assert!(
        flow.summary(0, &[])
            .steps
            .iter()
            .all(|s| s.state == StepState::NotChecked)
    );
    let binding = EvidenceBinding {
        local_node: NodeId([1; 32]),
        peer: None,
        link_generation: None,
        epochs: Epochs {
            instance_id: 2,
            gate_epoch: 0,
            grants_epoch: 0,
            layout_epoch: 0,
            backends_epoch: 0,
        },
    };
    let start = CounterSample {
        source: ObservationSource::Live,
        observed_at_ms: 1,
        binding: binding.clone(),
        values: BTreeMap::from([(CounterId(1), 0)]),
    };
    let end = CounterSample {
        observed_at_ms: 2,
        values: BTreeMap::from([(CounterId(1), 1)]),
        ..start.clone()
    };
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![end.clone()],
        },
        2,
    )
    .unwrap();
    for id in [1, 2] {
        let detect = flow
            .reduce(FlowEvent::Begin { step: StepId(id) }, 2)
            .unwrap()
            .remove(0);
        let verify = flow
            .reduce(
                FlowEvent::Detected {
                    step: detect.step,
                    operation: detect.operation,
                    needs_action: false,
                },
                2,
            )
            .unwrap()
            .remove(0);
        flow.reduce(
            FlowEvent::Verified {
                step: verify.step,
                operation: verify.operation,
                verification: Verification {
                    source: ObservationSource::Live,
                    observed_at_ms: 2,
                    binding: if id == 2 { Some(binding.clone()) } else { None },
                    activity: None,
                    human_attempt: None,
                    fixture_attempt: None,
                },
            },
            2,
        )
        .unwrap();
    }
    assert_eq!(
        flow.summary(2, std::slice::from_ref(&end)).milestone,
        Milestone::InstalledWaiting
    );
    assert_eq!(
        flow.summary(2, std::slice::from_ref(&end)).steps[2].state,
        StepState::NotChecked
    );
    let detect = flow
        .reduce(FlowEvent::Begin { step: StepId(3) }, 2)
        .unwrap()
        .remove(0);
    let verify = flow
        .reduce(
            FlowEvent::Detected {
                step: detect.step,
                operation: detect.operation,
                needs_action: false,
            },
            2,
        )
        .unwrap()
        .remove(0);
    let proof = check_activity(
        AttemptId(1),
        &start,
        &end,
        EpochDependencies {
            gate: true,
            grants: true,
            layout: true,
            backends: true,
        },
        &[CounterId(1)],
        &[],
    )
    .unwrap();
    flow.reduce(
        FlowEvent::Verified {
            step: verify.step,
            operation: verify.operation,
            verification: Verification {
                source: ObservationSource::Live,
                observed_at_ms: 2,
                binding: Some(binding),
                activity: Some(proof),
                human_attempt: Some(AttemptId(1)),
                fixture_attempt: Some(AttemptId(1)),
            },
        },
        2,
    )
    .unwrap();
    assert_eq!(flow.summary(2, &[end]).milestone, Milestone::WorkspaceReady);
}
