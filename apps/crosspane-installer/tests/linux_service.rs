#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*, service::*},
};
use crosspane_installer_core::MutationOutcome;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::UnixListener,
    },
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
type Hook = Box<dyn FnOnce() + Send>;
type Failure = (Option<i32>, Vec<u8>, Vec<u8>);
fn shown_properties(properties: &BTreeMap<String, String>, all: bool) -> Vec<u8> {
    properties
        .iter()
        .filter(|(_, value)| all || !value.is_empty())
        .map(|(key, value)| format!("{key}={value}\n"))
        .collect::<String>()
        .into_bytes()
}
struct Runner {
    calls: Mutex<Vec<CommandSpec>>,
    properties: Mutex<BTreeMap<String, String>>,
    cat: Mutex<Vec<u8>>,
    hook: Mutex<Option<Hook>>,
    failure: Mutex<Option<Failure>>,
    runner_error: Mutex<Option<NativeError>>,
    mutation_failure: Mutex<Option<Failure>>,
    post_failure: Mutex<Option<Failure>>,
    mutation_stderr: Mutex<Vec<u8>>,
    mutation_hook: Mutex<Option<Hook>>,
    block_mutation: AtomicBool,
    no_change: AtomicBool,
}
impl CommandRunner for Runner {
    fn run(&self, c: &CommandSpec, d: &Deadline) -> Result<CommandOutput, NativeError> {
        d.check()?;
        self.calls.lock().unwrap().push(c.clone());
        if let Some(hook) = self.hook.lock().unwrap().take() {
            hook();
        }
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
        assert_eq!(c.executable(), Path::new("/usr/bin/systemctl"));
        assert_eq!(c.argv()[0], "--user");
        let verb = c.argv()[1].as_str();
        if let Some(error) = self.runner_error.lock().unwrap().take() {
            return Err(error);
        }
        if let Some((code, stdout, stderr)) = self.failure.lock().unwrap().take() {
            return Ok(CommandOutput {
                code,
                stdout,
                stderr,
            });
        }
        let mut p = self.properties.lock().unwrap();
        let (code, stdout) = match verb {
            "show" => (
                0,
                shown_properties(&p, c.argv().iter().any(|arg| arg == "--all")),
            ),
            "cat" => (0, self.cat.lock().unwrap().clone()),
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
            _ => {
                if let Some(hook) = self.mutation_hook.lock().unwrap().take() {
                    hook();
                }
                if let Some((code, stdout, stderr)) = self.mutation_failure.lock().unwrap().take() {
                    return Ok(CommandOutput {
                        code,
                        stdout,
                        stderr,
                    });
                }
                while self.block_mutation.load(Ordering::Acquire) {
                    d.check()?;
                    thread::sleep(Duration::from_millis(1));
                }
                if !self.no_change.load(Ordering::Acquire) {
                    match verb {
                        "enable" => {
                            p.insert("UnitFileState".into(), "enabled".into());
                            p.insert("WantedBy".into(), "graphical-session.target".into());
                        }
                        "disable" => {
                            p.insert("UnitFileState".into(), "disabled".into());
                            p.insert("WantedBy".into(), "".into());
                        }
                        "start" | "restart" => {
                            p.insert("ActiveState".into(), "active".into());
                            p.insert("SubState".into(), "running".into());
                            p.insert("MainPID".into(), "4242".into());
                        }
                        "stop" => {
                            p.insert("ActiveState".into(), "inactive".into());
                            p.insert("SubState".into(), "dead".into());
                            p.insert("MainPID".into(), "0".into());
                        }
                        "daemon-reload" => {
                            p.insert("NeedDaemonReload".into(), "no".into());
                        }
                        _ => panic!("unapproved fake command"),
                    }
                }
                *self.failure.lock().unwrap() = self.post_failure.lock().unwrap().take();
                (0, vec![])
            }
        };
        Ok(CommandOutput {
            code: Some(code),
            stdout,
            stderr: if matches!(
                verb,
                "enable" | "disable" | "start" | "stop" | "restart" | "daemon-reload"
            ) {
                self.mutation_stderr.lock().unwrap().clone()
            } else {
                vec![]
            },
        })
    }
}
struct Probe {
    executable: Mutex<PathBuf>,
    generation: AtomicU64,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
        d.check()?;
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: self.executable.lock().unwrap().clone(),
            generation: self.generation.load(Ordering::Acquire),
        })
    }
}
fn deadline() -> Deadline {
    Deadline::new(5000, Cancellation::default()).unwrap()
}
fn facts(io: &LinuxNativeIo) -> SupportObservations {
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
    let data: Vec<Vec<u8>> = (0..FILES.len())
        .map(|i| match i {
            0..=3 => elf.clone(),
            4 => include_bytes!("../../../packaging/linux/crosspane-agent.service").to_vec(),
            5 => include_bytes!("../../../packaging/linux/crosspane-settings.desktop").to_vec(),
            6 => include_bytes!("../../../packaging/linux/crosspane-installer.desktop").to_vec(),
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
struct Fixture {
    root: PathBuf,
    io: Arc<LinuxNativeIo>,
    runner: Arc<Runner>,
    resources: Vec<RenderedResource>,
    probe: Arc<Probe>,
    _listener: UnixListener,
}
impl Fixture {
    fn new(suffix: &str) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp47c-{}-{}{suffix}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runner = Arc::new(Runner {
            calls: Mutex::default(),
            properties: Mutex::default(),
            cat: Mutex::default(),
            hook: Mutex::default(),
            failure: Mutex::default(),
            runner_error: Mutex::default(),
            mutation_failure: Mutex::default(),
            post_failure: Mutex::default(),
            mutation_stderr: Mutex::default(),
            mutation_hook: Mutex::default(),
            block_mutation: AtomicBool::new(false),
            no_change: AtomicBool::new(false),
        });
        let probe = Arc::new(Probe {
            executable: Mutex::new(root.join(".local/bin/crosspane-agent")),
            generation: AtomicU64::new(77),
        });
        let io = Arc::new(LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap());
        let proof = io.scratch_support(facts(&io)).unwrap();
        for path in [
            io.target().paths().runtime_home.join("systemd"),
            io.target().paths().prefix.join("bin"),
        ] {
            io.create_private_dir(&proof, &path).unwrap();
        }
        let listener =
            UnixListener::bind(io.target().paths().runtime_home.join("systemd/private")).unwrap();
        fs::write(io.target().agent_path(), b"inert, never executed").unwrap();
        fs::set_permissions(io.target().agent_path(), fs::Permissions::from_mode(0o755)).unwrap();
        let install = PayloadInstaller::new(io.clone()).unwrap();
        io.create_private_dir(
            &proof,
            &io.target().paths().state_home.join("crosspane/installer"),
        )
        .unwrap();
        let resources = install.rendered_resources(&package()).unwrap();
        for record in &resources {
            io.create_private_dir(&proof, record.target.parent().unwrap())
                .unwrap();
            io.atomic_write(&proof, &record.target, &record.bytes)
                .unwrap();
        }
        let unit = &resources[0];
        let mut cat = format!("# {}\n", unit.target.display()).into_bytes();
        cat.extend(&unit.bytes);
        *runner.cat.lock().unwrap() = cat;
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
        let exe = io.target().agent_path().to_string_lossy().into_owned();
        p.insert("ExecStart".into(),format!("{{ path={exe} ; argv[]={exe} run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert("ExecStartEx".into(),format!("{{ path={exe} ; argv[]={exe} run ; flags= ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert(
            "Environment".into(),
            [
                format!(
                    "XDG_CONFIG_HOME={}",
                    io.target().paths().config_home.display()
                ),
                format!(
                    "XDG_STATE_HOME={}",
                    io.target().paths().state_home.display()
                ),
                format!(
                    "XDG_RUNTIME_DIR={}",
                    io.target().paths().runtime_home.display()
                ),
                format!(
                    "CROSSPANE_RUNTIME_DIR={}",
                    io.target().runtime_dir().display()
                ),
            ]
            .iter()
            .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" "),
        );
        *runner.properties.lock().unwrap() = p;
        Self {
            root,
            io,
            runner,
            resources,
            probe,
            _listener: listener,
        }
    }
    fn proof(&self) -> SupportProof {
        self.io.scratch_support(facts(&self.io)).unwrap()
    }
    fn service(&self) -> LinuxService {
        LinuxService::new(
            self.io.clone(),
            BTreeMap::new(),
            self.resources.clone(),
            &deadline(),
        )
        .unwrap()
    }
    fn environment(&self) -> ChildEnvironment {
        self.io
            .manager_environment(BTreeMap::new(), &deadline())
            .unwrap()
    }
    fn command(&self, verb: &str) -> CommandSpec {
        let mut argv = vec!["--user".into(), verb.into()];
        if verb == "show" {
            argv.push("--all".into());
        }
        if verb != "daemon-reload" {
            argv.push(UNIT.into());
        }
        if verb == "show" {
            argv.extend(["-p".into(), MANAGER_PROPERTIES.into()]);
        }
        CommandSpec::new(
            "/usr/bin/systemctl".into(),
            argv,
            self.environment(),
            if verb == "show" {
                crosspane_installer::platform::linux::detect::MAX_PROBE_BYTES
            } else {
                MAX_COMMAND_BYTES
            },
        )
        .unwrap()
    }
    fn active(&self) {
        let mut p = self.runner.properties.lock().unwrap();
        p.insert("ActiveState".into(), "active".into());
        p.insert("SubState".into(), "running".into());
        p.insert("MainPID".into(), "4242".into());
    }
    fn mutate(
        &self,
        proof: &SupportProof,
        command: &CommandSpec,
    ) -> Result<CommandOutput, NativeError> {
        self.io
            .run_manager_mutation(
                proof,
                command,
                self.io.install_lease(&self.proof()).unwrap(),
                &deadline(),
            )
            .result
    }
    fn bootstrap(&self, id: u64, phase: &str) {
        let proof = self.proof();
        self.io
            .create_private_dir(&proof, self.io.target().runtime_dir())
            .unwrap();
        self.io
            .atomic_write(
                &proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &serde_json::to_vec(&json!({"schema_version":1,"instance_id":id,"pid":4242,
            "started_unix_ms":parse_ps_start(START).unwrap(),"phase":phase,"phase_seq":2,
            "keystore":if phase=="waiting_for_keystore" { Value::Null } else { json!("os_store") },
            "reason":null,"runtime_dir":self.io.target().runtime_dir()}))
                .unwrap(),
            )
            .unwrap();
    }
    fn reply(&self, id: u64) -> AgentReply {
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let h = &mut v["result"]["installer"];
        h["instance"]["id"] = json!(id);
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
    fn mutations(&self) -> usize {
        self.runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| {
                c.executable() == Path::new("/usr/bin/systemctl")
                    && matches!(
                        c.argv()[1].as_str(),
                        "start" | "stop" | "restart" | "enable" | "disable" | "daemon-reload"
                    )
            })
            .count()
    }
}

/// WP-4.32b: real Applied journal shape, synthetic paths and an injected /proc observation.
/// The old process survives publication on a deleted inode; only this run's receipt can admit
/// it for a restart, and it remains inadmissible for ordinary health verification.
#[test]
fn an_applied_upgrade_restarts_only_its_recorded_deleted_instance_and_reports_submission() {
    use crosspane_installer_core::OperationId;
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new("");
    f.active();
    f.bootstrap(9, "ready");
    let service = f.service();
    let p = f.io.target().paths();
    let paths = [
        p.prefix.join("bin/crosspane-agent"),
        p.prefix.join("bin/crosspanectl"),
        p.prefix.join("bin/crosspane-ui"),
        p.prefix.join("bin/crosspane-installer"),
        p.config_home.join("systemd/user/crosspane-agent.service"),
        p.data_home.join("applications/crosspane-settings.desktop"),
        p.data_home.join("applications/crosspane-installer.desktop"),
        p.data_home
            .join("icons/hicolor/scalable/apps/crosspane.svg"),
        p.data_home.join("crosspane/LICENSE"),
    ];
    let meta = fs::metadata(&paths[0]).unwrap();
    let parent = fs::metadata(paths[0].parent().unwrap()).unwrap();
    let new = sha256(&fs::read(&paths[0]).unwrap());
    let old = sha256(b"the old recorded agent");
    let record = p
        .state_home
        .join("crosspane/installer/payload-outcome.json");
    let mut journal = json!({
        "receipt": {"schema_version":1,"operation_id":4,"product_version":"0.0.0",
            "manifest_sha256":vec![0u8;32],"payload_sha256":vec![0u8;32],
            "resources": FILES.iter().zip(&paths).map(|(name,path)| json!({
                "resource_id":name,"resolved_path":path,"ownership":"Created",
                "before":"Different","after":"Matching","outcome":"Unknown"
            })).collect::<Vec<_>>(), "unfinished":[47]},
        "items": (0..FILES.len()).map(|i| json!({"old":old,"new":new,"template":new,
            "ownership":"Created","replacement":if i == 0 { json!({
                "file":[meta.dev(),meta.ino()],"parent":[parent.dev(),parent.ino()],
                "hash":new,"mode":493}) } else { Value::Null }
        })).collect::<Vec<_>>(),
        "phase":"Applied","source":"Demo","previous_instance":9,"base_generation":null
    });
    let write = |value: &Value| {
        f.io.atomic_write(&f.proof(), &record, &serde_json::to_vec(value).unwrap())
            .unwrap()
    };
    let mut deleted = paths[0].clone().into_os_string();
    deleted.push(" (deleted)");
    *f.probe.executable.lock().unwrap() = deleted.into();
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Restart, &deadline())
            .is_err()
    );
    assert!(
        service
            .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
            .is_err()
    );
    write(&journal);
    assert!(
        service
            .plan_restart_after_payload(&f.proof(), OperationId(5), &deadline())
            .is_err()
    );
    journal["previous_instance"] = json!(8);
    write(&journal);
    assert!(
        service
            .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
            .is_err()
    );
    journal["previous_instance"] = json!(9);
    write(&journal);
    let deleted_path = f.probe.executable.lock().unwrap().clone();
    *f.probe.executable.lock().unwrap() = paths[0].with_file_name("foreign-agent (deleted)");
    assert!(
        service
            .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
            .is_err()
    );
    *f.probe.executable.lock().unwrap() = deleted_path;
    let plan = service
        .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
        .unwrap();
    f.probe.generation.store(78, Ordering::Release);
    assert!(service.apply(&f.proof(), plan, &deadline()).is_err());
    f.probe.generation.store(77, Ordering::Release);
    let plan = service
        .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
        .unwrap();
    journal["receipt"]["unfinished"] = json!([48]);
    write(&journal);
    assert!(service.apply(&f.proof(), plan, &deadline()).is_err());
    assert_eq!(f.mutations(), 0);
    // Restore the exact journal, then submit once to the selected fake manager.
    journal["receipt"]["unfinished"] = json!([47]);
    write(&journal);
    let plan = service
        .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
        .unwrap();
    let installed = fs::read(&paths[0]).unwrap();
    fs::write(&paths[0], b"changed after planning").unwrap();
    assert!(service.apply(&f.proof(), plan, &deadline()).is_err());
    fs::write(&paths[0], installed).unwrap();
    assert_eq!(f.mutations(), 0);
    let plan = service
        .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
        .unwrap();
    let result = service.apply(&f.proof(), plan, &deadline()).unwrap();
    assert!(result.submitted);
    assert!(result.after.is_some());
    assert!(result.diagnostic.is_none());
    assert_eq!(result.previous_instance, Some(9));
    assert_eq!(f.mutations(), 1);
    assert!(f.io.bootstrap(&deadline()).is_err(), "health stays strict");
    // A real manager rejection is submitted, but carries its failure rather than success.
    *f.runner.mutation_failure.lock().unwrap() =
        Some((Some(1), vec![], b"fixture restart refused\n".to_vec()));
    let plan = service
        .plan_restart_after_payload(&f.proof(), OperationId(4), &deadline())
        .unwrap();
    let result = service.apply(&f.proof(), plan, &deadline()).unwrap();
    assert!(result.submitted);
    assert!(result.after.is_none());
    assert!(
        result
            .diagnostic
            .unwrap()
            .contains("fixture restart refused")
    );
    let cancelled = Cancellation::default();
    cancelled.cancel();
    let cancelled = Deadline::new(5000, cancelled).unwrap();
    let proof = f.proof();
    let unsubmitted = f.io.run_manager_mutation(
        &proof,
        &f.command("restart"),
        f.io.install_lease(&proof).unwrap(),
        &cancelled,
    );
    assert!(!unsubmitted.submitted);
    assert!(unsubmitted.pending.is_none());
    assert!(unsubmitted.result.is_err());
    assert_eq!(
        f.mutations(),
        2,
        "cancelled before dispatch never contacts the manager"
    );
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}
fn replace_socket(path: &Path) {
    fs::rename(path, path.with_extension("old")).unwrap();
    let _listener = UnixListener::bind(path).unwrap();
}

#[test]
fn exact_argv_endpoint_capability_environment_and_mutation_proof() {
    let f = Fixture::new("");
    let env = f.environment();
    assert_eq!(
        env.values()["XDG_RUNTIME_DIR"],
        f.io.target().paths().runtime_home.to_string_lossy()
    );
    assert!(
        !env.values()
            .keys()
            .any(|k| k == "DBUS_SYSTEM_BUS_ADDRESS" || k.starts_with("SYSTEMD_"))
    );
    assert!(!env.values().contains_key("DBUS_SESSION_BUS_ADDRESS"));
    for verb in [
        "start",
        "stop",
        "restart",
        "enable",
        "disable",
        "is-active",
        "is-enabled",
        "daemon-reload",
        "show",
        "cat",
    ] {
        let c = f.command(verb);
        assert_eq!(c.argv()[0], "--user");
        if verb != "daemon-reload" {
            assert_eq!(c.argv()[if verb == "show" { 3 } else { 2 }], UNIT);
        }
        if matches!(
            verb,
            "start" | "stop" | "restart" | "enable" | "disable" | "daemon-reload"
        ) {
            assert_eq!(
                f.io.run(&c, &deadline()).unwrap_err(),
                NativeError::Unsupported
            );
        }
    }
    for args in [
        vec!["--system", "start", UNIT],
        vec!["--global", "start", UNIT],
        vec!["--user", "--global", "enable", UNIT],
        vec!["--user", "--global=true", "enable", UNIT],
        vec!["--user", "enable", "--now", UNIT],
        vec!["--user", "start", "other.service"],
        vec!["--user", "mask", UNIT],
        vec!["--user", "--runtime", "enable", UNIT],
        vec!["--user", "-H", "host", "start", UNIT],
        vec!["--user", "-M", "machine", "start", UNIT],
        vec!["--user", "--host", "host", "start", UNIT],
        vec!["--user", "--host=host", "start", UNIT],
        vec!["--user", "--machine", "machine", "start", UNIT],
        vec!["--user", "--machine=machine", "start", UNIT],
        vec!["--user", "-Hhost", "start", UNIT],
        vec!["--user", "-Mmachine", "start", UNIT],
        vec!["--user", "start", UNIT, "other.service"],
        vec!["--user", "start", UNIT, "--no-block"],
        vec!["--user", "daemon-reload", UNIT],
        vec!["--user", "cat", UNIT, "extra"],
        vec!["--user", "edit", UNIT],
        vec!["--user", "link", UNIT],
        vec!["--user", "set-environment", "X=1"],
        vec!["--user", "import-environment"],
        vec!["--user", "show", UNIT, "-p", "MainPID"],
        vec!["--user", "show", UNIT, "-p", MANAGER_PROPERTIES],
        vec!["--user", "show", "--all", UNIT, "-p", "MainPID"],
        vec!["--user", "show", UNIT, "--all", "-p", MANAGER_PROPERTIES],
        vec![
            "--user",
            "show",
            "--all",
            "--all",
            UNIT,
            "-p",
            MANAGER_PROPERTIES,
        ],
    ] {
        assert!(
            CommandSpec::new(
                "/usr/bin/systemctl".into(),
                args.into_iter().map(str::to_owned).collect(),
                env.clone(),
                256
            )
            .is_err()
        );
    }
    assert!(
        CommandSpec::new(
            "/bin/systemctl".into(),
            vec!["--user".into(), "cat".into(), UNIT.into()],
            env.clone(),
            256
        )
        .is_err()
    );
    let other = Fixture::new("");
    assert_eq!(
        f.mutate(&other.proof(), &f.command("start")).unwrap_err(),
        NativeError::Unsupported
    );
    let proof = f.proof();
    let mut changed = facts(&f.io);
    changed.active = false;
    assert!(proof.revalidate(&f.io, &changed).is_err());
    assert_eq!(
        f.mutate(&proof, &f.command("start")).unwrap_err(),
        NativeError::Unsupported
    );
    assert!(f.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn manager_and_optional_bus_replacement_before_spawn_or_during_runner() {
    for bus in [false, true] {
        for during in [false, true] {
            for mutation in [false, true] {
                let f = Fixture::new("");
                let path = if bus {
                    f.io.target().paths().runtime_home.join("bus")
                } else {
                    f.io.target().paths().runtime_home.join("systemd/private")
                };
                let _bus = bus.then(|| UnixListener::bind(&path).unwrap());
                let session = if bus {
                    BTreeMap::from([(
                        "DBUS_SESSION_BUS_ADDRESS".into(),
                        format!("unix:path={}", path.display()),
                    )])
                } else {
                    BTreeMap::new()
                };
                let env = f.io.manager_environment(session, &deadline()).unwrap();
                if bus {
                    assert_eq!(
                        env.values()["DBUS_SESSION_BUS_ADDRESS"],
                        format!("unix:path={}", path.display())
                    );
                }
                let verb = if mutation { "start" } else { "cat" };
                let c = CommandSpec::new(
                    "/usr/bin/systemctl".into(),
                    vec!["--user".into(), verb.into(), UNIT.into()],
                    env,
                    256,
                )
                .unwrap();
                if during {
                    let path = path.clone();
                    *f.runner.hook.lock().unwrap() = Some(Box::new(move || replace_socket(&path)));
                } else {
                    replace_socket(&path);
                }
                let result = if mutation {
                    f.mutate(&f.proof(), &c)
                } else {
                    f.io.run(&c, &deadline())
                };
                assert_eq!(
                    result.unwrap_err(),
                    if during && mutation {
                        NativeError::OutcomeUnknown
                    } else {
                        NativeError::Foreign
                    }
                );
                assert_eq!(f.runner.calls.lock().unwrap().len(), usize::from(during));
            }
        }
    }
}

#[test]
fn show_all_preserves_supported_empty_properties_but_missing_properties_refuse() {
    let f = Fixture::new("");
    {
        let properties = f.runner.properties.lock().unwrap();
        let normal = String::from_utf8(shown_properties(&properties, false)).unwrap();
        let all = String::from_utf8(shown_properties(&properties, true)).unwrap();
        for key in ["EnvironmentFiles", "ExecStop", "ExecCondition", "OpenFile"] {
            assert!(
                !normal
                    .lines()
                    .any(|line| line.starts_with(&format!("{key}=")))
            );
            assert!(all.lines().any(|line| line == format!("{key}=")));
        }
    }
    let service = f.service();
    assert!(service.observe(&deadline()).is_ok());
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Enable, &deadline())
            .is_ok()
    );
    let calls = f.runner.calls.lock().unwrap();
    let shows: Vec<_> = calls.iter().filter(|c| c.argv()[1] == "show").collect();
    assert!(!shows.is_empty());
    for command in shows {
        assert_eq!(
            command.argv(),
            ["--user", "show", "--all", UNIT, "-p", MANAGER_PROPERTIES]
        );
    }
    drop(calls);
    // A scalar property is never optional: absence refuses.
    f.runner.properties.lock().unwrap().remove("Type");
    assert_eq!(
        service.observe(&deadline()).unwrap_err(),
        ServiceError::Unknown
    );
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Enable, &deadline())
            .is_err()
    );
    assert_eq!(f.mutations(), 0);
}

/// `systemctl --user show --all crosspane-agent.service -p …` on systemd 261.2 (owner's desktop):
/// 20 empty list/path properties are omitted, and several defaults print differently.
const SYSTEMD_261_OMITTED: [&str; 20] = [
    "CacheDirectorySymlink",
    "EnvironmentFiles",
    "ExecCondition",
    "ExecConditionEx",
    "ExecReload",
    "ExecReloadEx",
    "ExecReloadPost",
    "ExecReloadPostEx",
    "ExecStartPost",
    "ExecStartPostEx",
    "ExecStartPre",
    "ExecStartPreEx",
    "ExecStop",
    "ExecStopEx",
    "ExecStopPost",
    "ExecStopPostEx",
    "LogsDirectorySymlink",
    "OpenFile",
    "RuntimeDirectorySymlink",
    "StateDirectorySymlink",
];
type Mutation<'a> = Box<dyn Fn(&Fixture, &mut BTreeMap<String, String>) + 'a>;
fn systemd_261(f: &Fixture) -> Vec<String> {
    let home = f.io.target().paths().home.to_string_lossy().into_owned();
    let first = home.split('/').nth(1).unwrap().to_owned();
    assert!(first.bytes().all(|b| b.is_ascii_alphanumeric()));
    let mounts = vec!["-.mount".to_owned(), format!("{first}.mount")];
    let mut properties = f.runner.properties.lock().unwrap();
    for key in SYSTEMD_261_OMITTED {
        assert_eq!(properties.remove(key).as_deref(), Some(""), "{key}");
    }
    for (key, value) in [
        ("WorkingDirectory", format!("!{home}")),
        ("WantsMountsFor", home.clone()),
        (
            "After",
            format!(
                "{} app.slice basic.target graphical-session.target {}",
                mounts[1], mounts[0]
            ),
        ),
        ("TTYPath", String::new()),
        ("WatchdogUSec", "infinity".into()),
        ("Conditions", "[unprintable]".into()),
        ("Asserts", "[unprintable]".into()),
    ] {
        properties.insert(key.into(), value);
    }
    mounts
}

#[test]
fn systemd_261_show_output_is_read_as_the_same_sealed_unit() {
    let f = Fixture::new("");
    systemd_261(&f);
    let service = f.service();
    let facts = service.observe(&deadline()).unwrap();
    assert!(!facts.enabled);
    assert_eq!(facts.active_state, "inactive");
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Enable, &deadline())
            .is_ok()
    );
    assert_eq!(f.mutations(), 0);
    // systemd 261 prints dependency sets in hash order, which differs between two calls.
    let mut reordered = f.runner.properties.lock().unwrap().clone();
    let mut after: Vec<_> = reordered["After"].split(' ').map(String::from).collect();
    after.reverse();
    reordered.insert("After".into(), after.join(" "));
    *f.runner.failure.lock().unwrap() = Some((Some(0), shown_properties(&reordered, true), vec![]));
    assert!(service.observe(&deadline()).is_ok());
}

