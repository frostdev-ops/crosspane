#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::status::{
    DriverStatus, FirewallStatus, STATUS_SCHEMA, StatusReport, describe, verified,
};
use crosspane_installer_core::elevated::step::PartResult::{
    Declined, Failed, NotRun, RebootRequired, Refused, Unknown, Verified,
};
use crosspane_installer_core::elevated::step::{
    ElevatedPlan, Gate, INSTALL_DECLINE_NOTE, INSTALL_WITHOUT_SETUP, JOURNAL_LOST,
    MAX_PREVIEW_BLOCK, PREVIEW_HEADER, PartResult, REMOVAL_NOT_STARTED, StepResult, admin_gate,
    classify, removal_gate, report_lines, status_summary,
};
use crosspane_installer_core::elevated::{
    AgentProgram, DriverState, ElevatedError, FirewallState, InstallId, Outcome, RuleScope, Verb,
    VerbName,
};

const PROGRAM: &str = r"C:\Users\user\AppData\Local\Programs\Crosspane\crosspane-agent.exe";
/// The reason every `NotRun` test passes; the frozen wording is `unchanged (<reason>)`.
const REASON: &str = "a test reason";
const PENDING_NOTE: &str =
    " An earlier administrator step was interrupted; it is checked again before the next one runs.";

fn scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(PROGRAM).unwrap(),
    }
}

/// The same program under another install ID, so its rule name and its lines differ.
fn other_scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-2").unwrap(),
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

/// A `StepResult` with one part per entry of `parts`, in that order. The gates read only the
/// states, so every part is named `AddFirewall`.
fn result_of(parts: &[PartResult], journal_failed: bool) -> StepResult {
    StepResult {
        parts: parts
            .iter()
            .map(|part| (VerbName::AddFirewall, *part))
            .collect(),
        not_run: None,
        journal_failed,
    }
}

fn removal(parts: &[PartResult]) -> Gate {
    removal_gate(&result_of(parts, false))
}

fn admin(parts: &[PartResult]) -> Gate {
    admin_gate(&result_of(parts, false))
}

/// The consent block as `ElevatedPlan::preview_block` builds it, for a verb the plan may refuse.
fn block_text(verb: &Verb) -> String {
    format!("{PREVIEW_HEADER}\n{}", describe(verb).join("\n"))
}

/// A valid agent path whose first component is `count` copies of `filler`. Only that component
/// grows, so for an ASCII filler the consent block grows by one byte per copy.
fn padded_scope(filler: &str, count: usize) -> RuleScope {
    let program = format!(
        r"C:\{}\Programs\Crosspane\crosspane-agent.exe",
        filler.repeat(count)
    );
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(&program).unwrap(),
    }
}

#[test]
fn frozen_constants_are_verbatim() {
    assert_eq!(
        PREVIEW_HEADER,
        "Windows will ask once for administrator approval to:"
    );
    assert_eq!(MAX_PREVIEW_BLOCK, 1_800);
    assert_eq!(
        INSTALL_DECLINE_NOTE,
        "If you decline, Crosspane is still installed without them."
    );
    assert_eq!(
        INSTALL_WITHOUT_SETUP,
        "Crosspane is installed without them. Until the firewall rule is added, other computers may not reach this PC; without the display driver, Crosspane mirrors windows in place."
    );
    assert_eq!(
        REMOVAL_NOT_STARTED,
        "Nothing was removed. To remove Crosspane and keep the firewall rule and display driver, clear that choice and try again."
    );
    assert_eq!(
        JOURNAL_LOST,
        "The record of this administrator step could not be saved, so its result is unknown. It is checked again before the next one runs."
    );
}

