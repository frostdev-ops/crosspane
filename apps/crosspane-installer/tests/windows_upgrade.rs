#![allow(dead_code, unused_imports, clippy::unwrap_used, clippy::expect_used)]
//! Pure production-driver interruption fixtures. No task, process, root, handle or native path
//! authority is constructed by these fakes, and no helper process is launched.
use crosspane_installer::agent_contract;
#[path = "../src/platform/windows/detect.rs"]
mod detect;
#[path = "../src/platform/windows/native_io.rs"]
mod native_io;
#[path = "../src/platform/windows/payload.rs"]
mod payload;
#[path = "../src/platform/windows/service.rs"]
mod service;
#[cfg(any(windows, test))]
#[path = "../src/platform/windows/transport.rs"]
mod transport;

use native_io::{NativeError, NativeResult};
use payload::helper::*;
use payload::inventory::PeFacts;
use payload::recovery::{OperationRecord, Phase, RecoveryDecision};
use std::ffi::OsString;

fn handoff() -> HandoffRecord {
    HandoffRecord {
        inherited_parent_handle: 64,
        parent_pid: 101,
        parent_created: 200,
        parent_image: "C:\\fixture\\crosspane-installer.exe".into(),
        installer_size: 4096,
        installer_sha256: [17; 32],
        installer_machine: 0x8664,
        installer_subsystem: 2,
        stage: HandoffStage::CreateIntent,
    }
}
fn image() -> PeFacts {
    PeFacts {
        size: 4096,
        sha256: [17; 32],
        machine: 0x8664,
        subsystem: 2,
        version: "fixture-1".into(),
    }
}
fn operation(stage: HandoffStage, phase: Phase) -> OperationRecord {
    let mut value = OperationRecord::new([7; 16]).unwrap();
    let mut h = handoff();
    h.stage = stage;
    value.set_handoff(h).unwrap();
    value.set_phase(phase);
    value
}

#[test]
fn helper_argument_is_exact_only_and_extra_arguments_never_reach_gui() {
    let flag = OsString::from(REPLACE_HELPER_ARGUMENT);
    assert_eq!(replace_helper_mode(std::slice::from_ref(&flag)), Ok(true));
    assert_eq!(replace_helper_mode(&[]), Ok(false));
    assert_eq!(
        replace_helper_mode(&[OsString::from("--diagnose")]),
        Ok(false)
    );
    for arguments in [
        vec![flag.clone(), OsString::from("C:\\foreign")],
        vec![flag.clone(), flag.clone()],
        vec![OsString::from("--supervisor"), flag],
    ] {
        assert_eq!(replace_helper_mode(&arguments), Err(NativeError::Invalid));
    }
}
#[test]
fn helper_catalog_rejects_unknown_zero_pseudo_and_path_authority() {
    let mut json = serde_json::to_value(handoff()).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("operation_argv".into(), serde_json::json!("foreign"));
    assert!(serde_json::from_value::<HandoffRecord>(json).is_err());
    for handle in [0, 1, u64::MAX, u64::MAX - 3] {
        let mut h = handoff();
        h.inherited_parent_handle = handle;
        assert!(h.validate().is_err());
    }
    for path in [
        "relative.exe",
        "C:\\fixture\\..\\foreign.exe",
        "C:\\fixture\\bad:stream",
        "C:\\fixture\\bad.",
    ] {
        let mut h = handoff();
        h.parent_image = path.into();
        assert!(h.validate().is_err());
    }
}
#[test]
fn helper_actual_parent_must_match_creation_image_and_not_self_even_if_already_exited() {
    let h = handoff();
    let facts = || ParentFacts {
        pid: h.parent_pid,
        created: h.parent_created,
        image: h.parent_image.clone(),
        inheritable: true,
    };
    assert_eq!(parent_matches(&h, &facts(), 300), Ok(()));
    for mutation in 0..5 {
        let mut f = facts();
        match mutation {
            0 => f.pid += 1,
            1 => f.created += 1,
            2 => f.image = "C:\\foreign.exe".into(),
            3 => f.inheritable = false,
            _ => f.pid = 0,
        };
        assert_eq!(parent_matches(&h, &f, 300), Err(NativeError::Foreign));
    }
    assert_eq!(
        parent_matches(&h, &facts(), h.parent_pid),
        Err(NativeError::Foreign)
    );
}
#[test]
fn helper_self_measurement_matches_catalog_only_as_correlation() {
    assert_eq!(handoff().matches_image(&image()), Ok(()));
    for mutation in 0..4 {
        let mut actual = image();
        match mutation {
            0 => actual.size += 1,
            1 => actual.sha256[0] += 1,
            2 => actual.machine = 0xaa64,
            _ => actual.subsystem = 3,
        };
        assert_eq!(handoff().matches_image(&actual), Err(NativeError::Foreign));
    }
}

