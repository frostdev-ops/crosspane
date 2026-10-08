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
