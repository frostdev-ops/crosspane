#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*, removal::*, repair::*, service::*},
};
use crosspane_installer_core::{MutationOutcome, OperationId};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
type Hook = Box<dyn FnOnce() + Send>;

struct Auth(Arc<AtomicU64>, Arc<AtomicU64>);
struct AuthChild {
    reaped: bool,
    code: i32,
}
impl PkexecRunner for Auth {
    fn spawn(
        &self,
        _: &PkexecCommand,
        deadline: &Deadline,
    ) -> Result<Box<dyn PkexecChild>, NativeError> {
        deadline.check()?;
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(AuthChild {
            reaped: false,
            code: self.1.load(Ordering::Relaxed) as i32,
        }))
    }
}
impl PkexecChild for AuthChild {
    fn poll(&mut self) -> Result<Option<PkexecOutcome>, NativeError> {
        if self.reaped {
            return Ok(None);
        }
        self.reaped = true;
        Ok(Some(PkexecOutcome::Exited {
            code: self.code,
            stdout: Vec::new(),
            stderr: Vec::new(),
            stdout_truncated: false,
            stderr_truncated: false,
        }))
    }
    fn terminate(&mut self) {
        self.reaped = true;
    }
    fn reaped(&mut self) -> bool {
        self.reaped
    }
}

