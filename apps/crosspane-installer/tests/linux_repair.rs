#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{
    agent_contract::*,
    platform::linux::{native_io::*, payload::*, removal::*, service::*},
};
use crosspane_installer_core::{OperationId, ResourceObservation, ResourceOwnership};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
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
    manager: Mutex<Option<uninstall_tests::Manager>>,
    calls: Mutex<Vec<(PathBuf, Vec<String>)>>,
    output: Mutex<FakeOutput>,
    stall: Mutex<bool>,
    erase_gate: Mutex<Option<Arc<lease_dispatch_tests::Gate>>>,
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
            if let Some(manager) = self.manager.lock().unwrap().as_mut() {
                return manager.run(c, d);
            }
            // Deliberately unknown service facts; never pretend stop-zero or missing socket proves exit.
            return Ok(CommandOutput {
                code: Some(1),
                stdout: vec![],
                stderr: vec![],
            });
        }
        assert_eq!(c.argv(), ["erase-identity"]);
        let gate = self.erase_gate.lock().unwrap().clone();
        if let Some(gate) = &gate {
            gate.pause();
        }
        while *self.stall.lock().unwrap() {
            d.check()?;
            thread::sleep(Duration::from_millis(1));
        }
        let result = self
            .output
            .lock()
            .unwrap()
            .clone()
            .map(|(code, stdout, stderr)| CommandOutput {
                code,
                stdout,
                stderr,
            });
        if let Some(gate) = gate {
            gate.finished.store(true, Ordering::Release);
        }
        result
    }
}
struct Probe(Mutex<Result<Option<ProcessFacts>, NativeError>>);
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, d: &Deadline) -> Result<ProcessFacts, NativeError> {
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
    auth: Arc<uninstall_tests::Auth>,
    _listener: UnixListener,
}
#[allow(dead_code)]
impl Fixture {
    fn new(agent: bool) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp419-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        let probe = Arc::new(Probe(Mutex::new(Ok(Some(ProcessFacts {
            uid: rustix::process::geteuid().as_raw(),
            executable: root.join(".local/bin/crosspane-agent"),
            generation: 77,
        })))));
        let runner = Arc::new(Runner { manager: Mutex::default(), calls: Mutex::default(), stall: Mutex::new(false), erase_gate: Mutex::default(),
            output: Mutex::new(Ok((Some(0),br#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"removed"}"#.to_vec(), vec![]))) });
        // Exclusive mkdir: collision is an error. No helper initializes a pre-existing root.
        let auth = Arc::new(uninstall_tests::Auth::default());
        let mut native = LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap();
        native
            .set_scratch_pkexec_runner(Arc::new(uninstall_tests::AuthRunner(auth.clone())))
            .unwrap();
        let io = Arc::new(native);
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
            auth,
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
        self.tracked_with(self.probe.clone())
    }
    fn tracked_with(&self, reader: Arc<dyn ExitReader>) -> Arc<TrackedAgent> {
        let deadline = deadline();
        loop {
            match TrackedAgent::scratch_capture(self.io.clone(), reader.clone(), &deadline) {
                Ok(tracked) => return Arc::new(tracked),
                // Read-only fixture acquisition can race the previous worker's slot release.
                // Retry only Busy under this same deadline; mutations are never retried.
                Err(RemovalError::Native(NativeError::Busy)) => {
                    deadline.check().unwrap();
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture capture failed: {error:?}"),
            }
        }
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
        let mut v: Value = serde_json::from_str(HEALTH).unwrap();
        let i = &mut v["result"]["installer"]["instance"];
        i["uid"] = json!(self.io.target().paths().uid);
        i["exe"] = json!(self.io.target().agent_path());
        i["runtime_dir"] = json!(self.io.target().runtime_dir());
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
    package_for(Architecture::native().unwrap())
}
fn package_for(architecture: Architecture) -> Package {
    package_with_icon(architecture, b"inert resource")
}
fn package_with_icon(architecture: Architecture, icon: &[u8]) -> Package {
    let mut elf = vec![0; 64];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&3u16.to_le_bytes());
    elf[18..20].copy_from_slice(
        &(if architecture == Architecture::X86_64 {
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
            7 => icon.to_vec(),
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
        architecture,
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
    Package::read(archive.as_slice(), architecture, sha256(&archive)).unwrap()
}

fn installed(f: &Fixture, p: &Package) {
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = install
        .plan(&f.proof, p, OperationId(47), MatchingFiles::Preserve)
        .unwrap();
    install.apply(&f.proof, p, plan, &deadline()).unwrap();
    f.bootstrap(9);
    install
        .verify(&f.proof, p, 19, 100, &f.reply(), &deadline())
        .unwrap();
}

mod uninstall_tests {
    #[test]
    fn original_replaced_between_executor_check_and_worker_observation_sends_zero_stop() {
        for reused_pid in [false, true] {
            let f = Fixture::new(false);
            let mut run = start(&f, RemovalSelection::default(), true, true);
            run.disable(&deadline()).unwrap();
            let replaced = Arc::new(AtomicBool::new(false));
            let marked = replaced.clone();
            let (io, proof, probe) = (f.io.clone(), f.proof.clone(), f.probe.clone());
            let mut manager_guard = f.runner.manager.lock().unwrap();
            let manager = manager_guard.as_mut().unwrap();
            manager.shows = 0;
            // Each stable observation reads show twice; the worker starts the third read.
            manager.show_hook = Some((
                3,
                Box::new(move |properties| {
                    if !reused_pid {
                        properties.insert("MainPID".into(), "4343".into());
                    }
                    probe
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .generation += 1;
                    io.atomic_write(&proof, &io.target().runtime_dir().join("bootstrap.json"), &serde_json::to_vec(&json!({"schema_version":1,"instance_id":10,"pid":if reused_pid {4242} else {4343},"started_unix_ms":parse_ps_start(START).unwrap(),"phase":"ready","phase_seq":2,"keystore":"os_store","reason":null,"runtime_dir":io.target().runtime_dir()})).unwrap()).unwrap();
                    marked.store(true, Ordering::Release);
                }),
            ));
            drop(manager_guard);
            let _ = run.stop(&deadline());
            assert!(replaced.load(Ordering::Acquire));
            assert_eq!(
                f.runner
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                    .count(),
                0,
                "reused PID {reused_pid}"
            );
            assert!(run.report().recovery_retained);
            assert!(
                f.io.metadata(&f.io.target().agent_path())
                    .unwrap()
                    .is_some()
            );
        }
    }
    #[test]
    fn actual_firewall_detect_expiry_keeps_pending_then_fresh_stop_records_unknown() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        let (mut fw, mut store) = firewall(&f, &[RuleKind::Mdns]);
        run.disable(&deadline()).unwrap();
        let admitted = receipt(&f, &store, RuleKind::Mdns);
        let mut context = UninstallFirewall {
            firewall: &mut fw,
            support: &f.proof,
            store: &mut store,
            manager: ManagerSelection::Ufw,
        };
        f.auth.detect_stall.store(true, Ordering::Release);
        let result = run.prepare_rule(
            &mut context,
            RuleKind::Mdns,
            admitted,
            OperationId(101),
            // Allow intent preparation to finish; the injected read stall consumes the budget.
            &Deadline::new(2000, Cancellation::default()).unwrap(),
        );
        f.auth.detect_stall.store(false, Ordering::Release);
        assert!(f.auth.detect_entered.load(Ordering::Acquire));
        assert!(f.auth.detect_expired.load(Ordering::Acquire));
        assert!(matches!(
            result,
            Err(UninstallError::Firewall(FirewallError::Native(
                NativeError::Timeout
            )))
        ));
        assert_eq!(run.stage(), UninstallStage::Stop);
        assert_eq!(run.report().progress.mdns, CleanupResult::Unknown);
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let record =
            CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        assert_eq!(record.progress.mdns, CleanupResult::Pending);
        finish_clean(&f, &mut run);
        let record =
            CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        assert_eq!(record.progress.mdns, CleanupResult::Unknown);
        assert_eq!(record.progress.stop, CleanupResult::Removed);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(run.report().recovery_retained);
        assert_eq!(f.erase_count(), 0);
        assert_eq!(
            f.auth
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == "ufw")
                .count(),
            0
        );
    }
    #[test]
    fn resumed_uncertain_file_intent_never_dispatches_again_when_file_remains() {
        for index in [6, 7] {
            let f = Fixture::new(false);
            let mut run = start(&f, RemovalSelection::default(), true, true);
            run.disable(&deadline()).unwrap();
            run.stop(&deadline()).unwrap();
            run.observe_exit(&deadline()).unwrap();
            assert_clean_prerequisite(&run);
            run.identity([0; 32], f.environment(), &deadline()).unwrap();
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            let mut record =
                CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
            record.progress.stage = CleanupStage::FilesObserved;
            // Simulated crash after this row's durable dispatch intent, before any outcome.
            record.progress.resources[index] = CleanupResult::Unknown;
            let target = PayloadInstaller::new(f.io.clone()).unwrap().targets()[index].clone();
            let before = f.io.read(&target, 4096, false).unwrap();
            f.io.atomic_write(&f.proof, &path, &record.encode().unwrap())
                .unwrap();
            drop(run);
            let (plan, consent) = planned(&f, RemovalSelection::default(), None, 2, 200);
            let mut resumed = plan
                .resume(consent, Arc::new(f.service(&package())), &deadline())
                .unwrap();
            resumed.disable(&deadline()).unwrap();
            finish(&f, &mut resumed);
            assert_eq!(
                f.io.read(&target, 4096, false).unwrap(),
                before,
                "row {index}"
            );
            assert_eq!(
                resumed.report().progress.resources[index],
                CleanupResult::Unknown
            );
            assert!(resumed.report().recovery_retained);
            let unattempted = if index == 6 { 7 } else { 6 };
            assert_eq!(
                resumed.report().progress.resources[unattempted],
                CleanupResult::Removed
            );
            assert!(
                f.io.metadata(Path::new(
                    &PayloadInstaller::new(f.io.clone()).unwrap().targets()[unattempted]
                ))
                .unwrap()
                .is_none()
            );
        }
    }
    #[test]
    fn reused_pid_or_changed_bootstrap_or_unknown_process_refuses_before_stop_dispatch() {
        for axis in 0..3 {
            let f = Fixture::new(false);
            let mut run = start(&f, RemovalSelection::default(), true, true);
            run.disable(&deadline()).unwrap();
            match axis {
                0 => {
                    f.probe
                        .0
                        .lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .generation += 1
                }
                1 => f.bootstrap(10),
                _ => *f.probe.0.lock().unwrap() = Err(NativeError::Unavailable),
            }
            let _ = run.stop(&deadline());
            assert_eq!(
                f.runner
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                    .count(),
                0,
                "axis {axis}"
            );
            assert!(run.report().recovery_retained);
            assert!(
                f.io.metadata(&f.io.target().agent_path())
                    .unwrap()
                    .is_some()
            );
        }
    }
    #[test]
    fn reenabled_autostart_after_clean_admission_refuses_erase_and_retains_files() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(&run);
        {
            let mut manager = f.runner.manager.lock().unwrap();
            let properties = &mut manager.as_mut().unwrap().properties;
            properties.insert("UnitFileState".into(), "enabled".into());
            properties.insert("WantedBy".into(), "graphical-session.target".into());
        }
        let hash = sha256(&f.io.read(&f.io.target().agent_path(), 4096, false).unwrap());
        assert!(run.identity(hash, f.environment(), &deadline()).is_err());
        assert_eq!(f.erase_count(), 0);
        assert!(run.report().recovery_retained);
        assert!(run.remove_files(&deadline()).is_err());
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn ledger_drift_before_disable_keeps_last_durable_record_and_sends_no_mutation() {
        let f = Fixture::new(false);
        let mut run = start(&f, RemovalSelection::default(), true, true);
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let before = f.io.read(&path, MAX_RECORD_BYTES, true).unwrap();
        let (ledger_path, mut ledger) = cleanup_ledger(&f);
        ledger["manifest_hash"] = json!("00".repeat(32));
        f.io.atomic_write(
            &f.proof,
            &ledger_path,
            &serde_json::to_vec(&ledger).unwrap(),
        )
        .unwrap();
        assert!(run.disable(&deadline()).is_err());
        assert_eq!(f.io.read(&path, MAX_RECORD_BYTES, true).unwrap(), before);
        assert_eq!(
            f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a
                    .get(1)
                    .is_some_and(|s| ["disable", "stop"].contains(&s.as_str())))
                .count(),
            0
        );
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn replacement_instance_before_files_rejects_before_any_file_deletion() {
        let f = Fixture::new(false);
        let mut run = start(&f, RemovalSelection::default(), true, true);
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(&run);
        run.identity([0; 32], f.environment(), &deadline()).unwrap();
        let prerequisite = run.report();
        let before = PayloadInstaller::new(f.io.clone())
            .unwrap()
            .targets()
            .iter()
            .map(|path| {
                (
                    PathBuf::from(path),
                    f.io.read(Path::new(path), 4 * 1024 * 1024, false).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        f.bootstrap(10);
        let result = run.remove_files(&deadline());
        let changed = before
            .iter()
            .enumerate()
            .filter_map(|(index, (path, bytes))| {
                (f.io.read(path, 4 * 1024 * 1024, false).ok().as_ref() != Some(bytes))
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        assert!(
            result.is_err(),
            "result={result:?}; prerequisite stop={:?}; clean Busy={}; stop Busy={}; changed members={changed:?}; erases={}",
            prerequisite.progress.stop,
            prerequisite.issues.iter().any(|issue| matches!(
                issue,
                UninstallIssue::CleanExit(RemovalError::Native(NativeError::Busy))
            )),
            prerequisite.issues.iter().any(|issue| matches!(
                issue,
                UninstallIssue::Stop(RemovalError::Native(NativeError::Busy))
            )),
            f.erase_count(),
        );
        assert_eq!(run.report().form, UninstallForm::NotClean);
        for (path, bytes) in before {
            assert_eq!(f.io.read(&path, 4 * 1024 * 1024, false).unwrap(), bytes);
        }
    }
    #[test]
    fn erase_wrong_digest_or_environment_refuses_before_spawn() {
        for wrong_environment in [false, true] {
            let f = Fixture::new(false);
            let mut run = start(
                &f,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                },
                true,
                true,
            );
            run.disable(&deadline()).unwrap();
            run.stop(&deadline()).unwrap();
            run.observe_exit(&deadline()).unwrap();
            assert_clean_prerequisite(&run);
            let other = Fixture::new(false);
            let hash = if wrong_environment {
                sha256(&f.io.read(&f.io.target().agent_path(), 4096, false).unwrap())
            } else {
                [0; 32]
            };
            let environment = if wrong_environment {
                other.environment()
            } else {
                f.environment()
            };
            assert!(run.identity(hash, environment, &deadline()).is_err());
            assert_eq!(f.erase_count(), 0);
            assert_eq!(other.erase_count(), 0);
            assert!(
                f.io.metadata(&f.io.target().agent_path())
                    .unwrap()
                    .is_some()
            );
        }
    }
    #[test]
    fn reenabled_autostart_before_clean_observation_keeps_identity_and_recovery() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        {
            let mut manager = f.runner.manager.lock().unwrap();
            let properties = &mut manager.as_mut().unwrap().properties;
            properties.insert("UnitFileState".into(), "enabled".into());
            properties.insert("WantedBy".into(), "graphical-session.target".into());
        }
        run.observe_exit(&deadline()).unwrap();
        let hash = sha256(&f.io.read(&f.io.target().agent_path(), 4096, false).unwrap());
        run.identity(hash, f.environment(), &deadline()).unwrap();
        run.remove_files(&deadline()).unwrap();
        assert_eq!(f.erase_count(), 0);
        assert!(run.report().recovery_retained);
        assert_eq!(run.report().form, UninstallForm::NotClean);
    }

    #[test]
    fn actual_firewall_deadline_records_unknown_on_fresh_stop_call_without_replay() {
        let f = Fixture::new(false);
        let mut old = start(
            &f,
            RemovalSelection {
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        let (mut fw, mut store) = firewall(&f, &[RuleKind::Mdns]);
        old.disable(&deadline()).unwrap();
        let admitted = receipt(&f, &store, RuleKind::Mdns);
        let mut ctx = UninstallFirewall {
            firewall: &mut fw,
            support: &f.proof,
            store: &mut store,
            manager: ManagerSelection::Ufw,
        };
        let plan = old
            .prepare_rule(
                &mut ctx,
                RuleKind::Mdns,
                admitted,
                OperationId(101),
                &deadline(),
            )
            .unwrap();
        let consent = plan.consent(OperationId(101), 1).unwrap();
        f.auth.stall.store(true, Ordering::Release);
        assert!(
            old.apply_rule(
                &mut ctx,
                plan,
                consent,
                &Deadline::new(2000, Cancellation::default()).unwrap()
            )
            .is_ok()
        );
        // Caller timeout precedes retained-worker termination/reaping. Observe completion,
        // rather than racing its cleanup or releasing the injected child stall early.
        let cleanup = deadline();
        while !f.auth.apply_finished.load(Ordering::Acquire) {
            cleanup.check().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        f.auth.stall.store(false, Ordering::Release);
        assert!(f.auth.apply_entered.load(Ordering::Acquire));
        assert!(f.auth.apply_expired.load(Ordering::Acquire));
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let durable =
            CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        assert_eq!(durable.progress.stage, CleanupStage::FirewallObserved);
        assert_eq!(durable.progress.mdns, CleanupResult::Pending);
        assert_eq!(old.stage(), UninstallStage::Stop);
        assert_eq!(old.report().progress.mdns, CleanupResult::Unknown);
        finish_clean(&f, &mut old);
        let durable =
            CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        assert_eq!(durable.progress.mdns, CleanupResult::Unknown);
        assert_eq!(durable.progress.stop, CleanupResult::Removed);
        assert_eq!(old.report().form, UninstallForm::NotClean);
        assert!(old.report().recovery_retained);
        assert_eq!(
            f.auth
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.as_str() == "ufw")
                .count(),
            1
        );
        assert_eq!(f.erase_count(), 0);
    }
    #[test]
    fn crash_at_each_completed_stage_preserves_original_loss_and_does_not_replay_mutations() {
        for cut in 0..4 {
            let f = Fixture::new(false);
            let mut old = start(&f, RemovalSelection::default(), true, true);
            if cut >= 1 {
                old.disable(&deadline()).unwrap();
            }
            if cut >= 2 {
                old.stop(&deadline()).unwrap();
            }
            if cut >= 3 {
                old.observe_exit(&deadline()).unwrap();
                assert_clean_prerequisite(&old);
                old.identity([0; 32], f.environment(), &deadline()).unwrap();
            }
            drop(old);
            let (plan, consent) = planned(&f, RemovalSelection::default(), None, 2, 200);
            let mut resumed = plan
                .resume(consent, Arc::new(f.service(&package())), &deadline())
                .unwrap();
            resumed.disable(&deadline()).unwrap();
            finish(&f, &mut resumed);
            assert_eq!(resumed.report().form, UninstallForm::NotClean, "cut {cut}");
            assert_eq!(f.erase_count(), 0);
            let calls = f.runner.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                    .count(),
                1,
                "cut {cut}"
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|(_, a)| a.get(1).is_some_and(|s| s == "disable"))
                    .count(),
                1,
                "cut {cut}"
            );
        }
    }

    #[test]
    fn timed_out_erase_retains_worker_lease_and_rejects_repeated_dispatch() {
        struct Release(Arc<lease_dispatch_tests::Gate>);
        impl Drop for Release {
            fn drop(&mut self) {
                self.0.release();
                self.0.wait(|| self.0.finished.load(Ordering::Acquire));
            }
        }
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(&run);
        let gate = lease_dispatch_tests::Gate::new();
        let _release = Release(gate.clone());
        *f.runner.erase_gate.lock().unwrap() = Some(gate.clone());
        let hash = sha256(&f.io.read(&f.io.target().agent_path(), 4096, false).unwrap());
        assert!(
            run.identity(
                hash,
                f.environment(),
                &Deadline::new(200, Cancellation::default()).unwrap()
            )
            .is_err()
        );
        assert_eq!(f.erase_count(), 1);
        assert!(run.pending().is_some_and(|p| !p.completed()));
        assert!(run.identity(hash, f.environment(), &deadline()).is_err());
        let fresh = f.io.admit_cleanup(&deadline()).unwrap();
        assert!(matches!(fresh.lease(&deadline()), Err(NativeError::Busy)));
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
        drop(run);
        assert!(matches!(fresh.lease(&deadline()), Err(NativeError::Busy)));
        gate.release();
        gate.wait(|| gate.finished.load(Ordering::Acquire));
    }
    #[test]
    fn reinstalled_matching_unprovenanced_rows_are_retained_without_mutations() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let install = PayloadInstaller::new(f.io.clone()).unwrap();
        let repeat = install
            .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
            .unwrap();
        install.apply(&f.proof, &p, repeat, &deadline()).unwrap();
        install
            .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
            .unwrap();
        let service = known_manager(&f, &p, true);
        let (plan, consent) = planned(&f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
        assert!(plan.actions().iter().all(|a| *a == ResourceAction::Retain));
        let mut run = plan.begin(consent, service, &deadline()).unwrap();
        run.disable(&deadline()).unwrap();
        finish(&f, &mut run);
        assert!(
            run.report()
                .progress
                .resources
                .iter()
                .all(|r| *r == CleanupResult::Kept)
        );
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert_eq!(f.erase_count(), 0);
    }
    #[test]
    fn already_absent_identity_receipt_is_complete_without_extra_deletes() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        *f.runner.output.lock().unwrap()=Ok((Some(0),br#"{"schema_version":1,"result":"already_absent","reason":null,"key":"absent","trust":"absent"}"#.to_vec(),vec![]));
        run.disable(&deadline()).unwrap();
        finish_clean(&f, &mut run);
        assert_eq!(run.report().form, UninstallForm::Complete);
        assert_eq!(f.erase_count(), 1);
        assert!(run.identity([0; 32], f.environment(), &deadline()).is_err());
        assert_eq!(f.erase_count(), 1);
    }
    #[test]
    fn unknown_rule_without_supported_firewall_authority_is_named_and_stop_continues() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                lan_rule: true,
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        assert!(run.stop(&deadline()).is_err()); // Wrong order retires this attempt before dispatch.
        assert_eq!(
            f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                .count(),
            0
        );
        drop(run);
        let (plan, consent) = planned(
            &f,
            RemovalSelection {
                lan_rule: true,
                mdns_rule: true,
                ..Default::default()
            },
            None,
            2,
            200,
        );
        let mut run = plan
            .resume(consent, Arc::new(f.service(&package())), &deadline())
            .unwrap();
        run.disable(&deadline()).unwrap();
        run.retain_rule(RuleKind::Lan, FirewallError::Manual, &deadline())
            .unwrap();
        run.retain_rule(RuleKind::Mdns, FirewallError::CurrentRequired, &deadline())
            .unwrap();
        finish(&f, &mut run);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(run.report().issues.iter().any(|i| matches!(
            i,
            UninstallIssue::Rule(RuleKind::Lan, FirewallError::Manual)
        )));
        assert!(run.report().issues.iter().any(|i| matches!(
            i,
            UninstallIssue::Rule(RuleKind::Mdns, FirewallError::CurrentRequired)
        )));
        assert!(f.auth.trace.lock().unwrap().is_empty());
    }
    #[test]
    fn cancel_before_new_intent_leaves_files_and_commands_untouched() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let (plan, consent) = planned(&f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let count = f.runner.calls.lock().unwrap().len();
        assert!(
            plan.begin(
                consent,
                Arc::new(f.service(&package())),
                &Deadline::new(5000, cancellation).unwrap()
            )
            .is_err()
        );
        assert_eq!(count, f.runner.calls.lock().unwrap().len());
        assert!(
            f.io.metadata(
                &f.io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn repeated_crash_resume_does_not_regress_durable_stage_or_lose_removed_rows() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let service = known_manager(&f, &package(), false);
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        let digest = sha256(&serde_json::to_vec(proof.receipt()).unwrap());
        let lease = proof.lease(&deadline()).unwrap();
        assert!(lease.delete(8, &deadline()).unwrap());
        let mut resources = [CleanupResult::Pending; CLEANUP_FILES.len()];
        resources[8] = CleanupResult::Removed;
        CleanupStore::new(lease)
            .write(
                &CleanupIntent {
                    revision: 1,
                    operation: OperationId(100),
                    ledger_digest: digest,
                    delete_identity: false,
                    lan_rule: false,
                    mdns_rule: false,
                    progress: CleanupProgress {
                        stage: CleanupStage::FilesObserved,
                        resources,
                        autostart: CleanupResult::AlreadyAbsent,
                        stop: CleanupResult::AlreadyAbsent,
                        identity: CleanupResult::Kept,
                        lan: CleanupResult::Kept,
                        mdns: CleanupResult::Kept,
                    },
                },
                &deadline(),
            )
            .unwrap();
        let (plan, consent) = planned(&f, RemovalSelection::default(), None, 2, 200);
        let mut first = plan.resume(consent, service.clone(), &deadline()).unwrap();
        first.disable(&deadline()).unwrap();
        drop(first);
        let (plan, consent) = planned(&f, RemovalSelection::default(), None, 3, 300);
        assert!(plan.resume(consent, service, &deadline()).is_ok());
        assert!(
            f.io.metadata(&PayloadInstaller::new(f.io.clone()).unwrap().targets()[7])
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn changed_service_main_pid_cannot_stop_a_different_process_or_gain_clean_authority() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        f.runner
            .manager
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .properties
            .insert("MainPID".into(), "7777".into());
        let _ = run.stop(&deadline());
        assert_eq!(
            f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                .count(),
            0
        );
        assert_eq!(f.erase_count(), 0);
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
    }

    use crosspane_installer::platform::linux::firewall::{current::*, receipts::*, *};
    use crosspane_types::id::NodeId;
    use std::sync::atomic::AtomicBool;
    #[derive(Default)]
    pub(super) struct Auth {
        present: Mutex<[bool; 2]>,
        unknown: AtomicBool,
        stall: AtomicBool,
        detect_stall: AtomicBool,
        detect_entered: AtomicBool,
        detect_expired: AtomicBool,
        apply_entered: AtomicBool,
        apply_expired: AtomicBool,
        apply_finished: AtomicBool,
        trace: Mutex<Vec<String>>,
        reads: AtomicU64,
    }
    pub(super) struct AuthRunner(pub Arc<Auth>);
    struct AuthChild {
        auth: Arc<Auth>,
        outcome: Option<PkexecOutcome>,
        kind: usize,
        deadline: Deadline,
    }
    impl Drop for AuthChild {
        fn drop(&mut self) {
            self.auth.apply_finished.store(true, Ordering::Release);
        }
    }
    impl PkexecRunner for AuthRunner {
        fn spawn(
            &self,
            c: &PkexecCommand,
            d: &Deadline,
        ) -> Result<Box<dyn PkexecChild>, NativeError> {
            d.check()?;
            assert_eq!(c.executable(), Path::new("/usr/bin/setsid"));
            assert_eq!(
                &c.argv()[..5],
                [
                    "--wait",
                    "/usr/bin/pkexec",
                    "/usr/bin/ufw",
                    "delete",
                    "allow"
                ]
            );
            self.0.trace.lock().unwrap().push("ufw".into());
            let kind = if c.argv().contains(&"5353".into()) {
                1
            } else {
                0
            };
            let outcome = if self.0.unknown.load(Ordering::Acquire) {
                PkexecOutcome::TimedOut
            } else {
                PkexecOutcome::Exited {
                    code: 0,
                    stdout: vec![],
                    stderr: vec![],
                    stdout_truncated: false,
                    stderr_truncated: false,
                }
            };
            Ok(Box::new(AuthChild {
                auth: self.0.clone(),
                outcome: Some(outcome),
                kind,
                deadline: d.clone(),
            }))
        }
    }
    impl PkexecChild for AuthChild {
        fn poll(&mut self) -> Result<Option<PkexecOutcome>, NativeError> {
            if self.auth.stall.load(Ordering::Acquire) {
                self.auth.apply_entered.store(true, Ordering::Release);
                return Ok(None);
            }
            if matches!(&self.outcome, Some(PkexecOutcome::Exited { code: 0, .. })) {
                self.auth.present.lock().unwrap()[self.kind] = false;
            }
            Ok(self.outcome.take())
        }
        fn terminate(&mut self) {
            self.auth.apply_expired.store(
                self.deadline.check() == Err(NativeError::Timeout),
                Ordering::Release,
            );
            self.auth.trace.lock().unwrap().push("terminate".into());
            self.outcome = None;
        }
        fn reaped(&mut self) -> bool {
            self.outcome.is_none()
        }
    }
    struct RuleReads(Arc<Auth>);
    impl FirewallReader for RuleReads {
        fn file(&self, r: SystemRead, d: &Deadline) -> Result<Vec<u8>, NativeError> {
            if self.0.detect_stall.load(Ordering::Acquire) {
                self.0.detect_entered.store(true, Ordering::Release);
            }
            while self.0.detect_stall.load(Ordering::Acquire) {
                if let Err(error) = d.check() {
                    self.0
                        .detect_expired
                        .store(error == NativeError::Timeout, Ordering::Release);
                    return Err(error);
                }
                thread::sleep(Duration::from_millis(1));
            }
            d.check()?;
            self.0.reads.fetch_add(1, Ordering::Relaxed);
            match r {
                SystemRead::UfwConfig => Ok(b"ENABLED=yes\n".to_vec()),
                SystemRead::UfwRules6 => {
                    Ok(b"*filter\n### RULES ###\n### END RULES ###\nCOMMIT\n".to_vec())
                }
                SystemRead::UfwRules => {
                    let mut text = "*filter\n### RULES ###\n".to_string();
                    for (i, present) in self.0.present.lock().unwrap().iter().enumerate() {
                        if !present {
                            continue;
                        }
                        let (ports, args, comment) = if i == 0 {
                            (
                                "47811:47812",
                                "-m multiport --dports 47811:47812",
                                "Crosspane (LAN)",
                            )
                        } else {
                            ("5353", "--dport 5353", "Crosspane (mDNS)")
                        };
                        let hex = comment
                            .bytes()
                            .map(|v| format!("{v:02x}"))
                            .collect::<String>();
                        text.push_str(&format!("### tuple ### allow udp {ports} 0.0.0.0/0 any 192.168.4.0/24 in comment={hex}\n-A ufw-user-input -p udp {args} -s 192.168.4.0/24 -j ACCEPT\n"));
                    }
                    text.push_str("### END RULES ###\nCOMMIT\n");
                    Ok(text.into_bytes())
                }
                _ => panic!("unapproved fake file"),
            }
        }
        fn command(&self, r: FirewallRead, d: &Deadline) -> Result<CommandOutput, NativeError> {
            d.check()?;
            self.0.reads.fetch_add(1, Ordering::Relaxed);
            let bytes=match r {
                FirewallRead::Activity=>b"active\n".to_vec(),
                FirewallRead::Addresses=>br#"[{"ifname":"enp1s0","flags":["UP","LOWER_UP"],"link_type":"ether","addr_info":[{"family":"inet","scope":"global","local":"192.168.4.31","prefixlen":24}]}]"#.to_vec(),
                FirewallRead::Default4=>br#"[{"dst":"default","dev":"enp1s0"}]"#.to_vec(),
                FirewallRead::Default6=>b"[]".to_vec(),
            };
            Ok(CommandOutput {
                code: Some(0),
                stdout: bytes,
                stderr: vec![],
            })
        }
    }
    struct LiveReader {
        io: Arc<LinuxNativeIo>,
        probe: Arc<Probe>,
        auth: Arc<Auth>,
        stamp: u64,
    }
    impl CurrentReader for LiveReader {
        fn read(
            &mut self,
            target: &LinuxTarget,
            _: &LanLink,
            peer: NodeId,
            d: &Deadline,
        ) -> Result<CurrentObservations, NativeError> {
            d.check()?;
            assert_eq!(target.paths(), self.io.target().paths());
            assert!(
                self.probe.0.lock().unwrap().as_ref().unwrap().is_some(),
                "current evidence must precede stop"
            );
            self.auth.trace.lock().unwrap().push("current".into());
            self.stamp += 20;
            let mut value: Value = serde_json::from_str(HEALTH).unwrap();
            let s = &mut value["result"]["installer"];
            s["instance"]["uid"] = json!(target.paths().uid);
            s["instance"]["exe"] = json!(target.agent_path());
            s["instance"]["runtime_dir"] = json!(target.runtime_dir());
            s["peers"] = json!([{"node":peer,"name":"inert","connected":true,"link_generation":2,"features":[],
                "grants_given":[],"last_source_parking":null,"counters":{
                "e1_controller_started":0,"e1_controller_ended":0,"e1_target_started":0,"e1_target_ended":0,
                "e1_injections_ok":0,"e1_hud_shows":0,"e1_chord_releases":0,"e1_command_releases":0,
                "e2_source_started":0,"e2_source_returned":0,"e2_dest_started":0,"e2_dest_returned":0,
                "e2_frames_presented":null,"e2_returns_failed":0}}]);
            let reply = |id, at| AgentReply {
                id,
                observed_at_ms: at,
                source: ObservationSource::Demo,
                result: decode_reply(
                    &InstallerRequest::Status,
                    &serde_json::to_vec(&value).unwrap(),
                    AgentPlatform::Linux,
                ),
            };
            let a = reply(self.stamp, self.stamp);
            let b = reply(self.stamp + 1, self.stamp + 1);
            assert!(matches!(
                &a.result,
                Ok(DecodedReply::Status(StatusAdmission::Supported(_)))
            ));
            Ok(CurrentObservations {
                peer,
                connection: a,
                discovery: b,
            })
        }
        fn dispatch_stamp_ms(&self) -> Result<u64, NativeError> {
            Ok(self.stamp + 2)
        }
    }
    fn firewall(f: &Fixture, kinds: &[RuleKind]) -> (LinuxFirewall, DurableIntentStore) {
        let mut fw =
            LinuxFirewall::scratch(f.io.clone(), Arc::new(RuleReads(f.auth.clone()))).unwrap();
        fw.install_current_reader(
            NodeId([2; 32]),
            Box::new(LiveReader {
                io: f.io.clone(),
                probe: f.probe.clone(),
                auth: f.auth.clone(),
                stamp: 100,
            }),
        );
        let mut store = DurableIntentStore::open(&mut fw, &f.proof).unwrap();
        for kind in kinds {
            let i = FirewallIntent {
                operation: OperationId(if *kind == RuleKind::Lan { 1 } else { 2 }),
                revision: 1,
                target: f.io.target().paths().clone(),
                kind: *kind,
                link: LanLink {
                    interface: "enp1s0".into(),
                    cidr: LanCidr::parse("192.168.4.0/24").unwrap(),
                    default_route: true,
                },
            };
            store.record_intent(&f.proof, &i).unwrap();
            store
                .record_outcome(&f.proof, &i, RuleResult::PendingVerification)
                .unwrap();
            f.auth.present.lock().unwrap()[usize::from(*kind == RuleKind::Mdns)] = true;
        }
        (fw, store)
    }
    fn receipt(f: &Fixture, store: &DurableIntentStore, kind: RuleKind) -> AdmittedReceipt {
        let bytes = store
            .receipt(
                &f.proof,
                OperationId(if kind == RuleKind::Lan { 1 } else { 2 }),
            )
            .unwrap();
        store.admit_receipt(&f.proof, &bytes).unwrap()
    }
    #[test]
    fn present_lan_and_mdns_remove_with_separate_consent_and_live_evidence_before_stop() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                lan_rule: true,
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        let (mut fw, mut store) = firewall(&f, &[RuleKind::Lan, RuleKind::Mdns]);
        run.disable(&deadline()).unwrap();
        for (kind, op) in [(RuleKind::Lan, 101), (RuleKind::Mdns, 102)] {
            let admitted = receipt(&f, &store, kind);
            let mut ctx = UninstallFirewall {
                firewall: &mut fw,
                support: &f.proof,
                store: &mut store,
                manager: ManagerSelection::Ufw,
            };
            let plan = run
                .prepare_rule(&mut ctx, kind, admitted, OperationId(op), &deadline())
                .unwrap();
            assert!(plan.preview().contains("global rule"));
            let consent = plan.consent(OperationId(op), 1).unwrap();
            assert_eq!(format!("{plan:?}"), "UninstallRulePlan(..)");
            assert_eq!(format!("{consent:?}"), "UninstallRuleConsent(..)");
            assert_eq!(format!("{ctx:?}"), "UninstallFirewall(..)");
            run.apply_rule(&mut ctx, plan, consent, &deadline())
                .unwrap();
        }
        assert_eq!(
            *f.auth.trace.lock().unwrap(),
            ["current", "ufw", "current", "ufw"]
        );
        assert!(f.probe.0.lock().unwrap().as_ref().unwrap().is_some());
        finish_clean(&f, &mut run);
        assert_eq!(run.report().form, UninstallForm::Complete);
        assert_eq!(run.report().progress.lan, CleanupResult::AlreadyAbsent);
        assert_eq!(run.report().progress.mdns, CleanupResult::AlreadyAbsent);
        assert_eq!(*f.auth.present.lock().unwrap(), [false, false]);
    }
    #[test]
    fn firewall_unknown_is_durable_stop_continues_and_recovery_tools_stay() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        let (mut fw, mut store) = firewall(&f, &[RuleKind::Mdns]);
        f.auth.unknown.store(true, Ordering::Release);
        run.disable(&deadline()).unwrap();
        let admitted = receipt(&f, &store, RuleKind::Mdns);
        let mut ctx = UninstallFirewall {
            firewall: &mut fw,
            support: &f.proof,
            store: &mut store,
            manager: ManagerSelection::Ufw,
        };
        let plan = run
            .prepare_rule(
                &mut ctx,
                RuleKind::Mdns,
                admitted,
                OperationId(101),
                &deadline(),
            )
            .unwrap();
        let consent = plan.consent(OperationId(101), 1).unwrap();
        run.apply_rule(&mut ctx, plan, consent, &deadline())
            .unwrap();
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let durable =
            CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        assert_eq!(durable.progress.mdns, CleanupResult::Unknown);
        assert_eq!(run.stage(), UninstallStage::Stop);
        finish_clean(&f, &mut run);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(run.report().issues.iter().any(|i| matches!(
            i,
            UninstallIssue::RuleOutcome(RuleKind::Mdns, RuleResult::OutcomeUnknown, _)
        )));
        assert!(
            run.report().progress.resources[..6]
                .iter()
                .enumerate()
                .all(|(i, r)| *r
                    == if i == 4 {
                        CleanupResult::AlreadyAbsent
                    } else {
                        CleanupResult::Kept
                    })
        );
        assert!(!f.probe.0.lock().unwrap().as_ref().unwrap().is_some());
        assert_eq!(
            f.auth
                .trace
                .lock()
                .unwrap()
                .iter()
                .filter(|e| e.as_str() == "ufw")
                .count(),
            1
        );
    }
    #[test]
    fn admitted_wrong_rule_kind_refuses_before_any_firewall_io() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                mdns_rule: true,
                ..Default::default()
            },
            true,
            true,
        );
        let (mut fw, mut store) = firewall(&f, &[RuleKind::Lan]);
        run.disable(&deadline()).unwrap();
        let admitted = receipt(&f, &store, RuleKind::Lan);
        assert_eq!(admitted.kind(), RuleKind::Lan);
        let count = f.auth.reads.load(Ordering::Acquire);
        let mut ctx = UninstallFirewall {
            firewall: &mut fw,
            support: &f.proof,
            store: &mut store,
            manager: ManagerSelection::Ufw,
        };
        assert!(
            run.prepare_rule(
                &mut ctx,
                RuleKind::Mdns,
                admitted,
                OperationId(101),
                &deadline()
            )
            .is_err()
        );
        assert_eq!(f.auth.reads.load(Ordering::Acquire), count);
        assert!(f.auth.trace.lock().unwrap().is_empty());
    }

    #[test]
    fn each_unclean_or_mismatched_exit_fact_keeps_recovery_and_selected_identity() {
        for axis in 0..9 {
            let f = Fixture::new(false);
            let mut run = start(
                &f,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                },
                false,
                true,
            );
            *f.probe.0.lock().unwrap() = Ok(None);
            if axis != 0 {
                f.exit(|v| match axis {
                    1 => {
                        v["instance_id"] = json!(10);
                    }
                    2 => {
                        v["stopped_unix_ms"] = json!(1);
                    }
                    3 => {
                        v["parking"] = json!("failed");
                        v["clean"] = json!(false);
                    }
                    4 => {
                        v["input_journals_empty"] = json!(false);
                        v["clean"] = json!(false);
                    }
                    5 => {
                        v["audio_stopped"] = json!(false);
                        v["clean"] = json!(false);
                    }
                    6 => {
                        v["clean"] = json!(false);
                    } // codec rejects contradictory otherwise-clean facts
                    _ => {}
                });
            }
            if axis == 7 {
                f.bootstrap(10);
            }
            if axis == 8 {
                *f.probe.0.lock().unwrap() = Ok(Some(ProcessFacts {
                    uid: f.io.target().paths().uid,
                    executable: f.io.target().agent_path(),
                    generation: 88,
                }));
            }
            run.disable(&deadline()).unwrap();
            finish(&f, &mut run);
            assert_eq!(run.report().form, UninstallForm::NotClean, "axis {axis}");
            assert_eq!(f.erase_count(), 0, "axis {axis}");
            assert!(
                run.report().progress.resources[..6]
                    .iter()
                    .enumerate()
                    .all(|(i, r)| *r
                        == if i == 4 {
                            CleanupResult::AlreadyAbsent
                        } else {
                            CleanupResult::Kept
                        })
            );
        }
    }
    #[test]
    fn erase_literal_refused_waiting_failed_and_kept_trust_keep_the_tools() {
        for bytes in [
            r#"{"schema_version":1,"result":"refused","reason":"agent_running","key":"kept","trust":"kept"}"#,
            r#"{"schema_version":1,"result":"waiting","reason":"keystore_locked","key":"kept","trust":"kept"}"#,
            r#"{"schema_version":1,"result":"failed","reason":"io","key":"removed","trust":"failed"}"#,
            r#"{"schema_version":1,"result":"removed","reason":null,"key":"removed","trust":"kept"}"#,
        ] {
            let f = Fixture::new(false);
            let mut run = start(
                &f,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                },
                true,
                true,
            );
            *f.runner.output.lock().unwrap() = Ok((Some(0), bytes.as_bytes().to_vec(), vec![]));
            run.disable(&deadline()).unwrap();
            finish_clean(&f, &mut run);
            assert_eq!(f.erase_count(), 1);
            assert_eq!(run.report().progress.identity, CleanupResult::Refused);
            assert_eq!(run.report().form, UninstallForm::NotClean);
            assert!(run.report().identity_receipt.is_some());
            assert!(
                run.report().progress.resources[..6]
                    .iter()
                    .enumerate()
                    .all(|(i, r)| *r
                        == if i == 4 {
                            CleanupResult::AlreadyAbsent
                        } else {
                            CleanupResult::Kept
                        })
            );
        }
    }
    #[test]
    fn malformed_or_nonzero_erase_retires_without_a_retry_and_keeps_recovery() {
        for (code, bytes) in [
            (Some(0), b"{}\n".to_vec()),
            (Some(0), vec![b'x'; 4097]),
            (Some(1), b"{}\n".to_vec()),
        ] {
            let f = Fixture::new(false);
            let mut run = start(
                &f,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                },
                true,
                true,
            );
            *f.runner.output.lock().unwrap() = Ok((code, bytes, vec![]));
            run.disable(&deadline()).unwrap();
            run.stop(&deadline()).unwrap();
            run.observe_exit(&deadline()).unwrap();
            assert_clean_prerequisite(&run);
            let hash = sha256(&f.io.read(&f.io.target().agent_path(), 4096, false).unwrap());
            assert!(run.identity(hash, f.environment(), &deadline()).is_err());
            assert!(run.identity(hash, f.environment(), &deadline()).is_err());
            assert_eq!(f.erase_count(), 1);
            assert_eq!(run.report().form, UninstallForm::NotClean);
            assert!(
                f.io.metadata(&f.io.target().agent_path())
                    .unwrap()
                    .is_some()
            );
        }
    }
    #[test]
    fn resume_loses_original_watch_never_replays_erase_and_observes_completed_disable() {
        let f = Fixture::new(false);
        let mut old = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        old.disable(&deadline()).unwrap();
        old.stop(&deadline()).unwrap();
        old.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(&old);
        drop(old);
        let service = Arc::new(f.service(&package()));
        let (plan, consent) = planned(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            None,
            2,
            200,
        );
        let mut run = plan.resume(consent, service, &deadline()).unwrap();
        run.disable(&deadline()).unwrap();
        finish(&f, &mut run);
        assert_eq!(f.erase_count(), 0);
        let stops = f
            .runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
            .count();
        assert_eq!(stops, 1);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(run.report().recovery_retained);
    }
    #[test]
    fn contradictory_resume_resources_or_selection_reject_before_mutation() {
        for axis in 0..3 {
            let f = Fixture::new(false);
            let old = start(&f, RemovalSelection::default(), true, true);
            drop(old);
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            let mut record =
                CleanupIntent::decode(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
            match axis {
                0 => {
                    record.progress.resources[0] = CleanupResult::Removed;
                    record.progress.stage = CleanupStage::FilesObserved;
                }
                1 => {
                    record.delete_identity = true;
                }
                _ => {
                    record.progress.stage = CleanupStage::Finished;
                }
            }
            f.io.atomic_write(&f.proof, &path, &record.encode().unwrap())
                .unwrap();
            let (plan, consent) = planned(&f, RemovalSelection::default(), None, 2, 200);
            let commands = f.runner.calls.lock().unwrap().len();
            assert!(
                plan.resume(consent, Arc::new(f.service(&package())), &deadline())
                    .is_err()
            );
            assert_eq!(commands, f.runner.calls.lock().unwrap().len());
        }
    }
    #[test]
    fn superseded_coherent_plan_and_consent_fail_before_any_native_io() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let cp = CleanupPlanner::default();
        let up = UninstallPlanner::default();
        let make = |r, o| {
            let inv =
                CleanupInventory::admit(f.io.admit_cleanup(&deadline()).unwrap(), &deadline())
                    .unwrap();
            let c = cp
                .plan(inv, r, OperationId(o), RemovalSelection::default())
                .unwrap();
            let cc = cp.consent(&c, r, OperationId(o), &deadline()).unwrap();
            let p = up.plan(c, f.io.clone(), Some(f.tracked())).unwrap();
            let consent = up.consent(&p, cc, r, OperationId(o)).unwrap();
            (p, consent)
        };
        let (old, consent) = make(1, 100);
        let (_new, _) = make(2, 200);
        let commands = f.runner.calls.lock().unwrap().len();
        assert!(
            old.begin(consent, Arc::new(f.service(&package())), &deadline())
                .is_err()
        );
        assert_eq!(commands, f.runner.calls.lock().unwrap().len());
        assert!(
            f.io.metadata(
                &f.io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json")
            )
            .unwrap()
            .is_none()
        );
    }
    #[test]
    fn new_uninstall_public_debug_is_type_only() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let (plan, consent) = planned(&f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
        let values = [
            format!("{plan:?}"),
            format!("{consent:?}"),
            format!("{:?}", UninstallPlanner::default()),
            format!("{:?}", UninstallError::Invalid),
            format!("{:?}", UninstallIssue::CleanExit(RemovalError::NotClean)),
        ];
        assert_eq!(
            values,
            [
                "UninstallPlan(..)",
                "UninstallConsent(..)",
                "UninstallPlanner(..)",
                "UninstallError(..)",
                "UninstallIssue(..)"
            ]
        );
        let run = plan
            .begin(consent, known_manager(&f, &package(), true), &deadline())
            .unwrap();
        assert_eq!(format!("{run:?}"), "UninstallRun(..)");
        assert_eq!(format!("{:?}", run.report()), "UninstallReport(..)");
    }

    use super::*;
    use crosspane_installer::platform::linux::removal::executor::*;
    type ShowHook = Box<dyn FnOnce(&mut BTreeMap<String, String>) + Send>;
    pub(super) struct Manager {
        properties: BTreeMap<String, String>,
        cat: Vec<u8>,
        on_stop: Option<Box<dyn Fn() + Send>>,
        shows: u64,
        show_hook: Option<(u64, ShowHook)>,
    }
    impl Manager {
        pub(super) fn run(
            &mut self,
            c: &CommandSpec,
            d: &Deadline,
        ) -> Result<CommandOutput, NativeError> {
            d.check()?;
            assert_eq!(c.argv()[0], "--user");
            if c.argv()[1] == "show" {
                self.shows += 1;
                if self
                    .show_hook
                    .as_ref()
                    .is_some_and(|(at, _)| *at == self.shows)
                {
                    let (_, hook) = self.show_hook.take().unwrap();
                    hook(&mut self.properties);
                }
            }
            let p = &mut self.properties;
            let (code, stdout) = match c.argv()[1].as_str() {
                "show" => (
                    0,
                    p.iter()
                        .filter(|(_, v)| c.argv().iter().any(|a| a == "--all") || !v.is_empty())
                        .map(|(k, v)| format!("{k}={v}\n"))
                        .collect::<String>()
                        .into_bytes(),
                ),
                "cat" => (0, self.cat.clone()),
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
                "disable" => {
                    p.insert("UnitFileState".into(), "disabled".into());
                    p.insert("WantedBy".into(), "".into());
                    (0, vec![])
                }
                "stop" => {
                    p.insert("ActiveState".into(), "inactive".into());
                    p.insert("SubState".into(), "dead".into());
                    p.insert("MainPID".into(), "0".into());
                    if let Some(effect) = &self.on_stop {
                        effect();
                    }
                    (0, vec![])
                }
                _ => panic!("unapproved fake mutation"),
            };
            Ok(CommandOutput {
                code: Some(code),
                stdout,
                stderr: vec![],
            })
        }
    }
    fn known_manager(f: &Fixture, p: &Package, clean: bool) -> Arc<LinuxService> {
        let resources = PayloadInstaller::new(f.io.clone())
            .unwrap()
            .rendered_resources(p)
            .unwrap();
        let unit = &resources[0];
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
        p.insert(
            "FragmentPath".into(),
            unit.target.to_string_lossy().into_owned(),
        );
        let exe = f.io.target().agent_path().to_string_lossy().into_owned();
        p.insert("ExecStart".into(),format!("{{ path={exe} ; argv[]={exe} run ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert("ExecStartEx".into(),format!("{{ path={exe} ; argv[]={exe} run ; flags= ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}"));
        p.insert(
            "Environment".into(),
            [
                format!(
                    "XDG_CONFIG_HOME={}",
                    f.io.target().paths().config_home.display()
                ),
                format!(
                    "XDG_STATE_HOME={}",
                    f.io.target().paths().state_home.display()
                ),
                format!(
                    "XDG_RUNTIME_DIR={}",
                    f.io.target().paths().runtime_home.display()
                ),
                format!(
                    "CROSSPANE_RUNTIME_DIR={}",
                    f.io.target().runtime_dir().display()
                ),
            ]
            .iter()
            .map(|v| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\"")))
            .collect::<Vec<_>>()
            .join(" "),
        );

        p.insert("WantedBy".into(), "graphical-session.target".into());
        let mut cat = format!("# {}\n", unit.target.display()).into_bytes();
        cat.extend(&unit.bytes);
        let (io, proof, probe) = (f.io.clone(), f.proof.clone(), f.probe.clone());
        let on_stop = clean.then(|| {
            Box::new(move || {
                *probe.0.lock().unwrap() = Ok(None);
                let bytes = serde_json::to_vec(&json!({"schema_version":1,"instance_id":9,
                "stopped_unix_ms":parse_ps_start(START).unwrap()+1000,"clean":true,
                "parking":"restored","input_journals_empty":true,"audio_stopped":true}))
                .unwrap();
                io.atomic_write(
                    &proof,
                    &io.target()
                        .paths()
                        .state_home
                        .join("crosspane/last_exit.json"),
                    &bytes,
                )
                .unwrap();
            }) as Box<dyn Fn() + Send>
        });
        *f.runner.manager.lock().unwrap() = Some(Manager {
            properties: p,
            cat,
            on_stop,
            shows: 0,
            show_hook: None,
        });
        Arc::new(f.service(&package()))
    }
    fn planned(
        f: &Fixture,
        selection: RemovalSelection,
        watch: Option<Arc<TrackedAgent>>,
        revision: u64,
        operation: u64,
    ) -> (UninstallPlan, UninstallConsent) {
        let cleanup_planner = CleanupPlanner::default();
        let inventory =
            CleanupInventory::admit(f.io.admit_cleanup(&deadline()).unwrap(), &deadline()).unwrap();
        let cleanup = cleanup_planner
            .plan(inventory, revision, OperationId(operation), selection)
            .unwrap();
        let cc = cleanup_planner
            .consent(&cleanup, revision, OperationId(operation), &deadline())
            .unwrap();
        let planner = UninstallPlanner::default();
        let plan = planner.plan(cleanup, f.io.clone(), watch).unwrap();
        let consent = planner
            .consent(&plan, cc, revision, OperationId(operation))
            .unwrap();
        (plan, consent)
    }
    fn start(
        f: &Fixture,
        selection: RemovalSelection,
        clean: bool,
        original: bool,
    ) -> UninstallRun {
        let p = package();
        installed(f, &p);
        let service = known_manager(f, &p, clean);
        assert!(service.observe(&deadline()).unwrap().enabled);
        let (plan, consent) = planned(f, selection, original.then(|| f.tracked()), 1, 100);
        plan.begin(consent, service, &deadline()).unwrap()
    }
    fn finish(f: &Fixture, run: &mut UninstallRun) {
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        finish_files(f, run);
    }
    #[test]
    fn legacy_cleanup_old_ten_removes_tutorial_only_after_genuine_clean_exit() {
        for clean in [false, true] {
            let f = Fixture::new(false);
            let p = package();
            installed(&f, &p);
            let leaf = legacy_cleanup_ledger(&f);
            let service = known_manager(&f, &p, clean);
            let (plan, consent) =
                planned(&f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
            let mut run = plan.begin(consent, service, &deadline()).unwrap();
            run.disable(&deadline()).unwrap();
            finish(&f, &mut run);
            assert_eq!(run.report().progress.resources.len(), 10);
            assert_eq!(leaf.exists(), !clean);
            assert_eq!(
                run.report().progress.resources[4],
                if clean {
                    CleanupResult::Removed
                } else {
                    CleanupResult::Kept
                }
            );
        }
    }
    fn finish_clean(f: &Fixture, run: &mut UninstallRun) {
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(run);
        finish_files(f, run);
    }
    fn assert_clean_prerequisite(run: &UninstallRun) {
        let report = run.report();
        let failures = report
            .issues
            .iter()
            .filter_map(|issue| match issue {
                UninstallIssue::Stop(error) => Some(format!("stop:{error:?}")),
                UninstallIssue::CleanExit(error) => Some(format!("clean:{error:?}")),
                _ => None,
            })
            .collect::<Vec<_>>();
        // observe_exit changes stop to Unknown whenever no genuine clean token was obtained.
        assert!(
            matches!(
                report.progress.stop,
                CleanupResult::Removed | CleanupResult::AlreadyAbsent
            ),
            "clean prerequisite absent: stop={:?}; failures={failures:?}",
            report.progress.stop
        );
        assert!(
            failures.is_empty(),
            "clean prerequisite failures={failures:?}"
        );
    }
    fn finish_files(f: &Fixture, run: &mut UninstallRun) {
        let bytes =
            f.io.read(&f.io.target().agent_path(), 4 * 1024 * 1024, false)
                .unwrap();
        run.identity(sha256(&bytes), f.environment(), &deadline())
            .unwrap();
        run.remove_files(&deadline()).unwrap();
    }
    fn r2_record(f: &Fixture, stage: &str) -> PathBuf {
        let paths = f.io.target().paths();
        let path = paths
            .state_home
            .join("crosspane/installer/repair-intent.json");
        let value = json!({"version":1,"operation":99,"revision":99,
            "manifest":sha256(&serde_json::to_vec(package().manifest()).unwrap()),
            "original_instance":9,"stage":stage,"target":{"uid":paths.uid,
            "roots":[paths.home,paths.prefix,paths.config_home,paths.state_home,paths.data_home,paths.runtime_home],
            "runtime_override":paths.runtime_override,"scratch":true}});
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        path
    }
    fn r2_start(f: &Fixture, valid: bool) -> (UninstallRun, PathBuf) {
        let p = package();
        installed(f, &p);
        let service = known_manager(f, &p, true);
        let path = r2_record(f, "stopped");
        if !valid {
            fs::write(&path, b"unknown repair record").unwrap();
        }
        let (plan, consent) = planned(f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
        (plan.begin(consent, service, &deadline()).unwrap(), path)
    }
    #[test]
    fn r2_ordered_uninstall_deletes_the_captured_fixed_repair_intent_only_after_clean_exit() {
        let f = Fixture::new(false);
        let (mut run, path) = r2_start(&f, true);
        run.disable(&deadline()).unwrap();
        assert!(path.exists());
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert!(path.exists(), "stop/exit alone never deletes recovery");
        let agent =
            f.io.read(&f.io.target().agent_path(), 4 * 1024 * 1024, false)
                .unwrap();
        run.identity(sha256(&agent), f.environment(), &deadline())
            .unwrap();
        let result = run.remove_files(&deadline());
        let issues = run
            .report()
            .issues
            .iter()
            .map(|i| match i {
                UninstallIssue::RepairIntent(e) | UninstallIssue::Resource(_, e) => e.to_string(),
                _ => format!("{i:?}"),
            })
            .collect::<Vec<_>>();
        assert!(
            result.is_ok(),
            "result {} issues {issues:?}",
            result.unwrap_err()
        );
        assert!(
            !path.exists(),
            "receipt-bound clean uninstall must retire the fixed repair journal"
        );
        assert_eq!(run.report().form, UninstallForm::Complete);
    }
    #[test]
    fn r2_uninstall_retains_unknown_repair_records_and_replaced_captured_journals() {
        for replacement in [false, true] {
            let f = Fixture::new(false);
            let (mut run, path) = r2_start(&f, replacement);
            run.disable(&deadline()).unwrap();
            run.stop(&deadline()).unwrap();
            run.observe_exit(&deadline()).unwrap();
            let agent =
                f.io.read(&f.io.target().agent_path(), 4 * 1024 * 1024, false)
                    .unwrap();
            run.identity(sha256(&agent), f.environment(), &deadline())
                .unwrap();
            if replacement {
                let bytes = fs::read(&path).unwrap();
                fs::rename(&path, path.with_extension("original")).unwrap();
                f.io.atomic_write(&f.proof, &path, &bytes).unwrap();
            }
            let before = fs::read(&path).unwrap();
            let _ = run.remove_files(&deadline());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_ne!(run.report().form, UninstallForm::Complete);
            assert!(
                f.io.target().agent_path().exists(),
                "unknown repair recovery is retained"
            );
        }
    }
    #[test]
    fn r2_a_foreign_shaped_repair_journal_never_blocks_removal_and_is_kept() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let service = known_manager(&f, &p, true);
        let path = r2_record(&f, "stopped");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::read(&path).unwrap();
        let (plan, consent) = planned(&f, RemovalSelection::default(), Some(f.tracked()), 1, 100);
        let mut run = plan.begin(consent, service, &deadline()).unwrap();
        run.disable(&deadline()).unwrap();
        finish_clean(&f, &mut run);
        assert_eq!(fs::read(&path).unwrap(), before);
        let report = run.report();
        assert_ne!(report.form, UninstallForm::Complete);
        assert!(report.recovery_retained);
        assert!(f.io.target().agent_path().exists(), "recovery is kept");
        let installer = PayloadInstaller::new(f.io.clone()).unwrap();
        for index in [6, 7, 8, 9] {
            assert!(
                f.io.metadata(&installer.targets()[index])
                    .unwrap()
                    .is_none(),
                "row {index} is still removed"
            );
        }
    }
    #[test]
    fn r2_resume_after_a_crash_in_the_files_stage_never_deletes_the_repair_journal() {
        let f = Fixture::new(false);
        let (mut run, path) = r2_start(&f, true);
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        run.observe_exit(&deadline()).unwrap();
        assert_clean_prerequisite(&run);
        let agent =
            f.io.read(&f.io.target().agent_path(), 4 * 1024 * 1024, false)
                .unwrap();
        run.identity(sha256(&agent), f.environment(), &deadline())
            .unwrap();
        let intent =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let mut record =
            CleanupIntent::decode(&f.io.read(&intent, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        // Simulated crash after FilesObserved was persisted, before the journal step ran.
        record.progress.stage = CleanupStage::FilesObserved;
        f.io.atomic_write(&f.proof, &intent, &record.encode().unwrap())
            .unwrap();
        drop(run);
        let before = fs::read(&path).unwrap();
        let (plan, consent) = planned(&f, RemovalSelection::default(), None, 2, 200);
        let mut resumed = plan
            .resume(consent, Arc::new(f.service(&package())), &deadline())
            .unwrap();
        resumed.disable(&deadline()).unwrap();
        finish(&f, &mut resumed);
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "no clean authority on resume"
        );
        assert_ne!(resumed.report().form, UninstallForm::Complete);
        assert!(resumed.report().recovery_retained);
        assert!(f.io.target().agent_path().exists());
    }
    #[test]
    fn clean_default_keep_removes_owned_files_after_real_original_exit() {
        let f = Fixture::new(false);
        let mut run = start(&f, RemovalSelection::default(), true, true);
        run.disable(&deadline()).unwrap();
        finish_clean(&f, &mut run);
        let report = run.report();
        assert_eq!(report.form, UninstallForm::Complete);
        assert_eq!(report.progress.identity, CleanupResult::Kept);
        assert!(
            report
                .progress
                .resources
                .iter()
                .all(|r| *r == CleanupResult::Removed)
        );
        assert!(report.empty_directories_retained);
        assert_eq!(f.erase_count(), 0);
        let mutations = f
            .runner
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, a)| {
                a.first().is_some_and(|s| s == "--user")
                    && ["disable", "stop"].contains(&a[1].as_str())
            })
            .map(|(_, a)| a.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            mutations,
            vec![
                vec!["--user", "disable", UNIT],
                vec!["--user", "stop", UNIT]
            ]
        );
        assert!(
            f.io.read(
                &f.io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/last_exit.json"),
                4096,
                true
            )
            .is_ok()
        );
    }
    #[test]
    fn stop_zero_with_running_process_never_erases_or_removes_recovery() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            false,
            true,
        );
        f.exit(|_| {}); // A matching receipt still grants nothing while the original process lives.
        run.disable(&deadline()).unwrap();
        finish(&f, &mut run);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert_eq!(f.erase_count(), 0);
        assert!(
            run.report().progress.resources[..6]
                .iter()
                .enumerate()
                .all(|(i, r)| *r
                    == if i == 4 {
                        CleanupResult::AlreadyAbsent
                    } else {
                        CleanupResult::Kept
                    })
        );
        assert!(
            run.report().progress.resources[6..]
                .iter()
                .all(|r| *r == CleanupResult::Removed)
        );
    }
    #[test]
    fn genuine_clean_explicit_delete_admits_one_shot_semantics_before_recovery_removal() {
        let f = Fixture::new(false);
        let mut run = start(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            true,
            true,
        );
        run.disable(&deadline()).unwrap();
        finish_clean(&f, &mut run);
        let report = run.report();
        assert_eq!(
            f.erase_count(),
            1,
            "stop={:?}; clean Busy={}; stop Busy={}; recovery retained={}",
            report.progress.stop,
            report.issues.iter().any(|issue| matches!(
                issue,
                UninstallIssue::CleanExit(RemovalError::Native(NativeError::Busy))
            )),
            report.issues.iter().any(|issue| matches!(
                issue,
                UninstallIssue::Stop(RemovalError::Native(NativeError::Busy))
            )),
            report.recovery_retained,
        );
        assert!(
            run.report()
                .identity_receipt
                .unwrap()
                .identity_and_pairings_removed()
        );
        assert_eq!(run.report().form, UninstallForm::Complete);
    }
    #[test]
    fn uncorrelated_stop_is_not_clean_and_preview_never_promises_recovery_removal() {
        let f = Fixture::new(false);
        let mut run = start(&f, RemovalSelection::default(), true, false);
        run.disable(&deadline()).unwrap();
        finish(&f, &mut run);
        assert!(run.stop(&deadline()).is_err());
        assert_eq!(
            f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, a)| a.get(1).is_some_and(|s| s == "stop"))
                .count(),
            1
        );
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(
            run.report().progress.resources[..6]
                .iter()
                .enumerate()
                .all(|(i, r)| *r
                    == if i == 4 {
                        CleanupResult::AlreadyAbsent
                    } else {
                        CleanupResult::Kept
                    })
        );
    }
    #[test]
    fn busy_at_clean_acquisition_is_unknown_without_erase_or_recovery_deletion() {
        struct PausedExit {
            base: Arc<Probe>,
            entered: std::sync::atomic::AtomicUsize,
            paused: Mutex<bool>,
            changed: std::sync::Condvar,
        }
        impl ExitReader for PausedExit {
            fn snapshot(
                &self,
                pid: u32,
                d: &Deadline,
            ) -> Result<Option<ProcessFacts>, NativeError> {
                d.check()?;
                let mut paused = self.paused.lock().unwrap();
                if *paused {
                    self.entered.fetch_add(1, Ordering::Release);
                    while *paused {
                        d.check()?;
                        paused = self
                            .changed
                            .wait_timeout(paused, Duration::from_millis(1))
                            .unwrap()
                            .0;
                    }
                }
                ExitReader::snapshot(self.base.as_ref(), pid, d)
            }
        }
        struct ReadLoad {
            reader: Arc<PausedExit>,
            workers: Vec<thread::JoinHandle<()>>,
        }
        impl Drop for ReadLoad {
            fn drop(&mut self) {
                *self.reader.paused.lock().unwrap() = false;
                self.reader.changed.notify_all();
                for worker in self.workers.drain(..) {
                    let _ = worker.join();
                }
            }
        }
        let f = Fixture::new(false);
        installed(&f, &package());
        let service = known_manager(&f, &package(), true);
        let reader = Arc::new(PausedExit {
            base: f.probe.clone(),
            entered: std::sync::atomic::AtomicUsize::new(0),
            paused: Mutex::new(false),
            changed: std::sync::Condvar::new(),
        });
        let original = f.tracked_with(reader.clone());
        let (plan, consent) = planned(
            &f,
            RemovalSelection {
                identity: IdentityChoice::DeleteIdentityAndPairings,
                ..Default::default()
            },
            Some(original.clone()),
            1,
            100,
        );
        let mut run = plan.begin(consent, service, &deadline()).unwrap();
        run.disable(&deadline()).unwrap();
        run.stop(&deadline()).unwrap();
        assert_eq!(run.report().progress.stop, CleanupResult::Removed);
        // Fill the actual four-slot read pool with paused, read-only exit observations.
        // Reader errors themselves become ProcessExit::Unknown, not acquisition Busy.
        *reader.paused.lock().unwrap() = true;
        let mut load = ReadLoad {
            reader: reader.clone(),
            workers: Vec::new(),
        };
        for _ in 0..4 {
            let original = original.clone();
            load.workers.push(thread::spawn(move || {
                let d = deadline();
                loop {
                    match original.clean_authority(&d) {
                        Err(RemovalError::Native(NativeError::Busy)) => {
                            d.check().unwrap();
                            thread::sleep(Duration::from_millis(1));
                        }
                        result => {
                            result.unwrap();
                            break;
                        }
                    }
                }
            }));
        }
        let acquisition = deadline();
        while reader.entered.load(Ordering::Acquire) != 4 {
            acquisition.check().unwrap();
            thread::sleep(Duration::from_millis(1));
        }
        run.observe_exit(&deadline()).unwrap();
        assert_eq!(run.report().progress.stop, CleanupResult::Unknown);
        assert!(run.report().issues.iter().any(|issue| matches!(
            issue,
            UninstallIssue::CleanExit(RemovalError::Native(NativeError::Busy))
        )));
        drop(load);
        finish_files(&f, &mut run);
        assert_eq!(f.erase_count(), 0);
        assert_eq!(run.report().form, UninstallForm::NotClean);
        assert!(run.report().identity_retained);
        assert!(run.report().recovery_retained);
        assert!(
            run.report().progress.resources[..6]
                .iter()
                .enumerate()
                .all(|(i, r)| *r
                    == if i == 4 {
                        CleanupResult::AlreadyAbsent
                    } else {
                        CleanupResult::Kept
                    })
        );
        assert!(
            run.report().progress.resources[6..]
                .iter()
                .all(|r| *r == CleanupResult::Removed)
        );
    }
    #[test]
    fn uncorrelated_old_continuation_without_renewed_consent_sends_zero_stop() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let cleanup_planner = CleanupPlanner::default();
        let planner = UninstallPlanner::default();
        let make = |revision, operation| {
            let inventory =
                CleanupInventory::admit(f.io.admit_cleanup(&deadline()).unwrap(), &deadline())
                    .unwrap();
            let cleanup = cleanup_planner
                .plan(
                    inventory,
                    revision,
                    OperationId(operation),
                    RemovalSelection::default(),
                )
                .unwrap();
            let cc = cleanup_planner
                .consent(&cleanup, revision, OperationId(operation), &deadline())
                .unwrap();
            let plan = planner.plan(cleanup, f.io.clone(), None).unwrap();
            let consent = planner
                .consent(&plan, cc, revision, OperationId(operation))
                .unwrap();
            (plan, consent)
        };
        let (old, consent) = make(1, 100);
        let mut old = old
            .begin(consent, known_manager(&f, &package(), false), &deadline())
            .unwrap();
        old.disable(&deadline()).unwrap();
        let (_new, _renewed_consent) = make(2, 200);
        let calls = f.runner.calls.lock().unwrap().len();
        assert!(old.stop(&deadline()).is_err());
        assert_eq!(f.runner.calls.lock().unwrap().len(), calls);
        assert_eq!(old.report().form, UninstallForm::NotClean);
        assert_eq!(f.erase_count(), 0);
        assert!(
            f.io.metadata(&f.io.target().agent_path())
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn cleanup_admits_completed_literal_ledger_with_read_only_exact_snapshots() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let commands = f.runner.calls.lock().unwrap().len();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.receipt().resources.len(), CLEANUP_FILES.len());
    assert_eq!(proof.observation(0).unwrap(), ResourceObservation::Matching);
    assert!(proof.owned(0).unwrap());
    assert_eq!(proof.observation(4).unwrap(), ResourceObservation::Absent);
    assert!(!proof.owned(4).unwrap());
    assert_eq!(proof.observation(10), Err(NativeError::Invalid));
    assert_eq!(proof.owned(10), Err(NativeError::Invalid));
    assert_eq!(format!("{proof:?}"), "CleanupProof(..)");
    let before: Vec<_> = proof
        .receipt()
        .resources
        .iter()
        .filter(|r| r.resource_id != "bin/crosspane-tutorial")
        .map(|r| fs::read(&r.resolved_path).unwrap())
        .collect();
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
    proof.revalidate(&deadline()).unwrap();
    let after: Vec<_> = proof
        .receipt()
        .resources
        .iter()
        .filter(|r| r.resource_id != "bin/crosspane-tutorial")
        .map(|r| fs::read(&r.resolved_path).unwrap())
        .collect();
    assert_eq!(before, after);
    assert!(
        f.io.metadata(
            &f.io
                .target()
                .paths()
                .state_home
                .join("crosspane/installer/removal-intent.json")
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
}

#[test]
fn cleanup_refuses_changed_ledger_modified_hardlinked_and_foreign_resources() {
    for mutation in 0..4 {
        let f = Fixture::new(false);
        installed(&f, &package());
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
        if mutation == 0 {
            let ledger =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/payload-outcome.json");
            let mut value: Value =
                serde_json::from_slice(&f.io.read(&ledger, MAX_RECORD_BYTES, true).unwrap())
                    .unwrap();
            value["receipt"]["operation_id"] = json!(999);
            f.io.atomic_write(&f.proof, &ledger, &serde_json::to_vec(&value).unwrap())
                .unwrap();
        } else if mutation == 1 {
            fs::write(&path, b"administrator modification").unwrap();
        } else if mutation == 2 {
            fs::hard_link(&path, f.root.join("outside-inventory-link")).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(f.io.target().agent_path(), &path).unwrap();
        }
        assert!(matches!(
            proof.revalidate(&deadline()),
            Err(NativeError::Foreign)
        ));
        // Captured facts are not silently refreshed by revalidation or clicks.
        assert_eq!(proof.observation(8).unwrap(), ResourceObservation::Matching);
        assert!(proof.owned(8).unwrap());
        assert!(fs::symlink_metadata(path).is_ok());
    }
}

#[test]
fn cleanup_rejects_unfinished_and_malformed_ledger_without_creating_lock_or_intent() {
    for field in ["phase", "resources", "path", "replacement", "source"] {
        let f = Fixture::new(false);
        installed(&f, &package());
        let state = f.io.target().paths().state_home.join("crosspane/installer");
        let ledger = state.join("payload-outcome.json");
        let mut value: Value =
            serde_json::from_slice(&f.io.read(&ledger, MAX_RECORD_BYTES, true).unwrap()).unwrap();
        match field {
            "phase" => value["phase"] = json!("Applied"),
            "resources" => value["receipt"]["resources"][0] = json!([]),
            "path" => {
                value["receipt"]["resources"][0]["resolved_path"] = json!(f.root.join("unrecorded"))
            }
            "replacement" => value["items"][0]["replacement"]["mode"] = json!(0o777),
            "source" => value["source"] = json!("Live"),
            _ => unreachable!(),
        }
        f.io.atomic_write(&f.proof, &ledger, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(f.io.admit_cleanup(&deadline()).is_err(), "{field}");
        assert!(
            f.io.metadata(&state.join("removal-intent.json"))
                .unwrap()
                .is_none()
        );
    }
}

fn cleanup_ledger(f: &Fixture) -> (PathBuf, Value) {
    let path =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-outcome.json");
    let value = serde_json::from_slice(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
    (path, value)
}

fn legacy_cleanup_ledger(f: &Fixture) -> PathBuf {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let leaf = f.io.target().paths().prefix.join("bin/crosspane-tutorial");
    let bytes = b"owned inert V1 tutorial fixture";
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&leaf)
        .unwrap()
        .write_all(bytes)
        .unwrap();
    let file = fs::metadata(&leaf).unwrap();
    let parent = fs::metadata(leaf.parent().unwrap()).unwrap();
    let hash = sha256(bytes);
    let (path, mut ledger) = cleanup_ledger(f);
    ledger["items"].as_array_mut().unwrap().insert(
        4,
        json!({
        "old":null,"new":hash,"template":hash,"ownership":"Created",
        "replacement":{"file":[file.dev(),file.ino()],"parent":[parent.dev(),parent.ino()],
        "hash":hash,"mode":493}}),
    );
    ledger["receipt"]["resources"]
        .as_array_mut()
        .unwrap()
        .insert(
            4,
            json!({
        "resource_id":"bin/crosspane-tutorial","resolved_path":leaf,
        "ownership":"Created","before":"Absent","after":"Matching","outcome":"Verified"}),
        );
    f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&ledger).unwrap())
        .unwrap();
    leaf
}

#[test]
fn legacy_cleanup_current_nine_preserves_ten_slots_without_obsolete_authority() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.receipt().resources.len(), 10);
    assert_eq!(
        proof.receipt().resources[4].resource_id,
        "bin/crosspane-tutorial"
    );
    assert_eq!(proof.observation(4).unwrap(), ResourceObservation::Absent);
    assert!(!proof.owned(4).unwrap());
    assert_eq!(
        proof.receipt().resources[5].resource_id,
        "resources/crosspane-agent.service"
    );
    assert!(proof.owned(5).unwrap());
}

