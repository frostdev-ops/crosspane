//! WP-W4.1c T12: the firewall rule model (`elevated::firewall`) on synthetic COM rules.
#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::firewall::{
    ComRule, FirewallPlan, MAX_FAMILY, desired_rule, firewall_state, plan_add, plan_remove,
    rule_matches,
};
use crosspane_installer_core::elevated::{AgentProgram, FirewallState, InstallId, RuleScope};
use std::slice;

const PROGRAM: &str = r"C:\Users\alice\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
const OTHER_PROGRAM: &str = r"D:\Other\Programs\Crosspane\crosspane-agent.exe";
const DIFFERS: &str = "the Crosspane firewall rule differs from its specification";
const COVERED: &str = "another Crosspane rule already covers this agent";

/// One single-field change to the desired rule.
type Perturb = fn(&mut ComRule);

fn scope_for(id: &str, program: &str) -> RuleScope {
    RuleScope {
        id: InstallId::parse(id).unwrap(),
        program: AgentProgram::parse(program).unwrap(),
    }
}

fn scope() -> RuleScope {
    scope_for("w41c-vm-1", PROGRAM)
}

/// The exact specification for another install ID. Only its name and group differ from ours
/// unless `program` is changed.
fn other_install(program: &str) -> ComRule {
    desired_rule(&scope_for("other-id", program))
}

/// An unrelated Crosspane-named rule for a different program.
fn filler(index: usize) -> ComRule {
    ComRule {
        name: format!("Crosspane.Agent.UDP.Private.filler-{index}"),
        application_name: format!(r"E:\Other\app-{index}.exe"),
        ..Default::default()
    }
}

#[test]
fn desired_rule_matches_itself() {
    let scope = scope();
    let rule = desired_rule(&scope);
    assert!(rule_matches(&rule, &scope));
    assert_eq!(rule.name, "Crosspane.Agent.UDP.Private.w41c-vm-1");
    assert_eq!(rule.grouping, "Crosspane.w41c-vm-1");
    assert_eq!(rule.application_name, PROGRAM);
    assert_eq!(
        (rule.protocol, rule.direction, rule.action, rule.profiles),
        (17, 1, 1, 2)
    );
    assert!(rule.enabled);
    assert_eq!(rule.remote_addresses, "LocalSubnet");
    assert_eq!(rule.local_ports, "*");
    assert_eq!(rule.remote_ports, "*");
    assert_eq!(rule.local_addresses, "*");
    assert!(rule.icmp_types_and_codes.is_empty());
    assert_eq!(rule.interface_types, "All");
    assert!(rule.interfaces_any);
    assert!(!rule.edge_traversal);
    assert_eq!((rule.edge_traversal_options, rule.secure_flags), (0, 0));
    assert!(rule.service_name.is_empty());
    assert!(rule.local_app_package_id.is_empty());
    assert!(rule.local_user_owner.is_empty());
    assert!(rule.local_user_authorized_list.is_empty());
    assert!(rule.remote_user_authorized_list.is_empty());
    assert!(rule.remote_machine_authorized_list.is_empty());
    // A second, differently shaped install still matches its own desired rule.
    let other = scope_for(
        "A-1",
        r"C:\Program Files\Programs\Crosspane\crosspane-agent.exe",
    );
    assert!(rule_matches(&desired_rule(&other), &other));
}

