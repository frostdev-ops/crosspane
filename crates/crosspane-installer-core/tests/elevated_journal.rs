#![allow(clippy::unwrap_used)] // Test fixtures may assert successful setup.
use crosspane_installer_core::elevated::journal::{
    ElevatedRecord, MAX_ENTRIES, RECORD_SCHEMA, journaled,
};
use crosspane_installer_core::elevated::status::{
    DriverStatus, FirewallStatus, JournalEntry, JournalPhase, STATUS_SCHEMA, StatusReport,
    intent_entry, outcome_entry,
};
use crosspane_installer_core::elevated::{
    AgentProgram, DriverState, ElevatedError, FirewallState, HARDWARE_ID, InstallId, Outcome,
    RuleScope, Verb, VerbName,
};
use crosspane_installer_core::{MutationOutcome, ResourceObservation, ResourceOwnership};
use serde_json::json;

const PROGRAM: &str = r"C:\Users\user\AppData\Local\Programs\Crosspane\crosspane-agent.exe";

fn scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-1").unwrap(),
        program: AgentProgram::parse(PROGRAM).unwrap(),
    }
}

/// Another install on the same machine. Its firewall rule has a different name.
fn other_scope() -> RuleScope {
    RuleScope {
        id: InstallId::parse("w41c-vm-2").unwrap(),
        program: AgentProgram::parse(PROGRAM).unwrap(),
    }
}

fn record() -> ElevatedRecord {
    ElevatedRecord::new(&scope())
}

