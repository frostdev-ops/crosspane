#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Mac binding against in-memory native domains. Fakes are test-only construction (`Parts`
//! with injected domains); the production `open` composes none of them, and a build without probes
//! composes only `Blocked` domains. No real agent, fixture, GUI session, launchd, TCC, Keychain,
//! installer package or audio device is used.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_installer::agent_contract::*;
use crosspane_installer::live::{
    self, Availability, Consent, LiveController, MaintenanceId, MaintenanceOutcome,
    MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome, NativeReport, Platform,
    RepairOutcome, StatusEvidence, StepReport,
};
use crosspane_installer::platform::macos::integration::{
    Admitted, AgentBackend, AgentSource, Agents, AudioError, AudioPackages, AudioPreview,
    AudioState, Blocked, DomainFactory, Domains, InstallApplied, InstallError, InstallPreview,
    InstallState, Installs, MacPlatform, Parts, RepairFinish, RepairOffer, RepairStep, Repairer,
    Support, SupportOutcome, UninstallOffer, UninstallResult, Uninstaller,
};
use crosspane_installer::platform::macos::native_io::{Deadline, NativeError};
use crosspane_installer::platform::macos::transport::SelectedLink;
use crosspane_installer_core::{
    ApplyOutcome, JobIntent, JobStage, ObservationSource, OperationId, StepId, WaitKind,
};
use crosspane_types::id::NodeId;
use serde_json::{Value, json};

const SUPPORT: StepId = StepId(10);
const INSTALL: StepId = StepId(20);
const AGENT: StepId = StepId(21);
const PERMISSIONS: StepId = StepId(30);
const AUDIO: StepId = StepId(31);

fn peer() -> NodeId {
    NodeId([0x22; 32])
}

fn local() -> NodeId {
    NodeId([0x11; 32])
}

// ---- the world the fake domains share ---------------------------------------------------------

struct World {
    support: SupportOutcome,
    installed: bool,
    detect: Option<Result<InstallState, InstallError>>,
    plan: Option<Result<Option<InstallPreview>, InstallError>>,
    apply: Option<Result<InstallApplied, InstallError>>,
    verify: Option<Result<ObservationSource, InstallError>>,
    audio_installed: bool,
    audio_opened: bool,
    audio_detect: Option<Result<AudioState, AudioError>>,
    audio_plan: Option<Result<AudioPreview, AudioError>>,
    audio_apply: Option<Result<(), AudioError>>,
    audio_verify: Option<Result<ObservationSource, AudioError>>,
    admit: Result<(), String>,
    /// Admissions wait here until the test opens the gate.
    gate_open: bool,
    offer: UninstallOffer,
    uninstall_plan: Result<String, String>,
    uninstall_apply: Result<UninstallResult, String>,
    /// What repair offers, and the scripted answers of each repair stage.
    repair_offer: RepairOffer,
    repair_plan: Result<String, String>,
    repair_confirm: VecDeque<Result<RepairStep, String>>,
    repair_verify: VecDeque<RepairStep>,
    repair_resume: Result<RepairFinish, String>,
    repair_confirmed: Vec<(u64, u64)>,
    repair_verified: Vec<Option<u64>>,
    /// Verification keeps waiting while set; a confirmation blocks (a slow stage) while set.
    repair_hold: bool,
    repair_hold_confirm: bool,
    calls: Vec<String>,
    /// The Status id each domain call was given, by call name.
    seen_status: Vec<(String, Option<u64>)>,
    links: Vec<Option<SelectedLink>>,
}

type Shared = Arc<Mutex<World>>;

impl World {
    fn new() -> Shared {
        Arc::new(Mutex::new(Self {
            support: SupportOutcome::Supported(ObservationSource::Live),
            installed: false,
            detect: None,
            plan: None,
            apply: None,
            verify: None,
            audio_installed: false,
            audio_opened: false,
            audio_detect: None,
            audio_plan: None,
            audio_apply: None,
            audio_verify: None,
            admit: Ok(()),
            gate_open: true,
            offer: UninstallOffer {
                uninstall: Availability::Available,
                choices: vec![
                    live::RemovalChoice {
                        id: 1,
                        role: crosspane_installer::view::ToggleRole::DeleteIdentity,
                        label: "Also forget this Mac's pairings".into(),
                        checked: false,
                        enabled: true,
                    },
                    live::RemovalChoice {
                        id: 2,
                        role: crosspane_installer::view::ToggleRole::RemoveAudioDriver,
                        label: "Remove the Crosspane audio driver".into(),
                        checked: true,
                        enabled: true,
                    },
                ],
            },
            uninstall_plan: Ok("Remove Crosspane from this account.".into()),
            uninstall_apply: Ok(UninstallResult {
                outcome: MaintenanceOutcome::Removed,
                lines: vec!["Crosspane was removed from this account.".into()],
            }),
            repair_offer: RepairOffer {
                repair: Availability::Available,
                resumable: None,
            },
            repair_plan: Ok(
                "Repair puts back Crosspane's app, its command-line tool and its sign-in item."
                    .into(),
            ),
            repair_confirm: VecDeque::new(),
            repair_verify: VecDeque::new(),
            repair_resume: Ok(finish(
                RepairOutcome::Verified,
                "Resumed and verified.",
                false,
            )),
            repair_confirmed: Vec::new(),
            repair_verified: Vec::new(),
            repair_hold: false,
            repair_hold_confirm: false,
            calls: Vec::new(),
            seen_status: Vec::new(),
            links: Vec::new(),
        }))
    }
}

fn note(w: &Shared, text: impl Into<String>) {
    w.lock().unwrap().calls.push(text.into());
}

fn calls(w: &Shared) -> Vec<String> {
    w.lock().unwrap().calls.clone()
}

fn saw(w: &Shared, call: &str, status: Option<&AgentReply>) {
    w.lock()
        .unwrap()
        .seen_status
        .push((call.to_owned(), status.map(|r| r.id)));
}

struct FakeSupport(Shared);

impl Support for FakeSupport {
    fn observe(&mut self, _: &Deadline) -> SupportOutcome {
        note(&self.0, "support.observe");
        self.0.lock().unwrap().support.clone()
    }
}

struct FakeInstalls(Shared);