#[test]
fn classify_table_covers_every_outcome_against_no_report_and_both_reports() {
    // Per leaf part: a report that verifies it, and one that does not.
    let parts: [(Verb, StatusReport, StatusReport); 4] = [
        (
            Verb::AddFirewall(scope()),
            report(DriverState::Absent, FirewallState::Present),
            report(DriverState::Absent, FirewallState::Missing),
        ),
        (
            Verb::RemoveFirewall(scope()),
            report(DriverState::Absent, FirewallState::Missing),
            report(DriverState::Absent, FirewallState::Present),
        ),
        (
            Verb::InstallDriver,
            report(DriverState::Installed, FirewallState::NotRequested),
            report(DriverState::Absent, FirewallState::NotRequested),
        ),
        (
            Verb::RemoveDriver,
            report(DriverState::Absent, FirewallState::NotRequested),
            report(DriverState::Installed, FirewallState::NotRequested),
        ),
    ];
    // Rows are outcomes. Columns are: no report, the verifying report, the other report.
    // RebootRequired wins over verification (ruling L6), and refusals stay refusals.
    let table: [(Option<Outcome>, [PartResult; 3]); 8] = [
        (None, [Unknown, Unknown, Unknown]),
        (Some(Outcome::Done), [Unknown, Verified, Unknown]),
        (Some(Outcome::AlreadyDone), [Unknown, Verified, Unknown]),
        (
            Some(Outcome::RebootRequired),
            [RebootRequired, RebootRequired, RebootRequired],
        ),
        (Some(Outcome::Refused), [Refused, Refused, Refused]),
        (Some(Outcome::NotElevated), [Refused, Refused, Refused]),
        (Some(Outcome::Mismatch), [Refused, Refused, Refused]),
        (Some(Outcome::Failed), [Failed, Failed, Failed]),
    ];
    for (part, verifying, other) in &parts {
        assert!(verified(part, verifying), "{:?} precondition", part.name());
        assert!(!verified(part, other), "{:?} precondition", part.name());
        let afters = [None, Some(verifying), Some(other)];
        for (outcome, expected) in table {
            for (column, after) in afters.into_iter().enumerate() {
                assert_eq!(
                    classify(part, outcome, after),
                    expected[column],
                    "{:?} {outcome:?} column {column}",
                    part.name()
                );
            }
        }
    }
}

#[test]
fn classify_checks_each_part_against_its_own_state() {
    // The firewall report does not verify a driver part, and the driver report does not verify a
    // firewall part.
    let firewall_only = report(DriverState::Absent, FirewallState::Present);
    assert_eq!(
        classify(
            &Verb::InstallDriver,
            Some(Outcome::Done),
            Some(&firewall_only)
        ),
        Unknown
    );
    let driver_only = report(DriverState::Installed, FirewallState::Missing);
    assert_eq!(
        classify(
            &Verb::AddFirewall(scope()),
            Some(Outcome::Done),
            Some(&driver_only)
        ),
        Unknown
    );
}

#[test]
fn constructors_fill_every_part_in_run_order() {
    let setup = Verb::Setup(scope());
    let teardown = Verb::Teardown(scope());

    assert_eq!(
        StepResult::not_run(&setup, REASON),
        StepResult {
            parts: vec![
                (VerbName::AddFirewall, NotRun),
                (VerbName::InstallDriver, NotRun)
            ],
            not_run: Some(REASON),
            journal_failed: false,
        }
    );
    assert_eq!(
        StepResult::declined(&setup),
        StepResult {
            parts: vec![
                (VerbName::AddFirewall, Declined),
                (VerbName::InstallDriver, Declined),
            ],
            not_run: None,
            journal_failed: false,
        }
    );
    assert_eq!(
        StepResult::journal_failure(&setup),
        StepResult {
            parts: vec![
                (VerbName::AddFirewall, Unknown),
                (VerbName::InstallDriver, Unknown),
            ],
            not_run: None,
            journal_failed: true,
        }
    );

    // Teardown runs the driver first.
    assert_eq!(
        StepResult::not_run(&teardown, REASON),
        StepResult {
            parts: vec![
                (VerbName::RemoveDriver, NotRun),
                (VerbName::RemoveFirewall, NotRun),
            ],
            not_run: Some(REASON),
            journal_failed: false,
        }
    );
    assert_eq!(
        StepResult::declined(&teardown).parts,
        vec![
            (VerbName::RemoveDriver, Declined),
            (VerbName::RemoveFirewall, Declined),
        ]
    );
    let lost = StepResult::journal_failure(&teardown);
    assert_eq!(
        lost.parts,
        vec![
            (VerbName::RemoveDriver, Unknown),
            (VerbName::RemoveFirewall, Unknown),
        ]
    );
    assert!(lost.journal_failed);
    assert_eq!(lost.not_run, None);

    // A single verb is one part, named after itself.
    for verb in [
        Verb::AddFirewall(scope()),
        Verb::RemoveFirewall(scope()),
        Verb::InstallDriver,
        Verb::RemoveDriver,
    ] {
        let name = verb.name();
        let not_run = StepResult::not_run(&verb, REASON);
        assert_eq!(not_run.parts, vec![(name, NotRun)]);
        assert_eq!(not_run.not_run, Some(REASON));
        assert!(!not_run.journal_failed);
        assert_eq!(StepResult::declined(&verb).parts, vec![(name, Declined)]);
        let lost = StepResult::journal_failure(&verb);
        assert_eq!(lost.parts, vec![(name, Unknown)]);
        assert!(lost.journal_failed);
    }
}

