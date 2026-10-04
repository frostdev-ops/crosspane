#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
// The scratch fixture is the coordinator's own test scaffolding; not every helper is used here.
#![allow(dead_code)]
//! The Linux repair binding (`NativeRepairer`) over the real compatible-repair coordinator on a
//! scratch target, with the same fake service manager, clean-exit reader and keystore facts the
//! coordinator's own tests use. Nothing here touches the real user systemd, `~/.local`,
//! `~/.config/crosspane`, the session bus or the keyring.

use crosspane_installer::live::{Availability, RepairOutcome as Shown};
use crosspane_installer::platform::linux::integration::{
    NativeRepairer, RepairFinish, RepairStep, Repairer, Support, SupportOutcome,
};
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
            "/tmp/cp421r-{}-{}",
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

// ---- the binding over the scratch target ------------------------------------------------------

struct ScratchSupport(Arc<LinuxNativeIo>);
impl Support for ScratchSupport {
    fn detect(&self, _: Option<&Package>, _: &Deadline) -> SupportOutcome {
        SupportOutcome::Supported(self.0.scratch_support(support(&self.0)).unwrap())
    }
    fn source(&self) -> ObservationSource {
        ObservationSource::Demo
    }
}

fn repairer(f: &Fixture) -> NativeRepairer {
    let env = ChildEnvironment::selected(f.io.target(), BTreeMap::new()).unwrap();
    let mut repairer =
        NativeRepairer::new(f.io.clone(), env, Arc::new(ScratchSupport(f.io.clone())));
    repairer.scratch_exit_reader(f.probe.clone());
    repairer
}

const NOW: u64 = 100;

fn finished(step: RepairStep) -> RepairFinish {
    match step {
        RepairStep::Finished(finish) => finish,
        other => panic!("expected the repair to end, got {other:?}"),
    }
}

fn waiting(step: RepairStep) -> (String, RepairFinish) {
    match step {
        RepairStep::Waiting {
            detail,
            if_timed_out,
            ..
        } => (detail, if_timed_out),
        other => panic!("expected the repair to wait, got {other:?}"),
    }
}

#[test]
fn repair_is_offered_only_for_a_compatible_install_with_a_running_original() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let offer = repairer.inspect(Some(&package), NOW);
    assert_eq!(offer.repair, Availability::Available);
    assert_eq!(offer.resumable, None);
    assert!(f.mutations().is_empty(), "inspecting changes nothing");

    // Without the staged payload nothing can be judged.
    let offer = repairer.inspect(None, NOW);
    assert!(matches!(offer.repair, Availability::Unavailable(_)));

    // A file the person edited is not Crosspane's to replace: remove and reinstall.
    fs::write(&f.installer.targets()[2], b"owned fixture user edit").unwrap();
    let offer = repairer.inspect(Some(&package), NOW);
    let Availability::Unavailable(text) = offer.repair else {
        panic!("{offer:?}")
    };
    assert!(
        text.contains("Remove Crosspane and install it again"),
        "{text}"
    );
    assert!(f.mutations().is_empty());
    assert_eq!(
        fs::read(&f.installer.targets()[2]).unwrap(),
        b"owned fixture user edit",
        "the edit is still there, untouched"
    );
}

#[test]
fn repair_needs_the_original_agent_running_so_it_can_be_stopped_cleanly() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    *f.probe.0.lock().unwrap() = None;
    let offer = repairer.inspect(Some(&package), NOW);
    let Availability::Unavailable(text) = offer.repair else {
        panic!("{offer:?}")
    };
    assert!(text.contains("to be running"), "{text}");
    assert!(
        text.contains("remove crosspane and install it again"),
        "{text}"
    );
    let reply = f.reply(9, 1, |_| {});
    let err = repairer
        .plan(Some(&package), Some(&reply), OperationId(10), NOW)
        .unwrap_err();
    assert!(err.contains("to be running"), "{err}");
    assert!(f.mutations().is_empty());
}