impl Installs for FakeInstalls {
    fn detect(
        &mut self,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<InstallState, InstallError> {
        note(&self.0, "install.detect");
        saw(&self.0, "install.detect", status);
        let w = self.0.lock().unwrap();
        if let Some(over) = w.detect.clone() {
            return over;
        }
        Ok(if w.installed {
            InstallState::Current
        } else {
            InstallState::Needed
        })
    }

    fn plan(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<Option<InstallPreview>, InstallError> {
        note(&self.0, format!("install.plan:{}", operation.0));
        saw(&self.0, "install.plan", status);
        let w = self.0.lock().unwrap();
        if let Some(over) = w.plan.clone() {
            return over;
        }
        Ok((!w.installed).then(|| InstallPreview {
            version: "0.0.1".into(),
            interrupts_agent: false,
            replacing: false,
        }))
    }

    fn apply(
        &mut self,
        operation: OperationId,
        _: &Deadline,
    ) -> Result<InstallApplied, InstallError> {
        note(&self.0, format!("install.apply:{}", operation.0));
        let mut w = self.0.lock().unwrap();
        if let Some(over) = w.apply.clone() {
            return over;
        }
        w.installed = true;
        Ok(InstallApplied::Requested)
    }

    fn verify(
        &mut self,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<ObservationSource, InstallError> {
        note(&self.0, "install.verify");
        saw(&self.0, "install.verify", status);
        let w = self.0.lock().unwrap();
        if let Some(over) = w.verify.clone() {
            return over;
        }
        if w.installed {
            Ok(ObservationSource::Live)
        } else {
            Err(InstallError::Unavailable)
        }
    }
}

struct FakeAudio(Shared);

impl AudioPackages for FakeAudio {
    fn detect(&mut self, _: &Deadline) -> Result<AudioState, AudioError> {
        note(&self.0, "audio.detect");
        let w = self.0.lock().unwrap();
        if let Some(over) = w.audio_detect.clone() {
            return over;
        }
        Ok(if w.audio_installed {
            AudioState::Installed
        } else if w.audio_opened {
            AudioState::InProgress
        } else {
            AudioState::Needed
        })
    }

    fn plan(&mut self, operation: OperationId, _: &Deadline) -> Result<AudioPreview, AudioError> {
        note(&self.0, format!("audio.plan:{}", operation.0));
        let w = self.0.lock().unwrap();
        if let Some(over) = w.audio_plan.clone() {
            return over;
        }
        Ok(AudioPreview {
            version: "1.0.0".into(),
            interrupts_system_audio: true,
            keeps_previous: false,
        })
    }

    fn apply(&mut self, operation: OperationId, _: &Deadline) -> Result<(), AudioError> {
        note(&self.0, format!("audio.apply:{}", operation.0));
        let mut w = self.0.lock().unwrap();
        if let Some(over) = w.audio_apply.clone() {
            return over;
        }
        w.audio_opened = true;
        Ok(())
    }

    fn verify(&mut self, _: &Deadline) -> Result<ObservationSource, AudioError> {
        note(&self.0, "audio.verify");
        let w = self.0.lock().unwrap();
        if let Some(over) = w.audio_verify.clone() {
            return over;
        }
        if w.audio_installed {
            Ok(ObservationSource::Live)
        } else if w.audio_opened {
            Err(AudioError::Waiting)
        } else {
            Err(AudioError::Unavailable)
        }
    }
}

struct FakeAgents(Shared);

impl Agents for FakeAgents {
    fn admit(&mut self, link: Option<SelectedLink>, _: &Deadline) -> Result<Admitted, String> {
        note(&self.0, "agents.admit");
        loop {
            {
                let mut w = self.0.lock().unwrap();
                if w.gate_open {
                    w.links.push(link);
                    return w.admit.clone().map(|()| Admitted::injected());
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

struct FakeUninstaller(Shared);

impl Uninstaller for FakeUninstaller {
    fn inspect(&mut self, _: &Deadline) -> UninstallOffer {
        note(&self.0, "uninstall.inspect");
        self.0.lock().unwrap().offer.clone()
    }

    fn plan(
        &mut self,
        choices: &[(u16, bool)],
        operation: OperationId,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<String, String> {
        note(
            &self.0,
            format!("uninstall.plan:{choices:?}:{}", operation.0),
        );
        saw(&self.0, "uninstall.plan", status);
        self.0.lock().unwrap().uninstall_plan.clone()
    }

    fn apply(
        &mut self,
        operation: OperationId,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<UninstallResult, String> {
        note(&self.0, format!("uninstall.apply:{}", operation.0));
        saw(&self.0, "uninstall.apply", status);
        self.0.lock().unwrap().uninstall_apply.clone()
    }
}

fn finish(outcome: RepairOutcome, line: &str, resumable: bool) -> RepairFinish {
    RepairFinish {
        outcome,
        lines: vec![line.into()],
        resumable,
    }
}

fn status_id(status: Option<&AgentReply>) -> Option<u64> {
    status
        .filter(|r| {
            matches!(
                r.result,
                Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
            )
        })
        .map(|r| r.id)
}

struct FakeRepairer(Shared);

impl Repairer for FakeRepairer {
    fn inspect(&mut self, _: &Deadline) -> RepairOffer {
        note(&self.0, "repair.inspect");
        self.0.lock().unwrap().repair_offer.clone()
    }

    fn plan(
        &mut self,
        status: Option<&AgentReply>,
        operation: OperationId,
        _: &Deadline,
    ) -> Result<String, String> {
        note(
            &self.0,
            format!(
                "repair.plan(op={},status={:?})",
                operation.0,
                status_id(status)
            ),
        );
        self.0.lock().unwrap().repair_plan.clone()
    }

    fn confirm(
        &mut self,
        status: Option<&AgentReply>,
        plan: OperationId,
        operation: OperationId,
        _: &Deadline,
    ) -> Result<RepairStep, String> {
        let end = Instant::now() + Duration::from_secs(30);
        while self.0.lock().unwrap().repair_hold_confirm && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut w = self.0.lock().unwrap();
        w.calls.push(format!(
            "repair.confirm(plan={},status={:?})",
            plan.0,
            status_id(status)
        ));
        w.repair_confirmed.push((plan.0, operation.0));
        w.repair_confirm.pop_front().unwrap_or_else(|| {
            Ok(RepairStep::Waiting {
                detail: "Crosspane was started again. Waiting for the new instance…".into(),
                if_timed_out: finish(
                    RepairOutcome::OutcomeUnknown,
                    "The new Crosspane didn't report healthy in time.",
                    true,
                ),
                closeable: false,
            })
        })
    }

    fn verify(&mut self, status: Option<&AgentReply>, _: &Deadline) -> RepairStep {
        let mut w = self.0.lock().unwrap();
        w.calls
            .push(format!("repair.verify(status={:?})", status_id(status)));
        w.repair_verified.push(status_id(status));
        if w.repair_hold {
            return RepairStep::Waiting {
                detail: "Waiting for the new instance to report healthy…".into(),
                if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health in time.", true),
                closeable: false,
            };
        }
        w.repair_verify.pop_front().unwrap_or_else(|| {
            RepairStep::Finished(finish(
                RepairOutcome::Verified,
                "The new Crosspane reported healthy.",
                false,
            ))
        })
    }

    fn resume(
        &mut self,
        status: Option<&AgentReply>,
        _: &Deadline,
    ) -> Result<RepairFinish, String> {
        note(
            &self.0,
            format!("repair.resume(status={:?})", status_id(status)),
        );
        self.0.lock().unwrap().repair_resume.clone()
    }
}

fn fake_domains(w: &Shared) -> DomainFactory {
    let shared = w.clone();
    Box::new(move || Domains {
        support: Box::new(FakeSupport(shared.clone())),
        installs: Box::new(FakeInstalls(shared.clone())),
        audio: Box::new(FakeAudio(shared.clone())),
        agents: Box::new(FakeAgents(shared.clone())),
        uninstaller: Box::new(FakeUninstaller(shared.clone())),
        repairer: Box::new(FakeRepairer(shared)),
    })
}

// ---- a scripted agent backend and fixtures ----------------------------------------------------

#[derive(Default)]
struct BackendState {
    queue: AgentQueue,
    adopted: u32,
    busy: bool,
    refuse: bool,
}

struct FakeBackend(Arc<Mutex<BackendState>>);

impl AgentPort for FakeBackend {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        self.0.lock().unwrap().queue.submit(call)
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        self.0.lock().unwrap().queue.poll()
    }
}

impl AgentBackend for FakeBackend {
    fn idle(&self) -> bool {
        !self.0.lock().unwrap().busy
    }
    fn adopt(&mut self, _: Admitted) -> Result<(), NativeError> {
        let mut state = self.0.lock().unwrap();
        if state.refuse {
            return Err(NativeError::Unsupported);
        }
        state.adopted += 1;
        Ok(())
    }
}

struct Rig {
    platform: MacPlatform,
    w: Shared,
    backend: Arc<Mutex<BackendState>>,
    clock: Arc<AtomicU64>,
    next_op: u64,
    steps: Vec<StepReport>,
    maintenance: Vec<MaintenanceReport>,
}

impl Rig {
    fn new() -> Self {
        let w = World::new();
        let domains = fake_domains(&w);
        Self::with(w, domains)
    }

    fn with(w: Shared, domains: DomainFactory) -> Self {
        let clock = Arc::new(AtomicU64::new(10_000));
        let time = clock.clone();
        let backend = Arc::new(Mutex::new(BackendState::default()));
        let platform = MacPlatform::compose(Parts {
            clock: Arc::new(move || time.load(Ordering::SeqCst)),
            domains,
            agent: AgentSource::Injected(Box::new(FakeBackend(backend.clone()))),
        })
        .unwrap();
        Self {
            platform,
            w,
            backend,
            clock,
            next_op: 0,
            steps: Vec::new(),
            maintenance: Vec::new(),
        }
    }

    fn job(&mut self, step: StepId, stage: JobStage) -> JobIntent {
        self.next_op += 1;
        JobIntent {
            step,
            operation: OperationId(self.next_op),
            stage,
        }
    }

    fn send(&mut self, job: NativeJob) {
        self.platform.submit(job).unwrap();
    }

    fn pump(&mut self, until: impl Fn(&Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            for report in self.platform.poll() {
                match report {
                    NativeReport::Step(r) => self.steps.push(r),
                    NativeReport::Maintenance(m) => self.maintenance.push(m),
                }
            }
            if until(self) {
                return;
            }
            assert!(Instant::now() < end, "the worker never answered");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn run(
        &mut self,
        step: StepId,
        stage: JobStage,
        consent: Option<Consent>,
        status: Option<AgentReply>,
    ) -> (JobIntent, StepReport) {
        let job = self.job(step, stage);
        self.run_job(job, consent, status)
    }

    fn run_job(
        &mut self,
        job: JobIntent,
        consent: Option<Consent>,
        status: Option<AgentReply>,
    ) -> (JobIntent, StepReport) {
        let before = self.steps.len();
        self.send(NativeJob::Step {
            job: job.clone(),
            consent,
            status: status.map(StatusEvidence),
        });
        self.pump(|r| r.steps.len() > before);
        (job, self.steps.last().unwrap().clone())
    }

    fn outcome(&mut self, step: StepId, stage: JobStage) -> NativeOutcome {
        self.run(step, stage, None, None).1.outcome
    }

    /// Plan then apply with a consent that names that plan; return the apply outcome.
    fn consented_install(&mut self) -> NativeOutcome {
        let (plan_job, plan) = self.run(INSTALL, JobStage::Plan, None, None);
        assert!(
            matches!(plan.outcome, NativeOutcome::Planned { .. }),
            "{:?}",
            plan.outcome
        );
        let apply = self.job(INSTALL, JobStage::Apply);
        let consent = Consent {
            plan: plan_job.operation,
            operation: apply.operation,
            revision: 7,
        };
        self.run_job(apply, Some(consent), None).1.outcome
    }

    fn set(&self, change: impl FnOnce(&mut World)) {
        change(&mut self.w.lock().unwrap());
    }

    fn calls(&self) -> Vec<String> {
        calls(&self.w)
    }

    fn count(&self, name: &str) -> usize {
        self.calls().iter().filter(|c| c.starts_with(name)).count()
    }

    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.platform.shutdown();
    }
}

// ---- Mac statuses -----------------------------------------------------------------------------

fn health_json(permissions: [&str; 3], keystore: &str, recovery: u64, audio: bool) -> Value {
    let mut v: Value = serde_json::from_str(
        r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[{"id":1,"name":"local panel","pixels":[2560,1440],"scale":1.0,"mm":[600.0,340.0],"origin":[0.0,0.0]}],
"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.1","features":["private-vdisplay","video"]},"instance":{"id":9,"pid":4242,"uid":501,
"exe":"/Users/u/Applications/Crosspane.app/Contents/MacOS/Crosspane","runtime_dir":"/private/tmp/crosspane","started_unix_ms":1790942400000},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"nothing_parked","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"backends":[
{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
{"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
{"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},
{"name":"discovery","state":"ready","reason":null}],"keystore":"os_store","permissions":[],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#,
    )
    .unwrap();
    let names = ["screen_recording", "accessibility", "input_monitoring"];
    v["result"]["installer"]["permissions"] = json!(
        names
            .iter()
            .zip(permissions)
            .map(|(name, state)| json!({"name": name, "state": state}))
            .collect::<Vec<_>>()
    );
    v["result"]["installer"]["keystore"] = json!(keystore);
    v["result"]["installer"]["recovery_pending"] = json!(recovery);
    v["result"]["installer"]["audio"]["enabled"] = json!(audio);
    v
}

fn status(id: u64, source: ObservationSource, body: Value) -> AgentReply {
    let decoded = match parse_status(&serde_json::to_vec(&body).unwrap(), AgentPlatform::Macos) {
        Ok(admission) => Ok(DecodedReply::Status(admission)),
        Err(_) => Err(CallFailure::Unavailable),
    };
    AgentReply {
        id,
        observed_at_ms: 10_000,
        source,
        result: decoded,
    }
}

const GRANTED: [&str; 3] = ["granted", "granted", "granted"];

fn healthy(id: u64) -> AgentReply {
    status(
        id,
        ObservationSource::Live,
        health_json(GRANTED, "os_store", 0, false),
    )
}

fn with_permissions(id: u64, permissions: [&str; 3], audio: bool) -> AgentReply {
    status(
        id,
        ObservationSource::Live,
        health_json(permissions, "os_store", 0, audio),
    )
}

fn detect_needs_action(outcome: &NativeOutcome) -> Option<bool> {
    match outcome {
        NativeOutcome::Detected { needs_action } => Some(*needs_action),
        _ => None,
    }
}

// ---- support ----------------------------------------------------------------------------------

#[test]
fn an_unsupported_mac_is_refused_and_no_install_call_is_ever_made() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.support = SupportOutcome::Unsupported("This Mac needs macOS 26.".into());
    });
    assert_eq!(
        rig.outcome(SUPPORT, JobStage::Detect),
        NativeOutcome::Unsupported
    );
    assert_eq!(
        rig.outcome(INSTALL, JobStage::Detect),
        NativeOutcome::Unsupported
    );
    assert_eq!(
        rig.outcome(INSTALL, JobStage::Plan),
        NativeOutcome::Unsupported
    );
    let (_, applied) = rig.run(INSTALL, JobStage::Apply, None, None);
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("install."), 0, "{:?}", rig.calls());
}

#[test]
fn an_unprovable_mac_stays_pending_and_changes_nothing() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.support = SupportOutcome::Unavailable("The session can't be proved.".into());
    });
    assert_eq!(
        rig.outcome(SUPPORT, JobStage::Detect),
        NativeOutcome::Waiting(WaitKind::User)
    );
    assert_eq!(
        rig.outcome(INSTALL, JobStage::Detect),
        NativeOutcome::Waiting(WaitKind::User)
    );
    let (_, applied) = rig.run(INSTALL, JobStage::Apply, None, None);
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert!(applied.detail.contains("can't be proved"), "{applied:?}");
    assert_eq!(rig.count("install."), 0);
}

#[test]
fn a_supported_mac_detects_and_verifies_with_the_targets_source() {
    for source in [ObservationSource::Live, ObservationSource::Demo] {
        let mut rig = Rig::new();
        rig.set(|w| w.support = SupportOutcome::Supported(source));
        assert_eq!(
            detect_needs_action(&rig.outcome(SUPPORT, JobStage::Detect)),
            Some(false)
        );
        // A scratch target is never live: core refuses what it observes, so the worker must not
        // dress it up.
        match rig.outcome(SUPPORT, JobStage::Verify) {
            NativeOutcome::Verified { source: s, .. } => assert_eq!(s, source),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn support_has_nothing_to_plan_or_apply() {
    let mut rig = Rig::new();
    assert_eq!(rig.outcome(SUPPORT, JobStage::Plan), NativeOutcome::Failed);
    assert_eq!(rig.outcome(SUPPORT, JobStage::Apply), NativeOutcome::Failed);
}

// ---- install ----------------------------------------------------------------------------------

#[test]
fn install_detection_maps_each_state_to_an_honest_outcome() {
    let cases: Vec<(Result<InstallState, InstallError>, NativeOutcome)> = vec![
        (
            Ok(InstallState::Current),
            NativeOutcome::Detected {
                needs_action: false,
            },
        ),
        (
            Ok(InstallState::Needed),
            NativeOutcome::Detected { needs_action: true },
        ),
        (
            Err(InstallError::Foreign),
            NativeOutcome::Waiting(WaitKind::User),
        ),
        (
            Err(InstallError::UserDisabled),
            NativeOutcome::Waiting(WaitKind::User),
        ),
        (
            Err(InstallError::Unobservable),
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Err(InstallError::Unavailable),
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Err(InstallError::OutcomeUnknown),
            NativeOutcome::Waiting(WaitKind::User),
        ),
        (
            Err(InstallError::Blocked("No approved inventory.".into())),
            NativeOutcome::Waiting(WaitKind::User),
        ),
        (Err(InstallError::Unsupported), NativeOutcome::Unsupported),
        (Err(InstallError::Failed), NativeOutcome::Failed),
    ];
    for (result, expected) in cases {
        let mut rig = Rig::new();
        rig.set(|w| w.detect = Some(result.clone()));
        let (_, report) = rig.run(INSTALL, JobStage::Detect, None, None);
        assert_eq!(report.outcome, expected, "{result:?}");
    }
}

#[test]
fn an_uncleared_or_disabled_install_is_explained_without_ownership_words() {
    let mut rig = Rig::new();
    // WP-4.32: the native adapter clears its install paths itself; this only remains for a
    // failure to do that, and it never speaks of who put what there.
    rig.set(|w| w.detect = Some(Err(InstallError::Foreign)));
    let (_, foreign) = rig.run(INSTALL, JobStage::Detect, None, None);
    assert!(foreign.detail.contains("Check again"), "{foreign:?}");
    for word in ["put there", "left exactly", "didn't create"] {
        assert!(!foreign.detail.contains(word), "{foreign:?}");
    }
    rig.set(|w| w.detect = Some(Err(InstallError::UserDisabled)));
    let (_, disabled) = rig.run(INSTALL, JobStage::Detect, None, None);
    assert!(disabled.detail.contains("Login Items"), "{disabled:?}");
    assert_eq!(rig.count("install.apply"), 0);
}

#[test]
fn install_apply_needs_a_consent_for_the_exact_plan_that_was_previewed() {
    let mut rig = Rig::new();
    let (plan_job, plan) = rig.run(INSTALL, JobStage::Plan, None, None);
    let NativeOutcome::Planned { preview } = &plan.outcome else {
        panic!("{:?}", plan.outcome);
    };
    assert!(preview.contains("0.0.1"), "{preview}");
    assert!(preview.contains("this account only"), "{preview}");

    // A consent that names some other plan is refused before the adapter is asked.
    let wrong = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: OperationId(wrong.operation.0 + 100),
        operation: wrong.operation,
        revision: 7,
    };
    let (_, refused) = rig.run_job(wrong, Some(consent), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("install.apply"), 0);

    // The refused apply also consumed the held plan: even the right consent now needs a new plan.
    let apply = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, late) = rig.run_job(apply, Some(consent), None);
    assert_eq!(late.outcome, NativeOutcome::Applied(ApplyOutcome::Refused));
    assert_eq!(rig.count("install.apply"), 0);

    // A fresh plan and its own consent run exactly once, naming the plan's operation.
    let (plan_job, _) = rig.run(INSTALL, JobStage::Plan, None, None);
    let apply = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, applied) = rig.run_job(apply.clone(), Some(consent), None);
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(ApplyOutcome::Applied)
    );
    assert_eq!(
        rig.calls()
            .iter()
            .filter(|c| c.starts_with("install.apply"))
            .cloned()
            .collect::<Vec<_>>(),
        [format!("install.apply:{}", plan_job.operation.0)]
    );
    // The same consent can't run twice.
    let (_, again) = rig.run_job(apply, Some(consent), None);
    assert_eq!(again.outcome, NativeOutcome::Applied(ApplyOutcome::Refused));
    assert_eq!(rig.count("install.apply"), 1);
}

#[test]
fn install_apply_outcomes_map_honestly_and_unknown_is_never_retried_blindly() {
    let cases: Vec<(Result<InstallApplied, InstallError>, ApplyOutcome)> = vec![
        (Ok(InstallApplied::Requested), ApplyOutcome::Applied),
        (Ok(InstallApplied::Unknown), ApplyOutcome::Unknown),
        (Err(InstallError::OutcomeUnknown), ApplyOutcome::Unknown),
        (Err(InstallError::Foreign), ApplyOutcome::Refused),
        (Err(InstallError::Refused), ApplyOutcome::Refused),
        (Err(InstallError::UserDisabled), ApplyOutcome::Refused),
        (Err(InstallError::Failed), ApplyOutcome::Failed),
    ];
    for (result, expected) in cases {
        let mut rig = Rig::new();
        rig.set(|w| w.apply = Some(result.clone()));
        let outcome = rig.consented_install();
        assert_eq!(outcome, NativeOutcome::Applied(expected), "{result:?}");
        assert_eq!(rig.count("install.apply"), 1, "{result:?}");
    }
}

#[test]
fn an_install_plan_whose_apply_a_gate_refused_can_never_be_consented_again() {
    let mut rig = Rig::new();
    let (plan_job, plan) = rig.run(INSTALL, JobStage::Plan, None, None);
    assert!(matches!(plan.outcome, NativeOutcome::Planned { .. }));
    rig.set(|w| w.support = SupportOutcome::Unsupported("not now".into()));
    let apply = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, refused) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    rig.set(|w| w.support = SupportOutcome::Supported(ObservationSource::Live));
    let again = rig.job(INSTALL, JobStage::Apply);
    let replay = Consent {
        operation: again.operation,
        ..consent
    };
    let (_, report) = rig.run_job(again, Some(replay), None);
    assert_eq!(
        report.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused),
        "a held plan is single use"
    );
    assert_eq!(rig.count("install.apply"), 0);
}

#[test]
fn an_install_consent_for_another_apply_job_is_refused() {
    let mut rig = Rig::new();
    let (plan_job, _) = rig.run(INSTALL, JobStage::Plan, None, None);
    let apply = rig.job(INSTALL, JobStage::Apply);
    let other = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: other.operation,
        revision: 7,
    };
    let (_, report) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        report.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("install.apply"), 0);
}

