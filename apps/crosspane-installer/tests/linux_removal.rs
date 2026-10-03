#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*, removal::*, service::*},
};
use crosspane_installer_core::OperationId;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
static ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
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
type FakeOutput = Result<(Option<i32>, Vec<u8>, Vec<u8>), NativeError>;
struct Runner {
    calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    output: Mutex<FakeOutput>,
    stall: Mutex<bool>,
    service_properties: Mutex<Option<BTreeMap<String, String>>>,
    service_cat: Mutex<Vec<u8>>,
    service_delay_ms: AtomicU64,
}
impl CommandRunner for Runner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        self.calls
            .lock()
            .unwrap()
            .push((c.executable().into(), c.argv().to_vec()));
        if c.executable() == Path::new("/bin/ps") {
            return Ok(CommandOutput {
                code: Some(0),
                stdout: if c.argv()[1] == "lstart=" {
                    START.to_vec()
                } else {
                    b"crosspane-agent\n".to_vec()
                },
                stderr: vec![],
            });
        }
        if c.executable() == Path::new("/usr/bin/systemctl") {
            thread::sleep(Duration::from_millis(
                self.service_delay_ms.swap(0, Ordering::AcqRel),
            ));
            if let Some(p) = self.service_properties.lock().unwrap().as_ref() {
                let (code, stdout) = match c.argv()[1].as_str() {
                    "show" => (
                        0,
                        p.iter()
                            .map(|(k, v)| format!("{k}={v}\n"))
                            .collect::<String>()
                            .into_bytes(),
                    ),
                    "cat" => (0, self.service_cat.lock().unwrap().clone()),
                    "is-active" => (
                        if p["ActiveState"] == "active" { 0 } else { 3 },
                        format!("{}\n", p["ActiveState"]).into_bytes(),
                    ),
                    "is-enabled" => (
                        if p["UnitFileState"] == "enabled" {
                            0
                        } else {
                            1
                        },
                        format!("{}\n", p["UnitFileState"]).into_bytes(),
                    ),
                    _ => return Err(NativeError::Unsupported),
                };
                return Ok(CommandOutput {
                    code: Some(code),
                    stdout,
                    stderr: vec![],
                });
            }
            // Deliberately unknown service facts; never pretend stop-zero or missing socket proves exit.
            return Ok(CommandOutput {
                code: Some(1),
                stdout: vec![],
                stderr: vec![],
            });
        }
        assert_eq!(c.argv(), ["erase-identity"]);
        while *self.stall.lock().unwrap() {
            d.check()?;
            thread::sleep(Duration::from_millis(1));
        }
        self.output
            .lock()
            .unwrap()
            .clone()
            .map(|(code, stdout, stderr)| CommandOutput {
                code,
                stdout,
                stderr,
            })
    }
}
struct Probe(
    Mutex<Result<Option<ProcessFacts>, NativeError>>,
    AtomicBool,
    AtomicBool,
    AtomicBool,
);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        self.2.store(true, Ordering::Release);
        while self.1.load(Ordering::Acquire) {
            // Deliberately ignore the deadline, including when called after expiry.
            thread::sleep(Duration::from_millis(1));
        }
        self.3.store(true, Ordering::Release);
        d.check()?;
        self.0
            .lock()
            .unwrap()
            .clone()?
            .ok_or(NativeError::Unavailable)
    }
}
impl ExitReader for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<Option<ProcessFacts>, NativeError> {
        d.check()?;
        self.0.lock().unwrap().clone()
    }
}
struct BlockingReader {
    inner: Arc<Probe>,
    block: AtomicBool,
    entered: AtomicBool,
    finished: AtomicBool,
}
impl BlockingReader {
    fn new(inner: Arc<Probe>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            block: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        })
    }
}
impl ExitReader for BlockingReader {
    fn snapshot(&self, pid: u32, d: &Deadline) -> Result<Option<ProcessFacts>, NativeError> {
        self.entered.store(true, Ordering::Release);
        while self.block.load(Ordering::Acquire) {
            // Intentionally noncooperative: the caller must not wait for this reader.
            thread::sleep(Duration::from_millis(1));
        }
        let result = ExitReader::snapshot(&*self.inner, pid, d);
        self.finished.store(true, Ordering::Release);
        result
    }
}
fn blocked_result<T: Send>(
    reader: &Arc<BlockingReader>,
    cancel: Option<&Cancellation>,
    call: impl FnOnce() -> T + Send,
) -> Option<T> {
    reader.entered.store(false, Ordering::Release);
    reader.finished.store(false, Ordering::Release);
    reader.block.store(true, Ordering::Release);
    thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let child = scope.spawn(move || {
            let _ = tx.send(call());
        });
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while !reader.entered.load(Ordering::Acquire) && std::time::Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        let entered = reader.entered.load(Ordering::Acquire);
        if let Some(cancel) = cancel {
            cancel.cancel();
        }
        let result = rx.recv_timeout(Duration::from_millis(400)).ok();
        reader.block.store(false, Ordering::Release);
        child.join().unwrap();
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while !reader.finished.load(Ordering::Acquire) && std::time::Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(entered && reader.finished.load(Ordering::Acquire));
        result
    })
}
fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn facts(io: &LinuxNativeIo) -> SupportObservations {
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
struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    probe: Arc<Probe>,
    runner: Arc<Runner>,
    proof: SupportProof,
    _listener: UnixListener,
}
impl Fixture {
    fn new(agent: bool) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp419-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe(
            Mutex::new(Ok(Some(ProcessFacts {
                uid: rustix::process::geteuid().as_raw(),
                executable: root.join(".local/bin/crosspane-agent"),
                generation: 77,
            }))),
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        ));
        let runner = Arc::new(Runner { calls: Mutex::default(), stall: Mutex::new(false), service_properties: Mutex::new(None), service_cat: Mutex::default(), service_delay_ms: AtomicU64::new(0),
            output: Mutex::new(Ok((Some(0),br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"}"#.to_vec(), vec![]))) });
        // Exclusive mkdir: collision is an error. No helper initializes a pre-existing root.
        let io = Arc::new(LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap());
        let proof = io.scratch_support(facts(&io)).unwrap();
        for path in [
            io.target().paths().prefix.join("bin"),
            io.target().runtime_dir().into(),
            io.target().paths().runtime_home.join("systemd"),
            io.target().paths().state_home.join("crosspane"),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        let listener =
            UnixListener::bind(io.target().paths().runtime_home.join("systemd/private")).unwrap();
        let fixture = Self {
            root,
            io,
            probe,
            runner,
            proof,
            _listener: listener,
        };
        if agent {
            fixture.executable(b"owned inert test agent");
            fixture.bootstrap(9);
        }
        fixture
    }
    fn executable(&self, bytes: &[u8]) {
        self.io.validate_target().unwrap();
        let parent = rustix::fs::open(
            self.io.target().agent_path().parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let fd = rustix::fs::openat(
            &parent,
            ".fixture-agent-new",
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        let mut file = fs::File::from(fd);
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_bits_truncate(0o755)).unwrap();
        rustix::fs::renameat(&parent, ".fixture-agent-new", &parent, "crosspane-agent").unwrap();
        rustix::fs::fsync(parent).unwrap();
    }
    fn bootstrap(&self, id: u64) {
        self.io
            .atomic_write(
                &self.proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &serde_json::to_vec(&json!({"schema_version":1,"instance_id":id,"pid":4242,
                "started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,
                "keystore":"os_store","reason":null,"runtime_dir":self.io.target().runtime_dir()}))
                .unwrap(),
            )
            .unwrap();
    }
    fn exit(&self, change: impl FnOnce(&mut Value)) {
        let mut value = json!({"schema_version":1,"instance_id":9,"stopped_unix_ms":parse_ps_start(START).unwrap()+1000,
            "clean":true,"parking":"restored","input_journals_empty":true,"audio_stopped":true});
        change(&mut value);
        self.io
            .atomic_write(
                &self.proof,
                &self
                    .io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/last_exit.json"),
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
    }
    fn tracked(&self) -> Arc<TrackedAgent> {
        Arc::new(
            TrackedAgent::scratch_capture(self.io.clone(), self.probe.clone(), &deadline())
                .unwrap(),
        )
    }
    fn planner(&self) -> RemovalPlanner {
        let mut planner = RemovalPlanner::new(self.io.clone());
        planner.scratch_exit_reader(self.probe.clone()).unwrap();
        planner
    }
    fn service(&self, package: &Package) -> LinuxService {
        LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            PayloadInstaller::new(self.io.clone())
                .unwrap()
                .rendered_resources(package)
                .unwrap(),
            &deadline(),
        )
        .unwrap()
    }
    fn inventory(&self, planner: &RemovalPlanner, package: &Package) -> Inventory {
        planner
            .inventory(
                &self.proof,
                package,
                &self.service(package),
                None,
                100,
                &deadline(),
            )
            .unwrap()
    }
    fn managed_service(&self, package: &Package, main_pid: u32) -> LinuxService {
        let resources = PayloadInstaller::new(self.io.clone())
            .unwrap()
            .rendered_resources(package)
            .unwrap();
        for record in &resources {
            self.io
                .create_private_dir(&self.proof, record.target.parent().unwrap())
                .unwrap();
            self.io
                .atomic_write(&self.proof, &record.target, &record.bytes)
                .unwrap();
        }
        let unit = &resources[0];
        let mut cat = format!("# {}\n", unit.target.display()).into_bytes();
        cat.extend(&unit.bytes);
        *self.runner.service_cat.lock().unwrap() = cat;
        let mut p: BTreeMap<String, String> = MANAGER_PROPERTIES
            .split(',')
            .map(|k| (k.into(), "".into()))
            .collect();
        for (k, v) in [
            ("Id", UNIT),
            ("LoadState", "loaded"),
            ("DynamicUser", "no"),
            ("ActiveState", "inactive"),
            ("SubState", "dead"),
            ("UnitFileState", "disabled"),
            ("MainPID", "0"),
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
        p.insert(
            "FragmentPath".into(),
            unit.target.to_string_lossy().into_owned(),
        );
        let exe = self.io.target().agent_path().to_string_lossy().into_owned();
        p.insert("ExecStart".into(),format!("{{ path={exe} ; argv[]={exe} run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert("ExecStartEx".into(),format!("{{ path={exe} ; argv[]={exe} run ; flags= ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
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
            .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" "),
        );
        p.insert("ActiveState".into(), "active".into());
        p.insert("SubState".into(), "running".into());
        p.insert("MainPID".into(), main_pid.to_string());
        *self.runner.service_properties.lock().unwrap() = Some(p);
        self.service(package)
    }
    fn request<'a>(
        &'a self,
        package: &'a Package,
        service: &'a LinuxService,
        deadline: &'a Deadline,
    ) -> InventoryRequest<'a> {
        InventoryRequest {
            proof: &self.proof,
            package,
            service,
            reply: None,
            now_ms: 100,
            deadline,
        }
    }
    fn environment(&self) -> ChildEnvironment {
        ChildEnvironment::selected(self.io.target(), BTreeMap::new()).unwrap()
    }
    fn erase_count(&self) -> usize {
        self.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, a)| a == &["erase-identity"])
            .count()
    }
    fn reply(&self) -> AgentReply {
        self.reply_with(|_| {})
    }
    fn reply_with(&self, change: impl FnOnce(&mut Value)) -> AgentReply {
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let i = &mut v["result"]["installer"]["instance"];
        i["uid"] = json!(self.io.target().paths().uid);
        i["exe"] = json!(self.io.target().agent_path());
        i["runtime_dir"] = json!(self.io.target().runtime_dir());
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
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn package() -> Package {
    let mut elf = vec![0; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    elf[18..20].copy_from_slice(
        &(if Architecture::native().unwrap() == Architecture::X86_64 {
            62u16
        } else {
            183u16
        })
        .to_le_bytes(),
    );
    elf[20] = 1;
    elf[52] = 64;
    let data: Vec<Vec<u8>> = (0..10)
        .map(|i| match i {
            0..=4 => elf.clone(),
            5 => include_bytes!("../../../packaging/linux/crosspane-agent.service").to_vec(),
            6 => include_bytes!("../../../packaging/linux/crosspane-settings.desktop").to_vec(),
            7 => include_bytes!("../../../packaging/linux/crosspane-installer.desktop").to_vec(),
            _ => b"inert resource".to_vec(),
        })
        .collect();
    let hex = |b: &[u8]| {
        sha256(b)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>()
    };
    let manifest = Manifest {
        schema_version: 1,
        product_version: "0.0.0".into(),
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
            .map(|(i, (name, b))| Artifact {
                name: (*name).into(),
                size: b.len(),
                sha256: hex(b),
                features: if i == 0 { vec!["video".into()] } else { vec![] },
            })
            .collect(),
    };
    fn member(name: &str, b: &[u8]) -> Vec<u8> {
        let mut h = vec![0; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        for (at, width, n) in [
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
            (124, 12, b.len()),
            (136, 12, 0),
        ] {
            h[at..at + width]
                .copy_from_slice(format!("{n:0width$o}\0", width = width - 1).as_bytes());
        }
        h[156] = b'0';
        h[257..265].copy_from_slice(b"ustar\x0000");
        h[148..156].fill(b' ');
        let sum: usize = h.iter().map(|b| usize::from(*b)).sum();
        h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
        h.extend(b);
        h.resize(h.len().div_ceil(512) * 512, 0);
        h
    }
    let mut archive = member("manifest.json", &serde_json::to_vec(&manifest).unwrap());
    for (name, b) in FILES.iter().zip(&data) {
        archive.extend(member(name, b));
    }
    archive.extend(vec![0; 1024]);
    Package::read(
        archive.as_slice(),
        Architecture::native().unwrap(),
        sha256(&archive),
    )
    .unwrap()
}

#[test]
fn clean_requires_original_exit_and_literal_receipt_not_stop_zero_or_socket_absence() {
    let f = Fixture::new(true);
    let tracked = f.tracked();
    f.exit(|_| {});
    assert!(
        f.io.metadata(&f.io.target().socket_path())
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        tracked.clean_authority(&deadline()),
        Err(RemovalError::NotClean)
    ));
    *f.probe.0.lock().unwrap() = Err(NativeError::Unavailable);
    assert!(matches!(
        tracked.clean_authority(&deadline()),
        Err(RemovalError::NotClean)
    ));
    *f.probe.0.lock().unwrap() = Ok(None);
    let clean = tracked.clean_authority(&deadline()).unwrap();
    assert_eq!(clean.receipt().instance_id, 9);
    clean.revalidate(&deadline()).unwrap();
    assert_eq!(f.erase_count(), 0);
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
}

#[test]
fn noncooperative_capture_returns_timeout_and_keeps_reader_until_completion() {
    let f = Fixture::new(true);
    let original = f.io.bootstrap(&deadline()).unwrap().1;
    let reader = BlockingReader::new(f.probe.clone());
    let result = blocked_result(&reader, None, || {
        f.io.scratch_track_process(
            &original,
            reader.clone(),
            &Deadline::new(30, Cancellation::default()).unwrap(),
        )
    });
    assert_eq!(
        result
            .expect("capture must return while reader is still blocked")
            .unwrap_err(),
        NativeError::Timeout
    );
}

#[test]
fn noncooperative_observation_returns_cancellation_without_releasing_reader_early() {
    let f = Fixture::new(true);
    let original = f.io.bootstrap(&deadline()).unwrap().1;
    let reader = BlockingReader::new(f.probe.clone());
    let watch =
        f.io.scratch_track_process(&original, reader.clone(), &deadline())
            .unwrap();
    let cancel = Cancellation::default();
    let d = Deadline::new(5000, cancel.clone()).unwrap();
    let result = blocked_result(&reader, Some(&cancel), || watch.observe(&f.io, &d));
    assert_eq!(
        result
            .expect("observe must return while reader is still blocked")
            .unwrap_err(),
        NativeError::Cancelled
    );
}

#[test]
fn noncooperative_exit_cannot_block_or_grant_clean_authority() {
    let f = Fixture::new(true);
    let reader = BlockingReader::new(f.probe.clone());
    let tracked =
        Arc::new(TrackedAgent::scratch_capture(f.io.clone(), reader.clone(), &deadline()).unwrap());
    f.exit(|_| {});
    *f.probe.0.lock().unwrap() = Ok(None);
    let result = blocked_result(&reader, None, || {
        tracked.clean_authority(&Deadline::new(30, Cancellation::default()).unwrap())
    });
    assert!(
        result
            .expect("clean check must return while reader is still blocked")
            .is_err()
    );
    assert_eq!(f.erase_count(), 0);
}
#[test]
fn full_range_instance_and_each_clean_parking_outcome_require_ready_original() {
    for parking in ["restored", "nothing_parked", "none"] {
        let f = Fixture::new(true);
        f.bootstrap(u64::MAX);
        let tracked = f.tracked();
        *f.probe.0.lock().unwrap() = Ok(None);
        f.exit(|value| {
            value["instance_id"] = json!(u64::MAX);
            value["parking"] = json!(parking);
        });
        assert_eq!(
            tracked
                .clean_authority(&deadline())
                .unwrap()
                .receipt()
                .instance_id,
            u64::MAX
        );
    }
    for (phase, reason) in [
        ("starting", None),
        ("waiting_for_keystore", None),
        ("failed", Some("keystore")),
    ] {
        let f = Fixture::new(true);
        let tracked = f.tracked();
        *f.probe.0.lock().unwrap() = Ok(None);
        f.exit(|_| {});
        let path = f.io.target().runtime_dir().join("bootstrap.json");
        let mut value: Value =
            serde_json::from_slice(&f.io.read(&path, 4096, true).unwrap()).unwrap();
        value["phase"] = json!(phase);
        value["keystore"] = Value::Null;
        value["reason"] = json!(reason);
        parse_bootstrap(&serde_json::to_vec(&value).unwrap()).unwrap();
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(tracked.clean_authority(&deadline()).is_err());
    }
}
#[test]
fn missing_stale_wrong_instance_and_each_unclean_or_contradictory_exit_are_rejected() {
    let cases = [
        json!({"instance_id":8}),
        json!({"stopped_unix_ms":0}),
        json!({"clean":false,"parking":"failed"}),
        json!({"clean":false,"input_journals_empty":false}),
        json!({"clean":false,"audio_stopped":false}),
        json!({"parking":"failed"}),
        json!({"input_journals_empty":false}),
        json!({"audio_stopped":false}),
        json!({"schema_version":2}),
        json!({"clean":"true"}),
        json!({"parking":"unknown"}),
    ];
    for change in cases {
        let f = Fixture::new(true);
        let tracked = f.tracked();
        *f.probe.0.lock().unwrap() = Ok(None);
        assert!(tracked.clean_authority(&deadline()).is_err());
        f.exit(|value| {
            for (k, v) in change.as_object().unwrap() {
                value[k] = v.clone();
            }
        });
        assert!(tracked.clean_authority(&deadline()).is_err(), "{change}");
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
        assert_eq!(f.erase_count(), 0);
    }
}
#[test]
fn current_instance_pid_reuse_changed_uid_and_changed_executable_never_become_clean() {
    for field in ["generation", "uid", "exe"] {
        let f = Fixture::new(true);
        let tracked = f.tracked();
        f.exit(|_| {});
        let mut facts = f.probe.0.lock().unwrap().clone().unwrap().unwrap();
        match field {
            "generation" => facts.generation += 1,
            "uid" => facts.uid += 1,
            _ => facts.executable = f.root.join("unrelated"),
        }
        *f.probe.0.lock().unwrap() = Ok(Some(facts));
        assert!(matches!(
            tracked.clean_authority(&deadline()),
            Err(RemovalError::NotClean)
        ));
    }
    let f = Fixture::new(true);
    let tracked = f.tracked();
    f.exit(|_| {});
    *f.probe.0.lock().unwrap() = Ok(None);
    f.bootstrap(10);
    assert!(matches!(
        tracked.clean_authority(&deadline()),
        Err(RemovalError::NotClean)
    ));
}
#[test]
fn no_original_can_be_reconstructed_after_exit_and_foreign_target_watch_is_refused() {
    let f = Fixture::new(true);
    let original = f.io.bootstrap(&deadline()).unwrap().1;
    let watch =
        f.io.scratch_track_process(&original, f.probe.clone(), &deadline())
            .unwrap();
    let foreign = Fixture::new(true);
    assert_eq!(
        watch.observe(&foreign.io, &deadline()).unwrap_err(),
        NativeError::Foreign
    );
    *f.probe.0.lock().unwrap() = Ok(None);
    f.exit(|_| {});
    assert!(TrackedAgent::scratch_capture(f.io.clone(), f.probe.clone(), &deadline()).is_err());
    assert!(
        f.io.scratch_track_process(&original, f.probe.clone(), &deadline())
            .is_err()
    );
}
#[test]
fn erase_command_is_exact_selected_digest_admitted_and_cannot_run_as_read_only() {
    let f = Fixture::new(true);
    let spec = CommandSpec::erase_identity(
        &f.io,
        sha256(b"owned inert test agent"),
        f.environment(),
        &deadline(),
    )
    .unwrap();
    assert_eq!(spec.executable(), f.io.target().agent_path());
    assert_eq!(spec.argv(), ["erase-identity"]);
    assert_eq!(spec.output_limit(), 4096);
    for (key, path) in [
        ("HOME", f.io.target().paths().home.clone()),
        ("XDG_CONFIG_HOME", f.io.target().paths().config_home.clone()),
        ("XDG_STATE_HOME", f.io.target().paths().state_home.clone()),
        (
            "CROSSPANE_RUNTIME_DIR",
            f.io.target().runtime_dir().to_path_buf(),
        ),
    ] {
        assert_eq!(spec.environment().values()[key], path.to_string_lossy());
    }
    assert_eq!(
        f.io.run(&spec, &deadline()).unwrap_err(),
        NativeError::Unsupported
    );
    let output = f.io.run_mutation(&f.proof, &spec, &deadline()).unwrap();
    assert!(
        admit_erase_output(&output)
            .unwrap()
            .identity_and_pairings_removed()
    );
    assert_eq!(f.erase_count(), 1);
    for argv in [
        vec!["erase-identity"],
        vec!["erase-identity", "--keep-trust"],
        vec!["run"],
    ] {
        assert!(
            CommandSpec::new(
                f.io.target().agent_path(),
                argv.into_iter().map(String::from).collect(),
                f.environment(),
                4096
            )
            .is_err()
        );
    }
}
#[test]
fn erase_rechecks_digest_inode_mode_links_and_target_before_any_fake_dispatch() {
    let f = Fixture::new(true);
    let digest = sha256(b"owned inert test agent");
    assert!(CommandSpec::erase_identity(&f.io, [0; 32], f.environment(), &deadline()).is_err());
    let spec = CommandSpec::erase_identity(&f.io, digest, f.environment(), &deadline()).unwrap();
    f.executable(b"changed");
    assert_eq!(
        f.io.run_mutation(&f.proof, &spec, &deadline()).unwrap_err(),
        NativeError::Foreign
    );
    f.executable(b"owned inert test agent");
    assert_eq!(
        f.io.run_mutation(&f.proof, &spec, &deadline()).unwrap_err(),
        NativeError::Foreign
    );
    fs::set_permissions(
        f.io.target().agent_path(),
        fs::Permissions::from_mode(0o775),
    )
    .unwrap();
    assert!(CommandSpec::erase_identity(&f.io, digest, f.environment(), &deadline()).is_err());
    fs::set_permissions(
        f.io.target().agent_path(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    f.executable(b"owned inert test agent");
    fs::hard_link(f.io.target().agent_path(), f.root.join("second-link")).unwrap();
    assert!(CommandSpec::erase_identity(&f.io, digest, f.environment(), &deadline()).is_err());
    assert_eq!(f.erase_count(), 0);
    let other = Fixture::new(true);
    assert!(CommandSpec::erase_identity(&f.io, digest, other.environment(), &deadline()).is_err());
}
#[test]
fn erase_cancellation_and_timeout_are_unknown_once_dispatched_never_retried() {
    let f = Fixture::new(true);
    let spec = CommandSpec::erase_identity(
        &f.io,
        sha256(b"owned inert test agent"),
        f.environment(),
        &deadline(),
    )
    .unwrap();
    let cancellation = Cancellation::default();
    cancellation.cancel();
    assert_eq!(
        f.io.run_mutation(&f.proof, &spec, &Deadline::new(50, cancellation).unwrap())
            .unwrap_err(),
        NativeError::Cancelled
    );
    assert_eq!(f.erase_count(), 0);
    *f.runner.output.lock().unwrap() = Err(NativeError::Timeout);
    assert_eq!(
        f.io.run_mutation(&f.proof, &spec, &deadline()).unwrap_err(),
        NativeError::OutcomeUnknown
    );
    assert_eq!(f.erase_count(), 1);
    let f = Fixture::new(true);
    let spec = CommandSpec::erase_identity(
        &f.io,
        sha256(b"owned inert test agent"),
        f.environment(),
        &deadline(),
    )
    .unwrap();
    *f.runner.stall.lock().unwrap() = true;
    let cancellation = Cancellation::default();
    let d = Deadline::new(5000, cancellation.clone()).unwrap();
    thread::scope(|scope| {
        let worker = scope.spawn(|| f.io.run_mutation(&f.proof, &spec, &d));
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while f.erase_count() == 0 && std::time::Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(f.erase_count(), 1);
        cancellation.cancel();
        assert_eq!(
            worker.join().unwrap().unwrap_err(),
            NativeError::OutcomeUnknown
        );
    });
    *f.runner.stall.lock().unwrap() = false;
    assert_eq!(f.erase_count(), 1);
}
#[test]
fn erased_receipt_semantics_preserve_keep_trust_refusal_waiting_and_partial_failure() {
    for (result, reason, key, trust, complete) in [
        ("removed", None, "removed", "removed", true),
        ("removed", None, "absent", "removed", true), // producer may have removed only trust/revocations
        ("removed", None, "removed", "absent", true),
        ("already_absent", None, "absent", "absent", true),
        ("removed", None, "removed", "kept", false),
        ("refused", Some("agent_running"), "kept", "kept", false),
        ("refused", Some("no_exit_receipt"), "kept", "kept", false),
        ("refused", Some("unclean_exit"), "kept", "kept", false),
        ("waiting", Some("keystore_locked"), "kept", "kept", false),
        ("failed", Some("keystore_error"), "failed", "kept", false),
        ("failed", Some("io"), "removed", "failed", false),
    ] {
        let output = CommandOutput {
            code: Some(0),
            stdout: serde_json::to_vec(&json!({
            "schema_version":1,"result":result,"reason":reason,"key":key,"trust":trust }))
            .unwrap(),
            stderr: vec![],
        };
        let receipt = admit_erase_output(&output).unwrap();
        assert_eq!(receipt.identity_and_pairings_removed(), complete);
        assert_eq!(
            serde_json::to_value(receipt).unwrap(),
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        );
    }
    for bytes in [b"{}".as_slice(),br#"{"schema_version":1,"result":"already_absent","reason":null,"key":"absent","trust":"removed"}"#,
        br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"} {}"#] {
        assert!(admit_erase_output(&CommandOutput{code:Some(0),stdout:bytes.to_vec(),stderr:vec![]}).is_err());
    }
    assert!(
        admit_erase_output(&CommandOutput {
            code: Some(1),
            stdout: vec![],
            stderr: vec![]
        })
        .is_err()
    );
}
#[test]
fn uncorrelated_plan_retains_identity_recovery_and_default_keep_is_explicit() {
    let f = Fixture::new(false);
    let p = package();
    let mut planner = f.planner();
    let inventory = f.inventory(&planner, &p);
    let plan = planner
        .plan(
            inventory,
            1,
            OperationId(1),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(
        plan.cleanup_form(),
        CleanupForm::NotCleanRetainIdentityAndRecovery
    );
    assert_eq!(plan.selection().identity, IdentityChoice::Keep);
    assert!(plan.preview().contains("revocations"));
    assert!(plan.consent(1, OperationId(1), false).is_err());
    let _consent = plan.consent(1, OperationId(1), true).unwrap();
    assert_eq!(f.erase_count(), 0);
    let deletion = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Uninstall,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                lan_rule: true,
                mdns_rule: true,
            },
        )
        .unwrap();
    assert_eq!(
        deletion.cleanup_form(),
        CleanupForm::NotCleanRetainIdentityAndRecovery
    );
    assert!(deletion.tracked().is_none());
    assert!(deletion.preview().contains("remote offline trust"));
    assert!(deletion.preview().contains("every Crosspane user"));
}
#[test]
fn coherent_old_consent_changed_preview_foreign_controller_and_decreasing_ids_are_stale() {
    let f = Fixture::new(false);
    let p = package();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let current = f.inventory(&planner, &p);
    planner
        .validate(
            &plan,
            &consent,
            &current,
            f.request(&p, &f.service(&p), &deadline()),
        )
        .unwrap();
    assert!(plan.consent(2, OperationId(1), true).is_err());
    let second = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &current,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    let foreign = f.planner();
    assert_eq!(
        foreign
            .validate(
                &second,
                &second.consent(2, OperationId(2), true).unwrap(),
                &current,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert!(
        planner
            .plan(
                f.inventory(&planner, &p),
                2,
                OperationId(3),
                PlanKind::Uninstall,
                RemovalSelection::default()
            )
            .is_err()
    );
    assert!(
        planner
            .plan(
                f.inventory(&planner, &p),
                3,
                OperationId(2),
                PlanKind::Uninstall,
                RemovalSelection::default()
            )
            .is_err()
    );
}
#[test]
fn activity_keeps_receipt_time_and_rejects_stale_future_wrong_source_or_instance() {
    let f = Fixture::new(true);
    let p = package();
    let planner = f.planner();
    let service = f.service(&p);
    let reply = f.reply();
    let inventory = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 101, &deadline())
        .unwrap();
    let activity = inventory.facts().activity.as_ref().unwrap();
    assert_eq!(activity.observed_at_ms, 100);
    assert_eq!(activity.source, ObservationSource::Demo);
    assert_eq!(activity.input, None);
    assert_eq!(activity.audio, None);
    assert_eq!(activity.projections, None);
    for now in [99, 5101] {
        assert!(
            planner
                .inventory(&f.proof, &p, &service, Some(&reply), now, &deadline())
                .unwrap()
                .facts()
                .activity
                .is_none()
        );
    }
    let mut wrong = reply.clone();
    wrong.source = ObservationSource::Live;
    assert!(
        planner
            .inventory(&f.proof, &p, &service, Some(&wrong), 101, &deadline())
            .unwrap()
            .facts()
            .activity
            .is_none()
    );
    f.bootstrap(10);
    let wrong_instance = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 101, &deadline())
        .unwrap();
    assert_eq!(wrong_instance.facts().instance_id, Some(10));
    assert!(wrong_instance.facts().activity.is_none());
}

#[test]
fn foreign_selected_service_fragment_and_source_cannot_bind_original_agent() {
    let f = Fixture::new(true);
    let foreign = Fixture::new(true);
    let p = package();
    let service = foreign.managed_service(&p, 4242);
    let facts = service.observe(&deadline()).unwrap();
    assert_eq!(
        facts.fragment,
        foreign
            .io
            .target()
            .paths()
            .config_home
            .join("systemd/user/crosspane-agent.service")
    );
    assert_eq!(facts.source, foreign.io.target().source());
    let planner = f.planner();
    let inventory = planner
        .inventory(&f.proof, &p, &service, None, 100, &deadline())
        .unwrap();
    assert_eq!(inventory.facts().service, Err(ServiceError::Foreign));
    assert!(inventory.tracked().is_none());
}

#[test]
fn selected_service_main_pid_mismatch_stays_pending_without_clean_tracking() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4243);
    assert_eq!(service.observe(&deadline()).unwrap().main_pid, 4243);
    let planner = f.planner();
    let inventory = planner
        .inventory(&f.proof, &p, &service, None, 100, &deadline())
        .unwrap();
    assert_eq!(inventory.facts().service, Err(ServiceError::Foreign));
    assert!(inventory.tracked().is_none());
}

#[test]
fn matching_selected_service_and_original_pid_are_correlated_without_readiness_claims() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    let planner = f.planner();
    let inventory = planner
        .inventory(&f.proof, &p, &service, None, 100, &deadline())
        .unwrap();
    assert_eq!(
        inventory.facts().service.as_ref().unwrap().main_pid,
        inventory.facts().original.as_ref().unwrap().pid
    );
    assert!(inventory.tracked().is_some());
    assert!(inventory.facts().activity.is_none());
}

#[test]
fn cached_activity_expiry_rejects_consent_before_any_fresh_detection() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.service(&p);
    let reply = f.reply();
    let mut planner = f.planner();
    let inventory = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 100, &deadline())
        .unwrap();
    let plan = planner
        .plan(
            inventory,
            1,
            OperationId(1),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 100, &deadline())
        .unwrap();
    let d = deadline();
    let mut input = f.request(&p, &service, &d);
    input.reply = Some(&reply);
    input.now_ms = 5101;
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner
            .validate(&plan, &consent, &cached, input)
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
}

#[test]
fn unknown_service_observation_never_exposes_clean_capable_tracking() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    *f.runner.service_properties.lock().unwrap() = None;
    assert_eq!(service.observe(&deadline()), Err(ServiceError::Unknown));
    let inventory = f
        .planner()
        .inventory(&f.proof, &p, &service, None, 100, &deadline())
        .unwrap();
    assert_eq!(inventory.facts().original.as_ref().unwrap().pid, 4242);
    assert!(inventory.tracked().is_none());
    assert_eq!(
        inventory.facts().correlation,
        Err(RemovalError::Service(ServiceError::Unknown))
    );
}

#[test]
fn nested_native_foreign_service_observation_never_exposes_clean_tracking() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    let unit = PayloadInstaller::new(f.io.clone())
        .unwrap()
        .rendered_resources(&p)
        .unwrap()[0]
        .target
        .clone();
    fs::set_permissions(unit, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        service.observe(&deadline()),
        Err(ServiceError::Native(NativeError::Foreign))
    );
    let inventory = f
        .planner()
        .inventory(&f.proof, &p, &service, None, 100, &deadline())
        .unwrap();
    assert!(inventory.tracked().is_none());
    assert_eq!(
        inventory.facts().correlation,
        Err(RemovalError::Service(ServiceError::Native(
            NativeError::Foreign
        )))
    );
}

#[test]
fn changed_detection_permanently_retires_consent_and_superseded_activity() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    let a = f.reply();
    let mut b = f.reply_with(|v| {
        v["result"]["installer"]["recovery_pending"] = json!(1);
        v["result"]["installer"]["epochs"] = json!({"gate":2,"grants":2,"layout":2,"backends":2});
    });
    b.observed_at_ms = 101;
    let mut planner = f.planner();
    let initial = planner
        .inventory(&f.proof, &p, &service, Some(&a), 100, &deadline())
        .unwrap();
    let plan = planner
        .plan(
            initial,
            1,
            OperationId(1),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let changed = planner
        .inventory(&f.proof, &p, &service, Some(&b), 101, &deadline())
        .unwrap();
    assert_eq!(
        changed.facts().activity.as_ref().unwrap().projections,
        Some(true)
    );
    // Reconstruct a NEW inventory from cached A, not merely reuse the retired original inventory.
    let cached_a = planner
        .inventory(&f.proof, &p, &service, Some(&a), 101, &deadline())
        .unwrap();
    let before = f.runner.calls.lock().unwrap().len();
    let d = deadline();
    let mut input = f.request(&p, &service, &d);
    input.reply = Some(&a);
    input.now_ms = 101;
    assert_eq!(
        planner.validate(&plan, &consent, &cached_a, input),
        Err(RemovalError::Stale)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    assert!(cached_a.facts().activity.is_none());
    // A newer receipt does not legitimize epochs which were already superseded.
    let mut decreasing = a.clone();
    decreasing.observed_at_ms = 102;
    assert!(
        planner
            .inventory(&f.proof, &p, &service, Some(&decreasing), 102, &deadline())
            .unwrap()
            .facts()
            .activity
            .is_none()
    );
    for epoch in ["gate", "grants", "layout", "backends"] {
        let mut one_decreasing = f.reply_with(|v| {
            v["result"]["installer"]["recovery_pending"] = json!(1);
            v["result"]["installer"]["epochs"] =
                json!({"gate":2,"grants":2,"layout":2,"backends":2});
            v["result"]["installer"]["epochs"][epoch] = json!(1);
        });
        one_decreasing.observed_at_ms = 102;
        assert!(
            planner
                .inventory(
                    &f.proof,
                    &p,
                    &service,
                    Some(&one_decreasing),
                    102,
                    &deadline()
                )
                .unwrap()
                .facts()
                .activity
                .is_none(),
            "{epoch}"
        );
    }
    let mut same_receipt_changed = f.reply_with(|v| {
        v["result"]["installer"]["epochs"] = json!({"gate":2,"grants":2,"layout":2,"backends":2});
    });
    same_receipt_changed.observed_at_ms = 101;
    assert!(
        planner
            .inventory(
                &f.proof,
                &p,
                &service,
                Some(&same_receipt_changed),
                102,
                &deadline()
            )
            .unwrap()
            .facts()
            .activity
            .is_none()
    );
}

#[test]
fn noncooperative_process_probe_with_status_cannot_bypass_capture_deadline() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    let reply = f.reply();
    let planner = f.planner();
    f.probe.2.store(false, Ordering::Release);
    f.probe.3.store(false, Ordering::Release);
    f.probe.1.store(true, Ordering::Release);
    let d = Deadline::new(35, Cancellation::default()).unwrap();
    let result = thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let (planner, f, p, service, reply, d) = (&planner, &f, &p, &service, &reply, &d);
        let child = scope.spawn(move || {
            let _ = tx.send(planner.inventory(&f.proof, p, service, Some(reply), 100, d));
        });
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while !f.probe.2.load(Ordering::Acquire) && std::time::Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        let received = rx.recv_timeout(Duration::from_millis(400)).ok();
        let retained = !f.probe.3.load(Ordering::Acquire);
        f.probe.1.store(false, Ordering::Release);
        child.join().unwrap();
        let limit = std::time::Instant::now() + Duration::from_secs(2);
        while !f.probe.3.load(Ordering::Acquire) && std::time::Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(f.probe.2.load(Ordering::Acquire) && f.probe.3.load(Ordering::Acquire));
        assert!(
            retained,
            "probe resources must remain owned while the worker is blocked"
        );
        received
    });
    assert!(matches!(
        result,
        Some(Err(RemovalError::Native(NativeError::Timeout)))
    ));
}

#[test]
fn activity_remaining_lifetime_is_checked_after_validation_io() {
    let f = Fixture::new(true);
    let p = package();
    let service = f.managed_service(&p, 4242);
    let reply = f.reply();
    let mut planner = f.planner();
    let initial = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 5000, &deadline())
        .unwrap();
    let plan = planner
        .plan(
            initial,
            1,
            OperationId(1),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = planner
        .inventory(&f.proof, &p, &service, Some(&reply), 5000, &deadline())
        .unwrap();
    assert!(cached.facts().activity.is_some());
    f.runner.service_delay_ms.store(200, Ordering::Release);
    let d = deadline();
    let mut input = f.request(&p, &service, &d);
    input.reply = Some(&reply);
    input.now_ms = 5000;
    let started = std::time::Instant::now();
    let result = planner.validate(&plan, &consent, &cached, input);
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(result, Err(RemovalError::Stale));
    assert_eq!(f.erase_count(), 0);
}
