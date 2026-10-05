#![allow(clippy::unwrap_used)]
use crosspane_installer::{agent_contract::*, settings_transition::*};
use crosspane_installer_core::*;
use crosspane_types::id::NodeId;
use serde_json::{Value, json};
const STATUS: &[u8] = br#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":[]},"instance":{"id":99,"pid":1,"uid":1000,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":1},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store","permissions":[],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,
"peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"sensitive peer","connected":true,"link_generation":2,
"features":[],"grants_given":["browse","input","present","share","speaker"],"last_source_parking":null,
"counters":{"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,"e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,
"e1_chord_releases":0,"e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,"e2_dest_returned":0,
"e2_frames_presented":0,"e2_returns_failed":0}}]}}}"#;
struct Attempt {
    step: StepId,
}
struct Harness {
    core: Flow,
    value: Value,
    now: u64,
    platform: AgentPlatform,
    attempt: Attempt,
}
impl Harness {
    fn new() -> Self {
        let step = StepId(1);
        Self {
            core: Flow::new(vec![StepSpec {
                id: step,
                prerequisites: vec![],
                required_for_installed: false,
                required_for_ready: true,
                requires_fresh_observation: true,
                requires_activity: false,
                requires_human: false,
                requires_fixture: false,
            }])
            .unwrap(),
            value: value(),
            now: 1,
            platform: AgentPlatform::Linux,
            attempt: Attempt { step },
        }
    }
}
fn local() -> NodeId {
    NodeId([0x11; 32])
}

fn peer() -> NodeId {
    NodeId([0x22; 32])
}

fn value() -> Value {
    serde_json::from_slice(STATUS).unwrap()
}

fn health(v: &Value, platform: AgentPlatform) -> Box<HealthSnapshot> {
    match parse_status(&serde_json::to_vec(v).unwrap(), platform).unwrap() {
        StatusAdmission::Supported(h) => h,
        p => panic!("unexpected admission {p:?}"),
    }
}

fn settings_reply(id: u64, result: Result<DecodedReply, CallFailure>, at: u64) -> AgentReply {
    AgentReply {
        id,
        observed_at_ms: at,
        source: ObservationSource::Live,
        result,
    }
}

#[test]
fn settings_returned_revision_requires_restart_consent_and_new_opaque_instance() {
    let v = value();
    let h = health(&v, AgentPlatform::Linux);
    let mut s = SettingsTransition::new(local(), 17);
    s.track_peers(&[peer()]).unwrap();
    let detected = s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
    let FlowEvent::Observe { samples } = &detected[0] else {
        panic!()
    };
    assert_eq!(samples.len(), 2);
    assert!(samples.iter().all(|s| s.observed_at_ms == 1));
    let update = s.consent_update(100, 17, true).unwrap();
    assert_eq!(
        update.request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "1111111111111111".into(),
            mac_virtual_display: true
        }
    );
    assert_eq!(
        s.reply(
            settings_reply(
                100,
                Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                    revision: "2222222222222222".into(),
                    restart_required: true
                })),
                2
            ),
            2
        ),
        Ok(SettingsOutcome {
            observations: Vec::new(),
            detect_after_unknown: false
        })
    );
    assert_eq!(s.returned_revision(), Some("2222222222222222"));
    assert_eq!(s.state(), &SettingsTransitionState::NeedsRestartConsent);
    let samples = samples.clone();
    let (events, restart) = s
        .consent_restart(101, 17, &[StepId(1), StepId(2)], &samples)
        .unwrap();
    assert_eq!(restart.request, InstallerRequest::Restart);
    assert_eq!(
        events,
        vec![
            FlowEvent::Observe { samples },
            FlowEvent::Invalidate { step: StepId(1) },
            FlowEvent::Invalidate { step: StepId(2) }
        ]
    );
    s.reply(settings_reply(101, Ok(DecodedReply::Acknowledged), 3), 3)
        .unwrap();
    let mut old = v.clone();
    old["result"]["installer"]["config_revision"] = json!("2222222222222222");
    for (id, instance, revision) in [
        (102, 99, "2222222222222222"),
        (103, 100, "1111111111111111"),
        (104, u64::MAX, "2222222222222222"),
    ] {
        old["result"]["installer"]["instance"]["id"] = json!(instance);
        old["result"]["installer"]["config_revision"] = json!(revision);
        s.poll_new_instance(id).unwrap();
        let outcome = s
            .reply(
                settings_reply(
                    id,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &old,
                        AgentPlatform::Linux,
                    )))),
                    id,
                ),
                id,
            )
            .unwrap();
        let FlowEvent::Observe { samples } = &outcome.observations[0] else {
            panic!()
        };
        assert_eq!(samples.len(), 2);
        assert!(
            samples
                .iter()
                .all(|s| s.observed_at_ms == id && s.binding.epochs.instance_id == instance)
        );
        assert_eq!(
            s.state(),
            if id == 104 {
                &SettingsTransitionState::Complete
            } else {
                &SettingsTransitionState::WaitingNewInstance
            }
        );
    }
}

