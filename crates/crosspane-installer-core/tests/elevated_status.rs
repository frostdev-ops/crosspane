#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::status::{
    Consent, DeviceStatus, DriverStatus, FirewallStatus, JournalEntry, JournalPhase, STATUS_SCHEMA,
    StatusReport, describe, intent_entry, outcome_entry, verified,
};
use crosspane_installer_core::elevated::{
    AgentProgram, DriverState, ElevatedError, FirewallState, HARDWARE_ID, InstallId, Outcome,
    RuleScope, Verb, VerbName,
};
use crosspane_installer_core::{MutationOutcome, ResourceObservation, ResourceOwnership};
use serde_json::json;

const PROGRAM: &str = r"C:\Users\user\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
const RULE: &str = "Crosspane.Agent.UDP.Private.w41c-vm-1";

const DRIVER_STATES: [DriverState; 6] = [
    DriverState::Absent,
    DriverState::PackageOnly,
    DriverState::DeviceWithoutDriver,
    DriverState::Installed,
    DriverState::Mismatch,
    DriverState::Unavailable,
];

const FIREWALL_STATES: [FirewallState; 5] = [
    FirewallState::Present,
    FirewallState::Missing,
    FirewallState::Mismatch,
    FirewallState::Unavailable,
    FirewallState::NotRequested,
];

fn scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(PROGRAM).unwrap(),
    }
}

fn report(driver: DriverState, firewall: FirewallState) -> StatusReport {
    StatusReport {
        schema: STATUS_SCHEMA,
        elevated: true,
        driver: DriverStatus {
            state: driver,
            packages: Vec::new(),
            devices: Vec::new(),
        },
        firewall: FirewallStatus {
            state: firewall,
            family_count: 0,
            local_rules_apply: None,
        },
    }
}

fn device(instance_id: &str, driver_inf: Option<&str>) -> DeviceStatus {
    DeviceStatus {
        instance_id: instance_id.to_owned(),
        driver_inf: driver_inf.map(str::to_owned),
        problem: Some(0),
        present: true,
    }
}

fn mutating_verbs() -> Vec<Verb> {
    vec![
        Verb::InstallDriver,
        Verb::RemoveDriver,
        Verb::AddFirewall(scope()),
        Verb::RemoveFirewall(scope()),
    ]
}

#[test]
fn verified_is_true_only_for_the_state_each_verb_reaches() {
    assert!(verified(
        &Verb::InstallDriver,
        &report(DriverState::Installed, FirewallState::NotRequested)
    ));
    assert!(verified(
        &Verb::RemoveDriver,
        &report(DriverState::Absent, FirewallState::NotRequested)
    ));
    assert!(verified(
        &Verb::AddFirewall(scope()),
        &report(DriverState::Absent, FirewallState::Present)
    ));
    assert!(verified(
        &Verb::RemoveFirewall(scope()),
        &report(DriverState::Absent, FirewallState::Missing)
    ));
    assert!(verified(
        &Verb::Status(None),
        &report(DriverState::Unavailable, FirewallState::Unavailable)
    ));
    assert!(verified(
        &Verb::Status(Some(scope())),
        &report(DriverState::Mismatch, FirewallState::Mismatch)
    ));

    for driver in DRIVER_STATES {
        for firewall in FIREWALL_STATES {
            let observed = report(driver, firewall);
            assert_eq!(
                verified(&Verb::InstallDriver, &observed),
                driver == DriverState::Installed
            );
            assert_eq!(
                verified(&Verb::RemoveDriver, &observed),
                driver == DriverState::Absent
            );
            assert_eq!(
                verified(&Verb::AddFirewall(scope()), &observed),
                firewall == FirewallState::Present
            );
            assert_eq!(
                verified(&Verb::RemoveFirewall(scope()), &observed),
                firewall == FirewallState::Missing
            );
            assert!(verified(&Verb::Status(None), &observed));
            assert!(verified(&Verb::Status(Some(scope())), &observed));
        }
    }
}

