#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! In-memory task activation graph only. No COM, task, process or native authority constructor runs.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(windows)]
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use native_io::{
    NativeError, NativeResult,
    activation::{
        self, ActivationPort, Phase, SupervisorClaim, TaskActivationRecord, TaskSubmission,
    },
    files::FileIdentity,
};
use service::task::*;
use std::collections::VecDeque;
const GUID: &str = "{01234567-89ab-cdef-0123-456789abcdef}";
fn identity() -> FileIdentity {
    FileIdentity {
        volume: 1,
        file: [2; 16],
    }
}
fn desired() -> Definition {
    Definition {
        name: TASK_NAME.into(),
        principal: "S-1-5-21-101".into(),
        trigger_user: "S-1-5-21-101".into(),
        logon: Logon::InteractiveToken,
        run_level: RunLevel::Limited,
        action: "C:\\fixture\\Programs\\Crosspane\\crosspane-installer.exe".into(),
        arguments: SUPERVISOR_ARGUMENT.into(),
        working_directory: "C:\\fixture\\Programs\\Crosspane".into(),
        logon_trigger_only: true,
        ignore_new_instance: true,
        manager_restart_count: 0,
        enabled: true,
    }
}
fn snapshot() -> Snapshot {
    Snapshot {
        definition: desired(),
        xml: "<own-fixture/>".into(),
    }
}
fn record() -> TaskActivationRecord {
    TaskActivationRecord::new(
        [1; 16],
        "S-1-5-21-101".into(),
        identity(),
        Some("<own-fixture/>".into()),
    )
    .unwrap()
}
struct Fake {
    views: VecDeque<Option<Snapshot>>,
    events: Vec<&'static str>,
    fail: Option<(&'static str, NativeError)>,
    registers: usize,
    runs: usize,
    lock: bool,
    retired: bool,
    reopened: bool,
}
impl Fake {
    fn new(views: Vec<Option<Snapshot>>) -> Self {
        Self {
            views: views.into(),
            events: Vec::new(),
            fail: None,
            registers: 0,
            runs: 0,
            lock: true,
            retired: false,
            reopened: false,
        }
    }
    fn stage(&mut self, name: &'static str) -> NativeResult<()> {
        self.events.push(name);
        if self.fail.is_some_and(|(stage, _)| stage == name) {
            return Err(self.fail.unwrap().1);
        }
        Ok(())
    }
}
impl TaskPort for Fake {
    fn inspect(&mut self) -> NativeResult<Option<Snapshot>> {
        self.stage("inspect")?;
        Ok(self.views.pop_front().unwrap())
    }
    fn record_intent(&mut self, _: Option<&Snapshot>) -> NativeResult<()> {
        self.stage("registration-intent")
    }
    fn register(&mut self, _: &Definition) -> NativeResult<()> {
        self.registers += 1;
        self.stage("register")
    }
    fn record_result(&mut self) -> NativeResult<()> {
        self.stage("registered")
    }
    fn retire(&mut self) {
        self.events.push("retire");
        self.retired = true;
    }
}
impl ActivationPort for Fake {
    fn prepare(&mut self) -> NativeResult<()> {
        self.stage("prepare")?;
        if self.reopened {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    fn record_run_intent(&mut self) -> NativeResult<()> {
        self.stage("run-intent")
    }
    fn release_lock(&mut self) -> NativeResult<()> {
        self.stage("unlock")?;
        self.lock = false;
        Ok(())
    }
    fn run_once(&mut self) -> NativeResult<TaskSubmission> {
        assert!(
            !self.lock,
            "unchanged supervisor constructor must acquire this lock"
        );
        self.runs += 1;
        self.stage("run")?;
        TaskSubmission::new(GUID.into())
    }
    fn record_run_result(&mut self, _: &TaskSubmission) -> NativeResult<()> {
        self.stage("run-observed")
    }
}
#[test]
fn missing_approval_fails_before_scheduler_observation() {
    assert_eq!(service::start_supported(), Err(NativeError::Unsupported));
    let mut fake = Fake::new(vec![]);
    fake.fail = Some(("prepare", NativeError::Unsupported));
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Err(NativeError::Unsupported)
    );
    assert_eq!(fake.events, ["prepare"]);
}
#[test]
fn exact_limited_current_user_role_is_required_before_task_mutation() {
    for mutate in [0, 1, 2, 3] {
        let mut value = desired();
        match mutate {
            0 => value.run_level = RunLevel::Highest,
            1 => value.logon = Logon::ServiceAccount,
            2 => value.trigger_user = "S-1-5-21-202".into(),
            _ => value.arguments.push_str(" --foreign"),
        };
        let mut fake = Fake::new(vec![None]);
        assert!(activation::activate(&mut fake, &value).is_err());
        assert_eq!(fake.registers + fake.runs, 0);
    }
}
#[test]
fn bounded_original_xml_and_unknown_record_fields_refuse() {
    assert!(
        TaskActivationRecord::new(
            [1; 16],
            "S-1-5-21-101".into(),
            identity(),
            Some("x".repeat(MAX_XML_BYTES + 1))
        )
        .is_err()
    );
    let mut value: serde_json::Value = serde_json::from_slice(&record().encode().unwrap()).unwrap();
    value["data"]["foreign"] = serde_json::Value::Bool(true);
    assert!(TaskActivationRecord::decode(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(TaskActivationRecord::decode(b"not-json").is_err());
}
#[test]
fn disabled_task_has_no_registration_or_run() {
    let mut old = snapshot();
    old.definition.enabled = false;
    let mut fake = Fake::new(vec![Some(old)]);
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Ok(Plan::PreserveDisabled)
    );
    assert_eq!(fake.events, ["prepare", "inspect"]);
    assert_eq!(fake.registers + fake.runs, 0);
}
#[test]
fn changed_snapshot_refuses_before_dispatch() {
    let mut fake = Fake::new(vec![None, Some(snapshot())]);
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Err(NativeError::Foreign)
    );
    assert_eq!(fake.registers + fake.runs, 0);
}
#[test]
fn each_pre_dispatch_failure_has_at_most_one_start_and_no_replay() {
    for stage in [
        "prepare",
        "registration-intent",
        "register",
        "registered",
        "run-intent",
        "unlock",
        "run",
        "run-observed",
    ] {
        let mut fake = Fake::new(vec![None, None]);
        fake.fail = Some((stage, NativeError::Unavailable));
        assert!(activation::activate(&mut fake, &desired()).is_err());
        assert!(fake.registers <= 1 && fake.runs <= 1);
        if matches!(
            stage,
            "prepare" | "registration-intent" | "register" | "registered" | "run-intent" | "unlock"
        ) {
            assert_eq!(fake.runs, 0);
        }
    }
}
#[test]
fn ambiguous_run_or_late_result_retires_without_another_dispatch() {
    for stage in ["run", "run-observed"] {
        let mut fake = Fake::new(vec![Some(snapshot())]);
        fake.fail = Some((stage, NativeError::OutcomeUnknown));
        assert_eq!(
            activation::activate(&mut fake, &desired()),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(fake.runs, 1);
        assert!(fake.retired);
    }
}
#[test]
fn task_submission_never_mints_a_generation_or_instance() {
    let mut value = record();
    value.registered().unwrap();
    value.run_intent().unwrap();
    value.run_observed(GUID.into()).unwrap();
    assert_eq!(value.phase(), Phase::RunObserved);
    assert_eq!(value.claim(), None);
    assert!(TaskSubmission::new("42".into()).is_err());
    assert!(TaskSubmission::new("{foreign}".into()).is_err());
}
#[test]
fn image_sid_and_operation_correlation_are_exact() {
    let value = record();
    assert!(value.bind([9; 16], "S-1-5-21-101", identity()).is_err());
    assert!(value.bind([1; 16], "S-1-5-21-202", identity()).is_err());
    assert!(
        value
            .bind(
                [1; 16],
                "S-1-5-21-101",
                FileIdentity {
                    volume: 2,
                    file: [2; 16]
                }
            )
            .is_err()
    );
}
#[test]
fn reopened_run_intent_observes_only_and_never_runs() {
    let mut value = record();
    value.registered().unwrap();
    value.run_intent().unwrap();
    let reopened = TaskActivationRecord::decode(&value.encode().unwrap()).unwrap();
    assert_eq!(reopened.phase(), Phase::RunIntent);
    let mut fake = Fake::new(vec![]);
    fake.reopened = true;
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(fake.runs + fake.registers, 0);
}
#[test]
fn durable_intent_precedes_unlock_and_exact_one_run() {
    let mut fake = Fake::new(vec![None, None]);
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Ok(Plan::Register)
    );
    assert_eq!(
        fake.events,
        [
            "prepare",
            "inspect",
            "registration-intent",
            "inspect",
            "register",
            "registered",
            "run-intent",
            "unlock",
            "run",
            "run-observed"
        ]
    );
    assert_eq!(fake.runs, 1);
    assert!(!fake.lock);
}
#[test]
fn one_claim_survives_submission_and_duplicate_claim_refuses() {
    let mut value = record();
    assert!(
        value
            .claim_supervisor(SupervisorClaim {
                pid: 3,
                creation: 4
            })
            .is_err()
    );
    value.registered().unwrap();
    value.run_intent().unwrap();
    let claim = SupervisorClaim {
        pid: 3,
        creation: 4,
    };
    value.claim_supervisor(claim).unwrap();
    assert!(value.claim_supervisor(claim).is_err());
    value.run_observed(GUID.into()).unwrap();
    assert_eq!(
        TaskActivationRecord::decode(&value.encode().unwrap())
            .unwrap()
            .claim(),
        Some(claim)
    );
}

#[test]
fn consumed_claim_later_logon_refuses_without_reset_or_new_run() {
    let mut value = record();
    value.registered().unwrap();
    value.run_intent().unwrap();
    value
        .claim_supervisor(SupervisorClaim {
            pid: 3,
            creation: 4,
        })
        .unwrap();
    value.run_observed(GUID.into()).unwrap();
    let mut reopened = TaskActivationRecord::decode(&value.encode().unwrap()).unwrap();
    assert_eq!(
        reopened.claim_supervisor(SupervisorClaim {
            pid: 5,
            creation: 6
        }),
        Err(NativeError::Foreign)
    );
    assert_eq!(
        reopened.claim(),
        Some(SupervisorClaim {
            pid: 3,
            creation: 4
        })
    );
    let mut fake = Fake::new(vec![]);
    fake.reopened = true;
    assert_eq!(
        activation::activate(&mut fake, &desired()),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(fake.runs + fake.registers, 0);
}
#[test]
fn registration_steps_are_durable_ordered_and_partial_creation_cannot_run() {
    use native_io::activation::RegistrationStep::*;
    let mut value = record();
    value.registration_step(FolderIntent).unwrap();
    assert!(value.registered().is_err());
    assert!(value.run_intent().is_err());
    value.registration_step(FolderCreated).unwrap();
    let mut reopened = TaskActivationRecord::decode(&value.encode().unwrap()).unwrap();
    assert_eq!(reopened.phase(), Phase::RegistrationIntent);
    assert!(reopened.registration_step(DefinitionRegistered).is_err());
    reopened.registration_step(DefinitionIntent).unwrap();
    reopened.registration_step(DefinitionRegistered).unwrap();
    reopened.registered().unwrap();
    reopened.run_intent().unwrap();
}

fn old_epoch() -> service::journal::Journal {
    service::journal::Journal {
        schema_version: 1,
        registration: [3; 16],
        operation: [3; 16],
        user: "S-1-5-21-101".into(),
        phase: service::journal::Phase::Finished,
        current: Some(service::supervisor::Generation {
            pid: 7,
            creation: 8,
            instance: 9,
        }),
        stop_instance: None,
        original_xml: None,
        restart_times: Vec::new(),
        last_tick_ms: 10,
        clock_epoch: 11,
    }
}
fn upgrade_record() -> payload::recovery::OperationRecord {
    let mut value = payload::recovery::OperationRecord::new([4; 16]).unwrap();
    value.set_original_instance(format!("{:032x}", 9));
    value.set_phase(payload::recovery::Phase::StartIntent);
    value
}
#[test]
fn upgrade_lineage_requires_exact_finished_predecessor_and_current_start_intent() {
    let selected = upgrade_record();
    let old = old_epoch();
    let lineage =
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), Some(&old))
            .unwrap()
            .unwrap();
    assert_eq!(lineage.operation(), [4; 16]);
    assert_eq!(lineage.original_instance(), 9);
    assert_eq!(lineage.predecessor_operation(), [3; 16]);
    assert_eq!(lineage.predecessor_registration(), [3; 16]);
    assert_eq!(lineage.predecessor_generation(), old.current.unwrap());
    assert_eq!(lineage.predecessor_clock_epoch(), 11);
    assert!(lineage.matches_predecessor(&old));
    assert!(
        activation::correlate_upgrade([5; 16], "S-1-5-21-101", Some(&selected), Some(&old))
            .is_err()
    );
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-202", Some(&selected), Some(&old))
            .is_err()
    );
    let mut changed = old.clone();
    changed.registration = [8; 16];
    assert!(!lineage.matches_predecessor(&changed));
}
#[test]
fn lineage_absence_requires_both_absent_and_cold_conflicts_refuse() {
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", None, None)
            .unwrap()
            .is_none()
    );
    let old = old_epoch();
    let selected = upgrade_record();
    assert!(activation::correlate_upgrade([4; 16], "S-1-5-21-101", None, Some(&old)).is_err());
    assert!(activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), None).is_err());
    let mut unrelated = selected.clone();
    unrelated.set_phase(payload::recovery::Phase::Verified);
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&unrelated), Some(&old))
            .is_err()
    );
}
#[test]
fn stop_intent_lineage_needs_exact_original_instance_and_never_accepts_running_epoch() {
    let selected = upgrade_record();
    let mut old = old_epoch();
    old.phase = service::journal::Phase::StopIntent;
    old.stop_instance = Some(9);
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), Some(&old))
            .unwrap()
            .is_some()
    );
    old.stop_instance = Some(10);
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), Some(&old))
            .is_err()
    );
    old.stop_instance = None;
    old.phase = service::journal::Phase::Running;
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), Some(&old))
            .is_err()
    );
    let mut wrong = selected.clone();
    wrong.set_original_instance(format!("{:032x}", 10));
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&wrong), Some(&old_epoch()))
            .is_err()
    );
    wrong.set_original_instance("00000000000000000000000000000000".into());
    assert!(
        activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&wrong), Some(&old_epoch()))
            .is_err()
    );
}
#[test]
fn exactly_three_archive_slots_preserve_original_supervisor_envelope() {
    use native_io::records::{self, RecordName};
    let bytes = old_epoch().encode().unwrap();
    for slot in 0..=2 {
        let name = RecordName::SupervisorEpoch(slot);
        assert_eq!(
            name.file_name().unwrap().as_str(),
            format!("supervisor-epoch-{slot}.json")
        );
        records::validate_for(&name, &bytes).unwrap();
        assert_eq!(
            service::journal::Journal::decode(&bytes).unwrap(),
            old_epoch()
        );
    }
    assert!(RecordName::SupervisorEpoch(3).file_name().is_err());
    let name = RecordName::SupervisorArchiveIntent;
    assert_eq!(
        name.file_name().unwrap().as_str(),
        "supervisor-archive-intent.json"
    );
    let intent = records::encode_record(&name, serde_json::json!({"fixture":true})).unwrap();
    assert!(records::validate_for(&name, &bytes).is_err());
    records::validate_for(&name, &intent).unwrap();
}