#[test]
fn settings_conflict_and_unknown_mutation_require_detection_and_renewed_view_consent() {
    for error in [
        CallFailure::Refused(AgentRefusal::RevisionConflict),
        CallFailure::TimeoutOutcomeUnknown,
    ] {
        let h = health(&value(), AgentPlatform::Linux);
        let mut s = SettingsTransition::new(local(), 17);
        s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
        s.consent_update(1, 17, true).unwrap();
        assert_eq!(
            s.reply(settings_reply(1, Err(error.clone()), 2), 2),
            Ok(SettingsOutcome {
                observations: Vec::new(),
                detect_after_unknown: true
            })
        );
        assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
        assert!(s.consent_update(2, 17, true).is_err());
        assert!(s.detected(&h, ObservationSource::Live, 3, 3, 17).is_err());
        s.detected(&h, ObservationSource::Live, 3, 3, 18).unwrap();
        assert!(s.consent_update(2, 17, true).is_err());
        if error == CallFailure::Refused(AgentRefusal::RevisionConflict) {
            assert_eq!(
                s.state(),
                &SettingsTransitionState::NeedsRecoveryRestartConsent
            );
            assert!(s.consent_update(2, 18, true).is_err());
            continue;
        }
        assert_eq!(
            s.consent_update(2, 18, true).unwrap().request,
            InstallerRequest::SettingsUpdate {
                expected_revision: "1111111111111111".into(),
                mac_virtual_display: true
            }
        );
    }
}

#[test]
fn settings_refusal_invalid_response_stale_reply_and_demo_cannot_complete() {
    for result in [
        Err(CallFailure::Refused(AgentRefusal::NotSupported)),
        Ok(DecodedReply::Acknowledged),
        Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
            revision: "2222222222222222".into(),
            restart_required: false,
        })),
    ] {
        let mut s = SettingsTransition::new(local(), 17);
        s.detected(
            &health(&value(), AgentPlatform::Linux),
            ObservationSource::Live,
            1,
            1,
            17,
        )
        .unwrap();
        s.consent_update(1, 17, true).unwrap();
        let _ = s.reply(settings_reply(1, result, 2), 2);
        assert!(matches!(s.state(), SettingsTransitionState::Failed(_)));
    }
    let mut s = SettingsTransition::new(local(), 17);
    let h = health(&value(), AgentPlatform::Linux);
    assert!(s.detected(&h, ObservationSource::Demo, 1, 1, 17).is_err());
    s.detected(&h, ObservationSource::Live, 1, 1, 17).unwrap();
    s.consent_update(1, 17, true).unwrap();
    let mut demo = settings_reply(1, Ok(DecodedReply::Acknowledged), 2);
    demo.source = ObservationSource::Demo;
    assert!(s.reply(demo, 2).is_err());
    assert!(
        s.reply(settings_reply(1, Ok(DecodedReply::Acknowledged), 100), 2)
            .is_err()
    );
}