#[test]
fn describe_text_is_frozen_byte_for_byte() {
    assert_eq!(
        describe(&Verb::InstallDriver),
        vec![
            "Add the Crosspane display driver (CrosspaneIdd.inf, publisher Crosspane) to the Windows driver store.",
            "Create one Crosspane virtual display adapter (hardware ID Crosspane\\IddTwinV1). It shows no display until Crosspane needs one.",
        ]
    );
    assert_eq!(
        describe(&Verb::RemoveDriver),
        vec![
            "Remove the Crosspane virtual display adapter (hardware ID Crosspane\\IddTwinV1).",
            "Remove the Crosspane display driver from the Windows driver store.",
        ]
    );
    assert_eq!(
        describe(&Verb::AddFirewall(scope())),
        vec![
            r#"Allow C:\Users\user\AppData\Local\Programs\Crosspane\crosspane-agent.exe to receive UDP traffic from your local subnet on private networks (Windows Defender Firewall rule "Crosspane.Agent.UDP.Private.w41c-vm-1")."#,
        ]
    );
    assert_eq!(
        describe(&Verb::RemoveFirewall(scope())),
        vec![
            r#"Remove the Windows Defender Firewall rule "Crosspane.Agent.UDP.Private.w41c-vm-1"."#
        ]
    );
    assert!(describe(&Verb::Status(None)).is_empty());
    assert!(describe(&Verb::Status(Some(scope()))).is_empty());
    // The driver text names the hardware ID that the helper creates.
    assert!(describe(&Verb::InstallDriver)[1].contains(&format!("hardware ID {HARDWARE_ID})")));
}

#[test]
fn consent_accepts_exact_lines_and_refuses_edits_and_status() {
    for verb in mutating_verbs() {
        let shown = describe(&verb);
        let consent = Consent::presented(&verb, &shown).unwrap();
        assert_eq!(consent.verb(), verb.name());

        for index in 0..shown.len() {
            let mut edited = shown.clone();
            edited[index].push('.');
            assert_eq!(
                Consent::presented(&verb, &edited),
                Err(ElevatedError::Consent),
                "{} accepted an edited line {index}",
                verb.name().as_str()
            );
        }
        assert_eq!(
            Consent::presented(&verb, &shown[..shown.len() - 1]),
            Err(ElevatedError::Consent)
        );
        assert_eq!(Consent::presented(&verb, &[]), Err(ElevatedError::Consent));
        let mut extra = shown.clone();
        extra.push(String::new());
        assert_eq!(
            Consent::presented(&verb, &extra),
            Err(ElevatedError::Consent)
        );
    }

    let install = describe(&Verb::InstallDriver);
    let reordered: Vec<String> = install.iter().rev().cloned().collect();
    assert_eq!(
        Consent::presented(&Verb::InstallDriver, &reordered),
        Err(ElevatedError::Consent)
    );
    assert_eq!(
        Consent::presented(&Verb::RemoveDriver, &install),
        Err(ElevatedError::Consent)
    );

    // `Status` never mutates, so consent is refused even with its own (empty) text.
    assert_eq!(
        Consent::presented(&Verb::Status(None), &describe(&Verb::Status(None))),
        Err(ElevatedError::Consent)
    );
    assert_eq!(
        Consent::presented(&Verb::Status(Some(scope())), &[]),
        Err(ElevatedError::Consent)
    );
}