#[test]
fn launched_classifies_each_part_in_run_order_and_missing_parts_are_unknown() {
    let setup = Verb::Setup(scope());
    let teardown = Verb::Teardown(scope());
    let installed_and_present = report(DriverState::Installed, FirewallState::Present);
    let firewall_only = report(DriverState::Absent, FirewallState::Present);

    // Both parts ran and both verify.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::AddFirewall(scope()), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::AlreadyDone)),
            ],
            Some(&installed_and_present),
        ),
        StepResult {
            parts: vec![
                (VerbName::AddFirewall, Verified),
                (VerbName::InstallDriver, Verified),
            ],
            not_run: None,
            journal_failed: false,
        }
    );

    // Verification is per part: the firewall verifies, but the driver did not install.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::AddFirewall(scope()), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::Done)),
            ],
            Some(&firewall_only),
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Verified),
            (VerbName::InstallDriver, Unknown),
        ]
    );

    // Outcomes are matched to parts, not to their position: the driver is listed first here.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::InstallDriver, Some(Outcome::RebootRequired)),
                (Verb::AddFirewall(scope()), Some(Outcome::Done)),
            ],
            Some(&installed_and_present),
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Verified),
            (VerbName::InstallDriver, RebootRequired),
        ]
    );

    // A part whose outcome is `None` is Unknown, whatever the report says.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::AddFirewall(scope()), None),
                (Verb::InstallDriver, Some(Outcome::Refused)),
            ],
            Some(&installed_and_present),
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Unknown),
            (VerbName::InstallDriver, Refused),
        ]
    );

    // A part missing from the outcomes is Unknown.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[(Verb::AddFirewall(scope()), Some(Outcome::Done))],
            Some(&installed_and_present),
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Verified),
            (VerbName::InstallDriver, Unknown),
        ]
    );

    // Without a report, a Done part is unverified.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::AddFirewall(scope()), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::Done)),
            ],
            None,
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Unknown),
            (VerbName::InstallDriver, Unknown),
        ]
    );

    // Outcomes for verbs that are not parts of this one are ignored.
    assert_eq!(
        StepResult::launched(
            &setup,
            &[
                (Verb::AddFirewall(scope()), Some(Outcome::Done)),
                (Verb::InstallDriver, Some(Outcome::Done)),
                (Verb::RemoveDriver, Some(Outcome::Failed)),
            ],
            Some(&installed_and_present),
        )
        .parts,
        vec![
            (VerbName::AddFirewall, Verified),
            (VerbName::InstallDriver, Verified),
        ]
    );

    // Teardown runs the driver first, so its entries come out in that order.
    assert_eq!(
        StepResult::launched(
            &teardown,
            &[
                (Verb::RemoveFirewall(scope()), Some(Outcome::Done)),
                (Verb::RemoveDriver, Some(Outcome::Failed)),
            ],
            Some(&report(DriverState::Absent, FirewallState::Missing)),
        )
        .parts,
        vec![
            (VerbName::RemoveDriver, Failed),
            (VerbName::RemoveFirewall, Verified),
        ]
    );

    // A single verb is one part, and an outcome for another scope is a different part.
    let add = Verb::AddFirewall(scope());
    let present = report(DriverState::Absent, FirewallState::Present);
    assert_eq!(
        StepResult::launched(&add, &[(add.clone(), Some(Outcome::Done))], Some(&present)).parts,
        vec![(VerbName::AddFirewall, Verified)]
    );
    assert_eq!(
        StepResult::launched(
            &add,
            &[(Verb::AddFirewall(other_scope()), Some(Outcome::Done))],
            Some(&present),
        )
        .parts,
        vec![(VerbName::AddFirewall, Unknown)]
    );
    assert_eq!(
        StepResult::launched(&Verb::InstallDriver, &[], Some(&installed_and_present)).parts,
        vec![(VerbName::InstallDriver, Unknown)]
    );
}

