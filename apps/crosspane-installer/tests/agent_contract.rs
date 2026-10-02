#![allow(clippy::unwrap_used)]

use crosspane_installer::agent_contract::*;
use crosspane_installer_core::{
    AttemptId, CounterId, EpochDependencies, EvidenceError, Flow, FlowError, FlowEvent, Milestone,
    StepId, StepSpec, StepState, Verification, check_activity,
};
use crosspane_types::id::{NodeId, WindowId};
use serde_json::{Value, json};
use std::collections::BTreeMap;

// Literal producer bytes, rather than serializing the consumer types to make its own oracle.
const STATUS: &[u8] = br#"{"ok":true,"result":{
 "controlling":null,"controlled_by":null,"projections":[],
 "displays":[{"id":1,"name":"local","pixels":[1920,1080],"scale":1.25,"mm":[500.5,300.5],"origin":[-10.5,0]}],
 "peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"remote",
 "connected":true,"displays":[{"id":2,"name":"remote","pixels":[3840,2160],"scale":2,"mm":[600,400],"origin":[0,0]}]}],
 "layout":[{"node":"1111111111111111","display":1,"origin_mm":[0,0],"version":17},
 {"node":"2222222222222222","display":2,"origin_mm":[500.5,-1.5],"version":19}],
 "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["private-vdisplay","video"]},
 "instance":{"id":18446744073709551615,"pid":4242,"uid":1000,"exe":"/home/u/.local/bin/crosspane-agent",
 "runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790950000000},
 "config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
 "recovery_pending":0,"startup_recovery":"restored",
 "gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
 "epochs":{"gate":7,"grants":3,"layout":2,"backends":1},
 "backends":[
 {"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
 {"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
 {"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
 {"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
 {"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
 {"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
 {"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
 {"name":"discovery","state":"ready","reason":null}],
 "keystore":"os_store","permissions":[],
 "discovery":{"enabled":true,"running":true,"candidates":1,"error":null},"tray":{"created":true},
 "audio":{"enabled":true,"active_peers":[],"frames_sent":14,"frames_played":15},"settings_opened":16,
 "peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"remote",
 "connected":true,"link_generation":3,"features":["e1","audio"],"grants_given":["browse","input"],
 "last_source_parking":null,"counters":{
 "e1_controller_started":0,"e1_controller_ended":1,"e1_target_started":2,"e1_target_ended":3,
 "e1_injections_ok":4,"e1_hud_shows":5,"e1_chord_releases":6,"e1_command_releases":7,
 "e2_source_started":8,"e2_source_returned":9,"e2_dest_started":10,"e2_dest_returned":11,
 "e2_frames_presented":null,"e2_returns_failed":13}}]}}}"#;
const BOOTSTRAP: &[u8] = br#"{"schema_version":1,"instance_id":1234567890123,"pid":4242,
 "started_unix_ms":1790950000000,"phase":"waiting_for_keystore","phase_seq":2,
 "keystore":null,"reason":null,"runtime_dir":"/run/user/1000/crosspane"}"#;
const EXIT: &[u8] =
    br#"{"schema_version":1,"instance_id":1234567890123,"stopped_unix_ms":1790950100000,
 "clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true}"#;
const ERASE: &[u8] =
    br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"}"#;

fn fixture() -> Value {
    serde_json::from_slice(STATUS).unwrap()
}
fn bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}
fn peer() -> NodeId {
    NodeId([0x22; 32])
}
fn local() -> NodeId {
    NodeId([0x11; 32])
}
fn admit(v: &Value, platform: AgentPlatform) -> Result<StatusAdmission, ContractError> {
    parse_status(&bytes(v), platform)
}
fn health(v: &Value) -> Box<HealthSnapshot> {
    match admit(v, AgentPlatform::Linux).unwrap() {
        StatusAdmission::Supported(h) => h,
        other => panic!("expected supported health: {other:?}"),
    }
}
fn expect_pending(v: &Value, reason: PendingHealthReason) {
    assert_eq!(
        admit(v, AgentPlatform::Linux),
        Ok(StatusAdmission::PendingHealthContract(reason))
    );
}
fn reply(request: &InstallerRequest, v: Value) -> Result<DecodedReply, CallFailure> {
    decode_reply(
        request,
        &bytes(&json!({"ok":true,"result":v})),
        AgentPlatform::Linux,
    )
}
fn mac(v: &mut Value, microphone: bool) {
    v["result"]["installer"]["permissions"] = json!([
        {"name":"input_monitoring","state":"granted"},
        {"name":"screen_recording","state":"granted"},
        {"name":"accessibility","state":"granted"}
    ]);
    if microphone {
        v["result"]["installer"]["permissions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"microphone","state":"granted"}));
    }
}

#[test]
fn every_used_request_has_exact_golden_json_and_one_line() {
    let node = peer().to_string();
    let cases = vec![
        (InstallerRequest::Status, json!({"cmd":"status"})),
        (InstallerRequest::Release, json!({"cmd":"release"})),
        (InstallerRequest::Panic, json!({"cmd":"panic"})),
        (InstallerRequest::Restart, json!({"cmd":"restart"})),
        (
            InstallerRequest::AskPermissions,
            json!({"cmd":"ask_permissions"}),
        ),
        (
            InstallerRequest::Dial {
                addr: "[::1]:47811".parse().unwrap(),
            },
            json!({"cmd":"dial","addr":"[::1]:47811"}),
        ),
        (
            InstallerRequest::PairListen { allow_input: false },
            json!({"cmd":"pair_listen","allow_input":false}),
        ),
        (
            InstallerRequest::PairJoin {
                addr: "192.0.2.1:47811".parse().unwrap(),
                allow_input: false,
            },
            json!({"cmd":"pair_join","addr":"192.0.2.1:47811","allow_input":false}),
        ),
        (InstallerRequest::PairStatus, json!({"cmd":"pair_status"})),
        (InstallerRequest::PairScan, json!({"cmd":"pair_scan"})),
        (
            InstallerRequest::PairConfirm { accept: true },
            json!({"cmd":"pair_confirm","accept":true}),
        ),
        (
            InstallerRequest::PairPick { index: 2 },
            json!({"cmd":"pair_pick","index":2}),
        ),
        (
            InstallerRequest::Allow {
                peer: peer(),
                capability: GrantableCapability::Speaker,
                allow: true,
            },
            json!({"cmd":"allow","peer":node,"capability":"speaker","allow":true}),
        ),
        (
            InstallerRequest::Place {
                placements: vec![Placement {
                    node: local(),
                    display: 1,
                    origin_mm: [-1.5, 0.0],
                }],
            },
            json!({"cmd":"place","placements":[{"node":local().to_string(),"display":1,"origin_mm":[-1.5,0.0]}]}),
        ),
        (InstallerRequest::Windows, json!({"cmd":"windows"})),
        (
            InstallerRequest::WindowsFrom { peer: peer() },
            json!({"cmd":"windows_from","peer":node}),
        ),
        (
            InstallerRequest::Project {
                window: WindowId(99),
                peer: peer(),
            },
            json!({"cmd":"project","window":99,"peer":node}),
        ),
        (
            InstallerRequest::Pull {
                peer: peer(),
                window: WindowId(99),
            },
            json!({"cmd":"pull","window":99,"peer":node}),
        ),
        (
            InstallerRequest::Return {
                projection: 42,
                source: None,
            },
            json!({"cmd":"return","projection":42,"source":null}),
        ),
        (
            InstallerRequest::Return {
                projection: 42,
                source: Some(peer()),
            },
            json!({"cmd":"return","projection":42,"source":node}),
        ),
        (
            InstallerRequest::SettingsUpdate {
                expected_revision: "0123456789abcdef".into(),
                mac_virtual_display: true,
            },
            json!({"cmd":"settings_update","expected_revision":"0123456789abcdef","mac_virtual_display":true}),
        ),
    ];
    for (request, expected) in cases {
        let encoded = encode_request(&request).unwrap();
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(encoded.iter().filter(|b| **b == b'\n').count(), 1);
        assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), expected);
    }
    assert_eq!(
        encode_request(&InstallerRequest::Dial {
            addr: "192.0.2.1:47811".parse().unwrap()
        })
        .unwrap(),
        b"{\"cmd\":\"dial\",\"addr\":\"192.0.2.1:47811\"}\n"
    );
}

#[test]
fn pairing_never_automatically_grants_input_and_requests_validate() {
    for r in [
        InstallerRequest::PairListen { allow_input: true },
        InstallerRequest::PairJoin {
            addr: "127.0.0.1:1".parse().unwrap(),
            allow_input: true,
        },
        InstallerRequest::PairPick { index: 128 },
    ] {
        assert_eq!(encode_request(&r), Err(ContractError::InvalidValue));
    }
    for r in ["ABCDEF0123456789", "abc", "000000000000000g"] {
        assert_eq!(
            encode_request(&InstallerRequest::SettingsUpdate {
                expected_revision: r.into(),
                mac_virtual_display: false
            }),
            Err(ContractError::InvalidValue)
        );
    }
    let place = |origins| InstallerRequest::Place {
        placements: origins,
    };
    let p = Placement {
        node: local(),
        display: 1,
        origin_mm: [0.0, 0.0],
    };
    assert_eq!(
        encode_request(&place(vec![p.clone(); 129])),
        Err(ContractError::Oversize)
    );
    assert_eq!(
        encode_request(&place(vec![p.clone(), p])),
        Err(ContractError::InvalidValue)
    );
    assert_eq!(
        encode_request(&place(vec![Placement {
            node: local(),
            display: 1,
            origin_mm: [f64::NAN, 0.0]
        }])),
        Err(ContractError::InvalidValue)
    );
}

