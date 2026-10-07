#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Authored physical metadata worlds drive the real portable repair/publication decisions.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
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
use aws_lc_rs::digest::{SHA256, digest};
use native_io::{
    NativeError, NativeResult,
    files::FileIdentity,
    identity::{Sid, TokenFacts},
    records::*,
};
use payload::recovery::{FileStamp, OuterContextCorrelation};
use repair::{record::*, *};
fn sha256(bytes: &[u8]) -> [u8; 32] {
    digest(&SHA256, bytes).as_ref().try_into().unwrap()
}
fn stamp(n: u8) -> FileStamp {
    FileStamp {
        volume: 7,
        file: [n; 16],
    }
}
fn id(n: u8) -> FileIdentity {
    FileIdentity {
        volume: 7,
        file: [n; 16],
    }
}
fn context() -> OuterContextCorrelation {
    fn sid(parts: &[u32]) -> Sid {
        let mut b = vec![1, parts.len() as u8, 0, 0, 0, 0, 0, 5];
        for p in parts {
            b.extend_from_slice(&p.to_le_bytes());
        }
        Sid::from_bytes(b).unwrap()
    }
    OuterContextCorrelation::new(&TokenFacts {
        user: sid(&[21, 7]),
        logon: sid(&[5, 9, 11]),
        authentication_id: 17,
        session: 2,
        elevated: false,
        integrity: 0x2000,
        impersonating: false,
    })
    .unwrap()
}
const XML: &str = "<Task fixture='exact-enabled-own-canonical'/>";
fn task(d: RepairDiagnostic) -> RepairTaskObservation {
    RepairTaskObservation::new(d, Some(XML.to_owned())).unwrap()
}
fn source(k: SourceKind, n: u8) -> (SourceSnapshot, Vec<u8>) {
    let b = format!("authored terminal metadata {}", n).into_bytes();
    (
        SourceSnapshot::new(k, [n; 16], stamp(n), sha256(&b), b.len() as u64).unwrap(),
        b,
    )
}
fn observation(
    payload: RepairDiagnostic,
    task_d: RepairDiagnostic,
    agent: RepairDiagnostic,
    j: Vec<JournalObservation>,
    publication: RepairDiagnostic,
) -> RepairObservation {
    RepairObservation::new(payload, task(task_d), agent, j, publication).unwrap()
}
fn plan(task_missing: bool, sources: &[(SourceSnapshot, Vec<u8>)]) -> RepairPlan {
    match classify(&observation(
        RepairDiagnostic::Healthy,
        if task_missing {
            RepairDiagnostic::Missing
        } else {
            RepairDiagnostic::Healthy
        },
        RepairDiagnostic::Healthy,
        sources
            .iter()
            .map(|(s, _)| JournalObservation::Terminal(s.clone()))
            .collect(),
        RepairDiagnostic::Healthy,
    ))
    .unwrap()
    {
        RepairDecision::Apply(p) => p,
        _ => panic!("fixture needs work"),
    }
}
#[derive(Clone)]
struct Physical {
    snapshot: SourceSnapshot,
    bytes: Vec<u8>,
}
impl Physical {
    fn exact(&self, s: &SourceSnapshot) -> bool {
        self.snapshot == *s
            && self.bytes.len() as u64 == s.len()
            && sha256(&self.bytes) == s.sha256()
    }
}
#[derive(Clone)]
struct Terminal {
    operation: [u8; 16],
    source: SourceSnapshot,
}
#[derive(Clone, Copy)]
enum Fault {
    Before(usize),
    After(usize),
}
struct Fake {
    durable: RepairRecord,
    sources: Vec<Option<Physical>>,
    destinations: Vec<Option<Physical>>,
    installed: bool,
    disabled: bool,
    task_race: bool,
    settled: bool,
    registrations: u32,
    moves: u32,
    writes: u32,
    finish_writes: u32,
    events: Vec<&'static str>,
    fault: Option<Fault>,
}
impl Fake {
    fn new(task_missing: bool, s: &[(SourceSnapshot, Vec<u8>)]) -> Self {
        let p = plan(task_missing, s);
        let mut r = RepairRecord::new([91; 16], context(), p, None).unwrap();
        if !s.is_empty() {
            let mut index = EvidenceIndex::default();
            let slot = index.reserve(r.operation(), r.plan().archives()).unwrap();
            r.bind_slot(slot).unwrap();
        }
        Self {
            durable: r,
            sources: s
                .iter()
                .map(|(snapshot, bytes)| {
                    Some(Physical {
                        snapshot: snapshot.clone(),
                        bytes: bytes.clone(),
                    })
                })
                .collect(),
            destinations: vec![None; s.len()],
            installed: !task_missing,
            disabled: false,
            task_race: false,
            settled: true,
            registrations: 0,
            moves: 0,
            writes: 0,
            finish_writes: 0,
            events: Vec::new(),
            fault: None,
        }
    }
    fn before(&mut self, name: &'static str) -> NativeResult<usize> {
        let n = self.events.len();
        self.events.push(name);
        if matches!(self.fault,Some(Fault::Before(i)) if i==n) {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(n)
    }
    fn after(&self, n: usize) -> NativeResult<()> {
        if matches!(self.fault,Some(Fault::After(i)) if i==n) {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    fn run(&mut self) -> NativeResult<RepairOutcome> {
        let mut r = self.durable.clone();
        run_repair(self, &mut r)
    }
    fn physical_preserved(&self) {
        for (i, s) in self.durable.plan().archives().iter().enumerate() {
            let all = [self.sources[i].as_ref(), self.destinations[i].as_ref()];
            assert!(all.into_iter().flatten().any(|p| p.exact(s)));
        }
    }
}
impl RepairPort for Fake {
    type Terminal = Terminal;
    fn renew(&mut self, r: &RepairRecord) -> NativeResult<()> {
        let n = self.before("renew")?;
        if self.durable != *r {
            return Err(NativeError::Foreign);
        }
        self.after(n)
    }
    fn persist(&mut self, r: &RepairRecord) -> NativeResult<()> {
        let n = self.before("publish-record")?;
        self.durable.publication_successor(r)?;
        self.durable = r.clone();
        self.writes += 1;
        self.after(n)
    }
    fn task_observe(&mut self, _: &RepairRecord) -> NativeResult<RepairTaskObservation> {
        let n = self.before("task-observe")?;
        let t = task(if self.disabled {
            RepairDiagnostic::Disabled
        } else if self.installed {
            RepairDiagnostic::Healthy
        } else {
            RepairDiagnostic::Missing
        });
        self.after(n)?;
        Ok(t)
    }
    fn register_task(&mut self, r: &RepairRecord) -> NativeResult<()> {
        let n = self.before("register-effect")?;
        if self.task_race {
            self.installed = true;
        }
        if r.cursor() != RepairCursor::TaskIntent
            || self.disabled
            || self.installed
            || self.registrations != 0
        {
            return Err(NativeError::Foreign);
        }
        self.registrations += 1;
        self.installed = true;
        self.after(n)
    }
    fn admit_terminal(&mut self, r: &RepairRecord, index: u8) -> NativeResult<Terminal> {
        let n = self.before("terminal-admit")?;
        if !self.settled {
            return Err(NativeError::OutcomeUnknown);
        }
        let s = r
            .plan()
            .archives()
            .get(usize::from(index))
            .ok_or(NativeError::Invalid)?
            .clone();
        let i = usize::from(index);
        if ![self.sources[i].as_ref(), self.destinations[i].as_ref()]
            .into_iter()
            .flatten()
            .any(|p| p.exact(&s))
        {
            return Err(NativeError::Foreign);
        }
        self.after(n)?;
        Ok(Terminal {
            operation: r.operation(),
            source: s,
        })
    }
    fn observe_archive(&mut self, r: &RepairRecord, index: u8) -> NativeResult<ArchiveObservation> {
        let n = self.before("archive-observe")?;
        assert_eq!(r.cursor(), RepairCursor::ArchiveIntent { index });
        let i = usize::from(index);
        let s = &r.plan().archives()[i];
        let a = self.sources[i].as_ref();
        let b = self.destinations[i].as_ref();
        let o = if a.is_some_and(|p| !p.exact(s)) || b.is_some_and(|p| !p.exact(s)) {
            ArchiveObservation::Changed
        } else {
            match (a.is_some(), b.is_some()) {
                (true, false) => ArchiveObservation::SourceOnly,
                (false, true) => ArchiveObservation::DestinationOnly,
                (true, true) => ArchiveObservation::Both,
                (false, false) => ArchiveObservation::Neither,
            }
        };
        self.after(n)?;
        Ok(o)
    }
    fn archive_exact(&mut self, r: &RepairRecord, index: u8, t: &Terminal) -> NativeResult<()> {
        let n = self.before("archive-effect")?;
        let i = usize::from(index);
        if !self.settled
            || t.operation != r.operation()
            || t.source != r.plan().archives()[i]
            || self.destinations[i].is_some()
            || !self.sources[i].as_ref().is_some_and(|p| p.exact(&t.source))
        {
            return Err(NativeError::Foreign);
        }
        self.destinations[i] = self.sources[i].take();
        self.moves += 1;
        self.after(n)
    }
    fn finish(&mut self, r: &RepairRecord) -> NativeResult<()> {
        let n = self.before("finish-index")?;
        assert_eq!(r.cursor(), RepairCursor::Complete);
        for (i, s) in r.plan().archives().iter().enumerate() {
            if self.sources[i].is_some()
                || !self.destinations[i].as_ref().is_some_and(|p| p.exact(s))
            {
                return Err(NativeError::Foreign);
            }
        }
        if self.finish_writes == 0 {
            self.finish_writes = 1;
        }
        self.after(n)
    }
}
#[test]
fn missing_is_distinct_from_denied_no_effects() {
    use RepairDiagnostic::*;
    let absent = observation(Healthy, Missing, Healthy, vec![], Healthy);
    assert!(matches!(
        classify(&absent).unwrap(),
        RepairDecision::Apply(_)
    ));
    let mut f = Fake::new(true, &[]);
    assert_eq!(f.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(f.registrations, 1);
    let mut race = Fake::new(true, &[]);
    race.task_race = true;
    assert_eq!(race.run(), Err(NativeError::Foreign));
    assert_eq!(race.registrations, 0);
    assert_eq!(race.durable.cursor(), RepairCursor::TaskIntent);
    race.task_race = false;
    assert_eq!(race.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(race.registrations, 0);
    for d in [AccessDenied, UnsafeForeign, Unavailable, Unknown] {
        let o = observation(Healthy, d, Healthy, vec![], Healthy);
        let RepairDecision::Report(i) = classify(&o).unwrap() else {
            panic!("must refuse")
        };
        if d == AccessDenied {
            assert_eq!(i.as_str(), "access denied: retained");
        }
        assert_eq!(f.registrations, 1);
    }
    for d in [Missing, AccessDenied, Unknown] {
        assert!(matches!(
            classify(&observation(Healthy, Healthy, d, vec![], Healthy)).unwrap(),
            RepairDecision::Report(_)
        ));
    }
    // Actual current-module denial is payload denial, never a guessed missing/approval fallback.
    let mut denied = Fake::new(true, &[]);
    let result = match classify(&observation(
        AccessDenied,
        Missing,
        Healthy,
        vec![],
        Healthy,
    ))
    .unwrap()
    {
        RepairDecision::Healthy => RepairOutcome::Healthy,
        RepairDecision::Report(issue) => RepairOutcome::Report(issue),
        RepairDecision::Apply(_) => denied.run().unwrap(),
    };
    assert_eq!(result.as_str(), "access denied: retained");
    assert!(denied.events.is_empty());
    assert_eq!(denied.writes, 0);
    assert_eq!(denied.registrations, 0);
    assert_eq!(denied.moves, 0);
    assert!(RepairTaskObservation::new(Missing, None).is_ok());
    assert!(matches!(
        classify(
            &RepairObservation::new(
                Healthy,
                RepairTaskObservation::new(Missing, None).unwrap(),
                Healthy,
                vec![],
                Healthy
            )
            .unwrap()
        )
        .unwrap(),
        RepairDecision::Report(_)
    ));
}
#[test]
fn resume_or_retire_requires_actual_proof_and_preserves_evidence() {
    use RepairDiagnostic::*;
    let s = vec![
        source(SourceKind::OuterUpgrade, 11),
        source(SourceKind::FileRecovery, 12),
        source(SourceKind::Removal, 13),
    ];
    let mut f = Fake::new(false, &s);
    f.settled = false;
    assert_eq!(f.run(), Err(NativeError::OutcomeUnknown));
    assert_eq!(f.moves, 0);
    assert_eq!(f.writes, 0);
    f.physical_preserved();
    f.settled = true;
    assert_eq!(f.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(f.moves, 3);
    f.physical_preserved();
    let events = f.events.len();
    assert_eq!(f.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(f.moves, 3);
    assert_eq!(f.finish_writes, 1);
    assert!(
        f.events[events..]
            .iter()
            .all(|e| *e == "renew" || *e == "finish-index")
    );
    for d in [Unknown, Unavailable, AccessDenied] {
        assert!(matches!(
            classify(&observation(
                Healthy,
                Missing,
                Healthy,
                vec![JournalObservation::Retained(SourceKind::OuterUpgrade, d)],
                Healthy
            ))
            .unwrap(),
            RepairDecision::Report(_)
        ));
    }
    let mut index = EvidenceIndex::default();
    for n in 1..=3 {
        let op = [n; 16];
        let src = source(SourceKind::Removal, n + 20).0;
        assert_eq!(index.reserve(op, &[src]).unwrap(), n - 1);
    }
    let saved = index.encode().unwrap();
    assert_eq!(
        index.reserve([99; 16], &[s[0].0.clone()]),
        Err(NativeError::Busy)
    );
    assert_eq!(index.encode().unwrap(), saved);
    assert_eq!(EvidenceIndex::decode(&saved).unwrap(), index);
    let mut malformed = serde_json::to_value(&index).unwrap();
    malformed["slots"][0]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<EvidenceIndex>(malformed).is_err());
    let mut r = f.durable.clone();
    assert!(r.advance(RepairCursor::ArchiveIntent { index: 0 }).is_err());
    assert!(r.bind_slot(2).is_err());
}
#[test]
fn partial_or_unknown_publication_retains_all_sources() {
    use RepairDiagnostic::*;
    let s = source(SourceKind::OuterUpgrade, 31);
    for d in [Unknown, AccessDenied, UnsafeForeign, Unavailable] {
        assert!(matches!(
            classify(&observation(
                Healthy,
                Missing,
                Healthy,
                vec![JournalObservation::Terminal(s.0.clone())],
                d
            ))
            .unwrap(),
            RepairDecision::Report(_)
        ));
    }
    let r = Fake::new(false, std::slice::from_ref(&s)).durable;
    let bytes = r.encode().unwrap();
    let old = RepairPublicationStamp::new(id(40), b"old exact bytes").unwrap();
    let pending = RepairPublicationStamp::new(id(41), &bytes).unwrap();
    let mut intent = RepairPublicationIntent::new(
        r.operation(),
        RepairPublicationTarget::Repair,
        Some(old),
        &bytes,
    )
    .unwrap();
    assert_eq!(
        recover_repair_publication(&intent, Some(old), Some(pending)),
        RepairPublicationObservation::Unknown
    );
    intent.pending_ready(pending).unwrap();
    intent.replace_intent().unwrap();
    assert_eq!(
        recover_repair_publication(&intent, None, None),
        RepairPublicationObservation::Unknown
    );
    assert_eq!(
        recover_repair_publication(&intent, Some(pending), Some(pending)),
        RepairPublicationObservation::Unknown
    );
    let wrong_same_bytes = RepairPublicationStamp::new(id(42), &bytes).unwrap();
    assert_eq!(
        recover_repair_publication(&intent, Some(wrong_same_bytes), None),
        RepairPublicationObservation::Unknown
    );
    assert_eq!(
        recover_repair_publication(&intent, Some(pending), None),
        RepairPublicationObservation::Published
    );
    let intent_bytes = intent.encode().unwrap();
    for n in [0, 1, intent_bytes.len() / 2, intent_bytes.len() - 1] {
        assert!(RepairPublicationIntent::decode(&intent_bytes[..n]).is_err());
    }
    let mut f = Fake::new(false, std::slice::from_ref(&s));
    f.destinations[0] = Some(f.sources[0].as_ref().unwrap().clone());
    assert_eq!(f.run().unwrap(), RepairOutcome::Retained);
    assert_eq!(f.moves, 0);
    f.physical_preserved();
    let mut changed = Fake::new(false, std::slice::from_ref(&s));
    changed.sources[0].as_mut().unwrap().bytes[0] ^= 1;
    assert_eq!(changed.run(), Err(NativeError::Foreign));
    assert_eq!(changed.moves, 0);
    let mut neither = Fake::new(false, std::slice::from_ref(&s));
    neither.sources[0] = None;
    assert_eq!(neither.run(), Err(NativeError::Foreign));
    assert_eq!(neither.moves, 0);
    assert_eq!(neither.writes, 0);
    assert_eq!(
        neither.durable.plan().archives(),
        std::slice::from_ref(&s.0)
    );
}
#[test]
fn user_disabled_task_is_preserved() {
    use RepairDiagnostic::*;
    for payload in [Healthy, Missing, Mismatch, AccessDenied, Unknown] {
        let o = observation(payload, Disabled, Healthy, vec![], Healthy);
        let RepairDecision::Report(i) = classify(&o).unwrap() else {
            panic!("disabled never apply")
        };
        if matches!(payload, Missing | Mismatch) {
            assert!(i.as_str().contains("a6b / reinstall"));
            assert!(i.as_str().contains("disabled task preserved"));
        }
    }
    for d in [
        RepairDiagnostic::Unknown,
        RepairDiagnostic::AccessDenied,
        RepairDiagnostic::Unavailable,
    ] {
        for (debt, publication) in [
            (vec![], d),
            (
                vec![JournalObservation::Retained(SourceKind::OuterUpgrade, d)],
                Healthy,
            ),
        ] {
            let RepairDecision::Report(i) =
                classify(&observation(Healthy, Disabled, Healthy, debt, publication)).unwrap()
            else {
                panic!("disabled never applies")
            };
            assert!(i.as_str().contains("disabled task preserved"));
            assert!(i.as_str().contains("retained"));
            if d == Unknown {
                assert!(i.as_str().contains("unknown"));
            }
        }
    }
    let mut f = Fake::new(true, &[]);
    f.disabled = true;
    assert_eq!(f.run().unwrap(), RepairOutcome::Retained);
    assert_eq!(f.registrations, 0);
    assert_eq!(f.writes, 0);
    f.durable.advance(RepairCursor::TaskIntent).unwrap();
    assert_eq!(f.run().unwrap(), RepairOutcome::Retained);
    assert_eq!(f.registrations, 0);
}
#[test]
fn healthy_payload_task_agent_is_noop_no_restart() {
    use RepairDiagnostic::*;
    let o = observation(
        Healthy,
        Healthy,
        Healthy,
        vec![
            JournalObservation::Absent(SourceKind::OuterUpgrade),
            JournalObservation::Absent(SourceKind::FileRecovery),
            JournalObservation::Absent(SourceKind::Removal),
        ],
        Healthy,
    );
    assert_eq!(classify(&o).unwrap(), RepairDecision::Healthy);
    let source = source(SourceKind::Removal, 51);
    let mut f = Fake::new(false, std::slice::from_ref(&source));
    assert_eq!(f.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(f.registrations, 0);
    assert_eq!(f.moves, 1);
    // The actual port structurally has no Stop/Run/start/readers method.
    assert!(f.sources[0].is_none());
}
#[test]
fn stale_receipt_catalog_and_image_observations_never_select_executable() {
    use RepairDiagnostic::*;
    for payload in [Missing, Mismatch] {
        let RepairDecision::Report(i) =
            classify(&observation(payload, Missing, Healthy, vec![], Healthy)).unwrap()
        else {
            panic!("damage is not repaired")
        };
        assert_eq!(i.as_str(), "payload damaged: repair needs a6b / reinstall");
    }
    assert!(serde_json::from_str::<SourceKind>("\"last_exit\"").is_err());
    assert!(serde_json::from_str::<SourceKind>("\"stage_catalog\"").is_err());
    assert!(serde_json::from_str::<SourceKind>("\"image_pin\"").is_err());
    assert_eq!(SourceKind::OuterUpgrade.leaf(), "outer-upgrade.json");
    assert!(SourceSnapshot::new(SourceKind::Removal, [0; 16], stamp(1), [2; 32], 4).is_err());
    assert!(
        RepairObservation::new(
            Healthy,
            task(Healthy),
            Healthy,
            vec![
                JournalObservation::Absent(SourceKind::Removal),
                JournalObservation::Absent(SourceKind::Removal)
            ],
            Healthy
        )
        .is_err()
    );
    let mut f = Fake::new(true, &[]);
    f.installed = true;
    assert_eq!(f.run().unwrap(), RepairOutcome::Repaired);
    assert_eq!(f.registrations, 0);
    assert_eq!(f.moves, 0);
}
#[test]
fn every_repair_intent_effect_result_interruption_reopens_safely() {
    let sources = vec![
        source(SourceKind::OuterUpgrade, 61),
        source(SourceKind::Removal, 62),
    ];
    let mut baseline = Fake::new(true, &sources);
    assert_eq!(baseline.run().unwrap(), RepairOutcome::Repaired);
    let events = baseline.events.clone();
    assert!(events.contains(&"register-effect"));
    assert!(events.contains(&"archive-effect"));
    assert!(events.contains(&"finish-index"));
    for n in 0..events.len() {
        for fault in [Fault::Before(n), Fault::After(n)] {
            let mut f = Fake::new(true, &sources);
            f.fault = Some(fault);
            let _ = f.run();
            f.physical_preserved();
            let r = RepairRecord::decode(&f.durable.encode().unwrap()).unwrap();
            f.durable = r;
            f.fault = None;
            let result = f.run().unwrap();
            assert!(matches!(
                result,
                RepairOutcome::Repaired | RepairOutcome::Retained
            ));
            assert!(f.registrations <= 1);
            assert!(f.moves <= 2);
            f.physical_preserved();
            if result == RepairOutcome::Repaired {
                assert!(f.installed);
                assert!(f.sources.iter().all(Option::is_none));
            }
            let prior = (f.registrations, f.moves);
            let _ = f.run().unwrap();
            assert_eq!((f.registrations, f.moves), prior);
        }
    }
    for target in [
        RepairPublicationTarget::Repair,
        RepairPublicationTarget::EvidenceIndex,
    ] {
        let bytes = match target {
            RepairPublicationTarget::Repair => baseline.durable.encode().unwrap(),
            RepairPublicationTarget::EvidenceIndex => {
                let mut idx = EvidenceIndex::default();
                let slot = idx
                    .reserve(
                        baseline.durable.operation(),
                        baseline.durable.plan().archives(),
                    )
                    .unwrap();
                idx.complete(slot, baseline.durable.operation()).unwrap();
                idx.encode().unwrap()
            }
        };
        let old = RepairPublicationStamp::new(id(71), b"original target").unwrap();
        let pending = RepairPublicationStamp::new(id(72), &bytes).unwrap();
        let mut intent = RepairPublicationIntent::new([91; 16], target, Some(old), &bytes).unwrap();
        assert_eq!(
            recover_repair_publication(&intent, Some(old), None),
            RepairPublicationObservation::Preparing
        );
        // Pending effect before its stamp result becomes durable cannot be inferred safe.
        assert_eq!(
            recover_repair_publication(&intent, Some(old), Some(pending)),
            RepairPublicationObservation::Unknown
        );
        intent.pending_ready(pending).unwrap();
        let reopened = RepairPublicationIntent::decode(&intent.encode().unwrap()).unwrap();
        assert_eq!(
            recover_repair_publication(&reopened, Some(old), Some(pending)),
            RepairPublicationObservation::PendingReady
        );
        intent.replace_intent().unwrap();
        assert_eq!(
            recover_repair_publication(&intent, Some(old), Some(pending)),
            RepairPublicationObservation::PendingReady
        );
        assert_eq!(
            recover_repair_publication(&intent, Some(pending), None),
            RepairPublicationObservation::Published
        );
        assert_eq!(
            recover_repair_publication(&intent, None, None),
            RepairPublicationObservation::Unknown
        );
        intent.published().unwrap();
        assert_eq!(
            recover_repair_publication(
                &RepairPublicationIntent::decode(&intent.encode().unwrap()).unwrap(),
                Some(pending),
                None
            ),
            RepairPublicationObservation::Published
        );
    }
}
