#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The Linux binding against in-memory native domains and scratch targets. Fakes are test-only
//! construction (`Parts` with injected domains); the production `open` composes none of them.
//! No real agent, fixture, compositor, bus, manager, firewall, prompt, service or keyring is used.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_installer::agent_contract::*;
use crosspane_installer::fixture::{FixtureCall, FixtureError, FixtureId, FixtureReceipt};
use crosspane_installer::live::{
    self, Availability, Consent, FixtureReadiness, LiveController, MaintenanceId,
    MaintenanceOutcome, MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome,
    NativeReport, Platform, PracticeFixtures, StatusEvidence, StepReport,
};
use crosspane_installer::platform::linux::integration::{
    AgentSource, DomainFactory, Domains, FirewallReading, Firewalls, FixtureSource, LinuxPlatform,
    Parts, PayloadPreview, Payloads, RuleApply, RulePresence, Services, Support, SupportOutcome,
    SupportedAgentPort, UninstallOffer, UninstallProgress, Uninstaller,
};
use crosspane_installer::platform::linux::{
    firewall::FirewallError,
    native_io::*,
    payload::*,
    service::{AgentEvidence, ServiceAction, ServiceError, ServiceFacts, ServiceResult},
};
use crosspane_installer_core::{
    ApplyOutcome, AttemptId, JobIntent, JobStage, MutationOutcome, ObservationSource, OperationId,
    ResourceObservation, ResourceOwnership, ResourceReceipt, StepId, WaitKind,
};
use crosspane_types::id::NodeId;
use serde_json::{Value, json};

const SUPPORT: StepId = StepId(10);
const PAYLOAD: StepId = StepId(20);
const SERVICE: StepId = StepId(21);
const RESTART: StepId = StepId(22);
const AGENT: StepId = StepId(23);
const NETWORK: StepId = StepId(30);

static ROOT: AtomicU64 = AtomicU64::new(0);

// ---- a staged package ------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

fn elf(version: u8) -> Vec<u8> {
    let mut v = vec![0; 64];
    v[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    v[16..18].copy_from_slice(&3u16.to_le_bytes());
    let machine: u16 = if Architecture::native().unwrap() == Architecture::X86_64 {
        62
    } else {
        183
    };
    v[18..20].copy_from_slice(&machine.to_le_bytes());
    v[20] = 1;
    v[52] = 64;
    v[63] = version;
    v
}

fn contents(version: u8) -> Vec<Vec<u8>> {
    (0..10)
        .map(|i| {
            if i < 5 {
                elf(version)
            } else if i == 5 {
                format!("[Service]\nExecStart={{{{agent_executable}}}} run\nEnvironment={{{{xdg_config_environment}}}}\nEnvironment={{{{xdg_state_environment}}}}\nEnvironment={{{{xdg_runtime_environment}}}}\nEnvironment={{{{crosspane_runtime_environment}}}}\n# fixture-version-{version}\n").into_bytes()
            } else if i < 8 {
                format!("[Desktop Entry]\nType=Application\nName=Crosspane\nExec={{{{{}_executable}}}}\n# fixture-version-{version}\n", if i == 6 { "settings" } else { "installer" }).into_bytes()
            } else {
                format!("inert-resource-{i}-{version}\n").into_bytes()
            }
        })
        .collect()
}

fn manifest(version: u8, bytes: &[Vec<u8>]) -> Manifest {
    Manifest {
        schema_version: 1,
        product_version: format!("0.0.{version}"),
        architecture: Architecture::native().unwrap(),
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf(version)),
        }],
        members: FILES
            .iter()
            .zip(bytes)
            .enumerate()
            .map(|(i, (name, b))| Artifact {
                name: (*name).into(),
                size: b.len(),
                sha256: hex(b),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    }
}

fn number(h: &mut [u8], start: usize, width: usize, n: usize) {
    let s = format!("{n:0width$o}\0", width = width - 1);
    h[start..start + width].copy_from_slice(s.as_bytes());
}