#[test]
fn legacy_cleanup_current_nine_never_deletes_an_unrecorded_obsolete_leaf() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let leaf = f.io.target().paths().prefix.join("bin/crosspane-tutorial");
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&leaf)
        .unwrap()
        .write_all(b"unrecorded own scratch leaf")
        .unwrap();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.receipt().resources.len(), 10);
    assert!(!proof.owned(4).unwrap());
    assert!(
        !proof
            .lease(&deadline())
            .unwrap()
            .delete(4, &deadline())
            .unwrap()
    );
    assert!(leaf.exists());
}

#[test]
fn legacy_cleanup_old_ten_refuses_wrong_slot_path_and_other_counts() {
    for mutation in 0..3 {
        let f = Fixture::new(false);
        installed(&f, &package());
        legacy_cleanup_ledger(&f);
        let (path, mut ledger) = cleanup_ledger(&f);
        match mutation {
            0 => ledger["receipt"]["resources"]
                .as_array_mut()
                .unwrap()
                .swap(3, 4),
            1 => ledger["receipt"]["resources"][4]["resolved_path"] = json!("/wrong/tutorial"),
            _ => {
                ledger["items"].as_array_mut().unwrap().pop();
            }
        }
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&ledger).unwrap())
            .unwrap();
        assert!(matches!(
            f.io.admit_cleanup(&deadline()),
            Err(NativeError::Foreign)
        ));
    }
}