#[test]
fn a_plan_that_finds_nothing_to_do_says_so_and_holds_no_plan() {
    let mut rig = Rig::new();
    rig.set(|w| w.plan = Some(Ok(None)));
    let (plan_job, plan) = rig.run(INSTALL, JobStage::Plan, None, None);
    // A Plan job is answered with a plan or a wait, never with a detection the controller
    // couldn't map: the step is simply checked again.
    assert_eq!(
        plan.outcome,
        NativeOutcome::Waiting(WaitKind::User),
        "{:?}",
        plan.outcome
    );
    let apply = rig.job(INSTALL, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, report) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        report.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("install.apply"), 0);
}

#[test]
fn an_interrupting_plan_says_the_running_agent_is_stopped_first() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.plan = Some(Ok(Some(InstallPreview {
            version: "0.0.2".into(),
            interrupts_agent: true,
            replacing: false,
        })))
    });
    let (_, plan) = rig.run(INSTALL, JobStage::Plan, None, None);
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!()
    };
    assert!(preview.contains("stopped first"), "{preview}");
    assert!(preview.contains("0.0.2"), "{preview}");
}

#[test]
fn the_status_that_came_with_the_job_reaches_the_adapter_and_only_that_one() {
    let mut rig = Rig::new();
    rig.run(INSTALL, JobStage::Detect, None, Some(healthy(41)));
    rig.run(INSTALL, JobStage::Plan, None, Some(healthy(42)));
    rig.run(INSTALL, JobStage::Verify, None, Some(healthy(43)));
    rig.run(INSTALL, JobStage::Detect, None, None);
    let seen = rig.w.lock().unwrap().seen_status.clone();
    assert_eq!(
        seen,
        [
            ("install.detect".to_owned(), Some(41)),
            ("install.plan".to_owned(), Some(42)),
            ("install.verify".to_owned(), Some(43)),
            ("install.detect".to_owned(), None),
        ]
    );
}

#[test]
fn install_verification_needs_live_confirmation_from_the_adapter() {
    let mut rig = Rig::new();
    // Nothing has been installed: the adapter can't confirm, so this only waits.
    let (_, waiting) = rig.run(INSTALL, JobStage::Verify, None, Some(healthy(5)));
    assert_eq!(
        waiting.outcome,
        NativeOutcome::Waiting(WaitKind::Contract),
        "{waiting:?}"
    );
    rig.set(|w| w.installed = true);
    let (_, verified) = rig.run(INSTALL, JobStage::Verify, None, Some(healthy(6)));
    assert_eq!(
        verified.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            observed_at_ms: 10_000,
        }
    );
    // A Demo-source confirmation stays Demo, which core refuses.
    rig.set(|w| w.verify = Some(Ok(ObservationSource::Demo)));
    let (_, demo) = rig.run(INSTALL, JobStage::Verify, None, Some(healthy(7)));
    assert!(matches!(
        demo.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Demo,
            ..
        }
    ));
    // The adapter's own refusal is reported, never turned into success.
    rig.set(|w| w.verify = Some(Err(InstallError::UserDisabled)));
    let (_, off) = rig.run(INSTALL, JobStage::Verify, None, Some(healthy(8)));
    assert_eq!(off.outcome, NativeOutcome::Waiting(WaitKind::User));
}

// ---- agent ------------------------------------------------------------------------------------

#[test]
fn the_agent_verifies_only_from_a_healthy_keychain_status_issued_for_the_job() {
    let mut rig = Rig::new();
    // No status: nothing is assumed.
    let (_, none) = rig.run(AGENT, JobStage::Verify, None, None);
    assert_eq!(none.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    // A call that failed is not an answer.
    let failed = AgentReply {
        id: 3,
        observed_at_ms: 10_000,
        source: ObservationSource::Live,
        result: Err(CallFailure::Unavailable),
    };
    let (_, down) = rig.run(AGENT, JobStage::Verify, None, Some(failed));
    assert_eq!(down.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    // The key is not in the Keychain.
    let file = status(
        4,
        ObservationSource::Live,
        health_json(GRANTED, "file", 0, false),
    );
    let (_, key) = rig.run(AGENT, JobStage::Verify, None, Some(file));
    assert_eq!(key.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(key.detail.contains("Keychain"), "{key:?}");
    // Windows from an earlier session are still being recovered.
    let recovering = status(
        5,
        ObservationSource::Live,
        health_json(GRANTED, "os_store", 2, false),
    );
    let (_, recover) = rig.run(AGENT, JobStage::Verify, None, Some(recovering));
    assert_eq!(recover.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    // Healthy: verified from this reply's own source and time.
    let (_, ok) = rig.run(AGENT, JobStage::Verify, None, Some(healthy(6)));
    assert_eq!(
        ok.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            observed_at_ms: 10_000,
        }
    );
    // A reply that wasn't live stays that way.
    let demo = status(
        7,
        ObservationSource::Demo,
        health_json(GRANTED, "os_store", 0, false),
    );
    let (_, d) = rig.run(AGENT, JobStage::Verify, None, Some(demo));
    assert!(matches!(
        d.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Demo,
            ..
        }
    ));
    // Looking at the agent never changes anything.
    assert_eq!(rig.outcome(AGENT, JobStage::Apply), NativeOutcome::Failed);
    assert_eq!(rig.outcome(AGENT, JobStage::Plan), NativeOutcome::Failed);
    assert_eq!(rig.count("install."), 0);
}

// ---- permissions ------------------------------------------------------------------------------

#[test]
fn permission_detection_follows_the_agents_own_facts() {
    let mut rig = Rig::new();
    let (_, all) = rig.run(PERMISSIONS, JobStage::Detect, None, Some(healthy(1)));
    assert_eq!(detect_needs_action(&all.outcome), Some(false));
    let missing = with_permissions(2, ["not_granted", "granted", "unknown"], false);
    let (_, some) = rig.run(PERMISSIONS, JobStage::Detect, None, Some(missing));
    assert_eq!(detect_needs_action(&some.outcome), Some(true));
    // An agent that didn't answer says nothing about permissions.
    let (_, silent) = rig.run(PERMISSIONS, JobStage::Detect, None, None);
    assert_eq!(silent.outcome, NativeOutcome::Waiting(WaitKind::Contract));
}

#[test]
fn the_permission_preview_names_only_what_is_missing_and_where_to_grant_it() {
    let mut rig = Rig::new();
    let missing = with_permissions(2, ["not_granted", "granted", "unknown"], false);
    let (_, plan) = rig.run(PERMISSIONS, JobStage::Plan, None, Some(missing));
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome);
    };
    assert!(preview.contains("Screen Recording"), "{preview}");
    assert!(preview.contains("Input Monitoring"), "{preview}");
    assert!(
        preview.contains("Screen & System Audio Recording > Crosspane"),
        "{preview}"
    );
    assert!(!preview.contains("Accessibility:"), "{preview}");
    assert!(
        preview.contains("doesn't mean permission was granted")
            || preview.contains("does not mean permission was granted"),
        "{preview}"
    );
    // Nothing missing: nothing to plan, and the step is checked again.
    let (_, none) = rig.run(PERMISSIONS, JobStage::Plan, None, Some(healthy(3)));
    assert_eq!(none.outcome, NativeOutcome::Waiting(WaitKind::User));
}

#[test]
fn permissions_verify_only_when_the_agent_reports_each_one() {
    let mut rig = Rig::new();
    let missing = with_permissions(1, ["granted", "not_granted", "granted"], false);
    let (_, waiting) = rig.run(PERMISSIONS, JobStage::Verify, None, Some(missing));
    assert_eq!(waiting.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(waiting.detail.contains("Accessibility"), "{waiting:?}");
    assert!(
        waiting
            .detail
            .contains("Opening a settings pane doesn't count"),
        "{waiting:?}"
    );
    let (_, done) = rig.run(PERMISSIONS, JobStage::Verify, None, Some(healthy(2)));
    assert!(matches!(
        done.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            ..
        }
    ));
}

