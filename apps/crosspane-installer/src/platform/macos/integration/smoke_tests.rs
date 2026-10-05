#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The production adapters over a scratch target, with injected probes and no commands.
//!
//! Only the adapters that need no payload tree or launchd are exercised here: support and the
//! agent admission, plus the refusals of the install and removal adapters when what is next to the
//! installer doesn't match the build's approved inventory. These run in the crate because an
//! approved inventory can only be injected through the private construction seam; production
//! reads it from the build alone and can't construct any of this without probes.

use std::collections::BTreeMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_installer_core::{
    JobIntent, JobStage, ObservationSource, OperationId, StepId, WaitKind,
};

use super::native::{MacProbes, NativeEnv};
use super::{INSTALL, MacPlatform, SUPPORT};
use crate::agent_contract::{AgentCall, AgentReply, CallFailure, InstallerRequest};
use crate::live::{
    self, Availability, MaintenanceId, MaintenanceReport, MaintenanceRequest, NativeJob,
    NativeOutcome, NativeReport, Platform, StatusEvidence, StepReport,
};
use crate::platform::macos::launch_agent::UnobservableApproval;
use crate::platform::macos::native_io::{
    CommandOutput, CommandRunner, CommandSpec, Deadline, GuiObservation, MacTarget, MonotonicClock,
    NativeError, NativeResult, SignatureObservation, SignatureProbe, SigningRequirement,
    SupportObservation, SupportProbe, TargetPaths,
};
use crate::platform::macos::payload::{ApprovedInventory, PayloadFile, PayloadRole, SigningRule};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scratch {
    root: PathBuf,
    home: PathBuf,
    tmp: PathBuf,
    payload: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/private/tmp/crosspane-wp421-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let make = |path: &Path| {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        let home = root.join("home");
        let tmp = root.join("tmp");
        let payload = home.join("payload");
        make(&root);
        make(&home);
        make(&tmp);
        make(&payload);
        let bin = payload.join("Crosspane.app/Contents/MacOS");
        make(&payload.join("Crosspane.app"));
        make(&payload.join("Crosspane.app/Contents"));
        make(&bin);
        std::fs::write(bin.join("Crosspane"), b"not a real agent").unwrap();
        std::fs::set_permissions(
            bin.join("Crosspane"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        Self {
            root,
            home,
            tmp,
            payload,
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Facts(Mutex<SupportObservation>);

impl SupportProbe for Facts {
    fn observe(&self, _: &Deadline) -> NativeResult<SupportObservation> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Signatures {
    verified: Mutex<bool>,
}

impl SignatureProbe for Signatures {
    fn observe(
        &self,
        _: &Path,
        approved: &SigningRequirement,
        _: &Deadline,
    ) -> NativeResult<SignatureObservation> {
        Ok(SignatureObservation {
            strict_verified: *self.verified.lock().unwrap(),
            team_identifier: "ABCDE12345".into(),
            identifier: approved.identifier.clone(),
            designated_requirement: approved.designated_requirement.clone(),
            entitlements: approved.entitlements.clone(),
            apple_development: true,
            hardened_runtime: true,
            ad_hoc: false,
        })
    }
}

struct NoCommands;

impl CommandRunner for NoCommands {
    fn run(&self, _: &CommandSpec, _: &Deadline) -> NativeResult<CommandOutput> {
        Err(NativeError::Unavailable)
    }
}

fn inventory() -> ApprovedInventory {
    ApprovedInventory {
        product_version: "0.0.1".into(),
        features: Vec::new(),
        files: vec![PayloadFile {
            path: "Crosspane.app/Contents/MacOS/Crosspane".into(),
            size: 16,
            sha256: [7; 32],
            mode: 0o755,
            signing: Some(SigningRule {
                role: PayloadRole::Agent,
                identifier: "io.frostdev.crosspane.agent".into(),
                designated_requirement: "identifier \"io.frostdev.crosspane.agent\"".into(),
                entitlements: BTreeMap::new(),
            }),
        }],
    }
}

struct Setup {
    scratch: Scratch,
    facts: Arc<Facts>,
    signatures: Arc<Signatures>,
    platform: MacPlatform,
}

fn setup(major: u16) -> Setup {
    let scratch = Scratch::new();
    let uid = rustix::process::geteuid().as_raw();
    let facts = Arc::new(Facts(Mutex::new(SupportObservation {
        macos_major: major,
        apple_silicon: true,
        gui: GuiObservation {
            console_uid: Some(uid),
            interactive_uid: Some(uid),
            console_session: "100".into(),
            interactive_session: "100".into(),
            active: true,
        },
        gui_tmpdir: scratch.tmp.clone(),
    })));
    let signatures = Arc::new(Signatures {
        verified: Mutex::new(true),
    });
    let target = MacTarget::scratch(TargetPaths {
        uid,
        home: scratch.home.clone(),
        gui_tmpdir: scratch.tmp.clone(),
        runtime_override: None,
        payload_root: scratch.payload.clone(),
    })
    .unwrap();
    let mono = Arc::new(MonotonicClock::default());
    let probes = MacProbes {
        support: facts.clone(),
        signatures: signatures.clone(),
        approval: Arc::new(UnobservableApproval),
        runner: Arc::new(NoCommands),
    };
    let env = NativeEnv::new(target, inventory(), probes, mono.clone());
    let clock: live::Clock = {
        let mono = mono.clone();
        Arc::new(move || {
            use crate::platform::macos::native_io::Clock;
            mono.now_ms()
        })
    };
    let platform = MacPlatform::native(clock, env).unwrap();
    Setup {
        scratch,
        facts,
        signatures,
        platform,
    }
}

fn run(
    s: &mut Setup,
    step: StepId,
    stage: JobStage,
    op: u64,
    status: Option<AgentReply>,
) -> StepReport {
    s.platform
        .submit(NativeJob::Step {
            job: JobIntent {
                step,
                operation: OperationId(op),
                stage,
            },
            consent: None,
            status: status.map(StatusEvidence),
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        for report in s.platform.poll() {
            if let NativeReport::Step(r) = report {
                return r;
            }
        }
        assert!(Instant::now() < end, "the worker never answered");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_scratch_target_is_supported_but_never_live() {
    let mut s = setup(26);
    let detect = run(&mut s, SUPPORT, JobStage::Detect, 1, None);
    assert_eq!(
        detect.outcome,
        NativeOutcome::Detected {
            needs_action: false
        },
        "{detect:?}"
    );
    // A scratch target observes as Demo, which core refuses: nothing here can ever count.
    let verify = run(&mut s, SUPPORT, JobStage::Verify, 2, None);
    assert!(
        matches!(
            verify.outcome,
            NativeOutcome::Verified {
                source: ObservationSource::Demo,
                ..
            }
        ),
        "{verify:?}"
    );
    let _ = &s.scratch.root;
}

#[test]
fn an_old_macos_or_a_bad_signature_is_unsupported_and_a_probe_failure_is_only_pending() {
    let mut s = setup(25);
    let old = run(&mut s, SUPPORT, JobStage::Detect, 1, None);
    assert_eq!(old.outcome, NativeOutcome::Unsupported, "{old:?}");
    s.facts.0.lock().unwrap().macos_major = 26;
    *s.signatures.verified.lock().unwrap() = false;
    let unsigned = run(&mut s, SUPPORT, JobStage::Detect, 2, None);
    assert_eq!(unsigned.outcome, NativeOutcome::Unsupported, "{unsigned:?}");
    assert!(unsigned.detail.contains("Nothing will be changed"));
    // The install adapter is gated on support too: it is never asked while unsupported.
    let install = run(&mut s, INSTALL, JobStage::Detect, 3, None);
    assert_eq!(install.outcome, NativeOutcome::Unsupported, "{install:?}");
}

#[test]
fn an_install_that_does_not_match_the_approved_inventory_is_a_blocked_build_not_a_failure() {
    let mut s = setup(26);
    // The payload's agent file exists but is not the approved file (wrong size and hash):
    // the adapters refuse to admit it, and the worker reports a blocked build.
    let detect = run(&mut s, INSTALL, JobStage::Detect, 1, None);
    assert_eq!(
        detect.outcome,
        NativeOutcome::Waiting(WaitKind::User),
        "{detect:?}"
    );
    assert!(
        detect
            .detail
            .contains("don't match what this build approved"),
        "{detect:?}"
    );
    let plan = run(&mut s, INSTALL, JobStage::Plan, 2, None);
    assert_eq!(
        plan.outcome,
        NativeOutcome::Waiting(WaitKind::User),
        "{plan:?}"
    );
}

#[test]
fn with_no_agent_running_every_agent_call_fails_after_an_honest_admission_attempt() {
    let mut s = setup(26);
    s.platform
        .agent()
        .submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: 5_000,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let replies = loop {
        let replies = s.platform.agent().poll();
        if !replies.is_empty() {
            break replies;
        }
        assert!(Instant::now() < end, "no failure reply");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(replies[0].result, Err(CallFailure::Unavailable));
}

#[test]
fn removal_is_not_offered_when_the_install_cannot_be_read() {
    let mut s = setup(26);
    let id = MaintenanceId(1);
    s.platform
        .submit(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }))
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let mut found = None;
        for report in s.platform.poll() {
            if let NativeReport::Maintenance(m) = report {
                found = Some(m);
            }
        }
        if let Some(report) = found {
            match report {
                MaintenanceReport::Inspected {
                    uninstall: Availability::Unavailable(text),
                    choices,
                    ..
                } => {
                    assert!(text.contains("Nothing was changed"), "{text}");
                    assert!(choices.is_empty());
                    return;
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(Instant::now() < end, "the worker never answered");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn maintenance_answer(s: &mut Setup, request: MaintenanceRequest) -> Vec<MaintenanceReport> {
    s.platform.submit(NativeJob::Maintenance(request)).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let found: Vec<MaintenanceReport> = s
            .platform
            .poll()
            .into_iter()
            .filter_map(|r| match r {
                NativeReport::Maintenance(m) => Some(m),
                NativeReport::Step(_) => None,
            })
            .collect();
        if !found.is_empty() {
            return found;
        }
        assert!(Instant::now() < end, "the worker never answered");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn repair_over_the_production_adapter_is_refused_with_guidance_when_the_install_cannot_be_read() {
    let mut s = setup(26);
    let id = MaintenanceId(1);
    let reports = maintenance_answer(&mut s, MaintenanceRequest::Inspect { id });
    let MaintenanceReport::Inspected {
        repair: Availability::Unavailable(text),
        ..
    } = &reports[0]
    else {
        panic!("{reports:?}")
    };
    assert!(text.contains("Nothing was changed"), "{text}");
    assert_eq!(reports.len(), 1, "no earlier repair to resume");
    // A plan, a confirmation and a resume are refused the same way, and change nothing.
    for request in [
        MaintenanceRequest::PlanRepair { id, status: None },
        MaintenanceRequest::ConfirmRepair {
            id,
            plan: 1,
            revision: 3,
            status: None,
        },
        MaintenanceRequest::ResumeRepair { id, status: None },
    ] {
        let reports = maintenance_answer(&mut s, request);
        let MaintenanceReport::Refused { reason, .. } = &reports[0] else {
            panic!("{reports:?}")
        };
        assert!(reason.contains("Nothing was changed"), "{reason}");
    }
}