mod cleanup_consent_tests {
    use super::*;
    use crosspane_installer::platform::linux::removal::executor::*;

    fn inventory(f: &Fixture) -> CleanupInventory {
        CleanupInventory::admit(f.io.admit_cleanup(&deadline()).unwrap(), &deadline()).unwrap()
    }
    fn edit_owned(f: &Fixture, path: &Path, bytes: &[u8]) {
        f.io.validate_target().unwrap();
        assert!(path.starts_with(&f.root));
        let parent = rustix::fs::open(
            path.parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut file = fs::File::from(
            rustix::fs::openat(
                &parent,
                path.file_name().unwrap(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::TRUNC
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap(),
        );
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
    fn progress() -> CleanupProgress {
        CleanupProgress {
            stage: CleanupStage::Prepared,
            resources: [CleanupResult::Pending; CLEANUP_FILES.len()],
            autostart: CleanupResult::Pending,
            stop: CleanupResult::Pending,
            identity: CleanupResult::Kept,
            lan: CleanupResult::Kept,
            mdns: CleanupResult::Kept,
        }
    }
    fn cancelled() -> Deadline {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        Deadline::new(5000, cancellation).unwrap()
    }

    #[test]
    fn genuine_cleanup_inventory_retains_recovery_and_never_uses_receipt_ownership_alone() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let first = inventory(&f);
        assert_eq!(first.resources().len(), CLEANUP_FILES.len());
        for (row, name) in first.resources().iter().zip(CLEANUP_FILES) {
            assert_eq!(row.receipt.resource_id, name);
            if name == "bin/crosspane-tutorial" {
                assert_eq!(row.observation, ResourceObservation::Absent);
                assert!(!row.owned);
                assert_eq!(row.action, ResourceAction::AlreadyAbsent);
                continue;
            }
            assert_eq!(row.observation, ResourceObservation::Matching);
            assert!(row.owned);
            assert_eq!(
                row.action,
                if name.starts_with("bin/") || name.ends_with(".service") {
                    ResourceAction::RetainRecovery
                } else {
                    ResourceAction::Remove
                }
            );
        }
        let installer = PayloadInstaller::new(f.io.clone()).unwrap();
        let p = package();
        let repeat = installer
            .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
            .unwrap();
        installer.apply(&f.proof, &p, repeat, &deadline()).unwrap();
        installer
            .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
            .unwrap();
        for row in inventory(&f).resources() {
            if row.receipt.resource_id == "bin/crosspane-tutorial" {
                assert_eq!(row.receipt.ownership, ResourceOwnership::Foreign);
                assert_eq!(row.observation, ResourceObservation::Absent);
                assert!(!row.owned);
                assert_eq!(row.action, ResourceAction::AlreadyAbsent);
                continue;
            }
            assert_eq!(row.receipt.ownership, ResourceOwnership::Created);
            assert_eq!(row.observation, ResourceObservation::Matching);
            assert!(!row.owned);
            assert_eq!(row.action, ResourceAction::Retain);
        }
        assert!(
            f.io.metadata(
                &f.io
                    .target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json")
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn cleanup_plan_is_immutable_and_absent_or_modified_rows_do_not_authorize_removal() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let targets = PayloadInstaller::new(f.io.clone()).unwrap();
        let path = &targets.targets()[7];
        edit_owned(&f, path, b"owned test edit");
        let changed = inventory(&f);
        assert_eq!(
            changed.resources()[8].observation,
            ResourceObservation::Different
        );
        assert_eq!(changed.resources()[8].action, ResourceAction::Retain);
        let parent = rustix::fs::open(
            path.parent().unwrap(),
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        rustix::fs::unlinkat(
            &parent,
            path.file_name().unwrap(),
            rustix::fs::AtFlags::empty(),
        )
        .unwrap();
        let planner = CleanupPlanner::default();
        let admitted = inventory(&f);
        for (revision, operation) in [(0, 0), (0, 1), (1, 0)] {
            assert!(
                planner
                    .plan(
                        admitted.clone(),
                        revision,
                        OperationId(operation),
                        RemovalSelection::default()
                    )
                    .is_err()
            );
        }
        let plan = planner
            .plan(admitted, 1, OperationId(1), RemovalSelection::default())
            .unwrap();
        assert_eq!(plan.resources()[8].observation, ResourceObservation::Absent);
        assert_eq!(plan.resources()[8].action, ResourceAction::AlreadyAbsent);
        let mut detached = plan.resources()[0].clone();
        detached.action = ResourceAction::Remove;
        assert_eq!(plan.resources()[0].action, ResourceAction::RetainRecovery);
        assert_eq!(plan.selection(), RemovalSelection::default());
        assert_eq!(plan.form(), CleanupForm::NotCleanRetainIdentityAndRecovery);
        assert_eq!(plan.revision(), 1);
        assert_eq!(plan.operation(), OperationId(1));
    }

    #[test]
    fn superseded_coherent_consent_refuses_before_deadline_io_and_releases_cached_lease() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let old = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&old, 1, OperationId(1), &deadline())
            .unwrap();
        assert!(old.read_intent(&consent, &deadline()).unwrap().is_none());
        let written = old.write_intent(&consent, progress(), &deadline()).unwrap();
        let new = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        let before = f.io.read(&path, MAX_RECORD_BYTES, true).unwrap();
        let commands = f.runner.calls.lock().unwrap().len();
        assert!(matches!(
            old.read_intent(&consent, &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            old.write_intent(&consent, progress(), &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            planner.consent(&old, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.io.read(&path, MAX_RECORD_BYTES, true).unwrap(), before);
        assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
        let renewed = planner
            .consent(&new, 2, OperationId(2), &deadline())
            .unwrap();
        // The old record remains diagnostic data; it never supplies this new consent.
        assert_eq!(
            new.read_intent(&renewed, &deadline()).unwrap(),
            Some(written)
        );
        assert_eq!(
            new.write_intent(&renewed, progress(), &deadline())
                .unwrap()
                .operation,
            OperationId(2)
        );
    }

    #[test]
    fn cross_controller_and_wrong_view_consents_never_open_native_lease() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let a = CleanupPlanner::default();
        let b = CleanupPlanner::default();
        let plan = a
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let other = b
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        assert!(matches!(
            b.consent(&plan, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        for (revision, op) in [(2, 1), (1, 2)] {
            assert!(matches!(
                a.consent(&plan, revision, OperationId(op), &cancelled()),
                Err(RemovalError::Stale)
            ));
        }
        let other_consent = b.consent(&other, 1, OperationId(1), &deadline()).unwrap();
        assert!(matches!(
            plan.write_intent(&other_consent, progress(), &cancelled()),
            Err(RemovalError::Stale)
        ));
        let parent = f.io.target().paths().state_home.join("crosspane/installer");
        assert!(
            f.io.metadata(&parent.join("cleanup-intent.json"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn failed_consent_permanently_retires_cached_inventory_and_last_ids_survive() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let cached = inventory(&f);
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                cached.clone(),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let target = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
        let original = f.io.read(&target, MAX_MEMBER_BYTES, false).unwrap();
        edit_owned(&f, &target, b"changed test state");
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &deadline()),
            Err(RemovalError::Native(NativeError::Foreign))
        ));
        edit_owned(&f, &target, &original);
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &cancelled()),
            Err(RemovalError::Stale)
        ));
        for (revision, op) in [(1, 1), (1, 2), (2, 1)] {
            assert!(
                planner
                    .plan(
                        cached.clone(),
                        revision,
                        OperationId(op),
                        RemovalSelection::default()
                    )
                    .is_err()
            );
        }
        let fresh = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        assert!(
            planner
                .consent(&fresh, 2, OperationId(2), &deadline())
                .is_ok()
        );
    }

    #[test]
    fn failed_native_access_retires_before_retry_and_new_consent_is_required() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&plan, 1, OperationId(1), &deadline())
            .unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &cancelled()),
            Err(RemovalError::Native(NativeError::Cancelled))
        ));
        assert!(matches!(
            plan.write_intent(&consent, progress(), &deadline()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            planner.consent(&plan, 1, OperationId(1), &deadline()),
            Err(RemovalError::Stale)
        ));
        let renewed = planner
            .plan(
                inventory(&f),
                2,
                OperationId(2),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&renewed, 2, OperationId(2), &deadline())
            .unwrap();
        assert!(
            renewed
                .read_intent(&consent, &deadline())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn guarded_intent_binds_selection_operation_and_ledger_without_recreating_clean_exit() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let planner = CleanupPlanner::default();
        let selection = RemovalSelection {
            identity: IdentityChoice::DeleteIdentityAndPairings,
            lan_rule: true,
            mdns_rule: true,
        };
        let admitted = inventory(&f);
        let plan = planner
            .plan(admitted.clone(), u64::MAX, OperationId(u64::MAX), selection)
            .unwrap();
        for (revision, operation) in [
            (u64::MAX, u64::MAX),
            (u64::MAX - 1, u64::MAX),
            (u64::MAX, u64::MAX - 1),
        ] {
            assert!(
                planner
                    .plan(
                        admitted.clone(),
                        revision,
                        OperationId(operation),
                        selection
                    )
                    .is_err()
            );
        }
        let consent = planner
            .consent(&plan, u64::MAX, OperationId(u64::MAX), &deadline())
            .unwrap();
        let record = plan
            .write_intent(&consent, progress(), &deadline())
            .unwrap();
        assert!(record.delete_identity && record.lan_rule && record.mdns_rule);
        assert_eq!(record.revision, u64::MAX);
        assert_eq!(record.operation, OperationId(u64::MAX));
        assert_eq!(
            record.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
        assert_eq!(
            plan.read_intent(&consent, &deadline()).unwrap(),
            Some(record.clone())
        );
        // A live plan retains its native installation lock and rejects file drift.
        let lease = f.io.admit_cleanup(&deadline()).unwrap().lease(&deadline());
        assert!(matches!(lease, Err(NativeError::Busy))); // current plan retains the installation lock.
        let parent = f.io.target().paths().state_home.join("crosspane/installer");
        let mut wrong = record;
        wrong.ledger_digest[0] ^= 1;
        f.io.atomic_write(
            &f.proof,
            &parent.join("cleanup-intent.json"),
            &wrong.encode().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Native(NativeError::Foreign))
        ));
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.erase_count(), 0);
    }

    #[test]
    fn observed_wrong_ledger_digest_retires_guard_even_when_native_snapshot_matches() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let wrong = CleanupIntent {
            revision: 77,
            operation: OperationId(88),
            ledger_digest: [0; 32],
            delete_identity: false,
            lan_rule: false,
            mdns_rule: false,
            progress: progress(),
        };
        let path =
            f.io.target()
                .paths()
                .state_home
                .join("crosspane/installer/cleanup-intent.json");
        // Persist BEFORE capturing the lease: native identity validation must succeed.
        f.io.atomic_write(&f.proof, &path, &wrong.encode().unwrap())
            .unwrap();
        let planner = CleanupPlanner::default();
        let plan = planner
            .plan(
                inventory(&f),
                1,
                OperationId(1),
                RemovalSelection::default(),
            )
            .unwrap();
        let consent = planner
            .consent(&plan, 1, OperationId(1), &deadline())
            .unwrap();
        let bytes = f.io.read(&path, MAX_RECORD_BYTES, true).unwrap();
        assert!(matches!(
            plan.read_intent(&consent, &deadline()),
            Err(RemovalError::Stale)
        ));
        assert!(matches!(
            plan.write_intent(&consent, progress(), &deadline()),
            Err(RemovalError::Stale)
        ));
        assert_eq!(f.io.read(&path, MAX_RECORD_BYTES, true).unwrap(), bytes);
        assert_eq!(f.erase_count(), 0);
    }

    #[test]
    fn each_cleanup_policy_debug_is_type_only_with_live_and_retired_binding() {
        let f = Fixture::new(false);
        installed(&f, &package());
        let admitted = inventory(&f);
        let planner = CleanupPlanner::default();
        let selection = RemovalSelection {
            identity: IdentityChoice::DeleteIdentityAndPairings,
            lan_rule: true,
            mdns_rule: true,
        };
        let plan = planner
            .plan(
                admitted.clone(),
                8877665544,
                OperationId(1122334455),
                selection,
            )
            .unwrap();
        let consent = planner
            .consent(&plan, plan.revision(), plan.operation(), &deadline())
            .unwrap();
        plan.write_intent(&consent, progress(), &deadline())
            .unwrap();
        for (name, debug) in [
            ("CleanupResource", format!("{:?}", plan.resources()[0])),
            ("CleanupInventory", format!("{admitted:?}")),
            ("CleanupPlanner", format!("{planner:?}")),
            ("CleanupPlan", format!("{plan:?}")),
            ("CleanupConsent", format!("{consent:?}")),
        ] {
            assert_eq!(debug, format!("{name}(..)"));
            for forbidden in [
                f.root.to_str().unwrap(),
                "resources",
                "ledger_digest",
                "DeleteIdentityAndPairings",
                "1122334455",
                "8877665544",
                "active",
                "lease",
            ] {
                assert!(!debug.contains(forbidden), "{name} exposed {forbidden}");
            }
        }
        assert!(plan.read_intent(&consent, &cancelled()).is_err());
        assert_eq!(format!("{planner:?}"), "CleanupPlanner(..)");
        assert_eq!(format!("{plan:?}"), "CleanupPlan(..)");
        assert_eq!(format!("{consent:?}"), "CleanupConsent(..)");
    }
}

mod cleanup_intent_tests {
    use super::*;
    use crosspane_installer::platform::linux::removal::executor::*;

