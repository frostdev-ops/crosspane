#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Focused authored metadata fakes; no task, filesystem, process, agent or GUI is opened.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/first_install.rs"]
mod first_install;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/removal.rs"]
mod removal;
#[path = "../src/platform/windows/repair.rs"]
mod repair;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use first_install::{driver::*, record::*, *};
use native_io::{NativeError, NativeResult};
use payload::{
    inventory::{PayloadRole, PeFacts},
    recovery::{FileStamp, ImageObservation, OriginalLeaf},
};

fn facts(role: PayloadRole) -> PeFacts {
    PeFacts {
        size: 512,
        sha256: [role as u8 + 1; 32],
        machine: 0x8664,
        subsystem: 3,
        version: "1.0.0".into(),
    }
}
fn image(role: PayloadRole) -> ImageObservation {
    ImageObservation {
        identity: FileStamp {
            volume: 1,
            file: [10 + role as u8; 16],
        },
        facts: facts(role),
    }
}
fn record() -> FirstInstallRecord {
    FirstInstallRecord::new([1; 16], vec![2], PayloadRole::ALL.map(facts)).unwrap()
}
fn absent() -> FirstInstallFacts {
    FirstInstallFacts {
        task: Presence::Missing,
        agent: Presence::Missing,
        supervisor: Presence::Missing,
        history: History::None,
    }
}
#[derive(Clone)]
struct Fake {
    saved: Option<FirstInstallRecord>,
    staged: [Option<ImageObservation>; 4],
    fixed: [Option<ImageObservation>; 4],
    originals: [OriginalLeaf; 4],
    backups: [Option<FileStamp>; 4],
    counts: [usize; 4],
    register: usize,
    runs: usize,
    ready: bool,
    running: bool,
    events: Vec<String>,
    cut: Option<usize>,
    steps: usize,
    elevated_calls: usize,
    elevated_phase: Option<Phase>,
    elevated_error: Option<NativeError>,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            saved: None,
            staged: [None, None, None, None],
            fixed: [None, None, None, None],
            originals: [OriginalLeaf::Missing; 4],
            backups: [None; 4],
            counts: [0; 4],
            register: 0,
            runs: 0,
            ready: false,
            running: false,
            events: vec![],
            cut: None,
            steps: 0,
            elevated_calls: 0,
            elevated_phase: None,
            elevated_error: None,
        }
    }
}
impl Fake {
    fn point(&mut self, label: impl Into<String>) -> NativeResult<()> {
        self.events.push(label.into());
        self.steps += 1;
        if self.cut == Some(self.steps) {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
}
impl FirstInstallPort for Fake {
    fn renew(&mut self, record: &FirstInstallRecord) -> NativeResult<()> {
        record.validate()?;
        if self
            .saved
            .as_ref()
            .is_some_and(|s| !s.same_selection(record))
        {
            return Err(NativeError::Foreign);
        }
        Ok(())
    }
    fn persist(&mut self, record: &FirstInstallRecord) -> NativeResult<()> {
        self.point(format!("intent-before:{:?}", record.phase()))?;
        self.saved = Some(record.clone());
        self.point(format!("intent-after:{:?}", record.phase()))
    }
    fn stage(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation> {
        self.point(format!("stage-before:{role:?}"))?;
        self.counts[role as usize] += 1;
        let value = image(role);
        self.staged[role as usize] = Some(value.clone());
        self.point(format!("stage-after:{role:?}"))?;
        Ok(value)
    }
    fn observe_stage(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<ImageObservation>> {
        Ok(self.staged[role as usize].clone())
    }
    fn original(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<OriginalLeaf> {
        Ok(self.originals[role as usize])
    }
    fn backup(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<FileStamp>> {
        self.point(format!("backup-before:{role:?}"))?;
        self.backups[role as usize] = match self.originals[role as usize] {
            OriginalLeaf::Present(id) => Some(id),
            _ => None,
        };
        self.originals[role as usize] = OriginalLeaf::Missing;
        self.point(format!("backup-after:{role:?}"))?;
        Ok(self.backups[role as usize])
    }
    fn observe_backup(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<FileStamp>> {
        Ok(self.backups[role as usize])
    }
    fn publish(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation> {
        self.point(format!("publish-before:{role:?}"))?;
        let value = self.staged[role as usize]
            .take()
            .ok_or(NativeError::Foreign)?;
        self.fixed[role as usize] = Some(value.clone());
        self.point(format!("publish-after:{role:?}"))?;
        Ok(value)
    }
    fn observe_fixed(
        &mut self,
        _r: &FirstInstallRecord,
        role: PayloadRole,
    ) -> NativeResult<Option<ImageObservation>> {
        Ok(self.fixed[role as usize].clone())
    }
    fn verify_files(&mut self, _r: &FirstInstallRecord) -> NativeResult<()> {
        if PayloadRole::ALL
            .into_iter()
            .any(|role| self.fixed[role as usize] != Some(image(role)))
        {
            return Err(NativeError::Foreign);
        }
        self.point("verify-files")
    }
    fn elevated(&mut self, _r: &FirstInstallRecord) -> NativeResult<()> {
        self.elevated_calls += 1;
        self.elevated_phase = self.saved.as_ref().map(FirstInstallRecord::phase);
        self.point("elevated")?;
        match self.elevated_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
    fn activate(&mut self, record: &FirstInstallRecord) -> NativeResult<FirstInstallRecord> {
        self.point("register-before")?;
        self.register += 1;
        self.point("register-after")?;
        let mut next = record.clone();
        next.advance(Phase::TaskRegistered)?;
        self.persist(&next)?;
        next.advance(Phase::RunIntent)?;
        self.persist(&next)?;
        self.point("run-before")?;
        self.runs += 1;
        self.point("run-after")?;
        next.advance(Phase::RunObserved)?;
        self.persist(&next)?;
        Ok(next)
    }
    fn observe_running(&mut self, _r: &FirstInstallRecord) -> NativeResult<Option<u64>> {
        if self.runs == 0 {
            return Ok(None);
        }
        self.point("observe-live-generation")?;
        self.ready = true;
        self.point("ready")?;
        self.running = true;
        self.point("running")?;
        Ok(Some(99))
    }
    fn prune(&mut self, _r: &FirstInstallRecord) -> NativeResult<bool> {
        self.point("retention-observe")?;
        Ok(true)
    }
}

#[test]
fn positive_absence_distinguishes_missing_denied_unknown_and_partial() {
    assert_eq!(preview(absent()), FirstInstallDisposition::Eligible);
    for denied in [Presence::AccessDenied, Presence::Unknown, Presence::Present] {
        for index in 0..3 {
            let mut f = absent();
            match index {
                0 => f.task = denied,
                1 => f.agent = denied,
                _ => f.supervisor = denied,
            }
            assert_ne!(preview(f), FirstInstallDisposition::Eligible);
        }
    }
    for history in [History::Partial, History::Other, History::Unknown] {
        let mut f = absent();
        f.history = history;
        assert_ne!(preview(f), FirstInstallDisposition::Eligible);
    }
}
#[test]
fn completed_removal_refuses_before_any_foundation_effect_and_names_a8b() {
    let names = vec!["install.lock".into(), "removal.json".into()];
    let disposition = classify_history(DecodedHistory {
        removal: Some(removal::RemovalCursor::Retired),
        first: None,
        journal: None,
        logon: false,
        claimed_activation: false,
        names: &names,
    });
    let foundation = std::cell::Cell::new(0);
    let mut port = Fake::default();
    let result = before_foundation(disposition, || {
        foundation.set(foundation.get() + 1);
        apply(&mut port, &mut record())
    })
    .unwrap();
    assert_eq!(result, Err(REMOVAL_REINSTALL));
    assert_eq!(foundation.get(), 0);
    assert!(port.events.is_empty());
    assert_eq!((port.register, port.runs), (0, 0));
    // The actual same gate reaches the foundation for genuine no-history facts.
    assert_eq!(
        before_foundation(FirstInstallDisposition::Eligible, || {
            foundation.set(foundation.get() + 1);
            Ok(())
        })
        .unwrap(),
        Ok(())
    );
    assert_eq!(foundation.get(), 1);
}
#[test]
fn driver_orders_four_role_publication_and_one_activation_ready_before_running() {
    let mut port = Fake::default();
    let mut r = record();
    apply(&mut port, &mut r).unwrap();
    assert_eq!(r.phase(), Phase::Complete);
    assert_eq!(r.instance(), Some(99));
    assert_eq!(port.counts, [1; 4]);
    assert_eq!((port.register, port.runs), (1, 1));
    let position = |value: &str| port.events.iter().position(|e| e == value).unwrap();
    assert!(position("verify-files") < position("register-before"));
    assert!(position("run-after") < position("ready"));
    assert!(position("ready") < position("running"));
    assert!(
        !port
            .events
            .iter()
            .any(|e| e.contains("stop") || e.contains("keeper"))
    );
}
#[test]
fn foreign_fixed_leaves_are_backed_up_opaquely_without_content_approval() {
    let mut port = Fake::default();
    for role in PayloadRole::ALL {
        port.originals[role as usize] = OriginalLeaf::Present(FileStamp {
            volume: 1,
            file: [100 + role as u8; 16],
        });
    }
    let mut r = record();
    apply(&mut port, &mut r).unwrap();
    for role in PayloadRole::ALL {
        assert_eq!(
            r.role(role).unwrap().backup,
            Some(FileStamp {
                volume: 1,
                file: [100 + role as u8; 16]
            })
        );
        assert_eq!(r.role(role).unwrap().published, Some(image(role)));
    }
}
#[test]
fn every_intent_effect_result_interruption_reopens_without_replaying_register_or_run() {
    let mut reference = Fake::default();
    apply(&mut reference, &mut record()).unwrap();
    for cut in 1..=reference.steps {
        let mut port = Fake {
            cut: Some(cut),
            ..Fake::default()
        };
        let mut r = record();
        let _ = apply(&mut port, &mut r);
        if let Some(saved) = port.saved.clone() {
            let mut reopened = FirstInstallRecord::decode(&saved.encode().unwrap()).unwrap();
            port.cut = None;
            let _ = resume(&mut port, &mut reopened);
            assert!(port.register <= 1, "register replay at boundary {cut}");
            assert!(port.runs <= 1, "Run replay at boundary {cut}");
            assert!(
                port.counts.iter().all(|count| *count <= 1),
                "stage replay at boundary {cut}"
            );
        }
    }
}
#[test]
fn strict_record_rejects_partial_pins_stale_selection_and_unknown_fields() {
    let r = record();
    let mut json: serde_json::Value = serde_json::from_slice(&r.encode().unwrap()).unwrap();
    json["data"]["unexpected"] = serde_json::json!(true);
    assert_eq!(
        FirstInstallRecord::decode(&serde_json::to_vec(&json).unwrap()),
        Err(NativeError::Invalid)
    );
    let mut incomplete = r.clone();
    assert_eq!(
        incomplete.advance(Phase::FilesVerified),
        Err(NativeError::Invalid)
    );
    let another = FirstInstallRecord::new([3; 16], vec![2], PayloadRole::ALL.map(facts)).unwrap();
    assert!(!r.same_selection(&another));
    let mut unknown = r;
    unknown.advance(Phase::Unknown).unwrap();
    assert_eq!(
        resume(&mut Fake::default(), &mut unknown),
        Err(NativeError::OutcomeUnknown)
    );
}
#[test]
fn first_partial_selection_never_becomes_an_upgrade_and_live_instances_refuse_cold() {
    let mut f = absent();
    f.history = History::FirstInstall;
    assert_eq!(preview(f), FirstInstallDisposition::Resume);
    for value in [Presence::Present, Presence::AccessDenied, Presence::Unknown] {
        let mut f = absent();
        f.agent = value;
        assert_ne!(preview(f), FirstInstallDisposition::Eligible);
    }
}

fn token(user: u32, logon: u32) -> native_io::identity::TokenFacts {
    use native_io::identity::{Sid, TokenFacts};
    let mut u = vec![1, 1, 0, 0, 0, 0, 0, 5];
    u.extend_from_slice(&user.to_le_bytes());
    let mut l = vec![1, 3, 0, 0, 0, 0, 0, 5, 5, 0, 0, 0];
    l.extend_from_slice(&logon.to_le_bytes());
    l.extend_from_slice(&0u32.to_le_bytes());
    TokenFacts {
        user: Sid::from_bytes(u).unwrap(),
        logon: Sid::from_bytes(l).unwrap(),
        session: logon,
        authentication_id: u64::from(logon),
        elevated: false,
        integrity: 0x2000,
        impersonating: false,
    }
}
fn context_record(current: &native_io::identity::TokenFacts) -> FirstInstallRecord {
    FirstInstallRecord::new(
        [1; 16],
        serde_json::to_vec(&payload::recovery::OuterContextCorrelation::new(current).unwrap())
            .unwrap(),
        PayloadRole::ALL.map(facts),
    )
    .unwrap()
}
#[test]
fn decoded_native_history_classifies_removal_epoch_claim_unknown_names_and_first_states() {
    let classify = |names: &[String], removal, first, journal, logon, claimed_activation| {
        classify_history(DecodedHistory {
            removal,
            first,
            journal,
            logon,
            claimed_activation,
            names,
        })
    };
    assert_eq!(
        classify(&[], None, None, None, false, false),
        FirstInstallDisposition::Eligible
    );
    let lock = vec!["install.lock".into()];
    assert_eq!(
        classify(&lock, None, None, None, false, false),
        FirstInstallDisposition::Eligible
    );
    assert_eq!(
        classify(
            &[],
            Some(removal::RemovalCursor::Retired),
            None,
            None,
            false,
            false
        ),
        FirstInstallDisposition::CompletedRemoval
    );
    for names in [
        vec!["supervisor-epoch-0.json".into()],
        vec!["supervisor-epoch-2.json".into()],
    ] {
        assert_eq!(
            classify(&names, None, None, None, false, false),
            FirstInstallDisposition::CompletedRemoval
        );
    }
    assert_eq!(
        classify(
            &[],
            None,
            None,
            Some(service::journal::Phase::Finished),
            false,
            false
        ),
        FirstInstallDisposition::CompletedRemoval
    );
    assert_eq!(
        classify(&[], None, None, None, true, false),
        FirstInstallDisposition::CompletedRemoval
    );
    assert_eq!(
        classify(&[], None, None, None, false, true),
        FirstInstallDisposition::CompletedRemoval
    );
    assert_eq!(
        classify(&["unexpected.json".into()], None, None, None, false, false),
        FirstInstallDisposition::Partial
    );
    assert_eq!(
        classify(
            &["task-activation.json".into()],
            None,
            None,
            None,
            false,
            false
        ),
        FirstInstallDisposition::Partial
    );
    assert_eq!(
        classify(
            &["first-install.json".into()],
            None,
            Some(Phase::Intent),
            None,
            false,
            false
        ),
        FirstInstallDisposition::Resume
    );
    assert_eq!(
        classify(
            &[
                "first-install.json".into(),
                "task-activation.json".into(),
                "supervisor.json".into()
            ],
            None,
            Some(Phase::RunObserved),
            Some(service::journal::Phase::Running),
            true,
            true
        ),
        FirstInstallDisposition::Resume
    );
    assert_eq!(
        classify(
            &["removal.json".into()],
            None,
            Some(Phase::RunObserved),
            None,
            false,
            false
        ),
        FirstInstallDisposition::Partial
    );
}
#[test]
fn stale_context_intent_reopens_for_the_same_user_under_fresh_absence() {
    let old = context_record(&token(20, 1));
    let mut next = match reopen(&old, &token(20, 2)).unwrap() {
        Reopen::Restart(next) => next,
        _ => panic!("Intent must restart"),
    };
    assert_eq!(next.operation(), old.operation());
    assert_ne!(next.context(), old.context());
    assert_eq!(next.phase(), Phase::Intent);
    let mut port = Fake::default();
    apply(&mut port, &mut next).unwrap();
    assert_eq!(next.phase(), Phase::Complete);
    assert_eq!((port.register, port.runs), (1, 1));
    assert!(matches!(
        reopen(&old, &token(21, 2)),
        Err(NativeError::Foreign)
    ));
    let mut conflicting = old;
    conflicting.role_mut(PayloadRole::Agent).unwrap().staged = Some(image(PayloadRole::Agent));
    assert!(matches!(
        reopen(&conflicting, &token(20, 2)),
        Err(NativeError::OutcomeUnknown)
    ));
}
#[test]
fn run_observed_completes_in_a_new_context_by_observation_without_replay() {
    for phase in [
        Phase::RunIntent,
        Phase::RunObserved,
        Phase::Ready,
        Phase::PruneIntent,
    ] {
        let mut old = context_record(&token(20, 1));
        for role in PayloadRole::ALL {
            let r = old.role_mut(role).unwrap();
            r.staged = Some(image(role));
            r.published = Some(image(role));
            r.original = OriginalLeaf::Missing;
        }
        old.advance(Phase::RunObserved).unwrap();
        if phase == Phase::RunIntent {
            let mut json: serde_json::Value =
                serde_json::from_slice(&old.encode().unwrap()).unwrap();
            json["data"]["phase"] = serde_json::json!("run-intent");
            old = FirstInstallRecord::decode(&serde_json::to_vec(&json).unwrap()).unwrap();
        } else if phase.rank() >= Phase::Ready.rank() {
            old.ready(99).unwrap();
            old.advance(phase).unwrap();
        }
        let mut observed = match reopen(&old, &token(20, 2)).unwrap() {
            Reopen::Observe(record) => record,
            _ => panic!("post-Run is observe-only"),
        };
        assert_eq!(observed.context(), old.context());
        let mut port = Fake {
            saved: Some(old.clone()),
            fixed: PayloadRole::ALL.map(|r| Some(image(r))),
            register: 1,
            runs: 1,
            ready: true,
            running: true,
            ..Fake::default()
        };
        resume(&mut port, &mut observed).unwrap();
        assert_eq!(observed.phase(), Phase::Complete);
        assert_eq!((port.register, port.runs), (1, 1));
        assert_eq!(port.counts, [0; 4]);
        assert!(!port.events.iter().any(|e| e.starts_with("stage-")
            || e.starts_with("backup-")
            || e.starts_with("publish-")
            || e.starts_with("register-")
            || e.starts_with("run-")));
    }
}
#[test]
fn mid_effect_first_install_stays_unknown_and_never_crosses_the_foundation() {
    let mut r = context_record(&token(20, 1));
    r.advance(Phase::StageIntent(PayloadRole::Installer))
        .unwrap();
    assert!(matches!(
        reopen(&r, &token(20, 2)),
        Err(NativeError::OutcomeUnknown)
    ));
    let disposition = classify_history(DecodedHistory {
        removal: None,
        first: Some(r.phase()),
        journal: None,
        logon: false,
        claimed_activation: false,
        names: &["first-install.json".into()],
    });
    assert_eq!(disposition, FirstInstallDisposition::Partial);
    let foundation = std::cell::Cell::new(0);
    assert_eq!(
        before_foundation(disposition, || {
            foundation.set(1);
            Ok(())
        }),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(foundation.get(), 0);
}
#[test]
fn installer_lock_busy_retries_only_within_the_original_deadline() {
    use native_io::{Cancellation, Clock, Deadline};
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    struct FakeClock(AtomicU64);
    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    let clock = Arc::new(FakeClock(AtomicU64::new(0)));
    let deadline = Deadline::new(100, clock.clone(), Cancellation::default()).unwrap();
    let mut attempts = 0;
    let result = retry_busy(
        &deadline,
        || {
            attempts += 1;
            if attempts < 3 {
                Err(NativeError::Busy)
            } else {
                Ok(7)
            }
        },
        |ms| {
            clock.0.fetch_add(ms, Ordering::SeqCst);
        },
    );
    assert_eq!(result, Ok(7));
    assert_eq!(attempts, 3);
    let mut attempts = 0;
    let deadline = Deadline::new(25, clock.clone(), Cancellation::default()).unwrap();
    assert_eq!(
        retry_busy(
            &deadline,
            || {
                attempts += 1;
                Err::<(), _>(NativeError::Busy)
            },
            |ms| {
                clock.0.fetch_add(ms, Ordering::SeqCst);
            }
        ),
        Err(NativeError::Timeout)
    );
    assert_eq!(attempts, 2);
    let deadline = Deadline::new(25, clock, Cancellation::default()).unwrap();
    let mut attempts = 0;
    assert_eq!(
        retry_busy(
            &deadline,
            || {
                attempts += 1;
                Err::<(), _>(NativeError::OutcomeUnknown)
            },
            |_| panic!("Unknown must not retry")
        ),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(attempts, 1);
}

#[derive(Clone)]
struct HistoryFake {
    source: Vec<Option<HistoryObservation>>,
    destination: Vec<Option<HistoryObservation>>,
    saved: Option<FirstHistoryIntent>,
    index: FirstHistoryIndex,
    effects: usize,
    cut: Option<usize>,
}
impl FirstHistoryPort for HistoryFake {
    fn renew(&mut self, intent: &FirstHistoryIntent) -> NativeResult<()> {
        intent.validate()
    }
    fn persist_intent(&mut self, intent: &FirstHistoryIntent) -> NativeResult<()> {
        self.saved = Some(intent.clone());
        Ok(())
    }
    fn observe_pair(
        &mut self,
        intent: &FirstHistoryIntent,
        leaf: &HistoryLeaf,
    ) -> NativeResult<(Option<HistoryObservation>, Option<HistoryObservation>)> {
        let n = intent
            .selected
            .leaves
            .iter()
            .position(|old| old.leaf == leaf.leaf)
            .ok_or(NativeError::Foreign)?;
        Ok((self.source[n].clone(), self.destination[n].clone()))
    }
    fn move_exact(&mut self, intent: &FirstHistoryIntent, leaf: &HistoryLeaf) -> NativeResult<()> {
        let n = intent
            .selected
            .leaves
            .iter()
            .position(|old| old.leaf == leaf.leaf)
            .ok_or(NativeError::Foreign)?;
        self.destination[n] = self.source[n].take();
        self.effects += 1;
        if self.cut == Some(self.effects) {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    fn persist_index(&mut self, index: &FirstHistoryIndex) -> NativeResult<()> {
        self.index = index.clone();
        Ok(())
    }
}
fn history_fixture() -> (HistoryFake, FirstHistoryIntent) {
    let source = [
        "supervisor.json",
        "task-activation.json",
        "supervisor-logon.json",
        "removal.json",
    ]
    .into_iter()
    .enumerate()
    .map(|(n, name)| {
        let id = FileStamp {
            volume: 1,
            file: [n as u8 + 10; 16],
        };
        let bytes = name.as_bytes().to_vec();
        (
            HistoryLeaf::observe(name.into(), id, &bytes).unwrap(),
            (id, bytes),
        )
    })
    .collect::<Vec<_>>();
    let index = FirstHistoryIndex::default();
    let intent = index
        .select(FirstHistorySlot {
            source: FirstHistorySource::Removal,
            operation: [1; 16],
            next_operation: [2; 16],
            context: vec![1],
            leaves: source.iter().map(|(leaf, _)| leaf.clone()).collect(),
        })
        .unwrap();
    (
        HistoryFake {
            destination: vec![None; source.len()],
            source: source.into_iter().map(|(_, bytes)| Some(bytes)).collect(),
            saved: None,
            index,
            effects: 0,
            cut: None,
        },
        intent,
    )
}
#[test]
fn a8b_history_each_move_interruption_observes_without_replay() {
    for cut in 1..=4 {
        let (mut port, mut intent) = history_fixture();
        let original = port.source.clone();
        let mut index = port.index.clone();
        port.cut = Some(cut);
        assert_eq!(
            archive_history(&mut port, &mut intent, &mut index),
            Err(NativeError::OutcomeUnknown)
        );
        let mut reopened = port.saved.clone().unwrap();
        port.cut = None;
        archive_history(&mut port, &mut reopened, &mut index).unwrap();
        assert!(reopened.complete);
        assert_eq!(port.effects, 4);
        assert!(port.source.iter().all(Option::is_none));
        assert_eq!(port.destination, original); // Exact bytes and FileIds, no old claim edit.
        let effects = port.effects;
        archive_history(&mut port, &mut reopened, &mut index).unwrap();
        assert_eq!(port.effects, effects);
    }
}
#[test]
fn a8b_history_drift_and_ambiguous_pairs_refuse_before_a_move() {
    for both in [false, true] {
        let (mut port, mut intent) = history_fixture();
        let mut index = port.index.clone();
        if both {
            port.destination[0] = port.source[0].clone();
        } else {
            port.source[0].as_mut().unwrap().1.push(0);
        }
        assert_eq!(
            archive_history(&mut port, &mut intent, &mut index),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(port.effects, 0);
        assert!(index.slots.iter().all(Option::is_none));
    }
}
#[test]
fn a8b_full_archive_has_no_eviction_or_effect() {
    let (mut port, intent) = history_fixture();
    let mut index = port.index.clone();
    for slot in 0..3 {
        let mut selected = intent.selected.clone();
        selected.operation = [slot + 1; 16];
        selected.next_operation = [slot + 11; 16];
        index.slots[usize::from(slot)] = Some(selected);
    }
    let original = index.clone();
    assert_eq!(
        index.select(intent.selected.clone()),
        Err(NativeError::Busy)
    );
    assert_eq!(index, original);
    assert_eq!(port.effects, 0);
    let mut rejected = intent.clone();
    rejected.slot = 0;
    assert_eq!(
        archive_history(&mut port, &mut rejected, &mut index),
        Err(NativeError::Foreign)
    );
    assert_eq!(port.effects, 0);
}
#[test]
fn a8b_archive_claim_bytes_stay_immutable_and_gate_new_epoch() {
    let (mut port, mut intent) = history_fixture();
    let mut index = port.index.clone();
    assert!(index.slots[0].is_none());
    archive_history(&mut port, &mut intent, &mut index).unwrap();
    assert_eq!(index.slots[0], Some(intent.selected.clone()));
    let mut events = vec!["archive-complete"];
    struct Epoch<'a>(&'a mut Vec<&'static str>);
    impl service::supervisor::InitialEpochPort for Epoch<'_> {
        type Child = ();
        type Ready = ();
        fn prepare_epoch(&mut self) -> NativeResult<()> {
            self.0.push("fresh-claim-and-owner");
            Ok(())
        }
        fn create(&mut self) -> NativeResult<()> {
            self.0.push("create");
            Ok(())
        }
        fn await_ready(&mut self, _: &()) -> NativeResult<()> {
            self.0.push("ready");
            Ok(())
        }
        fn publish_running(&mut self, _: &()) -> NativeResult<()> {
            self.0.push("running");
            Ok(())
        }
    }
    service::supervisor::initialize_epoch(&mut Epoch(&mut events)).unwrap();
    assert_eq!(
        events,
        [
            "archive-complete",
            "fresh-claim-and-owner",
            "create",
            "ready",
            "running"
        ]
    );
    let bytes = intent.encode().unwrap();
    assert_eq!(FirstHistoryIntent::decode(&bytes).unwrap(), intent);
}

#[derive(Clone)]
struct RecoveryFake {
    saved: Option<FirstRecoveryRecord>,
    triads: [RoleTriad; 4],
    task: bool,
    task_delete: usize,
    moves: [usize; 4],
    restores: [usize; 4],
    deletes: [usize; 4],
    retired: bool,
    events: Vec<&'static str>,
    cut: Option<usize>,
    effects: usize,
    scaffold: bool,
    refuse_delete: Option<PayloadRole>,
}
impl RecoveryFake {
    fn new(mode: FirstRecoveryMode) -> (Self, FirstRecoveryRecord) {
        let mut source = record();
        for role in PayloadRole::ALL {
            source.role_mut(role).unwrap().staged = Some(image(role));
        }
        for role in PayloadRole::ALL {
            let original = FileStamp {
                volume: 1,
                file: [40 + role as u8; 16],
            };
            let r = source.role_mut(role).unwrap();
            r.original = OriginalLeaf::Present(original);
            r.backup = Some(original);
            r.published = Some(image(role));
        }
        source.advance(Phase::FilesVerified).unwrap();
        source.advance(Phase::TaskRegistered).unwrap();
        let bytes = source.encode().unwrap();
        let selected = HistoryLeaf::observe(
            "first-install.json".into(),
            FileStamp {
                volume: 1,
                file: [70; 16],
            },
            &bytes,
        )
        .unwrap();
        let recovery = FirstRecoveryRecord::new(selected, source, mode).unwrap();
        let triads = PayloadRole::ALL.map(|role| RoleTriad {
            stage: None,
            fixed: Some(image(role).identity),
            backup: Some(FileStamp {
                volume: 1,
                file: [40 + role as u8; 16],
            }),
        });
        (
            Self {
                saved: None,
                triads,
                task: true,
                task_delete: 0,
                moves: [0; 4],
                restores: [0; 4],
                deletes: [0; 4],
                retired: false,
                events: vec![],
                cut: None,
                effects: 0,
                scaffold: true,
                refuse_delete: None,
            },
            recovery,
        )
    }
    /// A first install cut at `StageIntent(role)`: earlier roles are staged, `role` has no
    /// persisted staged identity, `recorded` is the stray stamp selection observed, and `leaf` is
    /// what currently sits at that role's stage name.
    fn stage_intent(
        mode: FirstRecoveryMode,
        role: PayloadRole,
        recorded: Option<FileStamp>,
        leaf: Option<FileStamp>,
    ) -> (Self, FirstRecoveryRecord) {
        let mut source = record();
        for earlier in PayloadRole::ALL {
            if (earlier as usize) < (role as usize) {
                source.role_mut(earlier).unwrap().staged = Some(image(earlier));
            }
        }
        source.advance(Phase::StageIntent(role)).unwrap();
        let bytes = source.encode().unwrap();
        let selected = HistoryLeaf::observe(
            "first-install.json".into(),
            FileStamp {
                volume: 1,
                file: [70; 16],
            },
            &bytes,
        )
        .unwrap();
        let mut recovery = FirstRecoveryRecord::new(selected, source, mode).unwrap();
        recovery.stray_stage = recorded.map(|id| (role, id));
        recovery.validate().unwrap();
        let triads = PayloadRole::ALL.map(|r| RoleTriad {
            stage: if r == role {
                leaf
            } else if (r as usize) < (role as usize) {
                Some(image(r).identity)
            } else {
                None
            },
            fixed: None,
            backup: None,
        });
        (
            Self {
                saved: None,
                triads,
                task: true,
                task_delete: 0,
                moves: [0; 4],
                restores: [0; 4],
                deletes: [0; 4],
                retired: false,
                events: vec![],
                cut: None,
                effects: 0,
                scaffold: true,
                refuse_delete: None,
            },
            recovery,
        )
    }
    fn cut(&mut self) -> NativeResult<()> {
        self.effects += 1;
        if self.cut == Some(self.effects) {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn persist(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        r.validate()?;
        if let Some(old) = &self.saved {
            r.follows(old)?;
        }
        self.saved = Some(r.clone());
        self.cut()
    }
    fn delete(
        &mut self,
        r: &FirstRecoveryRecord,
        role: PayloadRole,
        location: removal::inventory::PartialFirstLocation,
        id: FileStamp,
    ) -> NativeResult<bool> {
        use removal::inventory::PartialFirstLocation as L;
        assert!(!self.task);
        assert!(matches!(r.cursor, FirstRecoveryCursor::Role { .. }));
        if self.refuse_delete == Some(role) {
            return Ok(false);
        }
        let triad = &mut self.triads[role as usize];
        let value = match location {
            L::Fixed => &mut triad.fixed,
            L::Stage => &mut triad.stage,
            L::Backup => &mut triad.backup,
        };
        if value.is_some() {
            assert_eq!(*value, Some(id));
            *value = None;
            self.deletes[role as usize] += 1;
            self.events.push("delete");
            self.cut()?;
        }
        Ok(true)
    }
    fn retire(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        assert_eq!(r.cursor, FirstRecoveryCursor::RetireIntent);
        if !self.retired {
            self.retired = true;
            self.events.push("retire");
            self.cut()?;
        }
        Ok(())
    }
}
impl FirstRecoveryPort for RecoveryFake {
    fn renew_recovery(&mut self, _: &FirstRecoveryRecord) -> NativeResult<()> {
        Ok(())
    }
    fn persist_recovery(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        self.persist(r)
    }
    fn task_absent(&mut self, _: &FirstRecoveryRecord) -> NativeResult<bool> {
        Ok(!self.task)
    }
    fn delete_task(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        assert_eq!(r.cursor, FirstRecoveryCursor::TaskDeleteIntent);
        assert!(self.task);
        self.task = false;
        self.task_delete += 1;
        self.events.push("task-delete");
        self.cut()
    }
    fn triad(&mut self, _: &FirstRecoveryRecord, role: PayloadRole) -> NativeResult<RoleTriad> {
        Ok(self.triads[role as usize])
    }
    fn unpublish(&mut self, _: &FirstRecoveryRecord, role: PayloadRole) -> NativeResult<()> {
        assert!(!self.task);
        let t = &mut self.triads[role as usize];
        assert!(t.stage.is_none());
        t.stage = t.fixed.take();
        self.moves[role as usize] += 1;
        self.events.push("unpublish");
        self.cut()
    }
    fn restore_original(&mut self, _: &FirstRecoveryRecord, role: PayloadRole) -> NativeResult<()> {
        assert!(!self.task);
        let t = &mut self.triads[role as usize];
        assert!(t.fixed.is_none());
        t.fixed = t.backup.take();
        self.restores[role as usize] += 1;
        self.events.push("restore");
        self.cut()
    }
    fn delete_stage(&mut self, r: &FirstRecoveryRecord, role: PayloadRole) -> NativeResult<()> {
        let id = r.stage_identity(role)?.unwrap();
        if self.delete(r, role, removal::inventory::PartialFirstLocation::Stage, id)? {
            Ok(())
        } else {
            Err(NativeError::Unavailable)
        }
    }
    fn settle_scaffold(&mut self, _: &FirstRecoveryRecord) -> NativeResult<bool> {
        Ok(self.scaffold)
    }
    fn retire_first(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        self.retire(r)
    }
}
impl removal::executor::PartialFirstRemovalPort for RecoveryFake {
    fn renew_partial(&mut self, _: &FirstRecoveryRecord) -> NativeResult<()> {
        Ok(())
    }
    fn persist_partial(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        self.persist(r)
    }
    fn task_absent_partial(&mut self, r: &FirstRecoveryRecord) -> NativeResult<bool> {
        FirstRecoveryPort::task_absent(self, r)
    }
    fn delete_task_partial(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        FirstRecoveryPort::delete_task(self, r)
    }
    fn observe_partial(
        &mut self,
        _: &FirstRecoveryRecord,
        role: PayloadRole,
        location: removal::inventory::PartialFirstLocation,
    ) -> NativeResult<Option<FileStamp>> {
        use removal::inventory::PartialFirstLocation as L;
        let t = self.triads[role as usize];
        Ok(match location {
            L::Fixed => t.fixed,
            L::Stage => t.stage,
            L::Backup => t.backup,
        })
    }
    fn delete_partial(
        &mut self,
        r: &FirstRecoveryRecord,
        role: PayloadRole,
        location: removal::inventory::PartialFirstLocation,
        id: FileStamp,
    ) -> NativeResult<bool> {
        self.delete(r, role, location, id)
    }
    fn settle_scaffold(&mut self, _: &FirstRecoveryRecord) -> NativeResult<bool> {
        Ok(self.scaffold)
    }
    fn retire_partial(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        self.retire(r)
    }
}
#[test]
fn a8b_partial_rollback_every_cut_restores_originals_without_replay() {
    let (mut baseline, mut r) = RecoveryFake::new(FirstRecoveryMode::Rollback);
    assert_eq!(
        recover_first(&mut baseline, &mut r).unwrap(),
        FirstRecoveryOutcome::RolledBack
    );
    let boundaries = baseline.effects;
    for cut in 1..=boundaries {
        let (mut fake, mut r) = RecoveryFake::new(FirstRecoveryMode::Rollback);
        fake.cut = Some(cut);
        assert!(recover_first(&mut fake, &mut r).is_err(), "cut {cut}");
        fake.cut = None;
        r = fake.saved.clone().unwrap();
        assert_eq!(
            recover_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::RolledBack,
            "cut {cut}"
        );
        assert_eq!(fake.task_delete, 1);
        assert_eq!(fake.moves, [1; 4]);
        assert_eq!(fake.restores, [1; 4]);
        assert_eq!(fake.deletes, [1; 4]);
        for role in PayloadRole::ALL {
            assert_eq!(
                fake.triads[role as usize].fixed,
                Some(FileStamp {
                    volume: 1,
                    file: [40 + role as u8; 16]
                })
            );
            assert!(fake.triads[role as usize].stage.is_none());
            assert!(fake.triads[role as usize].backup.is_none());
        }
        assert_eq!(fake.events.first(), Some(&"task-delete"));
        assert_eq!(fake.events.last(), Some(&"retire"));
    }
}
#[test]
fn a8b_partial_removal_is_independent_retains_foreign_and_refuses_erase() {
    let (mut fake, mut r) = RecoveryFake::new(FirstRecoveryMode::Rollback);
    let foreign = FileStamp {
        volume: 1,
        file: [99; 16],
    };
    fake.triads[3].fixed = Some(foreign);
    assert!(recover_first(&mut fake, &mut r).is_err());
    let mut removal = r.select_removal().unwrap();
    assert_eq!(
        removal::executor::remove_partial_first(&mut fake, &mut removal).unwrap(),
        FirstRecoveryOutcome::Retained { pending: 1 }
    );
    assert_eq!(fake.triads[3].fixed, Some(foreign));
    assert!(!fake.retired);
    assert!(fake.deletes[..3].iter().all(|c| *c == 2));
    assert_eq!(
        removal::partial_first_removal_allowed(true),
        Err(NativeError::Unsupported)
    );
    assert!(removal::partial_first_removal_allowed(false).is_ok());
    let mut retry = removal.retry_retained().unwrap();
    fake.triads[3].fixed = None;
    assert_eq!(
        removal::executor::remove_partial_first(&mut fake, &mut retry).unwrap(),
        FirstRecoveryOutcome::Removed
    );
    assert_eq!(fake.task_delete, 1);
    assert!(fake.retired);
}
#[test]
fn a8b_partial_removal_each_cut_and_mapped_object_are_not_replayed() {
    let (mut baseline, mut r) = RecoveryFake::new(FirstRecoveryMode::Remove);
    removal::executor::remove_partial_first(&mut baseline, &mut r).unwrap();
    for cut in 1..=baseline.effects {
        let (mut fake, mut r) = RecoveryFake::new(FirstRecoveryMode::Remove);
        fake.cut = Some(cut);
        assert!(removal::executor::remove_partial_first(&mut fake, &mut r).is_err());
        fake.cut = None;
        r = fake.saved.clone().unwrap();
        assert_eq!(
            removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::Removed
        );
        assert_eq!(fake.task_delete, 1);
        assert_eq!(fake.deletes, [2; 4]);
    }
    let (mut fake, mut r) = RecoveryFake::new(FirstRecoveryMode::Remove);
    fake.refuse_delete = Some(PayloadRole::Installer);
    assert_eq!(
        removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
        FirstRecoveryOutcome::Retained { pending: 1 }
    );
    assert!(!fake.retired);
    assert_eq!(fake.deletes[PayloadRole::Installer as usize], 0);
    assert!(fake.triads[PayloadRole::Installer as usize].fixed.is_some());
}
/// Earlier roles delete their staged leaves; the cut role deletes only its recorded stray leaf.
fn stage_intent_deletes(role: PayloadRole, leaf_present: bool) -> [usize; 4] {
    PayloadRole::ALL
        .map(|r| usize::from((r as usize) < (role as usize) || (r == role && leaf_present)))
}
fn stray_stamp(role: PayloadRole) -> FileStamp {
    FileStamp {
        volume: 1,
        file: [90 + role as u8; 16],
    }
}
#[test]
fn a8b_stage_intent_cut_with_stray_leaf_rolls_back_and_removes() {
    for role in PayloadRole::ALL {
        let stray = stray_stamp(role);
        let (mut baseline, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Rollback, role, Some(stray), Some(stray));
        assert_eq!(
            recover_first(&mut baseline, &mut r).unwrap(),
            FirstRecoveryOutcome::RolledBack,
            "role {role:?}"
        );
        assert_eq!(baseline.deletes, stage_intent_deletes(role, true));
        assert_eq!(baseline.moves, [0; 4]);
        assert_eq!(baseline.restores, [0; 4]);
        assert_eq!(baseline.task_delete, 1);
        assert!(baseline.retired);
        assert!(
            baseline
                .triads
                .iter()
                .all(|t| t.stage.is_none() && t.fixed.is_none())
        );
        for cut in 1..=baseline.effects {
            let (mut fake, mut r) = RecoveryFake::stage_intent(
                FirstRecoveryMode::Rollback,
                role,
                Some(stray),
                Some(stray),
            );
            fake.cut = Some(cut);
            assert!(
                recover_first(&mut fake, &mut r).is_err(),
                "role {role:?} cut {cut}"
            );
            fake.cut = None;
            r = fake.saved.clone().unwrap();
            assert_eq!(
                recover_first(&mut fake, &mut r).unwrap(),
                FirstRecoveryOutcome::RolledBack,
                "role {role:?} cut {cut}"
            );
            assert_eq!(fake.deletes, stage_intent_deletes(role, true), "cut {cut}");
            assert_eq!(fake.task_delete, 1, "cut {cut}");
            assert!(fake.retired, "cut {cut}");
        }
        let (mut baseline, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Remove, role, Some(stray), Some(stray));
        assert_eq!(
            removal::executor::remove_partial_first(&mut baseline, &mut r).unwrap(),
            FirstRecoveryOutcome::Removed,
            "role {role:?}"
        );
        assert_eq!(baseline.deletes, stage_intent_deletes(role, true));
        assert_eq!(baseline.task_delete, 1);
        for cut in 1..=baseline.effects {
            let (mut fake, mut r) = RecoveryFake::stage_intent(
                FirstRecoveryMode::Remove,
                role,
                Some(stray),
                Some(stray),
            );
            fake.cut = Some(cut);
            assert!(
                removal::executor::remove_partial_first(&mut fake, &mut r).is_err(),
                "role {role:?} cut {cut}"
            );
            fake.cut = None;
            r = fake.saved.clone().unwrap();
            assert_eq!(
                removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
                FirstRecoveryOutcome::Removed,
                "role {role:?} cut {cut}"
            );
            assert_eq!(fake.deletes, stage_intent_deletes(role, true), "cut {cut}");
            assert_eq!(fake.task_delete, 1, "cut {cut}");
            assert!(fake.retired, "cut {cut}");
        }
        // A leaf that vanished before its effect is already settled, never a deletion.
        let (mut fake, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Rollback, role, Some(stray), None);
        assert_eq!(
            recover_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::RolledBack,
            "role {role:?}"
        );
        assert_eq!(fake.deletes, stage_intent_deletes(role, false));
        let (mut fake, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Remove, role, Some(stray), None);
        assert_eq!(
            removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::Removed,
            "role {role:?}"
        );
        assert_eq!(fake.deletes, stage_intent_deletes(role, false));
    }
}
#[test]
fn a8b_stage_intent_stray_leaf_replaced_is_retained_not_deleted() {
    for role in PayloadRole::ALL {
        let stray = stray_stamp(role);
        let other = FileStamp {
            volume: 1,
            file: [120 + role as u8; 16],
        };
        let (mut fake, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Rollback, role, Some(stray), Some(other));
        assert_eq!(
            recover_first(&mut fake, &mut r),
            Err(NativeError::OutcomeUnknown),
            "role {role:?}"
        );
        assert_eq!(fake.triads[role as usize].stage, Some(other));
        assert_eq!(fake.deletes[role as usize], 0);
        assert!(!fake.retired);
        let (mut fake, mut r) =
            RecoveryFake::stage_intent(FirstRecoveryMode::Remove, role, Some(stray), Some(other));
        assert_eq!(
            removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::Retained { pending: 1 },
            "role {role:?}"
        );
        assert_eq!(fake.triads[role as usize].stage, Some(other));
        assert_eq!(fake.deletes[role as usize], 0);
        assert!(!fake.retired);
    }
}
#[test]
fn a8b_recovery_never_replays_register_or_run_or_erases_unknown_scaffolds() {
    let (mut fake, mut r) = RecoveryFake::new(FirstRecoveryMode::Remove);
    fake.scaffold = false;
    assert_eq!(
        removal::executor::remove_partial_first(&mut fake, &mut r).unwrap(),
        FirstRecoveryOutcome::Retained { pending: 1 }
    );
    assert!(!fake.retired);
    let mut run = r.document.clone();
    run.advance(Phase::RunIntent).unwrap();
    assert_eq!(
        recovery_mode_allowed(&run, FirstRecoveryMode::Remove),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(
        recovery_mode_allowed(&run, FirstRecoveryMode::Rollback),
        Err(NativeError::OutcomeUnknown)
    );
    assert!(recovery_mode_allowed(&run, FirstRecoveryMode::RetireStale).is_ok());
}

struct StaleFake {
    saved: Option<FirstRecoveryRecord>,
    live: Option<u64>,
    exclusive: bool,
    claim: Vec<u8>,
    retired: usize,
    effects: usize,
    cut: Option<usize>,
}
impl FirstStalePort for StaleFake {
    fn renew_stale(&mut self, _: &FirstRecoveryRecord) -> NativeResult<()> {
        Ok(())
    }
    fn observe_supersession(&mut self, _: &FirstRecoveryRecord) -> NativeResult<Option<u64>> {
        Ok(self.live)
    }
    fn reserve_absent(&mut self, _: &FirstRecoveryRecord) -> NativeResult<()> {
        if self.exclusive {
            Ok(())
        } else {
            Err(NativeError::Busy)
        }
    }
    fn persist_stale(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        if let Some(old) = &self.saved {
            r.follows(old)?;
        }
        self.saved = Some(r.clone());
        self.effects += 1;
        if self.cut == Some(self.effects) {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn retire_stale(&mut self, r: &FirstRecoveryRecord) -> NativeResult<()> {
        assert_eq!(r.cursor, FirstRecoveryCursor::RetireIntent);
        assert!(self.exclusive);
        if self.retired == 0 {
            self.retired = 1;
            self.effects += 1;
            if self.cut == Some(self.effects) {
                return Err(NativeError::OutcomeUnknown);
            }
        }
        Ok(())
    }
}
fn stale_fixture(mode: FirstRecoveryMode) -> (StaleFake, FirstRecoveryRecord) {
    use native_io::activation::{SupervisorClaim, TaskActivationRecord};
    let (_, first) = RecoveryFake::new(FirstRecoveryMode::Rollback);
    let mut document = first.document;
    document.advance(Phase::RunObserved).unwrap();
    let bytes = document.encode().unwrap();
    let source =
        HistoryLeaf::observe("first-install.json".into(), first.source.identity, &bytes).unwrap();
    let mut task = TaskActivationRecord::new(
        document.operation(),
        "S-1-5-21-1".into(),
        native_io::files::FileIdentity {
            volume: 1,
            file: [1; 16],
        },
        None,
    )
    .unwrap();
    task.registered().unwrap();
    task.run_intent().unwrap();
    task.claim_supervisor(SupervisorClaim {
        pid: 1,
        creation: 1,
    })
    .unwrap();
    (
        StaleFake {
            saved: None,
            live: Some(2),
            exclusive: true,
            claim: task.encode().unwrap(),
            retired: 0,
            effects: 0,
            cut: None,
        },
        FirstRecoveryRecord::new(source, document, mode).unwrap(),
    )
}
#[test]
fn a8b_stale_new_logon_settles_metadata_and_preserves_the_consumed_claim() {
    let (mut fake, mut r) = stale_fixture(FirstRecoveryMode::Supersede);
    let before = fake.claim.clone();
    assert_eq!(
        settle_stale_first(&mut fake, &mut r).unwrap(),
        FirstRecoveryOutcome::Superseded
    );
    assert_eq!(r.cursor, FirstRecoveryCursor::Superseded);
    assert_eq!(r.document.phase(), Phase::RunObserved);
    assert_eq!(fake.claim, before);
    assert_eq!(fake.retired, 0);
    let count = fake.effects;
    assert_eq!(
        settle_stale_first(&mut fake, &mut r).unwrap(),
        FirstRecoveryOutcome::Superseded
    );
    assert_eq!(fake.effects, count);
    fake.live = None;
    assert_eq!(
        settle_stale_first(&mut fake, &mut r),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(fake.claim, before);
}
#[test]
fn a8b_stale_absence_retirement_each_cut_never_resets_or_runs_claim() {
    for cut in 1..=4 {
        let (mut fake, mut r) = stale_fixture(FirstRecoveryMode::RetireStale);
        fake.live = None;
        let before = fake.claim.clone();
        fake.cut = Some(cut);
        assert!(settle_stale_first(&mut fake, &mut r).is_err());
        fake.cut = None;
        r = fake.saved.clone().unwrap();
        assert_eq!(
            settle_stale_first(&mut fake, &mut r).unwrap(),
            FirstRecoveryOutcome::Retired
        );
        assert_eq!(fake.retired, 1);
        assert_eq!(fake.claim, before);
    }
    let (mut fake, mut r) = stale_fixture(FirstRecoveryMode::RetireStale);
    fake.exclusive = false;
    assert_eq!(
        settle_stale_first(&mut fake, &mut r),
        Err(NativeError::Busy)
    );
    assert_eq!(fake.effects, 0);
    assert_eq!(fake.retired, 0);
}

/// Every role staged, published and original-missing, persisted at FilesVerified.
fn verified_record() -> FirstInstallRecord {
    let mut r = record();
    for role in PayloadRole::ALL {
        let value = r.role_mut(role).unwrap();
        value.original = OriginalLeaf::Missing;
        value.staged = Some(image(role));
        value.published = Some(image(role));
    }
    r.advance(Phase::FilesVerified).unwrap();
    r
}
#[test]
fn elevated_runs_after_files_verified_is_durable_and_before_task_intent() {
    let mut port = Fake::default();
    let mut r = record();
    apply(&mut port, &mut r).unwrap();
    assert_eq!(r.phase(), Phase::Complete);
    let position = |value: &str| port.events.iter().position(|e| e == value).unwrap();
    let verified = position("verify-files");
    let persisted = position("intent-after:FilesVerified");
    let elevated = position("elevated");
    let task = position("intent-before:TaskIntent");
    let register = position("register-before");
    assert!(verified < persisted);
    assert!(persisted < elevated);
    assert!(elevated < task);
    assert!(task < register);
    // The hook runs after the FilesVerified record is the persisted one.
    assert_eq!(port.elevated_phase, Some(Phase::FilesVerified));
}
#[test]
fn elevated_is_called_exactly_once_per_forward_apply() {
    let mut port = Fake::default();
    apply(&mut port, &mut record()).unwrap();
    assert_eq!(port.elevated_calls, 1);
    // A forward Apply restarted from Intent under a fresh logon is still one forward Apply.
    let old = context_record(&token(20, 1));
    let mut next = match reopen(&old, &token(20, 2)).unwrap() {
        Reopen::Restart(next) => next,
        _ => panic!("Intent must restart"),
    };
    let mut port = Fake::default();
    apply(&mut port, &mut next).unwrap();
    assert_eq!(next.phase(), Phase::Complete);
    assert_eq!(port.elevated_calls, 1);
}
#[test]
fn elevated_is_never_called_when_a_reopen_resumes_or_observes() {
    // A record already at FilesVerified resumes without the administrator step.
    let mut verified = verified_record();
    let mut port = Fake {
        saved: Some(verified.clone()),
        fixed: PayloadRole::ALL.map(|role| Some(image(role))),
        ..Fake::default()
    };
    resume(&mut port, &mut verified).unwrap();
    assert_eq!(verified.phase(), Phase::Complete);
    assert_eq!(port.elevated_calls, 0);
    assert_eq!((port.register, port.runs), (1, 1));
    // Observe-only states after the Run never reach the hook either.
    for phase in [
        Phase::RunIntent,
        Phase::RunObserved,
        Phase::Ready,
        Phase::PruneIntent,
    ] {
        let mut old = context_record(&token(20, 1));
        for role in PayloadRole::ALL {
            let value = old.role_mut(role).unwrap();
            value.staged = Some(image(role));
            value.published = Some(image(role));
            value.original = OriginalLeaf::Missing;
        }
        if phase.rank() >= Phase::Ready.rank() {
            old.ready(99).unwrap();
        }
        old.advance(phase).unwrap();
        let mut observed = match reopen(&old, &token(20, 2)).unwrap() {
            Reopen::Observe(value) => value,
            _ => panic!("post-Run is observe-only"),
        };
        let mut port = Fake {
            saved: Some(old.clone()),
            fixed: PayloadRole::ALL.map(|role| Some(image(role))),
            register: 1,
            runs: 1,
            ready: true,
            running: true,
            ..Fake::default()
        };
        resume(&mut port, &mut observed).unwrap();
        assert_eq!(observed.phase(), Phase::Complete);
        assert_eq!(port.elevated_calls, 0, "elevated on Observe from {phase:?}");
    }
}
#[test]
fn elevated_is_called_at_most_once_across_every_interruption_and_never_on_reopen() {
    let mut reference = Fake::default();
    apply(&mut reference, &mut record()).unwrap();
    assert_eq!(reference.elevated_calls, 1);
    for cut in 1..=reference.steps {
        let mut port = Fake {
            cut: Some(cut),
            ..Fake::default()
        };
        let _ = apply(&mut port, &mut record());
        let forward = port.elevated_calls;
        assert!(forward <= 1, "elevated repeated at boundary {cut}");
        if let Some(saved) = port.saved.clone() {
            let mut reopened = FirstInstallRecord::decode(&saved.encode().unwrap()).unwrap();
            port.cut = None;
            let _ = resume(&mut port, &mut reopened);
            assert_eq!(
                port.elevated_calls, forward,
                "elevated on reopen at boundary {cut}"
            );
        }
    }
}
#[test]
fn elevated_error_ends_the_install_before_any_registration_or_run() {
    for error in [NativeError::OutcomeUnknown, NativeError::Foreign] {
        let mut port = Fake {
            elevated_error: Some(error),
            ..Fake::default()
        };
        let mut r = record();
        assert_eq!(apply(&mut port, &mut r), Err(error));
        assert_eq!(port.elevated_calls, 1);
        assert_eq!((port.register, port.runs), (0, 0));
        assert!(!port.ready && !port.running);
        assert!(!port.events.iter().any(|e| {
            e == "intent-before:TaskIntent"
                || e.starts_with("register-")
                || e.starts_with("run-")
                || e.starts_with("observe-")
                || e == "retention-observe"
        }));
        // The durable record stops at FilesVerified; no later phase was persisted.
        assert_eq!(r.phase(), Phase::FilesVerified);
        assert_eq!(
            port.saved.as_ref().map(FirstInstallRecord::phase),
            Some(Phase::FilesVerified)
        );
    }
}
#[test]
fn classifier_ignores_the_elevated_record_in_every_branch() {
    use FirstInstallDisposition as D;
    use crosspane_installer_core::elevated::journal::RECORD_LEAF;
    use removal::RemovalCursor as R;
    use service::journal::Phase as Journal;
    assert_eq!(RECORD_LEAF, "elevated-setup.json");
    type Branch = (
        &'static [&'static str],
        Option<R>,
        Option<Phase>,
        Option<Journal>,
        bool,
        bool,
        D,
    );
    let branches: &[Branch] = &[
        // Fresh and eligible.
        (&[], None, None, None, false, false, D::Eligible),
        (
            &["install.lock"],
            None,
            None,
            None,
            false,
            false,
            D::Eligible,
        ),
        // Partial and foreign or unknown names.
        (
            &["unexpected.json"],
            None,
            None,
            None,
            false,
            false,
            D::Partial,
        ),
        (
            &["task-activation.json"],
            None,
            None,
            None,
            false,
            false,
            D::Partial,
        ),
        // Completed removal, by the removal cursor.
        (
            &[],
            Some(R::Retired),
            None,
            None,
            false,
            false,
            D::CompletedRemoval,
        ),
        (
            &["removal.json"],
            Some(R::Complete {
                retained_copy: None,
            }),
            None,
            None,
            false,
            false,
            D::CompletedRemoval,
        ),
        (
            &["removal.json"],
            Some(R::Selected),
            None,
            None,
            false,
            false,
            D::Partial,
        ),
        // Stale supervisor epochs, a finished journal, a logon or a claimed activation.
        (
            &["supervisor-epoch-0.json"],
            None,
            None,
            None,
            false,
            false,
            D::CompletedRemoval,
        ),
        (
            &["supervisor-epoch-2.json"],
            None,
            None,
            None,
            false,
            false,
            D::CompletedRemoval,
        ),
        (
            &[],
            None,
            None,
            Some(Journal::Finished),
            false,
            false,
            D::CompletedRemoval,
        ),
        (&[], None, None, None, true, false, D::CompletedRemoval),
        (&[], None, None, None, false, true, D::CompletedRemoval),
        // First-install records at each phase family.
        (
            &["first-install.json"],
            None,
            Some(Phase::Intent),
            None,
            false,
            false,
            D::Resume,
        ),
        (
            &["install.lock", "first-install.json"],
            None,
            Some(Phase::Intent),
            None,
            false,
            false,
            D::Resume,
        ),
        (
            &["first-install.json", "unexpected.json"],
            None,
            Some(Phase::Intent),
            None,
            false,
            false,
            D::Partial,
        ),
        (
            &[
                "first-install.json",
                "task-activation.json",
                "supervisor.json",
            ],
            None,
            Some(Phase::RunObserved),
            Some(Journal::Running),
            true,
            true,
            D::Resume,
        ),
        (
            &["first-install.json", "supervisor-epoch-1.json"],
            None,
            Some(Phase::Ready),
            None,
            false,
            false,
            D::Resume,
        ),
        (
            &["removal.json"],
            None,
            Some(Phase::RunObserved),
            None,
            false,
            false,
            D::Partial,
        ),
        (
            &["first-install.json"],
            None,
            Some(Phase::Complete),
            None,
            false,
            false,
            D::Existing,
        ),
        (
            &["first-install.json"],
            None,
            Some(Phase::Unknown),
            None,
            false,
            false,
            D::Unknown,
        ),
        (
            &["first-install.json"],
            None,
            Some(Phase::StageIntent(PayloadRole::Agent)),
            None,
            false,
            false,
            D::Partial,
        ),
    ];
    for &(names, removal, first, journal, logon, claimed_activation, expected) in branches {
        let without: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let mut with = without.clone();
        with.push(RECORD_LEAF.to_owned());
        let classify = |history: &[String]| {
            classify_history(DecodedHistory {
                removal,
                first,
                journal,
                logon,
                claimed_activation,
                names: history,
            })
        };
        assert_eq!(
            classify(&without),
            expected,
            "without the record: {names:?}"
        );
        assert_eq!(classify(&with), expected, "with the record: {names:?}");
    }
}