fn checksum(h: &mut [u8]) {
    h[148..156].fill(b' ');
    let n: usize = h.iter().map(|b| usize::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{n:06o}\0 ").as_bytes());
}

fn member(name: &str, b: &[u8]) -> Vec<u8> {
    let mut h = vec![0; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    number(
        &mut h,
        100,
        8,
        if name.starts_with("bin/") {
            0o755
        } else {
            0o644
        },
    );
    number(&mut h, 108, 8, 0);
    number(&mut h, 116, 8, 0);
    number(&mut h, 124, 12, b.len());
    number(&mut h, 136, 12, 0);
    h[156] = b'0';
    h[257..265].copy_from_slice(b"ustar\x0000");
    checksum(&mut h);
    h.extend_from_slice(b);
    h.resize(h.len().div_ceil(512) * 512, 0);
    h
}

fn archive(m: &Manifest, data: &[Vec<u8>]) -> Vec<u8> {
    let mut a = member("manifest.json", &serde_json::to_vec(m).unwrap());
    for (n, b) in FILES.iter().zip(data) {
        a.extend(member(n, b));
    }
    a.extend(vec![0; 1024]);
    a
}

fn package(version: u8) -> Package {
    let data = contents(version);
    let a = archive(&manifest(version, &data), &data);
    Package::read(a.as_slice(), Architecture::native().unwrap(), sha256(&a)).unwrap()
}

// ---- a scratch target -------------------------------------------------------------------------

struct NoCommands;
impl CommandRunner for NoCommands {
    fn run(&self, _: &CommandSpec, _: &Deadline) -> Result<CommandOutput, NativeError> {
        Err(NativeError::Unavailable)
    }
}

struct NoProcesses;
impl ProcessProbe for NoProcesses {
    fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts, NativeError> {
        Err(NativeError::Unavailable)
    }
}

struct Scratch {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
}

impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp421-{}-{}",
            std::process::id(),
            ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let io = Arc::new(
            LinuxNativeIo::scratch(&root, Arc::new(NoCommands), Arc::new(NoProcesses)).unwrap(),
        );
        Self { root, io }
    }

    fn env(&self) -> ChildEnvironment {
        ChildEnvironment::selected(self.io.target(), BTreeMap::new()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---- the world the fake domains share ---------------------------------------------------------

#[derive(Clone)]
enum Sup {
    Supported,
    NotSupported(&'static str),
    Pending(&'static str),
}

#[derive(Clone, Copy, PartialEq)]
enum Evidence {
    Inactive,
    Starting,
    WaitingForKeystore,
    Failed,
    PendingStatus,
    PendingContract,
    StatusFailure,
    Matched,
}

struct World {
    sup: Sup,
    proofs: u32,
    calls: Vec<String>,
    detect: Result<Vec<ResourceReceipt>, PayloadError>,
    observed: Result<Vec<ResourceReceipt>, PayloadError>,
    plan: Result<PayloadPreview, PayloadError>,
    resume: Result<PayloadPreview, PayloadError>,
    apply: Result<(), PayloadError>,
    verify: Result<(), PayloadError>,
    held: Option<OperationId>,
    facts: ServiceFacts,
    observe_err: Option<ServiceError>,
    service_apply: VecDeque<Result<(), ServiceError>>,
    evidence: Evidence,
    fw_read: Result<FirewallReading, FirewallError>,
    fw_plan: Result<String, FirewallError>,
    fw_apply: Result<RuleApply, FirewallError>,
    fw_held: Option<OperationId>,
    offer: UninstallOffer,
    script: VecDeque<UninstallProgress>,
    /// A removal that reports progress forever without settling.
    endless: bool,
    /// Support detection blocks (the worker is busy) until this is cleared.
    hold_support: bool,
}

fn row(ownership: ResourceOwnership, before: ResourceObservation) -> ResourceReceipt {
    ResourceReceipt {
        resource_id: "bin/crosspane-agent".into(),
        resolved_path: "/scratch/bin/crosspane-agent".into(),
        ownership,
        before,
        after: before,
        outcome: MutationOutcome::Unknown,
    }
}

fn facts(enabled: bool, active: bool) -> ServiceFacts {
    ServiceFacts {
        fragment: PathBuf::from("/scratch/crosspane-agent.service"),
        enabled,
        active_state: if active { "active" } else { "inactive" }.into(),
        sub_state: if active { "running" } else { "dead" }.into(),
        main_pid: if active { 4242 } else { 0 },
        needs_reload: false,
        source: ObservationSource::Live,
    }
}

impl World {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            sup: Sup::Supported,
            proofs: 0,
            calls: Vec::new(),
            detect: Ok(vec![row(ResourceOwnership::Created, ResourceObservation::Absent)]),
            observed: Ok(vec![row(ResourceOwnership::Created, ResourceObservation::Matching)]),
            plan: Ok(PayloadPreview {
                version: "0.0.1".into(),
                resuming: false,
            }),
            resume: Ok(PayloadPreview {
                version: "0.0.1".into(),
                resuming: true,
            }),
            apply: Ok(()),
            verify: Ok(()),
            held: None,
            facts: facts(false, false),
            observe_err: None,
            service_apply: VecDeque::new(),
            evidence: Evidence::Matched,
            fw_read: Ok(FirewallReading {
                active: Some(false),
                lan_rule: RulePresence::Absent,
                link: None,
            }),
            fw_plan: Ok("pkexec /usr/bin/ufw allow from 192.0.2.0/24 to any port 47811:47812 proto udp comment 'Crosspane (LAN)'".into()),
            fw_apply: Ok(RuleApply::Dispatched),
            fw_held: None,
            offer: UninstallOffer {
                uninstall: Availability::Available,
                choices: Vec::new(),
            },
            script: VecDeque::new(),
            endless: false,
            hold_support: false,
        }))
    }
}

type Shared = Arc<Mutex<World>>;

fn note(w: &Shared, text: impl Into<String>) {
    w.lock().unwrap().calls.push(text.into());
}

fn calls(w: &Shared) -> Vec<String> {
    w.lock().unwrap().calls.clone()
}

struct FakeSupport {
    w: Shared,
    scratch: Arc<LinuxNativeIo>,
}

impl Support for FakeSupport {
    fn detect(&self, _: Option<&Package>, _: &Deadline) -> SupportOutcome {
        let end = Instant::now() + Duration::from_secs(30);
        while self.w.lock().unwrap().hold_support && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        let sup = {
            let mut w = self.w.lock().unwrap();
            w.proofs += 1;
            w.calls.push("support.detect".into());
            w.sup.clone()
        };
        match sup {
            Sup::Supported => SupportOutcome::Supported(
                self.scratch
                    .scratch_support(SupportObservations {
                        uid: self.scratch.target().paths().uid,
                        architecture: std::env::consts::ARCH.into(),
                        arch_based: true,
                        hyprland_version: [0, 56, 0],
                        protocols_ready: true,
                        runtime_libraries_ready: true,
                        uwsm_managed: true,
                        graphical_target_active: true,
                        graphical_sessions: 1,
                        session_id: "scratch".into(),
                        session_type: "wayland".into(),
                        seat: "seat0".into(),
                        active: true,
                    })
                    .unwrap(),
            ),
            Sup::NotSupported(t) => SupportOutcome::NotSupported(t.into()),
            Sup::Pending(t) => SupportOutcome::Pending(t.into()),
        }
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
}

struct FakePayloads(Shared);

impl Payloads for FakePayloads {
    fn detect(
        &mut self,
        _: &SupportProof,
        _: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        note(&self.0, "payload.detect");
        self.0.lock().unwrap().detect.clone()
    }
    fn observe(
        &mut self,
        _: &SupportProof,
        _: &Package,
    ) -> Result<Vec<ResourceReceipt>, PayloadError> {
        note(&self.0, "payload.observe");
        self.0.lock().unwrap().observed.clone()
    }
    fn plan(
        &mut self,
        _: &SupportProof,
        _: &Package,
        op: OperationId,
        resume: bool,
    ) -> Result<PayloadPreview, PayloadError> {
        note(&self.0, format!("payload.plan(resume={resume})"));
        let mut w = self.0.lock().unwrap();
        let result = if resume {
            w.resume.clone()
        } else {
            w.plan.clone()
        };
        if result.is_ok() {
            w.held = Some(op);
        }
        result
    }
    fn apply(
        &mut self,
        _: &SupportProof,
        _: &Package,
        op: OperationId,
        _: &Deadline,
    ) -> Result<(), PayloadError> {
        note(&self.0, "payload.apply");
        let mut w = self.0.lock().unwrap();
        if w.held.take() != Some(op) {
            return Err(PayloadError::Pending);
        }
        w.apply
    }
    fn verify(
        &mut self,
        _: &SupportProof,
        _: &Package,
        reply: &AgentReply,
        _: u64,
        _: &Deadline,
    ) -> Result<(), PayloadError> {
        note(&self.0, format!("payload.verify(id={})", reply.id));
        self.0.lock().unwrap().verify
    }
}

struct FakeServices(Shared);

impl Services for FakeServices {
    fn prepare(&mut self, _: &Package, _: &Deadline) -> Result<(), ServiceError> {
        Ok(())
    }
    fn observe(&mut self, _: &Deadline) -> Result<ServiceFacts, ServiceError> {
        let w = self.0.lock().unwrap();
        match w.observe_err {
            Some(error) => Err(error),
            None => Ok(w.facts.clone()),
        }
    }
    fn apply(
        &mut self,
        _: &SupportProof,
        action: ServiceAction,
        _: &Deadline,
    ) -> Result<ServiceResult, ServiceError> {
        let mut w = self.0.lock().unwrap();
        w.calls.push(format!("service.{action:?}"));
        if let Some(Err(error)) = w.service_apply.pop_front() {
            return Err(error);
        }
        let before = w.facts.clone();
        match action {
            ServiceAction::Enable => w.facts.enabled = true,
            ServiceAction::Disable => w.facts.enabled = false,
            ServiceAction::Reload => w.facts.needs_reload = false,
            ServiceAction::Start | ServiceAction::Restart => {
                w.facts.active_state = "active".into();
                w.facts.main_pid = 4242;
            }
            ServiceAction::Stop => {
                w.facts.active_state = "inactive".into();
                w.facts.main_pid = 0;
            }
        }
        Ok(ServiceResult {
            action,
            before: before.clone(),
            after: Some(w.facts.clone()),
            previous_instance: (before.main_pid != 0).then_some(9),
            outcome: MutationOutcome::Verified,
        })
    }
    fn agent(
        &mut self,
        _: &ServiceFacts,
        reply: Option<&AgentReply>,
        _: u64,
        _: u64,
        previous: Option<u64>,
        _: &Deadline,
    ) -> Result<AgentEvidence, ServiceError> {
        note(&self.0, format!("service.agent(previous={previous:?})"));
        let bootstrap = || {
            parse_bootstrap(
                br#"{"schema_version":1,"instance_id":9,"pid":4242,"started_unix_ms":1,"phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":"/run/x"}"#,
            )
            .unwrap()
        };
        Ok(match self.0.lock().unwrap().evidence {
            Evidence::Inactive => AgentEvidence::ManagerInactive,
            Evidence::Starting => AgentEvidence::Starting(bootstrap()),
            Evidence::WaitingForKeystore => AgentEvidence::WaitingForKeystore(bootstrap()),
            Evidence::Failed => AgentEvidence::Failed(bootstrap()),
            Evidence::PendingStatus => AgentEvidence::PendingStatus(bootstrap()),
            Evidence::PendingContract => {
                AgentEvidence::PendingHealthContract(PendingHealthReason::Absent, bootstrap())
            }
            Evidence::StatusFailure => AgentEvidence::StatusFailure(CallFailure::Unavailable),
            Evidence::Matched => match reply.map(|r| &r.result) {
                Some(Ok(DecodedReply::Status(StatusAdmission::Supported(h)))) => {
                    AgentEvidence::Matched(h.clone())
                }
                _ => AgentEvidence::PendingStatus(bootstrap()),
            },
        })
    }
}

struct FakeFirewalls(Shared);

impl Firewalls for FakeFirewalls {
    fn read(&mut self, _: &Deadline) -> Result<FirewallReading, FirewallError> {
        note(&self.0, "firewall.read");
        self.0.lock().unwrap().fw_read.clone()
    }
    fn plan_lan(
        &mut self,
        _: &SupportProof,
        op: OperationId,
        _: &Deadline,
    ) -> Result<String, FirewallError> {
        note(&self.0, "firewall.plan");
        let mut w = self.0.lock().unwrap();
        let result = w.fw_plan.clone();
        if result.is_ok() {
            w.fw_held = Some(op);
        }
        result
    }
    fn apply_lan(
        &mut self,
        _: &SupportProof,
        op: OperationId,
        _: &Deadline,
    ) -> Result<RuleApply, FirewallError> {
        note(&self.0, "firewall.apply");
        let mut w = self.0.lock().unwrap();
        if w.fw_held.take() != Some(op) {
            return Err(FirewallError::Stale);
        }
        w.fw_apply.clone()
    }
    fn receipt_lan(&mut self, _: &SupportProof, _: OperationId) -> Option<Vec<u8>> {
        note(&self.0, "firewall.receipt");
        Some(br#"{"version":1,"cidr":"192.0.2.0/24","kind":"Lan","result":0}"#.to_vec())
    }
}

struct FakeUninstaller(Shared);

impl Uninstaller for FakeUninstaller {
    fn inspect(&mut self, _: Option<&Package>, _: &Deadline) -> UninstallOffer {
        note(&self.0, "uninstall.inspect");
        self.0.lock().unwrap().offer.clone()
    }
    fn plan(
        &mut self,
        _: Option<&Package>,
        choices: &[(u16, bool)],
        _: OperationId,
        _: &Deadline,
    ) -> Result<String, String> {
        note(&self.0, format!("uninstall.plan({choices:?})"));
        Ok("Remove Crosspane; keep pairings".into())
    }
    fn begin(&mut self, _: Option<&Package>, _: OperationId) -> Result<(), String> {
        note(&self.0, "uninstall.begin");
        Ok(())
    }
    fn advance(&mut self, _: Option<&Package>) -> UninstallProgress {
        note(&self.0, "uninstall.advance");
        if self.0.lock().unwrap().endless {
            return UninstallProgress::Progress("still going".into());
        }
        self.0
            .lock()
            .unwrap()
            .script
            .pop_front()
            .unwrap_or(UninstallProgress::Finished(
                MaintenanceOutcome::Removed,
                vec!["done".into()],
            ))
    }
    fn follow_up(&mut self, _: Option<&Package>, id: u16, confirm: bool) -> UninstallProgress {
        note(&self.0, format!("uninstall.follow_up({id},{confirm})"));
        self.0
            .lock()
            .unwrap()
            .script
            .pop_front()
            .unwrap_or(UninstallProgress::Finished(
                MaintenanceOutcome::Removed,
                vec!["done".into()],
            ))
    }
}

// ---- a scripted agent and fixtures ------------------------------------------------------------

#[derive(Default)]
struct AgentState {
    queue: AgentQueue,
    proofs: u32,
}

struct FakeAgent(Arc<Mutex<AgentState>>);

impl AgentPort for FakeAgent {
    fn submit(&mut self, call: AgentCall) -> Result<(), CallFailure> {
        self.0.lock().unwrap().queue.submit(call)
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        self.0.lock().unwrap().queue.poll()
    }
}

impl SupportedAgentPort for FakeAgent {
    fn refresh_support(&mut self, _: SupportProof) -> Result<(), NativeError> {
        self.0.lock().unwrap().proofs += 1;
        Ok(())
    }
}

struct NoFixtures;

impl PracticeFixtures for NoFixtures {
    fn launch(&mut self, _: AttemptId) -> Result<(), FixtureError> {
        Err(FixtureError::Unavailable)
    }
    fn readiness(&mut self) -> FixtureReadiness {
        FixtureReadiness::Idle
    }
    fn submit(&mut self, _: FixtureCall) -> Result<(), FixtureError> {
        Err(FixtureError::Unavailable)
    }
    fn poll(&mut self) -> Vec<FixtureReceipt> {
        Vec::new()
    }
    fn complete_closed(&mut self, _: AttemptId, _: FixtureId) -> Result<(), FixtureError> {
        Ok(())
    }
    fn retire(&mut self) {}
}

// ---- the rig ----------------------------------------------------------------------------------

struct Rig {
    platform: LinuxPlatform,
    w: Shared,
    agent: Arc<Mutex<AgentState>>,
    scratch: Scratch,
    clock: Arc<AtomicU64>,
    next_op: u64,
    steps: Vec<StepReport>,
    maintenance: Vec<MaintenanceReport>,
}

impl Rig {
    fn new() -> Self {
        Self::build(true)
    }

    fn without_package() -> Self {
        Self::build(false)
    }

    fn build(with_package: bool) -> Self {
        Self::build_with(with_package, None)
    }

    fn build_with(with_package: bool, payload: Option<PathBuf>) -> Self {
        let scratch = Scratch::new();
        let w = World::new();
        let clock = Arc::new(AtomicU64::new(10_000));
        let time = clock.clone();
        let agent = Arc::new(Mutex::new(AgentState::default()));
        let support = Arc::new(FakeSupport {
            w: w.clone(),
            scratch: scratch.io.clone(),
        });
        let shared = w.clone();
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: Box::new(FakePayloads(shared.clone())),
            services: Box::new(FakeServices(shared.clone())),
            firewalls: Box::new(FakeFirewalls(shared.clone())),
            uninstaller: Box::new(FakeUninstaller(shared)),
        });
        let platform = LinuxPlatform::compose(Parts {
            io: scratch.io.clone(),
            env: scratch.env(),
            clock: Arc::new(move || time.load(Ordering::SeqCst)),
            payload,
            support,
            domains,
            agent: AgentSource::Injected(Box::new(FakeAgent(agent.clone()))),
            fixtures: FixtureSource::Injected(Box::new(NoFixtures)),
            package: with_package.then(|| package(1)),
        })
        .unwrap();
        Self {
            platform,
            w,
            agent,
            scratch,
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

    fn consent(plan: &JobIntent, apply: &JobIntent) -> Consent {
        Consent {
            plan: plan.operation,
            operation: apply.operation,
            revision: 7,
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

    /// Run one stage and return its single report.
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

    /// Detect, plan and apply a step with a matching consent; return the apply outcome.
    fn consented(&mut self, step: StepId, status: Option<AgentReply>) -> NativeOutcome {
        let (plan_job, plan) = self.run(step, JobStage::Plan, None, status.clone());
        assert!(
            matches!(plan.outcome, NativeOutcome::Planned { .. }),
            "{:?}",
            plan.outcome
        );
        let apply = self.job(step, JobStage::Apply);
        let consent = Self::consent(&plan_job, &apply);
        self.run_job(apply, Some(consent), status).1.outcome
    }

    fn advance(&self, ms: u64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
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

    fn root(&self) -> &Path {
        &self.scratch.root
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.platform.shutdown();
    }
}

fn health_json(peers: Value, keystore: &str, recovery: u64) -> Value {
    let mut v: Value = serde_json::from_str(
        r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.1","features":["video"]},"instance":{"id":9,"pid":4242,"uid":1000,
"exe":"/home/u/.local/bin/crosspane-agent","runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790942400000},
"config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
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
    v["result"]["installer"]["peers"] = peers;
    v["result"]["installer"]["keystore"] = json!(keystore);
    v["result"]["installer"]["recovery_pending"] = json!(recovery);
    v
}

fn peer_json(connected: bool) -> Value {
    json!({"node": NodeId([0x22; 32]).to_string(), "name": "other", "connected": connected,
        "link_generation": 2, "features": [], "grants_given": [], "last_source_parking": null,
        "counters": {"e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,
        "e1_target_ended":0,"e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,
        "e1_command_releases":0,"e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,
        "e2_dest_returned":0,"e2_frames_presented":0,"e2_returns_failed":0}})
}

fn status(id: u64, body: Value) -> AgentReply {
    let decoded = match parse_status(&serde_json::to_vec(&body).unwrap(), AgentPlatform::Linux) {
        Ok(admission) => Ok(DecodedReply::Status(admission)),
        Err(_) => Err(CallFailure::Unavailable),
    };
    AgentReply {
        id,
        observed_at_ms: 10_000,
        source: ObservationSource::Live,
        result: decoded,
    }
}

fn healthy(id: u64) -> AgentReply {
    status(id, health_json(json!([]), "os_store", 0))
}

fn connected(id: u64) -> AgentReply {
    status(id, health_json(json!([peer_json(true)]), "os_store", 0))
}

fn detect_needs_action(outcome: &NativeOutcome) -> Option<bool> {
    match outcome {
        NativeOutcome::Detected { needs_action } => Some(*needs_action),
        _ => None,
    }
}

// ---- support ----------------------------------------------------------------------------------

#[test]
fn an_unsupported_session_is_refused_with_zero_mutation_calls() {
    let mut rig = Rig::new();
    rig.set(|w| w.sup = Sup::NotSupported("Setup supports uwsm sessions only."));
    let (_, report) = rig.run(SUPPORT, JobStage::Detect, None, None);
    assert_eq!(report.outcome, NativeOutcome::Unsupported);
    assert!(report.detail.contains("uwsm"));
    // Later steps can't proceed either, and no domain beyond detection is ever consulted.
    for (step, stage) in [
        (PAYLOAD, JobStage::Plan),
        (SERVICE, JobStage::Plan),
        (NETWORK, JobStage::Plan),
    ] {
        let outcome = rig.outcome(step, stage);
        assert!(
            matches!(
                outcome,
                NativeOutcome::Unsupported | NativeOutcome::Waiting(_)
            ),
            "{step:?}: {outcome:?}"
        );
    }
    let apply = rig.outcome(PAYLOAD, JobStage::Apply);
    assert_eq!(apply, NativeOutcome::Applied(ApplyOutcome::Refused));
    let calls = rig.calls();
    assert!(
        calls.iter().all(|c| c == "support.detect"),
        "only detection ran: {calls:?}"
    );
}

#[test]
fn an_ambiguous_or_unreadable_session_stays_pending_and_changes_nothing() {
    let mut rig = Rig::new();
    rig.set(|w| w.sup = Sup::Pending("More than one graphical session could be this one."));
    assert_eq!(
        rig.outcome(SUPPORT, JobStage::Detect),
        NativeOutcome::Waiting(WaitKind::Contract)
    );
    assert_eq!(
        rig.outcome(SUPPORT, JobStage::Verify),
        NativeOutcome::Waiting(WaitKind::Contract)
    );
    assert_eq!(
        rig.outcome(PAYLOAD, JobStage::Detect),
        NativeOutcome::Waiting(WaitKind::Contract)
    );
    assert!(rig.calls().iter().all(|c| c == "support.detect"));
}

#[test]
fn a_supported_session_detects_and_verifies_with_the_targets_source() {
    let mut rig = Rig::new();
    let (_, detect) = rig.run(SUPPORT, JobStage::Detect, None, None);
    assert_eq!(detect_needs_action(&detect.outcome), Some(false));
    let (_, verify) = rig.run(SUPPORT, JobStage::Verify, None, None);
    assert!(matches!(
        verify.outcome,
        NativeOutcome::Verified {
            source: ObservationSource::Live,
            ..
        }
    ));
}

// ---- payload ----------------------------------------------------------------------------------

#[test]
fn a_staged_payload_that_is_a_fifo_link_or_oversized_is_pending_and_never_wedges_the_worker() {
    use rustix::fs::{FileType, Mode};
    for case in ["fifo", "link", "huge"] {
        let dir = PathBuf::from(format!(
            "/tmp/cp421-stage-{}-{}",
            std::process::id(),
            ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let sum = dir.join("payload.sha256");
        match case {
            "fifo" => rustix::fs::mknodat(
                rustix::fs::CWD,
                &sum,
                FileType::Fifo,
                Mode::from_raw_mode(0o600),
                0,
            )
            .unwrap(),
            "link" => std::os::unix::fs::symlink("/dev/zero", &sum).unwrap(),
            _ => std::fs::write(&sum, "a".repeat(1 << 20)).unwrap(),
        }
        let mut rig = Rig::build_with(false, Some(dir.clone()));
        let (_, detect) = rig.run(PAYLOAD, JobStage::Detect, None, None);
        assert_eq!(
            detect.outcome,
            NativeOutcome::Waiting(WaitKind::User),
            "{case}"
        );
        assert!(detect.detail.contains("Nothing will be changed"), "{case}");
        assert!(!rig.calls().iter().any(|c| c.starts_with("payload.")));
        drop(rig);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn a_missing_payload_source_is_pending_with_no_mutation() {
    let mut rig = Rig::without_package();
    let (_, detect) = rig.run(PAYLOAD, JobStage::Detect, None, None);
    assert_eq!(detect.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(detect.detail.contains("--payload"), "{}", detect.detail);
    assert_eq!(
        rig.outcome(PAYLOAD, JobStage::Plan),
        NativeOutcome::Waiting(WaitKind::User)
    );
    assert_eq!(
        rig.outcome(PAYLOAD, JobStage::Apply),
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert!(
        !rig.calls().iter().any(|c| c.starts_with("payload.")),
        "the payload domain is never touched: {:?}",
        rig.calls()
    );
}

#[test]
fn payload_apply_needs_a_consent_for_the_exact_plan_that_was_previewed() {
    let mut rig = Rig::new();
    let (plan_job, plan) = rig.run(PAYLOAD, JobStage::Plan, None, None);
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome)
    };
    assert!(preview.contains("0.0.1"));
    // A consent naming some other plan is refused before anything is written.
    let apply = rig.job(PAYLOAD, JobStage::Apply);
    let wrong = Consent {
        plan: OperationId(plan_job.operation.0 + 40),
        operation: apply.operation,
        revision: 3,
    };
    let (_, refused) = rig.run_job(apply, Some(wrong), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("payload.apply"), 0);
    // No consent at all is refused too, and the plan stays gone: it is single use.
    let apply = rig.job(PAYLOAD, JobStage::Apply);
    let (_, none) = rig.run_job(apply, None, None);
    assert_eq!(none.outcome, NativeOutcome::Applied(ApplyOutcome::Refused));
    // A fresh preview and its own consent apply.
    assert_eq!(
        rig.consented(PAYLOAD, None),
        NativeOutcome::Applied(ApplyOutcome::Applied)
    );
    assert_eq!(rig.count("payload.apply"), 1);
}

#[test]
fn payload_outcomes_map_honestly_and_unknown_is_never_retried_blindly() {
    let mut rig = Rig::new();
    rig.set(|w| w.apply = Err(PayloadError::OutcomeUnknown));
    assert_eq!(
        rig.consented(PAYLOAD, None),
        NativeOutcome::Applied(ApplyOutcome::Unknown)
    );
    rig.set(|w| w.apply = Err(PayloadError::Foreign));
    assert_eq!(
        rig.consented(PAYLOAD, None),
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    rig.set(|w| w.apply = Err(PayloadError::Invalid));
    assert_eq!(
        rig.consented(PAYLOAD, None),
        NativeOutcome::Applied(ApplyOutcome::Failed)
    );
    assert_eq!(
        rig.count("payload.apply"),
        3,
        "each attempt was a fresh, consented plan"
    );
}

#[test]
fn an_interrupted_install_is_resumed_from_its_journal_not_replanned() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.detect = Err(PayloadError::Pending);
        w.plan = Err(PayloadError::Pending);
    });
    let (_, detect) = rig.run(PAYLOAD, JobStage::Detect, None, None);
    assert_eq!(detect_needs_action(&detect.outcome), Some(true));
    let (_, plan) = rig.run(PAYLOAD, JobStage::Plan, None, None);
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome)
    };
    assert!(preview.to_lowercase().contains("interrupted"), "{preview}");
    let calls = rig.calls();
    assert!(calls.contains(&"payload.plan(resume=false)".to_owned()));
    assert!(calls.contains(&"payload.plan(resume=true)".to_owned()));
}

#[test]
fn foreign_files_are_left_alone_and_reported() {
    let mut rig = Rig::new();
    rig.set(|w| w.detect = Err(PayloadError::Foreign));
    let (_, detect) = rig.run(PAYLOAD, JobStage::Detect, None, None);
    assert_eq!(detect.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert!(detect.detail.contains("left"));
    assert_eq!(rig.count("payload.apply"), 0);
}

#[test]
fn already_installed_matching_files_need_no_action_and_verify_only_when_all_match() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.detect = Ok(vec![row(
            ResourceOwnership::Created,
            ResourceObservation::Matching,
        )])
    });
    assert_eq!(
        detect_needs_action(&rig.outcome(PAYLOAD, JobStage::Detect)),
        Some(false)
    );
    assert!(matches!(
        rig.outcome(PAYLOAD, JobStage::Verify),
        NativeOutcome::Verified { .. }
    ));
    rig.set(|w| {
        w.observed = Ok(vec![
            row(ResourceOwnership::Created, ResourceObservation::Matching),
            row(ResourceOwnership::Created, ResourceObservation::Different),
        ])
    });
    assert_eq!(
        rig.outcome(PAYLOAD, JobStage::Verify),
        NativeOutcome::Waiting(WaitKind::Contract)
    );
}

#[test]
fn a_plan_whose_apply_was_refused_by_a_gate_can_never_be_consented_again() {
    let mut rig = Rig::new();
    let (plan_job, plan) = rig.run(PAYLOAD, JobStage::Plan, None, None);
    assert!(matches!(plan.outcome, NativeOutcome::Planned { .. }));
    // The first Apply is refused at the support gate, before it reaches the plan.
    rig.set(|w| w.sup = Sup::Pending("unknown session"));
    let apply = rig.job(PAYLOAD, JobStage::Apply);
    let consent = Rig::consent(&plan_job, &apply);
    let (_, refused) = rig.run_job(apply, Some(consent), None);
    assert!(
        matches!(
            refused.outcome,
            NativeOutcome::Applied(ApplyOutcome::Refused) | NativeOutcome::Waiting(_)
        ),
        "{:?}",
        refused.outcome
    );
    // Support is back, and the same superseded consent is replayed on a new Apply job.
    rig.set(|w| w.sup = Sup::Supported);
    let again = rig.job(PAYLOAD, JobStage::Apply);
    let replay = Consent {
        operation: again.operation,
        ..consent
    };
    let (_, outcome) = rig.run_job(again, Some(replay), None);
    assert_eq!(
        outcome.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused),
        "a held plan is single use"
    );
    assert_eq!(rig.count("payload.apply"), 0, "nothing was installed");
}

#[test]
fn a_consent_for_another_apply_job_is_refused() {
    let mut rig = Rig::new();
    let (plan_job, _) = rig.run(PAYLOAD, JobStage::Plan, None, None);
    let apply = rig.job(PAYLOAD, JobStage::Apply);
    let other = rig.job(PAYLOAD, JobStage::Apply);
    let consent = Rig::consent(&plan_job, &other);
    let (_, outcome) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        outcome.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("payload.apply"), 0);
}

// ---- service ----------------------------------------------------------------------------------

#[test]
fn a_service_plan_that_finds_nothing_to_change_waits_and_never_answers_as_detection() {
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    assert_eq!(
        rig.outcome(SERVICE, JobStage::Plan),
        NativeOutcome::Waiting(WaitKind::User)
    );
}

#[test]
fn startup_runs_each_manager_command_with_a_fresh_proof_and_stops_on_doubt() {
    let mut rig = Rig::new();
    rig.set(|w| w.facts.needs_reload = true);
    let before = rig.w.lock().unwrap().proofs;
    assert_eq!(
        rig.consented(SERVICE, None),
        NativeOutcome::Applied(ApplyOutcome::Applied)
    );
    let calls = rig.calls();
    let order: Vec<_> = calls.iter().filter(|c| c.starts_with("service.")).collect();
    assert_eq!(order, ["service.Reload", "service.Enable", "service.Start"]);
    // One proof for the plan, three for the commands, one for the resume hint.
    assert!(rig.w.lock().unwrap().proofs - before >= 4);

    // A doubtful answer stops the sequence: nothing after it is sent.
    let mut rig = Rig::new();
    rig.set(|w| {
        w.service_apply
            .extend([Ok(()), Err(ServiceError::OutcomeUnknown)])
    });
    assert_eq!(
        rig.consented(SERVICE, None),
        NativeOutcome::Applied(ApplyOutcome::Unknown)
    );
    assert_eq!(
        rig.count("service."),
        2,
        "Enable then the doubtful Start; no resend"
    );
}

#[test]
fn a_foreign_unit_is_refused_and_nothing_is_started() {
    let mut rig = Rig::new();
    rig.set(|w| w.service_apply.push_back(Err(ServiceError::Foreign)));
    assert_eq!(
        rig.consented(SERVICE, None),
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    rig.set(|w| w.observe_err = Some(ServiceError::Foreign));
    assert_eq!(
        rig.outcome(SERVICE, JobStage::Detect),
        NativeOutcome::Failed
    );
}

#[test]
fn service_detection_distinguishes_running_enabled_from_everything_else() {
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    assert_eq!(
        detect_needs_action(&rig.outcome(SERVICE, JobStage::Detect)),
        Some(false)
    );
    rig.set(|w| w.facts = facts(true, false));
    assert_eq!(
        detect_needs_action(&rig.outcome(SERVICE, JobStage::Detect)),
        Some(true)
    );
    assert_eq!(
        rig.outcome(SERVICE, JobStage::Verify),
        NativeOutcome::Waiting(WaitKind::Contract)
    );
}

// ---- restart and agent ------------------------------------------------------------------------

#[test]
fn restart_is_asked_once_then_only_the_new_instance_is_looked_at() {
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    assert_eq!(
        detect_needs_action(&rig.outcome(RESTART, JobStage::Detect)),
        Some(true)
    );
    assert_eq!(
        rig.consented(RESTART, None),
        NativeOutcome::Applied(ApplyOutcome::Applied)
    );
    assert_eq!(
        detect_needs_action(&rig.outcome(RESTART, JobStage::Detect)),
        Some(false),
        "a second check doesn't restart again"
    );
    let (_, verify) = rig.run(RESTART, JobStage::Verify, None, Some(healthy(5)));
    assert!(
        matches!(verify.outcome, NativeOutcome::Verified { .. }),
        "{verify:?}"
    );
    assert!(
        rig.calls()
            .iter()
            .any(|c| c == "service.agent(previous=Some(9))"),
        "the new instance must differ from the one that was running: {:?}",
        rig.calls()
    );
}

#[test]
fn every_agent_evidence_variant_has_an_honest_outcome() {
    let cases = [
        (
            Evidence::Inactive,
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Evidence::Starting,
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Evidence::WaitingForKeystore,
            NativeOutcome::Waiting(WaitKind::User),
        ),
        (Evidence::Failed, NativeOutcome::Failed),
        (
            Evidence::PendingStatus,
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Evidence::PendingContract,
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Evidence::StatusFailure,
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
    ];
    for (evidence, expected) in cases {
        let mut rig = Rig::new();
        rig.set(|w| {
            w.evidence = evidence;
            w.facts = facts(true, true);
        });
        let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(healthy(5)));
        assert_eq!(report.outcome, expected, "{}", report.detail);
        assert_eq!(
            rig.count("payload.verify"),
            0,
            "no receipt is completed without matched health"
        );
    }
}

#[test]
fn the_agent_verifies_only_a_matched_os_store_status_and_then_completes_the_receipt() {
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    let (_, ok) = rig.run(AGENT, JobStage::Verify, None, Some(healthy(5)));
    assert!(
        matches!(ok.outcome, NativeOutcome::Verified { .. }),
        "{ok:?}"
    );
    assert!(rig.calls().iter().any(|c| c == "payload.verify(id=5)"));

    // A file-held key is not a secure identity, even when everything else is healthy.
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    let file = status(6, health_json(json!([]), "file", 0));
    let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(file));
    assert_eq!(report.outcome, NativeOutcome::Waiting(WaitKind::User));
    assert_eq!(rig.count("payload.verify"), 0);

    // Unfinished recovery is not healthy either.
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    let pending = status(7, health_json(json!([]), "os_store", 2));
    let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(pending));
    assert_eq!(report.outcome, NativeOutcome::Waiting(WaitKind::Contract));
    assert_eq!(rig.count("payload.verify"), 0);
}

#[test]
fn the_receipt_step_maps_its_own_refusals_and_a_missing_status_never_verifies() {
    for (result, expected) in [
        (
            Err(PayloadError::Pending),
            NativeOutcome::Waiting(WaitKind::Contract),
        ),
        (
            Err(PayloadError::Foreign),
            NativeOutcome::Waiting(WaitKind::User),
        ),
    ] {
        let mut rig = Rig::new();
        rig.set(|w| {
            w.facts = facts(true, true);
            w.verify = result;
        });
        let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(healthy(5)));
        assert_eq!(report.outcome, expected, "{}", report.detail);
    }
    let mut rig = Rig::new();
    rig.set(|w| w.facts = facts(true, true));
    let (_, report) = rig.run(AGENT, JobStage::Verify, None, None);
    assert!(!matches!(report.outcome, NativeOutcome::Verified { .. }));
    // Without a staged payload the receipt can't be completed, so the step stays pending.
    let mut rig = Rig::without_package();
    rig.set(|w| w.facts = facts(true, true));
    let (_, report) = rig.run(AGENT, JobStage::Verify, None, Some(healthy(5)));
    assert_eq!(report.outcome, NativeOutcome::Waiting(WaitKind::User));
}

// ---- network ----------------------------------------------------------------------------------

fn reading(active: Option<bool>, lan_rule: RulePresence) -> Result<FirewallReading, FirewallError> {
    Ok(FirewallReading {
        active,
        lan_rule,
        link: Some("192.0.2.0/24 on enp1s0".into()),
    })
}

#[test]
fn network_detection_never_assumes_an_unreadable_firewall_or_rule_is_fine() {
    let cases = [
        (reading(Some(false), RulePresence::Absent), Some(false)),
        (reading(Some(true), RulePresence::Present), Some(false)),
        (reading(Some(true), RulePresence::Absent), Some(true)),
    ];
    for (read, expected) in cases {
        let mut rig = Rig::new();
        rig.set(|w| w.fw_read = read);
        let (_, report) = rig.run(NETWORK, JobStage::Detect, None, Some(healthy(5)));
        assert_eq!(detect_needs_action(&report.outcome), expected, "{report:?}");
    }
    for (read, wait) in [
        (reading(None, RulePresence::Absent), WaitKind::Contract),
        (
            reading(Some(true), RulePresence::Unknown),
            WaitKind::Contract,
        ),
        (reading(Some(true), RulePresence::Modified), WaitKind::User),
        (Err(FirewallError::Invalid), WaitKind::Contract),
    ] {
        let mut rig = Rig::new();
        rig.set(|w| w.fw_read = read);
        let (_, report) = rig.run(NETWORK, JobStage::Detect, None, Some(healthy(5)));
        assert_eq!(
            report.outcome,
            NativeOutcome::Waiting(wait),
            "{}",
            report.detail
        );
    }
}

#[test]
fn a_connected_peer_settles_the_network_and_nothing_else_does() {
    let mut rig = Rig::new();
    rig.set(|w| w.fw_read = reading(Some(true), RulePresence::Present));
    // Even with the rule present and the firewall readable, no connection means no proof.
    let (_, waiting) = rig.run(NETWORK, JobStage::Verify, None, Some(healthy(5)));
    assert_eq!(waiting.outcome, NativeOutcome::Waiting(WaitKind::Peer));
    let (_, verified) = rig.run(NETWORK, JobStage::Verify, None, Some(connected(6)));
    assert!(matches!(verified.outcome, NativeOutcome::Verified { .. }));
    // A connected peer also skips the firewall read entirely at detection.
    let reads = rig.count("firewall.read");
    let (_, detect) = rig.run(NETWORK, JobStage::Detect, None, Some(connected(7)));
    assert_eq!(detect_needs_action(&detect.outcome), Some(false));
    assert_eq!(rig.count("firewall.read"), reads);
}

#[test]
fn a_reply_that_is_not_from_this_target_is_not_traffic_evidence() {
    let mut rig = Rig::new();
    let mut reply = connected(6);
    reply.source = ObservationSource::Demo;
    let (_, report) = rig.run(NETWORK, JobStage::Verify, None, Some(reply));
    assert_eq!(report.outcome, NativeOutcome::Waiting(WaitKind::Peer));
}

#[test]
fn the_firewall_rule_is_consented_per_plan_and_only_the_lan_rule_is_ever_offered() {
    let mut rig = Rig::new();
    rig.set(|w| w.fw_read = reading(Some(true), RulePresence::Absent));
    let (plan_job, plan) = rig.run(NETWORK, JobStage::Plan, None, None);
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome)
    };
    assert!(
        preview.contains("ufw allow from"),
        "the exact command is shown: {preview}"
    );
    assert!(preview.contains("Crosspane (LAN)"));
    assert!(
        !preview.contains("5353 proto udp comment 'Crosspane (mDNS)'"),
        "no mDNS rule is planned"
    );
    let apply = rig.job(NETWORK, JobStage::Apply);
    let wrong = Consent {
        plan: OperationId(plan_job.operation.0 + 9),
        operation: apply.operation,
        revision: 1,
    };
    let (_, refused) = rig.run_job(apply, Some(wrong), None);
    assert_eq!(
        refused.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert_eq!(rig.count("firewall.apply"), 0);
}