#[test]
fn capability_wire_names_are_exact_and_mic_is_not_grantable() {
    for (capability, name) in [
        (GrantableCapability::Input, "input"),
        (GrantableCapability::Share, "share"),
        (GrantableCapability::Browse, "browse"),
        (GrantableCapability::Present, "present"),
        (GrantableCapability::Speaker, "speaker"),
    ] {
        let v: Value = serde_json::from_slice(
            &encode_request(&InstallerRequest::Allow {
                peer: peer(),
                capability,
                allow: false,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(v["capability"], name);
    }
    assert!(serde_json::from_value::<GrantableCapability>(json!("mic")).is_err());
    let mut v = fixture();
    v["result"]["installer"]["peers"][0]["grants_given"] =
        json!(["browse", "input", "mic", "present", "share", "speaker"]);
    assert!(matches!(
        admit(&v, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
}

#[test]
fn acknowledgements_and_pull_prose_have_no_completion_payload() {
    for request in [
        InstallerRequest::Release,
        InstallerRequest::Restart,
        InstallerRequest::AskPermissions,
        InstallerRequest::Dial {
            addr: "127.0.0.1:47811".parse().unwrap(),
        },
        InstallerRequest::Pull {
            peer: peer(),
            window: WindowId(99),
        },
    ] {
        assert_eq!(
            reply(&request, json!("showing window 400 of remote")),
            Ok(DecodedReply::Acknowledged)
        );
        assert_eq!(
            decode_reply(&request, br#"{"ok":true}"#, AgentPlatform::Linux),
            Ok(DecodedReply::Acknowledged)
        );
    }
}

#[test]
fn envelopes_are_boolean_and_refusals_discard_prose() {
    for raw in [
        br#"{}"#.as_slice(),
        br#"{"ok":1}"#,
        br#"{"ok":false}"#,
        br#"{"ok":true,"error":"bad"}"#,
        br#"{"ok":false,"error":"bad","result":"success"}"#,
        br#"{"ok":true,"extra":1}"#,
    ] {
        assert_eq!(
            decode_reply(&InstallerRequest::Status, raw, AgentPlatform::Linux),
            Err(CallFailure::InvalidResponse)
        );
    }
    for (error, category) in [
        ("revision_conflict", AgentRefusal::RevisionConflict),
        ("not_supported", AgentRefusal::NotSupported),
        ("private unrecognized text", AgentRefusal::Other),
    ] {
        assert_eq!(
            decode_reply(
                &InstallerRequest::Restart,
                &bytes(&json!({"ok":false,"error":error})),
                AgentPlatform::Linux
            ),
            Err(CallFailure::Refused(category))
        );
    }
    assert!(parse_status(br#"{"ok":false,"error":"bad"}"#, AgentPlatform::Linux).is_err());
}

#[test]
fn frozen_status_admits_all_fields_without_policy_or_identity_derivation() {
    let h = health(&fixture());
    let i = h.installer();
    assert_eq!(i.instance.id, u64::MAX);
    assert_eq!(i.instance.pid, 4242);
    assert_eq!(i.instance.uid, 1000);
    assert_eq!(i.node, local());
    assert_eq!(i.config_revision, "9f86d081884c7d65");
    assert_eq!(i.backends.len(), 15);
    assert_eq!(i.permissions.len(), 0);
    assert_eq!(h.terminal().controlling, None);
    assert_eq!(h.display_layout().placements[1].version, 19);
    assert_eq!(
        decode_reply(&InstallerRequest::Status, STATUS, AgentPlatform::Linux),
        Ok(DecodedReply::Status(StatusAdmission::Supported(h)))
    );
}

#[test]
fn every_versioned_field_including_nullable_fields_is_required() {
    fn leaves(v: &Value, path: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                for (key, value) in o {
                    let path = format!("{path}/{key}");
                    out.push(path.clone());
                    leaves(value, &path, out);
                }
            }
            Value::Array(a) => {
                for (idx, value) in a.iter().enumerate() {
                    leaves(value, &format!("{path}/{idx}"), out);
                }
            }
            _ => {}
        }
    }
    let original = fixture();
    let mut paths = Vec::new();
    leaves(
        &original["result"]["installer"],
        "/result/installer",
        &mut paths,
    );
    for path in paths {
        let mut v = original.clone();
        let (parent, key) = path.rsplit_once('/').unwrap();
        v.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Ok(StatusAdmission::PendingHealthContract(
                PendingHealthReason::Incomplete
            )),
            "{path}"
        );
    }
}

#[test]
fn explicit_null_is_distinct_from_missing_and_unknown_session_is_preserved() {
    let mut v = fixture();
    v["result"]["installer"]["gate"]["active"] = Value::Null;
    v["result"]["installer"]["gate"]["session"] = json!("unknown");
    v["result"]["installer"]["peers"][0]["link_generation"] = Value::Null;
    let h = health(&v);
    assert_eq!(h.installer().gate.active, None);
    assert_eq!(h.installer().gate.session, SessionState::Unknown);
    assert_eq!(
        counter_sample(&h, Some(peer()), ObservationSource::Live, 5),
        Err(ContractError::DisconnectedPeer)
    );
    for field in [
        "controlling",
        "controlled_by",
        "projections",
        "displays",
        "peers",
        "layout",
    ] {
        let mut v = fixture();
        v["result"].as_object_mut().unwrap().remove(field);
        expect_pending(&v, PendingHealthReason::Incomplete);
    }
}

#[test]
fn unsupported_absent_and_unknown_health_never_become_supported() {
    let mut v = fixture();
    v["result"].as_object_mut().unwrap().remove("installer");
    expect_pending(&v, PendingHealthReason::Absent);
    let mut v = fixture();
    v["result"]["installer"]["schema_version"] = json!(2);
    expect_pending(&v, PendingHealthReason::UnsupportedVersion);
    for path in [
        "/result/installer/gate/session",
        "/result/installer/startup_recovery",
        "/result/installer/keystore",
        "/result/installer/backends/0/state",
        "/result/installer/backends/0/name",
        "/result/installer/discovery/error",
        "/result/installer/peers/0/last_source_parking",
        "/result/installer/peers/0/grants_given/0",
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = json!("future_value");
        expect_pending(&v, PendingHealthReason::UnknownEnum);
    }
    let mut v = fixture();
    mac(&mut v, true);
    v["result"]["installer"]["permissions"][0]["state"] = json!("future");
    assert_eq!(
        admit(&v, AgentPlatform::Macos),
        Ok(StatusAdmission::PendingHealthContract(
            PendingHealthReason::UnknownEnum
        ))
    );
}

#[test]
fn wrong_types_negative_overflow_and_noncanonical_identities_are_errors() {
    for (path, value) in [
        ("/result/installer/schema_version", json!(-1)),
        ("/result/installer/instance/pid", json!(4294967296_u64)),
        ("/result/installer/epochs/gate", json!(-1)),
        ("/result/installer/gate/open", json!("true")),
        (
            "/result/installer/peers/0/counters/e1_injections_ok",
            json!(1.5),
        ),
        ("/result/installer/recovery_pending", json!(-1)),
        ("/result/installer/node", json!("short")),
        ("/result/installer/peers/0/node", json!("A".repeat(64))),
        (
            "/result/installer/audio/active_peers",
            json!(["A".repeat(64)]),
        ),
        ("/result/controlling", json!("2222222222222222")),
        (
            "/result/installer/config_revision",
            json!("ABCDEF0123456789"),
        ),
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = value;
        assert!(admit(&v, AgentPlatform::Linux).is_err(), "{path}");
    }
}

#[test]
fn backend_list_order_reasons_and_all_literal_states_are_enforced() {
    for state in ["missing", "blocked", "failed"] {
        for reason in [
            "not_supported",
            "permission",
            "construction_failed",
            "worker_exited",
            "disabled",
            "unknown",
        ] {
            let mut v = fixture();
            v["result"]["installer"]["backends"][0]["state"] = json!(state);
            v["result"]["installer"]["backends"][0]["reason"] = json!(reason);
            assert!(matches!(
                admit(&v, AgentPlatform::Linux),
                Ok(StatusAdmission::Supported(_))
            ));
        }
    }
    for (state, reason) in [("ready", json!("unknown")), ("failed", Value::Null)] {
        let mut v = fixture();
        v["result"]["installer"]["backends"][0]["state"] = json!(state);
        v["result"]["installer"]["backends"][0]["reason"] = reason;
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::InvalidValue)
        );
    }
    let mut v = fixture();
    v["result"]["installer"]["backends"]
        .as_array_mut()
        .unwrap()
        .swap(0, 1);
    assert_eq!(
        admit(&v, AgentPlatform::Linux),
        Err(ContractError::InvalidValue)
    );
    let mut v = fixture();
    v["result"]["installer"]["backends"]
        .as_array_mut()
        .unwrap()
        .pop();
    expect_pending(&v, PendingHealthReason::Incomplete);
}

#[test]
fn sorted_sets_unique_full_nodes_and_unordered_peer_features() {
    for (path, value) in [
        (
            "/result/installer/build/features",
            json!(["video", "private-vdisplay"]),
        ),
        (
            "/result/installer/build/features",
            json!(["video", "video"]),
        ),
        (
            "/result/installer/peers/0/grants_given",
            json!(["input", "browse"]),
        ),
        (
            "/result/installer/peers/0/grants_given",
            json!(["input", "input"]),
        ),
        (
            "/result/installer/audio/active_peers",
            json!([peer().to_string(), peer().to_string()]),
        ),
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = value;
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::InvalidValue)
        );
    }
    let mut v = fixture();
    let p = v["result"]["installer"]["peers"][0].clone();
    v["result"]["installer"]["peers"]
        .as_array_mut()
        .unwrap()
        .push(p);
    assert_eq!(
        admit(&v, AgentPlatform::Linux),
        Err(ContractError::InvalidValue)
    );
    let mut v = fixture();
    v["result"]["installer"]["peers"][0]["features"] = json!(["video", "audio", "e1"]);
    assert!(matches!(
        admit(&v, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
}

#[test]
fn mac_three_base_permissions_and_optional_microphone_are_facts() {
    for (audio, microphone) in [(false, false), (true, true), (true, false)] {
        let mut v = fixture();
        mac(&mut v, microphone);
        v["result"]["installer"]["audio"]["enabled"] = json!(audio);
        let StatusAdmission::Supported(h) = admit(&v, AgentPlatform::Macos).unwrap() else {
            panic!()
        };
        assert_eq!(h.installer().audio.enabled, audio);
        assert_eq!(
            h.installer()
                .permissions
                .iter()
                .any(|p| p.name == PermissionName::Microphone),
            microphone
        );
        // Enabled audio without the optional fourth permission remains admitted facts, not readiness.
    }
    let mut v = fixture();
    mac(&mut v, true);
    v["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
        .pop();
    v["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
        .pop();
    assert_eq!(
        admit(&v, AgentPlatform::Macos),
        Ok(StatusAdmission::PendingHealthContract(
            PendingHealthReason::Incomplete
        ))
    );
    let mut v = fixture();
    mac(&mut v, false);
    let p = v["result"]["installer"]["permissions"][0].clone();
    v["result"]["installer"]["permissions"]
        .as_array_mut()
        .unwrap()
        .push(p);
    assert_eq!(
        admit(&v, AgentPlatform::Macos),
        Err(ContractError::InvalidValue)
    );
    for state in ["granted", "not_granted", "unknown"] {
        let mut v = fixture();
        mac(&mut v, true);
        v["result"]["installer"]["permissions"][3]["state"] = json!(state);
        assert!(matches!(
            admit(&v, AgentPlatform::Macos),
            Ok(StatusAdmission::Supported(_))
        ));
    }
}

#[test]
fn startup_failure_and_current_parking_count_are_independent_preserved_facts() {
    for recovery in ["restored", "nothing_parked", "failed", "none"] {
        for count in [0, 1] {
            let mut v = fixture();
            v["result"]["installer"]["startup_recovery"] = json!(recovery);
            v["result"]["installer"]["recovery_pending"] = json!(count);
            if count != 0 {
                v["result"]["projections"] = json!([{"source":local().short(),"projection":1,"text":"ignored","received":null}]);
            }
            let h = health(&v);
            assert_eq!(h.installer().recovery_pending, count);
            if recovery == "failed" {
                assert_eq!(h.installer().startup_recovery, StartupRecovery::Failed);
            }
        }
    }
}

#[test]
fn legacy_projection_short_identity_is_exact_and_never_a_window_mapping() {
    let mut v = fixture();
    v["result"]["controlling"] = json!(peer().to_string());
    v["result"]["projections"] =
        json!([{"source":peer().short(),"projection":99,"text":"secret title","received":null}]);
    let h = health(&v);
    assert_eq!(h.terminal().controlling, Some(peer()));
    assert_eq!(
        h.terminal().projections,
        vec![ProjectionRef {
            source: peer(),
            projection: 99
        }]
    );
    for source in [
        "22",
        "222222222222222",
        "22222222222222222",
        &peer().to_string(),
        "3333333333333333",
    ] {
        let mut v = v.clone();
        v["result"]["projections"][0]["source"] = json!(source);
        expect_pending(&v, PendingHealthReason::AmbiguousIdentity);
    }
    let mut v = v.clone();
    let p = v["result"]["projections"][0].clone();
    v["result"]["projections"].as_array_mut().unwrap().push(p);
    assert_eq!(
        admit(&v, AgentPlatform::Linux),
        Err(ContractError::InvalidValue)
    );
    for field in ["source", "projection"] {
        let mut v = fixture();
        v["result"]["projections"] =
            json!([{"source":peer().short(),"projection":99,"text":"ignored","received":null}]);
        v["result"]["projections"][0]
            .as_object_mut()
            .unwrap()
            .remove(field);
        expect_pending(&v, PendingHealthReason::Incomplete);
    }
}

#[test]
fn ambiguous_short_ids_in_layout_or_projection_never_admit() {
    let mut collision = local().0;
    collision[31] = 0x44;
    let collision = NodeId(collision);
    let mut v = fixture();
    v["result"]["installer"]["peers"][0]["node"] = json!(collision.to_string());
    v["result"]["peers"][0]["node"] = json!(collision.to_string());
    v["result"]["layout"] = json!([]);
    assert!(matches!(
        admit(&v, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
    v["result"]["layout"] =
        json!([{"node":local().short(),"display":1,"origin_mm":[0,0],"version":0}]);
    expect_pending(&v, PendingHealthReason::AmbiguousIdentity);
    v["result"]["layout"] = json!([]);
    v["result"]["projections"] =
        json!([{"source":local().short(),"projection":1,"text":"ignored","received":null}]);
    expect_pending(&v, PendingHealthReason::AmbiguousIdentity);
}

#[test]
fn legacy_geometry_units_and_committed_versions_are_preserved() {
    let h = health(&fixture());
    let d = h.display_layout();
    assert_eq!(d.local_displays[0].pixels, [1920, 1080]);
    assert_eq!(d.local_displays[0].scale, 1.25);
    assert_eq!(d.local_displays[0].mm, [500.5, 300.5]);
    assert_eq!(d.local_displays[0].origin, [-10.5, 0.0]);
    assert_eq!(d.peer_displays[0].node, peer());
    assert_eq!(d.placements[1].origin_mm, [500.5, -1.5]);
    assert_eq!(d.placements[1].version, 19);
    let mut v = fixture();
    v["result"]["layout"] = json!([]);
    v["result"]["displays"] = json!([]);
    v["result"]["peers"][0]["displays"] = json!([]);
    let h = health(&v);
    assert!(h.display_layout().local_displays.is_empty());
    assert!(h.display_layout().peer_displays[0].displays.is_empty());
}

#[test]
fn missing_geometry_or_duplicate_display_identity_is_not_invented() {
    let mut v = fixture();
    v["result"]["layout"][1]["display"] = json!(999);
    expect_pending(&v, PendingHealthReason::MissingDisplay);
    for (path, value) in [
        ("/result/displays/0/scale", json!(0)),
        ("/result/displays/0/mm", json!([-1, 2])),
        ("/result/displays/0/pixels", json!([1.5, 2])),
        (
            "/result/peers/0/node",
            json!(NodeId([0x33; 32]).to_string()),
        ),
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = value;
        assert!(!matches!(
            admit(&v, AgentPlatform::Linux),
            Ok(StatusAdmission::Supported(_))
        ));
    }
    for array in ["/result/displays", "/result/peers", "/result/layout"] {
        let mut v = fixture();
        let a = v.pointer_mut(array).unwrap().as_array_mut().unwrap();
        a.push(a[0].clone());
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::InvalidValue)
        );
    }
}

#[test]
fn local_fractional_nullable_display_and_remote_device_pixels_are_distinct() {
    let local =
        json!([{"id":9,"app":"test","title":"private","display":null,"size":[300.5,200.25]}]);
    let DecodedReply::Windows(w) = reply(&InstallerRequest::Windows, local.clone()).unwrap() else {
        panic!()
    };
    assert_eq!(w[0].display, None);
    assert_eq!(w[0].size, [300.5, 200.25]);
    let request = InstallerRequest::WindowsFrom { peer: peer() };
    assert_eq!(reply(&request, local), Err(CallFailure::InvalidResponse));
    let DecodedReply::WindowsFrom(w) = reply(
        &request,
        json!([{"id":9,"app":"test","title":"private","size":[601,401]}]),
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(w[0].size, [601, 401]);
    for bad in [
        json!([{"id":9,"app":"test","title":"private","size":[300,200]}]),
        json!([{"id":9,"app":"test","title":"private","display":null,"size":[-1,200]}]),
    ] {
        assert_eq!(
            reply(&InstallerRequest::Windows, bad),
            Err(CallFailure::InvalidResponse)
        );
    }
}

#[test]
fn pairing_literal_phases_scan_addresses_and_nullable_presence() {
    for phase in [
        "idle",
        "listening",
        "connecting",
        "confirm",
        "pick",
        "waiting",
        "paired",
        "failed",
        "future",
    ] {
        let DecodedReply::PairStatus(p) = reply(
            &InstallerRequest::PairStatus,
            json!({"phase":phase,
            "sas":null,"candidates":[],"peer":null,"error":null}),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(p.phase == PairPhase::Paired, phase == "paired");
        assert_eq!(p.phase == PairPhase::Unknown, phase == "future");
    }
    for missing in ["sas", "peer", "error", "candidates", "phase"] {
        let mut v =
            json!({"phase":"confirm","sas":"123 456","candidates":[],"peer":null,"error":null});
        v.as_object_mut().unwrap().remove(missing);
        assert_eq!(
            reply(&InstallerRequest::PairStatus, v),
            Err(CallFailure::InvalidResponse)
        );
    }
    let DecodedReply::PairScan(c) = reply(
        &InstallerRequest::PairScan,
        json!([{"name":"mac","addr":"[::1]:47811"}]),
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(c[0].addr, "[::1]:47811".parse().unwrap());
    assert_eq!(
        reply(
            &InstallerRequest::PairScan,
            json!([{"name":"mac","addr":"hostname:47811"}])
        ),
        Err(CallFailure::InvalidResponse)
    );
}

#[test]
fn duplicate_keys_recursive_bounds_trailing_objects_and_numeric_limits() {
    for raw in [
        br#"{"ok":true,"ok":false}"#.as_slice(),
        br#"{"ok":true,"result":{"nested":{"x":1,"x":2}}}"#,
        br#"{"ok":true,"result":[{"x":1,"x":2}]}"#,
        br#"{"ok":true,"result":1e400}"#,
    ] {
        assert_eq!(
            parse_status(raw, AgentPlatform::Linux),
            Err(ContractError::InvalidJson)
        );
    }
    for raw in [br#"{"ok":true} {}"#.as_slice(), br#"{"ok":true}garbage"#] {
        assert_eq!(
            parse_status(raw, AgentPlatform::Linux),
            Err(ContractError::TrailingData)
        );
    }
    assert_eq!(
        parse_status(&vec![b' '; MAX_RESPONSE_BYTES + 1], AgentPlatform::Linux),
        Err(ContractError::Oversize)
    );
    let mut v = fixture();
    v["result"]["unknown"] = json!("x".repeat(MAX_STRING_BYTES + 1));
    assert_eq!(
        admit(&v, AgentPlatform::Linux),
        Err(ContractError::Oversize)
    );
    for path in [
        "/result/installer/peers",
        "/result/projections",
        "/result/displays",
        "/result/layout",
    ] {
        let mut v = fixture();
        let item = v
            .pointer(path)
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or(json!({}));
        *v.pointer_mut(path).unwrap() = json!(vec![item; 129]);
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::Oversize)
        );
    }
    for (request, item) in [
        (
            InstallerRequest::PairScan,
            json!({"name":"peer","addr":"127.0.0.1:1"}),
        ),
        (
            InstallerRequest::Windows,
            json!({"id":1,"app":"test","title":"test","display":null,"size":[1,1]}),
        ),
    ] {
        assert_eq!(
            reply(&request, json!(vec![item; 129])),
            Err(CallFailure::InvalidResponse)
        );
    }
    for value in [
        json!({"phase":"confirm","sas":"x".repeat(129),"candidates":[],"peer":null,"error":null}),
        json!({"phase":"pick","sas":null,"candidates":["x".repeat(129)],"peer":null,"error":null}),
    ] {
        assert_eq!(
            reply(&InstallerRequest::PairStatus, value),
            Err(CallFailure::InvalidResponse)
        );
    }
    let mut too_large = fixture();
    too_large["result"]["installer"]["peers"][0]["counters"]["e1_controller_started"] =
        serde_json::from_str("18446744073709551616").unwrap();
    assert!(admit(&too_large, AgentPlatform::Linux).is_err());
}

#[test]
fn exact_bounds_whitespace_and_unknown_legacy_fields_are_admitted() {
    let mut v = fixture();
    v["result"]["legacy_extra"] = json!("x".repeat(4096));
    let mut raw = bytes(&v);
    raw.extend_from_slice(b" \r\n\t");
    assert!(matches!(
        parse_status(&raw, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
    let DecodedReply::PairStatus(p) = reply(
        &InstallerRequest::PairStatus,
        json!({"phase":"pick","sas":"x".repeat(128),
        "candidates":vec!["x".repeat(128);128],"peer":null,"error":null}),
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(p.candidates.len(), 128);
}

#[test]
fn stable_complete_metrics_preserve_global_scope_and_receipt_metadata() {
    let h = health(&fixture());
    let local_sample = counter_sample(&h, None, ObservationSource::Demo, 987).unwrap();
    assert_eq!(local_sample.sample.values.len(), 3);
    assert_eq!(local_sample.sample.source, ObservationSource::Demo);
    assert_eq!(local_sample.sample.observed_at_ms, 987);
    assert_eq!(local_sample.sample.binding.peer, None);
    assert_eq!(local_sample.sample.binding.link_generation, None);
    let sample = counter_sample(&h, Some(peer()), ObservationSource::Live, 123).unwrap();
    assert_eq!(sample.sample.values.len(), 16);
    assert_eq!(sample.unavailable, vec![Metric::FramesPresented]);
    for id in 0..17 {
        if id == 12 {
            assert!(!sample.sample.values.contains_key(&CounterId(id)));
        } else {
            assert_eq!(sample.sample.values[&CounterId(id)], u64::from(id));
        }
    }
    assert_eq!(sample.sample.binding.local_node, local());
    assert_eq!(sample.sample.binding.peer, Some(peer()));
    assert_eq!(sample.sample.binding.link_generation, Some(3));
    assert_eq!(sample.sample.binding.epochs.instance_id, u64::MAX);
    assert_eq!(sample.sample.binding.epochs.gate_epoch, 7);
    assert_eq!(sample.sample.binding.epochs.grants_epoch, 3);
    assert_eq!(sample.sample.binding.epochs.layout_epoch, 2);
    assert_eq!(sample.sample.binding.epochs.backends_epoch, 1);
    assert_eq!(sample.sample.observed_at_ms, 123);
    assert_eq!(
        sample.sample.values[&CounterId(14)],
        local_sample.sample.values[&CounterId(14)]
    );
    assert_eq!(
        sample.sample.values[&CounterId(15)],
        local_sample.sample.values[&CounterId(15)]
    );
}

#[test]
fn normalization_requires_exact_known_connected_peer_and_link() {
    let mut v = fixture();
    assert_eq!(
        counter_sample(
            &health(&v),
            Some(NodeId([0x33; 32])),
            ObservationSource::Live,
            5
        ),
        Err(ContractError::UnknownPeer)
    );
    v["result"]["installer"]["peers"][0]["connected"] = json!(false);
    assert_eq!(
        counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 5),
        Err(ContractError::DisconnectedPeer)
    );
    v["result"]["installer"]["peers"][0]["connected"] = json!(true);
    v["result"]["installer"]["peers"][0]["link_generation"] = Value::Null;
    assert_eq!(
        counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 5),
        Err(ContractError::DisconnectedPeer)
    );
}

#[test]
fn normalized_union_survives_exercise_changes_and_missing_presented_invalidates() {
    let mut v = fixture();
    v["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"] = json!(0);
    let first = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 1).unwrap();
    assert_eq!(first.sample.values.len(), 17);
    for counter in ["e1_controller_started", "e2_source_started"] {
        v["result"]["installer"]["peers"][0]["counters"][counter] = json!(99);
        let next = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 2).unwrap();
        assert_eq!(
            next.sample.values.keys().collect::<Vec<_>>(),
            first.sample.values.keys().collect::<Vec<_>>()
        );
    }
    v["result"]["installer"]["audio"]["frames_sent"] = json!(100);
    let next = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 3).unwrap();
    assert_eq!(next.sample.values.len(), 17);
    v["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"] = Value::Null;
    let missing = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 4).unwrap();
    assert_eq!(
        check_activity(
            AttemptId(1),
            &first.sample,
            &missing.sample,
            EpochDependencies {
                gate: true,
                grants: true,
                layout: true,
                backends: true
            },
            &[CounterId(0)],
            &[]
        ),
        Err(EvidenceError::MissingCounter)
    );
}

#[test]
fn core_observe_detects_regression_above_old_proof_and_never_revives() {
    let mut flow = Flow::new(vec![
        StepSpec {
            id: StepId(1),
            prerequisites: vec![],
            required_for_installed: true,
            required_for_ready: true,
            requires_fresh_observation: false,
            requires_activity: true,
            requires_human: false,
            requires_fixture: false,
        },
        StepSpec {
            id: StepId(2),
            prerequisites: vec![StepId(1)],
            required_for_installed: false,
            required_for_ready: true,
            requires_fresh_observation: true,
            requires_activity: false,
            requires_human: false,
            requires_fixture: false,
        },
    ])
    .unwrap();
    let mut v = fixture();
    let start = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 1)
        .unwrap()
        .sample;
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![start.clone()],
        },
        1,
    )
    .unwrap();
    let detect = flow
        .reduce(FlowEvent::Begin { step: StepId(1) }, 1)
        .unwrap()
        .remove(0);
    let job = flow
        .reduce(
            FlowEvent::Detected {
                step: StepId(1),
                operation: detect.operation,
                needs_action: false,
            },
            1,
        )
        .unwrap()
        .remove(0);
    v["result"]["installer"]["peers"][0]["counters"]["e1_controller_started"] = json!(10);
    let end = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 2)
        .unwrap()
        .sample;
    flow.reduce(
        FlowEvent::Observe {
            samples: vec![end.clone()],
        },
        2,
    )
    .unwrap();
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
        &[CounterId(0)],
        &[],
    )
    .unwrap();
    flow.reduce(
        FlowEvent::Verified {
            step: StepId(1),
            operation: job.operation,
            verification: Verification {
                source: ObservationSource::Live,
                observed_at_ms: 2,
                binding: Some(end.binding.clone()),
                activity: Some(proof),
                human_attempt: None,
                fixture_attempt: None,
            },
        },
        2,
    )
    .unwrap();
    for (time, value) in [(3, 100), (4, 50), (5, 75)] {
        v["result"]["installer"]["peers"][0]["counters"]["e1_controller_started"] = json!(value);
        let sample = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, time)
            .unwrap()
            .sample;
        let outcome = flow.reduce(
            FlowEvent::Observe {
                samples: vec![sample.clone()],
            },
            time,
        );
        if time >= 4 {
            assert_eq!(outcome, Err(FlowError::InvalidEvidence));
        } else {
            outcome.unwrap();
        }
        let summary = flow.summary(time, &[sample]);
        if time >= 4 {
            assert_ne!(summary.steps[0].state, StepState::Satisfied);
        }
        assert_ne!(summary.milestone, Milestone::WorkspaceReady);
    }
}

#[test]
fn loaded_revision_returned_revision_conflict_and_restart_are_distinct_values() {
    let h = health(&fixture());
    let request = InstallerRequest::SettingsUpdate {
        expected_revision: h.installer().config_revision.clone(),
        mac_virtual_display: true,
    };
    assert_eq!(
        decode_reply(
            &request,
            br#"{"ok":false,"error":"revision_conflict"}"#,
            AgentPlatform::Macos
        ),
        Err(CallFailure::Refused(AgentRefusal::RevisionConflict))
    );
    let DecodedReply::SettingsUpdated(update) = reply(
        &request,
        json!({"revision":"0123456789abcdef","restart_required":true}),
    )
    .unwrap() else {
        panic!()
    };
    assert_ne!(h.installer().config_revision, update.revision);
    assert_eq!(
        reply(&InstallerRequest::Restart, json!("restarting")),
        Ok(DecodedReply::Acknowledged)
    );
    let mut v = fixture();
    v["result"]["installer"]["instance"]["id"] = json!(5);
    v["result"]["installer"]["config_revision"] = json!(update.revision);
    let restarted = health(&v);
    assert_ne!(restarted.installer().instance.id, h.installer().instance.id);
    assert_eq!(restarted.installer().config_revision, update.revision);
    // Matching new-instance completion and renewed conflict consent belong to WP-4.4b.
    for result in [
        json!({"revision":"0123456789abcdef","restart_required":false}),
        json!({"revision":"BAD","restart_required":true}),
        json!("saved"),
    ] {
        assert_eq!(reply(&request, result), Err(CallFailure::InvalidResponse));
    }
}

#[test]
fn lifecycle_bytes_types_nullable_fields_and_bootstrap_phase_truth() {
    let b = parse_bootstrap(BOOTSTRAP).unwrap();
    assert_eq!(b.instance_id, 1234567890123);
    assert_eq!(b.phase, BootstrapPhase::WaitingForKeystore);
    assert_eq!(b.keystore, None);
    for phase in ["starting", "waiting_for_keystore", "ready", "failed"] {
        let mut v: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        v["phase"] = json!(phase);
        assert!(parse_bootstrap(&bytes(&v)).is_ok());
    }
    for reason in [
        "config",
        "lock_held",
        "platform",
        "keystore",
        "socket",
        "other",
    ] {
        let mut v: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        v["phase"] = json!("failed");
        v["reason"] = json!(reason);
        assert!(parse_bootstrap(&bytes(&v)).is_ok());
        v["phase"] = json!("starting");
        assert_eq!(
            parse_bootstrap(&bytes(&v)),
            Err(ContractError::InvalidValue)
        );
    }
    for keystore in ["os_store", "file"] {
        let mut v: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        v["keystore"] = json!(keystore);
        assert!(parse_bootstrap(&bytes(&v)).is_ok());
    }
    for field in ["reason", "keystore"] {
        let mut v: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        v.as_object_mut().unwrap().remove(field);
        assert!(parse_bootstrap(&bytes(&v)).is_err());
    }
    assert!(parse_bootstrap(br#"{"schema_version":1,"schema_version":1}"#).is_err());
    let mut raw = BOOTSTRAP.to_vec();
    raw.extend_from_slice(b"{}");
    assert_eq!(parse_bootstrap(&raw), Err(ContractError::TrailingData));
}

#[test]
fn clean_exit_boolean_is_exact_literal_derivation_for_all_parking_outcomes() {
    assert!(parse_last_exit(EXIT).unwrap().clean);
    for parking in ["restored", "nothing_parked", "failed", "none"] {
        for journals in [false, true] {
            for audio in [false, true] {
                let mut v: Value = serde_json::from_slice(EXIT).unwrap();
                v["parking"] = json!(parking);
                v["input_journals_empty"] = json!(journals);
                v["audio_stopped"] = json!(audio);
                let clean = parking != "failed" && journals && audio;
                v["clean"] = json!(clean);
                assert_eq!(parse_last_exit(&bytes(&v)).unwrap().clean, clean);
                v["clean"] = json!(!clean);
                assert_eq!(
                    parse_last_exit(&bytes(&v)),
                    Err(ContractError::InvalidValue)
                );
            }
        }
    }
}

#[test]
fn erase_items_preserve_keep_trust_and_never_equate_stdout_with_deletion() {
    assert!(
        parse_erase_identity(ERASE)
            .unwrap()
            .identity_and_pairings_removed()
    );
    for (result, key, trust, complete) in [
        ("removed", "removed", "kept", false),
        ("already_absent", "absent", "kept", false),
        ("already_absent", "absent", "absent", true),
        ("removed", "absent", "removed", true),
    ] {
        let v = json!({"schema_version":1,"result":result,"reason":null,"key":key,"trust":trust});
        assert_eq!(
            parse_erase_identity(&bytes(&v))
                .unwrap()
                .identity_and_pairings_removed(),
            complete
        );
    }
    for (result, reason, key, trust) in [
        ("refused", "agent_running", "kept", "kept"),
        ("refused", "no_exit_receipt", "kept", "kept"),
        ("refused", "unclean_exit", "kept", "kept"),
        ("waiting", "keystore_locked", "kept", "kept"),
        ("failed", "keystore_error", "failed", "kept"),
        ("failed", "io", "removed", "failed"),
    ] {
        let v = json!({"schema_version":1,"result":result,"reason":reason,"key":key,"trust":trust});
        let receipt = parse_erase_identity(&bytes(&v)).unwrap();
        assert!(!receipt.identity_and_pairings_removed());
        assert!(receipt.reason.is_some());
    }
    for (result, key, trust) in [
        ("removed", "absent", "absent"),
        ("already_absent", "removed", "absent"),
        ("removed", "failed", "removed"),
        ("removed", "kept", "removed"),
    ] {
        let v = json!({"schema_version":1,"result":result,"reason":null,"key":key,"trust":trust});
        assert_eq!(
            parse_erase_identity(&bytes(&v)),
            Err(ContractError::InvalidValue)
        );
    }
}

#[test]
fn lifecycle_unknown_versions_enums_and_all_fields_never_default() {
    let mut v: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
    v["schema_version"] = json!(2);
    assert_eq!(
        parse_bootstrap(&bytes(&v)),
        Err(ContractError::InvalidValue)
    );
    v = serde_json::from_slice(EXIT).unwrap();
    v["schema_version"] = json!(2);
    assert_eq!(
        parse_last_exit(&bytes(&v)),
        Err(ContractError::InvalidValue)
    );
    v = serde_json::from_slice(ERASE).unwrap();
    v["schema_version"] = json!(2);
    assert_eq!(
        parse_erase_identity(&bytes(&v)),
        Err(ContractError::InvalidValue)
    );
    for raw in [BOOTSTRAP, EXIT, ERASE] {
        let original: Value = serde_json::from_slice(raw).unwrap();
        for field in original.as_object().unwrap().keys() {
            let mut v = original.clone();
            v.as_object_mut().unwrap().remove(field);
            if raw == BOOTSTRAP {
                assert!(parse_bootstrap(&bytes(&v)).is_err(), "{field}");
            }
            if raw == EXIT {
                assert!(parse_last_exit(&bytes(&v)).is_err(), "{field}");
            }
            if raw == ERASE {
                assert!(parse_erase_identity(&bytes(&v)).is_err(), "{field}");
            }
        }
    }
    let mut v: Value = serde_json::from_slice(ERASE).unwrap();
    v["result"] = json!("future");
    assert!(parse_erase_identity(&bytes(&v)).is_err());
    v = serde_json::from_slice(EXIT).unwrap();
    v["parking"] = json!("future");
    assert!(parse_last_exit(&bytes(&v)).is_err());
}

#[test]
fn bounded_agent_port_enqueues_drains_and_retains_receipt_metadata() {
    let mut port = AgentQueue::default();
    port.submit(AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 1000,
    })
    .unwrap();
    assert_eq!(port.take_calls().len(), 1);
    assert!(port.take_calls().is_empty());
    port.push_reply(AgentReply {
        id: 1,
        observed_at_ms: 12,
        source: ObservationSource::Demo,
        result: decode_reply(&InstallerRequest::Status, STATUS, AgentPlatform::Linux),
    })
    .unwrap();
    let replies = port.poll();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].observed_at_ms, 12);
    assert_eq!(replies[0].source, ObservationSource::Demo);
    assert!(port.poll().is_empty());
    assert_eq!(
        port.push_reply(replies[0].clone()),
        Err(ContractError::InvalidValue)
    );
}