#[test]
fn settings_plan_replacement_retires_old_view_and_requires_new_consent() {
    let mut h = Harness::new();
    let mut s = settings_ready(&mut h);
    let mut v = h.value.clone();
    v["result"]["installer"]["instance"]["id"] = json!(100);
    v["result"]["installer"]["config_revision"] = json!("2222222222222222");
    h.now += 1;
    let observed = s
        .detected(
            &health(&v, AgentPlatform::Linux),
            ObservationSource::Live,
            h.now,
            h.now,
            17,
        )
        .unwrap();
    reduce_settings(&mut h.core, &observed, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert_eq!(s.last_error(), Some(&CallFailure::InvalidResponse));
    assert_eq!(
        h.core
            .summary(h.now, s.current_samples())
            .steps
            .iter()
            .find(|step| step.id == h.attempt.step)
            .unwrap()
            .state,
        StepState::Stale
    );
    assert!(s.consent_update(1, 17, true).is_err());
    h.now += 1;
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        h.now,
        h.now,
        18,
    )
    .unwrap();
    assert!(s.consent_update(1, 17, true).is_err());
    assert_eq!(
        s.consent_update(1, 18, true).unwrap().request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "2222222222222222".into(),
            mac_virtual_display: true
        }
    );
}

fn reduce_settings(core: &mut Flow, events: &[FlowEvent], now: u64) {
    assert!(events.is_empty() || matches!(events[0], FlowEvent::Observe { .. }));
    for event in events {
        core.reduce(event.clone(), now).unwrap();
    }
}

fn settings_ready(h: &mut Harness) -> SettingsTransition {
    let mut s = SettingsTransition::new(local(), 17);
    s.track_peers(&[peer()]).unwrap();
    h.now += 1;
    let events = s
        .detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            17,
        )
        .unwrap();
    reduce_settings(&mut h.core, &events, h.now);
    let step = h.attempt.step;
    let job = h
        .core
        .reduce(FlowEvent::Begin { step }, h.now)
        .unwrap()
        .remove(0);
    let verify = h
        .core
        .reduce(
            FlowEvent::Detected {
                step,
                operation: job.operation,
                needs_action: false,
            },
            h.now,
        )
        .unwrap()
        .remove(0);
    h.core
        .reduce(
            FlowEvent::Verified {
                step,
                operation: verify.operation,
                verification: Verification {
                    source: ObservationSource::Live,
                    observed_at_ms: h.now,
                    binding: Some(s.current_samples()[0].binding.clone()),
                    activity: None,
                    human_attempt: None,
                    fixture_attempt: None,
                },
            },
            h.now,
        )
        .unwrap();
    s
}

