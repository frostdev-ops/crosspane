#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Authored OS-free observations and actual portable controllers; no credentials or native roots.
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
#[path = "../src/platform/windows/service.rs"]
mod service;
#[path = "../src/platform/windows/transport.rs"]
mod transport;
use native_io::identity::{Sid, TokenFacts};
use native_io::{NativeError, NativeResult};
use payload::recovery::{FileStamp, OuterContextCorrelation};
use removal::{executor::*, inventory::*, plan::RemovalPlan, *};
use std::collections::BTreeMap;
fn stamp(n: u8) -> FileStamp {
    FileStamp {
        volume: 7,
        file: [n; 16],
    }
}
fn sid(parts: &[u32]) -> Sid {
    let mut b = vec![1, parts.len() as u8, 0, 0, 0, 0, 0, 5];
    for p in parts {
        b.extend_from_slice(&p.to_le_bytes())
    }
    Sid::from_bytes(b).unwrap()
}
fn token() -> TokenFacts {
    TokenFacts {
        user: sid(&[21, 7]),
        logon: sid(&[5, 9, 11]),
        authentication_id: 17,
        session: 2,
        elevated: false,
        integrity: 0x2000,
        impersonating: false,
    }
}
fn record(erase: bool) -> RemovalRecord {
    record_copies(erase, vec![RemovalCopyKind::Keeper])
}
fn record_copies(erase: bool, kinds: Vec<RemovalCopyKind>) -> RemovalRecord {
    let mut nodes = Vec::new();
    for (i, role) in payload::inventory::PayloadRole::ALL.into_iter().enumerate() {
        nodes.push(
            RemovalNode::new(
                vec![role.leaf().into()],
                stamp(1),
                stamp(10 + i as u8),
                RemovalNodeKind::File,
            )
            .unwrap(),
        )
    }
    nodes.push(
        RemovalNode::new(
            vec!["stale-link".into()],
            stamp(1),
            stamp(20),
            RemovalNodeKind::Reparse,
        )
        .unwrap(),
    );
    let inventory = RemovalInventory::new(
        Some(stamp(1)),
        nodes,
        RemovalTask::new(
            "<Task fixture=\"own\"/>".into(),
            Some("<Task fixture=\"own\"/>".into()),
        )
        .unwrap(),
        kinds.into_iter().map(RemovalCopy::planned).collect(),
    )
    .unwrap();
    RemovalRecord::new(
        [7; 16],
        OuterContextCorrelation::new(&token()).unwrap(),
        RemovalOptions {
            erase_identity: erase,
        },
        RemovalPlan::new(inventory).unwrap(),
    )
    .unwrap()
}
fn ready(r: &mut RemovalRecord) {
    use RemovalHandoffStage::*;
    for index in 0..r.plan().copies().len() {
        let index = index as u8;
        r.advance_handoff(CopyPrepareIntent { index }).unwrap();
        r.bind_copy_image(
            index,
            stamp(50 + index),
            payload::inventory::PeFacts {
                size: 4096,
                sha256: [50 + index; 32],
                machine: 0x8664,
                subsystem: 2,
                version: "fixture-1".into(),
            },
        )
        .unwrap();
        r.advance_handoff(CopyPrepared { index }).unwrap();
        r.advance_handoff(CreateIntent { index }).unwrap();
        r.bind_copy_child(index, 102 + u32::from(index), 200 + u64::from(index), 64)
            .unwrap();
        r.advance_handoff(Created { index }).unwrap();
        r.advance_handoff(ResumeIntent { index }).unwrap();
        r.advance_handoff(Ready { index }).unwrap();
    }
}
fn committed(r: &mut RemovalRecord) {
    ready(r);
    let index = (r.plan().copies().len() - 1) as u8;
    r.advance_handoff(RemovalHandoffStage::RemovalCommitIntent { index })
        .unwrap();
    r.advance_handoff(RemovalHandoffStage::Committed { index })
        .unwrap()
}
fn receipt() -> agent_contract::LastExitV1 {
    agent_contract::parse_last_exit(br#"{"schema_version":1,"instance_id":44,"stopped_unix_ms":200,"clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true}"#).unwrap()
}
fn erased() -> agent_contract::EraseIdentityV1 {
    agent_contract::parse_erase_identity(br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"}"#).unwrap()
}
#[derive(Clone)]
struct Tree;
struct Port {
    durable: RemovalRecord,
    nodes: Vec<NodePresence>,
    root: bool,
    task: bool,
    task_xml: String,
    copy: Vec<bool>,
    copy_identity: Vec<FileStamp>,
    executing: Option<u8>,
    copy_exited: Vec<bool>,
    copy_exit_known: Vec<bool>,
    retained_tree: bool,
    retained_erase: bool,
    authorized: bool,
    complete_tree: bool,
    install_aliases: bool,
    worker_aliases: bool,
    receipt: Option<agent_contract::LastExitV1>,
    stops: usize,
    erases: usize,
    task_deletes: usize,
    deletes: Vec<u16>,
    root_deletes: usize,
    copy_deletes: usize,
    writes: usize,
    calls: usize,
    fail: Option<(usize, bool)>,
    user_data: BTreeMap<&'static str, &'static str>,
    upgrade: bool,
}
impl Port {
    fn new(erase: bool) -> Self {
        let mut durable = record(erase);
        committed(&mut durable);
        Self {
            nodes: vec![NodePresence::Same; durable.plan().order().len()],
            durable,
            root: true,
            task: true,
            task_xml: "<Task fixture=\"own\"/>".into(),
            copy: vec![true],
            copy_identity: vec![stamp(50)],
            executing: Some(0),
            copy_exited: vec![false],
            copy_exit_known: vec![true],
            retained_tree: false,
            retained_erase: false,
            authorized: true,
            complete_tree: true,
            install_aliases: true,
            worker_aliases: false,
            receipt: Some(receipt()),
            stops: 0,
            erases: 0,
            task_deletes: 0,
            deletes: Vec::new(),
            root_deletes: 0,
            copy_deletes: 0,
            writes: 0,
            calls: 0,
            fail: None,
            user_data: BTreeMap::from([
                ("config", "fixture config"),
                ("keys", "fixture untouched"),
                ("logs", "fixture logs"),
            ]),
            upgrade: false,
        }
    }
    fn before(&mut self) -> NativeResult<()> {
        self.calls += 1;
        if self.fail == Some((self.calls, false)) {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn after(&self) -> NativeResult<()> {
        if self.fail == Some((self.calls, true)) {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn run(&mut self, c: &mut RemovalController<Tree>) -> NativeResult<RemovalResult> {
        let mut r = self.durable.clone();
        c.run(self, &mut r)
    }
}
impl RemovalPort for Port {
    type Tree = Tree;
    fn renew(&mut self, r: &RemovalRecord) -> NativeResult<()> {
        r.validate()?;
        admit_removal_selection(self.upgrade)?;
        self.durable.same_selection(r)
    }
    fn persist(&mut self, r: &RemovalRecord) -> NativeResult<()> {
        self.renew(r)?;
        self.before()?;
        self.durable = r.clone();
        self.writes += 1;
        self.after()
    }
    fn commit_authorized(&mut self, _: &RemovalRecord) -> NativeResult<()> {
        if self.authorized {
            Ok(())
        } else {
            Err(NativeError::Foreign)
        }
    }
    fn stop_once(&mut self, r: &RemovalRecord) -> NativeResult<Tree> {
        assert_eq!(r.cursor(), RemovalCursor::StopIntent);
        self.before()?;
        assert_eq!(self.stops, 0);
        self.stops += 1;
        if !self.complete_tree {
            return Err(NativeError::Unavailable);
        }
        self.retained_tree = true;
        self.after()?;
        Ok(Tree)
    }
    fn recover_stop(&mut self, _: &RemovalRecord) -> NativeResult<Option<Tree>> {
        Ok(self.retained_tree.then_some(Tree))
    }
    fn stopped_facts(&self, _: &Tree) -> NativeResult<StoppedTreeFacts> {
        Ok(StoppedTreeFacts {
            generation: service::supervisor::Generation {
                pid: 101,
                creation: 100,
                instance: 44,
            },
            started_unix_ms: 100,
        })
    }
    fn renew_completion(&mut self, r: &RemovalRecord, _: &Tree) -> NativeResult<()> {
        self.renew(r)?;
        if self.retained_tree {
            Ok(())
        } else {
            Err(NativeError::Foreign)
        }
    }
    fn receipt(
        &mut self,
        _: &RemovalRecord,
        _: &Tree,
    ) -> NativeResult<Option<agent_contract::LastExitV1>> {
        Ok(self.receipt.clone())
    }
    fn erase_once(
        &mut self,
        r: &RemovalRecord,
        _: &Tree,
    ) -> NativeResult<agent_contract::EraseIdentityV1> {
        assert_eq!(r.cursor(), RemovalCursor::EraseIntent);
        self.before()?;
        assert_eq!(self.erases, 0);
        self.erases += 1;
        self.retained_erase = true;
        self.after()?;
        Ok(erased())
    }
    fn recover_erase(
        &mut self,
        _: &RemovalRecord,
        _: &Tree,
    ) -> NativeResult<Option<agent_contract::EraseIdentityV1>> {
        Ok(self.retained_erase.then(erased))
    }
    fn task_absent(&mut self, _: &RemovalRecord) -> NativeResult<bool> {
        Ok(!self.task)
    }
    fn task_delete(&mut self, r: &RemovalRecord, _: &Tree) -> NativeResult<()> {
        assert_eq!(r.cursor(), RemovalCursor::TaskDeleteIntent);
        self.before()?;
        if self.task_xml != r.plan().task().expected_xml() {
            return Err(NativeError::Foreign);
        }
        self.task = false;
        self.task_deletes += 1;
        self.after()
    }
    fn observe_node(&mut self, _: &RemovalRecord, i: u16) -> NativeResult<NodePresence> {
        Ok(self.nodes[usize::from(i)])
    }
    fn delete_node(&mut self, r: &RemovalRecord, i: u16, _: &Tree) -> NativeResult<()> {
        assert_eq!(r.cursor(), RemovalCursor::DeleteIntent { index: i });
        self.before()?;
        assert_eq!(self.nodes[usize::from(i)], NodePresence::Same);
        assert!(!self.deletes.contains(&i));
        self.nodes[usize::from(i)] = NodePresence::Absent;
        self.deletes.push(i);
        self.after()
    }
    fn root_absent(&mut self, _: &RemovalRecord) -> NativeResult<bool> {
        Ok(!self.root)
    }
    fn settle_install_aliases(&mut self, _: &RemovalRecord, _: &Tree) -> NativeResult<()> {
        if self.worker_aliases {
            return Err(NativeError::Busy);
        }
        self.before()?;
        self.install_aliases = false;
        self.after()
    }
    fn delete_root(&mut self, r: &RemovalRecord, _: &Tree) -> NativeResult<()> {
        assert_eq!(r.cursor(), RemovalCursor::RootDeleteIntent);
        self.before()?;
        assert!(!self.install_aliases);
        assert!(self.nodes.iter().all(|n| *n == NodePresence::Absent));
        self.root = false;
        self.root_deletes += 1;
        self.after()
    }
    fn executing_copy(&mut self, _: &RemovalRecord) -> NativeResult<Option<u8>> {
        Ok(self.executing)
    }
    fn copy_absent(&mut self, r: &RemovalRecord, index: u8) -> NativeResult<bool> {
        let i = usize::from(index);
        if self.copy[i] && r.plan().copies()[i].identity() != Some(self.copy_identity[i]) {
            return Err(NativeError::Foreign);
        }
        Ok(!self.copy[i])
    }
    fn retire_copy(&mut self, r: &RemovalRecord, index: u8) -> NativeResult<()> {
        let i = usize::from(index);
        if !self.copy_exit_known[i] {
            return Err(NativeError::Unavailable);
        }
        if !self.copy_exited[i] {
            return Err(NativeError::Busy);
        }
        if self.executing == Some(index)
            || r.plan().copies()[i].identity() != Some(self.copy_identity[i])
        {
            return Err(NativeError::Foreign);
        }
        self.before()?;
        self.copy[i] = false;
        self.copy_deletes += 1;
        self.after()
    }
    fn cleanup_final_copy(&mut self, r: &RemovalRecord, i: u8) -> NativeResult<()> {
        assert_eq!(
            r.cursor(),
            RemovalCursor::FinalCopyCleanupIntent { index: i }
        );
        if !self.copy_exit_known[usize::from(i)]
            || !self.copy_exited[usize::from(i)]
            || self.executing == Some(i)
        {
            return Err(NativeError::Busy);
        }
        self.before()?;
        self.copy[usize::from(i)] = false;
        self.copy_deletes += 1;
        self.after()
    }
}
#[test]
fn default_preserves_all_authored_user_data_and_retains_external_final_copy() {
    let mut p = Port::new(false);
    let data = p.user_data.clone();
    let mut c = RemovalController::default();
    assert_eq!(
        p.run(&mut c),
        Ok(RemovalResult::RemovedWithRetainedCopy { index: 0 })
    );
    assert_eq!(p.user_data, data);
    assert_eq!(p.erases, 0);
    assert!(!p.root && !p.task);
    assert!(p.copy[0]);
    assert_eq!(p.copy_deletes, 0);
    assert_eq!(p.root_deletes, 1);
    let mut blocked = Port::new(false);
    blocked.worker_aliases = true;
    assert_eq!(
        blocked.run(&mut RemovalController::default()),
        Err(NativeError::Busy)
    );
    assert_eq!(blocked.root_deletes, 0);
    assert!(blocked.root);
    // A distinct non-final Helper requires its actual retained original exit observation.
    let mut two = Port::new(false);
    two.durable = record_copies(
        false,
        vec![RemovalCopyKind::Helper, RemovalCopyKind::Keeper],
    );
    committed(&mut two.durable);
    two.copy = vec![true, true];
    two.copy_identity = vec![stamp(50), stamp(51)];
    two.copy_exited = vec![false, false];
    two.copy_exit_known = vec![true, true];
    two.executing = Some(1);
    let mut owner = RemovalController::default();
    assert_eq!(two.run(&mut owner), Err(NativeError::Busy));
    assert_eq!(two.copy_deletes, 0);
    assert_eq!(two.copy, [true, true]);
    two.copy_exit_known[0] = false;
    assert_eq!(two.run(&mut owner), Err(NativeError::Unavailable));
    assert_eq!(two.copy_deletes, 0);
    two.copy_exit_known[0] = true;
    two.copy_exited[0] = true;
    two.copy_identity[0] = stamp(99);
    assert_eq!(two.run(&mut owner), Err(NativeError::Foreign));
    assert_eq!(two.copy_deletes, 0);
    two.copy_identity[0] = stamp(50);
    assert_eq!(
        two.run(&mut owner),
        Ok(RemovalResult::RemovedWithRetainedCopy { index: 1 })
    );
    assert_eq!(two.copy, [false, true]);
    assert_eq!(two.copy_deletes, 1);
    assert_eq!(two.durable.plan().copies()[0].pid(), Some(102));
    assert_eq!(two.durable.plan().copies()[1].pid(), Some(103));
    let writes = p.writes;
    assert_eq!(
        p.run(&mut c),
        Ok(RemovalResult::RemovedWithRetainedCopy { index: 0 })
    );
    assert_eq!(p.writes, writes);
}
#[test]
fn missing_unclean_stale_or_malformed_receipt_blocks_opt_in_erase_and_payload_deletion() {
    for case in 0..7 {
        let mut p = Port::new(true);
        match case {
            0 => p.receipt = None,
            1 => p.receipt.as_mut().unwrap().instance_id = 45,
            2 => p.receipt.as_mut().unwrap().stopped_unix_ms = 99,
            3 => p.receipt.as_mut().unwrap().clean = false,
            4 => p.receipt.as_mut().unwrap().input_journals_empty = false,
            5 => p.receipt.as_mut().unwrap().audio_stopped = false,
            _ => p.complete_tree = false,
        }
        assert!(p.run(&mut RemovalController::default()).is_err());
        assert_eq!(p.erases, 0);
        assert_eq!(p.task_deletes, 0);
        assert!(p.deletes.is_empty());
        assert!(p.root);
    }
    assert!(agent_contract::parse_last_exit(b"{}").is_err());
}
#[test]
fn erase_is_one_shot_and_unknown_delivery_uses_only_retained_original_attempt() {
    let mut baseline = Port::new(true);
    baseline.run(&mut RemovalController::default()).unwrap();
    assert_eq!(baseline.erases, 1);
    for after in [false, true] {
        let mut p = Port::new(true);
        p.fail = Some((5, after));
        let mut c = RemovalController::default();
        assert_eq!(p.run(&mut c), Err(NativeError::OutcomeUnknown));
        p.fail = None;
        let before = p.erases;
        let result = p.run(&mut c).unwrap();
        assert_eq!(p.erases, before);
        if after {
            assert_eq!(result, RemovalResult::RemovedWithRetainedCopy { index: 0 })
        } else {
            assert_eq!(result, RemovalResult::Retained)
        }
    }
}
#[test]
fn exact_task_and_file_identity_only_reparse_targets_never_enter_plan() {
    let mut p = Port::new(false);
    p.task_xml = "<Task fixture=\"foreign\"/>".into();
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Err(NativeError::Foreign)
    );
    assert!(p.deletes.is_empty());
    let mut p = Port::new(false);
    p.nodes[2] = NodePresence::Changed;
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Ok(RemovalResult::Retained)
    );
    assert!(!p.deletes.contains(&2));
    for name in ["..", r"C:\foreign", "link/target"] {
        assert!(
            RemovalNode::new(
                vec![name.into()],
                stamp(1),
                stamp(2),
                RemovalNodeKind::Reparse
            )
            .is_err()
        )
    }
    let mut absent = Port::new(false);
    absent.task = false;
    assert_eq!(
        absent.run(&mut RemovalController::default()),
        Ok(RemovalResult::RemovedWithRetainedCopy { index: 0 })
    );
    assert_eq!(absent.task_deletes, 0);
    let p = Port::new(false);
    assert_eq!(
        p.durable.plan().node(4).unwrap().kind(),
        RemovalNodeKind::Reparse
    );
    assert_eq!(
        p.durable.plan().node(4).unwrap().components(),
        ["stale-link"]
    );
}
#[test]
fn every_post_commit_intent_effect_and_result_interruption_reopens_without_replay() {
    for erase in [false, true] {
        let mut b = Port::new(erase);
        b.run(&mut RemovalController::default()).unwrap();
        let count = b.calls;
        for at in 1..=count {
            for after in [false, true] {
                let mut p = Port::new(erase);
                p.fail = Some((at, after));
                let mut c = RemovalController::default();
                assert_eq!(p.run(&mut c), Err(NativeError::OutcomeUnknown));
                p.fail = None;
                let stops = p.stops;
                let erases = p.erases;
                let result = p.run(&mut c).unwrap();
                assert!(matches!(
                    result,
                    RemovalResult::Retained | RemovalResult::RemovedWithRetainedCopy { .. }
                ));
                assert!(p.stops <= 1 && p.erases <= 1);
                if p.durable.cursor() != RemovalCursor::Committed {
                    assert_eq!(p.stops, stops)
                }
                if matches!(p.durable.cursor(), RemovalCursor::EraseIntent) {
                    assert_eq!(p.erases, erases)
                }
                let mut cold = RemovalController::default();
                p.retained_tree = false;
                p.retained_erase = false;
                p.authorized = false;
                let old = (p.stops, p.erases);
                let _ = p.run(&mut cold);
                assert_eq!((p.stops, p.erases), old);
            }
        }
    }
}
#[test]
fn later_final_copy_cleanup_requires_real_absence_and_never_repeats_stop_or_erase() {
    let mut p = Port::new(false);
    p.run(&mut RemovalController::default()).unwrap();
    let old = (p.stops, p.erases, p.deletes.clone());
    p.executing = None;
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Err(NativeError::Busy)
    );
    assert!(p.copy[0]);
    p.copy_exited[0] = true;
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Ok(RemovalResult::Removed)
    );
    assert_eq!(p.copy_deletes, 1);
    let writes = p.writes;
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Ok(RemovalResult::Removed)
    );
    assert_eq!(p.writes, writes);
    assert_eq!((p.stops, p.erases, p.deletes.clone()), old);
}
#[test]
fn upgrade_and_removal_are_mutually_exclusive_without_preemption() {
    let mut p = Port::new(false);
    p.upgrade = true;
    assert_eq!(
        p.run(&mut RemovalController::default()),
        Err(NativeError::Busy)
    );
    assert_eq!(p.stops, 0);
    assert_eq!(p.writes, 0);
    assert_eq!(
        admit_upgrade_selection(Some(&p.durable)),
        Err(NativeError::Busy)
    );
    p.upgrade = false;
    p.run(&mut RemovalController::default()).unwrap();
    assert_eq!(
        admit_upgrade_selection(Some(&p.durable)),
        Err(NativeError::Busy)
    );
    p.executing = None;
    p.copy_exited[0] = true;
    p.run(&mut RemovalController::default()).unwrap();
    assert_eq!(admit_upgrade_selection(Some(&p.durable)), Ok(()));
}
#[test]
fn pre_stop_handoff_metadata_ready_and_forged_commit_never_authorize_stop() {
    // The actual native publisher uses this shared gate against fresh durable bytes.
    // Same-stage actual image/child observation binding stays admissible; stale stages do not.
    let mut journal = record_copies(
        false,
        vec![RemovalCopyKind::Helper, RemovalCopyKind::Keeper],
    );
    let mut previous_ready = None;
    for index in 0..2u8 {
        journal
            .advance_handoff(RemovalHandoffStage::CopyPrepareIntent { index })
            .unwrap();
        if let Some(old) = previous_ready.take() {
            let old: RemovalRecord = old;
            assert_eq!(old.publication_successor(&journal), Ok(()));
            assert_eq!(
                journal.publication_successor(&old),
                Err(NativeError::Foreign)
            );
        }
        let unbound = journal.clone();
        journal
            .bind_copy_image(
                index,
                stamp(50 + index),
                payload::inventory::PeFacts {
                    size: 4096,
                    sha256: [50 + index; 32],
                    machine: 0x8664,
                    subsystem: 2,
                    version: "fixture-1".into(),
                },
            )
            .unwrap();
        assert_eq!(unbound.publication_successor(&journal), Ok(()));
        journal
            .advance_handoff(RemovalHandoffStage::CopyPrepared { index })
            .unwrap();
        journal
            .advance_handoff(RemovalHandoffStage::CreateIntent { index })
            .unwrap();
        let unbound = journal.clone();
        journal
            .bind_copy_child(index, 102 + u32::from(index), 200 + u64::from(index), 64)
            .unwrap();
        assert_eq!(unbound.publication_successor(&journal), Ok(()));
        journal
            .advance_handoff(RemovalHandoffStage::Created { index })
            .unwrap();
        journal
            .advance_handoff(RemovalHandoffStage::ResumeIntent { index })
            .unwrap();
        let resume = journal.clone();
        journal
            .advance_handoff(RemovalHandoffStage::Ready { index })
            .unwrap();
        assert_eq!(resume.publication_successor(&journal), Ok(()));
        assert_eq!(
            journal.publication_successor(&resume),
            Err(NativeError::Foreign)
        );
        previous_ready = Some(journal.clone());
    }
    let ready_stage = journal.clone();
    journal
        .advance_handoff(RemovalHandoffStage::RemovalCommitIntent { index: 1 })
        .unwrap();
    assert_eq!(ready_stage.publication_successor(&journal), Ok(()));
    assert_eq!(
        journal.publication_successor(&ready_stage),
        Err(NativeError::Foreign)
    );
    journal
        .advance_handoff(RemovalHandoffStage::Committed { index: 1 })
        .unwrap();
    let committed = journal.clone();
    journal.advance(RemovalCursor::StopIntent).unwrap();
    assert_eq!(
        journal.publication_successor(&committed),
        Err(NativeError::Foreign)
    );
    let mut p = Port::new(false);
    p.durable = record(false);
    let mut c = RemovalController::default();
    assert_eq!(p.run(&mut c), Ok(RemovalResult::Retained));
    assert_eq!(p.stops, 0);
    ready(&mut p.durable);
    assert_eq!(p.run(&mut c), Ok(RemovalResult::Retained));
    assert_eq!(p.stops, 0);
    p.durable
        .advance_handoff(RemovalHandoffStage::RemovalCommitIntent { index: 0 })
        .unwrap();
    p.durable
        .advance_handoff(RemovalHandoffStage::Committed { index: 0 })
        .unwrap();
    p.authorized = false;
    assert_eq!(p.run(&mut c), Err(NativeError::Foreign));
    assert_eq!(p.stops, 0);
    // Reuse the actual existing keeper controller for readiness/commit sequencing; the fake
    // child is held independently of serialized metadata, with no upgrade inputs or records.
    struct Prep {
        record: RemovalRecord,
        launches: usize,
        commits: usize,
        applies: usize,
        fail: bool,
    }
    impl native_io::activation::KeeperPort for Prep {
        type Child = u8;
        fn prepare(&mut self) -> NativeResult<u8> {
            self.launches += 1;
            ready(&mut self.record);
            if self.fail {
                Err(NativeError::OutcomeUnknown)
            } else {
                Ok(1)
            }
        }
        fn mark_ready(&mut self, child: &u8) -> NativeResult<()> {
            assert_eq!(*child, 1);
            assert!(matches!(
                self.record.handoff_stage(),
                RemovalHandoffStage::Ready { .. }
            ));
            Ok(())
        }
        fn commit_intent(&mut self, child: &u8) -> NativeResult<()> {
            assert_eq!(*child, 1);
            assert_eq!(self.commits, 0);
            self.commits += 1;
            self.record
                .advance_handoff(RemovalHandoffStage::RemovalCommitIntent { index: 0 })?;
            self.record
                .advance_handoff(RemovalHandoffStage::Committed { index: 0 })
        }
        fn apply_once(&mut self, _: &u8) -> NativeResult<native_io::activation::KeeperProgress> {
            assert_eq!(self.commits, 1);
            self.applies += 1;
            Ok(native_io::activation::KeeperProgress::Complete)
        }
        fn recover_same_owner(
            &mut self,
            _: &u8,
        ) -> NativeResult<native_io::activation::KeeperProgress> {
            Ok(native_io::activation::KeeperProgress::Complete)
        }
        fn settle(&mut self, _: &u8) -> NativeResult<()> {
            Ok(())
        }
        fn cancel_before_stop(&mut self, _: &u8) -> NativeResult<()> {
            assert_eq!(self.commits, 0);
            assert_eq!(self.applies, 0);
            Ok(())
        }
    }
    let mut prep = Prep {
        record: record(false),
        launches: 0,
        commits: 0,
        applies: 0,
        fail: false,
    };
    let mut owner = native_io::activation::KeeperControl::default();
    owner.prepare(&mut prep).unwrap();
    assert_eq!(prep.commits, 0);
    assert_eq!(prep.applies, 0);
    owner.cancel(&mut prep).unwrap();
    assert_eq!(prep.applies, 0);
    assert!(owner.commit(&mut prep).is_err());
    let mut lost = Prep {
        record: record(false),
        launches: 0,
        commits: 0,
        applies: 0,
        fail: true,
    };
    let mut owner = native_io::activation::KeeperControl::default();
    assert_eq!(owner.prepare(&mut lost), Err(NativeError::OutcomeUnknown));
    assert!(owner.prepare(&mut lost).is_err());
    assert_eq!(lost.launches, 1);
    assert_eq!(lost.applies, 0);
    let mut changed = serde_json::to_value(&p.durable).unwrap();
    changed["plan"]["inventory"]["copies"][0]["image"]["version"] =
        serde_json::json!("older-peer-build");
    changed["plan"]["inventory"]["copies"][0]["image"]["sha256"][0] = serde_json::json!(99);
    let changed: RemovalRecord = serde_json::from_value(changed).unwrap();
    changed.validate().unwrap();
    assert_eq!(
        p.durable.same_selection(&changed),
        Err(NativeError::Foreign)
    );
    assert_eq!(p.stops, 0);
    let mut raw = serde_json::to_value(&p.durable).unwrap();
    raw["unknown_authority"] = serde_json::json!(true);
    let bytes =
        native_io::records::encode_record(&native_io::records::RecordName::Removal, raw).unwrap();
    assert!(RemovalRecord::decode(&bytes).is_err());
}