#[test]
fn bounded_agent_port_rejects_deadlines_reused_ids_exhaustion_and_full_queues() {
    let call = |id, timeout_ms| AgentCall {
        id,
        request: InstallerRequest::Status,
        timeout_ms,
    };
    let response = |id| AgentReply {
        id,
        observed_at_ms: 1,
        source: ObservationSource::Demo,
        result: Err(CallFailure::TimeoutOutcomeUnknown),
    };
    let mut port = AgentQueue::default();
    for timeout in [0, 5001, u64::MAX] {
        assert_eq!(
            port.submit(call(1, timeout)),
            Err(CallFailure::InvalidCall(ContractError::InvalidDeadline))
        );
    }
    port.submit(call(1, 1)).unwrap();
    assert_eq!(
        port.submit(call(1, 1)),
        Err(CallFailure::InvalidCall(ContractError::InvalidValue))
    );
    for id in 2..=32 {
        port.submit(call(id, 5000)).unwrap();
    }
    assert_eq!(port.submit(call(33, 1)), Err(CallFailure::QueueFull));
    assert_eq!(port.take_calls().len(), 32);
    for id in 1..=32 {
        port.push_reply(response(id)).unwrap();
    }
    port.submit(call(33, 1)).unwrap();
    assert_eq!(port.push_reply(response(33)), Err(ContractError::QueueFull));
    assert_eq!(port.poll().len(), 32);
    port.push_reply(response(33)).unwrap();
    assert_eq!(
        port.poll()[0].result,
        Err(CallFailure::TimeoutOutcomeUnknown)
    );
    port.submit(call(u64::MAX, 1)).unwrap();
    assert_eq!(
        port.submit(call(u64::MAX, 1)),
        Err(CallFailure::InvalidCall(ContractError::IdExhausted))
    );
}