    fn literal() -> Vec<u8> {
        br#"{"schema_version":1,"revision":3,"operation":4,"ledger_digest":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7],"delete_identity":false,"lan_rule":true,"mdns_rule":false,"stage":0,"resources":[0,1,2,3,4,5,6,0,0,0],"autostart":1,"stop":6,"identity":3,"lan":4,"mdns":2}"#.to_vec()
    }
    fn record() -> CleanupIntent {
        CleanupIntent::decode(&literal()).unwrap()
    }
    fn lease(f: &Fixture) -> CleanupLease {
        installed(f, &package());
        f.io.admit_cleanup(&deadline())
            .unwrap()
            .lease(&deadline())
            .unwrap()
    }

    #[test]
    fn literal_results_roundtrip_and_resume_never_recreate_clean_authority() {
        let intent = record();
        assert_eq!(intent.revision, 3);
        assert_eq!(intent.operation, OperationId(4));
        assert!(!intent.delete_identity);
        assert!(intent.lan_rule);
        assert!(!intent.mdns_rule);
        assert_eq!(intent.progress.stage, CleanupStage::Prepared);
        assert_eq!(
            intent.progress.resources.as_slice(),
            [
                CleanupResult::Pending,
                CleanupResult::Removed,
                CleanupResult::AlreadyAbsent,
                CleanupResult::Kept,
                CleanupResult::Refused,
                CleanupResult::Failed,
                CleanupResult::Unknown,
                CleanupResult::Pending,
                CleanupResult::Pending,
                CleanupResult::Pending
            ]
            .as_slice()
        );
        assert_eq!(intent.progress.autostart, CleanupResult::Removed);
        assert_eq!(intent.progress.stop, CleanupResult::Unknown);
        assert_eq!(intent.progress.identity, CleanupResult::Kept);
        assert_eq!(intent.progress.lan, CleanupResult::Refused);
        assert_eq!(intent.progress.mdns, CleanupResult::AlreadyAbsent);
        assert_eq!(
            intent.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
        assert_eq!(
            CleanupIntent::decode(&intent.encode().unwrap()).unwrap(),
            intent
        );
        let mut explicit = intent.clone();
        explicit.delete_identity = true;
        assert_eq!(
            CleanupIntent::decode(&explicit.encode().unwrap())
                .unwrap()
                .form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
    }

    #[test]
    fn intent_requires_every_field_unique_object_and_exact_fixed_array_types() {
        let original: Value = serde_json::from_slice(&literal()).unwrap();
        for field in original.as_object().unwrap().keys() {
            let mut value = original.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                matches!(
                    CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                    Err(RemovalError::Invalid)
                ),
                "missing {field}"
            );
            let member = format!("\"{field}\":{}", original[field]);
            let text = serde_json::to_string(&original).unwrap();
            let duplicate = text.replacen(&member, &format!("{member},{member}"), 1);
            let _: Value = serde_json::from_str(&duplicate).unwrap();
            assert!(
                matches!(
                    CleanupIntent::decode(duplicate.as_bytes()),
                    Err(RemovalError::Invalid)
                ),
                "duplicate {field}"
            );
        }
        let mut variants = vec![json!([]), Value::Null];
        let mut unknown = original.clone();
        unknown["unknown"] = json!(true);
        variants.push(unknown);
        for field in ["ledger_digest", "resources"] {
            for invalid in [json!([]), json!({}), Value::Null, json!([256]), json!([-1])] {
                let mut value = original.clone();
                value[field] = invalid;
                variants.push(value);
            }
            let mut extra = original.clone();
            extra[field].as_array_mut().unwrap().push(json!(0));
            variants.push(extra);
            for invalid in [
                json!(-1),
                json!(256),
                json!(0.5),
                json!("0"),
                json!(true),
                Value::Null,
                json!({}),
            ] {
                let mut element = original.clone();
                element[field][0] = invalid;
                variants.push(element);
            }
        }
        for value in variants {
            assert!(matches!(
                CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                Err(RemovalError::Invalid)
            ));
        }
        let mut two = literal();
        two.extend(literal());
        assert!(matches!(
            CleanupIntent::decode(&two),
            Err(RemovalError::Invalid)
        ));
        assert!(matches!(
            CleanupIntent::decode(&vec![b' '; MAX_RECORD_BYTES + 1]),
            Err(RemovalError::Invalid)
        ));
    }

    #[test]
    fn intent_scalar_boundaries_and_each_closed_result_and_stage_are_exact() {
        let original: Value = serde_json::from_slice(&literal()).unwrap();
        for field in [
            "schema_version",
            "revision",
            "operation",
            "stage",
            "autostart",
            "stop",
            "identity",
            "lan",
            "mdns",
        ] {
            for invalid in [
                json!(-1),
                json!(1.5),
                json!(true),
                json!("1"),
                Value::Null,
                json!({"Prepared":null}),
            ] {
                let mut value = original.clone();
                value[field] = invalid;
                assert!(
                    matches!(
                        CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                        Err(RemovalError::Invalid)
                    ),
                    "{field}"
                );
            }
        }
        for field in ["delete_identity", "lan_rule", "mdns_rule"] {
            for invalid in [json!(0), json!("false"), Value::Null] {
                let mut value = original.clone();
                value[field] = invalid;
                assert!(
                    matches!(
                        CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                        Err(RemovalError::Invalid)
                    ),
                    "{field}"
                );
            }
        }
        for (field, invalid) in [
            ("schema_version", 2),
            ("revision", 0),
            ("operation", 0),
            ("stage", 7),
            ("autostart", 7),
            ("stop", 7),
            ("identity", 7),
            ("lan", 7),
            ("mdns", 7),
        ] {
            let mut value = original.clone();
            value[field] = json!(invalid);
            assert!(
                matches!(
                    CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
                    Err(RemovalError::Invalid)
                ),
                "{field}"
            );
        }
        let mut value = original.clone();
        value["resources"][9] = json!(7);
        assert!(matches!(
            CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()),
            Err(RemovalError::Invalid)
        ));
        value = original.clone();
        value["revision"] = json!(u64::MAX);
        value["operation"] = json!(u64::MAX);
        let decoded = CleanupIntent::decode(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.revision, u64::MAX);
        assert_eq!(decoded.operation, OperationId(u64::MAX));
        for (code, stage) in [
            CleanupStage::Prepared,
            CleanupStage::Disabled,
            CleanupStage::StopObserved,
            CleanupStage::IdentityObserved,
            CleanupStage::FirewallObserved,
            CleanupStage::FilesObserved,
            CleanupStage::Finished,
        ]
        .into_iter()
        .enumerate()
        {
            value["stage"] = json!(code);
            assert_eq!(
                CleanupIntent::decode(&serde_json::to_vec(&value).unwrap())
                    .unwrap()
                    .progress
                    .stage,
                stage
            );
        }
        let overflow = String::from_utf8(literal())
            .unwrap()
            .replace("\"operation\":4", "\"operation\":18446744073709551616");
        assert!(matches!(
            CleanupIntent::decode(overflow.as_bytes()),
            Err(RemovalError::Invalid)
        ));
        let mut invalid = record();
        invalid.revision = 0;
        assert!(matches!(invalid.encode(), Err(RemovalError::Invalid)));
    }