#[test]
fn all_verified_needs_a_non_empty_result_where_every_part_is_verified() {
    let setup = Verb::Setup(scope());
    let verified_setup = StepResult::launched(
        &setup,
        &[
            (Verb::AddFirewall(scope()), Some(Outcome::Done)),
            (Verb::InstallDriver, Some(Outcome::Done)),
        ],
        Some(&report(DriverState::Installed, FirewallState::Present)),
    );
    assert!(verified_setup.all_verified());

    let reboot = StepResult {
        parts: vec![
            (VerbName::AddFirewall, Verified),
            (VerbName::InstallDriver, RebootRequired),
        ],
        not_run: None,
        journal_failed: false,
    };
    assert!(!reboot.all_verified());
    assert!(result_of(&[Verified], false).all_verified());
    assert!(!result_of(&[Verified, Declined], false).all_verified());
    assert!(!result_of(&[Unknown], false).all_verified());
    // An empty result is not verified.
    assert!(!result_of(&[], false).all_verified());
}

#[test]
fn removal_gate_follows_c5_for_each_state_and_mix() {
    for (part, expected) in [
        (Verified, Gate::Proceed),
        (RebootRequired, Gate::Proceed),
        (Declined, Gate::NotSubmitted),
        (NotRun, Gate::NotSubmitted),
        (Refused, Gate::NotSubmitted),
        (Failed, Gate::Unknown),
        (Unknown, Gate::Unknown),
    ] {
        assert_eq!(removal(&[part]), expected, "{part:?} alone");
    }

    // Proceed needs every part verified or rebooting.
    assert_eq!(removal(&[Verified, RebootRequired]), Gate::Proceed);
    assert_eq!(removal(&[RebootRequired, RebootRequired]), Gate::Proceed);
    assert_eq!(removal(&[Verified, Verified]), Gate::Proceed);

    // NotSubmitted needs every part declined, not run or refused.
    assert_eq!(removal(&[Declined, NotRun]), Gate::NotSubmitted);
    assert_eq!(removal(&[NotRun, Refused]), Gate::NotSubmitted);
    assert_eq!(removal(&[Refused, Declined]), Gate::NotSubmitted);

    // Any other mix is Unknown, in either order.
    for mix in [
        [Verified, Declined],
        [Declined, Verified],
        [Verified, NotRun],
        [Verified, Refused],
        [Verified, Failed],
        [Failed, Declined],
        [Failed, NotRun],
        [Unknown, Declined],
        [RebootRequired, Unknown],
        [Verified, Unknown],
    ] {
        assert_eq!(removal(&mix), Gate::Unknown, "{mix:?}");
    }
}

#[test]
fn admin_gate_follows_c5_for_each_state_and_mix() {
    for (part, expected) in [
        (Verified, Gate::Proceed),
        (RebootRequired, Gate::Proceed),
        (Declined, Gate::NotSubmitted),
        (NotRun, Gate::NotSubmitted),
        (Refused, Gate::NotSubmitted),
        // C5 says "else Proceed": a failed part is not Unknown and not all-declined.
        (Failed, Gate::Proceed),
        (Unknown, Gate::Unknown),
    ] {
        assert_eq!(admin(&[part]), expected, "{part:?} alone");
    }

    // Proceed for any mix without an Unknown part and without an all-declined result.
    assert_eq!(admin(&[Verified, RebootRequired]), Gate::Proceed);
    assert_eq!(admin(&[Verified, Declined]), Gate::Proceed);
    assert_eq!(admin(&[Verified, Refused]), Gate::Proceed);
    assert_eq!(admin(&[Failed, Declined]), Gate::Proceed);

    // NotSubmitted needs every part declined, not run or refused.
    assert_eq!(admin(&[Declined, NotRun]), Gate::NotSubmitted);
    assert_eq!(admin(&[Refused, Refused]), Gate::NotSubmitted);
    assert_eq!(admin(&[NotRun, Refused]), Gate::NotSubmitted);

    // Any Unknown part makes the gate Unknown, even beside a declined part.
    for mix in [
        [Unknown, Declined],
        [Declined, Unknown],
        [Verified, Unknown],
        [Unknown, Unknown],
        [NotRun, Unknown],
    ] {
        assert_eq!(admin(&mix), Gate::Unknown, "{mix:?}");
    }
}