#[test]
fn debug_is_redacted_for_pairing_windows_health_calls_and_replies() {
    let DecodedReply::PairStatus(pairing) = reply(&InstallerRequest::PairStatus,json!({"phase":"confirm",
        "sas":"SECRET_SAS","candidates":["SECRET_CANDIDATE"],"peer":"SECRET_PEER","error":"SECRET_ERROR"})).unwrap() else { panic!() };
    let DecodedReply::Windows(windows) = reply(
        &InstallerRequest::Windows,
        json!([{"id":1,"app":"SECRET_APP",
        "title":"SECRET_TITLE","display":null,"size":[1,1]}]),
    )
    .unwrap() else {
        panic!()
    };
    let call = AgentCall {
        id: 1,
        request: InstallerRequest::Status,
        timeout_ms: 1,
    };
    let response = AgentReply {
        id: 1,
        observed_at_ms: 1,
        source: ObservationSource::Demo,
        result: Ok(DecodedReply::PairStatus(pairing.clone())),
    };
    let debug = format!(
        "{pairing:?} {windows:?} {:?} {call:?} {response:?}",
        health(&fixture())
    );
    for secret in [
        "SECRET_SAS",
        "SECRET_CANDIDATE",
        "SECRET_PEER",
        "SECRET_ERROR",
        "SECRET_APP",
        "SECRET_TITLE",
        "/home/u",
    ] {
        assert!(!debug.contains(secret));
    }
    assert_eq!(
        format!("{:?}", CallFailure::Refused(AgentRefusal::Other)),
        "Refused(Other)"
    );
}

