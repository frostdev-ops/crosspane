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
use crosspane_installer::live::{
    self, Availability, Consent, LiveController, MaintenanceId, MaintenanceOutcome,
    MaintenanceReport, MaintenanceRequest, NativeJob, NativeOutcome, NativeReport, Platform,
    RepairOutcome, StatusEvidence, StepReport,
};
use crosspane_installer::live::{CheckState, SupportCheck, SupportChecksSlot};
use crosspane_installer::platform::linux::integration::{
    AgentSource, DomainFactory, Domains, FirewallReading, Firewalls, LinuxPlatform, Parts,
    PayloadPreview, Payloads, RepairFinish, RepairOffer, RepairStep, Repairer, RuleApply,
    RulePresence, Services, Support, SupportOutcome, SupportedAgentPort, UninstallOffer,
    UninstallProgress, Uninstaller,
};
use crosspane_installer::platform::linux::{
    firewall::FirewallError,
    native_io::*,
    payload::*,
    service::{AgentEvidence, ServiceAction, ServiceError, ServiceFacts, ServiceResult},
};
use crosspane_installer_core::{
    ApplyOutcome, JobIntent, JobStage, MutationOutcome, ObservationSource, OperationId,
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
    (0..FILES.len())
        .map(|i| {
            if i < 4 {
                elf(version)
            } else if i == 4 {
                format!("[Service]\nExecStart={{{{agent_executable}}}} run\nEnvironment={{{{xdg_config_environment}}}}\nEnvironment={{{{xdg_state_environment}}}}\nEnvironment={{{{xdg_runtime_environment}}}}\nEnvironment={{{{crosspane_runtime_environment}}}}\n# fixture-version-{version}\n").into_bytes()
            } else if i < 7 {
                format!("[Desktop Entry]\nType=Application\nName=Crosspane\nExec={{{{{}_executable}}}}\n# fixture-version-{version}\n", if i == 5 { "settings" } else { "installer" }).into_bytes()
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
    /// What repair offers for the install, and the scripted answers of each repair stage.
    repair_offer: RepairOffer,
    repair_plan: Result<String, String>,
    repair_confirm: VecDeque<Result<RepairStep, String>>,
    repair_verify: VecDeque<RepairStep>,
    repair_resume: Result<RepairFinish, String>,
    discard_refusal: Option<String>,
    /// The (plan, operation) pairs each confirmation named, the status ids each verification
    /// was given, and whether each verification carried a Supported status.
    repair_confirmed: Vec<(u64, u64)>,
    repair_verified: Vec<Option<u64>>,
    /// Verification keeps waiting while set; a confirmation blocks (a slow stage) while set.
    repair_hold: bool,
    repair_hold_confirm: bool,
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
                replacing: false,
            }),
            resume: Ok(PayloadPreview {
                version: "0.0.1".into(),
                resuming: true,
                replacing: false,
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
            repair_offer: RepairOffer {
                discardable: false,
                repair: Availability::Available,
                resumable: None,
            },
            repair_plan: Ok("Repair puts back what this installer ships: bin/crosspane-agent (different).".into()),
            repair_confirm: VecDeque::new(),
            repair_verify: VecDeque::new(),
            repair_resume: Ok(finish(RepairOutcome::Verified, "Resumed and verified.", false)),
            discard_refusal: None,
            repair_confirmed: Vec::new(),
            repair_verified: Vec::new(),
            repair_hold: false,
            repair_hold_confirm: false,
            endless: false,
            hold_support: false,
        }))
    }
}