#[test]
fn malformed_predecessor_tuple_is_refused_by_the_actual_journal_validator() {
    let selected = upgrade_record();
    for invalid in 0..3 {
        let mut old = old_epoch();
        match invalid {
            0 => old.current.as_mut().unwrap().pid = 0,
            1 => old.current.as_mut().unwrap().creation = 0,
            _ => old.clock_epoch = 0,
        }
        assert!(
            activation::correlate_upgrade([4; 16], "S-1-5-21-101", Some(&selected), Some(&old))
                .is_err()
        );
    }
}

/// Only observations and production portable seams run here, never native permit factories.
mod a4c_logon {
    use super::*;
    use native_io::{
        activation::{
            EntrySelection, EpochProvenance, HistoryCorrelation, LogonRecordPhase,
            SupervisorLogonRecord, select_entry,
        },
        epoch_archive::{
            HistoryRequirement, classify_prior_logon_status, first_logon_absence,
            history_requirement, select_correlated_slots,
        },
        identity::{Sid, TokenFacts},
    };
    use service::{
        journal::{Journal, Phase as JournalPhase},
        supervisor::{Generation, InitialEpochPort, initialize_epoch},
    };

    const ABSENT: i32 = 0xC000005Fu32 as i32;

    fn sid(parts: &[u32]) -> Sid {
        let mut bytes = vec![1, parts.len() as u8, 0, 0, 0, 0, 0, 5];
        for part in parts {
            bytes.extend_from_slice(&part.to_le_bytes());
        }
        Sid::from_bytes(bytes).unwrap()
    }
    fn context(authentication_id: u64, session: u32) -> TokenFacts {
        TokenFacts {
            user: sid(&[21, 101]),
            logon: sid(&[
                5,
                (authentication_id >> 32) as u32,
                authentication_id as u32,
            ]),
            session,
            elevated: false,
            integrity: 0x2000,
            authentication_id,
            impersonating: false,
        }
    }
    fn provenance(epoch: u8) -> EpochProvenance {
        EpochProvenance::new(
            [epoch; 16],
            [epoch + 16; 16],
            &context(100 + u64::from(epoch), u32::from(epoch)),
            700 + u32::from(epoch),
            1000 + u64::from(epoch),
        )
        .unwrap()
    }
    fn journal(provenance: &EpochProvenance, phase: JournalPhase) -> Journal {
        let generation = Generation {
            pid: 900,
            creation: 2000,
            instance: u64::MAX,
        };
        Journal {
            schema_version: 1,
            registration: provenance.registration(),
            operation: provenance.operation(),
            user: provenance.user().into(),
            phase,
            current: Some(generation),
            stop_instance: (phase == JournalPhase::StopIntent).then_some(generation.instance),
            original_xml: None,
            restart_times: vec![],
            last_tick_ms: 10,
            clock_epoch: provenance.clock_epoch(),
        }
    }
    fn bound(epoch: u8) -> SupervisorLogonRecord {
        let provenance = provenance(epoch);
        let mut record = SupervisorLogonRecord::new(provenance.clone()).unwrap();
        record
            .bind(&journal(&provenance, JournalPhase::Running))
            .unwrap();
        record
    }
    fn stamp(slot: u8) -> payload::recovery::FileStamp {
        payload::recovery::FileStamp {
            volume: 1,
            file: [slot + 3; 16],
        }
    }
    fn hash(journal: &Journal) -> [u8; 32] {
        aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &journal.encode().unwrap())
            .as_ref()
            .try_into()
            .unwrap()
    }
    fn archived(record: &SupervisorLogonRecord, slot: u8, prior: &Journal) -> HistoryCorrelation {
        HistoryCorrelation::new(slot, record.current().clone(), stamp(slot), hash(prior)).unwrap()
    }

    struct InitialFake {
        events: Vec<&'static str>,
        fail: Option<&'static str>,
        admission: NativeResult<()>,
        preparing: bool,
        ready: bool,
        bound: bool,
    }
    impl InitialFake {
        fn admitted(admission: NativeResult<()>) -> Self {
            Self {
                events: vec![],
                fail: None,
                admission,
                preparing: false,
                ready: false,
                bound: false,
            }
        }
        fn step(&mut self, name: &'static str) -> NativeResult<()> {
            self.events.push(name);
            if self.fail == Some(name) {
                Err(NativeError::OutcomeUnknown)
            } else {
                Ok(())
            }
        }
    }
    impl InitialEpochPort for InitialFake {
        type Child = u8;
        type Ready = u8;
        fn prepare_epoch(&mut self) -> NativeResult<()> {
            self.step("epoch")?;
            self.admission?;
            self.preparing = true;
            Ok(())
        }
        fn create(&mut self) -> NativeResult<Self::Child> {
            assert!(self.preparing && !self.ready && !self.bound);
            self.step("create")?;
            Ok(1)
        }
        fn await_ready(&mut self, child: &Self::Child) -> NativeResult<Self::Ready> {
            assert_eq!(*child, 1);
            self.step("ready")?;
            self.ready = true;
            Ok(2)
        }
        fn publish_running(&mut self, ready: &Self::Ready) -> NativeResult<()> {
            assert_eq!(*ready, 2);
            assert!(self.preparing && self.ready && !self.bound);
            self.step("running")?;
            self.bound = true;
            Ok(())
        }
    }

    #[test]
    fn first_logon_absence_before_create_and_ready_before_running() {
        assert_eq!(select_entry(None), Ok(EntrySelection::Logon));
        assert_eq!(
            first_logon_absence(None, false, false, false, false),
            Ok(())
        );
        let mut fake = InitialFake::admitted(first_logon_absence(None, false, false, false, false));
        assert_eq!(initialize_epoch(&mut fake), Ok(2));
        assert_eq!(fake.events, ["epoch", "create", "ready", "running"]);
        assert!(fake.bound);
        // A residual fixed archive slot is not positive first-logon absence.
        assert_eq!(
            first_logon_absence(None, false, false, false, true),
            Err(NativeError::Unsupported)
        );
        let preparing = SupervisorLogonRecord::new(provenance(1)).unwrap();
        let running = journal(preparing.current(), JournalPhase::Running);
        assert_eq!(
            preparing.matches_current(&running),
            Err(NativeError::OutcomeUnknown)
        );
        for facts in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            let mut fake = InitialFake::admitted(first_logon_absence(
                None, facts.0, facts.1, facts.2, facts.3,
            ));
            assert!(initialize_epoch(&mut fake).is_err());
            assert_eq!(fake.events, ["epoch"]);
            assert!(!fake.preparing && !fake.ready && !fake.bound);
        }
        for (index, stage) in ["epoch", "create", "ready", "running"].iter().enumerate() {
            let mut fake = InitialFake::admitted(Ok(()));
            fake.fail = Some(stage);
            assert_eq!(
                initialize_epoch(&mut fake),
                Err(NativeError::OutcomeUnknown)
            );
            assert_eq!(fake.events.len(), index + 1);
            assert!(!fake.bound);
        }
    }

    #[test]
    fn second_clean_logon_requires_exact_disposition() {
        let prior_record = bound(1);
        let prior = journal(prior_record.current(), JournalPhase::Finished);
        let bytes = prior.encode().unwrap();
        assert_eq!(
            prior_record.matches_current(&prior),
            Ok(prior_record.current())
        );
        let new_context = context(102, 2);
        assert!(!prior_record.current().matches_context(&new_context));
        let mut fake = InitialFake::admitted(classify_prior_logon_status(ABSENT, false));
        assert_eq!(initialize_epoch(&mut fake), Ok(2));
        let mut next = prior_record
            .rotate(provenance(2), archived(&prior_record, 0, &prior))
            .unwrap();
        assert_eq!(next.phase(), LogonRecordPhase::Preparing);
        next.bind(&journal(next.current(), JournalPhase::Running))
            .unwrap();
        assert!(
            next.matches_history(0, &prior, stamp(0), hash(&prior))
                .is_ok()
        );
        assert_eq!(prior.encode().unwrap(), bytes);
        for (status, data) in [
            (0, true),
            (0, false),
            (0xC0000022u32 as i32, false),
            (0xC0000001u32 as i32, false),
            (ABSENT, true),
        ] {
            let mut fake = InitialFake::admitted(classify_prior_logon_status(status, data));
            assert!(initialize_epoch(&mut fake).is_err());
            assert_eq!(fake.events, ["epoch"]);
        }
        let mut foreign = prior.clone();
        foreign.operation = [99; 16];
        assert_eq!(
            prior_record.matches_current(&foreign),
            Err(NativeError::Foreign)
        );
    }

    #[test]
    fn earlier_crash_residue_preserves_known_history() {
        for phase in [
            JournalPhase::Running,
            JournalPhase::Backoff,
            JournalPhase::StartRequested,
        ] {
            let prior_record = bound(1);
            let prior = journal(prior_record.current(), phase);
            let bytes = prior.encode().unwrap();
            assert_eq!(
                history_requirement(&prior),
                Ok(HistoryRequirement::PriorLogonDisposition)
            );
            prior_record.matches_current(&prior).unwrap();
            let admission = classify_prior_logon_status(ABSENT, false);
            let mut fake = InitialFake::admitted(admission);
            assert_eq!(initialize_epoch(&mut fake), Ok(2));
            let next = prior_record
                .rotate(provenance(2), archived(&prior_record, 1, &prior))
                .unwrap();
            assert!(
                next.matches_history(1, &prior, stamp(1), hash(&prior))
                    .is_ok()
            );
            assert_eq!(prior.encode().unwrap(), bytes);
            assert!(
                next.matches_current(&journal(next.current(), JournalPhase::Running))
                    .is_err()
            );
            assert_eq!(
                next.matches_history(1, &prior, stamp(0), hash(&prior)),
                Err(NativeError::Foreign)
            );
            assert_eq!(
                next.matches_history(1, &prior, stamp(1), [9; 32]),
                Err(NativeError::Foreign)
            );
        }
        let record = bound(1);
        let unknown = journal(record.current(), JournalPhase::Unknown);
        assert_eq!(history_requirement(&unknown), Err(NativeError::Foreign));
        assert_eq!(record.matches_current(&unknown), Err(NativeError::Foreign));
        let mut malformed = journal(record.current(), JournalPhase::Running);
        malformed.current = None;
        assert!(history_requirement(&malformed).is_err());
        assert!(record.matches_current(&malformed).is_err());
        let mut encoded: serde_json::Value =
            serde_json::from_slice(&record.encode().unwrap()).unwrap();
        encoded["data"]["extra_authority"] = serde_json::json!(true);
        assert!(SupervisorLogonRecord::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
    }

    #[test]
    fn concurrent_same_logon_refuses_before_create() {
        let original = bound(1);
        let before = original.encode().unwrap();
        let mut first = InitialFake::admitted(Ok(()));
        assert_eq!(initialize_epoch(&mut first), Ok(2));
        // The production port short-circuits the failed first-instance admission. This proves
        // ordering only; actual kernel pipe uniqueness remains an unrun native row.
        let mut second = InitialFake::admitted(Err(NativeError::Busy));
        assert_eq!(initialize_epoch(&mut second), Err(NativeError::Busy));
        assert_eq!(second.events, ["epoch"]);
        assert!(!second.preparing && !second.ready && !second.bound);
        assert_eq!(original.encode().unwrap(), before);
        assert!(original.current().matches_context(&context(101, 1)));
        // Present/reused LUID cannot acquire a fresh budget through mere identity inequality.
        assert!(classify_prior_logon_status(0, false).is_err());
    }

    #[test]
    fn consumed_activation_stays_unchanged() {
        let mut task = record();
        task.registered().unwrap();
        assert_eq!(select_entry(Some(&task)), Ok(EntrySelection::Logon));
        task.run_intent().unwrap();
        assert_eq!(select_entry(Some(&task)), Ok(EntrySelection::Installer));
        task.claim_supervisor(SupervisorClaim {
            pid: 700,
            creation: 1000,
        })
        .unwrap();
        let bytes = task.encode().unwrap();
        assert_eq!(select_entry(Some(&task)), Ok(EntrySelection::Logon));
        assert_eq!(
            first_logon_absence(Some(&task), false, false, false, false),
            Err(NativeError::Unsupported)
        );
        let prior_record = bound(1);
        let prior = journal(prior_record.current(), JournalPhase::Finished);
        prior_record.matches_current(&prior).unwrap();
        let mut fake = InitialFake::admitted(classify_prior_logon_status(ABSENT, false));
        assert_eq!(initialize_epoch(&mut fake), Ok(2));
        assert_eq!(task.encode().unwrap(), bytes);
        assert_eq!(
            task.claim_supervisor(SupervisorClaim {
                pid: 701,
                creation: 1001
            }),
            Err(NativeError::Foreign)
        );
        assert_eq!(task.encode().unwrap(), bytes);
        assert_eq!(
            select_entry(Some(&record())),
            Err(NativeError::OutcomeUnknown)
        );
        let mut encoded: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        encoded["data"]["phase"] = serde_json::json!("unknown");
        assert!(TaskActivationRecord::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());
        let mut registered = record();
        registered.registered().unwrap();
        let mut unknown: serde_json::Value =
            serde_json::from_slice(&registered.encode().unwrap()).unwrap();
        unknown["data"]["phase"] = serde_json::json!("unknown");
        let unknown = TaskActivationRecord::decode(&serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert_eq!(
            select_entry(Some(&unknown)),
            Err(NativeError::OutcomeUnknown)
        );
    }

    #[test]
    fn upgrade_then_logon_preserves_later_upgrade_history() {
        let mut record = bound(1);
        let mut slots: [Option<Journal>; 3] = [None, None, None];
        for epoch in 1..=5 {
            let phase = if epoch == 1 {
                JournalPhase::Finished
            } else {
                JournalPhase::Running
            };
            let prior = journal(record.current(), phase);
            let (slot, prune) = select_correlated_slots(&slots).unwrap();
            assert_eq!(prune, epoch > 3);
            let expected = history_requirement(&prior).unwrap();
            assert_eq!(
                expected,
                if epoch == 1 {
                    HistoryRequirement::Terminal
                } else {
                    HistoryRequirement::PriorLogonDisposition
                }
            );
            classify_prior_logon_status(ABSENT, false).unwrap();
            let next = record
                .rotate(provenance(epoch + 1), archived(&record, slot, &prior))
                .unwrap();
            slots[usize::from(slot)] = Some(prior.clone());
            record = next;
            record
                .bind(&journal(record.current(), JournalPhase::Running))
                .unwrap();
            assert_eq!(record.history().len(), usize::from(epoch.min(3)));
            for (index, historical) in slots.iter().enumerate() {
                if let Some(historical) = historical {
                    let slot = index as u8;
                    record
                        .matches_history(slot, historical, stamp(slot), hash(historical))
                        .unwrap();
                    assert_eq!(
                        record.matches_history(slot, historical, stamp(slot), [7; 32]),
                        Err(NativeError::Foreign)
                    );
                }
            }
            assert_eq!(
                SupervisorLogonRecord::decode(&record.encode().unwrap()).unwrap(),
                record
            );
        }
        let mut unknown = slots[0].clone().unwrap();
        unknown.phase = JournalPhase::Unknown;
        assert_eq!(
            select_correlated_slots(&[Some(unknown), None, None]),
            Err(NativeError::Foreign)
        );
        let missing = bound(6);
        let historical = slots[0].as_ref().unwrap();
        assert_eq!(
            missing.matches_history(0, historical, stamp(0), hash(historical)),
            Err(NativeError::Foreign)
        );
    }
}