#[test]
fn each_checked_field_alone_is_a_mismatch() {
    let scope = scope();
    let desired = desired_rule(&scope);
    // The 24 checked fields. `description` is the only field of `ComRule` that is not checked.
    let cases: Vec<(&str, Perturb)> = vec![
        ("name", |rule| {
            rule.name = "Crosspane.Agent.UDP.Private.other-id".to_owned()
        }),
        ("grouping", |rule| {
            rule.grouping = "Crosspane.other-id".to_owned()
        }),
        ("application_name", |rule| {
            rule.application_name = r"C:\Other\crosspane-agent.exe".to_owned()
        }),
        ("protocol", |rule| rule.protocol = 6),
        ("direction", |rule| rule.direction = 2),
        ("action", |rule| rule.action = 0),
        ("profiles", |rule| rule.profiles = 7),
        ("enabled", |rule| rule.enabled = false),
        ("edge_traversal", |rule| rule.edge_traversal = true),
        ("edge_traversal_options", |rule| {
            rule.edge_traversal_options = 1
        }),
        ("secure_flags", |rule| rule.secure_flags = 1),
        ("remote_addresses", |rule| {
            rule.remote_addresses = "10.0.0.0/8".to_owned()
        }),
        ("local_ports", |rule| rule.local_ports = "47811".to_owned()),
        ("remote_ports", |rule| {
            rule.remote_ports = "47811".to_owned()
        }),
        ("local_addresses", |rule| {
            rule.local_addresses = "10.0.0.5".to_owned()
        }),
        ("icmp_types_and_codes", |rule| {
            rule.icmp_types_and_codes = "8:*".to_owned()
        }),
        ("interface_types", |rule| {
            rule.interface_types = "Wireless".to_owned()
        }),
        ("interfaces_any", |rule| rule.interfaces_any = false),
        ("service_name", |rule| {
            rule.service_name = "Dnscache".to_owned()
        }),
        ("local_app_package_id", |rule| {
            rule.local_app_package_id = "S-1-15-2-1".to_owned()
        }),
        ("local_user_owner", |rule| {
            rule.local_user_owner = "S-1-5-18".to_owned()
        }),
        ("local_user_authorized_list", |rule| {
            rule.local_user_authorized_list = "O:SYD:".to_owned()
        }),
        ("remote_user_authorized_list", |rule| {
            rule.remote_user_authorized_list = "O:SYD:".to_owned()
        }),
        ("remote_machine_authorized_list", |rule| {
            rule.remote_machine_authorized_list = "O:SYD:".to_owned()
        }),
    ];
    assert_eq!(cases.len(), 24);
    for (label, perturb) in cases {
        let mut rule = desired.clone();
        perturb(&mut rule);
        assert_ne!(rule, desired, "{label}: the perturbation changed nothing");
        assert!(!rule_matches(&rule, &scope), "{label}: still matches");
        let family = slice::from_ref(&rule);
        assert!(
            matches!(plan_add(family, &scope), FirewallPlan::Mismatch(_)),
            "{label}: plan_add"
        );
        assert!(
            matches!(plan_remove(family, &scope), FirewallPlan::Mismatch(_)),
            "{label}: plan_remove"
        );
        assert_eq!(
            firewall_state(family, Some(&scope)),
            FirewallState::Mismatch,
            "{label}: state"
        );
    }
}

#[test]
fn description_is_not_compared() {
    let scope = scope();
    let mut rule = desired_rule(&scope);
    rule.description = "edited by someone else".to_owned();
    assert!(rule_matches(&rule, &scope));
    rule.description.clear();
    assert!(rule_matches(&rule, &scope));
}

#[test]
fn empty_and_star_are_normalized() {
    let scope = scope();
    for spelling in ["", "*"] {
        let mut rule = desired_rule(&scope);
        rule.local_ports = spelling.to_owned();
        rule.remote_ports = spelling.to_owned();
        rule.local_addresses = spelling.to_owned();
        rule.icmp_types_and_codes = spelling.to_owned();
        assert!(rule_matches(&rule, &scope), "spelling {spelling:?}");
        assert_eq!(
            plan_add(slice::from_ref(&rule), &scope),
            FirewallPlan::AlreadyPresent
        );
    }
    for interface_types in ["", "All"] {
        let mut rule = desired_rule(&scope);
        rule.interface_types = interface_types.to_owned();
        assert!(rule_matches(&rule, &scope), "interface {interface_types:?}");
    }
    let mut rule = desired_rule(&scope);
    rule.remote_addresses = "localsubnet".to_owned();
    assert!(rule_matches(&rule, &scope));
}