#[test]
fn validate_enforces_schema_item_bounds_names_and_instance_ids() {
    assert_eq!(
        report(DriverState::Installed, FirewallState::Present).validate(),
        Ok(())
    );

    let mut schema = report(DriverState::Absent, FirewallState::Missing);
    schema.schema = 0;
    assert_eq!(schema.validate(), Err(ElevatedError::Report));
    schema.schema = STATUS_SCHEMA + 1;
    assert_eq!(schema.validate(), Err(ElevatedError::Report));

    let mut packages = report(DriverState::PackageOnly, FirewallState::Missing);
    packages.driver.packages = (1..=16).map(|n| format!("oem{n}.inf")).collect();
    assert_eq!(packages.validate(), Ok(()));
    packages.driver.packages.push("oem17.inf".to_owned());
    assert_eq!(packages.validate(), Err(ElevatedError::Report));

    let mut devices = report(DriverState::DeviceWithoutDriver, FirewallState::Missing);
    devices.driver.devices = (1..=16)
        .map(|n| device(&format!(r"ROOT\DISPLAY\{n:04}"), None))
        .collect();
    assert_eq!(devices.validate(), Ok(()));
    devices
        .driver
        .devices
        .push(device(r"ROOT\DISPLAY\0017", None));
    assert_eq!(devices.validate(), Err(ElevatedError::Report));

    for (name, valid) in [
        ("oem1.inf", true),
        ("OEM7.INF", true),
        ("oem12345.inf", true),
        ("oem123456.inf", false),
        ("oem.inf", false),
        ("foo.inf", false),
        ("oem1.sys", false),
    ] {
        let mut named = report(DriverState::PackageOnly, FirewallState::Missing);
        named.driver.packages = vec![name.to_owned()];
        assert_eq!(
            named.validate().is_ok(),
            valid,
            "package name {name:?} validity"
        );
    }

    for (inf, valid) in [
        (None, true),
        (Some("oem7.inf"), true),
        (Some("crosspane.inf"), false),
        (Some(""), false),
    ] {
        let mut named = report(DriverState::Installed, FirewallState::Missing);
        named.driver.devices = vec![device(r"ROOT\DISPLAY\0000", inf)];
        assert_eq!(
            named.validate().is_ok(),
            valid,
            "driver_inf {inf:?} validity"
        );
    }

    for (length, valid) in [(0, false), (1, true), (256, true), (257, false)] {
        let mut sized = report(DriverState::Installed, FirewallState::Missing);
        sized.driver.devices = vec![device(&"x".repeat(length), None)];
        assert_eq!(
            sized.validate().is_ok(),
            valid,
            "instance id of {length} bytes validity"
        );
    }

    // The bound counts characters, so 256 two-byte characters are still valid.
    let mut accented = report(DriverState::Installed, FirewallState::Missing);
    accented.driver.devices = vec![device(&"é".repeat(256), None)];
    assert_eq!(accented.validate(), Ok(()));
    accented.driver.devices = vec![device(&"é".repeat(257), None)];
    assert_eq!(accented.validate(), Err(ElevatedError::Report));
}

#[test]
fn receipts_carry_resource_ids_paths_and_created_ownership() {
    let cases = [
        (
            Verb::Status(None),
            "windows-elevated-status",
            HARDWARE_ID.to_owned(),
        ),
        (
            Verb::Status(Some(scope())),
            "windows-elevated-status",
            HARDWARE_ID.to_owned(),
        ),
        (
            Verb::InstallDriver,
            "windows-idd-driver",
            HARDWARE_ID.to_owned(),
        ),
        (
            Verb::RemoveDriver,
            "windows-idd-driver",
            HARDWARE_ID.to_owned(),
        ),
        (
            Verb::AddFirewall(scope()),
            "windows-firewall-rule",
            RULE.to_owned(),
        ),
        (
            Verb::RemoveFirewall(scope()),
            "windows-firewall-rule",
            RULE.to_owned(),
        ),
    ];
    for (verb, resource_id, resolved_path) in cases {
        let intent = intent_entry(&verb, None);
        assert_eq!(intent.phase, JournalPhase::Intent);
        assert_eq!(intent.verb, verb.name());
        assert_eq!(intent.exit, None);
        assert_eq!(intent.receipt.resource_id, resource_id);
        assert_eq!(intent.receipt.resolved_path, resolved_path);
        assert_eq!(intent.receipt.ownership, ResourceOwnership::Created);

        let outcome = outcome_entry(&verb, None, None);
        assert_eq!(outcome.phase, JournalPhase::Outcome);
        assert_eq!(outcome.verb, verb.name());
        assert_eq!(outcome.receipt.resource_id, resource_id);
        assert_eq!(outcome.receipt.resolved_path, resolved_path);
        assert_eq!(outcome.receipt.ownership, ResourceOwnership::Created);
    }
}