#[test]
fn gates_are_unknown_when_the_journal_failed_or_the_result_is_empty() {
    // A lost journal entry stops both gates, whatever the parts say.
    let cases: [&[PartResult]; 5] = [
        &[Verified, Verified],
        &[Declined],
        &[NotRun, Refused],
        &[RebootRequired],
        &[],
    ];
    for parts in cases {
        assert_eq!(
            removal_gate(&result_of(parts, true)),
            Gate::Unknown,
            "{parts:?} removal"
        );
        assert_eq!(
            admin_gate(&result_of(parts, true)),
            Gate::Unknown,
            "{parts:?} admin"
        );
    }
    let lost_setup = StepResult::journal_failure(&Verb::Setup(scope()));
    assert_eq!(removal_gate(&lost_setup), Gate::Unknown);
    assert_eq!(admin_gate(&lost_setup), Gate::Unknown);
    let lost_teardown = StepResult::journal_failure(&Verb::Teardown(scope()));
    assert_eq!(removal_gate(&lost_teardown), Gate::Unknown);
    assert_eq!(admin_gate(&lost_teardown), Gate::Unknown);

    // An empty result is Unknown for both gates: nothing was reported, so nothing is known.
    assert_eq!(removal(&[]), Gate::Unknown);
    assert_eq!(admin(&[]), Gate::Unknown);
}

#[test]
fn constructed_results_gate_as_their_constructors_say() {
    let setup = Verb::Setup(scope());
    let teardown = Verb::Teardown(scope());

    // Nothing ran, or the person declined: the gates say the step was not submitted.
    assert_eq!(
        removal_gate(&StepResult::not_run(&teardown, REASON)),
        Gate::NotSubmitted
    );
    assert_eq!(
        admin_gate(&StepResult::not_run(&setup, REASON)),
        Gate::NotSubmitted
    );
    assert_eq!(
        removal_gate(&StepResult::declined(&teardown)),
        Gate::NotSubmitted
    );
    assert_eq!(
        admin_gate(&StepResult::declined(&setup)),
        Gate::NotSubmitted
    );

    // Every part verified: both gates proceed.
    let removed = StepResult::launched(
        &teardown,
        &[
            (Verb::RemoveDriver, Some(Outcome::Done)),
            (Verb::RemoveFirewall(scope()), Some(Outcome::Done)),
        ],
        Some(&report(DriverState::Absent, FirewallState::Missing)),
    );
    assert_eq!(removal_gate(&removed), Gate::Proceed);
    assert_eq!(admin_gate(&removed), Gate::Proceed);

    // A part that needs a restart counts as done for both gates.
    let reboot = StepResult::launched(
        &setup,
        &[
            (Verb::AddFirewall(scope()), Some(Outcome::Done)),
            (Verb::InstallDriver, Some(Outcome::RebootRequired)),
        ],
        Some(&report(DriverState::Installed, FirewallState::Present)),
    );
    assert_eq!(removal_gate(&reboot), Gate::Proceed);
    assert_eq!(admin_gate(&reboot), Gate::Proceed);

    // A part with no outcome is Unknown for both gates.
    let unknown = StepResult::launched(
        &setup,
        &[(Verb::AddFirewall(scope()), Some(Outcome::Done))],
        Some(&report(DriverState::Installed, FirewallState::Present)),
    );
    assert_eq!(removal_gate(&unknown), Gate::Unknown);
    assert_eq!(admin_gate(&unknown), Gate::Unknown);
}

#[test]
fn report_lines_use_the_frozen_wording_for_every_state_and_subject() {
    // The wording of every state that reads the same for both subjects. NotRun uses `REASON`.
    let states: [(PartResult, &str); 6] = [
        (RebootRequired, "Windows needs a restart to finish"),
        (
            Declined,
            "unchanged, because administrator approval was declined",
        ),
        (
            Refused,
            "unchanged; Windows or an existing conflicting object refused the change",
        ),
        (
            Failed,
            "could not be changed; anything this step created was undone",
        ),
        (
            Unknown,
            "result unknown; it is checked again before the next administrator step",
        ),
        (NotRun, "unchanged (a test reason)"),
    ];
    // Each verb's subject, and the Verified wording of that verb.
    let verbs = [
        (
            VerbName::AddFirewall,
            "Windows Defender Firewall rule",
            "added",
        ),
        (
            VerbName::RemoveFirewall,
            "Windows Defender Firewall rule",
            "removed",
        ),
        (
            VerbName::InstallDriver,
            "Crosspane display driver",
            "installed",
        ),
        (
            VerbName::RemoveDriver,
            "Crosspane display driver",
            "removed",
        ),
    ];
    for (name, subject, verified_word) in verbs {
        let mut rows: Vec<(PartResult, &str)> = vec![(Verified, verified_word)];
        rows.extend(states);
        for (part, wording) in rows {
            let result = StepResult {
                parts: vec![(name, part)],
                not_run: Some(REASON),
                journal_failed: false,
            };
            assert_eq!(
                report_lines(&result),
                vec![format!("{subject}: {wording}.")],
                "{name:?} {part:?}"
            );
        }
    }
}

