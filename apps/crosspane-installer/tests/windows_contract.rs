#![allow(clippy::unwrap_used)]

use crosspane_installer::agent_contract::*;
use serde_json::{Value, json};

// Literal producer-shaped bytes: parsing these never constructs an OS port or reads user data.
const STATUS: &[u8] = br#"{"ok":true,"result":{
 "controlling":null,"controlled_by":null,"projections":[],
 "displays":[{"id":1,"name":"local","pixels":[1920,1080],"scale":1.25,"mm":[500.5,300.5],"origin":[-10.5,0]}],
 "peers":[{"node":"2222222222222222222222222222222222222222222222222222222222222222","name":"remote",
 "connected":true,"displays":[{"id":2,"name":"remote","pixels":[3840,2160],"scale":2,"mm":[600,400],"origin":[0,0]}]}],
 "layout":[{"node":"1111111111111111","display":1,"origin_mm":[0,0],"version":17},
 {"node":"2222222222222222","display":2,"origin_mm":[500.5,-1.5],"version":19}],
 "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
 "instance":{"id":18446744073709551615,"pid":4242,"uid":null,"exe":"C:\\fixture\\crosspane-agent.exe",
 "runtime_dir":"C:\\fixture\\runtime","started_unix_ms":1790950000000},
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
 "connected":true,"link_generation":3,"features":["e1"],"grants_given":["browse","input"],
 "last_source_parking":null,"counters":{
 "e1_controller_started":0,"e1_controller_ended":1,"e1_target_started":2,"e1_target_ended":3,
 "e1_injections_ok":4,"e1_hud_shows":5,"e1_chord_releases":6,"e1_command_releases":7,
 "e2_source_started":8,"e2_source_returned":9,"e2_dest_started":10,"e2_dest_returned":11,
 "e2_frames_presented":null,"e2_returns_failed":13}}]}}}"#;

fn fixture() -> Value {
    serde_json::from_slice(STATUS).unwrap()
}
fn windows() -> AgentPlatform {
    serde_json::from_value(json!("windows")).unwrap()
}
fn parse(value: &Value, platform: AgentPlatform) -> Result<StatusAdmission, ContractError> {
    parse_status(&serde_json::to_vec(value).unwrap(), platform)
}
fn supported(value: &Value, platform: AgentPlatform) -> Box<HealthSnapshot> {
    match parse(value, platform).unwrap() {
        StatusAdmission::Supported(health) => health,
        other => panic!("expected supported fixture: {other:?}"),
    }
}
fn mac_permissions(value: &mut Value) {
    value["result"]["installer"]["permissions"] = json!([
        {"name":"input_monitoring","state":"granted"},
        {"name":"screen_recording","state":"granted"},
        {"name":"accessibility","state":"granted"}
    ]);
}

#[test]
fn instance_uid_accepts_explicit_null_but_never_omission() {
    let mut value = fixture()["result"]["installer"]["instance"].clone();
    let instance: InstanceStatus = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(instance.uid).unwrap(), Value::Null);
    value.as_object_mut().unwrap().remove("uid");
    assert!(serde_json::from_value::<InstanceStatus>(value).is_err());
}

#[test]
fn observed_clipboard_names_are_dotted_and_not_tutorial_grants() {
    for token in ["clipboard.read", "clipboard.write"] {
        let capability: Capability = serde_json::from_value(json!(token)).unwrap();
        assert_eq!(serde_json::to_value(capability).unwrap(), json!(token));
        assert!(serde_json::from_value::<GrantableCapability>(json!(token)).is_err());
    }
}

#[test]
fn windows_status_requires_an_explicit_null_uid() {
    let value = fixture();
    let health = supported(&value, windows());
    assert_eq!(
        serde_json::to_value(health.installer().instance.uid).unwrap(),
        Value::Null
    );
    // Decode/serialize is schema admission only; native SID/logon/process/pipe proof is separate.
    let instance = serde_json::to_value(&health.installer().instance).unwrap();
    assert!(instance.as_object().unwrap().contains_key("uid"));
    assert!(instance["uid"].is_null());
}

#[test]
fn missing_uid_is_incomplete_for_every_platform() {
    for platform in [windows(), AgentPlatform::Linux, AgentPlatform::Macos] {
        let mut value = fixture();
        value["result"]["installer"]["instance"]
            .as_object_mut()
            .unwrap()
            .remove("uid");
        assert_eq!(
            parse(&value, platform),
            Ok(StatusAdmission::PendingHealthContract(
                PendingHealthReason::Incomplete
            ))
        );
        assert!(
            serde_json::from_value::<InstanceStatus>(
                value["result"]["installer"]["instance"].clone()
            )
            .is_err()
        );
    }
}

#[test]
fn windows_rejects_numeric_uid_and_unix_rejects_null_uid() {
    let mut value = fixture();
    for uid in [0, 1000, u32::MAX] {
        value["result"]["installer"]["instance"]["uid"] = json!(uid);
        assert_eq!(parse(&value, windows()), Err(ContractError::InvalidValue));
    }
    value["result"]["installer"]["instance"]["uid"] = Value::Null;
    for platform in [AgentPlatform::Linux, AgentPlatform::Macos] {
        assert_eq!(parse(&value, platform), Err(ContractError::InvalidValue));
    }
}

