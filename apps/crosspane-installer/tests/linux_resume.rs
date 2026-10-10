#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Interrupted, hostile and resumed states on real native adapters over scratch targets. Only the
//! support detection is injected (a scratch target has no session to read); the payload installer
//! and the removal coordinator are the merged production adapters, working on a fresh directory
//! under /tmp. No real agent, manager, bus, firewall, keyring or owner path is touched.

use std::collections::BTreeMap;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crosspane_installer::agent_contract::*;
use crosspane_installer::live::{
    Availability, MaintenanceId, MaintenanceOutcome, MaintenanceReport, MaintenanceRequest,
    NativeJob, NativeOutcome, NativeReport, Platform, StepReport,
};
use crosspane_installer::platform::linux::integration::{
    AgentSource, DomainFactory, Domains, FirewallReading, Firewalls, LinuxPlatform, NativePayloads,
    NativeRepairer, NativeUninstaller, Parts, RuleApply, RulePresence, Services, Support,
    SupportOutcome, SupportedAgentPort,
};
use crosspane_installer::platform::linux::{
    firewall::FirewallError,
    native_io::*,
    payload::*,
    service::{AgentEvidence, ServiceAction, ServiceError, ServiceFacts, ServiceResult},
};
use crosspane_installer_core::{JobIntent, JobStage, ObservationSource, OperationId, StepId};
use serde_json::{Value, json};

const PAYLOAD: StepId = StepId(20);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
static ID: AtomicU64 = AtomicU64::new(0);

const HEALTH: &str = r#"{"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],
"displays":[],"peers":[],"layout":[],"installer":{"schema_version":1,
"build":{"version":"0.0.0","features":["video"]},"instance":{"id":9,"pid":4242,"uid":1000,
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
"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;

// ---- a staged package -------------------------------------------------------------------------

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
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../packaging/linux/crosspane-agent.service"
                ))
                .to_vec()
            } else if i == 5 {
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../packaging/linux/crosspane-settings.desktop"
                ))
                .to_vec()
            } else if i == 6 {
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../packaging/linux/crosspane-installer.desktop"
                ))
                .to_vec()
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
    h[148..156].fill(b' ');
    let n: usize = h.iter().map(|b| usize::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{n:06o}\0 ").as_bytes());
    h.extend_from_slice(b);
    h.resize(h.len().div_ceil(512) * 512, 0);
    h
}

fn package(version: u8) -> Package {
    let data = contents(version);
    let m = manifest(version, &data);
    let mut a = member("manifest.json", &serde_json::to_vec(&m).unwrap());
    for (n, b) in FILES.iter().zip(&data) {
        a.extend(member(n, b));
    }
    a.extend(vec![0; 1024]);
    Package::read(a.as_slice(), Architecture::native().unwrap(), sha256(&a)).unwrap()
}

// ---- a scratch target with just enough fake process facts -------------------------------------

struct Runner;
impl CommandRunner for Runner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        if c.executable() != Path::new("/bin/ps") {
            // No manager, firewall or anything else exists on a scratch target.
            return Err(NativeError::Unavailable);
        }
        Ok(CommandOutput {
            code: Some(0),
            stdout: if c.argv()[1] == "lstart=" {
                START.to_vec()
            } else {
                b"crosspane-agent\n".to_vec()
            },
            stderr: vec![],
        })
    }
}

struct Probe(PathBuf);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: self.0.join(".local/bin/crosspane-agent"),
            generation: 77,
        })
    }
}

fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}

fn observations(io: &LinuxNativeIo) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
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
    }
}

struct Scratch {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    /// Stands in for the user manager's private socket, which the service adapter requires to
    /// exist. Nothing ever answers on it.
    _manager: std::os::unix::net::UnixListener,
}