fn settings_updated(s: &mut SettingsTransition, h: &mut Harness) {
    s.consent_update(100, s.view_revision(), true).unwrap();
    h.now += 1;
    let o = s
        .reply(
            settings_reply(
                100,
                Ok(DecodedReply::SettingsUpdated(SettingsUpdated {
                    revision: "2222222222222222".into(),
                    restart_required: true,
                })),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
}

fn restart_settings(s: &mut SettingsTransition, h: &mut Harness, id: u64) {
    let samples = s.current_samples().to_vec();
    let (events, call) = s
        .consent_restart(id, s.view_revision(), &[h.attempt.step], &samples)
        .unwrap();
    let FlowEvent::Observe { samples } = &events[0] else {
        panic!()
    };
    assert_eq!(samples.len(), 2);
    assert_eq!(call.request, InstallerRequest::Restart);
    reduce_settings(&mut h.core, &events, h.now);
    assert_ne!(
        h.core
            .summary(h.now, samples)
            .steps
            .iter()
            .find(|step| step.id == h.attempt.step)
            .unwrap()
            .state,
        StepState::Satisfied
    );
}

#[test]
fn settings_conflict_recovery_restart_loads_disk_before_renewed_update_consent() {
    let mut h = Harness::new();
    let mut s = settings_ready(&mut h);
    s.consent_update(100, 17, true).unwrap();
    h.now += 1;
    let outcome = s
        .reply(
            settings_reply(
                100,
                Err(CallFailure::Refused(AgentRefusal::RevisionConflict)),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    assert!(outcome.detect_after_unknown);
    reduce_settings(&mut h.core, &outcome.observations, h.now);
    h.now += 1;
    let detected = s
        .detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            18,
        )
        .unwrap();
    reduce_settings(&mut h.core, &detected, h.now);
    let FlowEvent::Observe { samples } = &detected[0] else {
        panic!()
    };
    assert_eq!(
        s.state(),
        &SettingsTransitionState::NeedsRecoveryRestartConsent
    );
    assert!(s.consent_update(101, 18, true).is_err());
    let (events, call) = s
        .consent_restart(101, 18, &[h.attempt.step], samples)
        .unwrap();
    reduce_settings(&mut h.core, &events, h.now);
    assert_eq!(call.request, InstallerRequest::Restart);
    h.now += 1;
    s.reply(
        settings_reply(101, Ok(DecodedReply::Acknowledged), h.now),
        h.now,
    )
    .unwrap();
    assert_ne!(s.state(), &SettingsTransitionState::Complete);
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("3333333333333333");
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert!(s.consent_update(103, 18, true).is_err());
    let view = s.view_revision();
    h.now += 1;
    reduce_settings(
        &mut h.core,
        &s.detected(
            &health(&h.value, h.platform),
            ObservationSource::Live,
            h.now,
            h.now,
            view,
        )
        .unwrap(),
        h.now,
    );
    assert_eq!(
        s.consent_update(103, view, true).unwrap().request,
        InstallerRequest::SettingsUpdate {
            expected_revision: "3333333333333333".into(),
            mac_virtual_display: true
        }
    );
}

#[test]
fn restart_timeout_refusal_and_transient_poll_error_keep_revision_and_require_detection() {
    for error in [
        CallFailure::TimeoutOutcomeUnknown,
        CallFailure::Refused(AgentRefusal::NotSupported),
    ] {
        let mut h = Harness::new();
        let mut s = settings_ready(&mut h);
        settings_updated(&mut s, &mut h);
        restart_settings(&mut s, &mut h, 101);
        h.now += 1;
        let o = s
            .reply(settings_reply(101, Err(error.clone()), h.now), h.now)
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert!(o.detect_after_unknown);
        assert_eq!(s.last_error(), Some(&error));
        assert_eq!(s.returned_revision(), Some("2222222222222222"));
        assert!(s.consent_restart(102, 17, &[], &[]).is_err());
        s.poll_new_instance(102).unwrap();
        h.now += 1;
        let o = s
            .reply(
                settings_reply(102, Err(CallFailure::Unavailable), h.now),
                h.now,
            )
            .unwrap();
        assert!(o.detect_after_unknown);
        assert_eq!(s.state(), &SettingsTransitionState::WaitingNewInstance);
        s.poll_new_instance(103).unwrap();
        h.now += 1;
        let o = s
            .reply(
                settings_reply(
                    103,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &h.value, h.platform,
                    )))),
                    h.now,
                ),
                h.now,
            )
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert_eq!(s.state(), &SettingsTransitionState::NeedsRestartConsent);
        assert!(s.consent_restart(104, 17, &[h.attempt.step], &[]).is_err());
        let FlowEvent::Observe { samples } = &o.observations[0] else {
            panic!()
        };
        let (events, _) = s
            .consent_restart(104, s.view_revision(), &[h.attempt.step], samples)
            .unwrap();
        reduce_settings(&mut h.core, &events, h.now);
        h.now += 1;
        s.reply(
            settings_reply(104, Ok(DecodedReply::Acknowledged), h.now),
            h.now,
        )
        .unwrap();
        s.poll_new_instance(105).unwrap();
        h.now += 1;
        h.value["result"]["installer"]["instance"]["id"] = json!(100);
        h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
        let o = s
            .reply(
                settings_reply(
                    105,
                    Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                        &h.value, h.platform,
                    )))),
                    h.now,
                ),
                h.now,
            )
            .unwrap();
        reduce_settings(&mut h.core, &o.observations, h.now);
        assert_eq!(s.state(), &SettingsTransitionState::Complete);
    }
}