#[derive(Default)]
struct FakeHandoff {
    events: Vec<&'static str>,
    fail: Option<&'static str>,
    creates: u32,
    resumes: u32,
    cancels: u32,
    retired: bool,
}
impl FakeHandoff {
    fn step(&mut self, label: &'static str) -> NativeResult<()> {
        self.events.push(label);
        if self.fail == Some(label) {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
}
impl HandoffPort for FakeHandoff {
    type Child = ();
    fn prepare(&mut self) -> NativeResult<HandoffRecord> {
        if self.retired {
            return Err(NativeError::OutcomeUnknown);
        }
        self.step("prepare")?;
        Ok(handoff())
    }
    fn save(&mut self, r: &HandoffRecord) -> NativeResult<()> {
        let label = match r.stage {
            HandoffStage::CreateIntent => "create-intent",
            HandoffStage::ChildCreated => "child-result",
            HandoffStage::ResumeIntent => "resume-intent",
            HandoffStage::Dispatched => "resume-result",
        };
        self.step(label)
    }
    fn create_suspended(&mut self, _: &HandoffRecord) -> NativeResult<()> {
        self.step("before-create")?;
        self.creates += 1;
        self.events.push("create");
        self.step("after-create")
            .map_err(|_| NativeError::OutcomeUnknown)
    }
    fn resume(&mut self, _: &mut ()) -> NativeResult<()> {
        self.step("before-resume")?;
        self.resumes += 1;
        self.events.push("resume");
        self.step("after-resume")
    }
    fn cancel_suspended(&mut self, _: &mut ()) -> NativeResult<()> {
        self.cancels += 1;
        self.step("cancel")
    }
    fn retire(&mut self) {
        self.retired = true;
        self.events.push("retire")
    }
}
#[test]
fn helper_handoff_journals_each_native_effect_and_leaves_copy_for_reopen() {
    let mut f = FakeHandoff::default();
    assert!(matches!(
        drive_handoff(&mut f),
        Ok(HandoffOutcome::Dispatched(()))
    ));
    assert_eq!(
        f.events,
        [
            "prepare",
            "create-intent",
            "before-create",
            "create",
            "after-create",
            "child-result",
            "resume-intent",
            "before-resume",
            "resume",
            "after-resume",
            "resume-result"
        ]
    );
    assert_eq!((f.creates, f.resumes, f.cancels), (1, 1, 0));
    assert!(!f.events.contains(&"delete-helper-copy"));
}
#[test]
fn helper_handoff_interruptions_before_after_effect_and_result_never_replay() {
    for failed in [
        "prepare",
        "create-intent",
        "before-create",
        "after-create",
        "child-result",
        "resume-intent",
        "before-resume",
        "after-resume",
        "resume-result",
    ] {
        let mut f = FakeHandoff {
            fail: Some(failed),
            ..Default::default()
        };
        let _ = drive_handoff(&mut f);
        assert!(f.creates <= 1 && f.resumes <= 1);
        if matches!(failed, "prepare" | "create-intent" | "before-create") {
            assert_eq!((f.creates, f.resumes), (0, 0));
        }
        if matches!(failed, "child-result" | "resume-intent") {
            assert_eq!((f.resumes, f.cancels), (0, 1));
        }
        if matches!(failed, "before-resume" | "after-resume" | "resume-result") {
            assert_eq!(f.cancels, 0);
            assert!(f.retired);
        }
        if f.retired {
            let counts = (f.creates, f.resumes);
            assert!(matches!(
                drive_handoff(&mut f),
                Err(NativeError::OutcomeUnknown)
            ));
            assert_eq!((f.creates, f.resumes), counts);
        }
    }
}
#[test]
fn helper_unknown_suspended_cleanup_retains_original_child_owner() {
    let mut f = FakeHandoff {
        fail: Some("child-result"),
        ..Default::default()
    };
    // The exact cleanup can independently fail. It cannot claim settlement or dispatch resume.
    struct CleanupFails(FakeHandoff);
    impl HandoffPort for CleanupFails {
        type Child = ();
        fn prepare(&mut self) -> NativeResult<HandoffRecord> {
            self.0.prepare()
        }
        fn save(&mut self, r: &HandoffRecord) -> NativeResult<()> {
            self.0.save(r)
        }
        fn create_suspended(&mut self, r: &HandoffRecord) -> NativeResult<()> {
            self.0.create_suspended(r)
        }
        fn resume(&mut self, c: &mut ()) -> NativeResult<()> {
            self.0.resume(c)
        }
        fn cancel_suspended(&mut self, _: &mut ()) -> NativeResult<()> {
            self.0.cancels += 1;
            Err(NativeError::OutcomeUnknown)
        }
        fn retire(&mut self) {
            self.0.retire()
        }
    }
    let mut wrapped = CleanupFails(std::mem::take(&mut f));
    assert!(matches!(
        drive_handoff(&mut wrapped),
        Ok(HandoffOutcome::RecoveryRetained(()))
    ));
    assert_eq!(wrapped.0.resumes, 0);
    assert!(wrapped.0.retired);
}

struct FakeEntry {
    selected: Option<OperationRecord>,
    current: Option<OperationRecord>,
    events: Vec<&'static str>,
    fail: Option<&'static str>,
    decision: NativeResult<RecoveryDecision>,
    already_exited: bool,
}
impl FakeEntry {
    fn new() -> Self {
        Self {
            selected: Some(operation(HandoffStage::Dispatched, Phase::HelperDispatched)),
            current: Some(operation(HandoffStage::Dispatched, Phase::HelperDispatched)),
            events: Vec::new(),
            fail: None,
            decision: Ok(RecoveryDecision::Complete),
            already_exited: false,
        }
    }
    fn step(&mut self, label: &'static str) -> NativeResult<()> {
        self.events.push(label);
        if self.fail == Some(label) {
            Err(NativeError::Unavailable)
        } else {
            Ok(())
        }
    }
}
impl HelperEntryPort for FakeEntry {
    type Parent = ();
    fn select(&mut self) -> NativeResult<Option<OperationRecord>> {
        self.step("select")?;
        Ok(self.selected.clone())
    }
    fn verify_self(&mut self, r: &HandoffRecord) -> NativeResult<()> {
        self.step("self-measure")?;
        r.matches_image(&image())
    }
    fn inherited_parent(&mut self, r: &HandoffRecord) -> NativeResult<()> {
        self.step("verify-original-parent")?;
        parent_matches(
            r,
            &ParentFacts {
                pid: 101,
                created: 200,
                image: handoff().parent_image,
                inheritable: true,
            },
            300,
        )
    }
    fn wait_parent(&mut self, op: [u8; 16], _: ()) -> NativeResult<ParentExited> {
        self.step(if self.already_exited {
            "already-exited"
        } else {
            "wait-original-parent"
        })?;
        ParentExited::fixture(op)
    }
    fn lock_and_reselect(&mut self) -> NativeResult<Option<OperationRecord>> {
        self.step("acquire-lock-reselect")?;
        Ok(self.current.clone())
    }
    fn resume(&mut self, r: OperationRecord, p: &ParentExited) -> NativeResult<RecoveryDecision> {
        self.step("resume-recovery")?;
        assert_eq!(r.operation(), p.operation());
        self.decision
    }
}
#[test]
fn helper_verifies_then_waits_parent_before_lock_and_selected_recovery() {
    let mut f = FakeEntry::new();
    assert_eq!(drive_entry(&mut f), Ok(HelperExit::HandoffCompleted));
    assert_eq!(
        f.events,
        [
            "select",
            "self-measure",
            "verify-original-parent",
            "wait-original-parent",
            "acquire-lock-reselect",
            "resume-recovery"
        ]
    );
}
#[test]
fn helper_early_parent_exit_does_not_skip_actual_handle_verification() {
    let mut f = FakeEntry::new();
    f.already_exited = true;
    assert_eq!(drive_entry(&mut f), Ok(HelperExit::HandoffCompleted));
    assert!(
        f.events
            .iter()
            .position(|s| *s == "verify-original-parent")
            .unwrap()
            < f.events
                .iter()
                .position(|s| *s == "already-exited")
                .unwrap()
    );
    let mut f = FakeEntry::new();
    f.already_exited = true;
    f.fail = Some("verify-original-parent");
    assert!(drive_entry(&mut f).is_err());
    assert!(!f.events.contains(&"acquire-lock-reselect"));
}
#[test]
fn helper_entry_interruptions_never_replay_stop_or_start_intent() {
    for failed in [
        "select",
        "self-measure",
        "verify-original-parent",
        "wait-original-parent",
        "acquire-lock-reselect",
        "resume-recovery",
    ] {
        let mut f = FakeEntry::new();
        f.fail = Some(failed);
        assert!(drive_entry(&mut f).is_err());
        assert!(!f.events.contains(&"stop") && !f.events.contains(&"start"));
    }
    for phase in [Phase::StopIntent, Phase::StartIntent, Phase::Unknown] {
        let mut f = FakeEntry::new();
        f.selected = Some(operation(HandoffStage::Dispatched, phase));
        assert_eq!(drive_entry(&mut f), Err(NativeError::Unsupported));
        assert_eq!(f.events, ["select"]);
    }
}
#[test]
fn helper_reselects_same_dispatched_operation_and_refuses_unknown_resume_result() {
    let mut f = FakeEntry::new();
    f.selected = Some(operation(HandoffStage::ResumeIntent, Phase::HandoffIntent));
    assert_eq!(drive_entry(&mut f), Ok(HelperExit::HandoffCompleted));
    for altered in [
        None,
        Some(operation(HandoffStage::ResumeIntent, Phase::HandoffIntent)),
    ] {
        let mut f = FakeEntry::new();
        f.current = altered;
        assert_eq!(drive_entry(&mut f), Ok(HelperExit::RecoveryRetained));
        assert!(!f.events.contains(&"resume-recovery"));
    }
    for decision in [
        Ok(RecoveryDecision::RecoveryRequired),
        Err(NativeError::Unsupported),
        Err(NativeError::OutcomeUnknown),
    ] {
        let mut f = FakeEntry::new();
        f.decision = decision;
        assert_eq!(drive_entry(&mut f), Ok(HelperExit::RecoveryRetained));
    }
}
#[test]
fn helper_parent_exit_is_not_old_agent_job_or_clean_stop_proof() {
    let mut f = FakeEntry::new();
    f.decision = Err(NativeError::Unsupported);
    assert_eq!(drive_entry(&mut f), Ok(HelperExit::RecoveryRetained));
    assert!(
        !f.events.contains(&"stop")
            && !f.events.contains(&"start")
            && !f.events.contains(&"delete-helper-copy")
    );
}

#[test]
fn production_upgrade_stop_port_remains_unsupported_without_a4b_completion() {
    use payload::health::ServicePort;
    let mut port = service::NativeUpgradePort::new();
    assert!(matches!(
        port.stop_for_replace([7; 16]),
        Err(NativeError::Unsupported)
    ));
}
#[test]
fn production_upgrade_start_port_remains_unsupported_before_any_pin_or_start_effect() {
    use payload::health::{ServicePort, VerifiedPayload};
    let mut port = service::NativeUpgradePort::new();
    let payload = VerifiedPayload::fixture([7; 16]).unwrap();
    assert!(matches!(
        port.start_once([7; 16], &payload),
        Err(NativeError::Unsupported)
    ));
}

use native_io::files::FileIdentity;
use payload::inventory::{ApprovedPe, PayloadRole};
use payload::recovery::{FileStamp, ImageObservation};
const OLD_INSTANCE: u64 = 9_007_199_254_741_111;
const NEW_INSTANCE: u64 = 9_007_199_254_741_113;
#[derive(Clone, Debug, PartialEq, Eq)]
enum Effect {
    Journal(Phase, Option<PayloadRole>),
    Stop,
    Released,
    Stage(PayloadRole),
    Backup(PayloadRole),
    Publish(PayloadRole),
    Verify,
    Start,
    Health,
    Prune,
}
struct FakeUpgrade {
    events: Vec<Effect>,
    durable: Option<OperationRecord>,
    checkpoint: usize,
    fail_at: Option<usize>,
    stops: u32,
    starts: u32,
    loaded_old_image: bool,
    prunable: bool,
    new_instance: u64,
    wrong_new_image: bool,
}
impl FakeUpgrade {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            durable: None,
            checkpoint: 0,
            fail_at: None,
            stops: 0,
            starts: 0,
            loaded_old_image: true,
            prunable: true,
            new_instance: NEW_INSTANCE,
            wrong_new_image: false,
        }
    }
    fn boundary(&mut self) -> NativeResult<()> {
        let at = self.checkpoint;
        self.checkpoint += 1;
        if self.fail_at == Some(at) {
            Err(NativeError::OutcomeUnknown)
        } else {
            Ok(())
        }
    }
    fn before(&mut self, effect: Effect) -> NativeResult<()> {
        self.boundary()?;
        self.events.push(effect);
        Ok(())
    }
    fn after(&mut self) -> NativeResult<()> {
        self.boundary()
    }
    fn observation(role: PayloadRole) -> ImageObservation {
        ImageObservation {
            identity: FileStamp {
                volume: 7,
                file: [role as u8 + 1; 16],
            },
            facts: image(),
        }
    }
    fn expect_intent(&self, phase: Phase, role: Option<PayloadRole>) {
        let record = self.durable.as_ref().unwrap();
        assert_eq!(record.phase(), phase);
        assert_eq!(record.current_role(), role);
    }
}
impl payload::staging::PayloadPort for FakeUpgrade {
    type Stop = service::UpgradeStopProof;
    type Verified = payload::health::VerifiedPayload;
    type Started = service::NewInstanceEvidence;
    fn journal(&mut self, r: &OperationRecord) -> NativeResult<()> {
        self.before(Effect::Journal(r.phase(), r.current_role()))?;
        self.durable = Some(r.clone());
        self.after()
    }
    fn stop(&mut self, operation: [u8; 16]) -> NativeResult<Self::Stop> {
        self.expect_intent(Phase::StopIntent, None);
        self.before(Effect::Stop)?;
        self.stops += 1;
        // Only this authored fake's positively completed stop releases the fake loaded-image flag.
        // Production NativeUpgradePort has no equivalent factory and remains Unsupported.
        self.loaded_old_image = false;
        let proof = service::UpgradeStopProof::fixture(operation, Some(OLD_INSTANCE))?;
        self.after()?;
        Ok(proof)
    }
    fn original_instance(&self, stop: &Self::Stop) -> Option<u64> {
        stop.original_instance()
    }
    fn released(&mut self, stop: &Self::Stop) -> NativeResult<()> {
        self.before(Effect::Released)?;
        if stop.operation() != [7; 16] || self.loaded_old_image {
            return Err(NativeError::Busy);
        }
        self.after()
    }
    fn stage(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<ImageObservation> {
        self.expect_intent(Phase::StageIntent, Some(role));
        assert!(!self.loaded_old_image);
        self.before(Effect::Stage(role))?;
        let observation = Self::observation(role);
        self.after()?;
        Ok(observation)
    }
    fn observe_original(
        &mut self,
        role: PayloadRole,
    ) -> NativeResult<payload::recovery::OriginalLeaf> {
        Ok(payload::recovery::OriginalLeaf::Present(FileStamp {
            volume: 7,
            file: [role as u8 + 11; 16],
        }))
    }
    fn backup(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<Option<FileStamp>> {
        self.expect_intent(Phase::BackupIntent, Some(role));
        self.before(Effect::Backup(role))?;
        let identity = FileStamp {
            volume: 7,
            file: [role as u8 + 11; 16],
        };
        self.after()?;
        Ok(Some(identity))
    }
    fn publish(&mut self, _: [u8; 16], role: PayloadRole) -> NativeResult<ImageObservation> {
        self.expect_intent(Phase::PublishIntent, Some(role));
        self.before(Effect::Publish(role))?;
        let observation = Self::observation(role);
        self.after()?;
        Ok(observation)
    }
    fn verify(&mut self, operation: [u8; 16]) -> NativeResult<Self::Verified> {
        self.before(Effect::Verify)?;
        let value = payload::health::VerifiedPayload::fixture_with_pins(
            operation,
            PayloadRole::ALL
                .into_iter()
                .map(|r| ApprovedPe::fixture(r, image()))
                .collect(),
            FileIdentity {
                volume: 7,
                file: [PayloadRole::Agent as u8 + 1; 16],
            },
        )?;
        self.after()?;
        Ok(value)
    }
    fn start(
        &mut self,
        operation: [u8; 16],
        verified: &Self::Verified,
    ) -> NativeResult<Self::Started> {
        self.expect_intent(Phase::StartIntent, None);
        self.before(Effect::Start)?;
        self.starts += 1;
        assert_eq!(verified.operation(), operation);
        let actual = if self.wrong_new_image {
            FileIdentity {
                volume: 7,
                file: [99; 16],
            }
        } else {
            verified.agent_identity()?
        };
        let proof = service::NewInstanceEvidence::fixture(operation, self.new_instance, actual)?;
        self.after()?;
        Ok(proof)
    }
    fn health(
        &mut self,
        operation: [u8; 16],
        started: &Self::Started,
        verified: &Self::Verified,
    ) -> NativeResult<String> {
        self.before(Effect::Health)?;
        let stop = service::UpgradeStopProof::fixture(operation, Some(OLD_INSTANCE))?;
        let current = format!(
            "{:032x}",
            payload::health::check_instance(&stop, started, verified)?
        );
        self.after()?;
        Ok(current)
    }
    fn prune(&mut self, _: [u8; 16]) -> NativeResult<bool> {
        self.expect_intent(Phase::PruneIntent, None);
        self.before(Effect::Prune)?;
        self.after()?;
        Ok(self.prunable)
    }
}
#[test]
fn upgrade_actual_driver_stops_releases_replaces_once_starts_and_verifies_exact_instance() {
    let mut f = FakeUpgrade::new();
    let mut record = OperationRecord::new([7; 16]).unwrap();
    payload::staging::apply(&mut f, &mut record).unwrap();
    assert_eq!((f.stops, f.starts), (1, 1));
    assert_eq!(record.phase(), Phase::Complete);
    assert_eq!(
        record.original_instance(),
        Some(format!("{OLD_INSTANCE:032x}").as_str())
    );
    assert_eq!(
        record.new_instance(),
        Some(format!("{NEW_INSTANCE:032x}").as_str())
    );
    let at = |e| f.events.iter().position(|v| *v == e).unwrap();
    assert!(at(Effect::Stop) < at(Effect::Released));
    assert!(at(Effect::Released) < at(Effect::Stage(PayloadRole::Installer)));
    for role in PayloadRole::ALL {
        assert!(at(Effect::Stage(role)) < at(Effect::Backup(role)));
        assert!(at(Effect::Backup(role)) < at(Effect::Publish(role)));
        assert!(at(Effect::Publish(role)) < at(Effect::Verify));
    }
    assert!(at(Effect::Verify) < at(Effect::Start));
    assert!(at(Effect::Start) < at(Effect::Health));
    assert!(at(Effect::Health) < at(Effect::Prune));
}
#[test]
fn upgrade_interruption_before_after_every_effect_and_result_preserves_intent_without_replay() {
    let mut complete = FakeUpgrade::new();
    let mut record = OperationRecord::new([7; 16]).unwrap();
    payload::staging::apply(&mut complete, &mut record).unwrap();
    // Every production journal and effect has an authored before/after boundary. Inject all of
    // them, including stop, release, four stage/backup/publish roles, start, health and prune.
    for interruption in 0..complete.checkpoint {
        let mut f = FakeUpgrade::new();
        f.fail_at = Some(interruption);
        let mut record = OperationRecord::new([7; 16]).unwrap();
        assert!(payload::staging::apply(&mut f, &mut record).is_err());
        assert!(f.stops <= 1 && f.starts <= 1);
        if let Some(mut durable) = f.durable.clone() {
            if durable.phase() != Phase::Intent {
                let effects = f.events.len();
                let counts = (f.stops, f.starts);
                assert_eq!(
                    payload::staging::apply(&mut f, &mut durable),
                    Err(NativeError::OutcomeUnknown)
                );
                assert_eq!(f.events.len(), effects);
                assert_eq!((f.stops, f.starts), counts);
            } else {
                assert_eq!((f.stops, f.starts), (0, 0));
            }
        } else {
            assert_eq!((f.stops, f.starts), (0, 0));
        }
    }
}
#[test]
fn upgrade_new_image_or_reused_instance_refuses_and_reopen_never_starts_again() {
    for same_instance in [false, true] {
        let mut f = FakeUpgrade::new();
        f.wrong_new_image = !same_instance;
        if same_instance {
            f.new_instance = OLD_INSTANCE;
        }
        let mut record = OperationRecord::new([7; 16]).unwrap();
        assert_eq!(
            payload::staging::apply(&mut f, &mut record),
            Err(NativeError::Foreign)
        );
        assert_eq!(f.starts, 1);
        let mut durable = f.durable.clone().unwrap();
        assert_eq!(durable.phase(), Phase::StartIntent);
        assert_eq!(
            payload::recovery::recovery_decision(&durable),
            RecoveryDecision::VerifyOnly
        );
        assert_eq!(
            payload::staging::apply(&mut f, &mut durable),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(f.starts, 1);
    }
}
#[test]
fn upgrade_unremovable_generation_reports_incomplete_retention_without_force_delete() {
    let mut f = FakeUpgrade::new();
    f.prunable = false;
    let mut record = OperationRecord::new([7; 16]).unwrap();
    payload::staging::apply(&mut f, &mut record).unwrap();
    assert!(record.retention_incomplete());
    assert_eq!(record.phase(), Phase::Complete);
    assert_eq!((f.stops, f.starts), (1, 1));
}

#[test]
fn helper_late_create_stores_ownership_before_one_suspended_cleanup_claim() {
    let mut control = HandoffControl::prepare();
    control.claim_create(false).unwrap();
    assert!(!control.retire()); // actual child handles have not returned yet
    assert!(control.publish_created(true, true));
    assert_eq!(control.phase, ChildPhase::Settling);
    assert!(!control.retire()); // a second caller cannot also terminate/wait this child
    control.settle_observed(false);
    assert_eq!(control.phase, ChildPhase::Quarantined);
    assert_eq!(
        control.claim_create(false),
        Err(NativeError::OutcomeUnknown)
    );
}
#[test]
fn helper_delivery_late_after_create_can_settle_only_the_never_resumed_owner() {
    let mut control = HandoffControl::prepare();
    control.claim_create(false).unwrap();
    assert!(!control.publish_created(true, false));
    assert_eq!(control.phase, ChildPhase::Suspended);
    assert!(control.retire());
    assert!(!control.retire());
    assert_eq!(
        control.claim_resume(false),
        Err(NativeError::OutcomeUnknown)
    );
    control.settle_observed(true);
    assert_eq!(control.phase, ChildPhase::Settled);
}
#[test]
fn helper_retirement_and_resume_share_claim_and_any_ambiguous_resume_is_quarantined() {
    for previous in [0, 2, u32::MAX] {
        let mut control = HandoffControl::prepare();
        control.claim_create(false).unwrap();
        control.publish_created(true, false);
        control.claim_resume(false).unwrap();
        assert_eq!(
            control.resume_observed(previous, false),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(!control.retire());
        assert_eq!(control.phase, ChildPhase::Quarantined);
    }
    let mut control = HandoffControl::prepare();
    control.claim_create(false).unwrap();
    control.publish_created(true, false);
    control.claim_resume(false).unwrap();
    assert!(!control.retire());
    assert_eq!(
        control.resume_observed(1, true),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(control.phase, ChildPhase::Quarantined);
}
#[test]
fn helper_partial_native_outputs_or_poisoned_classification_never_claim_suspended_cleanup() {
    let mut control = HandoffControl::prepare();
    control.claim_create(false).unwrap();
    assert!(!control.publish_created(false, false));
    assert!(!control.retire());
    let mut control = HandoffControl::prepare();
    control.claim_create(false).unwrap();
    control.quarantine();
    assert!(!control.publish_created(true, false));
    assert!(!control.retire());
}

#[test]
fn owned_lock_handoff_drops_actual_lock_before_run_and_only_restores_fresh_permit() {
    use std::{cell::Cell, rc::Rc};
    struct Lock(Rc<Cell<bool>>);
    impl Drop for Lock {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }
    let held = Rc::new(Cell::new(true));
    let fresh = Rc::new(Cell::new(false));
    let mut boundary = payload::LockHandoff::Owned(Some(Lock(held.clone())));
    let mut permit = Some(7);
    let observed = Cell::new(0);
    let result = boundary.run_once(
        &mut permit,
        true,
        || {
            assert!(!held.get());
            observed.set(observed.get() + 1);
            Ok(41)
        },
        || {
            assert!(!held.get());
            fresh.set(true);
            Ok((Lock(fresh.clone()), 8))
        },
    );
    assert_eq!(result, Ok(41));
    assert_eq!(observed.get(), 1);
    assert_eq!(permit, Some(8));
    assert!(fresh.get());
    assert!(boundary.get().is_ok());
    drop(boundary);
    assert!(!fresh.get());
}
#[test]
fn owned_lock_handoff_borrowed_nonidle_and_missing_permit_never_run() {
    let lock = 1;
    let mut borrowed = payload::LockHandoff::Borrowed(&lock);
    let mut permit = Some(7);
    assert_eq!(
        borrowed.run_once(
            &mut permit,
            true,
            || panic!("borrowed must refuse"),
            || Ok((1, 8))
        ),
        Err::<(), _>(NativeError::Unsupported)
    );
    for (idle, expected, initial) in [
        (false, NativeError::OutcomeUnknown, Some(7)),
        (true, NativeError::Foreign, None),
    ] {
        let mut owned = payload::LockHandoff::Owned(Some(1));
        let mut permit = initial;
        assert_eq!(
            owned.run_once(&mut permit, idle, || panic!("no authority"), || Ok((1, 8))),
            Err::<(), _>(expected)
        );
        assert!(owned.get().is_ok());
        assert_eq!(permit, initial);
    }
}
#[test]
fn owned_lock_handoff_run_failure_never_reenters_or_repeats_run() {
    let mut boundary = payload::LockHandoff::Owned(Some(1));
    let mut permit = Some(7);
    let runs = std::cell::Cell::new(0);
    assert_eq!(
        boundary.run_once(
            &mut permit,
            true,
            || {
                runs.set(runs.get() + 1);
                Err::<(), _>(NativeError::OutcomeUnknown)
            },
            || panic!("failed Run cannot reenter")
        ),
        Err(NativeError::OutcomeUnknown)
    );
    assert_eq!(permit, None);
    assert!(boundary.get().is_err());
    assert_eq!(
        boundary.run_once(
            &mut permit,
            true,
            || panic!("no repeated Run"),
            || Ok((2, 8))
        ),
        Err::<(), _>(NativeError::Foreign)
    );
    assert_eq!(runs.get(), 1);
}
#[test]
fn owned_lock_handoff_changed_selection_or_failed_reacquire_never_publishes() {
    for error in [
        NativeError::Foreign,
        NativeError::Busy,
        NativeError::OutcomeUnknown,
    ] {
        let mut boundary = payload::LockHandoff::Owned(Some(1));
        let mut permit = Some(7);
        let runs = std::cell::Cell::new(0);
        let result = boundary.run_once(
            &mut permit,
            true,
            || {
                runs.set(runs.get() + 1);
                Ok(())
            },
            || Err::<(i32, i32), _>(error),
        );
        assert_eq!(result, Err(error));
        assert_eq!(permit, None);
        assert!(boundary.get().is_err());
        assert_eq!(
            boundary.run_once(
                &mut permit,
                true,
                || panic!("old StartIntent never reruns"),
                || Ok((2, 8))
            ),
            Err::<(), _>(NativeError::Foreign)
        );
        assert_eq!(runs.get(), 1);
    }
}

struct EpochFake {
    events: Vec<&'static str>,
    fail: Option<(&'static str, bool)>,
    intent: Option<native_io::epoch_archive::ArchivePhase>,
    source: bool,
    target: bool,
    victim: bool,
}
impl EpochFake {
    fn step(&mut self, name: &'static str, effect: impl FnOnce(&mut Self)) -> NativeResult<()> {
        self.events.push(name);
        if self.fail == Some((name, false)) {
            return Err(NativeError::Unavailable);
        }
        effect(self);
        if self.fail == Some((name, true)) {
            return Err(NativeError::OutcomeUnknown);
        }
        Ok(())
    }
}
impl native_io::epoch_archive::ArchivePort for EpochFake {
    fn persist(&mut self, phase: native_io::epoch_archive::ArchivePhase) -> NativeResult<()> {
        use native_io::epoch_archive::ArchivePhase::*;
        self.step(
            match phase {
                PruneIntent => "prune-intent",
                MoveIntent => "move-intent",
                Complete => "complete",
            },
            |f| {
                f.intent = Some(phase);
            },
        )
    }
    fn prune(&mut self) -> NativeResult<()> {
        assert_eq!(
            self.intent,
            Some(native_io::epoch_archive::ArchivePhase::PruneIntent)
        );
        self.step("prune", |f| f.victim = false)
    }
    fn move_current(&mut self) -> NativeResult<()> {
        assert_eq!(
            self.intent,
            Some(native_io::epoch_archive::ArchivePhase::MoveIntent)
        );
        assert!(!self.victim);
        self.step("move", |f| {
            f.source = false;
            f.target = true;
        })
    }
    fn observe_complete(&mut self) -> NativeResult<()> {
        self.step("observe", |f| assert!(!f.source && f.target))
    }
}
#[test]
fn epoch_archive_actual_order_journals_each_effect_and_observes_before_complete() {
    let mut f = EpochFake {
        events: vec![],
        fail: None,
        intent: None,
        source: true,
        target: false,
        victim: true,
    };
    native_io::epoch_archive::ArchiveSequence::default()
        .run_once(&mut f, true)
        .unwrap();
    assert_eq!(
        f.events,
        [
            "prune-intent",
            "prune",
            "move-intent",
            "move",
            "observe",
            "complete"
        ]
    );
    assert!(!f.source && f.target && !f.victim);
}
#[test]
fn epoch_archive_every_before_after_interruption_refuses_repeated_effects() {
    for point in [
        "prune-intent",
        "prune",
        "move-intent",
        "move",
        "observe",
        "complete",
    ] {
        for after in [false, true] {
            let mut f = EpochFake {
                events: vec![],
                fail: Some((point, after)),
                intent: None,
                source: true,
                target: false,
                victim: true,
            };
            let mut sequence = native_io::epoch_archive::ArchiveSequence::default();
            assert!(sequence.run_once(&mut f, true).is_err());
            let effects = f.events.clone();
            assert_eq!(
                sequence.run_once(&mut f, true),
                Err(NativeError::OutcomeUnknown)
            );
            assert_eq!(f.events, effects);
            if f.source {
                assert!(!f.target);
            } else {
                assert!(f.target);
            }
        }
    }
}
#[test]
fn epoch_archive_vacancy_has_no_prune_effect() {
    let mut f = EpochFake {
        events: vec![],
        fail: None,
        intent: None,
        source: true,
        target: false,
        victim: false,
    };
    native_io::epoch_archive::ArchiveSequence::default()
        .run_once(&mut f, false)
        .unwrap();
    assert_eq!(f.events, ["move-intent", "move", "observe", "complete"]);
}

#[test]
fn owner_record_instance_is_exact_u64_and_cold_recovery_never_replays_effects() {
    use payload::health::ServicePort;
    for instance in [1, (1u64 << 53) + 7, u64::MAX] {
        assert_eq!(
            service::record_instance(&format!("{instance:032x}")),
            Ok(instance)
        );
    }
    for text in [
        "1",
        "00000000000000000000000000000000",
        "0000000000000000FFFFFFFFFFFFFFFF",
        "10000000000000000000000000000000",
    ] {
        assert!(service::record_instance(text).is_err());
    }
    let mut port = service::NativeUpgradePort::new();
    let mut record = OperationRecord::new([7; 16]).unwrap();
    record.set_phase(Phase::StopIntent);
    assert!(matches!(
        port.recover_stop(&record),
        Err(NativeError::Unsupported)
    ));
    record.set_phase(Phase::StartIntent);
    let payload = payload::health::VerifiedPayload::fixture([7; 16]).unwrap();
    assert!(matches!(
        port.recover_started(&record, &payload),
        Err(NativeError::Unsupported)
    ));
}

#[test]
fn start_readiness_stale_predecessor_waits_without_status_until_exact_new_candidate() {
    use agent_contract::BootstrapPhase;
    for error in [
        NativeError::Foreign,
        NativeError::Missing,
        NativeError::Busy,
        NativeError::Unavailable,
    ] {
        assert!(service::pending_start_observation(error));
    }
    for error in [
        NativeError::Invalid,
        NativeError::Unsupported,
        NativeError::OutcomeUnknown,
        NativeError::Oversize,
    ] {
        assert!(!service::pending_start_observation(error));
    }
    let previous = (1u64 << 53) + 7;
    let new = previous + 1;
    let mut status_attempts = 0;
    for (phase, instance) in [
        (BootstrapPhase::Ready, previous),
        (BootstrapPhase::Starting, new),
        (BootstrapPhase::Ready, new),
    ] {
        if service::new_ready_candidate(&phase, instance, Some(previous)).unwrap() {
            status_attempts += 1;
            assert_eq!(instance, new);
        }
    }
    assert_eq!(status_attempts, 1);
    assert!(service::new_ready_candidate(&BootstrapPhase::Ready, 0, None).is_err());
}