#[test]
fn report_lines_name_the_reason_and_append_journal_lost_only_when_the_journal_failed() {
    let lost = StepResult::journal_failure(&Verb::Setup(scope()));
    assert_eq!(
        report_lines(&lost),
        [
            "Windows Defender Firewall rule: result unknown; it is checked again before the next administrator step.",
            "Crosspane display driver: result unknown; it is checked again before the next administrator step.",
            "The record of this administrator step could not be saved, so its result is unknown. It is checked again before the next one runs.",
        ]
    );

    // A verified part whose journal entry was lost still ends with the line.
    let verified_but_lost = StepResult {
        parts: vec![(VerbName::AddFirewall, Verified)],
        not_run: None,
        journal_failed: true,
    };
    assert_eq!(
        report_lines(&verified_but_lost),
        [
            "Windows Defender Firewall rule: added.",
            "The record of this administrator step could not be saved, so its result is unknown. It is checked again before the next one runs.",
        ]
    );

    // Without the flag there is no journal line: one line per part.
    let plain = report_lines(&StepResult::declined(&Verb::Teardown(scope())));
    assert_eq!(plain.len(), 2);
    assert!(plain.iter().all(|line| line.as_str() != JOURNAL_LOST));

    // The reason is written for every NotRun part.
    assert_eq!(
        report_lines(&StepResult::not_run(&Verb::Setup(scope()), REASON)),
        [
            "Windows Defender Firewall rule: unchanged (a test reason).",
            "Crosspane display driver: unchanged (a test reason).",
        ]
    );

    // C5 gives no wording for a NotRun part without a reason, so the code writes plain "unchanged".
    let bare = StepResult {
        parts: vec![(VerbName::InstallDriver, NotRun)],
        not_run: None,
        journal_failed: false,
    };
    assert_eq!(
        report_lines(&bare),
        ["Crosspane display driver: unchanged."]
    );
}

#[test]
fn status_summary_text_and_configured_flag_for_every_state() {
    let firewall_words = [
        (FirewallState::Present, "present"),
        (FirewallState::Missing, "missing"),
        (FirewallState::Mismatch, "different from Crosspane's rule"),
        (FirewallState::Unavailable, "unreadable"),
        (FirewallState::NotRequested, "not checked"),
    ];
    let driver_words = [
        (DriverState::Absent, "not installed"),
        (DriverState::PackageOnly, "partly installed"),
        (DriverState::DeviceWithoutDriver, "partly installed"),
        (DriverState::Installed, "installed"),
        (DriverState::Mismatch, "different from Crosspane's driver"),
        (DriverState::Unavailable, "unreadable"),
    ];
    for (firewall, firewall_text) in firewall_words {
        for (driver, driver_text) in driver_words {
            let observed = report(driver, firewall);
            // Configured is what `Setup` reaches: the rule present and the driver installed.
            let configured = firewall == FirewallState::Present && driver == DriverState::Installed;
            let text = format!("Firewall rule: {firewall_text}. Display driver: {driver_text}.");
            assert_eq!(
                status_summary(&observed, false),
                (configured, text.clone()),
                "{firewall:?} {driver:?}"
            );
            assert_eq!(
                status_summary(&observed, true),
                (configured, format!("{text}{PENDING_NOTE}")),
                "{firewall:?} {driver:?} pending"
            );
        }
    }
}

#[test]
fn status_summary_appends_the_pending_note_only_when_pending() {
    let observed = report(DriverState::Installed, FirewallState::Present);
    assert_eq!(
        status_summary(&observed, false),
        (
            true,
            "Firewall rule: present. Display driver: installed.".to_owned()
        )
    );
    assert_eq!(
        status_summary(&observed, true),
        (
            true,
            "Firewall rule: present. Display driver: installed. An earlier administrator step was interrupted; it is checked again before the next one runs.".to_owned()
        )
    );
}