#[test]
fn the_worker_never_requests_permissions_itself() {
    let mut rig = Rig::new();
    let (_, report) = rig.run(
        PERMISSIONS,
        JobStage::Apply,
        None,
        Some(with_permissions(1, ["not_granted"; 3], false)),
    );
    assert_eq!(report.outcome, NativeOutcome::Failed);
}

#[test]
fn the_microphone_is_required_only_once_audio_is_enabled() {
    let mut rig = Rig::new();
    // Audio off: the three grants are everything.
    let off = with_permissions(1, GRANTED, false);
    let (_, ok) = rig.run(PERMISSIONS, JobStage::Verify, None, Some(off));
    assert!(
        matches!(ok.outcome, NativeOutcome::Verified { .. }),
        "{ok:?}"
    );
    // Audio on, with no microphone fact: a required permission that isn't reported isn't granted.
    let on = with_permissions(2, GRANTED, true);
    let (_, waiting) = rig.run(PERMISSIONS, JobStage::Verify, None, Some(on.clone()));
    assert_eq!(waiting.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(waiting.detail.contains("Microphone"), "{waiting:?}");
    // And its preview carries the exact loopback privacy copy.
    let (_, plan) = rig.run(PERMISSIONS, JobStage::Plan, None, Some(on));
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome);
    };
    assert!(
        preview.contains("your real microphone is never opened"),
        "{preview}"
    );
}

#[test]
fn a_failed_startup_recovery_is_not_a_healthy_agent_even_at_zero_pending() {
    let mut rig = Rig::new();
    let mut body = health_json(GRANTED, "os_store", 0, false);
    body["result"]["installer"]["startup_recovery"] = json!("failed");
    let failed = status(9, ObservationSource::Live, body);
    let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(failed));
    assert_eq!(report.outcome, NativeOutcome::Waiting(WaitKind::Contract));
}

// ---- sound driver -----------------------------------------------------------------------------

#[test]
fn the_sound_driver_is_applied_only_with_a_consent_for_the_exact_plan_previewed() {
    let mut rig = Rig::new();
    let (_, detect) = rig.run(AUDIO, JobStage::Detect, None, Some(healthy(1)));
    assert_eq!(detect_needs_action(&detect.outcome), Some(true));
    let (_, plan) = rig.run(AUDIO, JobStage::Plan, None, Some(healthy(2)));
    let NativeOutcome::Planned { preview } = &plan.outcome else {
        panic!("{:?}", plan.outcome);
    };
    assert!(preview.contains("1.0.0"), "{preview}");
    assert!(preview.contains("administrator password"), "{preview}");
    assert!(preview.contains("never sees"), "{preview}");
    assert!(preview.contains("every user"), "{preview}");
    assert!(preview.contains("briefly interrupts"), "{preview}");
    assert!(!preview.contains("earlier copy"), "{preview}");

    let wrong = rig.job(AUDIO, JobStage::Apply);
    let consent = Consent {
        plan: OperationId(wrong.operation.0 + 100),
        operation: wrong.operation,
        revision: 7,
    };
    let (_, refused) = rig.run_job(wrong, Some(consent), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("audio.apply"), 0);

    // The refused apply consumed the held plan, so a fresh plan is needed.
    let (plan_job, _) = rig.run(AUDIO, JobStage::Plan, None, None);
    let apply = rig.job(AUDIO, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, applied) = rig.run_job(apply.clone(), Some(consent), None);
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(ApplyOutcome::Applied)
    );
    assert!(applied.detail.contains("doesn't cancel"), "{applied:?}");
    assert_eq!(
        rig.calls()
            .iter()
            .filter(|c| c.starts_with("audio.apply"))
            .cloned()
            .collect::<Vec<_>>(),
        [format!("audio.apply:{}", plan_job.operation.0)]
    );
    let (_, again) = rig.run_job(apply, Some(consent), None);
    assert_eq!(again.outcome, NativeOutcome::Applied(ApplyOutcome::Refused));
    assert_eq!(rig.count("audio.apply"), 1);
}

#[test]
fn an_open_installer_is_waited_for_and_never_opened_twice() {
    let mut rig = Rig::new();
    rig.set(|w| w.audio_opened = true);
    let (_, detect) = rig.run(AUDIO, JobStage::Detect, None, None);
    assert_eq!(detect.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(detect.detail.contains("installer"), "{detect:?}");
    let (_, verify) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert_eq!(verify.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert_eq!(rig.count("audio.plan"), 0);
    assert_eq!(rig.count("audio.apply"), 0);
}

#[test]
fn the_sound_driver_verifies_only_from_the_installers_own_outcome() {
    let mut rig = Rig::new();
    // Nothing is installed or opened: nothing is assumed.
    let (_, none) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert_eq!(none.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    rig.set(|w| w.audio_installed = true);
    let (_, ok) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert!(matches!(
        ok.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            ..
        }
    ));
    let (_, detect) = rig.run(AUDIO, JobStage::Detect, None, None);
    assert_eq!(detect_needs_action(&detect.outcome), Some(false));
    // An installer that reports failure is a failure, with the recovery note.
    rig.set(|w| {
        w.audio_verify = Some(Err(AudioError::Failed(
            "The installer couldn't put the sound driver in place.".into(),
        )))
    });
    let (_, failed) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert_eq!(failed.outcome, NativeOutcome::Failed);
    // An outcome that can't be told from an earlier run only waits.
    rig.set(|w| w.audio_verify = Some(Err(AudioError::Unknown)));
    let (_, unknown) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert_eq!(unknown.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    // A demo-source confirmation stays demo.
    rig.set(|w| w.audio_verify = Some(Ok(ObservationSource::Demo)));
    let (_, demo) = rig.run(AUDIO, JobStage::Verify, None, None);
    assert!(matches!(
        demo.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Demo,
            ..
        }
    ));
}

#[test]
fn the_sound_driver_refusals_are_typed_and_nothing_is_opened() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.audio_plan = Some(Err(AudioError::Refused(
            "The folders the sound driver goes in aren't as Crosspane expects.".into(),
        )))
    });
    let (_, plan) = rig.run(AUDIO, JobStage::Plan, None, None);
    assert_eq!(plan.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(
        plan.detail.contains("aren't as Crosspane expects"),
        "{plan:?}"
    );
    rig.set(|w| {
        w.audio_plan = None;
        w.audio_apply = Some(Err(AudioError::Refused("macOS didn't open it.".into())));
    });
    let (plan_job, _) = rig.run(AUDIO, JobStage::Plan, None, None);
    let apply = rig.job(AUDIO, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, refused) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    // An unknown outcome is detected again, never retried blindly.
    rig.set(|w| w.audio_apply = Some(Err(AudioError::Unknown)));
    let (plan_job, _) = rig.run(AUDIO, JobStage::Plan, None, None);
    let apply = rig.job(AUDIO, JobStage::Apply);
    let consent = Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 7,
    };
    let (_, unknown) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        unknown.outcome,
        NativeOutcome::Applied(ApplyOutcome::Unknown)
    );
    // An unreadable driver folder keeps the step pending, never assumed present or absent.
    rig.set(|w| w.audio_detect = Some(Err(AudioError::Unavailable)));
    let (_, pending) = rig.run(AUDIO, JobStage::Detect, None, None);
    assert_eq!(pending.outcome, NativeOutcome::Waiting(WaitKind::Contract));
}

#[test]
fn an_unsupported_mac_never_reaches_the_sound_driver() {
    let mut rig = Rig::new();
    rig.set(|w| w.support = SupportOutcome::Unsupported("Needs macOS 26.".into()));
    for stage in [JobStage::Detect, JobStage::Plan, JobStage::Verify] {
        assert_eq!(rig.outcome(AUDIO, stage), NativeOutcome::Unsupported);
    }
    let (_, applied) = rig.run(AUDIO, JobStage::Apply, None, None);
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("audio."), 0);
}

// ---- maintenance ------------------------------------------------------------------------------

#[test]
fn removal_is_inspected_planned_confirmed_and_given_the_fresh_status() {
    let mut rig = Rig::new();
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    match &rig.maintenance[0] {
        MaintenanceReport::Inspected {
            uninstall,
            repair,
            choices,
            ..
        } => {
            assert_eq!(*uninstall, Availability::Available);
            assert_eq!(*repair, Availability::Available);
            assert_eq!(choices.len(), 2);
        }
        other => panic!("{other:?}"),
    }
    // Confirming before any preview is refused.
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 4,
            status: None,
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 2);
    assert!(matches!(
        &rig.maintenance[1],
        MaintenanceReport::Refused { .. }
    ));
    assert_eq!(rig.count("uninstall.apply"), 0);

    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: vec![(1, false), (2, true)],
        status: Some(StatusEvidence(healthy(61))),
    }));
    rig.pump(|r| r.maintenance.len() >= 3);
    assert!(matches!(
        &rig.maintenance[2],
        MaintenanceReport::Planned { .. }
    ));
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 4,
            status: Some(StatusEvidence(healthy(62))),
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 4);
    match &rig.maintenance[3] {
        MaintenanceReport::Finished { outcome, lines, .. } => {
            assert_eq!(*outcome, MaintenanceOutcome::Removed);
            assert!(lines[0].contains("removed"), "{lines:?}");
        }
        other => panic!("{other:?}"),
    }
    // Each stage was handed the Status that was issued for it, and the choices as ticked.
    let seen = rig.w.lock().unwrap().seen_status.clone();
    assert_eq!(
        seen,
        [
            ("uninstall.plan".to_owned(), Some(61)),
            ("uninstall.apply".to_owned(), Some(62)),
        ]
    );
    assert!(
        rig.calls()
            .iter()
            .any(|c| c.starts_with("uninstall.plan:[(1, false), (2, true)]")),
        "{:?}",
        rig.calls()
    );
    // Confirm cannot be repeated: the plan was consumed.
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 4,
            status: None,
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 5);
    assert!(matches!(
        &rig.maintenance[4],
        MaintenanceReport::Refused { .. }
    ));
    assert_eq!(rig.count("uninstall.apply"), 1);
}

#[test]
fn a_stale_removal_id_is_refused() {
    let mut rig = Rig::new();
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect {
        id: MaintenanceId(1),
    }));
    rig.pump(|r| !r.maintenance.is_empty());
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id: MaintenanceId(9),
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 2);
    assert!(matches!(
        &rig.maintenance[1],
        MaintenanceReport::Refused { .. }
    ));
    assert_eq!(rig.count("uninstall.plan"), 0);
    // A plan for the current id, then a confirm for a different one.
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id: MaintenanceId(1),
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 3);
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id: MaintenanceId(8),
            revision: 1,
            status: None,
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 4);
    assert!(matches!(
        &rig.maintenance[3],
        MaintenanceReport::Refused { .. }
    ));
    assert_eq!(rig.count("uninstall.apply"), 0);
}