#[test]
fn remaining_literal_enums_and_global_audio_membership_are_preserved() {
    for (field, cases) in [
        ("/result/installer/keystore", vec!["os_store", "file"]),
        (
            "/result/installer/gate/session",
            vec!["unlocked", "locked", "unknown"],
        ),
        (
            "/result/installer/peers/0/last_source_parking",
            vec!["twin", "mirror"],
        ),
        (
            "/result/installer/discovery/error",
            vec!["daemon_failed", "browse_failed", "unknown"],
        ),
    ] {
        for case in cases {
            let mut v = fixture();
            *v.pointer_mut(field).unwrap() = json!(case);
            assert!(matches!(
                admit(&v, AgentPlatform::Linux),
                Ok(StatusAdmission::Supported(_))
            ));
        }
    }
    let mut v = fixture();
    v["result"]["installer"]["audio"]["active_peers"] =
        json!([peer().to_string(), NodeId([0x33; 32]).to_string()]);
    assert_eq!(
        health(&v).installer().audio.active_peers,
        vec![peer(), NodeId([0x33; 32])]
    );
    v["result"]["installer"]["audio"]["active_peers"]
        .as_array_mut()
        .unwrap()
        .reverse();
    assert_eq!(
        admit(&v, AgentPlatform::Linux),
        Err(ContractError::InvalidValue)
    );
}