#[test]
fn plan_refuses_the_non_mutating_status_verb() {
    assert_eq!(
        ElevatedPlan::new(Verb::Status(None)),
        Err(ElevatedError::Consent)
    );
    assert_eq!(
        ElevatedPlan::new(Verb::Status(Some(scope()))),
        Err(ElevatedError::Consent)
    );
}

#[test]
fn plan_lines_preview_block_and_consent_come_from_describe() {
    let verbs = [
        Verb::InstallDriver,
        Verb::RemoveDriver,
        Verb::AddFirewall(scope()),
        Verb::RemoveFirewall(scope()),
        Verb::Setup(scope()),
        Verb::Teardown(scope()),
    ];
    for verb in verbs {
        let plan = ElevatedPlan::new(verb.clone()).unwrap();
        assert_eq!(plan.verb(), &verb);
        assert_eq!(plan.lines(), describe(&verb).as_slice());

        let block = plan.preview_block();
        assert_eq!(
            block,
            format!("{PREVIEW_HEADER}\n{}", plan.lines().join("\n"))
        );
        assert_eq!(block.split('\n').count(), plan.lines().len() + 1);

        let consent = plan.consent().unwrap();
        assert_eq!(consent.verb(), verb.name());
        assert_eq!(consent.action(), &verb);
    }

    // Setup shows the firewall line first, then the two driver lines.
    let setup = ElevatedPlan::new(Verb::Setup(scope())).unwrap();
    assert_eq!(setup.lines().len(), 3);
    assert_eq!(setup.lines()[0], describe(&Verb::AddFirewall(scope()))[0]);
    assert_eq!(setup.lines()[1], describe(&Verb::InstallDriver)[0]);
}

#[test]
fn preview_bound_counts_bytes_and_refuses_one_over() {
    // ASCII: one byte per character. Add copies until the block is exactly MAX_PREVIEW_BLOCK
    // bytes long. One more copy is one byte over, and it is refused.
    let one = block_text(&Verb::AddFirewall(padded_scope("x", 1))).len();
    let count = 1 + MAX_PREVIEW_BLOCK - one;

    let exact = Verb::AddFirewall(padded_scope("x", count));
    assert_eq!(block_text(&exact).len(), MAX_PREVIEW_BLOCK);
    let plan = ElevatedPlan::new(exact).unwrap();
    assert_eq!(plan.preview_block().len(), MAX_PREVIEW_BLOCK);

    let over = Verb::AddFirewall(padded_scope("x", count + 1));
    assert_eq!(block_text(&over).len(), MAX_PREVIEW_BLOCK + 1);
    assert_eq!(ElevatedPlan::new(over), Err(ElevatedError::Consent));

    // Setup adds the driver lines to the firewall line, so the same path is over the bound.
    let setup = Verb::Setup(padded_scope("x", count));
    assert!(block_text(&setup).len() > MAX_PREVIEW_BLOCK);
    assert_eq!(ElevatedPlan::new(setup), Err(ElevatedError::Consent));

    // Two-byte filler: each copy adds two bytes and one character. Take the fewest copies whose
    // block is over the bound in bytes. Its character count is under the bound, so a character
    // bound would accept it; the byte bound refuses it. One copy fewer is within the bound.
    let copies = (1..)
        .find(|&copies| {
            block_text(&Verb::AddFirewall(padded_scope("é", copies))).len() > MAX_PREVIEW_BLOCK
        })
        .unwrap();
    let two_byte_over = block_text(&Verb::AddFirewall(padded_scope("é", copies)));
    assert!(two_byte_over.chars().count() < MAX_PREVIEW_BLOCK);
    assert!(two_byte_over.len() > MAX_PREVIEW_BLOCK);
    assert_eq!(
        ElevatedPlan::new(Verb::AddFirewall(padded_scope("é", copies))),
        Err(ElevatedError::Consent)
    );

    let two_byte_fits = Verb::AddFirewall(padded_scope("é", copies - 1));
    assert!(block_text(&two_byte_fits).len() <= MAX_PREVIEW_BLOCK);
    assert!(ElevatedPlan::new(two_byte_fits).is_ok());
}