    #[test]
    fn lease_store_reads_fresh_absence_and_atomic_replacements_without_commands() {
        let f = Fixture::new(false);
        let lease = lease(&f);
        let commands = f.runner.calls.lock().unwrap().len();
        assert_eq!(lease.read_intent(&deadline()).unwrap(), None);
        let store = CleanupStore::new(lease.clone());
        assert_eq!(store.read(&deadline()).unwrap(), None);
        let mut intent = record();
        store.write(&intent, &deadline()).unwrap();
        assert_eq!(store.read(&deadline()).unwrap(), Some(intent.clone()));
        intent.progress.stage = CleanupStage::Disabled;
        intent.progress.autostart = CleanupResult::Unknown;
        store.write(&intent, &deadline()).unwrap();
        assert_eq!(store.read(&deadline()).unwrap(), Some(intent.clone()));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(intent.encode().unwrap())
        );
        assert_eq!(f.runner.calls.lock().unwrap().len(), commands);
        for target in PayloadInstaller::new(f.io.clone()).unwrap().targets() {
            assert!(target.is_file());
        }
        assert_eq!(
            intent.form(),
            CleanupForm::NotCleanRetainIdentityAndRecovery
        );
    }

    #[test]
    fn lease_read_rejects_intent_swap_symlink_mode_hardlink_and_oversize() {
        for change in 0..5 {
            let f = Fixture::new(false);
            let lease = lease(&f);
            lease.write_intent(&literal(), &deadline()).unwrap();
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            let parent = rustix::fs::open(
                path.parent().unwrap(),
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            let file = rustix::fs::openat(
                &parent,
                "cleanup-intent.json",
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOFOLLOW,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            match change {
                0 => {
                    rustix::fs::renameat(
                        &parent,
                        "cleanup-intent.json",
                        &parent,
                        "kept-old-intent",
                    )
                    .unwrap();
                    let fd = rustix::fs::openat(
                        &parent,
                        "cleanup-intent.json",
                        rustix::fs::OFlags::WRONLY
                            | rustix::fs::OFlags::CREATE
                            | rustix::fs::OFlags::EXCL
                            | rustix::fs::OFlags::NOFOLLOW,
                        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                    )
                    .unwrap();
                    fs::File::from(fd).write_all(&literal()).unwrap();
                }
                1 => {
                    rustix::fs::renameat(
                        &parent,
                        "cleanup-intent.json",
                        &parent,
                        "kept-old-intent",
                    )
                    .unwrap();
                    rustix::fs::symlinkat("kept-old-intent", &parent, "cleanup-intent.json")
                        .unwrap();
                }
                2 => {
                    rustix::fs::fchmod(&file, rustix::fs::Mode::from_bits_truncate(0o644)).unwrap()
                }
                3 => rustix::fs::linkat(
                    &parent,
                    "cleanup-intent.json",
                    &parent,
                    "other-link",
                    rustix::fs::AtFlags::empty(),
                )
                .unwrap(),
                4 => fs::File::from(file)
                    .write_all(&vec![b' '; MAX_RECORD_BYTES + 1])
                    .unwrap(),
                _ => unreachable!(),
            }
            assert_eq!(
                lease.read_intent(&deadline()).unwrap_err(),
                NativeError::Foreign,
                "change {change}"
            );
        }
    }

    #[test]
    fn store_invalid_record_and_cancelled_read_preserve_last_durable_bytes() {
        let f = Fixture::new(false);
        let lease = lease(&f);
        let store = CleanupStore::new(lease.clone());
        store.write(&record(), &deadline()).unwrap();
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(matches!(
            store.read(&Deadline::new(5000, cancellation).unwrap()),
            Err(RemovalError::Native(NativeError::Cancelled))
        ));
        let mut invalid = record();
        invalid.operation = OperationId(0);
        assert!(matches!(
            store.write(&invalid, &deadline()),
            Err(RemovalError::Invalid)
        ));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(record().encode().unwrap())
        );
        lease
            .write_intent(b"malformed observations", &deadline())
            .unwrap();
        assert!(matches!(
            store.read(&deadline()),
            Err(RemovalError::Invalid)
        ));
        assert_eq!(
            lease.read_intent(&deadline()).unwrap(),
            Some(b"malformed observations".to_vec())
        );
    }

    #[test]
    fn lease_read_preserves_captured_ancestry_and_refuses_new_uncaptured_intent() {
        for replace_parent in [false, true] {
            let f = Fixture::new(false);
            let lease = lease(&f);
            let path =
                f.io.target()
                    .paths()
                    .state_home
                    .join("crosspane/installer/cleanup-intent.json");
            if replace_parent {
                lease.write_intent(&literal(), &deadline()).unwrap();
                let ancestor = path.parent().unwrap().parent().unwrap();
                let root = rustix::fs::open(
                    ancestor.parent().unwrap(),
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .unwrap();
                rustix::fs::renameat(&root, "crosspane", &root, "retained-crosspane").unwrap();
                rustix::fs::mkdirat(&root, "crosspane", rustix::fs::Mode::RWXU).unwrap();
            } else {
                f.io.atomic_write(&f.proof, &path, &literal()).unwrap();
            }
            assert_eq!(
                lease.read_intent(&deadline()).unwrap_err(),
                NativeError::Foreign
            );
        }
    }
}

#[test]
fn cleanup_requires_each_nullable_field_and_strict_nested_objects_and_enums() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (path, original) = cleanup_ledger(&f);
    for (pointer, key) in [
        ("", "previous_instance"),
        ("", "base_generation"),
        ("/items/0", "old"),
        ("/items/0", "replacement"),
    ] {
        let mut value = original.clone();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(key);
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}/{key}"
        );
    }
    for pointer in [
        "",
        "/receipt",
        "/items/0",
        "/items/0/replacement",
        "/receipt/resources/0",
    ] {
        let mut value = original.clone();
        *value.pointer_mut(pointer).unwrap() = json!([]);
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}"
        );
    }
    for pointer in [
        "/source",
        "/items/0/ownership",
        "/receipt/resources/0/ownership",
        "/receipt/resources/0/before",
        "/receipt/resources/0/after",
        "/receipt/resources/0/outcome",
    ] {
        let mut value = original.clone();
        let enum_name = value.pointer(pointer).unwrap().as_str().unwrap().to_owned();
        *value.pointer_mut(pointer).unwrap() = json!({enum_name: null});
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "{pointer}"
        );
    }
}