#[test]
fn a_file_keystore_identity_is_guidance_and_never_a_repair() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |v| {
        v["result"]["installer"]["keystore"] = json!("file");
    });
    let before = f.bytes();
    let err = repairer
        .plan(Some(&package), Some(&reply), OperationId(10), NOW)
        .unwrap_err();
    assert!(err.contains("identity is kept in a file"), "{err}");
    assert!(
        err.contains("Remove Crosspane and install it again"),
        "{err}"
    );
    assert!(f.mutations().is_empty());
    assert_eq!(f.bytes(), before);
}

#[test]
fn the_preview_names_the_replacement_and_the_interruption_and_fits_the_view() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    let preview = repairer
        .plan(Some(&package), Some(&reply), OperationId(10), NOW)
        .unwrap();
    assert!(
        preview.contains("bin/crosspane-agent (different)"),
        "{preview}"
    );
    assert!(preview.contains("stopped, then started again"), "{preview}");
    assert!(preview.contains("nothing was in use"), "{preview}");
    assert!(preview.contains("identity, pairings"), "{preview}");
    assert!(preview.contains("No firewall change"), "{preview}");
    assert!(preview.len() <= 580, "{} bytes: {preview}", preview.len());
    // Without a Status the activity is said to be unknown, not idle.
    let blind = repairer
        .plan(Some(&package), None, OperationId(11), NOW)
        .unwrap();
    assert!(blind.contains("couldn't be checked"), "{blind}");
    assert!(f.mutations().is_empty());
}

#[test]
fn an_install_with_nothing_to_replace_has_nothing_to_repair() {
    let f = Fixture::new();
    let installed = fixture_package(1);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    let err = repairer
        .plan(Some(&installed), Some(&reply), OperationId(10), NOW)
        .unwrap_err();
    assert!(err.contains("Nothing needs repair"), "{err}");
    assert!(f.mutations().is_empty());
}

#[test]
fn a_confirmation_for_another_number_or_a_changed_install_changes_nothing() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    // No preview at all.
    assert!(
        repairer
            .confirm(
                Some(&package),
                Some(&reply),
                OperationId(10),
                OperationId(11),
                NOW
            )
            .is_err()
    );
    // The wrong number retires the preview.
    repairer
        .plan(Some(&package), Some(&reply), OperationId(20), NOW)
        .unwrap();
    assert!(
        repairer
            .confirm(
                Some(&package),
                Some(&reply),
                OperationId(21),
                OperationId(22),
                NOW
            )
            .is_err()
    );
    assert!(
        repairer
            .confirm(
                Some(&package),
                Some(&reply),
                OperationId(20),
                OperationId(23),
                NOW
            )
            .is_err(),
        "a retired preview can't be confirmed"
    );
    // The install changes between the preview and the confirmation: what was shown no longer
    // holds, so nothing starts.
    repairer
        .plan(Some(&package), Some(&reply), OperationId(30), NOW)
        .unwrap();
    fs::remove_file(&f.installer.targets()[1]).unwrap();
    let err = repairer
        .confirm(
            Some(&package),
            Some(&reply),
            OperationId(30),
            OperationId(31),
            NOW,
        )
        .unwrap_err();
    assert!(err.contains("Things changed since the preview"), "{err}");
    assert!(f.mutations().is_empty());
    assert!(!f.installer.targets()[1].exists(), "nothing was put back");
}

