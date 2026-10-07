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

// New payload-repair fixtures drive the real separate controller. No native capability is minted.
mod a6b_payload {
    use super::*;
    use crate::payload::inventory::{ApprovedInventory, ApprovedPe, PayloadRole, PeFacts};
    use crate::payload::recovery::{ImageObservation, OriginalLeaf};
    use crate::repair::payload::{
        PayloadRepairDecision, PayloadRepairOutcome, PayloadRepairPort, PayloadRepairState,
        classify as classify_payload, drive,
    };
    use crate::repair::payload_record::*;
    use crate::service::supervisor::{Generation, InitialEpochPort, initialize_epoch};
    use std::io::{Cursor, Read};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };

    fn index(role: PayloadRole) -> usize {
        role as usize
    }
    fn bytes(role: PayloadRole) -> Vec<u8> {
        let mut b = vec![role as u8; 512];
        b[..2].copy_from_slice(b"MZ");
        b[60..64].copy_from_slice(&64u32.to_le_bytes());
        b[64..68].copy_from_slice(b"PE\0\0");
        b[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        b[84..86].copy_from_slice(&240u16.to_le_bytes());
        b[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        b[156..158].copy_from_slice(&3u16.to_le_bytes());
        b
    }
    fn facts(role: PayloadRole) -> PeFacts {
        PeFacts {
            size: 512,
            sha256: sha256(&bytes(role)),
            machine: 0x8664,
            subsystem: 3,
            version: "a6b-fixture-1".into(),
        }
    }
    fn inventory() -> ApprovedInventory {
        ApprovedInventory::fixture(
            PayloadRole::ALL
                .into_iter()
                .map(|role| ApprovedPe::fixture(role, facts(role)))
                .collect(),
        )
        .unwrap()
    }
    fn old() -> Generation {
        Generation {
            pid: 100,
            creation: 200,
            instance: 0xfedc_ba98_7654_3210,
        }
    }
    fn new_ready() -> Generation {
        Generation {
            pid: 501,
            creation: 901,
            instance: 0x1234_5678_9abc_def0,
        }
    }
    fn selected() -> PayloadRepairRecord {
        PayloadRepairRecord::new(
            [77; 16],
            PayloadRepairSelection::new(
                context(),
                PayloadRepairProcess::new(101, 201).unwrap(),
                ImageObservation {
                    identity: stamp(11),
                    facts: facts(PayloadRole::Installer),
                },
                old(),
                1_000,
                PayloadRepairTask::new(RepairDiagnostic::Healthy, XML.into()).unwrap(),
                PayloadRole::ALL.map(facts),
                PayloadRole::ALL.map(|role| {
                    if role == PayloadRole::Ui {
                        OriginalLeaf::Missing
                    } else {
                        OriginalLeaf::Present(stamp(20 + role as u8))
                    }
                }),
            )
            .unwrap(),
            Some(0),
        )
        .unwrap()
    }
    fn predecessor(phase: service::journal::Phase) -> service::journal::Journal {
        service::journal::Journal {
            schema_version: 1,
            registration: [1; 16],
            operation: [2; 16],
            user: "fixture-same-user".into(),
            phase,
            current: Some(old()),
            stop_instance: (phase == service::journal::Phase::StopIntent).then_some(old().instance),
            original_xml: Some(XML.into()),
            restart_times: vec![],
            last_tick_ms: 5,
            clock_epoch: 200,
        }
    }
    struct FixtureClock(AtomicU64);
    impl native_io::Clock for FixtureClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }
    struct LateRead {
        inner: Cursor<Vec<u8>>,
        clock: Arc<FixtureClock>,
    }
    impl Read for LateRead {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(out)?;
            self.clock.0.store(10_000, Ordering::SeqCst);
            Ok(n)
        }
    }
    #[derive(Clone)]
    struct File {
        id: FileStamp,
        bytes: Vec<u8>,
    }
    struct Sources {
        bundle: crate::payload::ApprovedOuterSources,
        installer: Vec<u8>,
    }
    struct Owner {
        operation: [u8; 16],
    }
    struct Completion {
        operation: [u8; 16],
        generation: Generation,
    }
    struct Verified {
        files: [File; 4],
    }
    struct Started {
        operation: [u8; 16],
    }
    struct HeldLock(Arc<AtomicBool>);
    impl Drop for HeldLock {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
    }
    #[derive(Clone, Copy)]
    enum Fault {
        Before(usize),
        After(usize),
    }
    #[derive(Clone, Copy)]
    enum SourceFailure {
        Missing,
        Changed,
        Overflow,
        Late,
        WrongMetadata,
    }
    struct Fake {
        durable: PayloadRepairRecord,
        state: Option<PayloadRepairState<Self>>,
        pending_sources: Option<Arc<Sources>>,
        pending_owner: Option<Arc<Owner>>,
        pending_completion: Option<Arc<Completion>>,
        pending_verified: Option<Arc<Verified>>,
        pending_started: Option<Arc<Started>>,
        fixed: [Option<File>; 4],
        staged: [Option<File>; 4],
        backups: [Option<File>; 4],
        copied: Option<ImageObservation>,
        resumed: bool,
        committed: bool,
        task: RepairDiagnostic,
        task_changed: bool,
        original_admitted: bool,
        supervisor_exit: bool,
        agent_exit: bool,
        job_members: u32,
        clean_receipt: bool,
        worker_settled: bool,
        endpoint_aliases: u32,
        image_aliases: u32,
        parent_exit: bool,
        completion_override: bool,
        source_failure: Option<SourceFailure>,
        events: Vec<String>,
        fault: Option<Fault>,
        source_reads: usize,
        creates: usize,
        resumes: usize,
        commits: usize,
        stops: usize,
        runs: usize,
        stage_count: [usize; 4],
        backup_count: [usize; 4],
        publish_count: [usize; 4],
        final_deletes: usize,
        keeper_exited: bool,
        keeper_id_changed: bool,
        epoch_creates: usize,
        epoch_prepared: bool,
        epoch_ready: bool,
        epoch_running: bool,
        lock: crate::payload::LockHandoff<'static, HeldLock>,
        held: Arc<AtomicBool>,
        start_permit: Option<()>,
    }
    impl Fake {
        fn new() -> Self {
            let durable = selected();
            let held = Arc::new(AtomicBool::new(true));
            let fixed = PayloadRole::ALL.map(|role| match durable.selection().fixed(role) {
                OriginalLeaf::Present(id) => Some(File {
                    id,
                    bytes: vec![80 + role as u8; 512],
                }),
                _ => None,
            });
            Self {
                durable,
                state: Some(PayloadRepairState::default()),
                pending_sources: None,
                pending_owner: None,
                pending_completion: None,
                pending_verified: None,
                pending_started: None,
                fixed,
                staged: std::array::from_fn(|_| None),
                backups: std::array::from_fn(|_| None),
                copied: None,
                resumed: false,
                committed: false,
                task: RepairDiagnostic::Healthy,
                task_changed: false,
                original_admitted: true,
                supervisor_exit: false,
                agent_exit: false,
                job_members: 1,
                clean_receipt: false,
                worker_settled: false,
                endpoint_aliases: 1,
                image_aliases: 1,
                parent_exit: false,
                completion_override: false,
                source_failure: None,
                events: Vec::new(),
                fault: None,
                source_reads: 0,
                creates: 0,
                resumes: 0,
                commits: 0,
                stops: 0,
                runs: 0,
                stage_count: [0; 4],
                backup_count: [0; 4],
                publish_count: [0; 4],
                final_deletes: 0,
                keeper_exited: false,
                keeper_id_changed: false,
                epoch_creates: 0,
                epoch_prepared: false,
                epoch_ready: false,
                epoch_running: false,
                lock: crate::payload::LockHandoff::Owned(Some(HeldLock(held.clone()))),
                held,
                start_permit: Some(()),
            }
        }
        fn before(&mut self, label: impl Into<String>) -> NativeResult<usize> {
            let n = self.events.len();
            self.events.push(label.into());
            if matches!(self.fault, Some(Fault::Before(at)) if at == n) {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(n)
        }
        fn after(&self, n: usize) -> NativeResult<()> {
            if matches!(self.fault, Some(Fault::After(at)) if at == n) {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        fn observe(&mut self, label: impl Into<String>) -> NativeResult<()> {
            let n = self.before(label)?;
            self.after(n)
        }
        fn run(&mut self) -> NativeResult<PayloadRepairOutcome> {
            let mut record = self.durable.clone();
            drive(self, &mut record)
        }
        fn proof(&self, record: &PayloadRepairRecord, tree: &Completion) -> NativeResult<()> {
            if !self.original_admitted
                || tree.operation != record.operation()
                || tree.generation != old()
                || !self.supervisor_exit
                || !self.agent_exit
                || self.job_members != 0
                || !self.clean_receipt
                || !self.worker_settled
                || self.endpoint_aliases != 0
                || self.image_aliases != 0
                || !self.parent_exit
            {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        fn physical_preserved(&self) {
            for role in PayloadRole::ALL {
                let i = index(role);
                if let OriginalLeaf::Present(id) = self.durable.selection().fixed(role) {
                    let occurrences = [&self.fixed[i], &self.backups[i]]
                        .into_iter()
                        .flatten()
                        .filter(|f| f.id == id)
                        .count();
                    assert_eq!(occurrences, 1, "old opaque role never lost");
                }
                assert!(
                    self.stage_count[i] <= 1
                        && self.backup_count[i] <= 1
                        && self.publish_count[i] <= 1
                );
            }
            assert!(
                self.creates <= 1
                    && self.resumes <= 1
                    && self.commits <= 1
                    && self.stops <= 1
                    && self.runs <= 1
            );
            assert!(self.epoch_creates <= 1);
            if self.copied.is_some() && !self.keeper_exited {
                assert_eq!(self.final_deletes, 0);
            }
        }
        fn cold(&mut self) {
            self.state = Some(PayloadRepairState::default());
            self.pending_sources = None;
            self.pending_owner = None;
            self.pending_completion = None;
            self.pending_verified = None;
            self.pending_started = None;
        }
    }
    impl PayloadRepairPort for Fake {
        type Sources = Arc<Sources>;
        type Owner = Arc<Owner>;
        type Completion = Arc<Completion>;
        type Verified = Arc<Verified>;
        type Started = Arc<Started>;
        fn take_state(&mut self) -> NativeResult<PayloadRepairState<Self>> {
            self.state.take().ok_or(NativeError::Busy)
        }
        fn retain_state(&mut self, state: PayloadRepairState<Self>) {
            assert!(self.state.is_none());
            self.state = Some(state);
        }
        fn renew(&mut self, record: &PayloadRepairRecord) -> NativeResult<()> {
            self.observe("renew-original-selection")?;
            if !self.held.load(Ordering::SeqCst) {
                return Err(NativeError::OutcomeUnknown);
            }
            if record != &self.durable || !self.original_admitted {
                return Err(NativeError::Foreign);
            }
            record.validate()
        }
        fn persist(&mut self, record: &PayloadRepairRecord) -> NativeResult<()> {
            self.durable.publication_successor(record)?;
            let n = self.before(format!("persist-{:?}", record.phase()))?;
            self.durable = PayloadRepairRecord::decode(&record.encode()?)?;
            self.after(n)
        }
        fn task_observe(&mut self, _: &PayloadRepairRecord) -> NativeResult<RepairTaskObservation> {
            self.observe("task-reobserve")?;
            RepairTaskObservation::new(
                self.task,
                Some(if self.task_changed {
                    "<Task foreign/>".into()
                } else {
                    XML.into()
                }),
            )
        }
        fn verify_sources(&mut self, record: &PayloadRepairRecord) -> NativeResult<Self::Sources> {
            let n = self.before("independent-source-read")?;
            self.source_reads += 1;
            let inventory = inventory();
            let installer =
                ApprovedPe::fixture(PayloadRole::Installer, facts(PayloadRole::Installer));
            let clock = Arc::new(FixtureClock(AtomicU64::new(0)));
            let deadline =
                native_io::Deadline::new(5_000, clock.clone(), native_io::Cancellation::default())?;
            let mut inputs: Vec<crate::payload::PayloadInput> =
                [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                    .into_iter()
                    .map(|role| crate::payload::PayloadInput {
                        role,
                        content: Box::new(Cursor::new(bytes(role))),
                    })
                    .collect();
            match self.source_failure {
                Some(SourceFailure::Missing) => {
                    inputs.pop();
                }
                Some(SourceFailure::Changed) => {
                    let mut b = bytes(PayloadRole::Agent);
                    b[511] ^= 1;
                    inputs[0].content = Box::new(Cursor::new(b));
                }
                Some(SourceFailure::Overflow) => {
                    let mut b = bytes(PayloadRole::Agent);
                    b.push(1);
                    inputs[0].content = Box::new(Cursor::new(b));
                }
                Some(SourceFailure::Late) => {
                    inputs[0].content = Box::new(LateRead {
                        inner: Cursor::new(bytes(PayloadRole::Agent)),
                        clock,
                    });
                }
                Some(SourceFailure::WrongMetadata) => return Err(NativeError::Foreign),
                _ => {}
            }
            let bundle =
                crate::payload::buffer_outer_sources(inputs, &inventory, &installer, &deadline)?;
            // Receiver validates independently too; record hashes never approve incoming content.
            let received = crate::payload::ApprovedOuterSources::receive(
                [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
                    .map(|r| bundle.bytes(r).unwrap().to_vec()),
                &inventory,
                &installer,
                &deadline,
            )?;
            if record.sources() != &PayloadRole::ALL.map(facts) {
                return Err(NativeError::Foreign);
            }
            let value = Arc::new(Sources {
                bundle: received,
                installer: bytes(PayloadRole::Installer),
            });
            self.pending_sources = Some(value.clone());
            self.after(n)?;
            Ok(value)
        }
        fn recover_sources(
            &mut self,
            _: &PayloadRepairRecord,
        ) -> NativeResult<Option<Self::Sources>> {
            self.observe("same-owner-sources-observe")?;
            Ok(self.pending_sources.clone())
        }
        fn keeper_copy(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Sources,
        ) -> NativeResult<ImageObservation> {
            assert_eq!(record.phase(), PayloadRepairPhase::CopyIntent);
            let n = self.before("copy-effect")?;
            let image = ImageObservation {
                identity: stamp(41),
                facts: facts(PayloadRole::Installer),
            };
            self.copied = Some(image.clone());
            self.after(n)?;
            Ok(image)
        }
        fn recover_copy(
            &mut self,
            _: &PayloadRepairRecord,
        ) -> NativeResult<Option<ImageObservation>> {
            self.observe("copy-same-object-observe")?;
            Ok(self.copied.clone())
        }
        fn keeper_prepare(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Sources,
        ) -> NativeResult<Self::Owner> {
            assert_eq!(record.phase(), PayloadRepairPhase::HandoffIntent);
            let n = self.before("suspended-create-effect")?;
            self.creates += 1;
            let owner = Arc::new(Owner {
                operation: record.operation(),
            });
            self.pending_owner = Some(owner.clone());
            self.after(n)?;
            Ok(owner)
        }
        fn recover_owner(
            &mut self,
            record: &PayloadRepairRecord,
        ) -> NativeResult<Option<Self::Owner>> {
            self.observe("same-retained-owner-observe")?;
            Ok(self
                .pending_owner
                .clone()
                .filter(|o| o.operation == record.operation()))
        }
        fn keeper_facts(
            &self,
            owner: &Self::Owner,
        ) -> NativeResult<(PayloadRepairProcess, PayloadRepairProcess, u64)> {
            assert_eq!(owner.operation, self.durable.operation());
            Ok((
                self.durable.selection().source_process(),
                PayloadRepairProcess::new(301, 401)?,
                64,
            ))
        }
        fn keeper_resume(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Owner,
        ) -> NativeResult<()> {
            assert_eq!(record.phase(), PayloadRepairPhase::ResumeIntent);
            let n = self.before("resume-effect")?;
            self.resumes += 1;
            self.resumed = true;
            self.after(n)
        }
        fn keeper_ready(&mut self, _: &PayloadRepairRecord, _: &Self::Owner) -> NativeResult<bool> {
            self.observe("keeper-ready-observe")?;
            Ok(self.resumed)
        }
        fn keeper_commit(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Owner,
        ) -> NativeResult<()> {
            assert_eq!(record.phase(), PayloadRepairPhase::CommitIntent);
            let n = self.before("atomic-commit-effect")?;
            self.commits += 1;
            self.committed = true;
            self.after(n)
        }
        fn commit_authorized(
            &mut self,
            _: &PayloadRepairRecord,
            _: &Self::Owner,
        ) -> NativeResult<bool> {
            self.observe("same-live-commit-observe")?;
            Ok(self.committed)
        }
        fn stop_once(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Owner,
        ) -> NativeResult<Self::Completion> {
            assert_eq!(record.phase(), PayloadRepairPhase::StopIntent);
            assert!(self.committed);
            self.observe("durable-original-supervisor-stop-intent")?;
            self.observe("arm-original-terminal-latch")?;
            let n = self.before("one-stop-effect")?;
            self.stops += 1;
            let tree = Arc::new(Completion {
                operation: record.operation(),
                generation: old(),
            });
            self.pending_completion = Some(tree.clone());
            if !self.completion_override {
                self.supervisor_exit = true;
                self.agent_exit = true;
                self.job_members = 0;
                self.clean_receipt = true;
                self.worker_settled = true;
                self.endpoint_aliases = 0;
                self.image_aliases = 0;
                self.parent_exit = true;
            }
            self.after(n)?;
            Ok(tree)
        }
        fn recover_completion(
            &mut self,
            _: &PayloadRepairRecord,
            _: &Self::Owner,
        ) -> NativeResult<Option<Self::Completion>> {
            self.observe("same-original-completion-observe")?;
            Ok(self.pending_completion.clone())
        }
        fn settled(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Owner,
            tree: &Self::Completion,
        ) -> NativeResult<()> {
            self.observe("exact-tree-worker-image-parent-settlement")?;
            self.proof(record, tree)
        }
        fn stage(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            sources: &Self::Sources,
            tree: &Self::Completion,
        ) -> NativeResult<ImageObservation> {
            assert_eq!(record.phase(), PayloadRepairPhase::StageIntent { role });
            self.proof(record, tree)?;
            let n = self.before(format!("stage-{role:?}-effect"))?;
            let i = index(role);
            assert!(self.staged[i].is_none());
            self.stage_count[i] += 1;
            let content = if role == PayloadRole::Installer {
                sources.installer.clone()
            } else {
                sources.bundle.bytes(role)?.to_vec()
            };
            self.staged[i] = Some(File {
                id: stamp(51 + role as u8),
                bytes: content,
            });
            let image = ImageObservation {
                identity: self.staged[i].as_ref().unwrap().id,
                facts: facts(role),
            };
            self.after(n)?;
            Ok(image)
        }
        fn recover_stage(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<Option<ImageObservation>> {
            self.observe("stage-same-object-observe")?;
            self.proof(record, tree)?;
            Ok(self.staged[index(role)].as_ref().map(|f| ImageObservation {
                identity: f.id,
                facts: facts(role),
            }))
        }
        fn observe_original(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<OriginalLeaf> {
            self.observe("original-fixed-object-observe")?;
            self.proof(record, tree)?;
            Ok(self.fixed[index(role)]
                .as_ref()
                .map_or(OriginalLeaf::Missing, |f| OriginalLeaf::Present(f.id)))
        }
        fn backup(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<Option<FileStamp>> {
            assert_eq!(record.phase(), PayloadRepairPhase::BackupIntent { role });
            self.proof(record, tree)?;
            let n = self.before(format!("backup-{role:?}-effect"))?;
            let i = index(role);
            assert!(self.backups[i].is_none());
            self.backup_count[i] += 1;
            self.backups[i] = self.fixed[i].take();
            let id = self.backups[i].as_ref().map(|f| f.id);
            self.after(n)?;
            Ok(id)
        }
        fn recover_backup(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<Option<Option<FileStamp>>> {
            self.observe("backup-same-object-observe")?;
            self.proof(record, tree)?;
            if self.backup_count[index(role)] == 0 {
                return Ok(None);
            }
            Ok(Some(self.backups[index(role)].as_ref().map(|f| f.id)))
        }
        fn publish(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<ImageObservation> {
            assert_eq!(record.phase(), PayloadRepairPhase::PublishIntent { role });
            self.proof(record, tree)?;
            let n = self.before(format!("publish-{role:?}-effect"))?;
            let i = index(role);
            assert!(self.fixed[i].is_none());
            self.publish_count[i] += 1;
            self.fixed[i] = self.staged[i].take();
            let id = self.fixed[i].as_ref().unwrap().id;
            self.after(n)?;
            Ok(ImageObservation {
                identity: id,
                facts: facts(role),
            })
        }
        fn recover_publish(
            &mut self,
            record: &PayloadRepairRecord,
            role: PayloadRole,
            tree: &Self::Completion,
        ) -> NativeResult<Option<ImageObservation>> {
            self.observe("published-same-object-observe")?;
            self.proof(record, tree)?;
            if self.publish_count[index(role)] == 0 {
                return Ok(None);
            }
            Ok(self.fixed[index(role)].as_ref().map(|f| ImageObservation {
                identity: f.id,
                facts: facts(role),
            }))
        }
        fn verify_fixed(
            &mut self,
            record: &PayloadRepairRecord,
            tree: &Self::Completion,
        ) -> NativeResult<Self::Verified> {
            self.proof(record, tree)?;
            let n = self.before("fresh-four-fixed-image-approval")?;
            let fixed: [File; 4] = self
                .fixed
                .iter()
                .cloned()
                .collect::<Option<Vec<_>>>()
                .ok_or(NativeError::Missing)?
                .try_into()
                .map_err(|_| NativeError::Invalid)?;
            for role in PayloadRole::ALL {
                let expected = ApprovedPe::fixture(role, facts(role));
                let deadline = native_io::Deadline::new(
                    5_000,
                    Arc::new(native_io::MonotonicClock::default()),
                    native_io::Cancellation::default(),
                )?;
                crate::payload::verify_outer_source(
                    &fixed[index(role)].bytes,
                    &expected,
                    &deadline,
                )?;
                if record.role(role).published().map(|i| i.identity) != Some(fixed[index(role)].id)
                {
                    return Err(NativeError::Foreign);
                }
            }
            let value = Arc::new(Verified { files: fixed });
            self.pending_verified = Some(value.clone());
            self.after(n)?;
            Ok(value)
        }
        fn recover_verified(
            &mut self,
            _: &PayloadRepairRecord,
            _: &Self::Completion,
        ) -> NativeResult<Option<Self::Verified>> {
            self.observe("fresh-verified-same-files-observe")?;
            Ok(self.pending_verified.clone())
        }
        fn start_once(
            &mut self,
            record: &PayloadRepairRecord,
            verified: &Self::Verified,
            _: &Self::Completion,
        ) -> NativeResult<Self::Started> {
            assert_eq!(record.phase(), PayloadRepairPhase::StartIntent);
            let mut lock =
                std::mem::replace(&mut self.lock, crate::payload::LockHandoff::Owned(None));
            let mut permit = self.start_permit.take();
            let held = self.held.clone();
            let reenter_held = held.clone();
            let result = lock.run_once(
                &mut permit,
                true,
                || {
                    assert!(
                        !held.load(Ordering::SeqCst),
                        "actual lock dropped before Run"
                    );
                    if self.task == RepairDiagnostic::Disabled || self.task_changed {
                        return Err(NativeError::Foreign);
                    }
                    if self.task == RepairDiagnostic::Missing {
                        let n = self.before("task-register-effect")?;
                        self.task = RepairDiagnostic::Healthy;
                        self.after(n)?;
                    }
                    self.observe("fresh-repair-task-selection-and-claim")?;
                    let n = self.before("one-task-run-effect")?;
                    self.runs += 1;
                    let started = Arc::new(Started {
                        operation: record.operation(),
                    });
                    self.pending_started = Some(started.clone());
                    self.after(n)?;
                    let mut epoch = Epoch {
                        fake: self,
                        record,
                        verified,
                    };
                    let generation = initialize_epoch(&mut epoch)?;
                    assert_eq!(generation, new_ready());
                    Ok(started)
                },
                || {
                    reenter_held.store(true, Ordering::SeqCst);
                    Ok((HeldLock(reenter_held), ()))
                },
            );
            self.lock = lock;
            self.start_permit = permit;
            result
        }
        fn recover_started(
            &mut self,
            record: &PayloadRepairRecord,
            _: &Self::Verified,
        ) -> NativeResult<Option<Self::Started>> {
            self.observe("same-task-submission-observe")?;
            Ok(self
                .pending_started
                .clone()
                .filter(|s| s.operation == record.operation()))
        }
        fn submission(&self, started: &Self::Started) -> NativeResult<String> {
            assert_eq!(started.operation, self.durable.operation());
            Ok("{01234567-89ab-cdef-0123-456789abcdef}".into())
        }
        fn health(
            &mut self,
            _: &PayloadRepairRecord,
            verified: &Self::Verified,
            _: &Self::Started,
        ) -> NativeResult<Option<Generation>> {
            self.observe("fresh-same-ready-health-observe")?;
            for role in PayloadRole::ALL {
                if self.fixed[index(role)].as_ref().map(|f| (&f.id, &f.bytes))
                    != Some((
                        &verified.files[index(role)].id,
                        &verified.files[index(role)].bytes,
                    ))
                {
                    return Err(NativeError::Foreign);
                }
            }
            Ok((self.epoch_ready && self.epoch_running).then_some(new_ready()))
        }
        fn retire_settled(&mut self, record: &PayloadRepairRecord) -> NativeResult<bool> {
            assert_eq!(record.phase(), PayloadRepairPhase::Complete);
            self.observe("terminal-copy-exact-exit-and-absence-observe")?;
            if !self.keeper_exited || self.keeper_id_changed {
                return Ok(false);
            }
            if self.copied.is_none() && self.final_deletes == 1 {
                // This SAME fixture owner retains the exact deletion/absence observation;
                // a cold cursor alone is never used as an exit or absence capability.
                return Ok(true);
            }
            let n = self.before("final-copy-cleanup-effect")?;
            assert_eq!(self.copied.as_ref(), record.keeper().image());
            self.copied = None;
            self.final_deletes += 1;
            self.after(n)?;
            Ok(true)
        }
    }
    struct Epoch<'a> {
        fake: &'a mut Fake,
        record: &'a PayloadRepairRecord,
        verified: &'a Verified,
    }
    impl InitialEpochPort for Epoch<'_> {
        type Child = Generation;
        type Ready = Generation;
        fn prepare_epoch(&mut self) -> NativeResult<()> {
            let prior = predecessor(service::journal::Phase::Finished);
            let lineage = crate::repair::payload_record::correlate_repair(
                self.record.operation(),
                "fixture-same-user",
                self.record,
                &prior,
            )?;
            assert!(lineage.matches_predecessor(&prior));
            assert_eq!(lineage.operation(), self.record.operation());
            for role in PayloadRole::ALL {
                let file = self.fake.fixed[index(role)]
                    .as_ref()
                    .ok_or(NativeError::Missing)?;
                if file.id != self.verified.files[index(role)].id || file.bytes != bytes(role) {
                    return Err(NativeError::Foreign);
                }
            }
            self.fake.observe("repair-epoch-archive-intent")?;
            let n = self.fake.before("repair-epoch-archive-effect")?;
            self.fake.epoch_prepared = true;
            self.fake.after(n)?;
            self.fake.observe("repair-epoch-archive-result")
        }
        fn create(&mut self) -> NativeResult<Self::Child> {
            assert!(self.fake.epoch_prepared);
            let n = self.fake.before("fresh-owned-create-assign-resume")?;
            self.fake.epoch_creates += 1;
            self.fake.after(n)?;
            Ok(new_ready())
        }
        fn await_ready(&mut self, child: &Self::Child) -> NativeResult<Self::Ready> {
            assert_eq!(*child, new_ready());
            let n = self.fake.before("actual-new-ready")?;
            self.fake.epoch_ready = true;
            self.fake.after(n)?;
            Ok(*child)
        }
        fn publish_running(&mut self, ready: &Self::Ready) -> NativeResult<()> {
            assert!(self.fake.epoch_ready);
            assert_eq!(*ready, new_ready());
            let n = self.fake.before("running-publication")?;
            self.fake.epoch_running = true;
            self.fake.after(n)
        }
    }

    #[test]
    fn payload_repair_stop_start_order_and_fresh_proofs() {
        let mut fake = Fake::new();
        assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Complete);
        let event = |name: &str| fake.events.iter().position(|e| e == name).unwrap();
        assert!(event("independent-source-read") < event("one-stop-effect"));
        assert!(event("persist-StopIntent") < event("durable-original-supervisor-stop-intent"));
        assert!(
            event("durable-original-supervisor-stop-intent") < event("arm-original-terminal-latch")
        );
        assert!(event("arm-original-terminal-latch") < event("one-stop-effect"));
        assert!(
            event("exact-tree-worker-image-parent-settlement") < event("backup-Installer-effect")
        );
        assert!(
            event("fresh-four-fixed-image-approval")
                < event("fresh-repair-task-selection-and-claim")
        );
        assert!(event("repair-epoch-archive-result") < event("fresh-owned-create-assign-resume"));
        assert!(event("actual-new-ready") < event("running-publication"));
        assert_eq!(
            (
                fake.stops,
                fake.runs,
                fake.creates,
                fake.resumes,
                fake.commits
            ),
            (1, 1, 1, 1, 1)
        );
        fake.physical_preserved();
        assert!(fake.copied.is_some());
        assert_eq!(fake.final_deletes, 0);
        let before = (fake.stops, fake.runs);
        assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Complete);
        assert_eq!((fake.stops, fake.runs), before);
        let mut start = fake.durable.clone();
        // Use the actual strict predecessor matcher at the signed StartIntent boundary.
        let mut value = serde_json::to_value(&start).unwrap();
        value["phase"] = serde_json::json!({"state":"start-intent"});
        value["submission"] = serde_json::Value::Null;
        value["ready"] = serde_json::Value::Null;
        start = serde_json::from_value(value).unwrap();
        start.validate().unwrap();
        for invalid_guid in [
            "fixture-submission-1",
            "01234567-89ab-cdef-0123-456789abcdef",
            "{01234567-89ab-cdef-0123-456789abcdeg}",
            "{0123456789ab-cdef-0123-456789abcdef}",
        ] {
            assert!(start.clone().bind_submission(invalid_guid.into()).is_err());
        }
        assert!(
            start
                .clone()
                .bind_submission("{01234567-89AB-CDEF-0123-456789ABCDEF}".into())
                .is_ok()
        );
        let valid = predecessor(service::journal::Phase::Finished);
        assert!(
            crate::repair::payload_record::correlate_repair(
                start.operation(),
                "fixture-same-user",
                &start,
                &valid
            )
            .is_ok()
        );
        for phase in [
            service::journal::Phase::Running,
            service::journal::Phase::StopIntent,
        ] {
            assert!(
                crate::repair::payload_record::correlate_repair(
                    start.operation(),
                    "fixture-same-user",
                    &start,
                    &predecessor(phase)
                )
                .is_err()
            );
        }
        let mut wrong = valid.clone();
        wrong.current.as_mut().unwrap().instance ^= 1;
        assert!(
            crate::repair::payload_record::correlate_repair(
                start.operation(),
                "fixture-same-user",
                &start,
                &wrong
            )
            .is_err()
        );
        assert!(
            crate::repair::payload_record::correlate_repair(
                start.operation(),
                "foreign-user",
                &start,
                &valid
            )
            .is_err()
        );
        let mut invalid = valid;
        invalid.stop_instance = Some(old().instance);
        assert!(
            crate::repair::payload_record::correlate_repair(
                start.operation(),
                "fixture-same-user",
                &start,
                &invalid
            )
            .is_err()
        );
        for failure in 0..8 {
            let mut blocked = Fake::new();
            blocked.completion_override = true;
            blocked.supervisor_exit = true;
            blocked.agent_exit = true;
            blocked.job_members = 0;
            blocked.clean_receipt = true;
            blocked.worker_settled = true;
            blocked.endpoint_aliases = 0;
            blocked.image_aliases = 0;
            blocked.parent_exit = true;
            match failure {
                0 => blocked.supervisor_exit = false,
                1 => blocked.agent_exit = false,
                2 => blocked.job_members = 1,
                3 => blocked.clean_receipt = false,
                4 => blocked.worker_settled = false,
                5 => blocked.endpoint_aliases = 1,
                6 => blocked.image_aliases = 1,
                _ => blocked.parent_exit = false,
            }
            assert!(blocked.run().is_err());
            assert_eq!(blocked.stage_count, [0; 4]);
            assert_eq!(blocked.backup_count, [0; 4]);
            assert_eq!(blocked.runs, 0);
        }
        fake.keeper_exited = true;
        fake.keeper_id_changed = true;
        assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Complete);
        assert_eq!(fake.final_deletes, 0);
        fake.keeper_id_changed = false;
        assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retired);
        assert_eq!(fake.final_deletes, 1);
        assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retired);
        let mut catalog = PayloadRepairCatalog::default();
        for n in 0..3 {
            let mut initial = selected();
            let mut value = serde_json::to_value(&initial).unwrap();
            value["operation"] = serde_json::to_value([n + 1; 16]).unwrap();
            value["slot"] = serde_json::Value::Null;
            initial = serde_json::from_value(value).unwrap();
            let slot = catalog.reserve(&initial).unwrap();
            let mut value = serde_json::to_value(&fake.durable).unwrap();
            value["operation"] = serde_json::to_value([n + 1; 16]).unwrap();
            value["slot"] = serde_json::to_value(slot).unwrap();
            value["phase"] = serde_json::json!({"state":"complete"});
            let record = serde_json::from_value(value).unwrap();
            catalog.complete(slot, &record).unwrap();
        }
        let saved = catalog.encode().unwrap();
        assert_eq!(catalog.reserve(&selected()), Err(NativeError::Busy));
        assert_eq!(catalog.encode().unwrap(), saved);
        let mut changed = fake.durable.clone();
        let mut value = serde_json::to_value(&changed).unwrap();
        value["keeper"]["image"]["facts"]["sha256"] = serde_json::to_value([1; 32]).unwrap();
        changed = serde_json::from_value(value).unwrap();
        assert!(changed.validate().is_err());
    }
    #[test]
    fn disabled_task_prevents_stop_and_run() {
        for diagnostic in [
            RepairDiagnostic::Disabled,
            RepairDiagnostic::Mismatch,
            RepairDiagnostic::AccessDenied,
            RepairDiagnostic::Unknown,
        ] {
            let mut fake = Fake::new();
            fake.task = diagnostic;
            assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retained);
            assert_eq!(
                (fake.source_reads, fake.creates, fake.stops, fake.runs),
                (0, 0, 0, 0)
            );
            assert_eq!(fake.durable.phase(), PayloadRepairPhase::Selected);
        }
        let mut drift = Fake::new();
        drift.task_changed = true;
        assert_eq!(drift.run().unwrap(), PayloadRepairOutcome::Retained);
        assert_eq!(drift.stops, 0);
        let o = observation(
            RepairDiagnostic::Mismatch,
            RepairDiagnostic::Disabled,
            RepairDiagnostic::Unknown,
            vec![],
            RepairDiagnostic::Healthy,
        );
        assert_eq!(
            classify_payload(&o).unwrap(),
            PayloadRepairDecision::Disabled
        );
        let mut pending = Fake::new();
        let mut baseline = Fake::new();
        baseline.run().unwrap();
        let at = baseline
            .events
            .iter()
            .position(|e| e == "persist-FixedVerified")
            .unwrap();
        pending.fault = Some(Fault::After(at));
        assert!(pending.run().is_err());
        pending.fault = None;
        pending.task = RepairDiagnostic::Disabled;
        assert_eq!(pending.run().unwrap(), PayloadRepairOutcome::Retained);
        assert_eq!(pending.stops, 1);
        assert_eq!(pending.runs, 0);
    }
    #[test]
    fn missing_unapproved_or_late_sources_have_no_stop() {
        for failure in [
            SourceFailure::Missing,
            SourceFailure::Changed,
            SourceFailure::Overflow,
            SourceFailure::Late,
            SourceFailure::WrongMetadata,
        ] {
            let mut fake = Fake::new();
            fake.source_failure = Some(failure);
            assert!(fake.run().is_err());
            assert_eq!((fake.creates, fake.stops, fake.runs), (0, 0, 0));
            fake.source_failure = None;
            assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retained);
            assert_eq!(fake.source_reads, 1);
            assert_eq!(fake.stops, 0);
        }
        let o = observation(
            RepairDiagnostic::Mismatch,
            RepairDiagnostic::Healthy,
            RepairDiagnostic::Unknown,
            vec![],
            RepairDiagnostic::Healthy,
        );
        assert_eq!(
            classify_payload(&o).unwrap(),
            PayloadRepairDecision::Eligible
        );
        let mut missing_original = Fake::new();
        missing_original.original_admitted = false;
        assert!(missing_original.run().is_err());
        assert_eq!(
            (missing_original.source_reads, missing_original.stops),
            (0, 0)
        );
        let healthy_unknown = observation(
            RepairDiagnostic::Healthy,
            RepairDiagnostic::Healthy,
            RepairDiagnostic::Unknown,
            vec![],
            RepairDiagnostic::Healthy,
        );
        assert_eq!(
            classify_payload(&healthy_unknown).unwrap(),
            PayloadRepairDecision::ReinstallRequired
        );
        for diagnostic in [
            RepairDiagnostic::AccessDenied,
            RepairDiagnostic::UnsafeForeign,
            RepairDiagnostic::Unavailable,
        ] {
            let o = observation(
                RepairDiagnostic::Mismatch,
                RepairDiagnostic::Healthy,
                diagnostic,
                vec![],
                RepairDiagnostic::Healthy,
            );
            assert_eq!(
                classify_payload(&o).unwrap(),
                PayloadRepairDecision::ReinstallRequired
            );
        }
        // Own embedded pins and the receiver's independent validator reject a changed source;
        // a matching journal/backup hash never supplies ApprovedPe.
        let inventory = inventory();
        let installer = ApprovedPe::fixture(PayloadRole::Installer, facts(PayloadRole::Installer));
        let deadline = native_io::Deadline::new(
            5_000,
            Arc::new(native_io::MonotonicClock::default()),
            native_io::Cancellation::default(),
        )
        .unwrap();
        let mut incoming = [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl].map(bytes);
        incoming[1][511] ^= 1;
        assert!(
            crate::payload::ApprovedOuterSources::receive(
                incoming, &inventory, &installer, &deadline
            )
            .is_err()
        );
    }
    #[test]
    fn every_intent_effect_result_interruption_retains_owner_and_refuses_replay() {
        let mut baseline = Fake::new();
        assert_eq!(baseline.run().unwrap(), PayloadRepairOutcome::Complete);
        let events = baseline.events.clone();
        for name in [
            "copy-effect",
            "suspended-create-effect",
            "resume-effect",
            "atomic-commit-effect",
            "one-stop-effect",
            "stage-Installer-effect",
            "backup-Ctl-effect",
            "publish-Ctl-effect",
            "fresh-four-fixed-image-approval",
            "one-task-run-effect",
            "repair-epoch-archive-effect",
            "actual-new-ready",
            "running-publication",
        ] {
            assert!(events.iter().any(|e| e == name));
        }
        for at in 0..events.len() {
            for fault in [Fault::Before(at), Fault::After(at)] {
                let mut fake = Fake::new();
                fake.fault = Some(fault);
                let _ = fake.run();
                assert!(
                    fake.state.is_some(),
                    "resident state returned after every Err"
                );
                fake.physical_preserved();
                fake.fault = None;
                fake.durable =
                    PayloadRepairRecord::decode(&fake.durable.encode().unwrap()).unwrap();
                let _ = fake.run();
                fake.physical_preserved();
                let prior = (
                    fake.creates,
                    fake.resumes,
                    fake.commits,
                    fake.stops,
                    fake.runs,
                    fake.stage_count,
                    fake.backup_count,
                    fake.publish_count,
                );
                let _ = fake.run();
                fake.physical_preserved();
                assert_eq!(
                    (
                        fake.creates,
                        fake.resumes,
                        fake.commits,
                        fake.stops,
                        fake.runs,
                        fake.stage_count,
                        fake.backup_count,
                        fake.publish_count
                    ),
                    prior
                );
                if fake.stops != 0 {
                    assert!(fake.pending_owner.is_some());
                    assert!(fake.state.as_ref().unwrap().retains_owner());
                }
                if fake.pending_completion.is_some() {
                    assert!(fake.pending_owner.is_some());
                }
                if fake.durable.phase().rank() >= PayloadRepairPhase::CopyIntent.rank()
                    && !matches!(
                        fake.durable.phase(),
                        PayloadRepairPhase::Complete | PayloadRepairPhase::Retired
                    )
                {
                    fake.cold();
                    let before = (
                        fake.creates,
                        fake.resumes,
                        fake.commits,
                        fake.stops,
                        fake.runs,
                    );
                    assert!(matches!(
                        fake.run(),
                        Ok(PayloadRepairOutcome::Retained) | Err(NativeError::OutcomeUnknown)
                    ));
                    assert_eq!(
                        (
                            fake.creates,
                            fake.resumes,
                            fake.commits,
                            fake.stops,
                            fake.runs
                        ),
                        before
                    );
                }
            }
        }
        // Later final-copy cleanup has its own before/effect/result interruptions. A
        // positive result stays with the exact same settled owner and never repeats DELETE.
        let mut cleanup_baseline = Fake::new();
        cleanup_baseline.run().unwrap();
        cleanup_baseline.events.clear();
        cleanup_baseline.keeper_exited = true;
        assert_eq!(
            cleanup_baseline.run().unwrap(),
            PayloadRepairOutcome::Retired
        );
        assert!(
            cleanup_baseline
                .events
                .iter()
                .any(|e| e == "final-copy-cleanup-effect")
        );
        for at in 0..cleanup_baseline.events.len() {
            for fault in [Fault::Before(at), Fault::After(at)] {
                let mut fake = Fake::new();
                fake.run().unwrap();
                fake.events.clear();
                fake.keeper_exited = true;
                fake.fault = Some(fault);
                let _ = fake.run();
                fake.fault = None;
                assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retired);
                assert_eq!(fake.final_deletes, 1);
                fake.physical_preserved();
                assert_eq!(fake.run().unwrap(), PayloadRepairOutcome::Retired);
                assert_eq!(fake.final_deletes, 1);
            }
        }
        // The production publication classifier is separate; torn/unrecorded pending output
        // never becomes a mutation permission, while exact same-identity destination may settle.
        let bytes = baseline.durable.encode().unwrap();
        let old_stamp = PayloadRepairPublicationStamp::new(stamp(81), b"old fixed record").unwrap();
        let pending = PayloadRepairPublicationStamp::new(stamp(82), &bytes).unwrap();
        let mut intent = PayloadRepairPublicationIntent::new(
            [77; 16],
            PayloadRepairPublicationTarget::Record,
            Some(old_stamp),
            &bytes,
        )
        .unwrap();
        assert_eq!(
            recover_payload_repair_publication(&intent, Some(old_stamp), Some(pending)),
            PayloadRepairPublicationObservation::Unknown
        );
        intent.pending_ready(pending).unwrap();
        intent.replace_intent().unwrap();
        assert_eq!(
            recover_payload_repair_publication(&intent, Some(pending), None),
            PayloadRepairPublicationObservation::Published
        );
        assert_eq!(
            recover_payload_repair_publication(&intent, None, None),
            PayloadRepairPublicationObservation::Unknown
        );
        for n in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(PayloadRepairRecord::decode(&bytes[..n]).is_err());
        }
        let encoded = intent.encode().unwrap();
        for n in [0, 1, encoded.len() / 2, encoded.len() - 1] {
            assert!(PayloadRepairPublicationIntent::decode(&encoded[..n]).is_err());
        }
        // M1: a source may settle only prior to an actual Resume reservation. Durable
        // cancellation precedes release; the freed Active slot is reusable, not a new claim.
        let mut selection = selected();
        let mut history = PayloadRepairCatalog::default();
        let slot = history.reserve(&selection).unwrap();
        assert_eq!(selection.slot(), Some(slot));
        assert!(history.release_cancelled(&selection).is_err());
        for phase in [
            PayloadRepairPhase::Selected,
            PayloadRepairPhase::CopyIntent,
            PayloadRepairPhase::CopyReady,
            PayloadRepairPhase::HandoffIntent,
            PayloadRepairPhase::Created,
            PayloadRepairPhase::ResumeIntent,
        ] {
            assert!(source_can_cancel(phase, false));
            assert!(!source_can_cancel(phase, true));
        }
        for phase in [
            PayloadRepairPhase::Ready,
            PayloadRepairPhase::Committed,
            PayloadRepairPhase::StopIntent,
            PayloadRepairPhase::TreeCompleted,
        ] {
            assert!(!source_can_cancel(phase, false));
        }
        selection.advance(PayloadRepairPhase::Cancelled).unwrap();
        let prior_history = history.clone();
        history.release_cancelled(&selection).unwrap();
        prior_history.publication_successor(&history).unwrap();
        assert!(history.get(slot).is_none());
        history.release_cancelled(&selection).unwrap();
        let mut next = selected();
        // Correlation changes alone never release a selected operation's slot.
        let mut value = serde_json::to_value(&next).unwrap();
        value["operation"] = serde_json::json!(vec![88u8; 16]);
        next = serde_json::from_value(value).unwrap();
        assert_eq!(history.reserve(&next).unwrap(), slot);
        assert!(history.release_cancelled(&selection).is_err());

        // M2: a transient DELETE sharing failure leaves the actual durable Preparing
        // intent. Only that EXACT terminal request is adoptable on retry; deletion still
        // needs the adapter's namespace, exact FileId and positive absence proof.
        let operation = [77; 16];
        let old = PayloadRepairPublicationStamp::new(stamp(90), b"old complete").unwrap();
        let terminal = PayloadRepairPublicationIntent::new(
            operation,
            PayloadRepairPublicationTarget::Record,
            Some(old),
            &bytes,
        )
        .unwrap();
        let preparing = recover_payload_repair_publication(&terminal, Some(old), None);
        assert_eq!(preparing, PayloadRepairPublicationObservation::Preparing);
        assert!(terminal_publication_admitted(
            Some(&terminal),
            preparing,
            operation,
            PayloadRepairPublicationTarget::Record,
            &bytes
        ));
        assert!(!terminal_publication_admitted(
            Some(&terminal),
            preparing,
            [78; 16],
            PayloadRepairPublicationTarget::Record,
            &bytes
        ));
        assert!(!terminal_publication_admitted(
            Some(&terminal),
            preparing,
            operation,
            PayloadRepairPublicationTarget::Catalog,
            &bytes
        ));
        let mut changed = bytes.clone();
        changed.push(b' ');
        assert!(!terminal_publication_admitted(
            Some(&terminal),
            preparing,
            operation,
            PayloadRepairPublicationTarget::Record,
            &changed
        ));
        assert!(!terminal_publication_admitted(
            Some(&terminal),
            PayloadRepairPublicationObservation::Unknown,
            operation,
            PayloadRepairPublicationTarget::Record,
            &bytes
        ));
        let mut live = terminal.clone();
        let pending = PayloadRepairPublicationStamp::new(stamp(91), &bytes).unwrap();
        live.pending_ready(pending).unwrap();
        live.replace_intent().unwrap();
        assert!(terminal_publication_admitted(
            Some(&live),
            recover_payload_repair_publication(&live, Some(pending), None),
            operation,
            PayloadRepairPublicationTarget::Record,
            &bytes
        ));

        // R1: a subsequent Eligible begin observes the fully settled record/catalog
        // without creating a cleanup capability. An unfinished exact catalog stays adoptable.
        let mut history = PayloadRepairCatalog::default();
        let slot = history.reserve(&selected()).unwrap();
        let mut retired = baseline.durable.clone();
        history.complete(slot, &retired).unwrap();
        retired.advance(PayloadRepairPhase::Retired).unwrap();
        assert!(
            !retired_cleanup_settled(
                &retired,
                &history,
                None,
                PayloadRepairPublicationObservation::Published
            )
            .unwrap()
        );
        let mut active = PayloadRepairCatalog::default();
        active.reserve(&selected()).unwrap();
        assert!(
            retired_cleanup_settled(
                &retired,
                &active,
                None,
                PayloadRepairPublicationObservation::Published
            )
            .is_err()
        );
        history.retire(slot, &retired).unwrap();
        let mut different = serde_json::to_value(&history).unwrap();
        different["slots"][0]["operation"] = serde_json::json!(vec![88u8; 16]);
        let different = serde_json::from_value(different).unwrap();
        assert!(
            retired_cleanup_settled(
                &retired,
                &different,
                None,
                PayloadRepairPublicationObservation::Published
            )
            .is_err()
        );
        let catalog_bytes = history.encode().unwrap();
        let mut publication = PayloadRepairPublicationIntent::new(
            retired.operation(),
            PayloadRepairPublicationTarget::Catalog,
            Some(old),
            &catalog_bytes,
        )
        .unwrap();
        assert!(
            !retired_cleanup_settled(
                &retired,
                &history,
                Some(&publication),
                PayloadRepairPublicationObservation::Preparing
            )
            .unwrap()
        );
        let catalog_stamp = PayloadRepairPublicationStamp::new(stamp(92), &catalog_bytes).unwrap();
        publication.pending_ready(catalog_stamp).unwrap();
        publication.replace_intent().unwrap();
        // Replacement alone is not settled until the intent itself is Published.
        assert!(
            !retired_cleanup_settled(
                &retired,
                &history,
                Some(&publication),
                PayloadRepairPublicationObservation::Published
            )
            .unwrap()
        );
        publication.published().unwrap();
        let record_before = retired.encode().unwrap();
        let history_before = history.encode().unwrap();
        for _ in 0..3 {
            assert!(
                retired_cleanup_settled(
                    &retired,
                    &history,
                    Some(&publication),
                    PayloadRepairPublicationObservation::Published
                )
                .unwrap()
            );
            assert!(
                retired_cleanup_settled(
                    &retired,
                    &history,
                    None,
                    PayloadRepairPublicationObservation::Published
                )
                .unwrap()
            );
        }
        assert_eq!(retired.encode().unwrap(), record_before);
        assert_eq!(history.encode().unwrap(), history_before);
        assert!(retired_cleanup_settled(&retired, &history, Some(&terminal), preparing).is_err());
        assert!(
            retired_cleanup_settled(
                &retired,
                &history,
                Some(&publication),
                PayloadRepairPublicationObservation::Unknown
            )
            .is_err()
        );
        assert!(
            retired_cleanup_settled(
                &retired,
                &PayloadRepairCatalog::default(),
                None,
                PayloadRepairPublicationObservation::Published
            )
            .is_err()
        );

        let mut cancelled = Fake::new();
        let mut value = serde_json::to_value(&cancelled.durable).unwrap();
        value["phase"] = serde_json::json!({"state":"cancelled"});
        cancelled.durable = serde_json::from_value(value).unwrap();
        assert_eq!(cancelled.run().unwrap(), PayloadRepairOutcome::Cancelled);
        assert_eq!(
            (cancelled.creates, cancelled.stops, cancelled.runs),
            (0, 0, 0)
        );
    }
}