#[test]
fn firewall_results_map_exit_zero_to_unverified_and_doubt_to_unknown() {
    let cases: Vec<(Result<RuleApply, FirewallError>, NativeOutcome)> = vec![
        (
            Ok(RuleApply::Dispatched),
            NativeOutcome::Applied(ApplyOutcome::Applied),
        ),
        (
            Ok(RuleApply::AlreadyPresent),
            NativeOutcome::Applied(ApplyOutcome::Applied),
        ),
        (
            Ok(RuleApply::NotDispatched),
            NativeOutcome::Applied(ApplyOutcome::Refused),
        ),
        (
            Ok(RuleApply::Changed),
            NativeOutcome::Applied(ApplyOutcome::Refused),
        ),
        (
            Ok(RuleApply::NoPromptAgent(None)),
            NativeOutcome::Applied(ApplyOutcome::Refused),
        ),
        (
            Ok(RuleApply::Unknown),
            NativeOutcome::Applied(ApplyOutcome::Unknown),
        ),
        (
            Ok(RuleApply::Failed),
            NativeOutcome::Applied(ApplyOutcome::Failed),
        ),
        (
            Err(FirewallError::Native(NativeError::Timeout)),
            NativeOutcome::Applied(ApplyOutcome::Unknown),
        ),
        (
            Err(FirewallError::Stale),
            NativeOutcome::Applied(ApplyOutcome::Refused),
        ),
    ];
    for (result, expected) in cases {
        let mut rig = Rig::new();
        rig.set(|w| {
            w.fw_read = reading(Some(true), RulePresence::Absent);
            w.fw_apply = result;
        });
        assert_eq!(rig.consented(NETWORK, None), expected);
    }
}

