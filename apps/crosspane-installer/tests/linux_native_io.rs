#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
use crosspane_installer::{agent_contract::*, platform::linux::native_io::*};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

static ROOT_ID: AtomicU64 = AtomicU64::new(0);
const START: &[u8] = b"Fri Oct  2 12:00:00 2026\n";
type FakeOutput = (Option<i32>, Vec<u8>, Vec<u8>);
type QueryHook = (usize, Box<dyn FnOnce() + Send>);
#[derive(Default)]
struct Runner {
    calls: Mutex<Vec<CommandSpec>>,
    name: Mutex<Option<Vec<u8>>>,
    start: Mutex<Option<Vec<u8>>>,
    oversized: bool,
    output: Mutex<Option<FakeOutput>>,
    query_outputs: Mutex<std::collections::VecDeque<FakeOutput>>,
    hook: Mutex<Option<QueryHook>>,
}
impl CommandRunner for Runner {
    fn run(
        &self,
        command: &CommandSpec,
        deadline: &Deadline,
    ) -> Result<CommandOutput, NativeError> {
        deadline.check()?;
        let count = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(command.clone());
            calls.len()
        };
        let hook = { self.hook.lock().unwrap().take() };
        if let Some((at, hook)) = hook {
            if at == count {
                hook();
            } else {
                *self.hook.lock().unwrap() = Some((at, hook));
            }
        }
        let scripted = self.query_outputs.lock().unwrap().pop_front();
        if let Some((code, stdout, stderr)) =
            scripted.or_else(|| self.output.lock().unwrap().clone())
        {
            return Ok(CommandOutput {
                code,
                stdout,
                stderr,
            });
        }
        let stdout = if self.oversized {
            vec![0; command.output_limit() + 1]
        } else if command.argv()[1] == "lstart=" {
            self.start
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| START.to_vec())
        } else {
            self.name
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| b"crosspane-agent\n".to_vec())
        };
        Ok(CommandOutput {
            code: Some(0),
            stdout,
            stderr: Vec::new(),
        })
    }
}
struct Probe {
    root: PathBuf,
    calls: AtomicU64,
    reuse: bool,
    wrong_uid: bool,
    wrong_exe: bool,
    absent: bool,
    script: Mutex<std::collections::VecDeque<Result<ProcessFacts, NativeError>>>,
}
impl ProcessProbe for Probe {
    fn snapshot(&self, _: u32, deadline: &Deadline) -> Result<ProcessFacts, NativeError> {
        deadline.check()?;
        if let Some(value) = self.script.lock().unwrap().pop_front() {
            return value;
        }
        if self.absent {
            return Err(NativeError::Unavailable);
        }
        Ok(ProcessFacts {
            uid: rustix::process::geteuid().as_raw() + u32::from(self.wrong_uid),
            executable: self.root.join(if self.wrong_exe {
                ".local/bin/foreign"
            } else {
                ".local/bin/crosspane-agent"
            }),
            generation: if self.reuse {
                self.calls.fetch_add(1, Ordering::Relaxed)
            } else {
                77
            },
        })
    }
}
struct Fixture {
    io: LinuxNativeIo,
    runner: Arc<Runner>,
    root: PathBuf,
    probe: Arc<Probe>,
}
impl Fixture {
    fn new(reuse: bool, wrong_uid: bool, wrong_exe: bool, absent: bool, oversized: bool) -> Self {
        let root = PathBuf::from(format!(
            "/tmp/cp47n-{}-{}",
            std::process::id(),
            ROOT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let runner = Arc::new(Runner {
            oversized,
            ..Default::default()
        });
        let probe = Arc::new(Probe {
            root: root.clone(),
            calls: AtomicU64::new(0),
            reuse,
            wrong_uid,
            wrong_exe,
            absent,
            script: Mutex::default(),
        });
        let io = LinuxNativeIo::scratch(&root, runner.clone(), probe.clone()).unwrap();
        let fixture = Self {
            io,
            runner,
            root,
            probe,
        };
        let bin = fixture.root.join(".local/bin");
        fixture
            .io
            .create_private_dir(&fixture.proof(), &bin)
            .unwrap();
        fs::write(fixture.io.target().agent_path(), b"inert test fixture").unwrap();
        fs::set_permissions(
            fixture.io.target().agent_path(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture
    }
    fn plain() -> Self {
        Self::new(false, false, false, false, false)
    }
    fn proof(&self) -> SupportProof {
        self.io.scratch_support(facts(&self.io)).unwrap()
    }
    fn bootstrap(&self, offset: i64) {
        let proof = self.proof();
        self.io
            .create_private_dir(&proof, self.io.target().runtime_dir())
            .unwrap();
        let started = parse_ps_start(START)
            .unwrap()
            .checked_add_signed(offset)
            .unwrap();
        let bytes = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"instance_id":9,"pid":4242,
            "started_unix_ms":started,"phase":"waiting_for_keystore","phase_seq":2,"keystore":null,
            "reason":null,"runtime_dir":self.io.target().runtime_dir()}),
        )
        .unwrap();
        self.io
            .atomic_write(
                &proof,
                &self.io.target().runtime_dir().join("bootstrap.json"),
                &bytes,
            )
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
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
        session_id: "test-session".into(),
        session_type: "wayland".into(),
        seat: "seat0".into(),
        active: true,
    }
}
fn deadline() -> Deadline {
    Deadline::new(1000, Cancellation::default()).unwrap()
}

#[test]
fn scratch_admission_cannot_be_reused_for_another_or_production_target() {
    let a = Fixture::plain();
    let b = Fixture::plain();
    let proof = a.proof();
    assert_eq!(proof.check(&b.io), Err(NativeError::Unsupported));
    assert_eq!(a.io.target().source(), ObservationSource::Demo);
    assert!(LinuxNativeIo::selected(a.io.target().paths().clone()).is_err());
    assert!(
        LinuxNativeIo::scratch(
            &a.root,
            a.runner.clone(),
            Arc::new(Probe {
                root: a.root.clone(),
                calls: AtomicU64::new(0),
                reuse: false,
                wrong_uid: false,
                wrong_exe: false,
                absent: false,
                script: Mutex::default()
            })
        )
        .is_err()
    );
    assert_eq!(proof.revalidate(&a.io, &facts(&a.io)), Ok(()));
    let mut changed = facts(&a.io);
    changed.session_id = "replaced-session".into();
    assert_eq!(
        proof.revalidate(&a.io, &changed),
        Err(NativeError::Unsupported)
    );
    assert_eq!(proof.check(&a.io), Err(NativeError::Unsupported));
}
#[test]
fn complete_support_admission_fails_closed_for_unknown_or_unsupported_facts() {
    let fixture = Fixture::plain();
    for index in 0..13 {
        let mut f = facts(&fixture.io);
        match index {
            0 => f.arch_based = false,
            1 => f.hyprland_version = [0, 55, 9],
            2 => f.protocols_ready = false,
            3 => f.runtime_libraries_ready = false,
            4 => f.uwsm_managed = false,
            5 => f.graphical_target_active = false,
            6 => f.graphical_sessions = 0,
            7 => f.graphical_sessions = 2,
            8 => f.session_type = "x11".into(),
            9 => f.seat.clear(),
            10 => f.uid += 1,
            11 => f.active = false,
            _ => f.architecture = "unsupported".into(),
        }
        assert!(matches!(
            fixture.io.scratch_support(f),
            Err(NativeError::Unsupported)
        ));
    }
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    assert!(!fixture.root.join("run").exists());
}
#[test]
fn private_atomic_files_and_lock_exclusion_are_bounded_and_owned() {
    let fixture = Fixture::plain();
    let proof = fixture.proof();
    let state = fixture.root.join(".local/state/crosspane/installer");
    fixture.io.create_private_dir(&proof, &state).unwrap();
    assert_eq!(
        fs::metadata(&state).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let path = state.join("intent.json");
    fixture.io.atomic_write(&proof, &path, b"intent").unwrap();
    assert_eq!(fixture.io.read(&path, 6, true).unwrap(), b"intent");
    assert_eq!(fixture.io.read(&path, 5, true), Err(NativeError::Oversize));
    assert_eq!(
        fixture.io.read(&path, MAX_FILE_BYTES + 1, true),
        Err(NativeError::Invalid)
    );
    let metadata = fixture.io.metadata(&path).unwrap().unwrap();
    assert_eq!(metadata.st_mode & 0o777, 0o600);
    assert!(
        fixture
            .io
            .metadata(&state.join("absent"))
            .unwrap()
            .is_none()
    );
    let lock_path = state.join("install.lock");
    let lock = fixture.io.lock(&proof, &lock_path).unwrap();
    assert!(matches!(
        fixture.io.lock(&proof, &lock_path),
        Err(NativeError::Busy)
    ));
    drop(lock);
    drop(fixture.io.lock(&proof, &lock_path).unwrap());
    fixture.io.atomic_write(&proof, &path, b"outcome").unwrap();
    assert_eq!(fixture.io.read(&path, 7, true).unwrap(), b"outcome");
    assert!(fs::read_dir(&state).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".crosspane-")
    }));
}
#[test]
fn symlinks_hardlinks_foreign_modes_and_traversal_never_authorize_writes() {
    let fixture = Fixture::plain();
    let proof = fixture.proof();
    let dir = fixture.root.join("private");
    fixture.io.create_private_dir(&proof, &dir).unwrap();
    let original = dir.join("original");
    fixture.io.atomic_write(&proof, &original, b"kept").unwrap();
    let link = dir.join("link");
    symlink(&original, &link).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &link, b"bad"),
        Err(NativeError::Foreign)
    );
    assert!(fixture.io.read(&link, 100, true).is_err());
    let hard = dir.join("hard");
    fs::hard_link(&original, &hard).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &hard, b"bad"),
        Err(NativeError::Foreign)
    );
    assert_eq!(
        fixture.io.read(&original, 100, true),
        Err(NativeError::Foreign)
    );
    fs::remove_file(hard).unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        fixture.io.atomic_write(&proof, &original, b"bad"),
        Err(NativeError::Foreign)
    );
    assert!(
        fixture
            .io
            .read(&dir.join("../private/original"), 100, false)
            .is_err()
    );
    assert!(
        fixture
            .io
            .atomic_write(
                &proof,
                &PathBuf::from("/nonexistent/crosspane-test/outside"),
                b"bad"
            )
            .is_err()
    );
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(fixture.io.read(&original, 100, false).is_err());
    assert_eq!(fs::read(&original).unwrap(), b"kept");
}
#[test]
fn selected_paths_are_read_only_validated_and_private_runtime_modes_are_required() {
    let fixture = Fixture::plain();
    fixture.bootstrap(0);
    let proof = fixture.proof();
    let path = fixture.io.target().runtime_dir().join("bootstrap.json");
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert_eq!(
        fixture.io.read(&path, 4096, true),
        Err(NativeError::Foreign)
    );
    fs::set_permissions(
        fixture.io.target().runtime_dir(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let foreign = fixture.root.join(".config");
    symlink(fixture.root.join("run"), &foreign).unwrap();
    assert_eq!(fixture.io.validate_target(), Err(NativeError::Foreign));
    assert_eq!(
        fixture.io.atomic_write(&proof, &path, b"bad"),
        Err(NativeError::Foreign)
    );
}
#[test]
fn child_commands_have_explicit_environment_bounds_and_no_shell() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let env = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    assert_eq!(env.values()["LC_ALL"], "C");
    assert_eq!(env.values()["TZ"], "UTC");
    for key in [
        "DISPLAY",
        "PULSE_SERVER",
        "PIPEWIRE_REMOTE",
        "CROSSPANE_AUDIO",
    ] {
        assert!(!env.values().contains_key(key));
    }
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("LD_PRELOAD".into(), "foreign".into())])
        )
        .is_err()
    );
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([(
                "DBUS_SESSION_BUS_ADDRESS".into(),
                "unix:path=/nonexistent/foreign".into()
            )])
        )
        .is_err()
    );
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("WAYLAND_DISPLAY".into(), "../foreign".into())])
        )
        .is_err()
    );
    for (exe, args, size) in [
        ("/bin/sh", vec![], 1),
        ("relative", vec![], 1),
        ("/bin/ps", vec!["x".repeat(4097)], 1),
        ("/bin/ps", vec!["x".into(); 33], 1),
        ("/bin/ps", vec![], MAX_COMMAND_BYTES + 1),
    ] {
        assert!(CommandSpec::new(exe.into(), args, env.clone(), size).is_err());
    }
    let wrong = ChildEnvironment::selected(other.io.target(), BTreeMap::new()).unwrap();
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        wrong,
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run(&spec, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}
#[test]
fn ps_argv_environment_calendar_and_start_tolerance_are_exact() {
    let fixture = Fixture::plain();
    for offset in [-2000, 0, 2000] {
        fixture.bootstrap(offset);
        assert!(fixture.io.bootstrap(&deadline()).is_ok());
    }
    for offset in [-2001, 2001] {
        fixture.bootstrap(offset);
        assert!(fixture.io.bootstrap(&deadline()).is_err());
    }
    for call in fixture.runner.calls.lock().unwrap().iter() {
        assert_eq!(call.executable(), std::path::Path::new("/bin/ps"));
        assert!(
            call.argv() == ["-o", "lstart=", "-p", "4242"]
                || call.argv() == ["-o", "comm=", "-p", "4242"]
        );
        assert_eq!(call.environment().values()["LC_ALL"], "C");
        assert_eq!(call.environment().values()["TZ"], "UTC");
    }
    for bad in [
        b"Thu Oct 2 12:00:00 2026".as_slice(),
        b"Fri Oct 32 12:00:00 2026",
        b"Fri Oct 2 24:00:00 2026",
        b"Fri Oct 2 12:00:00 2026\n\n",
        b"Fri Oct +2 12:00:00 2026",
        b"Thu Feb 29 00:00:00 2025",
        b"",
    ] {
        assert!(parse_ps_start(bad).is_err());
    }
    assert_eq!(parse_ps_start(b"Thu Jan 1 00:00:00 1970\n").unwrap(), 0);
    assert!(parse_ps_start(b"Thu Feb 29 00:00:00 2024").is_ok());
}
#[test]
fn malformed_absent_foreign_processes_and_pid_reuse_fail_closed() {
    for flags in [
        (true, false, false, false),
        (false, true, false, false),
        (false, false, true, false),
        (false, false, false, true),
    ] {
        let fixture = Fixture::new(flags.0, flags.1, flags.2, flags.3, false);
        assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    }
    let fixture = Fixture::plain();
    for name in [
        b"foreign\n".as_slice(),
        b"crosspane-agent\ncrosspane-agent\n",
        b"crosspane-agent\n\n",
        b"",
    ] {
        *fixture.runner.name.lock().unwrap() = Some(name.to_vec());
        assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    }
    *fixture.runner.name.lock().unwrap() = None;
    *fixture.runner.start.lock().unwrap() = Some(b"malformed".to_vec());
    assert!(fixture.io.process_identity(4242, &deadline()).is_err());
    assert!(fixture.io.process_identity(0, &deadline()).is_err());
    assert!(fixture.io.bootstrap(&deadline()).is_err());
}
#[test]
fn cancellation_timeouts_oversize_outputs_and_expired_support_refuse_mutation() {
    let fixture = Fixture::new(false, false, false, false, true);
    assert_eq!(
        fixture.io.process_identity(4242, &deadline()),
        Err(NativeError::Oversize)
    );
    let fixture = Fixture::plain();
    let cancel = Cancellation::default();
    let d = Deadline::new(1000, cancel.clone()).unwrap();
    cancel.cancel();
    assert_eq!(
        fixture.io.process_identity(4242, &d),
        Err(NativeError::Cancelled)
    );
    let d = Deadline::new(1, Cancellation::default()).unwrap();
    thread::sleep(Duration::from_millis(3));
    assert_eq!(d.check(), Err(NativeError::Timeout));
    let proof = fixture.proof();
    thread::sleep(SUPPORT_LIFETIME + Duration::from_millis(10));
    let dir = fixture.root.join("never-created");
    assert_eq!(
        fixture.io.create_private_dir(&proof, &dir),
        Err(NativeError::Unsupported)
    );
    assert!(matches!(
        fixture.io.lock(&proof, &fixture.root.join("lock")),
        Err(NativeError::Unsupported)
    ));
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run_mutation(&proof, &spec, &deadline()),
        Err(NativeError::Unsupported)
    ));
    assert!(matches!(
        CommandSpec::new(
            "/usr/bin/systemctl".into(),
            vec![
                "--user".into(),
                "start".into(),
                "crosspane-agent.service".into(),
            ],
            ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
            256,
        ),
        Err(NativeError::Invalid)
    ));
    assert!(!dir.exists());
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn scripted_exit_bootstrap_replacement_regression_and_ps_failure_are_refused() {
    let fixture = Fixture::plain();
    let stable = ProcessFacts {
        uid: fixture.io.target().paths().uid,
        executable: fixture.io.target().agent_path(),
        generation: 77,
    };
    *fixture.probe.script.lock().unwrap() =
        [Ok(stable.clone()), Err(NativeError::Unavailable)].into();
    assert_eq!(
        fixture.io.process_identity(4242, &deadline()),
        Err(NativeError::Unavailable)
    );
    assert!(fixture.io.process_identity(4242, &deadline()).is_ok());
    for query in 0..2 {
        for (code, stderr) in [
            (Some(1), Vec::new()),
            (None, Vec::new()),
            (Some(0), b"untrusted error".to_vec()),
        ] {
            let mut outputs = vec![
                (Some(0), START.to_vec(), Vec::new()),
                (Some(0), b"crosspane-agent\n".to_vec(), Vec::new()),
            ];
            outputs[query].0 = code;
            outputs[query].2 = stderr;
            *fixture.runner.query_outputs.lock().unwrap() = outputs.into();
            assert!(fixture.io.process_identity(4242, &deadline()).is_err());
            fixture.runner.query_outputs.lock().unwrap().clear();
        }
    }
    for field in ["instance_id", "phase_seq"] {
        fixture.bootstrap(0);
        let path = fixture.io.target().runtime_dir().join("bootstrap.json");
        let count = fixture.runner.calls.lock().unwrap().len() + 2;
        *fixture.runner.hook.lock().unwrap() = Some((
            count,
            Box::new(move || {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                json[field] = serde_json::json!(if field == "phase_seq" { 1 } else { 10 });
                fs::write(path, serde_json::to_vec(&json).unwrap()).unwrap();
            }),
        ));
        assert_eq!(
            fixture.io.bootstrap(&deadline()).unwrap_err(),
            NativeError::Foreign
        );
    }
    fixture.bootstrap(0);
    let changed = ProcessFacts {
        generation: 78,
        ..stable.clone()
    };
    *fixture.probe.script.lock().unwrap() = [
        Ok(stable.clone()),
        Ok(stable),
        Ok(changed.clone()),
        Ok(changed),
    ]
    .into();
    assert_eq!(
        fixture.io.bootstrap(&deadline()).unwrap_err(),
        NativeError::Foreign
    );
}

