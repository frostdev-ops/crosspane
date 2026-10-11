//! Pure first-install and administrator-step facts; no native reader, lock or mutation port is built.
#[path = "../src/platform/windows/integration/domains.rs"]
#[allow(dead_code)]
mod domains;
use crosspane_installer_core::ObservationSource;
use crosspane_installer_core::elevated::status::{
    DriverStatus, FirewallStatus, STATUS_SCHEMA, StatusReport,
};
use crosspane_installer_core::elevated::step::{INSTALL_WITHOUT_SETUP, StepResult, report_lines};
use crosspane_installer_core::elevated::{
    AgentProgram, DriverState, FirewallState, InstallId, Outcome, RuleScope, Verb,
};
use domains::*;

/// The rule scope the real core consent path accepts (windows_integration.rs uses the same one).
#[allow(clippy::unwrap_used)] // Test-only fixture; the core must accept this scope.
fn scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41d-vm-1").unwrap(),
        program: AgentProgram::parse(
            r"C:\Users\jame\AppData\Local\Programs\Crosspane\crosspane-agent.exe",
        )
        .unwrap(),
    }
}
fn setup() -> Verb {
    Verb::Setup(scope())
}
fn add_firewall() -> Verb {
    Verb::AddFirewall(scope())
}
/// A first install whose files, sign-in task and settlement are in place, and whose agent has not
/// started yet.
fn installed() -> Snapshot {
    Snapshot {
        supported: true,
        inventory: true,
        sources: true,
        payload: State::Healthy,
        task: State::Healthy,
        agent: State::Missing,
        cold: Cold::Existing,
        terminal_history: false,
        unsettled: false,
        correlation: vec![1],
        source: ObservationSource::Live,
    }
}
fn healthy() -> Snapshot {
    let mut snapshot = installed();
    snapshot.agent = State::Healthy;
    snapshot
}
fn report(firewall: FirewallState, driver: DriverState) -> StatusReport {
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
/// The setup step as the helper ran it: each part's outcome, then the state the status shows after.
fn ran(firewall: Option<Outcome>, driver: Option<Outcome>, after: &StatusReport) -> StepResult {
    StepResult::launched(
        &setup(),
        &[(add_firewall(), firewall), (Verb::InstallDriver, driver)],
        Some(after),
    )
}
fn says(lines: &[String], text: &str) -> bool {
    lines.iter().any(|line| line == text)
}
#[test]
fn installed_ignores_agent_health_but_not_files_task_or_settlement() {
    for agent in [
        State::Missing,
        State::Unknown,
        State::Mismatch,
        State::Disabled,
        State::Unavailable,
        State::Healthy,
    ] {
        let mut snapshot = installed();
        snapshot.agent = agent;
        assert!(snapshot.installed(), "agent {agent:?}");
    }
    // Health still needs a healthy agent; installed() does not.
    assert!(!installed().healthy());
    assert!(healthy().healthy());
    let breaks: [fn(&mut Snapshot); 10] = [
        |s| s.payload = State::Missing,
        |s| s.payload = State::Mismatch,
        |s| s.task = State::Missing,
        |s| s.task = State::Mismatch,
        |s| s.task = State::Disabled,
        |s| s.cold = Cold::Observe,
        |s| s.cold = Cold::Partial,
        |s| s.unsettled = true,
        |s| s.supported = false,
        |s| s.inventory = false,
    ];
    for (index, breaks) in breaks.iter().enumerate() {
        let mut snapshot = installed();
        breaks(&mut snapshot);
        assert!(!snapshot.installed(), "break {index}");
    }
}
#[test]
fn install_verification_needs_a_live_installed_observation() {
    assert!(installed().verified(Operation::Install));
    assert!(healthy().verified(Operation::Install));
    let mut demo = installed();
    demo.source = ObservationSource::Demo;
    assert!(!demo.verified(Operation::Install));
    let mut observe = installed();
    observe.cold = Cold::Observe;
    assert!(!observe.verified(Operation::Install));
    let mut unsettled = installed();
    unsettled.unsettled = true;
    assert!(!unsettled.verified(Operation::Install));
    // The other operations keep their own rules: the agent must be healthy for them.
    let no_agent = installed();
    assert!(!no_agent.verified(Operation::Upgrade));
    assert!(!no_agent.verified(Operation::PayloadRepair));
    assert!(!no_agent.verified(Operation::MetadataRepair));
    assert!(healthy().verified(Operation::Upgrade));
    assert!(healthy().verified(Operation::PayloadRepair));
    assert!(healthy().verified(Operation::MetadataRepair));
    let mut removed = installed();
    removed.payload = State::Missing;
    removed.task = State::Missing;
    removed.agent = State::Missing;
    assert!(removed.verified(Operation::Removal {
        erase_identity: false
    }));
    assert!(!healthy().verified(Operation::Removal {
        erase_identity: false
    }));
}
#[test]
fn admin_is_satisfied_by_the_firewall_rule_and_reports_a_missing_driver() {
    let check = admin_detection(&report(FirewallState::Present, DriverState::Absent), false);
    assert!(check.configured);
    assert_eq!(
        check.detail,
        format!("Firewall rule: present. Display driver: not installed. {DRIVER_OPTIONAL_NOTE}")
    );
    for driver in [
        DriverState::PackageOnly,
        DriverState::DeviceWithoutDriver,
        DriverState::Mismatch,
        DriverState::Unavailable,
    ] {
        let check = admin_detection(&report(FirewallState::Present, driver), false);
        assert!(check.configured, "{driver:?}");
        assert!(check.detail.starts_with("Firewall rule: present. "));
        assert!(check.detail.ends_with(DRIVER_OPTIONAL_NOTE), "{driver:?}");
    }
    let complete = admin_detection(
        &report(FirewallState::Present, DriverState::Installed),
        false,
    );
    assert!(complete.configured);
    assert_eq!(
        complete.detail,
        "Firewall rule: present. Display driver: installed."
    );
}
#[test]
fn admin_is_not_satisfied_without_the_firewall_rule_whatever_the_driver() {
    for firewall in [
        FirewallState::Missing,
        FirewallState::Mismatch,
        FirewallState::Unavailable,
        FirewallState::NotRequested,
    ] {
        for driver in [
            DriverState::Installed,
            DriverState::Absent,
            DriverState::PackageOnly,
        ] {
            let check = admin_detection(&report(firewall, driver), false);
            assert!(!check.configured, "{firewall:?} {driver:?}");
            assert!(!check.detail.contains(DRIVER_OPTIONAL_NOTE), "{firewall:?}");
        }
    }
}
#[test]
fn install_lines_say_installed_without_them_only_when_the_firewall_was_not_added() {
    let no_firewall = report(FirewallState::Missing, DriverState::Installed);
    let all_ok = report(FirewallState::Present, DriverState::Installed);
    let refused = ran(Some(Outcome::Refused), Some(Outcome::Done), &no_firewall);
    assert!(!firewall_part_verified(&refused));
    assert!(says(
        &install_setup_lines(&refused, true),
        INSTALL_WITHOUT_SETUP
    ));
    let failed = ran(Some(Outcome::Failed), Some(Outcome::Done), &no_firewall);
    assert!(says(
        &install_setup_lines(&failed, true),
        INSTALL_WITHOUT_SETUP
    ));
    // A step that never ran (no kit) is a firewall rule that was not added.
    let not_run = StepResult::not_run(&setup(), "the kit isn't available");
    assert!(!firewall_part_verified(&not_run));
    assert!(says(
        &install_setup_lines(&not_run, true),
        INSTALL_WITHOUT_SETUP
    ));
    // Not said when the install did not complete.
    assert!(!says(
        &install_setup_lines(&refused, false),
        INSTALL_WITHOUT_SETUP
    ));
    // Not said when the firewall rule was added, even if the driver failed.
    let added = ran(Some(Outcome::Done), Some(Outcome::Failed), &all_ok);
    assert!(firewall_part_verified(&added));
    assert!(!says(
        &install_setup_lines(&added, true),
        INSTALL_WITHOUT_SETUP
    ));
    let verified = ran(Some(Outcome::Done), Some(Outcome::Done), &all_ok);
    assert!(firewall_part_verified(&verified));
    assert_eq!(
        install_setup_lines(&verified, true),
        report_lines(&verified)
    );
}
#[test]
fn install_lines_note_the_optional_driver_when_only_the_driver_failed() {
    let driver_failed = ran(
        Some(Outcome::Done),
        Some(Outcome::Failed),
        &report(FirewallState::Present, DriverState::Absent),
    );
    let lines = install_setup_lines(&driver_failed, true);
    assert_eq!(lines.len(), report_lines(&driver_failed).len() + 1);
    assert!(says(&lines, DRIVER_OPTIONAL_NOTE));
    assert!(!says(&lines, INSTALL_WITHOUT_SETUP));
    // Not said when the install did not complete.
    assert!(!says(
        &install_setup_lines(&driver_failed, false),
        DRIVER_OPTIONAL_NOTE
    ));
}
#[test]
fn restart_parts_add_no_install_note() {
    let restart_firewall = ran(
        Some(Outcome::RebootRequired),
        Some(Outcome::Done),
        &report(FirewallState::Missing, DriverState::Installed),
    );
    assert_eq!(
        install_setup_lines(&restart_firewall, true),
        report_lines(&restart_firewall)
    );
    let restart_driver = ran(
        Some(Outcome::Done),
        Some(Outcome::RebootRequired),
        &report(FirewallState::Present, DriverState::Absent),
    );
    assert_eq!(
        install_setup_lines(&restart_driver, true),
        report_lines(&restart_driver)
    );
}