#[test]
fn systemd_261_tolerances_never_admit_other_values_or_absent_scalars() {
    let home = |f: &Fixture| f.io.target().paths().home.to_string_lossy().into_owned();
    let cases: Vec<Mutation<'_>> = vec![
        // Scalars and non-hook lists stay mandatory.
        Box::new(|_, p| {
            p.remove("KillMode");
        }),
        Box::new(|_, p| {
            p.remove("ExecStart");
        }),
        Box::new(|_, p| {
            p.remove("Environment");
        }),
        Box::new(|_, p| {
            p.remove("WorkingDirectory");
        }),
        // A shown hook must still be empty.
        Box::new(|_, p| {
            p.insert(
                "ExecStartPre".into(),
                "{ path=/foreign ; argv[]=/foreign }".into(),
            );
        }),
        Box::new(|_, p| {
            p.insert("OpenFile".into(), "/foreign".into());
        }),
        // Only the home default, only with its matching mount wants.
        Box::new(|_, p| {
            p.insert("WorkingDirectory".into(), "!/foreign".into());
            p.insert("WantsMountsFor".into(), "/foreign".into());
        }),
        Box::new(|f, p| {
            p.insert("WorkingDirectory".into(), home(f));
        }),
        Box::new(|_, p| {
            p.insert("WantsMountsFor".into(), "".into());
        }),
        Box::new(|f, p| {
            p.insert("WorkingDirectory".into(), "".into());
            p.insert("WantsMountsFor".into(), home(f));
        }),
        // After: no other units, no duplicates, nothing missing.
        Box::new(|_, p| {
            let after = format!("{} foreign.service", p["After"]);
            p.insert("After".into(), after);
        }),
        Box::new(|_, p| {
            let after = format!("{} -.mount", p["After"]);
            p.insert("After".into(), after);
        }),
        Box::new(|_, p| {
            p.insert("After".into(), "-.mount app.slice basic.target".into());
        }),
        Box::new(|_, p| {
            p.insert("TTYPath".into(), "/dev/tty1".into());
        }),
        Box::new(|_, p| {
            p.insert("WatchdogUSec".into(), "1s".into());
        }),
        Box::new(|_, p| {
            p.insert("Conditions".into(), "foreign".into());
        }),
    ];
    for (index, case) in cases.iter().enumerate() {
        let f = Fixture::new("");
        systemd_261(&f);
        case(&f, &mut f.runner.properties.lock().unwrap());
        let service = f.service();
        assert!(service.observe(&deadline()).is_err(), "case {index}");
        assert!(
            service
                .plan(&f.proof(), ServiceAction::Start, &deadline())
                .is_err(),
            "case {index}"
        );
        assert_eq!(f.mutations(), 0);
    }
    // Unrelated properties are ignored; duplicate consumed authority stays unknown.
    for extra in ["Foreign=x\n", "Type=simple\n"] {
        let f = Fixture::new("");
        systemd_261(&f);
        let mut stdout = shown_properties(&f.runner.properties.lock().unwrap(), true);
        stdout.extend_from_slice(extra.as_bytes());
        *f.runner.failure.lock().unwrap() = Some((Some(0), stdout, vec![]));
        if extra.starts_with("Foreign") {
            assert!(f.service().observe(&deadline()).is_ok());
        } else {
            assert_eq!(
                f.service().observe(&deadline()).unwrap_err(),
                ServiceError::Unknown,
                "{extra}"
            );
        }
    }
}