#[test]
fn unix_uid_and_permission_rules_remain_strict() {
    let mut value = fixture();
    value["result"]["installer"]["instance"]["uid"] = json!(1000);
    assert_eq!(
        serde_json::to_value(
            supported(&value, AgentPlatform::Linux)
                .installer()
                .instance
                .uid
        )
        .unwrap(),
        json!(1000)
    );
    mac_permissions(&mut value);
    assert_eq!(
        serde_json::to_value(
            supported(&value, AgentPlatform::Macos)
                .installer()
                .instance
                .uid
        )
        .unwrap(),
        json!(1000)
    );
    assert_eq!(
        parse(&value, AgentPlatform::Linux),
        Ok(StatusAdmission::PendingHealthContract(
            PendingHealthReason::Incomplete
        ))
    );
    value["result"]["installer"]["permissions"] = json!([]);
    assert_eq!(
        parse(&value, AgentPlatform::Macos),
        Ok(StatusAdmission::PendingHealthContract(
            PendingHealthReason::Incomplete
        ))
    );
}

#[test]
fn malformed_uid_is_rejected_before_schema_admission() {
    for uid in [
        json!(-1),
        json!(u64::from(u32::MAX) + 1),
        json!(1.5),
        json!("1000"),
        json!(false),
    ] {
        let mut value = fixture();
        value["result"]["installer"]["instance"]["uid"] = uid;
        for platform in [windows(), AgentPlatform::Linux, AgentPlatform::Macos] {
            assert_eq!(parse(&value, platform), Err(ContractError::WrongType));
        }
    }
}

#[test]
fn windows_requires_empty_permissions() {
    let mut value = fixture();
    supported(&value, windows());
    mac_permissions(&mut value);
    assert_eq!(
        parse(&value, windows()),
        Ok(StatusAdmission::PendingHealthContract(
            PendingHealthReason::Incomplete
        ))
    );
}

#[test]
fn dotted_clipboard_grants_round_trip_without_new_tutorial_choices() {
    let mut value = fixture();
    value["result"]["installer"]["peers"][0]["grants_given"] =
        json!(["browse", "clipboard.read", "clipboard.write", "input"]);
    for platform in [windows(), AgentPlatform::Linux, AgentPlatform::Macos] {
        value["result"]["installer"]["instance"]["uid"] = if platform == windows() {
            Value::Null
        } else {
            json!(1000)
        };
        value["result"]["installer"]["permissions"] = json!([]);
        if platform == AgentPlatform::Macos {
            mac_permissions(&mut value);
        }
        let health = supported(&value, platform);
        assert_eq!(
            serde_json::to_value(&health.installer().peers[0].grants_given).unwrap(),
            json!(["browse", "clipboard.read", "clipboard.write", "input"])
        );
    }
    for token in ["clipboard.read", "clipboard.write"] {
        assert!(serde_json::from_value::<GrantableCapability>(json!(token)).is_err());
    }
}

#[test]
fn clipboard_grants_remain_sorted_unique_and_bounded() {
    for grants in [
        json!(["clipboard.write", "clipboard.read"]),
        json!(["clipboard.read", "clipboard.read"]),
    ] {
        let mut value = fixture();
        value["result"]["installer"]["peers"][0]["grants_given"] = grants;
        assert_eq!(parse(&value, windows()), Err(ContractError::InvalidValue));
    }
    let mut value = fixture();
    value["result"]["installer"]["peers"][0]["grants_given"] =
        json!(vec!["clipboard.read"; MAX_ITEMS + 1]);
    assert_eq!(parse(&value, windows()), Err(ContractError::Oversize));
}

#[test]
fn unrecognized_grants_remain_pending_contract() {
    for token in ["clipboard.future", "clipboard_read", "ClipboardRead"] {
        let mut value = fixture();
        value["result"]["installer"]["peers"][0]["grants_given"] = json!([token]);
        assert_eq!(
            parse(&value, windows()),
            Ok(StatusAdmission::PendingHealthContract(
                PendingHealthReason::UnknownEnum
            ))
        );
    }
}

#[test]
fn windows_platform_keeps_existing_wire_names() {
    for (platform, token) in [
        (AgentPlatform::Linux, "linux"),
        (AgentPlatform::Macos, "macos"),
        (windows(), "windows"),
    ] {
        assert_eq!(serde_json::to_value(platform).unwrap(), json!(token));
        assert_eq!(
            serde_json::from_value::<AgentPlatform>(json!(token)).unwrap(),
            platform
        );
    }
}

#[test]
fn windows_status_keeps_existing_json_bounds_and_envelope() {
    let mut bytes = STATUS.to_vec();
    bytes.extend_from_slice(b"false");
    assert_eq!(
        parse_status(&bytes, windows()),
        Err(ContractError::TrailingData)
    );
    assert_eq!(
        parse_status(&vec![b' '; MAX_RESPONSE_BYTES + 1], windows()),
        Err(ContractError::Oversize)
    );
    let value = json!({"ok":false,"error":"fixture refusal"});
    assert_eq!(parse(&value, windows()), Err(ContractError::WrongEnvelope));
}

#[cfg(not(unix))]
#[test]
fn non_unix_tutorial_is_unavailable_without_opening_font_or_pipes() {
    use crosspane_installer::{
        fixture::FixtureError,
        tutorial_window::{TutorialOptions, run},
    };
    assert_eq!(
        run(TutorialOptions {
            controlled: true,
            font: Some(std::path::PathBuf::from(r"C:\fixture\never-opened.ttf")),
        }),
        Err(FixtureError::Unavailable)
    );
}