#[test]
fn list_and_line_exact_boundaries_and_duplicate_window_handles() {
    let result: Vec<_> = (0..128)
        .map(|id| json!({"id":id,"app":"test","title":"test","display":null,"size":[1.5,2.5]}))
        .collect();
    let DecodedReply::Windows(w) = reply(&InstallerRequest::Windows, json!(result)).unwrap() else {
        panic!()
    };
    assert_eq!(w.len(), 128);
    let duplicate = json!([{"id":1,"app":"test","title":"test","display":null,"size":[1,2]},
        {"id":1,"app":"test","title":"test","display":null,"size":[1,2]}]);
    assert_eq!(
        reply(&InstallerRequest::Windows, duplicate),
        Err(CallFailure::InvalidResponse)
    );
    let mut raw = STATUS.to_vec();
    raw.resize(MAX_RESPONSE_BYTES, b' ');
    assert!(matches!(
        parse_status(&raw, AgentPlatform::Linux),
        Ok(StatusAdmission::Supported(_))
    ));
    raw.push(b' ');
    assert_eq!(
        parse_status(&raw, AgentPlatform::Linux),
        Err(ContractError::Oversize)
    );
}

#[test]
fn structs_require_objects_and_enums_require_strings_everywhere() {
    assert!(parse_erase_identity(br#"[1,"removed",null,"removed","removed"]"#).is_err());
    assert!(parse_bootstrap(br#"[1,1234567890123,4242,1790950000000,"ready",2,null,null,"/run/user/1000/crosspane"]"#).is_err());
    assert!(
        parse_last_exit(br#"[1,1234567890123,1790950100000,true,"restored",true,true]"#).is_err()
    );
    assert_eq!(
        reply(
            &InstallerRequest::PairStatus,
            json!({"phase":{"paired":null},"sas":null,
        "candidates":[],"peer":null,"error":null})
        ),
        Err(CallFailure::InvalidResponse)
    );
    assert_eq!(
        reply(
            &InstallerRequest::PairStatus,
            json!(["paired", null, [], null, null])
        ),
        Err(CallFailure::InvalidResponse)
    );
    let settings = InstallerRequest::SettingsUpdate {
        expected_revision: "0123456789abcdef".into(),
        mac_virtual_display: true,
    };
    assert_eq!(
        reply(&settings, json!(["0123456789abcdef", true])),
        Err(CallFailure::InvalidResponse)
    );
    for (path, value) in [
        ("/result/installer/build", json!(["0.0.0", ["video"]])),
        (
            "/result/installer/gate",
            json!([true, "unlocked", true, true, false]),
        ),
        ("/result/installer/epochs", json!([7, 3, 2, 1])),
        (
            "/result/installer/backends/0",
            json!(["capture", "ready", null]),
        ),
        ("/result/installer/backends/0/state", json!({"ready":null})),
        ("/result/installer/keystore", json!({"os_store":null})),
        ("/result/installer/discovery", json!([true, true, 1, null])),
        ("/result/installer/tray", json!([true])),
        ("/result/installer/audio", json!([true, [], 14, 15])),
        (
            "/result/installer/peers/0/counters",
            json!([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, null, 13]),
        ),
        (
            "/result/displays/0",
            json!([1, "local", [1920, 1080], 1, [500, 300], [0, 0]]),
        ),
    ] {
        let mut v = fixture();
        *v.pointer_mut(path).unwrap() = value;
        assert!(admit(&v, AgentPlatform::Linux).is_err(), "{path}");
    }
    let mut v = fixture();
    mac(&mut v, true);
    v["result"]["installer"]["permissions"][0]["state"] = json!({"granted":null});
    assert!(admit(&v, AgentPlatform::Macos).is_err());
    for (raw, field) in [(BOOTSTRAP, "phase"), (EXIT, "parking"), (ERASE, "result")] {
        let mut v: Value = serde_json::from_slice(raw).unwrap();
        let name = v[field].as_str().unwrap().to_owned();
        v[field] = json!({name:null});
        if raw == BOOTSTRAP {
            assert!(parse_bootstrap(&bytes(&v)).is_err());
        }
        if raw == EXIT {
            assert!(parse_last_exit(&bytes(&v)).is_err());
        }
        if raw == ERASE {
            assert!(parse_erase_identity(&bytes(&v)).is_err());
        }
    }
}

#[test]
fn trusted_offline_peers_need_no_legacy_hello_cache_entry() {
    let mut cold = fixture();
    cold["result"]["installer"]["peers"][0]["connected"] = json!(false);
    cold["result"]["installer"]["peers"][0]["link_generation"] = Value::Null;
    cold["result"]["peers"] = json!([]);
    cold["result"]["layout"].as_array_mut().unwrap().pop();
    let h = health(&cold);
    assert_eq!(h.installer().peers.len(), 1);
    assert!(h.display_layout().peer_displays.is_empty());
    assert_eq!(
        counter_sample(&h, Some(peer()), ObservationSource::Live, 1),
        Err(ContractError::DisconnectedPeer)
    );

    let mut mixed = fixture();
    let offline = NodeId([0x33; 32]);
    let mut trust = mixed["result"]["installer"]["peers"][0].clone();
    trust["node"] = json!(offline.to_string());
    trust["connected"] = json!(false);
    trust["link_generation"] = Value::Null;
    mixed["result"]["installer"]["peers"]
        .as_array_mut()
        .unwrap()
        .push(trust);
    let h = health(&mixed);
    assert_eq!(h.installer().peers.len(), 2);
    assert_eq!(h.display_layout().peer_displays.len(), 1);
    assert!(counter_sample(&h, Some(peer()), ObservationSource::Live, 2).is_ok());
    mixed["result"]["layout"]
        .as_array_mut()
        .unwrap()
        .push(json!({
        "node":offline.short(),"display":99,"origin_mm":[1000,0],"version":20}));
    expect_pending(&mixed, PendingHealthReason::MissingDisplay);
}

#[test]
fn every_required_legacy_field_missing_is_pending_but_wrong_types_are_errors() {
    let mut original = fixture();
    original["result"]["projections"] = serde_json::from_slice(
        br#"[{"source":"2222222222222222",
        "projection":99,"text":"projected test window","received":{"frames":4,"bytes":800,
        "last_ms_ago":5,"latency_ms":12.5}}]"#,
    )
    .unwrap();
    for (path, fields) in [
        (
            "/result",
            vec![
                "controlling",
                "controlled_by",
                "projections",
                "displays",
                "peers",
                "layout",
            ],
        ),
        (
            "/result/projections/0",
            vec!["source", "projection", "text", "received"],
        ),
        (
            "/result/displays/0",
            vec!["id", "name", "pixels", "scale", "mm", "origin"],
        ),
        ("/result/peers/0", vec!["node", "displays"]),
        (
            "/result/peers/0/displays/0",
            vec!["id", "name", "pixels", "scale", "mm", "origin"],
        ),
        (
            "/result/layout/0",
            vec!["node", "display", "origin_mm", "version"],
        ),
    ] {
        for field in fields {
            let mut v = original.clone();
            v.pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            expect_pending(&v, PendingHealthReason::Incomplete);
            assert_eq!(
                decode_reply(&InstallerRequest::Status, &bytes(&v), AgentPlatform::Linux),
                Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete)
                ))
            );
        }
    }
    for path in [
        "/result/layout/0/node",
        "/result/layout/0/display",
        "/result/layout/0/origin_mm",
        "/result/layout/0/version",
        "/result/displays/0/name",
        "/result/peers/0/displays/0/scale",
        "/result/peers/0/displays",
        "/result/projections/0/source",
        "/result/projections/0/projection",
        "/result/projections/0/text",
    ] {
        let mut v = original.clone();
        *v.pointer_mut(path).unwrap() = Value::Null;
        assert!(admit(&v, AgentPlatform::Linux).is_err(), "{path}");
    }
    original["result"]["projections"][0]["received"] = json!(4);
    assert_eq!(
        admit(&original, AgentPlatform::Linux),
        Err(ContractError::WrongType)
    );
}