impl Scratch {
    fn new() -> Arc<Self> {
        let root = PathBuf::from(format!(
            "/tmp/cp421r-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let io = Arc::new(
            LinuxNativeIo::scratch(&root, Arc::new(Runner), Arc::new(Probe(root.clone()))).unwrap(),
        );
        let proof = io.scratch_support(observations(&io)).unwrap();
        for path in [
            io.target().paths().prefix.join("bin"),
            io.target().paths().config_home.join("crosspane"),
            io.target().paths().state_home.join("crosspane"),
            io.target().paths().data_home.join("crosspane"),
            io.target().runtime_dir().to_path_buf(),
            io.target().paths().runtime_home.join("systemd"),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        let manager = std::os::unix::net::UnixListener::bind(
            io.target().paths().runtime_home.join("systemd/private"),
        )
        .unwrap();
        Arc::new(Self {
            root,
            io,
            _manager: manager,
        })
    }

    fn proof(&self) -> SupportProof {
        self.io.scratch_support(observations(&self.io)).unwrap()
    }

    fn state(&self) -> PathBuf {
        self.io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer")
    }

    fn installed(&self) -> Vec<PathBuf> {
        PayloadInstaller::new(self.io.clone())
            .unwrap()
            .targets()
            .to_vec()
    }

    fn bootstrap(&self) {
        let proof = self.proof();
        self.io
            .atomic_write(
                &proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &serde_json::to_vec(&json!({"schema_version":1,"instance_id":9,"pid":4242,
                    "started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,
                    "keystore":"os_store","reason":null,"runtime_dir":self.io.target().runtime_dir()}))
                .unwrap(),
            )
            .unwrap();
    }

    fn reply(&self, version: &str) -> AgentReply {
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let h = &mut v["result"]["installer"];
        h["build"]["version"] = json!(version);
        h["instance"]["uid"] = json!(self.io.target().paths().uid);
        h["instance"]["exe"] = json!(self.io.target().agent_path());
        h["instance"]["runtime_dir"] = json!(self.io.target().runtime_dir());
        h["instance"]["started_unix_ms"] = json!(parse_ps_start(START).unwrap());
        AgentReply {
            id: 19,
            observed_at_ms: 100,
            source: ObservationSource::Demo,
            result: decode_reply(
                &InstallerRequest::Status,
                &serde_json::to_vec(&v).unwrap(),
                AgentPlatform::Linux,
            ),
        }
    }

    /// A completed, verified install, as an earlier run of the installer would have left it.
    fn install(&self, pkg: &Package) {
        let installer = PayloadInstaller::new(self.io.clone()).unwrap();
        let proof = self.proof();
        let plan = installer
            .plan(&proof, pkg, OperationId(47), MatchingFiles::Preserve)
            .unwrap();
        installer.apply(&proof, pkg, plan, &deadline()).unwrap();
        self.bootstrap();
        installer
            .verify(
                &proof,
                pkg,
                19,
                100,
                &self.reply(&pkg.manifest().product_version),
                &deadline(),
            )
            .unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---- fakes for what a scratch target cannot be -------------------------------------------------

struct ScratchSupport(Arc<LinuxNativeIo>);
impl Support for ScratchSupport {
    fn detect(&self, _: Option<&Package>, _: &Deadline) -> SupportOutcome {
        SupportOutcome::Supported(self.0.scratch_support(observations(&self.0)).unwrap())
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Live
    }
}

struct NoServices;
impl Services for NoServices {
    fn prepare(&mut self, _: &Package, _: &Deadline) -> Result<(), ServiceError> {
        Ok(())
    }
    fn observe(&mut self, _: &Deadline) -> Result<ServiceFacts, ServiceError> {
        Err(ServiceError::Unknown)
    }
    fn apply(
        &mut self,
        _: &SupportProof,
        _: ServiceAction,
        _: &Deadline,
    ) -> Result<ServiceResult, ServiceError> {
        Err(ServiceError::Unknown)
    }
    fn agent(
        &mut self,
        _: &ServiceFacts,
        _: Option<&AgentReply>,
        _: u64,
        _: u64,
        _: Option<u64>,
        _: &Deadline,
    ) -> Result<AgentEvidence, ServiceError> {
        Err(ServiceError::Unknown)
    }
}

struct NoFirewalls;
impl Firewalls for NoFirewalls {
    fn read(&mut self, _: &Deadline) -> Result<FirewallReading, FirewallError> {
        Ok(FirewallReading {
            active: None,
            lan_rule: RulePresence::Unknown,
            link: None,
        })
    }
    fn plan_lan(
        &mut self,
        _: &SupportProof,
        _: OperationId,
        _: &Deadline,
    ) -> Result<String, FirewallError> {
        Err(FirewallError::Manual)
    }
    fn apply_lan(
        &mut self,
        _: &SupportProof,
        _: OperationId,
        _: &Deadline,
    ) -> Result<RuleApply, FirewallError> {
        Err(FirewallError::Stale)
    }
    fn receipt_lan(&mut self, _: &SupportProof, _: OperationId) -> Option<Vec<u8>> {
        None
    }
}

struct IdleAgent;
impl AgentPort for IdleAgent {
    fn submit(&mut self, _: AgentCall) -> Result<(), CallFailure> {
        Err(CallFailure::Unavailable)
    }
    fn poll(&mut self) -> Vec<AgentReply> {
        Vec::new()
    }
}
impl SupportedAgentPort for IdleAgent {
    fn refresh_support(&mut self, _: SupportProof) -> Result<(), NativeError> {
        Ok(())
    }
}

struct Rig {
    platform: LinuxPlatform,
    scratch: Arc<Scratch>,
    steps: Vec<StepReport>,
    maintenance: Vec<MaintenanceReport>,
    next_op: u64,
    clock: Arc<AtomicU64>,
}

impl Rig {
    fn new(scratch: Arc<Scratch>, pkg: Option<Package>) -> Self {
        let clock = Arc::new(AtomicU64::new(10_000));
        let time = clock.clone();
        let io = scratch.io.clone();
        let support = Arc::new(ScratchSupport(io.clone()));
        let env = ChildEnvironment::selected(io.target(), BTreeMap::new()).unwrap();
        let domain_io = io.clone();
        let uninstall_support = support.clone();
        let uninstall_env = env.clone();
        let repair_support = support.clone();
        let repair_env = env.clone();
        let uninstall_clock: Arc<dyn Fn() -> u64 + Send + Sync> = Arc::new(|| 10_000);
        let domains: DomainFactory = Box::new(move || Domains {
            payloads: Box::new(NativePayloads::new(domain_io.clone()).unwrap()),
            services: Box::new(NoServices),
            firewalls: Box::new(NoFirewalls),
            uninstaller: Box::new(NativeUninstaller::new(
                domain_io.clone(),
                uninstall_env,
                uninstall_clock,
                uninstall_support,
            )),
            repairer: Box::new(NativeRepairer::new(domain_io, repair_env, repair_support)),
        });
        let platform = LinuxPlatform::compose(Parts {
            io,
            env,
            clock: Arc::new(move || time.load(Ordering::SeqCst)),
            payload: None,
            support,
            domains,
            agent: AgentSource::Injected(Box::new(IdleAgent)),
            package: pkg,
        })
        .unwrap();
        Self {
            platform,
            scratch,
            steps: Vec::new(),
            maintenance: Vec::new(),
            next_op: 0,
            clock,
        }
    }

    fn pump(&mut self, until: impl Fn(&Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
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

    fn job(&mut self, stage: JobStage) -> JobIntent {
        self.next_op += 1;
        JobIntent {
            step: PAYLOAD,
            operation: OperationId(self.next_op),
            stage,
        }
    }

    fn step(
        &mut self,
        stage: JobStage,
        consent: Option<crosspane_installer::live::Consent>,
    ) -> (JobIntent, StepReport) {
        let job = self.job(stage);
        self.step_job(job, consent)
    }

    fn step_job(
        &mut self,
        job: JobIntent,
        consent: Option<crosspane_installer::live::Consent>,
    ) -> (JobIntent, StepReport) {
        let before = self.steps.len();
        self.platform
            .submit(NativeJob::Step {
                job: job.clone(),
                consent,
                status: None,
            })
            .unwrap();
        self.pump(|r| r.steps.len() > before);
        (job, self.steps.last().unwrap().clone())
    }

    fn install_through_the_worker(&mut self) -> NativeOutcome {
        let (plan_job, plan) = self.step(JobStage::Plan, None);
        assert!(
            matches!(plan.outcome, NativeOutcome::Planned { .. }),
            "{plan:?}"
        );
        let apply = self.job(JobStage::Apply);
        let consent = crosspane_installer::live::Consent {
            plan: plan_job.operation,
            operation: apply.operation,
            revision: 1,
        };
        self.step_job(apply, Some(consent)).1.outcome
    }

    fn maintain(&mut self, request: MaintenanceRequest, expect: usize) {
        self.platform
            .submit(NativeJob::Maintenance(request))
            .unwrap();
        self.pump(|r| r.maintenance.len() >= expect);
    }

    fn files_present(&self) -> usize {
        self.scratch
            .installed()
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .count()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.platform.shutdown();
        self.clock.fetch_add(1, Ordering::SeqCst);
    }
}

fn is_verified(outcome: &NativeOutcome) -> bool {
    matches!(outcome, NativeOutcome::Verified { .. })
}

// ---- install: interrupted, resumed ------------------------------------------------------------

#[test]
fn an_install_cut_off_midway_is_detected_resumed_and_only_then_verified() {
    let scratch = Scratch::new();
    let pkg = package(1);
    // An earlier run stopped partway through writing the files.
    {
        let mut installer = PayloadInstaller::new(scratch.io.clone()).unwrap();
        installer
            .scratch_interrupt(Some(Interruption::Replaced(2)))
            .unwrap();
        let proof = scratch.proof();
        let plan = installer
            .plan(&proof, &pkg, OperationId(9), MatchingFiles::Preserve)
            .unwrap();
        assert!(installer.apply(&proof, &pkg, plan, &deadline()).is_err());
    }
    let mut rig = Rig::new(scratch, Some(package(1)));
    // Detection sees an unfinished install and asks for action; nothing is silently green.
    let (_, detect) = rig.step(JobStage::Detect, None);
    assert_eq!(
        detect.outcome,
        NativeOutcome::Detected { needs_action: true },
        "{detect:?}"
    );
    let (_, verify) = rig.step(JobStage::Verify, None);
    assert!(
        !is_verified(&verify.outcome),
        "an unfinished install is never verified"
    );
    // The plan is a resume of the journal, previewed before it is applied.
    let (plan_job, plan) = rig.step(JobStage::Plan, None);
    let NativeOutcome::Planned { preview } = plan.outcome else {
        panic!("{:?}", plan.outcome)
    };
    assert!(preview.to_lowercase().contains("interrupted"), "{preview}");
    let apply = rig.job(JobStage::Apply);
    let consent = crosspane_installer::live::Consent {
        plan: plan_job.operation,
        operation: apply.operation,
        revision: 2,
    };
    let (_, applied) = rig.step_job(apply, Some(consent));
    assert_eq!(
        applied.outcome,
        NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
    );
    assert_eq!(
        rig.files_present(),
        FILES.len().min(rig.scratch.installed().len())
    );
    let (_, verify) = rig.step(JobStage::Verify, None);
    assert!(is_verified(&verify.outcome), "{verify:?}");
}

#[test]
fn a_clean_install_goes_through_the_worker_and_leaves_an_owned_matching_install() {
    let mut rig = Rig::new(Scratch::new(), Some(package(1)));
    let (_, detect) = rig.step(JobStage::Detect, None);
    assert_eq!(
        detect.outcome,
        NativeOutcome::Detected { needs_action: true }
    );
    assert_eq!(
        rig.install_through_the_worker(),
        NativeOutcome::Applied(crosspane_installer_core::ApplyOutcome::Applied)
    );
    let (_, verify) = rig.step(JobStage::Verify, None);
    assert!(is_verified(&verify.outcome), "{verify:?}");
    // Until the running agent confirms the install, its record is unfinished, and a later check
    // says so instead of assuming the files are owned.
    let (_, unconfirmed) = rig.step(JobStage::Detect, None);
    assert_eq!(
        unconfirmed.outcome,
        NativeOutcome::Detected { needs_action: true },
        "{unconfirmed:?}"
    );
    // The agent step completes the record against a fresh matched status.
    let pkg = package(1);
    rig.scratch.bootstrap();
    let proof = rig.scratch.proof();
    PayloadInstaller::new(rig.scratch.io.clone())
        .unwrap()
        .verify(
            &proof,
            &pkg,
            19,
            100,
            &rig.scratch.reply("0.0.1"),
            &deadline(),
        )
        .unwrap();
    let (_, again) = rig.step(JobStage::Detect, None);
    assert_eq!(
        again.outcome,
        NativeOutcome::Detected {
            needs_action: false
        },
        "{again:?}"
    );
}

#[test]
fn a_missing_payload_never_mutates_the_install_locations() {
    let scratch = Scratch::new();
    let mut rig = Rig::new(scratch, None);
    let (_, detect) = rig.step(JobStage::Detect, None);
    assert!(matches!(detect.outcome, NativeOutcome::Waiting(_)));
    assert_eq!(rig.files_present(), 0);
}

// ---- hostile and stale records ----------------------------------------------------------------

/// A named way of leaving a hostile or stale record behind on the scratch target.
type Forgery = (&'static str, Box<dyn Fn(&Scratch)>);

fn record(scratch: &Scratch) -> PathBuf {
    scratch.state().join("payload-outcome.json")
}

fn forge(scratch: &Scratch, bytes: &[u8], mode: u32) {
    let path = record(scratch);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[test]
fn forged_oversized_wrong_mode_linked_and_unknown_schema_records_never_authorize_removal() {
    let forgeries: Vec<Forgery> = vec![
        (
            "not json",
            Box::new(|s| forge(s, b"this is not json", 0o600)),
        ),
        ("empty", Box::new(|s| forge(s, b"", 0o600))),
        (
            "oversize",
            Box::new(|s| forge(s, &vec![b'{'; 128 * 1024], 0o600)),
        ),
        (
            "unknown schema",
            Box::new(|s| {
                forge(
                    s,
                    br#"{"receipt":{"schema_version":99,"operation_id":1},"items":[],"phase":"Verified"}"#,
                    0o600,
                )
            }),
        ),
        (
            "group writable",
            Box::new(|s| {
                let original = std::fs::read(record(s)).unwrap();
                forge(s, &original, 0o660);
            }),
        ),
        (
            "symlink to elsewhere",
            Box::new(|s| {
                let original = std::fs::read(record(s)).unwrap();
                let elsewhere = s.root.join("elsewhere.json");
                std::fs::write(&elsewhere, original).unwrap();
                std::fs::remove_file(record(s)).unwrap();
                symlink(&elsewhere, record(s)).unwrap();
            }),
        ),
    ];
    for (name, forge) in forgeries {
        let scratch = Scratch::new();
        let pkg = package(1);
        scratch.install(&pkg);
        let present = scratch.installed().len();
        forge(&scratch);
        let mut rig = Rig::new(scratch, Some(package(1)));
        rig.maintain(
            MaintenanceRequest::Inspect {
                id: MaintenanceId(1),
            },
            1,
        );
        let MaintenanceReport::Inspected { uninstall, .. } = &rig.maintenance[0] else {
            panic!("{name}: {:?}", rig.maintenance[0])
        };
        assert!(
            matches!(uninstall, Availability::Unavailable(_)),
            "{name}: a hostile record must not offer removal, got {uninstall:?}"
        );
        // Planning and confirming anyway deletes nothing.
        rig.maintain(
            MaintenanceRequest::PlanUninstall {
                id: MaintenanceId(1),
                choices: Vec::new(),
                status: None,
            },
            2,
        );
        assert!(
            matches!(rig.maintenance[1], MaintenanceReport::Refused { .. }),
            "{name}: {:?}",
            rig.maintenance[1]
        );
        assert_eq!(
            rig.files_present(),
            present,
            "{name}: every installed file is still there"
        );
    }
}

#[test]
fn a_forged_record_does_not_recreate_a_green_install() {
    let scratch = Scratch::new();
    let pkg = package(1);
    scratch.install(&pkg);
    forge(&scratch, br#"{"receipt":{},"phase":"Verified"}"#, 0o600);
    let mut rig = Rig::new(scratch, Some(package(1)));
    let (_, verify) = rig.step(JobStage::Verify, None);
    assert!(
        !is_verified(&verify.outcome) || rig.files_present() > 0,
        "a forged record can't make a missing install green"
    );
    // With the files removed behind its back, no outcome record can claim they are there.
    for path in rig.scratch.installed() {
        let _ = std::fs::remove_file(path);
    }
    let (_, verify) = rig.step(JobStage::Verify, None);
    assert!(!is_verified(&verify.outcome), "{verify:?}");
}

// ---- removal: unknown outcomes, retained recovery, resume --------------------------------------

fn removal_lines(rig: &Rig) -> (MaintenanceOutcome, Vec<String>) {
    let MaintenanceReport::Finished { outcome, lines, .. } = rig.maintenance.last().unwrap() else {
        panic!("{:?}", rig.maintenance.last())
    };
    (*outcome, lines.clone())
}

#[test]
fn removal_that_cannot_prove_a_clean_stop_ends_partial_and_keeps_recovery_tools() {
    let scratch = Scratch::new();
    let pkg = package(1);
    scratch.install(&pkg);
    let present = scratch.installed().len();
    let mut rig = Rig::new(scratch, Some(package(1)));
    rig.maintain(
        MaintenanceRequest::Inspect {
            id: MaintenanceId(1),
        },
        1,
    );
    let MaintenanceReport::Inspected {
        uninstall, choices, ..
    } = &rig.maintenance[0]
    else {
        panic!("{:?}", rig.maintenance[0])
    };
    assert_eq!(*uninstall, Availability::Available);
    assert!(
        choices.iter().all(|c| !c.checked),
        "no destructive choice is pre-checked"
    );
    rig.maintain(
        MaintenanceRequest::PlanUninstall {
            id: MaintenanceId(1),
            choices: vec![(1, false)],
            status: None,
        },
        2,
    );
    let MaintenanceReport::Planned { preview, .. } = &rig.maintenance[1] else {
        panic!("{:?}", rig.maintenance[1])
    };
    assert!(preview.contains("identity"), "{preview}");
    rig.platform
        .submit(NativeJob::Maintenance(
            MaintenanceRequest::ConfirmUninstall {
                id: MaintenanceId(1),
                revision: 3,
                status: None,
            },
        ))
        .unwrap();
    rig.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
    });
    let (outcome, lines) = removal_lines(&rig);
    // There is no manager on a scratch target, so startup removal is unknown: the run ends
    // honestly instead of guessing, and what it could not settle is kept.
    assert_ne!(outcome, MaintenanceOutcome::Removed, "{lines:?}");
    let joined = lines.join("\n");
    assert!(
        joined.contains("Recovery tools were kept") || joined.contains("clean stop"),
        "{joined}"
    );
    assert!(
        joined.contains("identity and pairings were kept"),
        "{joined}"
    );
    assert!(
        rig.files_present() > 0 && present > 0,
        "nothing uncertain was deleted"
    );
}

#[test]
fn an_interrupted_removal_is_resumed_from_its_record_after_a_new_preview_and_consent() {
    let scratch = Scratch::new();
    let pkg = package(1);
    scratch.install(&pkg);
    let mut first = Rig::new(scratch, Some(package(1)));
    first.maintain(
        MaintenanceRequest::Inspect {
            id: MaintenanceId(1),
        },
        1,
    );
    first.maintain(
        MaintenanceRequest::PlanUninstall {
            id: MaintenanceId(1),
            choices: Vec::new(),
            status: None,
        },
        2,
    );
    first
        .platform
        .submit(NativeJob::Maintenance(
            MaintenanceRequest::ConfirmUninstall {
                id: MaintenanceId(1),
                revision: 1,
                status: None,
            },
        ))
        .unwrap();
    first.pump(|r| {
        r.maintenance
            .iter()
            .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
    });
    // The installer is closed. A new one over the same target must not begin a second removal
    // over the first's record: it resumes it, and says so in the preview.
    let scratch = first.scratch.clone();
    let mut second = Rig::new(scratch, Some(package(1)));
    second.maintain(
        MaintenanceRequest::Inspect {
            id: MaintenanceId(1),
        },
        1,
    );
    second.maintain(
        MaintenanceRequest::PlanUninstall {
            id: MaintenanceId(1),
            choices: Vec::new(),
            status: None,
        },
        2,
    );
    let MaintenanceReport::Planned { preview, .. } = &second.maintenance[1] else {
        panic!("{:?}", second.maintenance[1])
    };
    assert!(
        preview.contains("earlier removal didn't finish"),
        "the resume is disclosed before anything runs: {preview}"
    );
    // Confirming without a preview first is still refused.
    second
        .platform
        .submit(NativeJob::Maintenance(
            MaintenanceRequest::ConfirmUninstall {
                id: MaintenanceId(9),
                revision: 1,
                status: None,
            },
        ))
        .unwrap();
    second.pump(|r| r.maintenance.len() >= 3);
    assert!(matches!(
        second.maintenance[2],
        MaintenanceReport::Refused { .. }
    ));
}

#[test]
fn an_administrator_edit_is_preserved_by_removal() {
    let scratch = Scratch::new();
    let pkg = package(1);
    scratch.install(&pkg);
    // The administrator changed an owned data file after install.
    let edited = scratch.installed().last().unwrap().clone();
    let original = std::fs::read(&edited).unwrap();
    let mut changed = original.clone();
    changed.extend_from_slice(b"# local note\n");
    std::fs::write(&edited, &changed).unwrap();
    let mut rig = Rig::new(scratch, Some(package(1)));
    rig.maintain(
        MaintenanceRequest::Inspect {
            id: MaintenanceId(1),
        },
        1,
    );
    rig.maintain(
        MaintenanceRequest::PlanUninstall {
            id: MaintenanceId(1),
            choices: Vec::new(),
            status: None,
        },
        2,
    );
    if matches!(rig.maintenance[1], MaintenanceReport::Planned { .. }) {
        rig.platform
            .submit(NativeJob::Maintenance(
                MaintenanceRequest::ConfirmUninstall {
                    id: MaintenanceId(1),
                    revision: 1,
                    status: None,
                },
            ))
            .unwrap();
        rig.pump(|r| {
            r.maintenance
                .iter()
                .any(|m| matches!(m, MaintenanceReport::Finished { .. }))
        });
    }
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        changed,
        "a file that no longer matches what setup wrote is never deleted"
    );
}

// ---- the resume hint --------------------------------------------------------------------------

fn hint_path(scratch: &Scratch) -> PathBuf {
    scratch.state().join("resume-hint.json")
}

fn note_for(scratch: Arc<Scratch>) -> (Option<String>, Arc<Scratch>) {
    let rig = Rig::new(scratch, Some(package(1)));
    let note = rig.platform.describe().resume_note;
    let scratch = rig.scratch.clone();
    (note, scratch)
}

#[test]
fn a_valid_hint_only_adds_a_welcome_note_and_never_changes_what_is_detected() {
    let scratch = Scratch::new();
    let proof = scratch.proof();
    scratch
        .io
        .create_private_dir(&proof, &scratch.state())
        .unwrap();
    scratch
        .io
        .atomic_write(
            &proof,
            &hint_path(&scratch),
            br#"{"schema":1,"step":"payload","phase":"applied"}"#,
        )
        .unwrap();
    let (note, scratch) = note_for(scratch);
    assert!(note.unwrap().contains("earlier setup stopped"));
    // With files absent, detection still says the install is needed: the hint is not evidence.
    let mut rig = Rig::new(scratch, Some(package(1)));
    let (_, detect) = rig.step(JobStage::Detect, None);
    assert_eq!(
        detect.outcome,
        NativeOutcome::Detected { needs_action: true }
    );
}

#[test]
fn forged_oversized_wrong_mode_linked_or_unknown_hints_are_ignored_entirely() {
    let cases: Vec<Forgery> = vec![
        (
            "unknown schema",
            Box::new(|s| {
                std::fs::write(
                    hint_path(s),
                    br#"{"schema":9,"step":"payload","phase":"applied"}"#,
                )
                .unwrap();
            }),
        ),
        (
            "unknown step",
            Box::new(|s| {
                std::fs::write(
                    hint_path(s),
                    br#"{"schema":1,"step":"rm -rf","phase":"applied"}"#,
                )
                .unwrap();
            }),
        ),
        (
            "extra field",
            Box::new(|s| {
                std::fs::write(
                    hint_path(s),
                    br#"{"schema":1,"step":"payload","phase":"applied","x":1}"#,
                )
                .unwrap();
            }),
        ),
        (
            "oversize",
            Box::new(|s| {
                std::fs::write(hint_path(s), vec![b' '; 4096]).unwrap();
            }),
        ),
        (
            "garbage",
            Box::new(|s| {
                std::fs::write(hint_path(s), b"\xff\xfe\x00").unwrap();
            }),
        ),
        (
            "symlink",
            Box::new(|s| {
                let target = s.root.join("elsewhere");
                std::fs::write(
                    &target,
                    br#"{"schema":1,"step":"payload","phase":"applied"}"#,
                )
                .unwrap();
                symlink(&target, hint_path(s)).unwrap();
            }),
        ),
    ];
    for (name, make) in cases {
        let scratch = Scratch::new();
        let proof = scratch.proof();
        scratch
            .io
            .create_private_dir(&proof, &scratch.state())
            .unwrap();
        make(&scratch);
        if let Ok(meta) = std::fs::symlink_metadata(hint_path(&scratch))
            && meta.file_type().is_file()
        {
            std::fs::set_permissions(hint_path(&scratch), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        let (note, _scratch) = note_for(scratch);
        assert!(
            note.is_none(),
            "{name}: a hostile hint produced a note: {note:?}"
        );
    }
    // Permissions looser than owner-only are refused too.
    let scratch = Scratch::new();
    let proof = scratch.proof();
    scratch
        .io
        .create_private_dir(&proof, &scratch.state())
        .unwrap();
    std::fs::write(
        hint_path(&scratch),
        br#"{"schema":1,"step":"payload","phase":"applied"}"#,
    )
    .unwrap();
    std::fs::set_permissions(hint_path(&scratch), std::fs::Permissions::from_mode(0o666)).unwrap();
    let (note, _scratch) = note_for(scratch);
    assert!(note.is_none(), "a world-writable hint is ignored");
}

#[test]
fn a_finished_install_leaves_no_resume_note() {
    let scratch = Scratch::new();
    let proof = scratch.proof();
    scratch
        .io
        .create_private_dir(&proof, &scratch.state())
        .unwrap();
    scratch
        .io
        .atomic_write(
            &proof,
            &hint_path(&scratch),
            br#"{"schema":1,"step":"agent","phase":"done"}"#,
        )
        .unwrap();
    let (note, _scratch) = note_for(scratch);
    assert!(note.is_none());
}

#[allow(dead_code)]
fn _silence(_: Option<Value>, _: Option<Arc<Mutex<()>>>) {}
