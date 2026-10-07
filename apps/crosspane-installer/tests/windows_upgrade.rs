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

/// These ports own authored fixture effects, not native handles or permits. The real keeper
/// controller, payload apply/recovery and admission predicate are exercised without OS calls.
mod a4d_outer {
    use super::*;
    use native_io::activation::{
        KeeperApplyAttempt, KeeperControl, KeeperPort, KeeperProgress, KeeperStage,
    };
    use payload::recovery::{
        FixedObservation, RecoveryPort, ReopenedRole, RollbackOutcome, StageObservation,
    };
    use payload::staging::PayloadPort;
    use std::{cell::Cell, rc::Rc, sync::Arc};

    struct FixtureLock(Rc<Cell<bool>>);
    impl Drop for FixtureLock {
        fn drop(&mut self) {
            self.0.set(false);
        }
    }
    struct OwnedUpgrade {
        inner: FakeUpgrade,
        retained_stop: Option<Arc<service::UpgradeStopProof>>,
        lock_held: Rc<Cell<bool>>,
        lock_boundary: payload::LockHandoff<'static, FixtureLock>,
        start_permit: Option<u8>,
    }
    impl OwnedUpgrade {
        fn new() -> Self {
            let held = Rc::new(Cell::new(true));
            Self {
                inner: FakeUpgrade::new(),
                retained_stop: None,
                lock_held: held.clone(),
                lock_boundary: payload::LockHandoff::Owned(Some(FixtureLock(held))),
                start_permit: Some(1),
            }
        }
    }
    impl PayloadPort for OwnedUpgrade {
        type Stop = Arc<service::UpgradeStopProof>;
        type Verified = payload::health::VerifiedPayload;
        type Started = service::NewInstanceEvidence;
        fn journal(&mut self, r: &OperationRecord) -> NativeResult<()> {
            self.inner.journal(r)
        }
        fn stop(&mut self, op: [u8; 16]) -> NativeResult<Self::Stop> {
            // Store this fake's completed effect before its result can be lost. No journal
            // observation can populate this retained slot, and cold recovery has no slot.
            self.inner.expect_intent(Phase::StopIntent, None);
            self.inner.before(Effect::Stop)?;
            self.inner.stops += 1;
            self.inner.loaded_old_image = false;
            let value = Arc::new(service::UpgradeStopProof::fixture(op, Some(OLD_INSTANCE))?);
            self.retained_stop = Some(value.clone());
            self.inner.after()?;
            Ok(value)
        }
        fn original_instance(&self, s: &Self::Stop) -> Option<u64> {
            s.original_instance()
        }
        fn released(&mut self, s: &Self::Stop) -> NativeResult<()> {
            self.inner.released(s)
        }
        fn stage(&mut self, op: [u8; 16], r: PayloadRole) -> NativeResult<ImageObservation> {
            self.inner.stage(op, r)
        }
        fn observe_original(
            &mut self,
            r: PayloadRole,
        ) -> NativeResult<payload::recovery::OriginalLeaf> {
            self.inner.observe_original(r)
        }
        fn backup(&mut self, op: [u8; 16], r: PayloadRole) -> NativeResult<Option<FileStamp>> {
            self.inner.backup(op, r)
        }
        fn publish(&mut self, op: [u8; 16], r: PayloadRole) -> NativeResult<ImageObservation> {
            self.inner.publish(op, r)
        }
        fn verify(&mut self, op: [u8; 16]) -> NativeResult<Self::Verified> {
            self.inner.verify(op)
        }
        fn start(&mut self, op: [u8; 16], v: &Self::Verified) -> NativeResult<Self::Started> {
            let held = self.lock_held.clone();
            let fresh = held.clone();
            let inner = &mut self.inner;
            self.lock_boundary.run_once(
                &mut self.start_permit,
                true,
                || {
                    assert!(!held.get());
                    inner.start(op, v)
                },
                || {
                    assert!(!fresh.get());
                    fresh.set(true);
                    Ok((FixtureLock(fresh), 2))
                },
            )
        }
        fn health(
            &mut self,
            op: [u8; 16],
            s: &Self::Started,
            v: &Self::Verified,
        ) -> NativeResult<String> {
            assert!(self.lock_held.get());
            self.inner.health(op, s, v)
        }
        fn prune(&mut self, op: [u8; 16]) -> NativeResult<bool> {
            self.inner.prune(op)
        }
    }
    impl RecoveryPort for OwnedUpgrade {
        fn recover_stop(&mut self, r: &OperationRecord) -> NativeResult<Self::Stop> {
            let value = self
                .retained_stop
                .as_ref()
                .ok_or(NativeError::Unsupported)?;
            if value.operation() != r.operation() {
                return Err(NativeError::Foreign);
            }
            Ok(value.clone())
        }
        fn observe_role(
            &mut self,
            _: &OperationRecord,
            role: PayloadRole,
        ) -> NativeResult<ReopenedRole> {
            let events = &self.inner.events;
            let published = events.contains(&Effect::Publish(role));
            let staged = events.contains(&Effect::Stage(role));
            let backed = events.contains(&Effect::Backup(role));
            let old = FileStamp {
                volume: 7,
                file: [role as u8 + 11; 16],
            };
            Ok(ReopenedRole {
                staged: if staged && !published {
                    StageObservation::Ready(FakeUpgrade::observation(role))
                } else {
                    StageObservation::Missing
                },
                fixed: if published {
                    FixedObservation::Published(FakeUpgrade::observation(role))
                } else if backed {
                    FixedObservation::Missing
                } else {
                    FixedObservation::Original(old)
                },
                backup: backed.then_some(old),
                unknown_backup: false,
            })
        }
        fn recover_started(
            &mut self,
            r: &OperationRecord,
            v: &Self::Verified,
        ) -> NativeResult<Option<Self::Started>> {
            if self.inner.starts != 1 {
                return Ok(None);
            }
            Ok(Some(service::NewInstanceEvidence::fixture(
                r.operation(),
                self.inner.new_instance,
                v.agent_identity()?,
            )?))
        }
        fn settle_stage(
            &mut self,
            _: &OperationRecord,
            role: PayloadRole,
        ) -> NativeResult<ImageObservation> {
            if !self.inner.events.contains(&Effect::Stage(role)) {
                return Err(NativeError::Missing);
            }
            Ok(FakeUpgrade::observation(role))
        }
        fn rollback_stage(&mut self, _: &OperationRecord) -> NativeResult<RollbackOutcome> {
            if self.inner.stops == 0 {
                Ok(RollbackOutcome::RolledBack)
            } else {
                Ok(RollbackOutcome::Retained)
            }
        }
    }
    struct Child(Rc<Cell<bool>>);
    struct FakeKeeper {
        exclusive: Rc<Cell<bool>>,
        upgrade: OwnedUpgrade,
        operation: OperationRecord,
        lose_apply_reply: bool,
        lose_commit_intent_reply: bool,
        commit_published: bool,
        attempt: KeeperApplyAttempt,
        launches: usize,
        settles: usize,
        cancellations: usize,
    }
    impl FakeKeeper {
        fn new(exclusive: Rc<Cell<bool>>) -> Self {
            Self {
                exclusive,
                upgrade: OwnedUpgrade::new(),
                operation: OperationRecord::new([7; 16]).unwrap(),
                lose_apply_reply: false,
                lose_commit_intent_reply: false,
                commit_published: false,
                attempt: KeeperApplyAttempt::default(),
                launches: 0,
                settles: 0,
                cancellations: 0,
            }
        }
    }
    impl KeeperPort for FakeKeeper {
        type Child = Child;
        fn prepare(&mut self) -> NativeResult<Child> {
            // Authored fake of kernel first-instance acquisition. Sharing this primitive tests
            // a second independent caller; the control's own preparing bit alone cannot do so.
            if self.exclusive.replace(true) {
                return Err(NativeError::Busy);
            }
            self.launches += 1;
            Ok(Child(self.exclusive.clone()))
        }
        fn mark_ready(&mut self, _: &Child) -> NativeResult<()> {
            Ok(())
        }
        fn commit_intent(&mut self, child: &Child) -> NativeResult<()> {
            assert!(Rc::ptr_eq(&child.0, &self.exclusive));
            assert!(child.0.get());
            self.commit_published = true;
            if self.lose_commit_intent_reply {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        fn apply_once(&mut self, _: &Child) -> NativeResult<KeeperProgress> {
            self.attempt.reserve()?;
            payload::staging::apply(&mut self.upgrade, &mut self.operation)?;
            if self.lose_apply_reply {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(KeeperProgress::Complete)
        }
        fn recover_same_owner(&mut self, child: &Child) -> NativeResult<KeeperProgress> {
            if !self.attempt.started() {
                // Authored fixture of the ORIGINAL live committed owner, not a cold journal
                // constructor. Production additionally renews actual lock/source/native-idle seals.
                if !self.commit_published
                    || !Rc::ptr_eq(&child.0, &self.exclusive)
                    || !child.0.get()
                {
                    return Err(NativeError::OutcomeUnknown);
                }
                return self.apply_once(child);
            }
            self.upgrade.inner.fail_at = None;
            let mut durable = self
                .upgrade
                .inner
                .durable
                .clone()
                .ok_or(NativeError::Missing)?;
            let result = payload::recovery::resume(&mut self.upgrade, &mut durable)?;
            self.operation = durable;
            Ok(if result == RecoveryDecision::Complete {
                KeeperProgress::Complete
            } else {
                KeeperProgress::Pending
            })
        }
        fn settle(&mut self, child: &Child) -> NativeResult<()> {
            self.settles += 1;
            child.0.set(false);
            Ok(())
        }
        fn cancel_before_stop(&mut self, child: &Child) -> NativeResult<()> {
            assert_eq!(
                (self.upgrade.inner.stops, self.upgrade.inner.starts),
                (0, 0)
            );
            self.cancellations += 1;
            child.0.set(false);
            Ok(())
        }
    }
    fn sid(sub: &[u32]) -> native_io::identity::Sid {
        let mut bytes = vec![1, sub.len() as u8, 0, 0, 0, 0, 0, 5];
        for value in sub {
            bytes.extend(value.to_le_bytes());
        }
        native_io::identity::Sid::from_bytes(bytes).unwrap()
    }
    fn token() -> native_io::identity::TokenFacts {
        native_io::identity::TokenFacts {
            user: sid(&[21, 7]),
            logon: sid(&[5, 9, 11]),
            session: 2,
            elevated: false,
            integrity: 0x2000,
            authentication_id: 17,
            impersonating: false,
        }
    }
    fn peer(
        actual: &native_io::identity::TokenFacts,
        process: (u32, u64),
        identity: FileIdentity,
        path: &str,
    ) -> NativeResult<()> {
        native_io::supervisor_owner::outer_peer_observations_match(
            &token(),
            actual,
            (101, 200),
            process,
            (
                FileIdentity {
                    volume: 7,
                    file: [17; 16],
                },
                r"C:\fixture\keeper-copy.exe",
            ),
            (identity, path),
        )
    }

    #[derive(Default)]
    struct SelectionFake {
        selected: Option<payload::recovery::OuterUpgradeRecord>,
        created: Option<[u8; 16]>,
        lose_selection_reply: bool,
    }
    impl payload::recovery::OuterSelectionPort for SelectionFake {
        fn publish_selection(
            &mut self,
            r: &payload::recovery::OuterUpgradeRecord,
        ) -> NativeResult<()> {
            if self.selected.is_some() {
                return Err(NativeError::OutcomeUnknown);
            }
            self.selected = Some(r.clone());
            if self.lose_selection_reply {
                return Err(NativeError::OutcomeUnknown);
            }
            Ok(())
        }
        fn create_operation(&mut self, r: &OperationRecord) -> NativeResult<()> {
            assert_eq!(self.selected.as_ref().unwrap().operation(), r.operation());
            assert!(self.created.is_none());
            self.created = Some(r.operation());
            Ok(())
        }
    }
    fn selection() -> payload::recovery::OuterUpgradeRecord {
        use payload::recovery::{
            OuterContextCorrelation, OuterProcessCorrelation, OuterUpgradeRecord,
        };
        OuterUpgradeRecord::new(
            [7; 16],
            OuterProcessCorrelation::new(
                101,
                200,
                FileStamp {
                    volume: 7,
                    file: [17; 16],
                },
                image(),
            )
            .unwrap(),
            OuterContextCorrelation::new(&token()).unwrap(),
            std::array::from_fn(|_| image()),
        )
        .unwrap()
    }
    fn source_bytes(role: PayloadRole) -> Vec<u8> {
        let mut bytes = vec![role as u8; 512];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[84..86].copy_from_slice(&240u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes[156..158].copy_from_slice(&3u16.to_le_bytes());
        bytes
    }
    fn source_facts(role: PayloadRole) -> PeFacts {
        let bytes = source_bytes(role);
        let hash = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &bytes);
        let mut sha256 = [0; 32];
        sha256.copy_from_slice(hash.as_ref());
        PeFacts {
            size: bytes.len() as u64,
            sha256,
            machine: 0x8664,
            subsystem: 3,
            version: "1".into(),
        }
    }
    fn source_inputs() -> Vec<payload::PayloadInput> {
        [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl]
            .into_iter()
            .map(|role| payload::PayloadInput {
                role,
                content: Box::new(std::io::Cursor::new(source_bytes(role))),
            })
            .collect()
    }
    fn verify_sources_before_stop() {
        use payload::inventory::ApprovedInventory;
        let inventory = ApprovedInventory::fixture(
            PayloadRole::ALL
                .into_iter()
                .map(|role| ApprovedPe::fixture(role, source_facts(role)))
                .collect(),
        )
        .unwrap();
        let installer =
            ApprovedPe::fixture(PayloadRole::Installer, source_facts(PayloadRole::Installer));
        let deadline = native_io::Deadline::new(
            5000,
            Arc::new(native_io::process::MonotonicClock::default()),
            native_io::process::Cancellation::default(),
        )
        .unwrap();
        let sources =
            payload::buffer_outer_sources(source_inputs(), &inventory, &installer, &deadline)
                .unwrap();
        for role in [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl] {
            assert_eq!(sources.bytes(role).unwrap(), source_bytes(role));
        }
        let received = payload::ApprovedOuterSources::receive(
            [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl].map(source_bytes),
            &inventory,
            &installer,
            &deadline,
        )
        .unwrap();
        assert_eq!(sources.facts(), received.facts());
        let mut missing = source_inputs();
        missing.pop();
        assert!(payload::buffer_outer_sources(missing, &inventory, &installer, &deadline).is_err());
        for kind in 0..4 {
            let mut bytes = source_bytes(PayloadRole::Agent);
            match kind {
                0 => {
                    bytes.pop();
                }
                1 => bytes.push(7),
                2 => bytes[400] ^= 1,
                _ => bytes[0] = 0,
            }
            let mut inputs = source_inputs();
            inputs[0].content = Box::new(std::io::Cursor::new(bytes.clone()));
            assert!(
                payload::buffer_outer_sources(inputs, &inventory, &installer, &deadline).is_err()
            );
            let mut receiver =
                [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl].map(source_bytes);
            receiver[0] = bytes;
            assert!(
                payload::ApprovedOuterSources::receive(receiver, &inventory, &installer, &deadline)
                    .is_err()
            );
        }
        let mut wrong_pins: Vec<_> = PayloadRole::ALL
            .into_iter()
            .map(|role| ApprovedPe::fixture(role, source_facts(role)))
            .collect();
        let mut wrong = source_facts(PayloadRole::Agent);
        wrong.sha256[0] ^= 1;
        let at = wrong_pins
            .iter()
            .position(|pin| pin.role() == PayloadRole::Agent)
            .unwrap();
        wrong_pins[at] = ApprovedPe::fixture(PayloadRole::Agent, wrong);
        let own_inventory = ApprovedInventory::fixture(wrong_pins).unwrap();
        assert!(
            payload::ApprovedOuterSources::receive(
                [PayloadRole::Agent, PayloadRole::Ui, PayloadRole::Ctl].map(source_bytes),
                &own_inventory,
                &installer,
                &deadline
            )
            .is_err()
        );
    }

    #[test]
    fn outer_upgrade_end_to_end_ready_is_not_stop_and_lost_reply_never_replays() {
        let mut selection_port = SelectionFake::default();
        payload::recovery::publish_outer_selection(&mut selection_port, &selection()).unwrap();
        assert_eq!(selection_port.created, Some([7; 16]));
        for lose_intent in [false, true] {
            let exclusive = Rc::new(Cell::new(false));
            let mut port = FakeKeeper::new(exclusive.clone());
            let mut owner = KeeperControl::default();
            owner.prepare(&mut port).unwrap();
            assert_eq!(owner.stage(), KeeperStage::Ready);
            assert_eq!(
                (port.upgrade.inner.stops, port.upgrade.inner.starts),
                (0, 0)
            );
            port.lose_apply_reply = !lose_intent;
            port.lose_commit_intent_reply = lose_intent;
            assert_eq!(owner.commit(&mut port), Err(NativeError::OutcomeUnknown));
            assert!(owner.committed());
            assert!(exclusive.get());
            assert_eq!(
                (port.upgrade.inner.stops, port.upgrade.inner.starts),
                if lose_intent { (0, 0) } else { (1, 1) }
            );
            assert_eq!(port.attempt.started(), !lose_intent);
            let effects = port.upgrade.inner.events.len();
            assert_eq!(owner.commit(&mut port), Err(NativeError::OutcomeUnknown));
            assert_eq!(port.upgrade.inner.events.len(), effects);
            assert_eq!(owner.recover(&mut port), Ok(KeeperProgress::Complete));
            assert_eq!(
                (port.upgrade.inner.stops, port.upgrade.inner.starts),
                (1, 1)
            );
            assert_eq!(port.operation.phase(), Phase::Complete);
            assert!(!exclusive.get());
            assert!(port.attempt.started());
            assert_eq!(port.attempt.reserve(), Err(NativeError::OutcomeUnknown));
            let settled = port.settles;
            assert_eq!(owner.recover(&mut port), Ok(KeeperProgress::Complete));
            assert_eq!(port.settles, settled);
        }
    }

    #[test]
    fn outer_crash_after_stop_new_caller_uses_same_owner_and_cold_port_refuses() {
        // Find a boundary before the first stage effect, after genuine completed Stop/release.
        let mut baseline = FakeUpgrade::new();
        let mut record = OperationRecord::new([7; 16]).unwrap();
        payload::staging::apply(&mut baseline, &mut record).unwrap();
        let stage_index = baseline
            .events
            .iter()
            .position(|e| *e == Effect::Stage(PayloadRole::Installer))
            .unwrap();
        let stop_index = baseline
            .events
            .iter()
            .position(|e| *e == Effect::Stop)
            .unwrap();
        // Both actual completed Stop with a lost reply and the later preStage interruption
        // retain the same authored owner; neither may manufacture proof from its journal.
        for interruption in [stop_index * 2 + 1, stage_index * 2] {
            let exclusive = Rc::new(Cell::new(false));
            let mut port = FakeKeeper::new(exclusive.clone());
            port.upgrade.inner.fail_at = Some(interruption);
            let mut keeper_owner = KeeperControl::default();
            keeper_owner.prepare(&mut port).unwrap();
            let outer_client = Rc::new(());
            let outer_weak = Rc::downgrade(&outer_client);
            assert_eq!(
                keeper_owner.commit(&mut port),
                Err(NativeError::OutcomeUnknown)
            );
            assert_eq!(
                (port.upgrade.inner.stops, port.upgrade.inner.starts),
                (1, 0)
            );
            assert!(port.upgrade.retained_stop.is_some());
            assert!(exclusive.get());
            drop(outer_client);
            assert!(outer_weak.upgrade().is_none());
            let mut cold = service::NativeUpgradePort::new();
            assert!(matches!(
                payload::health::ServicePort::recover_stop(
                    &mut cold,
                    &port.upgrade.inner.durable.clone().unwrap()
                ),
                Err(NativeError::Unsupported)
            ));
            // A new client reaches the still resident owner/port; it does not recreate either.
            let next_outer_client = Rc::new(());
            assert_eq!(
                keeper_owner.recover(&mut port),
                Ok(KeeperProgress::Complete)
            );
            drop(next_outer_client);
            assert_eq!(
                (port.upgrade.inner.stops, port.upgrade.inner.starts),
                (1, 1)
            );
            assert_eq!(port.operation.phase(), Phase::Complete);
        }
    }

    #[test]
    fn outer_cancel_before_stop_and_source_or_selection_failure_leave_old_tree_running() {
        verify_sources_before_stop();
        // Terminal metadata is not deletion authority. Even policy retirement requires positive
        // absence plus an exact no-Stop/no-mutation cancelled operation and matching catalog.
        use payload::recovery::{
            OriginalLeaf, OuterPhase, StageCatalog, validate_outer_terminal_retirement,
        };
        let mut terminal = selection();
        terminal.advance(OuterPhase::Cancelled).unwrap();
        let operation = OperationRecord::new(terminal.operation()).unwrap();
        let mut catalog = StageCatalog::default();
        catalog.active = Some(terminal.operation());
        assert!(validate_outer_terminal_retirement(&terminal, &operation, &catalog).is_err());
        terminal.begin_cleanup().unwrap();
        assert!(validate_outer_terminal_retirement(&terminal, &operation, &catalog).is_err());
        terminal.copy_absent().unwrap();
        assert_eq!(
            validate_outer_terminal_retirement(&terminal, &operation, &catalog),
            Ok(())
        );
        let mut stopped = operation.clone();
        stopped.advance(Phase::StopIntent, None);
        assert!(validate_outer_terminal_retirement(&terminal, &stopped, &catalog).is_err());
        let mut observed = operation.clone();
        observed.set_original_instance(format!("{:032x}", 11));
        assert!(validate_outer_terminal_retirement(&terminal, &observed, &catalog).is_err());
        let mut changed = operation.clone();
        changed.role_mut(PayloadRole::Agent).unwrap().original = OriginalLeaf::Present(FileStamp {
            volume: 7,
            file: [18; 16],
        });
        assert!(validate_outer_terminal_retirement(&terminal, &changed, &catalog).is_err());
        let mut pending = operation.clone();
        pending.set_retention_incomplete(true);
        assert!(validate_outer_terminal_retirement(&terminal, &pending, &catalog).is_err());
        catalog.active = Some([8; 16]);
        assert!(validate_outer_terminal_retirement(&terminal, &operation, &catalog).is_err());
        let mut interrupted = SelectionFake {
            lose_selection_reply: true,
            ..Default::default()
        };
        assert_eq!(
            payload::recovery::publish_outer_selection(&mut interrupted, &selection()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(interrupted.selected.is_some());
        assert!(interrupted.created.is_none());
        interrupted.lose_selection_reply = false;
        assert_eq!(
            payload::recovery::publish_outer_selection(&mut interrupted, &selection()),
            Err(NativeError::OutcomeUnknown)
        );
        assert!(interrupted.created.is_none());
        let mut port = FakeKeeper::new(Rc::new(Cell::new(false)));
        let mut owner = KeeperControl::default();
        owner.prepare(&mut port).unwrap();
        owner.cancel(&mut port).unwrap();
        assert_eq!(owner.stage(), KeeperStage::Cancelled);
        assert!(port.upgrade.inner.loaded_old_image);
        assert_eq!(
            (port.upgrade.inner.stops, port.upgrade.inner.starts),
            (0, 0)
        );
        assert_eq!(port.cancellations, 1);
        assert_eq!(port.launches, 1);
        assert_eq!(owner.commit(&mut port), Err(NativeError::OutcomeUnknown));
        assert_eq!(owner.cancel(&mut port), Err(NativeError::OutcomeUnknown));
        assert_eq!(owner.prepare(&mut port), Err(NativeError::OutcomeUnknown));
        assert_eq!(port.cancellations, 1);
    }

    #[test]
    fn wrong_user_logon_session_or_nonlimited_peer_is_refused() {
        let expected = token();
        let identity = FileIdentity {
            volume: 7,
            file: [17; 16],
        };
        assert_eq!(
            peer(
                &expected,
                (101, 200),
                identity,
                r"C:\fixture\keeper-copy.exe"
            ),
            Ok(())
        );
        let history = payload::recovery::OuterContextCorrelation::new(&expected).unwrap();
        let mut later = expected.clone();
        later.logon = sid(&[5, 19, 21]);
        later.authentication_id += 9;
        later.session += 1;
        // Terminal correlation history may survive logon; it does not admit a live peer.
        assert_eq!(history.same_user(&later), Ok(()));
        assert!(history.matches(&later).is_err());
        assert!(peer(&later, (101, 200), identity, r"C:\fixture\keeper-copy.exe").is_err());
        later.user = sid(&[21, 8]);
        assert!(history.same_user(&later).is_err());
        for index in 0..7 {
            let mut actual = expected.clone();
            match index {
                0 => actual.user = sid(&[21, 8]),
                1 => actual.logon = sid(&[5, 9, 12]),
                2 => actual.session += 1,
                3 => actual.elevated = true,
                4 => actual.integrity = 0x3000,
                5 => actual.authentication_id += 1,
                _ => actual.impersonating = true,
            }
            assert!(peer(&actual, (101, 200), identity, r"C:\fixture\keeper-copy.exe").is_err());
        }
    }

    #[test]
    fn forged_path_pid_creation_or_fileid_correlation_never_matches_actual_peer() {
        // No argv path, process number or operation can select the private native keeper.
        let flag = std::ffi::OsString::from("--windows-upgrade-keeper");
        assert_eq!(
            service::upgrade_keeper_mode(std::slice::from_ref(&flag)),
            Ok(true)
        );
        assert_eq!(service::upgrade_keeper_mode(&[]), Ok(false));
        assert_eq!(
            service::upgrade_keeper_mode(&["--diagnose".into()]),
            Ok(false)
        );
        for arguments in [
            vec![flag.clone(), "C:\\fixture\\foreign.exe".into()],
            vec!["--diagnose".into(), flag.clone()],
            vec![flag.clone(), flag],
            vec!["--windows-upgrade-keeper=101".into()],
        ] {
            assert_eq!(
                service::upgrade_keeper_mode(&arguments),
                Err(NativeError::Invalid)
            );
        }
        let actual = token();
        let identity = FileIdentity {
            volume: 7,
            file: [17; 16],
        };
        for process in [(0, 200), (102, 200), (101, 0), (101, 201)] {
            assert!(peer(&actual, process, identity, r"C:\fixture\keeper-copy.exe").is_err());
        }
        for path in [
            r"C:\other\keeper-copy.exe",
            r"C:\fixture\agent.exe",
            r"..\keeper-copy.exe",
        ] {
            assert!(peer(&actual, (101, 200), identity, path).is_err());
        }
        for changed in [
            FileIdentity {
                volume: 8,
                file: [17; 16],
            },
            FileIdentity {
                volume: 7,
                file: [18; 16],
            },
        ] {
            assert!(peer(&actual, (101, 200), changed, r"C:\fixture\keeper-copy.exe").is_err());
        }
    }

    #[test]
    fn concurrent_second_outer_never_gets_another_keeper_or_stop() {
        let exclusive = Rc::new(Cell::new(false));
        let mut first_port = FakeKeeper::new(exclusive.clone());
        let mut first = KeeperControl::default();
        first.prepare(&mut first_port).unwrap();
        let mut second_port = FakeKeeper::new(exclusive.clone());
        let mut second = KeeperControl::default();
        assert_eq!(second.prepare(&mut second_port), Err(NativeError::Busy));
        assert_eq!(second_port.launches, 0);
        assert!(second.child().is_err());
        assert_eq!(
            second.commit(&mut second_port),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(
            (
                second_port.upgrade.inner.stops,
                second_port.upgrade.inner.starts
            ),
            (0, 0)
        );
        assert_eq!(first.commit(&mut first_port), Ok(KeeperProgress::Complete));
        assert_eq!(
            (
                first_port.upgrade.inner.stops,
                first_port.upgrade.inner.starts
            ),
            (1, 1)
        );
        assert_eq!(
            second.prepare(&mut second_port),
            Err(NativeError::OutcomeUnknown)
        );
        assert_eq!(second_port.launches, 0);
    }
}

/// FILE-only fixtures operate on authored slot observations and the actual production driver.
/// None constructs a native seal, stop proof, approved old image, handle or installed context.
mod a4e_files {
    use super::*;
    use native_io::identity::{Sid, TokenFacts};
    use payload::inventory::{ApprovedPe, PayloadRole};
    use payload::recovery::*;
    use std::collections::BTreeMap;

    fn stamp(n: u8) -> FileStamp {
        FileStamp {
            volume: 7,
            file: [n; 16],
        }
    }
    fn facts(n: u8) -> PeFacts {
        PeFacts {
            size: 4096,
            sha256: [n; 32],
            machine: 0x8664,
            subsystem: 2,
            version: format!("fixture-{n}"),
        }
    }
    fn sid(parts: &[u32]) -> Sid {
        let mut b = vec![1, parts.len() as u8, 0, 0, 0, 0, 0, 5];
        for p in parts {
            b.extend_from_slice(&p.to_le_bytes());
        }
        Sid::from_bytes(b).unwrap()
    }
    fn token(old: bool) -> TokenFacts {
        TokenFacts {
            user: sid(&[21, 7]),
            logon: sid(&[5, 9, if old { 11 } else { 12 }]),
            authentication_id: if old { 17 } else { 18 },
            session: if old { 2 } else { 3 },
            elevated: false,
            integrity: 0x2000,
            impersonating: false,
        }
    }
    fn outer() -> OuterUpgradeRecord {
        let value = OuterUpgradeRecord::new(
            [7; 16],
            OuterProcessCorrelation::new(101, 200, stamp(70), facts(10)).unwrap(),
            OuterContextCorrelation::new(&token(true)).unwrap(),
            [facts(11), facts(12), facts(13)],
        )
        .unwrap();
        let mut json = serde_json::to_value(value).unwrap();
        json["phase"] = serde_json::json!("committed");
        json["keeper_image"] = serde_json::to_value(stamp(71)).unwrap();
        json["keeper"] = serde_json::to_value(
            OuterProcessCorrelation::new(102, 201, stamp(71), facts(10)).unwrap(),
        )
        .unwrap();
        json["inherited_parent_handle"] = serde_json::json!(64);
        json["launch_stage"] = serde_json::json!("resumed");
        let result: OuterUpgradeRecord = serde_json::from_value(json).unwrap();
        result.validate().unwrap();
        result
    }
    fn new_image(i: usize) -> ImageObservation {
        ImageObservation {
            identity: stamp(40 + i as u8),
            facts: facts(10 + i as u8),
        }
    }
    fn index(role: PayloadRole) -> usize {
        PayloadRole::ALL.iter().position(|r| *r == role).unwrap()
    }
    #[derive(Clone)]
    struct World {
        views: [ReopenedRole; 4],
        fixed_contents: [Option<PeFacts>; 4],
        original_contents: [PeFacts; 4],
        copy: bool,
        catalog: bool,
        outer: bool,
        completed: bool,
        operation: OperationRecord,
        selection: OuterUpgradeRecord,
        journal: Option<Vec<u8>>,
        renames: Vec<(&'static str, PayloadRole)>,
        terminal_effects: Vec<&'static str>,
        flushes: usize,
        writes: usize,
    }
    impl World {
        fn new(forward: bool) -> Self {
            let mut operation = OperationRecord::new([7; 16]).unwrap();
            for (i, role) in PayloadRole::ALL.into_iter().enumerate() {
                let row = operation.role_mut(role).unwrap();
                row.original = OriginalLeaf::Present(stamp(20 + i as u8));
                row.staged = Some(new_image(i));
                if i < if forward { 1 } else { 2 } {
                    row.backup = Some(stamp(20 + i as u8));
                    row.published = Some(new_image(i));
                }
            }
            operation.advance(
                if forward {
                    Phase::PublishIntent
                } else {
                    Phase::BackupIntent
                },
                Some(if forward {
                    PayloadRole::Agent
                } else {
                    PayloadRole::Ui
                }),
            );
            let views = std::array::from_fn(|i| {
                let published = i < if forward { 1 } else { 2 };
                let moved = i == if forward { 1 } else { 2 };
                ReopenedRole {
                    staged: if published {
                        StageObservation::Missing
                    } else {
                        StageObservation::Ready(new_image(i))
                    },
                    fixed: if published {
                        FixedObservation::Published(new_image(i))
                    } else if moved {
                        FixedObservation::Missing
                    } else {
                        FixedObservation::Original(stamp(20 + i as u8))
                    },
                    backup: if published || moved {
                        Some(stamp(20 + i as u8))
                    } else {
                        None
                    },
                    unknown_backup: false,
                }
            });
            // Physical contents are separate authored observations, not approval from journal IDs.
            let original_contents = std::array::from_fn(|i| facts(30 + i as u8));
            let fixed_contents = std::array::from_fn(|i| match &views[i].fixed {
                FixedObservation::Original(_) => Some(original_contents[i].clone()),
                FixedObservation::Published(image) => Some(image.facts.clone()),
                _ => None,
            });
            Self {
                views,
                fixed_contents,
                original_contents,
                copy: true,
                catalog: true,
                outer: true,
                completed: false,
                operation,
                selection: outer(),
                journal: None,
                renames: Vec::new(),
                terminal_effects: Vec::new(),
                flushes: 0,
                writes: 0,
            }
        }
        fn plan(&self) -> NativeResult<FileRecoveryJournal> {
            FileRecoveryJournal::prepare(
                self.selection.clone(),
                FileRecordStamp::new(stamp(80), [80; 32])?,
                &self.operation,
                FileRecordStamp::new(stamp(81), [81; 32])?,
                self.views.clone(),
            )
        }
    }
    struct Port {
        world: World,
        status: i32,
        data: bool,
        timely: bool,
        calls: usize,
        fail: Option<(usize, bool)>,
    }
    impl Port {
        fn new(forward: bool) -> Self {
            Self {
                world: World::new(forward),
                status: 0xC000005Fu32 as i32,
                data: false,
                timely: true,
                calls: 0,
                fail: None,
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
        fn journal_copy(&self) -> FileRecoveryJournal {
            FileRecoveryJournal::decode(self.world.journal.as_ref().unwrap()).unwrap()
        }
        fn begin(&mut self) -> NativeResult<FileRecoveryJournal> {
            let journal = self.world.plan()?;
            self.renew(&journal)?;
            self.journal(&journal)?;
            Ok(journal)
        }
        fn intent(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
            step: FileRecoveryRoleStep,
        ) -> NativeResult<()> {
            self.renew(j)?;
            assert_eq!(j.cursor(), FileRecoveryCursor::Role { role, step });
            self.before()
        }
        fn rename(&mut self, kind: &'static str, role: PayloadRole) {
            assert!(
                !self.world.renames.contains(&(kind, role)),
                "completed rename replayed"
            );
            self.world.renames.push((kind, role));
        }
        fn terminal(
            &mut self,
            j: &FileRecoveryJournal,
            cursor: FileRecoveryCursor,
        ) -> NativeResult<()> {
            self.renew(j)?;
            assert_eq!(j.cursor(), cursor);
            self.before()
        }
    }
    impl FileRecoveryPort for Port {
        fn renew(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            assert_eq!(
                file_recovery_prior_logon(&self.world.selection, &token(false))?,
                Some(17)
            );
            native_io::epoch_archive::classify_prior_logon_status(self.status, self.data)?;
            if !self.timely {
                return Err(NativeError::Timeout);
            }
            j.matches_sources(
                &self.world.selection,
                FileRecordStamp::new(stamp(80), [80; 32])?,
                &self.world.operation,
                FileRecordStamp::new(stamp(81), [81; 32])?,
            )?;
            if !self.world.outer
                && !matches!(
                    j.cursor(),
                    FileRecoveryCursor::OuterRetireIntent | FileRecoveryCursor::Retired
                )
            {
                return Err(NativeError::Foreign);
            }
            Ok(())
        }
        fn journal(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            self.renew(j)?;
            self.before()?;
            if let Some(bytes) = &self.world.journal {
                let mut previous = FileRecoveryJournal::decode(bytes)?;
                previous.same_plan(j)?;
                previous.advance(j.cursor())?;
                assert_eq!(previous, *j);
            }
            self.world.journal = Some(j.encode()?);
            self.world.writes += 1;
            self.after()
        }
        fn observe_role(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
        ) -> NativeResult<ReopenedRole> {
            self.renew(j)?;
            Ok(self.world.views[index(role)].clone())
        }
        fn return_published(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
        ) -> NativeResult<()> {
            self.intent(j, role, FileRecoveryRoleStep::ReturnPublishedIntent)?;
            let i = index(role);
            let view = &mut self.world.views[i];
            assert!(matches!(view.staged, StageObservation::Missing));
            let FixedObservation::Published(image) = &view.fixed else {
                panic!("not published");
            };
            assert_eq!(Some(image), j.role(role)?.new_image());
            view.staged = StageObservation::Ready(image.clone());
            view.fixed = FixedObservation::Missing;
            self.world.fixed_contents[i] = None;
            self.rename("return", role);
            self.after()
        }
        fn restore_original(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
        ) -> NativeResult<()> {
            self.intent(j, role, FileRecoveryRoleStep::RestoreOriginalIntent)?;
            let view = &mut self.world.views[index(role)];
            assert!(matches!(view.fixed, FixedObservation::Missing));
            let old = view.backup.take().ok_or(NativeError::Foreign)?;
            assert_eq!(j.role(role)?.original(), OriginalLeaf::Present(old));
            view.fixed = FixedObservation::Original(old);
            self.world.fixed_contents[index(role)] =
                Some(self.world.original_contents[index(role)].clone());
            self.rename("restore", role);
            self.after()
        }
        fn settle_stage(&mut self, j: &FileRecoveryJournal, role: PayloadRole) -> NativeResult<()> {
            self.intent(j, role, FileRecoveryRoleStep::SettleStageIntent)?;
            assert!(
                matches!(&self.world.views[index(role)].staged,StageObservation::Ready(image) if Some(image)==j.role(role).unwrap().new_image())
            );
            self.world.flushes += 1;
            self.after()
        }
        fn backup_original(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
        ) -> NativeResult<()> {
            self.intent(j, role, FileRecoveryRoleStep::BackupOriginalIntent)?;
            let view = &mut self.world.views[index(role)];
            assert!(view.backup.is_none());
            let FixedObservation::Original(id) = view.fixed else {
                panic!("not original");
            };
            assert_eq!(j.role(role)?.original(), OriginalLeaf::Present(id));
            view.backup = Some(id);
            view.fixed = FixedObservation::Missing;
            self.world.fixed_contents[index(role)] = None;
            self.rename("backup", role);
            self.after()
        }
        fn publish_stage(
            &mut self,
            j: &FileRecoveryJournal,
            role: PayloadRole,
        ) -> NativeResult<()> {
            self.intent(j, role, FileRecoveryRoleStep::PublishStageIntent)?;
            let view = &mut self.world.views[index(role)];
            assert!(matches!(view.fixed, FixedObservation::Missing));
            let StageObservation::Ready(image) = &view.staged else {
                panic!("no stage");
            };
            assert_eq!(Some(image), j.role(role)?.new_image());
            self.world.fixed_contents[index(role)] = Some(image.facts.clone());
            self.world.flushes += 1;
            view.fixed = FixedObservation::Published(image.clone());
            view.staged = StageObservation::Missing;
            self.rename("publish", role);
            self.after()
        }
        fn cleanup_keeper_copy(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            self.terminal(j, FileRecoveryCursor::CopyDeleteIntent)?;
            if self.world.copy {
                self.world.copy = false;
                self.world.terminal_effects.push("copy");
            }
            self.after()
        }
        fn retire_catalog(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            self.terminal(j, FileRecoveryCursor::CatalogRetireIntent)?;
            assert!(!self.world.copy);
            assert!(!self.world.completed);
            if self.world.catalog {
                self.world.catalog = false;
                self.world.terminal_effects.push("catalog");
            }
            self.after()
        }
        fn retire_outer(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            self.terminal(j, FileRecoveryCursor::OuterRetireIntent)?;
            assert!(!self.world.copy && !self.world.catalog);
            assert!(!self.world.completed);
            if self.world.outer {
                self.world.outer = false;
                self.world.terminal_effects.push("outer");
            }
            self.after()
        }
        fn observe_terminal(&mut self, j: &FileRecoveryJournal) -> NativeResult<()> {
            self.renew(j)?;
            if self.world.copy || self.world.catalog || self.world.outer || self.world.completed {
                Err(NativeError::Foreign)
            } else {
                Ok(())
            }
        }
    }
    fn assert_targets(p: &Port, forward: bool) {
        for (i, view) in p.world.views.iter().enumerate() {
            if forward {
                assert_eq!(view.fixed, FixedObservation::Published(new_image(i)));
                assert_eq!(view.staged, StageObservation::Missing);
                assert_eq!(view.backup, Some(stamp(20 + i as u8)));
            } else {
                assert_eq!(view.fixed, FixedObservation::Original(stamp(20 + i as u8)));
                assert_eq!(view.staged, StageObservation::Ready(new_image(i)));
                assert!(view.backup.is_none());
            }
        }
        assert!(!p.world.copy && !p.world.catalog && !p.world.outer && !p.world.completed);
    }
    /// Independent fresh-build approval and the existing a4c lifecycle driver are separate from
    /// FILE convergence. These authored image approvals are never derived from old opaque IDs.
    struct FreshEpoch {
        approved: [ApprovedPe; 4],
        views: [ReopenedRole; 4],
        fixed_contents: [Option<PeFacts>; 4],
        events: Vec<&'static str>,
    }
    impl service::supervisor::InitialEpochPort for FreshEpoch {
        type Child = u8;
        type Ready = u8;
        fn prepare_epoch(&mut self) -> NativeResult<()> {
            for (i, expected) in self.approved.iter().enumerate() {
                if expected.role() != PayloadRole::ALL[i]
                    || !expected.facts().valid()
                    || !matches!(
                        self.views[i].fixed,
                        FixedObservation::Original(_) | FixedObservation::Published(_)
                    )
                    || self.fixed_contents[i].as_ref() != Some(expected.facts())
                {
                    return Err(NativeError::Foreign);
                }
            }
            self.events.push("fresh approval/disposition");
            Ok(())
        }
        fn create(&mut self) -> NativeResult<u8> {
            assert_eq!(self.events.len(), 1);
            self.events.push("create");
            Ok(1)
        }
        fn await_ready(&mut self, _: &u8) -> NativeResult<u8> {
            self.events.push("actual authored Ready");
            Ok(2)
        }
        fn publish_running(&mut self, _: &u8) -> NativeResult<()> {
            self.events.push("Bound");
            Ok(())
        }
    }
    fn independent_epoch(world: &World, forward: bool) -> NativeResult<()> {
        let mut fresh = FreshEpoch {
            approved: std::array::from_fn(|i| {
                ApprovedPe::fixture(
                    PayloadRole::ALL[i],
                    facts(if forward { 10 + i as u8 } else { 30 + i as u8 }),
                )
            }),
            views: world.views.clone(),
            fixed_contents: world.fixed_contents.clone(),
            events: Vec::new(),
        };
        let result = service::supervisor::initialize_epoch(&mut fresh);
        if result.is_err() {
            assert!(fresh.events.is_empty());
            return result.map(|_| ());
        }
        assert_eq!(result, Ok(2));
        assert_eq!(
            fresh.events,
            [
                "fresh approval/disposition",
                "create",
                "actual authored Ready",
                "Bound"
            ]
        );
        Ok(())
    }
    fn interrupted_matrix(forward: bool) {
        let mut baseline = Port::new(forward);
        let mut j = baseline.begin().unwrap();
        assert_eq!(
            recover_file_only(&mut baseline, &mut j).unwrap(),
            if forward {
                FileRecoveryDecision::FilesForwardComplete
            } else {
                FileRecoveryDecision::FilesRestored
            }
        );
        let count = baseline.calls;
        assert_targets(&baseline, forward);
        for point in 1..=count {
            for after in [false, true] {
                let mut port = Port::new(forward);
                port.fail = Some((point, after));
                let result = port
                    .begin()
                    .and_then(|mut journal| recover_file_only(&mut port, &mut journal));
                assert_eq!(
                    result,
                    Err(NativeError::OutcomeUnknown),
                    "point {point}, after {after}"
                );
                let world = port.world.clone();
                let mut reopened = Port {
                    world,
                    status: port.status,
                    data: false,
                    timely: true,
                    calls: 0,
                    fail: None,
                };
                let mut journal = match &reopened.world.journal {
                    Some(_) => reopened.journal_copy(),
                    None => reopened.begin().unwrap(),
                };
                let result = recover_file_only(&mut reopened, &mut journal).unwrap();
                assert_eq!(
                    result,
                    if forward {
                        FileRecoveryDecision::FilesForwardComplete
                    } else {
                        FileRecoveryDecision::FilesRestored
                    }
                );
                assert_targets(&reopened, forward);
                independent_epoch(&reopened.world, forward).unwrap();
                assert_eq!(
                    reopened.world.terminal_effects,
                    ["copy", "catalog", "outer"]
                );
            }
        }
    }
    #[test]
    fn mid_backup_rolls_back_files_before_independent_a4c_start() {
        interrupted_matrix(false);
        let mut p = Port::new(false);
        // A missing original remains missing after rollback; it never becomes an approved image.
        p.world
            .operation
            .role_mut(PayloadRole::Ctl)
            .unwrap()
            .original = OriginalLeaf::Missing;
        p.world.views[3].fixed = FixedObservation::Missing;
        p.world.fixed_contents[3] = None;
        let mut journal = p.begin().unwrap();
        assert_eq!(
            recover_file_only(&mut p, &mut journal),
            Ok(FileRecoveryDecision::FilesRestored)
        );
        assert_eq!(p.world.views[3].fixed, FixedObservation::Missing);
        assert_eq!(
            independent_epoch(&p.world, false),
            Err(NativeError::Foreign)
        );
    }
    #[test]
    fn mid_publish_finishes_files_before_independent_a4c_start() {
        interrupted_matrix(true);
        let mut repaired = Port::new(true);
        let mut journal = repaired.begin().unwrap();
        recover_file_only(&mut repaired, &mut journal).unwrap();
        independent_epoch(&repaired.world, true).unwrap();
        repaired.world.fixed_contents[0] = Some(facts(99));
        assert_eq!(
            independent_epoch(&repaired.world, true),
            Err(NativeError::Foreign)
        );
        for case in 0..5 {
            let mut p = Port::new(true);
            match case {
                0 => p.world.views[2].staged = StageObservation::Missing,
                1 => p.world.views[2].staged = StageObservation::Unknown,
                2 => p.world.views[2].staged = StageObservation::Ready(new_image(3)),
                3 => p.world.views[2].backup = Some(stamp(22)),
                _ => p.world.views[0].staged = StageObservation::Ready(new_image(0)),
            }
            assert!(p.begin().is_err());
            assert_eq!(p.world.writes, 0);
            assert!(p.world.renames.is_empty());
        }
    }
    #[test]
    fn still_live_prior_logon_refuses_before_any_file_or_journal_effect() {
        let mut p = Port::new(false);
        p.status = 0;
        p.data = true;
        assert!(p.begin().is_err());
        assert_eq!(p.world.writes, 0);
        assert!(p.world.renames.is_empty());
        assert_eq!(
            file_recovery_prior_logon(&p.world.selection, &token(true)),
            Ok(None)
        );
        let mut reused = token(false);
        reused.authentication_id = 17;
        assert_eq!(
            file_recovery_prior_logon(&p.world.selection, &reused),
            Err(NativeError::Foreign)
        );
        reused = token(false);
        reused.logon = token(true).logon;
        assert_eq!(
            file_recovery_prior_logon(&p.world.selection, &reused),
            Err(NativeError::Foreign)
        );
    }
    #[test]
    fn missing_or_changed_lineage_never_yields_file_authority() {
        let mut p = Port::new(false);
        let journal = p.world.plan().unwrap();
        assert!(FileRecoveryJournal::decode(b"{}").is_err());
        assert!(
            journal
                .matches_sources(
                    &p.world.selection,
                    FileRecordStamp::new(stamp(82), [80; 32]).unwrap(),
                    &p.world.operation,
                    FileRecordStamp::new(stamp(81), [81; 32]).unwrap()
                )
                .is_err()
        );
        assert!(
            journal
                .matches_sources(
                    &p.world.selection,
                    FileRecordStamp::new(stamp(80), [80; 32]).unwrap(),
                    &p.world.operation,
                    FileRecordStamp::new(stamp(81), [82; 32]).unwrap()
                )
                .is_err()
        );
        // A rewritten same-operation pointer must not take the ordinary current-context bypass
        // ahead of exact pending source matching. The classifier alone returns None here.
        let mut rewritten = serde_json::to_value(&p.world.selection).unwrap();
        rewritten["context"] =
            serde_json::to_value(OuterContextCorrelation::new(&token(false)).unwrap()).unwrap();
        let rewritten: OuterUpgradeRecord = serde_json::from_value(rewritten).unwrap();
        assert_eq!(
            file_recovery_prior_logon(&rewritten, &token(false)),
            Ok(None)
        );
        assert_eq!(
            journal
                .matches_sources(
                    &rewritten,
                    journal.outer_record(),
                    &p.world.operation,
                    journal.operation_record()
                )
                .and_then(|_| file_recovery_prior_logon(&rewritten, &token(false))),
            Err(NativeError::Foreign)
        );
        let mut operation = p.world.operation.clone();
        operation.set_phase(Phase::Unknown);
        assert!(
            journal
                .matches_sources(
                    &p.world.selection,
                    journal.outer_record(),
                    &operation,
                    journal.operation_record()
                )
                .is_err()
        );
        let mut alien = token(false);
        alien.user = sid(&[21, 99]);
        assert_eq!(
            file_recovery_prior_logon(&p.world.selection, &alien),
            Err(NativeError::Foreign)
        );
        let mut body = serde_json::to_value(&journal).unwrap();
        body["healthy"] = serde_json::json!(true);
        let bytes =
            native_io::records::encode_record(&native_io::records::RecordName::FileRecovery, body)
                .unwrap();
        assert!(FileRecoveryJournal::decode(&bytes).is_err());
        p.world.views[2].unknown_backup = true;
        assert!(p.begin().is_err());
        assert_eq!(p.world.writes, 0);
    }
    #[test]
    fn denied_ambiguous_or_late_lsa_results_refuse_without_file_effects() {
        for (status, data, timely) in [
            (0xC0000022u32 as i32, false, true),
            (0, false, true),
            (0xC000005Fu32 as i32, true, true),
            (0xC000005Fu32 as i32, false, false),
            (-1, false, true),
        ] {
            let mut p = Port::new(true);
            p.status = status;
            p.data = data;
            p.timely = timely;
            assert!(p.begin().is_err());
            assert_eq!(p.world.writes, 0);
            assert!(p.world.renames.is_empty());
        }
    }
    #[test]
    fn terminal_recovery_is_read_only_and_never_replays_effects() {
        for forward in [false, true] {
            let mut p = Port::new(forward);
            let mut journal = p.begin().unwrap();
            recover_file_only(&mut p, &mut journal).unwrap();
            let writes = p.world.writes;
            let renames = p.world.renames.clone();
            let effects = p.world.terminal_effects.clone();
            let mut reopened = p.journal_copy();
            assert_eq!(reopened.cursor(), FileRecoveryCursor::Retired);
            recover_file_only(&mut p, &mut reopened).unwrap();
            assert_eq!(p.world.writes, writes);
            assert_eq!(p.world.renames, renames);
            assert_eq!(p.world.terminal_effects, effects);
            // A new conflicting pointer or changed converged fixed image is not cleared.
            p.world.outer = true;
            assert_eq!(
                recover_file_only(&mut p, &mut reopened),
                Err(NativeError::Foreign)
            );
            assert_eq!(p.world.writes, writes);
            p.world.outer = false;
            p.world.views[0].fixed = FixedObservation::Unknown;
            assert_eq!(
                recover_file_only(&mut p, &mut reopened),
                Ok(FileRecoveryDecision::Retained)
            );
            assert_eq!(p.world.writes, writes);
        }
    }
    #[test]
    fn exit_receipt_reads_state_parent_once_and_never_runtime_fallback() {
        let local = r"C:\fixture\LocalAppData";
        let state = format!("{local}\\Crosspane");
        let runtime = format!("{state}\\runtime");
        let receipt = |id| {
            serde_json::to_vec(&serde_json::json!({"schema_version":1,"instance_id":id,
            "stopped_unix_ms":500,"clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true})).unwrap()
        };
        let mut leaves = BTreeMap::new();
        leaves.insert(
            (state.clone(), "last_exit.json".to_string()),
            receipt(41u64),
        );
        leaves.insert((runtime, "last_exit.json".to_string()), receipt(99u64));
        let mut calls = 0;
        let bytes = native_io::with_state_exit_receipt(local, |root, leaf| {
            calls += 1;
            leaves
                .get(&(root.to_string(), leaf.to_string()))
                .cloned()
                .ok_or(NativeError::Missing)
        })
        .unwrap();
        let parsed = agent_contract::parse_last_exit(&bytes).unwrap();
        assert_eq!(parsed.instance_id, 41);
        assert_eq!(parsed.stopped_unix_ms, 500);
        assert!(parsed.clean);
        assert_eq!(calls, 1);
        leaves.remove(&(state, "last_exit.json".to_string()));
        calls = 0;
        assert_eq!(
            native_io::with_state_exit_receipt(local, |root, leaf| {
                calls += 1;
                leaves
                    .get(&(root.to_string(), leaf.to_string()))
                    .cloned()
                    .ok_or(NativeError::Missing)
            }),
            Err(NativeError::Missing)
        );
        assert_eq!(calls, 1);
    }
}