#[test]
fn health_enums_and_every_safety_boolean_preserve_exact_readiness_facts() {
    for (state, expected_state) in [
        ("ready", BackendState::Ready),
        ("missing", BackendState::Missing),
        ("blocked", BackendState::Blocked),
        ("failed", BackendState::Failed),
    ] {
        for (reason, expected_reason) in [
            ("not_supported", BackendReason::NotSupported),
            ("permission", BackendReason::Permission),
            ("construction_failed", BackendReason::ConstructionFailed),
            ("worker_exited", BackendReason::WorkerExited),
            ("disabled", BackendReason::Disabled),
            ("unknown", BackendReason::Unknown),
        ] {
            let mut v = fixture();
            v["result"]["installer"]["backends"][0]["state"] = json!(state);
            v["result"]["installer"]["backends"][0]["reason"] = if state == "ready" {
                Value::Null
            } else {
                json!(reason)
            };
            let h = health(&v);
            assert_eq!(h.installer().backends[0].state, expected_state);
            assert_eq!(
                h.installer().backends[0].reason,
                if state == "ready" {
                    None
                } else {
                    Some(expected_reason)
                }
            );
        }
    }
    assert_eq!(
        health(&fixture())
            .installer()
            .backends
            .iter()
            .map(|b| b.name)
            .collect::<Vec<_>>(),
        vec![
            BackendName::Capture,
            BackendName::Keys,
            BackendName::Pointer,
            BackendName::Overlay,
            BackendName::Hotkeys,
            BackendName::Keystore,
            BackendName::Windows,
            BackendName::Parking,
            BackendName::Frames,
            BackendName::Tray,
            BackendName::Links,
            BackendName::Gpu,
            BackendName::Home,
            BackendName::Audio,
            BackendName::Discovery
        ]
    );
    for (name, expected) in [
        ("os_store", KeyStoreProvenance::OsStore),
        ("file", KeyStoreProvenance::File),
    ] {
        let mut v = fixture();
        v["result"]["installer"]["keystore"] = json!(name);
        assert_eq!(health(&v).installer().keystore, expected);
    }
    for (state, expected) in [
        ("granted", PermissionState::Granted),
        ("not_granted", PermissionState::NotGranted),
        ("unknown", PermissionState::Unknown),
    ] {
        let mut v = fixture();
        mac(&mut v, true);
        for p in v["result"]["installer"]["permissions"]
            .as_array_mut()
            .unwrap()
        {
            p["state"] = json!(state);
        }
        let StatusAdmission::Supported(h) = admit(&v, AgentPlatform::Macos).unwrap() else {
            panic!()
        };
        assert_eq!(
            h.installer()
                .permissions
                .iter()
                .map(|p| p.state)
                .collect::<Vec<_>>(),
            vec![expected; 4]
        );
        assert_eq!(
            h.installer()
                .permissions
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>(),
            vec![
                PermissionName::InputMonitoring,
                PermissionName::ScreenRecording,
                PermissionName::Accessibility,
                PermissionName::Microphone
            ]
        );
    }
    for value in [false, true] {
        let mut v = fixture();
        for field in ["open", "armed", "active", "panic"] {
            v["result"]["installer"]["gate"][field] = json!(value);
        }
        for field in ["enabled", "running"] {
            v["result"]["installer"]["discovery"][field] = json!(value);
        }
        v["result"]["installer"]["tray"]["created"] = json!(value);
        v["result"]["installer"]["audio"]["enabled"] = json!(value);
        v["result"]["installer"]["peers"][0]["connected"] = json!(value);
        let h = health(&v);
        let i = h.installer();
        assert_eq!(
            (i.gate.open, i.gate.armed, i.gate.active, i.gate.panic),
            (value, value, Some(value), value)
        );
        assert_eq!(
            (
                i.discovery.enabled,
                i.discovery.running,
                i.tray.created,
                i.audio.enabled,
                i.peers[0].connected
            ),
            (value, value, value, value, value)
        );
    }
}