#[test]
fn verbatim_program_prefix_is_the_same_program() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let mut verbatim = ours.clone();
    verbatim.application_name = format!(r"\\?\{PROGRAM}");
    assert!(rule_matches(&verbatim, &scope));
    assert_eq!(
        plan_add(slice::from_ref(&verbatim), &scope),
        FirewallPlan::AlreadyPresent
    );
    assert_eq!(
        plan_remove(slice::from_ref(&verbatim), &scope),
        FirewallPlan::Remove
    );
    assert_eq!(
        firewall_state(slice::from_ref(&verbatim), Some(&scope)),
        FirewallState::Present
    );

    let mut upper = ours.clone();
    upper.application_name = PROGRAM.to_uppercase();
    assert!(rule_matches(&upper, &scope));

    let mut other_path = ours.clone();
    other_path.application_name = format!(r"\\?\{OTHER_PROGRAM}");
    assert!(!rule_matches(&other_path, &scope));
}

#[test]
fn same_program_under_another_name_mismatches() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let covered = other_install(PROGRAM);
    let mut covered_verbatim = other_install(PROGRAM);
    covered_verbatim.application_name = format!(r"\\?\{PROGRAM}");
    for family in [
        vec![covered.clone()],
        vec![covered_verbatim.clone()],
        vec![ours.clone(), covered.clone()],
    ] {
        assert_eq!(
            plan_add(&family, &scope),
            FirewallPlan::Mismatch(COVERED),
            "{family:?}"
        );
        assert_eq!(
            plan_remove(&family, &scope),
            FirewallPlan::Mismatch(COVERED),
            "{family:?}"
        );
        assert_eq!(
            firewall_state(&family, Some(&scope)),
            FirewallState::Mismatch,
            "{family:?}"
        );
    }
}

#[test]
fn other_program_under_another_name_is_unrelated() {
    let scope = scope();
    let unrelated = other_install(OTHER_PROGRAM);
    assert_eq!(
        plan_add(slice::from_ref(&unrelated), &scope),
        FirewallPlan::Add
    );
    assert_eq!(
        plan_remove(slice::from_ref(&unrelated), &scope),
        FirewallPlan::AlreadyAbsent
    );
    assert_eq!(
        firewall_state(slice::from_ref(&unrelated), Some(&scope)),
        FirewallState::Missing
    );
}

#[test]
fn two_rules_with_the_same_name_mismatch() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let mut differs = ours.clone();
    differs.profiles = 7;
    let mut other_program = ours.clone();
    other_program.application_name = OTHER_PROGRAM.to_owned();
    for family in [
        vec![ours.clone(), ours.clone()],
        vec![ours.clone(), differs.clone()],
        vec![ours.clone(), other_program.clone()],
        vec![differs.clone(), differs.clone()],
    ] {
        assert_eq!(
            plan_add(&family, &scope),
            FirewallPlan::Mismatch(DIFFERS),
            "{family:?}"
        );
        assert_eq!(
            plan_remove(&family, &scope),
            FirewallPlan::Mismatch(DIFFERS),
            "{family:?}"
        );
        assert_eq!(
            firewall_state(&family, Some(&scope)),
            FirewallState::Mismatch,
            "{family:?}"
        );
    }
}

#[test]
fn case_variant_name_is_the_same_rule() {
    let scope = scope();
    let mut shouted = desired_rule(&scope);
    shouted.name = shouted.name.to_uppercase();
    assert!(!rule_matches(&shouted, &scope));
    assert_eq!(
        plan_add(slice::from_ref(&shouted), &scope),
        FirewallPlan::Mismatch(DIFFERS)
    );
    assert_eq!(
        plan_remove(slice::from_ref(&shouted), &scope),
        FirewallPlan::Mismatch(DIFFERS)
    );
    assert_eq!(
        firewall_state(slice::from_ref(&shouted), Some(&scope)),
        FirewallState::Mismatch
    );
}