#[test]
fn confirm_runs_the_repair_up_to_the_new_agent_and_only_its_health_verifies_it() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    repairer
        .plan(Some(&package), Some(&reply), OperationId(20), NOW)
        .unwrap();
    f.stop_receipt(|_| {});
    f.start_instance(10);
    let before = f.bytes();
    let step = repairer
        .confirm(
            Some(&package),
            Some(&reply),
            OperationId(20),
            OperationId(21),
            NOW,
        )
        .unwrap();
    let (detail, if_timed_out) = waiting(step);
    assert!(detail.contains("Waiting for the new instance"), "{detail}");
    assert_eq!(
        if_timed_out.outcome,
        Shown::OutcomeUnknown,
        "a silent new agent is unknown, not failed"
    );
    assert!(if_timed_out.resumable);
    assert_eq!(f.mutations(), ["stop", "start"]);
    assert_ne!(f.bytes(), before, "the files were put back");

    // The old instance's Status is not health of the new one: still waiting, never verified.
    let old = f.reply(9, 1, |_| {});
    let (_, retained) = waiting(repairer.verify(Some(&package), Some(&old), NOW));
    assert_eq!(retained.outcome, Shown::RecoveryRetained);
    // No Status at all is not health either.
    waiting(repairer.verify(Some(&package), None, NOW));
    // The new instance's own Status is.
    let fresh = f.reply(10, 2, |_| {});
    let done = finished(repairer.verify(Some(&package), Some(&fresh), NOW));
    assert_eq!(done.outcome, Shown::Verified);
    assert!(!done.resumable);
    assert_eq!(
        done.lines
            .iter()
            .filter(|l| l.contains("were put back"))
            .collect::<Vec<_>>(),
        ["5 file(s) were put back."],
        "one count, of the confirmed delta: {:?}",
        done.lines
    );
    // The repair is over: nothing is left to look at.
    let after = finished(repairer.verify(Some(&package), Some(&fresh), NOW));
    assert_eq!(after.outcome, Shown::OutcomeUnknown);
}

#[test]
fn an_unproved_clean_exit_waits_with_every_file_untouched_and_continues_once_it_is_proved() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    repairer
        .plan(Some(&package), Some(&reply), OperationId(20), NOW)
        .unwrap();
    // The service stops, but the original leaves no clean receipt yet.
    let probe = f.probe.clone();
    *f.runner.stop.lock().unwrap() = Some(Box::new(move || {
        *probe.0.lock().unwrap() = None;
    }));
    f.start_instance(10);
    let before = f.bytes();
    let step = repairer
        .confirm(
            Some(&package),
            Some(&reply),
            OperationId(20),
            OperationId(21),
            NOW,
        )
        .unwrap();
    let (detail, if_timed_out) = waiting(step);
    assert!(detail.contains("exit cleanly"), "{detail}");
    assert_eq!(
        f.bytes(),
        before,
        "no file is replaced before the clean exit is proved"
    );
    assert_eq!(f.mutations(), ["stop"]);
    assert_eq!(if_timed_out.outcome, Shown::RecoveryRetained);
    assert!(
        if_timed_out
            .lines
            .iter()
            .any(|l| l.contains("after Crosspane was stopped, before any file was replaced")),
        "{:?}",
        if_timed_out.lines
    );
    assert!(
        if_timed_out
            .lines
            .iter()
            .any(|l| l.contains("may not be running")),
        "a stopped Crosspane is said to be stopped: {:?}",
        if_timed_out.lines
    );
    // The receipt arrives: the next look continues the same stage once, and starts the agent.
    let proof = f.proof();
    f.io.atomic_write(
        &proof,
        &f.io
            .target()
            .paths()
            .state_home
            .join("crosspane/last_exit.json"),
        &serde_json::to_vec(&json!({"schema_version":1,"instance_id":9,"stopped_unix_ms":parse_ps_start(START).unwrap()+1000,"clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true})).unwrap(),
    )
    .unwrap();
    let (detail, _) = waiting(repairer.verify(Some(&package), None, NOW));
    assert!(detail.contains("Waiting for the new instance"), "{detail}");
    assert_eq!(f.mutations(), ["stop", "start"]);
    let fresh = f.reply(10, 2, |_| {});
    let done = finished(repairer.verify(Some(&package), Some(&fresh), NOW));
    assert_eq!(done.outcome, Shown::Verified);
}

#[test]
fn an_interrupted_repair_is_found_after_the_window_closed_and_resumes_to_verified() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let reply = f.reply(9, 1, |_| {});
    {
        let mut first = repairer(&f);
        first
            .plan(Some(&package), Some(&reply), OperationId(20), NOW)
            .unwrap();
        f.stop_receipt(|_| {});
        f.start_instance(10);
        waiting(
            first
                .confirm(
                    Some(&package),
                    Some(&reply),
                    OperationId(20),
                    OperationId(21),
                    NOW,
                )
                .unwrap(),
        );
        // The window closes here: this adapter, and everything it remembers, is gone.
    }
    let mut reopened = repairer(&f);
    let offer = reopened.inspect(Some(&package), NOW);
    let Availability::Unavailable(text) = &offer.repair else {
        panic!("{offer:?}")
    };
    assert!(
        text.contains("Resume"),
        "no new repair over an unfinished one: {text}"
    );
    let lines = offer
        .resumable
        .expect("the interrupted repair is offered for resume");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("after Crosspane was started again, before it was seen healthy")),
        "{lines:?}"
    );
    // A new repair can't start over the unfinished one.
    let err = reopened
        .plan(
            Some(&package),
            Some(&f.reply(10, 2, |_| {})),
            OperationId(30),
            NOW,
        )
        .unwrap_err();
    assert!(err.contains("Nothing was changed"), "{err}");
    assert!(
        err.contains("didn't finish"),
        "the refusal names the unfinished repair, not foreign files: {err}"
    );
    // Even with the new instance healthy and its Status in hand, planning only looks: it never
    // completes, verifies or retires anything. Only the person's Resume does.
    let still = reopened.inspect(Some(&package), NOW);
    assert!(
        still.resumable.is_some(),
        "the repair is still unfinished after a refused plan"
    );
    // Resume without the new instance's health is honest about it and changes nothing.
    let before = f.bytes();
    let not_yet = reopened.resume(Some(&package), None, NOW).unwrap();
    assert_ne!(not_yet.outcome, Shown::Verified);
    assert!(not_yet.resumable);
    assert_eq!(f.bytes(), before);
    // With the new instance's Status the very same record is verified, nothing is replayed.
    let fresh = f.reply(10, 2, |_| {});
    let done = reopened.resume(Some(&package), Some(&fresh), NOW).unwrap();
    assert_eq!(done.outcome, Shown::Verified);
    assert_eq!(
        f.mutations(),
        ["stop", "start"],
        "resume never repeats a change"
    );
    // And afterwards repair is offered again, with nothing left to resume.
    let offer = reopened.inspect(Some(&package), NOW);
    assert_eq!(offer.resumable, None);
}