/// A record for `scope()` holding `entries` as they are. Validation is what the tests check.
fn with_entries(entries: Vec<JournalEntry>) -> ElevatedRecord {
    ElevatedRecord {
        entries,
        ..record()
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

/// The four verbs the journal records, all with `scope()`.
fn journaled_verbs() -> Vec<Verb> {
    vec![
        Verb::AddFirewall(scope()),
        Verb::RemoveFirewall(scope()),
        Verb::InstallDriver,
        Verb::RemoveDriver,
    ]
}

/// The Intent written before `verb` runs.
fn intent(verb: &Verb) -> JournalEntry {
    intent_entry(verb, None)
}

/// The Outcome of a run that exited `Done`, with no observation.
fn settle(verb: &Verb) -> JournalEntry {
    outcome_entry(verb, Some(Outcome::Done), None)
}

/// The Intent and Outcome of one settled run of `verb`.
fn pair(verb: &Verb) -> [JournalEntry; 2] {
    [intent(verb), settle(verb)]
}

/// The settled pair of `verb` as a record, with `change` applied to its entry at `index` (0 is the
/// Intent, 1 the Outcome).
fn pair_changed(
    verb: &Verb,
    index: usize,
    change: impl FnOnce(&mut JournalEntry),
) -> ElevatedRecord {
    let mut entries = pair(verb).to_vec();
    change(&mut entries[index]);
    with_entries(entries)
}

#[test]
fn intent_then_outcome_settles_each_journaled_verb() {
    for verb in journaled_verbs() {
        let mut journal = record();
        journal.append(&intent(&verb)).unwrap();
        assert_eq!(journal.pending(), vec![verb.name()]);
        journal.append(&settle(&verb)).unwrap();
        assert!(
            journal.pending().is_empty(),
            "{} still pending",
            verb.name().as_str()
        );
        assert_eq!(journal.entries.len(), 2);
        assert_eq!(journal.validate(), Ok(()));
    }
}

#[test]
fn outcome_without_a_pending_intent_is_refused() {
    for verb in journaled_verbs() {
        let mut journal = record();
        assert_eq!(
            journal.append(&settle(&verb)),
            Err(ElevatedError::Journal),
            "{} outcome with no intent",
            verb.name().as_str()
        );
        assert!(journal.entries.is_empty());

        // A second Outcome for a settled verb has nothing pending to settle.
        journal.append(&intent(&verb)).unwrap();
        journal.append(&settle(&verb)).unwrap();
        let settled = journal.clone();
        assert_eq!(
            journal.append(&settle(&verb)),
            Err(ElevatedError::Journal),
            "{} second outcome",
            verb.name().as_str()
        );
        assert_eq!(journal, settled);
    }
}

#[test]
fn second_intent_for_a_pending_verb_is_refused() {
    for verb in journaled_verbs() {
        let mut journal = record();
        journal.append(&intent(&verb)).unwrap();
        let pending = journal.clone();
        assert_eq!(
            journal.append(&intent(&verb)),
            Err(ElevatedError::Journal),
            "{} second intent",
            verb.name().as_str()
        );
        assert_eq!(journal, pending);

        // Once its Outcome is written, the same verb may be intended again.
        journal.append(&settle(&verb)).unwrap();
        journal.append(&intent(&verb)).unwrap();
        assert_eq!(journal.pending(), vec![verb.name()]);
    }
}

#[test]
fn interleaved_verbs_are_accepted() {
    let add = Verb::AddFirewall(scope());
    let driver = Verb::InstallDriver;
    let mut journal = record();
    journal.append(&intent(&add)).unwrap();
    journal.append(&intent(&driver)).unwrap();
    assert_eq!(
        journal.pending(),
        vec![VerbName::AddFirewall, VerbName::InstallDriver]
    );
    journal.append(&settle(&add)).unwrap();
    assert_eq!(journal.pending(), vec![VerbName::InstallDriver]);
    journal.append(&settle(&driver)).unwrap();
    assert!(journal.pending().is_empty());
    assert_eq!(journal.validate(), Ok(()));
}

#[test]
fn validate_replays_the_protocol_in_order() {
    let add = Verb::AddFirewall(scope());
    let refused = [
        vec![settle(&add)],
        vec![intent(&add), intent(&add)],
        vec![intent(&add), settle(&add), settle(&add)],
    ];
    for entries in refused {
        assert_eq!(with_entries(entries).validate(), Err(ElevatedError::Record));
    }

    let interleaved = vec![
        intent(&add),
        intent(&Verb::InstallDriver),
        settle(&add),
        settle(&Verb::InstallDriver),
    ];
    assert_eq!(with_entries(interleaved).validate(), Ok(()));
}

#[test]
fn validate_refuses_any_schema_but_one() {
    assert_eq!(RECORD_SCHEMA, 1);
    let entries = pair(&Verb::AddFirewall(scope())).to_vec();
    assert_eq!(with_entries(entries.clone()).validate(), Ok(()));
    for schema in [0, RECORD_SCHEMA + 1, u32::MAX] {
        let journal = ElevatedRecord {
            schema,
            ..with_entries(entries.clone())
        };
        assert_eq!(
            journal.validate(),
            Err(ElevatedError::Record),
            "schema {schema}"
        );
    }
}

#[test]
fn validate_bounds_the_entry_count_at_max_entries() {
    assert_eq!(MAX_ENTRIES, 32);
    let add = Verb::AddFirewall(scope());
    let full: Vec<JournalEntry> = (0..MAX_ENTRIES / 2).flat_map(|_| pair(&add)).collect();
    assert_eq!(full.len(), MAX_ENTRIES);
    assert_eq!(with_entries(full.clone()).validate(), Ok(()));

    // One more Intent makes 33 entries. The protocol is still valid, so only the bound refuses it.
    let mut over = full;
    over.push(intent(&Verb::InstallDriver));
    assert_eq!(with_entries(over).validate(), Err(ElevatedError::Record));
}

#[test]
fn validate_refuses_entries_of_verbs_that_are_not_journaled() {
    let not_journaled = [
        Verb::Status(None),
        Verb::Status(Some(scope())),
        Verb::Setup(scope()),
        Verb::Teardown(scope()),
    ];
    for verb in not_journaled {
        assert!(!journaled(verb.name()));
        assert_eq!(
            with_entries(vec![intent(&verb)]).validate(),
            Err(ElevatedError::Record),
            "{} intent",
            verb.name().as_str()
        );
        assert_eq!(
            with_entries(pair(&verb).to_vec()).validate(),
            Err(ElevatedError::Record),
            "{} pair",
            verb.name().as_str()
        );
    }
}

#[test]
fn validate_refuses_entries_that_the_helper_did_not_create() {
    for ownership in [ResourceOwnership::Adopted, ResourceOwnership::Foreign] {
        for verb in journaled_verbs() {
            for index in 0..2 {
                let journal = pair_changed(&verb, index, |entry| {
                    entry.receipt.ownership = ownership;
                });
                assert_eq!(
                    journal.validate(),
                    Err(ElevatedError::Record),
                    "{ownership:?} {} at {index}",
                    verb.name().as_str()
                );
            }
        }
    }
}

#[test]
fn validate_refuses_a_resource_id_that_is_not_the_verbs() {
    let wrong = [
        (Verb::AddFirewall(scope()), "windows-idd-driver"),
        (Verb::RemoveFirewall(scope()), "windows-elevated-setup"),
        (Verb::InstallDriver, "windows-firewall-rule"),
        (Verb::RemoveDriver, ""),
    ];
    for (verb, resource_id) in wrong {
        for index in 0..2 {
            let journal = pair_changed(&verb, index, |entry| {
                entry.receipt.resource_id = resource_id.to_owned();
            });
            assert_eq!(
                journal.validate(),
                Err(ElevatedError::Record),
                "{} resource id {resource_id:?} at {index}",
                verb.name().as_str()
            );
        }
    }
}

#[test]
fn validate_refuses_a_resolved_path_that_is_not_the_verbs() {
    // A firewall verb names its own install's rule. Another install's rule, the hardware ID and an
    // empty path are all refused.
    let firewall: [fn(RuleScope) -> Verb; 2] = [Verb::AddFirewall, Verb::RemoveFirewall];
    for make in firewall {
        let verb = make(scope());
        let foreign = intent_entry(&make(other_scope()), None);
        assert_eq!(
            with_entries(vec![foreign]).validate(),
            Err(ElevatedError::Record),
            "{} entry of another install",
            verb.name().as_str()
        );
        for path in [
            other_scope().id.rule_name(),
            HARDWARE_ID.to_owned(),
            String::new(),
        ] {
            for index in 0..2 {
                let journal = pair_changed(&verb, index, |entry| {
                    entry.receipt.resolved_path = path.clone();
                });
                assert_eq!(
                    journal.validate(),
                    Err(ElevatedError::Record),
                    "{} path {path:?} at {index}",
                    verb.name().as_str()
                );
            }
        }
    }

    // A driver verb names the hardware ID, not a rule name.
    for verb in [Verb::InstallDriver, Verb::RemoveDriver] {
        for path in [
            scope().id.rule_name(),
            r"Crosspane\IddTwinV2".to_owned(),
            String::new(),
        ] {
            for index in 0..2 {
                let journal = pair_changed(&verb, index, |entry| {
                    entry.receipt.resolved_path = path.clone();
                });
                assert_eq!(
                    journal.validate(),
                    Err(ElevatedError::Record),
                    "{} path {path:?} at {index}",
                    verb.name().as_str()
                );
            }
        }
    }
}

#[test]
fn validate_refuses_an_intent_that_already_carries_an_exit_or_an_observation() {
    for verb in journaled_verbs() {
        for exit in [0, 64] {
            let entry = JournalEntry {
                exit: Some(exit),
                ..intent(&verb)
            };
            assert_eq!(
                with_entries(vec![entry]).validate(),
                Err(ElevatedError::Record),
                "{} intent with exit {exit}",
                verb.name().as_str()
            );
        }
        for observation in [
            ResourceObservation::Absent,
            ResourceObservation::Matching,
            ResourceObservation::Different,
        ] {
            let mut entry = intent(&verb);
            entry.receipt.after = observation;
            assert_eq!(
                with_entries(vec![entry]).validate(),
                Err(ElevatedError::Record),
                "{} intent after {observation:?}",
                verb.name().as_str()
            );
        }
        for outcome in [
            MutationOutcome::Verified,
            MutationOutcome::Refused,
            MutationOutcome::Failed,
        ] {
            let mut entry = intent(&verb);
            entry.receipt.outcome = outcome;
            assert_eq!(
                with_entries(vec![entry]).validate(),
                Err(ElevatedError::Record),
                "{} intent outcome {outcome:?}",
                verb.name().as_str()
            );
        }

        // An Intent may show the state observed before the verb ran, and an Outcome carries its exit
        // and the state observed after.
        let observed = report(DriverState::Installed, FirewallState::Present);
        let before = intent_entry(&verb, Some(&observed));
        assert_eq!(with_entries(vec![before]).validate(), Ok(()));
        let settled = outcome_entry(&verb, Some(Outcome::Done), Some(&observed));
        assert_eq!(
            with_entries(vec![intent(&verb), settled]).validate(),
            Ok(())
        );
    }
}

#[test]
fn refused_append_leaves_the_record_unchanged_and_is_a_journal_error() {
    let add = Verb::AddFirewall(scope());
    let mut journal = with_entries(pair(&add).to_vec());
    journal.append(&intent(&Verb::InstallDriver)).unwrap();
    let before = journal.clone();

    let mut owned_by_other = intent(&Verb::RemoveDriver);
    owned_by_other.receipt.ownership = ResourceOwnership::Adopted;
    let mut with_exit = intent(&Verb::RemoveDriver);
    with_exit.exit = Some(0);
    let mut with_after = intent(&Verb::RemoveDriver);
    with_after.receipt.after = ResourceObservation::Absent;
    let mut with_outcome = intent(&Verb::RemoveDriver);
    with_outcome.receipt.outcome = MutationOutcome::Failed;
    let mut wrong_id = settle(&Verb::InstallDriver);
    wrong_id.receipt.resource_id = "windows-firewall-rule".to_owned();

    let refused = [
        // An Outcome for a verb that already settled, or never ran.
        settle(&add),
        settle(&Verb::RemoveDriver),
        // An Intent for a verb that is still pending.
        intent(&Verb::InstallDriver),
        // Verbs the journal does not record, and another install's rule.
        intent(&Verb::Status(None)),
        intent(&Verb::Setup(scope())),
        intent(&Verb::AddFirewall(other_scope())),
        // Identity and shape faults.
        owned_by_other,
        with_exit,
        with_after,
        with_outcome,
        wrong_id,
    ];
    for entry in refused {
        assert_eq!(
            journal.append(&entry),
            Err(ElevatedError::Journal),
            "{:?} {:?}",
            entry.phase,
            entry.verb
        );
        assert_eq!(journal, before);
    }

    // A record whose schema is not 1 refuses every append, and stays empty.
    let mut bad_schema = ElevatedRecord {
        schema: 2,
        ..record()
    };
    assert_eq!(
        bad_schema.append(&intent(&add)),
        Err(ElevatedError::Journal)
    );
    assert!(bad_schema.entries.is_empty());
}

#[test]
fn append_drops_the_earliest_settled_pair_past_max_entries() {
    assert_eq!(MAX_ENTRIES, 32);
    let verbs = journaled_verbs();
    let mut journal = record();
    for verb in verbs.iter().cycle().take(MAX_ENTRIES / 2) {
        journal.append(&intent(verb)).unwrap();
        journal.append(&settle(verb)).unwrap();
    }
    assert_eq!(journal.entries.len(), MAX_ENTRIES);
    let before = journal.entries.clone();

    // The 33rd entry forces out the first pair, the AddFirewall pair written before any other.
    let newest = intent(&Verb::RemoveDriver);
    journal.append(&newest).unwrap();
    assert_eq!(journal.entries.len(), MAX_ENTRIES - 1);
    let mut expected = before[2..].to_vec();
    expected.push(newest.clone());
    assert_eq!(journal.entries, expected);
    assert_eq!(journal.entries[0].phase, JournalPhase::Intent);
    assert_eq!(journal.entries[0].verb, VerbName::RemoveFirewall);
    assert_eq!(journal.entries.last(), Some(&newest));
    assert_eq!(journal.pending(), vec![VerbName::RemoveDriver]);
    assert_eq!(journal.validate(), Ok(()));
}

#[test]
fn append_refuses_when_no_settled_pair_can_be_dropped() {
    // Every valid record has a settled pair once it holds more than four entries. At most four
    // journaled verbs can be pending, and each verb's entries alternate Intent and Outcome. So
    // this record is built directly, with 32 Intents and nothing settled. Its validation fails.
    let add = Verb::AddFirewall(scope());
    let mut journal = record();
    journal.entries = (0..MAX_ENTRIES).map(|_| intent(&add)).collect();
    assert_eq!(journal.validate(), Err(ElevatedError::Record));
    let before = journal.clone();

    assert_eq!(
        journal.append(&intent(&Verb::InstallDriver)),
        Err(ElevatedError::Journal)
    );
    assert_eq!(journal, before);
}

/// A full record whose front entry is a pending AddFirewall Intent. Settled pairs of RemoveFirewall,
/// InstallDriver and RemoveDriver fill the middle, and a pending RemoveDriver Intent ends it.
fn full_record_with_pending_front() -> ElevatedRecord {
    let add = Verb::AddFirewall(scope());
    let settled_verbs = [
        Verb::RemoveFirewall(scope()),
        Verb::InstallDriver,
        Verb::RemoveDriver,
    ];
    let mut journal = record();
    journal.append(&intent(&add)).unwrap();
    for verb in settled_verbs.iter().cycle().take(MAX_ENTRIES / 2 - 1) {
        journal.append(&intent(verb)).unwrap();
        journal.append(&settle(verb)).unwrap();
    }
    journal.append(&intent(&Verb::RemoveDriver)).unwrap();
    assert_eq!(journal.entries.len(), MAX_ENTRIES);
    assert_eq!(
        journal.pending(),
        vec![VerbName::AddFirewall, VerbName::RemoveDriver]
    );
    journal
}

#[test]
fn append_keeps_a_pending_intent_at_the_front_when_trimming() {
    let mut journal = full_record_with_pending_front();
    let before = journal.entries.clone();
    let pending = journal.pending();

    // The 33rd entry is an InstallDriver Intent, which is not pending. The earliest settled pair,
    // the RemoveFirewall pair at 1 and 2, is dropped. The front AddFirewall Intent stays pending.
    let newest = intent(&Verb::InstallDriver);
    journal.append(&newest).unwrap();
    assert_eq!(journal.entries.len(), MAX_ENTRIES - 1);
    let mut expected = vec![before[0].clone()];
    expected.extend_from_slice(&before[3..]);
    expected.push(newest);
    assert_eq!(journal.entries, expected);
    assert_eq!(journal.entries[0], intent(&Verb::AddFirewall(scope())));

    let mut expected_pending = pending;
    expected_pending.push(VerbName::InstallDriver);
    assert_eq!(journal.pending(), expected_pending);
    assert_eq!(journal.validate(), Ok(()));
}

#[test]
fn append_never_drops_the_outcome_it_writes() {
    let add = Verb::AddFirewall(scope());
    let mut journal = full_record_with_pending_front();
    let before = journal.entries.clone();

    // The 33rd entry is the Outcome of the pending front AddFirewall Intent. That pair now includes
    // the new entry, so the earliest other settled pair (RemoveFirewall, at 1 and 2) is dropped.
    let outcome = settle(&add);
    journal.append(&outcome).unwrap();
    assert_eq!(journal.entries.len(), MAX_ENTRIES - 1);
    assert_eq!(journal.entries.last(), Some(&outcome));

    // The front Intent and the new Outcome are now a settled pair. Every other entry is kept in order.
    let mut expected = vec![before[0].clone()];
    expected.extend_from_slice(&before[3..]);
    expected.push(outcome);
    assert_eq!(journal.entries, expected);
    assert_eq!(journal.pending(), vec![VerbName::RemoveDriver]);
    assert_eq!(journal.validate(), Ok(()));
}

#[test]
fn append_refuses_when_the_only_settled_pair_holds_the_new_entry() {
    // A valid record of MAX_ENTRIES entries always holds other settled pairs. At most four Intents
    // are pending, and every other entry is half of a settled pair. So this record is built through
    // its public fields: a front AddFirewall Intent, then unpaired RemoveFirewall Intents. The only
    // settled pair is the front Intent with the Outcome appended below.
    let add = Verb::AddFirewall(scope());
    let mut journal = record();
    journal.entries = std::iter::once(intent(&add))
        .chain((1..MAX_ENTRIES).map(|_| intent(&Verb::RemoveFirewall(scope()))))
        .collect();
    assert_eq!(journal.entries.len(), MAX_ENTRIES);
    assert_eq!(journal.validate(), Err(ElevatedError::Record));
    let before = journal.clone();

    assert_eq!(journal.append(&settle(&add)), Err(ElevatedError::Journal));
    assert_eq!(journal, before);
}

#[test]
fn pending_lists_unsettled_verbs_in_intent_order_with_the_record_scope() {
    let mut journal = record();
    for verb in [
        Verb::RemoveDriver,
        Verb::RemoveFirewall(scope()),
        Verb::AddFirewall(scope()),
        Verb::InstallDriver,
    ] {
        journal.append(&intent(&verb)).unwrap();
    }
    assert_eq!(
        journal.pending(),
        vec![
            VerbName::RemoveDriver,
            VerbName::RemoveFirewall,
            VerbName::AddFirewall,
            VerbName::InstallDriver,
        ]
    );
    assert_eq!(
        journal.pending_verbs(),
        vec![
            Verb::RemoveDriver,
            Verb::RemoveFirewall(scope()),
            Verb::AddFirewall(scope()),
            Verb::InstallDriver,
        ]
    );

    journal.append(&settle(&Verb::RemoveDriver)).unwrap();
    journal
        .append(&settle(&Verb::AddFirewall(scope())))
        .unwrap();
    assert_eq!(
        journal.pending(),
        vec![VerbName::RemoveFirewall, VerbName::InstallDriver]
    );
    let pending = journal.pending_verbs();
    assert_eq!(
        pending,
        vec![Verb::RemoveFirewall(scope()), Verb::InstallDriver]
    );
    assert_eq!(pending[0].scope(), Some(&scope()));
    assert_eq!(pending[1].scope(), None);

    journal
        .append(&settle(&Verb::RemoveFirewall(scope())))
        .unwrap();
    journal.append(&settle(&Verb::InstallDriver)).unwrap();
    assert!(journal.pending().is_empty());
    assert!(journal.pending_verbs().is_empty());
}

#[test]
fn pending_firewall_verbs_carry_the_records_own_scope() {
    let mut journal = ElevatedRecord::new(&other_scope());
    journal
        .append(&intent(&Verb::RemoveFirewall(other_scope())))
        .unwrap();
    assert_eq!(
        journal.pending_verbs(),
        vec![Verb::RemoveFirewall(other_scope())]
    );
    assert_ne!(journal.pending_verbs(), vec![Verb::RemoveFirewall(scope())]);
}

#[test]
fn journaled_is_true_only_for_the_firewall_and_driver_mutations() {
    for name in [
        VerbName::AddFirewall,
        VerbName::RemoveFirewall,
        VerbName::InstallDriver,
        VerbName::RemoveDriver,
    ] {
        assert!(journaled(name), "{} should be journaled", name.as_str());
    }
    for name in [VerbName::Status, VerbName::Setup, VerbName::Teardown] {
        assert!(
            !journaled(name),
            "{} should not be journaled",
            name.as_str()
        );
    }
}

#[test]
fn new_record_is_empty_and_reports_the_scope_it_was_made_for() {
    let journal = ElevatedRecord::new(&scope());
    assert_eq!(journal.schema, RECORD_SCHEMA);
    assert_eq!(journal.install_id, scope().id);
    assert_eq!(journal.program, scope().program);
    assert!(journal.entries.is_empty());
    assert!(journal.pending().is_empty());
    assert_eq!(journal.validate(), Ok(()));
    assert_eq!(journal.scope(), scope());
}

#[test]
fn record_round_trips_through_json() {
    let add = Verb::AddFirewall(scope());
    let mut journal = record();
    journal.append(&intent(&add)).unwrap();
    journal.append(&settle(&add)).unwrap();
    journal.append(&intent(&Verb::InstallDriver)).unwrap();

    let text = serde_json::to_string(&journal).unwrap();
    let back: ElevatedRecord = serde_json::from_str(&text).unwrap();
    assert_eq!(back, journal);
    assert_eq!(back.validate(), Ok(()));
    assert_eq!(back.pending(), vec![VerbName::InstallDriver]);

    // The wire names: the schema, the install ID, and the verb in kebab case.
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["schema"], json!(1));
    assert_eq!(value["install_id"], json!("w41c-vm-1"));
    assert_eq!(value["entries"][0]["verb"], json!("add-firewall"));
}

#[test]
fn json_refuses_unknown_fields_at_the_record_and_entry_level() {
    let mut journal = record();
    journal
        .append(&intent(&Verb::AddFirewall(scope())))
        .unwrap();
    let value = serde_json::to_value(&journal).unwrap();
    assert!(serde_json::from_value::<ElevatedRecord>(value.clone()).is_ok());

    let mut top = value.clone();
    top["extra"] = json!(1);
    assert!(serde_json::from_value::<ElevatedRecord>(top).is_err());

    let mut nested = value;
    nested["entries"][0]["extra"] = json!(1);
    assert!(serde_json::from_value::<ElevatedRecord>(nested).is_err());
}
