#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

// Runtime tests are owned by WP-4.8c and will be added in a separate runtime_tests block.
mod session_tests {
    mod support_report {
        use super::*;
        use crosspane_installer::platform::linux::native_io::*;
        use rustix::{
            fd::OwnedFd,
            fs::{self, AtFlags, Mode, OFlags},
        };
        use std::{
            collections::BTreeMap,
            io::{Read, Write},
            os::unix::net::UnixListener,
            sync::{
                Arc, Mutex,
                atomic::{AtomicU64, Ordering},
            },
            thread,
            time::Duration,
        };
        const START: &[u8] = b"Mon Sep 28 08:00:00 2026\n";
        static IDS: AtomicU64 = AtomicU64::new(1);
        struct Fake {
            path: Mutex<PathBuf>,
            error: Mutex<Option<NativeError>>,
            calls: Mutex<Vec<Vec<String>>>,
        }
        impl CommandRunner for Fake {
            fn run(
                &self,
                command: &CommandSpec,
                deadline: &Deadline,
            ) -> Result<CommandOutput, NativeError> {
                deadline.check()?;
                assert_eq!(command.executable(), std::path::Path::new("/bin/ps"));
                self.calls.lock().unwrap().push(command.argv().to_vec());
                if let Some(error) = *self.error.lock().unwrap() {
                    return Err(error);
                }
                let stdout = if command.argv()[1] == "lstart=" {
                    START.to_vec()
                } else {
                    b"crosspane-agent\n".to_vec()
                };
                Ok(CommandOutput {
                    code: Some(0),
                    stdout,
                    stderr: vec![],
                })
            }
        }
        impl ProcessProbe for Fake {
            fn snapshot(&self, pid: u32, deadline: &Deadline) -> Result<ProcessFacts, NativeError> {
                deadline.check()?;
                assert_eq!(pid, std::process::id());
                Ok(ProcessFacts {
                    uid: rustix::process::geteuid().as_raw(),
                    executable: self.path.lock().unwrap().clone(),
                    generation: 1,
                })
            }
        }
        struct Scratch {
            io: Arc<LinuxNativeIo>,
            fake: Arc<Fake>,
            root: OwnedFd,
            parent: OwnedFd,
            name: String,
            files: Vec<(OwnedFd, String)>,
            dirs: Vec<(OwnedFd, String)>,
        }
        impl Scratch {
            fn new() -> Self {
                let name = format!(
                    "crosspane-support-{}-{}",
                    std::process::id(),
                    IDS.fetch_add(1, Ordering::SeqCst)
                );
                let fake = Arc::new(Fake {
                    path: Mutex::new(PathBuf::new()),
                    error: Mutex::new(None),
                    calls: Mutex::new(vec![]),
                });
                let io = Arc::new(
                    LinuxNativeIo::scratch(
                        &PathBuf::from("/tmp").join(&name),
                        fake.clone(),
                        fake.clone(),
                    )
                    .unwrap(),
                );
                *fake.path.lock().unwrap() = io.target().agent_path();
                let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let parent = fs::open("/tmp", flags, Mode::empty()).unwrap();
                let root = fs::openat(&parent, &name, flags, Mode::empty()).unwrap();
                Self {
                    io,
                    fake,
                    root,
                    parent,
                    name,
                    files: vec![],
                    dirs: vec![],
                }
            }
            fn dir(&mut self, parent: &OwnedFd, name: &str) -> OwnedFd {
                fs::mkdirat(parent, name, Mode::from_raw_mode(0o700)).unwrap();
                self.dirs.push((parent.try_clone().unwrap(), name.into()));
                fs::openat(
                    parent,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .unwrap()
            }
            fn write(&mut self, parent: &OwnedFd, name: &str, bytes: &[u8], mode: u32) {
                let fd = fs::openat(
                    parent,
                    name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(mode),
                )
                .unwrap();
                std::fs::File::from(fd).write_all(bytes).unwrap();
                self.files.push((parent.try_clone().unwrap(), name.into()));
            }
            fn ready(&mut self) -> OwnedFd {
                let root = self.root.try_clone().unwrap();
                let run = self.dir(&root, "run");
                let runtime = self.dir(&run, "crosspane");
                let local = self.dir(&root, ".local");
                let bin = self.dir(&local, "bin");
                self.write(&bin, "crosspane-agent", b"inert fixture", 0o700);
                self.write(
                    &runtime,
                    "bootstrap.json",
                    &serde_json::to_vec(&self.bootstrap()).unwrap(),
                    0o600,
                );
                runtime
            }
            fn bootstrap(&self) -> BootstrapV1 {
                let mut value = parse_bootstrap(BOOTSTRAP).unwrap();
                value.pid = std::process::id();
                value.started_unix_ms = parse_ps_start(START).unwrap();
                value.runtime_dir = self.io.target().runtime_dir().to_string_lossy().into();
                value
            }
            fn status(&self) -> Vec<u8> {
                let mut value: serde_json::Value = serde_json::from_slice(STATUS).unwrap();
                let instance = &mut value["result"]["installer"]["instance"];
                instance["pid"] = std::process::id().into();
                instance["uid"] = self.io.target().paths().uid.into();
                instance["exe"] = self
                    .io
                    .target()
                    .agent_path()
                    .to_string_lossy()
                    .into_owned()
                    .into();
                instance["runtime_dir"] = self.bootstrap().runtime_dir.into();
                instance["started_unix_ms"] = self.bootstrap().started_unix_ms.into();
                serde_json::to_vec(&value).unwrap()
            }
            fn probes(&self) -> NativeSessionProbes {
                NativeSessionProbes::new(
                    self.io.clone(),
                    ChildEnvironment::selected(self.io.target(), BTreeMap::new()).unwrap(),
                    Arc::new(|| 901),
                )
                .unwrap()
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                for (parent, name) in self.files.iter().rev() {
                    fs::unlinkat(parent, name.as_str(), AtFlags::empty()).unwrap();
                }
                for (parent, name) in self.dirs.iter().rev() {
                    fs::unlinkat(parent, name.as_str(), AtFlags::REMOVEDIR).unwrap();
                }
                let expected = fs::fstat(&self.root).unwrap();
                let current =
                    fs::statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
                assert_eq!(
                    (expected.st_dev, expected.st_ino),
                    (current.st_dev, current.st_ino)
                );
                fs::unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR).unwrap();
            }
        }
        fn deadline() -> Deadline {
            Deadline::new(1000, Cancellation::default()).unwrap()
        }
        struct Server {
            thread: Option<thread::JoinHandle<()>>,
            stop: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for Server {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::SeqCst);
                if let Some(thread) = self.thread.take() {
                    let result = thread.join();
                    if !thread::panicking() {
                        result.unwrap();
                    }
                }
            }
        }
        fn start_server(
            scratch: &mut Scratch,
            response: Option<Vec<u8>>,
        ) -> (Server, std::sync::mpsc::Receiver<()>) {
            let runtime = scratch.ready();
            let listener = UnixListener::bind(scratch.io.target().socket_path()).unwrap();
            let stat = fs::statat(&runtime, "agent.sock", AtFlags::SYMLINK_NOFOLLOW).unwrap();
            assert_eq!(stat.st_uid, scratch.io.target().paths().uid);
            assert_eq!(stat.st_mode & 0o170000, 0o140000);
            fs::chmodat(
                &runtime,
                "agent.sock",
                Mode::from_raw_mode(0o600),
                AtFlags::empty(),
            )
            .unwrap();
            scratch.files.push((runtime, "agent.sock".into()));
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let stopped = stop.clone();
            let (sent, requested) = std::sync::mpsc::sync_channel(1);
            let thread = thread::spawn(move || {
                let end = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < end =>
                        {
                            thread::sleep(Duration::from_millis(2))
                        }
                        other => panic!("owned accept {other:?}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut bytes = vec![];
                loop {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    let mut one = [0];
                    match stream.read(&mut one) {
                        Ok(0) => panic!("request closed early"),
                        Ok(_) if one[0] == b'\n' => break,
                        Ok(_) => {
                            bytes.push(one[0]);
                            assert!(bytes.len() < 65536);
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) && std::time::Instant::now() < end =>
                        {
                            continue;
                        }
                        other => panic!("owned request {other:?}"),
                    }
                }
                assert_eq!(bytes, b"{\"cmd\":\"status\"}");
                sent.send(()).unwrap();
                if let Some(response) = response {
                    stream.write_all(&response).unwrap();
                    stream.write_all(b"\n").unwrap();
                } else {
                    while !stopped.load(Ordering::SeqCst) && std::time::Instant::now() < end {
                        thread::sleep(Duration::from_millis(2));
                    }
                    assert!(
                        matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock)
                    );
                    return;
                }
                loop {
                    let mut one = [0];
                    match stream.read(&mut one) {
                        Ok(0) => break,
                        Ok(_) => panic!("unexpected second request"),
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) && std::time::Instant::now() < end =>
                        {
                            continue;
                        }
                        other => panic!("owned close {other:?}"),
                    }
                }
                assert!(
                    matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock)
                );
            });
            (
                Server {
                    thread: Some(thread),
                    stop,
                },
                requested,
            )
        }
        fn owned_exchange(
            scratch: &mut Scratch,
            response: Option<Vec<u8>>,
            deadline: &Deadline,
        ) -> Fact<InstalledAgentFacts> {
            let (_server, _requested) = start_server(scratch, response);
            scratch.probes().installed_agent(deadline)
        }
        fn assert_worker_released_with_server_alive(scratch: &Scratch, server: &Server) {
            use crosspane_installer::platform::linux::transport::LinuxAgentPort;
            assert!(!server.thread.as_ref().unwrap().is_finished());
            let end = std::time::Instant::now() + Duration::from_millis(500);
            let mut ports = vec![];
            while ports.len() < 4 && std::time::Instant::now() < end {
                match LinuxAgentPort::new(scratch.io.clone(), None, Arc::new(|| 902)) {
                    Ok(port) => ports.push(port),
                    Err(NativeError::Busy) => thread::sleep(Duration::from_millis(2)),
                    other => panic!("fresh owned port {other:?}"),
                }
            }
            assert_eq!(
                ports.len(),
                4,
                "timed-out/cancelled worker still owns its slot"
            );
            assert!(!server.thread.as_ref().unwrap().is_finished());
        }
        fn pass(scratch: &Scratch) -> (EffectiveEnvironment, DetectionPass) {
            let mut s = session();
            s.uid = scratch.io.target().paths().uid;
            s.selected_environment.runtime_dir = scratch.io.target().paths().runtime_home.clone();
            s.selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .uid = Some(s.uid);
            (
                s.selected_environment.clone(),
                DetectionPass {
                    os: OsFacts {
                        family: s.os,
                        path: Some("/usr/lib/os-release".into()),
                    },
                    architecture: known(parse_architecture(std::env::consts::ARCH).unwrap()),
                    logind: known(LogindFacts {
                        selected_session: s.selected_session,
                        graphical_sessions: s.graphical_sessions,
                    }),
                    manager: known(ManagerFacts {
                        compositor_managed: s.compositor_managed,
                        graphical_target_active: s.graphical_target_active,
                        compositor_pid: Some(71),
                    }),
                    manager_environment: known(s.selected_environment),
                    hyprland: known(HyprlandFacts {
                        version: s.compositor_version,
                        pid: 71,
                    }),
                    registry: known(RegistryFacts {
                        globals: vec![],
                        protocols: s.protocols,
                        pid: 71,
                    }),
                    lineage: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 299),
                    installed_agent: Fact::issue(ProbeIssue::Missing, ObservationSource::Demo, 300),
                    reduced_motion: Fact::issue(
                        ProbeIssue::Unverified,
                        ObservationSource::Demo,
                        301,
                    ),
                },
            )
        }
        fn expected_observations(io: &LinuxNativeIo) -> SupportObservations {
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
                session_id: "c7".into(),
                session_type: "wayland".into(),
                seat: "seat0".into(),
                active: true,
            }
        }
        #[test]
        fn os_fallback_on_every_read_error_records_supplier_and_original_receipt() {
            for error in [
                NativeError::Invalid,
                NativeError::Unavailable,
                NativeError::Foreign,
                NativeError::Timeout,
                NativeError::Cancelled,
                NativeError::Busy,
                NativeError::Oversize,
                NativeError::Unsupported,
                NativeError::OutcomeUnknown,
            ] {
                let mut calls = vec![];
                let time = AtomicU64::new(10);
                let facts = os_from_reader(
                    |request, _| {
                        calls.push(request.clone());
                        time.fetch_add(10, Ordering::SeqCst);
                        if calls.len() == 1 {
                            Err(error)
                        } else {
                            Ok(SystemBytes {
                                path: "/usr/lib/os-release".into(),
                                bytes: b"ID=arch\n".to_vec(),
                                file_size: 8,
                            })
                        }
                    },
                    &deadline(),
                    ObservationSource::Demo,
                    &|| time.load(Ordering::SeqCst),
                );
                assert!(matches!(
                    calls.as_slice(),
                    [SystemRead::OsRelease, SystemRead::OsReleaseFallback]
                ));
                assert_eq!(
                    facts.family,
                    Fact::known(OsFamily::Arch, ObservationSource::Demo, 30)
                );
                assert_eq!(facts.path, Some("/usr/lib/os-release".into()));
            }
        }
        #[test]
        fn os_success_even_malformed_never_falls_back_and_both_fail_unverified() {
            for (bytes, expected) in [
                (b"ID=arch\n".as_slice(), Ok(OsFamily::Arch)),
                (b"ID=\"arch".as_slice(), Err(ProbeIssue::Malformed)),
            ] {
                let mut count = 0;
                let result = os_from_reader(
                    |request, _| {
                        count += 1;
                        assert!(matches!(request, SystemRead::OsRelease));
                        Ok(SystemBytes {
                            path: "/etc/os-release".into(),
                            bytes: bytes.into(),
                            file_size: bytes.len() as u64,
                        })
                    },
                    &deadline(),
                    ObservationSource::Demo,
                    &|| 5,
                );
                assert_eq!(count, 1);
                assert_eq!(result.family.value, expected);
                assert_eq!(result.path, Some("/etc/os-release".into()));
            }
            let result = os_from_reader(
                |_, _| Err(NativeError::Foreign),
                &deadline(),
                ObservationSource::Demo,
                &|| 6,
            );
            assert_eq!(result.family.value, Err(ProbeIssue::Unverified));
            assert_eq!(result.path, None);
        }
        #[test]
        fn every_required_backend_nonready_carries_literal_state_reason_and_optionals_never_block()
        {
            let facts = agent_facts(STATUS, ObservationSource::Demo, 10).unwrap();
            let StatusAdmission::Supported(health) = facts.status else {
                panic!()
            };
            let mut base = health.installer().clone();
            base.startup_recovery = StartupRecovery::NothingParked;
            for index in 0..15 {
                for state in [
                    BackendState::Ready,
                    BackendState::Blocked,
                    BackendState::Missing,
                    BackendState::Failed,
                ] {
                    let mut status = base.clone();
                    let fact = &mut status.backends[index];
                    fact.state = state;
                    fact.reason = Some(BackendReason::WorkerExited);
                    let expected =
                        if [9, 11, 12, 13, 14].contains(&index) || state == BackendState::Ready {
                            BackendReadiness::Ready
                        } else if state == BackendState::Blocked {
                            BackendReadiness::Pending(vec![fact.clone()])
                        } else {
                            BackendReadiness::NotReady(vec![fact.clone()])
                        };
                    assert_eq!(backend_readiness(&status), expected, "{index} {state:?}");
                }
            }
        }
        #[test]
        fn backend_invalid_order_missing_duplicates_and_failed_recovery_zero_pending() {
            let facts = agent_facts(STATUS, ObservationSource::Demo, 10).unwrap();
            let StatusAdmission::Supported(health) = facts.status else {
                panic!()
            };
            let base = health.installer().clone();
            for index in 0..15 {
                let mut status = base.clone();
                status.backends.remove(index);
                assert_eq!(backend_readiness(&status), BackendReadiness::Invalid);
                let mut status = base.clone();
                status
                    .backends
                    .insert(index, status.backends[index].clone());
                assert_eq!(backend_readiness(&status), BackendReadiness::Invalid);
                if index < 14 {
                    let mut status = base.clone();
                    status.backends.swap(index, index + 1);
                    assert_eq!(backend_readiness(&status), BackendReadiness::Invalid);
                }
            }
            assert_eq!(
                backend_readiness(&base),
                BackendReadiness::Recovery {
                    pending: 0,
                    startup: StartupRecovery::Failed
                }
            );
            let mut status = base;
            status.startup_recovery = StartupRecovery::Restored;
            status.recovery_pending = 5;
            assert_eq!(
                backend_readiness(&status),
                BackendReadiness::Recovery {
                    pending: 5,
                    startup: StartupRecovery::Restored
                }
            );
        }
        #[test]
        fn start_hyprland_launcher_admits_only_its_direct_child_compositor() {
            let scratch = Scratch::new();
            let lineage = |launcher: u32, exe: &str, parent: u32| CompositorLineage {
                launcher_pid: launcher,
                launcher_executable: exe.into(),
                compositor_pid: 71,
                compositor_parent: parent,
            };
            // uwsm MainPID 70 is start-hyprland; Hyprland 71 is its direct child (real shape).
            let (env, mut p) = pass(&scratch);
            p.manager.value.as_mut().unwrap().compositor_pid = Some(70);
            p.lineage = known(lineage(70, START_HYPRLAND, 70));
            let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
            assert_eq!(result.report.eligibility, Eligibility::Supported);
            assert!(result.proof.is_some());
            for (value, expected) in [
                // Grandchild: Hyprland's parent isn't MainPID.
                (Ok(lineage(70, START_HYPRLAND, 69)), ProbeIssue::Foreign),
                // MainPID isn't the launcher.
                (Ok(lineage(70, "/usr/bin/uwsm", 70)), ProbeIssue::Foreign),
                // Evidence about other processes.
                (Ok(lineage(68, START_HYPRLAND, 70)), ProbeIssue::Foreign),
                (Err(ProbeIssue::Unavailable), ProbeIssue::Unavailable),
                (Err(ProbeIssue::Foreign), ProbeIssue::Foreign),
            ] {
                let (env, mut p) = pass(&scratch);
                p.manager.value.as_mut().unwrap().compositor_pid = Some(70);
                p.lineage.value = value;
                let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
                assert_eq!(result.report.eligibility, Eligibility::Pending(expected));
                assert!(result.proof.is_none());
            }
            // The IPC and Wayland peers must still be one process, launcher or not.
            let (env, mut p) = pass(&scratch);
            p.manager.value.as_mut().unwrap().compositor_pid = Some(70);
            p.lineage = known(lineage(70, START_HYPRLAND, 70));
            p.registry.value.as_mut().unwrap().pid = 72;
            let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
            assert_eq!(
                result.report.eligibility,
                Eligibility::Pending(ProbeIssue::Foreign)
            );
        }
        #[test]
        fn report_admits_exact_fresh_proof_preserving_receipts_not_readiness() {
            let scratch = Scratch::new();
            let (env, acquired) = pass(&scratch);
            let result = assemble_support(&scratch.io, env, acquired, runtime(), &deadline());
            assert_eq!(result.report.eligibility, Eligibility::Supported);
            assert_eq!(result.os_path, Some("/usr/lib/os-release".into()));
            assert_eq!(result.report.session.os.observed_at_ms, 17);
            assert_eq!(
                result.report.runtime.pipewire.value,
                Err(ProbeIssue::Unverified)
            );
            assert_eq!(
                result.report.installed_agent.value,
                Err(ProbeIssue::Missing)
            );
            assert_eq!(
                result.report.reduced_motion.value,
                Err(ProbeIssue::Unverified)
            );
            let first = result.proof.unwrap();
            first
                .revalidate(&scratch.io, &expected_observations(&scratch.io))
                .unwrap();
            let (env, pass) = pass(&scratch);
            let second = assemble_support(&scratch.io, env, pass, runtime(), &deadline())
                .proof
                .unwrap();
            let mut changed = expected_observations(&scratch.io);
            changed.active = false;
            assert!(first.revalidate(&scratch.io, &changed).is_err());
            second.check(&scratch.io).unwrap();
        }
        #[test]
        fn compatibility_is_advisory_and_session_authority_still_gates_proof() {
            let scratch = Scratch::new();
            for field in 0..15 {
                let (env, mut p) = pass(&scratch);
                let mut r = runtime();
                let expected = match field {
                    0 => {
                        p.os.family.value = Ok(OsFamily::Other("debian".into()));
                        Eligibility::NotSupported(UnsupportedReason::OperatingSystem)
                    }
                    1 => {
                        p.architecture.value = Ok(Architecture::Other("riscv64".into()));
                        Eligibility::NotSupported(UnsupportedReason::Architecture)
                    }
                    2 => {
                        p.hyprland.value.as_mut().unwrap().version.value = Ok([0, 55, 0]);
                        Eligibility::NotSupported(UnsupportedReason::HyprlandVersion)
                    }
                    3 => {
                        p.registry.value.as_mut().unwrap().protocols.value = Ok(false);
                        Eligibility::NotSupported(UnsupportedReason::RequiredProtocols)
                    }
                    4 => {
                        p.manager.value.as_mut().unwrap().compositor_managed.value = Ok(false);
                        Eligibility::NotSupported(UnsupportedReason::Uwsm)
                    }
                    5 => {
                        p.logind
                            .value
                            .as_mut()
                            .unwrap()
                            .selected_session
                            .value
                            .as_mut()
                            .unwrap()
                            .as_mut()
                            .unwrap()
                            .session
                            .kind = Some("x11".into());
                        Eligibility::NotSupported(UnsupportedReason::SessionType)
                    }
                    6 => {
                        r.video_feature.value = Ok(false);
                        Eligibility::NotSupported(UnsupportedReason::VideoFeature)
                    }
                    7 => {
                        r.opus.value = Ok(false);
                        Eligibility::NotSupported(UnsupportedReason::RuntimeLibrary)
                    }
                    8 => {
                        p.logind.value = Err(ProbeIssue::Timeout);
                        Eligibility::Pending(ProbeIssue::Timeout)
                    }
                    9 => {
                        p.manager_environment.value = Err(ProbeIssue::Malformed);
                        Eligibility::Pending(ProbeIssue::Malformed)
                    }
                    10 => {
                        p.registry.value.as_mut().unwrap().pid = 72;
                        Eligibility::Pending(ProbeIssue::Foreign)
                    }
                    11 => {
                        p.hyprland.value.as_mut().unwrap().pid = 72;
                        Eligibility::Pending(ProbeIssue::Foreign)
                    }
                    12 => {
                        p.manager.value.as_mut().unwrap().compositor_pid = None;
                        Eligibility::Pending(ProbeIssue::Unverified)
                    }
                    13 => {
                        p.manager.value = Err(ProbeIssue::Unavailable);
                        Eligibility::Pending(ProbeIssue::Unavailable)
                    }
                    _ => {
                        r.libraries[0].resolved.value = Err(ProbeIssue::Unavailable);
                        Eligibility::Pending(ProbeIssue::Unavailable)
                    }
                };
                let result = assemble_support(&scratch.io, env, p, r, &deadline());
                assert_eq!(result.report.eligibility, expected, "{field}");
                if matches!(field, 0..=3 | 6 | 7 | 14) {
                    let proof = result.proof.expect("session authority is sufficient");
                    assert_eq!(proof.advisory().eligibility, expected);
                    assert_eq!(proof.check_agent_compatibility().is_ok(), field == 14);
                    assert!(!proof.advisory().notes.is_empty());
                } else {
                    assert!(result.proof.is_none(), "{field}");
                }
                assert!(scratch.fake.calls.lock().unwrap().is_empty());
            }
        }
        #[test]
        fn support_deadline_cancel_foreign_target_and_absent_selected_environment_never_admit() {
            let scratch = Scratch::new();
            let cancel = Cancellation::default();
            cancel.cancel();
            let (env, p) = pass(&scratch);
            let result = assemble_support(
                &scratch.io,
                env,
                p,
                runtime(),
                &Deadline::new(1000, cancel).unwrap(),
            );
            assert_eq!(
                result.report.eligibility,
                Eligibility::Pending(ProbeIssue::Cancelled)
            );
            assert!(result.proof.is_none());
            for field in 0..3 {
                let (mut env, mut p) = pass(&scratch);
                if field == 0 {
                    env.runtime_dir = "/run/user/other".into()
                } else if field == 1 {
                    env.wayland_display.clear()
                } else {
                    env.hyprland_instance_signature.clear()
                };
                p.manager_environment = known(env.clone());
                let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
                assert!(result.proof.is_none());
            }
        }
        #[test]
        fn native_missing_is_only_fresh_metadata_absence_and_existing_failures_keep_cause() {
            let mut scratch = Scratch::new();
            assert_eq!(
                scratch.probes().installed_agent(&deadline()).value,
                Err(ProbeIssue::Foreign)
            );
            let root = scratch.root.try_clone().unwrap();
            let run = scratch.dir(&root, "run");
            assert_eq!(
                scratch.probes().installed_agent(&deadline()).value,
                Err(ProbeIssue::Missing)
            );
            let runtime = scratch.dir(&run, "crosspane");
            assert_eq!(
                scratch.probes().installed_agent(&deadline()).value,
                Err(ProbeIssue::Missing)
            );
            scratch.write(&runtime, "bootstrap.json", b"invalid", 0o600);
            assert_eq!(
                scratch.probes().installed_agent(&deadline()).value,
                Err(ProbeIssue::Malformed)
            );
            assert!(scratch.fake.calls.lock().unwrap().is_empty());
            let cancellation = Cancellation::default();
            cancellation.cancel();
            assert_eq!(
                scratch
                    .probes()
                    .installed_agent(&Deadline::new(500, cancellation).unwrap())
                    .value,
                Err(ProbeIssue::Cancelled)
            );
        }
        #[test]
        fn native_bootstrap_foreign_and_timeout_preserve_original_bounded_cause() {
            let mut scratch = Scratch::new();
            let _runtime = scratch.ready();
            for (error, expected) in [
                (NativeError::Foreign, ProbeIssue::Foreign),
                (NativeError::Timeout, ProbeIssue::Timeout),
            ] {
                *scratch.fake.error.lock().unwrap() = Some(error);
                assert_eq!(
                    scratch.probes().installed_agent(&deadline()).value,
                    Err(expected)
                );
            }
        }
        #[test]
        fn direct_native_instance_helper_rejects_each_field_and_retains_pending_contract() {
            let scratch = Scratch::new();
            let bootstrap = scratch.bootstrap();
            let process = ProcessIdentity {
                pid: std::process::id(),
                uid: scratch.io.target().paths().uid,
                executable: scratch.io.target().agent_path(),
                started_unix_ms: bootstrap.started_unix_ms,
                generation: 1,
            };
            for field in ["id", "pid", "uid", "exe", "runtime_dir", "started_unix_ms"] {
                let mut json: serde_json::Value =
                    serde_json::from_slice(&scratch.status()).unwrap();
                let v = &mut json["result"]["installer"]["instance"][field];
                *v = if v.is_string() {
                    "/wrong".into()
                } else if field == "id" {
                    1u64.into()
                } else {
                    (v.as_u64().unwrap() + 1).into()
                };
                let reply = AgentReply {
                    id: 1,
                    source: ObservationSource::Demo,
                    observed_at_ms: 888,
                    result: Ok(DecodedReply::Status(
                        parse_status(&serde_json::to_vec(&json).unwrap(), AgentPlatform::Linux)
                            .unwrap(),
                    )),
                };
                assert_eq!(
                    associate_installed(&scratch.io, bootstrap.clone(), &process, reply).value,
                    Err(ProbeIssue::Foreign),
                    "{field}"
                );
            }
            let reply = AgentReply {
                id: 1,
                source: ObservationSource::Demo,
                observed_at_ms: 889,
                result: Ok(DecodedReply::Status(
                    StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete),
                )),
            };
            let facts = associate_installed(&scratch.io, bootstrap, &process, reply);
            assert_eq!(facts.observed_at_ms, 889);
            assert!(matches!(
                facts.value.unwrap().status,
                StatusAdmission::PendingHealthContract(_)
            ));
        }
        #[test]
        fn single_status_malformed_refusal_and_incomplete_contract_do_not_gain_readiness() {
            for (bytes, issue) in [
                (b"invalid".as_slice(), Some(ProbeIssue::Malformed)),
                (
                    br#"{"ok":false,"error":"not_supported"}"#.as_slice(),
                    Some(ProbeIssue::Unverified),
                ),
                (br#"{"ok":true,"result":{}}"#.as_slice(), None),
            ] {
                let mut scratch = Scratch::new();
                let result = owned_exchange(&mut scratch, Some(bytes.into()), &deadline());
                if let Some(issue) = issue {
                    assert_eq!(result.value, Err(issue));
                } else {
                    assert!(matches!(
                        result.value.unwrap().status,
                        StatusAdmission::PendingHealthContract(_)
                    ));
                }
            }
        }
        #[test]
        fn single_status_outer_deadline_shuts_down_owned_transport_without_retry() {
            let mut scratch = Scratch::new();
            let deadline = Deadline::new(150, Cancellation::default()).unwrap();
            let (server, requested) = start_server(&mut scratch, None);
            let begin = std::time::Instant::now();
            let result = scratch.probes().installed_agent(&deadline);
            requested.recv_timeout(Duration::from_secs(1)).unwrap();
            assert_eq!(result.value, Err(ProbeIssue::Timeout));
            assert_worker_released_with_server_alive(&scratch, &server);
            assert!(begin.elapsed() < Duration::from_secs(2));
        }
        #[test]
        fn cancellation_after_single_status_request_releases_worker_while_server_stays_alive() {
            let mut scratch = Scratch::new();
            let cancellation = Cancellation::default();
            let shared = Deadline::new(1000, cancellation.clone()).unwrap();
            let (server, requested) = start_server(&mut scratch, None);
            let probes = scratch.probes();
            let caller = thread::spawn(move || probes.installed_agent(&shared));
            requested.recv_timeout(Duration::from_secs(1)).unwrap();
            cancellation.cancel();
            assert_eq!(caller.join().unwrap().value, Err(ProbeIssue::Cancelled));
            assert_worker_released_with_server_alive(&scratch, &server);
        }
        #[test]
        fn actual_facade_status_identity_mismatches_are_unavailable_without_backends() {
            for field in ["id", "pid", "uid", "exe", "runtime_dir", "started_unix_ms"] {
                let mut scratch = Scratch::new();
                let mut json: serde_json::Value =
                    serde_json::from_slice(&scratch.status()).unwrap();
                let value = &mut json["result"]["installer"]["instance"][field];
                *value = if value.is_string() {
                    "/wrong".into()
                } else if field == "id" {
                    1u64.into()
                } else {
                    (value.as_u64().unwrap() + 1).into()
                };
                let installed = owned_exchange(
                    &mut scratch,
                    Some(serde_json::to_vec(&json).unwrap()),
                    &deadline(),
                );
                assert_eq!(installed.value, Err(ProbeIssue::Unavailable), "{field}");
                let (env, mut p) = pass(&scratch);
                p.installed_agent = installed.clone();
                let report = assemble_support(&scratch.io, env, p, runtime(), &deadline());
                assert_eq!(report.report.installed_agent, installed);
                assert_eq!(report.backends.value, Err(ProbeIssue::Unavailable));
            }
        }
        #[test]
        fn known_unsupported_survives_expiry_and_cancellation_without_admission() {
            let scratch = Scratch::new();
            for cancel in [false, true] {
                let cancellation = Cancellation::default();
                let shared = Deadline::new(1, cancellation.clone()).unwrap();
                if cancel {
                    cancellation.cancel();
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
                let (env, mut p) = pass(&scratch);
                p.os.family.value = Ok(OsFamily::Other("debian".into()));
                let result = assemble_support(&scratch.io, env, p, runtime(), &shared);
                assert_eq!(
                    result.report.eligibility,
                    Eligibility::NotSupported(UnsupportedReason::OperatingSystem)
                );
                assert!(result.proof.is_none());
                assert!(scratch.fake.calls.lock().unwrap().is_empty());
            }
        }
        #[test]
        fn every_transport_failure_maps_bounded_issue_without_retiming_reply() {
            let scratch = Scratch::new();
            let bootstrap = scratch.bootstrap();
            let process = ProcessIdentity {
                pid: bootstrap.pid,
                uid: scratch.io.target().paths().uid,
                executable: scratch.io.target().agent_path(),
                started_unix_ms: bootstrap.started_unix_ms,
                generation: 1,
            };
            for (failure, expected) in [
                (CallFailure::Unavailable, ProbeIssue::Unavailable),
                (CallFailure::QueueFull, ProbeIssue::Unavailable),
                (
                    CallFailure::InvalidCall(ContractError::InvalidDeadline),
                    ProbeIssue::Malformed,
                ),
                (CallFailure::InvalidResponse, ProbeIssue::Malformed),
                (CallFailure::TimeoutOutcomeUnknown, ProbeIssue::Timeout),
                (
                    CallFailure::Refused(AgentRefusal::NotSupported),
                    ProbeIssue::Unverified,
                ),
            ] {
                let facts = associate_installed(
                    &scratch.io,
                    bootstrap.clone(),
                    &process,
                    AgentReply {
                        id: 1,
                        source: ObservationSource::Demo,
                        observed_at_ms: 101,
                        result: Err(failure),
                    },
                );
                assert_eq!(facts, Fact::issue(expected, ObservationSource::Demo, 101));
            }
        }
        #[test]
        fn native_cancelled_pass_preserves_pending_fact_with_unverified_motion_and_no_commands() {
            let scratch = Scratch::new();
            let cancellation = Cancellation::default();
            cancellation.cancel();
            let result = scratch
                .probes()
                .detect(runtime(), &Deadline::new(1000, cancellation).unwrap());
            assert_eq!(
                result.report.eligibility,
                Eligibility::Pending(ProbeIssue::Unavailable)
            );
            // No session identity was observed; cancelled compatibility is not authority.
            assert_eq!(
                result.report.installed_agent.value,
                Err(ProbeIssue::Cancelled)
            );
            assert_eq!(
                result.report.reduced_motion.value,
                Err(ProbeIssue::Unverified)
            );
            assert_eq!(result.report.reduced_motion.observed_at_ms, 901);
            assert!(result.proof.is_none());
            assert!(scratch.fake.calls.lock().unwrap().is_empty());
        }
        #[test]
        fn inner_receipts_and_installed_gate_remain_literal_through_report_delivery() {
            let scratch = Scratch::new();
            let (env, mut p) = pass(&scratch);
            let original = agent_facts(STATUS, ObservationSource::Demo, 700).unwrap();
            p.installed_agent = Fact::known(original.clone(), ObservationSource::Demo, 700);
            p.logind
                .value
                .as_mut()
                .unwrap()
                .selected_session
                .observed_at_ms = 101;
            p.manager
                .value
                .as_mut()
                .unwrap()
                .compositor_managed
                .observed_at_ms = 202;
            p.hyprland.value.as_mut().unwrap().version.observed_at_ms = 303;
            p.registry.value.as_mut().unwrap().protocols.observed_at_ms = 404;
            p.manager_environment.observed_at_ms = 505;
            let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
            assert_eq!(result.report.eligibility, Eligibility::Supported);
            assert!(result.proof.is_some());
            assert_eq!(result.report.session.selected_session.observed_at_ms, 101);
            assert_eq!(result.report.session.compositor_managed.observed_at_ms, 202);
            assert_eq!(result.report.session.compositor_version.observed_at_ms, 303);
            assert_eq!(result.report.session.protocols.observed_at_ms, 404);
            assert_eq!(
                result.report.session.manager_environment.observed_at_ms,
                505
            );
            assert_eq!(result.report.installed_agent.value, Ok(original));
            assert_eq!(
                result.backends,
                Fact::known(
                    BackendReadiness::Recovery {
                        pending: 0,
                        startup: StartupRecovery::Failed
                    },
                    ObservationSource::Demo,
                    700
                )
            );
        }
        #[test]
        fn admission_refusal_and_expired_pass_keep_observations_and_issue_no_proof() {
            let scratch = Scratch::new();
            let (env, mut p) = pass(&scratch);
            p.logind
                .value
                .as_mut()
                .unwrap()
                .selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .id = "s".repeat(65);
            let result = assemble_support(&scratch.io, env, p, runtime(), &deadline());
            assert_eq!(
                result.report.eligibility,
                Eligibility::Pending(ProbeIssue::Unavailable)
            );
            assert!(result.proof.is_none());
            assert_eq!(result.report.session.selected_session.observed_at_ms, 17);
            let (env, p) = pass(&scratch);
            let expired = Deadline::new(1, Cancellation::default()).unwrap();
            thread::sleep(Duration::from_millis(5));
            let result = assemble_support(&scratch.io, env, p, runtime(), &expired);
            assert_eq!(
                result.report.eligibility,
                Eligibility::Pending(ProbeIssue::Timeout)
            );
            assert!(result.proof.is_none());
            assert_eq!(result.report.session.os.observed_at_ms, 17);
        }
        #[test]
        fn native_single_status_owned_socket_retains_exact_health_and_complete_receipt_time() {
            let mut scratch = Scratch::new();
            let runtime = scratch.ready();
            let listener = UnixListener::bind(scratch.io.target().socket_path()).unwrap();
            let stat = fs::statat(&runtime, "agent.sock", AtFlags::SYMLINK_NOFOLLOW).unwrap();
            assert_eq!(stat.st_uid, scratch.io.target().paths().uid);
            assert_eq!(stat.st_mode & 0o170000, 0o140000);
            fs::chmodat(
                &runtime,
                "agent.sock",
                Mode::from_raw_mode(0o600),
                AtFlags::empty(),
            )
            .unwrap();
            scratch
                .files
                .push((runtime.try_clone().unwrap(), "agent.sock".into()));
            listener.set_nonblocking(true).unwrap();
            let response = scratch.status();
            let server = thread::spawn(move || {
                let end = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && std::time::Instant::now() < end =>
                        {
                            thread::sleep(Duration::from_millis(2))
                        }
                        other => panic!("owned accept {other:?}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = vec![];
                loop {
                    let mut one = [0];
                    if stream.read(&mut one).unwrap() == 0 || one[0] == b'\n' {
                        break;
                    };
                    bytes.push(one[0]);
                }
                assert_eq!(bytes, b"{\"cmd\":\"status\"}");
                stream.write_all(&response).unwrap();
                stream.write_all(b"\n").unwrap();
                let mut one = [0];
                assert_eq!(stream.read(&mut one).unwrap(), 0);
            });
            let result = scratch.probes().installed_agent(&deadline());
            server.join().unwrap();
            assert_eq!(result.observed_at_ms, 901);
            assert_eq!(result.source, ObservationSource::Demo);
            let facts = result.value.unwrap();
            assert_eq!(facts.call_id, 1);
            assert_eq!(facts.received_at_ms, 901);
            let StatusAdmission::Supported(health) = facts.status else {
                panic!()
            };
            assert_eq!(health.installer().startup_recovery, StartupRecovery::Failed);
            assert_eq!(health.installer().recovery_pending, 0);
            assert!(!health.installer().gate.open);
        }
    }
    mod native_desktop {
        use crosspane_installer::platform::linux::{
            detect::*,
            native_io::{
                Cancellation, ChildEnvironment, CommandOutput, CommandRunner, CommandSpec,
                Deadline, LinuxNativeIo, NativeError, ProcessFacts, ProcessProbe,
            },
            transport::CallerClock,
        };
        use std::{
            collections::BTreeMap,
            io::{Read, Write},
            net::Shutdown,
            num::NonZeroU32,
            os::unix::net::{UnixListener, UnixStream},
            path::PathBuf,
            sync::{
                Arc, Mutex,
                atomic::{AtomicU64, Ordering},
            },
            thread::{self, JoinHandle},
            time::Duration,
        };
        use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};
        fn path(value: &str) -> OwnedObjectPath {
            value.try_into().unwrap()
        }
        fn value<T: Into<Value<'static>> + DynamicType>(value: T) -> OwnedValue {
            OwnedValue::try_from(Value::new(value)).unwrap()
        }
        const WM: &str = "wayland-wm@Hyprland.service";
        const SESSION: &str = "wayland-session@Hyprland.target";
        const GRAPHICAL: &str = "graphical-session.target";
        fn row(
            id: &str,
            object: &str,
        ) -> (
            String,
            String,
            String,
            String,
            String,
            String,
            OwnedObjectPath,
            u32,
            String,
            OwnedObjectPath,
        ) {
            (
                id.into(),
                "test description".into(),
                "loaded".into(),
                "active".into(),
                "running".into(),
                "".into(),
                path(object),
                0,
                "".into(),
                path("/"),
            )
        }
        fn rows() -> UnitRows {
            vec![
                row(GRAPHICAL, "/units/graphical"),
                row(WM, "/units/wm"),
                row(SESSION, "/units/session"),
            ]
        }
        fn unit(id: &str, binds: &[&str], requires: &[&str]) -> Properties {
            [
                ("Id", value(id.to_string())),
                ("LoadState", value("loaded".to_string())),
                ("ActiveState", value("active".to_string())),
                (
                    "BindsTo",
                    value(binds.iter().map(|v| v.to_string()).collect::<Vec<_>>()),
                ),
                (
                    "Requires",
                    value(requires.iter().map(|v| v.to_string()).collect::<Vec<_>>()),
                ),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect()
        }
        type Exec = (String, Vec<String>, bool, u64, u64, u64, u64, u32, i32, i32);
        fn exec() -> Exec {
            (
                "/usr/bin/uwsm".into(),
                vec![
                    "/usr/bin/uwsm".into(),
                    "aux".into(),
                    "exec".into(),
                    "--".into(),
                    "Hyprland".into(),
                ],
                false,
                123,
                124,
                0,
                0,
                4242,
                0,
                0,
            )
        }
        fn service() -> Properties {
            [
                ("Type".into(), value("notify".to_string())),
                ("MainPID".into(), value(4242u32)),
                ("ExecStart".into(), value(vec![exec()])),
            ]
            .into_iter()
            .collect()
        }
        enum Reply {
            Rows(UnitRows),
            Properties(Properties),
            Text(String),
            Error,
        }
        struct Step {
            method: &'static str,
            path: &'static str,
            interface: &'static str,
            body: &'static str,
            reply: Reply,
        }
        fn script() -> Vec<Step> {
            vec![
                Step {
                    method: "ListUnitsByPatterns",
                    path: "/org/freedesktop/systemd1",
                    interface: "org.freedesktop.systemd1.Manager",
                    body: "wayland-wm@*.service",
                    reply: Reply::Rows(rows()),
                },
                Step {
                    method: "GetAll",
                    path: "/units/graphical",
                    interface: "org.freedesktop.DBus.Properties",
                    body: "org.freedesktop.systemd1.Unit",
                    reply: Reply::Properties(unit(GRAPHICAL, &[], &[])),
                },
                Step {
                    method: "GetAll",
                    path: "/units/wm",
                    interface: "org.freedesktop.DBus.Properties",
                    body: "org.freedesktop.systemd1.Unit",
                    reply: Reply::Properties(unit(WM, &[SESSION], &[])),
                },
                Step {
                    method: "GetAll",
                    path: "/units/session",
                    interface: "org.freedesktop.DBus.Properties",
                    body: "org.freedesktop.systemd1.Unit",
                    reply: Reply::Properties(unit(SESSION, &[GRAPHICAL], &[WM])),
                },
                Step {
                    method: "GetAll",
                    path: "/units/wm",
                    interface: "org.freedesktop.DBus.Properties",
                    body: "org.freedesktop.systemd1.Service",
                    reply: Reply::Properties(service()),
                },
            ]
        }
        fn properties(step: &mut Step) -> &mut Properties {
            match &mut step.reply {
                Reply::Properties(values) => values,
                _ => panic!("fixture is not properties"),
            }
        }
        fn encoded(text: &str) -> Vec<u8> {
            let mut result = (text.len() as u32).to_le_bytes().to_vec();
            result.extend_from_slice(text.as_bytes());
            result.push(0);
            result
        }
        fn contains(bytes: &[u8], text: &str) -> bool {
            bytes.windows(text.len() + 5).any(|v| v == encoded(text))
        }
        fn line(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
            let mut result = Vec::new();
            while !result.ends_with(b"\r\n") {
                let mut one = [0];
                stream.read_exact(&mut one)?;
                result.push(one[0]);
                assert!(result.len() <= 4096);
            }
            Ok(result)
        }
        fn frame(stream: &mut UnixStream) -> std::io::Result<(NonZeroU32, Vec<u8>, Vec<u8>)> {
            let mut header = [0; 16];
            stream.read_exact(&mut header)?;
            assert_eq!(header[0], b'l');
            let body = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let fields = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let offset = (16 + fields + 7) & !7;
            assert!(offset + body <= MAX_PROBE_BYTES);
            let mut bytes = header.to_vec();
            bytes.resize(offset + body, 0);
            stream.read_exact(&mut bytes[16..])?;
            Ok((
                NonZeroU32::new(u32::from_le_bytes(header[8..12].try_into().unwrap())).unwrap(),
                bytes.clone(),
                bytes[offset..].to_vec(),
            ))
        }
        fn reply<T: serde::Serialize + DynamicType>(
            serial: NonZeroU32,
            value: &T,
        ) -> zbus::Message {
            let dummy = zbus::Message::method_call("/fake", "Read")
                .unwrap()
                .serial(serial)
                .build(&())
                .unwrap();
            zbus::Message::method_return(&dummy.header())
                .unwrap()
                .sender(":1.1")
                .unwrap()
                .build(value)
                .unwrap()
        }
        struct BusServer {
            stop: UnixStream,
            join: Option<JoinHandle<std::io::Result<()>>>,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl Drop for BusServer {
            fn drop(&mut self) {
                let _ = self.stop.shutdown(Shutdown::Both);
                if let Some(join) = self.join.take() {
                    let _ = join.join();
                }
            }
        }
        impl BusServer {
            fn new(script: Vec<Step>) -> (UnixStream, Self) {
                let (client, mut peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let stop = peer.try_clone().unwrap();
                let log = Arc::new(Mutex::new(Vec::new()));
                let record = log.clone();
                let join = thread::spawn(move || {
                    loop {
                        let message = line(&mut peer)?;
                        if message.ends_with(b"BEGIN\r\n") {
                            break;
                        }
                        if message.starts_with(b"\0AUTH") || message.starts_with(b"AUTH") {
                            peer.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")?;
                        } else {
                            assert!(message.starts_with(b"NEGOTIATE_UNIX_FD"));
                            peer.write_all(b"ERROR no fd passing\r\n")?;
                        }
                    }
                    let (serial, bytes, _) = frame(&mut peer)?;
                    assert!(contains(&bytes, "Hello"));
                    peer.write_all(reply(serial, &":1.42").data().bytes())?;
                    for step in script {
                        let (serial, bytes, body) = frame(&mut peer)?;
                        assert!(contains(&bytes, step.method));
                        assert!(contains(&bytes, step.path));
                        assert!(contains(&bytes, step.interface));
                        assert!(
                            contains(&bytes, "org.freedesktop.systemd1")
                                || contains(&bytes, "org.gnome.Shell")
                        );
                        assert!(contains(&body, step.body));
                        record
                            .lock()
                            .unwrap()
                            .push(format!("{}:{}", step.method, step.path));
                        let message = match step.reply {
                            Reply::Rows(rows) => reply(serial, &rows),
                            Reply::Properties(values) => reply(serial, &values),
                            Reply::Text(text) => reply(serial, &text),
                            Reply::Error => {
                                let dummy = zbus::Message::method_call("/fake", "Read")
                                    .unwrap()
                                    .serial(serial)
                                    .build(&())
                                    .unwrap();
                                zbus::Message::error(
                                    &dummy.header(),
                                    "org.freedesktop.DBus.Error.AccessDenied",
                                )
                                .unwrap()
                                .sender(":1.1")
                                .unwrap()
                                .build(&"fake denied")
                                .unwrap()
                            }
                        };
                        peer.write_all(message.data().bytes())?;
                    }
                    let mut one = [0];
                    assert_eq!(peer.read(&mut one)?, 0, "unexpected manager request");
                    Ok(())
                });
                (
                    client,
                    Self {
                        stop,
                        join: Some(join),
                        log,
                    },
                )
            }
            fn finish(mut self) -> Vec<String> {
                self.join.take().unwrap().join().unwrap().unwrap();
                self.log.lock().unwrap().clone()
            }
        }
        fn clock() -> CallerClock {
            let value = AtomicU64::new(10);
            Arc::new(move || value.fetch_add(10, Ordering::SeqCst))
        }
        fn manager(script: Vec<Step>) -> (Fact<ManagerFacts>, Vec<String>) {
            let (stream, server) = BusServer::new(script);
            let result = manager_from_stream(
                stream,
                &Deadline::new(1500, Cancellation::default()).unwrap(),
                clock(),
            );
            (result, server.finish())
        }
        #[test]
        fn systemd_unit_rows_bound_every_field_and_reject_duplicate_identity() {
            assert_eq!(decode_units(rows()).unwrap(), rows());
            for mutate in [0, 1, 2, 3] {
                let mut rows = rows();
                match mutate {
                    0 => rows[1].0 = rows[0].0.clone(),
                    1 => rows[1].6 = rows[0].6.clone(),
                    2 => rows[1].0.clear(),
                    _ => rows[0].6 = path("/"),
                }
                assert_eq!(decode_units(rows), Err(ProbeIssue::Malformed));
            }
            let exact: UnitRows = (0..MAX_UNIT_ROWS)
                .map(|i| row(&format!("u{i}"), &format!("/unit/u{i}")))
                .collect();
            assert_eq!(decode_units(exact.clone()).unwrap().len(), MAX_UNIT_ROWS);
            let mut oversized = exact;
            oversized.push(row("extra", "/unit/extra"));
            assert_eq!(decode_units(oversized), Err(ProbeIssue::Oversize));
            for index in 0..7 {
                let mut rows = rows();
                let r = &mut rows[0];
                let target = match index {
                    0 => &mut r.0,
                    1 => &mut r.1,
                    2 => &mut r.2,
                    3 => &mut r.3,
                    4 => &mut r.4,
                    5 => &mut r.5,
                    _ => &mut r.8,
                };
                *target = "x".repeat(513);
                if matches!(index, 0 | 2 | 3) {
                    assert_eq!(decode_units(rows), Err(ProbeIssue::Oversize));
                } else {
                    assert!(decode_units(rows).is_ok(), "unused field {index}");
                }
            }
        }
        #[test]
        fn actual_manager_requires_typed_live_exec_and_graph_with_original_receipts() {
            let (fact, log) = manager(script());
            let decoded = fact.value.unwrap();
            assert_eq!(
                decoded.compositor_managed,
                Fact::known(true, ObservationSource::Demo, 50)
            );
            assert_eq!(
                decoded.graphical_target_active,
                Fact::known(true, ObservationSource::Demo, 20)
            );
            assert_eq!(decoded.compositor_pid, Some(4242));
            assert_eq!(fact.observed_at_ms, 60);
            assert_eq!(fact.source, ObservationSource::Demo);
            assert_eq!(
                log,
                [
                    "ListUnitsByPatterns:/org/freedesktop/systemd1",
                    "GetAll:/units/graphical",
                    "GetAll:/units/wm",
                    "GetAll:/units/session",
                    "GetAll:/units/wm"
                ]
            );
            assert_eq!(decoded.clone(), decoded);
        }
        #[test]
        fn real_systemd_property_counts_are_admitted_and_still_bounded() {
            // systemd 261 on the owner's desktop: Unit GetAll 102 properties, Service 369.
            let mut steps = script();
            for (index, count) in [(1, 102), (2, 102), (3, 102), (4, 369)] {
                let values = properties(&mut steps[index]);
                for i in values.len()..count {
                    values.insert(format!("Property{i}"), value(0u64));
                }
                assert_eq!(values.len(), count);
            }
            let decoded = manager(steps).0.value.unwrap();
            assert_eq!(decoded.compositor_managed.value, Ok(true));
            assert_eq!(decoded.graphical_target_active.value, Ok(true));
            let mut steps = script();
            let values = properties(&mut steps[1]);
            for i in values.len()..=MAX_PROPERTIES {
                values.insert(format!("Property{i}"), value(0u64));
            }
            steps.truncate(2);
            assert_eq!(manager(steps).0.value, Err(ProbeIssue::Oversize));
        }
        /// systemd 261 on the owner's desktop: the running ExecStart row is all zero and the
        /// main process lives in ExecMain* (ExecMainPID = MainPID, started, not exited).
        fn systemd_261_service() -> Properties {
            let mut values = service();
            let mut row = exec();
            (row.3, row.4, row.5, row.6, row.7) = (0, 0, 0, 0, 0);
            values.insert("ExecStart".into(), value(vec![row]));
            values.insert("ExecMainPID".into(), value(4242u32));
            values.insert(
                "ExecMainStartTimestamp".into(),
                value(1_791_096_132_706_343u64),
            );
            values.insert("ExecMainExitTimestamp".into(), value(0u64));
            values.insert("ExecMainCode".into(), value(0i32));
            values.insert("ExecMainStatus".into(), value(0i32));
            values
        }
        #[test]
        fn systemd_261_exec_main_properties_prove_the_running_uwsm_process() {
            let mut steps = script();
            steps[4].reply = Reply::Properties(systemd_261_service());
            let decoded = manager(steps).0.value.unwrap();
            assert_eq!(decoded.compositor_managed.value, Ok(true));
            assert_eq!(decoded.compositor_pid, Some(4242));
            // The populated-row path still works, with or without the ExecMain* properties.
            let mut steps = script();
            let mut both = systemd_261_service();
            both.insert("ExecStart".into(), value(vec![exec()]));
            steps[4].reply = Reply::Properties(both);
            assert_eq!(manager(steps).0.value.unwrap().compositor_pid, Some(4242));
            // Any contradiction in the ExecMain* record, or a half-filled row, is not proof.
            let contradictions: [(&str, OwnedValue); 6] = [
                ("ExecMainPID", value(4243u32)),
                ("ExecMainPID", value(0u32)),
                ("ExecMainStartTimestamp", value(0u64)),
                ("ExecMainExitTimestamp", value(1u64)),
                ("ExecMainCode", value(1i32)),
                ("ExecMainStatus", value(1i32)),
            ];
            for (key, bad) in contradictions {
                let mut steps = script();
                let mut values = systemd_261_service();
                values.insert(key.into(), bad);
                steps[4].reply = Reply::Properties(values);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Unverified), "{key}");
            }
            for key in [
                "ExecMainPID",
                "ExecMainStartTimestamp",
                "ExecMainExitTimestamp",
                "ExecMainCode",
                "ExecMainStatus",
            ] {
                let mut steps = script();
                let mut values = systemd_261_service();
                values.remove(key);
                steps[4].reply = Reply::Properties(values);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Unverified), "{key}");
                let mut steps = script();
                let mut values = systemd_261_service();
                values.insert(key.into(), value("wrong".to_string()));
                steps[4].reply = Reply::Properties(values);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Malformed), "{key}");
            }
            for field in 3..=6 {
                let mut steps = script();
                let mut values = systemd_261_service();
                let mut row = exec();
                (row.3, row.4, row.5, row.6, row.7) = (0, 0, 0, 0, 0);
                match field {
                    3 => row.3 = 1,
                    4 => row.4 = 1,
                    5 => row.5 = 1,
                    _ => row.6 = 1,
                }
                values.insert("ExecStart".into(), value(vec![row]));
                steps[4].reply = Reply::Properties(values);
                assert_eq!(
                    manager(steps).0.value,
                    Err(ProbeIssue::Unverified),
                    "{field}"
                );
            }
        }
        #[test]
        fn active_target_and_effective_environment_never_manufacture_uwsm_lifecycle() {
            let mut steps = script();
            steps.truncate(2);
            steps[0].reply = Reply::Rows(vec![rows()[0].clone()]);
            let (fact, log) = manager(steps);
            let decoded = fact.value.unwrap();
            assert_eq!(
                decoded.compositor_managed,
                Fact::known(false, ObservationSource::Demo, 10)
            );
            assert_eq!(
                decoded.graphical_target_active,
                Fact::known(true, ObservationSource::Demo, 20)
            );
            assert_eq!(decoded.compositor_pid, None);
            assert_eq!(log.len(), 2);
            let (fact, log) = manager(vec![Step {
                reply: Reply::Rows(vec![]),
                ..script().remove(0)
            }]);
            let decoded = fact.value.unwrap();
            assert_eq!(decoded.compositor_managed.value, Ok(false));
            assert_eq!(decoded.graphical_target_active.value, Ok(false));
            assert_eq!(log.len(), 1);
        }
        #[test]
        fn unknown_unit_load_or_active_state_is_pending_not_known_absence() {
            for active in [false, true] {
                let mut entries = rows();
                if active {
                    entries[1].3 = "future-state".into();
                } else {
                    entries[1].2 = "future-state".into();
                }
                assert_eq!(decode_units(entries.clone()), Err(ProbeIssue::Malformed));
                let mut steps = script();
                steps.truncate(1);
                steps[0].reply = Reply::Rows(entries);
                let (fact, log) = manager(steps);
                assert_eq!(fact.value, Err(ProbeIssue::Malformed));
                assert_eq!(fact.observed_at_ms, 20);
                assert_eq!(log.len(), 1);
            }
        }
        #[test]
        fn known_systemd_state_and_service_type_vocabularies_are_literal() {
            for load in [
                "stub",
                "loaded",
                "not-found",
                "bad-setting",
                "error",
                "merged",
                "masked",
            ] {
                let mut entries = rows();
                entries[1].2 = load.into();
                assert_eq!(decode_units(entries.clone()), Ok(entries));
            }
            for active in [
                "active",
                "reloading",
                "inactive",
                "failed",
                "activating",
                "deactivating",
                "maintenance",
                "refreshing",
            ] {
                let mut entries = rows();
                entries[1].3 = active.into();
                assert_eq!(decode_units(entries.clone()), Ok(entries));
            }
            for kind in [
                "simple",
                "exec",
                "forking",
                "oneshot",
                "dbus",
                "notify",
                "notify-reload",
                "idle",
            ] {
                let mut steps = script();
                properties(&mut steps[4]).insert("Type".into(), value(kind.to_string()));
                let decoded = manager(steps).0.value.unwrap();
                assert_eq!(decoded.compositor_managed.value, Ok(kind == "notify"));
            }
            for active in [
                "reloading",
                "activating",
                "deactivating",
                "maintenance",
                "refreshing",
            ] {
                let mut entries = rows();
                entries[1].3 = active.into();
                let mut steps = script();
                steps.truncate(1);
                steps[0].reply = Reply::Rows(entries);
                let (fact, log) = manager(steps);
                assert_eq!(fact.value, Err(ProbeIssue::Unverified));
                assert_eq!(log.len(), 1);
            }
        }
        #[test]
        fn unknown_service_type_is_pending_not_known_non_uwsm() {
            let mut steps = script();
            properties(&mut steps[4])
                .insert("Type".into(), value("future-service-type".to_string()));
            assert_eq!(manager(steps).0.value, Err(ProbeIssue::Malformed));
        }
        #[test]
        fn session_target_transitions_remain_pending_with_valid_uwsm_service() {
            for active in [
                "activating",
                "deactivating",
                "reloading",
                "maintenance",
                "refreshing",
            ] {
                let mut steps = script();
                let mut entries = rows();
                entries[2].3 = active.into();
                steps[0].reply = Reply::Rows(entries);
                properties(&mut steps[3]).insert("ActiveState".into(), value(active.to_string()));
                steps.truncate(4);
                let (fact, log) = manager(steps);
                assert_eq!(fact.value, Err(ProbeIssue::Unverified));
                assert_eq!(log.len(), 4);
            }
        }
        #[test]
        fn each_uwsm_exec_or_graph_contradiction_independently_prevents_management() {
            for variant in 0..17 {
                let mut steps = script();
                let mut executable = exec();
                match variant {
                    0 => {
                        properties(&mut steps[2])
                            .insert("BindsTo".into(), value(Vec::<String>::new()));
                    }
                    1 => {
                        properties(&mut steps[3])
                            .insert("BindsTo".into(), value(Vec::<String>::new()));
                    }
                    2 => {
                        properties(&mut steps[3])
                            .insert("Requires".into(), value(Vec::<String>::new()));
                    }
                    3 => {
                        properties(&mut steps[4])
                            .insert("Type".into(), value("simple".to_string()));
                    }
                    4 => {
                        properties(&mut steps[4]).insert("MainPID".into(), value(0u32));
                    }
                    5 => executable.0 = "/usr/bin/other".into(),
                    6 => executable.1[0] = "/usr/bin/other".into(),
                    7 => executable.1[1] = "not-aux".into(),
                    8 => executable.1[2] = "not-exec".into(),
                    9 => executable.1[3] = "not-delimiter".into(),
                    10 => executable.1[4] = "other".into(),
                    11 => executable.2 = true,
                    12 => executable.3 = 0,
                    13 => executable.5 = 1234,
                    14 => executable.6 = 1234,
                    15 => executable.7 = 99,
                    _ => executable.4 = 0,
                }
                if variant >= 5 {
                    properties(&mut steps[4]).insert("ExecStart".into(), value(vec![executable]));
                }
                let result = manager(steps).0.value;
                if matches!(variant, 4 | 12..=16) {
                    assert_eq!(result, Err(ProbeIssue::Unverified), "variant {variant}");
                    continue;
                }
                let decoded = result.unwrap();
                assert_eq!(
                    decoded.compositor_managed.value,
                    Ok(false),
                    "variant {variant}"
                );
                assert_eq!(decoded.compositor_pid, None);
            }
            for exec in [Vec::<Exec>::new(), vec![exec(), exec()]] {
                let mut steps = script();
                let empty = exec.is_empty();
                properties(&mut steps[4]).insert("ExecStart".into(), value(exec));
                assert_eq!(
                    manager(steps).0.value,
                    Err(if empty {
                        ProbeIssue::Unverified
                    } else {
                        ProbeIssue::Ambiguous
                    })
                );
            }
        }
        #[test]
        fn manager_each_required_property_absence_and_wrong_signature_is_pending() {
            for stage in [1usize, 2, 3, 4] {
                let keys: &[&str] = if stage == 4 {
                    &["Type", "MainPID", "ExecStart"]
                } else {
                    &["Id", "LoadState", "ActiveState", "BindsTo", "Requires"]
                };
                for key in keys {
                    for missing in [true, false] {
                        let mut steps = script();
                        if missing {
                            properties(&mut steps[stage]).remove(*key);
                        } else {
                            properties(&mut steps[stage]).insert((*key).into(), value(7i64));
                        }
                        steps.truncate(stage + 1);
                        let (fact, log) = manager(steps);
                        assert_eq!(
                            fact.value,
                            Err(if missing {
                                ProbeIssue::Unverified
                            } else {
                                ProbeIssue::Malformed
                            }),
                            "stage {stage} field {key}"
                        );
                        assert_eq!(log.len(), stage + 1);
                    }
                }
            }
        }
        #[test]
        fn systemd_wrong_empty_array_and_short_extra_exec_tuples_fail_without_panic() {
            for malformed in [
                value(Vec::<String>::new()),
                value(vec![("/usr/bin/uwsm".to_string(),)]),
                value(vec![(exec(), true)]),
            ] {
                let mut steps = script();
                properties(&mut steps[4]).insert("ExecStart".into(), malformed);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Malformed));
            }
            let mut steps = script();
            steps[0].reply = Reply::Text("wrong rows".into());
            steps.truncate(1);
            assert_eq!(manager(steps).0.value, Err(ProbeIssue::Malformed));
        }
        #[test]
        fn manager_ambiguity_unreadable_drift_and_bounds_stop_at_receipt() {
            let mut steps = script();
            let mut entries = rows();
            entries.push(row("wayland-wm@other.service", "/units/other"));
            steps[0].reply = Reply::Rows(entries);
            steps.truncate(2);
            assert_eq!(manager(steps).0.value, Err(ProbeIssue::Ambiguous));
            let mut steps = script();
            steps[2].reply = Reply::Error;
            steps.truncate(3);
            assert_eq!(manager(steps).0.value, Err(ProbeIssue::Unavailable));
            for key in ["Id", "LoadState", "ActiveState"] {
                let mut steps = script();
                properties(&mut steps[2]).insert(key.into(), value("changed".to_string()));
                steps.truncate(3);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Foreign));
            }
            for key in ["BindsTo", "Requires"] {
                let mut steps = script();
                properties(&mut steps[2])
                    .insert(key.into(), value(vec!["x".to_string(); MAX_UNIT_ROWS + 1]));
                steps.truncate(3);
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Oversize));
            }
            let mut steps = script();
            properties(&mut steps[1]).insert("Ignored".into(), value("x".repeat(64 * 1024)));
            assert_eq!(
                manager(steps).0.value.unwrap().compositor_managed.value,
                Ok(true)
            );
        }
        #[test]
        fn uwsm_instance_decoding_preserves_escaped_identity_and_all_configured_arguments() {
            let wm = "wayland-wm@Hyprland\\x2dtest.service";
            let session = "wayland-session@Hyprland\\x2dtest.target";
            let mut steps = script();
            steps[0].reply = Reply::Rows(vec![
                rows()[0].clone(),
                row(wm, "/units/wm"),
                row(session, "/units/session"),
            ]);
            steps[2].reply = Reply::Properties(unit(wm, &[session], &[]));
            steps[3].reply = Reply::Properties(unit(session, &[GRAPHICAL], &[wm]));
            let mut command = exec();
            command.1[4] = "Hyprland-test".into();
            command
                .1
                .extend(["".into(), "--config".into(), "/test/config".into()]);
            properties(&mut steps[4]).insert("ExecStart".into(), value(vec![command]));
            assert_eq!(
                manager(steps).0.value.unwrap().compositor_managed.value,
                Ok(true)
            );
            for instance in ["", r"bad\x", r"bad\xZZ", r"bad\x00", r"bad\xff"] {
                let wm = format!("wayland-wm@{instance}.service");
                let session = format!("wayland-session@{instance}.target");
                let mut steps = script();
                steps[0].reply = Reply::Rows(vec![
                    rows()[0].clone(),
                    row(&wm, "/units/wm"),
                    row(&session, "/units/session"),
                ]);
                steps[2].reply = Reply::Properties(unit(&wm, &[&session], &[]));
                steps[3].reply = Reply::Properties(unit(&session, &[GRAPHICAL], &[&wm]));
                assert_eq!(manager(steps).0.value, Err(ProbeIssue::Malformed));
            }
        }
        fn globals() -> Vec<(String, u32)> {
            REQUIRED_PROTOCOLS
                .iter()
                .map(|(name, version)| (name.to_string(), *version))
                .collect()
        }
        #[test]
        fn every_current_backend_protocol_and_minimum_is_required_without_binding() {
            assert_eq!(protocols_satisfy(&globals()), Ok(true));
            for index in 0..REQUIRED_PROTOCOLS.len() {
                let mut entries = globals();
                entries.remove(index);
                assert_eq!(protocols_satisfy(&entries), Ok(false));
                let mut entries = globals();
                entries[index].1 -= 1;
                assert_eq!(
                    protocols_satisfy(&entries),
                    if entries[index].1 == 0 {
                        Err(ProbeIssue::Malformed)
                    } else {
                        Ok(false)
                    }
                );
            }
            assert_eq!(
                protocols_satisfy(&vec![("valid".into(), 1); MAX_REGISTRY_GLOBALS]),
                Ok(false)
            );
            assert_eq!(
                protocols_satisfy(&vec![("valid".into(), 1); MAX_REGISTRY_GLOBALS + 1]),
                Err(ProbeIssue::Oversize)
            );
            assert_eq!(
                protocols_satisfy(&[("wl_seat".into(), 0)]),
                Err(ProbeIssue::Malformed)
            );
            for entries in [vec![("".into(), 1)], vec![("bad\n".into(), 1)]] {
                assert_eq!(protocols_satisfy(&entries), Ok(false));
            }
            assert_eq!(protocols_satisfy(&[("x".repeat(129), 1)]), Ok(false));
        }
        #[test]
        fn capture_managers_are_independently_required_at_backend_minimum_versions() {
            let complete = globals();
            for name in [
                "ext_image_copy_capture_manager_v1",
                "ext_output_image_capture_source_manager_v1",
            ] {
                assert_eq!(
                    REQUIRED_PROTOCOLS
                        .iter()
                        .find(|(interface, _)| *interface == name)
                        .unwrap()
                        .1,
                    1
                );
                let mut missing = complete.clone();
                missing.retain(|(interface, _)| interface != name);
                assert_eq!(protocols_satisfy(&missing), Ok(false), "missing {name}");
                let mut wrong = complete.clone();
                wrong
                    .iter_mut()
                    .find(|(interface, _)| interface == name)
                    .unwrap()
                    .1 = 0;
                assert_eq!(
                    protocols_satisfy(&wrong),
                    Err(ProbeIssue::Malformed),
                    "version {name}"
                );
            }
        }
        #[test]
        fn hyprland_version_boundary_unknown_and_protocol_envelope_are_literal() {
            for (version, expected) in [
                ("0.55.9", [0, 55, 9]),
                ("0.56.0", [0, 56, 0]),
                ("0.56.1", [0, 56, 1]),
                ("1.0.0", [1, 0, 0]),
                ("65535.65535.65535", [65535; 3]),
            ] {
                assert_eq!(
                    parse_hyprland_version(
                        format!(
                            "Hyprland {version} built from branch main at commit abc.\nDate: test\n"
                        )
                        .as_bytes()
                    ),
                    Ok(expected)
                );
            }
            for value in [
                "",
                "0.56.0",
                "Other 0.56.0 built from branch main",
                "Hyprland 0.56 built from branch main",
                "Hyprland 0.56.0.1 built from branch main",
                "Hyprland 0.56.0-dev built from branch main",
                "Hyprland -1.56.0 built from branch main",
                "Hyprland 65536.56.0 built from branch main",
            ] {
                assert_eq!(
                    parse_hyprland_version(value.as_bytes()),
                    Err(ProbeIssue::Malformed)
                );
            }
            for header in [
                "Hyprland 0.56.0",
                "Hyprland v0.56.0",
                "Hyprland version 0.56.0",
                "Hyprland 0.56.0+abc123",
            ] {
                let mut bytes = header.as_bytes().to_vec();
                bytes.extend_from_slice(b"\nunused metadata: \xff");
                assert_eq!(parse_hyprland_version(&bytes), Ok([0, 56, 0]));
            }
            assert_eq!(parse_hyprland_version(&[0xff]), Err(ProbeIssue::Malformed));
            assert_eq!(
                parse_hyprland_version(&vec![b'x'; MAX_PROBE_BYTES + 1]),
                Err(ProbeIssue::Oversize)
            );
        }
        struct OwnedPeer {
            stop: UnixStream,
            join: Option<JoinHandle<()>>,
        }
        impl Drop for OwnedPeer {
            fn drop(&mut self) {
                let _ = self.stop.shutdown(Shutdown::Both);
                if let Some(join) = self.join.take() {
                    let _ = join.join();
                }
            }
        }
        impl OwnedPeer {
            fn new(work: impl FnOnce(UnixStream) + Send + 'static) -> (UnixStream, Self) {
                let (client, peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                peer.set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let stop = peer.try_clone().unwrap();
                let join = Some(thread::spawn(move || work(peer)));
                (client, Self { stop, join })
            }
            fn finish(mut self) {
                self.join.take().unwrap().join().unwrap();
            }
        }
        #[test]
        fn actual_hyprland_partial_reply_pins_exact_read_request_peer_and_receipt() {
            let (stream, server) = OwnedPeer::new(|mut peer| {
                let mut request = [0; 8];
                peer.read_exact(&mut request).unwrap();
                assert_eq!(&request, b"/version");
                for part in [
                    b"Hyprland 0.".as_slice(),
                    b"56.0 built from ",
                    b"branch main at commit test.\n",
                ] {
                    peer.write_all(part).unwrap();
                }
                peer.shutdown(Shutdown::Write).unwrap();
                let mut one = [0];
                assert_eq!(peer.read(&mut one).unwrap(), 0);
            });
            let fact = hyprland_from_stream(
                stream,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                clock(),
            );
            server.finish();
            let facts = fact.value.unwrap();
            assert_eq!(facts.pid, std::process::id());
            assert_eq!(
                facts.version,
                Fact::known([0, 56, 0], ObservationSource::Demo, 10)
            );
            assert_eq!(fact.observed_at_ms, 20);
            assert_eq!(fact.source, ObservationSource::Demo);
            assert_eq!(facts.clone(), facts);
        }
        #[test]
        fn hyprland_oversized_malformed_and_stalled_reads_never_report_version_success() {
            for (bytes, expected) in [
                (vec![b'x'; MAX_PROBE_BYTES + 1], ProbeIssue::Oversize),
                (b"not Hyprland".to_vec(), ProbeIssue::Malformed),
            ] {
                let (stream, server) = OwnedPeer::new(move |mut peer| {
                    let mut request = [0; 8];
                    peer.read_exact(&mut request).unwrap();
                    assert_eq!(&request, b"/version");
                    peer.write_all(&bytes).unwrap();
                    peer.shutdown(Shutdown::Write).unwrap();
                });
                let result = hyprland_from_stream(
                    stream,
                    &Deadline::new(500, Cancellation::default()).unwrap(),
                    clock(),
                );
                server.finish();
                assert_eq!(result.value.unwrap().version.value, Err(expected));
            }
            let (stream, server) = OwnedPeer::new(|mut peer| {
                let mut request = [0; 8];
                peer.read_exact(&mut request).unwrap();
                assert_eq!(&request, b"/version");
                let mut one = [0];
                assert_eq!(peer.read(&mut one).unwrap(), 0);
            });
            assert_eq!(
                hyprland_from_stream(
                    stream,
                    &Deadline::new(40, Cancellation::default()).unwrap(),
                    clock()
                )
                .value,
                Err(ProbeIssue::Timeout)
            );
            server.finish();
        }
        fn wl_request(peer: &mut UnixStream) -> (u32, u16, Vec<u8>) {
            let mut header = [0; 8];
            peer.read_exact(&mut header).unwrap();
            let object = u32::from_ne_bytes(header[..4].try_into().unwrap());
            let word = u32::from_ne_bytes(header[4..].try_into().unwrap());
            let len = (word >> 16) as usize;
            assert!((8..=MAX_PROBE_BYTES).contains(&len));
            let mut body = vec![0; len - 8];
            peer.read_exact(&mut body).unwrap();
            (object, word as u16, body)
        }
        fn wl_event(peer: &mut UnixStream, object: u32, opcode: u16, body: &[u8]) {
            assert_eq!(body.len() % 4, 0);
            let mut bytes = object.to_ne_bytes().to_vec();
            bytes.extend_from_slice(
                &((((body.len() + 8) as u32) << 16) | u32::from(opcode)).to_ne_bytes(),
            );
            bytes.extend_from_slice(body);
            peer.write_all(&bytes).unwrap();
        }
        fn wl_global(
            peer: &mut UnixStream,
            registry: u32,
            name: u32,
            interface: &str,
            version: u32,
        ) {
            let mut body = name.to_ne_bytes().to_vec();
            body.extend_from_slice(&((interface.len() + 1) as u32).to_ne_bytes());
            body.extend_from_slice(interface.as_bytes());
            body.push(0);
            while !body.len().is_multiple_of(4) {
                body.push(0);
            }
            body.extend_from_slice(&version.to_ne_bytes());
            wl_event(peer, registry, 0, &body);
        }
        fn registry_peer(
            work: impl FnOnce(&mut UnixStream, u32) + Send + 'static,
        ) -> (UnixStream, OwnedPeer) {
            OwnedPeer::new(move |mut peer| {
                let (object, opcode, body) = wl_request(&mut peer);
                assert_eq!((object, opcode), (1, 1));
                assert_eq!(body.len(), 4);
                let registry = u32::from_ne_bytes(body.try_into().unwrap());
                let (object, opcode, body) = wl_request(&mut peer);
                assert_eq!((object, opcode), (1, 0));
                assert_eq!(body.len(), 4);
                let callback = u32::from_ne_bytes(body.try_into().unwrap());
                work(&mut peer, registry);
                wl_event(&mut peer, callback, 0, &7u32.to_ne_bytes());
                // No bind request, capture/input/output object, or additional display request.
                let mut one = [0];
                assert_eq!(
                    peer.read(&mut one).unwrap(),
                    0,
                    "registry-only read sent another request"
                );
            })
        }
        #[test]
        fn actual_registry_only_sends_get_registry_and_sync_and_retains_peer_receipt() {
            let (stream, server) = registry_peer(|peer, registry| {
                for (index, (name, version)) in REQUIRED_PROTOCOLS.iter().enumerate() {
                    wl_global(peer, registry, index as u32 + 1, name, *version);
                }
            });
            let result = registry_from_stream(
                stream,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                clock(),
            );
            server.finish();
            let facts = result.value.unwrap();
            assert_eq!(facts.pid, std::process::id());
            assert_eq!(facts.globals, globals());
            assert_eq!(
                facts.protocols,
                Fact::known(true, ObservationSource::Demo, 10)
            );
            assert_eq!(result.observed_at_ms, 20);
            assert_eq!(result.source, ObservationSource::Demo);
        }
        #[test]
        fn registry_removal_changes_snapshot_and_duplicate_or_bad_events_fail_closed() {
            let (stream, server) = registry_peer(|peer, registry| {
                for (index, (name, version)) in REQUIRED_PROTOCOLS.iter().enumerate() {
                    wl_global(peer, registry, index as u32 + 1, name, *version);
                }
                wl_event(peer, registry, 1, &1u32.to_ne_bytes());
            });
            let result = registry_from_stream(
                stream,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                clock(),
            );
            server.finish();
            assert_eq!(result.value.unwrap().protocols.value, Ok(false));
            for variant in 0..3 {
                let (stream, server) = OwnedPeer::new(move |mut peer| {
                    let (_, _, body) = wl_request(&mut peer);
                    let registry = u32::from_ne_bytes(body.try_into().unwrap());
                    let _ = wl_request(&mut peer);
                    match variant {
                        0 => {
                            wl_global(&mut peer, registry, 1, "wl_seat", 5);
                            wl_global(&mut peer, registry, 1, "wl_seat", 5);
                        }
                        1 => wl_global(&mut peer, registry, 0, "wl_seat", 5),
                        2 => wl_global(&mut peer, registry, 1, "wl_seat", 0),
                        _ => wl_event(&mut peer, registry, 1, &7u32.to_ne_bytes()),
                    }
                    let mut one = [0];
                    assert_eq!(peer.read(&mut one).unwrap(), 0);
                });
                let result = registry_from_stream(
                    stream,
                    &Deadline::new(500, Cancellation::default()).unwrap(),
                    clock(),
                );
                server.finish();
                assert_eq!(
                    result.value.unwrap().protocols.value,
                    Err(ProbeIssue::Malformed),
                    "variant {variant}"
                );
            }
        }
        #[test]
        fn registry_global_row_and_string_limits_close_owned_stream_without_binding() {
            {
                let count = (MAX_REGISTRY_GLOBALS + 1) as u32;
                let interface = "valid".to_string();
                let (stream, server) = OwnedPeer::new(move |mut peer| {
                    let (_, _, body) = wl_request(&mut peer);
                    let registry = u32::from_ne_bytes(body.try_into().unwrap());
                    let _ = wl_request(&mut peer);
                    for name in 1..=count {
                        wl_global(&mut peer, registry, name, &interface, 1);
                    }
                    let mut one = [0];
                    assert_eq!(peer.read(&mut one).unwrap(), 0);
                });
                let result = registry_from_stream(
                    stream,
                    &Deadline::new(500, Cancellation::default()).unwrap(),
                    clock(),
                );
                server.finish();
                let facts = result.value.unwrap();
                assert_eq!(facts.pid, std::process::id());
                assert_eq!(facts.protocols.value, Err(ProbeIssue::Oversize));
            }
        }
        #[test]
        fn irrelevant_registry_metadata_does_not_erase_required_protocols() {
            let (stream, server) = registry_peer(|peer, registry| {
                for (index, (name, version)) in REQUIRED_PROTOCOLS.iter().enumerate() {
                    wl_global(peer, registry, index as u32 + 1, name, *version);
                }
                wl_global(peer, registry, 1000, &"x".repeat(129), 1);
                wl_event(peer, registry, 1, &2000u32.to_ne_bytes());
            });
            let result = registry_from_stream(
                stream,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                clock(),
            );
            server.finish();
            assert_eq!(result.value.unwrap().protocols.value, Ok(true));
        }
        #[test]
        fn manager_registry_auth_and_reads_share_deadline_cancel_without_other_socket_effects() {
            for registry in [false, true] {
                let (stream, server) = OwnedPeer::new(|mut peer| {
                    let mut one = [0];
                    loop {
                        if peer.read(&mut one).unwrap() == 0 {
                            break;
                        }
                    }
                });
                let deadline = Deadline::new(40, Cancellation::default()).unwrap();
                let issue = if registry {
                    registry_from_stream(stream, &deadline, clock()).value.err()
                } else {
                    manager_from_stream(stream, &deadline, clock()).value.err()
                };
                server.finish();
                assert_eq!(issue, Some(ProbeIssue::Timeout));
            }
            for probe in 0..3 {
                let (stream, mut peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let cancellation = Cancellation::default();
                cancellation.cancel();
                let deadline = Deadline::new(500, cancellation).unwrap();
                let issue = match probe {
                    0 => manager_from_stream(stream, &deadline, clock()).value.err(),
                    1 => hyprland_from_stream(stream, &deadline, clock()).value.err(),
                    _ => registry_from_stream(stream, &deadline, clock()).value.err(),
                };
                assert_eq!(issue, Some(ProbeIssue::Cancelled));
                let mut one = [0];
                assert_eq!(peer.read(&mut one).unwrap(), 0);
            }
            let (mut unrelated, mut peer) = UnixStream::pair().unwrap();
            unrelated.write_all(b"owned").unwrap();
            let mut bytes = [0; 5];
            peer.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"owned");
        }
        struct NeverProcess;
        #[test]
        fn cancellation_after_each_owned_protocol_starts_shuts_down_and_releases_worker() {
            for protocol in 0..3 {
                let (send, receive) = std::sync::mpsc::sync_channel(1);
                let (stream, server) = OwnedPeer::new(move |mut peer| {
                    match protocol {
                        0 => {
                            assert!(line(&mut peer).unwrap().starts_with(b"\0AUTH"));
                        }
                        1 => {
                            let mut request = [0; 8];
                            peer.read_exact(&mut request).unwrap();
                            assert_eq!(&request, b"/version");
                        }
                        _ => {
                            assert_eq!(wl_request(&mut peer).0, 1);
                            assert_eq!(wl_request(&mut peer).0, 1);
                        }
                    }
                    send.send(()).unwrap();
                    let mut one = [0];
                    assert_eq!(peer.read(&mut one).unwrap(), 0);
                });
                let cancellation = Cancellation::default();
                let deadline = Deadline::new(1000, cancellation.clone()).unwrap();
                let caller = thread::spawn(move || match protocol {
                    0 => manager_from_stream(stream, &deadline, clock()).value.err(),
                    1 => hyprland_from_stream(stream, &deadline, clock()).value.err(),
                    _ => registry_from_stream(stream, &deadline, clock()).value.err(),
                });
                receive.recv_timeout(Duration::from_secs(1)).unwrap();
                cancellation.cancel();
                assert_eq!(caller.join().unwrap(), Some(ProbeIssue::Cancelled));
                server.finish();
            }
            // A successful next owned exchange confirms cancelled clients retired their workers.
            assert_eq!(
                manager(script()).0.value.unwrap().compositor_managed.value,
                Ok(true)
            );
        }
        impl ProcessProbe for NeverProcess {
            fn snapshot(&self, _: u32, _: &Deadline) -> Result<ProcessFacts, NativeError> {
                panic!("detector queried an agent process")
            }
        }
        struct Runner {
            reply: Mutex<Option<Result<CommandOutput, NativeError>>>,
            calls: Mutex<usize>,
        }
        impl CommandRunner for Runner {
            fn run(
                &self,
                spec: &CommandSpec,
                deadline: &Deadline,
            ) -> Result<CommandOutput, NativeError> {
                deadline.check()?;
                assert_eq!(
                    spec.executable(),
                    std::path::Path::new("/usr/bin/systemctl")
                );
                assert_eq!(spec.argv(), ["--user", "show-environment"]);
                assert_eq!(spec.output_limit(), MAX_PROBE_BYTES);
                assert_eq!(spec.environment().values()["LC_ALL"], "C");
                assert_eq!(spec.environment().values()["TZ"], "UTC");
                assert!(
                    !spec
                        .environment()
                        .values()
                        .contains_key("DBUS_SYSTEM_BUS_ADDRESS")
                );
                *self.calls.lock().unwrap() += 1;
                self.reply.lock().unwrap().take().unwrap()
            }
        }
        static ROOT_IDS: AtomicU64 = AtomicU64::new(1);
        struct Scratch {
            io: Arc<LinuxNativeIo>,
            root: rustix::fd::OwnedFd,
            parent: rustix::fd::OwnedFd,
            run: rustix::fd::OwnedFd,
            systemd: rustix::fd::OwnedFd,
            name: String,
            listener: Option<UnixListener>,
        }
        impl Scratch {
            fn new(runner: Arc<Runner>) -> Self {
                use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, open, openat, statat};
                let name = format!(
                    "crosspane-a2-{}-{}",
                    std::process::id(),
                    ROOT_IDS.fetch_add(1, Ordering::SeqCst)
                );
                let path = PathBuf::from("/tmp").join(&name);
                // Frozen scratch construction exclusively creates this root; existing names fail.
                let io = Arc::new(
                    LinuxNativeIo::scratch(&path, runner, Arc::new(NeverProcess)).unwrap(),
                );
                let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                let parent = open("/tmp", flags, Mode::empty()).unwrap();
                let root = openat(&parent, &name, flags, Mode::empty()).unwrap();
                mkdirat(&root, "run", Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
                let run = openat(&root, "run", flags, Mode::empty()).unwrap();
                mkdirat(&run, "systemd", Mode::RUSR | Mode::WUSR | Mode::XUSR).unwrap();
                let systemd = openat(&run, "systemd", flags, Mode::empty()).unwrap();
                // Pathname bind is confined to this newly exclusive private root; verify immediately.
                let listener = UnixListener::bind(path.join("run/systemd/private")).unwrap();
                let socket = statat(&systemd, "private", AtFlags::SYMLINK_NOFOLLOW).unwrap();
                assert_eq!(socket.st_uid, io.target().paths().uid);
                assert_eq!(socket.st_mode & 0o170000, 0o140000);
                Self {
                    io,
                    root,
                    parent,
                    run,
                    systemd,
                    name,
                    listener: Some(listener),
                }
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                use rustix::fs::{AtFlags, fstat, statat, unlinkat};
                self.listener.take();
                unlinkat(&self.systemd, "private", AtFlags::empty()).unwrap();
                unlinkat(&self.run, "systemd", AtFlags::REMOVEDIR).unwrap();
                unlinkat(&self.root, "run", AtFlags::REMOVEDIR).unwrap();
                let expected = fstat(&self.root).unwrap();
                let current = statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
                assert_eq!(
                    (expected.st_dev, expected.st_ino),
                    (current.st_dev, current.st_ino)
                );
                unlinkat(&self.parent, &self.name, AtFlags::REMOVEDIR).unwrap();
            }
        }
        #[test]
        fn effective_environment_uses_only_exact_injected_show_environment_and_receipt() {
            let runner = Arc::new(Runner {
                reply: Mutex::new(None),
                calls: Mutex::new(0),
            });
            let scratch = Scratch::new(runner.clone());
            let runtime = scratch.io.target().paths().runtime_home.clone();
            let output = format!(
                "XDG_RUNTIME_DIR={}\nWAYLAND_DISPLAY=wayland-test\nHYPRLAND_INSTANCE_SIGNATURE=test-instance\nXDG_SESSION_ID=c7\n",
                runtime.display()
            );
            *runner.reply.lock().unwrap() = Some(Ok(CommandOutput {
                code: Some(0),
                stdout: output.into_bytes(),
                stderr: vec![],
            }));
            let environment =
                ChildEnvironment::selected(scratch.io.target(), BTreeMap::new()).unwrap();
            let probes =
                NativeSessionProbes::new(scratch.io.clone(), environment, clock()).unwrap();
            let result =
                probes.manager_environment(&Deadline::new(500, Cancellation::default()).unwrap());
            assert_eq!(
                result,
                Fact::known(
                    EffectiveEnvironment {
                        runtime_dir: runtime,
                        wayland_display: "wayland-test".into(),
                        hyprland_instance_signature: "test-instance".into(),
                        session_id: Some("c7".into()),
                        xdg_current_desktop: None,
                        xdg_session_type: None,
                    },
                    ObservationSource::Demo,
                    10
                )
            );
            assert_eq!(*runner.calls.lock().unwrap(), 1);
            // No installation, config, service file, agent directory, or environment import exists.
            assert_eq!(
                rustix::fs::statat(
                    &scratch.root,
                    ".local",
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW
                )
                .unwrap_err(),
                rustix::io::Errno::NOENT
            );
        }
        #[test]
        fn effective_environment_failure_timeout_malformed_and_oversize_remain_pending() {
            let runner = Arc::new(Runner {
                reply: Mutex::new(None),
                calls: Mutex::new(0),
            });
            let scratch = Scratch::new(runner.clone());
            let probes = NativeSessionProbes::new(
                scratch.io.clone(),
                ChildEnvironment::selected(scratch.io.target(), BTreeMap::new()).unwrap(),
                clock(),
            )
            .unwrap();
            for (output, expected) in [
                (Err(NativeError::Timeout), ProbeIssue::Timeout),
                (Err(NativeError::Cancelled), ProbeIssue::Cancelled),
                (
                    Ok(CommandOutput {
                        code: Some(1),
                        stdout: vec![],
                        stderr: b"fake denied".to_vec(),
                    }),
                    ProbeIssue::Unavailable,
                ),
                (
                    Ok(CommandOutput {
                        code: Some(0),
                        // An undecodable unrelated line is skipped; a required one is malformed.
                        stdout: b"OTHER=\xff\nXDG_RUNTIME_DIR=\xff\n".to_vec(),
                        stderr: vec![],
                    }),
                    ProbeIssue::Malformed,
                ),
                (
                    Ok(CommandOutput {
                        code: Some(0),
                        stdout: b"WAYLAND_DISPLAY=only-one-fact\n".to_vec(),
                        stderr: vec![],
                    }),
                    ProbeIssue::Missing,
                ),
                (
                    Ok(CommandOutput {
                        code: Some(0),
                        stdout: vec![b'x'; MAX_PROBE_BYTES + 1],
                        stderr: vec![],
                    }),
                    ProbeIssue::Oversize,
                ),
            ] {
                *runner.reply.lock().unwrap() = Some(output);
                let result = probes
                    .manager_environment(&Deadline::new(500, Cancellation::default()).unwrap());
                assert_eq!(result.value, Err(expected));
                assert_eq!(result.source, ObservationSource::Demo);
            }
            assert_eq!(*runner.calls.lock().unwrap(), 6);
            let cancel = Cancellation::default();
            cancel.cancel();
            assert_eq!(
                probes
                    .manager_environment(&Deadline::new(500, cancel).unwrap())
                    .value,
                Err(ProbeIssue::Cancelled)
            );
            assert_eq!(*runner.calls.lock().unwrap(), 6);
        }
        /// GNOME Shell and KDE Plasma lifecycle reads (`probe/manager/portal.rs`), over the same
        /// scripted systemd fake as the Hyprland read and judged by the same unit properties.
        mod portal {
            use super::*;
            const SHELL_BIN: &str = "/usr/bin/gnome-shell";
            const KWIN_BIN: &str = "/usr/bin/kwin_wayland";
            const KWIN_WRAPPER_BIN: &str = "/usr/bin/kwin_wayland_wrapper";
            const SHELL_ID: &str = "org.gnome.Shell@user.service";
            const SHELL_OBJECT: &str = "/units/shell";
            const SESSION_ID: &str = "gnome-session@gnome.target";
            const SESSION_OBJECT: &str = "/units/session";
            const KWIN_ID: &str = "plasma-kwin_wayland.service";
            const KWIN_OBJECT: &str = "/units/kwin";
            const UNIT_IFACE: &str = "org.freedesktop.systemd1.Unit";
            const SERVICE_IFACE: &str = "org.freedesktop.systemd1.Service";
            const GNOME_PID: u32 = 4242;
            const KDE_PID: u32 = 4300;
            fn strings(texts: &[&str]) -> Vec<String> {
                texts.iter().map(|text| text.to_string()).collect()
            }
            /// The parent's `unit` fixture plus the `PartOf` list that GNOME and KDE also read.
            fn unit_part_of(
                id: &str,
                part_of: &[&str],
                binds: &[&str],
                requires: &[&str],
            ) -> Properties {
                let mut values = unit(id, binds, requires);
                values.insert("PartOf".into(), value(strings(part_of)));
                values
            }
            /// One `ExecStart` record of a running main process `pid`, with systemd's timestamps.
            fn command(path: &str, argv: &[&str], ignore_errors: bool, pid: u32) -> Exec {
                (
                    path.into(),
                    strings(argv),
                    ignore_errors,
                    123,
                    124,
                    0,
                    0,
                    pid,
                    0,
                    0,
                )
            }
            fn gnome_exec(argv: &[&str]) -> Exec {
                command(SHELL_BIN, argv, false, GNOME_PID)
            }
            fn kde_exec(path: &str, argv: &[&str]) -> Exec {
                command(path, argv, false, KDE_PID)
            }
            /// A `Service` GetAll: its `Type`, its `MainPID` and the one `ExecStart` record.
            fn service_props(kind: &str, pid: u32, exec: Exec) -> Properties {
                [
                    ("Type".into(), value(kind.to_string())),
                    ("MainPID".into(), value(pid)),
                    ("ExecStart".into(), value(vec![exec])),
                ]
                .into_iter()
                .collect()
            }
            /// systemd 261 (see the Hyprland fixture): the `ExecStart` record is all zero and the
            /// running main process is only in the `ExecMain*` properties.
            fn systemd_261(kind: &str, pid: u32, exec: Exec) -> Properties {
                let mut row = exec;
                (row.3, row.4, row.5, row.6, row.7) = (0, 0, 0, 0, 0);
                let mut values = service_props(kind, pid, row);
                values.insert("ExecMainPID".into(), value(pid));
                values.insert(
                    "ExecMainStartTimestamp".into(),
                    value(1_791_096_132_706_343u64),
                );
                values.insert("ExecMainExitTimestamp".into(), value(0u64));
                values.insert("ExecMainCode".into(), value(0i32));
                values.insert("ExecMainStatus".into(), value(0i32));
                values
            }
            fn list_step(body: &'static str, rows: UnitRows) -> Step {
                Step {
                    method: "ListUnitsByPatterns",
                    path: "/org/freedesktop/systemd1",
                    interface: "org.freedesktop.systemd1.Manager",
                    body,
                    reply: Reply::Rows(rows),
                }
            }
            /// A `GetAll` on `path`; `body` names the interface it asks for.
            fn get_all(path: &'static str, body: &'static str, values: Properties) -> Step {
                Step {
                    method: "GetAll",
                    path,
                    interface: "org.freedesktop.DBus.Properties",
                    body,
                    reply: Reply::Properties(values),
                }
            }
            fn listed(steps: &mut [Step]) -> &mut UnitRows {
                match &mut steps[0].reply {
                    Reply::Rows(rows) => rows,
                    _ => panic!("fixture is not rows"),
                }
            }
            /// The GNOME session as systemd lists and reads it: `shell` runs `exec` and is required
            /// by `gnome-session@gnome.target`, which is part of `graphical-session.target`.
            fn gnome_script_with(shell: &str, exec: Exec) -> Vec<Step> {
                vec![
                    list_step(
                        "org.gnome.Shell@*.service",
                        vec![
                            row(GRAPHICAL, "/units/graphical"),
                            row(shell, SHELL_OBJECT),
                            row(SESSION_ID, SESSION_OBJECT),
                        ],
                    ),
                    get_all("/units/graphical", UNIT_IFACE, unit(GRAPHICAL, &[], &[])),
                    get_all(SHELL_OBJECT, UNIT_IFACE, unit(shell, &[], &[])),
                    get_all(
                        SESSION_OBJECT,
                        UNIT_IFACE,
                        unit_part_of(SESSION_ID, &[GRAPHICAL], &[], &[shell]),
                    ),
                    get_all(
                        SHELL_OBJECT,
                        SERVICE_IFACE,
                        service_props("notify", GNOME_PID, exec),
                    ),
                ]
            }
            fn gnome_script() -> Vec<Step> {
                gnome_script_with(SHELL_ID, gnome_exec(&[SHELL_BIN, "--mode=user"]))
            }
            fn instance_script(instance: &str, argv: &[&str]) -> Vec<Step> {
                gnome_script_with(
                    &format!("org.gnome.Shell@{instance}.service"),
                    gnome_exec(argv),
                )
            }
            /// KWin's unit as systemd lists and reads it, running `exec`.
            fn kde_script_with(exec: Exec) -> Vec<Step> {
                vec![
                    list_step(
                        KWIN_ID,
                        vec![
                            row(GRAPHICAL, "/units/graphical"),
                            row(KWIN_ID, KWIN_OBJECT),
                        ],
                    ),
                    get_all("/units/graphical", UNIT_IFACE, unit(GRAPHICAL, &[], &[])),
                    get_all(
                        KWIN_OBJECT,
                        UNIT_IFACE,
                        unit_part_of(KWIN_ID, &[GRAPHICAL], &[], &[]),
                    ),
                    get_all(
                        KWIN_OBJECT,
                        SERVICE_IFACE,
                        service_props("notify", KDE_PID, exec),
                    ),
                ]
            }
            fn kde_script() -> Vec<Step> {
                kde_script_with(kde_exec(
                    KWIN_WRAPPER_BIN,
                    &[KWIN_WRAPPER_BIN, "--xwayland"],
                ))
            }
            /// Runs the `desktop` read over the scripted bus: the fact, and the calls in order.
            fn run(desktop: Desktop, script: Vec<Step>) -> (Fact<ManagerFacts>, Vec<String>) {
                let (stream, server) = BusServer::new(script);
                let result = manager_from_stream_for(
                    stream,
                    &Deadline::new(1500, Cancellation::default()).unwrap(),
                    clock(),
                    desktop,
                );
                (result, server.finish())
            }
            /// Not proven managed: an established `false`, and no PID.
            fn unmanaged(desktop: Desktop, script: Vec<Step>) -> ManagerFacts {
                let decoded = run(desktop, script).0.value.unwrap();
                assert_eq!(decoded.compositor_managed.value, Ok(false));
                assert_eq!(decoded.compositor_pid, None);
                decoded
            }
            /// Proven managed: `pid` is the running main process of the compositor's unit.
            fn managed(desktop: Desktop, script: Vec<Step>, pid: u32) -> ManagerFacts {
                let decoded = run(desktop, script).0.value.unwrap();
                assert_eq!(decoded.compositor_managed.value, Ok(true));
                assert_eq!(decoded.compositor_pid, Some(pid));
                decoded
            }
            /// Only the listing and the graphical read happen: nothing is proven managed.
            fn only_listing(desktop: Desktop, mut script: Vec<Step>) -> ManagerFacts {
                script.truncate(2);
                let (fact, log) = run(desktop, script);
                assert_eq!(log.len(), 2);
                let decoded = fact.value.unwrap();
                assert_eq!(decoded.compositor_managed.value, Ok(false));
                assert_eq!(decoded.compositor_pid, None);
                assert_eq!(decoded.graphical_target_active.value, Ok(true));
                decoded
            }
            #[test]
            fn gnome_healthy_session_is_managed_with_exactly_the_five_expected_calls() {
                let (fact, log) = run(Desktop::Gnome, gnome_script());
                assert_eq!(fact.observed_at_ms, 60);
                assert_eq!(fact.source, ObservationSource::Demo);
                let decoded = fact.value.unwrap();
                assert_eq!(
                    decoded.compositor_managed,
                    Fact::known(true, ObservationSource::Demo, 50)
                );
                assert_eq!(
                    decoded.graphical_target_active,
                    Fact::known(true, ObservationSource::Demo, 20)
                );
                assert_eq!(decoded.compositor_pid, Some(GNOME_PID));
                assert_eq!(
                    log,
                    [
                        "ListUnitsByPatterns:/org/freedesktop/systemd1",
                        "GetAll:/units/graphical",
                        "GetAll:/units/shell",
                        "GetAll:/units/session",
                        "GetAll:/units/shell",
                    ]
                );
            }
            #[test]
            fn gnome_inactive_second_shell_row_does_not_displace_the_session_shell() {
                let mut steps = gnome_script();
                let rows = listed(&mut steps);
                rows.push(row("org.gnome.Shell@wayland.service", "/units/wayland"));
                rows[3].3 = "inactive".into();
                managed(Desktop::Gnome, steps, GNOME_PID);
            }
            #[test]
            fn gnome_shell_instance_must_be_user_or_wayland_and_its_mode_must_match() {
                // The greeter's `@gdm` shell belongs to another account's manager.
                unmanaged(
                    Desktop::Gnome,
                    instance_script("gdm", &[SHELL_BIN, "--mode=user"]),
                );
                // `@wayland` (GNOME 48 and earlier) takes its own mode, with or without switches.
                managed(
                    Desktop::Gnome,
                    instance_script("wayland", &[SHELL_BIN, "--mode=wayland"]),
                    GNOME_PID,
                );
                managed(
                    Desktop::Gnome,
                    instance_script(
                        "wayland",
                        &[SHELL_BIN, "--mode=wayland", "--wayland", "--no-x11"],
                    ),
                    GNOME_PID,
                );
                managed(
                    Desktop::Gnome,
                    instance_script("user", &[SHELL_BIN, "--mode=user", "--wayland"]),
                    GNOME_PID,
                );
                // The mode must be the instance's own, and any other instance name is refused.
                unmanaged(
                    Desktop::Gnome,
                    instance_script("wayland", &[SHELL_BIN, "--mode=user"]),
                );
                unmanaged(
                    Desktop::Gnome,
                    instance_script("user", &[SHELL_BIN, "--mode=wayland"]),
                );
                unmanaged(
                    Desktop::Gnome,
                    instance_script("other", &[SHELL_BIN, "--mode=other"]),
                );
            }
            #[test]
            fn gnome_executable_and_arguments_must_be_the_shell_itself() {
                managed(
                    Desktop::Gnome,
                    gnome_script_with(SHELL_ID, gnome_exec(&[SHELL_BIN])),
                    GNOME_PID,
                );
                let refused: [(&str, &[&str]); 4] = [
                    ("/usr/bin/evil", &["/usr/bin/evil", "--mode=user"]),
                    (SHELL_BIN, &["/usr/bin/evil", "--mode=user"]),
                    ("/usr/local/bin/gnome-shell", &[SHELL_BIN, "--mode=user"]),
                    (SHELL_BIN, &[SHELL_BIN, "--mode=user", "--eval"]),
                ];
                for (path, argv) in refused {
                    let exec = command(path, argv, false, GNOME_PID);
                    unmanaged(Desktop::Gnome, gnome_script_with(SHELL_ID, exec));
                }
            }
            #[test]
            fn gnome_service_must_be_notify_and_must_not_ignore_errors() {
                let mut steps = gnome_script();
                properties(&mut steps[4]).insert("Type".into(), value("simple".to_string()));
                unmanaged(Desktop::Gnome, steps);
                let exec = command(SHELL_BIN, &[SHELL_BIN, "--mode=user"], true, GNOME_PID);
                unmanaged(Desktop::Gnome, gnome_script_with(SHELL_ID, exec));
            }
            #[test]
            fn gnome_session_must_require_the_shell_and_be_bound_to_graphical() {
                let mut steps = gnome_script();
                steps[3].reply =
                    Reply::Properties(unit_part_of(SESSION_ID, &[GRAPHICAL], &[], &[]));
                unmanaged(Desktop::Gnome, steps);
                // Neither PartOf nor BindsTo names the graphical target.
                let mut steps = gnome_script();
                steps[3].reply = Reply::Properties(unit_part_of(SESSION_ID, &[], &[], &[SHELL_ID]));
                unmanaged(Desktop::Gnome, steps);
                let mut steps = gnome_script();
                steps[3].reply = Reply::Properties(unit_part_of(
                    SESSION_ID,
                    &["default.target"],
                    &[],
                    &[SHELL_ID],
                ));
                unmanaged(Desktop::Gnome, steps);
                // BindsTo is the other accepted way to be bound to the graphical target.
                let mut steps = gnome_script();
                steps[3].reply =
                    Reply::Properties(unit_part_of(SESSION_ID, &[], &[GRAPHICAL], &[SHELL_ID]));
                managed(Desktop::Gnome, steps, GNOME_PID);
            }
            #[test]
            fn gnome_inactive_units_and_a_missing_session_stop_after_the_graphical_read() {
                let mut steps = gnome_script();
                listed(&mut steps)[1].3 = "inactive".into();
                only_listing(Desktop::Gnome, steps);
                let mut steps = gnome_script();
                listed(&mut steps)[2].3 = "inactive".into();
                only_listing(Desktop::Gnome, steps);
                let mut steps = gnome_script();
                listed(&mut steps).truncate(2);
                only_listing(Desktop::Gnome, steps);
                // No graphical row: the target reads inactive and its call never happens. The
                // session's own PartOf still names the target, so the shell stays managed.
                let mut steps = gnome_script();
                listed(&mut steps).remove(0);
                steps.remove(1);
                let (fact, log) = run(Desktop::Gnome, steps);
                let decoded = fact.value.unwrap();
                assert_eq!(decoded.graphical_target_active.value, Ok(false));
                assert_eq!(decoded.compositor_managed.value, Ok(true));
                assert_eq!(decoded.compositor_pid, Some(GNOME_PID));
                assert_eq!(
                    log,
                    [
                        "ListUnitsByPatterns:/org/freedesktop/systemd1",
                        "GetAll:/units/shell",
                        "GetAll:/units/session",
                        "GetAll:/units/shell",
                    ]
                );
            }
            #[test]
            fn gnome_ambiguous_or_transitional_units_fail_closed() {
                let mut steps = gnome_script();
                listed(&mut steps).push(row("org.gnome.Shell@wayland.service", "/units/wayland"));
                steps.truncate(2);
                let (fact, log) = run(Desktop::Gnome, steps);
                assert_eq!(fact.value, Err(ProbeIssue::Ambiguous));
                assert_eq!(log.len(), 2);
                let mut steps = gnome_script();
                listed(&mut steps).push(row("gnome-session@other.target", "/units/other"));
                steps.truncate(2);
                assert_eq!(
                    run(Desktop::Gnome, steps).0.value,
                    Err(ProbeIssue::Ambiguous)
                );
                for state in ["activating", "deactivating", "reloading", "maintenance"] {
                    for index in [1, 2] {
                        let mut steps = gnome_script();
                        listed(&mut steps)[index].3 = state.into();
                        steps.truncate(1);
                        let (fact, log) = run(Desktop::Gnome, steps);
                        assert_eq!(fact.value, Err(ProbeIssue::Unverified), "{state} {index}");
                        assert_eq!(log.len(), 1);
                    }
                }
                let mut steps = gnome_script();
                properties(&mut steps[4]).insert("ExecStart".into(), value(Vec::<Exec>::new()));
                assert_eq!(
                    run(Desktop::Gnome, steps).0.value,
                    Err(ProbeIssue::Unverified)
                );
                let mut steps = gnome_script();
                let two = vec![
                    gnome_exec(&[SHELL_BIN, "--mode=user"]),
                    gnome_exec(&[SHELL_BIN, "--mode=user"]),
                ];
                properties(&mut steps[4]).insert("ExecStart".into(), value(two));
                assert_eq!(
                    run(Desktop::Gnome, steps).0.value,
                    Err(ProbeIssue::Ambiguous)
                );
            }
            #[test]
            fn gnome_systemd_261_exec_main_properties_are_accepted_and_contradictions_fail() {
                let argv = [SHELL_BIN, "--mode=user"];
                let mut steps = gnome_script();
                let values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                steps[4].reply = Reply::Properties(values);
                managed(Desktop::Gnome, steps, GNOME_PID);
                // A populated row beside the ExecMain* properties is accepted as well.
                let mut steps = gnome_script();
                let mut values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                values.insert("ExecStart".into(), value(vec![gnome_exec(&argv)]));
                steps[4].reply = Reply::Properties(values);
                managed(Desktop::Gnome, steps, GNOME_PID);
                let contradictions: [(&str, OwnedValue); 6] = [
                    ("ExecMainPID", value(4243u32)),
                    ("ExecMainPID", value(0u32)),
                    ("ExecMainStartTimestamp", value(0u64)),
                    ("ExecMainExitTimestamp", value(1u64)),
                    ("ExecMainCode", value(1i32)),
                    ("ExecMainStatus", value(1i32)),
                ];
                for (key, bad) in contradictions {
                    let mut steps = gnome_script();
                    let mut values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                    values.insert(key.into(), bad);
                    steps[4].reply = Reply::Properties(values);
                    let result = run(Desktop::Gnome, steps).0.value;
                    assert_eq!(result, Err(ProbeIssue::Unverified), "{key}");
                }
                for key in [
                    "ExecMainPID",
                    "ExecMainStartTimestamp",
                    "ExecMainExitTimestamp",
                    "ExecMainCode",
                    "ExecMainStatus",
                ] {
                    let mut steps = gnome_script();
                    let mut values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                    values.remove(key);
                    steps[4].reply = Reply::Properties(values);
                    let result = run(Desktop::Gnome, steps).0.value;
                    assert_eq!(result, Err(ProbeIssue::Unverified), "missing {key}");
                    let mut steps = gnome_script();
                    let mut values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                    values.insert(key.into(), value("wrong".to_string()));
                    steps[4].reply = Reply::Properties(values);
                    let result = run(Desktop::Gnome, steps).0.value;
                    assert_eq!(result, Err(ProbeIssue::Malformed), "mistyped {key}");
                }
                for field in 3..=6 {
                    let mut row = gnome_exec(&argv);
                    (row.3, row.4, row.5, row.6, row.7) = (0, 0, 0, 0, 0);
                    match field {
                        3 => row.3 = 1,
                        4 => row.4 = 1,
                        5 => row.5 = 1,
                        _ => row.6 = 1,
                    }
                    let mut steps = gnome_script();
                    let mut values = systemd_261("notify", GNOME_PID, gnome_exec(&argv));
                    values.insert("ExecStart".into(), value(vec![row]));
                    steps[4].reply = Reply::Properties(values);
                    let result = run(Desktop::Gnome, steps).0.value;
                    assert_eq!(result, Err(ProbeIssue::Unverified), "field {field}");
                }
            }
            #[test]
            fn gnome_list_error_is_unavailable_and_nothing_else_is_read() {
                let mut steps = gnome_script();
                steps[0].reply = Reply::Error;
                steps.truncate(1);
                let (fact, log) = run(Desktop::Gnome, steps);
                assert_eq!(fact.value, Err(ProbeIssue::Unavailable));
                assert_eq!(log, ["ListUnitsByPatterns:/org/freedesktop/systemd1"]);
            }
            #[test]
            fn kde_healthy_kwin_unit_is_managed_with_four_calls_and_its_wrapper_pid() {
                let (fact, log) = run(Desktop::Kde, kde_script());
                assert_eq!(fact.observed_at_ms, 50);
                let decoded = fact.value.unwrap();
                assert_eq!(
                    decoded.compositor_managed,
                    Fact::known(true, ObservationSource::Demo, 40)
                );
                assert_eq!(
                    decoded.graphical_target_active,
                    Fact::known(true, ObservationSource::Demo, 20)
                );
                assert_eq!(decoded.compositor_pid, Some(KDE_PID));
                assert_eq!(
                    log,
                    [
                        "ListUnitsByPatterns:/org/freedesktop/systemd1",
                        "GetAll:/units/graphical",
                        "GetAll:/units/kwin",
                        "GetAll:/units/kwin",
                    ]
                );
                // The compositor binary itself is accepted, and so is the systemd 261 form.
                let exec = kde_exec(KWIN_BIN, &[KWIN_BIN, "--xwayland"]);
                managed(Desktop::Kde, kde_script_with(exec), KDE_PID);
                let mut steps = kde_script();
                let exec = kde_exec(KWIN_WRAPPER_BIN, &[KWIN_WRAPPER_BIN]);
                steps[3].reply = Reply::Properties(systemd_261("notify", KDE_PID, exec));
                managed(Desktop::Kde, steps, KDE_PID);
            }
            #[test]
            fn kde_compositor_path_argv_ignore_and_binding_must_all_match() {
                let refused: [(&str, &[&str]); 3] = [
                    ("/usr/bin/other", &["/usr/bin/other"]),
                    (KWIN_WRAPPER_BIN, &[KWIN_BIN, "--xwayland"]),
                    (KWIN_BIN, &[KWIN_WRAPPER_BIN, "--xwayland"]),
                ];
                for (path, argv) in refused {
                    unmanaged(Desktop::Kde, kde_script_with(kde_exec(path, argv)));
                }
                let exec = command(KWIN_WRAPPER_BIN, &[KWIN_WRAPPER_BIN], true, KDE_PID);
                unmanaged(Desktop::Kde, kde_script_with(exec));
                let mut steps = kde_script();
                steps[2].reply =
                    Reply::Properties(unit_part_of(KWIN_ID, &["default.target"], &[], &[]));
                unmanaged(Desktop::Kde, steps);
                let mut steps = kde_script();
                steps[2].reply = Reply::Properties(unit_part_of(KWIN_ID, &[], &[GRAPHICAL], &[]));
                managed(Desktop::Kde, steps, KDE_PID);
            }
            #[test]
            fn kde_inactive_absent_or_transitional_kwin_stops_before_the_unit_reads() {
                let mut steps = kde_script();
                listed(&mut steps)[1].3 = "inactive".into();
                only_listing(Desktop::Kde, steps);
                let mut steps = kde_script();
                listed(&mut steps).truncate(1);
                only_listing(Desktop::Kde, steps);
                let mut steps = kde_script();
                listed(&mut steps)[1].3 = "activating".into();
                steps.truncate(1);
                let (fact, log) = run(Desktop::Kde, steps);
                assert_eq!(fact.value, Err(ProbeIssue::Unverified));
                assert_eq!(log.len(), 1);
            }
            #[test]
            fn hyprland_desktop_read_is_the_original_uwsm_read_unchanged() {
                assert_eq!(run(Desktop::Hyprland, script()), manager(script()));
            }
            fn version_cases() -> [(&'static str, [u16; 3]); 4] {
                [
                    ("50.4", [50, 4, 0]),
                    ("49.2.1", [49, 2, 1]),
                    ("50.rc", [50, 0, 0]),
                    ("50.beta.1", [50, 0, 0]),
                ]
            }
            fn read_shell_version(script: Vec<Step>) -> (Fact<[u16; 3]>, Vec<String>) {
                let (stream, server) = BusServer::new(script);
                let result = shell_version_from_stream(
                    stream,
                    &Deadline::new(1500, Cancellation::default()).unwrap(),
                    clock(),
                );
                (result, server.finish())
            }
            /// The one call the version read makes: GNOME Shell's own properties.
            fn shell_step(values: Properties) -> Step {
                get_all("/org/gnome/Shell", "org.gnome.Shell", values)
            }
            fn shell_version_text(text: OwnedValue) -> Properties {
                [("ShellVersion".into(), text)].into_iter().collect()
            }
            #[test]
            fn shell_version_property_maps_through_the_literal_parser() {
                for (text, expected) in version_cases() {
                    let values = shell_version_text(value(text.to_string()));
                    let (fact, log) = read_shell_version(vec![shell_step(values)]);
                    assert_eq!(fact.value, Ok(expected), "{text}");
                    assert_eq!(log, ["GetAll:/org/gnome/Shell"]);
                }
                let values = shell_version_text(value("50.4".to_string()));
                let (fact, _) = read_shell_version(vec![shell_step(values)]);
                assert_eq!(fact.observed_at_ms, 20);
            }
            #[test]
            fn shell_version_missing_or_mistyped_property_is_pending_or_malformed() {
                let (fact, log) = read_shell_version(vec![shell_step(Properties::new())]);
                assert_eq!(fact.value, Err(ProbeIssue::Unverified));
                assert_eq!(log.len(), 1);
                let values = shell_version_text(value(7u32));
                let (fact, _) = read_shell_version(vec![shell_step(values)]);
                assert_eq!(fact.value, Err(ProbeIssue::Malformed));
                let values = shell_version_text(value("x.y".to_string()));
                let (fact, _) = read_shell_version(vec![shell_step(values)]);
                assert_eq!(fact.value, Err(ProbeIssue::Malformed));
            }
            #[test]
            fn parse_shell_version_keeps_the_leading_numbers_and_rejects_the_rest() {
                for (text, expected) in version_cases() {
                    assert_eq!(parse_shell_version(text), Ok(expected), "{text}");
                }
                assert_eq!(parse_shell_version(""), Err(ProbeIssue::Malformed));
                assert_eq!(parse_shell_version("x.y"), Err(ProbeIssue::Malformed));
                let long = "5".repeat(100);
                assert_eq!(parse_shell_version(&long), Err(ProbeIssue::Oversize));
            }
        }
    }
    mod native_logind {
        use crosspane_installer::agent_contract::ObservationSource;
        use crosspane_installer::platform::linux::{
            detect::*,
            native_io::{Cancellation, Deadline},
            transport::CallerClock,
        };
        use serde::Serialize;
        use std::{
            io::{Read, Write},
            net::Shutdown,
            num::NonZeroU32,
            os::unix::net::UnixStream,
            sync::{
                Arc, Mutex,
                atomic::{AtomicU64, Ordering},
            },
            thread::{self, JoinHandle},
            time::{Duration, Instant},
        };
        use zbus::zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Value};

        fn path(value: &str) -> OwnedObjectPath {
            value.try_into().unwrap()
        }
        fn value<T: Into<Value<'static>> + DynamicType>(value: T) -> OwnedValue {
            OwnedValue::try_from(Value::new(value)).unwrap()
        }
        fn session(id: &str, kind: &str, uid: u32, active: bool, locked: bool) -> Properties {
            [
                ("Id", value(id.to_string())),
                ("Type", value(kind.to_string())),
                (
                    "User",
                    value((uid, path("/org/freedesktop/login1/user/_1000"))),
                ),
                (
                    "Seat",
                    value((
                        "seat0".to_string(),
                        path("/org/freedesktop/login1/seat/seat0"),
                    )),
                ),
                ("Active", value(active)),
                ("LockedHint", value(locked)),
            ]
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect()
        }
        fn user(ids: &[&str], display: &str) -> Properties {
            let rows: Vec<_> = ids
                .iter()
                .map(|id| (id.to_string(), path(&format!("/session/{id}"))))
                .collect();
            let display_path = if display.is_empty() {
                path("/")
            } else {
                path(&format!("/session/{display}"))
            };
            [
                ("Display".into(), value((display.to_string(), display_path))),
                ("Sessions".into(), value(rows)),
            ]
            .into_iter()
            .collect()
        }
        enum Response {
            Text(String),
            Path(OwnedObjectPath),
            Properties(Properties),
            Error(&'static str),
            ErrorBody(&'static str, String),
            Stall,
        }
        struct Step {
            method: &'static str,
            path: &'static str,
            body: Option<Vec<u8>>,
            response: Response,
        }
        fn step(method: &'static str, path: &'static str, response: Response) -> Step {
            Step {
                method,
                path,
                body: None,
                response,
            }
        }
        fn return_message<T: Serialize + DynamicType>(
            serial: NonZeroU32,
            value: &T,
        ) -> zbus::Message {
            let dummy = zbus::Message::method_call("/fake", "Request")
                .unwrap()
                .serial(serial)
                .build(&())
                .unwrap();
            zbus::Message::method_return(&dummy.header())
                .unwrap()
                .sender(":1.1")
                .unwrap()
                .build(value)
                .unwrap()
        }
        fn read_line(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
            let mut line = Vec::new();
            while !line.ends_with(b"\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                line.push(byte[0]);
                if line.len() > 4096 {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
            }
            Ok(line)
        }
        fn frame(stream: &mut UnixStream) -> std::io::Result<(NonZeroU32, Vec<u8>, Vec<u8>)> {
            let mut header = [0; 16];
            stream.read_exact(&mut header)?;
            assert_eq!(header[0], b'l');
            let body_len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            let fields_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let body_at = (16 + fields_len + 7) & !7;
            assert!(body_at + body_len <= MAX_PROBE_BYTES);
            let mut bytes = header.to_vec();
            bytes.resize(body_at + body_len, 0);
            stream.read_exact(&mut bytes[16..])?;
            let body = bytes[body_at..].to_vec();
            Ok((
                NonZeroU32::new(u32::from_le_bytes(header[8..12].try_into().unwrap())).unwrap(),
                bytes,
                body,
            ))
        }
        fn has_string(bytes: &[u8], text: &str) -> bool {
            let mut encoded = (text.len() as u32).to_le_bytes().to_vec();
            encoded.extend_from_slice(text.as_bytes());
            encoded.push(0);
            bytes.windows(encoded.len()).any(|window| window == encoded)
        }
        struct Server {
            control: UnixStream,
            join: Option<JoinHandle<std::io::Result<()>>>,
            log: Arc<Mutex<Vec<String>>>,
        }
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.control.shutdown(Shutdown::Both);
                if let Some(join) = self.join.take() {
                    let _ = join.join();
                }
            }
        }
        impl Server {
            fn new(script: Vec<Step>) -> (UnixStream, Self) {
                let (client, mut server) = UnixStream::pair().unwrap();
                server
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                server
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let control = server.try_clone().unwrap();
                let log = Arc::new(Mutex::new(Vec::new()));
                let record = log.clone();
                let join = thread::spawn(move || {
                    loop {
                        let line = read_line(&mut server)?;
                        if line.ends_with(b"BEGIN\r\n") {
                            break;
                        }
                        if line.starts_with(b"\0AUTH") || line.starts_with(b"AUTH") {
                            server.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n")?;
                        } else {
                            assert!(line.starts_with(b"NEGOTIATE_UNIX_FD"));
                            server.write_all(b"ERROR no descriptor passing\r\n")?;
                        }
                    }
                    for request in std::iter::once(step(
                        "Hello",
                        "/org/freedesktop/DBus",
                        Response::Text(":1.42".into()),
                    ))
                    .chain(script)
                    {
                        let (serial, bytes, body) = frame(&mut server)?;
                        assert!(
                            has_string(&bytes, request.method),
                            "wrong method for {}",
                            request.method
                        );
                        assert!(
                            has_string(&bytes, request.path),
                            "wrong path for {}",
                            request.method
                        );
                        if let Some(expected) = request.body {
                            assert_eq!(body, expected);
                        }
                        if request.method != "Hello" {
                            let interface = if request.method == "GetAll" {
                                "org.freedesktop.DBus.Properties"
                            } else {
                                "org.freedesktop.login1.Manager"
                            };
                            assert!(has_string(&bytes, interface));
                            assert!(has_string(&bytes, "org.freedesktop.login1"));
                        }
                        record.lock().unwrap().push(request.method.into());
                        let reply = match request.response {
                            Response::Text(v) => return_message(serial, &v),
                            Response::Path(v) => return_message(serial, &v),
                            Response::Properties(v) => return_message(serial, &v),
                            Response::Error(name) => {
                                let dummy = zbus::Message::method_call("/fake", "Request")
                                    .unwrap()
                                    .serial(serial)
                                    .build(&())
                                    .unwrap();
                                zbus::Message::error(&dummy.header(), name)
                                    .unwrap()
                                    .sender(":1.1")
                                    .unwrap()
                                    .build(&"injected error")
                                    .unwrap()
                            }
                            Response::ErrorBody(name, body) => {
                                let dummy = zbus::Message::method_call("/fake", "Request")
                                    .unwrap()
                                    .serial(serial)
                                    .build(&())
                                    .unwrap();
                                zbus::Message::error(&dummy.header(), name)
                                    .unwrap()
                                    .sender(":1.1")
                                    .unwrap()
                                    .build(&body)
                                    .unwrap()
                            }
                            Response::Stall => {
                                let mut one = [0];
                                server.read_exact(&mut one)?;
                                panic!("unexpected extra request");
                            }
                        };
                        server.write_all(reply.data().bytes())?;
                    }
                    let mut one = [0];
                    assert_eq!(server.read(&mut one)?, 0, "unexpected request after script");
                    Ok(())
                });
                (
                    client,
                    Self {
                        control,
                        join: Some(join),
                        log,
                    },
                )
            }
            fn finish(mut self) -> Vec<String> {
                let result = self.join.take().unwrap().join().unwrap();
                result.unwrap();
                self.log.lock().unwrap().clone()
            }
        }
        fn absent() -> Step {
            step(
                "GetSessionByPID",
                "/org/freedesktop/login1",
                Response::Error("org.freedesktop.login1.NoSessionForPID"),
            )
        }
        fn display_script(rows: &[&str], props: Vec<Properties>) -> Vec<Step> {
            let mut script = vec![
                absent(),
                step(
                    "GetUser",
                    "/org/freedesktop/login1",
                    Response::Path(path("/user/1000")),
                ),
                step(
                    "GetAll",
                    "/user/1000",
                    Response::Properties(user(rows, rows.first().copied().unwrap_or(""))),
                ),
            ];
            for (id, properties) in rows.iter().zip(props) {
                // These fixture IDs are fixed test literals; no filesystem path is opened.
                let session_path = match *id {
                    "c7" => "/session/c7",
                    "c8" => "/session/c8",
                    "tty" => "/session/tty",
                    _ => panic!("unknown test fixture"),
                };
                script.push(step(
                    "GetAll",
                    session_path,
                    Response::Properties(properties),
                ));
            }
            script
        }
        fn probe(script: Vec<Step>, id: Option<&str>) -> (Fact<LogindFacts>, Vec<String>) {
            let (stream, server) = Server::new(script);
            let sequence = Arc::new(AtomicU64::new(10));
            let clock: CallerClock = Arc::new(move || sequence.fetch_add(10, Ordering::SeqCst));
            let result = logind_from_stream(
                stream,
                1000,
                4242,
                id.map(str::to_string),
                &Deadline::new(1500, Cancellation::default()).unwrap(),
                clock,
            );
            (result, server.finish())
        }

        #[test]
        fn login1_codec_pins_signatures_and_each_missing_or_wrong_typed_field() {
            let decoded =
                decode_session("/session/c7", &session("c7", "x11", 1000, false, true)).unwrap();
            assert_eq!(
                decoded,
                SessionCandidate {
                    id: "c7".into(),
                    path: "/session/c7".into(),
                    kind: Some("x11".into()),
                    uid: Some(1000),
                    seat: Some("seat0".into()),
                    active: Some(false),
                    locked_hint: Some(true)
                }
            );
            for field in ["Type", "User", "Seat", "Active", "LockedHint"] {
                let mut values = session("c7", "wayland", 1000, true, false);
                values.remove(field);
                let result = decode_session("/session/c7", &values).unwrap();
                match field {
                    "Type" => assert!(result.kind.is_none()),
                    "User" => assert!(result.uid.is_none()),
                    "Seat" => assert!(result.seat.is_none()),
                    "Active" => assert!(result.active.is_none()),
                    "LockedHint" => assert!(result.locked_hint.is_none()),
                    _ => unreachable!(),
                }
                values.insert(field.into(), value(7i64));
                assert_eq!(
                    decode_session("/session/c7", &values),
                    Err(ProbeIssue::Malformed)
                );
            }
            let mut values = session("c7", "wayland", 1000, true, false);
            values.remove("Id");
            assert_eq!(
                decode_session("/session/c7", &values).unwrap().id,
                "/session/c7"
            );
            values.insert("Id".into(), value(String::new()));
            assert_eq!(
                decode_session("/session/c7", &values),
                Err(ProbeIssue::Malformed)
            );
            for field in ["Display", "Sessions"] {
                let mut values = user(&["c7"], "c7");
                values.remove(field);
                assert_eq!(decode_user(&values), Err(ProbeIssue::Unverified));
                values.insert(field.into(), value(true));
                assert_eq!(decode_user(&values), Err(ProbeIssue::Malformed));
            }
        }
        #[test]
        fn login1_codec_rejects_short_extra_tuples_and_wrong_empty_arrays() {
            for (field, malformed) in [
                ("User", value((1000u32,))),
                ("User", value((1000u32, path("/user/1000"), true))),
                ("Seat", value(("seat0".to_string(),))),
                (
                    "Seat",
                    value(("seat0".to_string(), path("/seat/seat0"), true)),
                ),
            ] {
                let mut values = session("c7", "wayland", 1000, true, false);
                values.insert(field.into(), malformed);
                assert_eq!(
                    decode_session("/session/c7", &values),
                    Err(ProbeIssue::Malformed),
                    "wrong {field} tuple must fail without conversion panic"
                );
            }
            for malformed in [
                value(("c7".to_string(),)),
                value(("c7".to_string(), path("/session/c7"), true)),
            ] {
                let mut values = user(&["c7"], "c7");
                values.insert("Display".into(), malformed);
                assert_eq!(decode_user(&values), Err(ProbeIssue::Malformed));
            }
            for (signature, malformed) in [
                ("as", value(Vec::<String>::new())),
                ("au", value(Vec::<u32>::new())),
                ("a(uo)", value(Vec::<(u32, OwnedObjectPath)>::new())),
                ("a(s)", value(Vec::<(String,)>::new())),
                (
                    "a(sob)",
                    value(Vec::<(String, OwnedObjectPath, bool)>::new()),
                ),
            ] {
                assert_eq!(malformed.value_signature().to_string(), signature);
                let mut values = user(&[], "");
                values.insert("Sessions".into(), malformed);
                assert_eq!(decode_user(&values), Err(ProbeIssue::Malformed));
            }
            let exact = user(&[], "");
            assert_eq!(exact["Sessions"].value_signature().to_string(), "a(so)");
            assert_eq!(decode_user(&exact), Ok(("/".into(), vec![])));
            assert_eq!(
                decode_session("/session/c7", &session("c7", "wayland", 1000, true, false))
                    .unwrap()
                    .uid,
                Some(1000)
            );
        }
        #[test]
        fn actual_client_rejects_malformed_tuple_and_empty_array_signatures() {
            for malformed in [
                value((1000u32,)),
                value((1000u32, path("/user/1000"), true)),
            ] {
                let mut properties = session("c7", "wayland", 1000, true, false);
                properties.insert("User".into(), malformed);
                let (fact, log) = probe(
                    vec![
                        step(
                            "GetSessionByPID",
                            "/org/freedesktop/login1",
                            Response::Path(path("/session/c7")),
                        ),
                        step("GetAll", "/session/c7", Response::Properties(properties)),
                    ],
                    Some("never_queried"),
                );
                let decoded = fact.value.unwrap();
                assert_eq!(decoded.selected_session.value, Err(ProbeIssue::Malformed));
                assert_eq!(decoded.graphical_sessions.value, Err(ProbeIssue::Malformed));
                assert_eq!(log, ["Hello", "GetSessionByPID", "GetAll"]);
            }
            for malformed in [
                value(Vec::<String>::new()),
                value(Vec::<(String, OwnedObjectPath, bool)>::new()),
            ] {
                let mut properties = user(&[], "");
                properties.insert("Sessions".into(), malformed);
                let (fact, log) = probe(
                    vec![
                        absent(),
                        step(
                            "GetUser",
                            "/org/freedesktop/login1",
                            Response::Path(path("/user/1000")),
                        ),
                        step("GetAll", "/user/1000", Response::Properties(properties)),
                    ],
                    None,
                );
                let decoded = fact.value.unwrap();
                assert_eq!(decoded.selected_session.value, Err(ProbeIssue::Malformed));
                assert_eq!(decoded.graphical_sessions.value, Err(ProbeIssue::Malformed));
                assert_eq!(log, ["Hello", "GetSessionByPID", "GetUser", "GetAll"]);
            }
        }
        #[test]
        fn login1_codec_bounds_rows_keys_paths_ids_and_identity_duplicates() {
            let ids: Vec<_> = (0..MAX_SESSIONS).map(|i| format!("c{i}")).collect();
            let refs: Vec<_> = ids.iter().map(String::as_str).collect();
            assert_eq!(
                decode_user(&user(&refs, "c0")).unwrap().1.len(),
                MAX_SESSIONS
            );
            let mut too_many = refs.clone();
            too_many.push("extra");
            assert_eq!(
                decode_user(&user(&too_many, "c0")),
                Err(ProbeIssue::Oversize)
            );
            assert_eq!(
                decode_user(&user(&["c7", "c7"], "c7")),
                Err(ProbeIssue::Malformed)
            );
            let mut duplicate = user(&["c7"], "c7");
            duplicate.insert(
                "Sessions".into(),
                value(vec![
                    ("c7".to_string(), path("/session/c7")),
                    ("c7".to_string(), path("/session/c8")),
                ]),
            );
            assert_eq!(decode_user(&duplicate), Err(ProbeIssue::Malformed));
            for field in ["Id", "Type"] {
                let mut values = session("c7", "wayland", 1000, true, false);
                values.insert(field.into(), value("x".repeat(65)));
                assert_eq!(
                    decode_session("/session/c7", &values),
                    Err(ProbeIssue::Oversize)
                );
            }
            for field in ["Id", "Type"] {
                let mut values = session("c7", "wayland", 1000, true, false);
                values.insert(field.into(), value("bad\nvalue".to_string()));
                assert_eq!(
                    decode_session("/session/c7", &values),
                    Err(ProbeIssue::Malformed)
                );
            }
            for bad in ["/", "not-an-object-path", "/bad-path"] {
                assert_eq!(
                    decode_session(bad, &Properties::new()),
                    Err(ProbeIssue::Malformed)
                );
            }
            assert_eq!(
                decode_session(&format!("/{}", "p".repeat(512)), &Properties::new()),
                Err(ProbeIssue::Oversize)
            );
            // Real systemd 261 replies carry hundreds of properties (Service GetAll: 369).
            let mut values = session("c7", "wayland", 1000, true, false);
            for i in 0..300 {
                values.insert(format!("Extra{i}"), value(true));
            }
            assert!(decode_session("/session/c7", &values).is_ok());
            let mut values = session("c7", "wayland", 1000, true, false);
            for i in values.len()..MAX_PROPERTIES {
                values.insert(format!("Extra{i}"), value(true));
            }
            assert_eq!(values.len(), MAX_PROPERTIES);
            assert!(decode_session("/session/c7", &values).is_ok());
            values.insert("Extra".into(), value(true));
            assert_eq!(
                decode_session("/session/c7", &values),
                Err(ProbeIssue::Oversize)
            );
        }
        #[test]
        fn actual_client_preserves_pid_selection_inactive_locked_and_distinct_receipts() {
            let mut own = step(
                "GetSessionByPID",
                "/org/freedesktop/login1",
                Response::Path(path("/session/c7")),
            );
            own.body = Some(4242u32.to_le_bytes().to_vec());
            let mut script = vec![
                own,
                step(
                    "GetAll",
                    "/session/c7",
                    Response::Properties(session("c7", "wayland", 1000, false, true)),
                ),
            ];
            script.extend(
                display_script(&["c7"], vec![session("c7", "wayland", 1000, false, true)])
                    .into_iter()
                    .skip(1),
            );
            let (fact, log) = probe(script, Some("must_not_be_queried"));
            let result = fact.value.unwrap();
            let selected = result
                .selected_session
                .value
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap();
            assert_eq!(selected.selection, SessionSelection::Pid);
            assert_eq!(selected.session.active, Some(false));
            assert_eq!(selected.session.locked_hint, Some(true));
            assert_eq!(result.graphical_sessions.value, Ok(1));
            assert_eq!(result.selected_session.observed_at_ms, 20);
            assert_eq!(result.graphical_sessions.observed_at_ms, 50);
            assert_eq!(fact.observed_at_ms, 60);
            assert_eq!(fact.source, ObservationSource::Demo);
            assert_eq!(result.selected_session.source, ObservationSource::Demo);
            assert_eq!(result.graphical_sessions.source, ObservationSource::Demo);
            assert_eq!(
                log,
                [
                    "Hello",
                    "GetSessionByPID",
                    "GetAll",
                    "GetUser",
                    "GetAll",
                    "GetAll"
                ]
            );
            let delivered_later = result.clone();
            assert_eq!(delivered_later.selected_session.observed_at_ms, 20);
        }
        #[test]
        fn actual_client_uses_exact_absence_fallback_and_display_reads_each_listed_session() {
            for kind in ["wayland", "x11"] {
                let mut script = display_script(
                    &["c7", "tty"],
                    vec![
                        session("c7", kind, 1000, true, false),
                        session("tty", "tty", 1000, true, false),
                    ],
                );
                script.insert(
                    1,
                    step(
                        "GetSession",
                        "/org/freedesktop/login1",
                        Response::Error("org.freedesktop.login1.NoSuchSession"),
                    ),
                );
                let (fact, log) = probe(script, Some("absent"));
                let result = fact.value.unwrap();
                assert_eq!(
                    result.selected_session.value.unwrap().unwrap().selection,
                    SessionSelection::Display
                );
                assert_eq!(result.graphical_sessions.value, Ok(1));
                assert_eq!(
                    log,
                    [
                        "Hello",
                        "GetSessionByPID",
                        "GetSession",
                        "GetUser",
                        "GetAll",
                        "GetAll",
                        "GetAll"
                    ]
                );
            }
            let (fact, log) = probe(
                display_script(&["c7"], vec![session("c7", "wayland", 1000, true, false)]),
                None,
            );
            assert!(
                fact.value
                    .unwrap()
                    .selected_session
                    .value
                    .unwrap()
                    .is_some()
            );
            assert!(!log.iter().any(|s| s == "GetSession"));
        }
        #[test]
        fn actual_client_other_bus_errors_stop_without_any_fallback_or_mutation() {
            for error in [
                "org.freedesktop.DBus.Error.AccessDenied",
                "org.freedesktop.login1.NoSuchSession",
            ] {
                let (fact, log) = probe(
                    vec![step(
                        "GetSessionByPID",
                        "/org/freedesktop/login1",
                        Response::Error(error),
                    )],
                    Some("c7"),
                );
                let facts = fact.value.unwrap();
                assert_eq!(facts.selected_session.value, Err(ProbeIssue::Unavailable));
                assert_eq!(facts.graphical_sessions.value, Err(ProbeIssue::Unavailable));
                assert_eq!(facts.selected_session.observed_at_ms, 10);
                assert_eq!(log, ["Hello", "GetSessionByPID"]);
            }
            let (fact, log) = probe(
                vec![
                    absent(),
                    step(
                        "GetSession",
                        "/org/freedesktop/login1",
                        Response::Error("org.freedesktop.DBus.Error.AccessDenied"),
                    ),
                ],
                Some("c7"),
            );
            assert_eq!(
                fact.value.unwrap().selected_session.value,
                Err(ProbeIssue::Unavailable)
            );
            assert_eq!(log, ["Hello", "GetSessionByPID", "GetSession"]);
        }
        #[test]
        fn actual_client_display_zero_multiple_and_each_unreadable_listed_fact_fail_closed() {
            let (fact, _) = probe(display_script(&[], vec![]), None);
            let result = fact.value.unwrap();
            assert_eq!(result.selected_session.value, Ok(None));
            assert_eq!(result.graphical_sessions.value, Ok(0));
            let (fact, _) = probe(
                display_script(
                    &["c7", "c8"],
                    vec![
                        session("c7", "wayland", 1000, true, false),
                        session("c8", "x11", 1000, false, true),
                    ],
                ),
                None,
            );
            assert_eq!(
                fact.value.unwrap().selected_session.value,
                Err(ProbeIssue::Ambiguous)
            );
            for field in ["Type", "User", "Seat"] {
                let mut unreadable = session("tty", "tty", 2000, true, false);
                unreadable.remove(field);
                let (fact, _) = probe(
                    display_script(
                        &["c7", "tty"],
                        vec![session("c7", "wayland", 1000, true, false), unreadable],
                    ),
                    None,
                );
                let result = fact.value.unwrap();
                assert_eq!(result.selected_session.value, Err(ProbeIssue::Unverified));
                assert_eq!(result.graphical_sessions.value, Err(ProbeIssue::Unverified));
            }
        }
        #[test]
        fn actual_client_oversized_and_wrong_signature_replies_are_explicit() {
            let mut large = session("c7", "wayland", 1000, true, false);
            large.insert("Ignored".into(), value("s".repeat(MAX_PROBE_BYTES)));
            let (fact, _) = probe(
                vec![
                    step(
                        "GetSessionByPID",
                        "/org/freedesktop/login1",
                        Response::Path(path("/session/c7")),
                    ),
                    step("GetAll", "/session/c7", Response::Properties(large)),
                ],
                None,
            );
            assert_eq!(
                fact.value.unwrap().selected_session.value,
                Err(ProbeIssue::Oversize)
            );
            let (fact, _) = probe(
                vec![step(
                    "GetSessionByPID",
                    "/org/freedesktop/login1",
                    Response::Text("wrong signature".into()),
                )],
                None,
            );
            assert_eq!(
                fact.value.unwrap().selected_session.value,
                Err(ProbeIssue::Malformed)
            );
            let (fact, log) = probe(
                vec![step(
                    "GetSessionByPID",
                    "/org/freedesktop/login1",
                    Response::ErrorBody(
                        "org.freedesktop.login1.NoSessionForPID",
                        "s".repeat(MAX_PROBE_BYTES),
                    ),
                )],
                Some("never_queried"),
            );
            assert_eq!(
                fact.value.unwrap().selected_session.value,
                Err(ProbeIssue::Oversize)
            );
            assert_eq!(log, ["Hello", "GetSessionByPID"]);
        }
        #[test]
        fn actual_client_environment_success_follows_own_nongraphical_session() {
            let mut script = vec![
                step(
                    "GetSessionByPID",
                    "/org/freedesktop/login1",
                    Response::Path(path("/session/tty")),
                ),
                step(
                    "GetAll",
                    "/session/tty",
                    Response::Properties(session("tty", "tty", 1000, true, false)),
                ),
                step(
                    "GetSession",
                    "/org/freedesktop/login1",
                    Response::Path(path("/session/c7")),
                ),
                step(
                    "GetAll",
                    "/session/c7",
                    Response::Properties(session("c7", "x11", 1000, false, true)),
                ),
            ];
            script.extend(
                display_script(&["c7"], vec![session("c7", "x11", 1000, false, true)])
                    .into_iter()
                    .skip(1),
            );
            let (fact, log) = probe(script, Some("c7"));
            let result = fact.value.unwrap();
            let chosen = result.selected_session.value.unwrap().unwrap();
            assert_eq!(chosen.selection, SessionSelection::Environment);
            assert_eq!(chosen.session.kind.as_deref(), Some("x11"));
            assert_eq!(chosen.session.active, Some(false));
            assert_eq!(chosen.session.locked_hint, Some(true));
            assert_eq!(result.graphical_sessions.value, Ok(1));
            assert_eq!(
                log,
                [
                    "Hello",
                    "GetSessionByPID",
                    "GetAll",
                    "GetSession",
                    "GetAll",
                    "GetUser",
                    "GetAll",
                    "GetAll"
                ]
            );
        }
        #[test]
        fn worker_slot_is_released_when_caller_clock_unwinds_in_owned_worker() {
            let (stream, peer) = UnixStream::pair().unwrap();
            drop(peer);
            let calls = Arc::new(AtomicU64::new(0));
            let count = calls.clone();
            let fact = logind_from_stream(
                stream,
                1000,
                4242,
                None,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                Arc::new(move || {
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    assert_ne!(n, 0, "test-owned clock unwind");
                    n
                }),
            );
            assert_eq!(fact.value, Err(ProbeIssue::Unavailable));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let (fact, _) = probe(display_script(&[], vec![]), None);
            assert_eq!(fact.value.unwrap().graphical_sessions.value, Ok(0));
        }
        #[test]
        fn owned_stream_authentication_and_method_reads_share_deadline_and_cancel() {
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let started = Instant::now();
            let result = logind_from_stream(
                stream,
                1000,
                4242,
                None,
                &Deadline::new(40, Cancellation::default()).unwrap(),
                Arc::new(|| 77),
            );
            assert_eq!(result.value, Err(ProbeIssue::Timeout));
            assert!(started.elapsed() < Duration::from_millis(300));
            let mut auth = Vec::new();
            peer.read_to_end(&mut auth).unwrap();
            assert!(auth.starts_with(b"\0AUTH"));
            let (stream, server) = Server::new(vec![step(
                "GetSessionByPID",
                "/org/freedesktop/login1",
                Response::Stall,
            )]);
            let result = logind_from_stream(
                stream,
                1000,
                4242,
                Some("must_not_query".into()),
                &Deadline::new(80, Cancellation::default()).unwrap(),
                Arc::new(|| 88),
            );
            assert_eq!(result.value, Err(ProbeIssue::Timeout));
            assert_eq!(*server.log.lock().unwrap(), ["Hello", "GetSessionByPID"]);
            drop(server);
            let cancellation = Cancellation::default();
            cancellation.cancel();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            let result = logind_from_stream(
                stream,
                1000,
                4242,
                None,
                &Deadline::new(100, cancellation).unwrap(),
                Arc::new(|| 99),
            );
            assert_eq!(result.value, Err(ProbeIssue::Cancelled));
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).unwrap(), 0);
        }
        #[test]
        fn worker_capacity_refuses_without_auth_bytes_and_recovers_after_cancel() {
            let cancellation = Cancellation::default();
            let mut jobs = Vec::new();
            let mut peers = Vec::new();
            for _ in 0..4 {
                let (stream, mut peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let token = cancellation.clone();
                jobs.push(thread::spawn(move || {
                    logind_from_stream(
                        stream,
                        1000,
                        4242,
                        None,
                        &Deadline::new(1500, token).unwrap(),
                        Arc::new(|| 1),
                    )
                }));
                assert!(read_line(&mut peer).unwrap().starts_with(b"\0AUTH"));
                peers.push(peer);
            }
            let (stream, mut fifth) = UnixStream::pair().unwrap();
            let result = logind_from_stream(
                stream,
                1000,
                4242,
                None,
                &Deadline::new(500, Cancellation::default()).unwrap(),
                Arc::new(|| 2),
            );
            assert_eq!(result.value, Err(ProbeIssue::Unavailable));
            let mut byte = [0];
            assert_eq!(fifth.read(&mut byte).unwrap(), 0);
            cancellation.cancel();
            for job in jobs {
                assert_eq!(job.join().unwrap().value, Err(ProbeIssue::Cancelled));
            }
            for peer in peers {
                let _ = peer.shutdown(Shutdown::Both);
            }
            let limit = Instant::now() + Duration::from_secs(1);
            loop {
                let (stream, server) = Server::new(display_script(&[], vec![]));
                let fact = logind_from_stream(
                    stream,
                    1000,
                    4242,
                    None,
                    &Deadline::new(500, Cancellation::default()).unwrap(),
                    Arc::new(|| 3),
                );
                if fact.value == Err(ProbeIssue::Unavailable)
                    && server.log.lock().unwrap().is_empty()
                {
                    drop(server);
                    assert!(Instant::now() < limit);
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                assert_eq!(fact.value.unwrap().graphical_sessions.value, Ok(0));
                assert_eq!(
                    server.finish(),
                    ["Hello", "GetSessionByPID", "GetUser", "GetAll"]
                );
                break;
            }
        }
        #[test]
        fn cancellation_during_authentication_closes_only_owned_stream() {
            let cancellation = Cancellation::default();
            let token = cancellation.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            let job = thread::spawn(move || {
                logind_from_stream(
                    stream,
                    1000,
                    4242,
                    None,
                    &Deadline::new(1000, token).unwrap(),
                    Arc::new(|| 123),
                )
            });
            assert!(read_line(&mut peer).unwrap().starts_with(b"\0AUTH"));
            cancellation.cancel();
            assert_eq!(job.join().unwrap().value, Err(ProbeIssue::Cancelled));
            let mut byte = [0];
            assert_eq!(peer.read(&mut byte).unwrap(), 0);
            let (mut unrelated, mut unrelated_peer) = UnixStream::pair().unwrap();
            unrelated.write_all(b"owned independent stream").unwrap();
            let mut bytes = [0; 24];
            unrelated_peer.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"owned independent stream");
        }
    }
    use crosspane_installer::agent_contract::*;
    use crosspane_installer::platform::linux::detect::*;
    use serde_json::{Value, json};
    use std::path::PathBuf;

    fn known<T>(value: T) -> Fact<T> {
        Fact::known(value, ObservationSource::Demo, 17)
    }
    fn candidate(kind: &str) -> SessionCandidate {
        SessionCandidate {
            id: "c7".into(),
            path: "/org/freedesktop/login1/session/c7".into(),
            kind: Some(kind.into()),
            uid: Some(1000),
            seat: Some("seat0".into()),
            active: Some(true),
            locked_hint: Some(false),
        }
    }
    fn environment() -> EffectiveEnvironment {
        EffectiveEnvironment {
            runtime_dir: "/run/user/1000".into(),
            wayland_display: "wayland-2".into(),
            hyprland_instance_signature: "test_1790950000".into(),
            session_id: None,
            xdg_current_desktop: None,
            xdg_session_type: None,
        }
    }
    fn session() -> SessionFacts {
        SessionFacts {
            uid: 1000,
            os: known(OsFamily::Arch),
            architecture: known(Architecture::X86_64),
            desktop: Ok(Desktop::Hyprland),
            compositor_version: known([0, 56, 0]),
            protocols: known(true),
            compositor_managed: known(true),
            graphical_target_active: known(true),
            graphical_sessions: known(1),
            selected_session: known(Some(SelectedSession {
                selection: SessionSelection::Pid,
                session: candidate("wayland"),
            })),
            selected_environment: environment(),
            manager_environment: known(environment()),
        }
    }
    fn runtime() -> RuntimeFacts {
        RuntimeFacts {
            dependency_graph: known(true),
            libraries: vec![LibraryFact {
                name: "libavcodec.so.62".into(),
                required: true,
                resolved: known(PathBuf::from("/usr/lib/libavcodec.so.62")),
            }],
            video_feature: known(true),
            ffmpeg: known(true),
            opus: known(true),
            pipewire_library: known(true),
            xkb: known(true),
            wayland_library: known(true),
            software_video: known(true),
            gpu: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 18),
            libei_required: false,
            pipewire: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 19),
            session_manager: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 20),
            secret_service: Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 21),
            keystore: known(KeyStoreProvenance::OsStore),
        }
    }
    struct Lookup {
        own: Result<Option<SessionCandidate>, ProbeIssue>,
        named: Result<Option<SessionCandidate>, ProbeIssue>,
        display: Result<DisplaySessions, ProbeIssue>,
        calls: Vec<String>,
    }
    impl Default for Lookup {
        fn default() -> Self {
            Self {
                own: Ok(None),
                named: Ok(None),
                display: Ok(DisplaySessions {
                    display: "/".into(),
                    sessions: vec![],
                }),
                calls: vec![],
            }
        }
    }
    impl SessionLookup for Lookup {
        fn own_session(&mut self) -> Result<Option<SessionCandidate>, ProbeIssue> {
            self.calls.push("own".into());
            self.own.clone()
        }
        fn named_session(&mut self, id: &str) -> Result<Option<SessionCandidate>, ProbeIssue> {
            self.calls.push(format!("named:{id}"));
            self.named.clone()
        }
        fn display_sessions(&mut self, uid: u32) -> Result<DisplaySessions, ProbeIssue> {
            self.calls.push(format!("display:{uid}"));
            self.display.clone()
        }
    }
    fn display_reader(sessions: Vec<SessionCandidate>) -> Lookup {
        Lookup {
            display: Ok(DisplaySessions {
                display: candidate("wayland").path,
                sessions,
            }),
            ..Lookup::default()
        }
    }
    fn bus_error(name: &str) -> zbus::Error {
        let call = zbus::Message::method_call("/org/freedesktop/login1", "GetSessionByPID")
            .unwrap()
            .build(&(4242u32,))
            .unwrap();
        let reply = zbus::Message::error(&call.header(), name)
            .unwrap()
            .build(&"test error")
            .unwrap();
        zbus::Error::MethodError(name.try_into().unwrap(), Some("test error".into()), reply)
    }

    #[test]
    fn facts_preserve_original_source_receipt_and_all_bounded_issues() {
        for issue in [
            ProbeIssue::Missing,
            ProbeIssue::WrongVersion,
            ProbeIssue::Unavailable,
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Oversize,
            ProbeIssue::Malformed,
            ProbeIssue::Foreign,
            ProbeIssue::Ambiguous,
            ProbeIssue::Unverified,
        ] {
            let fact = Fact::<bool>::issue(issue, ObservationSource::Live, u64::MAX);
            assert_eq!(fact.value, Err(issue));
            assert_eq!(fact.source, ObservationSource::Live);
            assert_eq!(fact.observed_at_ms, u64::MAX);
        }
        assert_eq!(known(false).value, Ok(false));
    }

    #[test]
    fn os_release_arch_family_is_token_exact_and_other_os_is_known() {
        for bytes in [
            b"ID=arch\n".as_slice(),
            b"ID='endeavouros'\nID_LIKE=\"arch linux\"\n",
            b"# producer comment\nID=manjaro\nID_LIKE=arch\n",
        ] {
            assert_eq!(parse_os_release(bytes), Ok(OsFamily::Arch));
        }
        for bytes in [
            b"ID=debian\nID_LIKE=archlinux\n".as_slice(),
            b"ID=debian\nID_LIKE=debian\n",
        ] {
            assert_eq!(
                parse_os_release(bytes),
                Ok(OsFamily::Other("debian".into()))
            );
        }
    }

    #[test]
    fn os_release_malformed_duplicate_oversize_and_utf8_never_default_arch() {
        for bytes in [
            b"".as_slice(),
            b"ID=arch\nID=arch",
            b"ID=\"arch",
            b"ID=",
            b"ID=a/b",
            b"ID=\xff",
        ] {
            assert_eq!(
                parse_os_release(bytes),
                Err(ProbeIssue::Malformed),
                "{bytes:?}"
            );
        }
        assert_eq!(
            parse_os_release(&vec![b'a'; MAX_PROBE_BYTES + 1]),
            Err(ProbeIssue::Oversize)
        );
        assert!(parse_os_release(format!("ID={}\n", "a".repeat(65)).as_bytes()).is_err());
        assert_eq!(
            parse_os_release(format!("ID=arch\nX={}\n", "a".repeat(4097)).as_bytes()),
            Ok(OsFamily::Arch)
        );
        assert_eq!(
            parse_os_release(format!("ID=arch\n{}=x\n", "A".repeat(129)).as_bytes()),
            Ok(OsFamily::Arch)
        );
        let many = format!(
            "ID=arch\n{}",
            (0..256).map(|i| format!("X{i}=y\n")).collect::<String>()
        );
        assert_eq!(parse_os_release(many.as_bytes()), Ok(OsFamily::Arch));
    }

    #[test]
    fn os_release_rejects_broken_consumed_identifiers_and_ignores_unrelated_assignments() {
        for assignment in [
            r#"ID_LIKE="arch "broken""#,
            r#"ID_LIKE="arch "broken"""#,
            r#"ID_LIKE='arch 'broken'"#,
            r#"ID_LIKE="arch""linux""#,
            r#"ID_LIKE=arch"broken"#,
            r#"ID_LIKE="arch"suffix"#,
            r#"NAME="Arch "broken""#,
            r#"NAME='Arch"#,
            r#"NAME=Arch\"#,
            r#"NAME=$OS"#,
            r#"NAME="${OS}""#,
            "NAME=`command`",
            "NAME=two words",
            "NAME=Arch;command",
            "1NAME=Arch",
            "NAME=Arch\u{b}",
        ] {
            let bytes = format!("ID=arch\n{assignment}\n");
            let malformed = assignment.starts_with("ID_LIKE=");
            assert_eq!(
                parse_os_release(bytes.as_bytes()),
                if malformed {
                    Err(ProbeIssue::Malformed)
                } else {
                    Ok(OsFamily::Arch)
                },
                "{assignment}"
            );
            let mut s = session();
            s.os = Fact {
                value: parse_os_release(bytes.as_bytes()),
                source: ObservationSource::Demo,
                observed_at_ms: 17,
            };
            assert_eq!(
                classify(&s, &runtime()),
                if malformed {
                    Eligibility::Pending(ProbeIssue::Malformed)
                } else {
                    Eligibility::Supported
                }
            );
        }
        for key in ["ID", "ID_LIKE"] {
            for invalid in [
                "ARCH",
                "arch/other",
                "arch+other",
                "árch",
                "arch$",
                "arch\"",
                "a;b",
                &"a".repeat(65),
            ] {
                let bytes = if key == "ID" {
                    format!("ID='{invalid}'\nID_LIKE=arch\n")
                } else {
                    format!("ID=arch\nID_LIKE='arch {invalid}'\n")
                };
                assert_eq!(
                    parse_os_release(bytes.as_bytes()),
                    Err(ProbeIssue::Malformed),
                    "{key}: {invalid}"
                );
            }
        }
    }

    #[test]
    fn os_release_shell_escapes_and_quotes_decode_without_expansion_or_concatenation() {
        for bytes in [
            r#"ID=\a\r\c\h
NAME="Arch \"Linux\" \$OS \`literal\` \\path \q"
"#,
            r#"ID='arch'
NAME='Owner\path $OS `literal` "quoted"'
"#,
            "ID=arch\nID_LIKE=\"linux arch\"\nNAME=\"Αrch Linux; example\"\n",
            "ID=arch\nID_LIKE=\"\"\nNAME=''\n",
        ] {
            assert_eq!(
                parse_os_release(bytes.as_bytes()),
                Ok(OsFamily::Arch),
                "{bytes}"
            );
        }
        assert_eq!(
            parse_os_release(b"ID=vendor.os_1-2\nID_LIKE='linux other.os_2-3'\n"),
            Ok(OsFamily::Other("vendor.os_1-2".into()))
        );
        // Escapes that shell double quotes do not consume remain literal, hence invalid IDs.
        for bytes in [
            br#"ID="\arch""#.as_slice(),
            br#"ID='\arch'"#,
            br#"ID=debian
ID_LIKE="arch \linux""#,
        ] {
            assert_eq!(parse_os_release(bytes), Err(ProbeIssue::Malformed));
        }
        let escaped = MANAGER.replace("wayland-2", r#""wayland\-2""#);
        assert_eq!(
            parse_manager_environment(escaped.as_bytes()),
            Err(ProbeIssue::Malformed)
        );
        let escaped = MANAGER.replace("wayland-2", r#"wayland\-2"#);
        assert_eq!(
            parse_manager_environment(escaped.as_bytes()),
            Ok(environment())
        );
    }

    #[test]
    fn shell_word_decoding_preserves_exact_literals_and_double_quote_backslashes() {
        for (encoded, decoded) in [
            (r#"/run/\$x\`y\`\\path"#, r#"/run/$x`y`\path"#),
            (
                r#""/run/\$x\`y\`\\back\"quote'\q""#,
                r#"/run/$x`y`\back"quote'\q"#,
            ),
            (r#"'/run/$x`y`"quote"\back'"#, r#"/run/$x`y`"quote"\back"#),
        ] {
            let environment =
                parse_manager_environment(MANAGER.replace("/run/user/1000", encoded).as_bytes())
                    .unwrap();
            assert_eq!(environment.runtime_dir, PathBuf::from(decoded), "{encoded}");
        }
    }

    #[test]
    fn architectures_are_explicit_not_host_guesses() {
        assert_eq!(parse_architecture("x86_64"), Ok(Architecture::X86_64));
        assert_eq!(parse_architecture("aarch64"), Ok(Architecture::Aarch64));
        assert_eq!(
            parse_architecture("riscv64"),
            Ok(Architecture::Other("riscv64".into()))
        );
        for value in ["", "a\nb", &"x".repeat(33)] {
            assert_eq!(parse_architecture(value), Err(ProbeIssue::Malformed));
        }
    }

    const MANAGER: &str = "XDG_RUNTIME_DIR=/run/user/1000\nWAYLAND_DISPLAY=wayland-2\nHYPRLAND_INSTANCE_SIGNATURE=test_1790950000\n";
    #[test]
    fn manager_environment_returns_only_selected_fields_without_repair() {
        assert_eq!(
            parse_manager_environment(MANAGER.as_bytes()),
            Ok(environment())
        );
        let bytes = format!(
            "{MANAGER}XDG_SESSION_ID='c7'\nSECRET=sentinel-secret\nDBUS_SESSION_BUS_ADDRESS=sentinel-address\n"
        );
        let mut expected = environment();
        expected.session_id = Some("c7".into());
        let decoded = parse_manager_environment(bytes.as_bytes()).unwrap();
        assert_eq!(decoded, expected);
        assert!(!format!("{decoded:?}").contains("sentinel"));
        assert_eq!(
            parse_manager_environment(
                MANAGER
                    .replace("/run/user/1000", "\"/run/user/1000\"")
                    .as_bytes()
            ),
            Ok(environment())
        );
    }

    // Line shapes from `systemctl --user show-environment` on the owner's Omarchy desktop
    // (systemd 261): values with spaces, `;`, `*` and trailing blanks print as `$'…'`.
    const REAL_SHAPES: &str = "DEBUGINFOD_URLS=$'https://debuginfod.archlinux.org '\nEDITOR=$'omarchy-launch-editor --inline'\nGDK_BACKEND=$'wayland,x11,*'\nHYPRLAND_CMD=$'Hyprland --watchdog-fd 4'\nHOME=/home/owner\nPATH=/usr/local/bin:/usr/bin\nQT_QPA_PLATFORM=$'wayland;xcb'\nUWSM_FINALIZE_VARNAMES=$'HYPRLAND_INSTANCE_SIGNATURE HYPRLAND_CMD HYPRCURSOR_THEME'\nXCURSOR_SIZE=24\n";

    #[test]
    fn manager_environment_accepts_real_systemd_quoting_and_skips_unrelated_lines() {
        let real = format!("{REAL_SHAPES}{MANAGER}XDG_SESSION_ID=2\n");
        let mut expected = environment();
        expected.session_id = Some("2".into());
        assert_eq!(parse_manager_environment(real.as_bytes()), Ok(expected));
        // systemd quotes required values too; decode its C escapes exactly.
        for (encoded, decoded) in [
            ("$'/run/user/1000'", "/run/user/1000"),
            (r"$'/run/a b\'c\\d\x41\x2a'", r"/run/a b'c\dA*"),
        ] {
            let bytes = MANAGER.replace("/run/user/1000", encoded);
            let environment = parse_manager_environment(bytes.as_bytes()).unwrap();
            assert_eq!(environment.runtime_dir, PathBuf::from(decoded), "{encoded}");
        }
        // Unrelated variables that can't be decoded are skipped, never returned or fatal.
        for unrelated in [
            "BROKEN=$'unterminated\n",
            "BROKEN=$'bad\\q escape'\n",
            "BROKEN=$'bell\\a'\n",
            "BROKEN=$'\\xff'\n",
            "BROKEN=a b\n",
            "BROKEN=$(command)\n",
            "BROKEN=tab\there\n",
            "not a key=value\n",
            "NO_EQUALS\n",
            "9BAD=value\n",
        ] {
            let bytes = format!("{unrelated}{MANAGER}");
            assert_eq!(
                parse_manager_environment(bytes.as_bytes()),
                Ok(environment()),
                "{unrelated:?}"
            );
            let mut invalid = format!("{unrelated}{MANAGER}").into_bytes();
            invalid.extend_from_slice(b"OTHER=\xff\xfe\n");
            assert_eq!(parse_manager_environment(&invalid), Ok(environment()));
        }
        // The four variables detection reads stay strict, in either quoting.
        for (key, original) in [
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("WAYLAND_DISPLAY", "wayland-2"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "test_1790950000"),
        ] {
            for bad in [
                "$'unterminated",
                "$'early'end'",
                "$'bad\\q'",
                "$'nl\\n'",
                "$'\\xff'",
                "$'\\x4'",
                "a b",
            ] {
                let bytes = MANAGER.replace(&format!("{key}={original}"), &format!("{key}={bad}"));
                assert_eq!(
                    parse_manager_environment(bytes.as_bytes()),
                    Err(ProbeIssue::Malformed),
                    "{key}={bad}"
                );
            }
        }
        for bad in ["$'unterminated", "$'c\\t7'", "$''"] {
            assert_eq!(
                parse_manager_environment(format!("{MANAGER}XDG_SESSION_ID={bad}\n").as_bytes()),
                Err(ProbeIssue::Malformed),
                "{bad}"
            );
        }
        // Line count stays bounded; the byte bound still applies first.
        let filler = (0..MAX_ENVIRONMENT_LINES)
            .map(|i| format!("V{i}=x\n"))
            .collect::<String>();
        assert_eq!(
            parse_manager_environment(format!("{MANAGER}{filler}").as_bytes()),
            Err(ProbeIssue::Oversize)
        );
        let filler = (0..MAX_ENVIRONMENT_LINES - 3)
            .map(|i| format!("V{i}=x\n"))
            .collect::<String>();
        assert!(parse_manager_environment(format!("{MANAGER}{filler}").as_bytes()).is_ok());
    }

    #[test]
    fn manager_missing_invalid_alias_duplicate_and_bounds_are_pending_facts() {
        for key in [
            "XDG_RUNTIME_DIR",
            "WAYLAND_DISPLAY",
            "HYPRLAND_INSTANCE_SIGNATURE",
        ] {
            let bytes = MANAGER
                .lines()
                .filter(|line| !line.starts_with(key))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                parse_manager_environment(bytes.as_bytes()),
                Err(ProbeIssue::Missing)
            );
        }
        for path in [
            "relative",
            "/run//user/1000",
            "/run/./1000",
            "/run/../1000",
            "/run/user/1000/",
        ] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("/run/user/1000", path).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for name in ["", ".", "..", "a/b", "a b", &"a".repeat(129)] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("wayland-2", name).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for signature in ["", ".", "..", "../other", &"a".repeat(257)] {
            assert_eq!(
                parse_manager_environment(MANAGER.replace("test_1790950000", signature).as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        for extra in [
            "WAYLAND_DISPLAY=wayland-2\n",
            "XDG_SESSION_ID=\n",
            &format!("XDG_SESSION_ID={}\n", "x".repeat(65)),
        ] {
            assert_eq!(
                parse_manager_environment(format!("{MANAGER}{extra}").as_bytes()),
                Err(ProbeIssue::Malformed)
            );
        }
        assert_eq!(
            parse_manager_environment(&vec![b'a'; MAX_PROBE_BYTES + 1]),
            Err(ProbeIssue::Oversize)
        );
    }

    #[test]
    fn own_pid_graphical_selection_wins_even_inactive_locked_or_x11() {
        for kind in ["wayland", "x11"] {
            let mut own = candidate(kind);
            own.active = Some(false);
            own.locked_hint = Some(true);
            let mut reader = Lookup {
                own: Ok(Some(own.clone())),
                named: Err(ProbeIssue::Foreign),
                ..Lookup::default()
            };
            assert_eq!(
                choose_session(&mut reader, 1000, Some("other")),
                Ok(Some(SelectedSession {
                    selection: SessionSelection::Pid,
                    session: own
                }))
            );
            assert_eq!(reader.calls, ["own"]);
        }
    }

    #[test]
    fn environment_fallback_is_ordered_and_never_compositor_pid_membership() {
        for own in [
            None,
            Some(candidate("tty")),
            Some(SessionCandidate {
                uid: Some(1001),
                ..candidate("wayland")
            }),
        ] {
            let named = candidate("wayland");
            let mut reader = Lookup {
                own: Ok(own),
                named: Ok(Some(named.clone())),
                display: Err(ProbeIssue::Foreign),
                calls: vec![],
            };
            assert_eq!(
                choose_session(&mut reader, 1000, Some("c7")),
                Ok(Some(SelectedSession {
                    selection: SessionSelection::Environment,
                    session: named
                }))
            );
            assert_eq!(reader.calls, ["own", "named:c7"]);
        }
    }

    #[test]
    fn absent_or_empty_environment_uses_only_sole_seated_display_candidate() {
        for id in [None, Some("")] {
            for kind in ["wayland", "x11"] {
                let mut displayed = candidate(kind);
                displayed.active = Some(false);
                let mut reader = display_reader(vec![displayed.clone()]);
                assert_eq!(
                    choose_session(&mut reader, 1000, id),
                    Ok(Some(SelectedSession {
                        selection: SessionSelection::Display,
                        session: displayed
                    }))
                );
                assert_eq!(reader.calls, ["own", "display:1000"]);
            }
        }
        let mut reader = display_reader(vec![candidate("wayland")]);
        assert!(
            choose_session(&mut reader, 1000, Some("c7"))
                .unwrap()
                .is_some()
        );
        assert_eq!(reader.calls, ["own", "named:c7", "display:1000"]);
    }

    #[test]
    fn zero_multiple_foreign_unseated_or_wrong_display_never_choose_active_guess() {
        for sessions in [
            vec![],
            vec![candidate("tty")],
            vec![SessionCandidate {
                uid: Some(1001),
                ..candidate("wayland")
            }],
            vec![SessionCandidate {
                seat: Some("".into()),
                ..candidate("wayland")
            }],
        ] {
            let mut reader = display_reader(sessions);
            assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
        }
        let mut second = candidate("x11");
        second.id = "c8".into();
        second.path = "/org/freedesktop/login1/session/c8".into();
        let mut reader = display_reader(vec![candidate("wayland"), second]);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Ambiguous)
        );
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "/other".into();
        assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "/".into();
        assert_eq!(choose_session(&mut reader, 1000, None), Ok(None));
    }

    #[test]
    fn any_listed_unreadable_type_user_or_seat_blocks_display_even_for_other_session() {
        for field in ["type", "user", "seat"] {
            let mut other = candidate("tty");
            other.uid = Some(1001);
            match field {
                "type" => other.kind = None,
                "user" => other.uid = None,
                "seat" => other.seat = None,
                _ => unreachable!(),
            }
            let mut reader = display_reader(vec![candidate("wayland"), other]);
            assert_eq!(
                choose_session(&mut reader, 1000, None),
                Err(ProbeIssue::Unverified),
                "{field}"
            );
        }
    }

    #[test]
    fn errors_stop_at_exact_step_and_selection_bounds_do_not_probe_further() {
        for issue in [
            ProbeIssue::Unavailable,
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Malformed,
        ] {
            let mut reader = Lookup {
                own: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, Some("c7")), Err(issue));
            assert_eq!(reader.calls, ["own"]);
            let mut reader = Lookup {
                named: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, Some("c7")), Err(issue));
            assert_eq!(reader.calls, ["own", "named:c7"]);
            let mut reader = Lookup {
                display: Err(issue),
                ..Lookup::default()
            };
            assert_eq!(choose_session(&mut reader, 1000, None), Err(issue));
            assert_eq!(reader.calls, ["own", "display:1000"]);
        }
        for id in ["bad\nname", &"x".repeat(65)] {
            let mut reader = Lookup::default();
            assert_eq!(
                choose_session(&mut reader, 1000, Some(id)),
                Err(ProbeIssue::Malformed)
            );
            assert_eq!(reader.calls, ["own"]);
        }
        let mut reader = display_reader(vec![candidate("tty"); MAX_SESSIONS + 1]);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Oversize)
        );
        let mut reader = display_reader(vec![candidate("wayland")]);
        reader.display.as_mut().unwrap().display = "x".repeat(513);
        assert_eq!(
            choose_session(&mut reader, 1000, None),
            Err(ProbeIssue::Malformed)
        );
    }

    #[test]
    fn only_exact_logind_absence_method_errors_permit_fallback() {
        let no_pid = "org.freedesktop.login1.NoSessionForPID";
        let no_named = "org.freedesktop.login1.NoSuchSession";
        assert_eq!(
            lookup_reply(Err(bus_error(no_pid)), SessionSelection::Pid),
            Ok(None)
        );
        assert_eq!(
            lookup_reply(Err(bus_error(no_named)), SessionSelection::Environment),
            Ok(None)
        );
        for (name, step) in [
            (no_pid, SessionSelection::Environment),
            (no_named, SessionSelection::Pid),
            (no_pid, SessionSelection::Display),
            (
                "org.freedesktop.DBus.Error.AccessDenied",
                SessionSelection::Pid,
            ),
            (
                "org.freedesktop.login1.NoSessionForPID.extra",
                SessionSelection::Pid,
            ),
        ] {
            assert_eq!(
                lookup_reply(Err(bus_error(name)), step),
                Err(ProbeIssue::Unavailable)
            );
        }
        assert_eq!(
            lookup_reply(
                Err(zbus::Error::Failure("unavailable".into())),
                SessionSelection::Pid
            ),
            Err(ProbeIssue::Unavailable)
        );
        let path = "/org/freedesktop/login1/session/c7";
        assert_eq!(
            lookup_reply(Ok(path.try_into().unwrap()), SessionSelection::Pid),
            Ok(Some(path.into()))
        );
    }

    #[test]
    fn supported_structural_report_ignores_optional_gpu_audio_store_availability() {
        let s = session();
        let mut r = runtime();
        assert_eq!(classify(&s, &r), Eligibility::Supported);
        r.gpu = known(false);
        r.pipewire = known(false);
        r.session_manager = known(false);
        r.secret_service = known(false);
        r.keystore = known(KeyStoreProvenance::File);
        assert_eq!(classify(&s, &r), Eligibility::Supported);
        let report = SupportReport {
            eligibility: classify(&s, &r),
            session: s,
            runtime: r,
            installed_agent: Fact::issue(ProbeIssue::Missing, ObservationSource::Demo, 44),
            reduced_motion: Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 45),
        };
        assert_eq!(report.runtime.keystore.value, Ok(KeyStoreProvenance::File));
        assert_eq!(report.reduced_motion.value, Err(ProbeIssue::Unverified));
        assert_eq!(report.installed_agent.value, Err(ProbeIssue::Missing));
    }

    #[test]
    fn each_known_support_mismatch_has_its_exact_reason_and_unknown_stays_pending() {
        for (field, reason) in [
            ("os", UnsupportedReason::OperatingSystem),
            ("arch", UnsupportedReason::Architecture),
            ("version", UnsupportedReason::HyprlandVersion),
            ("protocol", UnsupportedReason::RequiredProtocols),
            ("uwsm", UnsupportedReason::Uwsm),
        ] {
            let mut s = session();
            match field {
                "os" => s.os = known(OsFamily::Other("debian".into())),
                "arch" => s.architecture = known(Architecture::Other("riscv64".into())),
                "version" => s.compositor_version = known([0, 55, 99]),
                "protocol" => s.protocols = known(false),
                "uwsm" => s.compositor_managed = known(false),
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::NotSupported(reason),
                "{field}"
            );
            match field {
                "os" => s.os = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1),
                "arch" => {
                    s.architecture =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "version" => {
                    s.compositor_version =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "protocol" => {
                    s.protocols = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                "uwsm" => {
                    s.compositor_managed =
                        Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 1)
                }
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unavailable),
                "{field}"
            );
        }
        for arch in [Architecture::X86_64, Architecture::Aarch64] {
            let mut s = session();
            s.architecture = known(arch);
            for version in [[0, 56, 0], [0, 57, 0], [1, 0, 0]] {
                s.compositor_version = known(version);
                assert_eq!(classify(&s, &runtime()), Eligibility::Supported);
            }
        }
    }

    #[test]
    fn target_and_environment_alone_never_prove_uwsm_and_known_negative_precedes_pending() {
        let mut s = session();
        s.compositor_managed = Fact::issue(ProbeIssue::Unverified, ObservationSource::Demo, 42);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
        s.os = Fact::issue(ProbeIssue::Timeout, ObservationSource::Demo, 43);
        s.protocols = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::RequiredProtocols)
        );
        s.protocols = known(true);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Timeout)
        );
    }

    #[test]
    fn every_required_runtime_candidate_is_distinct_from_optional_or_unknown() {
        for field in [
            "video", "ffmpeg", "opus", "pipewire", "xkb", "wayland", "software",
        ] {
            let mut r = runtime();
            let fact = match field {
                "video" => &mut r.video_feature,
                "ffmpeg" => &mut r.ffmpeg,
                "opus" => &mut r.opus,
                "pipewire" => &mut r.pipewire_library,
                "xkb" => &mut r.xkb,
                "wayland" => &mut r.wayland_library,
                "software" => &mut r.software_video,
                _ => unreachable!(),
            };
            *fact = known(false);
            assert_eq!(
                classify(&session(), &r),
                Eligibility::NotSupported(if field == "video" {
                    UnsupportedReason::VideoFeature
                } else {
                    UnsupportedReason::RuntimeLibrary
                }),
                "{field}"
            );
            let fact = match field {
                "video" => &mut r.video_feature,
                "ffmpeg" => &mut r.ffmpeg,
                "opus" => &mut r.opus,
                "pipewire" => &mut r.pipewire_library,
                "xkb" => &mut r.xkb,
                "wayland" => &mut r.wayland_library,
                "software" => &mut r.software_video,
                _ => unreachable!(),
            };
            *fact = Fact::issue(ProbeIssue::Unavailable, ObservationSource::Demo, 46);
            assert_eq!(
                classify(&session(), &r),
                Eligibility::Pending(ProbeIssue::Unavailable),
                "{field}"
            );
        }
        let mut r = runtime();
        r.libei_required = true;
        assert_eq!(
            classify(&session(), &r),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
    }

    #[test]
    fn unavailable_library_is_not_fabricated_missing_or_wrong_version() {
        for issue in [
            ProbeIssue::Missing,
            ProbeIssue::WrongVersion,
            ProbeIssue::Unavailable,
            ProbeIssue::Malformed,
            ProbeIssue::Timeout,
        ] {
            let mut r = runtime();
            r.libraries[0].resolved = Fact::issue(issue, ObservationSource::Demo, 41);
            assert_eq!(
                classify(&session(), &r),
                if matches!(issue, ProbeIssue::Missing | ProbeIssue::WrongVersion) {
                    Eligibility::NotSupported(UnsupportedReason::RuntimeLibrary)
                } else {
                    Eligibility::Pending(issue)
                }
            );
            r.libraries[0].required = false;
            assert_eq!(
                classify(&session(), &r),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
            r.libraries.push(runtime().libraries.remove(0));
            assert_eq!(classify(&session(), &r), Eligibility::Supported);
        }
    }

    #[test]
    fn producer_selection_and_mutation_eligibility_remain_separate() {
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .kind = Some("x11".into());
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::NotSupported(UnsupportedReason::SessionType)
        );
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .kind = None;
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
        for active in [Some(false), None] {
            let mut s = session();
            s.selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .active = active;
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
        }
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .locked_hint = Some(true);
        assert_eq!(classify(&s, &runtime()), Eligibility::Supported); // lock is readiness/gate, not support
        for seat in [None, Some("".into())] {
            let mut s = session();
            s.selected_session
                .value
                .as_mut()
                .unwrap()
                .as_mut()
                .unwrap()
                .session
                .seat = seat;
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Unverified)
            );
        }
        let mut s = session();
        s.selected_session
            .value
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .session
            .uid = Some(1001);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Foreign)
        );
        s.selected_session = known(None);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Ambiguous)
        );
    }

    #[test]
    fn manager_runtime_display_signature_mismatch_and_ambiguous_or_unknown_lifecycle_refuse() {
        for field in ["runtime", "display", "signature"] {
            let mut s = session();
            let manager = s.manager_environment.value.as_mut().unwrap();
            match field {
                "runtime" => manager.runtime_dir = "/run/user/1001".into(),
                "display" => manager.wayland_display = "wayland-other".into(),
                "signature" => manager.hyprland_instance_signature = "other".into(),
                _ => unreachable!(),
            }
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Foreign),
                "{field}"
            );
        }
        for count in [0, 2, 3] {
            let mut s = session();
            s.graphical_sessions = known(count);
            assert_eq!(
                classify(&s, &runtime()),
                Eligibility::Pending(ProbeIssue::Ambiguous)
            );
        }
        for issue in [
            ProbeIssue::Timeout,
            ProbeIssue::Cancelled,
            ProbeIssue::Unavailable,
            ProbeIssue::Malformed,
        ] {
            let mut s = session();
            s.manager_environment = Fact::issue(issue, ObservationSource::Demo, 2);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
            let mut s = session();
            s.graphical_target_active = Fact::issue(issue, ObservationSource::Demo, 3);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
            let mut s = session();
            s.graphical_sessions = Fact::issue(issue, ObservationSource::Demo, 4);
            assert_eq!(classify(&s, &runtime()), Eligibility::Pending(issue));
        }
        let mut s = session();
        s.graphical_target_active = known(false);
        assert_eq!(
            classify(&s, &runtime()),
            Eligibility::Pending(ProbeIssue::Unverified)
        );
    }

    const BOOTSTRAP: &[u8] = br#"{"schema_version":1,"instance_id":18446744073709551615,"pid":4242,
      "started_unix_ms":1790950000000,"phase":"waiting_for_keystore","phase_seq":2,
      "keystore":null,"reason":null,"runtime_dir":"/run/user/1000/crosspane"}"#;
    const STATUS: &[u8] = br#"{"ok":true,"result":{
      "controlling":null,"controlled_by":null,"projections":[],"displays":[],"peers":[],"layout":[],
      "installer":{"schema_version":1,"build":{"version":"0.0.0","features":["video"]},
      "instance":{"id":18446744073709551615,"pid":4242,"uid":1000,"exe":"/home/u/.local/bin/crosspane-agent",
      "runtime_dir":"/run/user/1000/crosspane","started_unix_ms":1790950000000},
      "config_revision":"9f86d081884c7d65","node":"1111111111111111111111111111111111111111111111111111111111111111",
      "recovery_pending":0,"startup_recovery":"failed",
      "gate":{"open":false,"session":"unknown","active":null,"armed":false,"panic":true},
      "epochs":{"gate":7,"grants":3,"layout":2,"backends":1},"backends":[
      {"name":"capture","state":"ready","reason":null},{"name":"keys","state":"ready","reason":null},
      {"name":"pointer","state":"ready","reason":null},{"name":"overlay","state":"ready","reason":null},
      {"name":"hotkeys","state":"ready","reason":null},{"name":"keystore","state":"ready","reason":null},
      {"name":"windows","state":"ready","reason":null},{"name":"parking","state":"ready","reason":null},
      {"name":"frames","state":"ready","reason":null},{"name":"tray","state":"ready","reason":null},
      {"name":"links","state":"ready","reason":null},{"name":"gpu","state":"missing","reason":"disabled"},
      {"name":"home","state":"blocked","reason":"not_supported"},{"name":"audio","state":"failed","reason":"worker_exited"},
      {"name":"discovery","state":"ready","reason":null}],"keystore":"file","permissions":[],
      "discovery":{"enabled":true,"running":false,"candidates":0,"error":null},"tray":{"created":false},
      "audio":{"enabled":false,"active_peers":[],"frames_sent":0,"frames_played":0},"settings_opened":0,"peers":[]}}}"#;
    fn agent_facts(
        status: &[u8],
        source: ObservationSource,
        time: u64,
    ) -> Result<InstalledAgentFacts, ProbeIssue> {
        InstalledAgentFacts::from_reply(
            parse_bootstrap(BOOTSTRAP).unwrap(),
            AgentReply {
                id: 999,
                observed_at_ms: time,
                source,
                result: Ok(DecodedReply::Status(
                    parse_status(status, AgentPlatform::Linux).unwrap(),
                )),
            },
        )
    }

    #[test]
    fn installed_wire_facts_preserve_failed_recovery_zero_pending_wait_and_loaded_revision() {
        let facts = agent_facts(STATUS, ObservationSource::Live, 300).unwrap();
        assert_eq!(facts.call_id, 999);
        assert_eq!(facts.received_at_ms, 300);
        assert_eq!(facts.source, ObservationSource::Live);
        assert_eq!(facts.bootstrap.phase, BootstrapPhase::WaitingForKeystore);
        assert_eq!(facts.bootstrap.keystore, None);
        let StatusAdmission::Supported(health) = facts.status else {
            panic!("literal complete contract expected");
        };
        let installer = health.installer();
        assert_eq!(installer.startup_recovery, StartupRecovery::Failed);
        assert_eq!(installer.recovery_pending, 0);
        assert_eq!(installer.config_revision, "9f86d081884c7d65");
        assert_ne!(installer.config_revision, "edited-disk-revision");
        assert_eq!(installer.keystore, KeyStoreProvenance::File);
        assert!(installer.permissions.is_empty());
        assert_eq!(installer.gate.session, SessionState::Unknown);
        assert_eq!(installer.gate.active, None);
        assert!(!installer.gate.open);
        assert!(installer.gate.panic);
        assert_eq!(installer.backends[11].name, BackendName::Gpu);
        assert_eq!(installer.backends[11].state, BackendState::Missing);
        assert_eq!(installer.backends[12].name, BackendName::Home);
        assert_eq!(installer.backends[12].state, BackendState::Blocked);
        assert_eq!(installer.backends[13].name, BackendName::Audio);
        assert_eq!(installer.backends[13].state, BackendState::Failed);
        assert_eq!(
            installer.backends[13].reason,
            Some(BackendReason::WorkerExited)
        );
    }

    #[test]
    fn installed_identity_mismatch_and_nonstatus_reply_never_manufacture_admission() {
        let original: Value = serde_json::from_slice(STATUS).unwrap();
        for field in ["id", "pid", "started_unix_ms", "runtime_dir"] {
            let mut changed = original.clone();
            changed["result"]["installer"]["instance"][field] = match field {
                "id" => json!(1),
                "pid" => json!(4243),
                "started_unix_ms" => json!(1790950000001u64),
                "runtime_dir" => json!("/run/user/1000/other"),
                _ => unreachable!(),
            };
            assert_eq!(
                agent_facts(
                    &serde_json::to_vec(&changed).unwrap(),
                    ObservationSource::Demo,
                    17
                ),
                Err(ProbeIssue::Foreign),
                "{field}"
            );
        }
        for result in [
            Ok(DecodedReply::Acknowledged),
            Err(CallFailure::Unavailable),
            Err(CallFailure::TimeoutOutcomeUnknown),
        ] {
            assert_eq!(
                InstalledAgentFacts::from_reply(
                    parse_bootstrap(BOOTSTRAP).unwrap(),
                    AgentReply {
                        id: 1,
                        observed_at_ms: 2,
                        source: ObservationSource::Demo,
                        result,
                    }
                ),
                Err(ProbeIssue::Unverified)
            );
        }
    }

    #[test]
    fn incomplete_health_remains_contract_pending_with_original_demo_receipt() {
        let facts =
            agent_facts(br#"{"ok":true,"result":{}}"#, ObservationSource::Demo, 11).unwrap();
        assert_eq!(
            facts.status,
            StatusAdmission::PendingHealthContract(PendingHealthReason::Absent)
        );
        assert_eq!(facts.received_at_ms, 11);
        assert_eq!(facts.source, ObservationSource::Demo);
        let mut status: Value = serde_json::from_slice(STATUS).unwrap();
        status["result"]["installer"]
            .as_object_mut()
            .unwrap()
            .remove("gate");
        let facts = agent_facts(
            &serde_json::to_vec(&status).unwrap(),
            ObservationSource::Demo,
            12,
        )
        .unwrap();
        assert_eq!(
            facts.status,
            StatusAdmission::PendingHealthContract(PendingHealthReason::Incomplete)
        );
    }
}