#[test]
fn every_metric_discriminant_and_both_scope_maps_are_frozen_with_large_values() {
    for (metric, id) in [
        (Metric::ControllerStarted, 0),
        (Metric::ControllerEnded, 1),
        (Metric::TargetStarted, 2),
        (Metric::TargetEnded, 3),
        (Metric::InjectionsOk, 4),
        (Metric::HudShows, 5),
        (Metric::ChordReleases, 6),
        (Metric::CommandReleases, 7),
        (Metric::SourceStarted, 8),
        (Metric::SourceReturned, 9),
        (Metric::DestStarted, 10),
        (Metric::DestReturned, 11),
        (Metric::FramesPresented, 12),
        (Metric::ReturnsFailed, 13),
        (Metric::GlobalAudioSent, 14),
        (Metric::GlobalAudioPlayed, 15),
        (Metric::SettingsOpened, 16),
    ] {
        assert_eq!(metric as u16, id);
    }
    let mut v = fixture();
    let second = NodeId([0x33; 32]);
    let mut p = v["result"]["installer"]["peers"][0].clone();
    p["node"] = json!(second.to_string());
    p["link_generation"] = json!(5);
    v["result"]["installer"]["peers"]
        .as_array_mut()
        .unwrap()
        .push(p);
    let names = [
        "e1_controller_started",
        "e1_controller_ended",
        "e1_target_started",
        "e1_target_ended",
        "e1_injections_ok",
        "e1_hud_shows",
        "e1_chord_releases",
        "e1_command_releases",
        "e2_source_started",
        "e2_source_returned",
        "e2_dest_started",
        "e2_dest_returned",
        "e2_frames_presented",
        "e2_returns_failed",
    ];
    for (idx, base) in [(0, 1000_u64), (1, 2000_u64)] {
        for (metric, name) in names.iter().enumerate() {
            v["result"]["installer"]["peers"][idx]["counters"][name] = json!(base + metric as u64);
        }
    }
    v["result"]["installer"]["audio"]["frames_sent"] = json!(u64::MAX);
    v["result"]["installer"]["audio"]["frames_played"] = json!(u64::MAX - 1);
    v["result"]["installer"]["settings_opened"] = json!(u64::MAX - 2);
    let globals = BTreeMap::from([
        (CounterId(14), u64::MAX),
        (CounterId(15), u64::MAX - 1),
        (CounterId(16), u64::MAX - 2),
    ]);
    assert_eq!(
        counter_sample(&health(&v), None, ObservationSource::Live, 1)
            .unwrap()
            .sample
            .values,
        globals
    );
    for frames in [9, u64::MAX] {
        for (idx, selected, base, link) in [(0, peer(), 1000_u64, 3), (1, second, 2000_u64, 5)] {
            v["result"]["installer"]["peers"][idx]["counters"]["e2_frames_presented"] =
                json!(frames);
            let sample =
                counter_sample(&health(&v), Some(selected), ObservationSource::Live, 2).unwrap();
            let mut expected = globals.clone();
            for id in 0_u16..14 {
                expected.insert(CounterId(id), base + u64::from(id));
            }
            expected.insert(CounterId(12), frames);
            assert_eq!(sample.sample.values, expected);
            assert!(sample.unavailable.is_empty());
            assert_eq!(sample.sample.binding.peer, Some(selected));
            assert_eq!(sample.sample.binding.link_generation, Some(link));
        }
    }
}

#[test]
fn literal_decoder_statistics_never_become_presented_frame_evidence() {
    let projection: Value = serde_json::from_slice(br#"[{"source":"2222222222222222","projection":99,
        "text":"projected test window","received":{"frames":40,"bytes":8000,"last_ms_ago":2,"latency_ms":12.5}}]"#).unwrap();
    let mut v = fixture();
    v["result"]["projections"] = projection;
    let start = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 1).unwrap();
    assert_eq!(start.unavailable, vec![Metric::FramesPresented]);
    assert!(!start.sample.values.contains_key(&CounterId(12)));
    v["result"]["projections"][0]["received"]["frames"] = json!(u64::MAX);
    v["result"]["projections"][0]["received"]["bytes"] = json!(u64::MAX);
    v["result"]["installer"]["peers"][0]["counters"]["e2_dest_started"] = json!(20);
    let end = counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 2).unwrap();
    assert_eq!(end.unavailable, vec![Metric::FramesPresented]);
    assert!(!end.sample.values.contains_key(&CounterId(12)));
    assert_eq!(
        check_activity(
            AttemptId(1),
            &start.sample,
            &end.sample,
            EpochDependencies {
                gate: true,
                grants: true,
                layout: true,
                backends: true
            },
            &[CounterId(12)],
            &[]
        ),
        Err(EvidenceError::MissingCounter)
    );
    v["result"]["projections"][0]["received"] = Value::Null;
    assert_eq!(
        counter_sample(&health(&v), Some(peer()), ObservationSource::Live, 3)
            .unwrap()
            .unavailable,
        vec![Metric::FramesPresented]
    );
}

#[test]
fn explicit_json_depth_accepts_shallow_and_limit_but_rejects_excessive_nesting() {
    fn nest(depth: usize, objects: bool) -> Value {
        (0..depth).fold(json!(0), |child, _| {
            if objects {
                json!({"child":child})
            } else {
                json!([child])
            }
        })
    }
    for objects in [false, true] {
        for depth in [3, MAX_JSON_DEPTH - 1] {
            assert_eq!(
                reply(&InstallerRequest::Panic, nest(depth, objects)),
                Ok(DecodedReply::Acknowledged)
            );
        }
        assert_eq!(
            reply(&InstallerRequest::Panic, nest(MAX_JSON_DEPTH, objects)),
            Err(CallFailure::InvalidResponse)
        );
        let mut v = fixture();
        v["result"]["ignored"] = nest(MAX_JSON_DEPTH - 2, objects);
        assert!(matches!(
            admit(&v, AgentPlatform::Linux),
            Ok(StatusAdmission::Supported(_))
        ));
        v["result"]["ignored"] = nest(MAX_JSON_DEPTH - 1, objects);
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::InvalidJson)
        );
        v["result"]["ignored"] = nest(256, objects);
        assert_eq!(
            admit(&v, AgentPlatform::Linux),
            Err(ContractError::InvalidJson)
        );
    }
}

#[test]
fn lifecycle_golden_values_and_scalar_boundaries_preserve_opaque_full_range_ids() {
    assert_eq!(
        parse_bootstrap(BOOTSTRAP).unwrap(),
        BootstrapV1 {
            schema_version: 1,
            instance_id: 1234567890123,
            pid: 4242,
            started_unix_ms: 1790950000000,
            phase: BootstrapPhase::WaitingForKeystore,
            phase_seq: 2,
            keystore: None,
            reason: None,
            runtime_dir: "/run/user/1000/crosspane".into()
        }
    );
    assert_eq!(
        parse_last_exit(EXIT).unwrap(),
        LastExitV1 {
            schema_version: 1,
            instance_id: 1234567890123,
            stopped_unix_ms: 1790950100000,
            clean: true,
            parking: ParkingExit::Restored,
            input_journals_empty: true,
            audio_stopped: true
        }
    );
    for id in [0, u64::MAX] {
        let mut b: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        b["instance_id"] = json!(id);
        b["pid"] = json!(u32::MAX);
        b["phase_seq"] = json!(u64::MAX);
        b["started_unix_ms"] = json!(u64::MAX);
        let parsed = parse_bootstrap(&bytes(&b)).unwrap();
        assert_eq!(
            (
                parsed.instance_id,
                parsed.pid,
                parsed.phase_seq,
                parsed.started_unix_ms
            ),
            (id, u32::MAX, u64::MAX, u64::MAX)
        );
        let mut e: Value = serde_json::from_slice(EXIT).unwrap();
        e["instance_id"] = json!(id);
        e["stopped_unix_ms"] = json!(u64::MAX);
        assert_eq!(parse_last_exit(&bytes(&e)).unwrap().instance_id, id);
        assert_eq!(
            parse_last_exit(&bytes(&e)).unwrap().stopped_unix_ms,
            u64::MAX
        );
    }
    let overflow: Value = serde_json::from_str("18446744073709551616").unwrap();
    for bad in [
        json!(-1),
        json!(1.5),
        json!(true),
        json!("1"),
        Value::Null,
        overflow,
    ] {
        for field in ["instance_id", "started_unix_ms", "phase_seq"] {
            let mut b: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
            b[field] = bad.clone();
            assert!(
                parse_bootstrap(&bytes(&b)).is_err(),
                "bootstrap {field}: {bad}"
            );
        }
        for field in ["instance_id", "stopped_unix_ms"] {
            let mut e: Value = serde_json::from_slice(EXIT).unwrap();
            e[field] = bad.clone();
            assert!(parse_last_exit(&bytes(&e)).is_err(), "exit {field}: {bad}");
        }
    }
    for bad in [json!(-1), json!(1.5), json!(4294967296_u64), json!(true)] {
        let mut b: Value = serde_json::from_slice(BOOTSTRAP).unwrap();
        b["pid"] = bad;
        assert!(parse_bootstrap(&bytes(&b)).is_err());
    }
    for bad in [json!(0), json!(1), json!("true"), Value::Null] {
        for field in ["clean", "input_journals_empty", "audio_stopped"] {
            let mut e: Value = serde_json::from_slice(EXIT).unwrap();
            e[field] = bad.clone();
            assert!(parse_last_exit(&bytes(&e)).is_err());
        }
    }
}

#[test]
fn directly_exposed_text_types_redact_sensitive_sentinels() {
    let remote = RemoteWindow {
        id: WindowId(1),
        app: "REMOTE_APP_SENTINEL".into(),
        title: "REMOTE_TITLE_SENTINEL".into(),
        size: [1, 2],
    };
    let candidate = PairCandidate {
        name: "PAIR_NAME_SENTINEL".into(),
        addr: "192.0.2.1:47811".parse().unwrap(),
    };
    let mut p = health(&fixture()).installer().peers[0].clone();
    p.name = "PEER_NAME_SENTINEL".into();
    p.features = vec!["PEER_FEATURE_SENTINEL".into()];
    let mut display = health(&fixture()).display_layout().local_displays[0].clone();
    display.name = "DISPLAY_NAME_SENTINEL".into();
    let debug = format!("{remote:?} {candidate:?} {p:?} {display:?}");
    for sentinel in [
        "REMOTE_APP_SENTINEL",
        "REMOTE_TITLE_SENTINEL",
        "PAIR_NAME_SENTINEL",
        "PEER_NAME_SENTINEL",
        "PEER_FEATURE_SENTINEL",
        "DISPLAY_NAME_SENTINEL",
    ] {
        assert!(!debug.contains(sentinel));
    }
}