#[test]
fn a_missing_polkit_agent_shows_the_manual_command_instead_of_a_silent_failure() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.fw_read = reading(Some(true), RulePresence::Absent);
        w.fw_apply = Ok(RuleApply::NoPromptAgent(Some(
            "sudo /usr/bin/ufw allow from 192.0.2.0/24 to any port 47811:47812 proto udp comment 'Crosspane (LAN)'".into(),
        )));
    });
    let (plan_job, _) = rig.run(NETWORK, JobStage::Plan, None, None);
    let apply = rig.job(NETWORK, JobStage::Apply);
    let consent = Rig::consent(&plan_job, &apply);
    let (_, report) = rig.run_job(apply, Some(consent), None);
    assert_eq!(
        report.outcome,
        NativeOutcome::Applied(ApplyOutcome::Refused)
    );
    assert!(
        report.detail.contains("sudo /usr/bin/ufw"),
        "{}",
        report.detail
    );
}

// ---- maintenance ------------------------------------------------------------------------------

#[test]
fn removal_is_planned_previewed_confirmed_and_driven_through_follow_ups() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.script.extend([
            UninstallProgress::Progress("Startup disabled.".into()),
            UninstallProgress::FollowUp {
                id: 1,
                label: "Remove the firewall rule".into(),
                preview: "pkexec /usr/bin/ufw delete allow from 192.0.2.0/24 ...".into(),
            },
            UninstallProgress::Finished(MaintenanceOutcome::Partial, vec!["Files kept.".into()]),
        ])
    });
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    assert!(matches!(
        &rig.maintenance[0],
        MaintenanceReport::Inspected {
            repair: Availability::NotAvailableYet(_),
            ..
        }
    ));
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
    assert_eq!(rig.count("uninstall.begin"), 0);

    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: vec![(1, false)],
        status: None,
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
            status: None,
        },
    ));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::FollowUp { .. }))
    });
    // The run waits for the person at the follow-up; nothing else is dispatched.
    assert_eq!(rig.count("uninstall.follow_up"), 0);
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmFollowUp {
            id,
            follow_up: 1,
            revision: 5,
        },
    ));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
    });
    assert!(
        rig.calls()
            .contains(&"uninstall.follow_up(1,true)".to_owned())
    );
    let MaintenanceReport::Finished { outcome, .. } = rig.maintenance.last().unwrap() else {
        panic!()
    };
    assert_eq!(*outcome, MaintenanceOutcome::Partial);
}