#[test]
fn family_over_128_rules_mismatches() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let full: Vec<ComRule> = (0..MAX_FAMILY).map(filler).collect();
    let mut full_with_ours: Vec<ComRule> = (1..MAX_FAMILY).map(filler).collect();
    full_with_ours.push(ours.clone());
    let over: Vec<ComRule> = (0..=MAX_FAMILY).map(filler).collect();
    let mut over_with_ours: Vec<ComRule> = (0..MAX_FAMILY).map(filler).collect();
    over_with_ours.push(ours.clone());

    // The boundary: exactly 128 rules is accepted.
    assert_eq!(plan_add(&full, &scope), FirewallPlan::Add);
    assert_eq!(plan_remove(&full, &scope), FirewallPlan::AlreadyAbsent);
    assert_eq!(firewall_state(&full, Some(&scope)), FirewallState::Missing);
    assert_eq!(
        plan_add(&full_with_ours, &scope),
        FirewallPlan::AlreadyPresent
    );
    assert_eq!(plan_remove(&full_with_ours, &scope), FirewallPlan::Remove);
    assert_eq!(
        firewall_state(&full_with_ours, Some(&scope)),
        FirewallState::Present
    );

    // 129 rules is a mismatch, whether or not ours is among them.
    assert!(matches!(plan_add(&over, &scope), FirewallPlan::Mismatch(_)));
    assert!(matches!(
        plan_remove(&over, &scope),
        FirewallPlan::Mismatch(_)
    ));
    assert_eq!(firewall_state(&over, Some(&scope)), FirewallState::Mismatch);
    assert!(matches!(
        plan_add(&over_with_ours, &scope),
        FirewallPlan::Mismatch(_)
    ));
    assert!(matches!(
        plan_remove(&over_with_ours, &scope),
        FirewallPlan::Mismatch(_)
    ));
    assert_eq!(
        firewall_state(&over_with_ours, Some(&scope)),
        FirewallState::Mismatch
    );
}

#[test]
fn plan_table() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let mut differs = ours.clone();
    differs.local_ports = "47811".to_owned();
    let covered = other_install(PROGRAM);

    assert_eq!(plan_add(&[], &scope), FirewallPlan::Add);
    assert_eq!(
        plan_add(slice::from_ref(&ours), &scope),
        FirewallPlan::AlreadyPresent
    );
    assert_eq!(
        plan_add(slice::from_ref(&differs), &scope),
        FirewallPlan::Mismatch(DIFFERS)
    );
    assert_eq!(
        plan_add(slice::from_ref(&covered), &scope),
        FirewallPlan::Mismatch(COVERED)
    );

    assert_eq!(plan_remove(&[], &scope), FirewallPlan::AlreadyAbsent);
    assert_eq!(
        plan_remove(slice::from_ref(&ours), &scope),
        FirewallPlan::Remove
    );
    assert_eq!(
        plan_remove(slice::from_ref(&differs), &scope),
        FirewallPlan::Mismatch(DIFFERS)
    );
    assert_eq!(
        plan_remove(slice::from_ref(&covered), &scope),
        FirewallPlan::Mismatch(COVERED)
    );
}

#[test]
fn firewall_state_table() {
    let scope = scope();
    let ours = desired_rule(&scope);
    let mut differs = ours.clone();
    differs.profiles = 7;
    let covered = other_install(PROGRAM);
    let unrelated = other_install(OTHER_PROGRAM);
    let over: Vec<ComRule> = (0..=MAX_FAMILY).map(filler).collect();

    // No requested rule: NotRequested, whatever the family holds.
    assert_eq!(firewall_state(&[], None), FirewallState::NotRequested);
    assert_eq!(
        firewall_state(slice::from_ref(&differs), None),
        FirewallState::NotRequested
    );
    assert_eq!(firewall_state(&over, None), FirewallState::NotRequested);

    // Requested rule.
    assert_eq!(firewall_state(&[], Some(&scope)), FirewallState::Missing);
    assert_eq!(
        firewall_state(slice::from_ref(&ours), Some(&scope)),
        FirewallState::Present
    );
    assert_eq!(
        firewall_state(slice::from_ref(&differs), Some(&scope)),
        FirewallState::Mismatch
    );
    assert_eq!(
        firewall_state(&[ours.clone(), ours.clone()], Some(&scope)),
        FirewallState::Mismatch
    );
    assert_eq!(
        firewall_state(slice::from_ref(&covered), Some(&scope)),
        FirewallState::Mismatch
    );
    assert_eq!(
        firewall_state(slice::from_ref(&unrelated), Some(&scope)),
        FirewallState::Missing
    );
    assert_eq!(firewall_state(&over, Some(&scope)), FirewallState::Mismatch);
}