#[test]
fn cleanup_rejects_duplicate_unknown_fields_and_incomplete_completion_truth() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (path, original) = cleanup_ledger(&f);
    let text = serde_json::to_string(&original).unwrap();
    for (field, value) in [
        ("phase", &original["phase"]),
        (
            "resource_id",
            &original["receipt"]["resources"][0]["resource_id"],
        ),
        ("ownership", &original["items"][0]["ownership"]),
        ("hash", &original["items"][0]["replacement"]["hash"]),
    ] {
        let member = format!("\"{field}\":{}", serde_json::to_string(value).unwrap());
        assert!(text.contains(&member));
        let duplicate = text.replacen(&member, &format!("{member},{member}"), 1);
        // A permissive parser must accept the duplicate fixture before strict admission rejects it.
        let _: Value = serde_json::from_str(&duplicate).unwrap();
        f.io.atomic_write(&f.proof, &path, duplicate.as_bytes())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "duplicate {field}"
        );
    }
    for change in 0..8 {
        let mut value = original.clone();
        match change {
            0 => value["receipt"]["unfinished"] = json!([1]),
            1 => value["receipt"]["resources"][0]["outcome"] = json!("Unknown"),
            2 => value["receipt"]["resources"][0]["after"] = json!("Unknown"),
            3 => value["items"][0]["replacement"] = Value::Null,
            4 => value["items"][0]["unexpected"] = json!(true),
            5 => value["items"][0]["replacement"]["file"] = json!([1]),
            6 => {
                value["items"][0]["replacement"] = Value::Null;
                value["items"][0]["old"] = value["items"][0]["new"].clone();
            }
            7 => {
                value["items"][0]["replacement"] = Value::Null;
                value["receipt"]["resources"][0]["before"] = json!("Matching");
            }
            _ => unreachable!(),
        }
        f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
            .unwrap();
        assert!(
            matches!(f.io.admit_cleanup(&deadline()), Err(NativeError::Foreign)),
            "change {change}"
        );
    }
}