#[test]
fn a_removal_that_never_settles_is_stopped_with_a_failed_report_instead_of_spinning() {
    let mut rig = Rig::new();
    rig.set(|w| w.endless = true);
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Planned { .. }))
    });
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 4,
            status: None,
        },
    ));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
    });
    let Some(MaintenanceReport::Finished { outcome, .. }) = rig.maintenance.last() else {
        panic!("the run ends with its report")
    };
    assert_eq!(*outcome, MaintenanceOutcome::Failed);
    // A new inspection is allowed once the run has ended.
    let before = rig.maintenance.len();
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect {
        id: MaintenanceId(2),
    }));
    rig.pump(|r| r.maintenance.len() > before);
    assert!(matches!(
        rig.maintenance.last(),
        Some(MaintenanceReport::Inspected { .. })
    ));
}

#[test]
fn a_running_removal_waiting_for_a_follow_up_is_never_abandoned_by_a_new_inspection() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.script.extend([UninstallProgress::FollowUp {
            id: 1,
            label: "Remove the firewall rule".into(),
            preview: "pkexec ...".into(),
        }])
    });
    let id = MaintenanceId(1);
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect { id }));
    rig.pump(|r| !r.maintenance.is_empty());
    rig.send(NativeJob::Maintenance(MaintenanceRequest::PlanUninstall {
        id,
        choices: Vec::new(),
        status: None,
    }));
    rig.pump(|r| r.maintenance.len() >= 2);
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::ConfirmUninstall {
            id,
            revision: 4,
            status: None,
        },
    ));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::FollowUp { .. }))
    });
    let before = rig.maintenance.len();
    rig.send(NativeJob::Maintenance(MaintenanceRequest::Inspect {
        id: MaintenanceId(2),
    }));
    rig.pump(|r| r.maintenance.len() > before);
    assert!(matches!(
        rig.maintenance.last(),
        Some(MaintenanceReport::Refused { .. })
    ));
    assert_eq!(rig.count("uninstall.inspect"), 1);
    // The original run can still be answered.
    rig.send(NativeJob::Maintenance(
        MaintenanceRequest::DeclineFollowUp { id, follow_up: 1 },
    ));
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
    });
}

#[test]
fn a_stale_maintenance_id_is_refused_and_confirm_cannot_be_repeated() {
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
}

// ---- the GUI-thread agent port ----------------------------------------------------------------