#[test]
fn resume_with_no_earlier_repair_is_a_refusal() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let err = repairer
        .resume(Some(&package), Some(&f.reply(9, 1, |_| {})), NOW)
        .unwrap_err();
    assert!(err.contains("no earlier repair"), "{err}");
    assert!(f.mutations().is_empty());
}

#[test]
fn a_new_agent_that_keeps_its_identity_in_a_file_is_tier_two_and_keeps_every_backup() {
    let f = Fixture::new();
    let package = fixture_package(2);
    let mut repairer = repairer(&f);
    let reply = f.reply(9, 1, |_| {});
    repairer
        .plan(Some(&package), Some(&reply), OperationId(20), NOW)
        .unwrap();
    f.stop_receipt(|_| {});
    f.start_instance(10);
    waiting(
        repairer
            .confirm(
                Some(&package),
                Some(&reply),
                OperationId(20),
                OperationId(21),
                NOW,
            )
            .unwrap(),
    );
    let proof = f.proof();
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut bootstrap: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bootstrap["keystore"] = json!("file");
    f.io.atomic_write(&proof, &path, &serde_json::to_vec(&bootstrap).unwrap())
        .unwrap();
    let health = f.reply(10, 2, |v| {
        v["result"]["installer"]["keystore"] = json!("file")
    });
    let done = finished(repairer.verify(Some(&package), Some(&health), NOW));
    assert_eq!(done.outcome, Shown::RecoveryRetained);
    assert!(
        done.lines
            .iter()
            .any(|l| l.contains("identity in a file instead of the keyring")),
        "{:?}",
        done.lines
    );
    assert!(
        done.lines.iter().any(|l| l.starts_with("Kept: ")),
        "{:?}",
        done.lines
    );
    assert!(done.resumable);
}