mod runtime_tests {
    use crosspane_installer::{
        agent_contract::{KeyStoreProvenance, ObservationSource},
        platform::linux::{
            detect::{
                ProbeIssue,
                runtime::{
                    MAX_GRAPH_LIBRARIES, MAX_LIBRARIES, RuntimeInput, RuntimeReader, inspect_with,
                },
            },
            native_io::{Cancellation, Deadline, NativeError, SystemBytes},
            payload::Architecture,
        },
    };
    use std::{
        cell::{Cell, RefCell},
        collections::BTreeMap,
        path::PathBuf,
        time::Duration,
    };

    fn put(bytes: &mut [u8], offset: usize, size: usize, value: u64) {
        bytes[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
    }
    // Synthetic metadata has no executable entry point and is never loaded or executed.
    fn elf(needed: &[&str], soname: Option<&str>) -> Vec<u8> {
        let mut strings = vec![0];
        let mut tags = vec![(5, 0), (10, 0)];
        for name in needed {
            tags.push((1, strings.len() as u64));
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        if let Some(name) = soname {
            tags.push((14, strings.len() as u64));
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
        }
        tags.push((0, 0));
        let table = 176 + tags.len() * 16;
        tags[0].1 = 0x1000 + table as u64;
        tags[1].1 = strings.len() as u64;
        let mut bytes = vec![0; table + strings.len()];
        bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        for (offset, size, value) in [
            (16, 2, 3),
            (18, 2, 62),
            (20, 4, 1),
            (32, 8, 64),
            (52, 2, 64),
            (54, 2, 56),
            (56, 2, 2),
            (64, 4, 1),
            (80, 8, 0x1000),
            (120, 4, 2),
            (128, 8, 176),
            (136, 8, 0x1000 + 176),
            (152, 8, (tags.len() * 16) as u64),
            (160, 8, (tags.len() * 16) as u64),
        ] {
            put(&mut bytes, offset, size, value);
        }
        let length = bytes.len() as u64;
        put(&mut bytes, 96, 8, length);
        put(&mut bytes, 104, 8, length);
        for (index, (tag, value)) in tags.into_iter().enumerate() {
            put(&mut bytes, 176 + index * 16, 8, tag);
            put(&mut bytes, 184 + index * 16, 8, value);
        }
        bytes[table..].copy_from_slice(&strings);
        bytes
    }
    type Image = Result<(PathBuf, Vec<u8>), NativeError>;
    struct Reader {
        images: BTreeMap<String, Image>,
        calls: RefCell<Vec<String>>,
        clock: Cell<u64>,
        secret: Result<bool, NativeError>,
        secret_calls: Cell<usize>,
        cancel: Option<Cancellation>,
        stall: Duration,
    }
    impl Reader {
        fn fixed() -> Self {
            let mut reader = Self {
                images: BTreeMap::new(),
                calls: RefCell::new(Vec::new()),
                clock: Cell::new(100),
                secret: Ok(true),
                secret_calls: Cell::new(0),
                cancel: None,
                stall: Duration::ZERO,
            };
            for name in [
                "libopus.so.0",
                "libpipewire-0.3.so.0",
                "libxkbcommon.so.0",
                "libwayland-client.so.0",
            ] {
                reader.add(name, &[]);
            }
            reader
        }
        fn add(&mut self, name: &str, needed: &[&str]) {
            self.images.insert(
                name.into(),
                Ok((
                    PathBuf::from("/usr/lib").join(name),
                    elf(needed, Some(name)),
                )),
            );
        }
    }
    impl RuntimeReader for Reader {
        fn source(&self) -> ObservationSource {
            ObservationSource::Demo
        }
        fn library(&self, name: &str, _: &Deadline) -> Result<SystemBytes, NativeError> {
            self.calls.borrow_mut().push(name.into());
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            if self.stall != Duration::ZERO {
                std::thread::sleep(self.stall);
            }
            self.clock.set(self.clock.get() + 10);
            self.images
                .get(name)
                .unwrap_or(&Err(NativeError::Unavailable))
                .as_ref()
                .map(|(path, bytes)| SystemBytes {
                    path: path.clone(),
                    file_size: bytes.len() as u64,
                    bytes: bytes.clone(),
                })
                .map_err(|error| *error)
        }
        fn secret_service(&self, _: &Deadline) -> Result<bool, NativeError> {
            self.secret_calls.set(self.secret_calls.get() + 1);
            self.clock.set(self.clock.get() + 7);
            self.secret
        }
    }
    fn deadline() -> Deadline {
        Deadline::new(1000, Cancellation::default()).unwrap()
    }
    fn inspect(
        reader: &Reader,
        bytes: &[u8],
        features: &[String],
        keystore: Option<KeyStoreProvenance>,
        deadline: &Deadline,
    ) -> crosspane_installer::platform::linux::detect::RuntimeFacts {
        inspect_with(
            reader,
            deadline,
            RuntimeInput {
                architecture: Architecture::X86_64,
                features,
                agent_elf: bytes,
                keystore,
            },
            &|| reader.clock.get(),
        )
    }

    #[test]
    fn structural_software_video_requirements_keep_gpu_libei_and_audio_health_separate() {
        let mut reader = Reader::fixed();
        for name in [
            "libavcodec.so.61",
            "libavutil.so.59",
            "libavformat.so.61",
            "libswscale.so.8",
            "libx264.so.164",
        ] {
            reader.add(name, &[]);
        }
        let bytes = elf(
            &[
                "libavcodec.so.61",
                "libavutil.so.59",
                "libavformat.so.61",
                "libswscale.so.8",
                "libx264.so.164",
            ],
            None,
        );
        let facts = inspect(
            &reader,
            &bytes,
            &["video".into()],
            Some(KeyStoreProvenance::OsStore),
            &deadline(),
        );
        for value in [
            &facts.video_feature.value,
            &facts.ffmpeg.value,
            &facts.software_video.value,
            &facts.opus.value,
            &facts.pipewire_library.value,
            &facts.xkb.value,
            &facts.wayland_library.value,
        ] {
            assert_eq!(value, &Ok(true));
        }
        for value in [
            &facts.gpu.value,
            &facts.pipewire.value,
            &facts.session_manager.value,
        ] {
            assert_eq!(value, &Err(ProbeIssue::Unverified));
        }
        assert!(!facts.libei_required);
        assert!(
            facts
                .libraries
                .iter()
                .all(|row| row.required && row.resolved.value.is_ok())
        );
        assert!(
            !reader
                .calls
                .borrow()
                .iter()
                .any(|name| name.contains("cuda") || name.contains("libei"))
        );
    }

    #[test]
    fn ffmpeg_is_exactly_the_libav_libraries_the_release_agent_links() {
        // DT_NEEDED of the real stripped release agent: no libavformat.
        let needed = [
            "libavutil.so.61",
            "libswscale.so.10",
            "libavcodec.so.63",
            "libxkbcommon.so.0",
            "libpipewire-0.3.so.0",
            "libopus.so.0",
            "libgcc_s.so.1",
            "libm.so.6",
            "libc.so.6",
            "ld-linux-x86-64.so.2",
        ];
        let mut reader = Reader::fixed();
        for name in needed {
            reader.add(name, &[]);
        }
        reader.add("libavcodec.so.63", &["libx264.so.165"]);
        reader.add("libx264.so.165", &[]);
        let facts = inspect(
            &reader,
            &elf(&needed, None),
            &["video".into()],
            None,
            &deadline(),
        );
        assert_eq!(facts.ffmpeg.value, Ok(true));
        assert_eq!(facts.software_video.value, Ok(true));
        // Each of the three is still required; a missing one is a known absence.
        for missing in ["libavutil.so.61", "libswscale.so.10", "libavcodec.so.63"] {
            let without: Vec<_> = needed.into_iter().filter(|n| *n != missing).collect();
            let facts = inspect(&reader, &elf(&without, None), &[], None, &deadline());
            assert_eq!(facts.ffmpeg.value, Ok(false), "{missing}");
        }
    }

    #[test]
    fn complete_graph_non_declaration_feature_absence_and_unknown_reads_are_distinct() {
        let mut reader = Reader::fixed();
        let bytes = elf(&[], None);
        let facts = inspect(&reader, &bytes, &[], None, &deadline());
        assert_eq!(facts.video_feature.value, Ok(false));
        assert_eq!(facts.ffmpeg.value, Ok(false));
        assert_eq!(facts.software_video.value, Ok(false));
        assert_eq!(facts.keystore.value, Err(ProbeIssue::Unverified));
        reader
            .images
            .insert("libconcealed.so.1".into(), Err(NativeError::Unavailable));
        let bytes = elf(&["libconcealed.so.1"], None);
        let facts = inspect(&reader, &bytes, &[], None, &deadline());
        assert_eq!(facts.ffmpeg.value, Err(ProbeIssue::Unavailable));
        assert_eq!(facts.opus.value, Ok(true));
        assert_eq!(facts.xkb.value, Ok(true));
        assert_eq!(facts.dependency_graph.value, Err(ProbeIssue::Unavailable));
        assert_eq!(
            facts.libraries[0].resolved.value,
            Err(ProbeIssue::Unavailable)
        );
        assert!(
            facts
                .libraries
                .iter()
                .all(|row| row.resolved.value != Err(ProbeIssue::Missing))
        );
    }

    #[test]
    fn transitive_cycles_deduplicate_and_global_library_bounds_fail_closed() {
        let mut reader = Reader::fixed();
        reader.add("libfirst.so.1", &["libsecond.so.1"]);
        reader.add("libsecond.so.1", &["libfirst.so.1"]);
        let facts = inspect(
            &reader,
            &elf(&["libfirst.so.1"], None),
            &[],
            None,
            &deadline(),
        );
        assert_eq!(facts.libraries.len(), 6);
        assert_eq!(
            reader
                .calls
                .borrow()
                .iter()
                .filter(|name| name.as_str() == "libfirst.so.1")
                .count(),
            1
        );
        // The real agent's closure is ~100 libraries; the graph bound is MAX_GRAPH_LIBRARIES.
        // DT_NEEDED stays at most MAX_LIBRARIES per image, so fan out through intermediates.
        let names = (0..MAX_GRAPH_LIBRARIES - 4 - 8)
            .map(|i| format!("libfixture{i}.so.1"))
            .collect::<Vec<_>>();
        let groups = names.chunks(MAX_LIBRARIES).collect::<Vec<_>>();
        assert_eq!(groups.len(), 8);
        let parents = (0..groups.len())
            .map(|i| format!("libgroup{i}.so.1"))
            .collect::<Vec<_>>();
        for (parent, group) in parents.iter().zip(&groups) {
            let refs = group.iter().map(String::as_str).collect::<Vec<_>>();
            reader.add(parent, &refs);
            for name in *group {
                reader.add(name, &[]);
            }
        }
        let refs = parents.iter().map(String::as_str).collect::<Vec<_>>();
        reader.calls.borrow_mut().clear();
        let facts = inspect(&reader, &elf(&refs, None), &[], None, &deadline());
        assert_eq!(facts.libraries.len(), MAX_GRAPH_LIBRARIES);
        assert_eq!(facts.opus.value, Ok(true));
        reader.add(&names[0], &["libextra.so.1"]);
        reader.add("libextra.so.1", &[]);
        reader.calls.borrow_mut().clear();
        let facts = inspect(&reader, &elf(&refs, None), &[], None, &deadline());
        assert_eq!(facts.libraries.len(), MAX_GRAPH_LIBRARIES);
        assert_eq!(reader.calls.borrow().len(), MAX_GRAPH_LIBRARIES);
        assert_eq!(facts.opus.value, Ok(true));
        assert_eq!(facts.dependency_graph.value, Err(ProbeIssue::Oversize));
    }

    #[test]
    fn library_namespace_architecture_soname_and_probe_errors_never_resolve_candidates() {
        let bytes = elf(&["libwrong.so.1"], None);
        for error in [
            NativeError::Unavailable,
            NativeError::Timeout,
            NativeError::Foreign,
            NativeError::Oversize,
            NativeError::Invalid,
        ] {
            let mut reader = Reader::fixed();
            reader.images.insert("libwrong.so.1".into(), Err(error));
            let facts = inspect(&reader, &bytes, &[], None, &deadline());
            let expected = match error {
                NativeError::Timeout => ProbeIssue::Timeout,
                NativeError::Foreign => ProbeIssue::Foreign,
                NativeError::Oversize => ProbeIssue::Oversize,
                NativeError::Invalid => ProbeIssue::Malformed,
                _ => ProbeIssue::Unavailable,
            };
            assert_eq!(facts.libraries[0].resolved.value, Err(expected));
        }
        let mut wrong_architecture = elf(&[], Some("libwrong.so.1"));
        put(&mut wrong_architecture, 18, 2, 183);
        let mut executable = elf(&[], Some("libwrong.so.1"));
        put(&mut executable, 16, 2, 2);
        for (path, image, expected) in [
            (
                "/usr/lib/libwrong.so.1",
                elf(&[], Some("libother.so.1")),
                ProbeIssue::Malformed,
            ),
            (
                "/usr/lib/libwrong.so.1",
                wrong_architecture,
                ProbeIssue::Malformed,
            ),
            ("/usr/lib/libwrong.so.1", executable, ProbeIssue::Malformed),
            (
                "/owner/libwrong.so.1",
                elf(&[], Some("libwrong.so.1")),
                ProbeIssue::Foreign,
            ),
            (
                "/usr/lib/sub/libwrong.so.1",
                elf(&[], Some("libwrong.so.1")),
                ProbeIssue::Foreign,
            ),
        ] {
            let mut reader = Reader::fixed();
            reader
                .images
                .insert("libwrong.so.1".into(), Ok((PathBuf::from(path), image)));
            let facts = inspect(&reader, &bytes, &[], None, &deadline());
            assert_eq!(facts.libraries[0].resolved.value, Err(expected));
        }
    }

    #[test]
    fn secret_name_ownership_never_infers_unlock_or_changes_literal_keystore_provenance() {
        let bytes = elf(&[], None);
        for secret in [
            Ok(true),
            Ok(false),
            Err(NativeError::Unavailable),
            Err(NativeError::Timeout),
        ] {
            for literal in ["\"os_store\"", "\"file\""] {
                let keystore: KeyStoreProvenance = serde_json::from_str(literal).unwrap();
                let mut reader = Reader::fixed();
                reader.secret = secret;
                let facts = inspect(&reader, &bytes, &[], Some(keystore), &deadline());
                assert_eq!(facts.keystore.value, Ok(keystore));
                assert_eq!(
                    facts.secret_service.value,
                    secret.map_err(|error| match error {
                        NativeError::Timeout => ProbeIssue::Timeout,
                        _ => ProbeIssue::Unavailable,
                    })
                );
                assert_eq!(reader.secret_calls.get(), 1);
                assert_eq!(facts.pipewire.value, Err(ProbeIssue::Unverified));
                assert_eq!(facts.session_manager.value, Err(ProbeIssue::Unverified));
            }
        }
    }

    #[test]
    fn receipt_stamps_follow_completed_reads_and_preserve_demo_source() {
        let reader = Reader::fixed();
        let facts = inspect(
            &reader,
            &elf(&[], None),
            &[],
            Some(KeyStoreProvenance::File),
            &deadline(),
        );
        for (index, row) in facts.libraries.iter().enumerate() {
            assert_eq!(row.resolved.observed_at_ms, 110 + index as u64 * 10);
            assert_eq!(row.resolved.source, ObservationSource::Demo);
        }
        assert_eq!(facts.secret_service.observed_at_ms, 147);
        assert_eq!(facts.keystore.observed_at_ms, 147);
        assert_eq!(facts.secret_service.source, ObservationSource::Demo);
    }

    #[test]
    fn cancellation_deadline_and_malformed_input_cannot_publish_late_probe_success() {
        let bytes = elf(&[], None);
        let cancellation = Cancellation::default();
        let expired = Deadline::new(1000, cancellation.clone()).unwrap();
        cancellation.cancel();
        let reader = Reader::fixed();
        let facts = inspect(&reader, &bytes, &[], None, &expired);
        assert!(reader.calls.borrow().is_empty());
        assert_eq!(reader.secret_calls.get(), 0);
        assert_eq!(facts.opus.value, Err(ProbeIssue::Cancelled));
        assert_eq!(facts.secret_service.value, Err(ProbeIssue::Cancelled));
        let cancellation = Cancellation::default();
        let deadline = Deadline::new(1000, cancellation.clone()).unwrap();
        let mut reader = Reader::fixed();
        reader.cancel = Some(cancellation);
        let facts = inspect(&reader, &bytes, &[], None, &deadline);
        assert_eq!(reader.calls.borrow().len(), 1);
        assert!(
            facts
                .libraries
                .iter()
                .all(|row| row.resolved.value == Err(ProbeIssue::Cancelled))
        );
        let mut reader = Reader::fixed();
        reader.stall = Duration::from_millis(20);
        let facts = inspect(
            &reader,
            &bytes,
            &[],
            None,
            &Deadline::new(5, Cancellation::default()).unwrap(),
        );
        assert_eq!(reader.calls.borrow().len(), 1);
        assert_eq!(facts.opus.value, Err(ProbeIssue::Timeout));
        assert_eq!(reader.secret_calls.get(), 0);
        for bytes in [&b"invalid"[..], &vec![0; 4 * 1024 * 1024 + 1][..]] {
            let reader = Reader::fixed();
            let facts = inspect(&reader, bytes, &[], None, &super::runtime_tests::deadline());
            assert!(matches!(
                facts.dependency_graph.value,
                Err(ProbeIssue::Malformed | ProbeIssue::Oversize)
            ));
        }
        let reader = Reader::fixed();
        let facts = inspect(
            &reader,
            &elf(&[], None),
            &vec!["video".into(); 33],
            None,
            &super::runtime_tests::deadline(),
        );
        assert_eq!(facts.video_feature.value, Err(ProbeIssue::Oversize));
    }
}