#[test]
fn a_mutating_agent_call_waits_for_a_fresh_proof_and_a_read_does_not() {
    let mut rig = Rig::new();
    let before = rig.w.lock().unwrap().proofs;
    let agent = rig.platform.agent();
    agent
        .submit(AgentCall {
            id: 1,
            request: InstallerRequest::Status,
            timeout_ms: 1000,
        })
        .unwrap();
    assert_eq!(
        rig.w.lock().unwrap().proofs,
        before,
        "a read asks for no proof"
    );
    let agent = rig.platform.agent();
    agent
        .submit(AgentCall {
            id: 2,
            request: InstallerRequest::PairListen { allow_input: false },
            timeout_ms: 1000,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let _ = rig.platform.agent().poll();
        if rig.agent.lock().unwrap().proofs >= 1 {
            break;
        }
        assert!(Instant::now() < end, "no proof reached the port");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        rig.w.lock().unwrap().proofs,
        before + 1,
        "exactly one proof for one mutation"
    );
    // Ids must still increase, so an old id is refused locally.
    let late = rig.platform.agent().submit(AgentCall {
        id: 2,
        request: InstallerRequest::Status,
        timeout_ms: 1000,
    });
    assert!(late.is_err());
}

#[test]
fn a_queued_agent_call_spends_its_own_deadline_and_is_never_sent_late() {
    let mut rig = Rig::new();
    // The worker is busy (support detection blocks), so the proof for a mutation is slow.
    rig.set(|w| w.hold_support = true);
    rig.platform
        .agent()
        .submit(AgentCall {
            id: 1,
            request: InstallerRequest::PairListen { allow_input: false },
            timeout_ms: 1000,
        })
        .unwrap();
    rig.platform
        .agent()
        .submit(AgentCall {
            id: 2,
            request: InstallerRequest::Status,
            timeout_ms: 1000,
        })
        .unwrap();
    rig.platform
        .agent()
        .submit(AgentCall {
            id: 3,
            request: InstallerRequest::PairListen { allow_input: false },
            timeout_ms: 4000,
        })
        .unwrap();
    rig.advance(1_500);
    let failed: Vec<u64> = rig.platform.agent().poll().iter().map(|r| r.id).collect();
    assert_eq!(
        failed,
        [1, 2],
        "both calls ran out of time while they waited"
    );
    rig.set(|w| w.hold_support = false);
    let end = Instant::now() + Duration::from_secs(30);
    let sent = loop {
        let _ = rig.platform.agent().poll();
        let calls = rig.agent.lock().unwrap().queue.take_calls();
        if !calls.is_empty() {
            break calls;
        }
        assert!(Instant::now() < end, "the live call was never sent");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].id, 3, "an expired call is never sent late");
    assert!(
        sent[0].timeout_ms <= 2_500,
        "only the rest of its own budget goes with it: {}",
        sent[0].timeout_ms
    );
}

#[test]
fn when_support_is_not_proved_a_mutating_call_fails_without_being_sent() {
    let mut rig = Rig::new();
    rig.set(|w| w.sup = Sup::Pending("unknown session"));
    rig.platform
        .agent()
        .submit(AgentCall {
            id: 1,
            request: InstallerRequest::PairListen { allow_input: false },
            timeout_ms: 1000,
        })
        .unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    let reply = loop {
        let mut replies = rig.platform.agent().poll();
        if let Some(reply) = replies.pop() {
            break reply;
        }
        assert!(Instant::now() < end, "the failure never arrived");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(reply.id, 1);
    assert!(reply.result.is_err());
    assert!(
        rig.agent.lock().unwrap().queue.take_calls().is_empty(),
        "nothing reached the agent"
    );
    let _ = rig.root();
    rig.advance(1);
}

// ---- the shared controller on the Linux binding ------------------------------------------------

#[test]
fn the_linux_graph_is_data_the_core_validates() {
    let desc = crosspane_installer::platform::linux::integration::description(None);
    let steps: Vec<u16> = desc.steps.iter().map(|s| s.id.0).collect();
    assert_eq!(steps, [10, 20, 21, 22, 23, 30]);
    // Installed means files and startup; the live checks are separate.
    let installed: Vec<u16> = desc
        .steps
        .iter()
        .filter(|s| s.required_for_installed)
        .map(|s| s.id.0)
        .collect();
    assert_eq!(installed, [10, 20, 21]);
    assert!(desc.steps.iter().all(|s| s.required_for_ready));
    let network = desc.steps.iter().find(|s| s.id == NETWORK).unwrap();
    assert!(network.settles_with_peer && network.uses_status);
    assert_eq!(
        desc.speakers_device.as_deref(),
        Some("crosspane.{peer}.speaker")
    );
    assert!(!desc.hiding_choice);
    // The shared controller accepts it and keeps the demo-free production mode.
    let rig = Rig::new();
    let clock: live::Clock = Arc::new(|| 1);
    let controller = LiveController::new(Box::new(rig.platform_for_controller()), clock);
    assert!(controller.is_ok());
}

impl Rig {
    /// A second platform with the same fakes, for building a controller.
    fn platform_for_controller(&self) -> LinuxPlatform {
        let scratch = Scratch::new();
        let w = self.w.clone();
        let support = Arc::new(FakeSupport {
            w: w.clone(),
            scratch: scratch.io.clone(),
        });
        let shared = w.clone();
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: Box::new(FakePayloads(shared.clone())),
            services: Box::new(FakeServices(shared.clone())),
            firewalls: Box::new(FakeFirewalls(shared.clone())),
            uninstaller: Box::new(FakeUninstaller(shared)),
        });
        let platform = LinuxPlatform::compose(Parts {
            io: scratch.io.clone(),
            env: scratch.env(),
            clock: Arc::new(|| 1),
            payload: None,
            support,
            domains,
            agent: AgentSource::Injected(Box::new(FakeAgent(Arc::new(Mutex::new(
                AgentState::default(),
            ))))),
            fixtures: FixtureSource::Injected(Box::new(NoFixtures)),
            package: Some(package(1)),
        })
        .unwrap();
        std::mem::forget(scratch);
        platform
    }
}

// ---- the whole flow through the real controller on the real Linux worker ----------------------

mod flow {
    use super::*;

    /// Fake time per harness round while the worker thread runs for real. Kept small, with a
    /// real pause each round, so a loaded machine's slow worker never lets the fake clock run
    /// a waiting call past its own deadline; the wall-clock bounds are failure bounds only.
    const PACE_MS: u64 = 20;
    use std::cell::RefCell;
    use std::rc::Rc;

    use crosspane_installer::fixture::{
        FixtureCommand, FixtureEvent, FixtureMessage, FixtureSnapshot, OwnToneState,
        OwnWindowFacts, PhaseId, ToneId,
    };
    use crosspane_installer::gui::InstallerController;
    use crosspane_installer::tutorial_flow::{HumanConfirmation, TutorialRole};
    use crosspane_installer::view::*;
    use crosspane_types::id::WindowId;