#[test]
fn a_removal_that_cannot_start_says_nothing_was_removed() {
    let mut rig = Rig::new();
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    rig.set(|w| w.uninstall_plan = Err("Crosspane's install can't be read.".into()));
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 2);
    assert!(
        matches!(&rig.maintenance[1], MaintenanceReport::Refused { reason, .. } if reason.contains("can't be read"))
    );
    rig.set(|w| {
        w.uninstall_plan = Ok("Remove it.".into());
        w.uninstall_apply = Err("Things changed since the preview.".into());
    });
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 3);
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 1,
            status: None,
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 4);
    match &rig.maintenance[3] {
        MaintenanceReport::Finished { outcome, lines, .. } => {
            assert_eq!(*outcome, MaintenanceOutcome::Refused);
            assert!(lines.iter().any(|l| l.contains("Nothing was removed")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn removal_has_no_follow_ups_on_a_mac() {
    let mut rig = Rig::new();
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmFollowUp {
            id,
            follow_up: 3,
            revision: 1,
        },
    ));
    rig.pump(|r| r.maintenance.len() >= 2);
    assert!(matches!(
        &rig.maintenance[1],
        MaintenanceReport::Progress { .. }
    ));
    assert_eq!(rig.count("uninstall.apply"), 0);
}

// ---- a build that cannot prove the Mac -------------------------------------------------------

#[test]
fn a_blocked_build_refuses_every_step_with_the_typed_reason_and_changes_nothing() {
    let reason = "This build has no approved inventory (unsigned or dev build), so nothing will \
                  be copied, started or removed.";
    let w = World::new();
    let mut rig = Rig::with(w, Box::new(move || Blocked::domains(reason)));
    for step in [SUPPORT, INSTALL, AUDIO] {
        let (_, detect) = rig.run(step, JobStage::Detect, None, None);
        assert_eq!(detect.outcome, NativeOutcome::Waiting(WaitKind::User));
        assert!(
            detect.detail.contains("no approved inventory"),
            "{detect:?}"
        );
    }
    let (_, plan) = rig.run(INSTALL, JobStage::Plan, None, Some(healthy(1)));
    assert_eq!(plan.outcome, NativeOutcome::Waiting(WaitKind::User));
    let (_, apply) = rig.run(INSTALL, JobStage::Apply, None, None);
    assert_eq!(apply.outcome, NativeOutcome::Applied(ApplyOutcome::Refused));
    // Removal is shown as unavailable, with the same reason, and offers no choices.
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    match &rig.maintenance[0] {
        MaintenanceReport::Inspected {
            uninstall: Availability::Unavailable(text),
            repair: Availability::Unavailable(repair_text),
            choices,
            ..
        } => {
            assert!(text.contains("no approved inventory"));
            // Repair is gated like install: the same typed reason, never a callable repair.
            assert_eq!(repair_text, text);
            assert!(choices.is_empty());
        }
        other => panic!("{other:?}"),
    }
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 2);
    assert!(matches!(
        &rig.maintenance[1],
        MaintenanceReport::Refused { .. }
    ));
}

// ---- repair -----------------------------------------------------------------------------------

impl Rig {
    fn maint(&mut self, request: MaintenanceRequest) -> Vec<MaintenanceReport> {
        let before = self.maintenance.len();
        self.send(NativeJob::Maintenance(request));
        self.pump(|r| r.maintenance.len() > before);
        self.maintenance[before..].to_vec()
    }

    fn inspect(&mut self, id: u64) -> Vec<MaintenanceReport> {
        let before = self.maintenance.len();
        self.send(NativeJob::Maintenance(MaintenanceRequest::Inspect {
            id: MaintenanceId(id),
        }));
        self.pump(|r| r.maintenance.len() > before);
        std::thread::sleep(Duration::from_millis(30));
        self.pump(|_| true);
        self.maintenance[before..].to_vec()
    }

    fn plan_repair(&mut self, id: u64) -> u64 {
        let reports = self.maint(MaintenanceRequest::PlanRepair {
            id: MaintenanceId(id),
            status: Some(StatusEvidence(healthy(31))),
        });
        match &reports[0] {
            MaintenanceReport::RepairPlanned { plan, .. } => *plan,
            other => panic!("expected a repair preview, got {other:?}"),
        }
    }

    fn confirm_repair(&mut self, id: u64, plan: u64) -> MaintenanceReport {
        self.maint(MaintenanceRequest::ConfirmRepair {
            id: MaintenanceId(id),
            plan,
            revision: 7,
            status: Some(StatusEvidence(healthy(32))),
        })
        .remove(0)
    }

    fn verify_repair(&mut self, id: u64, call: u64) -> MaintenanceReport {
        self.maint(MaintenanceRequest::VerifyRepair {
            id: MaintenanceId(id),
            status: Some(StatusEvidence(healthy(call))),
        })
        .remove(0)
    }
}

#[test]
fn repair_is_offered_only_when_the_adapters_admission_says_the_install_is_compatible() {
    let mut rig = Rig::new();
    let reports = rig.inspect(1);
    assert!(matches!(
        &reports[0],
        MaintenanceReport::Inspected {
            repair: Availability::Available,
            ..
        }
    ));
    assert_eq!(reports.len(), 1, "nothing to resume");
    // An incompatible install says why, with the next step, and nothing can be planned.
    let reason = "Repair needs the Crosspane that setup installed to be running. \
                  Uninstall (keeping identity by default), then install. Nothing was changed.";
    rig.w.lock().unwrap().repair_offer = RepairOffer {
        repair: Availability::Unavailable(reason.into()),
        resumable: None,
    };
    rig.w.lock().unwrap().repair_plan = Err(reason.into());
    let reports = rig.inspect(2);
    let MaintenanceReport::Inspected { repair, .. } = &reports[0] else {
        panic!("{reports:?}")
    };
    assert_eq!(*repair, Availability::Unavailable(reason.into()));
    let reports = rig.maint(MaintenanceRequest::PlanRepair {
        id: MaintenanceId(2),
        status: None,
    });
    let MaintenanceReport::Refused { reason: text, .. } = &reports[0] else {
        panic!("{reports:?}")
    };
    assert!(text.contains("Uninstall (keeping identity by default), then install."));
    assert_eq!(rig.count("repair.confirm"), 0);
}

#[test]
fn a_blocked_build_cannot_repair_for_the_same_reason_it_cannot_install() {
    let w = World::new();
    let reason = "This build has no approved inventory (unsigned or dev build), so nothing will \
                  be copied, started or removed.";
    let mut rig = Rig::with(w, Box::new(move || Blocked::domains(reason)));
    let reports = rig.inspect(1);
    let MaintenanceReport::Inspected { repair, .. } = &reports[0] else {
        panic!("{reports:?}")
    };
    assert_eq!(*repair, Availability::Unavailable(reason.into()));
    assert_eq!(reports.len(), 1);
    for request in [
        MaintenanceRequest::PlanRepair {
            id: MaintenanceId(1),
            status: Some(StatusEvidence(healthy(31))),
        },
        MaintenanceRequest::ConfirmRepair {
            id: MaintenanceId(1),
            plan: 1,
            revision: 7,
            status: Some(StatusEvidence(healthy(32))),
        },
        MaintenanceRequest::ResumeRepair {
            id: MaintenanceId(1),
            status: Some(StatusEvidence(healthy(33))),
        },
    ] {
        let reports = rig.maint(request);
        assert!(
            matches!(&reports[0], MaintenanceReport::Refused { .. }),
            "{reports:?}"
        );
    }
}

#[test]
fn a_repair_is_confirmed_only_for_the_numbered_preview_and_only_once() {
    let mut rig = Rig::new();
    rig.inspect(1);
    let report = rig.confirm_repair(1, 1);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "{report:?}"
    );
    assert_eq!(rig.count("repair.confirm"), 0);

    let plan = rig.plan_repair(1);
    let report = rig.confirm_repair(1, plan + 1);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "{report:?}"
    );
    assert_eq!(rig.count("repair.confirm"), 0);
    let report = rig.confirm_repair(1, plan);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "a refused confirmation retires the preview: {report:?}"
    );

    let plan = rig.plan_repair(1);
    let report = rig.confirm_repair(1, plan);
    assert!(
        matches!(report, MaintenanceReport::RepairWaiting { .. }),
        "{report:?}"
    );
    {
        let w = rig.w.lock().unwrap();
        assert_eq!(w.repair_confirmed.len(), 1);
        assert_eq!(w.repair_confirmed[0].0, plan);
        assert!(w.repair_confirmed[0].1 > plan);
    }
    assert!(calls(&rig.w).contains(&format!("repair.confirm(plan={plan},status=Some(32))")));
    let report = rig.confirm_repair(1, plan);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "{report:?}"
    );
    assert_eq!(rig.count("repair.confirm"), 1);
}

#[test]
fn a_repair_waits_for_the_new_agent_and_the_wait_ends_with_the_adapters_typed_result() {
    let mut rig = Rig::new();
    rig.inspect(1);
    let plan = rig.plan_repair(1);
    assert!(matches!(
        rig.confirm_repair(1, plan),
        MaintenanceReport::RepairWaiting { .. }
    ));
    rig.w.lock().unwrap().repair_hold = true;
    assert!(matches!(
        rig.verify_repair(1, 41),
        MaintenanceReport::RepairWaiting { .. }
    ));
    // Past the bound the repair ends as unknown, never as a guess at success or failure.
    rig.advance(100_000);
    let report = rig.verify_repair(1, 42);
    let MaintenanceReport::RepairFinished {
        outcome, resumable, ..
    } = report
    else {
        panic!("{report:?}")
    };
    assert_eq!(outcome, RepairOutcome::OutcomeUnknown);
    assert!(resumable);
    // A resume (in this window) asks the adapter again.
    let reports = rig.maint(MaintenanceRequest::ResumeRepair {
        id: MaintenanceId(1),
        status: Some(StatusEvidence(healthy(51))),
    });
    assert!(matches!(
        &reports[0],
        MaintenanceReport::RepairFinished {
            outcome: RepairOutcome::Verified,
            ..
        }
    ));
}

#[test]
fn every_typed_repair_outcome_is_reported_and_none_is_invented() {
    for (outcome, resumable) in [
        (RepairOutcome::Verified, false),
        (RepairOutcome::HealthVerifiedCleanupIncomplete, false),
        (RepairOutcome::OutcomeUnknown, true),
        (RepairOutcome::RecoveryRetained, false),
    ] {
        let mut rig = Rig::new();
        rig.inspect(1);
        rig.w
            .lock()
            .unwrap()
            .repair_confirm
            .push_back(Ok(RepairStep::Finished(finish(
                outcome,
                "The adapter's own words.",
                resumable,
            ))));
        let plan = rig.plan_repair(1);
        let report = rig.confirm_repair(1, plan);
        assert_eq!(
            report,
            MaintenanceReport::RepairFinished {
                id: MaintenanceId(1),
                outcome,
                lines: vec!["The adapter's own words.".into()],
                resumable,
            }
        );
    }
}