#[test]
fn cleanup_retains_adopted_foreign_modified_and_same_hash_replaced_resources() {
    for change in 0..4 {
        let f = Fixture::new(false);
        installed(&f, &package());
        let (path, mut value) = cleanup_ledger(&f);
        let resource = PathBuf::from(
            value["receipt"]["resources"][7]["resolved_path"]
                .as_str()
                .unwrap(),
        );
        if change < 2 {
            let ownership = if change == 0 { "Adopted" } else { "Foreign" };
            value["items"][7]["ownership"] = json!(ownership);
            value["receipt"]["resources"][7]["ownership"] = json!(ownership);
            f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&value).unwrap())
                .unwrap();
        } else if change == 2 {
            fs::write(&resource, b"retained administrator edit").unwrap();
        } else {
            let bytes = fs::read(&resource).unwrap();
            fs::rename(&resource, f.root.join("retained-previous-icon")).unwrap();
            fs::write(&resource, bytes).unwrap();
        }
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        assert!(!proof.owned(8).unwrap());
        assert_eq!(
            proof.observation(8).unwrap(),
            if change == 2 {
                ResourceObservation::Different
            } else {
                ResourceObservation::Matching
            }
        );
        proof.revalidate(&deadline()).unwrap();
        assert!(resource.is_file());
    }
}

#[test]
fn cleanup_absence_is_fresh_noent_and_parent_replacement_cannot_transfer_authority() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let (_, value) = cleanup_ledger(&f);
    let path = PathBuf::from(
        value["receipt"]["resources"][7]["resolved_path"]
            .as_str()
            .unwrap(),
    );
    fs::remove_file(&path).unwrap();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(proof.observation(8).unwrap(), ResourceObservation::Absent);
    assert!(proof.owned(8).unwrap());
    proof.revalidate(&deadline()).unwrap();
    let parent = path.parent().unwrap();
    fs::rename(parent, f.root.join("retained-parent")).unwrap();
    std::os::unix::fs::DirBuilderExt::mode(&mut fs::DirBuilder::new(), 0o700)
        .create(parent)
        .unwrap();
    assert!(matches!(
        proof.revalidate(&deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(!path.exists());
    let refreshed = f.io.admit_cleanup(&deadline()).unwrap();
    assert_eq!(
        refreshed.observation(8).unwrap(),
        ResourceObservation::Absent
    );
    assert!(!refreshed.owned(8).unwrap());
}

#[test]
fn cleanup_unfinished_intent_and_deadline_cancellation_never_create_or_dispatch() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let calls = f.runner.calls.lock().unwrap().len();
    let cancel = Cancellation::default();
    let d = Deadline::new(5000, cancel.clone()).unwrap();
    cancel.cancel();
    assert!(matches!(
        f.io.admit_cleanup(&d),
        Err(NativeError::Cancelled)
    ));
    assert_eq!(proof.revalidate(&d), Err(NativeError::Cancelled));
    let intent =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-intent.json");
    f.io.atomic_write(&f.proof, &intent, b"unfinished payload mutation")
        .unwrap();
    assert!(matches!(
        f.io.admit_cleanup(&deadline()),
        Err(NativeError::Foreign)
    ));
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert_eq!(f.runner.calls.lock().unwrap().len(), calls);
}

#[test]
fn cleanup_refuses_replaced_ancestor_even_when_final_parent_and_file_are_preserved() {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let parent = path.parent().unwrap();
    let final_inode = fs::metadata(parent).unwrap().ino();
    let ancestor = parent.parent().unwrap();
    let retained = f.root.join("retained-scalable-ancestor");
    fs::rename(ancestor, &retained).unwrap();
    fs::DirBuilder::new().mode(0o700).create(ancestor).unwrap();
    fs::rename(retained.join("apps"), parent).unwrap();
    assert_eq!(fs::metadata(parent).unwrap().ino(), final_inode);
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert!(path.is_file());
}

#[test]
fn cleanup_refuses_parent_permission_drift_from_0700_to_0755() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let parent = path.parent().unwrap();
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    assert!(path.is_file());
}

fn completed_cleanup_repair(minimal: bool) {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    if minimal {
        fs::remove_file(&installer.targets()[7]).unwrap();
    }
    let plan = installer
        .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
        .unwrap();
    installer.apply(&f.proof, &p, plan, &deadline()).unwrap();
    installer
        .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
        .unwrap();
    let (_, ledger) = cleanup_ledger(&f);
    assert_eq!(ledger["phase"], "Verified");
    assert_eq!(ledger["items"][0]["ownership"], "Created");
    assert!(ledger["items"][0]["replacement"].is_null());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    for index in 0..CLEANUP_FILES.len() {
        assert_eq!(
            proof.observation(index).unwrap(),
            if index == 4 {
                ResourceObservation::Absent
            } else {
                ResourceObservation::Matching
            }
        );
        assert_eq!(proof.owned(index).unwrap(), minimal && index == 8);
    }
    proof.revalidate(&deadline()).unwrap();
}

#[test]
fn cleanup_accepts_genuine_completed_repeat_without_replacement_provenance() {
    completed_cleanup_repair(false);
}

#[test]
fn cleanup_mutations_never_delete_genuine_repeat_resources_without_replacement_provenance() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let installer = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = installer
        .plan(&f.proof, &p, OperationId(48), MatchingFiles::Preserve)
        .unwrap();
    installer.apply(&f.proof, &p, plan, &deadline()).unwrap();
    installer
        .verify(&f.proof, &p, 19, 100, &f.reply(), &deadline())
        .unwrap();
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    let previous_calls = f.runner.calls.lock().unwrap().len();
    for (index, path) in installer.targets().iter().enumerate() {
        let bytes = fs::read(path).unwrap();
        assert!(!proof.owned(index).unwrap());
        assert_eq!(lease.delete(index, &deadline()), Ok(false));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(f.runner.calls.lock().unwrap().len(), previous_calls);
}

#[test]
fn cleanup_accepts_genuine_completed_minimal_repair_with_only_one_owned_replacement() {
    completed_cleanup_repair(true);
}

#[test]
fn cleanup_member_mode_boundary_is_pinned_to_frozen_payload_inventory() {
    assert_eq!(
        CLEANUP_FILES,
        [
            "bin/crosspane-agent",
            "bin/crosspanectl",
            "bin/crosspane-ui",
            "bin/crosspane-installer",
            "bin/crosspane-tutorial",
            "resources/crosspane-agent.service",
            "resources/crosspane-settings.desktop",
            "resources/crosspane-installer.desktop",
            "resources/crosspane-icon.svg",
            "resources/LICENSE",
        ]
    );
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    assert!(
        CLEANUP_FILES
            .iter()
            .enumerate()
            .all(|(index, name)| if index < 5 {
                name.starts_with("bin/")
            } else {
                name.starts_with("resources/")
            })
    );
    proof.revalidate(&deadline()).unwrap();
}

#[test]
fn cleanup_mutation_lease_locks_writes_only_intent_and_tracks_its_exact_removals() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    assert!(matches!(proof.lease(&deadline()), Err(NativeError::Busy)));
    let intent =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/cleanup-intent.json");
    lease
        .write_intent(b"private first intent", &deadline())
        .unwrap();
    lease
        .write_intent(b"private replacement intent", &deadline())
        .unwrap();
    assert_eq!(fs::read(&intent).unwrap(), b"private replacement intent");
    let first = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let second = PathBuf::from(&proof.receipt().resources[9].resolved_path);
    assert!(lease.delete(8, &deadline()).unwrap());
    assert!(!lease.delete(8, &deadline()).unwrap());
    assert!(lease.delete(9, &deadline()).unwrap());
    assert!(!first.exists() && !second.exists());
    assert_eq!(proof.revalidate(&deadline()), Err(NativeError::Foreign));
    lease
        .write_intent(b"private completed mutations", &deadline())
        .unwrap();
    assert!(f.io.target().agent_path().is_file());
    drop(lease);
    let renewed =
        f.io.admit_cleanup(&deadline())
            .unwrap()
            .lease(&deadline())
            .unwrap();
    assert!(!renewed.delete(8, &deadline()).unwrap());
}