    const STATUS: &str = r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[{"id":1,"name":"local panel","pixels":[2560,1440],"scale":1.0,"mm":[600.0,340.0],"origin":[0.0,0.0]}],
"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.1","features":["video"]},"instance":{"id":99,"pid":4242,"uid":1000,"exe":"fixture","runtime_dir":"fixture","started_unix_ms":1},
"config_revision":"1111111111111111","node":"1111111111111111111111111111111111111111111111111111111111111111",
"recovery_pending":0,"startup_recovery":"restored","gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},
"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"keystore":"os_store","permissions":[],
"backends":[{"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},{"name":"pointer","state":"ready","reason":null},
{"name":"overlay","state":"ready","reason":null},{"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
{"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},{"name":"frames","state":"ready","reason":null},
{"name":"tray","state":"ready","reason":null},{"name":"links","state":"ready","reason":null},{"name":"gpu","state":"ready","reason":null},
{"name":"home","state":"ready","reason":null},{"name":"audio","state":"ready","reason":null},{"name":"discovery","state":"ready","reason":null}],
"discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},
"audio":{"enabled":true,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,
"peers":[]}}}"#;

    fn local() -> NodeId {
        NodeId([0x11; 32])
    }
    fn peer() -> NodeId {
        NodeId([0x22; 32])
    }

    #[derive(Default)]
    pub struct FixtureState {
        launches: Vec<AttemptId>,
        readiness: Option<FixtureReadiness>,
        calls: Vec<FixtureCall>,
        receipts: Vec<FixtureReceipt>,
        retired: u32,
    }

    struct SharedFixtures(Rc<RefCell<FixtureState>>);
    impl PracticeFixtures for SharedFixtures {
        fn launch(&mut self, attempt: AttemptId) -> Result<(), FixtureError> {
            let mut s = self.0.borrow_mut();
            s.launches.push(attempt);
            s.readiness = Some(FixtureReadiness::Ready);
            Ok(())
        }
        fn readiness(&mut self) -> FixtureReadiness {
            self.0.borrow().readiness.unwrap_or(FixtureReadiness::Idle)
        }
        fn submit(&mut self, call: FixtureCall) -> Result<(), FixtureError> {
            self.0.borrow_mut().calls.push(call);
            Ok(())
        }
        fn poll(&mut self) -> Vec<FixtureReceipt> {
            std::mem::take(&mut self.0.borrow_mut().receipts)
        }
        fn complete_closed(&mut self, _: AttemptId, _: FixtureId) -> Result<(), FixtureError> {
            Ok(())
        }
        fn retire(&mut self) {
            let mut s = self.0.borrow_mut();
            s.retired += 1;
            s.readiness = None;
        }
    }

    pub struct Flow {
        pub c: LiveController,
        w: Shared,
        agent: Arc<Mutex<AgentState>>,
        fixtures: Rc<RefCell<FixtureState>>,
        clock: Arc<AtomicU64>,
        pub status: Value,
        calls: Vec<AgentCall>,
        sequence: u64,
        answered: u64,
        _scratch: Scratch,
    }

    impl Flow {
        pub fn new() -> Self {
            let scratch = Scratch::new();
            let w = World::new();
            let clock = Arc::new(AtomicU64::new(10_000));
            let time = clock.clone();
            let agent = Arc::new(Mutex::new(AgentState::default()));
            let fixtures = Rc::new(RefCell::new(FixtureState::default()));
            let support = Arc::new(FakeSupport {
                w: w.clone(),
                scratch: scratch.io.clone(),
            });
            let shared = w.clone();
            let domains: DomainFactory = Box::new(move || Domains {
                payloads: Box::new(FakePayloads(shared.clone())),
                services: Box::new(FakeServices(shared.clone())),
                firewalls: Box::new(FakeFirewalls(shared.clone())),
                uninstaller: Box::new(FakeUninstaller(shared)),
            });
            let clock_fn: live::Clock = {
                let time = time.clone();
                Arc::new(move || time.load(Ordering::SeqCst))
            };
            let platform = LinuxPlatform::compose(Parts {
                io: scratch.io.clone(),
                env: scratch.env(),
                clock: clock_fn.clone(),
                payload: None,
                support,
                domains,
                agent: AgentSource::Injected(Box::new(FakeAgent(agent.clone()))),
                fixtures: FixtureSource::Injected(Box::new(SharedFixtures(fixtures.clone()))),
                package: Some(package(1)),
            })
            .unwrap();
            let c = LiveController::new(Box::new(platform), clock_fn).unwrap();
            let mut status: Value = serde_json::from_str(STATUS).unwrap();
            status["result"]["installer"]["instance"]["id"] = json!(9);
            Self {
                c,
                w,
                agent,
                fixtures,
                clock,
                status,
                calls: Vec::new(),
                sequence: 0,
                answered: 0,
                _scratch: scratch,
            }
        }

        pub fn view(&self) -> &WizardView {
            self.c.view()
        }

        fn advance(&self, ms: u64) {
            self.clock.fetch_add(ms, Ordering::SeqCst);
        }

        fn now(&self) -> u64 {
            self.clock.load(Ordering::SeqCst)
        }

        fn tick(&mut self) {
            self.advance(1);
            let _ = self.c.tick();
            self.calls
                .extend(self.agent.lock().unwrap().queue.take_calls());
        }

        fn health(&self) -> Box<HealthSnapshot> {
            match parse_status(
                &serde_json::to_vec(&self.status).unwrap(),
                AgentPlatform::Linux,
            )
            .unwrap()
            {
                StatusAdmission::Supported(h) => h,
                _ => panic!("fixture status not admitted"),
            }
        }

        /// Answer every outstanding Status call with the current status.
        fn answer_status(&mut self) {
            self.calls
                .extend(self.agent.lock().unwrap().queue.take_calls());
            let (status, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.calls)
                .into_iter()
                .partition(|c| c.request == InstallerRequest::Status);
            self.calls = rest;
            for call in status {
                self.answered += 1;
                self.advance(1);
                let reply = AgentReply {
                    id: call.id,
                    observed_at_ms: self.now(),
                    source: ObservationSource::Live,
                    result: Ok(DecodedReply::Status(StatusAdmission::Supported(
                        self.health(),
                    ))),
                };
                self.agent.lock().unwrap().queue.push_reply(reply).unwrap();
            }
        }

        /// Run until one more status call has been answered and its reply processed.
        pub fn status_once(&mut self) {
            let before = self.answered;
            self.until("a status call to be answered", |f| f.answered > before);
            self.tick();
        }

        /// Let the worker thread and the controller run until `cond`, answering status as the
        /// agent would. Time moves with every round, so poll intervals elapse.
        pub fn until(&mut self, what: &str, cond: impl Fn(&Self) -> bool) {
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                if cond(self) {
                    return;
                }
                assert!(
                    Instant::now() < end,
                    "{what}: not reached; screen {:?}, rows {:?}",
                    self.view().screen,
                    self.c
                        .rows()
                        .iter()
                        .map(|r| (r.id, r.state))
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
            self.calls
                .extend(self.agent.lock().unwrap().queue.take_calls());
            let (taken, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.calls)
                .into_iter()
                .partition(|c| matches(&c.request));
            self.calls = rest;
            taken
        }

        fn ack(&mut self, call: &AgentCall) {
            let ack = decode_reply(
                &call.request,
                br#"{"ok":true,"result":"arbitrary producer prose"}"#,
                AgentPlatform::Linux,
            )
            .unwrap();
            self.advance(1);
            let reply = AgentReply {
                id: call.id,
                observed_at_ms: self.now(),
                source: ObservationSource::Live,
                result: Ok(ack),
            };
            self.agent.lock().unwrap().queue.push_reply(reply).unwrap();
        }

        pub fn counter(&mut self, name: &str, value: u64) {
            self.status["result"]["installer"]["peers"][0]["counters"][name] = json!(value);
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

        // ---- practice ----

        fn confirm(&mut self, c: HumanConfirmation) {
            self.click(live::ids::confirm(c));
        }

        fn begin_practice(&mut self, role: TutorialRole) {
            self.click(live::ids::practice_start(role));
            self.status_once();
            self.status_once();
        }

        /// The next matching command the sequencer sends the fixture, polling status as the
        /// agent would while it is produced.
        fn fixture_command(
            &mut self,
            matches: impl Fn(&FixtureCommand) -> bool,
        ) -> Option<FixtureCall> {
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                {
                    let mut state = self.fixtures.borrow_mut();
                    if let Some(i) = state.calls.iter().position(|c| matches(&c.command)) {
                        return Some(state.calls.remove(i));
                    }
                }
                if Instant::now() >= end {
                    return None;
                }
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn fixture_reply(
            &mut self,
            call: &FixtureCall,
            result: Result<FixtureEvent, FixtureError>,
        ) {
            self.sequence += 1;
            let receipt = FixtureReceipt {
                received_at_ms: self.now() + 1,
                message: FixtureMessage {
                    call_id: Some(call.id),
                    attempt: call.attempt,
                    sequence: self.sequence,
                    result,
                },
            };
            self.fixtures.borrow_mut().receipts.push(receipt);
            self.advance(2);
            self.tick();
        }

        fn fixture_event(&mut self, attempt: AttemptId, event: FixtureEvent) {
            self.sequence += 1;
            let receipt = FixtureReceipt {
                received_at_ms: self.now() + 1,
                message: FixtureMessage {
                    call_id: None,
                    attempt,
                    sequence: self.sequence,
                    result: Ok(event),
                },
            };
            self.fixtures.borrow_mut().receipts.push(receipt);
            self.advance(2);
            self.tick();
        }

        fn fixture_open(&mut self) {
            let call = self
                .fixture_command(|c| matches!(c, FixtureCommand::Open { .. }))
                .unwrap_or_else(|| {
                    panic!(
                        "the sequencer never asked the fixture to open: {} / {:?} / launches {:?} / pending {:?}",
                        self.view().message,
                        self.c.rows().iter().filter(|r| (70..=78).contains(&r.id)).map(|r| (r.id, r.state, r.detail.clone())).collect::<Vec<_>>(),
                        self.fixtures.borrow().launches,
                        self.calls.iter().map(|c| c.id).collect::<Vec<_>>(),
                    )
                });
            // The window carries this computer's name; the sequencer matches it exactly.
            let FixtureCommand::Open { machine_label } = &call.command else {
                unreachable!()
            };
            assert_eq!(Some(machine_label), self.view().machine.as_ref());
            let label = machine_label.clone();
            self.fixture_reply(
                &call,
                Ok(FixtureEvent::Opened {
                    fixture: FixtureId(10),
                    pid: 123,
                    window: WindowId(100),
                    label,
                }),
            );
        }

        fn fixture_close(&mut self) {
            let call = self
                .fixture_command(|c| matches!(c, FixtureCommand::Close { .. }))
                .expect("the owned fixture is closed");
            self.fixture_reply(
                &call,
                Ok(FixtureEvent::Closed {
                    fixture: FixtureId(10),
                }),
            );
        }

        fn snapshot(phase: Option<u64>, clicks: u64, facts: OwnWindowFacts) -> FixtureEvent {
            FixtureEvent::Snapshot {
                snapshot: FixtureSnapshot {
                    fixture: FixtureId(10),
                    window: WindowId(100),
                    phase: phase.map(PhaseId),
                    pattern_ticks: 10,
                    target_clicks: clicks,
                    window_facts: facts,
                    tone: OwnToneState::Stopped,
                },
            }
        }

        fn projection(&mut self, source: NodeId, on: bool) {
            self.status["result"]["projections"] = if on {
                json!([{"source": source.short(), "projection": 55, "text": "sensitive title",
                    "received": {"frames": 999999, "bytes": 999999}}])
            } else {
                json!([])
            };
        }

        fn run_e1_controller(&mut self) {
            self.begin_practice(TutorialRole::E1Controller);
            for name in [
                "e1_controller_started",
                "e1_controller_ended",
                "e1_chord_releases",
            ] {
                self.counter(name, 1);
            }
            self.status_once();
            self.confirm(HumanConfirmation::RemotePracticeAndHud);
        }

        fn run_e1_target(&mut self) {
            self.begin_practice(TutorialRole::E1Target);
            self.fixture_open();
            let arm = self
                .fixture_command(|c| matches!(c, FixtureCommand::ArmTarget { .. }))
                .unwrap_or_else(|| {
                    panic!(
                        "the target is armed: {} / {:?} / calls {:?}",
                        self.view().message,
                        self.row(71),
                        self.fixtures
                            .borrow()
                            .calls
                            .iter()
                            .map(|c| format!("{:?}", c.command))
                            .collect::<Vec<_>>()
                    )
                });
            let FixtureCommand::ArmTarget { phase, .. } = arm.command.clone() else {
                unreachable!()
            };
            self.fixture_reply(
                &arm,
                Ok(FixtureEvent::TargetArmed {
                    fixture: FixtureId(10),
                    phase,
                }),
            );
            let attempt = *self.fixtures.borrow().launches.last().unwrap();
            self.fixture_event(
                attempt,
                Self::snapshot(Some(phase.0), 1, OwnWindowFacts::Unknown),
            );
            for name in [
                "e1_target_started",
                "e1_target_ended",
                "e1_injections_ok",
                "e1_hud_shows",
            ] {
                self.counter(name, 1);
            }
            self.status_once();
            self.confirm(HumanConfirmation::ControllerCrossingAndRelease);
            self.fixture_close();
        }

        fn windows_reply(&mut self, request: InstallerRequest, body: &str) {
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                let found = self.calls_of(|r| *r == request);
                if let Some(call) = found.into_iter().next() {
                    let decoded =
                        decode_reply(&request, body.as_bytes(), AgentPlatform::Linux).unwrap();
                    self.advance(1);
                    let reply = AgentReply {
                        id: call.id,
                        observed_at_ms: self.now(),
                        source: ObservationSource::Live,
                        result: Ok(decoded),
                    };
                    self.agent.lock().unwrap().queue.push_reply(reply).unwrap();
                    self.tick();
                    return;
                }
                assert!(Instant::now() < end, "{request:?} was never requested");
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn take_and_ack(&mut self, matches: impl Fn(&InstallerRequest) -> bool) -> AgentCall {
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(call) = self.calls_of(&matches).into_iter().next() {
                    self.ack(&call);
                    self.tick();
                    return call;
                }
                assert!(Instant::now() < end, "the expected call was never made");
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn e2(&mut self, role: TutorialRole) {
            let source = matches!(
                role,
                TutorialRole::E2SourcePush | TutorialRole::E2SourcePull
            );
            self.begin_practice(role);
            match role {
                TutorialRole::E2SourcePush => {
                    self.fixture_open();
                    self.windows_reply(
                        InstallerRequest::Windows,
                        r#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"sensitive","display":null,"size":[20.5,30.5]}]}"#,
                    );
                    let project =
                        self.take_and_ack(|r| matches!(r, InstallerRequest::Project { .. }));
                    assert_eq!(
                        project.request,
                        InstallerRequest::Project {
                            window: WindowId(100),
                            peer: peer()
                        }
                    );
                }
                TutorialRole::E2SourcePull => {
                    self.fixture_open();
                    self.confirm(HumanConfirmation::SourceMachineAndAttempt);
                }
                TutorialRole::E2DestinationPull => {
                    self.windows_reply(
                        InstallerRequest::WindowsFrom { peer: peer() },
                        r#"{"ok":true,"result":[{"id":100,"app":"fixture","title":"advisory","size":[20,30]}]}"#,
                    );
                    self.click(live::ids::remote_window(0));
                    self.confirm(HumanConfirmation::SourceMachineAndAttempt);
                    self.take_and_ack(|r| matches!(r, InstallerRequest::Pull { .. }));
                }
                TutorialRole::E2DestinationPush => {
                    self.confirm(HumanConfirmation::SourceMachineAndAttempt)
                }
                _ => unreachable!(),
            }
            self.projection(if source { local() } else { peer() }, true);
            let metric = if source {
                "e2_source_started"
            } else {
                "e2_dest_started"
            };
            let n = self.status["result"]["installer"]["peers"][0]["counters"][metric]
                .as_u64()
                .unwrap_or(0);
            self.counter(metric, n + 1);
            if source {
                self.set(&["installer", "recovery_pending"], json!(1));
                let mut peers = self.status["result"]["installer"]["peers"].clone();
                peers[0]["last_source_parking"] = json!("twin");
                self.set(&["installer", "peers"], peers);
            }
            self.status_once();
            self.confirm(HumanConfirmation::DestinationPatternInteractionAndClose);
            self.take_and_ack(|r| matches!(r, InstallerRequest::Return { .. }));
            self.projection(local(), false);
            let metric = if source {
                "e2_source_returned"
            } else {
                "e2_dest_returned"
            };
            let n = self.status["result"]["installer"]["peers"][0]["counters"][metric]
                .as_u64()
                .unwrap_or(0);
            self.counter(metric, n + 1);
            self.set(&["installer", "recovery_pending"], json!(0));
            if !source {
                let n = self.status["result"]["installer"]["peers"][0]["counters"]["e2_frames_presented"]
                    .as_u64()
                    .unwrap_or(0);
                self.counter("e2_frames_presented", n + 10);
            }
            self.status_once();
            if source {
                let call = self
                    .fixture_command(|c| matches!(c, FixtureCommand::ObserveWindow { .. }))
                    .expect("home is observed");
                self.fixture_reply(
                    &call,
                    Ok(Self::snapshot(
                        None,
                        0,
                        OwnWindowFacts::Present {
                            visible_on_user_workspace: Some(true),
                            on_initial_display: Some(true),
                        },
                    )),
                );
                if self
                    .fixtures
                    .borrow()
                    .calls
                    .iter()
                    .any(|c| matches!(c.command, FixtureCommand::Close { .. }))
                {
                    self.fixture_close();
                }
            } else {
                self.confirm(HumanConfirmation::SourceRestored);
            }
        }

        fn run_audio_sender(&mut self) {
            self.begin_practice(TutorialRole::AudioSender);
            self.fixture_open();
            self.click(live::ids::PLAY_TONE);
            let play = self
                .fixture_command(|c| matches!(c, FixtureCommand::PlayTone { .. }))
                .expect("the owned fixture plays its own tone");
            let FixtureCommand::PlayTone { output, .. } = &play.command else {
                unreachable!()
            };
            assert_eq!(output.peer, peer());
            assert_eq!(output.device_key, format!("crosspane.{}.speaker", peer()));
            self.fixture_reply(
                &play,
                Ok(FixtureEvent::ToneStarted {
                    fixture: FixtureId(10),
                    tone: ToneId(33),
                }),
            );
            self.set(&["installer", "audio", "active_peers"], json!([peer()]));
            self.status_once();
            self.set(&["installer", "audio", "frames_sent"], json!(10));
            self.status_once();
            let stop = self
                .fixture_command(|c| matches!(c, FixtureCommand::StopTone { .. }))
                .expect("the tone is stopped");
            self.fixture_reply(
                &stop,
                Ok(FixtureEvent::ToneStopped {
                    fixture: FixtureId(10),
                    tone: ToneId(33),
                }),
            );
            self.confirm(HumanConfirmation::FarSpeakerHeard);
            self.confirm(HumanConfirmation::ExclusiveAudioInterval);
            self.fixture_close();
        }

        fn run_audio_receiver(&mut self) {
            self.begin_practice(TutorialRole::AudioReceiver);
            self.confirm(HumanConfirmation::SelectedSourceToneStarted);
            self.set(&["installer", "audio", "active_peers"], json!([peer()]));
            self.status_once();
            self.set(&["installer", "audio", "frames_played"], json!(10));
            self.status_once();
            self.confirm(HumanConfirmation::LocalSpeakerHeard);
            self.confirm(HumanConfirmation::ExclusiveAudioInterval);
        }

        fn run_menu(&mut self) {
            self.begin_practice(TutorialRole::Menu);
            self.set(&["installer", "settings_opened"], json!(1));
            self.status_once();
            self.confirm(HumanConfirmation::TrayAndSettingsVisible);
        }

        pub fn practise_everything(&mut self) {
            self.run_e1_controller();
            self.run_e1_target();
            for role in [
                TutorialRole::E2SourcePush,
                TutorialRole::E2DestinationPush,
                TutorialRole::E2SourcePull,
                TutorialRole::E2DestinationPull,
            ] {
                self.e2(role);
            }
            self.run_audio_sender();
            self.run_audio_receiver();
            self.run_menu();
        }

        /// Install, restart and look at the agent; stop at the network step's waiting state.
        pub fn install(&mut self) {
            self.until("the welcome screen", |f| {
                f.view().screen == ScreenId::Welcome
            });
            self.go_next();
            self.until("support verified", |f| f.verified(10));
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::InstallPlan);
            self.until("a payload preview to consent to", |f| {
                f.has_button(live::ids::consent(StepId(20)))
            });
            let preview = self.view().message.clone();
            assert!(preview.contains("0.0.1"), "{preview}");
            self.click(live::ids::consent(StepId(20)));
            self.until("files installed", |f| f.verified(20));
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Installing);
            self.until("a startup preview", |f| {
                f.has_button(live::ids::consent(StepId(21)))
            });
            self.click(live::ids::consent(StepId(21)));
            self.until("startup done", |f| f.verified(21));
            // Installed, not ready: only support, files and startup are done.
            assert_eq!(self.view().summary, SummaryView::InstalledWaiting);
            self.until("a restart preview", |f| {
                f.has_button(live::ids::consent(StepId(22)))
            });
            self.click(live::ids::consent(StepId(22)));
            self.until("the restart verified against a fresh status", |f| {
                f.verified(22)
            });
            self.until("the agent verified", |f| f.verified(23));
        }

        pub fn network_then_pair(&mut self) {
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Network);
            self.until("the network step to wait for traffic", |f| {
                f.row(30).state == RowState::Waiting
            });
            // Next stays available: the network is proved by the other computer connecting.
            assert!(self.has_button(live::ids::NEXT));
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Connect);
            self.pair_externally(&[]);
            self.until("pairing verified from status", |f| f.verified(60));
            self.until("the network proved by the connection", |f| f.verified(30));
        }

        pub fn arrange(&mut self) {
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Grants);
            self.pair_externally(&["browse", "input", "present", "share", "speaker"]);
            self.until("grants verified", |f| f.verified(61));
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Layout);
            self.place();
            self.until("a layout to accept", |f| {
                f.has_button(live::ids::LAYOUT_ACCEPT)
            });
            self.click(live::ids::LAYOUT_ACCEPT);
            self.until("layout committed", |f| f.verified(62));
            self.go_next();
            assert_eq!(self.view().screen, ScreenId::Practice);
        }

        pub fn summary(&self) -> SummaryView {
            self.view().summary
        }

        pub fn world(&self) -> &Shared {
            &self.w
        }

        pub fn close(&mut self) -> bool {
            let revision = self.view().revision;
            self.c.accept(WizardAction {
                revision,
                intent: WizardIntent::Close,
            })
        }
    }

    impl Drop for Flow {
        fn drop(&mut self) {
            InstallerController::close(&mut self.c);
        }
    }
}