// WP-4.21r2: legacy v1 records carry no original-resource snapshot. Retirement instead requires
// a pre-apply stage AND the current installation to match its genuine completed receipt.
fn retirement_journal(f: &Fixture, stage: &str) -> PathBuf {
    let paths = f.io.target().paths();
    let path = paths
        .state_home
        .join("crosspane/installer/repair-intent.json");
    let journal = json!({"version":1,"operation":10,"revision":10,
        "manifest":sha256(&serde_json::to_vec(fixture_package(1).manifest()).unwrap()),
        "original_instance":9,"stage":stage,"target":{"uid":paths.uid,
        "roots":[paths.home,paths.prefix,paths.config_home,paths.state_home,
                 paths.data_home,paths.runtime_home],
        "runtime_override":paths.runtime_override,"scratch":true}});
    f.io.atomic_write(&f.proof(), &path, &serde_json::to_vec(&journal).unwrap())
        .unwrap();
    path
}
fn retirement_input<'a>(
    proof: &'a SupportProof,
    package: &'a Package,
    service: &'a LinuxService,
    reply: Option<&'a AgentReply>,
    d: &'a Deadline,
) -> RepairInput<'a> {
    RepairInput {
        proof,
        package,
        service,
        reply,
        expected_reply_id: 19,
        now_ms: NOW,
        deadline: d,
    }
}