fn finish(outcome: RepairOutcome, line: &str, resumable: bool) -> RepairFinish {
    RepairFinish {
        outcome,
        lines: vec![line.into()],
        resumable,
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
    checks: SupportChecksSlot,
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
        // Like the native detection, each finished pass writes its checklist.
        self.checks.publish(vec![SupportCheck::new(
            "Fake check",
            match &sup {
                Sup::Supported => CheckState::Passed(None),
                Sup::NotSupported(t) => CheckState::Failed((*t).into()),
                Sup::Pending(t) => CheckState::Unconfirmed((*t).into()),
            },
        )]);
        match sup {
            Sup::Supported => SupportOutcome::Supported(
                self.scratch
                    .scratch_support(SupportObservations {
                        uid: self.scratch.target().paths().uid,
                        desktop: crosspane_installer::platform::linux::detect::Desktop::Hyprland,
                        architecture: std::env::consts::ARCH.into(),
                        arch_based: true,
                        compositor_version: [0, 56, 0],
                        protocols_ready: true,
                        runtime_libraries_ready: true,
                        compositor_managed: true,
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
    fn checks(&self) -> Option<SupportChecksSlot> {
        Some(self.checks.clone())
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
            submitted: true,
            submission_pending: false,
            diagnostic: None,
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

struct FakeRepairer(Shared);

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

impl Repairer for FakeRepairer {
    fn discard(
        &mut self,
        _: Option<&Package>,
        status: Option<&AgentReply>,
        _: u64,
    ) -> Result<RepairFinish, String> {
        let mut w = self.0.lock().unwrap();
        w.calls
            .push(format!("repair.discard(status={:?})", status_id(status)));
        if let Some(reason) = w.discard_refusal.clone() {
            return Err(reason);
        }
        w.repair_offer = RepairOffer {
            discardable: false,
            repair: Availability::Available,
            resumable: None,
        };
        Ok(finish(
            RepairOutcome::Retired,
            "The earlier repair record was discarded.",
            false,
        ))
    }
    fn inspect(&mut self, _: Option<&Package>, _: u64) -> RepairOffer {
        note(&self.0, "repair.inspect");
        self.0.lock().unwrap().repair_offer.clone()
    }
    fn plan(
        &mut self,
        _: Option<&Package>,
        status: Option<&AgentReply>,
        op: OperationId,
        _: u64,
    ) -> Result<String, String> {
        note(
            &self.0,
            format!("repair.plan(op={},status={:?})", op.0, status_id(status)),
        );
        self.0.lock().unwrap().repair_plan.clone()
    }
    fn confirm(
        &mut self,
        _: Option<&Package>,
        status: Option<&AgentReply>,
        plan: OperationId,
        op: OperationId,
        _: u64,
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
        w.repair_confirmed.push((plan.0, op.0));
        w.repair_confirm.pop_front().unwrap_or_else(|| {
            Ok(RepairStep::Waiting {
                detail: "Crosspane was started again. Waiting for the new instance…".into(),
                if_timed_out: finish(
                    RepairOutcome::OutcomeUnknown,
                    "The new Crosspane didn't report healthy in time.",
                    true,
                ),
                closeable: true,
            })
        })
    }
    fn verify(&mut self, _: Option<&Package>, status: Option<&AgentReply>, _: u64) -> RepairStep {
        let mut w = self.0.lock().unwrap();
        w.calls
            .push(format!("repair.verify(status={:?})", status_id(status)));
        w.repair_verified.push(status_id(status));
        if w.repair_hold {
            return RepairStep::Waiting {
                detail: "Waiting for the new instance to report healthy…".into(),
                if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health in time.", true),
                closeable: true,
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
        _: Option<&Package>,
        status: Option<&AgentReply>,
        _: u64,
    ) -> Result<RepairFinish, String> {
        note(
            &self.0,
            format!("repair.resume(status={:?})", status_id(status)),
        );
        self.0.lock().unwrap().repair_resume.clone()
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
            checks: SupportChecksSlot::new(SUPPORT),
        });
        let shared = w.clone();
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: Box::new(FakePayloads(shared.clone())),
            services: Box::new(FakeServices(shared.clone())),
            firewalls: Box::new(FakeFirewalls(shared.clone())),
            uninstaller: Box::new(FakeUninstaller(shared.clone())),
            repairer: Box::new(FakeRepairer(shared)),
        });
        let platform = LinuxPlatform::compose(Parts {
            io: scratch.io.clone(),
            env: scratch.env(),
            clock: Arc::new(move || time.load(Ordering::SeqCst)),
            payload,
            support,
            domains,
            agent: AgentSource::Injected(Box::new(FakeAgent(agent.clone()))),
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
fn the_platform_hands_each_finished_support_pass_to_the_window() {
    let mut rig = Rig::new();
    // Before any pass: the empty placeholder, which already names the support step.
    let first = rig.platform.support_checks().unwrap();
    assert_eq!((first.step, first.pass), (SUPPORT, 0));
    assert!(first.checks.is_empty());
    rig.set(|w| w.sup = Sup::Pending("couldn't read the session environment"));
    let (_, report) = rig.run(SUPPORT, JobStage::Detect, None, None);
    assert!(
        matches!(report.outcome, NativeOutcome::Waiting(_)),
        "{report:?}"
    );
    let pending = rig.platform.support_checks().unwrap();
    assert!(pending.pass >= 1);
    assert_eq!(
        pending.checks,
        vec![SupportCheck::new(
            "Fake check",
            CheckState::Unconfirmed("couldn't read the session environment".into())
        )]
    );
    rig.set(|w| w.sup = Sup::Supported);
    let (_, report) = rig.run(SUPPORT, JobStage::Detect, None, None);
    assert!(
        matches!(report.outcome, NativeOutcome::Detected { .. }),
        "{report:?}"
    );
    let passed = rig.platform.support_checks().unwrap();
    assert!(passed.pass > pending.pass);
    assert_eq!(passed.checks[0].state, CheckState::Passed(None));
}

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
fn files_setup_cant_build_on_are_an_install_to_do_not_a_stop() {
    // WP-4.32: setup owns its install paths. Unrecorded, modified or unreadable files there are
    // installed over (saved aside first); detection says the install is needed, in plain words.
    for error in [
        PayloadError::Foreign,
        PayloadError::Pending,
        PayloadError::OutcomeUnknown,
    ] {
        let mut rig = Rig::new();
        rig.set(|w| w.detect = Err(error));
        let (_, detect) = rig.run(PAYLOAD, JobStage::Detect, None, None);
        assert_eq!(
            detect.outcome,
            NativeOutcome::Detected { needs_action: true }
        );
        for word in ["put there", "left", "foreign", "Foreign", "didn't"] {
            assert!(!detect.detail.contains(word), "{}", detect.detail);
        }
        assert_eq!(rig.count("payload.apply"), 0);
    }
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
            repair: Availability::Available,
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
        // Anything the worker sends right behind Inspected arrives in the same pump or the next.
        std::thread::sleep(Duration::from_millis(30));
        self.pump(|_| true);
        self.maintenance[before..].to_vec()
    }

    /// Plan a repair and return the plan number its preview carried.
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
fn repair_is_offered_only_when_the_inventory_says_the_install_is_compatible() {
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
    let reason = "Some files where Crosspane installs weren't put there by Crosspane. Remove \
                  Crosspane and install it again to start clean.";
    rig.set(|w| {
        w.repair_offer = RepairOffer {
            discardable: false,
            repair: Availability::Unavailable(reason.into()),
            resumable: None,
        };
        w.repair_plan = Err(reason.into());
    });
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
    assert!(text.contains("Remove Crosspane and install it again"));
    assert_eq!(rig.count("repair.confirm"), 0);
}

#[test]
fn a_repair_is_confirmed_only_for_the_numbered_preview_and_only_once() {
    let mut rig = Rig::new();
    rig.inspect(1);
    // Confirming before any preview is refused, and nothing ran.
    let report = rig.confirm_repair(1, 1);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "{report:?}"
    );
    assert_eq!(rig.count("repair.confirm"), 0);

    // A stale preview number is refused too, and it retires the preview.
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
    assert_eq!(rig.count("repair.confirm"), 0);

    // A new preview, then its own number: the repair starts, and the status the click carried
    // reached the adapter. The adapter was given a second, fresh operation for its own re-plan.
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
    assert!(
        rig.calls()
            .contains(&format!("repair.confirm(plan={plan},status=Some(32))"))
    );
    // The same preview can never start a second repair.
    let report = rig.confirm_repair(1, plan);
    assert!(
        matches!(report, MaintenanceReport::Refused { .. }),
        "{report:?}"
    );
    assert_eq!(rig.count("repair.confirm"), 1);
}

#[test]
fn a_repair_waits_for_the_new_agent_and_verifies_only_from_a_fresh_status() {
    let mut rig = Rig::new();
    rig.inspect(1);
    let plan = rig.plan_repair(1);
    assert!(matches!(
        rig.confirm_repair(1, plan),
        MaintenanceReport::RepairWaiting { .. }
    ));
    rig.set(|w| {
        w.repair_verify.push_back(RepairStep::Waiting {
            detail: "Still waiting for the new instance…".into(),
            if_timed_out: finish(RepairOutcome::OutcomeUnknown, "no health", true),
            closeable: true,
        });
    });
    let report = rig.verify_repair(1, 41);
    let MaintenanceReport::RepairWaiting { detail, .. } = report else {
        panic!("{report:?}")
    };
    assert!(detail.contains("Still waiting"));
    let report = rig.verify_repair(1, 42);
    let MaintenanceReport::RepairFinished {
        outcome, resumable, ..
    } = report
    else {
        panic!("{report:?}")
    };
    assert_eq!(outcome, RepairOutcome::Verified);
    assert!(!resumable);
    assert_eq!(rig.w.lock().unwrap().repair_verified, [Some(41), Some(42)]);
    // Once the repair ended, another verification answers nothing.
    let before = rig.maintenance.len();
    rig.send(NativeJob::Maintenance(MaintenanceRequest::VerifyRepair {
        id: MaintenanceId(1),
        status: Some(StatusEvidence(healthy(43))),
    }));
    std::thread::sleep(Duration::from_millis(60));
    rig.pump(|_| true);
    assert_eq!(rig.maintenance.len(), before);
    assert_eq!(rig.count("repair.verify"), 2);
}

#[test]
fn the_wait_for_the_new_agent_ends_with_the_adapters_typed_result_never_a_guess() {
    let mut rig = Rig::new();
    rig.inspect(1);
    let plan = rig.plan_repair(1);
    assert!(matches!(
        rig.confirm_repair(1, plan),
        MaintenanceReport::RepairWaiting { .. }
    ));
    let waiting = |outcome| RepairStep::Waiting {
        detail: "Waiting…".into(),
        if_timed_out: finish(outcome, "Gave up waiting.", true),
        closeable: true,
    };
    rig.set(|w| {
        w.repair_verify.extend([
            waiting(RepairOutcome::RecoveryRetained),
            waiting(RepairOutcome::RecoveryRetained),
        ]);
    });
    assert!(matches!(
        rig.verify_repair(1, 41),
        MaintenanceReport::RepairWaiting { .. }
    ));
    rig.advance(100_000);
    let report = rig.verify_repair(1, 42);
    let MaintenanceReport::RepairFinished {
        outcome, resumable, ..
    } = report
    else {
        panic!("{report:?}")
    };
    assert_eq!(outcome, RepairOutcome::RecoveryRetained);
    assert!(
        resumable,
        "recovery material is kept, so a resume can still make progress"
    );
}

#[test]
fn every_typed_repair_outcome_is_reported_and_none_is_invented() {
    for (outcome, resumable) in [
        (RepairOutcome::Verified, false),
        (RepairOutcome::HealthVerifiedCleanupIncomplete, false),
        (RepairOutcome::OutcomeUnknown, true),
        (RepairOutcome::RecoveryRetained, true),
    ] {
        let mut rig = Rig::new();
        rig.inspect(1);
        rig.set(|w| {
            w.repair_confirm.push_back(Ok(RepairStep::Finished(finish(
                outcome,
                "The adapter's own words.",
                resumable,
            ))))
        });
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
fn an_interrupted_repair_is_offered_for_resume_and_resumes_to_verified() {
    let mut rig = Rig::new();
    rig.set(|w| {
        w.repair_offer = RepairOffer {
            discardable: false,
            repair: Availability::Unavailable("An earlier repair didn't finish.".into()),
            resumable: Some(vec!["It stopped while files were being replaced.".into()]),
        }
    });
    let reports = rig.inspect(1);
    assert!(matches!(&reports[0], MaintenanceReport::Inspected { .. }));
    assert_eq!(
        reports[1],
        MaintenanceReport::RepairResumable {
            id: MaintenanceId(1),
            lines: vec!["It stopped while files were being replaced.".into()],
        }
    );
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
    assert!(
        rig.calls()
            .contains(&"repair.resume(status=Some(51))".to_owned())
    );
}

#[test]
fn a_resume_with_nothing_to_resume_is_a_refusal_not_a_repair() {
    let mut rig = Rig::new();
    rig.inspect(1);
    rig.set(|w| {
        w.repair_resume = Err("There is no earlier repair to resume. Nothing was changed.".into())
    });
    let reports = rig.maint(MaintenanceRequest::ResumeRepair {
        id: MaintenanceId(1),
        status: None,
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
}

#[test]
fn removal_cannot_be_planned_while_a_repair_is_active() {
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
    // And a second repair preview can't be started over it.
    let reports = rig.maint(MaintenanceRequest::PlanRepair {
        id: MaintenanceId(1),
        status: None,
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
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
            checks: SupportChecksSlot::new(SUPPORT),
        });
        let shared = w.clone();
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: Box::new(FakePayloads(shared.clone())),
            services: Box::new(FakeServices(shared.clone())),
            firewalls: Box::new(FakeFirewalls(shared.clone())),
            uninstaller: Box::new(FakeUninstaller(shared.clone())),
            repairer: Box::new(FakeRepairer(shared)),
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

    use crosspane_installer::gui::InstallerController;
    use crosspane_installer::view::*;

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

    pub struct Flow {
        pub c: LiveController,
        w: Shared,
        agent: Arc<Mutex<AgentState>>,
        clock: Arc<AtomicU64>,
        pub status: Value,
        /// The agent answers no Status call (it is stopped, or not up yet).
        pub silent: bool,
        calls: Vec<AgentCall>,
        answered: u64,
        _scratch: Scratch,
    }

    impl Flow {
        pub fn new() -> Self {
            Self::on(World::new())
        }

        /// A controller and worker over an existing world: a "reopened" installer window.
        pub fn on(w: Shared) -> Self {
            let scratch = Scratch::new();
            let clock = Arc::new(AtomicU64::new(10_000));
            let time = clock.clone();
            let agent = Arc::new(Mutex::new(AgentState::default()));
            let support = Arc::new(FakeSupport {
                w: w.clone(),
                scratch: scratch.io.clone(),
                checks: SupportChecksSlot::new(SUPPORT),
            });
            let shared = w.clone();
            let domains: DomainFactory = Box::new(move || Domains {
                payloads: Box::new(FakePayloads(shared.clone())),
                services: Box::new(FakeServices(shared.clone())),
                firewalls: Box::new(FakeFirewalls(shared.clone())),
                uninstaller: Box::new(FakeUninstaller(shared.clone())),
                repairer: Box::new(FakeRepairer(shared)),
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
                clock,
                status,
                silent: false,
                calls: Vec::new(),
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
            if self.silent {
                return;
            }
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

        /// Start setup, then let it install, restart and look at the agent by itself; stop at
        /// the network step's waiting state.
        pub fn install(&mut self) {
            self.until("the welcome screen", |f| {
                f.view().screen == ScreenId::Welcome
            });
            for _ in 0..20 {
                self.answer_status();
                self.advance(PACE_MS);
                self.tick();
            }
            assert!(
                !super::calls(&self.w)
                    .iter()
                    .any(|c| c.starts_with("payload.apply")),
                "nothing is installed before the person starts setup"
            );
            self.go_next();
            self.until("support verified", |f| f.verified(10));
            // From here the install page carries on by itself: no further click.
            self.until("files installed", |f| f.verified(20));
            self.until("startup done", |f| f.verified(21));
            // Installed, not ready: only support, files and startup are done.
            assert_eq!(self.view().summary, SummaryView::InstalledWaiting);
            self.until("the restart verified against a fresh status", |f| {
                f.verified(22)
            });
            self.until("the agent verified", |f| f.verified(23));
        }

        /// The screen the flow is on now, letting a finished screen move on by itself first.
        pub fn reach(&mut self, screen: ScreenId) {
            self.until(&format!("the {screen:?} screen"), |f| {
                f.view().screen == screen
            });
        }

        pub fn network_then_pair(&mut self) {
            self.reach(ScreenId::Network);
            self.until("the network step to wait for traffic", |f| {
                f.row(30).state == RowState::Waiting
            });
            // Nothing to answer there: the network is proved by the other computer connecting,
            // so the screen moves on by itself.
            self.reach(ScreenId::Connect);
            self.pair_externally(&[]);
            self.until("pairing verified from status", |f| f.verified(60));
            self.until("the network proved by the connection", |f| f.verified(30));
        }

        pub fn arrange(&mut self) {
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

#[test]
fn the_full_linux_install_pairing_arrange_and_final_health_reach_ready_only_at_the_end() {
    use crosspane_installer::view::SummaryView;
    let mut f = flow::Flow::new();
    f.install();
    f.network_then_pair();
    f.arrange();
    f.reach(crosspane_installer::view::ScreenId::Summary);
    f.until("final fresh health", |f| {
        f.summary() == SummaryView::WorkspaceReady
    });
    // The install inside the person's account went ahead on the start click, each step against
    // its own preview, and the service commands were the previewed ones in order. Nothing that
    // needs more than that (the firewall, removal) ran without its own click.
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
fn repair_is_reviewed_confirmed_and_verified_from_fresh_statuses_in_the_view() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_verify.extend([
        RepairStep::Waiting {
            detail: "Waiting for the new instance to report healthy…".into(),
            if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health.", true),
            closeable: true,
        },
        RepairStep::Waiting {
            detail: "Waiting for the new instance to report healthy…".into(),
            if_timed_out: finish(RepairOutcome::OutcomeUnknown, "No health.", true),
            closeable: true,
        },
    ]);
    f.open_repair();
    assert!(f.enabled(live::ids::REPAIR));
    assert!(
        !f.shown(live::ids::REPAIR_CONFIRM),
        "no consent without a preview"
    );
    assert!(!f.shown(live::ids::REPAIR_RESUME));
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    assert!(
        f.message()
            .contains("Repair puts back what this installer ships")
    );
    assert!(
        !f.enabled(live::ids::REPAIR),
        "the review can't be started again while a preview is up"
    );
    assert!(
        !f.shown(live::ids::REMOVE_REVIEW),
        "removal isn't offered over a repair"
    );
    // The plan was made from a Status issued after the click.
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
    assert!(
        !f.enabled(live::ids::BACK),
        "Back waits while the new agent is watched"
    );
    f.until("the verified repair", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(f.message().contains("The new Crosspane reported healthy."));
    assert!(f.enabled(live::ids::BACK));
    assert!(
        !f.shown(live::ids::REPAIR_RESUME),
        "nothing to resume after a verified repair"
    );
    // Every verification was given a Supported Status, and each one was newer than the last.
    let w = f.world().lock().unwrap();
    assert_eq!(w.repair_verified.len(), 3);
    let ids: Vec<u64> = w.repair_verified.iter().map(|i| i.unwrap()).collect();
    assert!(ids.windows(2).all(|p| p[0] < p[1]), "{ids:?}");
    assert_eq!(w.repair_confirmed.len(), 1);
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
            .any(|c| c.starts_with("repair.confirm")),
        "a click on a view that is no longer current is dropped"
    );
    assert!(f.shown(live::ids::REPAIR_CONFIRM));
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
            true,
        ),
    ] {
        let mut f = flow::Flow::new();
        f.world()
            .lock()
            .unwrap()
            .repair_confirm
            .push_back(Ok(RepairStep::Finished(finish(
                outcome,
                "Kept: the old backups.",
                resume,
            ))));
        f.open_repair();
        f.click(live::ids::REPAIR);
        f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
        f.click(live::ids::REPAIR_CONFIRM);
        f.until("the outcome", |f| f.message().contains(head));
        assert!(
            f.message().contains("Kept: the old backups."),
            "{outcome:?}: {}",
            f.message()
        );
        assert_eq!(f.enabled(live::ids::REPAIR_RESUME), resume, "{outcome:?}");
        assert!(!f.shown(live::ids::REPAIR_CONFIRM), "{outcome:?}");
        assert!(
            f.enabled(live::ids::CLOSE),
            "nothing is left working: {outcome:?}"
        );
    }
}

#[test]
fn an_incompatible_install_shows_the_guidance_and_cannot_be_repaired() {
    let mut f = flow::Flow::new();
    let guidance = "Some files where Crosspane installs weren't put there by Crosspane. Remove \
                    Crosspane and install it again to start clean.";
    f.world().lock().unwrap().repair_offer = RepairOffer {
        discardable: false,
        repair: Availability::Unavailable(guidance.into()),
        resumable: None,
    };
    f.open_repair();
    assert!(
        f.message()
            .contains("Remove Crosspane and install it again"),
        "{}",
        f.message()
    );
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(
        f.enabled(live::ids::REMOVE_REVIEW),
        "removal stays available"
    );
}

#[test]
fn an_interrupted_repair_shows_resume_and_resume_reaches_verified() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_offer = RepairOffer {
        discardable: false,
        repair: Availability::Unavailable("An earlier repair didn't finish.".into()),
        resumable: Some(vec!["It stopped while files were being replaced.".into()]),
    };
    f.open_repair();
    f.until("the resume offer", |f| f.shown(live::ids::REPAIR_RESUME));
    assert!(f.enabled(live::ids::REPAIR_RESUME));
    assert!(
        !f.enabled(live::ids::REPAIR),
        "no new repair over an unfinished one"
    );
    assert!(
        f.enabled(live::ids::REMOVE_REVIEW),
        "removal stays the way out when a resume can't settle the record"
    );
    assert!(f.message().contains("An earlier repair didn't finish"));
    assert!(
        f.message()
            .contains("It stopped while files were being replaced.")
    );
    f.click(live::ids::REPAIR_RESUME);
    f.until("the verified resume", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(
        calls(f.world())
            .iter()
            .any(|c| c.starts_with("repair.resume(status=Some("))
    );
    assert!(!f.shown(live::ids::REPAIR_RESUME));
}

#[test]
fn closing_the_window_mid_repair_leaves_nothing_working_and_reopening_offers_resume() {
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
    // While files are changing, closing would cut the repair short: it is refused.
    assert!(!f.enabled(live::ids::CLOSE) && !f.shown(live::ids::CLOSE));
    assert!(!f.request_close());
    assert!(f.message().contains("Wait for the current change"));
    world.lock().unwrap().repair_hold_confirm = false;
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    // Nothing is changing now: the window can be closed, and it closes.
    assert!(f.enabled(live::ids::CLOSE));
    assert!(f.close());
    drop(f);
    // A reopened window finds the interrupted repair (the record the coordinator keeps) and
    // offers its resume; nothing is stuck working.
    world.lock().unwrap().repair_offer = RepairOffer {
        discardable: false,
        repair: Availability::Unavailable("An earlier repair didn't finish.".into()),
        resumable: Some(vec!["It stopped after Crosspane was started again.".into()]),
    };
    world.lock().unwrap().repair_hold = false;
    let mut f = flow::Flow::on(world);
    f.open_repair();
    f.until("the resume offer", |f| f.shown(live::ids::REPAIR_RESUME));
    assert!(f.enabled(live::ids::REPAIR_RESUME));
    assert!(f.enabled(live::ids::CLOSE) && f.enabled(live::ids::BACK));
    f.click(live::ids::REPAIR_RESUME);
    f.until("the verified resume", |f| {
        f.message().contains("Crosspane was repaired")
    });
}

#[test]
fn a_repair_that_never_hears_from_the_new_agent_ends_as_unknown_with_a_working_resume() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_hold = true;
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    // The worker ends the wait with the adapter's typed result once the bound passes.
    f.advance_clock(100_000);
    f.until("the unknown outcome", |f| {
        f.message().contains("Outcome unknown, resume required.")
    });
    assert!(f.enabled(live::ids::REPAIR_RESUME));
    f.world().lock().unwrap().repair_hold = false;
    f.click(live::ids::REPAIR_RESUME);
    f.until("the verified resume", |f| {
        f.message().contains("Crosspane was repaired")
    });
}

#[test]
fn a_repair_whose_agent_answers_no_status_is_still_driven_to_the_workers_typed_end() {
    // The old agent is stopped (or the new one never comes up): no Status ever answers. The
    // repair must still be asked to look, so the worker's own bound ends it with the adapter's
    // typed result instead of the view working until the controller's silent-platform backstop.
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_hold = true;
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the wait for the new agent", |f| {
        f.message().contains("Waiting for the new instance")
    });
    f.silent = true;
    f.until("a look without a Status", |f| {
        calls(f.world()).contains(&"repair.verify(status=None)".to_owned())
    });
    f.advance_clock(100_000);
    f.until("the worker's typed end", |f| {
        f.message().contains("Outcome unknown, resume required.")
    });
    assert!(
        f.message().contains("No health in time."),
        "the adapter's own result: {}",
        f.message()
    );
    assert!(f.enabled(live::ids::REPAIR_RESUME));
    // The worker is free again: the resume it offers really runs.
    f.silent = false;
    f.world().lock().unwrap().repair_hold = false;
    f.click(live::ids::REPAIR_RESUME);
    f.until("the verified resume", |f| {
        f.message().contains("Crosspane was repaired")
    });
}

#[test]
fn a_repair_waiting_for_the_old_agents_clean_exit_cannot_be_closed() {
    let mut f = flow::Flow::new();
    {
        let mut w = f.world().lock().unwrap();
        w.repair_confirm.push_back(Ok(RepairStep::Waiting {
            detail: "Crosspane was stopped. Waiting for it to exit cleanly…".into(),
            if_timed_out: finish(RepairOutcome::RecoveryRetained, "No clean exit.", true),
            closeable: false,
        }));
        for _ in 0..3 {
            w.repair_verify.push_back(RepairStep::Waiting {
                detail: "Still waiting for it to exit cleanly…".into(),
                if_timed_out: finish(RepairOutcome::RecoveryRetained, "No clean exit.", true),
                closeable: false,
            });
        }
    }
    f.open_repair();
    f.click(live::ids::REPAIR);
    f.until("the repair preview", |f| f.shown(live::ids::REPAIR_CONFIRM));
    f.click(live::ids::REPAIR_CONFIRM);
    f.until("the clean-exit wait", |f| {
        f.message().contains("Waiting for it to exit cleanly")
    });
    // The old agent is down and the files are still to be replaced: closing would strand it.
    assert!(!f.shown(live::ids::CLOSE));
    assert!(!f.request_close());
    assert!(f.message().contains("Wait for the current change"));
    // Once the platform ends the repair, the window closes normally.
    f.until("the verified repair", |f| {
        f.message().contains("Crosspane was repaired")
    });
    assert!(f.enabled(live::ids::CLOSE));
    assert!(f.close());
}

#[test]
fn the_network_step_is_never_verified_by_a_rule_or_command_alone() {
    let mut f = flow::Flow::new();
    // Active ufw without the rule: the exact rule is offered, applied only after consent, and
    // even after the command succeeds the step stays waiting until traffic is observed.
    f.world().lock().unwrap().fw_read = reading(Some(true), RulePresence::Absent);
    f.install();
    f.reach(crosspane_installer::view::ScreenId::Network);
    f.until("a network preview", |f| {
        f.has_button(live::ids::consent(StepId(30)))
    });
    assert_ne!(
        f.row(30).state,
        crosspane_installer::view::RowState::Verified
    );
    // The firewall change never goes ahead by itself, and its question holds the screen.
    for _ in 0..3 {
        f.status_once();
    }
    assert!(
        !calls(f.world())
            .iter()
            .any(|c| c.starts_with("firewall.apply"))
    );
    assert_eq!(
        f.view().screen,
        crosspane_installer::view::ScreenId::Network
    );
    assert!(
        f.view().message.contains("pkexec /usr/bin/ufw"),
        "the exact command is shown before consent: {}",
        f.view().message
    );
    f.click(live::ids::consent(StepId(30)));
    // After the command the step waits for traffic, and (still without traffic) looks again;
    // either way it is never verified by the command.
    f.until("the command to finish", |f| {
        calls(f.world()).contains(&"firewall.apply".to_owned())
            && matches!(
                f.row(30).state,
                crosspane_installer::view::RowState::Waiting
                    | crosspane_installer::view::RowState::NeedsAction
            )
    });
    assert_ne!(
        f.row(30).state,
        crosspane_installer::view::RowState::Verified
    );
}

#[allow(dead_code)]
fn _unused(_: Option<Value>, _: Option<&Path>, _: Option<Consent>) {}

#[test]
fn r2_gui_discard_uses_fresh_status_then_reoffers_repair_without_resuming() {
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().repair_offer = RepairOffer {
        discardable: true,
        repair: Availability::Unavailable(
            "An earlier repair stopped before changing anything".into(),
        ),
        resumable: None,
    };
    f.open_repair();
    f.until("the discard offer", |f| f.shown(live::ids::REPAIR_DISCARD));
    assert!(
        f.message()
            .contains("An earlier repair stopped before changing anything")
    );
    assert!(f.enabled(live::ids::REPAIR_DISCARD));
    assert!(!f.shown(live::ids::REPAIR_RESUME));
    assert!(!f.enabled(live::ids::REPAIR));
    let before = calls(f.world()).len();
    f.click(live::ids::REPAIR_DISCARD);
    f.until("freshly offered repair after discard", |f| {
        f.enabled(live::ids::REPAIR)
    });
    assert!(!f.shown(live::ids::REPAIR_DISCARD));
    let after = calls(f.world());
    assert!(
        after[before..]
            .iter()
            .any(|c| c.starts_with("repair.discard(status=Some("))
    );
    assert!(
        !after[before..]
            .iter()
            .any(|c| c.starts_with("repair.resume") || c.starts_with("repair.confirm"))
    );
    assert_eq!(
        after[before..]
            .iter()
            .filter(|c| c.starts_with("repair.discard"))
            .count(),
        1
    );
}

#[test]
fn r2_gui_unknown_repair_keeps_the_old_refusal_and_has_no_discard_action() {
    let mut f = flow::Flow::new();
    let text = "The earlier repair record can't be read. Remove Crosspane and install it again.";
    f.world().lock().unwrap().repair_offer = RepairOffer {
        discardable: false,
        repair: Availability::Unavailable(text.into()),
        resumable: None,
    };
    f.open_repair();
    assert!(f.message().contains(text));
    assert!(!f.shown(live::ids::REPAIR_DISCARD));
    assert!(!f.enabled(live::ids::REPAIR));
    assert!(
        !calls(f.world())
            .iter()
            .any(|c| c.starts_with("repair.discard"))
    );
}

#[test]
fn r2_worker_discard_is_one_shot_and_refuses_unoffered_stale_or_replayed_requests() {
    let mut rig = Rig::new();
    // Not offered on this inspection: refused, the repairer is never asked.
    rig.inspect(1);
    let reports = rig.maint(MaintenanceRequest::DiscardRepair {
        id: MaintenanceId(1),
        status: Some(StatusEvidence(healthy(41))),
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
    assert_eq!(rig.count("repair.discard"), 0);
    rig.set(|w| {
        w.repair_offer = RepairOffer {
            discardable: true,
            repair: Availability::Unavailable("An earlier repair stopped.".into()),
            resumable: Some(vec!["ignored while discardable".into()]),
        }
    });
    let reports = rig.inspect(2);
    assert_eq!(
        reports[1],
        MaintenanceReport::RepairDiscardable {
            id: MaintenanceId(2)
        }
    );
    assert_eq!(reports.len(), 2, "discardable is offered instead of resume");
    // An older inspection's id is stale.
    let reports = rig.maint(MaintenanceRequest::DiscardRepair {
        id: MaintenanceId(1),
        status: Some(StatusEvidence(healthy(42))),
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
    assert_eq!(rig.count("repair.discard"), 0);
    let reports = rig.maint(MaintenanceRequest::DiscardRepair {
        id: MaintenanceId(2),
        status: Some(StatusEvidence(healthy(43))),
    });
    assert!(matches!(
        &reports[0],
        MaintenanceReport::RepairDiscarded { .. }
    ));
    // A replay of the same request is refused: the offer was consumed.
    let reports = rig.maint(MaintenanceRequest::DiscardRepair {
        id: MaintenanceId(2),
        status: Some(StatusEvidence(healthy(44))),
    });
    assert!(matches!(&reports[0], MaintenanceReport::Refused { .. }));
    assert_eq!(rig.count("repair.discard"), 1);
}

#[test]
fn r2_gui_a_refused_discard_leaves_nothing_working_and_removal_reachable() {
    let mut f = flow::Flow::new();
    {
        let world = f.world();
        let mut w = world.lock().unwrap();
        w.repair_offer = RepairOffer {
            discardable: true,
            repair: Availability::Unavailable("An earlier repair stopped.".into()),
            resumable: None,
        };
        w.discard_refusal =
            Some("Discarding the record needs Crosspane. Nothing was changed.".into());
    }
    f.open_repair();
    f.until("the discard offer", |f| f.shown(live::ids::REPAIR_DISCARD));
    assert!(
        f.enabled(live::ids::REMOVE_REVIEW),
        "removal stays offered beside Discard"
    );
    f.click(live::ids::REPAIR_DISCARD);
    f.until("the refusal", |f| f.message().contains("needs Crosspane"));
    assert!(!f.shown(live::ids::REPAIR_DISCARD));
    assert!(f.enabled(live::ids::CLOSE) && f.enabled(live::ids::BACK));
    // Back re-inspects: the record is offered again and removal is reachable.
    f.click(live::ids::BACK);
    f.open_repair();
    f.until("the discard offer again", |f| {
        f.enabled(live::ids::REPAIR_DISCARD)
    });
    assert!(f.enabled(live::ids::REMOVE_REVIEW));
    assert_eq!(
        calls(f.world())
            .iter()
            .filter(|c| c.starts_with("repair.discard"))
            .count(),
        1
    );
}

#[test]
fn the_compatibility_card_lists_each_check_from_the_real_worker_and_check_again_reruns_it() {
    use crosspane_installer::view::{RowState, RowView, ScreenId};
    let mut f = flow::Flow::new();
    f.world().lock().unwrap().sup = Sup::Pending("couldn't read the session environment");
    f.until("the welcome screen", |f| {
        f.view().screen == ScreenId::Welcome
    });
    f.go_next();
    assert_eq!(f.view().screen, ScreenId::Compatibility);
    let checks = |f: &flow::Flow| -> Vec<RowView> {
        f.view()
            .rows
            .iter()
            .filter(|r| r.is_check())
            .cloned()
            .collect()
    };
    f.until("the pending pass's checklist", |f| {
        checks(f)
            .first()
            .is_some_and(|r| r.state == RowState::Waiting)
    });
    let rows = checks(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "Fake check");
    assert_eq!(rows[0].detail, "couldn't read the session environment");
    let card = f.view().rows[0].clone();
    assert_eq!(card.id, 10);
    assert!(card.detail.contains("Last checked"), "{card:?}");
    // Check again: the rows read as checking until the new pass ends, then show what it found.
    f.world().lock().unwrap().sup = Sup::Supported;
    f.world().lock().unwrap().hold_support = true;
    f.click(live::ids::retry(StepId(10)));
    f.until("the rerun to start", |f| {
        checks(f)
            .first()
            .is_some_and(|r| r.state == RowState::Working)
    });
    assert!(f.view().rows[0].detail.ends_with("Checking now…"));
    f.world().lock().unwrap().hold_support = false;
    f.until("support verified", |f| f.verified(10));
    let rows = checks(&f);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, RowState::Verified);
}