struct Probe(Mutex<Option<ProcessFacts>>);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, deadline: &Deadline) -> Result<ProcessFacts, NativeError> {
        deadline.check()?;
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or(NativeError::Unavailable)
    }
}
impl ExitReader for Probe {
    fn snapshot(&self, _: u32, deadline: &Deadline) -> Result<Option<ProcessFacts>, NativeError> {
        deadline.check()?;
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Runner {
    calls: Mutex<Vec<Vec<String>>>,
    properties: Mutex<BTreeMap<String, String>>,
    unit: Mutex<PathBuf>,
    stop: Mutex<Option<Hook>>,
    start: Mutex<Option<Hook>>,
    failure: Mutex<Option<NativeError>>,
}
impl CommandRunner for Runner {
    fn run(
        &self,
        command: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        deadline.check()?;
        if command.executable() == Path::new("/bin/ps") {
            return Ok(CommandOutput {
                code: Some(0),
                stdout: if command.argv()[1] == "lstart=" {
                    START.to_vec()
                } else {
                    b"crosspane-agent\n".to_vec()
                },
                stderr: vec![],
            });
        }
        assert_eq!(command.executable(), Path::new("/usr/bin/systemctl"));
        assert_eq!(command.argv()[0], "--user");
        self.calls.lock().unwrap().push(command.argv().to_vec());
        let verb = command.argv()[1].as_str();
        let mut properties = self.properties.lock().unwrap();
        let (code, stdout) = match verb {
            "show" => (
                0,
                properties
                    .iter()
                    .map(|(k, v)| format!("{k}={v}\n"))
                    .collect::<String>()
                    .into_bytes(),
            ),
            "cat" => {
                let path = self.unit.lock().unwrap();
                let mut bytes = format!("# {}\n", path.display()).into_bytes();
                bytes.extend(fs::read(&*path).unwrap());
                (0, bytes)
            }
            "is-active" => (
                if properties["ActiveState"] == "active" {
                    0
                } else {
                    3
                },
                format!("{}\n", properties["ActiveState"]).into_bytes(),
            ),
            "is-enabled" => (
                if properties["UnitFileState"] == "enabled" {
                    0
                } else {
                    1
                },
                format!("{}\n", properties["UnitFileState"]).into_bytes(),
            ),
            "stop" | "start" | "restart" | "daemon-reload" => {
                if let Some(error) = self.failure.lock().unwrap().take() {
                    return Err(error);
                }
                match verb {
                    "stop" => {
                        properties.insert("ActiveState".into(), "inactive".into());
                        properties.insert("SubState".into(), "dead".into());
                        properties.insert("MainPID".into(), "0".into());
                    }
                    "start" | "restart" => {
                        properties.insert("ActiveState".into(), "active".into());
                        properties.insert("SubState".into(), "running".into());
                        properties.insert("MainPID".into(), "4242".into());
                    }
                    "daemon-reload" => {
                        properties.insert("NeedDaemonReload".into(), "no".into());
                    }
                    _ => unreachable!(),
                }
                drop(properties);
                let hook = if verb == "stop" {
                    self.stop.lock().unwrap().take()
                } else if matches!(verb, "start" | "restart") {
                    self.start.lock().unwrap().take()
                } else {
                    None
                };
                if let Some(hook) = hook {
                    hook();
                }
                (0, vec![])
            }
            _ => panic!("unexpected fake service mutation"),
        };
        Ok(CommandOutput {
            code: Some(code),
            stdout,
            stderr: vec![],
        })
    }
}

fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn support(io: &LinuxNativeIo) -> SupportObservations {
    SupportObservations {
        uid: io.target().paths().uid,
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
    }
}
fn hex(bytes: &[u8]) -> String {
    sha256(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn fixture_package(version: u8) -> Package {
    let mut elf = vec![0; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    let machine: u16 = if Architecture::native().unwrap() == Architecture::X86_64 {
        62
    } else {
        183
    };
    elf[18..20].copy_from_slice(&machine.to_le_bytes());
    elf[20] = 1;
    elf[52] = 64;
    elf[63] = version;
    let data: Vec<Vec<u8>> = (0..10)
        .map(|i| match i {
            0..=4 => elf.clone(),
            5 => include_bytes!("../../../packaging/linux/crosspane-agent.service").to_vec(),
            6 => include_bytes!("../../../packaging/linux/crosspane-settings.desktop").to_vec(),
            7 => include_bytes!("../../../packaging/linux/crosspane-installer.desktop").to_vec(),
            _ => format!("inert-owned-resource-{i}").into_bytes(),
        })
        .collect();
    let manifest = Manifest {
        schema_version: 1,
        product_version: format!("0.0.{version}"),
        architecture: Architecture::native().unwrap(),
        source_revision: "1".repeat(40),
        profile: "dev".into(),
        libraries: vec![LibraryProvenance {
            name: "libavcodec.so.61".into(),
            sha256: hex(&elf),
        }],
        members: FILES
            .iter()
            .zip(&data)
            .enumerate()
            .map(|(i, (name, bytes))| Artifact {
                name: (*name).into(),
                size: bytes.len(),
                sha256: hex(bytes),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    };
    fn member(name: &str, bytes: &[u8]) -> Vec<u8> {
        let mut header = vec![0; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        for (at, width, value) in [
            (
                100,
                8,
                if name.starts_with("bin/") {
                    0o755
                } else {
                    0o644
                },
            ),
            (108, 8, 0),
            (116, 8, 0),
            (124, 12, bytes.len()),
            (136, 12, 0),
        ] {
            header[at..at + width]
                .copy_from_slice(format!("{value:0width$o}\0", width = width - 1).as_bytes());
        }
        header[156] = b'0';
        header[257..265].copy_from_slice(b"ustar\x0000");
        header[148..156].fill(b' ');
        let sum: usize = header.iter().map(|b| usize::from(*b)).sum();
        header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        header.extend(bytes);
        header.resize(header.len().div_ceil(512) * 512, 0);
        header
    }
    let mut archive = member("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    for (name, bytes) in FILES.iter().zip(&data) {
        archive.extend(member(name, bytes));
    }
    archive.extend(vec![0; 1024]);
    Package::read(
        archive.as_slice(),
        Architecture::native().unwrap(),
        sha256(&archive),
    )
    .unwrap()
}

struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    installer: PayloadInstaller,
    runner: Arc<Runner>,
    probe: Arc<Probe>,
    auth_calls: Arc<AtomicU64>,
    auth_code: Arc<AtomicU64>,
    _manager: UnixListener,
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp419c-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe(Mutex::new(Some(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: root.join(".local/bin/crosspane-agent"),
            generation: 77,
        }))));
        let runner = Arc::new(Runner {
            calls: Mutex::default(),
            properties: Mutex::default(),
            unit: Mutex::default(),
            stop: Mutex::default(),
            start: Mutex::default(),
            failure: Mutex::default(),
        });
        let auth_calls = Arc::new(AtomicU64::new(0));
        let auth_code = Arc::new(AtomicU64::new(0));
        let mut native = LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap();
        native
            .set_scratch_pkexec_runner(Arc::new(Auth(auth_calls.clone(), auth_code.clone())))
            .unwrap();
        let io = Arc::new(native);
        let proof = io.scratch_support(support(&io)).unwrap();
        io.create_private_dir(&proof, &io.target().paths().runtime_home.join("systemd"))
            .unwrap();
        let manager =
            UnixListener::bind(io.target().paths().runtime_home.join("systemd/private")).unwrap();
        let installer = PayloadInstaller::new(io.clone()).unwrap();
        let installed = fixture_package(1);
        let plan = installer
            .plan(&proof, &installed, OperationId(1), MatchingFiles::Preserve)
            .unwrap();
        installer
            .apply(&proof, &installed, plan, &deadline())
            .unwrap();
        let fixture = Self {
            root,
            io,
            installer,
            runner,
            probe,
            auth_calls,
            auth_code,
            _manager: manager,
        };
        fixture.bootstrap(9);
        fixture
            .installer
            .verify(
                &proof,
                &installed,
                19,
                100,
                &fixture.reply(9, 1, |_| {}),
                &deadline(),
            )
            .unwrap();
        fixture.manager_properties();
        fixture
    }
    fn proof(&self) -> SupportProof {
        self.io.scratch_support(support(&self.io)).unwrap()
    }
    fn service(&self, package: &Package) -> LinuxService {
        LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            self.installer.rendered_resources(package).unwrap(),
            &deadline(),
        )
        .unwrap()
    }
    fn repair(&self) -> LinuxRepair {
        let mut repair = LinuxRepair::new(self.io.clone()).unwrap();
        repair.scratch_exit_reader(self.probe.clone()).unwrap();
        repair
    }
    fn manager_properties(&self) {
        let unit = self.installer.targets()[5].clone();
        *self.runner.unit.lock().unwrap() = unit.clone();
        let mut p: BTreeMap<String, String> = MANAGER_PROPERTIES
            .split(',')
            .map(|k| (k.into(), "".into()))
            .collect();
        for (k, v) in [
            ("Id", UNIT),
            ("LoadState", "loaded"),
            ("DynamicUser", "no"),
            ("ActiveState", "active"),
            ("SubState", "running"),
            ("UnitFileState", "enabled"),
            ("WantedBy", "graphical-session.target"),
            ("MainPID", "4242"),
            ("PartOf", "graphical-session.target"),
            ("After", "basic.target graphical-session.target app.slice"),
            ("Requisite", "graphical-session.target"),
            ("KillSignal", "15"),
            ("TimeoutStopUSec", "10s"),
            ("Restart", "on-failure"),
            ("RestartUSec", "3s"),
            ("NeedDaemonReload", "no"),
            ("StartLimitIntervalUSec", "2min"),
            ("StartLimitBurst", "30"),
            ("Type", "simple"),
            ("Requires", "basic.target app.slice"),
            ("Conflicts", "shutdown.target"),
            ("Before", "shutdown.target"),
            ("DefaultDependencies", "yes"),
            ("KillMode", "control-group"),
            ("SendSIGKILL", "yes"),
            ("FinalKillSignal", "9"),
            ("RestartKillSignal", "15"),
            ("SendSIGHUP", "no"),
            ("UMask", "0022"),
            ("RemainAfterExit", "no"),
            ("NotifyAccess", "none"),
            ("StandardInput", "null"),
            ("StandardOutput", "journal"),
            ("StandardError", "inherit"),
            ("TTYPath", "/dev/console"),
            ("Slice", "app.slice"),
            ("Delegate", "no"),
            ("OOMPolicy", "stop"),
            ("ManagedOOMSwap", "auto"),
            ("ManagedOOMMemoryPressure", "auto"),
            ("ManagedOOMPreference", "none"),
            ("SuccessAction", "none"),
            ("FailureAction", "none"),
            ("StartLimitAction", "none"),
            ("JobTimeoutAction", "none"),
            ("OnSuccessJobMode", "fail"),
            ("OnFailureJobMode", "replace"),
            ("StopWhenUnneeded", "no"),
            ("RefuseManualStart", "no"),
            ("RefuseManualStop", "no"),
            ("AllowIsolate", "no"),
            ("IgnoreOnIsolate", "no"),
            ("SurviveFinalKillSignal", "no"),
            ("JobTimeoutUSec", "infinity"),
            ("JobRunningTimeoutUSec", "infinity"),
            ("CollectMode", "inactive"),
            ("RestartMode", "normal"),
            ("RestartSteps", "0"),
            ("RestartMaxDelayUSec", "infinity"),
            ("TimeoutStartFailureMode", "terminate"),
            ("TimeoutStopFailureMode", "terminate"),
            ("RuntimeMaxUSec", "infinity"),
            ("RuntimeRandomizedExtraUSec", "0"),
            ("WatchdogUSec", "0"),
            ("ExitType", "main"),
            ("FileDescriptorStoreMax", "0"),
            ("NFileDescriptorStore", "0"),
            ("FileDescriptorStorePreserve", "restart"),
            ("RootDirectoryStartOnly", "no"),
            ("RootEphemeral", "no"),
            ("RuntimeDirectoryPreserve", "no"),
        ] {
            p.insert(k.into(), v.into());
        }
        p.insert("FragmentPath".into(), unit.to_string_lossy().into_owned());
        let exe = self.io.target().agent_path().to_string_lossy().into_owned();
        p.insert("ExecStart".into(), format!("{{ path={exe} ; argv[]={exe} run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert("ExecStartEx".into(), format!("{{ path={exe} ; argv[]={exe} run ; flags= ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert(
            "Environment".into(),
            [
                format!(
                    "XDG_CONFIG_HOME={}",
                    self.io.target().paths().config_home.display()
                ),
                format!(
                    "XDG_STATE_HOME={}",
                    self.io.target().paths().state_home.display()
                ),
                format!(
                    "XDG_RUNTIME_DIR={}",
                    self.io.target().paths().runtime_home.display()
                ),
                format!(
                    "CROSSPANE_RUNTIME_DIR={}",
                    self.io.target().runtime_dir().display()
                ),
            ]
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(" "),
        );
        *self.runner.properties.lock().unwrap() = p;
    }
    fn bootstrap(&self, instance: u64) {
        let proof = self.proof();
        self.io
            .create_private_dir(&proof, self.io.target().runtime_dir())
            .unwrap();
        write_bootstrap(&self.io, &proof, instance);
    }
    fn reply(&self, instance: u64, version: u8, change: impl FnOnce(&mut Value)) -> AgentReply {
        let backends: Vec<Value> = [
            "capture",
            "keys",
            "pointer",
            "overlay",
            "hotkeys",
            "keystore",
            "windows",
            "parking",
            "frames",
            "tray",
            "links",
            "gpu",
            "home",
            "audio",
            "discovery",
        ]
        .iter()
        .map(|name| json!({"name":name,"state":"ready","reason":null}))
        .collect();
        let mut v = json!({"ok":true,"result":{"controlling":null,"controlled_by":null,"projections":[],"displays":[],"peers":[],"layout":[],"installer":{
            "schema_version":1,"build":{"version":format!("0.0.{version}"),"features":["video"]},
            "instance":{"id":instance,"pid":4242,"uid":self.io.target().paths().uid,"exe":self.io.target().agent_path(),"runtime_dir":self.io.target().runtime_dir(),"started_unix_ms":parse_ps_start(START).unwrap()},
            "config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111","recovery_pending":0,"startup_recovery":"nothing_parked",
            "gate":{"open":true,"session":"unlocked","active":true,"armed":true,"panic":false},"epochs":{"gate":1,"grants":1,"layout":1,"backends":1},"backends":backends,"keystore":"os_store","permissions":[],
            "discovery":{"enabled":true,"running":true,"candidates":0,"error":null},"tray":{"created":true},"audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]
        }}});
        change(&mut v);
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
    fn stop_receipt(&self, change: impl FnOnce(&mut Value)) {
        let mut receipt = json!({"schema_version":1,"instance_id":9,"stopped_unix_ms":parse_ps_start(START).unwrap()+1000,"clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true});
        change(&mut receipt);
        let io = self.io.clone();
        let probe = self.probe.clone();
        *self.runner.stop.lock().unwrap() = Some(Box::new(move || {
            let proof = io.scratch_support(support(&io)).unwrap();
            io.atomic_write(
                &proof,
                &io.target()
                    .paths()
                    .state_home
                    .join("crosspane/last_exit.json"),
                &serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
            *probe.0.lock().unwrap() = None;
        }));
    }
    fn start_instance(&self, instance: u64) {
        let io = self.io.clone();
        let probe = self.probe.clone();
        *self.runner.start.lock().unwrap() = Some(Box::new(move || {
            let proof = io.scratch_support(support(&io)).unwrap();
            write_bootstrap(&io, &proof, instance);
            *probe.0.lock().unwrap() = Some(ProcessFacts {
                uid: io.target().paths().uid,
                executable: io.target().agent_path(),
                generation: 78,
            });
        }));
    }
    fn mutations(&self) -> Vec<String> {
        self.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|argv| match argv[1].as_str() {
                "stop" | "start" | "restart" | "daemon-reload" => Some(argv[1].clone()),
                _ => None,
            })
            .collect()
    }
    fn bytes(&self) -> Vec<Vec<u8>> {
        self.installer
            .targets()
            .iter()
            .map(|p| fs::read(p).unwrap())
            .collect()
    }
    fn clean_stop(&self) -> CleanStop {
        let tracked = Arc::new(
            TrackedAgent::scratch_capture(self.io.clone(), self.probe.clone(), &deadline())
                .unwrap(),
        );
        let service = self.service(&fixture_package(1));
        let proof = self.proof();
        self.stop_receipt(|_| {});
        let plan = service
            .plan(&proof, ServiceAction::Stop, &deadline())
            .unwrap();
        assert_eq!(
            service.apply(&proof, plan, &deadline()).unwrap().outcome,
            MutationOutcome::Verified
        );
        let authority = tracked.clean_authority(&deadline()).unwrap();
        authority.clean_stop(&self.io, &deadline()).unwrap()
    }
}
fn write_bootstrap(io: &LinuxNativeIo, proof: &SupportProof, instance: u64) {
    io.atomic_write(proof, &io.target().runtime_dir().join("bootstrap.json"), &serde_json::to_vec(&json!({"schema_version":1,"instance_id":instance,"pid":4242,"started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":io.target().runtime_dir()})).unwrap()).unwrap();
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn repair_input<'a>(
    proof: &'a SupportProof,
    package: &'a Package,
    service: &'a LinuxService,
    reply: Option<&'a AgentReply>,
    deadline: &'a Deadline,
) -> RepairInput<'a> {
    RepairInput {
        proof,
        package,
        service,
        reply,
        expected_reply_id: 19,
        now_ms: 100,
        deadline,
    }
}

#[test]
fn consent_is_correlated_and_superseded_before_any_mutation() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let mut repair = f.repair();
    let before = f.bytes();
    let inventory = repair.inventory(&input).unwrap();
    let first = repair.plan(inventory, 1, OperationId(2)).unwrap();
    assert!(first.consent(2, OperationId(2), true).is_err());
    assert!(first.consent(1, OperationId(3), true).is_err());
    assert!(first.consent(1, OperationId(2), false).is_err());
    let consent = first.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(&input).unwrap();
    let second = repair.plan(current, 2, OperationId(3)).unwrap();
    let current = repair.inventory(&input).unwrap();
    assert!(repair.begin(first, consent, &current, &input).is_err());
    assert!(second.preview().contains("input, projections and audio"));
    assert!(f.mutations().is_empty());
    assert_eq!(f.bytes(), before);
}

#[test]
fn ambiguous_prior_repair_intent_is_retained_without_service_dispatch() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let service = f.service(&fixture_package(1));
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let mut repair = f.repair();
    let inventory = repair.inventory(&input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(&input).unwrap();
    let path =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/repair-intent.json");
    f.io.atomic_write(&proof, &path, b"ambiguous owned fixture")
        .unwrap();
    let before = f.bytes();
    let result = repair.begin(plan, consent, &current, &input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
    assert!(result.error.is_some());
    assert!(
        result
            .recovery_material
            .iter()
            .any(|m| m.path == path && m.presence == RecoveryPresence::Present)
    );
    assert_eq!(fs::read(path).unwrap(), b"ambiguous owned fixture");
    assert_eq!(f.bytes(), before);
    assert!(f.mutations().is_empty());
}

#[test]
fn clean_stop_payload_start_new_recovered_health_is_the_only_verified_path() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let inventory = repair.inventory(&input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    assert_eq!(plan.delta().len(), 5);
    assert!(plan.preview().contains("Keep this machine"));
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(&input).unwrap();
    f.stop_receipt(|_| {});
    f.start_instance(10);
    let before = f.bytes();
    assert_eq!(
        repair
            .begin(plan, consent, &current, &input)
            .unwrap()
            .outcome,
        RepairOutcome::AwaitingCleanExit
    );
    assert_eq!(f.bytes(), before);
    let result = repair.continue_after_stop(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::AwaitingAgent);
    assert!(result.activity_retired);
    assert_eq!(f.mutations(), ["stop", "start"]);
    let new_service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &new_service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::Verified);
    assert!(
        result
            .resources
            .iter()
            .all(|r| r.outcome == MutationOutcome::Verified)
    );
    assert!(result.error.is_none());
}

#[test]
fn missing_unclean_or_ambiguous_exit_retains_original_payload_and_journals() {
    for change in [0, 1, 2, 3, 4] {
        let f = Fixture::new();
        let package = fixture_package(2);
        let old = fixture_package(1);
        let service = f.service(&old);
        let proof = f.proof();
        let reply = f.reply(9, 1, |_| {});
        let d = deadline();
        let mut repair = f.repair();
        let input = repair_input(&proof, &package, &service, Some(&reply), &d);
        let inventory = repair.inventory(&input).unwrap();
        let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
        let consent = plan.consent(1, OperationId(2), true).unwrap();
        let current = repair.inventory(&input).unwrap();
        if change != 0 {
            f.stop_receipt(|r| match change {
                1 => {
                    r["clean"] = json!(false);
                    r["parking"] = json!("failed");
                }
                2 => r["instance_id"] = json!(8),
                3 => {
                    r["clean"] = json!(false);
                    r["input_journals_empty"] = json!(false);
                }
                4 => {
                    r["clean"] = json!(false);
                    r["audio_stopped"] = json!(false);
                }
                _ => unreachable!(),
            });
        }
        let before = f.bytes();
        repair.begin(plan, consent, &current, &input).unwrap();
        let result = repair.continue_after_stop(&input).unwrap();
        assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
        assert_eq!(f.bytes(), before);
        assert_eq!(f.mutations(), ["stop"]);
        assert!(f.io.target().runtime_dir().join("bootstrap.json").exists());
        assert!(
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/payload-outcome.json")
                .exists()
        );
    }
}

#[test]
fn clean_stop_from_another_context_cannot_mutate_payload_or_start_service() {
    let first = Fixture::new();
    let second = Fixture::new();
    let first_clean = first.clean_stop();
    let second_clean = second.clean_stop();
    assert_eq!(format!("{first_clean:?}"), "CleanStop { .. }");
    let proof = second.proof();
    let package = fixture_package(2);
    let before = second.bytes();
    let plan = second
        .installer
        .plan(&proof, &package, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    assert!(
        second
            .installer
            .apply_after_clean_stop(&proof, &package, plan, &first_clean, &deadline())
            .is_err()
    );
    assert_eq!(second.bytes(), before);
    assert!(
        !second
            .io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-intent.json")
            .exists()
    );
    let service = second.service(&fixture_package(1));
    assert!(
        service
            .plan_after_clean_stop(&proof, ServiceAction::Start, &first_clean, &deadline())
            .is_err()
    );
    let plan = service
        .plan_after_clean_stop(&proof, ServiceAction::Start, &second_clean, &deadline())
        .unwrap();
    let mutations = second.mutations();
    assert!(
        service
            .apply_after_clean_stop(&proof, plan, &first_clean, &deadline())
            .is_err()
    );
    assert_eq!(second.mutations(), mutations);
}

#[test]
fn clean_stop_cannot_be_reused_after_a_new_instance_has_started() {
    let f = Fixture::new();
    let clean = f.clean_stop();
    let proof = f.proof();
    let service = f.service(&fixture_package(1));
    let plan = service
        .plan_after_clean_stop(&proof, ServiceAction::Start, &clean, &deadline())
        .unwrap();
    f.start_instance(10);
    service
        .apply_after_clean_stop(&proof, plan, &clean, &deadline())
        .unwrap();
    let mutations = f.mutations();
    // Even an inactive manager observation cannot turn a replacement bootstrap into the original.
    let mut properties = f.runner.properties.lock().unwrap();
    properties.insert("ActiveState".into(), "inactive".into());
    properties.insert("SubState".into(), "dead".into());
    properties.insert("MainPID".into(), "0".into());
    drop(properties);
    assert!(
        service
            .plan_after_clean_stop(&proof, ServiceAction::Start, &clean, &deadline())
            .is_err()
    );
    assert_eq!(f.mutations(), mutations);
}

#[test]
fn clean_stop_rechecks_changed_bootstrap_before_payload_or_start_dispatch() {
    let f = Fixture::new();
    let clean = f.clean_stop();
    let proof = f.proof();
    let old = fixture_package(1);
    let service = f.service(&old);
    let start = service
        .plan_after_clean_stop(&proof, ServiceAction::Start, &clean, &deadline())
        .unwrap();
    let package = fixture_package(2);
    let payload = f
        .installer
        .plan(&proof, &package, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    let before = f.bytes();
    let mutations = f.mutations();
    // The same PID and instance are insufficient: the exact exited bootstrap was bound at mint.
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut bootstrap: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bootstrap["phase_seq"] = json!(3);
    f.io.atomic_write(&proof, &path, &serde_json::to_vec(&bootstrap).unwrap())
        .unwrap();
    assert!(
        f.installer
            .apply_after_clean_stop(&proof, &package, payload, &clean, &deadline())
            .is_err()
    );
    assert!(
        service
            .apply_after_clean_stop(&proof, start, &clean, &deadline())
            .is_err()
    );
    assert_eq!(f.bytes(), before);
    assert_eq!(f.mutations(), mutations);
    assert!(
        !f.io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-intent.json")
            .exists()
    );
}

#[test]
fn retained_bootstrap_without_clean_stop_keeps_service_routes_refused() {
    let f = Fixture::new();
    let _clean = f.clean_stop();
    let proof = f.proof();
    let old = fixture_package(1);
    let service = f.service(&old);
    let mutations = f.mutations();
    assert!(
        service
            .plan(&proof, ServiceAction::Start, &deadline())
            .is_err()
    );
    assert!(
        service
            .plan(&proof, ServiceAction::Restart, &deadline())
            .is_err()
    );
    assert_eq!(f.mutations(), mutations);
    let package = fixture_package(2);
    let plan = f
        .installer
        .plan(&proof, &package, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    // WP-4.32: setup owns its install paths, so the ordinary install goes ahead over a stopped
    // agent's retained bootstrap. That stopped instance can never confirm the new files.
    let before = f.bytes();
    f.installer
        .apply(&proof, &package, plan, &deadline())
        .unwrap();
    assert_ne!(f.bytes(), before);
    let retained: Value = serde_json::from_slice(
        &fs::read(f.io.target().runtime_dir().join("bootstrap.json")).unwrap(),
    )
    .unwrap();
    let outcome: Value = serde_json::from_slice(
        &fs::read(
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/payload-outcome.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(outcome["previous_instance"], retained["instance_id"]);
}

#[test]
fn matching_payload_after_clean_stop_still_requires_a_new_instance_to_verify() {
    let f = Fixture::new();
    let clean = f.clean_stop();
    let proof = f.proof();
    let package = fixture_package(1);
    let plan = f
        .installer
        .plan(&proof, &package, OperationId(2), MatchingFiles::Preserve)
        .unwrap();
    f.installer
        .apply_after_clean_stop(&proof, &package, plan, &clean, &deadline())
        .unwrap();
    // Native admission succeeds for these owned fake observations, but the old instance ID must fail.
    *f.probe.0.lock().unwrap() = Some(ProcessFacts {
        uid: f.io.target().paths().uid,
        executable: f.io.target().agent_path(),
        generation: 78,
    });
    f.bootstrap(9);
    assert!(
        f.installer
            .verify(
                &proof,
                &package,
                19,
                100,
                &f.reply(9, 1, |_| {}),
                &deadline()
            )
            .is_err()
    );
    f.bootstrap(10);
    f.installer
        .verify(
            &proof,
            &package,
            19,
            100,
            &f.reply(10, 1, |_| {}),
            &deadline(),
        )
        .unwrap();
}

#[test]
fn matching_payload_has_no_implicit_restart_or_cached_health_success() {
    let f = Fixture::new();
    let package = fixture_package(1);
    let service = f.service(&package);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let inventory = repair.inventory(&input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    assert!(plan.delta().is_empty());
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(&input).unwrap();
    let result = repair.begin(plan, consent, &current, &input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::NoDelta);
    assert!(!result.activity_retired);
    assert!(f.mutations().is_empty());
    assert!(
        result
            .resources
            .iter()
            .all(|r| r.outcome == MutationOutcome::Unknown)
    );
    assert!(
        !f.io
            .target()
            .paths()
            .state_home
            .join("crosspane/installer/repair-intent.json")
            .exists()
    );
}

#[test]
fn mixed_or_edited_payload_and_fallback_identity_are_tier2_without_mutation() {
    for file_key in [false, true] {
        let f = Fixture::new();
        let package = fixture_package(2);
        let old = fixture_package(1);
        let service = f.service(&old);
        let proof = f.proof();
        let d = deadline();
        let mut repair = f.repair();
        if !file_key {
            fs::write(&f.installer.targets()[2], b"owned fixture user edit").unwrap();
        }
        let reply = f.reply(9, 1, |v| {
            if file_key {
                v["result"]["installer"]["keystore"] = json!("file");
            }
        });
        let input = repair_input(&proof, &package, &service, Some(&reply), &d);
        let before = f.bytes();
        let inventory = repair.inventory(&input).unwrap();
        assert_eq!(
            inventory.compatibility_issue(),
            Some(if file_key {
                CompatibilityIssue::FallbackIdentity
            } else {
                CompatibilityIssue::MixedOwnership
            })
        );
        assert!(matches!(
            repair.plan(inventory, 1, OperationId(2)),
            Err(RepairError::Tier2(_))
        ));
        assert_eq!(f.bytes(), before);
        assert!(f.mutations().is_empty());
    }
}

#[test]
fn support_revocation_or_cancellation_refuses_before_mutation() {
    for cancel in [false, true] {
        let f = Fixture::new();
        let package = fixture_package(2);
        let old = fixture_package(1);
        let service = f.service(&old);
        let proof = f.proof();
        let token = Cancellation::default();
        let d = Deadline::new(5000, token.clone()).unwrap();
        if cancel {
            token.cancel();
        } else {
            let mut changed = support(&f.io);
            changed.uwsm_managed = false;
            assert_eq!(
                proof.revalidate(&f.io, &changed),
                Err(NativeError::Unsupported)
            );
        }
        let input = repair_input(&proof, &package, &service, None, &d);
        assert!(f.repair().inventory(&input).is_err());
        assert!(f.mutations().is_empty());
    }
}

#[test]
fn changed_activity_retires_the_old_interruption_consent() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let inventory = repair.inventory(&input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let mut changed = f.reply(9, 1, |v| {
        v["result"]["controlling"] = v["result"]["installer"]["node"].clone();
    });
    changed.observed_at_ms = 200;
    let mut input = repair_input(&proof, &package, &service, Some(&changed), &d);
    input.now_ms = 200;
    let current = repair.inventory(&input).unwrap();
    assert_eq!(current.facts().activity.as_ref().unwrap().input, Some(true));
    assert!(repair.begin(plan, consent, &current, &input).is_err());
    assert!(f.mutations().is_empty());
}

fn begin_clean_repair(f: &Fixture, repair: &mut LinuxRepair, input: &RepairInput<'_>) {
    let inventory = repair.inventory(input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(input).unwrap();
    f.stop_receipt(|_| {});
    f.start_instance(10);
    assert_eq!(
        repair
            .begin(plan, consent, &current, input)
            .unwrap()
            .outcome,
        RepairOutcome::AwaitingCleanExit
    );
}
fn backups(f: &Fixture) -> Vec<PathBuf> {
    f.installer.targets()[..5]
        .iter()
        .enumerate()
        .map(|(i, target)| target.with_file_name(format!(".crosspane-previous-2-{i}")))
        .collect()
}

fn admitted_file_health(f: &Fixture, service: &LinuxService) -> AgentReply {
    let proof = f.proof();
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut bootstrap: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bootstrap["keystore"] = json!("file");
    f.io.atomic_write(&proof, &path, &serde_json::to_vec(&bootstrap).unwrap())
        .unwrap();
    let reply = f.reply(10, 2, |v| {
        v["result"]["installer"]["keystore"] = json!("file")
    });
    let facts = service.observe(&deadline()).unwrap();
    assert!(
        matches!(service.agent(&facts, Some(&reply), 19, 100, Some(9), &deadline()).unwrap(), AgentEvidence::Matched(health) if health.installer().keystore == KeyStoreProvenance::File)
    );
    reply
}

#[test]
fn verify_rejects_new_file_keystore_and_retains_backups_until_os_store_health() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let originals = f.bytes();
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    let service = f.service(&package);
    let reply = admitted_file_health(&f, &service);
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
    assert_eq!(
        result.error,
        Some(RepairError::Tier2(CompatibilityIssue::FallbackIdentity))
    );
    for (path, original) in backups(&f).iter().zip(&originals) {
        assert_eq!(fs::read(path).unwrap(), *original);
    }
    assert_eq!(f.mutations(), ["stop", "start"]);
    f.bootstrap(10);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    assert_eq!(
        repair.verify(&input).unwrap().outcome,
        RepairOutcome::Verified
    );
    assert!(backups(&f).iter().all(|p| !p.exists()));
}

#[test]
fn resume_rejects_new_file_keystore_and_retains_backups_until_os_store_health() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let service = f.service(&fixture_package(1));
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let originals = f.bytes();
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    drop(repair);
    let service = f.service(&package);
    let reply = admitted_file_health(&f, &service);
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let mut resumed = f.repair();
    let result = resumed.resume(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
    assert_eq!(
        result.error,
        Some(RepairError::Tier2(CompatibilityIssue::FallbackIdentity))
    );
    for (path, original) in backups(&f).iter().zip(&originals) {
        assert_eq!(fs::read(path).unwrap(), *original);
    }
    assert_eq!(f.mutations(), ["stop", "start"]);
    f.bootstrap(10);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    assert_eq!(
        resumed.resume(&input).unwrap().outcome,
        RepairOutcome::Verified
    );
    assert!(backups(&f).iter().all(|p| !p.exists()));
}

#[test]
fn partial_payload_failure_keeps_backups_and_resume_never_replays_a_mutation() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let before = f.bytes();
    repair
        .scratch_payload_interrupt(Some(Interruption::Replaced(0)))
        .unwrap();
    begin_clean_repair(&f, &mut repair, &input);
    let result = repair.continue_after_stop(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
    assert_eq!(fs::read(&backups(&f)[0]).unwrap(), before[0]);
    assert_eq!(f.bytes()[1..], before[1..]);
    assert_eq!(f.mutations(), ["stop"]);
    assert!(repair.continue_after_stop(&input).is_err());
    let resumed = f.repair().resume(&input).unwrap();
    assert_eq!(resumed.outcome, RepairOutcome::RecoveryRetained);
    assert_eq!(f.mutations(), ["stop"]);
    assert!(
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-intent.json")
            .exists()
    );
}

#[test]
fn failed_new_instance_recovery_keeps_every_backup_until_fresh_health_succeeds() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    let service = f.service(&package);
    let failed = f.reply(10, 2, |v| {
        v["result"]["installer"]["startup_recovery"] = json!("failed")
    });
    let input = repair_input(&proof, &package, &service, Some(&failed), &d);
    assert_eq!(
        repair.verify(&input).unwrap().outcome,
        RepairOutcome::RecoveryRetained
    );
    assert!(backups(&f).iter().all(|p| p.exists()));
    let healthy = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&healthy), &d);
    assert_eq!(
        repair.verify(&input).unwrap().outcome,
        RepairOutcome::Verified
    );
    assert!(backups(&f).iter().all(|p| !p.exists()));
}

#[test]
fn verify_failure_before_the_health_boundary_retains_every_backup() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    repair
        .scratch_payload_interrupt(Some(Interruption::BeforeJournal(false, 2)))
        .unwrap();
    let service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::RecoveryRetained);
    assert!(backups(&f).iter().all(|p| p.exists()));
}

#[test]
fn persisted_health_with_retirement_failure_is_cleanup_incomplete_and_resumable() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    repair
        .scratch_payload_interrupt(Some(Interruption::Retired(0)))
        .unwrap();
    let service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(
        result.outcome,
        RepairOutcome::HealthVerifiedCleanupIncomplete
    );
    assert!(!backups(&f)[0].exists());
    assert!(backups(&f)[1..].iter().all(|p| p.exists()));
    assert!(
        result
            .recovery_material
            .iter()
            .any(|m| m.path == backups(&f)[1] && m.presence == RecoveryPresence::Present)
    );
    assert_eq!(
        f.repair().resume(&input).unwrap().outcome,
        RepairOutcome::Verified
    );
    assert!(backups(&f).iter().all(|p| !p.exists()));
    assert_eq!(f.mutations(), ["stop", "start"]);
}

#[test]
fn cancellation_after_verify_reports_unknown_boundary_and_resume_required() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let token = Cancellation::default();
    let d = Deadline::new(5000, token.clone()).unwrap();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    repair
        .scratch_payload_hook(Some(Arc::new(move |at| {
            if at == Interruption::Retired(0) {
                token.cancel();
            }
            Ok(())
        })))
        .unwrap();
    let service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(result.outcome, RepairOutcome::OutcomeUnknownAfterVerify);
    assert!(
        result
            .recovery_material
            .iter()
            .all(|m| m.presence == RecoveryPresence::Unknown)
    );
    assert!(!backups(&f)[0].exists());
    assert!(backups(&f)[1..].iter().all(|p| p.exists()));
}

#[test]
fn repair_completion_write_failure_keeps_the_independently_verified_health_fact() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    let io = f.io.clone();
    repair
        .scratch_payload_hook(Some(Arc::new(move |at| {
            if at == Interruption::Retired(9) {
                // Change only our repair journal, never the frozen private payload record.
                let path = io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/repair-intent.json");
                let mut record: Value =
                    serde_json::from_slice(&io.read(&path, MAX_RECORD_BYTES, true).unwrap())
                        .unwrap();
                record["revision"] = json!(99);
                let proof = io.scratch_support(support(&io)).unwrap();
                io.atomic_write(&proof, &path, &serde_json::to_vec(&record).unwrap())
                    .unwrap();
            }
            Ok(())
        })))
        .unwrap();
    let service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let result = repair.verify(&input).unwrap();
    assert_eq!(
        result.outcome,
        RepairOutcome::HealthVerifiedCleanupIncomplete
    );
    assert!(result.error.is_some());
    assert!(backups(&f).iter().all(|p| !p.exists()));
    assert!(
        result
            .recovery_material
            .iter()
            .any(|m| m.path.ends_with("repair-intent.json"))
    );
}

#[test]
fn unknown_stop_is_recorded_and_is_never_blindly_retried() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let proof = f.proof();
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    let inventory = repair.inventory(&input).unwrap();
    let plan = repair.plan(inventory, 1, OperationId(2)).unwrap();
    let consent = plan.consent(1, OperationId(2), true).unwrap();
    let current = repair.inventory(&input).unwrap();
    let before = f.bytes();
    *f.runner.failure.lock().unwrap() = Some(NativeError::OutcomeUnknown);
    assert_eq!(
        repair
            .begin(plan, consent, &current, &input)
            .unwrap()
            .outcome,
        RepairOutcome::RecoveryRetained
    );
    assert!(repair.continue_after_stop(&input).is_err());
    assert_eq!(
        f.repair().resume(&input).unwrap().outcome,
        RepairOutcome::RecoveryRetained
    );
    assert_eq!(f.mutations(), ["stop"]);
    assert_eq!(f.bytes(), before);
}

#[test]
fn repair_preserves_identity_trust_config_and_unrelated_recovery_material() {
    let f = Fixture::new();
    let proof = f.proof();
    let paths = [
        f.io.target()
            .paths()
            .config_home
            .join("crosspane/config.toml"),
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/trust.json"),
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/revocations.json"),
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/parking-journal.json"),
    ];
    for path in &paths {
        f.io.create_private_dir(&proof, path.parent().unwrap())
            .unwrap();
        f.io.atomic_write(&proof, path, b"owned inert fixture state")
            .unwrap();
    }
    let package = fixture_package(2);
    let old = fixture_package(1);
    let service = f.service(&old);
    let reply = f.reply(9, 1, |_| {});
    let d = deadline();
    let mut repair = f.repair();
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    begin_clean_repair(&f, &mut repair, &input);
    repair.continue_after_stop(&input).unwrap();
    let service = f.service(&package);
    let reply = f.reply(10, 2, |_| {});
    let input = repair_input(&proof, &package, &service, Some(&reply), &d);
    assert_eq!(
        repair.verify(&input).unwrap().outcome,
        RepairOutcome::Verified
    );
    for path in paths {
        assert_eq!(fs::read(path).unwrap(), b"owned inert fixture state");
    }
    assert_eq!(f.mutations(), ["stop", "start"]);
}

struct FirewallReads;
impl crosspane_installer::platform::linux::firewall::FirewallReader for FirewallReads {
    fn file(&self, request: SystemRead, deadline: &Deadline) -> Result<Vec<u8>, NativeError> {
        deadline.check()?;
        match request {
            SystemRead::UfwConfig => Ok(b"ENABLED=yes\n".to_vec()),
            SystemRead::UfwRules | SystemRead::UfwRules6 => {
                Ok(b"*filter\n### RULES ###\n### END RULES ###\nCOMMIT\n".to_vec())
            }
            _ => panic!("unexpected fake firewall file"),
        }
    }
    fn command(
        &self,
        request: crosspane_installer::platform::linux::firewall::FirewallRead,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        use crosspane_installer::platform::linux::firewall::FirewallRead;
        deadline.check()?;
        let bytes: &[u8] = match request {
            FirewallRead::Activity => b"active\n",
            FirewallRead::Addresses => br#"[{"ifname":"fixture0","flags":["UP","LOWER_UP"],"link_type":"ether","addr_info":[{"family":"inet","scope":"global","local":"192.168.4.31","prefixlen":24}]}]"#,
            FirewallRead::Default4 => br#"[{"dst":"default","dev":"fixture0"}]"#,
            FirewallRead::Default6 => b"[]",
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout: bytes.to_vec(),
            stderr: Vec::new(),
        })
    }
}
fn firewall_request(operation: u64) -> crosspane_installer::platform::linux::firewall::PlanRequest {
    use crosspane_installer::platform::linux::firewall::{PlanRequest, RuleKind};
    PlanRequest {
        operation: OperationId(operation),
        revision: 1,
        kind: RuleKind::Lan,
        selected: None,
        ports: [47811, 47812],
    }
}

#[test]
fn firewall_has_separate_correlated_consent_and_current_support_authority() {
    use crosspane_installer::platform::linux::firewall::{
        LinuxFirewall, ManagerSelection, receipts::DurableIntentStore,
    };
    let f = Fixture::new();
    let other = Fixture::new();
    let proof = f.proof();
    let d = deadline();
    let repair = f.repair();
    let mut firewall = LinuxFirewall::scratch(f.io.clone(), Arc::new(FirewallReads)).unwrap();
    let mut store = DurableIntentStore::open(&mut firewall, &proof).unwrap();
    let snapshot = firewall.detect(ManagerSelection::Ufw, &d).unwrap();
    let plan = firewall.plan(&snapshot, firewall_request(50)).unwrap();
    assert!(plan.consent(OperationId(51), 1).is_err());
    assert!(plan.consent(OperationId(50), 2).is_err());
    let mut foreign = LinuxFirewall::scratch(f.io.clone(), Arc::new(FirewallReads)).unwrap();
    let snapshot = foreign.detect(ManagerSelection::Ufw, &d).unwrap();
    let foreign_plan = foreign.plan(&snapshot, firewall_request(50)).unwrap();
    let consent = foreign_plan.consent(OperationId(50), 1).unwrap();
    assert!(
        repair
            .apply_firewall(FirewallRepair {
                proof: &proof,
                firewall: &mut firewall,
                manager: ManagerSelection::Ufw,
                plan,
                consent,
                store: &mut store,
                deadline: &d
            })
            .is_err()
    );
    assert_eq!(f.auth_calls.load(Ordering::Relaxed), 0);
    let snapshot = firewall.detect(ManagerSelection::Ufw, &d).unwrap();
    let plan = firewall.plan(&snapshot, firewall_request(51)).unwrap();
    let consent = plan.consent(OperationId(51), 1).unwrap();
    let wrong = other.proof();
    assert!(
        repair
            .apply_firewall(FirewallRepair {
                proof: &wrong,
                firewall: &mut firewall,
                manager: ManagerSelection::Ufw,
                plan,
                consent,
                store: &mut store,
                deadline: &d
            })
            .is_err()
    );
    assert_eq!(f.auth_calls.load(Ordering::Relaxed), 0);
}

#[test]
fn firewall_unknown_outcome_remains_visible_without_automatic_retry() {
    use crosspane_installer::platform::linux::firewall::{
        FirewallError, LinuxFirewall, ManagerSelection, RuleResult, receipts::DurableIntentStore,
    };
    let f = Fixture::new();
    let proof = f.proof();
    let d = deadline();
    let repair = f.repair();
    let mut firewall = LinuxFirewall::scratch(f.io.clone(), Arc::new(FirewallReads)).unwrap();
    let mut store = DurableIntentStore::open(&mut firewall, &proof).unwrap();
    let snapshot = firewall.detect(ManagerSelection::Ufw, &d).unwrap();
    let plan = firewall.plan(&snapshot, firewall_request(50)).unwrap();
    let consent = plan.consent(OperationId(50), 1).unwrap();
    f.auth_code.store(1, Ordering::Relaxed);
    let result = repair
        .apply_firewall(FirewallRepair {
            proof: &proof,
            firewall: &mut firewall,
            manager: ManagerSelection::Ufw,
            plan,
            consent,
            store: &mut store,
            deadline: &d,
        })
        .unwrap();
    assert_eq!(result.result, RuleResult::OutcomeUnknown);
    assert_eq!(f.auth_calls.load(Ordering::Relaxed), 1);
    let snapshot = firewall.detect(ManagerSelection::Ufw, &d).unwrap();
    assert_eq!(
        firewall.plan(&snapshot, firewall_request(51)).unwrap_err(),
        FirewallError::CurrentRequired
    );
    assert_eq!(f.auth_calls.load(Ordering::Relaxed), 1);
}