#[test]
fn r2_retirement_is_allowed_only_in_the_three_pre_apply_journal_states() {
    for stage in [
        "recorded",
        "stop_pending",
        "stopped",
        "payload_pending",
        "payload_applied",
        "reload_pending",
        "start_pending",
        "awaiting_agent",
        "verified",
    ] {
        let f = Fixture::new();
        let path = retirement_journal(&f, stage);
        let before = fs::read(&path).unwrap();
        let installed = f.bytes();
        let package = fixture_package(1);
        let proof = f.proof();
        let service = f.service(&package);
        let reply = f.reply(9, 1, |_| {});
        let d = deadline();
        let input = retirement_input(&proof, &package, &service, Some(&reply), &d);
        let mut repair = f.repair();
        let permitted = matches!(stage, "recorded" | "stop_pending" | "stopped");
        let candidate = repair.inspect_retire_unapplied(&input);
        assert_eq!(candidate.is_ok(), permitted, "{stage}");
        if let Ok(candidate) = candidate {
            assert_eq!(
                repair.retire_unapplied(candidate, &input).unwrap(),
                RetirementOutcome::Retired
            );
            assert!(!path.exists());
        } else {
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert_eq!(f.bytes(), installed);
        assert!(f.mutations().is_empty());
        assert_eq!(f.auth_calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn r2_retirement_refuses_missing_changed_unreadable_foreign_and_pending_install_evidence() {
    for case in [
        "missing",
        "changed",
        "receipt",
        "pending",
        "unknown_stage",
        "foreign",
        "unreadable",
    ] {
        let f = Fixture::new();
        let path = retirement_journal(&f, "stopped");
        let state = f.io.target().paths().state_home.join("crosspane/installer");
        match case {
            "missing" => fs::remove_file(&f.installer.targets()[9]).unwrap(),
            "changed" => fs::write(&f.installer.targets()[9], b"user edit").unwrap(),
            "receipt" => fs::write(state.join("payload-outcome.json"), b"{}").unwrap(),
            "pending" => {
                f.io.atomic_write(&f.proof(), &state.join("payload-intent.json"), b"{}")
                    .unwrap()
            }
            "unknown_stage" | "foreign" => {
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                if case == "unknown_stage" {
                    value["stage"] = json!("future");
                } else {
                    value["target"]["uid"] = json!(f.io.target().paths().uid + 1);
                }
                fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "unreadable" => fs::write(&path, b"not a record").unwrap(),
            _ => unreachable!(),
        }
        let before = fs::read(&path).unwrap();
        let package = fixture_package(1);
        let proof = f.proof();
        let service = f.service(&package);
        let reply = f.reply(9, 1, |_| {});
        let d = deadline();
        let input = retirement_input(&proof, &package, &service, Some(&reply), &d);
        assert!(
            f.repair().inspect_retire_unapplied(&input).is_err(),
            "{case}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(f.mutations().is_empty());
    }
}

#[test]
fn r2_retirement_requires_fresh_supported_health_or_confirmed_inactive_absence() {
    for case in [
        "healthy",
        "absent",
        "stale",
        "wrong_id",
        "unhealthy",
        "inactive_alive",
        "no_status",
    ] {
        let f = Fixture::new();
        let path = retirement_journal(&f, "stopped");
        let package = fixture_package(1);
        let mut reply = f.reply(9, 1, |v| {
            if case == "unhealthy" {
                v["result"]["installer"]["backends"][0]["state"] = json!("failed");
            }
        });
        if case == "stale" {
            reply.observed_at_ms = 0;
        }
        if case == "wrong_id" {
            reply.id = 20;
        }
        if matches!(case, "absent" | "inactive_alive") {
            let mut p = f.runner.properties.lock().unwrap();
            p.insert("MainPID".into(), "0".into());
            p.insert("ActiveState".into(), "inactive".into());
            p.insert("SubState".into(), "dead".into());
            if case == "absent" {
                fs::remove_file(f.io.target().runtime_dir().join("bootstrap.json")).unwrap();
                *f.probe.0.lock().unwrap() = None;
            }
        }
        let proof = f.proof();
        let service = f.service(&package);
        let d = deadline();
        let mut input = retirement_input(
            &proof,
            &package,
            &service,
            (case != "no_status").then_some(&reply),
            &d,
        );
        if case == "stale" {
            input.now_ms = 5001;
        }
        let mut repair = f.repair();
        let candidate = repair.inspect_retire_unapplied(&input).unwrap();
        let result = repair.retire_unapplied(candidate, &input);
        let permitted = matches!(case, "healthy" | "absent");
        assert_eq!(result.is_ok(), permitted, "{case}: {result:?}");
        assert_eq!(path.exists(), !permitted);
        assert!(f.mutations().is_empty());
    }
}

#[test]
fn r2_retirement_rechecks_journal_and_resources_after_offer() {
    for case in [
        "record_replaced",
        "record_changed",
        "resource_changed",
        "expired",
    ] {
        let f = Fixture::new();
        let path = retirement_journal(&f, "stopped");
        let package = fixture_package(1);
        let proof = f.proof();
        let service = f.service(&package);
        let reply = f.reply(9, 1, |_| {});
        let cancel = Cancellation::default();
        let d = Deadline::new(5000, cancel.clone()).unwrap();
        let input = retirement_input(&proof, &package, &service, Some(&reply), &d);
        let mut repair = f.repair();
        let candidate = repair.inspect_retire_unapplied(&input).unwrap();
        match case {
            "record_replaced" => {
                let bytes = fs::read(&path).unwrap();
                fs::rename(&path, path.with_extension("saved")).unwrap();
                f.io.atomic_write(&proof, &path, &bytes).unwrap();
            }
            "record_changed" => {
                retirement_journal(&f, "payload_pending");
            }
            "resource_changed" => fs::write(&f.installer.targets()[9], b"later edit").unwrap(),
            "expired" => cancel.cancel(),
            _ => unreachable!(),
        }
        assert!(
            repair.retire_unapplied(candidate, &input).is_err(),
            "{case}"
        );
        assert!(path.exists());
        assert!(f.mutations().is_empty());
    }
}

#[test]
fn r2_native_binding_offers_discard_then_reoffers_repair_after_retirement() {
    let f = Fixture::new();
    let path = retirement_journal(&f, "stop_pending");
    let package = fixture_package(1);
    let mut binding = repairer(&f);
    let offer = binding.inspect(Some(&package), NOW);
    assert!(offer.discardable);
    assert!(offer.resumable.is_none());
    assert!(matches!(offer.repair, Availability::Unavailable(_)));
    let reply = f.reply(9, 1, |_| {});
    let finish = binding.discard(Some(&package), Some(&reply), NOW).unwrap();
    assert_eq!(finish.outcome, Shown::Retired);
    assert!(!finish.resumable);
    assert!(!path.exists());
    assert_eq!(
        binding.inspect(Some(&package), NOW).repair,
        Availability::Available
    );
    assert!(f.mutations().is_empty());
}