#[test]
fn intent_observation_table_maps_driver_and_firewall_states() {
    let driver_table = [
        (DriverState::Absent, ResourceObservation::Absent),
        (DriverState::Installed, ResourceObservation::Matching),
        (DriverState::PackageOnly, ResourceObservation::Different),
        (
            DriverState::DeviceWithoutDriver,
            ResourceObservation::Different,
        ),
        (DriverState::Mismatch, ResourceObservation::Different),
        (DriverState::Unavailable, ResourceObservation::Unknown),
    ];
    for (state, observation) in driver_table {
        let before = report(state, FirewallState::Present);
        let entry = intent_entry(&Verb::InstallDriver, Some(&before));
        assert_eq!(entry.receipt.before, observation, "driver {state:?}");
        assert_eq!(entry.receipt.after, ResourceObservation::Unknown);
        assert_eq!(entry.receipt.outcome, MutationOutcome::Unknown);
        assert_eq!(entry.exit, None);

        // `Status` reads the driver state, whatever the firewall says.
        let status = intent_entry(&Verb::Status(Some(scope())), Some(&before));
        assert_eq!(
            status.receipt.before, observation,
            "status driver {state:?}"
        );
    }

    let firewall_table = [
        (FirewallState::Missing, ResourceObservation::Absent),
        (FirewallState::Present, ResourceObservation::Matching),
        (FirewallState::Mismatch, ResourceObservation::Different),
        (FirewallState::Unavailable, ResourceObservation::Unknown),
        (FirewallState::NotRequested, ResourceObservation::Unknown),
    ];
    for (state, observation) in firewall_table {
        let before = report(DriverState::Absent, state);
        let entry = intent_entry(&Verb::AddFirewall(scope()), Some(&before));
        assert_eq!(entry.receipt.before, observation, "firewall {state:?}");
        let removal = intent_entry(&Verb::RemoveFirewall(scope()), Some(&before));
        assert_eq!(removal.receipt.before, observation, "removal {state:?}");
    }

    // Without a report the observation is unknown.
    for verb in mutating_verbs() {
        assert_eq!(
            intent_entry(&verb, None).receipt.before,
            ResourceObservation::Unknown
        );
    }
}

#[test]
fn outcome_receipt_mapping_table() {
    let verified_driver = report(DriverState::Installed, FirewallState::NotRequested);
    let unverified_driver = report(DriverState::Absent, FirewallState::NotRequested);

    let table = [
        (
            Some(Outcome::Done),
            Some(&verified_driver),
            MutationOutcome::Verified,
        ),
        (
            Some(Outcome::AlreadyDone),
            Some(&verified_driver),
            MutationOutcome::Verified,
        ),
        (
            Some(Outcome::RebootRequired),
            Some(&verified_driver),
            MutationOutcome::Verified,
        ),
        (
            Some(Outcome::Done),
            Some(&unverified_driver),
            MutationOutcome::Unknown,
        ),
        (
            Some(Outcome::RebootRequired),
            Some(&unverified_driver),
            MutationOutcome::Unknown,
        ),
        (Some(Outcome::Done), None, MutationOutcome::Unknown),
        (Some(Outcome::AlreadyDone), None, MutationOutcome::Unknown),
        // Refusals stay refusals, even when the state looks right afterwards.
        (
            Some(Outcome::Refused),
            Some(&verified_driver),
            MutationOutcome::Refused,
        ),
        (Some(Outcome::NotElevated), None, MutationOutcome::Refused),
        (
            Some(Outcome::Mismatch),
            Some(&unverified_driver),
            MutationOutcome::Refused,
        ),
        (
            Some(Outcome::Failed),
            Some(&verified_driver),
            MutationOutcome::Failed,
        ),
        (Some(Outcome::Failed), None, MutationOutcome::Failed),
        (None, Some(&verified_driver), MutationOutcome::Unknown),
        (None, None, MutationOutcome::Unknown),
    ];
    for (outcome, after, expected) in table {
        let entry = outcome_entry(&Verb::InstallDriver, outcome, after);
        assert_eq!(
            entry.receipt.outcome, expected,
            "{outcome:?} after {after:?}"
        );
        assert_eq!(entry.exit, outcome.map(Outcome::exit_code));
        assert_eq!(entry.receipt.before, ResourceObservation::Unknown);
        assert_eq!(
            entry.receipt.after,
            after.map_or(ResourceObservation::Unknown, |report| {
                if report.driver.state == DriverState::Installed {
                    ResourceObservation::Matching
                } else {
                    ResourceObservation::Absent
                }
            })
        );
    }

    // Firewall verbs judge the firewall state.
    let present = report(DriverState::Absent, FirewallState::Present);
    let missing = report(DriverState::Absent, FirewallState::Missing);
    assert_eq!(
        outcome_entry(
            &Verb::AddFirewall(scope()),
            Some(Outcome::Done),
            Some(&present)
        )
        .receipt
        .outcome,
        MutationOutcome::Verified
    );
    assert_eq!(
        outcome_entry(
            &Verb::AddFirewall(scope()),
            Some(Outcome::Done),
            Some(&missing)
        )
        .receipt
        .outcome,
        MutationOutcome::Unknown
    );
    assert_eq!(
        outcome_entry(
            &Verb::RemoveFirewall(scope()),
            Some(Outcome::Done),
            Some(&missing)
        )
        .receipt
        .outcome,
        MutationOutcome::Verified
    );
    assert_eq!(
        outcome_entry(
            &Verb::RemoveFirewall(scope()),
            Some(Outcome::Refused),
            Some(&missing)
        )
        .receipt
        .outcome,
        MutationOutcome::Refused
    );

    // Exit codes are recorded as the helper reports them.
    assert_eq!(
        outcome_entry(
            &Verb::InstallDriver,
            Some(Outcome::AlreadyDone),
            Some(&verified_driver)
        )
        .exit,
        Some(16)
    );
    assert_eq!(
        outcome_entry(&Verb::InstallDriver, Some(Outcome::Failed), None).exit,
        Some(64)
    );
}

