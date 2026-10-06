#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! In-memory payload/PE fixtures only. No native filesystem, task, agent, or process is run.
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
use native_io::{NativeError, NativeResult, files::FileIdentity};
use payload::{
    health::{self, ServicePort, VerifiedPayload},
    inventory::*,
    recovery::*,
    staging::{self, PayloadPort},
};
use service::{NewInstanceEvidence, UpgradeStopProof};
fn pe(marker: u8) -> Vec<u8> {
    let mut b = vec![marker; 512];
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
    let b = pe(role as u8);
    let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &b);
    let mut sha256 = [0; 32];
    sha256.copy_from_slice(hash.as_ref());
    let (machine, subsystem) = pe_header(&b, b.len() as u64).unwrap();
    PeFacts {
        size: b.len() as u64,
        sha256,
        machine,
        subsystem,
        version: "1.0.0".into(),
    }
}
fn pins() -> Vec<ApprovedPe> {
    PayloadRole::ALL
        .into_iter()
        .map(|role| ApprovedPe::fixture(role, facts(role)))
        .collect()
}
fn manifest() -> serde_json::Value {
    let payloads = [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl].map(|role| {
        let pin = facts(role);
        serde_json::json!({"role":role,"leaf":role.leaf(),"size":pin.size,"sha256":native_io::records::hex(&pin.sha256),"machine":pin.machine,"subsystem":pin.subsystem,"version":pin.version})
    });
    serde_json::json!({"schema_version":1,"payloads":payloads})
}
fn parse(v: serde_json::Value) -> NativeResult<ApprovedInventory> {
    ApprovedInventory::fixture_parse(Some(&v.to_string()))
}
#[test]
fn embedded_inventory_requires_a_genuine_present_source() {
    assert_eq!(
        ApprovedInventory::fixture_parse(None).err(),
        Some(NativeError::Unsupported)
    );
    assert_eq!(
        ApprovedInventory::fixture_parse(Some("{}")).err(),
        Some(NativeError::Unsupported)
    );
    let i = parse(manifest()).unwrap();
    assert!(i.role(PayloadRole::Installer).is_err());
    for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
        assert_eq!(i.role(role).unwrap().role(), role)
    }
}
#[test]
fn strict_inventory_rejects_duplicate_unknown_and_wrong_fixed_role() {
    let mut v = manifest();
    v["payloads"][1] = v["payloads"][0].clone();
    assert!(parse(v).is_err());
    let mut v = manifest();
    v["unknown"] = serde_json::json!(0);
    assert!(parse(v).is_err());
    let mut v = manifest();
    v["payloads"][0]["unknown"] = serde_json::json!(0);
    assert!(parse(v).is_err());
    let mut v = manifest();
    v["payloads"][0]["role"] = serde_json::json!("installer");
    assert!(parse(v).is_err());
    let mut v = manifest();
    v["payloads"][0]["role"] = serde_json::json!("foreign");
    assert!(parse(v).is_err());
    let mut v = manifest();
    v["payloads"][0]["leaf"] = serde_json::json!("other.exe");
    assert!(parse(v).is_err());
    let text = manifest().to_string().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert!(ApprovedInventory::fixture_parse(Some(&text)).is_err());
    let text = manifest()
        .to_string()
        .replacen("\"size\":512", "\"size\":512,\"size\":512", 1);
    assert!(ApprovedInventory::fixture_parse(Some(&text)).is_err());
}
#[test]
fn inventory_limits_hash_pe_and_version_are_strict() {
    for (field, value) in [
        ("size", serde_json::json!(MAX_IMAGE_BYTES + 1)),
        ("sha256", serde_json::json!("00")),
        ("machine", serde_json::json!(0x14c)),
        ("subsystem", serde_json::json!(0)),
        ("version", serde_json::json!("")),
    ] {
        let mut v = manifest();
        v["payloads"][0][field] = value;
        assert!(parse(v).is_err());
    }
    assert!(ApprovedInventory::fixture_parse(Some(&" ".repeat(MAX_INVENTORY_BYTES + 1))).is_err());
    let mut p = pins();
    p[0] = ApprovedPe::fixture(
        PayloadRole::Installer,
        PeFacts {
            size: MAX_IMAGE_BYTES,
            ..facts(PayloadRole::Installer)
        },
    );
    let i = ApprovedInventory::fixture(p.clone()).unwrap();
    assert_eq!(
        i.check_staging_budget(&p[0], true),
        Err(NativeError::Oversize)
    );
    let i = ApprovedInventory::fixture(pins()).unwrap();
    i.check_staging_budget(i.role(PayloadRole::Installer).unwrap(), true)
        .unwrap();
}
#[test]
fn pe_header_refuses_truncated_non_pe32plus_and_unsupported_machine() {
    let b = pe(7);
    assert_eq!(pe_header(&b, 512), Ok((0x8664, 3)));
    for end in [0, 2, 63, 68, 88, 157, 327] {
        assert!(pe_header(&b[..end], 512).is_err());
    }
    let mut b = pe(7);
    b[88..90].copy_from_slice(&0x10bu16.to_le_bytes());
    assert!(pe_header(&b, 512).is_err());
    let mut b = pe(7);
    b[68..70].copy_from_slice(&0x14cu16.to_le_bytes());
    assert!(pe_header(&b, 512).is_err());
    let mut b = pe(7);
    b[60..64].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(pe_header(&b, 512).is_err());
}
fn image(role: PayloadRole) -> ImageObservation {
    ImageObservation {
        identity: FileStamp {
            volume: 1,
            file: [role as u8 + 1; 16],
        },
        facts: facts(role),
    }
}
#[derive(Clone, Copy, Debug)]
enum OldLeaf {
    Wrong,
    Stale,
    Link,
    Hardlink,
    Directory,
    CaseVaried,
}
#[derive(Clone, Default)]
struct PhysicalRole {
    staged: Option<ImageObservation>,
    partial: bool,
    fixed_new: Option<ImageObservation>,
    old_present: bool,
    backup: Option<FileStamp>,
}
struct Fake {
    durable: Option<OperationRecord>,
    events: Vec<&'static str>,
    effects: Vec<&'static str>,
    count: usize,
    fault: Option<(usize, bool)>,
    retired: bool,
    loaded: bool,
    original: Option<u64>,
    next: u64,
    wrong_image: bool,
    incomplete: bool,
    old: OldLeaf,
    target_reads: usize,
    world: Vec<PhysicalRole>,
    stopped: bool,
    started: bool,
}
impl Default for Fake {
    fn default() -> Self {
        Self {
            durable: None,
            events: Vec::new(),
            effects: Vec::new(),
            count: 0,
            fault: None,
            retired: false,
            loaded: false,
            original: Some(41),
            next: 42,
            wrong_image: false,
            incomplete: false,
            old: OldLeaf::Wrong,
            target_reads: 0,
            world: (0..4)
                .map(|_| PhysicalRole {
                    old_present: true,
                    ..Default::default()
                })
                .collect(),
            stopped: false,
            started: false,
        }
    }
}
impl Fake {
    fn before(&mut self, event: &'static str) -> NativeResult<()> {
        if self.retired {
            return Err(NativeError::OutcomeUnknown);
        }
        self.events.push(event);
        self.count += 1;
        if self.fault == Some((self.count, false)) {
            self.retired = true;
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    fn after(&mut self) -> NativeResult<()> {
        if self.fault == Some((self.count, true)) {
            self.retired = true;
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
    fn perform(
        &mut self,
        event: &'static str,
        phase: Phase,
        role: Option<PayloadRole>,
        effect: impl FnOnce(&mut Self),
    ) -> NativeResult<()> {
        let durable = self.durable.as_ref().unwrap();
        assert_eq!(durable.phase(), phase);
        assert_eq!(durable.current_role(), role);
        self.before(event)?;
        self.effects.push(event);
        effect(self);
        self.after()
    }
    fn effect(
        &mut self,
        event: &'static str,
        phase: Phase,
        role: Option<PayloadRole>,
    ) -> NativeResult<()> {
        let durable = self.durable.as_ref().unwrap();
        assert_eq!(durable.phase(), phase);
        assert_eq!(durable.current_role(), role);
        self.before(event)?;
        self.effects.push(event);
        self.after()
    }
}
impl PayloadPort for Fake {
    type Stop = UpgradeStopProof;
    type Verified = VerifiedPayload;
    type Started = NewInstanceEvidence;
    fn journal(&mut self, record: &OperationRecord) -> NativeResult<()> {
        self.before("journal")?;
        record.validate()?;
        self.durable = Some(record.clone());
        self.after()
    }
    fn stop(&mut self, op: [u8; 16]) -> NativeResult<Self::Stop> {
        self.perform("stop", Phase::StopIntent, None, |world| {
            world.stopped = true
        })?;
        UpgradeStopProof::fixture(op, self.original)
    }
    fn original_instance(&self, proof: &Self::Stop) -> Option<u64> {
        proof.original_instance()
    }
    fn released(&mut self, _: &Self::Stop) -> NativeResult<()> {
        self.before("release")?;
        if self.loaded {
            return Err(NativeError::Busy);
        }
        self.after()
    }
    fn stage(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<ImageObservation> {
        self.perform("create", Phase::StageIntent, Some(role), |this| {
            this.world[role as usize].partial = true
        })?;
        self.perform("write", Phase::StageIntent, Some(role), |this| {
            this.world[role as usize].staged = Some(image(role));
            this.world[role as usize].partial = false;
        })?;
        self.effect("flush", Phase::StageIntent, Some(role))?;
        Ok(image(role))
    }
    fn observe_original(&mut self, role: PayloadRole) -> NativeResult<OriginalLeaf> {
        self.before("observe-original")?;
        self.after()?;
        Ok(if self.world[role as usize].old_present {
            OriginalLeaf::Present(FileStamp {
                volume: 1,
                file: [9; 16],
            })
        } else {
            OriginalLeaf::Missing
        })
    }
    fn backup(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<Option<FileStamp>> {
        self.perform("backup", Phase::BackupIntent, Some(role), |this| {
            let slot = &mut this.world[role as usize];
            assert!(slot.old_present);
            slot.old_present = false;
            slot.backup = Some(FileStamp {
                volume: 1,
                file: [9; 16],
            });
        })?;
        // Old bytes/ACL/link targets never become pins or execute/read candidates.
        let _opaque_kind = self.old;
        Ok(Some(FileStamp {
            volume: 1,
            file: [9; 16],
        }))
    }
    fn publish(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<ImageObservation> {
        self.perform("publish", Phase::PublishIntent, Some(role), |this| {
            let slot = &mut this.world[role as usize];
            assert!(!slot.old_present && slot.fixed_new.is_none());
            slot.fixed_new = slot.staged.take();
            assert!(slot.fixed_new.is_some());
        })?;
        Ok(image(role))
    }
    fn verify(&mut self, op: [u8; 16]) -> NativeResult<Self::Verified> {
        self.before("verify")?;
        self.after()?;
        VerifiedPayload::fixture_with_pins(
            op,
            pins(),
            FileIdentity {
                volume: 1,
                file: [2; 16],
            },
        )
    }
    fn start(&mut self, op: [u8; 16], payload: &Self::Verified) -> NativeResult<Self::Started> {
        self.perform("start", Phase::StartIntent, None, |this| {
            assert!(!this.started);
            this.started = true;
        })?;
        assert_eq!(op, payload.operation());
        NewInstanceEvidence::fixture(
            op,
            self.next,
            FileIdentity {
                volume: 1,
                file: [if self.wrong_image { 8 } else { 2 }; 16],
            },
        )
    }
    fn health(
        &mut self,
        op: [u8; 16],
        new: &Self::Started,
        payload: &Self::Verified,
    ) -> NativeResult<String> {
        self.before("health")?;
        self.after()?;
        let stop = UpgradeStopProof::fixture(op, self.original)?;
        Ok(format!(
            "{:032x}",
            health::check_instance(&stop, new, payload)?
        ))
    }
    fn prune(&mut self, _: [u8; 16]) -> NativeResult<bool> {
        self.effect("prune", Phase::PruneIntent, None)?;
        Ok(!self.incomplete)
    }
}
#[test]
fn wrong_stale_link_and_directory_leaves_are_backed_up_without_target_reads() {
    for old in [
        OldLeaf::Wrong,
        OldLeaf::Stale,
        OldLeaf::Link,
        OldLeaf::Hardlink,
        OldLeaf::Directory,
        OldLeaf::CaseVaried,
    ] {
        let mut port = Fake {
            old,
            ..Default::default()
        };
        let mut op = OperationRecord::new([1; 16]).unwrap();
        staging::apply(&mut port, &mut op).unwrap();
        assert_eq!(port.effects.iter().filter(|e| **e == "backup").count(), 4);
        assert_eq!(port.target_reads, 0);
        assert_eq!(op.phase(), Phase::Complete);
    }
}
#[test]
fn loaded_image_requires_clean_stop_and_positive_release_before_any_file_effect() {
    let mut port = Fake {
        loaded: true,
        ..Default::default()
    };
    let mut op = OperationRecord::new([1; 16]).unwrap();
    assert_eq!(staging::apply(&mut port, &mut op), Err(NativeError::Busy));
    assert_eq!(op.phase(), Phase::StopIntent);
    assert_eq!(port.effects, vec!["stop"]);
    assert_eq!(
        recovery_decision(port.durable.as_ref().unwrap()),
        RecoveryDecision::RecoveryRequired
    );
    port.loaded = false;
    assert_eq!(
        staging::apply(&mut port, &mut op),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(port.effects, vec!["stop"]);
}
#[test]
fn interruption_before_and_after_every_production_phase_never_retries() {
    let mut baseline = Fake::default();
    let mut complete = OperationRecord::new([1; 16]).unwrap();
    staging::apply(&mut baseline, &mut complete).unwrap();
    for call in 1..=baseline.count {
        for after in [false, true] {
            let mut port = Fake {
                fault: Some((call, after)),
                ..Default::default()
            };
            let mut record = OperationRecord::new([1; 16]).unwrap();
            assert_eq!(
                staging::apply(&mut port, &mut record),
                Err(NativeError::OutcomeUnknown),
                "call {call} after {after}"
            );
            let effects = port.effects.clone();
            let count = port.count;
            assert!(staging::apply(&mut port, &mut record).is_err());
            assert_eq!(port.count, count);
            assert_eq!(port.effects, effects);
            assert!(
                port.effects
                    .iter()
                    .filter(|event| **event == "start")
                    .count()
                    <= 1
            );
        }
    }
}
#[test]
fn uncertain_start_reopen_only_verifies_and_never_starts_again() {
    let mut baseline = Fake::default();
    let mut record = OperationRecord::new([1; 16]).unwrap();
    staging::apply(&mut baseline, &mut record).unwrap();
    let call = baseline.events.iter().position(|e| *e == "start").unwrap() + 1;
    let mut port = Fake {
        fault: Some((call, true)),
        ..Default::default()
    };
    let mut record = OperationRecord::new([1; 16]).unwrap();
    assert!(staging::apply(&mut port, &mut record).is_err());
    assert_eq!(
        recovery_decision(port.durable.as_ref().unwrap()),
        RecoveryDecision::VerifyOnly
    );
    assert_eq!(port.effects.iter().filter(|e| **e == "start").count(), 1);
    assert!(staging::apply(&mut port, &mut record).is_err());
    assert_eq!(port.effects.iter().filter(|e| **e == "start").count(), 1);
}
#[test]
fn new_exact_image_and_distinct_u64_instance_are_required() {
    for (next, wrong_image) in [(41, false), (42, true)] {
        let mut port = Fake {
            next,
            wrong_image,
            ..Default::default()
        };
        let mut record = OperationRecord::new([1; 16]).unwrap();
        assert_eq!(
            staging::apply(&mut port, &mut record),
            Err(NativeError::Foreign)
        );
        assert_eq!(record.phase(), Phase::StartIntent);
        assert!(!port.effects.contains(&"prune"));
    }
    let mut port = Fake::default();
    let mut record = OperationRecord::new([1; 16]).unwrap();
    staging::apply(&mut port, &mut record).unwrap();
    assert_eq!(
        record.new_instance(),
        Some("0000000000000000000000000000002a")
    );
}
#[test]
fn keep_three_completed_generations_preserves_active_and_unremovable() {
    let catalog:StageCatalog=serde_json::from_value(serde_json::json!({"schema_version":1,"active":vec![7;16],"generations":(1u8..=7).map(|n|serde_json::json!({"operation":vec![n;16],"sequence":n,"completed":n!=2})).collect::<Vec<_>>()})).unwrap();
    assert_eq!(
        prune_candidates(&catalog).unwrap(),
        vec![[4; 16], [3; 16], [1; 16]]
    );
    let mut port = Fake {
        incomplete: true,
        ..Default::default()
    };
    let mut record = OperationRecord::new([1; 16]).unwrap();
    staging::apply(&mut port, &mut record).unwrap();
    assert!(record.retention_incomplete());
    assert_eq!(record.phase(), Phase::Complete);
}
#[test]
fn malformed_journal_is_not_a_recovery_capability() {
    let record = OperationRecord::new([1; 16]).unwrap();
    let mut v = serde_json::to_value(record).unwrap();
    v["roles"][1] = v["roles"][0].clone();
    let bad: OperationRecord = serde_json::from_value(v).unwrap();
    assert_eq!(bad.validate(), Err(NativeError::Invalid));
    let record = OperationRecord::new([1; 16]).unwrap();
    let mut v = serde_json::to_value(record).unwrap();
    v["unexpected"] = serde_json::json!(1);
    assert!(serde_json::from_value::<OperationRecord>(v).is_err());
}
#[test]
fn production_upgrade_port_remains_unsupported_without_retained_old_owner() {
    let mut port = service::NativeUpgradePort::new();
    let verified = VerifiedPayload::fixture([1; 16]).unwrap();
    assert_eq!(
        port.stop_for_replace([1; 16]).err(),
        Some(NativeError::Unsupported)
    );
    assert_eq!(
        port.start_once([1; 16], &verified).err(),
        Some(NativeError::Unsupported)
    );
}

impl RecoveryPort for Fake {
    fn recover_stop(&mut self, record: &OperationRecord) -> NativeResult<Self::Stop> {
        self.before("recover-stop")?;
        self.after()?;
        // A separate fresh-world observation, not a proof reconstructed from journal claims.
        if !self.stopped {
            return Err(NativeError::Unsupported);
        }
        UpgradeStopProof::fixture(record.operation(), self.original)
    }
    fn observe_role(
        &mut self,
        _: &OperationRecord,
        role: PayloadRole,
    ) -> NativeResult<ReopenedRole> {
        self.before("reopen-role")?;
        self.after()?;
        let physical = &self.world[role as usize];
        Ok(ReopenedRole {
            staged: if physical.partial {
                StageObservation::Unknown
            } else {
                physical
                    .staged
                    .clone()
                    .map(StageObservation::Ready)
                    .unwrap_or(StageObservation::Missing)
            },
            fixed: if let Some(new) = &physical.fixed_new {
                FixedObservation::Published(new.clone())
            } else if physical.old_present {
                FixedObservation::Original(FileStamp {
                    volume: 1,
                    file: [9; 16],
                })
            } else {
                FixedObservation::Missing
            },
            backup: physical.backup,
            unknown_backup: false,
        })
    }
    fn recover_started(
        &mut self,
        record: &OperationRecord,
        payload: &Self::Verified,
    ) -> NativeResult<Option<Self::Started>> {
        self.before("recover-started")?;
        self.after()?;
        if !self.started {
            return Ok(None);
        }
        Ok(Some(NewInstanceEvidence::fixture(
            record.operation(),
            self.next,
            payload.agent_identity()?,
        )?))
    }
    fn settle_stage(
        &mut self,
        _: &OperationRecord,
        role: PayloadRole,
    ) -> NativeResult<ImageObservation> {
        self.effect("flush-reopened-stage", Phase::StageIntent, Some(role))?;
        self.world[role as usize]
            .staged
            .clone()
            .ok_or(NativeError::Unavailable)
    }
    fn rollback_stage(&mut self, _: &OperationRecord) -> NativeResult<RollbackOutcome> {
        if self.world.iter().any(|slot| slot.partial) {
            return Ok(RollbackOutcome::Retained);
        }
        self.perform("rollback-stage", Phase::RollbackIntent, None, |this| {
            for slot in &mut this.world {
                slot.staged = None
            }
        })?;
        Ok(RollbackOutcome::RolledBack)
    }
}
#[test]
fn fresh_reopen_at_every_before_after_effect_converges_or_retains_unknown_without_replay() {
    let mut baseline = Fake::default();
    let mut record = OperationRecord::new([1; 16]).unwrap();
    staging::apply(&mut baseline, &mut record).unwrap();
    for call in 1..=baseline.count {
        for after in [false, true] {
            let mut interrupted = Fake {
                fault: Some((call, after)),
                ..Default::default()
            };
            let mut memory = OperationRecord::new([1; 16]).unwrap();
            assert!(staging::apply(&mut interrupted, &mut memory).is_err());
            let Some(mut durable) = interrupted.durable.clone() else {
                continue;
            };
            let old_starts = interrupted
                .effects
                .iter()
                .filter(|e| **e == "start")
                .count();
            let old_stops = interrupted.effects.iter().filter(|e| **e == "stop").count();
            let mut reopened = Fake {
                world: interrupted.world.clone(),
                stopped: interrupted.stopped,
                started: interrupted.started,
                ..Default::default()
            };
            reopened.durable = Some(durable.clone());
            let result = resume(&mut reopened, &mut durable).unwrap();
            assert!(
                !reopened.effects.contains(&"stop"),
                "stop replay at{call}/{after}"
            );
            assert!(
                old_starts + reopened.effects.iter().filter(|e| **e == "start").count() <= 1,
                "start replay at{call}/{after}"
            );
            assert!(old_stops <= 1);
            match result {
                RecoveryDecision::Complete => {
                    if durable.phase() == Phase::RolledBack {
                        assert!(reopened.world.iter().all(|slot| slot.staged.is_none()));
                        assert!(!reopened.started)
                    } else {
                        assert!(reopened.world.iter().all(|slot| slot.fixed_new.is_some()));
                        assert!(reopened.started)
                    }
                }
                RecoveryDecision::RecoveryRequired => assert!(
                    reopened.effects.is_empty(),
                    "unknown must preserve material at{call}/{after}"
                ),
                other => panic!("unconsumed recovery classification {other:?}"),
            }
        }
    }
}
#[test]
fn reopened_completed_write_is_flushed_under_new_intent_before_adoption() {
    let mut port = Fake {
        stopped: true,
        ..Default::default()
    };
    let mut record = OperationRecord::new([1; 16]).unwrap();
    record.set_phase(Phase::StageIntent);
    port.world[0].staged = Some(image(PayloadRole::Installer));
    port.durable = Some(record.clone());
    assert_eq!(
        resume(&mut port, &mut record).unwrap(),
        RecoveryDecision::Complete
    );
    assert!(port.effects.contains(&"flush-reopened-stage"));
    assert!(!port.effects.contains(&"stop"));
}
#[test]
fn partial_stage_and_unproved_stop_are_retained_on_fresh_reopen() {
    let mut record = OperationRecord::new([1; 16]).unwrap();
    record.set_phase(Phase::StageIntent);
    let mut port = Fake {
        stopped: true,
        ..Default::default()
    };
    port.world[0].partial = true;
    port.durable = Some(record.clone());
    assert_eq!(
        resume(&mut port, &mut record).unwrap(),
        RecoveryDecision::RecoveryRequired
    );
    assert!(port.effects.is_empty());
    let mut port = Fake {
        durable: Some(record.clone()),
        ..Default::default()
    };
    assert_eq!(
        resume(&mut port, &mut record).unwrap(),
        RecoveryDecision::RecoveryRequired
    );
    assert!(port.effects.is_empty());
}
#[test]
fn newest_active_is_counted_in_three_and_old_active_is_preserved_outside_quota() {
    for active in [1u8, 4u8] {
        let catalog:StageCatalog=serde_json::from_value(serde_json::json!({"schema_version":1,"active":vec![active;16],"generations":(1u8..=4).map(|n|serde_json::json!({"operation":vec![n;16],"sequence":n,"completed":true})).collect::<Vec<_>>()})).unwrap();
        assert_eq!(
            prune_candidates(&catalog).unwrap(),
            if active == 4 { vec![[1; 16]] } else { vec![] }
        );
    }
}
#[test]
fn successful_prune_catalog_retirement_does_not_accumulate_deleted_generations() {
    let mut catalog: StageCatalog = serde_json::from_value(
        serde_json::json!({"schema_version":1,"active":null,"generations":[]}),
    )
    .unwrap();
    for sequence in 1u64..=1100 {
        let mut op = [0; 16];
        op[..8].copy_from_slice(&sequence.to_le_bytes());
        catalog.active = Some(op);
        catalog.generations.push(BackupGeneration {
            operation: op,
            sequence,
            completed: true,
        });
        let deleted = prune_candidates(&catalog).unwrap();
        // The native adapter can do this only after opaque PrunedGeneration + fresh absence.
        for operation in deleted {
            retire_completed(&mut catalog, operation).unwrap()
        }
        catalog.active = None;
        catalog.validate().unwrap();
        assert!(catalog.generations.len() <= 3);
    }
}

#[test]
fn catalog_retirement_refuses_active_or_newest_generation_without_eligible_prune() {
    let mut catalog:StageCatalog=serde_json::from_value(serde_json::json!({"schema_version":1,"active":vec![4;16],"generations":(1u8..=4).map(|n|serde_json::json!({"operation":vec![n;16],"sequence":n,"completed":true})).collect::<Vec<_>>()})).unwrap();
    for id in [2, 3, 4] {
        assert_eq!(
            retire_completed(&mut catalog, [id; 16]),
            Err(NativeError::Foreign)
        )
    }
    retire_completed(&mut catalog, [1; 16]).unwrap();
    assert_eq!(catalog.generations.len(), 3);
}