#[test]
fn settings_new_instance_wrong_node_and_incomplete_status_cannot_complete() {
    let mut h = Harness::new();
    let mut s = settings_ready(&mut h);
    settings_updated(&mut s, &mut h);
    restart_settings(&mut s, &mut h, 101);
    h.now += 1;
    s.reply(
        settings_reply(101, Ok(DecodedReply::Acknowledged), h.now),
        h.now,
    )
    .unwrap();
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    let mut foreign = h.value.clone();
    foreign["result"]["installer"]["node"] = json!("33".repeat(32));
    assert!(
        s.reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &foreign, h.platform
                )))),
                h.now
            ),
            h.now
        )
        .is_err()
    );
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete),
                )),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(o.observations, vec![FlowEvent::Observe { samples: vec![] }]);
    assert_eq!(s.state(), &SettingsTransitionState::WaitingNewInstance);
    s.poll_new_instance(103).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
    let o = s
        .reply(
            settings_reply(
                103,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::Complete);
}

#[test]
fn restart_timeout_can_complete_from_detected_new_instance_without_another_mutation() {
    let mut h = Harness::new();
    let mut s = settings_ready(&mut h);
    settings_updated(&mut s, &mut h);
    restart_settings(&mut s, &mut h, 101);
    h.now += 1;
    let o = s
        .reply(
            settings_reply(101, Err(CallFailure::TimeoutOutcomeUnknown), h.now),
            h.now,
        )
        .unwrap();
    assert!(o.detect_after_unknown);
    assert_eq!(s.returned_revision(), Some("2222222222222222"));
    s.poll_new_instance(102).unwrap();
    h.now += 1;
    h.value["result"]["installer"]["instance"]["id"] = json!(100);
    h.value["result"]["installer"]["config_revision"] = json!("2222222222222222");
    let o = s
        .reply(
            settings_reply(
                102,
                Ok(DecodedReply::Status(StatusAdmission::Supported(health(
                    &h.value, h.platform,
                )))),
                h.now,
            ),
            h.now,
        )
        .unwrap();
    reduce_settings(&mut h.core, &o.observations, h.now);
    assert_eq!(s.state(), &SettingsTransitionState::Complete);
    assert_eq!(s.last_call_id(), 102);
}

#[test]
fn recovery_restart_consent_is_also_retired_by_same_view_plan_replacement() {
    let mut s = SettingsTransition::new(local(), 17);
    let mut v = value();
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        1,
        1,
        17,
    )
    .unwrap();
    s.consent_update(1, 17, true).unwrap();
    s.reply(
        settings_reply(
            1,
            Err(CallFailure::Refused(AgentRefusal::RevisionConflict)),
            2,
        ),
        2,
    )
    .unwrap();
    s.detected(
        &health(&v, AgentPlatform::Linux),
        ObservationSource::Live,
        3,
        3,
        18,
    )
    .unwrap();
    assert_eq!(
        s.state(),
        &SettingsTransitionState::NeedsRecoveryRestartConsent
    );
    v["result"]["installer"]["instance"]["id"] = json!(100);
    v["result"]["installer"]["config_revision"] = json!("3333333333333333");
    let observed = s
        .detected(
            &health(&v, AgentPlatform::Linux),
            ObservationSource::Live,
            4,
            4,
            18,
        )
        .unwrap();
    assert!(
        matches!(&observed[0], FlowEvent::Observe { samples } if samples.iter().all(|sample| sample.binding.epochs.instance_id == 100))
    );
    assert_eq!(s.state(), &SettingsTransitionState::NeedsDetection);
    assert_eq!(s.last_error(), Some(&CallFailure::InvalidResponse));
    let samples = s.current_samples().to_vec();
    assert!(s.consent_restart(2, 18, &[], &samples).is_err());
    assert!(s.consent_update(2, 18, true).is_err());
}