#[test]
fn systemctl_is_never_admitted_and_fake_mutation_executor_requires_selected_proof() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let proof = fixture.proof();
    let environment = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    for operation in [
        "daemon-reload",
        "enable",
        "start",
        "stop",
        "is-enabled",
        "is-active",
        "show",
    ] {
        let mut argv = vec!["--user".into(), operation.into()];
        if operation != "daemon-reload" {
            argv.push("crosspane-agent.service".into());
        }
        if operation == "show" {
            argv.push("--property=FragmentPath,DropInPaths,ExecStart,User,ActiveState,UnitFileState,MainPID".into());
        }
        for executable in ["/usr/bin/systemctl", "/bin/systemctl"] {
            assert!(matches!(
                CommandSpec::new(executable.into(), argv.clone(), environment.clone(), 256),
                Err(NativeError::Invalid)
            ));
        }
        assert!(fixture.runner.calls.lock().unwrap().is_empty());
    }
    let command = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        environment,
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture
            .io
            .run_mutation(&other.proof(), &command, &deadline()),
        Err(NativeError::Unsupported)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    // Only the injected runner executes this generic proof-gated path.
    fixture
        .io
        .run_mutation(&proof, &command, &deadline())
        .unwrap();
    assert_eq!(fixture.runner.calls.lock().unwrap().len(), 1);
    fixture.runner.calls.lock().unwrap().clear();
    let mut changed = facts(&fixture.io);
    changed.active = false;
    assert_eq!(
        proof.revalidate(&fixture.io, &changed),
        Err(NativeError::Unsupported)
    );
    assert!(matches!(
        fixture.io.run_mutation(&proof, &command, &deadline()),
        Err(NativeError::Unsupported)
    ));
    let foreign = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(other.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    assert!(matches!(
        fixture.io.run(&foreign, &deadline()),
        Err(NativeError::Foreign)
    ));
    let fresh = fixture.proof();
    assert!(matches!(
        fixture.io.run_mutation(&fresh, &foreign, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    let observed = CommandSpec::new(
        "/bin/ps".into(),
        foreign.argv().to_vec(),
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    fs::create_dir_all(&fixture.io.target().paths().prefix).unwrap();
    symlink(&other.root, &fixture.io.target().paths().config_home).unwrap();
    assert!(matches!(
        fixture.io.run(&observed, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(matches!(
        fixture.io.run_mutation(&fresh, &command, &deadline()),
        Err(NativeError::Foreign)
    ));
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
}

#[test]
fn every_mutation_entry_checks_revoked_other_target_proofs_and_lock_files() {
    let fixture = Fixture::plain();
    let other = Fixture::plain();
    let proof = fixture.proof();
    let mut changed = facts(&fixture.io);
    changed.active = false;
    assert_eq!(
        proof.revalidate(&fixture.io, &changed),
        Err(NativeError::Unsupported)
    );
    let spec = CommandSpec::new(
        "/bin/ps".into(),
        vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()],
        ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap(),
        256,
    )
    .unwrap();
    for proof in [proof, other.proof()] {
        assert!(matches!(
            fixture.io.run_mutation(&proof, &spec, &deadline()),
            Err(NativeError::Unsupported)
        ));
        assert!(matches!(
            fixture.io.lock(&proof, &fixture.root.join("lock")),
            Err(NativeError::Unsupported)
        ));
        assert_eq!(
            fixture
                .io
                .create_private_dir(&proof, &fixture.root.join("no-dir")),
            Err(NativeError::Unsupported)
        );
    }
    assert!(!fixture.root.join("lock").exists());
    assert!(!fixture.root.join("no-dir").exists());
    assert!(fixture.runner.calls.lock().unwrap().is_empty());
    let good = fixture.proof();
    let path = fixture.root.join("lock");
    let original = fixture.root.join("original");
    fs::write(&original, b"preserved").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&original, &path).unwrap();
    assert!(fixture.io.lock(&good, &path).is_err());
    fs::remove_file(&path).unwrap();
    fs::hard_link(&original, &path).unwrap();
    assert_eq!(
        fixture.io.lock(&good, &path).unwrap_err(),
        NativeError::Foreign
    );
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"foreign").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        fixture.io.lock(&good, &path).unwrap_err(),
        NativeError::Foreign
    );
    assert_eq!(fs::read(&original).unwrap(), b"preserved");
    assert_eq!(fs::read(&path).unwrap(), b"foreign");
}

#[test]
fn command_environment_aggregate_control_and_combined_output_bounds() {
    let fixture = Fixture::plain();
    let env = ChildEnvironment::selected(fixture.io.target(), BTreeMap::new()).unwrap();
    let args = vec!["-o".into(), "comm=".into(), "-p".into(), "4242".into()];
    for args in [
        vec!["x".repeat(4096); 9],
        vec!["nul\0".into()],
        vec!["line\n".into()],
    ] {
        assert!(CommandSpec::new("/bin/ps".into(), args, env.clone(), 256).is_err());
    }
    assert!(CommandSpec::new("/bin/ps".into(), args.clone(), env.clone(), 0).is_err());
    for value in ["x".repeat(4097), "bad\0".into(), "bad\n".into()] {
        assert!(
            ChildEnvironment::selected(
                fixture.io.target(),
                BTreeMap::from([("HYPRLAND_INSTANCE_SIGNATURE".into(), value)])
            )
            .is_err()
        );
    }
    assert!(
        ChildEnvironment::selected(
            fixture.io.target(),
            BTreeMap::from([("HYPRLAND_INSTANCE_SIGNATURE".into(), "x".repeat(4096))])
        )
        .is_ok()
    );
    let spec = CommandSpec::new("/bin/ps".into(), args, env, 256).unwrap();
    *fixture.runner.output.lock().unwrap() = Some((Some(0), vec![0; 128], vec![0; 128]));
    assert!(fixture.io.run(&spec, &deadline()).is_ok());
    *fixture.runner.output.lock().unwrap() = Some((Some(0), vec![0; 128], vec![0; 129]));
    assert_eq!(
        fixture.io.run(&spec, &deadline()).unwrap_err(),
        NativeError::Oversize
    );
}