#[test]
fn public_manager_mutation_entry_refuses_every_verb_with_or_without_payload_lock() {
    for locked in [false, true] {
        let f = Fixture::new("");
        let proof = f.proof();
        let _lease = locked.then(|| f.io.install_lease(&proof).unwrap());
        for verb in [
            "start",
            "stop",
            "restart",
            "enable",
            "disable",
            "daemon-reload",
        ] {
            let command = f.command(verb);
            assert_eq!(
                f.io.run_mutation(&proof, &command, &deadline())
                    .unwrap_err(),
                NativeError::Unsupported,
            );
            assert_eq!(
                f.io.run(&command, &deadline()).unwrap_err(),
                NativeError::Unsupported
            );
        }
        assert!(f.runner.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn loaded_execution_propagation_and_action_drift_blocks_stop_and_restart_with_matching_disk() {
    for (key, hostile) in [
        ("OnSuccess", "foreign.service"),
        ("PropagatesStopTo", "foreign.service"),
        ("PropagatesReloadTo", "foreign.service"),
        ("ReloadPropagatedFrom", "foreign.service"),
        ("StopPropagatedFrom", "foreign.service"),
        ("JoinsNamespaceOf", "foreign.service"),
        ("RequiredBy", "foreign.service"),
        ("RequisiteOf", "foreign.service"),
        ("BoundBy", "foreign.service"),
        ("UpheldBy", "foreign.service"),
        ("ConsistsOf", "foreign.service"),
        ("ConflictedBy", "foreign.service"),
        ("OnSuccessOf", "foreign.service"),
        ("OnFailureOf", "foreign.service"),
        ("Triggers", "foreign.service"),
        ("TriggeredBy", "foreign.service"),
        ("Following", "foreign.service"),
        ("SliceOf", "foreign.service"),
        ("RequiresMountsFor", "/foreign"),
        ("WantsMountsFor", "/foreign"),
        ("DelegateControllers", "cpu"),
        ("DelegateSubgroup", "foreign"),
        ("Conditions", "ConditionPathExists=/foreign"),
        ("Asserts", "AssertPathExists=/foreign"),
        ("ExecConditionEx", "HOOK"),
        ("ExecStartPreEx", "HOOK"),
        ("ExecStartPostEx", "HOOK"),
        ("ExecStopEx", "HOOK"),
        ("ExecStopPostEx", "HOOK"),
        ("ExecReloadEx", "HOOK"),
        ("ExecReloadPost", "HOOK"),
        ("ExecReloadPostEx", "HOOK"),
        ("RestartPreventExitStatus", "1"),
        ("RestartForceExitStatus", "1"),
        ("SuccessExitStatus", "1"),
        ("OpenFile", "/foreign:foreign:read-only"),
        ("ExtraFileDescriptorNames", "foreign"),
        ("BindPaths", "/foreign"),
        ("BindReadOnlyPaths", "/foreign"),
        ("TemporaryFileSystem", "/foreign"),
        ("MountImages", "/foreign"),
        ("ExtensionImages", "/foreign"),
        ("ExtensionDirectories", "/foreign"),
        ("PAMName", "foreign"),
        ("Slice", "foreign.slice"),
        ("Delegate", "yes"),
        ("OOMPolicy", "kill"),
        ("ManagedOOMSwap", "kill"),
        ("ManagedOOMMemoryPressure", "kill"),
        ("ManagedOOMPreference", "omit"),
        ("SuccessAction", "exit"),
        ("FailureAction", "exit"),
        ("StartLimitAction", "exit"),
        ("JobTimeoutAction", "exit"),
        ("OnSuccessJobMode", "isolate"),
        ("OnFailureJobMode", "isolate"),
        ("StopWhenUnneeded", "yes"),
        ("RefuseManualStart", "yes"),
        ("RefuseManualStop", "yes"),
        ("AllowIsolate", "yes"),
        ("IgnoreOnIsolate", "yes"),
        ("SurviveFinalKillSignal", "yes"),
        ("JobTimeoutUSec", "1s"),
        ("JobRunningTimeoutUSec", "1s"),
        ("CollectMode", "inactive-or-failed"),
        ("RestartMode", "direct"),
        ("RestartSteps", "1"),
        ("RestartMaxDelayUSec", "1s"),
        ("TimeoutStartFailureMode", "kill"),
        ("TimeoutStopFailureMode", "kill"),
        ("RuntimeMaxUSec", "1s"),
        ("RuntimeRandomizedExtraUSec", "1s"),
        ("WatchdogUSec", "1s"),
        ("ExitType", "cgroup"),
        ("FileDescriptorStoreMax", "1"),
        ("NFileDescriptorStore", "1"),
        ("FileDescriptorStorePreserve", "yes"),
        ("RootDirectoryStartOnly", "yes"),
        ("RootEphemeral", "yes"),
        ("RuntimeDirectory", "foreign"),
        ("StateDirectory", "foreign"),
        ("CacheDirectory", "foreign"),
        ("LogsDirectory", "foreign"),
        ("ConfigurationDirectory", "foreign"),
        ("RuntimeDirectorySymlink", "foreign"),
        ("StateDirectorySymlink", "foreign"),
        ("CacheDirectorySymlink", "foreign"),
        ("LogsDirectorySymlink", "foreign"),
        ("RootMStack", "foreign"),
        ("RuntimeDirectoryPreserve", "yes"),
        ("ExecStartEx", "FLAGS"),
    ] {
        let f = Fixture::new("");
        let service = f.service();
        let mut p = f.runner.properties.lock().unwrap();
        assert_eq!(p["NeedDaemonReload"], "no");
        let value = match hostile {
            "HOOK" => p[if key.ends_with("Ex") {
                "ExecStartEx"
            } else {
                "ExecStart"
            }]
            .clone(),
            "FLAGS" => p["ExecStartEx"].replace("flags= ;", "flags=privileged ;"),
            value => value.into(),
        };
        p.insert(key.into(), value);
        drop(p);
        assert_eq!(
            fs::read(&f.resources[0].target).unwrap(),
            f.resources[0].bytes
        );
        for action in [ServiceAction::Stop, ServiceAction::Restart] {
            assert_eq!(
                service.plan(&f.proof(), action, &deadline()).unwrap_err(),
                ServiceError::Foreign,
                "{key}"
            );
        }
        assert_eq!(f.mutations(), 0, "{key}");
    }
}

#[test]
fn default_user_slice_dependency_order_is_irrelevant_but_extra_edges_are_foreign() {
    let f = Fixture::new("");
    let service = f.service();
    {
        let mut p = f.runner.properties.lock().unwrap();
        p.insert("Requires".into(), "app.slice basic.target".into());
        p.insert(
            "After".into(),
            "graphical-session.target app.slice basic.target".into(),
        );
    }
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Stop, &deadline())
            .is_ok()
    );
    for (key, value) in [
        ("Requires", "app.slice basic.target foreign.service"),
        (
            "After",
            "graphical-session.target app.slice basic.target foreign.socket",
        ),
    ] {
        let old = f
            .runner
            .properties
            .lock()
            .unwrap()
            .insert(key.into(), value.into())
            .unwrap();
        assert_eq!(
            service
                .plan(&f.proof(), ServiceAction::Stop, &deadline())
                .unwrap_err(),
            ServiceError::Foreign
        );
        f.runner.properties.lock().unwrap().insert(key.into(), old);
    }
    assert_eq!(f.mutations(), 0);
}

#[test]
fn manager_ancestry_socket_symlinks_modes_and_directory_identity_are_refused() {
    for choice in 0..6 {
        let f = Fixture::new("");
        let runtime = &f.io.target().paths().runtime_home;
        let dir = runtime.join("systemd");
        let socket = dir.join("private");
        match choice {
            0 => fs::set_permissions(runtime, fs::Permissions::from_mode(0o755)).unwrap(),
            1 => fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap(),
            2 => {
                fs::rename(&socket, dir.join("original")).unwrap();
                symlink("original", &socket).unwrap();
            }
            3 => {
                fs::rename(&dir, runtime.join("original")).unwrap();
                symlink("original", &dir).unwrap();
            }
            4 => {
                fs::remove_file(&socket).unwrap();
                fs::write(&socket, b"not a socket").unwrap();
            }
            _ => fs::hard_link(&socket, dir.join("alias")).unwrap(),
        }
        assert!(
            f.io.manager_environment(BTreeMap::new(), &deadline())
                .is_err()
        );
        assert!(f.runner.calls.lock().unwrap().is_empty());
    }
    let f = Fixture::new("");
    let c = f.command("cat");
    let dir = f.io.target().paths().runtime_home.join("systemd");
    fs::rename(&dir, dir.with_extension("old")).unwrap();
    fs::create_dir(&dir).unwrap();
    let _new = UnixListener::bind(dir.join("private")).unwrap();
    assert_eq!(f.io.run(&c, &deadline()).unwrap_err(), NativeError::Foreign);
    assert!(f.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn frozen_payload_templates_render_for_two_targets_and_preserve_literal_paths() {
    let a = Fixture::new(" space%$");
    let b = Fixture::new(" different");
    let sa = a.service();
    let sb = b.service();
    assert_ne!(a.resources[0].bytes, b.resources[0].bytes);
    assert!(
        String::from_utf8(a.resources[0].bytes.clone())
            .unwrap()
            .contains("space%%$")
    );
    assert_eq!(
        sa.observe(&deadline()).unwrap().source,
        ObservationSource::Demo
    );
    assert_eq!(
        sb.observe(&deadline()).unwrap().source,
        ObservationSource::Demo
    );
    for i in 0..3 {
        let mut r = a.resources.clone();
        r[i].bytes.extend(b"foreign");
        r[i].rendered_sha256 = sha256(&r[i].bytes);
        assert!(LinuxService::new(a.io.clone(), BTreeMap::new(), r, &deadline()).is_err());
    }
    let mut r = a.resources.clone();
    r[0].template_sha256 = [0; 32];
    assert!(LinuxService::new(a.io.clone(), BTreeMap::new(), r, &deadline()).is_err());
    let mut r = a.resources.clone();
    r[0].source = ObservationSource::Live;
    assert!(LinuxService::new(a.io.clone(), BTreeMap::new(), r, &deadline()).is_err());
}

#[test]
fn effective_foreign_unit_dropins_execution_environment_and_files_block_mutations() {
    for (key, value) in [
        ("FragmentPath", "/foreign/unit"),
        ("DropInPaths", "/foreign/override.conf"),
        (
            "ExecStart",
            "{ path=/foreign ; argv[]=/foreign run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }",
        ),
        ("User", "root"),
        ("Environment", "XDG_RUNTIME_DIR=/foreign"),
        ("ExecStop", "/foreign"),
        ("EnvironmentFiles", "/foreign/env"),
        ("RootDirectory", "/foreign"),
        ("Restart", "always"),
        ("ExecCondition", "/foreign/program"),
        ("ExecStartPre", "/foreign/program"),
        ("ExecStartPost", "/foreign/program"),
        ("ExecStopPost", "/foreign/program"),
        ("ExecReload", "/foreign/program"),
        ("Type", "forking"),
        ("Requires", "foreign.service"),
        ("Wants", "foreign.service"),
        ("BindsTo", "foreign.service"),
        ("PartOf", "foreign.service"),
        ("Upholds", "foreign.service"),
        ("OnFailure", "foreign.service"),
        ("Conflicts", "foreign.service"),
        ("Before", "foreign.service"),
        (
            "After",
            "basic.target graphical-session.target foreign.service",
        ),
        ("DefaultDependencies", "no"),
        ("KillMode", "none"),
        ("KillSignal", "9"),
        ("SendSIGKILL", "no"),
        ("FinalKillSignal", "15"),
        ("RestartKillSignal", "9"),
        ("SendSIGHUP", "yes"),
        ("UnsetEnvironment", "XDG_RUNTIME_DIR"),
        ("PassEnvironment", "FOREIGN"),
        ("WorkingDirectory", "/foreign"),
        ("RootImage", "/foreign"),
        ("Group", "root"),
        ("DynamicUser", "yes"),
        ("UMask", "0000"),
        ("BusName", "foreign.bus"),
        ("PIDFile", "/foreign"),
        ("RemainAfterExit", "yes"),
        ("NotifyAccess", "all"),
        ("ExecSearchPath", "/foreign"),
        ("StandardInput", "tty"),
        ("StandardOutput", "file:/foreign"),
        ("StandardError", "file:/foreign"),
        ("TTYPath", "/foreign"),
        ("Requisite", ""),
        ("TimeoutStopUSec", "infinity"),
        ("UnitFileState", "masked"),
    ] {
        let f = Fixture::new("");
        let service = f.service();
        f.runner
            .properties
            .lock()
            .unwrap()
            .insert(key.into(), value.into());
        assert!(
            service
                .plan(&f.proof(), ServiceAction::Start, &deadline())
                .is_err()
        );
        assert_eq!(f.mutations(), 0);
    }
    let f = Fixture::new("");
    let service = f.service();
    fs::write(&f.resources[0].target, b"hand edited").unwrap();
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Enable, &deadline())
            .is_err()
    );
    assert_eq!(f.mutations(), 0);
    let f = Fixture::new("");
    let service = f.service();
    f.runner
        .cat
        .lock()
        .unwrap()
        .extend(b"\n# /foreign/drop-in\n");
    assert!(
        service
            .plan(&f.proof(), ServiceAction::Stop, &deadline())
            .is_err()
    );
    assert_eq!(f.mutations(), 0);
}

#[test]
fn disabled_enable_reload_stop_and_start_command_success_are_distinct_facts() {
    let f = Fixture::new("");
    let service = f.service();
    let initial = service.observe(&deadline()).unwrap();
    assert!(!initial.enabled);
    assert_eq!(initial.main_pid, 0);
    for action in [
        ServiceAction::Enable,
        ServiceAction::Disable,
        ServiceAction::Reload,
    ] {
        let result = service
            .apply(
                &f.proof(),
                service.plan(&f.proof(), action, &deadline()).unwrap(),
                &deadline(),
            )
            .unwrap();
        assert_eq!(result.outcome, MutationOutcome::Verified);
        assert!(result.after.is_some());
    }
    let start = service
        .apply(
            &f.proof(),
            service
                .plan(&f.proof(), ServiceAction::Start, &deadline())
                .unwrap(),
            &deadline(),
        )
        .unwrap();
    assert_eq!(start.outcome, MutationOutcome::Unknown);
    assert!(
        service
            .agent(
                start.after.as_ref().unwrap(),
                None,
                19,
                100,
                None,
                &deadline()
            )
            .is_err()
    );
    f.bootstrap(9, "waiting_for_keystore");
    assert!(matches!(
        service
            .agent(
                start.after.as_ref().unwrap(),
                None,
                19,
                100,
                None,
                &deadline()
            )
            .unwrap(),
        AgentEvidence::WaitingForKeystore(_)
    ));
    let stop = service
        .apply(
            &f.proof(),
            service
                .plan(&f.proof(), ServiceAction::Stop, &deadline())
                .unwrap(),
            &deadline(),
        )
        .unwrap();
    assert_eq!(stop.outcome, MutationOutcome::Verified);
    assert!(matches!(
        service
            .agent(
                stop.after.as_ref().unwrap(),
                None,
                19,
                100,
                None,
                &deadline()
            )
            .unwrap(),
        AgentEvidence::ManagerInactive
    ));
    assert_eq!(f.mutations(), 5);
}

/// WP-4.33b: the agent can restart in place through exec, so systemd's MainPID and the process
/// start time stay the same and only the instance id changes. That is a new instance: the wait
/// after a restart completes on it, and never on the instance that was running before.
#[test]
fn an_in_place_exec_restart_with_the_same_main_pid_is_a_new_instance() {
    let f = Fixture::new("");
    let service = f.service();
    f.active();
    f.bootstrap(9, "ready");
    let result = service
        .apply(
            &f.proof(),
            service
                .plan(&f.proof(), ServiceAction::Restart, &deadline())
                .unwrap(),
            &deadline(),
        )
        .unwrap();
    let facts = result.after.as_ref().unwrap();
    assert_eq!(result.previous_instance, Some(9));
    let agent = |id| {
        service.agent(
            facts,
            Some(&f.reply(id)),
            19,
            100,
            result.previous_instance,
            &deadline(),
        )
    };
    assert!(
        agent(9).is_err(),
        "the old instance never completes the wait"
    );
    // Same MainPID (4242) and start time, new instance id.
    f.bootstrap(10, "ready");
    assert!(matches!(agent(10).unwrap(), AgentEvidence::Matched(_)));
}

#[test]
fn locked_starting_failed_ready_and_restart_require_actual_new_matched_status() {
    let f = Fixture::new("");
    let service = f.service();
    f.active();
    for phase in ["starting", "waiting_for_keystore", "failed", "ready"] {
        f.bootstrap(9, phase);
        let facts = service.observe(&deadline()).unwrap();
        let evidence = service
            .agent(&facts, None, 19, 100, None, &deadline())
            .unwrap();
        assert!(matches!(
            (phase, evidence),
            ("starting", AgentEvidence::Starting(_))
                | ("waiting_for_keystore", AgentEvidence::WaitingForKeystore(_))
                | ("failed", AgentEvidence::Failed(_))
                | ("ready", AgentEvidence::PendingStatus(_))
        ));
    }
    let result = service
        .apply(
            &f.proof(),
            service
                .plan(&f.proof(), ServiceAction::Restart, &deadline())
                .unwrap(),
            &deadline(),
        )
        .unwrap();
    let facts = result.after.as_ref().unwrap();
    assert_eq!(result.previous_instance, Some(9));
    assert!(
        service
            .agent(
                facts,
                Some(&f.reply(9)),
                19,
                100,
                result.previous_instance,
                &deadline()
            )
            .is_err()
    );
    f.bootstrap(10, "ready");
    assert!(matches!(
        service
            .agent(
                facts,
                Some(&f.reply(10)),
                19,
                100,
                result.previous_instance,
                &deadline()
            )
            .unwrap(),
        AgentEvidence::Matched(_)
    ));
    for bad in 0..6 {
        let mut r = f.reply(10);
        match bad {
            0 => r.id = 20,
            1 => r.source = ObservationSource::Live,
            2 => r.observed_at_ms = 101,
            3 => r.observed_at_ms = 0,
            4 => r.result = Err(CallFailure::Refused(AgentRefusal::NotSupported)),
            _ => r = f.reply(11),
        };
        let result = service.agent(
            facts,
            Some(&r),
            19,
            if bad == 3 { 5001 } else { 100 },
            None,
            &deadline(),
        );
        if bad == 4 {
            assert!(matches!(
                result,
                Ok(AgentEvidence::StatusFailure(CallFailure::Refused(
                    AgentRefusal::NotSupported
                )))
            ));
        } else {
            assert!(result.is_err());
        }
    }
    let mut reply = f.reply(10);
    reply.result = Ok(DecodedReply::Status(
        StatusAdmission::PendingHealthContract(PendingHealthReason::UnsupportedVersion),
    ));
    assert!(matches!(
        service
            .agent(facts, Some(&reply), 19, 100, None, &deadline())
            .unwrap(),
        AgentEvidence::PendingHealthContract(PendingHealthReason::UnsupportedVersion, _)
    ));
    reply.result = Ok(DecodedReply::Acknowledged);
    assert!(matches!(
        service.agent(facts, Some(&reply), 19, 100, None, &deadline()),
        Err(ServiceError::Unknown)
    ));
}

#[test]
fn unsupported_revoked_or_changed_plan_never_mutates() {
    let f = Fixture::new("");
    let service = f.service();
    let proof = f.proof();
    let plan = service
        .plan(&proof, ServiceAction::Enable, &deadline())
        .unwrap();
    let mut changed = facts(&f.io);
    changed.compositor_managed = false;
    assert!(proof.revalidate(&f.io, &changed).is_err());
    assert!(service.apply(&proof, plan, &deadline()).is_err());
    assert_eq!(f.mutations(), 0);
    assert!(f.io.scratch_support(changed).is_err());
    let plan = service
        .plan(&f.proof(), ServiceAction::Enable, &deadline())
        .unwrap();
    f.runner
        .properties
        .lock()
        .unwrap()
        .insert("NeedDaemonReload".into(), "yes".into());
    assert!(service.apply(&f.proof(), plan, &deadline()).is_err());
    assert_eq!(f.mutations(), 0);
}

#[test]
fn payload_lock_excludes_service_mutation_without_any_manager_io() {
    let f = Fixture::new("");
    let service = f.service();
    let proof = f.proof();
    let plan = service
        .plan(&proof, ServiceAction::Enable, &deadline())
        .unwrap();
    let _lock =
        f.io.lock(
            &proof,
            &f.io
                .target()
                .paths()
                .state_home
                .join("crosspane/installer/install.lock"),
        )
        .unwrap();
    f.runner.calls.lock().unwrap().clear();
    assert!(matches!(
        service.apply(&proof, plan, &deadline()),
        Err(ServiceError::Native(NativeError::Busy))
    ));
    assert!(f.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn stale_or_unmanaged_bootstrap_and_expired_proof_block_fresh_start() {
    let f = Fixture::new("");
    let service = f.service();
    f.bootstrap(9, "ready");
    assert!(matches!(
        service.plan(&f.proof(), ServiceAction::Start, &deadline()),
        Err(ServiceError::Foreign)
    ));
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut bootstrap: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    bootstrap["started_unix_ms"] = json!(1);
    fs::write(path, serde_json::to_vec(&bootstrap).unwrap()).unwrap();
    assert!(matches!(
        service.plan(&f.proof(), ServiceAction::Start, &deadline()),
        Err(ServiceError::Foreign)
    ));
    let proof = f.proof();
    let plan = service
        .plan(&proof, ServiceAction::Enable, &deadline())
        .unwrap();
    thread::sleep(SUPPORT_LIFETIME + Duration::from_millis(10));
    assert!(service.apply(&proof, plan, &deadline()).is_err());
    assert_eq!(f.mutations(), 0);
}

#[test]
fn malformed_queries_command_failure_timeout_and_cancel_never_resend_mutations() {
    let f = Fixture::new("");
    let service = f.service();
    for (code, stdout, stderr) in [
        (Some(1), vec![], vec![]),
        (None, vec![], vec![]),
        (Some(0), vec![], b"stderr".to_vec()),
        (Some(0), b"Id=bad\nId=duplicate\n".to_vec(), vec![]),
        (Some(0), vec![b'x'; MAX_COMMAND_BYTES + 1], vec![]),
    ] {
        *f.runner.failure.lock().unwrap() = Some((code, stdout, stderr));
        assert!(
            service
                .plan(&f.proof(), ServiceAction::Enable, &deadline())
                .is_err()
        );
        assert_eq!(f.mutations(), 0);
    }
    f.runner.block_mutation.store(true, Ordering::Release);
    let plan = service
        .plan(&f.proof(), ServiceAction::Enable, &deadline())
        .unwrap();
    let result = service
        .apply(
            &f.proof(),
            plan,
            &Deadline::new(50, Cancellation::default()).unwrap(),
        )
        .unwrap();
    assert_eq!(result.outcome, MutationOutcome::Unknown);
    assert_eq!(f.mutations(), 1);
    f.runner.block_mutation.store(false, Ordering::Release);
    thread::sleep(Duration::from_millis(10));
    let cancel = Cancellation::default();
    let d = Deadline::new(5000, cancel.clone()).unwrap();
    let plan = service
        .plan(&f.proof(), ServiceAction::Enable, &deadline())
        .unwrap();
    cancel.cancel();
    assert!(service.apply(&f.proof(), plan, &d).is_err());
    assert_eq!(f.mutations(), 1);
}

#[test]
fn postflight_drift_prioritizes_runner_error_and_uncertain_overflow() {
    for mutation in [false, true] {
        for drift in [false, true] {
            let f = Fixture::new("");
            let command = f.command(if mutation { "start" } else { "cat" });
            if drift {
                let path = f.io.target().paths().runtime_home.join("systemd/private");
                *f.runner.hook.lock().unwrap() = Some(Box::new(move || replace_socket(&path)));
            }
            *f.runner.runner_error.lock().unwrap() = Some(NativeError::Oversize);
            let result = if mutation {
                f.mutate(&f.proof(), &command)
            } else {
                f.io.run(&command, &deadline())
            };
            assert_eq!(
                result.unwrap_err(),
                if mutation {
                    NativeError::OutcomeUnknown
                } else if drift {
                    NativeError::Foreign
                } else {
                    NativeError::Oversize
                }
            );
            assert_eq!(f.runner.calls.lock().unwrap().len(), 1);
        }
    }
}

#[test]
fn vanished_or_replaced_endpoint_directories_refuse_with_original_child_inodes() {
    for choice in 0..4 {
        let f = Fixture::new("");
        let command = f.command("cat");
        let runtime = &f.io.target().paths().runtime_home;
        let dir = runtime.join("systemd");
        let socket = dir.join("private");
        let original = rustix::fs::stat(&socket).unwrap();
        match choice {
            0 => fs::rename(runtime, runtime.with_extension("gone")).unwrap(),
            1 => fs::rename(&dir, dir.with_extension("gone")).unwrap(),
            2 => {
                let old = dir.with_extension("old");
                fs::rename(&dir, &old).unwrap();
                fs::create_dir(&dir).unwrap();
                fs::rename(old.join("private"), &socket).unwrap();
            }
            _ => {
                let old = runtime.with_extension("old");
                let old_dir = rustix::fs::stat(&dir).unwrap();
                fs::rename(runtime, &old).unwrap();
                fs::create_dir(runtime).unwrap();
                fs::set_permissions(runtime, fs::Permissions::from_mode(0o700)).unwrap();
                fs::rename(old.join("systemd"), &dir).unwrap();
                let now = rustix::fs::stat(&dir).unwrap();
                assert_eq!((now.st_dev, now.st_ino), (old_dir.st_dev, old_dir.st_ino));
            }
        }
        if choice >= 2 {
            let now = rustix::fs::stat(&socket).unwrap();
            assert_eq!((now.st_dev, now.st_ino), (original.st_dev, original.st_ino));
        }
        assert_eq!(
            f.io.run(&command, &deadline()).unwrap_err(),
            NativeError::Foreign
        );
        assert!(f.runner.calls.lock().unwrap().is_empty());
    }
}

#[test]
fn start_capable_plan_and_apply_refuse_unmanaged_bootstrap_or_socket() {
    for action in [ServiceAction::Start, ServiceAction::Restart] {
        for socket in [false, true] {
            for between in [false, true] {
                let f = Fixture::new("");
                let service = f.service();
                let plan = between.then(|| service.plan(&f.proof(), action, &deadline()).unwrap());
                let _listener = if socket {
                    f.io.create_private_dir(&f.proof(), f.io.target().runtime_dir())
                        .unwrap();
                    Some(UnixListener::bind(f.io.target().socket_path()).unwrap())
                } else {
                    f.bootstrap(9, "ready");
                    None
                };
                let result = if let Some(plan) = plan {
                    service.apply(&f.proof(), plan, &deadline()).map(|_| ())
                } else {
                    service.plan(&f.proof(), action, &deadline()).map(|_| ())
                };
                assert!(matches!(result, Err(ServiceError::Foreign)));
                assert_eq!(f.mutations(), 0);
            }
        }
    }
}

#[test]
fn unchanged_state_mutation_failures_and_failed_post_observation_remain_unknown() {
    for action in [
        ServiceAction::Enable,
        ServiceAction::Disable,
        ServiceAction::Reload,
        ServiceAction::Stop,
    ] {
        for failure in 0..4 {
            let f = Fixture::new("");
            let service = f.service();
            if action == ServiceAction::Disable {
                let mut p = f.runner.properties.lock().unwrap();
                p.insert("UnitFileState".into(), "enabled".into());
                p.insert("WantedBy".into(), "graphical-session.target".into());
            } else if action == ServiceAction::Reload {
                f.runner
                    .properties
                    .lock()
                    .unwrap()
                    .insert("NeedDaemonReload".into(), "yes".into());
            } else if action == ServiceAction::Stop {
                f.active();
                f.bootstrap(9, "ready");
            }
            let plan = service.plan(&f.proof(), action, &deadline()).unwrap();
            match failure {
                0 => f.runner.no_change.store(true, Ordering::Release),
                1 => {
                    *f.runner.mutation_failure.lock().unwrap() =
                        Some((Some(1), vec![], b"failed".to_vec()))
                }
                2 => *f.runner.mutation_failure.lock().unwrap() = Some((None, vec![], vec![])),
                _ => *f.runner.post_failure.lock().unwrap() = Some((Some(1), vec![], vec![])),
            }
            let result = service.apply(&f.proof(), plan, &deadline()).unwrap();
            assert_eq!(result.outcome, MutationOutcome::Unknown);
            assert_eq!(f.mutations(), 1);
        }
    }
}

#[test]
fn informational_enable_disable_diagnostics_still_require_verified_manager_state() {
    let f = Fixture::new("");
    let service = f.service();
    for (action, diagnostic) in [
        (
            ServiceAction::Enable,
            "Created symlink '/scratch/graphical-session.target.wants/crosspane-agent.service' → '/scratch/crosspane-agent.service'.\n",
        ),
        (
            ServiceAction::Disable,
            "Removed '/scratch/graphical-session.target.wants/crosspane-agent.service'.\n",
        ),
    ] {
        *f.runner.mutation_stderr.lock().unwrap() = diagnostic.as_bytes().to_vec();
        let plan = service.plan(&f.proof(), action, &deadline()).unwrap();
        let result = service.apply(&f.proof(), plan, &deadline()).unwrap();
        assert_eq!(result.outcome, MutationOutcome::Verified);
        assert_eq!(
            result.after.unwrap().enabled,
            action == ServiceAction::Enable
        );
    }
    assert_eq!(f.mutations(), 2);
}

#[test]
fn only_manager_pid_changed_refuses_plan_and_valid_decoded_status() {
    let f = Fixture::new("");
    let service = f.service();
    f.active();
    f.bootstrap(9, "ready");
    f.runner
        .properties
        .lock()
        .unwrap()
        .insert("MainPID".into(), "4243".into());
    assert!(matches!(
        service.plan(&f.proof(), ServiceAction::Restart, &deadline()),
        Err(ServiceError::Foreign)
    ));
    let facts = service.observe(&deadline()).unwrap();
    let reply = f.reply(9);
    assert!(matches!(
        reply.result,
        Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
    ));
    assert!(matches!(
        service.agent(&facts, Some(&reply), 19, 100, None, &deadline()),
        Err(ServiceError::Foreign)
    ));
    assert_eq!(f.mutations(), 0);
}

#[test]
fn outstanding_manager_operation_retains_payload_lease_until_fake_child_finishes() {
    let f = Fixture::new("");
    let service = f.service();
    let plan = service
        .plan(&f.proof(), ServiceAction::Enable, &deadline())
        .unwrap();
    let (started, start) = std::sync::mpsc::sync_channel(1);
    let (finish, released) = std::sync::mpsc::sync_channel(1);
    *f.runner.mutation_hook.lock().unwrap() = Some(Box::new(move || {
        started.send(()).unwrap();
        // Fake child/cleanup deliberately ignores the operation deadline until explicitly finished.
        released.recv().unwrap();
    }));
    let result = service
        .apply(
            &f.proof(),
            plan,
            &Deadline::new(100, Cancellation::default()).unwrap(),
        )
        .unwrap();
    start.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(result.outcome, MutationOutcome::Unknown);
    assert!(matches!(
        f.io.install_lease(&f.proof()),
        Err(NativeError::Busy)
    ));
    let queries = f.runner.calls.lock().unwrap().len();
    for service in [&service, &f.service()] {
        assert!(matches!(
            service.plan(&f.proof(), ServiceAction::Disable, &deadline()),
            Err(ServiceError::Native(NativeError::Busy))
        ));
    }
    assert_eq!(f.runner.calls.lock().unwrap().len(), queries);
    finish.send(()).unwrap();
    let until = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        match f.io.install_lease(&f.proof()) {
            Ok(lease) => {
                drop(lease);
                break;
            }
            Err(NativeError::Busy) => {
                assert!(std::time::Instant::now() < until);
                thread::yield_now();
            }
            result => panic!("unexpected lease result: {result:?}"),
        }
    }
    let plan = service
        .plan(&f.proof(), ServiceAction::Disable, &deadline())
        .unwrap();
    assert!(f.runner.calls.lock().unwrap().len() > queries);
    drop(plan);
    assert_eq!(f.mutations(), 1);
}

fn dead_service_runtime(f: &Fixture) {
    f.bootstrap(9, "ready");
    let path = f.io.target().runtime_dir().join("bootstrap.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["pid"] = json!(i32::MAX as u32);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let listener = UnixListener::bind(f.io.target().socket_path()).unwrap();
    fs::set_permissions(
        f.io.target().socket_path(),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    drop(listener);
}
#[test]
fn fresh_service_start_recovers_only_exact_dead_state_under_the_install_lease() {
    let f = Fixture::new("-dead");
    dead_service_runtime(&f);
    let proof = f.proof();
    let service = f.service();
    let plan = service
        .plan(&proof, ServiceAction::Start, &deadline())
        .unwrap();
    assert!(f.io.target().socket_path().exists());
    assert!(f.io.target().runtime_dir().join("bootstrap.json").exists());
    let result = service.apply(&proof, plan, &deadline()).unwrap();
    assert_eq!(result.outcome, MutationOutcome::Unknown);
    assert_eq!(f.mutations(), 1);
    assert!(!f.io.target().socket_path().exists());
    assert!(!f.io.target().runtime_dir().join("bootstrap.json").exists());
}
#[test]
fn service_start_retains_changed_or_foreign_runtime_without_dispatching() {
    for variant in 0..3 {
        let f = Fixture::new("-runtime-race");
        dead_service_runtime(&f);
        let proof = f.proof();
        let service = f.service();
        let plan = service
            .plan(&proof, ServiceAction::Start, &deadline())
            .unwrap();
        match variant {
            0 => {
                fs::remove_file(f.io.target().socket_path()).unwrap();
                let listener = UnixListener::bind(f.io.target().socket_path()).unwrap();
                fs::set_permissions(
                    f.io.target().socket_path(),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
                drop(listener);
            }
            1 => fs::write(f.io.target().runtime_dir().join("foreign"), b"foreign").unwrap(),
            _ => {
                let path = f.io.target().runtime_dir().join("bootstrap.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["pid"] = json!(std::process::id());
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        assert!(service.apply(&proof, plan, &deadline()).is_err());
        assert_eq!(f.mutations(), 0);
        assert!(f.io.target().socket_path().exists());
        assert!(f.io.target().runtime_dir().join("bootstrap.json").exists());
    }
}
#[test]
fn successful_systemd_warnings_and_large_irrelevant_fields_preserve_exact_authority() {
    let f = Fixture::new("-show-note");
    let mut stdout = shown_properties(&f.runner.properties.lock().unwrap(), true);
    stdout.extend_from_slice(
        format!(
            "FutureProperty={}\nFutureProperty=duplicate\nunrelated annotation\n",
            "x".repeat(100_000)
        )
        .as_bytes(),
    );
    *f.runner.failure.lock().unwrap() = Some((Some(0), stdout, b"informational warning".to_vec()));
    assert!(f.service().observe(&deadline()).is_ok());
    assert_eq!(f.mutations(), 0);
}