#[test]
fn journal_and_report_round_trip_and_refuse_unknown_fields() {
    let full = StatusReport {
        schema: STATUS_SCHEMA,
        elevated: true,
        driver: DriverStatus {
            state: DriverState::Installed,
            packages: vec!["oem7.inf".to_owned()],
            devices: vec![device(r"ROOT\DISPLAY\0000", Some("oem7.inf"))],
        },
        firewall: FirewallStatus {
            state: FirewallState::Present,
            family_count: 1,
            local_rules_apply: Some(true),
        },
    };
    let text = serde_json::to_string(&full).unwrap();
    let restored: StatusReport = serde_json::from_str(&text).unwrap();
    assert_eq!(restored, full);

    let value = serde_json::to_value(&full).unwrap();
    let mut top = value.clone();
    top["extra"] = json!(1);
    assert!(serde_json::from_value::<StatusReport>(top).is_err());
    let mut driver = value.clone();
    driver["driver"]["extra"] = json!(1);
    assert!(serde_json::from_value::<StatusReport>(driver).is_err());
    let mut item = value.clone();
    item["driver"]["devices"][0]["extra"] = json!(1);
    assert!(serde_json::from_value::<StatusReport>(item).is_err());
    let mut firewall = value.clone();
    firewall["firewall"]["extra"] = json!(1);
    assert!(serde_json::from_value::<StatusReport>(firewall).is_err());

    for entry in [
        intent_entry(&Verb::AddFirewall(scope()), Some(&full)),
        outcome_entry(
            &Verb::AddFirewall(scope()),
            Some(Outcome::Done),
            Some(&full),
        ),
    ] {
        let text = serde_json::to_string(&entry).unwrap();
        let restored: JournalEntry = serde_json::from_str(&text).unwrap();
        assert_eq!(restored, entry);

        let mut value = serde_json::to_value(&entry).unwrap();
        value["extra"] = json!(true);
        assert!(serde_json::from_value::<JournalEntry>(value).is_err());
    }

    assert_eq!(
        serde_json::to_value(VerbName::AddFirewall).unwrap(),
        json!("add-firewall")
    );
    assert_eq!(
        serde_json::to_value(JournalPhase::Outcome).unwrap(),
        json!("Outcome")
    );
}