#[test]
fn removal_cannot_be_planned_while_a_repair_is_active_and_a_stale_verify_is_ignored() {
    let mut rig = Rig::new();
    rig.inspect(1);
    let plan = rig.plan_repair(1);
    assert!(matches!(
        rig.confirm_repair(1, plan),
        MaintenanceReport::RepairWaiting { .. }
    ));
    let reports = rig.maint(MaintenanceRequest::PlanUninstall {
        id: MaintenanceId(1),
        choices: Vec::new(),
        status: None,
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
    assert_eq!(rig.count("uninstall.plan"), 0);
    let before = rig.maintenance.len();
    rig.send(NativeJob::Maintenance(MaintenanceRequest::VerifyRepair {
        id: MaintenanceId(9),
        status: Some(StatusEvidence(healthy(43))),
    }));
    std::thread::sleep(Duration::from_millis(60));
    rig.pump(|_| true);
    assert_eq!(rig.maintenance.len(), before);
    assert_eq!(rig.count("repair.verify"), 0);
}

#[test]
fn a_blocked_build_never_admits_an_agent_so_every_agent_call_fails_promptly() {
    let w = World::new();
    let mut rig = Rig::with(w, Box::new(|| Blocked::domains("Blocked build.")));
    let agent = rig.platform.agent();
    agent
        .submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: 5_000,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    let replies = loop {
        let replies = rig.platform.agent().poll();
        if !replies.is_empty() {
            break replies;
        }
        assert!(Instant::now() < end, "no failure reply");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].id, 1);
    assert_eq!(replies[0].result, Err(CallFailure::Unavailable));
    assert_eq!(rig.backend.lock().unwrap().adopted, 0);
}

// ---- the GUI-thread agent port ----------------------------------------------------------------

impl Rig {
    fn call(&mut self, id: u64, request: InstallerRequest) -> Result<(), CallFailure> {
        self.platform.agent().submit(AgentCall {
            id,
            request,
            timeout_ms: 5_000,
        })
    }

    /// Poll the port until `want` calls reached the backend or failed, and return what the
    /// backend received and what failed.
    fn settle(&mut self, want: usize) -> (Vec<AgentCall>, Vec<AgentReply>) {
        let end = Instant::now() + Duration::from_secs(30);
        let mut received = Vec::new();
        let mut failed = Vec::new();
        loop {
            failed.extend(self.platform.agent().poll());
            received.extend(self.backend.lock().unwrap().queue.take_calls());
            if received.len() + failed.len() >= want {
                return (received, failed);
            }
            assert!(Instant::now() < end, "calls never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[test]
fn every_agent_call_waits_for_a_fresh_admission_before_it_is_sent() {
    let mut rig = Rig::new();
    rig.call(1, InstallerRequest::Status).unwrap();
    let (received, failed) = rig.settle(1);
    assert!(failed.is_empty());
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].id, 1);
    assert_eq!(rig.count("agents.admit"), 1);
    assert_eq!(rig.backend.lock().unwrap().adopted, 1);
}

#[test]
fn an_admission_is_reused_only_for_a_moment() {
    let mut rig = Rig::new();
    rig.call(1, InstallerRequest::Status).unwrap();
    rig.settle(1);
    // A moment later, the same admission serves the next read.
    rig.advance(300);
    rig.call(2, InstallerRequest::Status).unwrap();
    let (received, _) = rig.settle(1);
    assert_eq!(received[0].id, 2);
    assert_eq!(rig.count("agents.admit"), 1);
    // Long enough for the proofs to be near expiry: a new admission is asked for.
    rig.advance(3_000);
    rig.call(3, InstallerRequest::Status).unwrap();
    let (received, _) = rig.settle(1);
    assert_eq!(received[0].id, 3);
    assert_eq!(rig.count("agents.admit"), 2);
    assert_eq!(rig.backend.lock().unwrap().adopted, 2);
}

#[test]
fn a_mutation_never_rides_an_admission_that_is_getting_old() {
    let mut rig = Rig::new();
    rig.call(1, InstallerRequest::Status).unwrap();
    rig.settle(1);
    rig.advance(900);
    // Fresh enough for a read, too old for a mutation.
    rig.call(2, InstallerRequest::Restart).unwrap();
    let (received, _) = rig.settle(1);
    assert_eq!(received[0].request, InstallerRequest::Restart);
    assert_eq!(rig.count("agents.admit"), 2);
}

#[test]
fn a_failed_admission_fails_the_call_without_sending_it() {
    let mut rig = Rig::new();
    rig.set(|w| w.admit = Err("The agent isn't running.".into()));
    rig.call(1, InstallerRequest::Status).unwrap();
    rig.call(2, InstallerRequest::Status).unwrap();
    let (received, failed) = rig.settle(2);
    assert!(received.is_empty());
    let mut ids: Vec<u64> = failed.iter().map(|r| r.id).collect();
    ids.sort();
    assert_eq!(ids, [1, 2]);
    assert!(
        failed
            .iter()
            .all(|r| r.result == Err(CallFailure::Unavailable))
    );
    assert_eq!(rig.backend.lock().unwrap().adopted, 0);
}

#[test]
fn a_backend_that_refuses_the_admission_fails_the_calls_waiting_on_it() {
    let mut rig = Rig::new();
    rig.backend.lock().unwrap().refuse = true;
    rig.call(1, InstallerRequest::Status).unwrap();
    let (received, failed) = rig.settle(1);
    assert!(received.is_empty());
    assert_eq!(failed[0].result, Err(CallFailure::Unavailable));
}

#[test]
fn a_queued_call_behind_a_longer_one_expires_on_its_own_deadline_and_none_is_sent_late() {
    let mut rig = Rig::new();
    rig.set(|w| w.gate_open = false);
    for (id, timeout_ms) in [(1, 4_000), (2, 1_000)] {
        rig.platform
            .agent()
            .submit(AgentCall {
                id,
                request: InstallerRequest::Status,
                timeout_ms,
            })
            .unwrap();
    }
    rig.advance(1_500);
    let failed: Vec<u64> = rig.platform.agent().poll().iter().map(|r| r.id).collect();
    assert_eq!(failed, [2], "the shorter call expired while it waited");
    rig.set(|w| w.gate_open = true);
    let (received, failed) = rig.settle(1);
    assert!(failed.is_empty());
    assert_eq!(received[0].id, 1);
    assert!(
        received[0].timeout_ms <= 2_500,
        "only the rest of its own budget goes with it: {}",
        received[0].timeout_ms
    );
}

#[test]
fn an_admission_that_arrives_too_old_to_reuse_is_replaced_by_a_fresh_one() {
    let mut rig = Rig::new();
    rig.set(|w| w.gate_open = false);
    rig.call(1, InstallerRequest::Status).unwrap();
    // The worker takes longer than the reuse window to admit the agent.
    rig.advance(2_000);
    rig.set(|w| w.gate_open = true);
    let (received, failed) = rig.settle(1);
    assert!(failed.is_empty());
    assert_eq!(received[0].id, 1);
    assert_eq!(
        rig.count("agents.admit"),
        2,
        "the stale admission was not used"
    );
    assert_eq!(rig.backend.lock().unwrap().adopted, 1);
}

#[test]
fn calls_keep_their_order_and_a_stale_id_is_refused() {
    let mut rig = Rig::new();
    rig.call(5, InstallerRequest::Status).unwrap();
    rig.call(6, InstallerRequest::Status).unwrap();
    rig.call(7, InstallerRequest::Status).unwrap();
    assert!(rig.call(7, InstallerRequest::Status).is_err());
    assert!(rig.call(3, InstallerRequest::Status).is_err());
    let (received, failed) = rig.settle(3);
    assert!(failed.is_empty());
    assert_eq!(received.iter().map(|c| c.id).collect::<Vec<_>>(), [5, 6, 7]);
}

#[test]
fn a_call_that_waits_past_its_own_deadline_is_never_sent() {
    let mut rig = Rig::new();
    rig.set(|w| w.gate_open = false);
    rig.call(1, InstallerRequest::Status).unwrap();
    // The worker hasn't admitted the agent, and the call's own time runs out.
    rig.advance(6_000);
    let (received, failed) = rig.settle(1);
    assert!(received.is_empty());
    assert_eq!(failed[0].id, 1);
    assert_eq!(failed[0].result, Err(CallFailure::Unavailable));
    // The late admission arrives after the call is gone: nothing is sent on it.
    rig.set(|w| w.gate_open = true);
    std::thread::sleep(Duration::from_millis(80));
    let (received, _) = {
        let _ = rig.platform.agent().poll();
        (rig.backend.lock().unwrap().queue.take_calls(), ())
    };
    assert!(received.is_empty());
}

fn connected_peer_status(id: u64, generation: u64) -> AgentReply {
    let mut body = health_json(GRANTED, "os_store", 0, false);
    body["result"]["installer"]["peers"] = json!([{
        "node": peer().to_string(), "name": "other", "connected": true,
        "link_generation": generation, "features": [], "grants_given": [],
        "last_source_parking": null,
        "counters": {"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,
        "e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,
        "e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,
        "e2_dest_returned":0,"e2_frames_presented":0,"e2_returns_failed":0}}]);
    status(id, ObservationSource::Live, body)
}

#[test]
fn a_peer_bound_call_needs_a_connected_peer_and_is_bound_to_its_link() {
    let mut rig = Rig::new();
    // No Status has shown the peer connected: the call fails without an admission.
    rig.call(
        1,
        InstallerRequest::Project {
            window: crosspane_types::id::WindowId(100),
            peer: peer(),
        },
    )
    .unwrap();
    let (received, failed) = rig.settle(1);
    assert!(received.is_empty());
    assert_eq!(failed[0].result, Err(CallFailure::Unavailable));
    assert_eq!(rig.count("agents.admit"), 0);

    // A Status that shows the peer connected, with its link generation.
    rig.call(2, InstallerRequest::Status).unwrap();
    let (received, _) = rig.settle(1);
    assert_eq!(received[0].id, 2);
    rig.backend
        .lock()
        .unwrap()
        .queue
        .push_reply(connected_peer_status(2, 7))
        .unwrap();
    let _ = rig.platform.agent().poll();
    rig.advance(10);

    // Now a peer-bound call asks for an admission bound to that peer's link.
    rig.call(
        3,
        InstallerRequest::Project {
            window: crosspane_types::id::WindowId(100),
            peer: peer(),
        },
    )
    .unwrap();
    let (received, failed) = rig.settle(1);
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(received[0].id, 3);
    let links = rig.w.lock().unwrap().links.clone();
    assert_eq!(
        links.last().copied().flatten(),
        Some(SelectedLink {
            node: peer(),
            generation: 7
        })
    );
    // The admission without a link can't serve it: it needed a new one.
    assert_eq!(rig.count("agents.admit"), 2);
}

#[test]
fn the_graph_is_data_the_core_validates() {
    let desc = crosspane_installer::platform::macos::integration::description();
    let steps: Vec<u16> = desc.steps.iter().map(|s| s.id.0).collect();
    assert_eq!(steps, [10, 20, 21, 30, 31]);
    let installed: Vec<u16> = desc
        .steps
        .iter()
        .filter(|s| s.required_for_installed)
        .map(|s| s.id.0)
        .collect();
    assert_eq!(installed, [10, 20]);
    assert!(desc.steps.iter().all(|s| s.required_for_ready));
    let permissions = desc.steps.iter().find(|s| s.id == PERMISSIONS).unwrap();
    assert_eq!(
        permissions.agent_apply,
        Some(live::AgentApply::AskPermissions)
    );
    assert!(permissions.uses_status);
    assert!(desc.hiding_choice);
    assert_eq!(desc.platform, AgentPlatform::Macos);
    let w = World::new();
    let rig = Rig::with(w.clone(), fake_domains(&w));
    let clock: live::Clock = Arc::new(|| 1);
    let controller = LiveController::new(Box::new(rig_platform(&rig)), clock);
    assert!(controller.is_ok());
}

/// A second platform with the same fakes, for building a controller.
fn rig_platform(rig: &Rig) -> MacPlatform {
    let domains = fake_domains(&rig.w);
    MacPlatform::compose(Parts {
        clock: Arc::new(|| 1),
        domains,
        agent: AgentSource::Injected(Box::new(FakeBackend(Arc::new(Mutex::new(
            BackendState::default(),
        ))))),
    })
    .unwrap()
}

// ---- the whole flow through the real controller on the real Mac worker -----------------------

mod flow {
    use super::*;

    /// Fake time per harness round while the worker thread runs for real. Kept small, with a
    /// real pause each round, so a loaded machine's slow worker never lets the fake clock run
    /// a waiting call past its own deadline; the wall-clock bounds are failure bounds only.
    const PACE_MS: u64 = 20;

    use crosspane_installer::gui::InstallerController;
    use crosspane_installer::view::*;

    pub struct Flow {
        pub c: LiveController,
        w: Shared,
        backend: Arc<Mutex<BackendState>>,
        clock: Arc<AtomicU64>,
        pub status: Value,
        calls: Vec<AgentCall>,
        answered: u64,
    }

    impl Flow {
        pub fn new() -> Self {
            Self::with_domains(None)
        }

        pub fn blocked(reason: &'static str) -> Self {
            Self::with_domains(Some(Box::new(move || Blocked::domains(reason))))
        }

        fn with_domains(domains: Option<DomainFactory>) -> Self {
            let w = World::new();
            let clock = Arc::new(AtomicU64::new(10_000));
            let time = clock.clone();
            let backend = Arc::new(Mutex::new(BackendState::default()));
            let domains = domains.unwrap_or_else(|| fake_domains(&w));
            let clock_fn: live::Clock = {
                let time = time.clone();
                Arc::new(move || time.load(Ordering::SeqCst))
            };
            let platform = MacPlatform::compose(Parts {
                clock: clock_fn.clone(),
                domains,
                agent: AgentSource::Injected(Box::new(FakeBackend(backend.clone()))),
            })
            .unwrap();
            let c = LiveController::new(Box::new(platform), clock_fn).unwrap();
            let mut status = health_json(["not_granted"; 3], "os_store", 0, true);
            status["result"]["installer"]["instance"]["id"] = json!(9);
            // Audio is on, so the microphone is a fourth required permission.
            status["result"]["installer"]["permissions"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": "microphone", "state": "not_granted"}));
            Self {
                c,
                w,
                backend,
                clock,
                status,
                calls: Vec::new(),
                answered: 0,
            }
        }

        pub fn view(&self) -> &WizardView {
            self.c.view()
        }

        pub fn world(&self) -> &Shared {
            &self.w
        }

        fn advance(&self, ms: u64) {
            self.clock.fetch_add(ms, Ordering::SeqCst);
        }

        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }

        fn take_calls(&mut self) {
            let taken = self.backend.lock().unwrap().queue.take_calls();
            self.calls.extend(taken);
        }

        fn tick(&mut self) {
            self.advance(1);
            let _ = self.c.tick();
            self.take_calls();
        }

        fn health(&self) -> Box<HealthSnapshot> {
            match parse_status(
                &serde_json::to_vec(&self.status).unwrap(),
                AgentPlatform::Macos,
            )
            .unwrap()
            {
                StatusAdmission::Supported(h) => h,
                _ => panic!("fixture status not admitted"),
            }
        }

        fn push_reply(&mut self, id: u64, result: Result<DecodedReply, CallFailure>) {
            self.advance(1);
            let reply = AgentReply {
                id,
                observed_at_ms: self.now(),
                source: ObservationSource::Live,
                result,
            };
            self.backend
                .lock()
                .unwrap()
                .queue
                .push_reply(reply)
                .unwrap();
        }

        /// Answer every outstanding Status call with the current status.
        fn answer_status(&mut self) {
            self.take_calls();
            let (status, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.calls)
                .into_iter()
                .partition(|c| c.request == InstallerRequest::Status);
            self.calls = rest;
            for call in status {
                self.answered += 1;
                let result = Ok(DecodedReply::Status(StatusAdmission::Supported(
                    self.health(),
                )));
                self.push_reply(call.id, result);
            }
        }

        /// Let the worker thread and the controller run until `cond`, answering status as the
        /// agent would. Time moves with every round, so poll intervals elapse.
        pub fn until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                if cond(self) {
                    return;
                }
                assert!(
                    Instant::now() < end,
                    "{what}: not reached; screen {:?}, message {:?}, rows {:?}",
                    self.view().screen,
                    self.view().message,
                    self.c
                        .rows()
                        .iter()
                        .map(|r| (r.id, r.state, r.detail.clone()))
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        pub fn row(&self, id: u16) -> RowView {
            self.c
                .rows()
                .into_iter()
                .find(|r| r.id == id)
                .unwrap_or_else(|| panic!("row {id}"))
        }

        pub fn verified(&self, id: u16) -> bool {
            self.row(id).state == RowState::Verified
        }

        pub fn click(&mut self, id: u16) {
            let b = self
                .view()
                .buttons
                .iter()
                .find(|b| b.id == id)
                .unwrap_or_else(|| panic!("button {id} missing on {:?}", self.view().screen))
                .clone();
            assert!(
                b.enabled,
                "button {id} is disabled on {:?}",
                self.view().screen
            );
            let revision = self.view().revision;
            self.c.accept(WizardAction {
                revision,
                intent: WizardIntent::Button(id),
            });
            self.tick();
        }

        pub fn has_button(&self, id: u16) -> bool {
            self.view().buttons.iter().any(|b| b.id == id && b.enabled)
        }

        pub fn go_next(&mut self) {
            self.click(live::ids::NEXT);
        }

        fn calls_of(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> Vec<AgentCall> {
            self.take_calls();
            let (taken, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.calls)
                .into_iter()
                .partition(|c| matches(&c.request));
            self.calls = rest;
            taken
        }

        fn reply_with(&mut self, call: &AgentCall, body: &[u8]) {
            let decoded = decode_reply(&call.request, body, AgentPlatform::Macos).unwrap();
            self.push_reply(call.id, Ok(decoded));
        }

        fn ack(&mut self, call: &AgentCall) {
            self.reply_with(call, br#"{"ok":true,"result":"arbitrary producer prose"}"#);
        }

        /// The next matching agent call, answering status as the agent would while it is made.
        pub fn expect_call(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> AgentCall {
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(call) = self.calls_of(&matches).into_iter().next() {
                    return call;
                }
                assert!(Instant::now() < end, "the expected call was never made");
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn take_and_ack(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> AgentCall {
            let call = self.expect_call(matches);
            self.ack(&call);
            self.tick();
            call
        }

        fn set(&mut self, path: &[&str], value: Value) {
            let mut at = &mut self.status["result"];
            for key in path {
                at = &mut at[*key];
            }
            *at = value;
        }

        fn pair_externally(&mut self, grants: &[&str]) {
            self.status["result"]["installer"]["peers"] = json!([{
                "node": peer().to_string(), "name": "other", "connected": true,
                "link_generation": 2, "features": [], "grants_given": grants,
                "last_source_parking": null,
                "counters": {"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,
                "e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,
                "e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,
                "e2_dest_returned":0,"e2_frames_presented":0,"e2_returns_failed":0}}]);
            self.status["result"]["peers"] = json!([{"node": peer().to_string(),
                "displays": [{"id":1,"name":"peer panel","pixels":[1920,1080],"scale":1.0,"mm":[530.0,300.0],"origin":[0.0,0.0]}]}]);
        }

        fn place(&mut self) {
            self.status["result"]["layout"] = json!([
                {"node": local().short(), "display": 1, "origin_mm": [0.0, 0.0], "version": 1},
                {"node": peer().short(), "display": 1, "origin_mm": [600.0, 0.0], "version": 1}
            ]);
        }

        // ---- install, permissions, hiding ----

        /// Start setup; the install inside this account then runs by itself.
        pub fn install(&mut self) {
            self.until("the welcome screen", |f| {
                f.view().screen == ScreenId::Welcome
            });
            for _ in 0..20 {
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
            }
            // Nothing is installed before the person starts setup.
            assert_eq!(
                super::calls(&self.w)
                    .iter()
                    .filter(|c| c.starts_with("install.apply"))
                    .count(),
                0
            );
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Compatibility);
            self.until("support verified", |f| f.verified(10));
            // No further click: the start of setup is the install's consent.
            self.until("the install verified from a fresh status", |f| {
                f.verified(20)
            });
            assert_eq!(self.summary(), SummaryView::InstalledWaiting);
            self.until("the agent verified", |f| f.verified(21));
        }

        /// Let a finished screen move on by itself until `screen` is up.
        pub fn reach(&mut self, screen: ScreenId) {
            self.until(&format!("the {screen:?} screen"), |f| {
                f.view().screen == screen
            });
        }

        /// WP-4.33: one row per missing permission, in the order they are asked, each with its
        /// own Allow. Each click asks the agent for that one permission; only a later status that
        /// reports it granted turns the row into a check, and the step verifies once all are.
        pub fn permissions(&mut self) {
            const ORDER: [(&str, PermissionName); 4] = [
                ("accessibility", PermissionName::Accessibility),
                ("input_monitoring", PermissionName::InputMonitoring),
                ("screen_recording", PermissionName::ScreenRecording),
                ("microphone", PermissionName::Microphone),
            ];
            self.reach(ScreenId::Permissions);
            let missing: Vec<(usize, &str, PermissionName)> = ORDER
                .iter()
                .enumerate()
                .filter(|(_, (name, _))| {
                    self.status["result"]["installer"]["permissions"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|f| f["name"] == *name && f["state"] != "granted")
                })
                .map(|(i, (name, p))| (i, *name, *p))
                .collect();
            assert!(
                !missing.is_empty(),
                "the fixture starts with grants missing"
            );
            let first = 2600 + 10 * missing[0].0 as u16;
            self.until("the permission rows", |f| f.has_button(first));
            assert!(
                !self.has_button(live::ids::consent(StepId(30))),
                "no ask-for-everything button"
            );
            for (index, name, permission) in missing {
                let allow = 2600 + 10 * index as u16;
                let row = 960 + index as u16;
                assert!(
                    !self.calls.iter().any(|c| matches!(
                        c.request,
                        InstallerRequest::AskPermission { .. }
                            | InstallerRequest::AskPermissions
                            | InstallerRequest::ResetPermission { .. }
                    )),
                    "nothing is asked before the click"
                );
                self.until("its Allow", |f| f.has_button(allow));
                self.click(allow);
                let ask =
                    self.expect_call(|r| *r == InstallerRequest::AskPermission { permission });
                self.ack(&ask);
                self.tick();
                // The acknowledgement is not completion: only a later status verifies.
                for _ in 0..5 {
                    self.advance(PACE_MS);
                    self.tick();
                }
                assert_ne!(self.row(30).state, RowState::Verified);
                for fact in self.status["result"]["installer"]["permissions"]
                    .as_array_mut()
                    .unwrap()
                {
                    if fact["name"] == name {
                        fact["state"] = json!("granted");
                    }
                }
                self.until("the row checked from status", |f| {
                    f.view()
                        .rows
                        .iter()
                        .any(|r| r.id == row && r.state == RowState::Verified)
                        || f.verified(30)
                });
            }
            self.until("permissions verified from status", |f| f.verified(30));
            assert!(
                !self.calls.iter().any(|c| matches!(
                    c.request,
                    InstallerRequest::AskPermissions | InstallerRequest::ResetPermission { .. }
                )),
                "never an ask-all and never a reset without a click"
            );
        }

        pub fn audio(&mut self) {
            self.reach(ScreenId::AudioComponent);
            self.until("a sound driver preview", |f| {
                f.has_button(live::ids::consent(StepId(31)))
            });
            let preview = self.view().message.clone();
            assert!(preview.contains("administrator password"), "{preview}");
            assert_eq!(
                super::calls(&self.w)
                    .iter()
                    .filter(|c| c.starts_with("audio.apply"))
                    .count(),
                0,
                "nothing is opened before the click"
            );
            self.click(live::ids::consent(StepId(31)));
            self.until("the installer to be opened", |f| {
                super::calls(&f.w)
                    .iter()
                    .any(|c| c.starts_with("audio.apply:"))
            });
            // The installer window is open: opening it is not completion.
            self.until("the step to wait for the installer", |f| {
                f.row(31).state == RowState::Waiting
            });
            assert_ne!(self.row(31).state, RowState::Verified);
            self.w.lock().unwrap().audio_installed = true;
            self.until("the driver verified from the installer's outcome", |f| {
                f.verified(31)
            });
        }

        pub fn choose_hiding(&mut self, choice: HidingChoice) {
            self.reach(ScreenId::HidingChoice);
            self.until("the hiding choice to be offered", |f| {
                f.row(63).state == RowState::NeedsAction
            });
            let revision = self.view().revision;
            self.c.accept(WizardAction {
                revision,
                intent: WizardIntent::ChooseHiding(choice),
            });
            self.tick();
            self.until("the apply button", |f| {
                f.has_button(live::ids::HIDING_APPLY)
            });
            self.click(live::ids::HIDING_APPLY);
            let update = self.expect_call(|r| matches!(r, InstallerRequest::SettingsUpdate { .. }));
            let InstallerRequest::SettingsUpdate {
                mac_virtual_display,
                ..
            } = &update.request
            else {
                unreachable!()
            };
            assert_eq!(*mac_virtual_display, choice == HidingChoice::Hide);
            self.reply_with(
                &update,
                br#"{"ok":true,"result":{"revision":"aaaaaaaaaaaaaaaa","restart_required":true}}"#,
            );
            self.tick();
            // The Apply click said it restarts Crosspane, and nothing is shared yet: the restart
            // follows the saved setting without a second click.
            assert!(!self.has_button(live::ids::HIDING_RESTART));
            self.take_and_ack(|r| *r == InstallerRequest::Restart);
            // The new instance comes up with the new revision.
            self.set(&["installer", "instance", "id"], json!(10));
            self.set(&["installer", "config_revision"], json!("aaaaaaaaaaaaaaaa"));
            self.until("the choice verified from the new instance", |f| {
                f.verified(63)
            });
        }

        pub fn pair_and_arrange(&mut self) {
            self.reach(ScreenId::Connect);
            self.pair_externally(&[]);
            self.until("pairing verified from status", |f| f.verified(60));
            self.reach(ScreenId::Grants);
            self.pair_externally(&["browse", "input", "present", "share", "speaker"]);
            self.until("grants verified", |f| f.verified(61));
            self.reach(ScreenId::Layout);
            self.place();
            self.until("a layout to accept", |f| {
                f.has_button(live::ids::LAYOUT_ACCEPT)
            });
            self.click(live::ids::LAYOUT_ACCEPT);
            self.until("layout committed", |f| f.verified(62));
            self.reach(ScreenId::Summary);
        }

        pub fn summary(&self) -> SummaryView {
            self.view().summary
        }

        pub fn close(&mut self) -> bool {
            let revision = self.view().revision;
            self.c.accept(WizardAction {
                revision,
                intent: WizardIntent::Close,
            })
        }

        /// Open "Remove or repair" and wait for the inspection to be answered.
        pub fn open_repair(&mut self) {
            self.until("the welcome screen", |f| {
                f.view().screen == ScreenId::Welcome
            });
            self.click(live::ids::REMOVE_OR_REPAIR);
            assert_eq!(self.view().screen, ScreenId::RepairRemove);
            self.until("the inspection", |f| {
                !f.view().message.contains("Checking what can be removed")
                    && f.view().buttons.iter().any(|b| b.id == live::ids::REPAIR)
            });
        }

        pub fn message(&self) -> String {
            self.view().message.clone()
        }

        pub fn enabled(&self, id: u16) -> bool {
            self.has_button(id)
        }

        pub fn shown(&self, id: u16) -> bool {
            self.view().buttons.iter().any(|b| b.id == id)
        }

        /// A click that carries a view revision the person is no longer looking at.
        pub fn click_stale(&mut self, id: u16) {
            let revision = self.view().revision.saturating_sub(1);
            self.c.accept(WizardAction {
                revision,
                intent: WizardIntent::Button(id),
            });
            self.tick();
        }

        /// An action sent as is, whether or not the view offers it.
        pub fn c_accept(&mut self, intent: WizardIntent, revision: u64) {
            self.c.accept(WizardAction { revision, intent });
            self.tick();
        }

        pub fn request_close(&mut self) -> bool {
            self.c.request_close()
        }

        pub fn advance_clock(&self, ms: u64) {
            self.advance(ms);
        }
    }

    impl Drop for Flow {
        fn drop(&mut self) {
            InstallerController::close(&mut self.c);
        }
    }
}

fn full_mac_flow(choice: crosspane_installer::view::HidingChoice) {
    use crosspane_installer::view::SummaryView;
    let mut f = flow::Flow::new();
    f.install();
    f.permissions();
    f.audio();
    f.choose_hiding(choice);
    f.pair_and_arrange();
    f.reach(crosspane_installer::view::ScreenId::Summary);
    f.until("final fresh health", |f| {
        f.summary() == SummaryView::WorkspaceReady
    });
    // The install in this account went ahead on the start click; the permissions request and
    // the sound driver each had their own click, and nothing destructive was ever requested.
    let calls = calls(f.world());
    assert!(calls.iter().any(|c| c.starts_with("install.apply:")));
    assert!(
        !calls.iter().any(|c| c.starts_with("uninstall.apply")),
        "removal was never requested: {calls:?}"
    );
    // The agent was admitted again and again, never once for the whole run.
    assert!(calls.iter().filter(|c| *c == "agents.admit").count() > 3);
}

#[test]
fn the_full_mac_install_permissions_hiding_pairing_arrange_and_final_health_reach_ready_only_at_the_end()
 {
    full_mac_flow(crosspane_installer::view::HidingChoice::Hide);
}

#[test]
fn choosing_to_mirror_runs_the_same_flow_with_mirrored_sources() {
    full_mac_flow(crosspane_installer::view::HidingChoice::Mirror);
}

#[test]
fn closing_mid_install_shuts_the_worker_down_and_a_new_close_is_harmless() {
    let mut f = flow::Flow::new();
    f.until("welcome", |f| {
        f.view().screen == crosspane_installer::view::ScreenId::Welcome
    });
    assert!(f.close());
    assert!(f.close(), "closing twice stays closed");
}

#[test]
fn a_build_that_cannot_prove_the_mac_stops_at_compatibility_and_never_installs() {
    use crosspane_installer::view::{RowState, ScreenId};
    let mut f = flow::Flow::blocked(
        "This build has no approved inventory (unsigned or dev build), so nothing will be copied, \
         started or removed.",
    );
    f.until("welcome", |f| f.view().screen == ScreenId::Welcome);
    f.go_next();
    f.until("support to wait for the person", |f| {
        f.row(10).state == RowState::Waiting
    });
    assert!(
        f.row(10).detail.contains("no approved inventory"),
        "{:?}",
        f.row(10)
    );
    assert_ne!(f.row(10).state, RowState::Verified);
    // Next stays blocked: nothing after support can begin.
    assert!(!f.has_button(live::ids::NEXT));
    assert!(
        !f.world()
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c.starts_with("install."))
    );
}

#[test]
fn repair_is_not_callable_in_a_production_launch_that_cannot_build_a_native_io() {
    // The same gate as install: a build with no approved inventory is honestly non-mutating, and
    // repair shows that reason and nothing else.
    let reason = "This build has no approved inventory (unsigned or dev build), so nothing will \
                  be copied, started or removed.";
    let mut f = flow::Flow::blocked(reason);
    f.open_repair();
    assert!(
        f.message().contains("no approved inventory"),
        "{}",
        f.message()
    );
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(!f.shown(live::ids::REPAIR_CONFIRM));
    assert!(!f.shown(live::ids::REPAIR_RESUME));
    // Even a forced click starts nothing.
    let revision = f.view().revision;
    f.c_accept(
        crosspane_installer::view::WizardIntent::Button(live::ids::REPAIR),
        revision,
    );
    f.advance_clock(13_000);
    for _ in 0..5 {
        f.until("a tick", |_| true);
    }
    assert_eq!(
        f.view().screen,
        crosspane_installer::view::ScreenId::RepairRemove
    );
    assert!(!f.shown(live::ids::REPAIR_CONFIRM));
}

#[test]
fn repair_is_reviewed_confirmed_and_verified_from_the_view() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_verify.extend([
        RepairStep::Waiting {
            detail: "Waiting for the new instance to report healthy…".into(),
            if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health.", true),
            closeable: false,
        },
        RepairStep::Waiting {
            detail: "Waiting for the new instance to report healthy…".into(),
            if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health.", true),
            closeable: false,
        },
    ]);
    f.open_repair();
    assert!(f.enabled(live::ids::REPAIR));
    assert!(
        !f.shown(live::ids::REPAIR_CONFIRM),
        "no consent without a preview"
    );
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    assert!(f.message().contains("Repair puts back Crosspane's app"));
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(
        !f.shown(live::ids::REMOVE_REVIEW),
        "removal isn't offered over a repair"
    );
    assert!(
        calls(f.world())
            .iter()
            .any(|c| c.starts_with("repair.plan(") && c.contains("status=Some(")),
        "{:?}",
        calls(f.world())
    );
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    assert!(!f.enabled(live::ids::BACK));
    f.until("the verified repair", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(f.message().contains("The new Crosspane reported healthy."));
    assert!(f.enabled(live::ids::BACK));
    let w = f.world().lock().unwrap();
    assert_eq!(w.repair_verified.len(), 3);
    assert!(w.repair_verified.iter().all(|s| s.is_some()));
    assert_eq!(w.repair_confirmed.len(), 1);
}

#[test]
fn each_typed_repair_outcome_reaches_the_view_text() {
    for (outcome, head, resume) in [
        (RepairOutcome::Verified, "Crosspane was repaired", false),
        (
            RepairOutcome::HealthVerifiedCleanupIncomplete,
            "Repaired: health verified, cleanup incomplete.",
            false,
        ),
        (
            RepairOutcome::OutcomeUnknown,
            "Outcome unknown, resume required.",
            true,
        ),
        (
            RepairOutcome::RecoveryRetained,
            "The repair didn't finish. Backups and recovery files were kept.",
            false,
        ),
    ] {
        let mut f = flow::Flow::new();
        f.world()
            .lock()
            .unwrap()
            .repair_confirm
            .push_back(Ok(RepairStep::Finished(finish(
                outcome,
                "Kept: the previous copy.",
                resume,
            ))));
        f.open_repair();
        f.click(live::ids::REPAIR);
        f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
        f.click(live::ids::REPAIR_CONFIRM);
        f.until("the outcome", |f| f.message().contains(head));
        assert!(
            f.message().contains("Kept: the previous copy."),
            "{outcome:?}: {}",
            f.message()
        );
        assert_eq!(f.enabled(live::ids::REPAIR_RESUME), resume, "{outcome:?}");
        assert!(
            f.enabled(live::ids::CLOSE),
            "nothing is left working: {outcome:?}"
        );
    }
}

#[test]
fn closing_mid_repair_is_refused_until_it_ends_since_nothing_on_disk_resumes_it() {
    let mut f = flow::Flow::new();
    let world = f.world().clone();
    {
        let mut w = world.lock().unwrap();
        w.repair_hold = true;
        w.repair_hold_confirm = true;
    }
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the change to be running", |f| {
        f.message().contains("Repairing Crosspane")
    });
    assert!(!f.shown(live::ids::CLOSE));
    assert!(!f.request_close());
    assert!(f.message().contains("Wait for the current change"));
    world.lock().unwrap().repair_hold_confirm = false;
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    // A Mac repair keeps its pending state only in this window: closing now would leave nothing
    // to resume, so the wait isn't closeable either.
    assert!(!f.shown(live::ids::CLOSE));
    assert!(!f.request_close());
    world.lock().unwrap().repair_hold = false;
    f.until("the verified repair", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(f.enabled(live::ids::CLOSE));
    assert!(f.close());
}

#[test]
fn an_incompatible_install_shows_the_guidance_and_cannot_be_repaired() {
    let mut f = flow::Flow::new();
    let guidance = "Repair needs the Crosspane that setup installed to be running. Uninstall \
                    (keeping identity by default), then install. Nothing was changed.";
    f.world().lock().unwrap().repair_offer = RepairOffer {
        repair: Availability::Unavailable(guidance.into()),
        resumable: None,
    };
    f.open_repair();
    assert!(
        f.message()
            .contains("Uninstall (keeping identity by default), then install.")
    );
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(
        f.enabled(live::ids::REMOVE_REVIEW),
        "removal stays available"
    );
}

#[test]
fn a_stale_view_revision_never_confirms_a_repair() {
    let mut f = flow::Flow::new();
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click_stale(live::ids::REPAIR_CONFIRM);
    for _ in 0..5 {
        f.until("a tick", |_| true);
    }
    assert!(
        !calls(f.world())
            .iter()
            .any(|c| c.starts_with("repair.confirm"))
    );
    assert!(f.shown(live::ids::REPAIR_CONFIRM));
}

#[test]
fn an_unknown_outcome_offers_a_working_resume_that_reaches_verified() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_hold = true;
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    f.advance_clock(100_000);
    f.until("the unknown outcome", |f| {
        f.message().contains("Outcome unknown, resume required.")
    });
    assert!(f.enabled(live::ids::REPAIR_RESUME));
    f.click(live::ids::REPAIR_RESUME);
    f.until("the verified resume", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(
        calls(f.world())
            .iter()
            .any(|c| c.starts_with("repair.resume(status=Some("))
    );
}

#[test]
fn a_fresh_window_offers_persisted_reassessment_and_shows_only_observed_current_health() {
    let mut f = flow::Flow::new();
    {
        let mut world = f.world().lock().unwrap();
        world.repair_offer = RepairOffer {
            repair: Availability::Unavailable("An earlier repair needs checking.".into()),
            resumable: Some(vec![
                "Its private record survived the earlier window.".into(),
            ]),
        };
        world.repair_resume = Ok(finish(
            RepairOutcome::CheckedAfterEarlierRepair,
            "The installed files match verified receipts and Crosspane reports healthy now.",
            false,
        ));
    }
    f.open_repair();
    f.until("the saved repair offer", |f| {
        f.enabled(live::ids::REPAIR_RESUME)
    });
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(f.message().contains("survived the earlier window"));
    f.click(live::ids::REPAIR_RESUME);
    f.until("current health was checked", |f| {
        f.message()
            .contains("Crosspane is now verified and healthy")
    });
    assert!(!f.message().contains("The new instance reported healthy"));
    assert!(!f.enabled(live::ids::REPAIR_RESUME));
    assert!(
        calls(f.world())
            .iter()
            .any(|call| call.starts_with("repair.resume("))
    );
    assert!(
        !calls(f.world())
            .iter()
            .any(|call| call.starts_with("repair.confirm("))
    );
}

#[test]
fn an_install_that_cannot_be_confirmed_from_the_running_agent_never_verifies() {
    let mut f = flow::Flow::new();
    f.until("welcome", |f| {
        f.view().screen == crosspane_installer::view::ScreenId::Welcome
    });
    f.world().lock().unwrap().verify = Some(Err(InstallError::Unavailable));
    f.go_next();
    f.until("support verified", |f| f.verified(10));
    // The install goes ahead with the start click as its consent.
    f.until("the install to be applied", |f| {
        f.world()
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c.starts_with("install.apply:"))
    });
    f.until("the verification to wait", |f| {
        f.world()
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c == "install.verify")
    });
    assert_ne!(
        f.row(20).state,
        crosspane_installer::view::RowState::Verified,
        "an acknowledged apply is not completion"
    );
}

// The production adapters over a scratch target run as in-crate unit tests
// (`src/platform/macos/integration/smoke_tests.rs`): they need the build's private construction
// seam for an approved inventory, which is never public.