#[test]
fn the_full_linux_install_pairing_practice_and_final_health_reach_ready_only_at_the_end() {
    use crosspane_installer::view::SummaryView;
    let mut f = flow::Flow::new();
    f.install();
    f.network_then_pair();
    f.arrange();
    assert_eq!(
        f.summary(),
        SummaryView::InstalledWaiting,
        "nothing is practised yet"
    );
    f.practise_everything();
    f.go_next();
    f.until("final fresh health", |f| {
        f.summary() == SummaryView::WorkspaceReady
    });
    // Every mutation had its own consent: nothing destructive ran without a click, and the
    // service commands were the previewed ones in order.
    let calls = calls(f.world());
    assert!(calls.contains(&"payload.apply".to_owned()));
    assert!(calls.contains(&"service.Enable".to_owned()));
    assert!(calls.contains(&"service.Restart".to_owned()));
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("firewall.apply") || c.starts_with("uninstall.")),
        "no firewall change or removal was ever requested: {calls:?}"
    );
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
fn the_network_step_is_never_verified_by_a_rule_or_command_alone() {
    let mut f = flow::Flow::new();
    f.install();
    f.go_next();
    // Active ufw without the rule: the exact rule is offered, applied only after consent, and
    // even after the command succeeds the step stays waiting until traffic is observed.
    f.world().lock().unwrap().fw_read = reading(Some(true), RulePresence::Absent);
    f.until("a network preview", |f| {
        f.has_button(live::ids::consent(StepId(30)))
    });
    assert_ne!(
        f.row(30).state,
        crosspane_installer::view::RowState::Verified
    );
    f.click(live::ids::consent(StepId(30)));
    f.until("the command to finish", |f| {
        f.row(30).state == crosspane_installer::view::RowState::Waiting
    });
    assert!(calls(f.world()).contains(&"firewall.apply".to_owned()));
    assert_ne!(
        f.row(30).state,
        crosspane_installer::view::RowState::Verified
    );
}

#[allow(dead_code)]
fn _unused(_: Option<Value>, _: Option<&Path>, _: Option<Consent>) {}