#[test]
fn cleanup_delete_revalidates_admitted_identity_and_retains_foreign_replacements() {
    let f = Fixture::new(false);
    installed(&f, &package());
    let proof = f.io.admit_cleanup(&deadline()).unwrap();
    let lease = proof.lease(&deadline()).unwrap();
    let path = PathBuf::from(&proof.receipt().resources[8].resolved_path);
    let bytes = fs::read(&path).unwrap();
    use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};
    let parent = rfs::open(
        path.parent().unwrap(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .unwrap();
    let mut file = fs::File::from(
        rfs::openat(
            &parent,
            ".owned-test-replacement",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .unwrap(),
    );
    file.write_all(&bytes).unwrap();
    rfs::fchmod(&file, Mode::from_bits_truncate(0o644)).unwrap();
    rfs::renameat(
        &parent,
        ".owned-test-replacement",
        &parent,
        path.file_name().unwrap(),
    )
    .unwrap();
    assert!(
        rfs::statat(
            &parent,
            path.file_name().unwrap(),
            AtFlags::SYMLINK_NOFOLLOW
        )
        .is_ok()
    );
    assert_eq!(lease.delete(8, &deadline()), Err(NativeError::Foreign));
    assert_eq!(fs::read(&path).unwrap(), bytes);
}
#[test]
fn matching_repeat_is_noop_and_missing_file_is_the_exact_minimal_delta() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let matching = f.inventory(&planner, &p);
    assert!(
        matching
            .facts()
            .resources
            .as_ref()
            .unwrap()
            .iter()
            .all(|r| r.ownership == ResourceOwnership::Created
                && r.before == ResourceObservation::Matching)
    );
    let plan = planner
        .plan(
            matching,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    // Use the producer's exact target, never a guessed deletion path.
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let icon = install
        .targets()
        .iter()
        .find(|p| p.extension().is_some_and(|x| x == "svg"))
        .unwrap()
        .clone();
    fs::remove_file(&icon).unwrap();
    let next = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(next.repair_delta().len(), 1);
    assert_eq!(next.repair_delta()[0].resolved_path, icon.to_string_lossy());
    assert_eq!(next.repair_delta()[0].before, ResourceObservation::Absent);
}
#[test]
fn foreign_modified_and_unverified_resources_never_become_repair_or_delete_authority() {
    let f = Fixture::new(true);
    let p = package();
    let mut planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert_eq!(facts.facts().resources, Err(PayloadError::Foreign));
    let plan = planner
        .plan(
            facts,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    assert!(plan.preview().contains("retain files"));
    let f = Fixture::new(false);
    installed(&f, &p);
    f.executable(b"user changed the agent");
    let mut planner = f.planner();
    let inventory = f.inventory(&planner, &p);
    let rows = inventory.facts().resources.as_ref().unwrap();
    assert_eq!(rows[0].ownership, ResourceOwnership::Foreign);
    assert_eq!(rows[0].before, ResourceObservation::Different);
    let plan = planner
        .plan(
            inventory,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
}
#[test]
fn missing_agent_has_not_clean_form_and_interruption_or_identity_reset_is_never_implicit() {
    let f = Fixture::new(false);
    let p = package();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(
        plan.cleanup_form(),
        CleanupForm::NotCleanRetainIdentityAndRecovery
    );
    assert_eq!(plan.repair_delta().len(), FILES.len());
    assert!(plan.consent(1, OperationId(1), false).is_err());
    assert!(
        planner
            .plan(
                f.inventory(&planner, &p),
                2,
                OperationId(2),
                PlanKind::Repair,
                RemovalSelection {
                    identity: IdentityChoice::DeleteIdentityAndPairings,
                    ..Default::default()
                }
            )
            .is_err()
    );
    assert_eq!(f.erase_count(), 0);
}
#[test]
fn changed_owned_resource_retires_consent_before_any_future_executor_io() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    f.executable(b"user edit");
    let current = f.inventory(&planner, &p);
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
}
#[test]
fn receipt_never_restores_activity_and_failed_startup_remains_reported_by_frozen_verifier() {
    let f = Fixture::new(false);
    let p = package();
    let install = PayloadInstaller::new(f.io.clone()).unwrap();
    let plan = install
        .plan(&f.proof, &p, OperationId(47), MatchingFiles::Preserve)
        .unwrap();
    let receipt = install.apply(&f.proof, &p, plan, &deadline()).unwrap();
    assert!(!receipt.unfinished.is_empty());
    f.bootstrap(9);
    let mut v: Value = serde_json::from_str(HEALTH).unwrap();
    let i = &mut v["result"]["installer"]["instance"];
    i["uid"] = json!(f.io.target().paths().uid);
    i["exe"] = json!(f.io.target().agent_path());
    i["runtime_dir"] = json!(f.io.target().runtime_dir());
    v["result"]["installer"]["startup_recovery"] = json!("failed");
    v["result"]["installer"]["recovery_pending"] = json!(0);
    let reply = AgentReply {
        id: 19,
        observed_at_ms: 100,
        source: ObservationSource::Demo,
        result: decode_reply(
            &InstallerRequest::Status,
            &serde_json::to_vec(&v).unwrap(),
            AgentPlatform::Linux,
        ),
    };
    assert_eq!(
        install
            .verify(&f.proof, &p, 19, 100, &reply, &deadline())
            .unwrap_err(),
        PayloadError::Pending
    );
    let planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert!(facts.facts().activity.is_none());
    assert!(
        f.io.metadata(&f.io.target().agent_path())
            .unwrap()
            .is_some()
    );
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn adopted_matching_resource_is_retained_and_wrong_architecture_never_proposes_delta() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let path =
        f.io.target()
            .paths()
            .state_home
            .join("crosspane/installer/payload-outcome.json");
    let mut receipt: Value =
        serde_json::from_slice(&f.io.read(&path, MAX_RECORD_BYTES, true).unwrap()).unwrap();
    receipt["items"][7]["ownership"] = json!("Adopted");
    receipt["receipt"]["resources"][7]["ownership"] = json!("Adopted");
    f.io.atomic_write(&f.proof, &path, &serde_json::to_vec(&receipt).unwrap())
        .unwrap();
    let mut planner = f.planner();
    let facts = f.inventory(&planner, &p);
    assert_eq!(
        facts.facts().resources.as_ref().unwrap()[7].ownership,
        ResourceOwnership::Adopted
    );
    let plan = planner
        .plan(
            facts,
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    let wrong = package_for(if Architecture::native().unwrap() == Architecture::X86_64 {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    });
    let facts = f.inventory(&planner, &wrong);
    assert_eq!(facts.facts().resources, Err(PayloadError::Invalid));
    let plan = planner
        .plan(
            facts,
            2,
            OperationId(2),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert!(plan.repair_delta().is_empty());
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn changed_detection_retires_cached_matching_inventory_and_consent() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
    fs::write(icon, b"own injected administrator edit").unwrap();
    let _changed = f.inventory(&planner, &p);
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
}

#[test]
fn approved_package_and_exact_repair_delta_cannot_be_substituted() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
    fs::remove_file(icon).unwrap();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_eq!(plan.repair_delta().len(), 1);
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let substitute = package_with_icon(
        Architecture::native().unwrap(),
        b"different approved icon replacement",
    );
    let current = f.inventory(&planner, &substitute);
    assert_eq!(plan.facts().resources, current.facts().resources);
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &current,
                f.request(&substitute, &f.service(&substitute), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
}

#[test]
fn repair_and_uninstall_previews_state_different_typed_resource_actions() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let repair = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let removal = planner
        .plan(
            f.inventory(&planner, &p),
            2,
            OperationId(2),
            PlanKind::Uninstall,
            RemovalSelection::default(),
        )
        .unwrap();
    assert_ne!(repair.preview(), removal.preview());
    assert!(repair.preview().contains("Repair"));
    assert!(removal.preview().contains("Uninstall"));
    assert!(repair.preview().contains("Retain"));
    assert!(removal.preview().contains("clean exit"));
    // This fixture deliberately has unknown service ownership, so it cannot bind clean tracking.
    assert!(removal.tracked().is_none());
    assert!(
        repair
            .actions()
            .iter()
            .all(|(_, a)| *a == ResourceAction::Retain)
    );
    assert!(removal.actions().iter().any(
        |(r, a)| r.resource_id == "bin/crosspane-agent" && *a == ResourceAction::RetainRecovery
    ));
    assert!(
        removal
            .actions()
            .iter()
            .any(|(r, a)| r.resource_id.ends_with(".svg") && *a == ResourceAction::Remove)
    );
}

#[test]
fn validation_itself_redetects_resource_drift_without_a_caller_refresh() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
    fs::write(&icon, b"own injected edit after cached detection").unwrap();
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&p, &f.service(&p), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert_eq!(
        fs::read(icon).unwrap(),
        b"own injected edit after cached detection"
    );
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn validation_binds_package_even_when_the_supplied_snapshot_is_unchanged() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
    fs::remove_file(&icon).unwrap();
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let cached = f.inventory(&planner, &p);
    assert_eq!(cached.facts(), plan.facts());
    let replacement = package_with_icon(
        Architecture::native().unwrap(),
        b"different icon admitted at validation only",
    );
    assert_eq!(
        planner
            .validate(
                &plan,
                &consent,
                &cached,
                f.request(&replacement, &f.service(&replacement), &deadline())
            )
            .unwrap_err(),
        RemovalError::Stale
    );
    assert!(!icon.exists());
    assert_eq!(f.erase_count(), 0);
}

#[test]
fn restored_detection_cannot_revive_permanently_retired_consent() {
    let f = Fixture::new(false);
    let p = package();
    installed(&f, &p);
    let mut planner = f.planner();
    let plan = planner
        .plan(
            f.inventory(&planner, &p),
            1,
            OperationId(1),
            PlanKind::Repair,
            RemovalSelection::default(),
        )
        .unwrap();
    let consent = plan.consent(1, OperationId(1), true).unwrap();
    let icon = PayloadInstaller::new(f.io.clone()).unwrap().targets()[7].clone();
    let original = fs::read(&icon).unwrap();
    fs::write(&icon, b"own injected temporary administrator edit").unwrap();
    let changed = f.inventory(&planner, &p);
    assert_ne!(changed.facts(), plan.facts());
    fs::write(&icon, &original).unwrap();
    let restored = f.inventory(&planner, &p);
    assert_eq!(
        restored.facts(),
        plan.facts(),
        "test must reconstruct identical detection meaning"
    );
    let before = f.runner.calls.lock().unwrap().len();
    assert_eq!(
        planner.validate(
            &plan,
            &consent,
            &restored,
            f.request(&p, &f.service(&p), &deadline())
        ),
        Err(RemovalError::Stale)
    );
    assert_eq!(f.runner.calls.lock().unwrap().len(), before);
    assert_eq!(fs::read(icon).unwrap(), original);
}

mod lease_dispatch_tests {
    use super::*;
    use std::sync::{Condvar, atomic::AtomicBool};
    use std::time::Instant;

    pub(super) struct Gate {
        entered: AtomicBool,
        release: (Mutex<bool>, Condvar),
        pub(super) finished: AtomicBool,
    }
    impl Gate {
        pub(super) fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: AtomicBool::new(false),
                release: (Mutex::new(false), Condvar::new()),
                finished: AtomicBool::new(false),
            })
        }
        pub(super) fn pause(&self) {
            self.entered.store(true, Ordering::Release);
            let mut released = self.release.0.lock().unwrap();
            while !*released {
                released = self.release.1.wait(released).unwrap();
            }
        }
        pub(super) fn release(&self) {
            *self.release.0.lock().unwrap() = true;
            self.release.1.notify_all();
        }
        pub(super) fn wait(&self, condition: impl Fn() -> bool) {
            let end = Instant::now() + Duration::from_secs(2);
            while !condition() && Instant::now() < end {
                thread::sleep(Duration::from_millis(1));
            }
            assert!(condition());
        }
    }
    struct Release(Arc<Gate>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.release();
            if self.0.entered.load(Ordering::Acquire) {
                self.0.wait(|| self.0.finished.load(Ordering::Acquire));
            }
        }
    }
    fn clean_command(f: &Fixture, digest: [u8; 32]) -> CommandSpec {
        let original = f.tracked();
        f.exit(|_| {});
        *f.probe.0.lock().unwrap() = Ok(None);
        original
            .clean_authority(&deadline())
            .unwrap()
            .erase_command(digest, f.environment(), &deadline())
            .unwrap()
    }
    fn lease(f: &Fixture) -> (CleanupProof, CleanupLease) {
        let proof = f.io.admit_cleanup(&deadline()).unwrap();
        let lease = proof.lease(&deadline()).unwrap();
        (proof, lease)
    }
    fn digest(p: &Package) -> [u8; 32] {
        std::array::from_fn(|index| {
            u8::from_str_radix(
                &p.manifest().members[0].sha256[index * 2..index * 2 + 2],
                16,
            )
            .unwrap()
        })
    }
    fn dispatch(
        _f: &Fixture,
        lease: CleanupLease,
        command: CommandSpec,
        d: &Deadline,
    ) -> ManagerMutation {
        lease.erase_identity(command, d)
    }
    #[test]
    fn cleanup_stop_unverified_effective_service_refuses_without_mutation() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let (_, lease) = lease(&f);
        assert_eq!(
            lease
                .stop(Arc::new(f.service(&p)), &deadline())
                .result
                .unwrap_err(),
            NativeError::Foreign
        );
        assert!(
            f.runner
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, argv)| argv != &["--user", "stop", UNIT])
        );
    }
    #[test]
    fn cleanup_erase_wrong_target_refuses_without_spawn() {
        let selected = Fixture::new(false);
        let unrelated = Fixture::new(false);
        let p = package();
        installed(&selected, &p);
        installed(&unrelated, &p);
        let (_, lease) = lease(&unrelated);
        let command = clean_command(&selected, digest(&p));
        let result = dispatch(&selected, lease, command, &deadline());
        assert_eq!(result.result.unwrap_err(), NativeError::Foreign);
        assert_eq!(selected.erase_count(), 0);
        assert_eq!(unrelated.erase_count(), 0);
    }
    #[test]
    fn cleanup_erase_exact_selected_command_returns_literal_receipt_without_file_mutation() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let (proof, lease) = lease(&f);
        let command = clean_command(&f, digest(&p));
        let result = lease.erase_identity(command, &deadline());
        assert!(result.pending.is_none());
        let output = result.result.unwrap();
        assert_eq!(
            admit_erase_output(&output).unwrap(),
            parse_erase_identity(&output.stdout).unwrap()
        );
        assert_eq!(f.erase_count(), 1);
        proof.revalidate(&deadline()).unwrap();
        assert_eq!(lease.read_intent(&deadline()).unwrap(), None);
        assert!(matches!(proof.lease(&deadline()), Err(NativeError::Busy)));
        drop(lease);
        assert!(proof.lease(&deadline()).is_ok());
    }
    #[test]
    fn cleanup_erase_cancelled_before_dispatch_sends_no_request() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let (_, lease) = lease(&f);
        let command = clean_command(&f, digest(&p));
        let cancellation = Cancellation::default();
        cancellation.cancel();
        let result = lease.erase_identity(command, &Deadline::new(5000, cancellation).unwrap());
        assert_eq!(result.result.unwrap_err(), NativeError::OutcomeUnknown);
        assert!(result.pending.is_none());
        assert_eq!(f.erase_count(), 0);
    }
    #[test]
    fn cleanup_erase_changed_installed_hash_refuses_without_spawn() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let (_, lease) = lease(&f);
        f.executable(b"different inert executable after ledger capture");
        let command = clean_command(
            &f,
            sha256(b"different inert executable after ledger capture"),
        );
        assert_eq!(
            dispatch(&f, lease, command, &deadline())
                .result
                .unwrap_err(),
            NativeError::Foreign
        );
        assert_eq!(f.erase_count(), 0);
    }
    #[test]
    fn cleanup_erase_cancelled_noncooperative_worker_retains_original_flock() {
        let f = Fixture::new(false);
        let p = package();
        installed(&f, &p);
        let (proof, lease) = lease(&f);
        let command = clean_command(&f, digest(&p));
        let gate = Gate::new();
        let release = Release(gate.clone());
        *f.runner.erase_gate.lock().unwrap() = Some(gate.clone());
        let cancellation = Cancellation::default();
        let d = Deadline::new(5000, cancellation.clone()).unwrap();
        let result = thread::scope(|scope| {
            let call = scope.spawn(|| dispatch(&f, lease.clone(), command, &d));
            gate.wait(|| gate.entered.load(Ordering::Acquire));
            cancellation.cancel();
            call.join().unwrap()
        });
        assert_eq!(result.result.unwrap_err(), NativeError::OutcomeUnknown);
        assert_eq!(f.erase_count(), 1);
        drop(lease);
        assert!(matches!(proof.lease(&deadline()), Err(NativeError::Busy)));
        let pending = result.pending.unwrap();
        assert!(!pending.completed());
        gate.release();
        gate.wait(|| pending.completed());
        assert!(proof.lease(&deadline()).is_ok());
        drop(release);
    }
}
